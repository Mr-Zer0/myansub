//! The Gemini `generateContent` transport (`docs/LLM.md` §1).
//!
//! This is the only place that knows Gemini wire field names and JSON shape
//! (`docs/ARCHITECTURE.md` §5 directive). The logical [`LlmRequest`] /
//! [`LlmResponse`] types in [`client`](crate::llm::client) stay provider-agnostic.
//!
//! # Verified against the live `generateContent` reference (checked 2026-11)
//!
//! - **Structured output**: `generationConfig.responseJsonSchema` is the field to
//!   send; `responseSchema` is marked "an internal detail. Use responseJsonSchema
//!   rather than this field." (ADR-025.)
//! - **Thinking**: `generationConfig.thinkingConfig` with either `thinkingBudget`
//!   (integer thought-token budget) or `thinkingLevel` (string enum
//!   `MINIMAL|LOW|MEDIUM|HIGH|XHIGH`). The reference notes `thinkingLevel` is
//!   "Recommended for Gemini 3 or later models. Use with earlier models results in
//!   an error." We therefore send `thinkingLevel` for Gemini-3+ model names and
//!   `thinkingBudget` otherwise (ADR-028).
//! - **Service tier**: the reference documents `serviceTier` with values
//!   `standard|flex|priority` (plus `unspecified`), but on the `generateContent`
//!   REST page only as an *output-only* `usageMetadata` field ("Service tier of
//!   the request"). The request-side placement is not spelled out there, so we
//!   send it as a top-level request field `serviceTier` (a request-level, not a
//!   generation, concern) confined to this module (ADR-028).
//! - **Usage**: `usageMetadata.{promptTokenCount, cachedContentTokenCount,
//!   candidatesTokenCount, thoughtsTokenCount}`.
//! - **Errors / retry hints**: a 429's delay comes from the `Retry-After` header
//!   or a `google.rpc.RetryInfo` detail (`retryDelay`, a protobuf duration string
//!   like `"5s"`) inside the `error.details` array. 503 (overloaded / capacity /
//!   flex shed) maps to [`LlmError::Shed`]; 500/502/504 to [`LlmError::Server`]
//!   (ADR-028).
//! - **Explicit context cache** ([`ContextCacheClient`]): `POST {base}/cachedContents`
//!   with `{ model: "models/<name>", contents, systemInstruction, ttl: "<n>s" }`
//!   returns `{ name: "cachedContents/<id>", …, usageMetadata.totalTokenCount }`;
//!   `DELETE {base}/<name>` removes it; a `generateContent` call references it via a
//!   top-level `cachedContent` field (the system instruction and cached contents are
//!   baked into the cache, so they are *not* re-sent on such a call). Verified against
//!   the live Gemini v1beta reference (checked 2026-11): the endpoints, the string
//!   `ttl` duration, the `cachedContent` reference field, and the baked-in-prefix rule
//!   all match (ADR-042). All `cachedContents` wire names are confined to this module.

use std::time::Duration;

use serde_json::{Value, json};

use crate::llm::client::{
    ContentBlock, GenerationConfig, LlmClient, LlmError, LlmRequest, LlmResponse, Role, Secret,
    ServiceTier, ThinkingSetting, Usage,
};
use crate::llm::context_cache::{CacheHandle, ContextCacheClient, CreateCacheRequest};

/// The Gemini `generateContent` transport. Holds one shared [`reqwest::Client`]
/// (`docs/LLM.md` §1); clone it freely — the client is internally reference
/// counted.
#[derive(Debug, Clone)]
pub struct Gemini {
    client: reqwest::Client,
    base_url: String,
    // Held as `Secret` so a `Debug` of the transport (or any layer wrapping it)
    // never prints the key (`CLAUDE.md` "Never log API keys").
    api_key: Secret,
    timeout: Duration,
}

