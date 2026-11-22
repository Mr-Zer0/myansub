//! The OpenRouter transport (`docs/LLM.md` §10, ADR-051/052).
//!
//! OpenRouter is a second provider behind the same shared layers (local cache,
//! metering, retry engine, scheduler, prompts, quality checks). Every
//! OpenRouter-specific wire field name — `messages`, `cache_control`,
//! `response_format`, `reasoning`, `provider`, `max_tokens`, `session_id`, and the
//! `usage`/`error` body shapes — lives ONLY in this file (as Gemini's do in
//! `llm/gemini.rs`, verified by grep).
//!
//! ## `[verify]` items (`docs/LLM.md` §10)
//!
//! Marked against the OpenRouter API as understood at implementation time; recheck
//! against the live docs when the API changes:
//! - **Endpoint-selection tier** maps `[llm.openrouter].tier` to `provider.sort`
//!   (`floor`/`flex` → `price`, the cheapest endpoints incl. flex; `default` omits
//!   `sort` to use OpenRouter's default routing).
//! - **Session affinity** is sent as a top-level `session_id` (sha256 of the stable
//!   cached prefix, so every request of a stage shares one value).
//! - **Cache-write tokens** are read from `usage.prompt_tokens_details`
//!   (`cache_creation_tokens`) when reported.
//! - **Served tier** is read from the top-level `tier` field when present.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use crate::llm::catalog::ReasoningLevel;
use crate::llm::client::{
    CommitToken, ContentBlock, ErrorClass, LlmClient, LlmError, LlmRequest, LlmResponse, Role,
    Secret, ServiceTier, ThinkingSetting, Usage,
};

/// OpenRouter provider settings threaded from `[llm.openrouter]` (`docs/CONFIG.md`
/// §7). Logical form; the wire mapping is confined to this module.
#[derive(Debug, Clone)]
pub struct OpenRouterConfig {
    /// Endpoint-selection tier label: `floor` | `flex` | `default`.
    pub tier: String,
    /// `deny` | `allow` — whether endpoints that may train on data are allowed.
    pub data_collection: String,
    /// Require zero-data-retention endpoints.
    pub zdr: bool,
    /// When true, send `X-OpenRouter-Title: myansub`.
    pub attribution: bool,
    /// The reasoning effort levels the configured model supports (from the catalog),
    /// low→high, for the nearest-lower-supported fallback (`docs/LLM.md` §10.4). Empty
    /// means the model has no reasoning control.
    pub reasoning_levels: Vec<ReasoningLevel>,
}

impl Default for OpenRouterConfig {
    fn default() -> Self {
        Self {
            tier: "floor".to_string(),
            data_collection: "deny".to_string(),
            zdr: false,
            attribution: false,
            reasoning_levels: vec![
                ReasoningLevel::Minimal,
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
            ],
        }
    }
}

/// The OpenRouter transport (`docs/LLM.md` §10).
#[derive(Debug, Clone)]
pub struct OpenRouter {
    client: reqwest::Client,
    base_url: String,
    // Held as `Secret` so a `Debug` of the transport (or any wrapping layer) never
    // prints the key (`CLAUDE.md` "Never log API keys").
    api_key: Secret,
    timeout: Duration,
    config: OpenRouterConfig,
    // Shared across clones so the reasoning-downgrade warning fires at most once per
    // run (`docs/LLM.md` §10.4).
    reasoning_warned: Arc<AtomicBool>,
}

