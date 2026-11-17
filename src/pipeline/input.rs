//! The input stage and the `init` command (`docs/REQUIREMENTS.md` FR-1/FR-2,
//! `docs/CLI.md` §6).
//!
//! The input stage reads the source bytes, records their SHA-256, decodes them
//! to UTF-8 (`super::super::formats::encoding`), detects the format from content,
//! parses and masks it (`super::super::formats::codec`), and stores the project
//! row, the cues, and the `input` stage state — the clear and the re-insert in
//! one transaction so a crash is all-or-nothing (`docs/ARCHITECTURE.md` §6).
//!
//! [`run_init`] is the `init` command handler: it creates the project directory,
//! opens the store, and decides between a fresh init, a no-op (already
//! initialized), a refusal (changed source without `--force`), or a destructive
//! re-init. The TTY/`--yes` branch is factored into the pure [`decide_reinit`] so
//! it is testable without a terminal (`docs/CLI.md` §5).

use std::path::Path;

use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::domain::Document;
use crate::formats::{codec, encoding};
use crate::store::repo_project::{get_project, insert_project, set_stage};
use crate::store::{
    Project, Stage, StageStatus, Store, StoreError, clear_all_data, insert_cues_in_tx,
};

use super::error::PipelineError;

/// Marker prefixing the stored source hash, matching the review-file format
/// (`docs/DATA_MODEL.md` §6 uses `"sha256:…"`).
const HASH_PREFIX: &str = "sha256:";

/// What the input stage observed while storing the source.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputOutcome {
    /// Indices of cues whose decoded text contains U+FFFD replacement characters
    /// (`docs/REQUIREMENTS.md` FR-1). Non-empty means the decode was lossy.
    pub replacement_cues: Vec<u32>,
}

/// Runs the input stage: read, hash, decode, detect, parse, mask, and store.
///
/// `ext_hint` is the source file extension (without the dot), used only as a
/// format tie-breaker (`docs/REQUIREMENTS.md` FR-1). On success the project row,
/// all cues, and `stage_state(input) = completed` are committed in one
/// transaction that first clears any prior data, so re-running (a `--force`
/// re-init) is atomic and idempotent.
pub async fn run_input(
    store: &Store,
    source_path: &Path,
    from_lang: &str,
    encoding_override: Option<&str>,
    ext_hint: Option<&str>,
) -> Result<InputOutcome, PipelineError> {
    run_input_with_fault(
        store,
        source_path,
        from_lang,
        encoding_override,
        ext_hint,
        |_| Ok(()),
    )
    .await
}

/// [`run_input`] with a fault-injection seam for crash-safety tests
/// (`docs/TESTING.md` §4).
///
/// `fault` runs inside the input-stage transaction immediately after the prior
/// project data is cleared and before any new rows are inserted. Returning an
/// error from `fault` aborts the transaction, which — per `docs/ARCHITECTURE.md`
/// §6 rule 1 — must roll the clear back and leave the previous project row and
/// cues fully intact (never empty or partial). Production callers use
/// [`run_input`], which injects a no-op.
pub async fn run_input_with_fault<H>(
    store: &Store,
    source_path: &Path,
    from_lang: &str,
    encoding_override: Option<&str>,
    ext_hint: Option<&str>,
    fault: H,
) -> Result<InputOutcome, PipelineError>
where
    H: FnOnce(&Connection) -> Result<(), StoreError> + Send + 'static,
{
    let bytes = read_source(source_path)?;
    let source_hash = hash_source(&bytes);

    let decoded = encoding::decode(&bytes, encoding_override, Some(from_lang))?;

    let format = codec::detect_format(&decoded.text, ext_hint)
        .ok_or(crate::formats::codec::FormatError::UnknownFormat)?;
    let document = codec::parse_document(&decoded.text, format)?;

    let replacement_cues = if decoded.had_replacement {
        let affected = cues_with_replacement(&document);
        tracing::warn!(
            count = affected.len(),
            "source decoding produced replacement characters; affected cue indices: {affected:?}"
        );
        affected
    } else {
        Vec::new()
    };

    let format_meta = serialize_format_meta(&document)?;
    let project = Project {
        source_path: source_path.to_string_lossy().into_owned(),
        source_hash,
        source_format: format.label().to_string(),
        source_encoding: decoded.encoding,
        encoding_confidence: decoded.confidence.map(f64::from),
        source_lang: from_lang.to_string(),
        target_lang: "my".to_string(),
        format_meta,
    };
    let cues = document.cues;

    // One transaction: wipe any prior data, then write project + cues + stage so
    // a crash leaves the database either fully old or fully new (§6 rule 1). The
    // fault seam fires after the clear and before any insert, so an error there
    // must roll the clear back, proving the destructive re-init is atomic.
    store
        .call(move |conn| {
            let tx = conn.transaction()?;
            clear_all_data(&tx)?;
            fault(&tx)?;
            insert_project(&tx, &project)?;
            insert_cues_in_tx(&tx, &cues)?;
            set_stage(&tx, Stage::Input, StageStatus::Completed)?;
            tx.commit()?;
            Ok(())
        })
        .await?;

    Ok(InputOutcome { replacement_cues })
}

