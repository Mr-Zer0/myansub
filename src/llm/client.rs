//! The logical LLM client surface: the [`LlmClient`] trait, the request/response
//! types, [`Usage`], and [`LlmError`] with its [`ErrorClass`] mapping.
//!
//! These types are *logical* and provider-agnostic. Gemini wire field names and
//! JSON mapping live in `llm/gemini.rs` (M3-B), never here (see
//! `docs/ARCHITECTURE.md` §5 and `docs/LLM.md` — "Keep every API-specific name
//! inside `llm/gemini.rs`").
//!
//! The trait method returns `impl Future<..> + Send` rather than using
//! `async-trait`, so the layer stack (`Metered<Cached<Retrying<Transport>>>`)
//! composes with static dispatch and futures can be spawned on the
//! multi-threaded runtime (`docs/ARCHITECTURE.md` §5, ADR-008).

use std::future::Future;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A secret string (e.g. an API key) whose `Debug` and `Display` both render
/// `<redacted>`, so it can never leak through structured logging, `anyhow`
/// context, or a `#[derive(Debug)]` on an enclosing struct
/// (`CLAUDE.md` "Never log API keys", `docs/ARCHITECTURE.md` §9). The raw value is
/// reachable only through the explicit [`Secret::expose`] accessor.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wraps a secret value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the raw secret. Call only where the value must be used (e.g. to set
    /// an auth header); never log the result.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

/// The neutral service tier carried by a request (`docs/LLM.md` §5/§10, ADR-051b).
///
/// The neutral values are `flex | default | floor`; `Standard` IS the neutral
/// `default` (kept under its original name to avoid churn across the Gemini path).
/// `Flex` is roughly half the standard price but may be shed under load; `Floor`
/// (new, M13) is "cheapest endpoint" and is only produced by the OpenRouter path.
/// Each transport maps these to its own wire representation: Gemini maps
/// `Flex→flex`, `Standard→standard`, `Floor→flex` (flex is Gemini's cheapest);
/// OpenRouter maps them to its tier/sort behaviour (`docs/LLM.md` §10.7). The wire
/// field name stays inside each adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceTier {
    Flex,
    Standard,
    Floor,
}

impl ServiceTier {
    /// The lowercase label used in config, the Gemini wire tier, and the
    /// `llm_calls.service_tier` column. `Standard` stays `"standard"` (Gemini's
    /// unchanged vocabulary); the OpenRouter config surface speaks of the same
    /// variant as the neutral `"default"` (`docs/LLM.md` §10.7) and maps it here.
    pub fn label(self) -> &'static str {
        match self {
            ServiceTier::Flex => "flex",
            ServiceTier::Standard => "standard",
            ServiceTier::Floor => "floor",
        }
    }
}

/// Per-stage thinking control (`docs/LLM.md` §1/§5). Logical form; the mapping
/// to `thinkingBudget` vs `thinkingLevel` for a model family is `gemini.rs`'s job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingSetting {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    /// An explicit token budget.
    Budget(u32),
}

/// The role of a logical content block. Mirrors the two user blocks of the
/// cache-friendly prompt layout (`docs/LLM.md` §2); `Model` is available for
/// completeness (e.g. map-reduce partials passed back in).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Model,
}

/// One ordered content block. The prompt layout fixes the order: block 0 is the
/// canonical analysis sheet (end of the cached prefix), block 1 the variable
/// block (`docs/LLM.md` §2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentBlock {
    pub role: Role,
    pub text: String,
}

impl ContentBlock {
    /// A convenience constructor for a `user` block.
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            text: text.into(),
        }
    }
}

/// Generation controls sent with every request (`docs/LLM.md` §1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerationConfig {
    /// Always set, computed per request (`docs/LLM.md` §5.8).
    pub max_output_tokens: u32,
    /// `application/json` for structured output.
    pub response_mime_type: String,
    /// The response schema from `llm/schemas.rs`, when structured output is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_schema: Option<serde_json::Value>,
    pub thinking: ThinkingSetting,
}

/// A logical LLM request. Everything needed to build the cache key
/// (`docs/LLM.md` §4) is present; the Gemini wire mapping is `gemini.rs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmRequest {
    pub model: String,
    /// Service tier is excluded from the cache key (`docs/LLM.md` §4).
    pub service_tier: ServiceTier,
    pub system_instruction: String,
    /// Ordered, cache-friendly blocks (`docs/LLM.md` §2).
    pub contents: Vec<ContentBlock>,
    pub generation_config: GenerationConfig,
}

