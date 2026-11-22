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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;
use thiserror::Error;

use crate::llm::pricing::{ModelPrice, PriceSource, PriceTable, bare_model};

/// How long the on-disk OpenRouter `/models` cache stays fresh (`docs/LLM.md`
/// §10.9, ADR-052b).
pub const CATALOG_TTL: Duration = Duration::from_secs(24 * 60 * 60);

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

    /// Lists the priced Gemini models for `models list` (M13-AC16), optionally
    /// filtered by a substring of the id. Gemini entries come from the price table;
    /// context length is not tracked and structured output is always supported.
    pub fn list(&self, filter: Option<&str>) -> Vec<ModelInfo> {
        self.prices
            .iter()
            .filter(|(id, _)| filter.is_none_or(|f| id.contains(f)))
            .map(|(id, price)| ModelInfo {
                id: id.clone(),
                input_per_m: Some(price.input),
                output_per_m: Some(price.output),
                context_length: None,
                structured_output: true,
            })
            .collect()
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

/// A row for `myansub models list` (`docs/CLI.md`, M13-AC16): the model id, input
/// and output price per 1M tokens (when known), context length, and whether it
/// supports structured output.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    pub input_per_m: Option<f64>,
    pub output_per_m: Option<f64>,
    pub context_length: Option<u64>,
    pub structured_output: bool,
}

/// One model's catalog entry parsed from the OpenRouter `/models` document
/// (`docs/LLM.md` §10.9).
#[derive(Debug, Clone, PartialEq)]
struct ModelEntry {
    context_length: Option<u64>,
    structured_output: bool,
    reasoning_levels: Vec<ReasoningLevel>,
    price: Option<ModelPrice>,
}

/// The OpenRouter catalog, backed by the fetched `/models` document (`docs/LLM.md`
/// §10.9, ADR-051f/052b). Built from the raw JSON body; the fetch-and-cache
/// orchestration is [`OpenRouterCatalog::load`].
#[derive(Debug, Clone, Default)]
pub struct OpenRouterCatalog {
    models: BTreeMap<String, ModelEntry>,
}

impl OpenRouterCatalog {
    /// Parses a `/models` response body into a catalog (`docs/LLM.md` §10.9). All
    /// OpenRouter catalog field names are confined to this constructor.
    pub fn from_models_bytes(bytes: &[u8]) -> Result<Self, CatalogError> {
        let value: Value = serde_json::from_slice(bytes)
            .map_err(|e| CatalogError::Unavailable(format!("invalid /models JSON: {e}")))?;
        let data = value
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| CatalogError::Unavailable("/models had no `data` array".to_string()))?;

        let mut models = BTreeMap::new();
        for entry in data {
            let Some(id) = entry.get("id").and_then(Value::as_str) else {
                continue;
            };
            models.insert(id.to_string(), parse_entry(entry));
        }
        Ok(Self { models })
    }

    /// Loads the catalog, fetching `GET {base_url}/models` and caching it to
    /// `cache_path` for [`CATALOG_TTL`] (atomic write, ADR-052b). A fresh cache is
    /// used without a fetch; on a fetch failure a present-but-stale cache is used with
    /// one warning; with no cache and no network the load fails clearly
    /// (`docs/LLM.md` §10.9). `now` is injected so tests control freshness.
    pub async fn load(
        client: &reqwest::Client,
        base_url: &str,
        cache_path: &Path,
        now: SystemTime,
    ) -> Result<Self, CatalogError> {
        if let Some(bytes) = read_fresh_cache(cache_path, now)
            && let Ok(catalog) = Self::from_models_bytes(&bytes)
        {
            return Ok(catalog);
        }

        match fetch_models(client, base_url).await {
            Ok(bytes) => {
                let catalog = Self::from_models_bytes(&bytes)?;
                if let Err(e) = write_cache(cache_path, &bytes) {
                    tracing::warn!("could not cache OpenRouter catalog: {e}");
                }
                Ok(catalog)
            }
            Err(fetch_err) => match read_cache(cache_path) {
                Some(bytes) => {
                    tracing::warn!(
                        "OpenRouter catalog fetch failed ({fetch_err}); using the stale cache"
                    );
                    Self::from_models_bytes(&bytes)
                }
                None => Err(CatalogError::Unavailable(format!(
                    "could not fetch the OpenRouter catalog and no cache is present: {fetch_err}"
                ))),
            },
        }
    }

    /// Lists the catalog models for `models list` (M13-AC16), optionally filtered by
    /// a substring of the id.
    pub fn list(&self, filter: Option<&str>) -> Vec<ModelInfo> {
        self.models
            .iter()
            .filter(|(id, _)| filter.is_none_or(|f| id.contains(f)))
            .map(|(id, entry)| ModelInfo {
                id: id.clone(),
                input_per_m: entry.price.as_ref().map(|p| p.input),
                output_per_m: entry.price.as_ref().map(|p| p.output),
                context_length: entry.context_length,
                structured_output: entry.structured_output,
            })
            .collect()
    }

    /// Up to five known ids closest to `query` (by edit distance), for an
    /// unknown-model error (`docs/LLM.md` §10.9).
    fn closest_ids(&self, query: &str) -> Vec<String> {
        let mut scored: Vec<(usize, &String)> = self
            .models
            .keys()
            .map(|id| (edit_distance(query, id), id))
            .collect();
        scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
        scored
            .into_iter()
            .take(5)
            .map(|(_, id)| id.clone())
            .collect()
    }
}

