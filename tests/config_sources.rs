//! Assembling [`ConfigSources`] from the real filesystem (`docs/CONFIG.md` §1):
//! global and project config files are included only when they exist; `--config`
//! is always included (a missing one is a hard error at load). Resolves M0
//! reliability finding #1.

use myansub::config::{CliOverrides, sources_from_paths};
use std::io::Write;
use tempfile::{NamedTempFile, tempdir};

// AC: CONFIG.md §1 — a missing global/project config file is not an error.
#[test]
fn missing_global_and_project_files_are_not_an_error() {
    let tmp = tempdir().expect("tempdir");
    let global = tmp.path().join("does-not-exist-global.toml");
    let project = tmp.path().join("does-not-exist-project.toml");

    let sources = sources_from_paths(
        Some(global),
        Some(project),
        None,
        Vec::new(),
        CliOverrides::default(),
    );

    // Non-existent file layers are dropped, so load falls back to defaults.
    assert!(sources.global_file.is_none());
    assert!(sources.project_file.is_none());
    let config = myansub::config::load(&sources).expect("missing files load to defaults");
    assert_eq!(config.translation.batch_size, 80);
}

#[test]
fn existing_file_layers_are_kept() {
    let mut global = NamedTempFile::new().expect("temp global");
    global
        .write_all(b"[translation]\nbatch_size = 50\n")
        .expect("write global");

    let sources = sources_from_paths(
        Some(global.path().to_path_buf()),
        None,
        None,
        Vec::new(),
        CliOverrides::default(),
    );

    assert_eq!(sources.global_file.as_deref(), Some(global.path()));
    let config = myansub::config::load(&sources).expect("load with existing global");
    assert_eq!(config.translation.batch_size, 50);
}

// A `--config` file is passed through unconditionally; a missing one is a hard
// error at load (docs/CONFIG.md §1), unlike the global/project layers.
#[test]
fn missing_config_flag_is_a_hard_error() {
    let tmp = tempdir().expect("tempdir");
    let missing = tmp.path().join("missing.toml");

    let sources = sources_from_paths(
        None,
        None,
        Some(missing.clone()),
        Vec::new(),
        CliOverrides::default(),
    );

    assert_eq!(sources.config_file.as_deref(), Some(missing.as_path()));
    let err = myansub::config::load(&sources).expect_err("missing --config is an error");
    assert!(err.to_string().contains("missing.toml"), "got: {err}");
}
