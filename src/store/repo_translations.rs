//! Repository for the `translations` and `translation_history` tables
//! (`docs/DATA_MODEL.md` §2).
//!
//! A translation row is keyed by `cue_id` (the `cues.id` the FK targets) and holds
//! the masked target text, its `origin` (`llm`/`user`), the `locked`/`stale`
//! flags, the deterministic quality `flags` (a JSON array of
//! [`QualityFlag`](crate::domain::QualityFlag)), and the provenance
//! (`analysis_version`, `model`). Whenever an existing row's text changes, the
//! previous text is appended to `translation_history` (`docs/DATA_MODEL.md` §6),
//! so every superseded translation is recoverable.
//!
//! Crash-safety (`docs/ARCHITECTURE.md` §6 rule 1): the translation engine writes
//! all of a batch's rows **and** the batch's `done` status in one transaction, so
//! the `*_in_tx` seam ([`upsert_translation_in_tx`]) is the primary entry point;
//! the plain [`upsert_translation`] wraps one transaction for standalone use.
//!
//! A `locked = 1` row is never overwritten (locking is enforced in M7, but this
//! repository respects it if present): [`upsert_translation_in_tx`] leaves a locked
//! row untouched and reports that it did not write.

use std::collections::BTreeMap;

use rusqlite::{Connection, Row, Transaction, params};

use super::db::StoreError;
use crate::domain::QualityFlag;

/// Where a translation came from (`docs/DATA_MODEL.md` §2, the `origin` column
/// `CHECK (origin IN ('llm','user'))`). `Llm` is a produced translation; `User`
/// is a reviewer edit (M7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranslationOrigin {
    Llm,
    User,
}

impl TranslationOrigin {
    /// The canonical label stored in the `origin` column.
    pub fn as_str(self) -> &'static str {
        match self {
            TranslationOrigin::Llm => "llm",
            TranslationOrigin::User => "user",
        }
    }

    /// Parses an origin label read from the database.
    pub fn from_db(value: &str) -> Result<TranslationOrigin, StoreError> {
        match value {
            "llm" => Ok(TranslationOrigin::Llm),
            "user" => Ok(TranslationOrigin::User),
            other => Err(StoreError::Unexpected(format!(
                "translation origin {other:?}"
            ))),
        }
    }
}

/// The fields needed to write one translation (`translations`,
/// `docs/DATA_MODEL.md` §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTranslation {
    /// The `cues.id` this translation belongs to (the FK target, *not* the cue
    /// `idx` sent to the LLM; resolve it with [`cue_id_by_idx`]).
    pub cue_id: i64,
    /// The masked target text (placeholders restored on export).
    pub text: String,
    pub origin: TranslationOrigin,
    pub locked: bool,
    pub stale: bool,
    /// Deterministic quality flags (`docs/DATA_MODEL.md` §5).
    pub flags: Vec<QualityFlag>,
    pub analysis_version: i64,
    pub model: Option<String>,
}

/// A translation row read back (`translations`, `docs/DATA_MODEL.md` §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslationRow {
    pub cue_id: i64,
    pub text: String,
    pub origin: TranslationOrigin,
    pub locked: bool,
    pub stale: bool,
    pub flags: Vec<QualityFlag>,
    pub analysis_version: i64,
    pub model: Option<String>,
}

/// A `translation_history` row (`docs/DATA_MODEL.md` §2): a superseded target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslationHistoryRow {
    pub id: i64,
    pub cue_id: i64,
    pub text: String,
    pub origin: TranslationOrigin,
}

const TRANSLATION_COLUMNS: &str =
    "cue_id, text, origin, locked, stale, flags, analysis_version, model";

/// Serializes quality flags to the `flags` JSON array (`docs/DATA_MODEL.md` §5).
fn flags_to_json(flags: &[QualityFlag]) -> Result<String, StoreError> {
    serde_json::to_string(flags)
        .map_err(|e| StoreError::Unexpected(format!("serialize flags: {e}")))
}

/// Parses the `flags` JSON array.
fn flags_from_json(json: &str) -> Result<Vec<QualityFlag>, StoreError> {
    serde_json::from_str(json).map_err(|e| StoreError::Unexpected(format!("parse flags: {e}")))
}

fn translation_from_row(row: &Row) -> Result<TranslationRow, StoreError> {
    let origin: String = row.get(2)?;
    let locked: i64 = row.get(3)?;
    let stale: i64 = row.get(4)?;
    let flags_json: String = row.get(5)?;
    Ok(TranslationRow {
        cue_id: row.get(0)?,
        text: row.get(1)?,
        origin: TranslationOrigin::from_db(&origin)?,
        locked: locked != 0,
        stale: stale != 0,
        flags: flags_from_json(&flags_json)?,
        analysis_version: row.get(6)?,
        model: row.get(7)?,
    })
}

