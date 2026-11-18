//! The stage state machine, gates, and the `run`/`status` commands
//! (`docs/ARCHITECTURE.md` §2/§4, `docs/CLI.md` §6).
//!
//! The orchestrator owns stage transitions and gate checks. It reads the project
//! state from the store, decides which stage may run next (the §2 "may run when"
//! table), and drives `run`: it advances through stages until it reaches a gate
//! that is not approved — or a stage whose execution is not yet implemented in
//! this milestone — then stops and names the next action for the user.
//!
//! Division of labor with `pipeline::input`: the input stage is ensured once,
//! before the advance loop, by [`run`] (it creates the project on first use and
//! resumes it otherwise). The loop then advances the whole pipeline with an
//! injected LLM transport (ADR-034): each executable stage (`analysis`,
//! `translation`, `output`) runs its units through the
//! [`scheduler`](super::scheduler) and `continue`s; a review gate is auto-approved
//! when `--auto-approve` names it (origin `auto`, ADR-019) or else stops the run
//! (exit 3); with both gates cleared `run` exports the output and finishes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::domain::SubtitleFormat;
use crate::error::exit;
use crate::llm::client::{LlmClient, ServiceTier};
use crate::llm::dry_run::Estimator;
use crate::llm::pricing::{self, Cost, PriceTable};
use crate::llm::retry::RetryConfig;
use crate::project::project_db_path;
use crate::store::repo_cues::load_cues;
use crate::store::repo_project::{
    get_project, get_stage, get_stage_origin, set_stage, set_stage_with_origin,
};
use crate::store::repo_units::{
    count_analysis_chunks_by_status, count_translation_batches_by_status,
};
use crate::store::{
    Stage, StageStatus, Store, UnitStatus, current_analysis_approved, usage_by_model,
};

use super::analysis::{
    AnalysisFlags, AnalysisOutcome, AnalysisParams, parse_thinking, parse_tier as parse_tier_label,
    run_analysis,
};
use super::error::PipelineError;
use super::input::{check_source_unchanged, run_input};
use super::output::{ResolvedOutput, resolve_output_target, run_output, stored_output_prefs};
use super::scheduler::SchedulerConfig;
use super::translation::{
    TranslationFlags, TranslationOutcome, TranslationParams, run_translation,
};

// ---------------------------------------------------------------------------
// Stage state machine
// ---------------------------------------------------------------------------

/// A snapshot of the project's stage state, enough to evaluate every gate in the
/// §2 "may run when" table (`docs/ARCHITECTURE.md` §2). Pure data, so the gate and
/// planner functions over it are testable without a database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectState {
    pub input: StageStatus,
    pub analysis: StageStatus,
    pub analysis_review: StageStatus,
    pub translation: StageStatus,
    pub translation_review: StageStatus,
    pub output: StageStatus,
    /// Whether the current (highest-version) analysis is approved — the signal the
    /// translation gate checks. Editing the analysis creates a new unapproved
    /// version, which flips this back to `false` and re-blocks translation
    /// (`docs/ARCHITECTURE.md` §2).
    pub analysis_approved: bool,
    /// Number of translation batches still `pending`; a non-zero count means
    /// translation is not complete, so the translation-review gate stays shut.
    pub translation_pending: usize,
}

impl ProjectState {
    /// A fresh project with every stage `not_started` (used as the default before
    /// any stage row exists, and as a test fixture).
    pub fn empty() -> Self {
        Self {
            input: StageStatus::NotStarted,
            analysis: StageStatus::NotStarted,
            analysis_review: StageStatus::NotStarted,
            translation: StageStatus::NotStarted,
            translation_review: StageStatus::NotStarted,
            output: StageStatus::NotStarted,
            analysis_approved: false,
            translation_pending: 0,
        }
    }
}

/// Checks whether `stage` may run given the current `state`, implementing the §2
/// "may run when" table exactly (`docs/ARCHITECTURE.md` §2).
///
/// On a violated precondition this returns [`PipelineError::GateViolation`] whose
/// message names the exact next action the user must take; it maps to exit code 3
/// (`docs/CLI.md` §7, M4-AC2).
pub fn can_run(stage: Stage, state: &ProjectState) -> Result<(), PipelineError> {
    let satisfied = match stage {
        // `input` runs on `init`/`run` with no precondition.
        Stage::Input => true,
        Stage::Analysis => state.input == StageStatus::Completed,
        Stage::AnalysisReview => state.analysis == StageStatus::Completed,
        Stage::Translation => state.analysis_approved,
        Stage::TranslationReview => {
            state.translation == StageStatus::Completed && state.translation_pending == 0
        }
        Stage::Output => state.translation_review == StageStatus::Approved,
    };
    if satisfied {
        Ok(())
    } else {
        Err(PipelineError::GateViolation {
            message: gate_message(stage),
        })
    }
}

