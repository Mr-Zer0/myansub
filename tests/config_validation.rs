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
fn implicit_hit_rate_out_of_range_names_the_key() {
    // The auto break-even treats implicit_hit_rate as a probability in [0, 1]
    // (docs/LLM.md §5.5); a value outside that range is rejected.
    let err = load_toml("[cache]\nimplicit_hit_rate = 1.5\n")
        .expect_err("should reject implicit_hit_rate 1.5");
    assert!(
        err.to_string().contains("implicit_hit_rate"),
        "error should name cache.implicit_hit_rate, got: {err}"
    );
    let err = load_toml("[cache]\nimplicit_hit_rate = -0.1\n")
        .expect_err("should reject implicit_hit_rate -0.1");
    assert!(err.to_string().contains("implicit_hit_rate"));
}

#[test]
fn implicit_hit_rate_in_range_passes() {
    let config = load_toml("[cache]\nimplicit_hit_rate = 0.0\n").expect("0.0 is valid");
    assert_eq!(config.cache.implicit_hit_rate, 0.0);
    let config = load_toml("[cache]\nimplicit_hit_rate = 1.0\n").expect("1.0 is valid");
    assert_eq!(config.cache.implicit_hit_rate, 1.0);
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

// AC: M12-AC10 — the `[burmese]` section defaults to `digits = "myanmar"` and
// `cue_final_stop = "strip"` (docs/CONFIG.md §6, ADR-049(e)).
#[test]
fn burmese_section_defaults_to_myanmar_and_strip() {
    let config = load_toml("").expect("empty config is valid");
    assert_eq!(config.burmese.digits, "myanmar");
    assert_eq!(config.burmese.cue_final_stop, "strip");
}

// AC: M12-AC10 — an invalid `burmese.digits` value is rejected at startup, naming
// the offending key (docs/CONFIG.md §5/§6).
#[test]
fn burmese_digits_invalid_value_names_the_key() {
    let err =
        load_toml("[burmese]\ndigits = \"arabic\"\n").expect_err("should reject digits = arabic");
    assert!(
        err.to_string().contains("burmese.digits"),
        "error should name burmese.digits, got: {err}"
    );
}

// AC: M12-AC10 — an invalid `burmese.cue_final_stop` value is rejected at startup,
// naming the offending key (docs/CONFIG.md §5/§6).
#[test]
fn burmese_cue_final_stop_invalid_value_names_the_key() {
    let err = load_toml("[burmese]\ncue_final_stop = \"maybe\"\n")
        .expect_err("should reject cue_final_stop = maybe");
    assert!(
        err.to_string().contains("burmese.cue_final_stop"),
        "error should name burmese.cue_final_stop, got: {err}"
    );
}

// AC: M13-AC1 — `llm.default_provider` defaults to `gemini` and `[llm.openrouter]`
// has the documented defaults (docs/CONFIG.md §7).
#[test]
fn default_provider_and_openrouter_defaults() {
    let config = load_toml("").expect("empty config is valid");
    assert_eq!(config.llm.default_provider, "gemini");
    assert_eq!(
        config.llm.openrouter.base_url,
        "https://openrouter.ai/api/v1"
    );
    assert_eq!(config.llm.openrouter.tier, "floor");
    assert_eq!(config.llm.openrouter.data_collection, "deny");
    assert!(!config.llm.openrouter.zdr);
    assert!(!config.llm.openrouter.attribution);
}

// AC: M13-AC1/AC14 — an invalid `llm.default_provider` is rejected, naming the key.
#[test]
fn invalid_default_provider_names_the_key() {
    let err = load_toml("[llm]\ndefault_provider = \"openai\"\n")
        .expect_err("should reject default_provider = openai");
    assert!(
        err.to_string().contains("llm.default_provider"),
        "error should name llm.default_provider, got: {err}"
    );
}

// AC: M13-AC14 — an invalid `llm.openrouter.tier` is rejected, naming the key.
#[test]
fn invalid_openrouter_tier_names_the_key() {
    let err = load_toml("[llm.openrouter]\ntier = \"turbo\"\n")
        .expect_err("should reject openrouter.tier = turbo");
    assert!(
        err.to_string().contains("llm.openrouter.tier"),
        "error should name llm.openrouter.tier, got: {err}"
    );
}

// AC: M13-AC14 — an invalid `llm.openrouter.data_collection` is rejected, naming the key.
#[test]
fn invalid_openrouter_data_collection_names_the_key() {
    let err = load_toml("[llm.openrouter]\ndata_collection = \"maybe\"\n")
        .expect_err("should reject openrouter.data_collection = maybe");
    assert!(
        err.to_string().contains("llm.openrouter.data_collection"),
        "error should name llm.openrouter.data_collection, got: {err}"
    );
}

// AC: M13-AC1 — `openrouter` is a valid default provider.
#[test]
fn default_provider_openrouter_passes() {
    let config =
        load_toml("[llm]\ndefault_provider = \"openrouter\"\n").expect("openrouter is valid");
    assert_eq!(config.llm.default_provider, "openrouter");
}
