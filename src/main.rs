//! Binary entry point: `.env` loading, tracing init, clap dispatch, and
//! exit-code mapping (see `docs/CLI.md` §7). `anyhow` is permitted only here.

use std::io::IsTerminal;
use std::path::Path;

use std::time::Duration;

use anyhow::{Context, bail};
use clap::Parser;
use myansub::cli::{
    AnalysisCommand, AnalyzeArgs, AutoApprove, CacheCommand, Cli, Command, ExportArgs, InitArgs,
    OutputFormat, ReviewCommand, ReviewListArgs, ReviewSelectionArgs, RunArgs, ServiceTier,
    TranslateArgs,
};
use myansub::config::{self, CliOverrides, Config, global_config_path, sources_from_paths};
use myansub::domain::SubtitleFormat;
use myansub::error::{ConfigError, ExitCode, PipelineError, ProjectError, ReviewError, exit};
use myansub::llm::client::Secret;
use myansub::llm::pricing::{Cost, PriceTable};
use myansub::llm::{DryRun, Estimator, Gemini};
use myansub::pipeline::analysis::{
    AnalysisFlags, AnalysisModeKind, AnalysisOutcome, DryRunReport, analysis_exit_code,
    dry_run_analysis, prepare_analysis, run_analysis_confirmed,
};
use myansub::pipeline::confirm::{ConfirmSettings, ConfirmedRun};
use myansub::pipeline::input::check_source_unchanged;
use myansub::pipeline::input::{InitOutcome, run_init};
use myansub::pipeline::orchestrator::{
    AutoApproveGate, RunOptions, RunOutcome, RunReport, RunRequest, StatusReport, analysis_params,
    run_confirmed, run_exit_code, status as gather_status, translation_params,
};
use myansub::pipeline::output::{
    OutputReport, resolve_output_format_from_flags, resolve_output_target, run_output,
    stored_output_prefs,
};
use myansub::pipeline::translation::{
    DryRunReport as TranslationDryRunReport, TranslationFlags, TranslationOutcome,
    dry_run_translation, prepare_translation, run_translation_confirmed, translation_exit_code,
};
use myansub::project::{project_db_path, resolve_project};
use myansub::review::EnvEditorLauncher;
use myansub::review::analysis as analysis_review;
use myansub::review::translation as translation_review;
use myansub::review::translation::{
    ReviewFilters, ReviewSelection, parse_cue_id, parse_flag, parse_id_range,
};
use myansub::store::repo_project::get_project;
use myansub::store::{StageStatus, Store, repo_llm};
use std::sync::Arc;

use myansub::pipeline::progress::{NoopProgress, Progress};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

mod progress_render;
use progress_render::StderrProgress;

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

/// Dispatches a parsed command, returning the success-side exit code (0, 130 when
/// interrupted, 3 at an unapproved gate, or 4 when a stage finished with failed
/// units). Errors are mapped to codes by [`exit_code_for`]. `run` now drives the
/// whole pipeline (analysis → translation → export, M7-C); `list` and `cache` map to
/// a generic error until their milestones land.
fn dispatch(cli: &Cli) -> anyhow::Result<i32> {
    match &cli.command {
        Command::Init(args) => run_init_command(cli, args).map(|()| exit::SUCCESS),
        Command::Run(args) => run_run_command(cli, args),
        Command::Status => run_status_command(cli),
        Command::Analyze(args) => run_analyze_command(cli, args),
        Command::Analysis { command } => run_analysis_review_command(cli, command),
        Command::Translate(args) => run_translate_command(cli, args),
        Command::Review { command } => run_translation_review_command(cli, command),
        Command::Export(args) => run_export_command(cli, args),
        Command::Cache { command } => run_cache_command(cli, command),
        other => not_implemented(command_name(other)),
    }
}

/// The cost-confirmation settings for a token-spending command (`docs/CLI.md` §5):
/// the configured threshold, whether stdin is a TTY, and the global `--yes` flag.
fn confirm_settings(cli: &Cli, config: &Config) -> ConfirmSettings {
    ConfirmSettings {
        confirm_above_usd: config.llm.confirm_above_usd,
        interactive: std::io::stdin().is_terminal(),
        yes: cli.yes,
    }
}

/// Builds the live-progress sink for a token-spending command (`docs/CLI.md` §8,
/// ADR-046(d)). `--no-progress` and `--dry-run` select the zero-cost [`NoopProgress`]
/// (a dry run executes no units, so it is silent). Otherwise the stderr renderer draws
/// an in-place line only when stderr is a terminal AND verbosity is default (no
/// `-v`/`-vv` and no `RUST_LOG` override — raised logging would clobber an in-place
/// line, so it degrades to throttled plain lines).
fn build_progress(cli: &Cli, dry_run: bool) -> Arc<dyn Progress> {
    let mode = progress_render::decide_mode(
        cli.no_progress,
        dry_run,
        std::io::stderr().is_terminal(),
        cli.verbose,
        std::env::var_os("RUST_LOG").is_some(),
    );
    match mode {
        progress_render::ProgressMode::Off => Arc::new(NoopProgress),
        progress_render::ProgressMode::Live => Arc::new(StderrProgress::new(true)),
        progress_render::ProgressMode::Plain => Arc::new(StderrProgress::new(false)),
    }
}

