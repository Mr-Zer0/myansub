//! The generic review-editor flow: export → `$EDITOR` → validate → import
//! (`docs/ARCHITECTURE.md` §3). This module is content-agnostic — it knows how to
//! write an export, launch an editor over a scratch file, and read a file back —
//! so both `review::analysis` (M5-B) and `review::translation` (M7) reuse it by
//! supplying their own serialize/validate/apply steps.
//!
//! The editor launch is behind the [`EditorLauncher`] trait so tests can simulate
//! "the user edited the file" without a real editor and without touching `$EDITOR`
//! (the key to offline testing, `docs/TESTING.md` §1). The production launcher
//! [`EnvEditorLauncher`] reads `$EDITOR` (then `$VISUAL`), spawns it, and waits.

use std::path::{Path, PathBuf};

use crate::store::atomic_write;

use super::error::ReviewError;

/// Launches an editor over a file and waits for it to exit (`docs/CLI.md` §6).
///
/// Implemented by [`EnvEditorLauncher`] in production and by a fake in tests, so
/// the edit flow is exercised without spawning a real editor.
pub trait EditorLauncher {
    /// Opens `path` in the editor and blocks until the editor exits. Returns
    /// [`ReviewError::EditorFailed`] on a non-zero exit.
    fn launch(&self, path: &Path) -> Result<(), ReviewError>;
}

/// The production launcher: the editor from `$EDITOR` (falling back to `$VISUAL`),
/// spawned over the file and waited on (`docs/CLI.md` §6).
#[derive(Debug, Clone, Copy, Default)]
pub struct EnvEditorLauncher;

impl EditorLauncher for EnvEditorLauncher {
    fn launch(&self, path: &Path) -> Result<(), ReviewError> {
        let editor = resolve_editor(|key| std::env::var(key).ok())?;
        // The editor setting may carry arguments, e.g. `code --wait`; split on
        // whitespace so the first token is the program and the rest are args.
        let mut parts = editor.split_whitespace();
        let program = parts.next().ok_or(ReviewError::EditorNotConfigured)?;
        let status = std::process::Command::new(program)
            .args(parts)
            .arg(path)
            .status()
            .map_err(|source| ReviewError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        if status.success() {
            Ok(())
        } else {
            Err(ReviewError::EditorFailed)
        }
    }
}

/// Resolves the configured editor using `lookup` to read environment variables,
/// preferring `$EDITOR` over `$VISUAL` (`docs/CLI.md` §6). A missing or blank
/// value yields [`ReviewError::EditorNotConfigured`].
///
/// Taking `lookup` as a parameter keeps this pure and unit-testable without
/// mutating process-global environment state.
pub fn resolve_editor<F>(lookup: F) -> Result<String, ReviewError>
where
    F: Fn(&str) -> Option<String>,
{
    for key in ["EDITOR", "VISUAL"] {
        if let Some(value) = lookup(key)
            && !value.trim().is_empty()
        {
            return Ok(value);
        }
    }
    Err(ReviewError::EditorNotConfigured)
}

/// Atomically writes `bytes` to `path` (`docs/ARCHITECTURE.md` §6 rule 4). The
/// export (and translation-review file in M7) is written via the crash-safe
/// tmp-write-then-rename helper so a crash never leaves a partial file.
pub fn export_to_path(path: &Path, bytes: &[u8]) -> Result<(), ReviewError> {
    atomic_write(path, bytes).map_err(ReviewError::Store)
}

/// Reads a review file to a string for import.
pub fn read_file(path: &Path) -> Result<String, ReviewError> {
    std::fs::read_to_string(path).map_err(|source| ReviewError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Runs the interactive edit round-trip: writes `initial` to a scratch file in
/// `work_dir`, launches the editor over it, and returns the edited contents. The
/// scratch file is removed before returning, whatever the outcome.
///
/// The scratch file lives in `work_dir` (the project directory) so it shares the
/// filesystem with the project; its name includes `flow` (e.g. `analysis`,
/// `translation`) so the two review flows never collide on one scratch path. This
/// does not itself apply the edit: the caller feeds the returned string to its
/// validate/apply step, keeping this function content-agnostic.
pub fn capture_edit<L: EditorLauncher>(
    launcher: &L,
    work_dir: &Path,
    flow: &str,
    initial: &[u8],
) -> Result<String, ReviewError> {
    let scratch = scratch_path(work_dir, flow);
    std::fs::write(&scratch, initial).map_err(|source| ReviewError::Io {
        path: scratch.clone(),
        source,
    })?;

    let result = launcher.launch(&scratch).and_then(|()| read_file(&scratch));
    // Best-effort cleanup: a leftover scratch file is harmless and the next edit
    // overwrites it, so a failed removal does not change the outcome.
    let _ = std::fs::remove_file(&scratch);
    result
}

/// The scratch path used for an in-editor edit, a hidden `flow`-specific sibling in
/// `work_dir`.
fn scratch_path(work_dir: &Path, flow: &str) -> PathBuf {
    work_dir.join(format!(".myansub-{flow}-edit.json"))
}
