//! M3-C store tests for the LLM persistence tables (`docs/DATA_MODEL.md` §2):
//! `llm_cache` put/get round-trips and pruning, and `llm_calls` inserts
//! (including the `dry_run` and `cache_hit_local` flags). Follows
//! `docs/TESTING.md`: in-memory and `tempfile` stores, no network.

use myansub::llm::client::{CachePayload, Usage};
use myansub::llm::metered::{CallOutcome, LlmCallRecord};
use myansub::store::Store;
use myansub::store::repo_llm;

fn payload(key: &str) -> CachePayload {
    CachePayload {
        key: key.to_string(),
        model: "gemini-flash-latest".to_string(),
        prompt_version: 1,
        response: r#"{"lines":[{"id":1,"t":"မင်္ဂလာပါ"}]}"#.to_string(),
        usage: Usage {
            prompt_tokens: 120,
            cached_tokens: 30,
            output_tokens: 44,
            thinking_tokens: 2,
            ..Usage::default()
        },
    }
}

fn record() -> LlmCallRecord {
    LlmCallRecord {
        stage: "translation".to_string(),
        unit: Some(3),
        model: "gemini-flash-latest".to_string(),
        service_tier: Some("flex".to_string()),
        prompt_tokens: Some(120),
        cached_tokens: Some(30),
        output_tokens: Some(44),
        thinking_tokens: Some(2),
        latency_ms: Some(812),
        cache_hit_local: false,
        dry_run: false,
        outcome: CallOutcome::Ok,
    }
}

#[tokio::test]
async fn llm_cache_put_get_round_trips() {
    let store = Store::open_in_memory().expect("open");
    let p = payload("k1");
    let expected = p.clone();

    store
        .call(move |conn| repo_llm::put_cache(conn, &p))
        .await
        .expect("put");

    let row = store
        .call(|conn| repo_llm::get_cache_row(conn, "k1"))
        .await
        .expect("get")
        .expect("present");

    assert_eq!(row.key, "k1");
    assert_eq!(row.model, expected.model);
    assert_eq!(row.prompt_version, 1);
    assert_eq!(row.response, expected.response);
    assert_eq!(row.usage, expected.usage);
}

// AC: M8-AC2 (store seam) — provider_cache get-by-(analysis_version, model), upsert,
// and delete round-trip, and a name-conflict upsert replaces every column.
#[tokio::test]
async fn provider_cache_upsert_get_delete_round_trips() {
    use myansub::llm::context_cache::ProviderCacheEntry;

    let store = Store::open_in_memory().expect("open");

    // No row for an unknown (version, model).
    let missing = store
        .call(|conn| repo_llm::get_provider_cache(conn, 3, "gemini-flash-latest"))
        .await
        .expect("get");
    assert!(missing.is_none());

    let entry = ProviderCacheEntry {
        name: "cachedContents/abc".to_string(),
        analysis_version: 3,
        model: "gemini-flash-latest".to_string(),
        token_count: 4096,
        expires_at: 1_700_000_000,
    };
    let to_store = entry.clone();
    store
        .call(move |conn| repo_llm::upsert_provider_cache(conn, &to_store))
        .await
        .expect("upsert");

    let read = store
        .call(|conn| repo_llm::get_provider_cache(conn, 3, "gemini-flash-latest"))
        .await
        .expect("get")
        .expect("present");
    assert_eq!(read, entry);

    // Re-upserting the same name replaces token_count and expires_at.
    let updated = ProviderCacheEntry {
        token_count: 8192,
        expires_at: 1_700_009_999,
        ..entry.clone()
    };
    let to_store = updated.clone();
    store
        .call(move |conn| repo_llm::upsert_provider_cache(conn, &to_store))
        .await
        .expect("upsert replace");
    let read = store
        .call(|conn| repo_llm::get_provider_cache(conn, 3, "gemini-flash-latest"))
        .await
        .expect("get")
        .expect("present");
    assert_eq!(read, updated);

    // Delete removes exactly one row.
    let removed = store
        .call(|conn| repo_llm::delete_provider_cache(conn, "cachedContents/abc"))
        .await
        .expect("delete");
    assert_eq!(removed, 1);
    let gone = store
        .call(|conn| repo_llm::get_provider_cache(conn, 3, "gemini-flash-latest"))
        .await
        .expect("get");
    assert!(gone.is_none());
}

#[tokio::test]
async fn llm_cache_get_missing_key_is_none() {
    let store = Store::open_in_memory().expect("open");
    let row = store
        .call(|conn| repo_llm::get_cache_row(conn, "absent"))
        .await
        .expect("get");
    assert!(row.is_none());
}