/// Prints the clean-abort message when the user declines a cost confirmation at the
/// interactive prompt; nothing was spent (`docs/CLI.md` §5).
fn report_declined() {
    println!("aborted at the cost confirmation; no tokens were spent.");
}

/// Handles `myansub init <FILE> --from <LANG> …` (docs/CLI.md §6).
fn run_init_command(cli: &Cli, args: &InitArgs) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("determine current directory")?;
    let location = resolve_project(cli.project.as_deref(), Some(&args.file), &cwd)?;

    // Validate configuration at startup (docs/CONFIG.md §5); unused for the input
    // stage but failing here surfaces a bad config before any work is done.
    let _config = load_config(cli, &location.dir)?;

    // Resolve the output-format flags up front so a format/extension disagreement
    // is a usage error (exit 2) *before* any work is done — never after the project
    // is created (docs/CLI.md §3). The resolved format (explicit or derived from a
    // known `--output` extension) is what gets stored.
    let flag_path = args.output.output.as_deref();
    let resolved_format = resolve_output_format_from_flags(
        args.output.output_format.map(map_output_format),
        flag_path,
    )?;
    let output_format = resolved_format.map(|f| f.label().to_string());
    let output_path = flag_path.map(|p| p.to_string_lossy().into_owned());

    let interactive = std::io::stdin().is_terminal();
    let runtime = tokio::runtime::Runtime::new().context("start async runtime")?;
    // The output preferences are persisted atomically with project creation inside
    // `run_init` (docs/CLI.md §3), so there is no second write to lose on a crash.
    let outcome = runtime.block_on(run_init(
        &location.dir,
        &args.file,
        &args.from,
        args.encoding.as_deref(),
        args.force,
        interactive,
        cli.yes,
        output_format.as_deref(),
        output_path.as_deref(),
    ))?;

    report_init_outcome(&location.dir, &outcome);
    Ok(())
}

/// Handles `myansub run [FILE] …` (docs/CLI.md §2/§6). Resolves or creates the
/// project, advances the whole pipeline (analysis → translation → export) with the
/// injected transport, auto-approving the gates `--auto-approve` names, and maps the
/// outcome to an exit code — preferring the interrupt code (130) on Ctrl-C.
///
/// The transport is the Gemini client built from `GEMINI_API_KEY` for a real run, or
/// `DryRun` for `--dry-run` (so a dry run never needs a key). `run` wraps it in the
/// `Metered<Cached<Retrying<_>>>` stack per LLM call (ADR-034, docs/ARCHITECTURE.md §5).
fn run_run_command(cli: &Cli, args: &RunArgs) -> anyhow::Result<i32> {
    let cwd = std::env::current_dir().context("determine current directory")?;
    let location = resolve_project(cli.project.as_deref(), args.file.as_deref(), &cwd)?;
    let config = load_config(cli, &location.dir)?;

    // Output-format flags are validated up front so a format/extension disagreement
    // is a usage error (exit 2) before any work — on an existing project too
    // (docs/CLI.md §3). The resolved preferences are threaded into the request,
    // stored atomically when this invocation creates the project, and used when `run`
    // exports the output.
    let flag_path = args.output.output.as_deref();
    let resolved_format = resolve_output_format_from_flags(
        args.output.output_format.map(map_output_format),
        flag_path,
    )?;

    let options = RunOptions {
        auto_approve: args.auto_approve.map(map_auto_approve),
        allow_incomplete: args.allow_incomplete,
        dry_run: args.dry_run,
        // `run_confirmed` sets the in-pipeline translation gate settings below.
        confirm: None,
    };
    let request = RunRequest {
        project_dir: &location.dir,
        source_path: args.file.as_deref(),
        from_lang: args.from.as_deref(),
        encoding: args.encoding.as_deref(),
        output_format: resolved_format,
        output_path: flag_path,
        options,
    };

    let settings = confirm_settings(cli, &config);
    let runtime = tokio::runtime::Runtime::new().context("start async runtime")?;
    let token = CancellationToken::new();
    let mut run_report = RunReport::default();
    let result = runtime.block_on(async {
        // Ctrl-C triggers the CancellationToken (docs/ARCHITECTURE.md §6 rule 6,
        // NFR-3). The orchestrator and each stage's scheduler watch the token.
        let signal_token = token.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                signal_token.cancel();
            }
        });

        if args.dry_run {
            // A dry run estimates the stages and writes nothing, so it needs no API
            // key and never prompts. The dry-run path inside `run` builds its own
            // `Cached<DryRun>` from config (ADR-033), so this `DryRun` only satisfies
            // the generic transport bound; it is never used to send a request.
            let output_ratio = config.translation.output_ratio;
            let estimator = Estimator {
                bytes_per_token_latin: config.estimate.bytes_per_token_latin,
                bytes_per_token_myanmar: config.estimate.bytes_per_token_myanmar,
                bytes_per_token_cjk: config.estimate.bytes_per_token_cjk,
            };
            run_confirmed(
                move || Ok(DryRun::new(estimator, output_ratio)),
                request,
                &config,
                &token,
                &mut run_report,
                settings,
                build_progress(cli, true),
            )
            .await
        } else {
            // Cost-confirmation gate (docs/CLI.md §4/§5): `--auto-approve` does not
            // imply `--yes`, so the gate still applies to an auto-approving run.
            // `build_gemini` runs only once the gate is cleared.
            run_confirmed(
                || build_gemini(&config),
                request,
                &config,
                &token,
                &mut run_report,
                settings,
                build_progress(cli, false),
            )
            .await
        }
    });

    // The interrupt wins over any error: if cancellation was observed, report 130
    // even when a stage errored out while shutting down. Only propagate a
    // non-cancellation error (docs/CLI.md §7).
    if token.is_cancelled() {
        report_run_outcome(&RunOutcome::Interrupted);
        return Ok(exit::INTERRUPTED);
    }
    match result? {
        ConfirmedRun::Ran(outcome) => {
            report_run_outcome(&outcome);
            report_run_extras(&run_report);
            Ok(run_exit_code(token.is_cancelled(), &outcome))
        }
        ConfirmedRun::Declined => {
            report_declined();
            Ok(exit::SUCCESS)
        }
    }
}