impl LlmRequest {
    /// The "cached prefix ends here" marker (`docs/LLM.md` §2, ADR-051b): the number
    /// of leading [`ContentBlock`]s that belong to the stable cached prefix — every
    /// block except the last, which is the variable block. For a translation request
    /// (`[analysis_sheet, variable]`) this is `1` (the sheet); for a single-content
    /// analysis request (`[transcript]`, the variable block) it is `0` (only the
    /// `system_instruction` is stable). Each adapter uses this to place its cache
    /// control: Gemini relies on the stable prefix for implicit/explicit caching;
    /// OpenRouter puts `cache_control: {type:"ephemeral"}` on the last prefix block
    /// (`docs/LLM.md` §10.2). A request with no content blocks has no prefix (`0`).
    pub fn cached_prefix_block_count(&self) -> usize {
        self.contents.len().saturating_sub(1)
    }

    /// The provider this request routes to, parsed from [`LlmRequest::model`] under
    /// `default` (`docs/LLM.md` §10.1). The [`Router`](crate::llm::router::Router)
    /// uses this to pick a transport.
    pub fn provider(&self, default: crate::domain::Provider) -> crate::domain::Provider {
        crate::domain::ModelRef::parse(&self.model, default)
            .map(|r| r.provider)
            .unwrap_or(default)
    }
}

/// Token usage read from a provider's response plus the markers the `llm_calls`
/// row and pricing need (`docs/DATA_MODEL.md` §2). Provider-neutral: a field a
/// provider does not report stays `0`/`None` rather than a misleading value
/// (`docs/LLM.md` §10.5, ADR-051b). Wire field names stay inside each adapter.
///
/// Gemini mapping (`llm/gemini.rs`): `promptTokenCount → prompt_tokens`,
/// `cachedContentTokenCount → cached_tokens`, `candidatesTokenCount →
/// output_tokens`, `thoughtsTokenCount → thinking_tokens` (Gemini leaves
/// `cost_usd`/`cache_write_tokens`/`reasoning_tokens` and the served-* fields
/// unset — cost is computed from the price table). OpenRouter mapping
/// (`llm/openrouter.rs`, §10.5) fills `cost_usd` and the served-* fields.
///
/// `Eq` is intentionally not derived: `cost_usd` is an `f64`. Nothing uses `Usage`
/// (or the response types that carry it) as a hash/set key, so `PartialEq` suffices
/// for the `assert_eq!`/equality uses in the pipeline and tests.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    /// Portion of the prompt served from cache (subset of `prompt_tokens`).
    pub cached_tokens: u64,
    /// Tokens written to a provider cache on this call, when the provider reports
    /// them (OpenRouter `cache_write`; `docs/LLM.md` §10.5). `0` for Gemini.
    #[serde(default)]
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    /// Thinking/reasoning tokens; billed as output (`docs/LLM.md` §9). Gemini fills
    /// this from `thoughtsTokenCount`; OpenRouter from
    /// `completion_tokens_details.reasoning_tokens`.
    pub thinking_tokens: u64,
    /// Explicit reasoning-token count when a provider reports it separately from
    /// `thinking_tokens` (OpenRouter, §10.5). Defaults to `0`.
    #[serde(default)]
    pub reasoning_tokens: u64,
    /// The provider's own reported cost in USD, when it reports one (OpenRouter
    /// `usage.cost`, §10.5). `None` for Gemini (cost is computed from the price
    /// table by `pricing.rs`). Additive and omitted from serialized output when
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Set when the response came from the local response cache (cost 0).
    #[serde(default)]
    pub cache_hit_local: bool,
    /// The service tier the server reported it actually applied, when present. Lets
    /// a smoke test confirm a `flex` request was not silently served on `standard`
    /// (ADR-028). Additive: `None` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_tier: Option<String>,
    /// OpenRouter generation id (`usage.id` / response `id`), for the `llm_calls`
    /// `generation_id` column (§10.5). `None` for Gemini.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_id: Option<String>,
    /// The upstream provider OpenRouter routed to, for `llm_calls.served_provider`
    /// (§10.5). `None` for Gemini.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub served_provider: Option<String>,
    /// The tier OpenRouter actually served, for `llm_calls.served_tier` (§10.5).
    /// `None` for Gemini.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub served_tier: Option<String>,
}

