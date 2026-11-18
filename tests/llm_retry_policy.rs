//! Transport retry policy (M3-AC2, `docs/LLM.md` §6). One test per row of the
//! retry table, with exact delays asserted using an injected jitter factor.

use std::time::Duration;

use myansub::llm::client::LlmError;
use myansub::llm::retry::{Decision, RetryConfig, decide};

fn cfg() -> RetryConfig {
    RetryConfig {
        max_retries: 6,
        backoff_base_ms: 1000,
        backoff_max_ms: 60000,
        max_sheds_before_fallback: 3,
        service_tier_fallback: true,
    }
}

// AC: M3-AC2 — 429 with a server-provided Retry-After uses that delay exactly.
#[test]
fn rate_limited_with_retry_after_uses_that_delay() {
    let err = LlmError::RateLimited {
        retry_after: Some(Duration::from_secs(7)),
    };
    // Jitter must NOT alter an explicit retry-after.
    let decision = decide(&err, 0, 0, &cfg(), 0.5);
    assert_eq!(
        decision,
        Decision::Retry {
            delay: Duration::from_secs(7)
        }
    );
}

// AC: M3-AC2 — 429 without a retry-after falls back to jittered backoff.
#[test]
fn rate_limited_without_retry_after_uses_backoff() {
    let err = LlmError::RateLimited { retry_after: None };
    // attempt 0 → ceiling 1000ms; full jitter 0.5 → 500ms.
    let decision = decide(&err, 0, 0, &cfg(), 0.5);
    assert_eq!(
        decision,
        Decision::Retry {
            delay: Duration::from_millis(500)
        }
    );
}

// AC: M3-AC2 — 5xx retries with jittered backoff; exponent grows with attempt.
#[test]
fn server_error_retries_with_backoff() {
    let err = LlmError::Server { status: 503 };
    // attempt 1 → ceiling 2000ms; jitter 0.25 → 500ms.
    let decision = decide(&err, 1, 0, &cfg(), 0.25);
    assert_eq!(
        decision,
        Decision::Retry {
            delay: Duration::from_millis(500)
        }
    );
}

// AC: M3-AC2 — timeout retries with backoff.
#[test]
fn timeout_retries_with_backoff() {
    let decision = decide(&LlmError::Timeout, 2, 0, &cfg(), 0.5);
    // attempt 2 → ceiling 4000ms; jitter 0.5 → 2000ms.
    assert_eq!(
        decision,
        Decision::Retry {
            delay: Duration::from_millis(2000)
        }
    );
}

// AC: M3-AC2 — connection error retries with backoff.
#[test]
fn connection_error_retries_with_backoff() {
    let err = LlmError::Connection("dns".to_string());
    let decision = decide(&err, 0, 0, &cfg(), 0.0);
    assert_eq!(
        decision,
        Decision::Retry {
            delay: Duration::ZERO
        }
    );
}

// AC: M3-AC2 — the backoff ceiling is capped at backoff_max_ms.
#[test]
fn backoff_is_capped_at_max() {
    let capped = RetryConfig {
        max_retries: 20,
        backoff_base_ms: 1000,
        backoff_max_ms: 1500,
        max_sheds_before_fallback: 3,
        service_tier_fallback: true,
    };
    let err = LlmError::Server { status: 500 };
    // attempt 4 → base*2^4 = 16000ms, clamped to the 1500ms ceiling; jitter 1.0.
    let decision = decide(&err, 4, 0, &capped, 1.0);
    assert_eq!(
        decision,
        Decision::Retry {
            delay: Duration::from_millis(1500)
        }
    );
}

// AC: M3-AC2 — a shed below the threshold retries with backoff.
#[test]
fn shed_below_threshold_retries() {
    let decision = decide(&LlmError::Shed, 0, 1, &cfg(), 0.5);
    assert_eq!(
        decision,
        Decision::Retry {
            delay: Duration::from_millis(500)
        }
    );
}

// AC: M3-AC2 — at the shed threshold, with fallback enabled, switch tiers.
#[test]
fn shed_at_threshold_falls_back_to_standard_tier() {
    let decision = decide(&LlmError::Shed, 0, 3, &cfg(), 0.5);
    assert_eq!(decision, Decision::FallbackTier);
}

// AC: M3-AC2 — with fallback disabled, a shed at the threshold is just a retry
// (there is nowhere to fall back to), until the retry budget is spent.
#[test]
fn shed_at_threshold_without_fallback_retries() {
    let mut cfg = cfg();
    cfg.service_tier_fallback = false;
    let decision = decide(&LlmError::Shed, 0, 5, &cfg, 0.5);
    assert_eq!(
        decision,
        Decision::Retry {
            delay: Duration::from_millis(500)
        }
    );
}

// AC: M3-AC2 — the shed threshold is `>=`, so counts beyond the exact boundary
// still fall back (locks the branch past the boundary).
#[test]
fn shed_well_above_threshold_falls_back() {
    let cfg = cfg(); // threshold 3, fallback enabled
    let decision = decide(&LlmError::Shed, 0, 5, &cfg, 0.5);
    assert_eq!(decision, Decision::FallbackTier);
}

// AC: M3-AC2 — a shed with fallback disabled gives up once the retry budget is
// exhausted (the shed exhaustion path, distinct from the server-error path).
#[test]
fn shed_without_fallback_gives_up_when_exhausted() {
    let mut cfg = cfg();
    cfg.service_tier_fallback = false;
    let decision = decide(&LlmError::Shed, cfg.max_retries, 10, &cfg, 0.5);
    assert_eq!(decision, Decision::GiveUp);
}

// AC: M3-AC2 — fatal 4xx errors give up immediately.
#[test]
fn fatal_request_error_gives_up() {
    for status in [400u16, 401, 403, 404] {
        let err = LlmError::Fatal {
            status,
            message: "bad".to_string(),
        };
        assert_eq!(decide(&err, 0, 0, &cfg(), 0.5), Decision::GiveUp);
    }
}

// AC: M3-AC2 — a decode/schema error gives up (content retries are the pipeline's).
#[test]
fn decode_error_gives_up() {
    let err = LlmError::Decode("not json".to_string());
    assert_eq!(decide(&err, 0, 0, &cfg(), 0.5), Decision::GiveUp);
}

// AC: M3-AC2 — reaching the retry cap gives up even for a retryable error.
#[test]
fn retry_budget_exhausted_gives_up() {
    let err = LlmError::Server { status: 500 };
    assert_eq!(decide(&err, 6, 0, &cfg(), 0.5), Decision::GiveUp);
}
