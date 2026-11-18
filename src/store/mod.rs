//! Persistence layer: SQLite connection, pragmas, async bridge, versioned
//! migrations, and the atomic file-write helper.
//!
//! `project.db` is the single source of truth (`docs/ARCHITECTURE.md` §1). All
//! persisted state changes go through a single [`rusqlite::Connection`] held
//! behind `Arc<Mutex<_>>` ([`db::Store`]); callers run short closures on the
//! blocking pool via [`db::Store::call`] (`docs/ARCHITECTURE.md` §7).
//!
//! Schema migrations are versioned by `PRAGMA user_version` and each runs inside
//! one transaction ([`migrations`], `docs/ARCHITECTURE.md` §6). The
//! [`atomic`] module provides the crash-safe file write used for exports, review
//! files, and outputs.

pub mod atomic;
pub mod db;
pub mod migrations;
pub mod repo_analysis;
pub mod repo_cues;
pub mod repo_llm;
pub mod repo_project;
pub mod repo_translations;
pub mod repo_units;

pub use atomic::{StagedWrite, atomic_write};
pub use db::{Store, StoreError};
pub use repo_analysis::{
    AnalysisOrigin, AnalysisVersion, CurrentAnalysis, approve_current_analysis, current_analysis,
    current_analysis_approved, get_analysis_version, insert_analysis_version,
    insert_analysis_version_in_tx, list_analysis_versions,
};
pub use repo_cues::{insert_cues, insert_cues_in_tx, load_cues};
pub use repo_llm::{CacheRow, ModelUsage, usage_by_model};
pub use repo_project::{
    Project, Stage, StageStatus, clear_all_data, get_output_prefs, get_stage_origin,
    set_output_prefs, set_stage_with_origin,
};
pub use repo_translations::{
    NewTranslation, TranslationHistoryRow, TranslationOrigin, TranslationRow,
    apply_user_edit_in_tx, cue_id_by_idx, get_translation, list_translations, set_locked_in_tx,
    upsert_translation, upsert_translation_in_tx,
};
pub use repo_units::{AnalysisChunk, NewTranslationBatch, TranslationBatch, UnitStatus};
