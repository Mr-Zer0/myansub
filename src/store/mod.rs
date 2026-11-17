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
pub mod repo_cues;
pub mod repo_project;
pub mod repo_units;

pub use atomic::{StagedWrite, atomic_write};
pub use db::{Store, StoreError};
pub use repo_cues::{insert_cues, load_cues};
pub use repo_project::{Project, Stage, StageStatus};
pub use repo_units::{AnalysisChunk, NewTranslationBatch, TranslationBatch, UnitStatus};
