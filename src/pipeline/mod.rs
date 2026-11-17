//! The pipeline: the stage state machine and the generic work-unit scheduler
//! (`docs/ARCHITECTURE.md` §4).
//!
//! `pipeline` sits at the top of the dependency graph and may depend on `llm`,
//! `quality`, `store`, `formats`, and `domain`; nothing depends on it
//! (`docs/ARCHITECTURE.md` §3). M4-A provides the generic [`scheduler`]; the
//! `orchestrator` and per-stage modules (`input`, `analysis`, `translation`,
//! `output`) arrive in later M4 slices.

pub mod analysis;
pub mod error;
pub mod input;
pub mod orchestrator;
pub mod scheduler;

pub use analysis::{
    AnalysisFlags, AnalysisModeKind, AnalysisOutcome, AnalysisParams, DryRunReport,
    analysis_exit_code, analysis_output_budget, dry_run_analysis, prepare_analysis, run_analysis,
    worst_case_doc_tokens,
};
pub use error::PipelineError;
pub use orchestrator::{
    NextStep, ProjectState, RunOptions, RunOutcome, RunRequest, StatusReport, can_run, next_step,
    run, run_exit_code, status,
};
pub use scheduler::{SchedulerConfig, Summary, UnitOutcome, UnitState, WorkUnit};
