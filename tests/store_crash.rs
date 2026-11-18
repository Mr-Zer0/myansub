//! Store crash-safety tests (`docs/TESTING.md` §1/§4): migrations and the atomic
//! file write. All tests use `tempfile` directories and never touch the network.

use std::fs;

use myansub::store::migrations::{SCHEMA_VERSION, run_migrations, run_migrations_with};
use myansub::store::{StagedWrite, Store, StoreError, atomic_write};
use tempfile::tempdir;

/// Every table defined by migration 1 (`docs/DATA_MODEL.md` §2).
const EXPECTED_TABLES: &[&str] = &[
    "project",
    "stage_state",
    "cues",
    "analysis_chunks",
    "analysis",
    "translation_batches",
    "translations",
    "translation_history",
    "llm_cache",
    "llm_calls",
    "provider_cache",
];

async fn user_version(store: &Store) -> i64 {
    store
        .call(|conn| {
            conn.pragma_query_value(None, "user_version", |row| row.get(0))
                .map_err(Into::into)
        })
        .await
        .expect("read user_version")
}

async fn table_names(store: &Store) -> Vec<String> {
    store
        .call(|conn| {
            let mut stmt = conn.prepare(
                "SELECT name FROM sqlite_master \
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )?;
            let names = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(names)
        })
        .await
        .expect("list tables")
}

// AC: M2-AC1 — a fresh database is migrated to the current version and every
// table from migration 1 exists.
#[tokio::test]
async fn fresh_open_sets_user_version_and_creates_all_tables() {
    let dir = tempdir().expect("tempdir");
    let store = Store::open(&dir.path().join("project.db")).expect("open");

    assert_eq!(user_version(&store).await, SCHEMA_VERSION as i64);
    // Migration 3 (M7-C) added `stage_state.approved_origin`.
    assert_eq!(SCHEMA_VERSION, 3);

    let tables = table_names(&store).await;
    for expected in EXPECTED_TABLES {
        assert!(
            tables.iter().any(|t| t == expected),
            "missing table {expected}; found {tables:?}"
        );
    }
}

// AC: M2-AC1 — re-running migrations on an already-current database is a no-op:
// no error, no duplicate-table failure, and the version is unchanged.
#[tokio::test]
async fn migrations_are_idempotent() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("project.db");
    let store = Store::open(&path).expect("open");

    // Run the migration routine again on the already-migrated connection.
    store
        .call(run_migrations)
        .await
        .expect("second migration run is a no-op");
    assert_eq!(user_version(&store).await, SCHEMA_VERSION as i64);

    // Reopening the same file also re-runs migrations; still idempotent.
    drop(store);
    let reopened = Store::open(&path).expect("reopen");
    assert_eq!(user_version(&reopened).await, SCHEMA_VERSION as i64);
    assert_eq!(table_names(&reopened).await.len(), EXPECTED_TABLES.len());
}

// A basic round-trip through the async bridge: a value returned from a closure
// and a StoreError propagated from a closure.
#[tokio::test]
async fn call_returns_value_and_propagates_error() {
    let store = Store::open_in_memory().expect("open in-memory");

    let seven = store
        .call(|conn| {
            conn.query_row("SELECT 7", [], |row| row.get::<_, i64>(0))
                .map_err(Into::into)
        })
        .await
        .expect("round-trip value");
    assert_eq!(seven, 7);

    let err = store
        .call(|conn| {
            conn.query_row("SELECT * FROM does_not_exist", [], |_| Ok(()))
                .map_err(Into::into)
        })
        .await
        .expect_err("error propagates as StoreError");
    assert!(matches!(err, myansub::store::StoreError::Sqlite(_)));
}

// AC: M2-AC4 — a successful atomic write leaves the target with the full contents.
#[test]
fn atomic_write_writes_full_contents() {
    let dir = tempdir().expect("tempdir");
    let target = dir.path().join("out.srt");

    atomic_write(&target, b"hello world").expect("atomic write");

    assert_eq!(fs::read(&target).expect("read target"), b"hello world");
}

// AC: M2-AC4 — a crash between tmp-write and rename (staged but not committed)
// leaves no target file at all; the partial content lives only in the .tmp.
#[test]
fn staged_write_without_commit_leaves_no_target() {
    let dir = tempdir().expect("tempdir");
    let target = dir.path().join("out.srt");

    let staged = StagedWrite::create(&target, b"partial, never committed").expect("stage");
    let tmp = staged.tmp_path().to_path_buf();
    drop(staged); // simulate crash before rename

    assert!(
        !target.exists(),
        "target must not exist after a crash before rename"
    );
    assert_ne!(tmp, target, "the leftover tmp must not be the target path");
}