/// Prints the side-information `run` surfaced: the suspect-cue count when the
/// translation gate was auto-approved, and the `--dry-run` token/cost estimate
/// (docs/CLI.md §4/§6, FR-9). All to stdout so a cost preview is visible regardless
/// of `RUST_LOG`.
fn report_run_extras(report: &RunReport) {
    if let Some(suspect) = report.suspect_auto_approved {
        if suspect > 0 {
            println!(
                "auto-approving translation; {suspect} cue(s) flagged suspect — \
                 review with `myansub review list --suspect`."
            );
        } else {
            println!("auto-approving translation; no cues flagged suspect.");
        }
    }
    if let Some(estimate) = &report.dry_run {
        println!("Dry run estimate:");
        println!(
            "  estimated tokens: input {} / output {}",
            estimate.prompt_tokens, estimate.output_tokens
        );
        match estimate.cost {
            Cost::Known(usd) => println!("  estimated cost:  ${usd:.4}"),
            Cost::Unknown => {
                println!("  estimated cost:  unknown (no price entry for the model)")
            }
        }
        println!("(dry run: nothing was written to the database.)");
    }
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

/// Prints where `run` stopped and the next action for the user (docs/CLI.md §6).
fn report_run_outcome(outcome: &RunOutcome) {
    match outcome {
        RunOutcome::StoppedAtStage { stage, next_action } => {
            println!(
                "stopped at the {} stage; next: {next_action}",
                stage.as_str()
            );
        }
        RunOutcome::StoppedAtGate { stage, next_action } => {
            println!(
                "stopped at the {} gate (not approved); next: {next_action}",
                stage.as_str()
            );
        }
        RunOutcome::Interrupted => {
            println!("interrupted; re-run the identical command to resume.");
        }
        RunOutcome::Completed => {
            println!("run complete; the output file was written.");
        }
        RunOutcome::CompletedWithFailures { failed } => {
            println!(
                "run complete with {failed} failed unit(s); the output used the source text \
                 for the affected cues. See them with `myansub status`."
            );
        }
        RunOutcome::DryRun => {
            println!("dry run complete; nothing was written.");
        }
        RunOutcome::Aborted => {
            println!("aborted at the cost confirmation; no further tokens were spent.");
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

/// Handles `myansub analyze [--model M] [--tier T] [--redo] [--retry-failed]
/// [--dry-run]` (docs/CLI.md §6). Resolves the project, enforces the analysis gate
/// (input completed), builds the LLM stack (DryRun for `--dry-run`, otherwise the
/// Gemini transport from `GEMINI_API_KEY`), runs the analysis stage, and maps the
/// outcome to an exit code (0 ok / 4 failed units / 3 precondition / 130 interrupt).
fn run_analyze_command(cli: &Cli, args: &AnalyzeArgs) -> anyhow::Result<i32> {
    let cwd = std::env::current_dir().context("determine current directory")?;
    let location = resolve_project(cli.project.as_deref(), None, &cwd)?;
    let overrides = CliOverrides {
        analysis_model: args.model.clone(),
        service_tier: args.tier.map(tier_label),
        ..CliOverrides::default()
    };
    let config = load_config_with(cli, &location.dir, overrides)?;

    let db = project_db_path(&location.dir);
    if !db.is_file() {
        return Err(PipelineError::Uninitialized {
            dir: location.dir.clone(),
        }
        .into());
    }

    let runtime = tokio::runtime::Runtime::new().context("start async runtime")?;
    let token = CancellationToken::new();
    let code = runtime.block_on(async {
        let store = Store::open(&db)?;
        let Some(project) = store.call(|conn| get_project(conn)).await? else {
            return Err(PipelineError::Uninitialized {
                dir: location.dir.clone(),
            });
        };

        // Source-hash check + analysis gate (input completed) + load cues
        // (docs/ARCHITECTURE.md §2/§6).
        let cues = prepare_analysis(&store, Path::new(&project.source_path)).await?;
        let params = analysis_params(&config, &project.source_hash);

        if args.dry_run {
            let prices = PriceTable::from_config(&config.pricing);
            let report = dry_run_analysis(&store, &cues, &params, &prices).await?;
            print_dry_run_report(&report);
            return Ok(exit::SUCCESS);
        }

        // Ctrl-C triggers the CancellationToken (docs/ARCHITECTURE.md §6 rule 6).
        let signal_token = token.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                signal_token.cancel();
            }
        });

        let flags = AnalysisFlags {
            redo: args.redo,
            retry_failed: args.retry_failed,
        };
        // Cost-confirmation gate (docs/CLI.md §5, docs/LLM.md §9): estimate first,
        // then confirm. `build_gemini` (and the API key it needs) is only invoked
        // once the gate is cleared, so a refusal exits 3 without requiring a key.
        let prices = PriceTable::from_config(&config.pricing);
        let settings = confirm_settings(cli, &config);
        // Live progress on stderr (`docs/CLI.md` §8); the dry-run path returned above.
        let progress = build_progress(cli, false);
        let confirmed = run_analysis_confirmed(
            || build_gemini(&config),
            &store,
            &cues,
            &params,
            flags,
            &token,
            &prices,
            settings,
            progress,
        )
        .await?;
        match confirmed {
            ConfirmedRun::Ran(outcome) => {
                report_analysis_outcome(&outcome);
                Ok(analysis_exit_code(token.is_cancelled(), &outcome))
            }
            ConfirmedRun::Declined => {
                report_declined();
                Ok(exit::SUCCESS)
            }
        }
    });

    // A typed PipelineError (gate, source, store, LLM) propagates for exit mapping.
    code.map_err(anyhow::Error::new)
}

/// Handles `myansub translate [--model M] [--tier T] [--concurrency N] [--dry-run]`
/// (docs/CLI.md §6). Resolves the project, enforces the translation gate (current
/// analysis approved), builds the LLM stack (DryRun for `--dry-run`, otherwise the
/// Gemini transport from `GEMINI_API_KEY`), runs the translation stage, and maps the
/// outcome to an exit code (0 ok / 4 failed units / 3 precondition / 130 interrupt).
///
/// `--retry-failed` requeues failed batches, `--redo` re-translates unlocked cues,
/// and `--stale` re-translates stale, unlocked cues (`docs/CLI.md` §6).
fn run_translate_command(cli: &Cli, args: &TranslateArgs) -> anyhow::Result<i32> {
    let cwd = std::env::current_dir().context("determine current directory")?;
    let location = resolve_project(cli.project.as_deref(), None, &cwd)?;
    let overrides = CliOverrides {
        translation_model: args.model.clone(),
        service_tier: args.tier.map(tier_label),
        concurrency: args.concurrency,
        ..CliOverrides::default()
    };
    let config = load_config_with(cli, &location.dir, overrides)?;

    let db = project_db_path(&location.dir);
    if !db.is_file() {
        return Err(PipelineError::Uninitialized {
            dir: location.dir.clone(),
        }
        .into());
    }

    let runtime = tokio::runtime::Runtime::new().context("start async runtime")?;
    let token = CancellationToken::new();
    let code = runtime.block_on(async {
        let store = Store::open(&db)?;
        let Some(project) = store.call(|conn| get_project(conn)).await? else {
            return Err(PipelineError::Uninitialized {
                dir: location.dir.clone(),
            });
        };

        // Source-hash check + translation gate (analysis approved) + load cues
        // (docs/ARCHITECTURE.md §2/§6).
        let cues = prepare_translation(&store, Path::new(&project.source_path)).await?;
        let params = translation_params(&config);

        if args.dry_run {
            let prices = PriceTable::from_config(&config.pricing);
            let report = dry_run_translation(&store, &cues, &params, &prices).await?;
            print_translation_dry_run(&report);
            return Ok(exit::SUCCESS);
        }

        // Ctrl-C triggers the CancellationToken (docs/ARCHITECTURE.md §6 rule 6).
        let signal_token = token.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                signal_token.cancel();
            }
        });

        let flags = TranslationFlags {
            redo: args.redo,
            retry_failed: args.retry_failed,
            stale: args.stale,
        };
        // Cost-confirmation gate (docs/CLI.md §5, docs/LLM.md §7/§9): for `--stale`
        // the estimate covers only the stale, unlocked cues. `build_gemini` runs only
        // once the gate is cleared, so a refusal exits 3 without requiring a key.
        let prices = PriceTable::from_config(&config.pricing);
        let settings = confirm_settings(cli, &config);
        // Live progress on stderr (`docs/CLI.md` §8); the dry-run path returned above.
        let progress = build_progress(cli, false);
        let confirmed = run_translation_confirmed(
            || build_gemini(&config),
            &store,
            &cues,
            &params,
            flags,
            &token,
            &prices,
            settings,
            progress,
        )
        .await?;
        match confirmed {
            ConfirmedRun::Ran(outcome) => {
                report_translation_outcome(&outcome);
                Ok(translation_exit_code(token.is_cancelled(), &outcome))
            }
            ConfirmedRun::Declined => {
                report_declined();
                Ok(exit::SUCCESS)
            }
        }
    });

    code.map_err(anyhow::Error::new)
}

