//! The pipeline error enum (`docs/ARCHITECTURE.md` §8).
//!
//! [`PipelineError`] wraps the lower-layer errors (`FormatError`,
//! `EncodingError`, `StoreError`, and project resolution) and adds the pipeline's
//! own precondition failures: a changed source, an ambiguous project, and a
//! destructive confirmation that cannot be obtained without a TTY. Its exit-code
//! mapping lives in `crate::error` alongside the other module enums
//! (`docs/CLI.md` §7).

use std::path::PathBuf;

use crate::formats::codec::FormatError;
use crate::formats::encoding::EncodingError;
use crate::project::ProjectError;
use crate::store::StoreError;

/// Errors raised by the pipeline (input stage and the `init` command for M4-B).
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    /// The source file's SHA-256 differs from the one recorded at `init`
    /// (`docs/ARCHITECTURE.md` §6 rule 5). Maps to exit code 3.
    #[error(
        "source file `{}` changed since init; re-run with `init --force` to re-initialize",
        path.display()
    )]
    SourceChanged { path: PathBuf },

    /// A destructive action needs confirmation, but stdin is not a TTY and the
    /// confirmation flag was not given (`docs/CLI.md` §5). Maps to exit code 3.
    #[error("confirmation required: stdin is not a TTY; pass `{flag}` to proceed")]
    ConfirmationNeeded { flag: String },

    /// Project resolution failed (`docs/CLI.md` §2). Maps to exit code 3 (except
    /// an unreadable directory, which is a generic error).
    #[error(transparent)]
    ProjectResolution(#[from] ProjectError),

    /// A subtitle could not be detected or parsed.
    #[error(transparent)]
    Format(#[from] FormatError),

    /// The source bytes could not be decoded (e.g. an unknown `--encoding` label).
    #[error(transparent)]
    Encoding(#[from] EncodingError),

    /// A persistence operation failed.
    #[error(transparent)]
    Store(#[from] StoreError),

    /// A filesystem operation failed (reading the source, creating the project
    /// directory).
    #[error("i/o error on `{}`: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}