impl ModelCatalog for OpenRouterCatalog {
    fn validate(&self, id: &str) -> Result<(), CatalogError> {
        let bare = bare_model(id);
        match self.models.get(&bare) {
            None => Err(CatalogError::UnknownModel {
                id: bare.clone(),
                suggestions: self.closest_ids(&bare),
            }),
            Some(entry) if !entry.structured_output => {
                Err(CatalogError::NoStructuredOutput { id: bare })
            }
            Some(_) => Ok(()),
        }
    }

    fn context_length(&self, id: &str) -> Option<u64> {
        self.models
            .get(&bare_model(id))
            .and_then(|e| e.context_length)
    }

    fn supports_structured_output(&self, id: &str) -> bool {
        self.models
            .get(&bare_model(id))
            .map(|e| e.structured_output)
            .unwrap_or(false)
    }

    fn supported_reasoning_levels(&self, id: &str) -> Vec<ReasoningLevel> {
        self.models
            .get(&bare_model(id))
            .map(|e| e.reasoning_levels.clone())
            .unwrap_or_default()
    }

    fn price(&self, id: &str) -> Option<ModelPrice> {
        self.models
            .get(&bare_model(id))
            .and_then(|e| e.price.clone())
    }
}

impl PriceSource for OpenRouterCatalog {
    fn model_price(&self, model: &str) -> Option<ModelPrice> {
        self.price(model)
    }
}

/// Parses one `/models` entry. OpenRouter prices are USD per token (as strings);
/// [`ModelPrice`] is USD per 1M tokens, so each is scaled by 1e6 (`docs/LLM.md`
/// §10.9). A model exposing `structured_outputs` or `response_format` supports
/// structured output; `reasoning` in `supported_parameters` means it accepts a
/// reasoning effort (`[verify]` the granular levels — low/medium/high are assumed).
fn parse_entry(entry: &Value) -> ModelEntry {
    let params: Vec<&str> = entry
        .get("supported_parameters")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let structured_output =
        params.contains(&"structured_outputs") || params.contains(&"response_format");
    let reasoning_levels = if params.contains(&"reasoning") {
        vec![
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
        ]
    } else {
        Vec::new()
    };

    let price = entry.get("pricing").map(|p| {
        let rate = |key: &str| -> f64 {
            p.get(key)
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0)
                * 1_000_000.0
        };
        let input = rate("prompt");
        let output = rate("completion");
        // Cached-read price, falling back to the fresh input rate when absent.
        let cached_input = p
            .get("input_cache_read")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<f64>().ok())
            .map(|v| v * 1_000_000.0)
            .unwrap_or(input);
        ModelPrice {
            input,
            cached_input,
            output,
            storage_per_hour: 0.0,
            // OpenRouter cost is the actual billed amount; there is no flex
            // multiplier on the price table (the floor tier routes to cheap
            // endpoints at the API level, not via a local multiplier).
            flex_multiplier: 1.0,
            long_prompt_threshold: None,
            long_input: None,
            long_output: None,
        }
    });

    ModelEntry {
        context_length: entry.get("context_length").and_then(Value::as_u64),
        structured_output,
        reasoning_levels,
        price,
    }
}

/// `GET {base_url}/models`, returning the raw body bytes.
async fn fetch_models(client: &reqwest::Client, base_url: &str) -> Result<Vec<u8>, String> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let response = client.get(&url).send().await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status().as_u16()));
    }
    response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| e.to_string())
}

/// Reads the cache only when it exists and is younger than [`CATALOG_TTL`].
fn read_fresh_cache(path: &Path, now: SystemTime) -> Option<Vec<u8>> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let age = now.duration_since(modified).ok()?;
    if age < CATALOG_TTL {
        std::fs::read(path).ok()
    } else {
        None
    }
}

