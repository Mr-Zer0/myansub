//! M4-AC1: generic bounded work-unit scheduler behaviors, exercised through the
//! public `pipeline::scheduler` surface with fake in-memory units (no DB, no
//! network, per `docs/TESTING.md` §1/§4).
//!
//! Determinism comes from a [`tokio::sync::Barrier`] (to force and observe
//! concurrency without sleeping), an [`AtomicUsize`] peak tracker, a recorded event
//! log, and `tokio::time` under `start_paused` for the grace window (auto-advancing
//! paused time instead of real sleeps).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use myansub::pipeline::scheduler::{self, SchedulerConfig, UnitOutcome, UnitState, WorkUnit};
use tokio::sync::Barrier;
use tokio_util::sync::CancellationToken;

/// Tracks the current and peak number of simultaneously running units.
#[derive(Default)]
struct Peak {
    current: AtomicUsize,
    max: AtomicUsize,
}

impl Peak {
    fn enter(&self) {
        let now = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        self.max.fetch_max(now, Ordering::SeqCst);
    }

    fn leave(&self) {
        self.current.fetch_sub(1, Ordering::SeqCst);
    }

    fn observed(&self) -> usize {
        self.max.load(Ordering::SeqCst)
    }
}

/// An event in the lifetime of a unit, used to assert ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    Start(u64),
    Done(u64),
}

/// A configurable fake [`WorkUnit`] that records what the scheduler did to it.
struct FakeUnit {
    id: u64,
    state: Mutex<UnitState>,
    reset_calls: AtomicUsize,
    run_calls: AtomicUsize,
    outcome: UnitOutcome,
    peak: Option<Arc<Peak>>,
    barrier: Option<Arc<Barrier>>,
    events: Option<Arc<Mutex<Vec<Event>>>>,
    cancel_on_run: Option<CancellationToken>,
    sleep: Option<Duration>,
    panic: bool,
    completed: AtomicUsize,
}

impl FakeUnit {
    fn new(id: u64) -> Self {
        Self {
            id,
            state: Mutex::new(UnitState::Pending),
            reset_calls: AtomicUsize::new(0),
            run_calls: AtomicUsize::new(0),
            outcome: UnitOutcome::Done,
            peak: None,
            barrier: None,
            events: None,
            cancel_on_run: None,
            sleep: None,
            panic: false,
            completed: AtomicUsize::new(0),
        }
    }

    fn state(mut self, state: UnitState) -> Self {
        self.state = Mutex::new(state);
        self
    }

    fn outcome(mut self, outcome: UnitOutcome) -> Self {
        self.outcome = outcome;
        self
    }

    fn peak(mut self, peak: Arc<Peak>) -> Self {
        self.peak = Some(peak);
        self
    }

    fn barrier(mut self, barrier: Arc<Barrier>) -> Self {
        self.barrier = Some(barrier);
        self
    }

    fn events(mut self, events: Arc<Mutex<Vec<Event>>>) -> Self {
        self.events = Some(events);
        self
    }

    fn cancel_on_run(mut self, token: CancellationToken) -> Self {
        self.cancel_on_run = Some(token);
        self
    }

    fn sleep(mut self, dur: Duration) -> Self {
        self.sleep = Some(dur);
        self
    }

    fn panics(mut self) -> Self {
        self.panic = true;
        self
    }

    fn reset_calls(&self) -> usize {
        self.reset_calls.load(Ordering::SeqCst)
    }

    fn run_calls(&self) -> usize {
        self.run_calls.load(Ordering::SeqCst)
    }

    /// How many times `run()` reached its final line (i.e. was not aborted mid-run).
    fn completed(&self) -> usize {
        self.completed.load(Ordering::SeqCst)
    }
}

impl WorkUnit for FakeUnit {
    fn id(&self) -> u64 {
        self.id
    }

    fn kind(&self) -> &str {
        "fake"
    }

    fn state(&self) -> UnitState {
        *self.state.lock().expect("state lock")
    }

    async fn reset(&self) {
        self.reset_calls.fetch_add(1, Ordering::SeqCst);
        *self.state.lock().expect("state lock") = UnitState::Pending;
    }