/// Verifies the source has not changed since `init` (`docs/ARCHITECTURE.md` §6
/// rule 5). Returns [`PipelineError::SourceChanged`] (exit 3) when the current
/// SHA-256 differs from the stored one.
///
/// Callers that read cues run this first. If no project row exists there is no
/// recorded hash to compare against; that is a caller precondition, so this
/// treats it as "unchanged" and lets the caller surface the missing project.
pub async fn check_source_unchanged(
    store: &Store,
    source_path: &Path,
) -> Result<(), PipelineError> {
    let bytes = read_source(source_path)?;
    let current = hash_source(&bytes);
    let stored = store
        .call(|conn| get_project(conn))
        .await?
        .map(|p| p.source_hash);
    match stored {
        Some(hash) if hash != current => Err(PipelineError::SourceChanged {
            path: source_path.to_path_buf(),
        }),
        _ => Ok(()),
    }
}

/// The result of the `init` command (`docs/CLI.md` §6). All variants are
/// successes (exit 0); refusals are returned as [`PipelineError`] instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitOutcome {
    /// A fresh project was created.
    Initialized(InputOutcome),
    /// The source changed and the project was re-initialized (`--force`).
    ReInitialized(InputOutcome),
    /// The project already exists and the source is unchanged (nothing to do).
    AlreadyInitialized,
    /// A destructive re-init was offered interactively and the user declined.
    Cancelled,
}

/// The decision taken when `init` runs against an existing project
/// (`docs/CLI.md` §5/§6). Pure, so it is testable without a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReinitAction {
    /// The source is unchanged: nothing to do (exit 0).
    AlreadyInitialized,
    /// The source changed but `--force` was not given: refuse (exit 3).
    RefuseNeedsForce,
    /// A destructive re-init is required but stdin is not a TTY and `--yes` was
    /// not given: refuse, naming `--yes` (exit 3).
    ConfirmationUnavailable,
    /// A destructive re-init is required and stdin is a TTY: prompt the user.
    Confirm,
    /// A destructive re-init is required and `--yes` was given: proceed.
    Proceed,
}

/// Decides how `init` should treat an existing project (`docs/CLI.md` §5/§6).
pub fn decide_reinit(changed: bool, force: bool, interactive: bool, yes: bool) -> ReinitAction {
    if !changed {
        return ReinitAction::AlreadyInitialized;
    }
    if !force {
        return ReinitAction::RefuseNeedsForce;
    }
    if yes {
        return ReinitAction::Proceed;
    }
    if interactive {
        ReinitAction::Confirm
    } else {
        ReinitAction::ConfirmationUnavailable
    }
}