/// Everything the [`Cached`](crate::llm::cache) layer needs to persist one
/// `llm_cache` row (`docs/DATA_MODEL.md` §2): the cache key plus the model,
/// prompt version, response text, and [`Usage`] to store under it. Carried by a
/// [`CommitToken`] from a cache *miss* so the caller can store the response once
/// it has validated it (`docs/LLM.md` §4). Public fields so the pipeline (M5/M6)
/// can hand the payload to a store `*_in_tx` helper and commit it inside the
/// unit's transaction.
///
/// `Eq` is not derived because [`Usage`] now carries an `f64` (`cost_usd`).
#[derive(Debug, Clone, PartialEq)]
pub struct CachePayload {
    pub key: String,
    pub model: String,
    pub prompt_version: u32,
    pub response: String,
    pub usage: Usage,
}

/// An opaque token returned alongside a response so the caller can confirm
/// validity before the [`Cached`](crate::llm::cache) layer stores it
/// (`docs/LLM.md` §4).
///
/// A *miss* carries a [`CachePayload`] (the response to store once validated); a
/// *hit*, a dry-run, or a transport with no cache behind it carries nothing
/// (`noop`). Invalid responses are simply never committed — the caller just
/// drops the token.
#[derive(Debug, Clone, PartialEq)]
pub struct CommitToken {
    key: Option<String>,
    // Boxed so the token (threaded through `LlmResponse` and several pipeline
    // outcome enums) stays small: `CachePayload` embeds a `Usage`, which grew with
    // the provider-neutral fields (`cost_usd` and the OpenRouter served-* fields,
    // ADR-051b). Keeping the common `None` token pointer-sized avoids a
    // `clippy::large_enum_variant` on `CallResult`/`CallStep`.
    payload: Option<Box<CachePayload>>,
}

impl CommitToken {
    /// A token that commits nothing (e.g. for a local cache hit, or a transport
    /// with no cache behind it).
    pub fn noop() -> Self {
        Self {
            key: None,
            payload: None,
        }
    }

    /// A token naming the cache key to store under, without a payload. Used where
    /// only the key is known; committing it is a no-op.
    pub fn for_key(key: impl Into<String>) -> Self {
        Self {
            key: Some(key.into()),
            payload: None,
        }
    }

    /// A token carrying the full [`CachePayload`] to store once the caller
    /// validates the response (`docs/LLM.md` §4). Built by the `Cached` layer on a
    /// cache miss.
    pub fn to_store(payload: CachePayload) -> Self {
        Self {
            key: Some(payload.key.clone()),
            payload: Some(Box::new(payload)),
        }
    }

    /// The cache key to commit under, if any.
    pub fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }

    /// The payload to store once validated, if this token carries one.
    pub fn payload(&self) -> Option<&CachePayload> {
        self.payload.as_deref()
    }
}

/// A logical LLM response: the raw payload text, token [`Usage`], and a
/// [`CommitToken`] for the cache flow (`docs/LLM.md` §4).
///
/// `Eq` is not derived because [`Usage`] now carries an `f64` (`cost_usd`).
#[derive(Debug, Clone, PartialEq)]
pub struct LlmResponse {
    /// The model's text output. For structured output this is the JSON document
    /// as a string; parsing/validation happens in the pipeline.
    pub text: String,
    pub usage: Usage,
    pub commit: CommitToken,
}

/// How an [`LlmError`] should be handled by the transport retry policy
/// (`docs/ARCHITECTURE.md` §8, `docs/LLM.md` §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorClass {
    /// Transient; retry with backoff (5xx, timeout, connection, transport).
    Retryable,
    /// Flex shed / capacity; retry, with tier fallback after repeated sheds.
    Shed,
    /// Rate limited (429), optionally carrying a server-provided delay.
    RateLimited { retry_after: Option<Duration> },
    /// Out of provider credits (OpenRouter 402, `docs/LLM.md` §10.6, ADR-051e).
    /// The run must stop before dispatching further work — it is not retryable and
    /// not a per-unit content failure. `main` surfaces it as a run-stopping error
    /// ("add OpenRouter credits, or use a `:free` model").
    Credits,
    /// The provider refused the content on moderation/guardrail grounds (OpenRouter
    /// 403, `docs/LLM.md` §10.6, ADR-051e). The batch fails and is flagged
    /// [`QualityFlag::Refused`](crate::domain::QualityFlag::Refused), but the run
    /// continues — it is handled by the content path, never a transport retry.
    ContentRefused,
    /// Permanent; give up (400/401/403/404, schema/decode errors).
    Fatal,
}

