//! Layered configuration: defaults → global file → project file → `--config`
//! file → `MYANSUB_*` environment → CLI flags (see `docs/CONFIG.md` §1).
//!
//! The struct tree mirrors `docs/config.example`. Unknown keys are rejected
//! (`#[serde(deny_unknown_fields)]`) and values are validated per
//! `docs/CONFIG.md` §5; validation errors name the offending key.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Errors raised while loading or validating configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A config file could not be read from disk.
    #[error("cannot read config file `{path}`: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A config file could not be parsed, or the merged configuration contained
    /// an unknown key or a type error.
    #[error("invalid config{}: {source}", .path.as_ref().map(|p| format!(" in `{}`", p.display())).unwrap_or_default())]
    Parse {
        path: Option<PathBuf>,
        #[source]
        source: toml::de::Error,
    },

    /// An environment override held a value of the wrong type.
    #[error("invalid value `{value}` for `{key}`: {message}")]
    InvalidEnv {
        key: String,
        value: String,
        message: String,
    },

    /// A value failed validation. `key` names the offending setting.
    #[error("invalid config value for `{key}`: {message}")]
    Invalid { key: String, message: String },
}

/// The places configuration can come from, in precedence order.
///
/// File layers are passed as explicit paths so this module stays independent of
/// project resolution. `env` is the set of `MYANSUB_*` variables (passed in
/// rather than read from the process so tests stay deterministic).
#[derive(Debug, Default, Clone)]
pub struct ConfigSources {
    /// `<config_dir>/myansub/config.toml`.
    pub global_file: Option<PathBuf>,
    /// `<project>/myansub.toml`.
    pub project_file: Option<PathBuf>,
    /// File given with `--config` (highest file precedence).
    pub config_file: Option<PathBuf>,
    /// `MYANSUB_*` environment overrides as `(name, value)` pairs.
    pub env: Vec<(String, String)>,
    /// Overrides coming from CLI flags (highest precedence).
    pub overrides: CliOverrides,
}

/// Config overrides sourced from CLI flags.
///
/// Command-specific `--model` flags are mapped by the caller to the relevant
/// field (analysis vs translation).
#[derive(Debug, Default, Clone)]
pub struct CliOverrides {
    pub analysis_model: Option<String>,
    pub translation_model: Option<String>,
    pub service_tier: Option<String>,
    pub concurrency: Option<u32>,
}