#[tokio::test]
async fn prune_removes_only_old_entries() {
    let store = Store::open_in_memory().expect("open");

    // A fresh entry via the normal path.
    let fresh = payload("fresh");
    store
        .call(move |conn| repo_llm::put_cache(conn, &fresh))
        .await
        .expect("put fresh");

    // A back-dated entry inserted directly so it predates the prune window.
    store
        .call(|conn| {
            conn.execute(
                "INSERT INTO llm_cache (key, model, prompt_version, response, usage, created_at) \
                 VALUES ('old', 'm', 1, 'r', '{}', datetime('now', '-100 days'))",
                [],
            )
            .map_err(myansub::store::StoreError::from)?;
            Ok(())
        })
        .await
        .expect("insert old");

    let removed = store
        .call(|conn| repo_llm::prune_cache(conn, 30))
        .await
        .expect("prune");
    assert_eq!(removed, 1);

    assert!(
        store
            .call(|conn| repo_llm::get_cache_row(conn, "old"))
            .await
            .expect("get old")
            .is_none()
    );
    assert!(
        store
            .call(|conn| repo_llm::get_cache_row(conn, "fresh"))
            .await
            .expect("get fresh")
            .is_some()
    );
}

#[tokio::test]
async fn recommit_preserves_created_at_so_prune_ages_by_first_seen() {
    let store = Store::open_in_memory().expect("open");

    // An original entry first seen 100 days ago.
    store
        .call(|conn| {
            conn.execute(
                "INSERT INTO llm_cache (key, model, prompt_version, response, usage, created_at) \
                 VALUES ('k', 'm', 1, 'r', '{}', datetime('now', '-100 days'))",
                [],
            )
            .map_err(myansub::store::StoreError::from)?;
            Ok(())
        })
        .await
        .expect("insert original");

    let read_created_at = |store: Store| async move {
        store
            .call(|conn| {
                conn.query_row(
                    "SELECT created_at FROM llm_cache WHERE key = 'k'",
                    [],
                    |r| r.get::<_, String>(0),
                )
                .map_err(myansub::store::StoreError::from)
            })
            .await
            .expect("read created_at")
    };

    let before = read_created_at(store.clone()).await;

    // Re-committing the same key (an identical response) must not reset its age.
    let p = payload("k");
    store
        .call(move |conn| repo_llm::put_cache(conn, &p))
        .await
        .expect("recommit");

    let after = read_created_at(store.clone()).await;
    assert_eq!(
        before, after,
        "re-commit must preserve created_at (first-seen age)"
    );

    // Because age is by first-seen, the entry is still prunable at 30 days.
    let removed = store
        .call(|conn| repo_llm::prune_cache(conn, 30))
        .await
        .expect("prune");
    assert_eq!(removed, 1);
}

#[tokio::test]
async fn llm_calls_insert_round_trips() {
    let store = Store::open_in_memory().expect("open");
    let rec = record();

    let id = store
        .call(move |conn| repo_llm::insert_call(conn, &rec))
        .await
        .expect("insert");
    assert!(id >= 1);

    let count = store
        .call(|conn| repo_llm::count_calls(conn))
        .await
        .expect("count");
    assert_eq!(count, 1);

    let (stage, unit, outcome, hit, dry): (String, Option<i64>, String, i64, i64) = store
        .call(move |conn| {
            conn.query_row(
                "SELECT stage, unit, outcome, cache_hit_local, dry_run FROM llm_calls WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .map_err(myansub::store::StoreError::from)
        })
        .await
        .expect("read back");

    assert_eq!(stage, "translation");
    assert_eq!(unit, Some(3));
    assert_eq!(outcome, "ok");
    assert_eq!(hit, 0);
    assert_eq!(dry, 0);
}

#[tokio::test]
async fn llm_calls_insert_preserves_flags() {
    let store = Store::open_in_memory().expect("open");
    let rec = LlmCallRecord {
        cache_hit_local: true,
        dry_run: true,
        outcome: CallOutcome::Cancelled,
        ..record()
    };

    let id = store
        .call(move |conn| repo_llm::insert_call(conn, &rec))
        .await
        .expect("insert");

    let (hit, dry, outcome): (i64, i64, String) = store
        .call(move |conn| {
            conn.query_row(
                "SELECT cache_hit_local, dry_run, outcome FROM llm_calls WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(myansub::store::StoreError::from)
        })
        .await
        .expect("read back");

    assert_eq!(hit, 1);
    assert_eq!(dry, 1);
    assert_eq!(outcome, "cancelled");
}