/// The helpful message for a blocked `stage`, naming the exact command that clears
/// the gate (`docs/ARCHITECTURE.md` §2, M4-AC2).
fn gate_message(stage: Stage) -> String {
    match stage {
        Stage::Input => {
            "the input stage requires a source file; run `myansub run <FILE> --from <LANG>`".into()
        }
        Stage::Analysis => "analysis requires the input stage to be completed first; \
             initialize the project with `myansub run <FILE> --from <LANG>`"
            .into(),
        Stage::AnalysisReview => {
            "approving the analysis requires analysis to be completed first; run `myansub analyze`"
                .into()
        }
        Stage::Translation => {
            "translation requires an approved analysis version; run `myansub analysis approve`"
                .into()
        }
        Stage::TranslationReview => {
            "approving the translation requires translation to be completed first; \
             run `myansub translate`"
                .into()
        }
        Stage::Output => "exporting the output requires the translation review to be approved; \
             run `myansub review approve`"
            .into(),
    }
}

/// What the `run` loop should do next, given the current project state
/// (`docs/ARCHITECTURE.md` §2). The planner walks the stages in order and returns
/// the first one that is not satisfied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextStep {
    /// An executable (non-review) stage is ready to run.
    Execute(Stage),
    /// A review gate is blocking; the string is the next action for the user.
    Approve(Stage, String),
    /// Nothing is left to do: the output has been produced.
    Done,
}

/// Decides the next step for `run` (`docs/ARCHITECTURE.md` §2). Advances through
/// the stages, returning the first executable stage that has not completed or the
/// first review gate that has not been cleared.
pub fn next_step(state: &ProjectState) -> NextStep {
    if state.input != StageStatus::Completed {
        return NextStep::Execute(Stage::Input);
    }
    if state.analysis != StageStatus::Completed {
        return NextStep::Execute(Stage::Analysis);
    }
    if !state.analysis_approved {
        return NextStep::Approve(Stage::AnalysisReview, gate_message(Stage::Translation));
    }
    if state.translation != StageStatus::Completed || state.translation_pending > 0 {
        return NextStep::Execute(Stage::Translation);
    }
    if state.translation_review != StageStatus::Approved {
        return NextStep::Approve(Stage::TranslationReview, gate_message(Stage::Output));
    }
    if state.output != StageStatus::Completed {
        return NextStep::Execute(Stage::Output);
    }
    NextStep::Done
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

/// Which review gate(s) `run` should auto-approve when reached (`docs/CLI.md` §4).
///
/// Mirrors `cli::AutoApprove` without depending on `cli` (the dependency direction
/// forbids `pipeline → cli`, `docs/ARCHITECTURE.md` §3). In M4 these are threaded
/// through but take effect only once the review stages execute (M5+).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoApproveGate {
    Analysis,
    Translation,
    All,
}

/// Flags that shape a `run` invocation (`docs/CLI.md` §6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunOptions {
    /// Gate(s) to approve automatically when reached (takes effect M5+).
    pub auto_approve: Option<AutoApproveGate>,
    /// Permit auto-approving/exporting with failed units (takes effect M6/M7).
    pub allow_incomplete: bool,
    /// Run without spending tokens or writing results (takes effect M5+).
    pub dry_run: bool,
}

/// Everything `run` needs to create-or-resume and advance a project
/// (`docs/CLI.md` §2/§6).
#[derive(Debug, Clone)]
pub struct RunRequest<'a> {
    /// The resolved project directory.
    pub project_dir: &'a Path,
    /// The source file given on the command line (`run [FILE]`), if any.
    pub source_path: Option<&'a Path>,
    /// `--from <LANG>`, if given.
    pub from_lang: Option<&'a str>,
    /// `--encoding <ENC>`, if given.
    pub encoding: Option<&'a str>,
    /// `--output-format <FMT>`, if given — stored when this invocation creates the
    /// project (`docs/CLI.md` §3). The caller has already resolved any
    /// `--output-format`/`--output`-extension disagreement (exit 2).
    pub output_format: Option<SubtitleFormat>,
    /// `--output <PATH>`, if given — stored when this invocation creates the
    /// project (`docs/CLI.md` §3).
    pub output_path: Option<&'a Path>,
    /// `run` flags.
    pub options: RunOptions,
}

/// Side-information `run` surfaces to the caller for printing, kept out of
/// [`RunOutcome`] so the outcome stays a small comparable enum (`docs/CLI.md`
/// §4/§6, FR-9). The caller passes a mutable [`RunReport`] and prints from it after
/// `run` returns.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunReport {
    /// When `run` auto-approved the translation gate, the number of cues carrying at
    /// least one quality flag (suspect) — printed but *not* blocking (`docs/CLI.md`
    /// §4). `None` when the translation gate was not auto-approved this run.
    pub suspect_auto_approved: Option<usize>,
    /// The `--dry-run` cost/token estimate, summed over the stages that were
    /// estimated (`docs/LLM.md` §9, FR-9). `None` on a non-dry run.
    pub dry_run: Option<RunDryRunEstimate>,
}

/// The `run --dry-run` estimate, summed across the estimated stages
/// (`docs/LLM.md` §9).
#[derive(Debug, Clone, PartialEq)]
pub struct RunDryRunEstimate {
    pub prompt_tokens: u64,
    pub output_tokens: u64,
    pub cost: Cost,
}