impl Gemini {
    /// Builds a transport against `base_url` (e.g.
    /// `https://generativelanguage.googleapis.com/v1beta`) with the given API key
    /// and per-request timeout, using a freshly built shared client.
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<Secret>,
        timeout: Duration,
    ) -> Result<Self, LlmError> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| LlmError::Transport(e.to_string()))?;
        Ok(Self::with_client(client, base_url, api_key, timeout))
    }

    /// Builds a transport reusing an existing shared [`reqwest::Client`] (the one
    /// client is shared across all transports, `docs/LLM.md` §1).
    pub fn with_client(
        client: reqwest::Client,
        base_url: impl Into<String>,
        api_key: impl Into<Secret>,
        timeout: Duration,
    ) -> Self {
        Self {
            client,
            base_url: base_url.into(),
            api_key: api_key.into(),
            timeout,
        }
    }

    fn endpoint(&self, model: &str) -> String {
        format!(
            "{}/models/{}:generateContent",
            self.base_url.trim_end_matches('/'),
            bare_model(model)
        )
    }
}

impl LlmClient for Gemini {
    async fn generate(&self, req: LlmRequest) -> Result<LlmResponse, LlmError> {
        let url = self.endpoint(&req.model);
        let body = build_request_body(&req);

        // The API key travels in a header and is never logged (`docs/ARCHITECTURE.md`
        // §9); only the model/tier (safe) are traced.
        tracing::trace!(model = %req.model, tier = req.service_tier.label(), "gemini request");

        let response = self
            .client
            .post(&url)
            .header("x-goog-api-key", self.api_key.expose())
            .timeout(self.timeout)
            .json(&body)
            .send()
            .await
            .map_err(map_reqwest_error)?;

        let status = response.status();
        let retry_after = parse_retry_after_header(&response);
        let text = response
            .text()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))?;

        if status.is_success() {
            parse_success(&text)
        } else {
            Err(map_error_response(status.as_u16(), retry_after, &text))
        }
    }
}

impl ContextCacheClient for Gemini {
    async fn create_cache(&self, req: CreateCacheRequest) -> Result<CacheHandle, LlmError> {
        let url = format!("{}/cachedContents", self.base_url.trim_end_matches('/'));
        let body = build_cached_content_body(&req);
        tracing::trace!(model = %req.model, "gemini cachedContents create");

        let response = self
            .client
            .post(&url)
            .header("x-goog-api-key", self.api_key.expose())
            .timeout(self.timeout)
            .json(&body)
            .send()
            .await
            .map_err(map_reqwest_error)?;

        let status = response.status();
        let retry_after = parse_retry_after_header(&response);
        let text = response
            .text()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))?;

        if status.is_success() {
            parse_cache_handle(&text)
        } else {
            Err(map_error_response(status.as_u16(), retry_after, &text))
        }
    }

    async fn delete_cache(&self, name: String) -> Result<(), LlmError> {
        // `name` is a `cachedContents/...` resource name; the delete URL is
        // `{base_url}/{name}`.
        let url = format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            name.trim_start_matches('/')
        );
        tracing::trace!(cache = %name, "gemini cachedContents delete");

        let response = self
            .client
            .delete(&url)
            .header("x-goog-api-key", self.api_key.expose())
            .timeout(self.timeout)
            .send()
            .await
            .map_err(map_reqwest_error)?;

        let status = response.status();
        if status.is_success() {
            Ok(())
        } else {
            let retry_after = parse_retry_after_header(&response);
            let text = response
                .text()
                .await
                .map_err(|e| LlmError::Transport(e.to_string()))?;
            Err(map_error_response(status.as_u16(), retry_after, &text))
        }
    }
}

/// Builds the `cachedContents` create request JSON from a logical
/// [`CreateCacheRequest`] (`docs/LLM.md` §5.5). All Gemini `cachedContents` wire
/// names are confined here (ADR-042).
fn build_cached_content_body(req: &CreateCacheRequest) -> Value {
    let mut body = json!({
        "model": qualify_model(&req.model),
        "contents": [ { "role": "user", "parts": [ { "text": req.content } ] } ],
        // TTL is a protobuf duration string (e.g. "900s").
        "ttl": format!("{}s", req.ttl_secs),
    });
    // The system instruction is baked into the cache (omitted when empty so the
    // cached prefix stays byte-stable, `docs/LLM.md` §2).
    if !req.system_instruction.is_empty() {
        body["systemInstruction"] = json!({ "parts": [ { "text": req.system_instruction } ] });
    }
    body
}

