//! Generic bounded work-unit scheduler (`docs/ARCHITECTURE.md` §4).
//!
//! The scheduler is generic over [`WorkUnit`] and knows nothing about stages, the
//! database, or the LLM. It is the single place that implements the mechanics the
//! architecture requires of any batched stage:
//!
//! 1. **Recover on start.** Units left `in_flight` from a previous (crashed) run are
//!    reset to `pending` before dispatch, so recovered units rerun
//!    (`docs/ARCHITECTURE.md` §4.1, §6.2, NFR-2).
//! 2. **Bounded concurrency.** No more than `concurrency` units run at once, enforced
//!    with a [`tokio::sync::Semaphore`] (`docs/ARCHITECTURE.md` §4.2).
//! 3. **Cache warm-up.** When `warm_cache_first` is set, the first unit runs *alone*
//!    to completion before the rest fan out, so a shared cache entry is populated once
//!    instead of raced (`docs/ARCHITECTURE.md` §4.3).
//! 4. **Graceful cancellation.** On [`CancellationToken`] cancel the scheduler stops
//!    *dispatching* new units, waits up to `grace` for already-running units, then
//!    returns. Waits race the token with [`tokio::select!`] (`docs/ARCHITECTURE.md`
//!    §4.4, §6.6, NFR-3).
//! 5. **Summary.** It reports how many units finished, failed, or were cancelled
//!    (`docs/ARCHITECTURE.md` §4.5).
//!
//! A unit performs its work *and* commits its own status/result in one transaction
//! (`docs/ARCHITECTURE.md` §6.1); that commit lives in the [`WorkUnit`] impl the
//! orchestrator provides later, never in the scheduler. The scheduler only observes
//! each unit's [`UnitOutcome`] and counts it. A single failing unit is recorded and
//! never aborts the run, so a failed batch does not block others
//! (`docs/MILESTONES.md` M6).

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::{JoinError, JoinHandle};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::error::exit;
use crate::pipeline::progress::Progress;

/// The scheduler's view of a unit's persisted status.
///
/// This mirrors the store's unit lifecycle but is defined here so the scheduler
/// stays independent of `store` (`docs/ARCHITECTURE.md` §3 dependency direction).
/// A real [`WorkUnit`] maps its database status onto this enum; a fake unit tracks
/// it in memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitState {
    /// Not yet started; the scheduler will run it.
    Pending,
    /// Claimed by a previous run that did not finish. The scheduler resets it to
    /// [`UnitState::Pending`] at start so it reruns (`docs/ARCHITECTURE.md` §4.1).
    InFlight,
    /// Already finished successfully; the scheduler skips it.
    Done,
    /// Already failed terminally; the scheduler skips it.
    Failed,
}

/// The result of running one unit, as observed by the scheduler.
///
/// The unit has already committed its own status in its transaction; this only tells
/// the scheduler how to count it in the [`Summary`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitOutcome {
    /// The unit completed and committed its result.
    Done,
    /// The unit failed. It is recorded and does not abort the run.
    Failed,
}

/// A single schedulable piece of work (`docs/ARCHITECTURE.md` §4).
///
/// Implementors are provided by the orchestrator (one impl per stage) and by tests
/// (fake in-memory units). `run` both performs the work and commits its own status
/// and result in one transaction (`docs/ARCHITECTURE.md` §6.1); the scheduler never
/// touches the database.
///
/// The `Send + Sync + 'static` bound lets the scheduler spawn each unit's future on
/// the runtime (`docs/ARCHITECTURE.md` §5: client futures are `Send`).
pub trait WorkUnit: Send + Sync + 'static {
    /// A stable identifier used for logging and ordering (e.g. a chunk or batch no).
    fn id(&self) -> u64;

    /// A short label for logging (e.g. `"analysis_chunk"`), used in the `unit` span.
    fn kind(&self) -> &str;

    /// The unit's current persisted status, read once before dispatch.
    fn state(&self) -> UnitState;

    /// Resets an `in_flight` unit back to `pending` (`docs/ARCHITECTURE.md` §4.1).
    /// Called by the scheduler only for units whose [`WorkUnit::state`] is
    /// [`UnitState::InFlight`]. After this returns the unit's state is
    /// [`UnitState::Pending`].
    fn reset(&self) -> impl Future<Output = ()> + Send;

    /// Performs the unit's work and commits its status/result in one transaction.
    fn run(&self) -> impl Future<Output = UnitOutcome> + Send;
}