/// Where `run` stopped (`docs/CLI.md` §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// Stopped at an executable stage that could not be advanced (e.g. the input
    /// stage is somehow not completed). `next_action` names the command to run.
    /// Normal runs no longer stop here — analysis/translation/output all execute
    /// inside `run` (M7-C, ADR-034) — but the variant is kept as a safety stop.
    StoppedAtStage { stage: Stage, next_action: String },
    /// Stopped at a review gate that is neither approved nor auto-approved (or a
    /// translation gate that `run` refused to auto-approve because a batch failed
    /// and `--allow-incomplete` was absent). `next_action` names the command that
    /// clears it. Maps to exit 3 (`docs/CLI.md` §7, ADR-031).
    StoppedAtGate { stage: Stage, next_action: String },
    /// Interrupted by cancellation (Ctrl-C). Maps to exit 130.
    Interrupted,
    /// The whole pipeline ran and the output file was written, with no failed units.
    /// Maps to exit 0.
    Completed,
    /// The pipeline ran to completion (output written) but some units failed and
    /// their source text was used (`--allow-incomplete`), or an executable stage
    /// finished with failures that stopped the advance. `failed` is the count.
    /// Maps to exit 4 (`docs/CLI.md` §7).
    CompletedWithFailures { failed: usize },
    /// A `--dry-run` finished: the stages were estimated and nothing was written.
    /// Maps to exit 0.
    DryRun,
}

/// Creates-or-resumes the project and advances it through the whole pipeline,
/// executing each stage with the injected `transport` until it reaches a gate it
/// cannot clear — or finishes by exporting the output (`docs/CLI.md` §6,
/// `docs/ARCHITECTURE.md` §2/§4, ADR-034).
///
/// `transport` is the bottom of the LLM layer stack (Gemini for a real run, Fake in
/// tests); `run_analysis`/`run_translation` wrap it in `Metered<Cached<Retrying<_>>>`
/// per call. This is the transport-injection seam that keeps `run` tests offline
/// (ADR-034): tests pass a Fake-backed transport, `main` the real Gemini one.
///
/// The loop advances `next_step`: an executable stage runs its units and `continue`s;
/// an unapproved review gate is auto-approved when `--auto-approve` names it (origin
/// `auto`, ADR-019), else `run` stops (exit 3). Translation auto-approval is refused
/// (exit 3) when a batch failed unless `--allow-incomplete`. With both gates cleared
/// `run` exports the output and finishes.
///
/// The `token` is honored cooperatively: cancellation between stages yields
/// [`RunOutcome::Interrupted`], and the same token is handed to each stage's scheduler
/// so an in-flight stage stops within its grace window (`docs/ARCHITECTURE.md` §6
/// rule 6). A `--dry-run` estimates the stages, writes nothing, and returns
/// [`RunOutcome::DryRun`].
///
/// `report` is an out-parameter the caller prints from (`docs/CLI.md` §4/§6, FR-9):
/// the suspect-cue count when the translation gate is auto-approved, and the
/// `--dry-run` token/cost estimate. It is left at [`RunReport::default`] (all `None`)
/// when neither applies.
pub async fn run<T>(
    transport: T,
    req: RunRequest<'_>,
    config: &Config,
    token: &CancellationToken,
    report: &mut RunReport,
) -> Result<RunOutcome, PipelineError>
where
    T: LlmClient + Clone + Send + Sync + 'static,
{
    tracing::debug!(
        auto_approve = ?req.options.auto_approve,
        allow_incomplete = req.options.allow_incomplete,
        dry_run = req.options.dry_run,
        "run: advancing the pipeline"
    );

    let store = prepare_project(&req).await?;
    // Crash recovery: reset any in-flight units before advancing
    // (`docs/ARCHITECTURE.md` §6 rule 2). A no-op on a fresh project.
    store.recover().await?;

    // The source path + hash drive the analysis version provenance and the export
    // source-hash re-check; both come from the project row that `prepare_project`
    // guaranteed.
    let project = store.call(|conn| get_project(conn)).await?.ok_or_else(|| {
        PipelineError::Uninitialized {
            dir: req.project_dir.to_path_buf(),
        }
    })?;
    let source_path = resume_source(&req, &project);
    let source_hash = project.source_hash.clone();

    if req.options.dry_run {
        return dry_run_pipeline(&store, config, &source_hash, report).await;
    }

    loop {
        if token.is_cancelled() {
            return Ok(RunOutcome::Interrupted);
        }

        let state = load_state(&store).await?;
        match next_step(&state) {
            NextStep::Execute(Stage::Analysis) => {
                let cues = store.call(|conn| load_cues(conn)).await?;
                let params = analysis_params(config, &source_hash);
                let outcome = run_analysis(
                    transport.clone(),
                    &store,
                    &cues,
                    &params,
                    AnalysisFlags::default(),
                    token,
                )
                .await?;
                match outcome {
                    AnalysisOutcome::Completed { .. } | AnalysisOutcome::NothingToDo => continue,
                    AnalysisOutcome::CompletedWithFailures { failed } => {
                        // No version was stored, so the pipeline cannot advance;
                        // stop rather than loop (exit 4).
                        return Ok(RunOutcome::CompletedWithFailures { failed });
                    }
                    AnalysisOutcome::Interrupted => return Ok(RunOutcome::Interrupted),
                }
            }
            NextStep::Execute(Stage::Translation) => {
                let cues = store.call(|conn| load_cues(conn)).await?;
                let params = translation_params(config);
                let outcome = run_translation(
                    transport.clone(),
                    &store,
                    &cues,
                    &params,
                    TranslationFlags::default(),
                    token,
                )
                .await?;
                match outcome {
                    TranslationOutcome::Completed { .. } => continue,
                    TranslationOutcome::NothingToDo => {
                        // A project with no translatable cues: translation trivially
                        // completes. Mark the stage completed so the review gate can
                        // open rather than looping forever (zero-translatable edge).
                        store
                            .call(|conn| {
                                set_stage(conn, Stage::Translation, StageStatus::Completed)
                            })
                            .await?;
                        continue;
                    }
                    TranslationOutcome::CompletedWithFailures { failed, incomplete } => {
                        if incomplete > 0 {
                            // The stage did not complete, so the review gate can never
                            // open; stop rather than loop (exit 4).
                            return Ok(RunOutcome::CompletedWithFailures {
                                failed: failed + incomplete,
                            });
                        }
                        // Failed-only: the stage completed (failed batches are allowed,
                        // ADR-038); advance to the review gate.
                        continue;
                    }
                    TranslationOutcome::Interrupted => return Ok(RunOutcome::Interrupted),
                }
            }
            NextStep::Execute(Stage::Output) => {
                return export_output(&store, &source_path, &project, config, &req).await;
            }
            // `input` is ensured by `prepare_project`; any other executable stage is
            // a safety stop (it should not occur in a normal run).
            NextStep::Execute(stage) => {
                return Ok(RunOutcome::StoppedAtStage {
                    stage,
                    next_action: boundary_action(stage),
                });
            }
            NextStep::Approve(Stage::AnalysisReview, next_action) => {
                if auto_approves(req.options.auto_approve, Stage::AnalysisReview) {
                    auto_approve_analysis(&store).await?;
                    continue;
                }
                return Ok(RunOutcome::StoppedAtGate {
                    stage: Stage::AnalysisReview,
                    next_action,
                });
            }
            NextStep::Approve(Stage::TranslationReview, next_action) => {
                if auto_approves(req.options.auto_approve, Stage::TranslationReview) {
                    let failed = store
                        .call(|conn| count_translation_batches_by_status(conn, UnitStatus::Failed))
                        .await?;
                    // Auto-approving translation is refused when a batch failed, unless
                    // `--allow-incomplete` (`docs/CLI.md` §4, ADR-019). Suspect cues do
                    // not block it.
                    if failed > 0 && !req.options.allow_incomplete {
                        return Ok(RunOutcome::StoppedAtGate {
                            stage: Stage::TranslationReview,
                            next_action: format!(
                                "{failed} batch(es) failed; re-run with `--allow-incomplete` \
                                 to auto-approve and export using the source text, or fix them \
                                 with `myansub translate --retry-failed`"
                            ),
                        });
                    }
                    // Suspect cues do not block auto-approval, but their count is
                    // surfaced for the user (`docs/CLI.md` §4). Reported via the
                    // out-param so the caller can print it.
                    report.suspect_auto_approved = Some(count_suspect_cues(&store).await?);
                    auto_approve_translation(&store).await?;
                    continue;
                }
                return Ok(RunOutcome::StoppedAtGate {
                    stage: Stage::TranslationReview,
                    next_action,
                });
            }
            // Only the two review stages are returned via `Approve`.
            NextStep::Approve(stage, next_action) => {
                return Ok(RunOutcome::StoppedAtGate { stage, next_action });
            }
            NextStep::Done => return Ok(RunOutcome::Completed),
        }
    }
}

