//! Binary entry point: `.env` loading, tracing init, clap dispatch, and
//! exit-code mapping (see `docs/CLI.md` §7). `anyhow` is permitted only here.

use std::io::IsTerminal;
use std::path::Path;

use anyhow::{Context, bail};
use clap::Parser;
use myansub::cli::{AutoApprove, Cli, Command, InitArgs, RunArgs};
use myansub::config::{self, CliOverrides, Config, global_config_path, sources_from_paths};
use myansub::error::{ConfigError, ExitCode, PipelineError, ProjectError, exit};
use myansub::llm::pricing::Cost;
use myansub::pipeline::input::{InitOutcome, run_init};
use myansub::pipeline::orchestrator::{
    AutoApproveGate, RunOptions, RunOutcome, RunRequest, StatusReport, run as run_pipeline,
    run_exit_code, status as gather_status,
};
use myansub::project::resolve_project;
use myansub::store::StageStatus;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

fn main() {
    // Missing `.env` is not an error (docs/CONFIG.md §1).
    let _ = dotenvy::dotenv();

    // clap handles `--help` (exit 0) and usage errors (exit 2) itself.
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let code = match dispatch(&cli) {
        Ok(code) => code,
        Err(err) => {
            tracing::error!("{err:#}");
            exit_code_for(&err)
        }
    };
    std::process::exit(code);
}

/// Initializes tracing. `RUST_LOG` wins; otherwise `-v`/`-vv` raise the level
/// above the default `info` (docs/ARCHITECTURE.md §9).
fn init_tracing(verbose: u8) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let level = match verbose {
            0 => "info",
            1 => "debug",
            _ => "trace",
        };
        EnvFilter::new(format!("myansub={level}"))
    });
    // A failure here means a global subscriber was already set; ignore it.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Dispatches a parsed command, returning the success-side exit code (0, or 130
/// when interrupted; failed-unit code 4 arrives with translation in M6). Errors
/// are mapped to codes by [`exit_code_for`]. `init`, `run` (up to analysis), and
/// `status` are implemented; the rest map to a generic error until their
/// milestones land.
fn dispatch(cli: &Cli) -> anyhow::Result<i32> {
    match &cli.command {
        Command::Init(args) => run_init_command(cli, args).map(|()| exit::SUCCESS),
        Command::Run(args) => run_run_command(cli, args),
        Command::Status => run_status_command(cli),
        other => not_implemented(command_name(other)),
    }
}

/// Handles `myansub init <FILE> --from <LANG> …` (docs/CLI.md §6).
fn run_init_command(cli: &Cli, args: &InitArgs) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("determine current directory")?;
    let location = resolve_project(cli.project.as_deref(), Some(&args.file), &cwd)?;

    // Validate configuration at startup (docs/CONFIG.md §5); unused for the input
    // stage but failing here surfaces a bad config before any work is done.
    let _config = load_config(cli, &location.dir)?;

    let interactive = std::io::stdin().is_terminal();
    let runtime = tokio::runtime::Runtime::new().context("start async runtime")?;
    let outcome = runtime.block_on(run_init(
        &location.dir,
        &args.file,
        &args.from,
        args.encoding.as_deref(),
        args.force,
        interactive,
        cli.yes,
    ))?;

    report_init_outcome(&location.dir, &outcome);
    Ok(())
}