/// Qualifies a plain model name to the `models/<name>` form the `cachedContents`
/// endpoint expects, leaving an already-qualified name untouched. Any `provider:`
/// prefix is stripped first (`docs/LLM.md` §10.1): the provider half selects the
/// transport and is never sent on the wire.
fn qualify_model(model: &str) -> String {
    let bare = bare_model(model);
    if bare.starts_with("models/") {
        bare
    } else {
        format!("models/{bare}")
    }
}

/// Strips any `provider:` prefix from a model reference, returning the bare model
/// id Gemini's wire API expects (`docs/LLM.md` §10.1). Backward compatible with a
/// bare model id (`gemini-flash-latest` → `gemini-flash-latest`). An unparseable
/// reference is returned unchanged.
fn bare_model(model: &str) -> String {
    crate::domain::ModelRef::parse(model, crate::domain::Provider::Gemini)
        .map(|r| r.model)
        .unwrap_or_else(|_| model.to_string())
}

/// Parses a successful `cachedContents` create body into a [`CacheHandle`]: the
/// `name` resource id and the `usageMetadata.totalTokenCount` (defaulting to `0`).
fn parse_cache_handle(body: &str) -> Result<CacheHandle, LlmError> {
    let value: Value = serde_json::from_str(body).map_err(|e| LlmError::Decode(e.to_string()))?;
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| LlmError::Decode("cachedContents response had no name".to_string()))?;
    let token_count = value
        .get("usageMetadata")
        .and_then(|m| m.get("totalTokenCount"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Ok(CacheHandle {
        name: name.to_string(),
        token_count,
    })
}

/// Builds the `generateContent` request JSON from a logical [`LlmRequest`].
fn build_request_body(req: &LlmRequest) -> Value {
    let mut body = json!({
        "contents": req.contents.iter().map(content_block_json).collect::<Vec<_>>(),
        "generationConfig": generation_config_json(&bare_model(&req.model), &req.generation_config),
        "serviceTier": service_tier_label(req.service_tier),
    });

    // `systemInstruction` is omitted entirely when empty so the request stays
    // minimal and the cached prefix is byte-stable (`docs/LLM.md` §2).
    if !req.system_instruction.is_empty() {
        body["systemInstruction"] = json!({ "parts": [ { "text": req.system_instruction } ] });
    }

    body
}

fn content_block_json(block: &ContentBlock) -> Value {
    json!({
        "role": role_label(block.role),
        "parts": [ { "text": block.text } ],
    })
}

fn role_label(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Model => "model",
    }
}

fn service_tier_label(tier: ServiceTier) -> &'static str {
    match tier {
        ServiceTier::Flex => "flex",
        ServiceTier::Standard => "standard",
        // Gemini has no "floor" tier; its cheapest is flex, so a neutral `Floor`
        // request (only produced by the OpenRouter path, but handled here for
        // exhaustiveness) maps to flex on the Gemini wire (ADR-051b).
        ServiceTier::Floor => "flex",
    }
}

fn generation_config_json(model: &str, gc: &GenerationConfig) -> Value {
    let mut config = json!({
        "responseMimeType": gc.response_mime_type,
        "maxOutputTokens": gc.max_output_tokens,
        "thinkingConfig": thinking_config_json(model, gc.thinking),
    });
    if let Some(schema) = &gc.response_schema {
        config["responseJsonSchema"] = schema.clone();
    }
    config
}

/// Maps the logical [`ThinkingSetting`] to the model family's thinking control.
///
/// Gemini-3+ models take `thinkingLevel` (an enum); earlier families take
/// `thinkingBudget` (a thought-token count, `0` disables). See the module docs and
/// ADR-028.
fn thinking_config_json(model: &str, setting: ThinkingSetting) -> Value {
    if model_uses_thinking_level(model) {
        json!({ "thinkingLevel": thinking_level(setting) })
    } else {
        json!({ "thinkingBudget": thinking_budget(setting) })
    }
}

