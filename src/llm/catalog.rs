//! The model catalog (`docs/LLM.md` §10.9, ADR-051f).
//!
//! A [`ModelCatalog`] supplies model validation, context length, supported
//! parameters (structured-output support and reasoning levels), and prices. The
//! Gemini implementation ([`GeminiCatalog`]) is backed by the `[pricing]` config
//! table (today's behaviour, unchanged observable prices); the OpenRouter
//! implementation ([`OpenRouterCatalog`](crate::llm::openrouter)) is backed by the
//! fetched `/models` document. `pricing.rs` reads prices only through the
//! [`PriceSource`](crate::llm::pricing::PriceSource) seam that catalogs implement,
//! so a dry-run estimate is priced the same way for both providers.

use thiserror::Error;

use crate::llm::pricing::{ModelPrice, PriceSource, PriceTable, bare_model};

/// A reasoning effort level, ordered low→high so the OpenRouter adapter can pick
/// the "nearest supported lower level" (`docs/LLM.md` §10.4). The `Ord` derive
/// follows declaration order: `Minimal < Low < Medium < High`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReasoningLevel {
    Minimal,
    Low,
    Medium,
    High,
}

impl ReasoningLevel {
    /// The lowercase label used by the OpenRouter `reasoning.effort` wire field.
    pub fn label(self) -> &'static str {
        match self {
            ReasoningLevel::Minimal => "minimal",
            ReasoningLevel::Low => "low",
            ReasoningLevel::Medium => "medium",
            ReasoningLevel::High => "high",
        }
    }
}

/// An error from a [`ModelCatalog`] (`docs/LLM.md` §10.9/§10.12).
#[derive(Debug, Error)]
pub enum CatalogError {
    /// The model id is not in the catalog. `suggestions` lists up to five of the
    /// closest known ids (empty when none are close). `main` maps this to exit 2.
    #[error("unknown model `{id}`{}", suggestion_tail(.suggestions))]
    UnknownModel {
        id: String,
        suggestions: Vec<String>,
    },
    /// The model exists but does not support structured output; the pipeline needs
    /// it, so this fails at startup (exit 2, `docs/LLM.md` §10.3).
    #[error("model `{id}` does not support structured output (json_schema)")]
    NoStructuredOutput { id: String },
    /// The catalog could not be loaded (no cache, offline). `main` maps this to a
    /// clear run-stopping error.
    #[error("model catalog unavailable: {0}")]
    Unavailable(String),
}

fn suggestion_tail(suggestions: &[String]) -> String {
    if suggestions.is_empty() {
        String::new()
    } else {
        format!(" (did you mean: {})", suggestions.join(", "))
    }
}

/// The model catalog trait (`docs/LLM.md` §10.9, ADR-051f). Methods take a model
/// id that may be a bare id or a full `provider:model` reference; implementations
/// strip the provider half before looking up.
pub trait ModelCatalog {
    /// Confirms the id is a usable model: known to the catalog and supporting
    /// structured output. An unknown id yields [`CatalogError::UnknownModel`]; a
    /// known id without structured-output support yields
    /// [`CatalogError::NoStructuredOutput`].
    fn validate(&self, id: &str) -> Result<(), CatalogError>;

    /// The model's context length in tokens, when known.
    fn context_length(&self, id: &str) -> Option<u64>;

    /// Whether the model supports structured output (`response_format` json_schema).
    fn supports_structured_output(&self, id: &str) -> bool;

    /// The reasoning effort levels the model supports, low→high. Empty when the
    /// model has no reasoning control.
    fn supported_reasoning_levels(&self, id: &str) -> Vec<ReasoningLevel>;

    /// The per-million-token price for the model, when known.
    fn price(&self, id: &str) -> Option<ModelPrice>;
}

/// The Gemini catalog, backed by the `[pricing]` config table (`docs/LLM.md` §9,
/// ADR-051f). Gemini model ids are not catalog-validated (a missing price is a
/// warning, not an error, `docs/CONFIG.md` §5), so [`validate`](GeminiCatalog::validate)
/// accepts any id; Gemini always supports structured output and reasoning.
#[derive(Debug, Clone, Default)]
pub struct GeminiCatalog {
    prices: PriceTable,
}

impl GeminiCatalog {
    /// Wraps the Gemini `[pricing]` table.
    pub fn new(prices: PriceTable) -> Self {
        Self { prices }
    }
}

impl ModelCatalog for GeminiCatalog {
    fn validate(&self, _id: &str) -> Result<(), CatalogError> {
        // Gemini does not maintain a validating catalog; an unknown model is a
        // cost warning (`docs/CONFIG.md` §5), not a startup failure. Structured
        // output is always supported.
        Ok(())
    }

    fn context_length(&self, _id: &str) -> Option<u64> {
        // Not tracked for Gemini (the price table has no context length).
        None
    }

    fn supports_structured_output(&self, _id: &str) -> bool {
        true
    }

    fn supported_reasoning_levels(&self, _id: &str) -> Vec<ReasoningLevel> {
        vec![
            ReasoningLevel::Minimal,
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
        ]
    }

    fn price(&self, id: &str) -> Option<ModelPrice> {
        self.prices.get(&bare_model(id)).cloned()
    }
}

impl PriceSource for GeminiCatalog {
    fn model_price(&self, model: &str) -> Option<ModelPrice> {
        self.prices.get(&bare_model(model)).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price() -> ModelPrice {
        ModelPrice {
            input: 2.0,
            cached_input: 0.5,
            output: 8.0,
            storage_per_hour: 1.0,
            flex_multiplier: 0.5,
            long_prompt_threshold: None,
            long_input: None,
            long_output: None,
        }
    }

    fn catalog() -> GeminiCatalog {
        let mut table = PriceTable::new();
        table.insert("gemini-flash-latest", price());
        GeminiCatalog::new(table)
    }

    #[test]
    fn gemini_catalog_accepts_any_id() {
        // AC: M13-AC7 — Gemini never hard-fails validation (a missing price is a
        // warning, not an error).
        assert!(catalog().validate("gemini-flash-latest").is_ok());
        assert!(catalog().validate("no-such-model").is_ok());
    }

    #[test]
    fn gemini_catalog_supports_structured_output() {
        // AC: M13-AC7 — Gemini always supports structured output.
        assert!(catalog().supports_structured_output("gemini-flash-latest"));
    }

    #[test]
    fn gemini_catalog_price_strips_provider_prefix() {
        // AC: M13-AC7 — prices are read through the catalog, keyed by the bare model
        // id; a full `provider:model` reference resolves to the same entry.
        assert_eq!(catalog().price("gemini-flash-latest"), Some(price()));
        assert_eq!(catalog().price("gemini:gemini-flash-latest"), Some(price()));
        assert_eq!(catalog().price("gemini:no-such-model"), None);
    }

    #[test]
    fn reasoning_levels_order_low_to_high() {
        assert!(ReasoningLevel::Minimal < ReasoningLevel::Low);
        assert!(ReasoningLevel::Low < ReasoningLevel::Medium);
        assert!(ReasoningLevel::Medium < ReasoningLevel::High);
    }
}
