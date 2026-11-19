//! The concrete live-progress renderer for the binary (`docs/ARCHITECTURE.md` §11,
//! ADR-046(d)). This is the only place that knows how progress is *drawn*; the
//! pipeline reports through the [`Progress`] trait and never learns the rendering.
//!
//! Everything here writes to **stderr** only (never stdout — stdout carries the final
//! outcome line, dry-run estimates, and `status`). Writes are infallible from the
//! caller's view: a closed stderr is not fatal, so write/flush errors are ignored
//! (ADR-046(e)). The renderer holds its counters behind a [`Mutex`], never touches the
//! store/LLM, and never holds the lock across an `.await`, so it cannot stall or abort
//! a unit.
//!
//! Caveat: a *slow-consumer* stderr pipe (stalled, not closed) could briefly block the
//! synchronous `write!`/`flush` while a unit's task holds its scheduler semaphore
//! permit. This is accepted — it mirrors `tracing`-to-stderr behaviour, and renders are
//! throttled (≤ ~10/s), so the exposure is bounded and no worse than ordinary logging.

use std::io::Write;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use myansub::pipeline::progress::{Progress, ProgressStage};
use myansub::pipeline::scheduler::{Summary, UnitOutcome};

/// Minimum wall-clock gap between live renders (≤ ~10 renders/second) so a fast run
/// does not flood the terminal (ADR-046(d)).
const RENDER_INTERVAL: Duration = Duration::from_millis(100);

/// Which progress display to use (ADR-046(d)). The decision is pure so its truth table
/// is unit-testable without a terminal; `main` maps it to the concrete sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressMode {
    /// No progress at all (`--no-progress`, or `--dry-run` which runs no units).
    Off,
    /// In-place carriage-return line (stderr is a TTY at default verbosity).
    Live,
    /// Throttled plain lines (pipe/CI, or raised logging that would clobber a line).
    Plain,
}

/// The pure mode decision (ADR-046(d)): `--no-progress`/`--dry-run` suppress progress;
/// otherwise an in-place line is used only when stderr is a terminal AND verbosity is
/// default (no `-v`/`-vv` and no `RUST_LOG` override, since raised logging would
/// clobber the line), degrading to plain lines otherwise.
pub fn decide_mode(
    no_progress: bool,
    dry_run: bool,
    stderr_is_tty: bool,
    verbose: u8,
    rust_log_set: bool,
) -> ProgressMode {
    if no_progress || dry_run {
        ProgressMode::Off
    } else if stderr_is_tty && verbose == 0 && !rust_log_set {
        ProgressMode::Live
    } else {
        ProgressMode::Plain
    }
}

/// A stderr progress renderer: an in-place carriage-return line on a terminal at
/// default verbosity, or throttled plain lines otherwise (ADR-046(d)).
pub struct StderrProgress {
    /// `true` → in-place CR-updated line (TTY, default verbosity); `false` → plain
    /// throttled lines (pipe/CI or raised logging).
    live: bool,
    state: Mutex<RenderState>,
}

struct RenderState {
    stage: Option<ProgressStage>,
    total: usize,
    done: usize,
    start: Instant,
    last_render: Instant,
    /// The `done` value at the last plain-line render; the plain path renders only on
    /// a change so a pipe/CI log is not spammed.
    last_plain_done: Option<usize>,
    /// Whether an in-place line is currently on screen and needs terminating.
    line_active: bool,
}

impl StderrProgress {
    /// Builds a renderer. `live` selects the in-place line (TTY + default verbosity)
    /// versus throttled plain lines; the caller decides it from
    /// `stderr().is_terminal()` and the log level (ADR-046(d)).
    pub fn new(live: bool) -> Self {
        let now = Instant::now();
        Self {
            live,
            state: Mutex::new(RenderState {
                stage: None,
                total: 0,
                done: 0,
                start: now,
                // Offset so the first unit tick is not swallowed by the throttle.
                last_render: now - RENDER_INTERVAL,
                last_plain_done: None,
                line_active: false,
            }),
        }
    }