/// Handles `myansub run [FILE] …` (docs/CLI.md §2/§6). Resolves or creates the
/// project, advances stages up to the analysis boundary (M4), and maps the
/// outcome to an exit code — preferring the interrupt code (130) on Ctrl-C.
fn run_run_command(cli: &Cli, args: &RunArgs) -> anyhow::Result<i32> {
    let cwd = std::env::current_dir().context("determine current directory")?;
    let location = resolve_project(cli.project.as_deref(), args.file.as_deref(), &cwd)?;
    let config = load_config(cli, &location.dir)?;

    let options = RunOptions {
        auto_approve: args.auto_approve.map(map_auto_approve),
        allow_incomplete: args.allow_incomplete,
        dry_run: args.dry_run,
    };
    let request = RunRequest {
        project_dir: &location.dir,
        source_path: args.file.as_deref(),
        from_lang: args.from.as_deref(),
        encoding: args.encoding.as_deref(),
        options,
    };

    let runtime = tokio::runtime::Runtime::new().context("start async runtime")?;
    let token = CancellationToken::new();
    let result = runtime.block_on(async {
        // Ctrl-C triggers the CancellationToken (docs/ARCHITECTURE.md §6 rule 6,
        // NFR-3). The orchestrator and (from M5) the scheduler watch the token.
        let signal_token = token.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                signal_token.cancel();
            }
        });
        run_pipeline(request, &config, &token).await
    });

    // The interrupt wins over any error: if cancellation was observed, report 130
    // even when a stage errored out while shutting down (matters once stages do
    // real work in M5+). Only propagate a non-cancellation error (docs/CLI.md §7).
    if token.is_cancelled() {
        report_run_outcome(&RunOutcome::Interrupted);
        return Ok(exit::INTERRUPTED);
    }
    let outcome = result?;
    report_run_outcome(&outcome);
    Ok(run_exit_code(token.is_cancelled(), &outcome))
}

/// Maps the CLI auto-approve choice onto the orchestrator's gate enum (the
/// orchestrator must not depend on `cli`, docs/ARCHITECTURE.md §3).
fn map_auto_approve(choice: AutoApprove) -> AutoApproveGate {
    match choice {
        AutoApprove::Analysis => AutoApproveGate::Analysis,
        AutoApprove::Translation => AutoApproveGate::Translation,
        AutoApprove::All => AutoApproveGate::All,
    }
}

/// Logs where `run` stopped and the next action for the user (docs/CLI.md §6).
fn report_run_outcome(outcome: &RunOutcome) {
    match outcome {
        RunOutcome::StoppedAtStage { stage, next_action } => {
            tracing::info!(
                "stopped at the {} stage; next: {next_action}",
                stage.as_str()
            );
        }
        RunOutcome::StoppedAtGate { stage, next_action } => {
            tracing::info!(
                "stopped at the {} gate (not approved); next: {next_action}",
                stage.as_str()
            );
        }
        RunOutcome::Interrupted => {
            tracing::info!("interrupted; re-run the identical command to resume");
        }
        RunOutcome::Completed => {
            tracing::info!("nothing left to do");
        }
    }
}

/// Handles `myansub status` (docs/CLI.md §6, docs/REQUIREMENTS.md FR-10).
fn run_status_command(cli: &Cli) -> anyhow::Result<i32> {
    let cwd = std::env::current_dir().context("determine current directory")?;
    let location = resolve_project(cli.project.as_deref(), None, &cwd)?;
    let config = load_config(cli, &location.dir)?;

    let runtime = tokio::runtime::Runtime::new().context("start async runtime")?;
    let report = runtime.block_on(gather_status(&location.dir, &config))?;
    print_status(&report);
    Ok(exit::SUCCESS)
}

/// Prints the `status` report to stdout (docs/CLI.md §6).
fn print_status(report: &StatusReport) {
    if report.source_changed {
        println!(
            "WARNING: the source file changed since init; re-run `myansub init --force` to re-initialize."
        );
    }

    println!("Stages:");
    for line in &report.stages {
        let origin = match line.approval_origin {
            Some(o) => format!(" ({o:?})"),
            None => String::new(),
        };
        println!(
            "  {:<18} {}{origin}",
            line.stage.as_str(),
            status_label(line.status)
        );
    }

    println!(
        "Analysis chunks:     pending {} / in-flight {} / done {} / failed {}",
        report.analysis_chunks.pending,
        report.analysis_chunks.in_flight,
        report.analysis_chunks.done,
        report.analysis_chunks.failed,
    );
    println!(
        "Translation batches: pending {} / in-flight {} / done {} / failed {}",
        report.translation_batches.pending,
        report.translation_batches.in_flight,
        report.translation_batches.done,
        report.translation_batches.failed,
    );

    if report.flag_counts.is_empty() {
        println!("Quality flags:       none");
    } else {
        println!("Quality flags:");
        for (flag, count) in &report.flag_counts {
            println!("  {flag:<20} {count}");
        }
    }

    println!(
        "Token usage: input {} (cached {}) / output {} / thinking {}",
        report.usage.prompt_tokens,
        report.usage.cached_tokens,
        report.usage.output_tokens,
        report.usage.thinking_tokens,
    );
    match report.cost {
        Cost::Known(usd) => println!("Estimated cost: ${usd:.4}"),
        Cost::Unknown => println!("Estimated cost: unknown"),
    }
    if !report.unknown_cost_models.is_empty() {
        println!(
            "  (no price for: {}; their cost is excluded)",
            report.unknown_cost_models.join(", ")
        );
    }
}