/// Reads the cache regardless of age (for the stale-fallback path).
fn read_cache(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

/// Writes the cache atomically (`store::atomic`, ADR-052b), creating the parent
/// directory first.
fn write_cache(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    crate::store::atomic::atomic_write(path, bytes).map_err(|e| e.to_string())
}

/// The OpenRouter catalog cache path `<cache_dir>/myansub/openrouter-models.json`
/// (ADR-052b). Resolved by hand — `XDG_CACHE_HOME`/`HOME/.cache` on Unix,
/// `LOCALAPPDATA`/`APPDATA` on Windows — with no `dirs` crate. `None` when no base
/// directory is set.
pub fn catalog_cache_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .or_else(|| std::env::var_os("APPDATA"))
            .map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    };
    base.map(|b| b.join("myansub").join("openrouter-models.json"))
}

/// Levenshtein edit distance between two strings (for closest-id suggestions). Pure,
/// no external crate.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
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

    fn openrouter_catalog() -> OpenRouterCatalog {
        let body = serde_json::json!({
            "data": [
                {
                    "id": "anthropic/claude-sonnet-5.5",
                    "context_length": 200000,
                    "pricing": { "prompt": "0.000003", "completion": "0.000015" },
                    "supported_parameters": ["response_format", "structured_outputs", "reasoning"]
                },
                {
                    "id": "openai/gpt-oss-chat",
                    "context_length": 32768,
                    "pricing": { "prompt": "0.0000005", "completion": "0.0000015" },
                    "supported_parameters": ["max_tokens", "tools"]
                }
            ]
        });
        OpenRouterCatalog::from_models_bytes(body.to_string().as_bytes()).expect("parse")
    }

    #[test]
    fn openrouter_catalog_parses_price_per_million() {
        // AC: M13-AC7/AC12 — OpenRouter per-token prices scale to per-1M.
        let price = openrouter_catalog()
            .price("anthropic/claude-sonnet-5.5")
            .expect("priced");
        assert!((price.input - 3.0).abs() < 1e-9, "input {}", price.input);
        assert!(
            (price.output - 15.0).abs() < 1e-9,
            "output {}",
            price.output
        );
    }

    #[test]
    fn openrouter_catalog_validates_structured_output() {
        // AC: M13-AC9 — a model without structured-output support fails validation.
        let catalog = openrouter_catalog();
        assert!(catalog.validate("anthropic/claude-sonnet-5.5").is_ok());
        let err = catalog
            .validate("openai/gpt-oss-chat")
            .expect_err("no structured output");
        assert!(
            matches!(err, CatalogError::NoStructuredOutput { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn openrouter_catalog_unknown_id_suggests_closest() {
        // AC: M13-AC12 — an unknown id lists up to five closest ids.
        let err = openrouter_catalog()
            .validate("anthropic/claude-sonnet-5")
            .expect_err("unknown id");
        match err {
            CatalogError::UnknownModel { suggestions, .. } => {
                assert!(
                    suggestions.contains(&"anthropic/claude-sonnet-5.5".to_string()),
                    "closest should be suggested: {suggestions:?}"
                );
                assert!(suggestions.len() <= 5);
            }
            other => panic!("expected UnknownModel, got {other:?}"),
        }
    }

    #[test]
    fn openrouter_catalog_reports_context_and_reasoning() {
        let catalog = openrouter_catalog();
        assert_eq!(
            catalog.context_length("anthropic/claude-sonnet-5.5"),
            Some(200000)
        );
        assert_eq!(
            catalog.supported_reasoning_levels("anthropic/claude-sonnet-5.5"),
            vec![
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High
            ]
        );
        assert!(
            catalog
                .supported_reasoning_levels("openai/gpt-oss-chat")
                .is_empty()
        );
    }

    #[test]
    fn openrouter_catalog_list_filters_by_substring() {
        // AC: M13-AC16 — list filters by a substring of the id and reports price,
        // context length, and structured-output support.
        let rows = openrouter_catalog().list(Some("anthropic"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "anthropic/claude-sonnet-5.5");
        assert_eq!(rows[0].context_length, Some(200000));
        assert!(rows[0].structured_output);
        assert_eq!(rows[0].input_per_m, Some(3.0));
    }

    #[test]
    fn gemini_catalog_list_from_price_table() {
        // AC: M13-AC16 — Gemini list entries come from the price table.
        let rows = catalog().list(None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "gemini-flash-latest");
        assert!(rows[0].structured_output);
    }

    #[test]
    fn free_model_is_priced_zero() {
        // AC: M13-AC15 — a `:free` model is accepted and estimated at $0.
        let body = serde_json::json!({
            "data": [ {
                "id": "meta-llama/llama-3.1-8b-instruct:free",
                "pricing": { "prompt": "0", "completion": "0" },
                "supported_parameters": ["response_format"]
            } ]
        });
        let catalog =
            OpenRouterCatalog::from_models_bytes(body.to_string().as_bytes()).expect("parse");
        let id = "meta-llama/llama-3.1-8b-instruct:free";
        assert!(catalog.validate(id).is_ok());
        let price = catalog.price(id).expect("priced");
        assert_eq!(price.input, 0.0);
        assert_eq!(price.output, 0.0);
    }
}