/// Upserts a translation within the caller's transaction (`docs/ARCHITECTURE.md`
/// §6 rule 1). When a prior row exists:
///
/// - a `locked = 1` prior row is **never overwritten** — the call is a no-op and
///   returns `false` (locking is enforced in M7, respected here if present);
/// - otherwise, if the prior text differs from the new text, the prior text is
///   appended to `translation_history` before the row is replaced
///   (`docs/DATA_MODEL.md` §6).
///
/// Returns `true` when the row was written, `false` when a locked row blocked it.
pub fn upsert_translation_in_tx(tx: &Transaction, t: &NewTranslation) -> Result<bool, StoreError> {
    // Read the prior row (text, origin, locked) if present.
    let prior: Option<(String, String, i64)> = {
        let mut stmt =
            tx.prepare("SELECT text, origin, locked FROM translations WHERE cue_id = ?1")?;
        let mut rows = stmt.query(params![t.cue_id])?;
        match rows.next()? {
            Some(row) => Some((row.get(0)?, row.get(1)?, row.get(2)?)),
            None => None,
        }
    };

    if let Some((prior_text, prior_origin, locked)) = &prior {
        if *locked != 0 {
            // Locked rows are authoritative; never clobber a reviewer's work.
            return Ok(false);
        }
        if prior_text != &t.text {
            tx.execute(
                "INSERT INTO translation_history (cue_id, text, origin) VALUES (?1, ?2, ?3)",
                params![t.cue_id, prior_text, prior_origin],
            )?;
        }
    }

    let flags_json = flags_to_json(&t.flags)?;
    tx.execute(
        "INSERT INTO translations \
         (cue_id, text, origin, locked, stale, flags, analysis_version, model, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, datetime('now')) \
         ON CONFLICT(cue_id) DO UPDATE SET \
           text = excluded.text, origin = excluded.origin, locked = excluded.locked, \
           stale = excluded.stale, flags = excluded.flags, \
           analysis_version = excluded.analysis_version, model = excluded.model, \
           updated_at = excluded.updated_at",
        params![
            t.cue_id,
            t.text,
            t.origin.as_str(),
            t.locked as i64,
            t.stale as i64,
            flags_json,
            t.analysis_version,
            t.model,
        ],
    )?;
    Ok(true)
}

/// Upserts a translation in its own transaction (see [`upsert_translation_in_tx`]).
pub fn upsert_translation(conn: &mut Connection, t: &NewTranslation) -> Result<bool, StoreError> {
    let tx = conn.transaction()?;
    let written = upsert_translation_in_tx(&tx, t)?;
    tx.commit()?;
    Ok(written)
}

/// Reads one translation by `cue_id`, if present.
pub fn get_translation(
    conn: &Connection,
    cue_id: i64,
) -> Result<Option<TranslationRow>, StoreError> {
    let sql = format!("SELECT {TRANSLATION_COLUMNS} FROM translations WHERE cue_id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![cue_id])?;
    match rows.next()? {
        Some(row) => Ok(Some(translation_from_row(row)?)),
        None => Ok(None),
    }
}

/// Lists every translation ordered by `cue_id`.
pub fn list_translations(conn: &Connection) -> Result<Vec<TranslationRow>, StoreError> {
    let sql = format!("SELECT {TRANSLATION_COLUMNS} FROM translations ORDER BY cue_id");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(translation_from_row(row)?);
    }
    Ok(out)
}

/// Counts translation rows (test/report helper).
pub fn count_translations(conn: &Connection) -> Result<usize, StoreError> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM translations", [], |row| row.get(0))?;
    Ok(count.max(0) as usize)
}

/// Replaces the stored quality flags for one translation within the caller's
/// transaction. Returns `true` if a row changed, `false` if no translation exists
/// for `cue_id`.
pub fn set_flags_in_tx(
    tx: &Transaction,
    cue_id: i64,
    flags: &[QualityFlag],
) -> Result<bool, StoreError> {
    let flags_json = flags_to_json(flags)?;
    let changed = tx.execute(
        "UPDATE translations SET flags = ?2, updated_at = datetime('now') WHERE cue_id = ?1",
        params![cue_id, flags_json],
    )?;
    Ok(changed == 1)
}

/// Deletes the translation for `cue_id` unless it is `locked = 1`, so a
/// `translate --redo` run can mint fresh translations for unlocked cues while
/// preserving reviewer-locked ones (`docs/CLI.md` §6, `docs/DATA_MODEL.md` §2).
/// Returns `true` when a row was removed, `false` when no row existed or it was
/// locked and left in place.
pub fn delete_unlocked_translation(conn: &Connection, cue_id: i64) -> Result<bool, StoreError> {
    let changed = conn.execute(
        "DELETE FROM translations WHERE cue_id = ?1 AND locked = 0",
        params![cue_id],
    )?;
    Ok(changed == 1)
}

/// Lists the `translation_history` rows for one cue, oldest first.
pub fn list_translation_history(
    conn: &Connection,
    cue_id: i64,
) -> Result<Vec<TranslationHistoryRow>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, cue_id, text, origin FROM translation_history WHERE cue_id = ?1 ORDER BY id",
    )?;
    let mut rows = stmt.query(params![cue_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let origin: String = row.get(3)?;
        out.push(TranslationHistoryRow {
            id: row.get(0)?,
            cue_id: row.get(1)?,
            text: row.get(2)?,
            origin: TranslationOrigin::from_db(&origin)?,
        });
    }
    Ok(out)
}

/// Maps each cue `idx` to its `cues.id` (the `translations.cue_id` FK target).
///
/// The LLM is addressed by cue `idx` (the `[idx]` markers in the prompt and the
/// `id` field in its response, `docs/DATA_MODEL.md` §4), but `translations` keys on
/// `cues.id`; the engine resolves one through this map rather than assuming the two
/// coincide.
pub fn cue_id_by_idx(conn: &Connection) -> Result<BTreeMap<u32, i64>, StoreError> {
    let mut stmt = conn.prepare("SELECT idx, id FROM cues")?;
    let rows = stmt.query_map([], |row| Ok((row.get::<_, u32>(0)?, row.get::<_, i64>(1)?)))?;
    let mut map = BTreeMap::new();
    for row in rows {
        let (idx, id) = row?;
        map.insert(idx, id);
    }
    Ok(map)
}