impl OpenRouter {
    /// Builds a transport against `base_url` (default
    /// `https://openrouter.ai/api/v1`) with the given API key, per-request timeout,
    /// and provider settings, using a freshly built shared client.
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<Secret>,
        timeout: Duration,
        config: OpenRouterConfig,
    ) -> Result<Self, LlmError> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| LlmError::Transport(e.to_string()))?;
        Ok(Self::with_client(
            client, base_url, api_key, timeout, config,
        ))
    }

    /// Builds a transport reusing an existing shared [`reqwest::Client`].
    pub fn with_client(
        client: reqwest::Client,
        base_url: impl Into<String>,
        api_key: impl Into<Secret>,
        timeout: Duration,
        config: OpenRouterConfig,
    ) -> Self {
        Self {
            client,
            base_url: base_url.into(),
            api_key: api_key.into(),
            timeout,
            config,
            reasoning_warned: Arc::new(AtomicBool::new(false)),
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    /// Resolves the reasoning effort for a request's thinking setting against the
    /// model's supported levels: the requested level if supported, else the nearest
    /// supported *lower* level (one warning per run), else none (`docs/LLM.md` §10.4).
    fn resolve_reasoning(&self, setting: ThinkingSetting) -> Option<ReasoningLevel> {
        let desired = desired_level(setting)?;
        let supported = &self.config.reasoning_levels;
        if supported.is_empty() {
            return None;
        }
        if supported.contains(&desired) {
            return Some(desired);
        }
        // Nearest supported lower level.
        let lower = supported.iter().copied().filter(|l| *l < desired).max();
        if !self.reasoning_warned.swap(true, Ordering::Relaxed) {
            match lower {
                Some(l) => tracing::warn!(
                    requested = desired.label(),
                    used = l.label(),
                    "model does not support the requested reasoning level; falling back"
                ),
                None => tracing::warn!(
                    requested = desired.label(),
                    "model does not support the requested reasoning level; omitting reasoning"
                ),
            }
        }
        lower
    }
}

impl LlmClient for OpenRouter {
    async fn generate(&self, req: LlmRequest) -> Result<LlmResponse, LlmError> {
        let url = self.endpoint();
        let effort = self.resolve_reasoning(req.generation_config.thinking);
        let body = build_request_body(&req, &self.config, effort);

        // The key travels in a header and is never logged; only safe fields are traced.
        tracing::trace!(model = %req.model, tier = %self.config.tier, "openrouter request");

        let mut builder = self
            .client
            .post(&url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.api_key.expose()),
            )
            .timeout(self.timeout)
            .json(&body);
        if self.config.attribution {
            builder = builder.header("X-OpenRouter-Title", "myansub");
        }

        let response = builder.send().await.map_err(map_reqwest_error)?;
        let status = response.status();
        let retry_after = parse_retry_after_header(&response);
        let text = response
            .text()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))?;

        if status.is_success() {
            parse_success(&text, &self.config, retry_after)
        } else {
            Err(map_error_response(
                &RawError::from_body(status.as_u16(), retry_after, &text),
                &self.config,
            ))
        }
    }
}

/// Maps a logical [`ThinkingSetting`] to the reasoning level it requests, or `None`
/// when reasoning should be omitted (`docs/LLM.md` §10.4).
fn desired_level(setting: ThinkingSetting) -> Option<ReasoningLevel> {
    match setting {
        ThinkingSetting::Off | ThinkingSetting::Budget(0) => None,
        ThinkingSetting::Minimal => Some(ReasoningLevel::Minimal),
        ThinkingSetting::Low => Some(ReasoningLevel::Low),
        ThinkingSetting::Medium => Some(ReasoningLevel::Medium),
        ThinkingSetting::High => Some(ReasoningLevel::High),
        // A token budget has no direct effort level; coarsen to Low (as gemini.rs
        // coarsens a budget to a level).
        ThinkingSetting::Budget(_) => Some(ReasoningLevel::Low),
    }
}

/// Strips any `provider:` prefix from a model reference, returning the bare id
/// OpenRouter's wire API expects (`docs/LLM.md` §10.1). A `:free` suffix is kept.
fn bare_model(model: &str) -> String {
    crate::domain::ModelRef::parse(model, crate::domain::Provider::OpenRouter)
        .map(|r| r.model)
        .unwrap_or_else(|_| model.to_string())
}

/// Builds the `chat/completions` request body (`docs/LLM.md` §10.2–§10.4). All
/// OpenRouter wire names are confined here.
fn build_request_body(
    req: &LlmRequest,
    config: &OpenRouterConfig,
    effort: Option<ReasoningLevel>,
) -> Value {
    let mut body = json!({
        "model": bare_model(&req.model),
        "messages": messages_json(req),
        "max_tokens": req.generation_config.max_output_tokens,
        "provider": provider_json(req.service_tier, config),
        "session_id": session_id(req),
    });

    if let Some(schema) = &req.generation_config.response_schema {
        body["response_format"] = json!({
            "type": "json_schema",
            "json_schema": {
                "name": "response",
                "strict": true,
                "schema": schema.clone(),
            }
        });
    }
    if let Some(level) = effort {
        body["reasoning"] = json!({ "effort": level.label() });
    }
    body
}