/// Prints where the translation stage stopped and the next action (docs/CLI.md §6).
fn report_translation_outcome(outcome: &TranslationOutcome) {
    match outcome {
        TranslationOutcome::Completed { batches } => {
            println!(
                "translation complete; {batches} batch(es) done. \
                 Next: `myansub review approve` (or review with `myansub review list`)."
            );
        }
        TranslationOutcome::NothingToDo => {
            println!("no translatable cues to translate.");
        }
        TranslationOutcome::CompletedWithFailures { failed, incomplete } => {
            println!(
                "translation finished with {failed} failed and {incomplete} incomplete batch(es). \
                 See them with `myansub status`; re-run `myansub translate --retry-failed` \
                 to retry just the failures."
            );
        }
        TranslationOutcome::Interrupted => {
            println!("interrupted; re-run `myansub translate` to resume.");
        }
    }
}

/// Prints the `translate --dry-run` estimate (docs/CLI.md §6, docs/LLM.md §9).
fn print_translation_dry_run(report: &TranslationDryRunReport) {
    println!("Translation dry run:");
    println!("  batches:          {}", report.batches);
    println!("  LLM calls:        {}", report.calls);
    println!(
        "  estimated tokens: input {} / output {}",
        report.estimated_prompt_tokens, report.estimated_output_tokens
    );
    match report.estimated_cost {
        Cost::Known(usd) => println!("  estimated cost:  ${usd:.4}"),
        Cost::Unknown => println!("  estimated cost:  unknown (no price entry for the model)"),
    }
    println!("(dry run: nothing was written to the database.)");
}