// AC: M2-AC4 — a crash before rename never corrupts an existing target: its
// previous contents remain fully intact (never partial/truncated).
#[test]
fn staged_write_without_commit_leaves_existing_target_unchanged() {
    let dir = tempdir().expect("tempdir");
    let target = dir.path().join("out.srt");
    atomic_write(&target, b"original contents").expect("initial write");

    let staged = StagedWrite::create(&target, b"new, uncommitted").expect("stage");
    drop(staged); // simulate crash before rename

    assert_eq!(
        fs::read(&target).expect("read target"),
        b"original contents",
        "existing target must be unchanged after a crash before rename"
    );
}

// #5 — a target with no final file-name component (ends in `..`) is rejected
// instead of silently writing a `.tmp` dotfile into the parent directory.
#[test]
fn staged_write_rejects_path_without_file_name() {
    let dir = tempdir().expect("tempdir");
    let no_name = dir.path().join("..");

    let result = StagedWrite::create(&no_name, b"data");
    assert!(
        matches!(result, Err(StoreError::InvalidPath { .. })),
        "path without a file name must be rejected"
    );

    // No stray `.tmp` dotfile was created in the parent.
    let stray = dir.path().join(".tmp");
    assert!(!stray.exists(), "no .tmp dotfile must be created");
}

// AC: M2-AC1 — the §6 crash-safety pragmas are actually in effect on an opened
// file-backed Store.
#[tokio::test]
async fn open_applies_crash_safety_pragmas() {
    let dir = tempdir().expect("tempdir");
    let store = Store::open(&dir.path().join("project.db")).expect("open");

    let (foreign_keys, journal_mode, synchronous, busy_timeout) = store
        .call(|conn| {
            let fk: i64 = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
            let jm: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
            let sync: i64 = conn.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
            let bt: i64 = conn.query_row("PRAGMA busy_timeout", [], |r| r.get(0))?;
            Ok((fk, jm, sync, bt))
        })
        .await
        .expect("read pragmas");

    assert_eq!(foreign_keys, 1, "foreign_keys must be ON");
    assert_eq!(journal_mode.to_lowercase(), "wal", "file DB must be WAL");
    assert_eq!(synchronous, 2, "synchronous must be FULL (2)");
    assert_eq!(busy_timeout, 5000, "busy_timeout must be 5000ms");
}

// AC: M2-AC1 — foreign-key enforcement is live: a translation or history row
// referencing a non-existent cue is rejected.
#[tokio::test]
async fn foreign_keys_are_enforced() {
    let store = Store::open_in_memory().expect("open in-memory");

    let translations_err = store
        .call(|conn| {
            conn.execute(
                "INSERT INTO translations (cue_id, text, origin, analysis_version) \
                 VALUES (999, 'x', 'llm', 1)",
                [],
            )
            .map_err(Into::into)
        })
        .await
        .expect_err("FK violation on translations");
    assert!(matches!(translations_err, StoreError::Sqlite(_)));

    let history_err = store
        .call(|conn| {
            conn.execute(
                "INSERT INTO translation_history (cue_id, text, origin) VALUES (999, 'x', 'llm')",
                [],
            )
            .map_err(Into::into)
        })
        .await
        .expect_err("FK violation on translation_history");
    assert!(matches!(history_err, StoreError::Sqlite(_)));
}

// #6 — load-bearing CHECK constraints reject bad values.
#[tokio::test]
async fn check_constraints_are_enforced() {
    let store = Store::open_in_memory().expect("open in-memory");

    // project.id must be 1.
    let project_err = store
        .call(|conn| {
            conn.execute(
                "INSERT INTO project (id, source_path, source_hash, source_format, \
                 source_encoding, source_lang) VALUES (2, 'p', 'h', 'srt', 'utf-8', 'en')",
                [],
            )
            .map_err(Into::into)
        })
        .await
        .expect_err("project id CHECK");
    assert!(matches!(project_err, StoreError::Sqlite(_)));

    // analysis.origin must be in ('llm','user').
    let analysis_err = store
        .call(|conn| {
            conn.execute(
                "INSERT INTO analysis (version, doc, origin, source_digest, signature) \
                 VALUES (1, '{}', 'bogus', 'd', 's')",
                [],
            )
            .map_err(Into::into)
        })
        .await
        .expect_err("analysis origin CHECK");
    assert!(matches!(analysis_err, StoreError::Sqlite(_)));

    // translations.origin must be in ('llm','user') — insert a valid cue first so
    // the FK passes and the CHECK is what rejects the row.
    let translations_err = store
        .call(|conn| {
            conn.execute(
                "INSERT INTO cues (id, idx, start_ms, end_ms, text_raw, text_masked, translatable) \
                 VALUES (1, 1, 0, 1000, 'hi', 'hi', 1)",
                [],
            )?;
            conn.execute(
                "INSERT INTO translations (cue_id, text, origin, analysis_version) \
                 VALUES (1, 'x', 'bogus', 1)",
                [],
            )
            .map_err(Into::into)
        })
        .await
        .expect_err("translations origin CHECK");
    assert!(matches!(translations_err, StoreError::Sqlite(_)));
}

