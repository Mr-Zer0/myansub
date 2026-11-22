//! OpenRouter catalog fetch + 24 h disk cache (M13-AC12), offline: the fetch goes
//! through `wiremock` and the on-disk cache lives in a `tempfile` dir.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use myansub::llm::catalog::{CATALOG_TTL, ModelCatalog, OpenRouterCatalog};
use tempfile::tempdir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("openrouter_models.json");
    std::fs::read_to_string(p).expect("read fixture")
}

async fn mount_models(server: &MockServer, body: &str, status: u16) {
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(status).set_body_string(body.to_string()))
        .mount(server)
        .await;
}

// AC: M13-AC12 — the catalog fetches `/models`, writes the cache, and the parsed
// catalog validates a known id.
#[tokio::test]
async fn fetch_writes_cache_and_parses() {
    let server = MockServer::start().await;
    mount_models(&server, &fixture(), 200).await;

    let dir = tempdir().expect("tempdir");
    let cache = dir.path().join("myansub").join("openrouter-models.json");
    let client = reqwest::Client::new();

    let catalog = OpenRouterCatalog::load(&client, &server.uri(), &cache, SystemTime::now())
        .await
        .expect("load");
    assert!(catalog.validate("anthropic/claude-sonnet-5.5").is_ok());
    assert!(cache.exists(), "cache file should be written");
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "one fetch"
    );
}

// AC: M13-AC12 — a fresh cache is used without a fetch.
#[tokio::test]
async fn fresh_cache_avoids_fetch() {
    let server = MockServer::start().await;
    mount_models(&server, &fixture(), 200).await;

    let dir = tempdir().expect("tempdir");
    let cache = dir.path().join("openrouter-models.json");
    std::fs::write(&cache, fixture()).expect("seed cache");
    let client = reqwest::Client::new();

    let catalog = OpenRouterCatalog::load(&client, &server.uri(), &cache, SystemTime::now())
        .await
        .expect("load from fresh cache");
    assert!(catalog.validate("anthropic/claude-sonnet-5.5").is_ok());
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        0,
        "a fresh cache must not fetch"
    );
}

// AC: M13-AC12 — on a fetch failure, a present-but-stale cache is used.
#[tokio::test]
async fn stale_cache_used_on_fetch_failure() {
    let server = MockServer::start().await;
    mount_models(&server, "{}", 500).await; // fetch fails

    let dir = tempdir().expect("tempdir");
    let cache = dir.path().join("openrouter-models.json");
    std::fs::write(&cache, fixture()).expect("seed cache");
    // Make the cache look stale (older than the TTL).
    let stale = SystemTime::now() + CATALOG_TTL + Duration::from_secs(60);
    let client = reqwest::Client::new();

    let catalog = OpenRouterCatalog::load(&client, &server.uri(), &cache, stale)
        .await
        .expect("stale cache is used on fetch failure");
    assert!(catalog.validate("anthropic/claude-sonnet-5.5").is_ok());
}

// AC: M13-AC12 — with no cache and a failed fetch, the load errors clearly.
#[tokio::test]
async fn no_cache_and_offline_errors() {
    let server = MockServer::start().await;
    mount_models(&server, "{}", 500).await;

    let dir = tempdir().expect("tempdir");
    let cache = dir.path().join("openrouter-models.json");
    let client = reqwest::Client::new();

    let err = OpenRouterCatalog::load(&client, &server.uri(), &cache, SystemTime::now())
        .await
        .expect_err("no cache + failed fetch is an error");
    assert!(
        err.to_string().contains("catalog"),
        "error should be clear: {err}"
    );
}