/// Handles `myansub analysis show|edit|export|import|approve` (docs/CLI.md §6).
/// Resolves the project, opens the store, and dispatches to the review module. A
/// missing current analysis maps to exit 3 (precondition); validation failures and
/// I/O to exit 1 (see [`exit_code_for`]).
fn run_analysis_review_command(cli: &Cli, command: &AnalysisCommand) -> anyhow::Result<i32> {
    let cwd = std::env::current_dir().context("determine current directory")?;
    let location = resolve_project(cli.project.as_deref(), None, &cwd)?;
    // Validate configuration at startup (docs/CONFIG.md §5).
    let _config = load_config(cli, &location.dir)?;

    let db = project_db_path(&location.dir);
    if !db.is_file() {
        return Err(PipelineError::Uninitialized {
            dir: location.dir.clone(),
        }
        .into());
    }

    let runtime = tokio::runtime::Runtime::new().context("start async runtime")?;
    runtime.block_on(async {
        let store = Store::open(&db)?;
        let Some(project) = store.call(|conn| get_project(conn)).await? else {
            return Err::<i32, anyhow::Error>(
                PipelineError::Uninitialized {
                    dir: location.dir.clone(),
                }
                .into(),
            );
        };

        match command {
            AnalysisCommand::Show { json } => {
                let rendered = analysis_review::show(&store, *json).await?;
                print!("{rendered}");
                Ok(exit::SUCCESS)
            }
            AnalysisCommand::Export { path } => {
                analysis_review::export(&store, path).await?;
                println!("exported current analysis to {}", path.display());
                Ok(exit::SUCCESS)
            }
            AnalysisCommand::Import { path } => {
                // Cues are read during range validation, so the source must be
                // unchanged (docs/ARCHITECTURE.md §6 rule 5).
                check_source_unchanged(&store, Path::new(&project.source_path)).await?;
                // The file read is blocking I/O, so it runs on the blocking pool
                // rather than an async worker (docs/ARCHITECTURE.md §7).
                let read_path = path.clone();
                let content = tokio::task::spawn_blocking(move || {
                    myansub::review::editor::read_file(&read_path)
                })
                .await
                .context("read import file")??;
                let version = analysis_review::import_doc(&store, &content).await?;
                println!(
                    "imported analysis as version {version} (unapproved); \
                     run `myansub analysis approve` to approve it."
                );
                Ok(exit::SUCCESS)
            }
            AnalysisCommand::Edit => {
                check_source_unchanged(&store, Path::new(&project.source_path)).await?;
                let version =
                    analysis_review::edit(&store, EnvEditorLauncher, &location.dir).await?;
                println!(
                    "edited analysis stored as version {version} (unapproved); \
                     run `myansub analysis approve` to approve it."
                );
                Ok(exit::SUCCESS)
            }
            AnalysisCommand::Approve => {
                // Approval AND stale-marking happen in ONE store transaction
                // (ADR-041): a crash or error can never leave an
                // approved-but-not-invalidated state the pipeline would never heal.
                // Orchestrated here, not inside `review::analysis::approve`, to avoid a
                // `review → pipeline` dependency edge (`docs/ARCHITECTURE.md` §3).
                let outcome = myansub::pipeline::approve_and_invalidate(&store, "user").await?;
                println!("approved analysis version {}.", outcome.approved_version);
                // Report the currently-stale count (not just the newly marked), so a
                // recovery re-run still signals pending re-translation (`docs/LLM.md` §7).
                if outcome.stale_unlocked > 0 {
                    println!(
                        "{} translation(s) stale; run `myansub translate --stale` \
                         to re-translate them.",
                        outcome.stale_unlocked
                    );
                }
                Ok(exit::SUCCESS)
            }
        }
    })
}

