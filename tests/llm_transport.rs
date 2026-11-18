//! Gemini transport + `Retrying` layer, driven by `wiremock` (never the network).
//!
//! Covers M3-AC1 (request shape, auth header, usage parsing, 429 with
//! `Retry-After`, 429 with `google.rpc.RetryInfo`, 5xx retry, 400 fatal, shed →
//! tier fallback) and M3-AC7 (backoff interrupted by cancellation).

use std::time::Duration;

use myansub::llm::client::{
    ContentBlock, GenerationConfig, LlmClient, LlmError, LlmRequest, ServiceTier, ThinkingSetting,
};
use myansub::llm::fake::Fake;
use myansub::llm::gemini::Gemini;
use myansub::llm::retry::RetryConfig;
use myansub::llm::retrying::Retrying;
use myansub::llm::schemas::translation_response_schema;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "gemini-flash-latest";
const API_KEY: &str = "secret-key-123";

fn gemini(server: &MockServer) -> Gemini {
    Gemini::new(server.uri(), API_KEY, Duration::from_secs(120)).expect("build gemini")
}

fn request(tier: ServiceTier) -> LlmRequest {
    LlmRequest {
        model: MODEL.to_string(),
        service_tier: tier,
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

fn success_body() -> serde_json::Value {
    serde_json::json!({
        "candidates": [ { "content": { "parts": [ { "text": "{\"lines\":[]}" } ] } } ],
        "usageMetadata": {
            "promptTokenCount": 100,
            "cachedContentTokenCount": 40,
            "candidatesTokenCount": 20,
            "thoughtsTokenCount": 5
        }
    })
}

fn gen_path() -> String {
    format!("/models/{MODEL}:generateContent")
}

fn retry_cfg() -> RetryConfig {
    RetryConfig {
        max_retries: 6,
        // Zero backoff keeps retries instant so no real time is slept; the
        // Retry-After and cancellation tests control time explicitly instead.
        backoff_base_ms: 0,
        backoff_max_ms: 0,
        max_sheds_before_fallback: 3,
        service_tier_fallback: true,
    }
}

// AC: M3-AC1 — request shape (method, path, body) and the auth header.
#[tokio::test]
async fn request_shape_and_auth_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(gen_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(success_body()))
        .mount(&server)
        .await;

    gemini(&server)
        .generate(request(ServiceTier::Flex))
        .await
        .expect("request succeeds");

    let received = server.received_requests().await.expect("has request log");
    assert_eq!(received.len(), 1);
    let req = &received[0];
    assert_eq!(req.method, wiremock::http::Method::POST);
    assert_eq!(req.url.path(), gen_path());

    // Auth header carries exactly the configured key.
    let key = req
        .headers
        .get("x-goog-api-key")
        .expect("auth header present");
    assert_eq!(key, API_KEY);

    let body: serde_json::Value = serde_json::from_slice(&req.body).expect("json body");
    assert_eq!(
        body["systemInstruction"]["parts"][0]["text"],
        "you are a translator"
    );
    // Ordered content blocks (cached prefix first, variable block second).
    assert_eq!(body["contents"][0]["parts"][0]["text"], "analysis sheet");
    assert_eq!(
        body["contents"][1]["parts"][0]["text"],
        "TRANSLATE:\n[1] hi"
    );
    let gc = &body["generationConfig"];
    assert_eq!(gc["responseMimeType"], "application/json");
    assert!(gc["responseJsonSchema"].is_object());
    assert_eq!(gc["maxOutputTokens"], 320);
}

// AC: M3-AC1 — usageMetadata is parsed into all four Usage counts.
#[tokio::test]
async fn usage_metadata_is_parsed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(gen_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(success_body()))
        .mount(&server)
        .await;

    let resp = gemini(&server)
        .generate(request(ServiceTier::Flex))
        .await
        .expect("succeeds");
    assert_eq!(resp.text, "{\"lines\":[]}");
    assert_eq!(resp.usage.prompt_tokens, 100);
    assert_eq!(resp.usage.cached_tokens, 40);
    assert_eq!(resp.usage.output_tokens, 20);
    assert_eq!(resp.usage.thinking_tokens, 5);
}