    async fn run(&self) -> UnitOutcome {
        self.run_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(events) = &self.events {
            events
                .lock()
                .expect("events lock")
                .push(Event::Start(self.id));
        }
        if let Some(token) = &self.cancel_on_run {
            token.cancel();
        }
        if self.panic {
            panic!("fake unit {} panicked on purpose", self.id);
        }
        if let Some(peak) = &self.peak {
            peak.enter();
        }
        if let Some(barrier) = &self.barrier {
            barrier.wait().await;
        }
        if let Some(dur) = self.sleep {
            tokio::time::sleep(dur).await;
        }
        if let Some(peak) = &self.peak {
            peak.leave();
        }
        if let Some(events) = &self.events {
            events
                .lock()
                .expect("events lock")
                .push(Event::Done(self.id));
        }
        self.completed.fetch_add(1, Ordering::SeqCst);
        self.outcome
    }
}

fn cfg(concurrency: usize, warm_cache_first: bool) -> SchedulerConfig {
    SchedulerConfig {
        concurrency,
        warm_cache_first,
        grace: Duration::from_secs(10),
    }
}

// AC: M4-AC1 (bounded concurrency) — never more than `concurrency` units run at
// once, and with enough units the bound is actually reached.
#[tokio::test]
async fn concurrency_is_bounded_and_reaches_the_limit() {
    let concurrency = 3;
    let total = 6; // a multiple of concurrency so the barrier releases in full waves
    let peak = Arc::new(Peak::default());
    let barrier = Arc::new(Barrier::new(concurrency));

    let units: Vec<Arc<FakeUnit>> = (0..total)
        .map(|id| {
            Arc::new(
                FakeUnit::new(id)
                    .peak(peak.clone())
                    .barrier(barrier.clone()),
            )
        })
        .collect();

    let token = CancellationToken::new();
    let summary = scheduler::run(units, cfg(concurrency, false), &token).await;

    assert_eq!(summary.done, total as usize);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.cancelled, 0);
    assert_eq!(
        peak.observed(),
        concurrency,
        "peak concurrency should reach but not exceed the bound"
    );
}

// AC: M4-AC1 (warm-up ordering) — with warm_cache_first the first unit completes
// before any other unit starts. The warm-up unit yields (sleeps) mid-run so that,
// without warm-up, the fan-out units would start during that yield; warm-up must
// hold them back until it finishes.
#[tokio::test(start_paused = true)]
async fn warm_cache_first_runs_the_first_unit_alone() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let units: Vec<Arc<FakeUnit>> = (0..4)
        .map(|id| {
            let unit = FakeUnit::new(id).events(events.clone());
            let unit = if id == 0 {
                unit.sleep(Duration::from_secs(1))
            } else {
                unit
            };
            Arc::new(unit)
        })
        .collect();

    let token = CancellationToken::new();
    let summary = scheduler::run(units, cfg(3, true), &token).await;
    assert_eq!(summary.done, 4);

    let log = events.lock().expect("events lock").clone();
    let done_first = log
        .iter()
        .position(|e| *e == Event::Done(0))
        .expect("first unit should finish");
    for id in 1..4 {
        let started = log
            .iter()
            .position(|e| *e == Event::Start(id))
            .expect("other unit should start");
        assert!(
            done_first < started,
            "unit {id} started (at {started}) before the warm-up unit finished (at {done_first})"
        );
    }
}

// AC: M4-AC1 (warm-up ordering, negated) — without warm_cache_first, units fan out
// from the start: two units can run at once before either finishes.
#[tokio::test]
async fn without_warm_cache_units_fan_out_immediately() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let barrier = Arc::new(Barrier::new(2));
    let units: Vec<Arc<FakeUnit>> = (0..2)
        .map(|id| {
            Arc::new(
                FakeUnit::new(id)
                    .events(events.clone())
                    .barrier(barrier.clone()),
            )
        })
        .collect();

    let token = CancellationToken::new();
    // The barrier only releases once both units have started; if the scheduler
    // serialized them (as warm-up would) this would never complete.
    let summary = scheduler::run(units, cfg(2, false), &token).await;
    assert_eq!(summary.done, 2);

    let log = events.lock().expect("events lock").clone();
    let first_done = log
        .iter()
        .position(|e| matches!(e, Event::Done(_)))
        .expect("some unit should finish");
    let starts_before_first_done = log[..first_done]
        .iter()
        .filter(|e| matches!(e, Event::Start(_)))
        .count();
    assert_eq!(
        starts_before_first_done, 2,
        "both units should start before either finishes"
    );
}