/// A human label for a stage status in the `status` view.
fn status_label(status: StageStatus) -> &'static str {
    match status {
        StageStatus::NotStarted => "not started",
        StageStatus::Running => "running",
        StageStatus::Completed => "completed",
        StageStatus::Approved => "approved",
    }
}

/// Logs the user-facing result of `init`.
fn report_init_outcome(project_dir: &Path, outcome: &InitOutcome) {
    let dir = project_dir.display();
    match outcome {
        InitOutcome::Initialized(input) => {
            tracing::info!("initialized project at {dir}");
            warn_replacement_cues(input);
        }
        InitOutcome::ReInitialized(input) => {
            tracing::info!("re-initialized project at {dir}");
            warn_replacement_cues(input);
        }
        InitOutcome::AlreadyInitialized => {
            tracing::info!("project at {dir} is already initialized; nothing to do");
        }
        InitOutcome::Cancelled => {
            tracing::info!("re-initialization cancelled; project at {dir} is unchanged");
        }
    }
}

/// Logs the affected cue indices when decoding produced replacement characters
/// (docs/REQUIREMENTS.md FR-1).
fn warn_replacement_cues(input: &myansub::pipeline::input::InputOutcome) {
    if !input.replacement_cues.is_empty() {
        tracing::warn!(
            "decoding produced replacement characters in cues: {:?}",
            input.replacement_cues
        );
    }
}

/// Loads and validates configuration from disk (docs/CONFIG.md §1). Global and
/// project config files are included only when present; `--config` is a hard
/// error if missing.
fn load_config(cli: &Cli, project_dir: &Path) -> anyhow::Result<Config> {
    let env: Vec<(String, String)> = std::env::vars()
        .filter(|(key, _)| key.starts_with("MYANSUB_"))
        .collect();
    let sources = sources_from_paths(
        global_config_path(),
        Some(project_dir.join("myansub.toml")),
        cli.config.clone(),
        env,
        CliOverrides::default(),
    );
    Ok(config::load(&sources)?)
}

/// Maps a dispatch error to its exit code by inspecting the error chain for a
/// known typed error (docs/CLI.md §7). Falls back to the generic error code.
fn exit_code_for(err: &anyhow::Error) -> i32 {
    for cause in err.chain() {
        if let Some(e) = cause.downcast_ref::<PipelineError>() {
            return e.exit_code();
        }
        if let Some(e) = cause.downcast_ref::<ProjectError>() {
            return e.exit_code();
        }
        if let Some(e) = cause.downcast_ref::<ConfigError>() {
            return e.exit_code();
        }
    }
    exit::ERROR
}

fn not_implemented(command: &str) -> anyhow::Result<i32> {
    bail!("`{command}` is not implemented yet")
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Init(_) => "init",
        Command::Run(_) => "run",
        Command::List(_) => "list",
        Command::Status => "status",
        Command::Analyze(_) => "analyze",
        Command::Analysis { .. } => "analysis",
        Command::Translate(_) => "translate",
        Command::Review { .. } => "review",
        Command::Export(_) => "export",
        Command::Cache { .. } => "cache",
    }
}