/// Whether `--auto-approve` clears `gate` (`docs/CLI.md` §4).
fn auto_approves(choice: Option<AutoApproveGate>, gate: Stage) -> bool {
    matches!(
        (choice, gate),
        (Some(AutoApproveGate::All), _)
            | (Some(AutoApproveGate::Analysis), Stage::AnalysisReview)
            | (Some(AutoApproveGate::Translation), Stage::TranslationReview)
    )
}

/// Auto-approves the current analysis version and the analysis-review gate, and
/// invalidates the affected unlocked translations, **all in one transaction**,
/// recording origin `auto` (`docs/CLI.md` §4, ADR-019/ADR-041). Approval and
/// stale-marking are atomic so a crash can never leave an approved-but-not-invalidated
/// state the `run` loop would skip (`docs/LLM.md` §7). A no-op invalidation before any
/// translation exists.
async fn auto_approve_analysis(store: &Store) -> Result<(), PipelineError> {
    crate::pipeline::invalidation::approve_and_invalidate(store, "auto").await?;
    Ok(())
}

/// Auto-approves the translation-review gate, recording origin `auto`
/// (`docs/CLI.md` §4, ADR-019).
async fn auto_approve_translation(store: &Store) -> Result<(), PipelineError> {
    store
        .call(|conn| {
            let tx = conn.transaction()?;
            set_stage_with_origin(
                &tx,
                Stage::TranslationReview,
                StageStatus::Approved,
                Some("auto"),
            )?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    Ok(())
}

/// Exports the output as the final `run` step and classifies the result
/// (`docs/CLI.md` §6, `docs/REQUIREMENTS.md` FR-7). Resolves the target by the same
/// precedence as `export` (command flag > stored value > default) and threads
/// `--allow-incomplete`. A completed export with failed/missing cues finishes as
/// [`RunOutcome::CompletedWithFailures`] (exit 4); otherwise [`RunOutcome::Completed`]
/// (exit 0).
async fn export_output(
    store: &Store,
    source_path: &Path,
    project: &crate::store::Project,
    config: &Config,
    req: &RunRequest<'_>,
) -> Result<RunOutcome, PipelineError> {
    let source_format = project
        .source_format
        .parse::<SubtitleFormat>()
        .map_err(|e| {
            PipelineError::Store(crate::store::StoreError::Unexpected(format!(
                "stored source_format: {e}"
            )))
        })?;
    let (stored_format, stored_path) = stored_output_prefs(store).await?;
    let resolved: ResolvedOutput = resolve_output_target(
        req.output_format,
        req.output_path,
        stored_format,
        stored_path.as_deref(),
        source_path,
        source_format,
    )?;

    // Count failed/missing translatable cues before the export so the final exit code
    // reflects them (exit 4). A failed batch leaves its cues untranslated.
    let failed = store
        .call(|conn| count_translation_batches_by_status(conn, UnitStatus::Failed))
        .await?;

    run_output(
        store,
        source_path,
        &resolved,
        &config.output.ass_font_override,
        config.output.utf8_bom,
        req.options.allow_incomplete,
    )
    .await?;

    if failed > 0 {
        Ok(RunOutcome::CompletedWithFailures { failed })
    } else {
        Ok(RunOutcome::Completed)
    }
}

/// Estimates the analysis (and, when an approved analysis exists, the translation)
/// stages without writing anything (`docs/LLM.md` §9, ADR-033). Sums the per-stage
/// token/cost estimates into `report.dry_run` so the caller can print the cost
/// preview (FR-9); returns [`RunOutcome::DryRun`].
async fn dry_run_pipeline(
    store: &Store,
    config: &Config,
    source_hash: &str,
    report: &mut RunReport,
) -> Result<RunOutcome, PipelineError> {
    let cues = store.call(|conn| load_cues(conn)).await?;
    let prices = PriceTable::from_config(&config.pricing);

    let analysis_params = analysis_params(config, source_hash);
    let analysis =
        super::analysis::dry_run_analysis(store, &cues, &analysis_params, &prices).await?;

    let mut prompt_tokens = analysis.estimated_prompt_tokens;
    let mut output_tokens = analysis.estimated_output_tokens;
    let mut cost = analysis.estimated_cost;

    // Translation can only be estimated once an approved analysis exists (it needs
    // the analysis sheet for the cached prefix); skip it otherwise.
    if current_analysis_approved_in(store).await? {
        let translation_params = translation_params(config);
        let translation =
            super::translation::dry_run_translation(store, &cues, &translation_params, &prices)
                .await?;
        prompt_tokens += translation.estimated_prompt_tokens;
        output_tokens += translation.estimated_output_tokens;
        cost = add_cost(cost, translation.estimated_cost);
    }

    report.dry_run = Some(RunDryRunEstimate {
        prompt_tokens,
        output_tokens,
        cost,
    });
    Ok(RunOutcome::DryRun)
}

/// Sums two [`Cost`]s; an unknown component makes the total unknown
/// (`docs/LLM.md` §9).
fn add_cost(acc: Cost, next: Cost) -> Cost {
    match (acc, next) {
        (Cost::Known(a), Cost::Known(b)) => Cost::Known(a + b),
        _ => Cost::Unknown,
    }
}

/// Whether the current analysis version is approved (one short read).
async fn current_analysis_approved_in(store: &Store) -> Result<bool, PipelineError> {
    Ok(store.call(|conn| current_analysis_approved(conn)).await?)
}

/// Counts cues carrying at least one quality flag (suspect cues, `docs/CLI.md` §4).
async fn count_suspect_cues(store: &Store) -> Result<usize, PipelineError> {
    let count = store
        .call(|conn| {
            let rows = crate::store::repo_translations::list_translations(conn)?;
            Ok(rows.iter().filter(|r| !r.flags.is_empty()).count())
        })
        .await?;
    Ok(count)
}

/// The command that advances past an executable `stage` boundary `run` does not
/// itself drive (a safety message; normal runs do not reach it).
fn boundary_action(stage: Stage) -> String {
    match stage {
        Stage::Input => "run `myansub run <FILE> --from <LANG>`".into(),
        Stage::Analysis => "run `myansub analyze`".into(),
        Stage::Translation => "run `myansub translate`".into(),
        Stage::Output => "run `myansub export`".into(),
        // Review stages are returned via `NextStep::Approve`, not `Execute`.
        Stage::AnalysisReview => "run `myansub analysis approve`".into(),
        Stage::TranslationReview => "run `myansub review approve`".into(),
    }
}

/// Builds [`AnalysisParams`] from the resolved config and the project source hash
/// (`docs/CONFIG.md`). Shared by `run` and `main`'s `analyze` handler so the two
/// agree on how config maps to params.
pub fn analysis_params(config: &Config, source_hash: &str) -> AnalysisParams {
    AnalysisParams {
        model: config.llm.models.analysis.clone(),
        tier: parse_tier_label(&config.llm.service_tier),
        thinking: parse_thinking(&config.llm.thinking.analysis),
        source_digest: source_hash.to_string(),
        max_content_attempts: config.translation.max_content_attempts,
        single_pass_max_tokens: config.analysis.single_pass_max_tokens,
        chunk_cues: config.analysis.chunk_cues,
        estimator: estimator_from(config),
        output_ratio: config.translation.output_ratio,
        scheduler: scheduler_from(config),
        seed: clock_seed(),
        retry: retry_from(config),
    }
}

/// Builds [`TranslationParams`] from the resolved config (`docs/CONFIG.md`
/// `[translation]`). Shared by `run` and `main`'s `translate` handler.
pub fn translation_params(config: &Config) -> TranslationParams {
    TranslationParams {
        model: config.llm.models.translation.clone(),
        tier: parse_tier_label(&config.llm.service_tier),
        thinking: parse_thinking(&config.llm.thinking.translation),
        batch_size: config.translation.batch_size,
        context_before: config.translation.context_before,
        context_after: config.translation.context_after,
        max_content_attempts: config.translation.max_content_attempts,
        output_ratio: config.translation.output_ratio,
        estimator: estimator_from(config),
        limits: (&config.quality).into(),
        scheduler: scheduler_from(config),
        seed: clock_seed(),
        retry: retry_from(config),
    }
}

fn estimator_from(config: &Config) -> Estimator {
    Estimator {
        bytes_per_token_latin: config.estimate.bytes_per_token_latin,
        bytes_per_token_myanmar: config.estimate.bytes_per_token_myanmar,
        bytes_per_token_cjk: config.estimate.bytes_per_token_cjk,
    }
}

fn scheduler_from(config: &Config) -> SchedulerConfig {
    SchedulerConfig {
        concurrency: config.translation.concurrency.max(1) as usize,
        warm_cache_first: config.translation.warm_cache_first,
        grace: Duration::from_secs(config.output.grace_secs),
    }
}

fn retry_from(config: &Config) -> RetryConfig {
    RetryConfig {
        max_retries: config.llm.max_retries,
        backoff_base_ms: config.llm.backoff_base_ms,
        backoff_max_ms: config.llm.backoff_max_ms,
        max_sheds_before_fallback: config.llm.max_sheds_before_fallback,
        service_tier_fallback: config.llm.service_tier_fallback,
    }
}

/// A jitter seed from the system clock (`docs/ARCHITECTURE.md` §10: a local seed, no
/// `rand` dependency).
fn clock_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
}

/// Maps a `run` result to its process exit code, preferring the interrupt code
/// whenever cancellation was observed (`docs/ARCHITECTURE.md` §6 rule 6,
/// `docs/CLI.md` §7, ADR-031). This is the robust rule the scheduler's
/// `Summary::exit_code` documents: trust `token.is_cancelled()` over the outcome
/// for the interrupt.
///
/// - [`RunOutcome::Completed`] (all gates passed, output written) → `0`.
/// - [`RunOutcome::DryRun`] (estimated, nothing written) → `0`.
/// - [`RunOutcome::StoppedAtGate`] (a review gate needs approval; the next action
///   was printed) → `3`: this is a precondition/action-needed state, listed under
///   "gate not approved" in `docs/CLI.md` §7.
/// - [`RunOutcome::CompletedWithFailures`] (pipeline finished with failed units) →
///   `4` (`docs/CLI.md` §7).
/// - [`RunOutcome::StoppedAtStage`] denotes an unexpected, un-advanceable state (it
///   should not occur in a normal run); it maps to `3` (precondition/anomaly) rather
///   than `0` so the anomaly is never masked as success.
pub fn run_exit_code(cancelled: bool, outcome: &RunOutcome) -> i32 {
    if cancelled {
        return exit::INTERRUPTED;
    }
    match outcome {
        RunOutcome::Interrupted => exit::INTERRUPTED,
        // Stopping at an unapproved review gate is a precondition/action-needed
        // state (`docs/CLI.md` §7, ADR-031). The safety boundary stop
        // (`StoppedAtStage`) is an unexpected state and also maps to a precondition
        // code so it is never reported as success.
        RunOutcome::StoppedAtGate { .. } | RunOutcome::StoppedAtStage { .. } => exit::PRECONDITION,
        // A completed run with failed units reports exit 4 (`docs/CLI.md` §7).
        RunOutcome::CompletedWithFailures { .. } => exit::FAILED_UNITS,
        // A completed run and a dry run are both success.
        RunOutcome::Completed | RunOutcome::DryRun => exit::SUCCESS,
    }
}

/// Opens the project, creating it on first use and validating it on resume
/// (`docs/CLI.md` §2). Guarantees the input stage is completed on return.
async fn prepare_project(req: &RunRequest<'_>) -> Result<Store, PipelineError> {
    let db = project_db_path(req.project_dir);

    // A missing `project.db` means a fresh create.
    if !db.is_file() {
        return create_project(req, &db).await;
    }

    let store = Store::open(&db)?;
    match store.call(|conn| get_project(conn)).await? {
        // A `project.db` with no project row is uninitialized (`docs/CLI.md` §2).
        None => {
            initialize_into(req, &store).await?;
            Ok(store)
        }
        Some(project) => {
            validate_resume(req, &project)?;
            let source = resume_source(req, &project);
            check_source_unchanged(&store, &source).await?;
            Ok(store)
        }
    }
}

/// Creates a brand-new project directory and runs the input stage.
async fn create_project(req: &RunRequest<'_>, db: &Path) -> Result<Store, PipelineError> {
    std::fs::create_dir_all(req.project_dir).map_err(|source| PipelineError::Io {
        path: req.project_dir.to_path_buf(),
        source,
    })?;
    let store = Store::open(db)?;
    initialize_into(req, &store).await?;
    Ok(store)
}

/// Runs the input stage into `store`, requiring a source file and language.
async fn initialize_into(req: &RunRequest<'_>, store: &Store) -> Result<(), PipelineError> {
    let source = req
        .source_path
        .ok_or_else(|| PipelineError::Uninitialized {
            dir: req.project_dir.to_path_buf(),
        })?;
    let from = req.from_lang.ok_or(PipelineError::MissingFromLanguage)?;
    let ext = extension_of(source);
    // Output preferences given at creation are stored atomically with the project
    // (`docs/CLI.md` §3). The format label is persisted; the path is stored as given.
    let output_format = req.output_format.map(|f| f.label());
    let output_path = req.output_path.map(|p| p.to_string_lossy());
    run_input(
        store,
        source,
        from,
        req.encoding,
        ext,
        output_format,
        output_path.as_deref(),
    )
    .await?;
    Ok(())
}

/// Validates `--from` against the stored source language on resume
/// (`docs/CLI.md` §2): a disagreement is a precondition failure (exit 3).
fn validate_resume(
    req: &RunRequest<'_>,
    project: &crate::store::Project,
) -> Result<(), PipelineError> {
    if let Some(from) = req.from_lang
        && from != project.source_lang
    {
        return Err(PipelineError::FromDisagreement {
            stored: project.source_lang.clone(),
            given: from.to_string(),
        });
    }
    Ok(())
}

/// The source path to hash-check on resume: the one given on the command line, or
/// the stored source path when `run` was invoked without a `FILE`.
fn resume_source(req: &RunRequest<'_>, project: &crate::store::Project) -> PathBuf {
    req.source_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(&project.source_path))
}

