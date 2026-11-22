//! OpenRouter transport, driven by `wiremock` (never the network).
//!
//! Covers M13-AC8 (request shape, auth header, cache_control, response_format,
//! session_id), M13-AC11 (usage parsing), and M13-AC13 (one case per row of the
//! `docs/LLM.md` §10.6 error table).

use std::time::Duration;

use myansub::llm::catalog::{ModelCatalog, OpenRouterCatalog, ReasoningLevel};
use myansub::llm::client::{
    ContentBlock, GenerationConfig, LlmClient, LlmError, LlmRequest, ServiceTier, ThinkingSetting,
};
use myansub::llm::openrouter::{OpenRouter, OpenRouterConfig};
use myansub::llm::retry::RetryConfig;
use myansub::llm::retrying::Retrying;
use myansub::llm::schemas::translation_response_schema;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "openrouter:anthropic/claude-sonnet-5.5";
const BARE: &str = "anthropic/claude-sonnet-5.5";
const API_KEY: &str = "or-secret-123";

fn openrouter(server: &MockServer) -> OpenRouter {
    OpenRouter::new(
        server.uri(),
        API_KEY,
        Duration::from_secs(120),
        OpenRouterConfig::default(),
    )
    .expect("build openrouter")
}

fn request() -> LlmRequest {
    LlmRequest {
        model: MODEL.to_string(),
        service_tier: ServiceTier::Floor,
        system_instruction: "you are a translator".to_string(),
        contents: vec![
            ContentBlock::user("analysis sheet"),
            ContentBlock::user("TRANSLATE:\n[1] hi"),
        ],
        generation_config: GenerationConfig {
            max_output_tokens: 320,
            response_mime_type: "application/json".to_string(),
            response_schema: Some(translation_response_schema()),
            thinking: ThinkingSetting::Off,
        },
    }
}

fn request_with_thinking(thinking: ThinkingSetting) -> LlmRequest {
    LlmRequest {
        generation_config: GenerationConfig {
            thinking,
            ..request().generation_config
        },
        ..request()
    }
}

/// Builds a transport whose config carries the given catalog-derived reasoning levels.
fn openrouter_with_levels(server: &MockServer, levels: Vec<ReasoningLevel>) -> OpenRouter {
    OpenRouter::new(
        server.uri(),
        API_KEY,
        Duration::from_secs(120),
        OpenRouterConfig {
            reasoning_levels: levels,
            ..OpenRouterConfig::default()
        },
    )
    .expect("build openrouter")
}

fn success_body() -> serde_json::Value {
    serde_json::json!({
        "id": "gen-xyz",
        "provider": "Anthropic",
        "choices": [ { "message": { "content": "{\"lines\":[]}" }, "finish_reason": "stop" } ],
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "cost": 0.0021,
            "prompt_tokens_details": { "cached_tokens": 40 },
            "completion_tokens_details": { "reasoning_tokens": 5 }
        }
    })
}

fn retry_cfg(max_retries: u32) -> RetryConfig {
    RetryConfig {
        max_retries,
        backoff_base_ms: 0,
        backoff_max_ms: 0,
        max_sheds_before_fallback: 3,
        service_tier_fallback: true,
    }
}

async fn mount_once(server: &MockServer, template: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(template)
        .mount(server)
        .await;
}

