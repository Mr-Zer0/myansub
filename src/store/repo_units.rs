//! Repository for LLM work units: analysis chunks and translation batches.
//!
//! Both unit tables share the status lifecycle `pending → in_flight → done`
//! (with `failed` as a terminal error state), `attempts`, and `last_error`
//! (`docs/DATA_MODEL.md` §2). They differ only in their primary-key column and in
//! what "a result" means: an analysis chunk stores a JSON `result`, while a
//! translation batch has no result column here (the per-cue translations are
//! written by `repo_translations` in M6), so completing a batch just flips its
//! status (`docs/ARCHITECTURE.md` §6 rule 1).
//!
//! Crash-safety (`docs/ARCHITECTURE.md` §6):
//! - **One unit, one transaction.** [`complete_analysis_chunk`] writes the result
//!   and flips the status in a single transaction; the `*_in_tx` variants expose
//!   that atomic step for callers (and the failure-injection test, `docs/TESTING.md`
//!   §4) that drive the transaction themselves.
//! - **Recover on start.** [`recover`] resets every `in_flight` unit back to
//!   `pending` in one transaction; it is a no-op on a fresh database.

use rusqlite::{Connection, Row, Transaction, params};

use super::db::StoreError;

/// The status of a work unit (`docs/DATA_MODEL.md` §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitStatus {
    Pending,
    InFlight,
    Done,
    Failed,
}

impl UnitStatus {
    /// The canonical label stored in the `status` column.
    pub fn as_str(self) -> &'static str {
        match self {
            UnitStatus::Pending => "pending",
            UnitStatus::InFlight => "in_flight",
            UnitStatus::Done => "done",
            UnitStatus::Failed => "failed",
        }
    }

    /// Parses a status label read from the database.
    pub fn from_db(value: &str) -> Result<UnitStatus, StoreError> {
        match value {
            "pending" => Ok(UnitStatus::Pending),
            "in_flight" => Ok(UnitStatus::InFlight),
            "done" => Ok(UnitStatus::Done),
            "failed" => Ok(UnitStatus::Failed),
            other => Err(StoreError::Unexpected(format!("unit status {other:?}"))),
        }
    }
}

/// Describes a unit table's identity for the operations shared by both tables.
///
/// Only the table name, primary-key column, and whether the table carries an
/// `updated_at` column differ between `analysis_chunks` and `translation_batches`.
/// These names are compile-time constants, never user input, so building SQL with
/// them is safe.
struct UnitTable {
    name: &'static str,
    id_col: &'static str,
    has_updated_at: bool,
}

const CHUNKS: UnitTable = UnitTable {
    name: "analysis_chunks",
    id_col: "chunk_no",
    has_updated_at: false,
};

const BATCHES: UnitTable = UnitTable {
    name: "translation_batches",
    id_col: "batch_no",
    has_updated_at: true,
};

impl UnitTable {
    /// The SQL fragment that bumps `updated_at` when the table has that column, or
    /// an empty string otherwise. Always returned with a leading comma so it can
    /// be appended to a `SET` list.
    fn updated_at_set(&self) -> &'static str {
        if self.has_updated_at {
            ", updated_at = datetime('now')"
        } else {
            ""
        }
    }
}

/// An analysis-chunk row (`analysis_chunks`, `docs/DATA_MODEL.md` §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisChunk {
    pub chunk_no: i64,
    pub first_cue: i64,
    pub last_cue: i64,
    pub status: UnitStatus,
    pub attempts: i64,
    pub last_error: Option<String>,
    /// JSON partial analysis, present once the chunk is `done`.
    pub result: Option<String>,
}

/// Fields needed to enqueue a translation batch (`translation_batches`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTranslationBatch {
    pub batch_no: i64,
    pub first_cue: i64,
    pub last_cue: i64,
    pub input_hash: Option<String>,
    pub model: Option<String>,
    pub analysis_version: Option<i64>,
}

/// A translation-batch row (`translation_batches`, `docs/DATA_MODEL.md` §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslationBatch {
    pub batch_no: i64,
    pub first_cue: i64,
    pub last_cue: i64,
    pub status: UnitStatus,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub input_hash: Option<String>,
    pub model: Option<String>,
    pub analysis_version: Option<i64>,
}

// ---------------------------------------------------------------------------
// Enqueue
// ---------------------------------------------------------------------------

/// Enqueues an analysis chunk with status `pending`.
pub fn create_analysis_chunk(
    conn: &Connection,
    chunk_no: i64,
    first_cue: i64,
    last_cue: i64,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO analysis_chunks (chunk_no, first_cue, last_cue, status, attempts) \
         VALUES (?1, ?2, ?3, 'pending', 0)",
        params![chunk_no, first_cue, last_cue],
    )?;
    Ok(())
}

