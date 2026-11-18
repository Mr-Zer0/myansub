//! Crash-safe atomic file writes (`docs/ARCHITECTURE.md` §6, rule 4).
//!
//! An atomic write never leaves a partial target file: bytes are written to a
//! sibling `<name>.tmp`, flushed with `fsync`, then renamed over the target
//! (`rename` is atomic on the same filesystem), after which the containing
//! directory is `fsync`ed where the platform supports it. If the process dies
//! before the rename, the target is untouched and only a harmless `.tmp` may
//! remain, which the next write overwrites.
//!
//! [`atomic_write`] is the one-shot helper. [`StagedWrite`] exposes the same two
//! steps separately ([`StagedWrite::create`] then [`StagedWrite::commit`]); this
//! is the failure-injection seam (M2-AC4): a test can stage a write and then not
//! commit, which is exactly what a crash between tmp-write and rename looks like.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use super::db::StoreError;

/// Atomically writes `bytes` to `path`, replacing any existing file.
///
/// On success the target holds exactly `bytes`. On failure the target is either
/// absent (if it did not exist) or unchanged (never partial).
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let staged = StagedWrite::create(path, bytes)?;
    staged.commit()
}

/// A pending atomic write: the temporary file is written and `fsync`ed, but the
/// rename over the target has not happened yet.
///
/// Dropping a `StagedWrite` without calling [`StagedWrite::commit`] leaves the
/// target untouched (this is the M2-AC4 seam). The temporary file is left on
/// disk, mirroring a real crash; the next write to the same target overwrites it.
#[must_use = "a StagedWrite does nothing until committed"]
pub struct StagedWrite {
    target: PathBuf,
    tmp: PathBuf,
}

impl StagedWrite {
    /// Writes `bytes` to the temporary file and `fsync`s it. Does not touch the
    /// target.
    pub fn create(target: &Path, bytes: &[u8]) -> Result<StagedWrite, StoreError> {
        let tmp = tmp_path(target)?;
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(StagedWrite {
            target: target.to_path_buf(),
            tmp,
        })
    }

    /// The path of the staged temporary file (for inspection in tests).
    pub fn tmp_path(&self) -> &Path {
        &self.tmp
    }

    /// Renames the temporary file over the target and `fsync`s the directory
    /// where supported.
    pub fn commit(self) -> Result<(), StoreError> {
        fs::rename(&self.tmp, &self.target)?;
        sync_parent_dir(&self.target);
        Ok(())
    }
}

/// Builds the sibling temporary path `<name>.tmp` next to `target`.
///
/// Returns [`StoreError::InvalidPath`] when `target` has no final file-name
/// component (e.g. it ends in `/` or `..`), rather than silently writing a
/// `.tmp` dotfile into the parent directory.
fn tmp_path(target: &Path) -> Result<PathBuf, StoreError> {
    let Some(file_name) = target.file_name() else {
        return Err(StoreError::InvalidPath {
            path: target.display().to_string(),
        });
    };
    let mut name = file_name.to_os_string();
    name.push(".tmp");
    Ok(target.with_file_name(name))
}

/// Best-effort `fsync` of the directory containing `target`.
///
/// Durability of the rename across a crash requires syncing the directory, but
/// directory `fsync` is not portable (notably Windows does not support opening a
/// directory as a file), so failures here are ignored (NFR-6). The rename itself
/// has already succeeded, so the target is in place regardless.
fn sync_parent_dir(target: &Path) {
    if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty())
        && let Ok(dir) = File::open(parent)
    {
        let _ = dir.sync_all();
    }
}