// AC: M13-AC8 — request shape (method, path, body) and the Bearer auth header.
#[tokio::test]
async fn request_shape_and_auth_header() {
    let server = MockServer::start().await;
    mount_once(
        &server,
        ResponseTemplate::new(200).set_body_json(success_body()),
    )
    .await;

    openrouter(&server)
        .generate(request())
        .await
        .expect("request succeeds");

    let received = server.received_requests().await.expect("has request log");
    assert_eq!(received.len(), 1);
    let req = &received[0];
    assert_eq!(req.method, wiremock::http::Method::POST);
    assert_eq!(req.url.path(), "/chat/completions");

    let auth = req
        .headers
        .get("authorization")
        .expect("auth header present");
    assert_eq!(auth, &format!("Bearer {API_KEY}"));

    let body: serde_json::Value = serde_json::from_slice(&req.body).expect("json body");
    // The provider prefix is never sent on the wire (bare model only).
    assert_eq!(body["model"], BARE);
    assert_eq!(body["max_tokens"], 320);
    // Messages: system → user(sheet, cache_control) → user(variable).
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(
        body["messages"][0]["content"][0]["text"],
        "you are a translator"
    );
    assert_eq!(body["messages"][1]["content"][0]["text"], "analysis sheet");
    assert_eq!(
        body["messages"][1]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert_eq!(
        body["messages"][2]["content"][0]["text"],
        "TRANSLATE:\n[1] hi"
    );
    assert_eq!(body["response_format"]["type"], "json_schema");
    assert_eq!(body["response_format"]["json_schema"]["strict"], true);
    assert_eq!(body["provider"]["require_parameters"], true);
    assert!(body["session_id"].is_string());
}

// AC: M13-AC11 — usage parsing fills cost_usd and the served-* fields.
#[tokio::test]
async fn usage_is_parsed() {
    let server = MockServer::start().await;
    mount_once(
        &server,
        ResponseTemplate::new(200).set_body_json(success_body()),
    )
    .await;

    let resp = openrouter(&server)
        .generate(request())
        .await
        .expect("succeeds");
    assert_eq!(resp.text, "{\"lines\":[]}");
    assert_eq!(resp.usage.prompt_tokens, 100);
    assert_eq!(resp.usage.cached_tokens, 40);
    assert_eq!(resp.usage.output_tokens, 20);
    assert_eq!(resp.usage.cost_usd, Some(0.0021));
    assert_eq!(resp.usage.generation_id.as_deref(), Some("gen-xyz"));
    assert_eq!(resp.usage.served_provider.as_deref(), Some("Anthropic"));
}

// AC: M13-AC13 — 400 is fatal (unit fails) and is not retried.
#[tokio::test]
async fn error_400_is_fatal() {
    let server = MockServer::start().await;
    mount_once(
        &server,
        ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": { "code": 400, "message": "bad request" }
        })),
    )
    .await;

    let err = openrouter(&server)
        .generate(request())
        .await
        .expect_err("400 is fatal");
    assert!(
        matches!(err, LlmError::Fatal { status: 400, .. }),
        "{err:?}"
    );
}

// AC: M13-AC13 — 401 is fatal and names OPENROUTER_API_KEY.
#[tokio::test]
async fn error_401_names_the_key() {
    let server = MockServer::start().await;
    mount_once(
        &server,
        ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": { "code": 401, "message": "invalid key" }
        })),
    )
    .await;

    let err = openrouter(&server)
        .generate(request())
        .await
        .expect_err("401 is fatal");
    match err {
        LlmError::Fatal {
            status: 401,
            message,
        } => {
            assert!(message.contains("OPENROUTER_API_KEY"), "{message}");
        }
        other => panic!("expected Fatal 401, got {other:?}"),
    }
}

// AC: M13-AC13 — 402 maps to Credits (run stops).
#[tokio::test]
async fn error_402_is_credits() {
    let server = MockServer::start().await;
    mount_once(
        &server,
        ResponseTemplate::new(402).set_body_json(serde_json::json!({
            "error": { "code": 402, "message": "no credits" }
        })),
    )
    .await;

    let err = openrouter(&server)
        .generate(request())
        .await
        .expect_err("402 is credits");
    assert!(matches!(err, LlmError::Credits { .. }), "{err:?}");
}

// AC: M13-AC13 — 403 moderation maps to ContentRefused (batch fails, run continues).
#[tokio::test]
async fn error_403_moderation_is_content_refused() {
    let server = MockServer::start().await;
    mount_once(
        &server,
        ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "error": { "code": 403, "message": "flagged by moderation" }
        })),
    )
    .await;

    let err = openrouter(&server)
        .generate(request())
        .await
        .expect_err("403 is refused");
    assert!(matches!(err, LlmError::ContentRefused { .. }), "{err:?}");
}