/// The resolved, validated configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub llm: Llm,
    pub cache: Cache,
    pub analysis: Analysis,
    pub translation: Translation,
    pub quality: Quality,
    pub estimate: Estimate,
    pub output: Output,
    /// Prices in USD per 1M tokens, keyed by model name.
    pub pricing: BTreeMap<String, PriceEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Llm {
    pub base_url: String,
    pub timeout_secs: u64,
    pub max_retries: u32,
    pub backoff_base_ms: u64,
    pub backoff_max_ms: u64,
    pub service_tier: String,
    pub service_tier_fallback: bool,
    pub max_sheds_before_fallback: u32,
    pub confirm_above_usd: f64,
    pub models: Models,
    pub thinking: Thinking,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Models {
    pub analysis: String,
    pub translation: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Thinking {
    pub analysis: String,
    pub translation: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Cache {
    pub local: bool,
    pub provider: String,
    pub implicit_hit_rate: f64,
    pub min_cache_tokens: u64,
    pub ttl_margin_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Analysis {
    pub single_pass_max_tokens: u64,
    pub chunk_cues: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Translation {
    pub batch_size: u32,
    pub context_before: u32,
    pub context_after: u32,
    pub concurrency: u32,
    pub warm_cache_first: bool,
    pub max_content_attempts: u32,
    pub output_ratio: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Quality {
    pub max_graphemes_per_line: u32,
    pub max_lines: u32,
    pub max_gps: f64,
    pub drift_ratio: f64,
}

/// Bridges the `[quality]` config to the pure [`quality::Limits`] the checks
/// consume. The mapping lives here (not in `quality`) so `quality`'s only crate
/// dependency stays `domain` (ADR-010).
///
/// [`quality::Limits`]: crate::quality::checks::Limits
impl From<&Quality> for crate::quality::checks::Limits {
    fn from(q: &Quality) -> Self {
        Self {
            max_graphemes_per_line: q.max_graphemes_per_line,
            max_lines: q.max_lines,
            max_gps: q.max_gps,
            drift_ratio: q.drift_ratio,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Estimate {
    pub bytes_per_token_latin: f64,
    pub bytes_per_token_myanmar: f64,
    pub bytes_per_token_cjk: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Output {
    pub ass_font_override: String,
    pub utf8_bom: bool,
    pub grace_secs: u64,
}

/// A pricing entry. Required prices have no default; optional long-prompt
/// bracket fields are absent unless configured.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceEntry {
    pub input: f64,
    pub cached_input: f64,
    pub output: f64,
    /// Explicit-cache storage rate, in USD per 1,000,000 tokens per hour.
    #[serde(default)]
    pub storage_per_hour: f64,
    #[serde(default = "default_flex_multiplier")]
    pub flex_multiplier: f64,
    #[serde(default)]
    pub long_prompt_threshold: Option<u64>,
    #[serde(default)]
    pub long_input: Option<f64>,
    #[serde(default)]
    pub long_output: Option<f64>,
}

fn default_flex_multiplier() -> f64 {
    0.5
}

impl Default for Llm {
    fn default() -> Self {
        Self {
            base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
            timeout_secs: 120,
            max_retries: 6,
            backoff_base_ms: 1000,
            backoff_max_ms: 60000,
            service_tier: "flex".to_string(),
            service_tier_fallback: true,
            max_sheds_before_fallback: 3,
            confirm_above_usd: 1.0,
            models: Models::default(),
            thinking: Thinking::default(),
        }
    }
}

impl Default for Models {
    fn default() -> Self {
        Self {
            analysis: "gemini-flash-latest".to_string(),
            translation: "gemini-flash-latest".to_string(),
        }
    }
}

impl Default for Thinking {
    fn default() -> Self {
        Self {
            analysis: "low".to_string(),
            translation: "off".to_string(),
        }
    }
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            local: true,
            provider: "auto".to_string(),
            implicit_hit_rate: 0.5,
            min_cache_tokens: 2048,
            ttl_margin_secs: 600,
        }
    }
}

impl Default for Analysis {
    fn default() -> Self {
        Self {
            single_pass_max_tokens: 150000,
            chunk_cues: 400,
        }
    }
}

impl Default for Translation {
    fn default() -> Self {
        Self {
            batch_size: 80,
            context_before: 3,
            context_after: 2,
            concurrency: 4,
            warm_cache_first: true,
            max_content_attempts: 3,
            output_ratio: 4.0,
        }
    }
}

impl Default for Quality {
    fn default() -> Self {
        Self {
            max_graphemes_per_line: 42,
            max_lines: 2,
            max_gps: 20.0,
            drift_ratio: 3.0,
        }
    }
}

impl Default for Estimate {
    fn default() -> Self {
        Self {
            bytes_per_token_latin: 4.0,
            bytes_per_token_myanmar: 3.0,
            bytes_per_token_cjk: 3.0,
        }
    }
}

impl Default for Output {
    fn default() -> Self {
        Self {
            ass_font_override: "Noto Sans Myanmar".to_string(),
            utf8_bom: false,
            grace_secs: 10,
        }
    }
}

/// Loads and validates configuration from the given sources.
pub fn load(sources: &ConfigSources) -> Result<Config, ConfigError> {
    let mut table = toml::Table::new();

    for path in [
        sources.global_file.as_deref(),
        sources.project_file.as_deref(),
        sources.config_file.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        let overlay = read_table(path)?;
        deep_merge(&mut table, overlay);
    }

    apply_env(&mut table, &sources.env)?;
    apply_overrides(&mut table, &sources.overrides);

    let config: Config = toml::Value::Table(table)
        .try_into()
        .map_err(|source| ConfigError::Parse { path: None, source })?;

    config.validate()?;
    for warning in config.price_warnings() {
        tracing::warn!("{warning}");
    }

    Ok(config)
}

/// Assembles [`ConfigSources`] from candidate filesystem paths.
///
/// The global and project file layers are included only when they exist on disk
/// (a missing one is not an error, `docs/CONFIG.md` §1). The `--config` layer is
/// passed through unconditionally, so a missing file given with `--config`
/// becomes a hard read error at [`load`]. This is where configuration first
/// loads from disk, so the existence filter lives here (M0 reliability fix).
pub fn sources_from_paths(
    global_file: Option<PathBuf>,
    project_file: Option<PathBuf>,
    config_file: Option<PathBuf>,
    env: Vec<(String, String)>,
    overrides: CliOverrides,
) -> ConfigSources {
    ConfigSources {
        global_file: global_file.filter(|p| p.is_file()),
        project_file: project_file.filter(|p| p.is_file()),
        config_file,
        env,
        overrides,
    }
}

/// Platform global config path: `<config_dir>/myansub/config.toml`.
///
/// Resolved from `XDG_CONFIG_HOME`/`HOME` on Unix and `APPDATA` on Windows
/// without pulling in an extra dependency. Returns `None` when no suitable
/// base directory is set.
pub fn global_config_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.map(|b| b.join("myansub").join("config.toml"))
}

fn read_table(path: &Path) -> Result<toml::Table, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: Some(path.to_path_buf()),
        source,
    })
}

/// Recursively merges `overlay` into `base`; `overlay` values win.
fn deep_merge(base: &mut toml::Table, overlay: toml::Table) {
    for (key, value) in overlay {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(base_tbl)), toml::Value::Table(overlay_tbl)) => {
                deep_merge(base_tbl, overlay_tbl);
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

/// Applies `MYANSUB_*` environment overrides. See `docs/CONFIG.md` §3 for the
/// mapping. Unknown `MYANSUB_*` names are ignored.
fn apply_env(table: &mut toml::Table, env: &[(String, String)]) -> Result<(), ConfigError> {
    for (key, value) in env {
        match key.as_str() {
            "MYANSUB_ANALYSIS_MODEL" => {
                set_path(table, &["llm", "models", "analysis"], value.clone().into());
            }
            "MYANSUB_TRANSLATION_MODEL" => {
                set_path(
                    table,
                    &["llm", "models", "translation"],
                    value.clone().into(),
                );
            }
            "MYANSUB_SERVICE_TIER" => {
                set_path(table, &["llm", "service_tier"], value.clone().into());
            }
            "MYANSUB_CONCURRENCY" => {
                let parsed: u32 = value.parse().ok().filter(|n| *n >= 1).ok_or_else(|| {
                    ConfigError::InvalidEnv {
                        key: key.clone(),
                        value: value.clone(),
                        message: "expected a positive integer (at least 1)".to_string(),
                    }
                })?;
                set_path(
                    table,
                    &["translation", "concurrency"],
                    i64::from(parsed).into(),
                );
            }
            _ => {}
        }
    }
    Ok(())
}

/// Applies CLI flag overrides (highest precedence).
fn apply_overrides(table: &mut toml::Table, overrides: &CliOverrides) {
    if let Some(model) = &overrides.analysis_model {
        set_path(table, &["llm", "models", "analysis"], model.clone().into());
    }
    if let Some(model) = &overrides.translation_model {
        set_path(
            table,
            &["llm", "models", "translation"],
            model.clone().into(),
        );
    }
    if let Some(tier) = &overrides.service_tier {
        set_path(table, &["llm", "service_tier"], tier.clone().into());
    }
    if let Some(concurrency) = overrides.concurrency {
        set_path(
            table,
            &["translation", "concurrency"],
            i64::from(concurrency).into(),
        );
    }
}

/// Sets a nested value, creating intermediate tables as needed.
fn set_path(table: &mut toml::Table, path: &[&str], value: toml::Value) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut current = table;
    for segment in parents {
        let slot = current
            .entry(segment.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if !slot.is_table() {
            *slot = toml::Value::Table(toml::Table::new());
        }
        // Invariant: `slot` is a table (just ensured above).
        current = slot
            .as_table_mut()
            .expect("slot was just set to a table value");
    }
    current.insert(last.to_string(), value);
}

impl Config {
    /// Validates values per `docs/CONFIG.md` §5. Errors name the offending key.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.translation.concurrency < 1 {
            return Err(ConfigError::Invalid {
                key: "translation.concurrency".to_string(),
                message: "must be at least 1".to_string(),
            });
        }
        if !(5..=300).contains(&self.translation.batch_size) {
            return Err(ConfigError::Invalid {
                key: "translation.batch_size".to_string(),
                message: format!(
                    "must be between 5 and 300, got {}",
                    self.translation.batch_size
                ),
            });
        }
        if self.llm.max_retries > 20 {
            return Err(ConfigError::Invalid {
                key: "llm.max_retries".to_string(),
                message: format!("must be at most 20, got {}", self.llm.max_retries),
            });
        }
        if self.llm.models.analysis.trim().is_empty() {
            return Err(ConfigError::Invalid {
                key: "llm.models.analysis".to_string(),
                message: "must not be empty".to_string(),
            });
        }
        if self.llm.models.translation.trim().is_empty() {
            return Err(ConfigError::Invalid {
                key: "llm.models.translation".to_string(),
                message: "must not be empty".to_string(),
            });
        }
        if !matches!(self.llm.service_tier.as_str(), "flex" | "standard") {
            return Err(ConfigError::Invalid {
                key: "llm.service_tier".to_string(),
                message: format!(
                    "must be one of flex, standard; got `{}`",
                    self.llm.service_tier
                ),
            });
        }
        if self.quality.drift_ratio < 1.0 {
            return Err(ConfigError::Invalid {
                key: "quality.drift_ratio".to_string(),
                message: format!("must be at least 1.0, got {}", self.quality.drift_ratio),
            });
        }
        if !matches!(
            self.cache.provider.as_str(),
            "off" | "implicit" | "explicit" | "auto"
        ) {
            return Err(ConfigError::Invalid {
                key: "cache.provider".to_string(),
                message: format!(
                    "must be one of off, implicit, explicit, auto; got `{}`",
                    self.cache.provider
                ),
            });
        }
        Ok(())
    }

    /// Warnings for configured models that lack a pricing entry. A missing
    /// price is a warning, not an error (`docs/CONFIG.md` §5).
    pub fn price_warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        for model in [&self.llm.models.analysis, &self.llm.models.translation] {
            if !self.pricing.contains_key(model) {
                warnings.push(format!(
                    "no pricing entry for model `{model}`; its cost will be reported as unknown"
                ));
            }
        }
        warnings.dedup();
        warnings
    }
}