/// Enqueues a translation batch with status `pending`.
pub fn create_translation_batch(
    conn: &Connection,
    batch: &NewTranslationBatch,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO translation_batches \
         (batch_no, first_cue, last_cue, status, attempts, input_hash, model, analysis_version) \
         VALUES (?1, ?2, ?3, 'pending', 0, ?4, ?5, ?6)",
        params![
            batch.batch_no,
            batch.first_cue,
            batch.last_cue,
            batch.input_hash,
            batch.model,
            batch.analysis_version,
        ],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Claim: pending → in_flight
// ---------------------------------------------------------------------------

/// Claims a pending analysis chunk, moving it to `in_flight` and incrementing
/// `attempts`. Returns `true` if this call won the claim, `false` if the unit was
/// not `pending` (already claimed or finished) and was left untouched.
pub fn claim_analysis_chunk(conn: &Connection, chunk_no: i64) -> Result<bool, StoreError> {
    claim(conn, &CHUNKS, chunk_no)
}

/// Claims a pending translation batch, moving it to `in_flight` and incrementing
/// `attempts`. Returns `true` if this call won the claim (see
/// [`claim_analysis_chunk`]).
pub fn claim_translation_batch(conn: &Connection, batch_no: i64) -> Result<bool, StoreError> {
    claim(conn, &BATCHES, batch_no)
}

/// Moves a pending unit to `in_flight`, incrementing `attempts`. A unit not in
/// `pending` is left untouched. Returns whether a row changed, so a caller can
/// distinguish winning the claim from losing it (`docs/ARCHITECTURE.md` §6).
fn claim(conn: &Connection, table: &UnitTable, id: i64) -> Result<bool, StoreError> {
    let sql = format!(
        "UPDATE {} SET status = 'in_flight', attempts = attempts + 1{} \
         WHERE {} = ?1 AND status = 'pending'",
        table.name,
        table.updated_at_set(),
        table.id_col,
    );
    let changed = conn.execute(&sql, params![id])?;
    Ok(changed == 1)
}

// ---------------------------------------------------------------------------
// Complete: in_flight → done (result + status, one transaction)
// ---------------------------------------------------------------------------

/// Writes an analysis chunk's `result` and flips it to `done` within the caller's
/// transaction. The single `UPDATE` makes the result and the status change atomic
/// (`docs/ARCHITECTURE.md` §6 rule 1).
/// Returns `true` if the chunk was `in_flight` and is now `done` with its result
/// written, `false` if it was in another state and left untouched.
pub fn complete_analysis_chunk_in_tx(
    tx: &Transaction,
    chunk_no: i64,
    result: &str,
) -> Result<bool, StoreError> {
    let changed = tx.execute(
        "UPDATE analysis_chunks SET result = ?2, status = 'done' \
         WHERE chunk_no = ?1 AND status = 'in_flight'",
        params![chunk_no, result],
    )?;
    Ok(changed == 1)
}

/// Completes an analysis chunk in its own transaction (result + status together).
/// Returns whether a row changed (see [`complete_analysis_chunk_in_tx`]).
pub fn complete_analysis_chunk(
    conn: &mut Connection,
    chunk_no: i64,
    result: &str,
) -> Result<bool, StoreError> {
    let tx = conn.transaction()?;
    let changed = complete_analysis_chunk_in_tx(&tx, chunk_no, result)?;
    tx.commit()?;
    Ok(changed)
}

/// Flips a translation batch to `done` within the caller's transaction. There is
/// no result column here (translations rows are written by `repo_translations` in
/// M6), so completion is the status change plus `updated_at`. Returns `true` if
/// the batch was `in_flight` and is now `done`, `false` otherwise.
pub fn complete_translation_batch_in_tx(
    tx: &Transaction,
    batch_no: i64,
) -> Result<bool, StoreError> {
    let changed = tx.execute(
        "UPDATE translation_batches SET status = 'done', updated_at = datetime('now') \
         WHERE batch_no = ?1 AND status = 'in_flight'",
        params![batch_no],
    )?;
    Ok(changed == 1)
}

/// Completes a translation batch in its own transaction. Returns whether a row
/// changed (see [`complete_translation_batch_in_tx`]).
pub fn complete_translation_batch(
    conn: &mut Connection,
    batch_no: i64,
) -> Result<bool, StoreError> {
    let tx = conn.transaction()?;
    let changed = complete_translation_batch_in_tx(&tx, batch_no)?;
    tx.commit()?;
    Ok(changed)
}

// ---------------------------------------------------------------------------
// Fail: in_flight → failed
// ---------------------------------------------------------------------------

/// Marks an `in_flight` analysis chunk `failed` and records `last_error`. Returns
/// `true` if a row changed, `false` if the chunk was not `in_flight` (e.g. already
/// `done`) and was left untouched, so a rerun never redoes paid work.
pub fn fail_analysis_chunk(
    conn: &Connection,
    chunk_no: i64,
    last_error: &str,
) -> Result<bool, StoreError> {
    fail(conn, &CHUNKS, chunk_no, last_error)
}

/// Marks an `in_flight` translation batch `failed` and records `last_error`.
/// Returns whether a row changed (see [`fail_analysis_chunk`]).
pub fn fail_translation_batch(
    conn: &Connection,
    batch_no: i64,
    last_error: &str,
) -> Result<bool, StoreError> {
    fail(conn, &BATCHES, batch_no, last_error)
}

/// Marks an `in_flight` unit `failed` and records `last_error`. The `in_flight`
/// guard keeps a `done` unit's status and result intact (symmetry with claim and
/// complete). Returns whether a row changed.
fn fail(
    conn: &Connection,
    table: &UnitTable,
    id: i64,
    last_error: &str,
) -> Result<bool, StoreError> {
    let sql = format!(
        "UPDATE {} SET status = 'failed', last_error = ?2{} \
         WHERE {} = ?1 AND status = 'in_flight'",
        table.name,
        table.updated_at_set(),
        table.id_col,
    );
    let changed = conn.execute(&sql, params![id, last_error])?;
    Ok(changed == 1)
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

/// Columns selected for an [`AnalysisChunk`], in row order.
const CHUNK_COLUMNS: &str = "chunk_no, first_cue, last_cue, status, attempts, last_error, result";

/// Columns selected for a [`TranslationBatch`], in row order.
const BATCH_COLUMNS: &str = "batch_no, first_cue, last_cue, status, attempts, last_error, \
     input_hash, model, analysis_version";

/// Maps a row (selected with [`CHUNK_COLUMNS`]) to an [`AnalysisChunk`].
fn chunk_from_row(row: &Row) -> Result<AnalysisChunk, StoreError> {
    let status: String = row.get(3)?;
    Ok(AnalysisChunk {
        chunk_no: row.get(0)?,
        first_cue: row.get(1)?,
        last_cue: row.get(2)?,
        status: UnitStatus::from_db(&status)?,
        attempts: row.get(4)?,
        last_error: row.get(5)?,
        result: row.get(6)?,
    })
}

/// Maps a row (selected with [`BATCH_COLUMNS`]) to a [`TranslationBatch`].
fn batch_from_row(row: &Row) -> Result<TranslationBatch, StoreError> {
    let status: String = row.get(3)?;
    Ok(TranslationBatch {
        batch_no: row.get(0)?,
        first_cue: row.get(1)?,
        last_cue: row.get(2)?,
        status: UnitStatus::from_db(&status)?,
        attempts: row.get(4)?,
        last_error: row.get(5)?,
        input_hash: row.get(6)?,
        model: row.get(7)?,
        analysis_version: row.get(8)?,
    })
}

/// Reads one analysis chunk, if present.
pub fn get_analysis_chunk(
    conn: &Connection,
    chunk_no: i64,
) -> Result<Option<AnalysisChunk>, StoreError> {
    let sql = format!("SELECT {CHUNK_COLUMNS} FROM analysis_chunks WHERE chunk_no = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![chunk_no])?;
    match rows.next()? {
        Some(row) => Ok(Some(chunk_from_row(row)?)),
        None => Ok(None),
    }
}

/// Reads one translation batch, if present.
pub fn get_translation_batch(
    conn: &Connection,
    batch_no: i64,
) -> Result<Option<TranslationBatch>, StoreError> {
    let sql = format!("SELECT {BATCH_COLUMNS} FROM translation_batches WHERE batch_no = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![batch_no])?;
    match rows.next()? {
        Some(row) => Ok(Some(batch_from_row(row)?)),
        None => Ok(None),
    }
}

/// Lists analysis chunks in the given status, ordered by `chunk_no`.
pub fn list_analysis_chunks_by_status(
    conn: &Connection,
    status: UnitStatus,
) -> Result<Vec<AnalysisChunk>, StoreError> {
    let sql =
        format!("SELECT {CHUNK_COLUMNS} FROM analysis_chunks WHERE status = ?1 ORDER BY chunk_no");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![status.as_str()])?;
    let mut chunks = Vec::new();
    while let Some(row) = rows.next()? {
        chunks.push(chunk_from_row(row)?);
    }
    Ok(chunks)
}

/// Lists translation batches in the given status, ordered by `batch_no`.
pub fn list_translation_batches_by_status(
    conn: &Connection,
    status: UnitStatus,
) -> Result<Vec<TranslationBatch>, StoreError> {
    let sql = format!(
        "SELECT {BATCH_COLUMNS} FROM translation_batches WHERE status = ?1 ORDER BY batch_no"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![status.as_str()])?;
    let mut batches = Vec::new();
    while let Some(row) = rows.next()? {
        batches.push(batch_from_row(row)?);
    }
    Ok(batches)
}

/// Counts analysis chunks in the given status.
pub fn count_analysis_chunks_by_status(
    conn: &Connection,
    status: UnitStatus,
) -> Result<usize, StoreError> {
    count_by_status(conn, &CHUNKS, status)
}

/// Counts translation batches in the given status.
pub fn count_translation_batches_by_status(
    conn: &Connection,
    status: UnitStatus,
) -> Result<usize, StoreError> {
    count_by_status(conn, &BATCHES, status)
}

/// Counts units of a table in the given status.
fn count_by_status(
    conn: &Connection,
    table: &UnitTable,
    status: UnitStatus,
) -> Result<usize, StoreError> {
    let sql = format!("SELECT COUNT(*) FROM {} WHERE status = ?1", table.name);
    let count: i64 = conn.query_row(&sql, params![status.as_str()], |row| row.get(0))?;
    Ok(count.max(0) as usize)
}

/// Lists every analysis chunk in `chunk_no` order, regardless of status. Used by
/// the analysis pipeline to build its scheduler units on a fresh run or a resume.
pub fn list_all_analysis_chunks(conn: &Connection) -> Result<Vec<AnalysisChunk>, StoreError> {
    let sql = format!("SELECT {CHUNK_COLUMNS} FROM analysis_chunks ORDER BY chunk_no");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let mut chunks = Vec::new();
    while let Some(row) = rows.next()? {
        chunks.push(chunk_from_row(row)?);
    }
    Ok(chunks)
}

// ---------------------------------------------------------------------------
// Requeue / reset / clear (analysis chunks)
// ---------------------------------------------------------------------------

/// Resets one `in_flight` analysis chunk back to `pending` (the scheduler's
/// per-unit recovery seam, `docs/ARCHITECTURE.md` §4.1). Returns whether a row
/// changed. A chunk in any other state is left untouched.
pub fn reset_analysis_chunk(conn: &Connection, chunk_no: i64) -> Result<bool, StoreError> {
    let changed = conn.execute(
        "UPDATE analysis_chunks SET status = 'pending' \
         WHERE chunk_no = ?1 AND status = 'in_flight'",
        params![chunk_no],
    )?;
    Ok(changed == 1)
}

/// Requeues every `failed` analysis chunk back to `pending` so a `--retry-failed`
/// run reruns only the failures (`docs/CLI.md` §6). `attempts` is preserved as the
/// running count. Returns the number of chunks requeued.
pub fn requeue_failed_analysis_chunks(conn: &Connection) -> Result<usize, StoreError> {
    let changed = conn.execute(
        "UPDATE analysis_chunks SET status = 'pending' WHERE status = 'failed'",
        [],
    )?;
    Ok(changed)
}

/// Deletes every analysis chunk row (a fresh `--redo` run starts from a clean map
/// phase). Returns the number of rows removed.
pub fn clear_analysis_chunks(conn: &Connection) -> Result<usize, StoreError> {
    let changed = conn.execute("DELETE FROM analysis_chunks", [])?;
    Ok(changed)
}

// ---------------------------------------------------------------------------
// Recovery
// ---------------------------------------------------------------------------

/// Resets every `in_flight` unit back to `pending` across both unit tables, in a
/// single transaction (`docs/ARCHITECTURE.md` §6 rule 2). Returns the number of
/// rows reset. A no-op on a fresh database.
pub fn recover(conn: &mut Connection) -> Result<usize, StoreError> {
    let tx = conn.transaction()?;
    let reset = recover_table(&tx, &CHUNKS)? + recover_table(&tx, &BATCHES)?;
    tx.commit()?;
    Ok(reset)
}

/// Resets one table's `in_flight` units to `pending` within `tx`, returning the
/// number of rows affected.
fn recover_table(tx: &Transaction, table: &UnitTable) -> Result<usize, StoreError> {
    let sql = format!(
        "UPDATE {} SET status = 'pending'{} WHERE status = 'in_flight'",
        table.name,
        table.updated_at_set(),
    );
    Ok(tx.execute(&sql, [])?)
}
