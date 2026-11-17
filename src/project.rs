//! Project resolution (`docs/CLI.md` §2).
//!
//! There are no job IDs: the project directory *is* the job. [`resolve_project`]
//! turns the global `--project` flag, an optional source `FILE` (for `init` and
//! `run FILE`), and the current directory into a single [`ProjectLocation`].
//!
//! This module depends on nothing else in the crate (only `std`), so it can be
//! used by both `main`/`cli` and `pipeline` without introducing a cycle
//! (`docs/ARCHITECTURE.md` §3). Resolution failures are reported with
//! [`ProjectError`], which maps to exit code 3 (`docs/CLI.md` §7).

use std::path::{Path, PathBuf};

/// The file name of the SQLite database inside a project directory.
pub const PROJECT_DB_FILE: &str = "project.db";

/// The suffix appended to a source file name to form its project directory,
/// e.g. `movie.ja.srt` → `movie.ja.srt.myansub` (`docs/CLI.md` §2).
pub const PROJECT_DIR_SUFFIX: &str = ".myansub";

/// A resolved project location: the project directory and whether it already
/// holds a `project.db` (i.e. has been initialized).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectLocation {
    /// The project directory.
    pub dir: PathBuf,
    /// Whether `<dir>/project.db` already exists.
    pub exists: bool,
}

impl ProjectLocation {
    /// The path of the `project.db` inside this project directory.
    pub fn db_path(&self) -> PathBuf {
        project_db_path(&self.dir)
    }
}

/// Errors raised while resolving a project directory (`docs/CLI.md` §2).
///
/// [`ProjectError::Ambiguous`] and [`ProjectError::NotFound`] are preconditions
/// that map to exit code 3; [`ProjectError::Io`] (an unreadable current
/// directory) is a generic error (exit 1). The exit-code mapping lives in
/// `crate::error`.
#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    /// More than one `*.myansub/` project exists in the current directory and no
    /// `--project` or source `FILE` disambiguated it.
    #[error(
        "ambiguous project in `{}`: multiple projects found ({}); pass --project DIR to choose one",
        dir.display(),
        candidates.join(", ")
    )]
    Ambiguous {
        dir: PathBuf,
        candidates: Vec<String>,
    },

    /// No project could be located: the current directory has no `project.db`
    /// and contains no `*.myansub/` project.
    #[error(
        "no project found in `{}`; run `myansub init <FILE> --from <LANG>` or pass --project DIR",
        dir.display()
    )]
    NotFound { dir: PathBuf },

    /// The current directory could not be read while scanning for projects.
    #[error("cannot read directory `{}`: {source}", dir.display())]
    Io {
        dir: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// The `project.db` path inside `dir`.
pub fn project_db_path(dir: &Path) -> PathBuf {
    dir.join(PROJECT_DB_FILE)
}

/// Derives the `<FILE>.myansub/` project directory for a source file by appending
/// [`PROJECT_DIR_SUFFIX`] to the whole file name, next to the source
/// (`docs/CLI.md` §2). Falls back to the suffix alone if `source` has no file
/// name component.
pub fn project_dir_for_source(source: &Path) -> PathBuf {
    match source.file_name() {
        Some(name) => {
            let mut dir_name = name.to_os_string();
            dir_name.push(PROJECT_DIR_SUFFIX);
            match source.parent() {
                Some(parent) => parent.join(dir_name),
                None => PathBuf::from(dir_name),
            }
        }
        None => PathBuf::from(PROJECT_DIR_SUFFIX),
    }
}

/// Resolves the project directory per `docs/CLI.md` §2.
///
/// 1. `project_flag` (`--project DIR`) → use `DIR`.
/// 2. `source_file` present (`init`, `run FILE`) → `<FILE>.myansub/` next to it.
/// 3. Otherwise → `cwd` if it holds `project.db`; else the single `*.myansub/`
///    project in `cwd`; else an error listing the candidates.
pub fn resolve_project(
    project_flag: Option<&Path>,
    source_file: Option<&Path>,
    cwd: &Path,
) -> Result<ProjectLocation, ProjectError> {
    if let Some(dir) = project_flag {
        return Ok(location(dir.to_path_buf()));
    }
    if let Some(source) = source_file {
        return Ok(location(project_dir_for_source(source)));
    }
    resolve_from_cwd(cwd)
}

/// Case 3 of `docs/CLI.md` §2: resolve from the current directory alone.
fn resolve_from_cwd(cwd: &Path) -> Result<ProjectLocation, ProjectError> {
    if project_db_path(cwd).is_file() {
        return Ok(ProjectLocation {
            dir: cwd.to_path_buf(),
            exists: true,
        });
    }

    let mut candidates = myansub_children(cwd)?;
    candidates.sort();
    match candidates.len() {
        0 => Err(ProjectError::NotFound {
            dir: cwd.to_path_buf(),
        }),
        1 => Ok(location(candidates.into_iter().next().unwrap_or_default())),
        _ => Err(ProjectError::Ambiguous {
            dir: cwd.to_path_buf(),
            candidates: candidates
                .iter()
                .filter_map(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .collect(),
        }),
    }
}

/// The `*.myansub/` sub-directories of `dir`.
fn myansub_children(dir: &Path) -> Result<Vec<PathBuf>, ProjectError> {
    let entries = std::fs::read_dir(dir).map_err(|source| ProjectError::Io {
        dir: dir.to_path_buf(),
        source,
    })?;
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| ProjectError::Io {
            dir: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.is_dir()
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(PROJECT_DIR_SUFFIX))
        {
            found.push(path);
        }
    }
    Ok(found)
}

/// Builds a [`ProjectLocation`] for `dir`, probing whether its `project.db` exists.
fn location(dir: PathBuf) -> ProjectLocation {
    let exists = project_db_path(&dir).is_file();
    ProjectLocation { dir, exists }
}
