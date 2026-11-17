// Tests for config validation per docs/CONFIG.md §5.

use myansub::config::ConfigSources;
use std::io::Write;
use tempfile::NamedTempFile;

fn load_toml(contents: &str) -> Result<myansub::config::Config, myansub::config::ConfigError> {
    let mut f = NamedTempFile::new().expect("create temp file");
    f.write_all(contents.as_bytes()).expect("write temp file");
    let sources = ConfigSources {
        project_file: Some(f.path().to_path_buf()),
        ..Default::default()
    };
    myansub::config::load(&sources)
}

// AC: M0-AC3
#[test]
fn batch_size_below_range_names_the_key() {
    let err = load_toml("[translation]\nbatch_size = 2\n").expect_err("should reject batch_size 2");
    let msg = err.to_string();
    assert!(
        msg.contains("batch_size"),
        "error should name the offending key, got: {msg}"
    );
}

// AC: M0-AC3
#[test]
fn batch_size_above_range_names_the_key() {
    let err =
        load_toml("[translation]\nbatch_size = 500\n").expect_err("should reject batch_size 500");
    assert!(err.to_string().contains("batch_size"));
}

#[test]
fn concurrency_zero_names_the_key() {
    let err =
        load_toml("[translation]\nconcurrency = 0\n").expect_err("should reject concurrency 0");
    assert!(err.to_string().contains("concurrency"));
}

#[test]
fn max_retries_above_limit_names_the_key() {
    let err = load_toml("[llm]\nmax_retries = 21\n").expect_err("should reject max_retries 21");
    assert!(err.to_string().contains("max_retries"));
}

#[test]
fn empty_model_names_the_key() {
    let err = load_toml("[llm.models]\nanalysis = \"\"\n").expect_err("should reject empty model");
    assert!(err.to_string().contains("analysis"));
}

#[test]
fn invalid_service_tier_names_the_key() {
    let err =
        load_toml("[llm]\nservice_tier = \"turbo\"\n").expect_err("should reject bad service tier");
    assert!(err.to_string().contains("service_tier"));
}

#[test]
fn invalid_cache_provider_names_the_key() {
    let err =
        load_toml("[cache]\nprovider = \"maybe\"\n").expect_err("should reject bad cache provider");
    assert!(err.to_string().contains("provider"));
}

#[test]
fn drift_ratio_below_one_names_the_key() {
    // drift_ratio < 1.0 makes boundary_drift over-flag (low = med/ratio → inf).
    let err =
        load_toml("[quality]\ndrift_ratio = 0.0\n").expect_err("should reject drift_ratio 0.0");
    assert!(
        err.to_string().contains("quality.drift_ratio"),
        "error should name quality.drift_ratio, got: {err}"
    );
    let err =
        load_toml("[quality]\ndrift_ratio = 0.5\n").expect_err("should reject drift_ratio 0.5");
    assert!(err.to_string().contains("quality.drift_ratio"));
}

#[test]
fn unknown_key_is_rejected() {
    let err = load_toml("[translation]\nnot_a_real_key = 1\n").expect_err("unknown key rejected");
    assert!(err.to_string().contains("not_a_real_key"));
}

#[test]
fn valid_config_passes() {
    let config = load_toml("[translation]\nbatch_size = 100\nconcurrency = 8\n").expect("valid");
    assert_eq!(config.translation.batch_size, 100);
    assert_eq!(config.translation.concurrency, 8);
}