// #4 — a panic inside a call() closure poisons the lock; the next call() returns
// StoreError::Poisoned rather than panicking.
#[tokio::test]
async fn panic_in_closure_poisons_lock_for_next_call() {
    let store = Store::open_in_memory().expect("open in-memory");

    let first = store
        .call(|_conn| -> Result<(), StoreError> { panic!("boom inside closure") })
        .await;
    assert!(
        first.is_err(),
        "the panicking call itself must report an error"
    );

    let next = store
        .call(|conn| {
            conn.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))
                .map_err(Into::into)
        })
        .await
        .expect_err("subsequent call must observe the poisoned lock");
    assert!(matches!(next, StoreError::Poisoned));
}

// #2 — a failing migration rolls back entirely (crash-safety rule 3): the
// user_version is unchanged, no partial objects remain, and a later valid
// migration to the same version succeeds.
#[tokio::test]
async fn failing_migration_rolls_back_and_leaves_schema_consistent() {
    let store = Store::open_in_memory().expect("open in-memory");
    // Fresh store is at the current version (the real migrations 1..3).
    assert_eq!(user_version(&store).await, SCHEMA_VERSION as i64);
    let current = SCHEMA_VERSION as i64;

    // A migration set whose next (version `current + 1`) script creates one object
    // then fails; the already-applied scripts are skipped.
    let err = store
        .call(|conn| {
            run_migrations_with(
                conn,
                &[
                    "-- v1 already applied; skipped",
                    "-- v2 already applied; skipped",
                    "-- v3 already applied; skipped",
                    "CREATE TABLE migration_next_partial (x INTEGER); \
                     CREATE TABLE this_is_not_valid_sql (;",
                ],
            )
        })
        .await
        .expect_err("invalid migration script must error");
    assert!(
        matches!(err, StoreError::Migration { version, .. } if version == (current + 1) as u32)
    );

    // Rollback: version unchanged, and the partial table does not exist.
    assert_eq!(user_version(&store).await, current);
    assert!(
        !object_exists(&store, "migration_next_partial").await,
        "a rolled-back migration must leave no partial objects"
    );

    // A subsequent valid migration to the next version succeeds.
    store
        .call(|conn| {
            run_migrations_with(
                conn,
                &[
                    "-- v1 already applied; skipped",
                    "-- v2 already applied; skipped",
                    "-- v3 already applied; skipped",
                    "CREATE TABLE migration_next (x INTEGER);",
                ],
            )
        })
        .await
        .expect("valid migration applies");
    assert_eq!(user_version(&store).await, current + 1);
    assert!(object_exists(&store, "migration_next").await);
}

// AC: M7-B / docs/DATA_MODEL.md §2 — migration 2 adds the output-preference
// columns; they round-trip through the repository and start NULL on a project
// created before any preference was stored.
#[tokio::test]
async fn migration_two_output_prefs_round_trip() {
    use myansub::store::repo_project::{
        Project, get_output_prefs, insert_project, set_output_prefs,
    };

    let store = Store::open_in_memory().expect("open in-memory");
    assert_eq!(user_version(&store).await, SCHEMA_VERSION as i64);

    let project = Project {
        source_path: "movie.en.srt".to_string(),
        source_hash: "sha256:abc".to_string(),
        source_format: "srt".to_string(),
        source_encoding: "utf-8".to_string(),
        encoding_confidence: None,
        source_lang: "en".to_string(),
        target_lang: "my".to_string(),
        format_meta: None,
    };
    store
        .call(move |conn| insert_project(conn, &project))
        .await
        .expect("insert project");

    // A pre-existing project has no stored preferences.
    let before = store
        .call(|conn| get_output_prefs(conn))
        .await
        .expect("get prefs before");
    assert_eq!(before, (None, None));

    store
        .call(|conn| set_output_prefs(conn, Some("vtt"), Some("out.vtt")))
        .await
        .expect("set prefs");
    let after = store
        .call(|conn| get_output_prefs(conn))
        .await
        .expect("get prefs after");
    assert_eq!(
        after,
        (Some("vtt".to_string()), Some("out.vtt".to_string()))
    );
}

async fn object_exists(store: &Store, name: &str) -> bool {
    let name = name.to_string();
    store
        .call(move |conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = ?1",
                [name],
                |row| row.get(0),
            )?;
            Ok(count > 0)
        })
        .await
        .expect("query sqlite_master")
}
