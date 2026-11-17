//! Versioned schema migrations keyed by `PRAGMA user_version`.
//!
//! Each migration is one SQL script applied inside a single transaction; the
//! matching `user_version` is bumped in the same transaction so a crash either
//! leaves the schema fully migrated or fully rolled back (`docs/ARCHITECTURE.md`
//! §6). Running [`run_migrations`] when the database is already current is a
//! no-op, so migrations are idempotent (M2-AC1).

use rusqlite::Connection;

use super::db::StoreError;

/// Migration scripts in order. Index `i` (0-based) upgrades the database to
/// `user_version = i + 1`. Append new scripts here; never edit a released one.
const MIGRATIONS: &[&str] = &[MIGRATION_1];

/// The current schema version (highest migration applied by a fresh open).
pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

/// Applies every migration whose version is greater than the stored
/// `user_version`, each in its own transaction.
///
/// Reads the current `user_version`; if it already equals (or exceeds) the number
/// of known migrations, returns without touching the database.
pub fn run_migrations(conn: &mut Connection) -> Result<(), StoreError> {
    run_migrations_with(conn, MIGRATIONS)
}

/// Applies `migrations` the same way [`run_migrations`] does, but against a
/// caller-supplied script list. This is the injection seam (`docs/TESTING.md`
/// §4): a test can supply a deliberately failing script to prove the migration
/// transaction rolls back (crash-safety rule 3) without touching the real schema.
///
/// Script at index `i` (0-based) upgrades the database to `user_version = i + 1`.
pub fn run_migrations_with(conn: &mut Connection, migrations: &[&str]) -> Result<(), StoreError> {
    let current = current_version(conn)?;
    for (index, script) in migrations.iter().enumerate() {
        let version = index as u32 + 1;
        if version <= current {
            continue;
        }
        apply_migration(conn, version, script)?;
    }
    Ok(())
}

/// Reads `PRAGMA user_version`.
fn current_version(conn: &Connection) -> Result<u32, StoreError> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    Ok(version.max(0) as u32)
}

/// Applies one migration script and bumps `user_version` atomically.
fn apply_migration(conn: &mut Connection, version: u32, script: &str) -> Result<(), StoreError> {
    let tx = conn.transaction()?;
    tx.execute_batch(script)
        .map_err(|err| StoreError::Migration {
            version,
            message: err.to_string(),
        })?;
    tx.pragma_update(None, "user_version", version)?;
    tx.commit()?;
    Ok(())
}

/// Migration 1: the full schema from `docs/DATA_MODEL.md` §2.
const MIGRATION_1: &str = r#"
CREATE TABLE project (
  id                  INTEGER PRIMARY KEY CHECK (id = 1),
  source_path         TEXT NOT NULL,
  source_hash         TEXT NOT NULL,
  source_format       TEXT NOT NULL,
  source_encoding     TEXT NOT NULL,
  encoding_confidence REAL,
  source_lang         TEXT NOT NULL,
  target_lang         TEXT NOT NULL DEFAULT 'my',
  format_meta         TEXT,
  created_at          TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE stage_state (
  stage      TEXT PRIMARY KEY,
  status     TEXT NOT NULL,
  updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE cues (
  id           INTEGER PRIMARY KEY,
  idx          INTEGER NOT NULL UNIQUE,
  start_ms     INTEGER NOT NULL,
  end_ms       INTEGER NOT NULL,
  text_raw     TEXT NOT NULL,
  text_masked  TEXT NOT NULL,
  lead_tags    TEXT,
  inline_tags  TEXT NOT NULL DEFAULT '[]',
  style        TEXT,
  speaker      TEXT,
  translatable INTEGER NOT NULL
);

CREATE TABLE analysis_chunks (
  chunk_no   INTEGER PRIMARY KEY,
  first_cue  INTEGER NOT NULL,
  last_cue   INTEGER NOT NULL,
  status     TEXT NOT NULL,
  attempts   INTEGER NOT NULL DEFAULT 0,
  last_error TEXT,
  result     TEXT
);

CREATE TABLE analysis (
  version       INTEGER PRIMARY KEY,
  doc           TEXT NOT NULL,
  origin        TEXT NOT NULL CHECK (origin IN ('llm','user')),
  source_digest TEXT NOT NULL,
  signature     TEXT NOT NULL,
  approved      INTEGER NOT NULL DEFAULT 0,
  created_at    TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE translation_batches (
  batch_no         INTEGER PRIMARY KEY,
  first_cue        INTEGER NOT NULL,
  last_cue         INTEGER NOT NULL,
  status           TEXT NOT NULL,
  attempts         INTEGER NOT NULL DEFAULT 0,
  last_error       TEXT,
  input_hash       TEXT,
  model            TEXT,
  analysis_version INTEGER,
  updated_at       TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE translations (
  cue_id           INTEGER PRIMARY KEY REFERENCES cues(id),
  text             TEXT NOT NULL,
  origin           TEXT NOT NULL CHECK (origin IN ('llm','user')),
  locked           INTEGER NOT NULL DEFAULT 0,
  stale            INTEGER NOT NULL DEFAULT 0,
  flags            TEXT NOT NULL DEFAULT '[]',
  analysis_version INTEGER NOT NULL,
  model            TEXT,
  updated_at       TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE translation_history (
  id     INTEGER PRIMARY KEY,
  cue_id INTEGER NOT NULL REFERENCES cues(id),
  text   TEXT NOT NULL,
  origin TEXT NOT NULL,
  at     TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE llm_cache (
  key            TEXT PRIMARY KEY,
  model          TEXT NOT NULL,
  prompt_version INTEGER NOT NULL,
  response       TEXT NOT NULL,
  usage          TEXT NOT NULL,
  created_at     TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE llm_calls (
  id              INTEGER PRIMARY KEY,
  stage           TEXT NOT NULL,
  unit            INTEGER,
  model           TEXT NOT NULL,
  service_tier    TEXT,
  prompt_tokens   INTEGER,
  cached_tokens   INTEGER,
  output_tokens   INTEGER,
  thinking_tokens INTEGER,
  latency_ms      INTEGER,
  cache_hit_local INTEGER NOT NULL DEFAULT 0,
  dry_run         INTEGER NOT NULL DEFAULT 0,
  outcome         TEXT NOT NULL,
  at              TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE provider_cache (
  name             TEXT PRIMARY KEY,
  analysis_version INTEGER NOT NULL,
  model            TEXT NOT NULL,
  token_count      INTEGER NOT NULL,
  expires_at       TEXT NOT NULL
);
"#;
