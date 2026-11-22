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
const MIGRATIONS: &[&str] = &[
    MIGRATION_1,
    MIGRATION_2,
    MIGRATION_3,
    MIGRATION_4,
    MIGRATION_5,
];

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

/// Migration 2: output preferences on the `project` row (`docs/DATA_MODEL.md`
/// §2). Both columns are nullable; a `NULL` means "no stored preference", so the
/// export stage falls back to the format/path defaults (`docs/CLI.md` §3). These
/// fill the output-preference gap deferred from M4-B. `ALTER TABLE ADD COLUMN` is
/// append-only and never rewrites existing rows, so this migrates existing
/// projects without data loss.
const MIGRATION_2: &str = r#"
ALTER TABLE project ADD COLUMN output_format TEXT;
ALTER TABLE project ADD COLUMN output_path TEXT;
"#;

/// Migration 3: the approval origin of a review gate (`docs/DATA_MODEL.md` §2,
/// `docs/CLI.md` §4, ADR-019). `approved_origin` is nullable and only meaningful
/// when a review stage's `status` is `approved`: `user` when a human ran the
/// approve command (`analysis approve` / `review approve`), `auto` when
/// `run --auto-approve` cleared the gate. `ALTER TABLE ADD COLUMN` is append-only,
/// so existing projects migrate without data loss (a migrated row reads `NULL`).
const MIGRATION_3: &str = r#"
ALTER TABLE stage_state ADD COLUMN approved_origin TEXT;
"#;

/// Migration 4: the translation digit style recorded on the `project` row
/// (`docs/DATA_MODEL.md` §2, ADR-050). `translation_digits` is nullable (`myanmar`/
/// `western`); a `NULL` means "not yet recorded". The translation stage records the
/// `[burmese].digits` style when it first creates the translation batches, and a
/// later run marks all unlocked translations stale when the current setting differs
/// from the recorded value (`docs/LLM.md` §7). `ALTER TABLE ADD COLUMN` is
/// append-only, so existing projects migrate without data loss (a migrated row reads
/// `NULL`).
const MIGRATION_4: &str = r#"
ALTER TABLE project ADD COLUMN translation_digits TEXT;
"#;

/// Migration 5: provider columns on `llm_calls` (`docs/DATA_MODEL.md` §2, ADR-051/052).
/// `provider` records which provider served the call (`gemini`/`openrouter`);
/// `generation_id`, `served_provider`, and `served_tier` are filled only by the
/// OpenRouter transport (`docs/LLM.md` §10.5) and read `NULL` for Gemini. All four
/// are nullable; `ALTER TABLE ADD COLUMN` is append-only, so existing projects
/// migrate without data loss (migrated rows read `NULL`).
const MIGRATION_5: &str = r#"
ALTER TABLE llm_calls ADD COLUMN provider TEXT;
ALTER TABLE llm_calls ADD COLUMN generation_id TEXT;
ALTER TABLE llm_calls ADD COLUMN served_provider TEXT;
ALTER TABLE llm_calls ADD COLUMN served_tier TEXT;
"#;

#[cfg(test)]
mod tests {
    use super::*;

    // AC: M7-B — migration 2 (`ALTER TABLE ADD COLUMN`) preserves the data of a
    // project created under migration 1: existing project and cue rows and their
    // prior columns survive, the new columns appear as NULL, and user_version → 2.
    #[test]
    fn migration_two_preserves_existing_v1_data() {
        let mut conn = Connection::open_in_memory().expect("open");

        // Bring the database up to version 1 only, then populate it.
        run_migrations_with(&mut conn, &[MIGRATION_1]).expect("apply migration 1");
        let v1: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("version");
        assert_eq!(v1, 1);

        conn.execute(
            "INSERT INTO project \
             (id, source_path, source_hash, source_format, source_encoding, source_lang) \
             VALUES (1, 'movie.en.srt', 'sha256:abc', 'srt', 'utf-8', 'en')",
            [],
        )
        .expect("insert project");
        conn.execute(
            "INSERT INTO cues (idx, start_ms, end_ms, text_raw, text_masked, translatable) \
             VALUES (1, 1000, 3000, 'Hello.', 'Hello.', 1)",
            [],
        )
        .expect("insert cue");

        // Apply through version 2: version 1 is skipped, version 2 is applied. Scoped
        // to the first two scripts so this stays a migration-2 test as later
        // migrations are appended to `MIGRATIONS`.
        run_migrations_with(&mut conn, &[MIGRATION_1, MIGRATION_2]).expect("apply migration 2");
        let v2: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("version");
        assert_eq!(v2, 2);

        // The pre-existing rows and their prior columns are intact.
        let (path, hash, fmt): (String, String, String) = conn
            .query_row(
                "SELECT source_path, source_hash, source_format FROM project WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read project");
        assert_eq!(path, "movie.en.srt");
        assert_eq!(hash, "sha256:abc");
        assert_eq!(fmt, "srt");
        let cue_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM cues", [], |row| row.get(0))
            .expect("count cues");
        assert_eq!(cue_count, 1);

        // The new columns exist and default to NULL for the migrated row.
        let (out_fmt, out_path): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT output_format, output_path FROM project WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read new columns");
        assert_eq!(out_fmt, None);
        assert_eq!(out_path, None);
    }

