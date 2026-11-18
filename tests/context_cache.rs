//! The Gemini explicit context-cache lifecycle (`docs/LLM.md` §5.5), driven by
//! `wiremock` (never the network) with an injected epoch clock so the
//! reuse-vs-recreate decision is deterministic (`docs/TESTING.md` §2, ADR-042).
//!
//! Covers M8-AC2: reuse on resume when unexpired, recreate when expired, create when
//! none, and delete at stage end.

use std::time::Duration;

use myansub::llm::context_cache::{self, CacheSpec, ProviderCacheEntry, ProviderCacheStore};
use myansub::llm::gemini::Gemini;
use myansub::store::Store;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "gemini-flash-latest";
const API_KEY: &str = "secret-key-123";

fn gemini(server: &MockServer) -> Gemini {
    Gemini::new(server.uri(), API_KEY, Duration::from_secs(120)).expect("build gemini")
}

fn spec() -> CacheSpec {
    CacheSpec {
        analysis_version: 3,
        model: MODEL.to_string(),
        system_instruction: "you are a translator".to_string(),
        content: "analysis sheet".to_string(),
        ttl_secs: 900,
    }
}

fn create_body(name: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "model": format!("models/{MODEL}"),
        "expireTime": "2099-01-01T00:00:00Z",
        "usageMetadata": { "totalTokenCount": 4096 }
    })
}

async fn seed(store: &Store, entry: ProviderCacheEntry) {
    store.upsert(entry).await.expect("seed provider_cache row");
}

// AC: M8-AC2 — a stored row whose expiry is in the FUTURE is reused on resume: no
// create request is sent and the stored name is returned.
#[tokio::test]
async fn reuses_unexpired_cache_without_creating() {
    let server = MockServer::start().await;
    // No mocks mounted: any outgoing request would 404, proving none is sent.
    let store = Store::open_in_memory().expect("store");

    let now = 1_000_000;
    seed(
        &store,
        ProviderCacheEntry {
            name: "cachedContents/existing".to_string(),
            analysis_version: 3,
            model: MODEL.to_string(),
            token_count: 4096,
            expires_at: now + 500, // still valid
        },
    )
    .await;

    let name = context_cache::get_or_create(&gemini(&server), &store, &spec(), now)
        .await
        .expect("reuse");
    assert_eq!(name, "cachedContents/existing");

    // No HTTP request reached the server.
    let received = server.received_requests().await.expect("log");
    assert!(
        received.is_empty(),
        "reuse must not call the API: {received:?}"
    );
}

// AC: M8-AC2 — a stored row whose expiry is in the PAST is recreated: the old cache
// is DELETEd, a new one is CREATEd, and the provider_cache row is updated with the
// new name and expiry.
#[tokio::test]
async fn recreates_expired_cache() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/cachedContents/old"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/cachedContents"))
        .respond_with(ResponseTemplate::new(200).set_body_json(create_body("cachedContents/new")))
        .mount(&server)
        .await;

    let store = Store::open_in_memory().expect("store");
    let now = 1_000_000;
    seed(
        &store,
        ProviderCacheEntry {
            name: "cachedContents/old".to_string(),
            analysis_version: 3,
            model: MODEL.to_string(),
            token_count: 4096,
            expires_at: now - 1, // expired
        },
    )
    .await;

    let name = context_cache::get_or_create(&gemini(&server), &store, &spec(), now)
        .await
        .expect("recreate");
    assert_eq!(name, "cachedContents/new");

    let received = server.received_requests().await.expect("log");
    let deletes = received
        .iter()
        .filter(|r| r.method == wiremock::http::Method::DELETE)
        .count();
    let creates = received
        .iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .count();
    assert_eq!(deletes, 1, "the expired cache is deleted once");
    assert_eq!(creates, 1, "a fresh cache is created once");

    // The row now names the new cache and expires at now + ttl.
    let row = store
        .get(3, MODEL.to_string())
        .await
        .expect("get")
        .expect("row present");
    assert_eq!(row.name, "cachedContents/new");
    assert_eq!(row.expires_at, now + 900);
    assert_eq!(row.token_count, 4096);
}

// AC: M8-AC2 — with no stored row, one create request is sent and the provider_cache
// row is written.
#[tokio::test]
async fn creates_cache_when_none_exists() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/cachedContents"))
        .respond_with(ResponseTemplate::new(200).set_body_json(create_body("cachedContents/fresh")))
        .mount(&server)
        .await;

    let store = Store::open_in_memory().expect("store");
    let now = 2_000_000;

    let name = context_cache::get_or_create(&gemini(&server), &store, &spec(), now)
        .await
        .expect("create");
    assert_eq!(name, "cachedContents/fresh");

    let received = server.received_requests().await.expect("log");
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].method, wiremock::http::Method::POST);
    assert_eq!(received[0].url.path(), "/cachedContents");
    // The create body qualifies the model and carries the TTL + cached content.
    let body: serde_json::Value = serde_json::from_slice(&received[0].body).expect("json body");
    assert_eq!(body["model"], format!("models/{MODEL}"));
    assert_eq!(body["ttl"], "900s");
    assert_eq!(body["contents"][0]["parts"][0]["text"], "analysis sheet");
    assert_eq!(
        body["systemInstruction"]["parts"][0]["text"],
        "you are a translator"
    );

    let row = store
        .get(3, MODEL.to_string())
        .await
        .expect("get")
        .expect("row present");
    assert_eq!(row.name, "cachedContents/fresh");
    assert_eq!(row.expires_at, now + 900);
}

// AC: M8-AC2 — delete at stage end removes the remote cachedContents and the
// provider_cache row.
#[tokio::test]
async fn deletes_cache_at_stage_end() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/cachedContents/x"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let store = Store::open_in_memory().expect("store");
    seed(
        &store,
        ProviderCacheEntry {
            name: "cachedContents/x".to_string(),
            analysis_version: 3,
            model: MODEL.to_string(),
            token_count: 4096,
            expires_at: 9_999_999,
        },
    )
    .await;

    context_cache::delete_cache(&gemini(&server), &store, "cachedContents/x")
        .await
        .expect("delete");

    let received = server.received_requests().await.expect("log");
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].method, wiremock::http::Method::DELETE);
    assert_eq!(received[0].url.path(), "/cachedContents/x");

    let row = store.get(3, MODEL.to_string()).await.expect("get");
    assert!(row.is_none(), "the provider_cache row is removed");
}
