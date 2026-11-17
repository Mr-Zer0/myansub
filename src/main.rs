//! Binary entry point: `.env` loading, tracing init, clap dispatch, and
//! exit-code mapping (see `docs/CLI.md` §7). `anyhow` is permitted only here.

use std::io::IsTerminal;
use std::path::Path;

use anyhow::{Context, bail};
use clap::Parser;
use myansub::cli::{Cli, Command, InitArgs};
use myansub::config::{self, CliOverrides, Config, global_config_path, sources_from_paths};
use myansub::error::{ConfigError, ExitCode, PipelineError, ProjectError, exit};
use myansub::pipeline::input::{InitOutcome, run_init};
use myansub::project::resolve_project;
use tracing_subscriber::EnvFilter;

fn main() {
    // Missing `.env` is not an error (docs/CONFIG.md §1).
    let _ = dotenvy::dotenv();

    // clap handles `--help` (exit 0) and usage errors (exit 2) itself.
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let code = match dispatch(&cli) {
        Ok(()) => exit::SUCCESS,
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

/// Dispatches a parsed command. Only `init` is implemented in M4-B; the rest map
/// to a generic error (exit 1) until their milestones land.
fn dispatch(cli: &Cli) -> anyhow::Result<()> {
    match &cli.command {
        Command::Init(args) => run_init_command(cli, args),
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

fn not_implemented(command: &str) -> anyhow::Result<()> {
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