// AC: M13-AC13 — 429 maps to RateLimited honoring Retry-After.
#[tokio::test]
async fn error_429_honors_retry_after() {
    let server = MockServer::start().await;
    mount_once(
        &server,
        ResponseTemplate::new(429).insert_header("Retry-After", "7"),
    )
    .await;

    let err = openrouter(&server)
        .generate(request())
        .await
        .expect_err("429 is rate limited");
    match err {
        LlmError::RateLimited { retry_after } => {
            assert_eq!(retry_after, Some(Duration::from_secs(7)));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

// AC: M13-AC13 — 408 is retryable and honors Retry-After when present.
#[tokio::test]
async fn error_408_honors_retry_after() {
    let server = MockServer::start().await;
    mount_once(
        &server,
        ResponseTemplate::new(408).insert_header("Retry-After", "3"),
    )
    .await;

    let err = openrouter(&server)
        .generate(request())
        .await
        .expect_err("408 errors");
    match err {
        LlmError::RateLimited { retry_after } => {
            assert_eq!(retry_after, Some(Duration::from_secs(3)));
        }
        other => panic!("expected RateLimited honoring Retry-After, got {other:?}"),
    }
}

// AC: M13-AC13 — 502 is retryable.
#[tokio::test]
async fn error_502_is_retryable() {
    let server = MockServer::start().await;
    mount_once(&server, ResponseTemplate::new(502)).await;

    let err = openrouter(&server)
        .generate(request())
        .await
        .expect_err("502 errors");
    assert_eq!(err.class(), myansub::llm::client::ErrorClass::Retryable);
}

// AC: M13-AC13 — 502 with a Retry-After honors the server delay (RateLimited).
#[tokio::test]
async fn error_502_honors_retry_after() {
    let server = MockServer::start().await;
    mount_once(
        &server,
        ResponseTemplate::new(502).insert_header("Retry-After", "5"),
    )
    .await;

    let err = openrouter(&server)
        .generate(request())
        .await
        .expect_err("502 errors");
    match err {
        LlmError::RateLimited { retry_after } => {
            assert_eq!(retry_after, Some(Duration::from_secs(5)));
        }
        other => panic!("expected RateLimited honoring Retry-After, got {other:?}"),
    }
}

// AC: M13-AC13 — 503 (no provider meets routing) is retried the configured number of
// times, then gives up with an error naming the routing settings.
#[tokio::test]
async fn error_503_retries_then_fatal_naming_routing() {
    let server = MockServer::start().await;
    mount_once(&server, ResponseTemplate::new(503)).await;

    let retrying = Retrying::new(
        openrouter(&server),
        retry_cfg(3),
        CancellationToken::new(),
        1,
    );
    let err = retrying
        .generate(request())
        .await
        .expect_err("503 gives up after retries");

    // 1 initial + 3 retries.
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
    let msg = err.to_string();
    assert!(msg.contains("tier"), "should name routing settings: {msg}");
    assert!(msg.contains("data_collection"), "{msg}");
    assert!(msg.contains("zdr"), "{msg}");
}

// AC: M13-AC10 — the transport downgrades a request's reasoning effort to the nearest
// supported LOWER level on the wire when the model (per the catalog-derived config)
// does not support the requested level. LLM.md §10.4.
#[tokio::test]
async fn reasoning_effort_downgraded_to_nearest_lower_on_wire() {
    let server = MockServer::start().await;
    mount_once(
        &server,
        ResponseTemplate::new(200).set_body_json(success_body()),
    )
    .await;

    // A model reporting only [minimal, low] (a trimmed catalog subset). A `High`
    // request must be downgraded to `low` (the nearest supported lower level).
    let transport =
        openrouter_with_levels(&server, vec![ReasoningLevel::Minimal, ReasoningLevel::Low]);
    transport
        .generate(request_with_thinking(ThinkingSetting::High))
        .await
        .expect("request succeeds");

    let received = server.received_requests().await.expect("has request log");
    let body: serde_json::Value = serde_json::from_slice(&received[0].body).expect("json body");
    assert_eq!(
        body["reasoning"]["effort"], "low",
        "High must downgrade to the nearest supported lower level, got {:?}",
        body["reasoning"]
    );
}

// AC: M13-AC10 — a non-reasoning OpenRouter model (per the catalog) never receives
// `reasoning.effort`, so `provider.require_parameters=true` cannot turn it into a 503.
// This threads the catalog's `supported_reasoning_levels` (empty) into the transport.
// LLM.md §10.4.
#[tokio::test]
async fn reasoning_omitted_for_non_reasoning_catalog_model() {
    let server = MockServer::start().await;
    mount_once(
        &server,
        ResponseTemplate::new(200).set_body_json(success_body()),
    )
    .await;

    // A trimmed /models fixture whose model declares no `reasoning` parameter.
    let catalog_body = serde_json::json!({
        "data": [ {
            "id": BARE,
            "context_length": 32768,
            "pricing": { "prompt": "0.000001", "completion": "0.000002" },
            "supported_parameters": ["response_format", "max_tokens"]
        } ]
    });
    let catalog = OpenRouterCatalog::from_models_bytes(catalog_body.to_string().as_bytes())
        .expect("parse catalog");
    let levels = catalog.supported_reasoning_levels(BARE);
    assert!(levels.is_empty(), "fixture model has no reasoning support");

    let transport = openrouter_with_levels(&server, levels);
    transport
        .generate(request_with_thinking(ThinkingSetting::High))
        .await
        .expect("request succeeds");

    let received = server.received_requests().await.expect("has request log");
    let body: serde_json::Value = serde_json::from_slice(&received[0].body).expect("json body");
    assert!(
        body.get("reasoning").is_none(),
        "a non-reasoning model must not receive reasoning.effort, got {:?}",
        body.get("reasoning")
    );
}

// AC: M13-AC13 — an error inside a 200 body (choices[0].error) is detected.
#[tokio::test]
async fn in_body_error_on_200_is_mapped() {
    let server = MockServer::start().await;
    mount_once(
        &server,
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [ { "error": { "code": 402, "message": "no credits" } } ]
        })),
    )
    .await;

    let err = openrouter(&server)
        .generate(request())
        .await
        .expect_err("in-body error is surfaced");
    assert!(matches!(err, LlmError::Credits { .. }), "{err:?}");
}