/// Errors from the LLM client layer stack (`docs/ARCHITECTURE.md` §8).
#[derive(Debug, Error)]
pub enum LlmError {
    /// HTTP 429. Carries an optional delay from `Retry-After` or
    /// `google.rpc.RetryInfo.retryDelay` (`docs/LLM.md` §6).
    #[error("rate limited (429)")]
    RateLimited { retry_after: Option<Duration> },

    /// A 5xx server error.
    #[error("server error (HTTP {status})")]
    Server { status: u16 },

    /// The request timed out.
    #[error("request timed out")]
    Timeout,

    /// A connection-level failure (DNS, TCP, TLS).
    #[error("connection error: {0}")]
    Connection(String),

    /// The flex tier shed the request, or a capacity error occurred.
    #[error("request shed (capacity / flex)")]
    Shed,

    /// A permanent request failure (400/401/403/404).
    #[error("fatal request error (HTTP {status}): {message}")]
    Fatal { status: u16, message: String },

    /// The model rejected the requested service tier (`docs/LLM.md` §1). The
    /// `Retrying` layer warns and retries once on the standard tier.
    #[error("service tier not supported: {message}")]
    TierUnsupported { message: String },

    /// Out of provider credits (OpenRouter 402, `docs/LLM.md` §10.6). Run-stopping;
    /// classed [`ErrorClass::Credits`].
    #[error("out of provider credits: {message}")]
    Credits { message: String },

    /// The provider refused the content on moderation/guardrail grounds (OpenRouter
    /// 403, `docs/LLM.md` §10.6). Classed [`ErrorClass::ContentRefused`]; handled by
    /// the content path, never a transport retry.
    #[error("content refused by provider: {message}")]
    ContentRefused { message: String },

    /// The operation was cancelled (Ctrl-C / [`CancellationToken`]), e.g. during
    /// a backoff sleep (`docs/ARCHITECTURE.md` §6.6). Maps to exit code 130.
    ///
    /// [`CancellationToken`]: tokio_util::sync::CancellationToken
    #[error("operation cancelled")]
    Cancelled,

    /// A generic transport failure not captured by the variants above.
    #[error("transport error: {0}")]
    Transport(String),

    /// The response body could not be decoded or did not match the schema.
    #[error("response decode error: {0}")]
    Decode(String),
}

impl LlmError {
    /// Maps this error to its [`ErrorClass`] for the retry policy
    /// (`docs/LLM.md` §6, `docs/ARCHITECTURE.md` §8).
    pub fn class(&self) -> ErrorClass {
        match self {
            LlmError::RateLimited { retry_after } => ErrorClass::RateLimited {
                retry_after: *retry_after,
            },
            LlmError::Server { .. }
            | LlmError::Timeout
            | LlmError::Connection(_)
            | LlmError::Transport(_) => ErrorClass::Retryable,
            LlmError::Shed => ErrorClass::Shed,
            LlmError::Credits { .. } => ErrorClass::Credits,
            LlmError::ContentRefused { .. } => ErrorClass::ContentRefused,
            // `TierUnsupported` and `Cancelled` are handled by the `Retrying`
            // layer directly (tier re-send / early return) and never flow through
            // `decide`; classed `Fatal` so the policy gives up if one ever does.
            LlmError::Fatal { .. }
            | LlmError::Decode(_)
            | LlmError::TierUnsupported { .. }
            | LlmError::Cancelled => ErrorClass::Fatal,
        }
    }
}

/// The common interface implemented by every layer of the client stack
/// (`docs/ARCHITECTURE.md` §5). Static dispatch; no `async-trait` (ADR-008).
pub trait LlmClient {
    fn generate(
        &self,
        req: LlmRequest,
    ) -> impl Future<Output = Result<LlmResponse, LlmError>> + Send;
}