/// The `init` command handler (`docs/CLI.md` §6).
///
/// `project_dir` is the resolved project directory; `interactive` is whether
/// stdin is a TTY (pass `std::io::stdin().is_terminal()`), and `yes` is the
/// global `--yes` flag. Output-preference flags (`--output-format`, `-o`) are
/// accepted by the CLI but not persisted yet (deferred to M7).
pub async fn run_init(
    project_dir: &Path,
    source_path: &Path,
    from_lang: &str,
    encoding_override: Option<&str>,
    force: bool,
    interactive: bool,
    yes: bool,
) -> Result<InitOutcome, PipelineError> {
    std::fs::create_dir_all(project_dir).map_err(|source| PipelineError::Io {
        path: project_dir.to_path_buf(),
        source,
    })?;

    let store = Store::open(&crate::project::project_db_path(project_dir))?;
    let ext_hint = extension_hint(source_path);

    let existing = store.call(|conn| get_project(conn)).await?;
    let Some(existing) = existing else {
        // Fresh init: no project row yet.
        let outcome =
            run_input(&store, source_path, from_lang, encoding_override, ext_hint).await?;
        return Ok(InitOutcome::Initialized(outcome));
    };

    let bytes = read_source(source_path)?;
    let changed = hash_source(&bytes) != existing.source_hash;

    match decide_reinit(changed, force, interactive, yes) {
        ReinitAction::AlreadyInitialized => Ok(InitOutcome::AlreadyInitialized),
        ReinitAction::RefuseNeedsForce => Err(PipelineError::SourceChanged {
            path: source_path.to_path_buf(),
        }),
        ReinitAction::ConfirmationUnavailable => Err(PipelineError::ConfirmationNeeded {
            flag: "--yes".to_string(),
        }),
        ReinitAction::Confirm => {
            // The prompt does blocking stdin I/O, so it runs on the blocking pool
            // rather than stalling an async runtime worker. A join failure (only a
            // panic or shutdown) is treated as a decline.
            let prompt_path = source_path.to_path_buf();
            let confirmed =
                tokio::task::spawn_blocking(move || confirm_destructive_reinit(&prompt_path))
                    .await
                    .unwrap_or(false);
            if confirmed {
                let outcome =
                    run_input(&store, source_path, from_lang, encoding_override, ext_hint).await?;
                Ok(InitOutcome::ReInitialized(outcome))
            } else {
                Ok(InitOutcome::Cancelled)
            }
        }
        ReinitAction::Proceed => {
            let outcome =
                run_input(&store, source_path, from_lang, encoding_override, ext_hint).await?;
            Ok(InitOutcome::ReInitialized(outcome))
        }
    }
}

/// Prompts on an interactive terminal before a destructive re-init. Accepts
/// `y`/`yes` (case-insensitive); anything else (including EOF) declines.
fn confirm_destructive_reinit(source_path: &Path) -> bool {
    eprint!(
        "Re-initializing will destroy existing project data for `{}`. Continue? [y/N] ",
        source_path.display()
    );
    use std::io::Write;
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    match std::io::stdin().read_line(&mut answer) {
        Ok(0) | Err(_) => false,
        Ok(_) => matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
    }
}

/// Reads the raw source bytes, mapping I/O failures with path context.
fn read_source(source_path: &Path) -> Result<Vec<u8>, PipelineError> {
    std::fs::read(source_path).map_err(|source| PipelineError::Io {
        path: source_path.to_path_buf(),
        source,
    })
}

/// Computes `sha256:<hex>` over the raw source bytes (`docs/REQUIREMENTS.md`
/// FR-1; `docs/DATA_MODEL.md` §6 format).
fn hash_source(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(HASH_PREFIX.len() + digest.len() * 2);
    out.push_str(HASH_PREFIX);
    for byte in digest {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

/// The source file extension (without the leading dot), if any.
fn extension_hint(source_path: &Path) -> Option<&str> {
    source_path.extension().and_then(|e| e.to_str())
}

/// Indices of cues whose decoded text carries a U+FFFD replacement character.
fn cues_with_replacement(document: &Document) -> Vec<u32> {
    document
        .cues
        .iter()
        .filter(|cue| cue.text_raw.contains('\u{FFFD}') || cue.text_masked.contains('\u{FFFD}'))
        .map(|cue| cue.idx)
        .collect()
}

/// Serializes the document's `format_meta` for the project row, if present.
fn serialize_format_meta(document: &Document) -> Result<Option<String>, PipelineError> {
    match &document.format_meta {
        Some(meta) => {
            let json = serde_json::to_string(meta).map_err(|err| {
                crate::store::StoreError::Unexpected(format!("serialize format_meta: {err}"))
            })?;
            Ok(Some(json))
        }
        None => Ok(None),
    }
}
