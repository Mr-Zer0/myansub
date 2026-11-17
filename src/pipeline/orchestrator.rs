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
//! resumes it otherwise). The loop itself is the skeleton that M5/M6/M7 extend:
//! each executable stage becomes a `match` arm that runs its units through the
//! [`scheduler`](super::scheduler) and then `continue`s. In M4 no stage executes
//! inside the loop, so the loop stops at the first executable stage
//! (analysis) and prints `run \`myansub analyze\``.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::error::exit;
use crate::llm::client::ServiceTier;
use crate::llm::pricing::{self, Cost, PriceTable};
use crate::project::project_db_path;
use crate::store::repo_project::{get_project, get_stage};
use crate::store::repo_units::{
    count_analysis_chunks_by_status, count_translation_batches_by_status,
};
use crate::store::{
    Stage, StageStatus, Store, UnitStatus, current_analysis_approved, usage_by_model,
};

use super::error::PipelineError;
use super::input::{check_source_unchanged, run_input};

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
    /// `run` flags.
    pub options: RunOptions,
}

/// Where `run` stopped (`docs/CLI.md` §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// Stopped at an executable stage whose execution is not yet implemented in
    /// this milestone (analysis = M5, translation = M6, output = M7). The
    /// `next_action` names the command to run.
    StoppedAtStage { stage: Stage, next_action: String },
    /// Stopped at a review gate that is not approved. `next_action` names the
    /// command that approves it.
    StoppedAtGate { stage: Stage, next_action: String },
    /// Interrupted by cancellation (Ctrl-C).
    Interrupted,
    /// Nothing left to do: the output has been produced.
    Completed,
}

/// Creates-or-resumes the project and advances stages until a gate or an
/// unimplemented boundary (`docs/CLI.md` §6, `docs/ARCHITECTURE.md` §2/§4).
///
/// The `token` is honored cooperatively: cancellation between stages yields
/// [`RunOutcome::Interrupted`]. Once the scheduler runs units (M5+) the same token
/// is handed to it so an in-flight stage stops within its grace window
/// (`docs/ARCHITECTURE.md` §6 rule 6).
pub async fn run(
    req: RunRequest<'_>,
    _config: &Config,
    token: &CancellationToken,
) -> Result<RunOutcome, PipelineError> {
    tracing::debug!(
        auto_approve = ?req.options.auto_approve,
        allow_incomplete = req.options.allow_incomplete,
        dry_run = req.options.dry_run,
        "run: options threaded (effective M5+)"
    );

    let store = prepare_project(&req).await?;
    // Crash recovery: reset any in-flight units before advancing
    // (`docs/ARCHITECTURE.md` §6 rule 2). A no-op on a fresh project.
    store.recover().await?;

    // The advance loop is the skeleton M5/M6/M7 extend: an executed stage will
    // `continue` back to re-evaluate `next_step`. In M4 no stage executes inside
    // the loop (input is ensured by `prepare_project`; analysis/translation/output
    // execution land later), so every arm returns on the first iteration — hence
    // the `never_loop` allow until an executable arm adds the `continue` path.
    #[allow(clippy::never_loop)]
    loop {
        if token.is_cancelled() {
            return Ok(RunOutcome::Interrupted);
        }

        let state = load_state(&store).await?;
        match next_step(&state) {
            // M4: stop at the first executable stage and name its command. M5
            // replaces the analysis arm with real execution followed by `continue`.
            NextStep::Execute(stage) => {
                return Ok(RunOutcome::StoppedAtStage {
                    stage,
                    next_action: boundary_action(stage),
                });
            }
            NextStep::Approve(stage, next_action) => {
                return Ok(RunOutcome::StoppedAtGate { stage, next_action });
            }
            NextStep::Done => return Ok(RunOutcome::Completed),
        }
    }
}

/// The command that advances past an executable `stage` boundary not yet wired in
/// M4 (`docs/MILESTONES.md`).
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

/// Maps a `run` result to its process exit code, preferring the interrupt code
/// whenever cancellation was observed (`docs/ARCHITECTURE.md` §6 rule 6,
/// `docs/CLI.md` §7, ADR-031). This is the robust rule the scheduler's
/// `Summary::exit_code` documents: trust `token.is_cancelled()` over the outcome
/// for the interrupt.
///
/// - [`RunOutcome::Completed`] (all gates passed, output written) → `0`.
/// - [`RunOutcome::StoppedAtGate`] (a review gate needs approval; the next action
///   was printed) → `3`: this is a precondition/action-needed state, listed under
///   "gate not approved" in `docs/CLI.md` §7.
/// - [`RunOutcome::StoppedAtStage`] is the M4-only transitional stop at the
///   analysis boundary (analysis execution is M5) and maps to `0`; M5 replaces it
///   with real gate/completion stops.
pub fn run_exit_code(cancelled: bool, outcome: &RunOutcome) -> i32 {
    if cancelled {
        return exit::INTERRUPTED;
    }
    match outcome {
        RunOutcome::Interrupted => exit::INTERRUPTED,
        // Stopping at an unapproved review gate is a precondition/action-needed
        // state (`docs/CLI.md` §7, ADR-031).
        RunOutcome::StoppedAtGate { .. } => exit::PRECONDITION,
        // The M4 transitional boundary stop and a completed run are both success
        // (including nothing-to-do) in M4 (`docs/CLI.md` §7).
        RunOutcome::StoppedAtStage { .. } | RunOutcome::Completed => exit::SUCCESS,
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
    run_input(store, source, from, req.encoding, ext).await?;
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

/// Reads the full [`ProjectState`] from the store in one short closure.
async fn load_state(store: &Store) -> Result<ProjectState, PipelineError> {
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

/// How a stage approval was obtained (`docs/CLI.md` §4/§6). Approval-origin
/// tracking (user vs auto) is wired with the review stages (M5+); M4 reports
/// `None` for every stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOrigin {
    User,
    Auto,
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
                stages.push(StageLine {
                    stage,
                    status,
                    // Approval-origin tracking arrives with the review stages (M5+).
                    approval_origin: None,
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
