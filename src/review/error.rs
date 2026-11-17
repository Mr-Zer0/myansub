//! The review error enum (`docs/ARCHITECTURE.md` §8).
//!
//! [`ReviewError`] covers the analysis-review gate (M5-B) and is reused by the
//! translation-review gate (M7). It wraps the lower-layer [`StoreError`] with
//! `#[from]` and adds the review's own failures: a missing current analysis (a
//! precondition, exit 3), a parse failure, a set of validation violations each
//! carrying its field *location* (M5-AC3), editor-launch failures, and I/O. Its
//! exit-code mapping lives in `crate::error` alongside the other module enums
//! (`docs/CLI.md` §7).

use std::fmt;
use std::path::PathBuf;

use crate::store::StoreError;

/// A single validation violation, naming the field *location* it occurred at
/// (M5-AC3, `docs/DATA_MODEL.md` §3). `location` is the logical path into the
/// document (e.g. `characters[3].gender`, `address_matrix[1].from`,
/// `scenes[2].cue_range`), not a byte offset, so the message tells the user which
/// field to fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The logical path to the offending field.
    pub location: String,
    /// What is wrong there.
    pub message: String,
}

impl Violation {
    /// Builds a violation at `location` with `message`.
    pub fn new(location: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            location: location.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.location, self.message)
    }
}

/// Renders a list of violations one per line, each prefixed with `  - ` so the
/// error message reads as a checklist of fields to fix.
fn format_violations(violations: &[Violation]) -> String {
    violations
        .iter()
        .map(|v| format!("  - {v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Errors raised by the review gates (`docs/ARCHITECTURE.md` §8).
#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    /// There is no current analysis version to show, edit, export, import, or
    /// approve (`docs/CLI.md` §7). A precondition failure; maps to exit code 3.
    #[error("no current analysis; run `myansub analyze` first")]
    NoCurrentAnalysis,

    /// The review file is not valid JSON (`docs/DATA_MODEL.md` §6). Maps to exit
    /// code 1.
    #[error("the analysis file is not valid JSON: {0}")]
    Parse(String),

    /// The review file parsed but violates one or more import rules, each named
    /// with its field location (M5-AC3). Maps to exit code 1; nothing is applied
    /// (`docs/ARCHITECTURE.md` §6 rule 7).
    #[error("the analysis file is not valid:\n{}", format_violations(.0))]
    Invalid(Vec<Violation>),

    /// A stored analysis document is not valid JSON — a broken invariant, since
    /// only validated documents are ever persisted. Maps to exit code 1.
    #[error("the stored analysis document is corrupt: {0}")]
    Malformed(String),

    /// No editor is configured: neither `$EDITOR` nor `$VISUAL` is set
    /// (`docs/CLI.md` §6). Maps to exit code 1.
    #[error("no editor configured; set $EDITOR (e.g. `export EDITOR=vi`) to edit the analysis")]
    EditorNotConfigured,

    /// The editor process exited unsuccessfully; the analysis was not changed.
    /// Maps to exit code 1.
    #[error("the editor exited with a failure; the analysis was not changed")]
    EditorFailed,

    /// A filesystem operation failed (reading an import file, writing a scratch
    /// or export file). Maps to exit code 1.
    #[error("i/o error on `{}`: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A persistence operation failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}
