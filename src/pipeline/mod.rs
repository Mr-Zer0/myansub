//! The pipeline: the stage state machine and the generic work-unit scheduler
//! (`docs/ARCHITECTURE.md` §4).
//!
//! `pipeline` sits at the top of the dependency graph and may depend on `llm`,
//! `quality`, `store`, `formats`, and `domain`; nothing depends on it
//! (`docs/ARCHITECTURE.md` §3). M4-A provides the generic [`scheduler`]; the
//! `orchestrator` and per-stage modules (`input`, `analysis`, `translation`,
//! `output`) arrive in later M4 slices.

pub mod analysis;
pub mod confirm;
pub mod error;
pub mod input;
pub mod invalidation;
pub mod orchestrator;
pub mod output;
pub mod progress;
pub mod scheduler;
pub mod translation;

pub use analysis::{
    AnalysisFlags, AnalysisModeKind, AnalysisOutcome, AnalysisParams, DryRunReport,
    analysis_exit_code, analysis_output_budget, dry_run_analysis, prepare_analysis, run_analysis,
    run_analysis_confirmed, worst_case_doc_tokens,
};
pub use confirm::{
    ConfirmAction, ConfirmSettings, ConfirmedRun, CostEstimate, GateDecision, confirm_decision,
    confirm_or_refuse,
};
pub use error::PipelineError;
pub use invalidation::{
    AddressPair, AnalysisDoc, ApprovalInvalidation, Character, GlossaryEntry, Scene, affected_cues,
    approve_and_invalidate, approve_and_invalidate_in_tx, invalidate_after_approval,
    parse_analysis_doc,
};
pub use orchestrator::{
    NextStep, ProjectState, RunDryRunEstimate, RunOptions, RunOutcome, RunReport, RunRequest,
    StatusReport, analysis_params, can_run, next_step, run, run_confirmed, run_exit_code, status,
    translation_params,
};
pub use output::{
    BuiltOutput, OutputReport, OutputWarning, ResolvedOutput, build_output_document,
    default_output_path, resolve_output_format_from_flags, resolve_output_target, run_output,
    store_creation_prefs, stored_output_prefs,
};
pub use progress::{NoopProgress, Progress, ProgressStage};
pub use scheduler::{SchedulerConfig, Summary, UnitOutcome, UnitState, WorkUnit};
pub use translation::{
    TranslationFlags, TranslationOutcome, TranslationParams, dry_run_translation,
    dry_run_translation_stale, prepare_translation, run_translation, run_translation_confirmed,
    stale_unlocked_idxs, translation_exit_code,
};