    // AC: M7-C — migration 3 adds `stage_state.approved_origin` without losing an
    // existing approved stage row: the prior status survives and the new column reads
    // NULL for a row written before the migration, and user_version → 3.
    #[test]
    fn migration_three_adds_approved_origin_column() {
        let mut conn = Connection::open_in_memory().expect("open");

        // Bring the database up to version 2 only, then record an approved stage.
        run_migrations_with(&mut conn, &[MIGRATION_1, MIGRATION_2]).expect("apply 1+2");
        conn.execute(
            "INSERT INTO stage_state (stage, status) VALUES ('analysis_review', 'approved')",
            [],
        )
        .expect("insert stage");

        // Apply through migration 3: versions 1 and 2 skipped, version 3 applied.
        run_migrations_with(&mut conn, &[MIGRATION_1, MIGRATION_2, MIGRATION_3])
            .expect("apply migration 3");
        let v3: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("version");
        assert_eq!(v3, 3);

        // The pre-existing stage row and its status survive; the new column is NULL.
        let (status, origin): (String, Option<String>) = conn
            .query_row(
                "SELECT status, approved_origin FROM stage_state WHERE stage = 'analysis_review'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read stage row");
        assert_eq!(status, "approved");
        assert_eq!(origin, None);
    }

    // AC: M12-AC14 — migration 4 adds `project.translation_digits` without losing an
    // existing project row: the prior columns survive, the new column reads NULL for a
    // row written before the migration, and user_version → 4.
    #[test]
    fn migration_four_adds_translation_digits_column() {
        let mut conn = Connection::open_in_memory().expect("open");

        // Bring the database up to version 3 only, then populate a project row.
        run_migrations_with(&mut conn, &[MIGRATION_1, MIGRATION_2, MIGRATION_3])
            .expect("apply 1+2+3");
        conn.execute(
            "INSERT INTO project \
             (id, source_path, source_hash, source_format, source_encoding, source_lang) \
             VALUES (1, 'movie.en.srt', 'sha256:abc', 'srt', 'utf-8', 'en')",
            [],
        )
        .expect("insert project");

        // Apply through version 4: versions 1..3 skipped, version 4 applied. Scoped to
        // the first four scripts so this stays a migration-4 test as later migrations
        // are appended to `MIGRATIONS`.
        run_migrations_with(
            &mut conn,
            &[MIGRATION_1, MIGRATION_2, MIGRATION_3, MIGRATION_4],
        )
        .expect("apply migration 4");
        let v4: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("version");
        assert_eq!(v4, 4);

        // The pre-existing row survives; the new column is NULL for the migrated row.
        let (path, digits): (String, Option<String>) = conn
            .query_row(
                "SELECT source_path, translation_digits FROM project WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read project row");
        assert_eq!(path, "movie.en.srt");
        assert_eq!(digits, None);
    }

    // AC: M13-AC5 — migration 5 adds the `llm_calls` provider columns without losing
    // an existing row: the prior columns survive, the new columns read NULL for a row
    // written before the migration, and user_version → 5.
    #[test]
    fn migration_five_adds_llm_calls_provider_columns() {
        let mut conn = Connection::open_in_memory().expect("open");

        // Bring the database up to version 4 only, then record an llm_calls row.
        run_migrations_with(
            &mut conn,
            &[MIGRATION_1, MIGRATION_2, MIGRATION_3, MIGRATION_4],
        )
        .expect("apply 1..4");
        conn.execute(
            "INSERT INTO llm_calls (stage, model, service_tier, outcome) \
             VALUES ('translation', 'gemini-flash-latest', 'flex', 'ok')",
            [],
        )
        .expect("insert llm_call");

        // Apply the full set: versions 1..4 skipped, version 5 applied.
        run_migrations_with(&mut conn, MIGRATIONS).expect("apply migration 5");
        let v5: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("version");
        assert_eq!(v5, 5);
        assert_eq!(SCHEMA_VERSION, 5);

        // The pre-existing row survives; the four new columns are NULL.
        let (model, provider, generation_id, served_provider, served_tier): (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT model, provider, generation_id, served_provider, served_tier \
                 FROM llm_calls LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .expect("read llm_calls row");
        assert_eq!(model, "gemini-flash-latest");
        assert_eq!(provider, None);
        assert_eq!(generation_id, None);
        assert_eq!(served_provider, None);
        assert_eq!(served_tier, None);
    }
}
