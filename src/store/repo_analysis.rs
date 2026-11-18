//! Repository for the `analysis` table (`docs/DATA_MODEL.md` §2/§3).
//!
//! The analysis table stores every analysis *version*: the highest `version` is
//! the current one, and only the current one is ever approved for the translation
//! gate (`docs/ARCHITECTURE.md` §2). Each row carries the document JSON plus its
//! provenance in table columns, not in the JSON (`docs/DATA_MODEL.md` §3):
//! `origin` (`llm`/`user`), `source_digest` (the project source hash the analysis
//! was produced against), and `signature` (`"prompt_v{N}|{model}"`).
//!
//! Writing a version is one transaction (`docs/ARCHITECTURE.md` §6 rule 1): the
//! [`insert_analysis_version_in_tx`] seam lets the analysis pipeline land the new
//! version inside the same transaction as the cache commit and the stage-state
//! update, so a crash leaves either the whole new version or none of it.

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::db::StoreError;

/// Where an analysis version came from (`docs/DATA_MODEL.md` §2, the `origin`
/// column `CHECK (origin IN ('llm','user'))`). `Llm` is a produced analysis; `User`
/// is an imported/edited one (M5-B).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalysisOrigin {
    Llm,
    User,
}

impl AnalysisOrigin {
    /// The canonical label stored in the `origin` column.
    pub fn as_str(self) -> &'static str {
        match self {
            AnalysisOrigin::Llm => "llm",
            AnalysisOrigin::User => "user",
        }
    }

    /// Parses an origin label read from the database.
    pub fn from_db(value: &str) -> Result<AnalysisOrigin, StoreError> {
        match value {
            "llm" => Ok(AnalysisOrigin::Llm),
            "user" => Ok(AnalysisOrigin::User),
            other => Err(StoreError::Unexpected(format!("analysis origin {other:?}"))),
        }
    }
}

/// A compact view of the current (highest-version) analysis row, used by the
/// stage gates. Absent when no analysis has been produced yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurrentAnalysis {
    /// The highest `version` present (the current analysis).
    pub version: i64,
    /// Whether that version has been approved (`analysis.approved = 1`).
    pub approved: bool,
}

/// A full analysis version row (`docs/DATA_MODEL.md` §2), document plus provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisVersion {
    pub version: i64,
    /// The analysis document JSON (`docs/DATA_MODEL.md` §3).
    pub doc: String,
    pub origin: AnalysisOrigin,
    /// The project source hash the analysis was produced against.
    pub source_digest: String,
    /// `"prompt_v{N}|{model}"` (`docs/DATA_MODEL.md` §2).
    pub signature: String,
    pub approved: bool,
}

/// Columns selected for an [`AnalysisVersion`], in row order.
const VERSION_COLUMNS: &str = "version, doc, origin, source_digest, signature, approved";

fn version_from_row(row: &rusqlite::Row) -> Result<AnalysisVersion, StoreError> {
    let origin: String = row.get(2)?;
    let approved: i64 = row.get(5)?;
    Ok(AnalysisVersion {
        version: row.get(0)?,
        doc: row.get(1)?,
        origin: AnalysisOrigin::from_db(&origin)?,
        source_digest: row.get(3)?,
        signature: row.get(4)?,
        approved: approved != 0,
    })
}

/// Inserts a new analysis version within the caller's transaction, returning the
/// new version number (`docs/ARCHITECTURE.md` §6 rule 1). The version is
/// `MAX(version) + 1` (1 for the first), `approved` defaults to 0, so the new
/// version re-blocks translation until it is approved (`docs/ARCHITECTURE.md` §2).
pub fn insert_analysis_version_in_tx(
    tx: &Transaction,
    doc: &str,
    origin: AnalysisOrigin,
    source_digest: &str,
    signature: &str,
) -> Result<i64, StoreError> {
    let next: i64 = tx.query_row(
        "SELECT COALESCE(MAX(version), 0) + 1 FROM analysis",
        [],
        |row| row.get(0),
    )?;
    tx.execute(
        "INSERT INTO analysis (version, doc, origin, source_digest, signature, approved) \
         VALUES (?1, ?2, ?3, ?4, ?5, 0)",
        params![next, doc, origin.as_str(), source_digest, signature],
    )?;
    Ok(next)
}

/// Inserts a new analysis version in its own transaction, returning the new
/// version number (see [`insert_analysis_version_in_tx`]).
pub fn insert_analysis_version(
    conn: &mut Connection,
    doc: &str,
    origin: AnalysisOrigin,
    source_digest: &str,
    signature: &str,
) -> Result<i64, StoreError> {
    let tx = conn.transaction()?;
    let version = insert_analysis_version_in_tx(&tx, doc, origin, source_digest, signature)?;
    tx.commit()?;
    Ok(version)
}

/// Reads one analysis version in full, if present.
pub fn get_analysis_version(
    conn: &Connection,
    version: i64,
) -> Result<Option<AnalysisVersion>, StoreError> {
    let sql = format!("SELECT {VERSION_COLUMNS} FROM analysis WHERE version = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![version])?;
    match rows.next()? {
        Some(row) => Ok(Some(version_from_row(row)?)),
        None => Ok(None),
    }
}

/// Lists every analysis version in ascending version order.
pub fn list_analysis_versions(conn: &Connection) -> Result<Vec<AnalysisVersion>, StoreError> {
    let sql = format!("SELECT {VERSION_COLUMNS} FROM analysis ORDER BY version");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(version_from_row(row)?);
    }
    Ok(out)
}

/// Reads the current analysis (highest version), if any exists.
pub fn current_analysis(conn: &Connection) -> Result<Option<CurrentAnalysis>, StoreError> {
    let row = conn
        .query_row(
            "SELECT version, approved FROM analysis ORDER BY version DESC LIMIT 1",
            [],
            |row| {
                let version: i64 = row.get(0)?;
                let approved: i64 = row.get(1)?;
                Ok(CurrentAnalysis {
                    version,
                    approved: approved != 0,
                })
            },
        )
        .optional()?;
    Ok(row)
}

/// Whether the current analysis version exists and is approved — the signal the
/// translation gate checks (`docs/ARCHITECTURE.md` §2). `false` when there is no
/// analysis yet.
pub fn current_analysis_approved(conn: &Connection) -> Result<bool, StoreError> {
    Ok(current_analysis(conn)?.is_some_and(|a| a.approved))
}

/// Marks the current (highest-version) analysis approved within the caller's
/// transaction (`docs/ARCHITECTURE.md` §2: "Approve = mark current analysis version
/// approved"). Returns `true` if a row was updated, `false` when there is no
/// analysis version to approve.
pub fn approve_current_analysis_in_tx(tx: &Transaction) -> Result<bool, StoreError> {
    let changed = tx.execute(
        "UPDATE analysis SET approved = 1 \
         WHERE version = (SELECT MAX(version) FROM analysis)",
        [],
    )?;
    Ok(changed == 1)
}

/// Marks the current analysis version approved in its own transaction (see
/// [`approve_current_analysis_in_tx`]).
pub fn approve_current_analysis(conn: &mut Connection) -> Result<bool, StoreError> {
    let tx = conn.transaction()?;
    let changed = approve_current_analysis_in_tx(&tx)?;
    tx.commit()?;
    Ok(changed)
}
