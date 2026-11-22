//! Repository for the LLM persistence tables: `llm_cache` and `llm_calls`
//! (`docs/DATA_MODEL.md` §2).
//!
//! This module is the **only** place where `store` and `llm` meet
//! (`docs/ARCHITECTURE.md` §3): it provides the store-backed implementations of
//! the narrow [`CacheStore`](crate::llm::cache::CacheStore) and
//! [`UsageSink`](crate::llm::metered::UsageSink) traits that `llm` defines. The
//! coupling points the correct way — `store` depends on the `llm` traits, and
//! `llm` never references `store` — so the dependency rule holds with no cycle.
//!
//! Each persisted change goes through one transaction (`docs/ARCHITECTURE.md` §6
//! rule 1). The `*_in_tx` helpers expose the atomic step so the pipeline (M5/M6)
//! can land a cache write inside the unit's own transaction; the plain functions
//! wrap one transaction for standalone use.

use rusqlite::{Connection, Row, Transaction, params};

use super::db::{Store, StoreError};
use crate::llm::cache::{CacheError, CacheStore, CachedEntry};
use crate::llm::client::{CachePayload, Usage};
use crate::llm::context_cache::{ProviderCacheEntry, ProviderCacheStore, ProviderCacheStoreError};
use crate::llm::metered::{LlmCallRecord, SinkError, UsageSink};

// ---------------------------------------------------------------------------
// llm_cache
// ---------------------------------------------------------------------------

/// A full `llm_cache` row (`docs/DATA_MODEL.md` §2).
#[derive(Debug, Clone, PartialEq)]
pub struct CacheRow {
    pub key: String,
    pub model: String,
    pub prompt_version: u32,
    pub response: String,
    pub usage: Usage,
}

const CACHE_COLUMNS: &str = "key, model, prompt_version, response, usage";

fn cache_row_from_row(row: &Row) -> Result<CacheRow, StoreError> {
    let usage_json: String = row.get(4)?;
    let usage: Usage = serde_json::from_str(&usage_json)
        .map_err(|e| StoreError::Unexpected(format!("llm_cache.usage JSON: {e}")))?;
    let prompt_version: i64 = row.get(2)?;
    Ok(CacheRow {
        key: row.get(0)?,
        model: row.get(1)?,
        prompt_version: prompt_version.max(0) as u32,
        response: row.get(3)?,
        usage,
    })
}