/// The source file extension (without the leading dot), if any.
fn extension_of(source: &Path) -> Option<&str> {
    source.extension().and_then(|e| e.to_str())
}

/// Reads the full [`ProjectState`] from the store in one short closure. Public so
/// command handlers (e.g. `analyze`) can evaluate a stage gate with [`can_run`]
/// without duplicating the per-stage reads.
pub async fn load_state(store: &Store) -> Result<ProjectState, PipelineError> {
    let state = store
        .call(|conn| {
            let stage = |s: Stage| -> Result<StageStatus, crate::store::StoreError> {
                Ok(get_stage(conn, s)?.unwrap_or(StageStatus::NotStarted))
            };
            Ok(ProjectState {
                input: stage(Stage::Input)?,
                analysis: stage(Stage::Analysis)?,
                analysis_review: stage(Stage::AnalysisReview)?,
                translation: stage(Stage::Translation)?,
                translation_review: stage(Stage::TranslationReview)?,
                output: stage(Stage::Output)?,
                analysis_approved: current_analysis_approved(conn)?,
                translation_pending: count_translation_batches_by_status(
                    conn,
                    UnitStatus::Pending,
                )?,
            })
        })
        .await?;
    Ok(state)
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

/// How a stage approval was obtained (`docs/CLI.md` §4/§6, ADR-019): `user` when a
/// human ran the approve command, `auto` when `run --auto-approve` cleared the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOrigin {
    User,
    Auto,
}

