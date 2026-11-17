//! The pipeline: the stage state machine and the generic work-unit scheduler
//! (`docs/ARCHITECTURE.md` §4).
//!
//! `pipeline` sits at the top of the dependency graph and may depend on `llm`,
//! `quality`, `store`, `formats`, and `domain`; nothing depends on it
//! (`docs/ARCHITECTURE.md` §3). M4-A provides the generic [`scheduler`]; the
//! `orchestrator` and per-stage modules (`input`, `analysis`, `translation`,
//! `output`) arrive in later M4 slices.

pub mod error;
pub mod input;
pub mod scheduler;

pub use error::PipelineError;
pub use scheduler::{SchedulerConfig, Summary, UnitOutcome, UnitState, WorkUnit};
