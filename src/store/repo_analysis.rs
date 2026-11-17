//! Read-side repository for the `analysis` table (`docs/DATA_MODEL.md` §2).
//!
//! The analysis *execution* (map-reduce, versioning, provenance) lands in M5.
//! M4-C needs only the gate's view of the current version: its number and whether
//! it is approved, so the orchestrator can decide when translation may run
//! (`docs/ARCHITECTURE.md` §2). The highest `version` is the current one.

use rusqlite::{Connection, OptionalExtension};

use super::db::StoreError;

/// A compact view of the current (highest-version) analysis row, used by the
/// stage gates. Absent when no analysis has been produced yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurrentAnalysis {
    /// The highest `version` present (the current analysis).
    pub version: i64,
    /// Whether that version has been approved (`analysis.approved = 1`).
    pub approved: bool,
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