// AC: M3-AC1 — 429 with a Retry-After header maps to RateLimited with that delay.
#[tokio::test]
async fn rate_limited_reads_retry_after_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(gen_path()))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "5"))
        .mount(&server)
        .await;

    let err = gemini(&server)
        .generate(request(ServiceTier::Flex))
        .await
        .expect_err("429 is an error");
    match err {
        LlmError::RateLimited { retry_after } => {
            assert_eq!(retry_after, Some(Duration::from_secs(5)));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

// AC: M3-AC1 — via Retrying, a 429's Retry-After delays the next request by
// exactly that long. Driven through the Fake transport (no network I/O) so the
// virtual clock is deterministic; the wire parsing of the header into
// `RateLimited` is proven by `rate_limited_reads_retry_after_header` above.
#[tokio::test(start_paused = true)]
async fn retrying_waits_the_retry_after_delay() {
    let fake = Fake::new()
        .push_result(Err(LlmError::RateLimited {
            retry_after: Some(Duration::from_secs(5)),
        }))
        .push_response(myansub::llm::client::LlmResponse {
            text: "{\"lines\":[]}".to_string(),
            usage: myansub::llm::client::Usage {
                prompt_tokens: 100,
                ..Default::default()
            },
            commit: myansub::llm::client::CommitToken::noop(),
        });

    let retrying = Retrying::new(
        fake.clone(),
        retry_cfg(),
        CancellationToken::new(),
        /* seed */ 1,
    );

    let start = tokio::time::Instant::now();
    let resp = retrying
        .generate(request(ServiceTier::Flex))
        .await
        .expect("retries then succeeds");
    let elapsed = start.elapsed();

    assert_eq!(resp.usage.prompt_tokens, 100);
    // The explicit Retry-After is honored exactly (no jitter) — the second call is
    // sent 5 virtual seconds later.
    assert_eq!(elapsed, Duration::from_secs(5));
    assert_eq!(fake.call_count(), 2);
}

// AC: M3-AC1 — 429 carrying google.rpc.RetryInfo.retryDelay (no header) parses
// the delay from the error body.
#[tokio::test]
async fn rate_limited_reads_retry_info_from_body() {
    let server = MockServer::start().await;
    let body = serde_json::json!({
        "error": {
            "code": 429,
            "status": "RESOURCE_EXHAUSTED",
            "message": "quota",
            "details": [
                { "@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "8s" }
            ]
        }
    });
    Mock::given(method("POST"))
        .and(path(gen_path()))
        .respond_with(ResponseTemplate::new(429).set_body_json(body))
        .mount(&server)
        .await;

    let err = gemini(&server)
        .generate(request(ServiceTier::Flex))
        .await
        .expect_err("429 error");
    match err {
        LlmError::RateLimited { retry_after } => {
            assert_eq!(retry_after, Some(Duration::from_secs(8)));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

// AC: M3-AC1 — a 5xx is retried and then succeeds.
#[tokio::test]
async fn server_error_is_retried_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(gen_path()))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(gen_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(success_body()))
        .with_priority(2)
        .mount(&server)
        .await;

    let retrying = Retrying::new(gemini(&server), retry_cfg(), CancellationToken::new(), 1);
    let resp = retrying
        .generate(request(ServiceTier::Flex))
        .await
        .expect("retries then succeeds");
    assert_eq!(resp.usage.output_tokens, 20);
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

// AC: M3-AC1 — a 400 is fatal and is NOT retried (one request reaches the server).
#[tokio::test]
async fn fatal_400_is_not_retried() {
    let server = MockServer::start().await;
    let body = serde_json::json!({
        "error": { "code": 400, "status": "INVALID_ARGUMENT", "message": "bad schema" }
    });
    Mock::given(method("POST"))
        .and(path(gen_path()))
        .respond_with(ResponseTemplate::new(400).set_body_json(body))
        .mount(&server)
        .await;

    let retrying = Retrying::new(gemini(&server), retry_cfg(), CancellationToken::new(), 1);
    let err = retrying
        .generate(request(ServiceTier::Flex))
        .await
        .expect_err("400 is fatal");
    assert!(matches!(err, LlmError::Fatal { status: 400, .. }));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

// AC: M3-AC1 — repeated flex sheds trigger a fallback to the standard tier.
#[tokio::test]
async fn repeated_sheds_fall_back_to_standard_tier() {
    let server = MockServer::start().await;
    // Flex requests are shed (503); standard requests succeed. The two mocks are
    // distinguished by the serviceTier field in the request body.
    Mock::given(method("POST"))
        .and(path(gen_path()))
        .and(body_string_contains("\"serviceTier\":\"flex\""))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(gen_path()))
        .and(body_string_contains("\"serviceTier\":\"standard\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(success_body()))
        .mount(&server)
        .await;

    let retrying = Retrying::new(gemini(&server), retry_cfg(), CancellationToken::new(), 1);
    let resp = retrying
        .generate(request(ServiceTier::Flex))
        .await
        .expect("falls back and succeeds");
    assert_eq!(resp.usage.prompt_tokens, 100);

    let received = server.received_requests().await.unwrap();
    let flex = received
        .iter()
        .filter(|r| String::from_utf8_lossy(&r.body).contains("\"serviceTier\":\"flex\""))
        .count();
    let standard = received
        .iter()
        .filter(|r| String::from_utf8_lossy(&r.body).contains("\"serviceTier\":\"standard\""))
        .count();
    // 3 flex sheds (threshold), then a standard request that succeeds.
    assert_eq!(flex, 3);
    assert_eq!(standard, 1);
}

fn tier_rejection_body() -> serde_json::Value {
    serde_json::json!({
        "error": {
            "code": 400,
            "status": "INVALID_ARGUMENT",
            "message": "Service tier flex is not supported for this model"
        }
    })
}

// AC: M3-AC1 — a flex tier rejection makes the layer retry once on standard and
// succeed; exactly one flex and one standard request are sent.
#[tokio::test]
async fn tier_rejection_retries_once_on_standard() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(gen_path()))
        .and(body_string_contains("\"serviceTier\":\"flex\""))
        .respond_with(ResponseTemplate::new(400).set_body_json(tier_rejection_body()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(gen_path()))
        .and(body_string_contains("\"serviceTier\":\"standard\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(success_body()))
        .mount(&server)
        .await;

    let retrying = Retrying::new(gemini(&server), retry_cfg(), CancellationToken::new(), 1);
    let resp = retrying
        .generate(request(ServiceTier::Flex))
        .await
        .expect("retries on standard and succeeds");
    assert_eq!(resp.usage.prompt_tokens, 100);

    let received = server.received_requests().await.unwrap();
    let flex = received
        .iter()
        .filter(|r| String::from_utf8_lossy(&r.body).contains("\"serviceTier\":\"flex\""))
        .count();
    let standard = received
        .iter()
        .filter(|r| String::from_utf8_lossy(&r.body).contains("\"serviceTier\":\"standard\""))
        .count();
    assert_eq!(flex, 1);
    assert_eq!(standard, 1);
}

// AC: M3-AC1 — if standard is also rejected, the layer returns TierUnsupported and
// does NOT loop (bounded to one flex + one standard request).
#[tokio::test]
async fn tier_rejection_on_both_tiers_does_not_loop() {
    let server = MockServer::start().await;
    // A single mock matching any tier returns a tier rejection for every request.
    Mock::given(method("POST"))
        .and(path(gen_path()))
        .respond_with(ResponseTemplate::new(400).set_body_json(tier_rejection_body()))
        .mount(&server)
        .await;

    let retrying = Retrying::new(gemini(&server), retry_cfg(), CancellationToken::new(), 1);
    let err = retrying
        .generate(request(ServiceTier::Flex))
        .await
        .expect_err("both tiers rejected");
    assert!(
        matches!(err, LlmError::TierUnsupported { .. }),
        "got {err:?}"
    );

    let received = server.received_requests().await.unwrap();
    assert_eq!(
        received.len(),
        2,
        "expected exactly one flex + one standard try"
    );
}

// AC: M3-AC7 — a backoff sleep is interrupted by cancellation: the layer returns
// Cancelled promptly instead of sleeping out the (long) backoff.
#[tokio::test(start_paused = true)]
async fn backoff_is_interrupted_by_cancellation() {
    let token = CancellationToken::new();
    // The fake cancels the token on its first (and only) call, then returns a
    // retryable error; the Retrying layer must abort its backoff immediately.
    let fake = Fake::new()
        .cancel_on_call(token.clone())
        .always(|| Err(LlmError::Timeout));

    let cfg = RetryConfig {
        max_retries: 10,
        // A deliberately huge backoff: without cancellation the layer would sleep
        // ~60s of virtual time between retries.
        backoff_base_ms: 60_000,
        backoff_max_ms: 60_000,
        max_sheds_before_fallback: 3,
        service_tier_fallback: true,
    };
    let retrying = Retrying::new(fake.clone(), cfg, token, 1);

    let start = tokio::time::Instant::now();
    let err = retrying
        .generate(request(ServiceTier::Flex))
        .await
        .expect_err("cancelled");
    let elapsed = start.elapsed();

    assert!(matches!(err, LlmError::Cancelled), "got {err:?}");
    // Exactly one transport call: cancellation stopped the first backoff before a
    // second attempt, so no full backoff was slept.
    assert_eq!(fake.call_count(), 1);
    assert!(
        elapsed < Duration::from_secs(1),
        "returned after {elapsed:?}, expected prompt cancellation"
    );
}
