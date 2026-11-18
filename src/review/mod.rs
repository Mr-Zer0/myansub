//! The review gates: the generic editor flow plus the analysis-specific review
//! (`docs/ARCHITECTURE.md` §3, `docs/REQUIREMENTS.md` FR-4).
//!
//! `review` sits at the top of the dependency graph alongside `pipeline` and may
//! depend on `llm`, `quality`, `store`, `formats`, and `domain`; nothing depends on
//! it (`docs/ARCHITECTURE.md` §3). [`editor`] is the content-agnostic export →
//! `$EDITOR` → validate → import flow, reused by [`analysis`] (M5-B) and later by
//! translation review (M7). All review failures are [`ReviewError`].

pub mod analysis;
pub mod editor;
pub mod error;
pub mod translation;

pub use editor::{EditorLauncher, EnvEditorLauncher};
pub use error::{ReviewError, Violation};
