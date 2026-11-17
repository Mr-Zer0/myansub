//! Repository for the immutable `cues` table (`docs/DATA_MODEL.md` §1/§2).
//!
//! Cues are written once, in bulk, inside a single transaction and then only
//! read. `inline_tags` is stored as a JSON array so the domain [`Cue`] round-trips
//! exactly (`docs/DATA_MODEL.md` §1).

use rusqlite::{Connection, params};

use super::db::StoreError;
use crate::domain::Cue;

/// Bulk-inserts `cues` in one transaction (`docs/ARCHITECTURE.md` §6).
pub fn insert_cues(conn: &mut Connection, cues: &[Cue]) -> Result<(), StoreError> {
    let tx = conn.transaction()?;
    insert_cues_in_tx(&tx, cues)?;
    tx.commit()?;
    Ok(())
}

/// Bulk-inserts `cues` using a caller-supplied connection, *without* opening its
/// own transaction.
///
/// The input stage groups the project row, cues, and stage state into a single
/// transaction (`docs/ARCHITECTURE.md` §6 rule 1), so it needs an insert that
/// participates in an existing transaction rather than starting a nested one.
/// [`insert_cues`] wraps this in its own transaction for standalone callers.
pub fn insert_cues_in_tx(conn: &Connection, cues: &[Cue]) -> Result<(), StoreError> {
    let mut stmt = conn.prepare(
        "INSERT INTO cues \
         (idx, start_ms, end_ms, text_raw, text_masked, lead_tags, \
          inline_tags, style, speaker, translatable) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )?;
    for cue in cues {
        let inline_tags = serde_json::to_string(&cue.inline_tags)
            .map_err(|err| StoreError::Unexpected(format!("serialize inline_tags: {err}")))?;
        // SQLite integers are signed i64; millisecond timings fit comfortably,
        // but convert fallibly rather than wrap a pathological value silently.
        let start_ms = i64::try_from(cue.start_ms).map_err(|_| {
            StoreError::Unexpected(format!("start_ms out of range: {}", cue.start_ms))
        })?;
        let end_ms = i64::try_from(cue.end_ms)
            .map_err(|_| StoreError::Unexpected(format!("end_ms out of range: {}", cue.end_ms)))?;
        stmt.execute(params![
            cue.idx,
            start_ms,
            end_ms,
            cue.text_raw,
            cue.text_masked,
            cue.lead_tags,
            inline_tags,
            cue.style,
            cue.speaker,
            cue.translatable,
        ])?;
    }
    Ok(())
}

/// Loads all cues ordered by `idx`.
pub fn load_cues(conn: &Connection) -> Result<Vec<Cue>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT idx, start_ms, end_ms, text_raw, text_masked, lead_tags, \
         inline_tags, style, speaker, translatable \
         FROM cues ORDER BY idx",
    )?;
    let rows = stmt.query_map([], |row| {
        // Timings come back as signed i64; convert fallibly below rather than wrap.
        let start_ms: i64 = row.get(1)?;
        let end_ms: i64 = row.get(2)?;
        let inline_tags_json: String = row.get(6)?;
        Ok(RawCue {
            idx: row.get(0)?,
            start_ms,
            end_ms,
            text_raw: row.get(3)?,
            text_masked: row.get(4)?,
            lead_tags: row.get(5)?,
            inline_tags_json,
            style: row.get(7)?,
            speaker: row.get(8)?,
            translatable: row.get(9)?,
        })
    })?;

    let mut cues = Vec::new();
    for row in rows {
        let raw = row?;
        let start_ms = u64::try_from(raw.start_ms).map_err(|_| {
            StoreError::Unexpected(format!("start_ms out of range: {}", raw.start_ms))
        })?;
        let end_ms = u64::try_from(raw.end_ms)
            .map_err(|_| StoreError::Unexpected(format!("end_ms out of range: {}", raw.end_ms)))?;
        let inline_tags = serde_json::from_str(&raw.inline_tags_json)
            .map_err(|err| StoreError::Unexpected(format!("parse inline_tags: {err}")))?;
        cues.push(Cue {
            idx: raw.idx,
            start_ms,
            end_ms,
            text_raw: raw.text_raw,
            text_masked: raw.text_masked,
            lead_tags: raw.lead_tags,
            inline_tags,
            style: raw.style,
            speaker: raw.speaker,
            translatable: raw.translatable,
        });
    }
    Ok(cues)
}

/// A cue row as read from SQLite, before the fallible `i64 → u64` timing and
/// `inline_tags` JSON conversions that would not fit a `rusqlite` row closure.
struct RawCue {
    idx: u32,
    start_ms: i64,
    end_ms: i64,
    text_raw: String,
    text_masked: String,
    lead_tags: Option<String>,
    inline_tags_json: String,
    style: Option<String>,
    speaker: Option<String>,
    translatable: bool,
}
