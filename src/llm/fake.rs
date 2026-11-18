//! A scripted [`LlmClient`] for tests (`docs/TESTING.md` §3).
//!
//! `Fake` holds a FIFO script of responses/errors and a call log. Tests drive it
//! and then assert both the outputs and the exact request sequence (e.g. "batch 3
//! sent exactly once"). It is gated behind `cfg(test)` for this crate's own unit
//! tests and the `testing` feature for the integration tests (see ADR-027), so it
//! never ships in a production build.
//!
//! `Fake` is cheaply cloneable (shared state): hand one clone to the layer stack
//! and keep another to inspect the call log afterwards.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::llm::client::{LlmClient, LlmError, LlmRequest, LlmResponse};

type Responder = Box<dyn Fn() -> Result<LlmResponse, LlmError> + Send + Sync>;

#[derive(Default)]
struct FakeState {
    /// Scripted results returned in order, one per call.
    script: VecDeque<Result<LlmResponse, LlmError>>,
    /// Fallback used once the script is exhausted (e.g. "always retryable").
    default: Option<Responder>,
    /// Every request the fake received, in order.
    calls: Vec<LlmRequest>,
    /// When set, the token is cancelled on each call before the result is
    /// returned — used to drive cancellation tests deterministically.
    cancel_on_call: Option<CancellationToken>,
    /// When set, the token is cancelled once this many calls have been received
    /// (the Nth call), just before that call's result is returned. Lets a test
    /// stop the scheduler after a fixed number of completed units — a simulated
    /// crash between units (`docs/TESTING.md` §4).
    cancel_after: Option<(CancellationToken, usize)>,
}

/// A scripted transport for tests.
#[derive(Clone, Default)]
pub struct Fake {
    state: Arc<Mutex<FakeState>>,
}

impl Fake {
    /// An empty fake. Without any scripted result or default, a call returns a
    /// fatal "no scripted response" error.
    pub fn new() -> Self {
        Self::default()
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut FakeState) -> R) -> R {
        // The fake is test-only; a poisoned lock means a prior test thread
        // panicked, so recovering the guard keeps assertions meaningful.
        let mut guard = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut guard)
    }

    /// Enqueues one scripted result (consumed by the next call).
    pub fn push_result(self, result: Result<LlmResponse, LlmError>) -> Self {
        self.with_state(|s| s.script.push_back(result));
        self
    }

    /// Enqueues a successful response for the next call.
    pub fn push_response(self, response: LlmResponse) -> Self {
        self.push_result(Ok(response))
    }

    /// Sets a fallback returned for every call once the script is exhausted. The
    /// closure is called per request so it can return a fresh (non-`Clone`) error.
    pub fn always(
        self,
        responder: impl Fn() -> Result<LlmResponse, LlmError> + Send + Sync + 'static,
    ) -> Self {
        self.with_state(|s| s.default = Some(Box::new(responder)));
        self
    }

    /// Cancels `token` on each call, just before returning the result. Lets a test
    /// prove the `Retrying` layer interrupts its backoff on cancellation.
    pub fn cancel_on_call(self, token: CancellationToken) -> Self {
        self.with_state(|s| s.cancel_on_call = Some(token));
        self
    }

    /// Cancels `token` once `after` calls have been received (on the `after`-th
    /// call), after that call's result is scripted but before it is returned. The
    /// `after`-th call still returns its scripted result, so the unit that made it
    /// completes; later units are then never dispatched by the scheduler, modelling
    /// a crash between units (`docs/TESTING.md` §4).
    pub fn cancel_after(self, token: CancellationToken, after: usize) -> Self {
        self.with_state(|s| s.cancel_after = Some((token, after)));
        self
    }

    /// Every request received, in order.
    pub fn calls(&self) -> Vec<LlmRequest> {
        self.with_state(|s| s.calls.clone())
    }

    /// Number of calls received.
    pub fn call_count(&self) -> usize {
        self.with_state(|s| s.calls.len())
    }

    /// Whether exactly one received request satisfies `predicate` (e.g. matches a
    /// given logical batch), per `docs/TESTING.md` §3.
    pub fn sent_exactly_once(&self, predicate: impl Fn(&LlmRequest) -> bool) -> bool {
        self.with_state(|s| s.calls.iter().filter(|r| predicate(r)).count() == 1)
    }
}

impl LlmClient for Fake {
    async fn generate(&self, req: LlmRequest) -> Result<LlmResponse, LlmError> {
        self.with_state(|s| {
            s.calls.push(req);
            if let Some(token) = &s.cancel_on_call {
                token.cancel();
            }
            if let Some((token, after)) = &s.cancel_after
                && s.calls.len() == *after
            {
                token.cancel();
            }
            if let Some(result) = s.script.pop_front() {
                result
            } else if let Some(default) = &s.default {
                default()
            } else {
                Err(LlmError::Fatal {
                    status: 0,
                    message: "fake: no scripted response".to_string(),
                })
            }
        })
    }
}