    fn render(state: &mut RenderState, live: bool, force: bool) {
        let Some(stage) = state.stage else { return };
        let now = Instant::now();
        if !force && now.duration_since(state.last_render) < RENDER_INTERVAL {
            return;
        }
        let total = state.total;
        let done = state.done;
        let pct = done.saturating_mul(100).checked_div(total).unwrap_or(0);
        let elapsed = state.start.elapsed();
        let secs = elapsed.as_secs();
        let mut err = std::io::stderr();
        if live {
            // In-place line: carriage return, no newline, flushed each render.
            let _ = write!(
                err,
                "\r{} {}/{} {} · {}% · {}m{:02}s",
                stage.gerund(),
                done,
                total,
                stage.unit_noun(),
                pct,
                secs / 60,
                secs % 60,
            );
            let _ = err.flush();
            state.line_active = true;
        } else {
            // Plain path: one line per observed count change (never an in-place line).
            if state.last_plain_done == Some(done) {
                return;
            }
            let _ = writeln!(
                err,
                "{}: {}/{} {} done",
                stage.label(),
                done,
                total,
                stage.unit_noun(),
            );
            state.last_plain_done = Some(done);
        }
        state.last_render = now;
    }
}

impl Progress for StderrProgress {
    fn stage_start(&self, stage: ProgressStage) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let now = Instant::now();
        state.stage = Some(stage);
        state.total = 0;
        state.done = 0;
        state.start = now;
        state.last_render = now - RENDER_INTERVAL;
        state.last_plain_done = None;
        state.line_active = false;
    }

    fn stage_units(&self, total: usize) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.total = total;
        // Force the first render so "0/N" appears immediately.
        Self::render(&mut state, self.live, true);
    }

    fn unit_finished(&self, _outcome: UnitOutcome) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.done += 1;
        Self::render(&mut state, self.live, false);
    }

    fn stage_finish(&self, _summary: Summary) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        // Final render, forced so the last count is shown, then terminate the in-place
        // line with a newline so it never collides with the stdout outcome line.
        Self::render(&mut state, self.live, true);
        if self.live && state.line_active {
            let mut err = std::io::stderr();
            let _ = writeln!(err);
            let _ = err.flush();
            state.line_active = false;
        }
        state.stage = None;
    }
}

#[cfg(test)]
mod tests {
    use super::{ProgressMode, decide_mode};

    // AC: M10 (ADR-046(d)) — `--no-progress` and `--dry-run` suppress progress
    // regardless of the terminal/verbosity inputs.
    #[test]
    fn off_when_disabled_or_dry_run() {
        // no_progress wins over everything.
        assert_eq!(decide_mode(true, false, true, 0, false), ProgressMode::Off);
        // dry_run wins over everything.
        assert_eq!(decide_mode(false, true, true, 0, false), ProgressMode::Off);
        assert_eq!(decide_mode(true, true, false, 2, true), ProgressMode::Off);
    }

    // AC: M10 — the in-place live line is used only on a TTY at default verbosity with
    // no RUST_LOG override.
    #[test]
    fn live_only_on_tty_at_default_verbosity() {
        assert_eq!(
            decide_mode(false, false, true, 0, false),
            ProgressMode::Live
        );
    }

    // AC: M10 — a pipe/CI (non-TTY) or raised logging (`-v`/`-vv` or RUST_LOG) degrades
    // to throttled plain lines.
    #[test]
    fn plain_under_pipe_or_raised_logging() {
        // Not a TTY → plain.
        assert_eq!(
            decide_mode(false, false, false, 0, false),
            ProgressMode::Plain
        );
        // TTY but -v → plain.
        assert_eq!(
            decide_mode(false, false, true, 1, false),
            ProgressMode::Plain
        );
        // TTY but -vv → plain.
        assert_eq!(
            decide_mode(false, false, true, 2, false),
            ProgressMode::Plain
        );
        // TTY, default verbosity, but RUST_LOG set → plain.
        assert_eq!(
            decide_mode(false, false, true, 0, true),
            ProgressMode::Plain
        );
    }
}