/// Scheduler tuning, sourced from config (`docs/CONFIG.md`,
/// `[translation].concurrency`/`warm_cache_first`, `[output].grace_secs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerConfig {
    /// Maximum number of units running at once (`concurrency ≥ 1`). A value of `0`
    /// is treated as `1` defensively; config validation already enforces `≥ 1`.
    pub concurrency: usize,
    /// Run the first unit alone before fanning out (`docs/ARCHITECTURE.md` §4.3).
    pub warm_cache_first: bool,
    /// How long to wait for in-flight units after cancellation before returning
    /// (`docs/CONFIG.md` `[output].grace_secs`, NFR-3).
    pub grace: Duration,
}

/// The outcome of a scheduler run (`docs/ARCHITECTURE.md` §4.5).
///
/// Counts cover only the units the scheduler actually considered runnable this run
/// (those `pending`, plus any `in_flight` units reset to `pending`). Units that were
/// already `done` or `failed` when the run started are skipped and not counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    /// Units that completed successfully.
    pub done: usize,
    /// Units that failed (recorded; a failure never aborts the run).
    pub failed: usize,
    /// Units never dispatched because of cancellation, plus in-flight units that did
    /// not finish within the grace period.
    pub cancelled: usize,
}

impl Summary {
    /// The process exit code this run implies (`docs/CLI.md` §7). Cancellation wins
    /// (interrupted, 130), then failed units (4), otherwise success (0). The final
    /// mapping is refined when the CLI wiring lands (M4-C/M6); this keeps it in one
    /// place for now.
    ///
    /// Note for the M4-C command wiring: this returns 130 only when `cancelled > 0`.
    /// If Ctrl-C arrives but every dispatched unit still finishes within the grace
    /// window (`cancelled == 0`), this will not report the interrupt. The robust rule
    /// is for the caller to prefer `token.is_cancelled()` to decide the interrupt code
    /// (130) regardless of the summary, and use this method otherwise.
    pub fn exit_code(&self) -> i32 {
        if self.cancelled > 0 {
            exit::INTERRUPTED
        } else if self.failed > 0 {
            exit::FAILED_UNITS
        } else {
            exit::SUCCESS
        }
    }
}

/// Runs a set of units under the crash-safety and concurrency rules of
/// `docs/ARCHITECTURE.md` §4.
///
/// Dispatch order follows the order of `units`. Already-`done`/`failed` units are
/// skipped; `in_flight` units are reset to `pending` and rerun. On cancellation the
/// scheduler stops dispatching and waits up to `cfg.grace` for in-flight units.
///
/// `progress` is an observe-only sink (`docs/ARCHITECTURE.md` §11, ADR-046): the
/// scheduler reports the runnable `total` through [`Progress::stage_units`] once it is
/// known (after the in-flight reset and the `Pending` filter), then one
/// [`Progress::unit_finished`] tick per finished unit, emitted from inside the unit's
/// spawned task so ticks are live and in completion order. Progress never affects the
/// returned [`Summary`], cancellation, or the grace/abort logic — a unit aborted after
/// the grace deadline never ticks, and that dropped tick is harmless (it is simply not
/// counted by the renderer, and the `Summary` still counts it as cancelled). The stage
/// boundaries ([`Progress::stage_start`] / [`Progress::stage_finish`]) are emitted by
/// the stage runners, not here.
pub async fn run<U: WorkUnit>(
    units: Vec<Arc<U>>,
    cfg: SchedulerConfig,
    token: &CancellationToken,
    progress: Arc<dyn Progress>,
) -> Summary {
    // §4.1 / §6.2: reset units left in_flight by a previous run so they rerun.
    for unit in &units {
        if unit.state() == UnitState::InFlight {
            unit.reset().await;
        }
    }

    let runnable: Vec<Arc<U>> = units
        .into_iter()
        .filter(|unit| unit.state() == UnitState::Pending)
        .collect();

    // §11: report the runnable total (remaining-only on a resume) once known.
    progress.stage_units(runnable.len());

    let mut summary = Summary::default();
    let mut remaining = runnable.into_iter();

    // §4.3: warm-up runs the first unit alone to completion before fan-out.
    if cfg.warm_cache_first {
        if let Some(first) = remaining.next() {
            let warmup_sem = Arc::new(Semaphore::new(1));
            dispatch(
                std::iter::once(first),
                warmup_sem,
                token,
                cfg.grace,
                &progress,
                &mut summary,
            )
            .await;
        }
        // If cancellation was observed during warm-up, do not fan out; the rest are
        // cancelled (§4.4).
        if token.is_cancelled() {
            summary.cancelled += remaining.by_ref().count();
            return summary;
        }
    }

    let sem = Arc::new(Semaphore::new(cfg.concurrency.max(1)));
    dispatch(remaining, sem, token, cfg.grace, &progress, &mut summary).await;
    summary
}

