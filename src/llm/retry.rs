//! Pure transport retry policy (`docs/LLM.md` §6). This module is the *policy*
//! only — the `Retrying` layer that applies it with real sleeps and the
//! cancellation token is M3-B. Content retries live in the pipeline, never here
//! (ADR-009).
//!
//! # Jitter
//!
//! Backoff uses `min(backoff_max_ms, backoff_base_ms * 2^attempt)` as the
//! *ceiling*, with **full jitter** applied: the delay is uniform in
//! `[0, ceiling]`. To keep the policy pure and let tests assert exact delays,
//! [`decide`] takes the drawn jitter factor (`f64` in `[0, 1)`) as a parameter
//! rather than drawing it internally (one of the two approaches `docs/LLM.md`
//! §6 permits). The `Retrying` layer (M3-B) draws the factor from [`Xorshift`],
//! which is seeded from the system clock in production and from a fixed seed in
//! tests. A server-provided `Retry-After` / `RetryInfo` delay on a 429 is
//! honored exactly and is *not* jittered.

use std::time::Duration;

use crate::llm::client::{ErrorClass, LlmError};

/// Tunables for the retry policy, taken from the `[llm]` config section
/// (`docs/CONFIG.md`). Kept free of the config type so the policy stays pure and
/// trivially testable; the caller (M3-B) constructs it from `config::Llm`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryConfig {
    pub max_retries: u32,
    pub backoff_base_ms: u64,
    pub backoff_max_ms: u64,
    pub max_sheds_before_fallback: u32,
    pub service_tier_fallback: bool,
}

/// The policy's verdict for one failed attempt (`docs/LLM.md` §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Retry after `delay`.
    Retry { delay: Duration },
    /// Switch this unit from the flex tier to standard, then retry.
    FallbackTier,
    /// Stop; the error is fatal or the retry budget is exhausted.
    GiveUp,
}

/// Decides what to do after attempt number `attempt` (0-based) failed with
/// `err`, having seen `consecutive_sheds` shed errors in a row on this unit.
///
/// `jitter` is the drawn full-jitter factor in `[0, 1)`; see the module docs.
/// Implements every row of the `docs/LLM.md` §6 retry table.
pub fn decide(
    err: &LlmError,
    attempt: u32,
    consecutive_sheds: u32,
    cfg: &RetryConfig,
    jitter: f64,
) -> Decision {
    let exhausted = attempt >= cfg.max_retries;
    let backoff = || full_jitter(backoff_ceiling(attempt, cfg), jitter);

    match err.class() {
        // Permanent errors always give up, regardless of the retry budget.
        ErrorClass::Fatal => Decision::GiveUp,

        // 429: honor a server-provided delay exactly (no jitter); otherwise back off.
        ErrorClass::RateLimited { retry_after } => {
            if exhausted {
                Decision::GiveUp
            } else {
                Decision::Retry {
                    delay: retry_after.unwrap_or_else(backoff),
                }
            }
        }

        // 5xx / timeout / connection / transport: back off and retry until the cap.
        ErrorClass::Retryable => {
            if exhausted {
                Decision::GiveUp
            } else {
                Decision::Retry { delay: backoff() }
            }
        }

        // Flex shed / capacity: after `max_sheds_before_fallback` consecutive
        // sheds, switch to the standard tier if a fallback is configured.
        // Without a fallback there is nowhere to go, so treat it as a plain
        // retryable error (retry until the cap, then give up).
        //
        // Precedence is deliberate: reaching the shed threshold is a distinct
        // tier-switch action, not a retry, so it overrides the retry cap. If
        // `max_sheds_before_fallback > max_retries`, a shed can still fall back
        // after the retry budget is spent (§6 leaves this interaction open).
        ErrorClass::Shed => {
            if cfg.service_tier_fallback && consecutive_sheds >= cfg.max_sheds_before_fallback {
                Decision::FallbackTier
            } else if exhausted {
                Decision::GiveUp
            } else {
                Decision::Retry { delay: backoff() }
            }
        }
    }
}

/// The backoff ceiling for `attempt`: `min(backoff_max_ms, base * 2^attempt)`.
/// Both the shift and the multiply saturate to `u64::MAX` on overflow, so a huge
/// `attempt` yields the configured maximum rather than a wrapped (too-small) value.
fn backoff_ceiling(attempt: u32, cfg: &RetryConfig) -> Duration {
    let scaled = 1u64
        .checked_shl(attempt)
        .and_then(|factor| cfg.backoff_base_ms.checked_mul(factor))
        .unwrap_or(u64::MAX)
        .min(cfg.backoff_max_ms);
    Duration::from_millis(scaled)
}

/// Applies full jitter to a ceiling: a delay uniform in `[0, ceiling]`, picked
/// deterministically from `jitter ∈ [0, 1)`.
fn full_jitter(ceiling: Duration, jitter: f64) -> Duration {
    let factor = jitter.clamp(0.0, 1.0);
    let millis = (ceiling.as_millis() as f64 * factor) as u64;
    Duration::from_millis(millis)
}

/// A tiny xorshift64 PRNG (`docs/ARCHITECTURE.md` §10 — no `rand` crate). Used by
/// the `Retrying` layer to draw a jitter factor; the seed is injectable so tests
/// are deterministic.
#[derive(Debug, Clone)]
pub struct Xorshift {
    state: u64,
}

impl Xorshift {
    /// Creates a generator from `seed`. A zero seed is replaced (xorshift cannot
    /// leave the all-zero state), so every seed yields a usable stream.
    pub fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
        }
    }

    /// Advances the generator and returns the next 64-bit value.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// Returns the next value as an `f64` in `[0, 1)`.
    pub fn next_f64(&mut self) -> f64 {
        // 53 bits of mantissa precision for a uniform value in [0, 1).
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}
