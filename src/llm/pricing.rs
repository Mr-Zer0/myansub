//! Pure cost calculation (`docs/LLM.md` §9, `docs/ARCHITECTURE.md` §4). No I/O.
//! An unknown model (no price entry) reports [`Cost::Unknown`] and never fails
//! the run.

use std::collections::BTreeMap;

use crate::config::PriceEntry;
use crate::llm::client::{ServiceTier, Usage};

/// Prices for one model, in USD per 1,000,000 tokens (`docs/CONFIG.md`,
/// `docs/config.example`). Mirrors [`crate::config::PriceEntry`] but lives in the
/// `llm` layer so pricing stays a pure, self-contained calculation.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelPrice {
    pub input: f64,
    pub cached_input: f64,
    pub output: f64,
    /// Explicit-cache storage rate, in USD per 1,000,000 tokens per hour.
    pub storage_per_hour: f64,
    /// Multiplier applied to the per-token cost on the flex tier.
    pub flex_multiplier: f64,
    /// Optional long-prompt bracket: when `prompt_tokens >= threshold`, use
    /// `long_input` / `long_output` instead of `input` / `output`.
    pub long_prompt_threshold: Option<u64>,
    pub long_input: Option<f64>,
    pub long_output: Option<f64>,
}

impl From<&PriceEntry> for ModelPrice {
    fn from(entry: &PriceEntry) -> Self {
        Self {
            input: entry.input,
            cached_input: entry.cached_input,
            output: entry.output,
            storage_per_hour: entry.storage_per_hour,
            flex_multiplier: entry.flex_multiplier,
            long_prompt_threshold: entry.long_prompt_threshold,
            long_input: entry.long_input,
            long_output: entry.long_output,
        }
    }
}

/// A set of model prices, keyed by model name.
#[derive(Debug, Clone, Default)]
pub struct PriceTable {
    models: BTreeMap<String, ModelPrice>,
}

impl PriceTable {
    /// An empty table (every lookup is unknown).
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces the price for `model`.
    pub fn insert(&mut self, model: impl Into<String>, price: ModelPrice) {
        self.models.insert(model.into(), price);
    }

    /// Returns the price for `model`, if known.
    pub fn get(&self, model: &str) -> Option<&ModelPrice> {
        self.models.get(model)
    }

    /// Builds a price table from the `[pricing]` config map (`docs/CONFIG.md`).
    pub fn from_config(entries: &BTreeMap<String, PriceEntry>) -> Self {
        let models = entries
            .iter()
            .map(|(name, entry)| (name.clone(), ModelPrice::from(entry)))
            .collect();
        Self { models }
    }
}

/// The cost of a call in USD, or [`Cost::Unknown`] when the model has no price
/// entry (`docs/LLM.md` §9).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Cost {
    Known(f64),
    Unknown,
}

/// Per-call token cost (`docs/LLM.md` §9). Components: fresh input, cached input,
/// output (with thinking billed as output), the flex tier multiplier, and the
/// optional long-prompt bracket. Explicit-cache storage is a separate component
/// ([`storage_cost`]). An unknown model yields [`Cost::Unknown`].
pub fn cost(model: &str, tier: ServiceTier, usage: &Usage, table: &PriceTable) -> Cost {
    // A local cache hit consumed no API tokens, so it is always free regardless of
    // the token counts carried by the stored response (`docs/LLM.md` §5). This
    // guards a caller that prices a hit's usage directly from over-charging.
    if usage.cache_hit_local {
        return Cost::Known(0.0);
    }

    let Some(price) = table.get(model) else {
        return Cost::Unknown;
    };

    // Long-prompt bracket: above the threshold, prefer the long input/output
    // rates (falling back to the base rate if a long rate is not configured).
    let long = price
        .long_prompt_threshold
        .is_some_and(|threshold| usage.prompt_tokens >= threshold);
    let input_rate = if long {
        price.long_input.unwrap_or(price.input)
    } else {
        price.input
    };
    let output_rate = if long {
        price.long_output.unwrap_or(price.output)
    } else {
        price.output
    };

    // `cached_tokens` is a subset of `prompt_tokens`; the remainder is fresh.
    let fresh = usage.prompt_tokens.saturating_sub(usage.cached_tokens) as f64;
    let cached = usage.cached_tokens as f64;
    // Thinking tokens are billed as output (`docs/LLM.md` §9).
    let output = (usage.output_tokens + usage.thinking_tokens) as f64;

    let per_token = fresh * input_rate + cached * price.cached_input + output * output_rate;
    let multiplier = match tier {
        ServiceTier::Flex => price.flex_multiplier,
        ServiceTier::Standard => 1.0,
    };

    Cost::Known(per_token / 1_000_000.0 * multiplier)
}

/// Explicit-cache storage cost (`docs/LLM.md` §9): `tokens` held for `hours` at
/// the model's `storage_per_hour` rate (USD per 1M tokens per hour). Not subject
/// to the flex multiplier. An unknown model yields [`Cost::Unknown`].
pub fn storage_cost(model: &str, tokens: u64, hours: f64, table: &PriceTable) -> Cost {
    match table.get(model) {
        Some(price) => Cost::Known(tokens as f64 * price.storage_per_hour * hours / 1_000_000.0),
        None => Cost::Unknown,
    }
}
