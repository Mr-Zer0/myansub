//! The pipeline error enum (`docs/ARCHITECTURE.md` §8).
//!
//! [`PipelineError`] wraps the lower-layer errors (`FormatError`,
//! `EncodingError`, `StoreError`, and project resolution) and adds the pipeline's
//! own precondition failures: a changed source, an ambiguous project, and a
//! destructive confirmation that cannot be obtained without a TTY. Its exit-code
//! mapping lives in `crate::error` alongside the other module enums
//! (`docs/CLI.md` §7).

use std::path::PathBuf;

use crate::domain::SubtitleFormat;
use crate::formats::codec::FormatError;
use crate::formats::encoding::EncodingError;
use crate::llm::client::LlmError;
use crate::project::ProjectError;
use crate::store::StoreError;

/// Errors raised by the pipeline (input stage, the `init` command, and the
/// orchestrator's `run`/`status` for M4-C).
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    /// The source file's SHA-256 differs from the one recorded at `init`
    /// (`docs/ARCHITECTURE.md` §6 rule 5). Maps to exit code 3.
    #[error(
        "source file `{}` changed since init; re-run with `init --force` to re-initialize",
        path.display()
    )]
    SourceChanged { path: PathBuf },

    /// A stage cannot run because its precondition in the stage state machine is
    /// not met (`docs/ARCHITECTURE.md` §2). The message names the exact next
    /// action the user must take. Maps to exit code 3.
    #[error("{message}")]
    GateViolation { message: String },

    /// `run FILE --from <LANG>` was given a language that disagrees with the one
    /// stored at `init` (`docs/CLI.md` §2). Maps to exit code 3.
    #[error(
        "project was initialized with `--from {stored}`, but `--from {given}` was given; \
         omit --from to resume, or re-initialize with `init --force --from {given}`"
    )]
    FromDisagreement { stored: String, given: String },

    /// A command that needs an initialized project found none — the project
    /// directory has no project row, even if a `project.db` file is present
    /// (`docs/CLI.md` §2). Maps to exit code 3.
    #[error(
        "no initialized project in `{}`; run `myansub init <FILE> --from <LANG>` first",
        dir.display()
    )]
    Uninitialized { dir: PathBuf },

    /// `run` must create the project (no project row exists) but no source
    /// language was given (`docs/CLI.md` §2). Maps to exit code 3.
    #[error("creating a project requires a source language; pass `--from <LANG>`")]
    MissingFromLanguage,

    /// A destructive action needs confirmation, but stdin is not a TTY and the
    /// confirmation flag was not given (`docs/CLI.md` §5). Maps to exit code 3.
    #[error("confirmation required: stdin is not a TTY; pass `{flag}` to proceed")]
    ConfirmationNeeded { flag: String },

    /// The output stage refused because one or more translatable cues have no
    /// stored translation and `--allow-incomplete` was not given
    /// (`docs/REQUIREMENTS.md` FR-7). Maps to exit code 3 (precondition).
    #[error(
        "{} translatable cue(s) have no translation; run `myansub translate`, \
         or pass `--allow-incomplete` to export using the source text. Missing cue ids: {cue_ids:?}",
        cue_ids.len()
    )]
    IncompleteTranslation { cue_ids: Vec<u32> },

    /// `--output-format` and the extension of `--output` disagree (`docs/CLI.md`
    /// §3). Maps to exit code 2 (usage).
    #[error(
        "`--output-format {format}` disagrees with the `.{extension}` extension of `--output`; \
         remove one of them"
    )]
    OutputFormatExtensionMismatch {
        format: SubtitleFormat,
        extension: SubtitleFormat,
    },

    /// A real (non-dry-run) LLM command was invoked without a Gemini API key
    /// (`docs/LLM.md` §1). A clear, actionable message; maps to exit code 1 (a
    /// generic error, not a pipeline precondition).
    #[error("GEMINI_API_KEY is not set; export it (or add it to a `.env` file) to run analysis")]
    MissingApiKey,

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

    /// An LLM call failed terminally (after transport retries), e.g. a fatal
    /// request error, or the operation was cancelled during a call. A cancelled
    /// call maps to exit 130; every other terminal `LlmError` to exit 1. Content
    /// (validation) failures are *not* errors — they surface as a
    /// completed-with-failures outcome (exit 4), never this variant.
    #[error(transparent)]
    Llm(#[from] LlmError),

    /// A filesystem operation failed (reading the source, creating the project
    /// directory).
    #[error("i/o error on `{}`: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}