impl ApprovalOrigin {
    /// Parses the stored `approved_origin` label, if recognized.
    fn from_label(label: &str) -> Option<ApprovalOrigin> {
        match label {
            "user" => Some(ApprovalOrigin::User),
            "auto" => Some(ApprovalOrigin::Auto),
            _ => None,
        }
    }
}

/// One stage's line in the `status` report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageLine {
    pub stage: Stage,
    pub status: StageStatus,
    pub approval_origin: Option<ApprovalOrigin>,
}

/// Unit progress for one unit table (`docs/CLI.md` §6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UnitCounts {
    pub pending: usize,
    pub in_flight: usize,
    pub done: usize,
    pub failed: usize,
}

/// Summed token usage across all `llm_calls` (`docs/DATA_MODEL.md` §2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageTotals {
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub thinking_tokens: u64,
}

/// The `status` report (`docs/CLI.md` §6, `docs/REQUIREMENTS.md` FR-10).
#[derive(Debug, Clone, PartialEq)]
pub struct StatusReport {
    /// Stage states (in pipeline order) with approval origin.
    pub stages: Vec<StageLine>,
    /// Analysis-chunk progress.
    pub analysis_chunks: UnitCounts,
    /// Translation-batch progress.
    pub translation_batches: UnitCounts,
    /// Quality-flag counts (empty until translation runs, M6).
    pub flag_counts: BTreeMap<String, usize>,
    /// Summed token usage.
    pub usage: UsageTotals,
    /// Estimated cost; [`Cost::Known`] summed over priced models.
    pub cost: Cost,
    /// Models whose cost could not be priced (no entry); their usage is excluded
    /// from [`StatusReport::cost`] (`docs/LLM.md` §9).
    pub unknown_cost_models: Vec<String>,
    /// Whether the source file changed since `init` (reported, not fatal, because
    /// `status` is read-only, `docs/REQUIREMENTS.md` FR-10).
    pub source_changed: bool,
}