/// Dispatches `units` under a semaphore bound, honoring cancellation and the grace
/// period, and folds each outcome into `summary`.
///
/// The semaphore's permit count is the concurrency bound: a unit is spawned only
/// after acquiring a permit, which it holds until it finishes. Cancellation is
/// checked before every dispatch and raced against permit acquisition, so no new unit
/// starts once the token is cancelled (`docs/ARCHITECTURE.md` §4.4, §6.6).
async fn dispatch<U, I>(
    units: I,
    sem: Arc<Semaphore>,
    token: &CancellationToken,
    grace: Duration,
    progress: &Arc<dyn Progress>,
    summary: &mut Summary,
) where
    U: WorkUnit,
    I: Iterator<Item = Arc<U>>,
{
    let mut units = units;
    let mut handles: Vec<JoinHandle<UnitOutcome>> = Vec::new();

    loop {
        if token.is_cancelled() {
            summary.cancelled += units.by_ref().count();
            break;
        }
        let Some(unit) = units.next() else { break };

        // Wait for a free permit, but stop promptly if cancelled while waiting so a
        // full pipeline does not keep a cancelled run alive (§6.6).
        let permit: OwnedSemaphorePermit = tokio::select! {
            biased;
            _ = token.cancelled() => {
                // This unit plus everything after it is cancelled.
                summary.cancelled += 1 + units.by_ref().count();
                break;
            }
            acquired = sem.clone().acquire_owned() => match acquired {
                Ok(permit) => permit,
                // The semaphore is never closed; treat the impossible error as a
                // cancellation of the remaining work rather than panicking.
                Err(_) => {
                    summary.cancelled += 1 + units.by_ref().count();
                    break;
                }
            },
        };

        // Attach the per-unit span to the future with `.instrument` rather than
        // holding an `enter()` guard across the `.await`: an entered guard held over
        // an await corrupts span attribution when units are polled concurrently.
        let span = tracing::debug_span!("unit", kind = unit.kind(), id = unit.id());
        // Clone the observe-only sink into the task so the tick fires live, from the
        // unit's completion, in completion order (§11, ADR-046(b)). A unit aborted
        // after the grace deadline is dropped before this line, so its tick is simply
        // never emitted — acceptable and never double-counted.
        let progress = Arc::clone(progress);
        handles.push(tokio::spawn(async move {
            let _permit = permit;
            let outcome = unit.run().instrument(span).await;
            progress.unit_finished(outcome);
            outcome
        }));
    }

    drain(handles, token, grace, summary).await;
}

/// Waits for spawned units to finish and counts their outcomes.
///
/// Each handle is awaited while racing the token: the moment cancellation is observed
/// — whether it was already cancelled on entry or flips mid-wait — the remaining
/// handles are drained within a single `grace` window (`docs/ARCHITECTURE.md` §4.4,
/// §6.6, NFR-3). This matters even when every unit was dispatched before cancellation
/// (`concurrency >= runnable`): without racing the token here a late Ctrl-C would be
/// ignored until the last wave fully completed.
async fn drain(
    handles: Vec<JoinHandle<UnitOutcome>>,
    token: &CancellationToken,
    grace: Duration,
    summary: &mut Summary,
) {
    let mut handles = handles.into_iter();
    while let Some(mut handle) = handles.next() {
        if token.is_cancelled() {
            grace_drain(std::iter::once(handle).chain(handles), grace, summary).await;
            return;
        }
        // Await this unit, but switch to the grace-bounded path if the token flips.
        let cancelled = tokio::select! {
            biased;
            result = &mut handle => {
                record(result, summary);
                false
            }
            _ = token.cancelled() => true,
        };
        if cancelled {
            grace_drain(std::iter::once(handle).chain(handles), grace, summary).await;
            return;
        }
    }
}

/// Drains the remaining handles within one `grace` window: a unit that finishes
/// before the shared deadline is counted by outcome; one still running at the
/// deadline is **aborted** (not merely detached) and counted as cancelled, so it can
/// no longer commit its transaction after being reported interrupted.
async fn grace_drain(
    remaining: impl Iterator<Item = JoinHandle<UnitOutcome>>,
    grace: Duration,
    summary: &mut Summary,
) {
    let deadline = Instant::now() + grace;
    for mut handle in remaining {
        match tokio::time::timeout_at(deadline, &mut handle).await {
            Ok(result) => record(result, summary),
            Err(_elapsed) => {
                handle.abort();
                summary.cancelled += 1;
            }
        }
    }
}

/// Folds one join result into the summary. A task that panicked is counted as a
/// failure (it must not abort the whole run); an aborted task as cancelled.
fn record(result: Result<UnitOutcome, JoinError>, summary: &mut Summary) {
    match result {
        Ok(UnitOutcome::Done) => summary.done += 1,
        Ok(UnitOutcome::Failed) => summary.failed += 1,
        Err(join_error) if join_error.is_cancelled() => summary.cancelled += 1,
        Err(_) => summary.failed += 1,
    }
}