/// Handles `myansub review list|show|set|unlock|edit|export|import|approve`
/// (docs/CLI.md §6). Resolves the project, opens the store, and dispatches to the
/// translation-review module. The review gate (translation completed) is enforced
/// inside the module and maps to exit 3; a bad id/range/flag is a usage error
/// (exit 2); validation/import rejections and I/O map to exit 1 (see
/// [`exit_code_for`]).
fn run_translation_review_command(cli: &Cli, command: &ReviewCommand) -> anyhow::Result<i32> {
    let cwd = std::env::current_dir().context("determine current directory")?;
    let location = resolve_project(cli.project.as_deref(), None, &cwd)?;
    let config = load_config(cli, &location.dir)?;
    let limits = (&config.quality).into();

    let db = project_db_path(&location.dir);
    if !db.is_file() {
        return Err(PipelineError::Uninitialized {
            dir: location.dir.clone(),
        }
        .into());
    }

    let runtime = tokio::runtime::Runtime::new().context("start async runtime")?;
    runtime.block_on(async {
        let store = Store::open(&db)?;
        let Some(project) = store.call(|conn| get_project(conn)).await? else {
            return Err::<i32, anyhow::Error>(
                PipelineError::Uninitialized {
                    dir: location.dir.clone(),
                }
                .into(),
            );
        };
        let source = Path::new(&project.source_path).to_path_buf();

        match command {
            ReviewCommand::List(args) => {
                let filters = review_filters(args)?;
                let listing = translation_review::list(&store, &filters).await?;
                print_review_listing(&listing);
                Ok(exit::SUCCESS)
            }
            ReviewCommand::Show { id } => {
                let id = parse_cue_id(id)?;
                let rendered = translation_review::show(&store, id).await?;
                print!("{rendered}");
                Ok(exit::SUCCESS)
            }
            ReviewCommand::Set { id, text } => {
                let id = parse_cue_id(id)?;
                // Cues (source placeholders) are read, so the source must be unchanged
                // (docs/ARCHITECTURE.md §6 rule 5).
                check_source_unchanged(&store, &source).await?;
                translation_review::set_target(&store, id, text, &limits).await?;
                println!("set and locked cue {id}.");
                Ok(exit::SUCCESS)
            }
            ReviewCommand::Unlock { id } => {
                let id = parse_cue_id(id)?;
                translation_review::unlock(&store, id).await?;
                println!("unlocked cue {id}.");
                Ok(exit::SUCCESS)
            }
            ReviewCommand::Edit(args) => {
                let selection = review_selection(args)?;
                check_source_unchanged(&store, &source).await?;
                let summary = translation_review::edit(
                    &store,
                    EnvEditorLauncher,
                    &location.dir,
                    &selection,
                    &limits,
                )
                .await?;
                println!(
                    "imported {} changed and {} unchanged cue(s); \
                     run `myansub review approve` to approve the review.",
                    summary.changed, summary.unchanged
                );
                Ok(exit::SUCCESS)
            }
            ReviewCommand::Export { path, selection } => {
                let selection = review_selection(selection)?;
                translation_review::export(&store, path, &selection).await?;
                println!("exported review file to {}", path.display());
                Ok(exit::SUCCESS)
            }
            ReviewCommand::Import { path } => {
                // Cues are read during placeholder validation, so the source must be
                // unchanged (docs/ARCHITECTURE.md §6 rule 5).
                check_source_unchanged(&store, &source).await?;
                // The file read is blocking I/O, so it runs on the blocking pool
                // (docs/ARCHITECTURE.md §7).
                let read_path = path.clone();
                let content = tokio::task::spawn_blocking(move || {
                    myansub::review::editor::read_file(&read_path)
                })
                .await
                .context("read import file")??;
                let summary = translation_review::import(&store, &content, &limits).await?;
                println!(
                    "imported {} changed and {} unchanged cue(s); \
                     run `myansub review approve` to approve the review.",
                    summary.changed, summary.unchanged
                );
                Ok(exit::SUCCESS)
            }
            ReviewCommand::Approve => {
                translation_review::approve(&store).await?;
                println!(
                    "approved the translation review; run `myansub export` to produce output."
                );
                Ok(exit::SUCCESS)
            }
        }
    })
}

