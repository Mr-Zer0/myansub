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

/// Service tier for a request (`docs/LLM.md` §5). `Flex` is roughly half the
/// standard price but may be shed under load; `Standard` is the fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceTier {
    Flex,
    Standard,
}

impl ServiceTier {
    /// The lowercase label used in config and the `llm_calls.service_tier` column.
    pub fn label(self) -> &'static str {
        match self {
            ServiceTier::Flex => "flex",
            ServiceTier::Standard => "standard",
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

/// Token usage read from `usageMetadata` (`docs/LLM.md` §1) plus the markers the
/// `llm_calls` row and pricing need (`docs/DATA_MODEL.md` §2).
///
/// Field mapping from Gemini `usageMetadata`:
/// `promptTokenCount → prompt_tokens`, `cachedContentTokenCount → cached_tokens`,
/// `candidatesTokenCount → output_tokens`, `thoughtsTokenCount → thinking_tokens`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    /// Portion of the prompt served from cache (subset of `prompt_tokens`).
    pub cached_tokens: u64,
    pub output_tokens: u64,
    /// Thinking tokens; billed as output (`docs/LLM.md` §9).
    pub thinking_tokens: u64,
    /// Set when the response came from the local response cache (cost 0).
    #[serde(default)]
    pub cache_hit_local: bool,
}

/// An opaque token returned alongside a response so the caller can confirm
/// validity before the [`Cached`](crate::llm) layer stores it
/// (`docs/LLM.md` §4). The cache implementation (M3-C) consumes it; here we only
/// define the type and the flow surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitToken {
    key: Option<String>,
}

impl CommitToken {
    /// A token that commits nothing (e.g. for a local cache hit, or a transport
    /// with no cache behind it).
    pub fn noop() -> Self {
        Self { key: None }
    }

    /// A token carrying the cache key to store under once the caller validates.
    pub fn for_key(key: impl Into<String>) -> Self {
        Self {
            key: Some(key.into()),
        }
    }

    /// The cache key to commit under, if any.
    pub fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }
}

/// A logical LLM response: the raw payload text, token [`Usage`], and a
/// [`CommitToken`] for the cache flow (`docs/LLM.md` §4).
#[derive(Debug, Clone, PartialEq, Eq)]
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
            LlmError::Fatal { .. } | LlmError::Decode(_) => ErrorClass::Fatal,
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
