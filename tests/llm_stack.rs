//! M3-C full-stack tests: `Metered<Cached<Retrying<Transport>>>`
//! (`docs/ARCHITECTURE.md` §5). Proves a miss flows through the retrying transport
//! and records one `llm_calls` row, while a subsequent identical *committed*
//! request is a local cache hit that short-circuits the transport yet still
//! records one row (`docs/LLM.md` §4/§5). Uses the scripted `Fake` transport and a
//! `tempfile`-backed `Store`; no network (`docs/TESTING.md`).

use myansub::llm::cache::CacheStore;
use myansub::llm::client::{
    CommitToken, ContentBlock, GenerationConfig, LlmError, LlmResponse, ServiceTier,
    ThinkingSetting, Usage,
};
use myansub::llm::fake::Fake;
use myansub::llm::{CallContext, LlmClient, LlmRequest, RetryConfig, assemble_stack};
use myansub::store::{Store, repo_llm};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

fn retry_cfg() -> RetryConfig {
    RetryConfig {
        max_retries: 6,
        backoff_base_ms: 0,
        backoff_max_ms: 0,
        max_sheds_before_fallback: 3,
        service_tier_fallback: true,
    }
}

fn request(tier: ServiceTier) -> LlmRequest {
    LlmRequest {
        model: "gemini-flash-latest".to_string(),
        service_tier: tier,
        system_instruction: "sys".to_string(),
        contents: vec![ContentBlock::user("sheet"), ContentBlock::user("lines")],
        generation_config: GenerationConfig {
            max_output_tokens: 128,
            response_mime_type: "application/json".to_string(),
            response_schema: None,
            thinking: ThinkingSetting::Off,
        },
    }
}

fn response() -> LlmResponse {
    LlmResponse {
        text: r#"{"lines":[{"id":1,"t":"ok"}]}"#.to_string(),
        usage: Usage {
            prompt_tokens: 100,
            output_tokens: 20,
            ..Usage::default()
        },
        commit: CommitToken::noop(),
    }
}

// AC: M3-AC3, M3-AC4 — end to end over a store-backed stack: a miss reaches the
// transport and records one row; a committed identical request is a hit that
// short-circuits the transport and still records one row (cache_hit_local = 1).
#[tokio::test]
async fn committed_request_hits_and_short_circuits_the_full_stack() {
    let dir = tempdir().expect("tempdir");
    let store = Store::open(&dir.path().join("project.db")).expect("open");

    let fake = Fake::new().push_response(response());
    let stack = assemble_stack(
        fake.clone(),
        retry_cfg(),
        CancellationToken::new(),
        1,
        store.clone(),
        store.clone(),
        CallContext::stage("translation").with_unit(1),
    );

    // First request: a miss flows through Retrying to the transport.
    let first = stack
        .generate(request(ServiceTier::Flex))
        .await
        .expect("miss");
    assert_eq!(fake.call_count(), 1);
    assert!(!first.usage.cache_hit_local);

    // The pipeline validates, then commits the payload the miss carried. The
    // realistic path writes it through the store seam (M5/M6 commits in-tx).
    let payload = first.commit.payload().expect("miss carries payload");
    CacheStore::put(&store, payload).await.expect("commit");

    // Second identical request (different tier, excluded from the key) is a hit:
    // the transport is not called again, but a second llm_calls row is written.
    let second = stack
        .generate(request(ServiceTier::Standard))
        .await
        .expect("hit");
    assert_eq!(fake.call_count(), 1, "a hit must not reach the transport");
    assert!(second.usage.cache_hit_local);

    // Two logical calls → two llm_calls rows; the second is flagged a hit.
    let rows = store
        .call(|conn| {
            let total = repo_llm::count_calls(conn)?;
            let hits: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM llm_calls WHERE cache_hit_local = 1",
                    [],
                    |row| row.get(0),
                )
                .map_err(myansub::store::StoreError::from)?;
            Ok((total, hits))
        })
        .await
        .expect("counts");
    assert_eq!(rows.0, 2, "one row per logical call, incl. the hit");
    assert_eq!(rows.1, 1, "exactly the hit is flagged cache_hit_local");
}

// AC: M3-AC4 — transport retries happen inside Retrying and are invisible to
// Metered: a fail-then-succeed call retries the transport (call_count > 1) yet
// writes EXACTLY ONE llm_calls row (one row per logical call, not per attempt).
#[tokio::test]
async fn transport_retry_records_a_single_llm_call_row() {
    let store = Store::open_in_memory().expect("open");
    let fake = Fake::new()
        .push_result(Err(LlmError::Server { status: 503 }))
        .push_response(response());
    let stack = assemble_stack(
        fake.clone(),
        retry_cfg(), // backoff 0ms: retries are instant
        CancellationToken::new(),
        1,
        store.clone(),
        store.clone(),
        CallContext::stage("translation").with_unit(1),
    );

    let resp = stack
        .generate(request(ServiceTier::Flex))
        .await
        .expect("succeeds after retry");
    assert!(!resp.usage.cache_hit_local);
    assert!(
        fake.call_count() > 1,
        "the transport should have been retried, got {}",
        fake.call_count()
    );

    let total = store
        .call(|conn| repo_llm::count_calls(conn))
        .await
        .expect("count");
    assert_eq!(total, 1, "one logical call → exactly one llm_calls row");
}

// AC: M3-AC3 — an uncommitted (invalid) response is never cached: the same
// request is a miss again and reaches the transport twice.
#[tokio::test]
async fn uncommitted_response_is_not_cached_in_full_stack() {
    let store = Store::open_in_memory().expect("open");
    let fake = Fake::new()
        .push_response(response())
        .push_response(response());
    let stack = assemble_stack(
        fake.clone(),
        retry_cfg(),
        CancellationToken::new(),
        1,
        store.clone(),
        store.clone(),
        CallContext::stage("translation"),
    );

    let _ = stack
        .generate(request(ServiceTier::Flex))
        .await
        .expect("miss");
    // Do not commit — simulate a validation failure.
    let _ = stack
        .generate(request(ServiceTier::Flex))
        .await
        .expect("miss");

    assert_eq!(
        fake.call_count(),
        2,
        "an uncommitted response is not cached"
    );
    let total = store
        .call(|conn| repo_llm::count_calls(conn))
        .await
        .expect("count");
    assert_eq!(total, 2);
}
