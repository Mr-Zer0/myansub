//! Live-progress sink (`docs/ARCHITECTURE.md` §11, ADR-046, FR-11).
//!
//! The long-running stages (`analyze`, `translate`, and both inside `run`) report
//! live "X of N done" feedback so an operator can see work advancing. This module
//! defines the narrow, observe-only seam the pipeline reports *through*; the concrete
//! renderer (`StderrProgress`) lives in `main` and is the only thing that knows how
//! progress is drawn. This mirrors the ADR-030/ADR-042 trait-seam style (a narrow
//! trait defined in the lower layer, implemented above) and keeps `main ─► pipeline`
//! the only edge — the scheduler and stage runners depend on the trait alone, never
//! on `main`/`cli` (`docs/ARCHITECTURE.md` §3).
//!
//! Progress is purely observational: a [`Progress`] implementation never touches the
//! database or the LLM, never holds a lock across an `.await`, and a tick that arrives
//! after the cancellation grace deadline is simply dropped (the unit's task is aborted
//! before it ticks). So progress cannot change the scheduler's [`Summary`],
//! cancellation, or the exit-code contract (ADR-046(e), NFR-3).

use crate::pipeline::scheduler::{Summary, UnitOutcome};

/// A pipeline stage that reports live progress (`docs/ARCHITECTURE.md` §11). Only the
/// two unit-bearing stages are tracked; `input` and `output` do no batched work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressStage {
    /// The analysis stage (single-pass call or map-reduce chunks).
    Analysis,
    /// The translation stage (per-batch requests).
    Translation,
}

impl ProgressStage {
    /// A short noun label for the stage, used as the plain-line prefix
    /// (e.g. `"translation"`).
    pub fn label(self) -> &'static str {
        match self {
            ProgressStage::Analysis => "analysis",
            ProgressStage::Translation => "translation",
        }
    }

    /// A present-participle label for the in-place terminal line
    /// (e.g. `"translating"`).
    pub fn gerund(self) -> &'static str {
        match self {
            ProgressStage::Analysis => "analyzing",
            ProgressStage::Translation => "translating",
        }
    }

    /// The noun for a unit of this stage's work (`"chunks"` / `"batches"`).
    pub fn unit_noun(self) -> &'static str {
        match self {
            ProgressStage::Analysis => "chunks",
            ProgressStage::Translation => "batches",
        }
    }
}

/// An observe-only sink for live stage/unit progress (`docs/ARCHITECTURE.md` §11,
/// ADR-046).
///
/// The scheduler emits [`Progress::stage_units`] once the runnable total is known and
/// [`Progress::unit_finished`] as each unit completes (from inside the unit's spawned
/// task, so ticks are live and arrive in completion order). The stage runners
/// (`run_analysis`, `run_translation`) bracket each stage with
/// [`Progress::stage_start`] / [`Progress::stage_finish`] so a `run` shows analysis →
/// translation distinctly.
///
/// The trait is object-safe (dyn-compatible): every method takes `&self` and only
/// `Copy` scalar arguments, so the pipeline can hold an `Arc<dyn Progress>` and clone
/// it into each spawned task. All methods have empty default bodies, so a sink
/// overrides only what it needs and [`NoopProgress`] is a one-liner.
pub trait Progress: Send + Sync {
    /// A stage's unit work is about to begin. Emitted by the stage runner.
    fn stage_start(&self, stage: ProgressStage) {
        let _ = stage;
    }

    /// The scheduler's runnable `total` — the number of units that will actually run
    /// this invocation, after resetting any `in_flight` units and skipping units that
    /// were already `done` on a resume. Emitted once by the scheduler.
    fn stage_units(&self, total: usize) {
        let _ = total;
    }

    /// One unit has finished with `outcome` (`Done`/`Failed`). Emitted by the unit's
    /// spawned task right after its work resolves, so ticks are live and in completion
    /// order. A unit aborted after the grace deadline never reaches this call — that
    /// dropped tick is acceptable and never double-counted (ADR-046(e)).
    fn unit_finished(&self, outcome: UnitOutcome) {
        let _ = outcome;
    }

    /// The stage's units are done; `summary` is the scheduler's [`Summary`]
    /// (done/failed/cancelled). Emitted by the stage runner, which terminates the
    /// renderer's in-place line so it never collides with the stdout outcome line.
    fn stage_finish(&self, summary: Summary) {
        let _ = summary;
    }
}

/// A zero-cost [`Progress`] that ignores every event (ADR-046(a)).
///
/// Used wherever a real renderer is not wanted: by `--no-progress`, by `--dry-run`
/// (which runs no units), and by every test and pipeline path that does not assert
/// progress. Because all [`Progress`] methods default to empty bodies, this is an
/// empty impl and the compiler elides the calls.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopProgress;

impl Progress for NoopProgress {}