/// Handles `myansub export [--output-format F] [-o PATH] [--allow-incomplete]`
/// (docs/CLI.md §6, docs/REQUIREMENTS.md FR-7). Resolves the project, resolves the
/// output format/path by precedence (command flag > stored value > default), and
/// runs the output stage. A format/extension disagreement is a usage error (exit
/// 2); the output gate (translation review approved) and an incomplete translation
/// without `--allow-incomplete` are preconditions (exit 3).
fn run_export_command(cli: &Cli, args: &ExportArgs) -> anyhow::Result<i32> {
    let cwd = std::env::current_dir().context("determine current directory")?;
    let location = resolve_project(cli.project.as_deref(), None, &cwd)?;
    let config = load_config(cli, &location.dir)?;

    let db = project_db_path(&location.dir);
    if !db.is_file() {
        return Err(PipelineError::Uninitialized {
            dir: location.dir.clone(),
        }
        .into());
    }

    let flag_format = args.output.output_format.map(map_output_format);
    let flag_path = args.output.output.clone();

    let runtime = tokio::runtime::Runtime::new().context("start async runtime")?;
    runtime.block_on(async {
        let store = Store::open(&db)?;
        let Some(project) = store.call(|conn| get_project(conn)).await? else {
            return Err::<i32, anyhow::Error>(
                PipelineError::Uninitialized {
                    dir: location.dir.clone(),
                }
                .into(),
            );
        };
        let source_format: SubtitleFormat = project
            .source_format
            .parse()
            .map_err(|e| anyhow::anyhow!("stored source_format is invalid: {e}"))?;
        let source_path = std::path::PathBuf::from(&project.source_path);

        let (stored_format, stored_path) = stored_output_prefs(&store).await?;
        let resolved = resolve_output_target(
            flag_format,
            flag_path.as_deref(),
            stored_format,
            stored_path.as_deref(),
            &source_path,
            source_format,
        )?;

        let report = run_output(
            &store,
            &source_path,
            &resolved,
            &config.output.ass_font_override,
            config.output.utf8_bom,
            args.allow_incomplete,
        )
        .await?;
        report_export(&report);
        Ok(exit::SUCCESS)
    })
}

/// Prints where the export wrote and any warnings it surfaced (docs/CLI.md §6).
fn report_export(report: &OutputReport) {
    for warning in &report.warnings {
        println!("WARNING: {warning}");
    }
    println!(
        "exported {} cue(s) as {} to {}",
        report.cue_count,
        report.format,
        report.path.display()
    );
}

/// Handles `myansub cache prune [--older-than DAYS]` (docs/CLI.md §6, docs/LLM.md
/// §4). Deletes local response-cache entries: `--older-than DAYS` ages them by
/// first-seen `created_at` (M3-C); without the flag every entry is removed. The cache
/// is disposable and spends no tokens, so pruning is not gated by `confirm_above_usd`
/// (docs/CLI.md §5). Prints the number removed.
fn run_cache_command(cli: &Cli, command: &CacheCommand) -> anyhow::Result<i32> {
    let cwd = std::env::current_dir().context("determine current directory")?;
    let location = resolve_project(cli.project.as_deref(), None, &cwd)?;
    // Validate configuration at startup (docs/CONFIG.md §5).
    let _config = load_config(cli, &location.dir)?;

    let db = project_db_path(&location.dir);
    if !db.is_file() {
        return Err(PipelineError::Uninitialized {
            dir: location.dir.clone(),
        }
        .into());
    }

    let runtime = tokio::runtime::Runtime::new().context("start async runtime")?;
    runtime.block_on(async {
        let store = Store::open(&db)?;
        let CacheCommand::Prune { older_than } = command;
        // The `Some(days)` → prune-by-age / `None` → prune-all routing and the day
        // clamp live in `repo_llm::prune`, which is unit-tested directly.
        let older_than = *older_than;
        let removed = store
            .call(move |conn| repo_llm::prune(conn, older_than))
            .await?;
        println!(
            "pruned {removed} cache {}.",
            if removed == 1 { "entry" } else { "entries" }
        );
        Ok::<i32, anyhow::Error>(exit::SUCCESS)
    })
}

/// Maps the CLI output-format choice onto the domain [`SubtitleFormat`].
fn map_output_format(format: OutputFormat) -> SubtitleFormat {
    match format {
        OutputFormat::Srt => SubtitleFormat::Srt,
        OutputFormat::Vtt => SubtitleFormat::Vtt,
        OutputFormat::Ass => SubtitleFormat::Ass,
        OutputFormat::Ssa => SubtitleFormat::Ssa,
        OutputFormat::Sbv => SubtitleFormat::Sbv,
    }
}

/// Builds the [`ReviewFilters`] from `review list` args. A bad `--flag` or `--range`
/// is a typed [`ReviewError`] that maps to the usage exit code (2) via
/// [`exit_code_for`] (docs/CLI.md §7).
fn review_filters(args: &ReviewListArgs) -> Result<ReviewFilters, ReviewError> {
    let flag = args.flag.as_deref().map(parse_flag).transpose()?;
    let range = args.range.as_deref().map(parse_id_range).transpose()?;
    Ok(ReviewFilters {
        suspect: args.suspect,
        failed: args.failed,
        stale: args.stale,
        flag,
        range,
    })
}