/// Reads one full `llm_cache` row by key, if present.
pub fn get_cache_row(conn: &Connection, key: &str) -> Result<Option<CacheRow>, StoreError> {
    let sql = format!("SELECT {CACHE_COLUMNS} FROM llm_cache WHERE key = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![key])?;
    match rows.next()? {
        Some(row) => Ok(Some(cache_row_from_row(row)?)),
        None => Ok(None),
    }
}

/// Reads a cached response by key as a [`CachedEntry`] (the `CacheStore` view).
pub fn get_cache_entry(conn: &Connection, key: &str) -> Result<Option<CachedEntry>, StoreError> {
    Ok(get_cache_row(conn, key)?.map(|row| CachedEntry {
        response: row.response,
        usage: row.usage,
    }))
}

/// Stores a validated response within the caller's transaction
/// (`docs/ARCHITECTURE.md` §6 rule 1). This is the seam the pipeline (M5/M6) uses
/// to commit a cache write inside the unit's transaction.
///
/// On a key conflict the existing row is kept untouched (`DO NOTHING`): a
/// re-commit of the same key is an identical response, and preserving the original
/// row keeps `created_at` at the first-seen time so `prune_cache` ages entries by
/// first-seen, not last-written.
pub fn put_cache_in_tx(tx: &Transaction, payload: &CachePayload) -> Result<(), StoreError> {
    let usage_json = serde_json::to_string(&payload.usage)
        .map_err(|e| StoreError::Unexpected(format!("serialize Usage: {e}")))?;
    tx.execute(
        "INSERT INTO llm_cache (key, model, prompt_version, response, usage) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(key) DO NOTHING",
        params![
            payload.key,
            payload.model,
            payload.prompt_version,
            payload.response,
            usage_json,
        ],
    )?;
    Ok(())
}

/// Stores a validated response in its own transaction.
pub fn put_cache(conn: &mut Connection, payload: &CachePayload) -> Result<(), StoreError> {
    let tx = conn.transaction()?;
    put_cache_in_tx(&tx, payload)?;
    tx.commit()?;
    Ok(())
}

/// Deletes `llm_cache` entries older than `older_than_days` days
/// (`docs/LLM.md` §4, `docs/CLI.md` `cache prune`). Returns the number removed.
pub fn prune_cache(conn: &mut Connection, older_than_days: u32) -> Result<usize, StoreError> {
    let modifier = format!("-{older_than_days} days");
    let tx = conn.transaction()?;
    let removed = tx.execute(
        "DELETE FROM llm_cache WHERE created_at < datetime('now', ?1)",
        params![modifier],
    )?;
    tx.commit()?;
    Ok(removed)
}

/// Deletes every `llm_cache` entry (`cache prune` with no `--older-than`,
/// `docs/CLI.md` §6). The local response cache is disposable — a pruned entry is
/// simply re-fetched on the next identical request — so pruning spends no tokens and
/// needs no cost confirmation. Returns the number removed.
pub fn prune_all_cache(conn: &mut Connection) -> Result<usize, StoreError> {
    let tx = conn.transaction()?;
    let removed = tx.execute("DELETE FROM llm_cache", [])?;
    tx.commit()?;
    Ok(removed)
}

/// Routes `cache prune [--older-than DAYS]` (`docs/CLI.md` §6): `Some(days)` ages
/// entries by first-seen `created_at` (clamped to `u32`), `None` removes every entry.
/// Returns the number removed. This is the single routing + clamp point the `cache`
/// command drives, so it is unit-tested directly.
pub fn prune(conn: &mut Connection, older_than_days: Option<u64>) -> Result<usize, StoreError> {
    match older_than_days {
        Some(days) => prune_cache(conn, days.min(u64::from(u32::MAX)) as u32),
        None => prune_all_cache(conn),
    }
}

// ---------------------------------------------------------------------------
// llm_calls
// ---------------------------------------------------------------------------

/// Inserts one `llm_calls` row within the caller's transaction, returning its id.
pub fn insert_call_in_tx(tx: &Transaction, call: &LlmCallRecord) -> Result<i64, StoreError> {
    tx.execute(
        "INSERT INTO llm_calls \
         (stage, unit, model, service_tier, prompt_tokens, cached_tokens, output_tokens, \
          thinking_tokens, latency_ms, cache_hit_local, dry_run, outcome) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            call.stage,
            call.unit,
            call.model,
            call.service_tier,
            call.prompt_tokens,
            call.cached_tokens,
            call.output_tokens,
            call.thinking_tokens,
            call.latency_ms,
            call.cache_hit_local as i64,
            call.dry_run as i64,
            call.outcome.as_str(),
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

/// Inserts one `llm_calls` row in its own transaction, returning its id.
pub fn insert_call(conn: &mut Connection, call: &LlmCallRecord) -> Result<i64, StoreError> {
    let tx = conn.transaction()?;
    let id = insert_call_in_tx(&tx, call)?;
    tx.commit()?;
    Ok(id)
}

/// Counts `llm_calls` rows (test/report helper).
pub fn count_calls(conn: &Connection) -> Result<usize, StoreError> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM llm_calls", [], |row| row.get(0))?;
    Ok(count.max(0) as usize)
}

/// Token usage for one `(model, service_tier)` group, summed over `llm_calls`
/// (`docs/DATA_MODEL.md` §2). Used by `status` to report usage and estimate cost
/// (`docs/CLI.md` §6, `docs/LLM.md` §9).
#[derive(Debug, Clone, PartialEq)]
pub struct ModelUsage {
    pub model: String,
    /// The `service_tier` label stored on the calls (`flex`/`standard`), or `None`
    /// when the column was null.
    pub service_tier: Option<String>,
    pub usage: Usage,
}

/// Aggregates `llm_calls` token counts grouped by `(model, service_tier)`.
///
/// Local cache hits and dry-run calls are included with their recorded (zero)
/// token counts, matching how they were metered (`docs/ARCHITECTURE.md` §5). An
/// empty table yields an empty vector, so `status` reports zeroed usage on a fresh
/// project without any special-casing.
pub fn usage_by_model(conn: &Connection) -> Result<Vec<ModelUsage>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT model, service_tier, \
         COALESCE(SUM(prompt_tokens), 0), COALESCE(SUM(cached_tokens), 0), \
         COALESCE(SUM(output_tokens), 0), COALESCE(SUM(thinking_tokens), 0) \
         FROM llm_calls \
         GROUP BY model, service_tier \
         ORDER BY model, service_tier",
    )?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let prompt: i64 = row.get(2)?;
        let cached: i64 = row.get(3)?;
        let output: i64 = row.get(4)?;
        let thinking: i64 = row.get(5)?;
        out.push(ModelUsage {
            model: row.get(0)?,
            service_tier: row.get(1)?,
            usage: Usage {
                prompt_tokens: prompt.max(0) as u64,
                cached_tokens: cached.max(0) as u64,
                output_tokens: output.max(0) as u64,
                thinking_tokens: thinking.max(0) as u64,
                cache_hit_local: false,
                applied_tier: None,
                ..Default::default()
            },
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// provider_cache (Gemini explicit context cache, `docs/DATA_MODEL.md` §2)
// ---------------------------------------------------------------------------

/// Reads the `provider_cache` row for `(analysis_version, model)`, if any
/// (`docs/LLM.md` §5.5). `expires_at` is stored as epoch seconds (ADR-042) in the
/// `TEXT` column and parsed back here.
pub fn get_provider_cache(
    conn: &Connection,
    analysis_version: i64,
    model: &str,
) -> Result<Option<ProviderCacheEntry>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT name, analysis_version, model, token_count, expires_at FROM provider_cache \
         WHERE analysis_version = ?1 AND model = ?2 LIMIT 1",
    )?;
    let mut rows = stmt.query(params![analysis_version, model])?;
    match rows.next()? {
        Some(row) => Ok(Some(provider_cache_entry_from_row(row)?)),
        None => Ok(None),
    }
}

fn provider_cache_entry_from_row(row: &Row) -> Result<ProviderCacheEntry, StoreError> {
    let analysis_version: i64 = row.get(1)?;
    let token_count: i64 = row.get(3)?;
    let expires_at_text: String = row.get(4)?;
    let expires_at: i64 = expires_at_text
        .parse()
        .map_err(|e| StoreError::Unexpected(format!("provider_cache.expires_at: {e}")))?;
    Ok(ProviderCacheEntry {
        name: row.get(0)?,
        analysis_version,
        model: row.get(2)?,
        token_count: token_count.max(0) as u64,
        expires_at,
    })
}

/// Inserts or replaces a `provider_cache` row (keyed by `name`). On a `name`
/// conflict every column is overwritten so a re-created cache updates its row.
pub fn upsert_provider_cache(
    conn: &mut Connection,
    entry: &ProviderCacheEntry,
) -> Result<(), StoreError> {
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO provider_cache (name, analysis_version, model, token_count, expires_at) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(name) DO UPDATE SET \
           analysis_version = excluded.analysis_version, \
           model = excluded.model, \
           token_count = excluded.token_count, \
           expires_at = excluded.expires_at",
        params![
            entry.name,
            entry.analysis_version,
            entry.model,
            entry.token_count as i64,
            entry.expires_at.to_string(),
        ],
    )?;
    tx.commit()?;
    Ok(())
}