/// Whether the model family uses `thinkingLevel` (Gemini 3 and later) rather than
/// `thinkingBudget`.
fn model_uses_thinking_level(model: &str) -> bool {
    model.contains("gemini-3") || model.contains("gemini-flash-3")
}

fn thinking_level(setting: ThinkingSetting) -> &'static str {
    match setting {
        // There is no explicit "off" level; `MINIMAL` is the lowest available.
        ThinkingSetting::Off | ThinkingSetting::Minimal => "MINIMAL",
        ThinkingSetting::Low => "LOW",
        ThinkingSetting::Medium => "MEDIUM",
        ThinkingSetting::High => "HIGH",
        // Levels have no token budget, so for gemini-3* models a `Budget(n)`
        // intent is intentionally coarsened to a level (MINIMAL for 0, else LOW);
        // the configured token count cannot be expressed here.
        ThinkingSetting::Budget(0) => "MINIMAL",
        ThinkingSetting::Budget(_) => "LOW",
    }
}

fn thinking_budget(setting: ThinkingSetting) -> u32 {
    match setting {
        ThinkingSetting::Off => 0,
        ThinkingSetting::Minimal => 128,
        ThinkingSetting::Low => 512,
        ThinkingSetting::Medium => 2048,
        ThinkingSetting::High => 8192,
        ThinkingSetting::Budget(n) => n,
    }
}

/// Parses a successful `generateContent` body into an [`LlmResponse`]. The
/// returned [`CommitToken`](crate::llm::client::CommitToken) is a no-op: the
/// `Cached` layer (M3-C) assigns real cache tokens, not the transport.
fn parse_success(body: &str) -> Result<LlmResponse, LlmError> {
    let value: Value = serde_json::from_str(body).map_err(|e| LlmError::Decode(e.to_string()))?;

    let parts = value
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
        .ok_or_else(|| LlmError::Decode("response had no candidate content parts".to_string()))?;

    let text = parts
        .iter()
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect::<String>();

    Ok(LlmResponse {
        text,
        usage: parse_usage(&value),
        commit: crate::llm::client::CommitToken::noop(),
    })
}

