//! SQLite connection, pragmas, and the synchronous→async bridge.
//!
//! [`Store`] owns one [`rusqlite::Connection`] behind `Arc<Mutex<_>>`. rusqlite
//! is synchronous, so DB work runs in short closures on the blocking pool via
//! [`Store::call`] (`docs/ARCHITECTURE.md` §7). On open, the crash-safety pragmas
//! from `docs/ARCHITECTURE.md` §6 are applied and all pending migrations are run.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;
use thiserror::Error;

use super::migrations::run_migrations;

/// Errors from the persistence layer (`docs/ARCHITECTURE.md` §8).
#[derive(Debug, Error)]
pub enum StoreError {
    /// A SQLite operation failed.
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A filesystem operation failed (atomic write, directory sync).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A schema migration failed.
    #[error("migration to version {version} failed: {message}")]
    Migration { version: u32, message: String },
    /// A crash-safety pragma did not take effect (e.g. the filesystem silently
    /// refused WAL and fell back to another journal mode).
    #[error("pragma {pragma} not in effect: got {got:?}")]
    Pragma { pragma: String, got: String },
    /// A path given to the atomic writer has no final file-name component, so no
    /// temporary sibling could be derived.
    #[error("path has no file name: {path}")]
    InvalidPath { path: String },
    /// The connection mutex was poisoned by a panic in another thread.
    #[error("database connection lock was poisoned")]
    Poisoned,
    /// A blocking database task failed to complete (panicked or was cancelled).
    #[error("database task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

/// A handle to the project database.
///
/// Cloning is cheap (`Arc`) and all clones share one connection and one lock, so
/// there is a single writer (`docs/ARCHITECTURE.md` §7). Use [`Store::call`] to
/// run work on the blocking pool.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    /// Opens (or creates) the database at `path`, applies the crash-safety
    /// pragmas, and runs all pending migrations.
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    /// Opens an in-memory database for tests. WAL is a no-op for in-memory
    /// databases; the remaining pragmas and all migrations still apply.
    pub fn open_in_memory() -> Result<Store, StoreError> {
        let conn = Connection::open_in_memory()?;
        Self::from_connection(conn)
    }

    fn from_connection(mut conn: Connection) -> Result<Store, StoreError> {
        apply_pragmas(&conn)?;
        run_migrations(&mut conn)?;
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Runs `f` against the connection on the blocking pool.
    ///
    /// The closure receives `&mut Connection` so it can open transactions. It
    /// must be short and must not perform network I/O (`docs/ARCHITECTURE.md`
    /// §7). A poisoned lock maps to [`StoreError::Poisoned`] rather than a panic.
    pub async fn call<F, T>(&self, f: F) -> Result<T, StoreError>
    where
        F: FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static,
        T: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        let handle = tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().map_err(|_| StoreError::Poisoned)?;
            f(&mut guard)
        });
        match handle.await {
            Ok(result) => result,
            Err(join_err) => Err(StoreError::from(join_err)),
        }
    }
}

/// Applies the crash-safety pragmas from `docs/ARCHITECTURE.md` §6.
///
/// `journal_mode = WAL` returns the resulting mode, so it is issued as a query;
/// for a file database the mode persists in the file header, while in-memory
/// databases report `memory` and are unaffected. The remaining pragmas are
/// per-connection and must be re-applied on every open.
fn apply_pragmas(conn: &Connection) -> Result<(), StoreError> {
    conn.busy_timeout(Duration::from_millis(5000))?;
    conn.execute_batch("PRAGMA foreign_keys = ON;\nPRAGMA synchronous = FULL;")?;
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL;", [], |row| row.get(0))?;
    verify_journal_mode(&mode)?;
    Ok(())
}

/// Confirms SQLite accepted WAL. On a filesystem that rejects WAL, SQLite does
/// not error — it silently falls back to another journal mode (e.g. `delete`),
/// which would quietly drop the crash-safety guarantee. In-memory databases
/// legitimately report `memory`, which is accepted.
fn verify_journal_mode(mode: &str) -> Result<(), StoreError> {
    if mode.eq_ignore_ascii_case("wal") || mode.eq_ignore_ascii_case("memory") {
        Ok(())
    } else {
        Err(StoreError::Pragma {
            pragma: "journal_mode".to_string(),
            got: mode.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_journal_mode_accepts_wal_and_memory_rejects_others() {
        assert!(verify_journal_mode("wal").is_ok());
        assert!(verify_journal_mode("WAL").is_ok());
        assert!(verify_journal_mode("memory").is_ok());
        let err = verify_journal_mode("delete").expect_err("non-WAL must be rejected");
        assert!(matches!(err, StoreError::Pragma { .. }));
    }
}