/// Deletes the `provider_cache` row named `name`, returning the number removed.
pub fn delete_provider_cache(conn: &mut Connection, name: &str) -> Result<usize, StoreError> {
    let tx = conn.transaction()?;
    let removed = tx.execute("DELETE FROM provider_cache WHERE name = ?1", params![name])?;
    tx.commit()?;
    Ok(removed)
}

// ---------------------------------------------------------------------------
// Store-backed trait implementations (the one llm↔store seam)
// ---------------------------------------------------------------------------

impl CacheStore for Store {
    async fn get(&self, key: &str) -> Result<Option<CachedEntry>, CacheError> {
        let key = key.to_string();
        self.call(move |conn| get_cache_entry(conn, &key))
            .await
            .map_err(|e| CacheError::Backend(e.to_string()))
    }

    async fn put(&self, payload: &CachePayload) -> Result<(), CacheError> {
        let payload = payload.clone();
        self.call(move |conn| put_cache(conn, &payload))
            .await
            .map_err(|e| CacheError::Backend(e.to_string()))
    }
}

impl UsageSink for Store {
    async fn record(&self, call: LlmCallRecord) -> Result<(), SinkError> {
        self.call(move |conn| insert_call(conn, &call).map(|_| ()))
            .await
            .map_err(|e| SinkError::Backend(e.to_string()))
    }
}

impl ProviderCacheStore for Store {
    async fn get(
        &self,
        analysis_version: i64,
        model: String,
    ) -> Result<Option<ProviderCacheEntry>, ProviderCacheStoreError> {
        self.call(move |conn| get_provider_cache(conn, analysis_version, &model))
            .await
            .map_err(|e| ProviderCacheStoreError::Backend(e.to_string()))
    }

    async fn upsert(&self, entry: ProviderCacheEntry) -> Result<(), ProviderCacheStoreError> {
        self.call(move |conn| upsert_provider_cache(conn, &entry))
            .await
            .map_err(|e| ProviderCacheStoreError::Backend(e.to_string()))
    }

    async fn delete(&self, name: String) -> Result<(), ProviderCacheStoreError> {
        self.call(move |conn| delete_provider_cache(conn, &name).map(|_| ()))
            .await
            .map_err(|e| ProviderCacheStoreError::Backend(e.to_string()))
    }
}