/// Builds the ordered `messages` array (`docs/LLM.md` §10.2): a `system` message
/// (the rules + style pack), then one `user`/`assistant` message per content block.
/// The last block of the cached prefix (`cached_prefix_block_count`) carries
/// `cache_control: {type:"ephemeral"}`.
fn messages_json(req: &LlmRequest) -> Vec<Value> {
    let mut messages = Vec::with_capacity(req.contents.len() + 1);
    if !req.system_instruction.is_empty() {
        messages.push(json!({
            "role": "system",
            "content": [ { "type": "text", "text": req.system_instruction } ],
        }));
    }

    // The last block at or before the "cached prefix ends here" marker gets the
    // ephemeral cache control (`docs/LLM.md` §10.2, ADR-051b).
    let marker = cache_marker_index(req);
    for (idx, block) in req.contents.iter().enumerate() {
        messages.push(content_message(block, Some(idx) == marker));
    }
    messages
}

/// The index of the content block that should carry `cache_control`: the last block
/// of the cached prefix, or `None` when the prefix is empty (`docs/LLM.md` §10.2).
fn cache_marker_index(req: &LlmRequest) -> Option<usize> {
    req.cached_prefix_block_count().checked_sub(1)
}

fn content_message(block: &ContentBlock, cached: bool) -> Value {
    let mut part = json!({ "type": "text", "text": block.text });
    if cached {
        part["cache_control"] = json!({ "type": "ephemeral" });
    }
    json!({
        "role": role_label(block.role),
        "content": [ part ],
    })
}

fn role_label(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Model => "assistant",
    }
}

/// Builds the `provider` routing object (`docs/LLM.md` §10.3/§10.7). `[verify]` the
/// tier→`sort` mapping against the live OpenRouter docs.
fn provider_json(tier: ServiceTier, config: &OpenRouterConfig) -> Value {
    let mut provider = json!({
        "require_parameters": true,
        "data_collection": config.data_collection,
    });
    if config.zdr {
        provider["zdr"] = json!(true);
    }
    // `floor`/`flex` prefer the cheapest endpoints (incl. flex), `default` uses
    // OpenRouter's default routing. The neutral `ServiceTier` carries the resolved
    // tier (`openrouter_tier` in the orchestrator).
    match tier {
        ServiceTier::Floor | ServiceTier::Flex => {
            provider["sort"] = json!("price");
        }
        ServiceTier::Standard => {}
    }
    provider
}

/// The sticky-routing `session_id` (`docs/LLM.md` §10.2): sha256 of the stable
/// cached prefix (the system instruction plus every cached-prefix content block), so
/// every request of a stage — which shares that prefix and differs only in the
/// variable block — sends the same value.
///
/// The logical [`LlmRequest`] carries no `source_hash`/`stage`/`analysis_version`
/// field; this derives an equivalent stable key from the request's own cached prefix
/// (ADR-051b), which is byte-identical across a stage's batches and differs across
/// stages and analysis versions.
fn session_id(req: &LlmRequest) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(req.system_instruction.as_bytes());
    for block in req.contents.iter().take(req.cached_prefix_block_count()) {
        hasher.update([0u8]);
        hasher.update(block.text.as_bytes());
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    use std::fmt::Write as _;
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Parses a 2xx body into an [`LlmResponse`], detecting an in-body error
/// (`docs/LLM.md` §10.6: top-level `error`, `choices[0].error`, or
/// `choices[0].finish_reason == "error"`). A `finish_reason == "length"` is NOT an
/// error here — it is returned as (possibly truncated) content and handled as a
/// content failure by the pipeline's content path, not a transport retry
/// (`docs/LLM.md` §10.4).
fn parse_success(
    body: &str,
    config: &OpenRouterConfig,
    retry_after: Option<Duration>,
) -> Result<LlmResponse, LlmError> {
    let value: Value = serde_json::from_str(body).map_err(|e| LlmError::Decode(e.to_string()))?;

    // A 200 body can still carry an error (top level or inside the first choice).
    if let Some(raw) = RawError::from_success_value(&value, retry_after) {
        return Err(map_error_response(&raw, config));
    }

    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first());
    let text = choice
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    Ok(LlmResponse {
        text,
        usage: parse_usage(&value),
        commit: CommitToken::noop(),
    })
}