/// Builds the [`ReviewSelection`] from `review edit`/`export` args. A bad `--range`
/// is a typed [`ReviewError`] that maps to the usage exit code (2). `--suspect` and
/// `--range` are already mutually exclusive at the clap layer.
fn review_selection(args: &ReviewSelectionArgs) -> Result<ReviewSelection, ReviewError> {
    let range = args.range.as_deref().map(parse_id_range).transpose()?;
    Ok(ReviewSelection {
        suspect: args.suspect,
        range,
    })
}

/// Prints a `review list` result: the matching cues and the project-wide counts
/// (docs/CLI.md §6, docs/REQUIREMENTS.md FR-6).
fn print_review_listing(listing: &myansub::review::translation::ReviewListing) {
    for cue in &listing.cues {
        let flags = if cue.flags.is_empty() {
            String::new()
        } else {
            let names: Vec<String> = cue
                .flags
                .iter()
                .map(|f| {
                    serde_json::to_value(f)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_else(|| format!("{f:?}"))
                })
                .collect();
            format!(" [{}]", names.join(","))
        };
        let lock = if cue.locked { " (locked)" } else { "" };
        let target = cue.target.as_deref().unwrap_or("(not translated)");
        println!("{:>5}  {}  {target}{flags}{lock}", cue.id, cue.time);
    }
    println!(
        "Counts: suspect {} / fixed {} / untouched {}",
        listing.counts.suspect, listing.counts.fixed, listing.counts.untouched
    );
}

/// Builds the real Gemini transport from the environment and config
/// (docs/LLM.md §1). The API key comes from `GEMINI_API_KEY` and is wrapped in a
/// [`Secret`] so it never leaks through logging.
fn build_gemini(config: &Config) -> Result<Gemini, PipelineError> {
    // Detect an absent/empty key before building the transport, so a missing key is
    // a clear, actionable error rather than a confusing 401 from a would-be request.
    let key = std::env::var("GEMINI_API_KEY").unwrap_or_default();
    if key.trim().is_empty() {
        return Err(PipelineError::MissingApiKey);
    }
    Gemini::new(
        config.llm.base_url.clone(),
        Secret::new(key),
        Duration::from_secs(config.llm.timeout_secs),
    )
    .map_err(PipelineError::Llm)
}

/// The config label for a CLI service tier.
fn tier_label(tier: ServiceTier) -> String {
    match tier {
        ServiceTier::Flex => "flex".to_string(),
        ServiceTier::Standard => "standard".to_string(),
    }
}

/// Prints where the analysis stage stopped and the next action (docs/CLI.md §6).
fn report_analysis_outcome(outcome: &AnalysisOutcome) {
    match outcome {
        AnalysisOutcome::Completed { version } => {
            println!(
                "analysis complete; stored version {version}. \
                 Next: `myansub analysis approve` (or edit with `myansub analysis edit`)."
            );
        }
        AnalysisOutcome::NothingToDo => {
            println!("an analysis version already exists; pass `--redo` to produce a new one.");
        }
        AnalysisOutcome::CompletedWithFailures { failed } => {
            println!(
                "analysis finished with {failed} failed chunk(s); \
                 re-run with `--retry-failed` to retry them."
            );
        }
        AnalysisOutcome::Interrupted => {
            println!("interrupted; re-run `myansub analyze` to resume.");
        }
    }
}

/// Prints the `analyze --dry-run` estimate (docs/CLI.md §6, docs/LLM.md §9).
fn print_dry_run_report(report: &DryRunReport) {
    let mode = match report.mode {
        AnalysisModeKind::SinglePass => "single-pass",
        AnalysisModeKind::MapReduce => "map-reduce",
    };
    println!("Analysis dry run ({mode}):");
    if report.mode == AnalysisModeKind::MapReduce {
        println!("  chunks:          {}", report.chunk_count);
    }
    println!("  LLM calls:       {}", report.calls);
    println!(
        "  estimated tokens: input {} / output {}",
        report.estimated_prompt_tokens, report.estimated_output_tokens
    );
    match report.estimated_cost {
        Cost::Known(usd) => println!("  estimated cost:  ${usd:.4}"),
        Cost::Unknown => println!("  estimated cost:  unknown (no price entry for the model)"),
    }
    println!("(dry run: nothing was written to the database.)");
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
    load_config_with(cli, project_dir, CliOverrides::default())
}

/// [`load_config`] with command-specific CLI overrides (e.g. `analyze --model`,
/// `--tier`) applied at the highest precedence (docs/CONFIG.md §1).
fn load_config_with(
    cli: &Cli,
    project_dir: &Path,
    overrides: CliOverrides,
) -> anyhow::Result<Config> {
    let env: Vec<(String, String)> = std::env::vars()
        .filter(|(key, _)| key.starts_with("MYANSUB_"))
        .collect();
    let sources = sources_from_paths(
        global_config_path(),
        Some(project_dir.join("myansub.toml")),
        cli.config.clone(),
        env,
        overrides,
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
        if let Some(e) = cause.downcast_ref::<ReviewError>() {
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