/// Gathers the `status` report for the project at `project_dir`
/// (`docs/CLI.md` §6). Returns [`PipelineError::Uninitialized`] (exit 3) when the
/// directory has no initialized project — including a `project.db` with no project
/// row — rather than crashing (`docs/CLI.md` §2).
pub async fn status(project_dir: &Path, config: &Config) -> Result<StatusReport, PipelineError> {
    let db = project_db_path(project_dir);
    if !db.is_file() {
        return Err(PipelineError::Uninitialized {
            dir: project_dir.to_path_buf(),
        });
    }

    let store = Store::open(&db)?;
    let Some(project) = store.call(|conn| get_project(conn)).await? else {
        return Err(PipelineError::Uninitialized {
            dir: project_dir.to_path_buf(),
        });
    };

    // Source-hash check (`docs/ARCHITECTURE.md` §6 rule 5). `status` is read-only,
    // so it is never fatal: only a confirmed *change* is reported. Any other
    // outcome — including an unreadable/missing source (an I/O error) — is ignored
    // and leaves `source_changed = false`.
    let source_changed = matches!(
        check_source_unchanged(&store, Path::new(&project.source_path)).await,
        Err(PipelineError::SourceChanged { .. })
    );

    let (stages, analysis_chunks, translation_batches, usage_rows) = store
        .call(|conn| {
            let mut stages = Vec::with_capacity(6);
            for stage in STAGE_ORDER {
                let status = get_stage(conn, stage)?.unwrap_or(StageStatus::NotStarted);
                // The approval origin is only meaningful for an approved review gate
                // (`docs/CLI.md` §4, ADR-019).
                let approval_origin = if status == StageStatus::Approved {
                    get_stage_origin(conn, stage)?
                        .as_deref()
                        .and_then(ApprovalOrigin::from_label)
                } else {
                    None
                };
                stages.push(StageLine {
                    stage,
                    status,
                    approval_origin,
                });
            }
            let analysis_chunks = UnitCounts {
                pending: count_analysis_chunks_by_status(conn, UnitStatus::Pending)?,
                in_flight: count_analysis_chunks_by_status(conn, UnitStatus::InFlight)?,
                done: count_analysis_chunks_by_status(conn, UnitStatus::Done)?,
                failed: count_analysis_chunks_by_status(conn, UnitStatus::Failed)?,
            };
            let translation_batches = UnitCounts {
                pending: count_translation_batches_by_status(conn, UnitStatus::Pending)?,
                in_flight: count_translation_batches_by_status(conn, UnitStatus::InFlight)?,
                done: count_translation_batches_by_status(conn, UnitStatus::Done)?,
                failed: count_translation_batches_by_status(conn, UnitStatus::Failed)?,
            };
            let usage_rows = usage_by_model(conn)?;
            Ok((stages, analysis_chunks, translation_batches, usage_rows))
        })
        .await?;

    let (usage, cost, unknown_cost_models) = summarize_usage(&usage_rows, config);

    Ok(StatusReport {
        stages,
        analysis_chunks,
        translation_batches,
        // No quality flags until translation runs (M6); report an empty map.
        flag_counts: BTreeMap::new(),
        usage,
        cost,
        unknown_cost_models,
        source_changed,
    })
}