fn parse_usage(value: &Value) -> Usage {
    let meta = value.get("usageMetadata");
    let count = |key: &str| -> u64 {
        meta.and_then(|m| m.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    // The tier the server actually applied (output-only `usageMetadata.serviceTier`);
    // lets callers detect a silently-downgraded flex request (ADR-028).
    let applied_tier = meta
        .and_then(|m| m.get("serviceTier"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Usage {
        prompt_tokens: count("promptTokenCount"),
        cached_tokens: count("cachedContentTokenCount"),
        output_tokens: count("candidatesTokenCount"),
        thinking_tokens: count("thoughtsTokenCount"),
        applied_tier,
        // Gemini leaves the provider-neutral extras (cost_usd, cache_write_tokens,
        // reasoning_tokens, generation_id, served_*) unset — cost is computed from
        // the price table and the served-* fields are OpenRouter-only (§10.5).
        ..Default::default()
    }
}

/// Maps a non-2xx response to an [`LlmError`] (`docs/LLM.md` §6). All Gemini
/// error-body field names are confined here.
fn map_error_response(status: u16, retry_after_header: Option<Duration>, body: &str) -> LlmError {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let error = parsed.as_ref().and_then(|v| v.get("error"));
    let rpc_status = error
        .and_then(|e| e.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let message = error
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    match status {
        429 => {
            let retry_after = retry_after_header.or_else(|| retry_info_delay(error));
            LlmError::RateLimited { retry_after }
        }
        // 503 = overloaded / capacity / flex shed: retry, and after repeated
        // sheds the `Retrying` layer falls back to the standard tier (§6).
        503 => LlmError::Shed,
        500 | 502 | 504 => LlmError::Server { status },
        400 if is_tier_rejection(rpc_status, &message) => LlmError::TierUnsupported { message },
        400 | 401 | 403 | 404 => LlmError::Fatal { status, message },
        other if other >= 500 => LlmError::Server { status: other },
        other => LlmError::Fatal {
            status: other,
            message,
        },
    }
}

/// Heuristic: a 400 whose status/message points at the service tier is a tier
/// rejection (handled by a one-shot standard retry, §1) rather than a plain fatal
/// request error.
fn is_tier_rejection(rpc_status: &str, message: &str) -> bool {
    let haystack = format!("{rpc_status} {message}").to_lowercase();
    haystack.contains("service tier")
        || haystack.contains("servicetier")
        || haystack.contains("service_tier")
        || haystack.contains("flex tier")
}

/// Reads `google.rpc.RetryInfo.retryDelay` from the `error.details` array.
fn retry_info_delay(error: Option<&Value>) -> Option<Duration> {
    let details = error
        .and_then(|e| e.get("details"))
        .and_then(Value::as_array)?;
    for detail in details {
        let is_retry_info = detail
            .get("@type")
            .and_then(Value::as_str)
            .is_some_and(|t| t.contains("RetryInfo"));
        if is_retry_info
            && let Some(delay) = detail.get("retryDelay").and_then(Value::as_str)
            && let Some(duration) = parse_proto_duration(delay)
        {
            return Some(duration);
        }
    }
    None
}

/// Parses a protobuf duration JSON string such as `"5s"`, `"1.5s"` or `"0.200s"`.
fn parse_proto_duration(text: &str) -> Option<Duration> {
    let seconds = text.strip_suffix('s')?.trim();
    let value: f64 = seconds.parse().ok()?;
    if value.is_finite() && value >= 0.0 {
        Some(Duration::from_secs_f64(value))
    } else {
        None
    }
}

/// Parses a `Retry-After` header expressed as a (possibly fractional) number of
/// seconds. An HTTP-date form is not honored here (we fall back to `RetryInfo` or
/// backoff instead).
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

    fn gc() -> GenerationConfig {
        GenerationConfig {
            max_output_tokens: 320,
            response_mime_type: "application/json".to_string(),
            response_schema: Some(json!({ "type": "object" })),
            thinking: ThinkingSetting::Off,
        }
    }

    fn request() -> LlmRequest {
        LlmRequest {
            model: "gemini-flash-latest".to_string(),
            service_tier: ServiceTier::Flex,
            system_instruction: "sys".to_string(),
            contents: vec![ContentBlock::user("sheet"), ContentBlock::user("vars")],
            generation_config: gc(),
        }
    }

    #[test]
    fn request_body_has_ordered_contents_and_generation_config() {
        let body = build_request_body(&request());
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "sys");
        assert_eq!(body["contents"][0]["parts"][0]["text"], "sheet");
        assert_eq!(body["contents"][1]["parts"][0]["text"], "vars");
        assert_eq!(body["contents"][0]["role"], "user");
        let config = &body["generationConfig"];
        assert_eq!(config["responseMimeType"], "application/json");
        assert_eq!(config["responseJsonSchema"]["type"], "object");
        assert_eq!(config["maxOutputTokens"], 320);
        assert_eq!(body["serviceTier"], "flex");
    }

    #[test]
    fn empty_system_instruction_is_omitted() {
        let mut req = request();
        req.system_instruction.clear();
        let body = build_request_body(&req);
        assert!(body.get("systemInstruction").is_none());
    }

    #[test]
    fn thinking_budget_for_flash_latest_off_is_zero() {
        let value = thinking_config_json("gemini-flash-latest", ThinkingSetting::Off);
        assert_eq!(value["thinkingBudget"], 0);
        assert!(value.get("thinkingLevel").is_none());
    }

    #[test]
    fn thinking_level_for_gemini_3_models() {
        let value = thinking_config_json("gemini-3-flash", ThinkingSetting::Low);
        assert_eq!(value["thinkingLevel"], "LOW");
        assert!(value.get("thinkingBudget").is_none());
    }

    #[test]
    fn success_body_parses_text_and_usage() {
        let body = json!({
            "candidates": [ { "content": { "parts": [ { "text": "hel" }, { "text": "lo" } ] } } ],
            "usageMetadata": {
                "promptTokenCount": 11,
                "cachedContentTokenCount": 4,
                "candidatesTokenCount": 6,
                "thoughtsTokenCount": 2
            }
        })
        .to_string();
        let resp = parse_success(&body).expect("parses");
        assert_eq!(resp.text, "hello");
        assert_eq!(resp.usage.prompt_tokens, 11);
        assert_eq!(resp.usage.cached_tokens, 4);
        assert_eq!(resp.usage.output_tokens, 6);
        assert_eq!(resp.usage.thinking_tokens, 2);
    }

    #[test]
    fn missing_candidates_is_decode_error() {
        let body = json!({ "usageMetadata": {} }).to_string();
        assert!(matches!(parse_success(&body), Err(LlmError::Decode(_))));
    }

    // AC: M3 security — Debug-formatting the transport must never print the key.
    #[test]
    fn debug_does_not_leak_api_key() {
        let key = "super-secret-key-value";
        let gemini =
            Gemini::new("https://example.test", key, Duration::from_secs(5)).expect("build gemini");
        let debug = format!("{gemini:?}");
        assert!(!debug.contains(key), "api key leaked in Debug: {debug}");
        assert!(debug.contains("redacted"));
    }

    // AC: M13-AC3 — a `gemini:` (or any `provider:`) prefix is stripped before the
    // wire URL is built; a bare model id is unchanged (backward compatible).
    #[test]
    fn endpoint_strips_provider_prefix() {
        let gemini = Gemini::new("https://example.test/v1", "k", Duration::from_secs(5))
            .expect("build gemini");
        assert_eq!(
            gemini.endpoint("gemini:gemini-flash-latest"),
            "https://example.test/v1/models/gemini-flash-latest:generateContent"
        );
        assert_eq!(
            gemini.endpoint("gemini-flash-latest"),
            "https://example.test/v1/models/gemini-flash-latest:generateContent"
        );
    }

    #[test]
    fn usage_reads_applied_service_tier() {
        let body = json!({
            "candidates": [ { "content": { "parts": [ { "text": "{}" } ] } } ],
            "usageMetadata": { "promptTokenCount": 1, "serviceTier": "flex" }
        })
        .to_string();
        let resp = parse_success(&body).expect("parses");
        assert_eq!(resp.usage.applied_tier.as_deref(), Some("flex"));
    }

    #[test]
    fn usage_applied_tier_is_none_when_absent() {
        let body = json!({
            "candidates": [ { "content": { "parts": [ { "text": "{}" } ] } } ],
            "usageMetadata": { "promptTokenCount": 1 }
        })
        .to_string();
        let resp = parse_success(&body).expect("parses");
        assert_eq!(resp.usage.applied_tier, None);
    }

    #[test]
    fn retry_info_delay_is_read_from_details() {
        let error = json!({
            "status": "RESOURCE_EXHAUSTED",
            "details": [
                { "@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "7s" }
            ]
        });
        assert_eq!(retry_info_delay(Some(&error)), Some(Duration::from_secs(7)));
    }

    #[test]
    fn proto_duration_parses_fractional_seconds() {
        assert_eq!(
            parse_proto_duration("1.5s"),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(
            parse_proto_duration("0.200s"),
            Some(Duration::from_millis(200))
        );
        assert_eq!(parse_proto_duration("bad"), None);
    }

    #[test]
    fn error_status_mapping() {
        assert!(matches!(
            map_error_response(503, None, "{}"),
            LlmError::Shed
        ));
        assert!(matches!(
            map_error_response(500, None, "{}"),
            LlmError::Server { status: 500 }
        ));
        assert!(matches!(
            map_error_response(404, None, "{}"),
            LlmError::Fatal { status: 404, .. }
        ));
    }

    #[test]
    fn tier_rejection_maps_to_tier_unsupported() {
        let body = json!({
            "error": { "status": "INVALID_ARGUMENT", "message": "Service tier flex is not supported for this model" }
        })
        .to_string();
        assert!(matches!(
            map_error_response(400, None, &body),
            LlmError::TierUnsupported { .. }
        ));
    }

    #[test]
    fn plain_400_is_fatal() {
        let body = json!({ "error": { "status": "INVALID_ARGUMENT", "message": "bad schema" } })
            .to_string();
        assert!(matches!(
            map_error_response(400, None, &body),
            LlmError::Fatal { status: 400, .. }
        ));
    }
}
