// Tests for layered config resolution per docs/CONFIG.md §1.

use myansub::config::{CliOverrides, ConfigSources};
use std::io::Write;
use tempfile::NamedTempFile;

fn write_toml(contents: &str) -> NamedTempFile {
    let mut f = NamedTempFile::new().expect("create temp file");
    f.write_all(contents.as_bytes()).expect("write temp file");
    f
}

#[test]
fn defaults_match_config_example() {
    let config = myansub::config::load(&ConfigSources::default()).expect("defaults load");
    assert_eq!(config.translation.batch_size, 80);
    assert_eq!(config.translation.concurrency, 4);
    assert_eq!(config.llm.service_tier, "flex");
    assert_eq!(config.llm.models.analysis, "gemini-flash-latest");
    assert_eq!(config.cache.provider, "auto");
}

#[test]
fn project_file_overrides_global_file() {
    let global = write_toml("[translation]\nbatch_size = 50\nconcurrency = 2\n");
    let project = write_toml("[translation]\nbatch_size = 120\n");

    let sources = ConfigSources {
        global_file: Some(global.path().to_path_buf()),
        project_file: Some(project.path().to_path_buf()),
        ..Default::default()
    };
    let config = myansub::config::load(&sources).expect("load");

    // project wins for batch_size
    assert_eq!(config.translation.batch_size, 120);
    // global value for a key the project did not set survives the merge
    assert_eq!(config.translation.concurrency, 2);
}

// AC: M0-AC2
#[test]
fn env_overrides_project_file() {
    let project = write_toml("[llm]\nservice_tier = \"standard\"\n");

    let sources = ConfigSources {
        project_file: Some(project.path().to_path_buf()),
        env: vec![("MYANSUB_SERVICE_TIER".to_string(), "flex".to_string())],
        ..Default::default()
    };
    let config = myansub::config::load(&sources).expect("load");

    assert_eq!(config.llm.service_tier, "flex");
}

// AC: M13-AC1 — `MYANSUB_DEFAULT_PROVIDER` overrides the config file.
#[test]
fn env_overrides_default_provider() {
    let project = write_toml("[llm]\ndefault_provider = \"gemini\"\n");

    let sources = ConfigSources {
        project_file: Some(project.path().to_path_buf()),
        env: vec![(
            "MYANSUB_DEFAULT_PROVIDER".to_string(),
            "openrouter".to_string(),
        )],
        ..Default::default()
    };
    let config = myansub::config::load(&sources).expect("load");

    assert_eq!(config.llm.default_provider, "openrouter");
}

// AC: M0-AC2
#[test]
fn cli_flag_overrides_env() {
    let sources = ConfigSources {
        env: vec![("MYANSUB_CONCURRENCY".to_string(), "7".to_string())],
        overrides: CliOverrides {
            concurrency: Some(12),
            ..Default::default()
        },
        ..Default::default()
    };
    let config = myansub::config::load(&sources).expect("load");

    assert_eq!(config.translation.concurrency, 12);
}

#[test]
fn config_file_overrides_project_file() {
    let project = write_toml("[llm.models]\nanalysis = \"from-project\"\n");
    let extra = write_toml("[llm.models]\nanalysis = \"from-config-flag\"\n");

    let sources = ConfigSources {
        project_file: Some(project.path().to_path_buf()),
        config_file: Some(extra.path().to_path_buf()),
        ..Default::default()
    };
    let config = myansub::config::load(&sources).expect("load");

    assert_eq!(config.llm.models.analysis, "from-config-flag");
}

// AC: M0-AC2
#[test]
fn env_overrides_config_file() {
    let extra = write_toml("[llm]\nservice_tier = \"standard\"\n");

    let sources = ConfigSources {
        config_file: Some(extra.path().to_path_buf()),
        env: vec![("MYANSUB_SERVICE_TIER".to_string(), "flex".to_string())],
        ..Default::default()
    };
    let config = myansub::config::load(&sources).expect("load");

    assert_eq!(config.llm.service_tier, "flex");
}

#[test]
fn negative_env_concurrency_names_the_variable() {
    let sources = ConfigSources {
        env: vec![("MYANSUB_CONCURRENCY".to_string(), "-3".to_string())],
        ..Default::default()
    };
    let err = myansub::config::load(&sources).expect_err("negative concurrency rejected");
    assert!(
        err.to_string().contains("MYANSUB_CONCURRENCY"),
        "error should name the env variable, got: {err}"
    );
}

#[test]
fn zero_env_concurrency_names_the_variable() {
    let sources = ConfigSources {
        env: vec![("MYANSUB_CONCURRENCY".to_string(), "0".to_string())],
        ..Default::default()
    };
    let err = myansub::config::load(&sources).expect_err("zero concurrency rejected");
    assert!(
        err.to_string().contains("MYANSUB_CONCURRENCY"),
        "error should name the env variable, got: {err}"
    );
}

#[test]
fn env_maps_models_and_tier() {
    let sources = ConfigSources {
        env: vec![
            (
                "MYANSUB_ANALYSIS_MODEL".to_string(),
                "env-analysis".to_string(),
            ),
            (
                "MYANSUB_TRANSLATION_MODEL".to_string(),
                "env-translation".to_string(),
            ),
        ],
        ..Default::default()
    };
    let config = myansub::config::load(&sources).expect("load");

    assert_eq!(config.llm.models.analysis, "env-analysis");
    assert_eq!(config.llm.models.translation, "env-translation");
}