/// The six stages in pipeline order (`docs/ARCHITECTURE.md` §2).
const STAGE_ORDER: [Stage; 6] = [
    Stage::Input,
    Stage::Analysis,
    Stage::AnalysisReview,
    Stage::Translation,
    Stage::TranslationReview,
    Stage::Output,
];

/// Sums usage and prices it per `(model, tier)` group (`docs/LLM.md` §9). The cost
/// is the sum over priced models; models without a price entry are listed instead
/// of failing the report.
fn summarize_usage(
    usage_rows: &[crate::store::ModelUsage],
    config: &Config,
) -> (UsageTotals, Cost, Vec<String>) {
    let table = PriceTable::from_config(&config.pricing);
    let mut totals = UsageTotals::default();
    let mut known = 0.0;
    let mut unknown = Vec::new();

    for row in usage_rows {
        totals.prompt_tokens += row.usage.prompt_tokens;
        totals.cached_tokens += row.usage.cached_tokens;
        totals.output_tokens += row.usage.output_tokens;
        totals.thinking_tokens += row.usage.thinking_tokens;

        let tier = parse_tier(row.service_tier.as_deref());
        match pricing::cost(&row.model, tier, &row.usage, &table) {
            Cost::Known(v) => known += v,
            Cost::Unknown => {
                if !unknown.contains(&row.model) {
                    unknown.push(row.model.clone());
                }
            }
        }
    }

    (totals, Cost::Known(known), unknown)
}

/// Parses a stored `service_tier` label, defaulting to `standard` (the un-shed
/// price) when absent or unrecognized.
fn parse_tier(label: Option<&str>) -> ServiceTier {
    match label {
        Some("flex") => ServiceTier::Flex,
        _ => ServiceTier::Standard,
    }
}
