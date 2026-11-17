//! Repository for the single project row and the per-stage state machine.
//!
//! The `project` table holds exactly one row (`id = 1`, `docs/DATA_MODEL.md`
//! §2) describing the source file and language settings. `stage_state` tracks
//! each pipeline stage's status (`docs/ARCHITECTURE.md` §2). All functions take a
//! [`rusqlite::Connection`] (obtained inside a [`super::db::Store::call`] closure)
//! and keep their work short (`docs/ARCHITECTURE.md` §7).

use rusqlite::{Connection, OptionalExtension, params};

use super::db::StoreError;

/// The single project row (`docs/DATA_MODEL.md` §2).
///
/// `created_at` is managed by SQLite and is not represented here. `target_lang`
/// defaults to `my` at the schema level; callers that do not set it should pass
/// `"my"`.
#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    pub source_path: String,
    pub source_hash: String,
    pub source_format: String,
    pub source_encoding: String,
    pub encoding_confidence: Option<f64>,
    pub source_lang: String,
    pub target_lang: String,
    /// JSON blob (serialized [`crate::domain::FormatMeta`]) or `None`.
    pub format_meta: Option<String>,
}

/// Inserts the single project row (`id = 1`).
pub fn insert_project(conn: &Connection, project: &Project) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO project \
         (id, source_path, source_hash, source_format, source_encoding, \
          encoding_confidence, source_lang, target_lang, format_meta) \
         VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            project.source_path,
            project.source_hash,
            project.source_format,
            project.source_encoding,
            project.encoding_confidence,
            project.source_lang,
            project.target_lang,
            project.format_meta,
        ],
    )?;
    Ok(())
}

/// Deletes every row from every project-scoped table, using a caller-supplied
/// connection without opening its own transaction.
///
/// This is the destructive half of `init --force` (`docs/CLI.md` §6): when the
/// source changed, the old project data is wiped before the input stage
/// repopulates it. The input stage runs this and the re-insert inside one
/// transaction so a crash leaves either the old data intact or the new data fully
/// in place (`docs/ARCHITECTURE.md` §6). Running it on an empty database is a
/// harmless no-op, so the input stage can call it unconditionally.
pub fn clear_all_data(conn: &Connection) -> Result<(), StoreError> {
    // Children before parents so foreign keys never block a delete.
    const TABLES: &[&str] = &[
        "translation_history",
        "translations",
        "translation_batches",
        "analysis",
        "analysis_chunks",
        "cues",
        "stage_state",
        "project",
        "llm_cache",
        "llm_calls",
        "provider_cache",
    ];
    for table in TABLES {
        conn.execute(&format!("DELETE FROM {table}"), [])?;
    }
    Ok(())
}

/// Reads the project row, if any.
pub fn get_project(conn: &Connection) -> Result<Option<Project>, StoreError> {
    let project = conn
        .query_row(
            "SELECT source_path, source_hash, source_format, source_encoding, \
             encoding_confidence, source_lang, target_lang, format_meta \
             FROM project WHERE id = 1",
            [],
            |row| {
                Ok(Project {
                    source_path: row.get(0)?,
                    source_hash: row.get(1)?,
                    source_format: row.get(2)?,
                    source_encoding: row.get(3)?,
                    encoding_confidence: row.get(4)?,
                    source_lang: row.get(5)?,
                    target_lang: row.get(6)?,
                    format_meta: row.get(7)?,
                })
            },
        )
        .optional()?;
    Ok(project)
}

/// A pipeline stage (`docs/ARCHITECTURE.md` §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Input,
    Analysis,
    AnalysisReview,
    Translation,
    TranslationReview,
    Output,
}

impl Stage {
    /// The canonical label stored in `stage_state.stage`.
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Input => "input",
            Stage::Analysis => "analysis",
            Stage::AnalysisReview => "analysis_review",
            Stage::Translation => "translation",
            Stage::TranslationReview => "translation_review",
            Stage::Output => "output",
        }
    }
}

/// The status of a stage (`docs/ARCHITECTURE.md` §2). `Approved` applies only to
/// the two review stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageStatus {
    NotStarted,
    Running,
    Completed,
    Approved,
}

impl StageStatus {
    /// The canonical label stored in `stage_state.status`.
    pub fn as_str(self) -> &'static str {
        match self {
            StageStatus::NotStarted => "not_started",
            StageStatus::Running => "running",
            StageStatus::Completed => "completed",
            StageStatus::Approved => "approved",
        }
    }

    /// Parses a status label read from the database.
    pub fn from_db(value: &str) -> Result<StageStatus, StoreError> {
        match value {
            "not_started" => Ok(StageStatus::NotStarted),
            "running" => Ok(StageStatus::Running),
            "completed" => Ok(StageStatus::Completed),
            "approved" => Ok(StageStatus::Approved),
            other => Err(StoreError::Unexpected(format!("stage status {other:?}"))),
        }
    }
}

/// Upserts a stage's status, stamping `updated_at`.
pub fn set_stage(conn: &Connection, stage: Stage, status: StageStatus) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO stage_state (stage, status, updated_at) \
         VALUES (?1, ?2, datetime('now')) \
         ON CONFLICT(stage) DO UPDATE SET status = excluded.status, \
         updated_at = excluded.updated_at",
        params![stage.as_str(), status.as_str()],
    )?;
    Ok(())
}

/// Reads a stage's status, if a row exists.
pub fn get_stage(conn: &Connection, stage: Stage) -> Result<Option<StageStatus>, StoreError> {
    let label: Option<String> = conn
        .query_row(
            "SELECT status FROM stage_state WHERE stage = ?1",
            params![stage.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    match label {
        Some(value) => Ok(Some(StageStatus::from_db(&value)?)),
        None => Ok(None),
    }
}