fn parse_usage(value: &Value) -> Usage {
    let usage = value.get("usage");
    let count = |obj: Option<&Value>, key: &str| -> u64 {
        obj.and_then(|m| m.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let prompt_details = usage.and_then(|u| u.get("prompt_tokens_details"));
    let completion_details = usage.and_then(|u| u.get("completion_tokens_details"));

    let reasoning_tokens = count(completion_details, "reasoning_tokens");
    let generation_id = value.get("id").and_then(Value::as_str).map(str::to_string);
    let served_provider = value
        .get("provider")
        .and_then(Value::as_str)
        .map(str::to_string);
    let served_tier = value
        .get("tier")
        .and_then(Value::as_str)
        .map(str::to_string);
    let cost_usd = usage.and_then(|u| u.get("cost")).and_then(Value::as_f64);

    Usage {
        prompt_tokens: count(usage, "prompt_tokens"),
        cached_tokens: count(prompt_details, "cached_tokens"),
        cache_write_tokens: count(prompt_details, "cache_creation_tokens"),
        output_tokens: count(usage, "completion_tokens"),
        // Reasoning tokens are billed as output (`docs/LLM.md` §9); mirror them into
        // `thinking_tokens` (the billed field) and keep the explicit count too.
        thinking_tokens: reasoning_tokens,
        reasoning_tokens,
        cost_usd,
        cache_hit_local: false,
        applied_tier: served_tier.clone(),
        generation_id,
        served_provider,
        served_tier,
    }
}

/// A normalized OpenRouter error: an HTTP status (or the synthetic status read from
/// an in-body `error.code`), an optional `Retry-After`, and the moderation marker.
#[derive(Debug, Clone)]
pub struct RawError {
    pub status: u16,
    pub retry_after: Option<Duration>,
    pub message: String,
    /// True when the error is a moderation / guardrail refusal (`docs/LLM.md` §10.6).
    pub moderation: bool,
}

impl RawError {
    /// Builds a [`RawError`] from a non-2xx response body.
    fn from_body(status: u16, retry_after: Option<Duration>, body: &str) -> Self {
        let parsed: Option<Value> = serde_json::from_str(body).ok();
        let error = parsed.as_ref().and_then(|v| v.get("error"));
        Self::from_error_value(status, retry_after, error)
    }

    /// Builds a [`RawError`] from an error carried inside a 2xx body, if any
    /// (`docs/LLM.md` §10.6). A `finish_reason == "length"` is NOT treated as an
    /// error (it is a content failure handled downstream).
    fn from_success_value(value: &Value, retry_after: Option<Duration>) -> Option<Self> {
        if let Some(error) = value.get("error") {
            let code = error.get("code").and_then(Value::as_u64).unwrap_or(400) as u16;
            return Some(Self::from_error_value(code, retry_after, Some(error)));
        }
        let choice = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())?;
        if let Some(error) = choice.get("error") {
            let code = error.get("code").and_then(Value::as_u64).unwrap_or(400) as u16;
            return Some(Self::from_error_value(code, retry_after, Some(error)));
        }
        let finish = choice.get("finish_reason").and_then(Value::as_str);
        if finish == Some("error") {
            return Some(Self::from_error_value(400, retry_after, None));
        }
        None
    }

    fn from_error_value(status: u16, retry_after: Option<Duration>, error: Option<&Value>) -> Self {
        let message = error
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // Moderation refusals surface via a 403 and/or an error `metadata.reasons`
        // array naming moderation. Detect either.
        let reason_text = error
            .and_then(|e| e.get("metadata"))
            .map(|m| m.to_string())
            .unwrap_or_default()
            .to_lowercase();
        let moderation = status == 403
            || reason_text.contains("moderation")
            || message.to_lowercase().contains("moderat")
            || message.to_lowercase().contains("flagged");
        Self {
            status,
            retry_after,
            message,
            moderation,
        }
    }
}

/// Maps a [`RawError`] to its [`ErrorClass`] (`docs/LLM.md` §10.6, ADR-051e). The
/// retry engine (§6) stays provider-agnostic; this is OpenRouter's status→class map.
pub fn classify(err: &RawError) -> ErrorClass {
    match err.status {
        402 => ErrorClass::Credits,
        403 if err.moderation => ErrorClass::ContentRefused,
        403 => ErrorClass::Fatal,
        400 | 401 | 404 => ErrorClass::Fatal,
        // Rate limiting always honors a server delay; 408/502 are retryable and honor
        // a `Retry-After` when one is present, else back off (`docs/LLM.md` §10.6).
        429 => ErrorClass::RateLimited {
            retry_after: err.retry_after,
        },
        408 | 502 if err.retry_after.is_some() => ErrorClass::RateLimited {
            retry_after: err.retry_after,
        },
        408 | 502 => ErrorClass::Retryable,
        503 => ErrorClass::Retryable,
        other if other >= 500 => ErrorClass::Retryable,
        _ => ErrorClass::Fatal,
    }
}

/// Maps a [`RawError`] to an [`LlmError`] carrying an actionable message
/// (`docs/LLM.md` §10.6).
fn map_error_response(err: &RawError, config: &OpenRouterConfig) -> LlmError {
    match classify(err) {
        ErrorClass::Credits => LlmError::Credits {
            message: "add OpenRouter credits, or use a `:free` model".to_string(),
        },
        ErrorClass::ContentRefused => LlmError::ContentRefused {
            message: if err.message.is_empty() {
                "content refused by provider moderation".to_string()
            } else {
                err.message.clone()
            },
        },
        ErrorClass::RateLimited { retry_after } => LlmError::RateLimited { retry_after },
        ErrorClass::Retryable => {
            if err.status == 503 {
                // No endpoint met the routing constraints; the message names the
                // routing settings so the run-stopping error (after the retry budget)
                // is actionable (`docs/LLM.md` §10.6).
                LlmError::Transport(format!(
                    "no OpenRouter endpoint met the routing settings \
                     (tier = {}, data_collection = {}, zdr = {})",
                    config.tier, config.data_collection, config.zdr
                ))
            } else {
                LlmError::Server { status: err.status }
            }
        }
        ErrorClass::Shed => LlmError::Shed,
        ErrorClass::Fatal => {
            if err.status == 401 {
                LlmError::Fatal {
                    status: 401,
                    message: "check key: set `OPENROUTER_API_KEY`".to_string(),
                }
            } else {
                LlmError::Fatal {
                    status: err.status,
                    message: err.message.clone(),
                }
            }
        }
    }
}

/// Parses a `Retry-After` header expressed as a number of seconds.
fn parse_retry_after_header(response: &reqwest::Response) -> Option<Duration> {
    let value = response.headers().get(reqwest::header::RETRY_AFTER)?;
    let text = value.to_str().ok()?.trim();
    let seconds: f64 = text.parse().ok()?;
    if seconds.is_finite() && seconds >= 0.0 {
        Some(Duration::from_secs_f64(seconds))
    } else {
        None
    }
}

fn map_reqwest_error(error: reqwest::Error) -> LlmError {
    if error.is_timeout() {
        LlmError::Timeout
    } else if error.is_connect() {
        LlmError::Connection(error.to_string())
    } else {
        LlmError::Transport(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::client::GenerationConfig;

    fn request() -> LlmRequest {
        LlmRequest {
            model: "openrouter:anthropic/claude-sonnet-5.5".to_string(),
            service_tier: ServiceTier::Floor,
            system_instruction: "rules".to_string(),
            contents: vec![ContentBlock::user("sheet"), ContentBlock::user("lines")],
            generation_config: GenerationConfig {
                max_output_tokens: 256,
                response_mime_type: "application/json".to_string(),
                response_schema: Some(json!({ "type": "object" })),
                thinking: ThinkingSetting::Off,
            },
        }
    }

    #[test]
    fn classify_covers_the_error_table() {
        let raw = |status, moderation| RawError {
            status,
            retry_after: None,
            message: String::new(),
            moderation,
        };
        assert_eq!(classify(&raw(400, false)), ErrorClass::Fatal);
        assert_eq!(classify(&raw(401, false)), ErrorClass::Fatal);
        assert_eq!(classify(&raw(402, false)), ErrorClass::Credits);
        assert_eq!(classify(&raw(403, true)), ErrorClass::ContentRefused);
        assert_eq!(
            classify(&raw(429, false)),
            ErrorClass::RateLimited { retry_after: None }
        );
        assert_eq!(classify(&raw(503, false)), ErrorClass::Retryable);
    }

    #[test]
    fn session_id_is_stable_across_variable_block_changes() {
        // AC: M13-AC8 — every request of a stage (same cached prefix, different
        // variable block) sends the same session_id.
        let mut a = request();
        let mut b = request();
        b.contents[1] = ContentBlock::user("DIFFERENT lines");
        assert_eq!(session_id(&a), session_id(&b));

        // A different cached prefix (analysis sheet) → a different session.
        a.contents[0] = ContentBlock::user("OTHER sheet");
        assert_ne!(session_id(&a), session_id(&b));
    }

    #[test]
    fn request_body_shape() {
        // AC: M13-AC8/AC9/AC10 — messages order + cache_control, response_format
        // strict json_schema + require_parameters, max_tokens.
        let body = build_request_body(&request(), &OpenRouterConfig::default(), None);
        assert_eq!(body["model"], "anthropic/claude-sonnet-5.5");
        assert_eq!(body["max_tokens"], 256);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"][0]["text"], "sheet");
        // cache_control on the last cached-prefix block (the sheet), not the variable.
        assert_eq!(
            body["messages"][1]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert!(
            body["messages"][2]["content"][0]
                .get("cache_control")
                .is_none()
        );
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        assert_eq!(body["provider"]["require_parameters"], true);
        assert_eq!(body["provider"]["data_collection"], "deny");
        assert_eq!(body["provider"]["sort"], "price");
    }

    #[test]
    fn reasoning_falls_back_to_nearest_lower_supported() {
        // AC: M13-AC10 — a High request on a model supporting only [minimal, low]
        // falls back to Low.
        let config = OpenRouterConfig {
            reasoning_levels: vec![ReasoningLevel::Minimal, ReasoningLevel::Low],
            ..OpenRouterConfig::default()
        };
        let or = OpenRouter::with_client(
            reqwest::Client::new(),
            "http://x",
            "k",
            Duration::from_secs(1),
            config,
        );
        assert_eq!(
            or.resolve_reasoning(ThinkingSetting::High),
            Some(ReasoningLevel::Low)
        );
        assert_eq!(or.resolve_reasoning(ThinkingSetting::Off), None);
        assert_eq!(
            or.resolve_reasoning(ThinkingSetting::Low),
            Some(ReasoningLevel::Low)
        );
    }

    #[test]
    fn usage_parses_cost_and_served_fields() {
        // AC: M13-AC11 — usage parsing fills cost_usd, cached/reasoning tokens, and
        // the served-* columns.
        let value = json!({
            "id": "gen-abc",
            "provider": "Anthropic",
            "tier": "flex",
            "choices": [ { "message": { "content": "{}" }, "finish_reason": "stop" } ],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "cost": 0.0012,
                "prompt_tokens_details": { "cached_tokens": 40, "cache_creation_tokens": 10 },
                "completion_tokens_details": { "reasoning_tokens": 7 }
            }
        });
        let usage = parse_usage(&value);
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.cached_tokens, 40);
        assert_eq!(usage.cache_write_tokens, 10);
        assert_eq!(usage.output_tokens, 20);
        assert_eq!(usage.reasoning_tokens, 7);
        assert_eq!(usage.cost_usd, Some(0.0012));
        assert_eq!(usage.generation_id.as_deref(), Some("gen-abc"));
        assert_eq!(usage.served_provider.as_deref(), Some("Anthropic"));
        assert_eq!(usage.served_tier.as_deref(), Some("flex"));
    }

    #[test]
    fn in_body_error_is_detected_on_200() {
        // AC: M13-AC13 — an error inside a 200 body (choices[0].error) is mapped.
        let value = json!({
            "choices": [ { "error": { "code": 402, "message": "no credits" } } ]
        });
        let raw = RawError::from_success_value(&value, None).expect("in-body error");
        assert_eq!(raw.status, 402);
        assert_eq!(classify(&raw), ErrorClass::Credits);
    }

    #[test]
    fn finish_reason_length_is_not_an_error() {
        // AC: M13-AC10 — finish_reason == "length" is a content failure, returned as
        // content (no transport error).
        let value = json!({
            "choices": [ { "message": { "content": "{partial" }, "finish_reason": "length" } ],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
        });
        assert!(RawError::from_success_value(&value, None).is_none());
        let resp = parse_success(&value.to_string(), &OpenRouterConfig::default(), None)
            .expect("length is content, not error");
        assert_eq!(resp.text, "{partial");
    }

    #[test]
    fn debug_does_not_leak_api_key() {
        let key = "super-secret-or-key";
        let or = OpenRouter::new(
            "http://x",
            key,
            Duration::from_secs(1),
            OpenRouterConfig::default(),
        )
        .expect("build");
        let debug = format!("{or:?}");
        assert!(!debug.contains(key), "api key leaked: {debug}");
    }
}
