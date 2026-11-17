//! Pricing (M3-AC5, `docs/LLM.md` §9). Each cost component is checked against a
//! known price table; an unknown model reports `Unknown` and never panics.

use myansub::llm::client::{ServiceTier, Usage};
use myansub::llm::pricing::{Cost, ModelPrice, PriceTable, cost, storage_cost};

const MODEL: &str = "gemini-flash-latest";

fn price() -> ModelPrice {
    ModelPrice {
        input: 2.0,        // USD per 1M input tokens
        cached_input: 0.5, // USD per 1M cached tokens
        output: 8.0,       // USD per 1M output tokens
        storage_per_hour: 1.0,
        flex_multiplier: 0.5,
        long_prompt_threshold: Some(200_000),
        long_input: Some(4.0),
        long_output: Some(12.0),
    }
}

fn table() -> PriceTable {
    let mut t = PriceTable::new();
    t.insert(MODEL, price());
    t
}

fn assert_known(cost: Cost, expected: f64) {
    match cost {
        Cost::Known(usd) => assert!(
            (usd - expected).abs() < 1e-9,
            "expected {expected} USD, got {usd}"
        ),
        Cost::Unknown => panic!("expected Known({expected}), got Unknown"),
    }
}

// AC: M3-AC5 — fresh input tokens are billed at the input rate.
// Prompt size is kept below the long-prompt threshold to isolate the base rate.
#[test]
fn input_tokens_billed_at_input_rate() {
    let usage = Usage {
        prompt_tokens: 100_000,
        cached_tokens: 0,
        output_tokens: 0,
        thinking_tokens: 0,
        cache_hit_local: false,
    };
    // 100k * 2.0 / 1M = 0.2
    assert_known(cost(MODEL, ServiceTier::Standard, &usage, &table()), 0.2);
}

// AC: M3-AC5 — cached tokens are billed at the cached rate; the rest at input.
#[test]
fn cached_tokens_billed_at_cached_rate() {
    let usage = Usage {
        prompt_tokens: 100_000,
        cached_tokens: 40_000,
        output_tokens: 0,
        thinking_tokens: 0,
        cache_hit_local: false,
    };
    // fresh 60k * 2.0 + cached 40k * 0.5 = 120k + 20k = 140k tokens·USD/1M = 0.14
    assert_known(cost(MODEL, ServiceTier::Standard, &usage, &table()), 0.14);
}

// AC: M3-AC5 — output tokens are billed at the output rate.
#[test]
fn output_tokens_billed_at_output_rate() {
    let usage = Usage {
        prompt_tokens: 0,
        cached_tokens: 0,
        output_tokens: 500_000,
        thinking_tokens: 0,
        cache_hit_local: false,
    };
    assert_known(cost(MODEL, ServiceTier::Standard, &usage, &table()), 4.0);
}

// AC: M3-AC5 — thinking tokens are billed as output.
#[test]
fn thinking_tokens_billed_as_output() {
    let usage = Usage {
        prompt_tokens: 0,
        cached_tokens: 0,
        output_tokens: 500_000,
        thinking_tokens: 500_000,
        cache_hit_local: false,
    };
    // (500k + 500k) * 8.0 / 1M = 8.0
    assert_known(cost(MODEL, ServiceTier::Standard, &usage, &table()), 8.0);
}

// AC: M3-AC5 — the flex multiplier halves the per-token cost.
#[test]
fn flex_tier_applies_multiplier() {
    let usage = Usage {
        prompt_tokens: 100_000,
        cached_tokens: 0,
        output_tokens: 0,
        thinking_tokens: 0,
        cache_hit_local: false,
    };
    // (100k * 2.0 / 1M) * 0.5 = 0.2 * 0.5 = 0.1
    assert_known(cost(MODEL, ServiceTier::Flex, &usage, &table()), 0.1);
}

// AC: M3-AC5 — above the long-prompt threshold, the long bracket prices apply.
#[test]
fn long_prompt_bracket_applies_above_threshold() {
    let usage = Usage {
        prompt_tokens: 1_000_000, // >= 200_000 threshold
        cached_tokens: 0,
        output_tokens: 1_000_000,
        thinking_tokens: 0,
        cache_hit_local: false,
    };
    // long_input 4.0 * 1M + long_output 12.0 * 1M = 4.0 + 12.0 = 16.0
    assert_known(cost(MODEL, ServiceTier::Standard, &usage, &table()), 16.0);
}

// AC: M3-AC5 — at exactly the threshold the long bracket applies (`>=` semantics).
#[test]
fn long_prompt_bracket_applies_at_exact_threshold() {
    let usage = Usage {
        prompt_tokens: 200_000, // == long_prompt_threshold
        cached_tokens: 0,
        output_tokens: 0,
        thinking_tokens: 0,
        cache_hit_local: false,
    };
    // long_input 4.0 * 200k / 1M = 0.8 (base input 2.0 would give 0.4)
    assert_known(cost(MODEL, ServiceTier::Standard, &usage, &table()), 0.8);
}

// AC: M3-AC5 — below the threshold, the base prices apply.
#[test]
fn base_bracket_applies_below_threshold() {
    let usage = Usage {
        prompt_tokens: 100_000, // < 200_000 threshold
        cached_tokens: 0,
        output_tokens: 100_000,
        thinking_tokens: 0,
        cache_hit_local: false,
    };
    // 0.1M*2.0 + 0.1M*8.0 = 0.2 + 0.8 = 1.0
    assert_known(cost(MODEL, ServiceTier::Standard, &usage, &table()), 1.0);
}

// AC: M3-AC5 — explicit-cache storage is its own component.
#[test]
fn storage_cost_uses_storage_rate() {
    // 1M tokens * 1.0 USD/1M/hour * 3 hours = 3.0
    assert_known(storage_cost(MODEL, 1_000_000, 3.0, &table()), 3.0);
}

// AC: M3-AC5 — an unknown model reports Unknown, for both cost and storage.
#[test]
fn unknown_model_reports_unknown() {
    let usage = Usage {
        prompt_tokens: 1_000_000,
        cached_tokens: 0,
        output_tokens: 0,
        thinking_tokens: 0,
        cache_hit_local: false,
    };
    assert_eq!(
        cost("no-such-model", ServiceTier::Standard, &usage, &table()),
        Cost::Unknown
    );
    assert_eq!(
        storage_cost("no-such-model", 1_000_000, 1.0, &table()),
        Cost::Unknown
    );
}
