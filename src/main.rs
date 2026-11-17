//! Binary entry point: `.env` loading, tracing init, clap dispatch, and
//! exit-code mapping (see `docs/CLI.md` §7). `anyhow` is permitted only here.

use std::io::IsTerminal;
use std::path::Path;

use std::time::Duration;

use anyhow::{Context, bail};
use clap::Parser;
use myansub::cli::{
    AnalysisCommand, AnalyzeArgs, AutoApprove, Cli, Command, InitArgs, RunArgs, ServiceTier,
    TranslateArgs,
};
use myansub::config::{self, CliOverrides, Config, global_config_path, sources_from_paths};
use myansub::error::{ConfigError, ExitCode, PipelineError, ProjectError, ReviewError, exit};
use myansub::llm::client::Secret;
use myansub::llm::pricing::{Cost, PriceTable};
use myansub::llm::{Estimator, Gemini, RetryConfig};
use myansub::pipeline::analysis::{
    AnalysisFlags, AnalysisModeKind, AnalysisOutcome, AnalysisParams, DryRunReport,
    analysis_exit_code, dry_run_analysis, parse_thinking, parse_tier, prepare_analysis,
    run_analysis,
};
use myansub::pipeline::input::check_source_unchanged;
use myansub::pipeline::input::{InitOutcome, run_init};
use myansub::pipeline::orchestrator::{
    AutoApproveGate, RunOptions, RunOutcome, RunRequest, StatusReport, run as run_pipeline,
    run_exit_code, status as gather_status,
};
use myansub::pipeline::scheduler::SchedulerConfig;
use myansub::pipeline::translation::{
    DryRunReport as TranslationDryRunReport, TranslationFlags, TranslationOutcome,
    TranslationParams, dry_run_translation, prepare_translation, run_translation,
    translation_exit_code,
};
use myansub::project::{project_db_path, resolve_project};
use myansub::review::EnvEditorLauncher;
use myansub::review::analysis as analysis_review;
use myansub::store::repo_project::get_project;
use myansub::store::{StageStatus, Store};
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

/// Dispatches a parsed command, returning the success-side exit code (0, 130 when
/// interrupted, or 4 when `analyze` finished with failed chunks). Errors are mapped
/// to codes by [`exit_code_for`]. `init`, `run` (up to the analysis boundary),
/// `status`, and `analyze` are implemented; the rest map to a generic error until
/// their milestones land.
fn dispatch(cli: &Cli) -> anyhow::Result<i32> {
    match &cli.command {
        Command::Init(args) => run_init_command(cli, args).map(|()| exit::SUCCESS),
        Command::Run(args) => run_run_command(cli, args),
        Command::Status => run_status_command(cli),
        Command::Analyze(args) => run_analyze_command(cli, args),
        Command::Analysis { command } => run_analysis_review_command(cli, command),
        Command::Translate(args) => run_translate_command(cli, args),
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

        let transport = build_gemini(&config)?;
        let flags = AnalysisFlags {
            redo: args.redo,
            retry_failed: args.retry_failed,
        };
        let outcome = run_analysis(transport, &store, &cues, &params, flags, &token).await?;
        report_analysis_outcome(&outcome);
        Ok(analysis_exit_code(token.is_cancelled(), &outcome))
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

        let transport = build_gemini(&config)?;
        let flags = TranslationFlags {
            redo: args.redo,
            retry_failed: args.retry_failed,
            stale: args.stale,
        };
        let outcome = run_translation(transport, &store, &cues, &params, flags, &token).await?;
        report_translation_outcome(&outcome);
        Ok(translation_exit_code(token.is_cancelled(), &outcome))
    });

    code.map_err(anyhow::Error::new)
}

/// Builds [`TranslationParams`] from the resolved config (docs/CONFIG.md
/// `[translation]`).
fn translation_params(config: &Config) -> TranslationParams {
    TranslationParams {
        model: config.llm.models.translation.clone(),
        tier: parse_tier(&config.llm.service_tier),
        thinking: parse_thinking(&config.llm.thinking.translation),
        batch_size: config.translation.batch_size,
        context_before: config.translation.context_before,
        context_after: config.translation.context_after,
        max_content_attempts: config.translation.max_content_attempts,
        output_ratio: config.translation.output_ratio,
        estimator: Estimator {
            bytes_per_token_latin: config.estimate.bytes_per_token_latin,
            bytes_per_token_myanmar: config.estimate.bytes_per_token_myanmar,
            bytes_per_token_cjk: config.estimate.bytes_per_token_cjk,
        },
        limits: (&config.quality).into(),
        scheduler: SchedulerConfig {
            concurrency: config.translation.concurrency.max(1) as usize,
            warm_cache_first: config.translation.warm_cache_first,
            grace: Duration::from_secs(config.output.grace_secs),
        },
        seed: clock_seed(),
        retry: RetryConfig {
            max_retries: config.llm.max_retries,
            backoff_base_ms: config.llm.backoff_base_ms,
            backoff_max_ms: config.llm.backoff_max_ms,
            max_sheds_before_fallback: config.llm.max_sheds_before_fallback,
            service_tier_fallback: config.llm.service_tier_fallback,
        },
    }
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
                let version = analysis_review::approve(&store).await?;
                println!("approved analysis version {version}.");
                Ok(exit::SUCCESS)
            }
        }
    })
}

/// Builds [`AnalysisParams`] from the resolved config and the project source hash.
fn analysis_params(config: &Config, source_hash: &str) -> AnalysisParams {
    AnalysisParams {
        model: config.llm.models.analysis.clone(),
        tier: parse_tier(&config.llm.service_tier),
        thinking: parse_thinking(&config.llm.thinking.analysis),
        source_digest: source_hash.to_string(),
        max_content_attempts: config.translation.max_content_attempts,
        single_pass_max_tokens: config.analysis.single_pass_max_tokens,
        chunk_cues: config.analysis.chunk_cues,
        estimator: Estimator {
            bytes_per_token_latin: config.estimate.bytes_per_token_latin,
            bytes_per_token_myanmar: config.estimate.bytes_per_token_myanmar,
            bytes_per_token_cjk: config.estimate.bytes_per_token_cjk,
        },
        output_ratio: config.translation.output_ratio,
        scheduler: SchedulerConfig {
            concurrency: config.translation.concurrency.max(1) as usize,
            warm_cache_first: config.translation.warm_cache_first,
            grace: Duration::from_secs(config.output.grace_secs),
        },
        seed: clock_seed(),
        retry: RetryConfig {
            max_retries: config.llm.max_retries,
            backoff_base_ms: config.llm.backoff_base_ms,
            backoff_max_ms: config.llm.backoff_max_ms,
            max_sheds_before_fallback: config.llm.max_sheds_before_fallback,
            service_tier_fallback: config.llm.service_tier_fallback,
        },
    }
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

/// A jitter seed from the system clock (docs/ARCHITECTURE.md §10: a local seed, no
/// `rand` dependency).
fn clock_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
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