// AC: M4-AC1 (recovered units rerun) — units left in_flight are reset to pending and
// executed; already-done units are skipped.
#[tokio::test]
async fn in_flight_units_are_reset_and_rerun() {
    let recovered_a = Arc::new(FakeUnit::new(0).state(UnitState::InFlight));
    let recovered_b = Arc::new(FakeUnit::new(1).state(UnitState::InFlight));
    let already_done = Arc::new(FakeUnit::new(2).state(UnitState::Done));
    let pending = Arc::new(FakeUnit::new(3).state(UnitState::Pending));

    let units: Vec<Arc<FakeUnit>> = vec![
        recovered_a.clone(),
        recovered_b.clone(),
        already_done.clone(),
        pending.clone(),
    ];

    let token = CancellationToken::new();
    let summary = scheduler::run(units, cfg(2, false), &token).await;

    // Both recovered units plus the pending one run; the done one is skipped.
    assert_eq!(summary.done, 3);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.cancelled, 0);

    assert_eq!(recovered_a.reset_calls(), 1);
    assert_eq!(recovered_b.reset_calls(), 1);
    assert_eq!(recovered_a.run_calls(), 1);
    assert_eq!(recovered_b.run_calls(), 1);

    assert_eq!(already_done.reset_calls(), 0);
    assert_eq!(already_done.run_calls(), 0);
    assert_eq!(pending.reset_calls(), 0);
    assert_eq!(pending.run_calls(), 1);
}

// AC: M4-AC1 (summary) — a mix of succeeding and failing units yields correct
// counts, and a failing unit does not prevent the others from completing.
#[tokio::test]
async fn summary_counts_done_and_failed_without_one_blocking_others() {
    let units: Vec<Arc<FakeUnit>> = vec![
        Arc::new(FakeUnit::new(0).outcome(UnitOutcome::Done)),
        Arc::new(FakeUnit::new(1).outcome(UnitOutcome::Failed)),
        Arc::new(FakeUnit::new(2).outcome(UnitOutcome::Done)),
        Arc::new(FakeUnit::new(3).outcome(UnitOutcome::Failed)),
        Arc::new(FakeUnit::new(4).outcome(UnitOutcome::Done)),
    ];

    let token = CancellationToken::new();
    let summary = scheduler::run(units, cfg(2, false), &token).await;

    assert_eq!(summary.done, 3);
    assert_eq!(summary.failed, 2);
    assert_eq!(summary.cancelled, 0);
    assert_eq!(summary.exit_code(), 4);
}

// AC: M4-AC1 (cancellation stops dispatch) — once cancellation is observed no new
// unit starts; in-flight units that finish within grace are counted done and the
// undispatched rest are cancelled.
#[tokio::test(start_paused = true)]
async fn cancellation_stops_dispatch_and_honors_grace() {
    let token = CancellationToken::new();

    // concurrency 2 so the third acquire blocks, forcing the scheduler to observe
    // the cancellation triggered by unit 0 before dispatching units 2 and 3.
    let canceller = Arc::new(FakeUnit::new(0).cancel_on_run(token.clone()));
    let in_flight = Arc::new(FakeUnit::new(1).sleep(Duration::from_secs(1)));
    let never_a = Arc::new(FakeUnit::new(2));
    let never_b = Arc::new(FakeUnit::new(3));

    let units: Vec<Arc<FakeUnit>> = vec![
        canceller.clone(),
        in_flight.clone(),
        never_a.clone(),
        never_b.clone(),
    ];

    let summary = scheduler::run(units, cfg(2, false), &token).await;

    // Units 0 and 1 were in flight when cancellation hit and both finished within
    // the 10s grace; units 2 and 3 were never dispatched.
    assert_eq!(summary.done, 2);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.cancelled, 2);
    assert_eq!(summary.exit_code(), 130);

    assert_eq!(canceller.run_calls(), 1);
    assert_eq!(in_flight.run_calls(), 1);
    assert_eq!(never_a.run_calls(), 0, "no unit starts after cancellation");
    assert_eq!(never_b.run_calls(), 0, "no unit starts after cancellation");
}

// AC: M4-AC1 (cancellation + grace) — an in-flight unit that does not finish within
// grace is aborted and counted as cancelled.
#[tokio::test(start_paused = true)]
async fn in_flight_unit_exceeding_grace_is_cancelled() {
    let token = CancellationToken::new();

    // concurrency 1: unit 0 holds the only permit, cancels, then runs far longer
    // than the grace window; unit 1 is never dispatched.
    let slow = Arc::new(
        FakeUnit::new(0)
            .cancel_on_run(token.clone())
            .sleep(Duration::from_secs(100)),
    );
    let never = Arc::new(FakeUnit::new(1));

    let units: Vec<Arc<FakeUnit>> = vec![slow.clone(), never.clone()];
    let summary = scheduler::run(units, cfg(1, false), &token).await;

    assert_eq!(summary.done, 0);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.cancelled, 2);
    assert_eq!(slow.run_calls(), 1);
    assert_eq!(never.run_calls(), 0);

    // The grace-exceeded unit must be aborted, not merely detached: give the clock
    // enough room to pass its 100s sleep and confirm it never reached its final line.
    tokio::time::advance(Duration::from_secs(300)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        slow.completed(),
        0,
        "a grace-exceeded unit must be aborted, not left running"
    );
}

