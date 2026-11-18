//! The `Retrying` layer: applies the pure [`retry`](crate::llm::retry) policy with
//! real backoff sleeps and the cancellation token (`docs/LLM.md` §6,
//! `docs/ARCHITECTURE.md` §5/§6.6).
//!
//! It sits *inside* the cache layer and *outside* the transport
//! (`Metered<Cached<Retrying<Transport>>>`), so a cache hit never triggers a retry
//! and the transport only ever sees a single attempt. Backoff sleeps race against
//! the [`CancellationToken`] via [`tokio::select!`], so Ctrl-C interrupts a sleep
//! promptly instead of waiting out the full delay.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio_util::sync::CancellationToken;

use crate::llm::client::{ErrorClass, LlmClient, LlmError, LlmRequest, LlmResponse, ServiceTier};
use crate::llm::retry::{Decision, RetryConfig, Xorshift, decide};

/// Wraps an inner [`LlmClient`] (normally the transport) with the transport retry
/// policy, flex→standard tier fallback, and cancellation-aware backoff.
#[derive(Debug)]
pub struct Retrying<T> {
    inner: T,
    cfg: RetryConfig,
    token: CancellationToken,
    /// Jitter source; behind a mutex so `generate(&self)` can draw from it without
    /// holding the lock across an `.await` (keeps the future `Send`).
    rng: Mutex<Xorshift>,
}

impl<T> Retrying<T> {
    /// Builds a layer with an explicit jitter seed (use a fixed seed in tests for
    /// determinism).
    pub fn new(inner: T, cfg: RetryConfig, token: CancellationToken, seed: u64) -> Self {
        Self {
            inner,
            cfg,
            token,
            rng: Mutex::new(Xorshift::new(seed)),
        }
    }

    /// Builds a layer seeding the jitter PRNG from the system clock (production).
    pub fn with_clock_seed(inner: T, cfg: RetryConfig, token: CancellationToken) -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        Self::new(inner, cfg, token, seed)
    }

    /// Draws the next full-jitter factor in `[0, 1)`. The lock is released before
    /// returning so it is never held across an `.await`.
    fn next_jitter(&self) -> f64 {
        match self.rng.lock() {
            Ok(mut rng) => rng.next_f64(),
            // A poisoned mutex only happens if a previous draw panicked, which the
            // pure PRNG cannot do; fall back to the midpoint rather than panic.
            Err(_) => 0.5,
        }
    }

    /// Sleeps for `delay`, returning early with `Err` if the token is cancelled
    /// first (`docs/ARCHITECTURE.md` §6.6).
    async fn sleep_or_cancel(&self, delay: std::time::Duration) -> Result<(), LlmError> {
        tokio::select! {
            biased;
            _ = self.token.cancelled() => Err(LlmError::Cancelled),
            _ = tokio::time::sleep(delay) => Ok(()),
        }
    }
}

impl<T: LlmClient + Sync> LlmClient for Retrying<T> {
    async fn generate(&self, mut req: LlmRequest) -> Result<LlmResponse, LlmError> {
        let mut attempt: u32 = 0;
        let mut consecutive_sheds: u32 = 0;
        let mut tried_standard_after_rejection = false;

        loop {
            // Cancellation is checked before every dispatch, so a cancelled run
            // returns promptly without starting another request (§6.6).
            if self.token.is_cancelled() {
                return Err(LlmError::Cancelled);
            }

            let error = match self.inner.generate(req.clone()).await {
                Ok(response) => return Ok(response),
                Err(error) => error,
            };

            // A model that rejects the requested tier is retried once on standard
            // (`docs/LLM.md` §1), independent of the backoff policy.
            if let LlmError::TierUnsupported { .. } = &error {
                if req.service_tier == ServiceTier::Flex && !tried_standard_after_rejection {
                    tracing::warn!("model rejected the flex service tier; retrying on standard");
                    req.service_tier = ServiceTier::Standard;
                    tried_standard_after_rejection = true;
                    consecutive_sheds = 0;
                    continue;
                }
                return Err(error);
            }

            consecutive_sheds = match error.class() {
                ErrorClass::Shed => consecutive_sheds + 1,
                _ => 0,
            };

            // Once on the standard tier there is nowhere to fall back to, so the
            // policy must not keep electing `FallbackTier`; disabling the fallback
            // in the effective config turns a continued shed into ordinary
            // backoff-and-retry (capped by `max_retries`).
            let effective_cfg = if req.service_tier == ServiceTier::Standard {
                RetryConfig {
                    service_tier_fallback: false,
                    ..self.cfg.clone()
                }
            } else {
                self.cfg.clone()
            };

            let jitter = self.next_jitter();
            match decide(&error, attempt, consecutive_sheds, &effective_cfg, jitter) {
                Decision::GiveUp => return Err(error),
                Decision::FallbackTier => {
                    tracing::warn!(
                        "flex tier shed {consecutive_sheds} times; falling back to standard"
                    );
                    req.service_tier = ServiceTier::Standard;
                    consecutive_sheds = 0;
                    // A tier switch is not a backoff attempt: retry immediately on
                    // the new tier without sleeping or spending the retry budget.
                    continue;
                }
                Decision::Retry { delay } => {
                    self.sleep_or_cancel(delay).await?;
                    attempt += 1;
                }
            }
        }
    }
}