// AC: M4-AC1 (summary) — a unit whose run() panics is counted as failed and does not
// abort its peers.
#[tokio::test]
async fn a_panicking_unit_is_counted_failed_and_does_not_block_peers() {
    let boom = Arc::new(FakeUnit::new(1).panics());
    let peer_a = Arc::new(FakeUnit::new(0));
    let peer_b = Arc::new(FakeUnit::new(2));

    let units: Vec<Arc<FakeUnit>> = vec![peer_a.clone(), boom.clone(), peer_b.clone()];
    let token = CancellationToken::new();
    let summary = scheduler::run(units, cfg(3, false), &token).await;

    assert_eq!(summary.done, 2, "both peers complete");
    assert_eq!(summary.failed, 1, "the panicking unit is counted failed");
    assert_eq!(summary.cancelled, 0);
    assert_eq!(peer_a.completed(), 1);
    assert_eq!(peer_b.completed(), 1);
}

// AC: M4-AC1 (cancellation during drain) — when every unit is already dispatched
// (concurrency >= total) and cancellation arrives while they run, the drain must
// still honor grace and report the interruption. This is the path that hid the
// "Ctrl-C ignored for the last wave" bug.
#[tokio::test(start_paused = true)]
async fn cancellation_during_drain_after_full_dispatch_is_honored() {
    let token = CancellationToken::new();

    // concurrency 3 == total, so all three are dispatched before any runs; the drain
    // then enters with the token not yet cancelled.
    let canceller = Arc::new(
        FakeUnit::new(0)
            .cancel_on_run(token.clone())
            .sleep(Duration::from_secs(100)),
    );
    let peer_a = Arc::new(FakeUnit::new(1).sleep(Duration::from_secs(100)));
    let peer_b = Arc::new(FakeUnit::new(2).sleep(Duration::from_secs(100)));

    let units: Vec<Arc<FakeUnit>> = vec![canceller.clone(), peer_a.clone(), peer_b.clone()];
    let summary = scheduler::run(units, cfg(3, false), &token).await;

    // All three were running and all exceed the 10s grace, so none complete.
    assert_eq!(summary.done, 0);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.cancelled, 3);
    assert_eq!(summary.exit_code(), 130);
    assert_eq!(canceller.completed(), 0);
    assert_eq!(peer_a.completed(), 0);
    assert_eq!(peer_b.completed(), 0);
}

// AC: M4-AC1 (default-path cancel) — the default is warm_cache_first=true, so the
// single warm-up unit is the likeliest Ctrl-C moment. Cancelling mid-warm-up must
// prevent any fan-out unit from starting and count the rest cancelled.
#[tokio::test(start_paused = true)]
async fn cancellation_during_warmup_prevents_fanout() {
    let token = CancellationToken::new();

    let warmup = Arc::new(
        FakeUnit::new(0)
            .cancel_on_run(token.clone())
            .sleep(Duration::from_secs(100)),
    );
    let fan_a = Arc::new(FakeUnit::new(1));
    let fan_b = Arc::new(FakeUnit::new(2));

    let units: Vec<Arc<FakeUnit>> = vec![warmup.clone(), fan_a.clone(), fan_b.clone()];
    let summary = scheduler::run(units, cfg(2, true), &token).await;

    // The warm-up unit exceeds grace (aborted, cancelled); the fan-out units never
    // run.
    assert_eq!(summary.done, 0);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.cancelled, 3);
    assert_eq!(summary.exit_code(), 130);
    assert_eq!(warmup.run_calls(), 1);
    assert_eq!(warmup.completed(), 0);
    assert_eq!(
        fan_a.run_calls(),
        0,
        "no fan-out after warm-up cancellation"
    );
    assert_eq!(
        fan_b.run_calls(),
        0,
        "no fan-out after warm-up cancellation"
    );
}

// Degenerate input: no runnable units yields an all-zero summary (exit success).
#[tokio::test]
async fn empty_run_is_success() {
    let units: Vec<Arc<FakeUnit>> = Vec::new();
    let token = CancellationToken::new();
    let summary = scheduler::run(units, cfg(4, true), &token).await;
    assert_eq!(summary, scheduler::Summary::default());
    assert_eq!(summary.exit_code(), 0);
}
