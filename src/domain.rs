//! Pure domain types shared across the crate.
//!
//! This module depends on nothing else in the crate (see
//! `docs/ARCHITECTURE.md` §3). Types here mirror `docs/DATA_MODEL.md` §1 and §5.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// An LLM provider (`docs/LLM.md` §10.1, ADR-051a).
///
/// A pure domain type (like [`DigitStyle`]) so `config`, `cli`, `llm`, and
/// `pipeline` can all name a provider without crossing the dependency direction
/// (`docs/ARCHITECTURE.md` §3). The canonical labels are `gemini` and
/// `openrouter`. The default is [`Provider::Gemini`] (`docs/CONFIG.md` §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Provider {
    #[default]
    Gemini,
    OpenRouter,
}

impl Provider {
    /// The canonical lowercase label (`gemini`/`openrouter`).
    pub fn label(self) -> &'static str {
        match self {
            Provider::Gemini => "gemini",
            Provider::OpenRouter => "openrouter",
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

impl FromStr for Provider {
    type Err = ParseModelRefError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "gemini" => Ok(Provider::Gemini),
            "openrouter" => Ok(Provider::OpenRouter),
            _ => Err(ParseModelRefError::UnknownProvider(s.to_string())),
        }
    }
}

/// A model reference of the form `[provider:]model` (`docs/LLM.md` §10.1,
/// ADR-051a). The `provider` half selects the transport and is folded into the
/// local cache key and the stored `model` columns; only the bare [`ModelRef::model`]
/// id is sent on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelRef {
    pub provider: Provider,
    pub model: String,
}

/// Error returned when a string cannot be parsed into a [`ModelRef`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseModelRefError {
    /// The `provider:` prefix was not a recognised provider. `main` maps this to
    /// exit code 2 (usage error, `docs/CLI.md` §7).
    UnknownProvider(String),
    /// The reference (or its model half) was empty.
    Empty,
}

impl fmt::Display for ParseModelRefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseModelRefError::UnknownProvider(p) => {
                write!(
                    f,
                    "unknown provider `{p}` (expected `gemini` or `openrouter`)"
                )
            }
            ParseModelRefError::Empty => f.write_str("empty model reference"),
        }
    }
}

impl std::error::Error for ParseModelRefError {}

impl ModelRef {
    /// Parses a `[provider:]model` reference. A `provider:` prefix selects the
    /// provider; with no prefix, `default` is used. An unrecognised prefix is a
    /// [`ParseModelRefError::UnknownProvider`].
    ///
    /// The split is on the first `:` only, and only a *provider-like* prefix (a
    /// simple alphabetic token such as `gemini`/`openrouter`) is treated as a
    /// provider claim. This keeps OpenRouter ids that legitimately contain a colon
    /// intact — a `:free` suffix (`meta-llama/llama-3.1-8b-instruct:free`) has a
    /// prefix with a `/`, so it is not provider-like and the whole string is a bare
    /// model under `default`. A provider-like prefix that is not a recognised
    /// provider is a [`ParseModelRefError::UnknownProvider`] (which `main` maps to
    /// exit 2), so a typo like `openai:gpt-4` fails loudly instead of being sent to
    /// the default provider as a nonsense model id.
    pub fn parse(s: &str, default: Provider) -> Result<ModelRef, ParseModelRefError> {
        let s = s.trim();
        if s.is_empty() {
            return Err(ParseModelRefError::Empty);
        }
        if let Some((prefix, rest)) = s.split_once(':') {
            let prefix = prefix.trim();
            // Provider-like: a non-empty, purely-alphabetic token. Both real
            // providers (`gemini`, `openrouter`) match; an OpenRouter id segment
            // (which carries `/`, `-`, `.`, or digits) does not.
            let provider_like =
                !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_alphabetic());
            if provider_like {
                let model = rest.trim();
                if model.is_empty() {
                    return Err(ParseModelRefError::Empty);
                }
                return Ok(ModelRef {
                    provider: prefix.parse()?,
                    model: model.to_string(),
                });
            }
        }
        // No provider-like prefix → bare model under the default provider.
        Ok(ModelRef {
            provider: default,
            model: s.to_string(),
        })
    }

    /// The canonical `provider:model` string stored in the `model` columns
    /// (`docs/DATA_MODEL.md` §2) and folded into the local cache key.
    pub fn as_ref_string(&self) -> String {
        format!("{}:{}", self.provider.label(), self.model)
    }
}

impl fmt::Display for ModelRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.provider.label(), self.model)
    }
}

/// A subtitle container format.
///
/// Parsing and display use the lowercase labels `srt|vtt|ass|ssa|sbv`
/// (`docs/DATA_MODEL.md` §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubtitleFormat {
    Srt,
    Vtt,
    Ass,
    Ssa,
    Sbv,
}

impl SubtitleFormat {
    /// The canonical lowercase label for this format.
    pub fn label(self) -> &'static str {
        match self {
            SubtitleFormat::Srt => "srt",
            SubtitleFormat::Vtt => "vtt",
            SubtitleFormat::Ass => "ass",
            SubtitleFormat::Ssa => "ssa",
            SubtitleFormat::Sbv => "sbv",
        }
    }

    /// The conventional file extension (without the leading dot).
    pub fn extension(self) -> &'static str {
        // Each format maps to its own lowercase label as the extension.
        self.label()
    }
}

/// Error returned when a string cannot be parsed into a [`SubtitleFormat`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseFormatError {
    label: String,
}

impl fmt::Display for ParseFormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown subtitle format: {}", self.label)
    }
}

impl std::error::Error for ParseFormatError {}

impl FromStr for SubtitleFormat {
    type Err = ParseFormatError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "srt" => Ok(SubtitleFormat::Srt),
            "vtt" | "webvtt" => Ok(SubtitleFormat::Vtt),
            "ass" => Ok(SubtitleFormat::Ass),
            "ssa" => Ok(SubtitleFormat::Ssa),
            "sbv" => Ok(SubtitleFormat::Sbv),
            _ => Err(ParseFormatError {
                label: s.to_string(),
            }),
        }
    }
}

impl fmt::Display for SubtitleFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Format-specific metadata preserved for same-format output.
///
/// This is an opaque container for things like ASS script-info and style blocks.
/// The codec populates it so same-format output can reproduce ASS/SSA headers
/// (see `docs/DECISIONS.md` ADR-023).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatMeta {
    /// ASS/SSA `[Script Info]` as JSON-encoded structured data (the codec's
    /// key→value map), when present (`docs/DECISIONS.md` ADR-023).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_info: Option<String>,
    /// ASS/SSA style table as JSON-encoded structured data (the codec's style
    /// records), when present (`docs/DECISIONS.md` ADR-023).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub styles: Option<String>,
}

/// A single subtitle cue, mirroring `docs/DATA_MODEL.md` §1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cue {
    /// 1-based index, stable for the life of the project.
    pub idx: u32,
    pub start_ms: u64,
    pub end_ms: u64,
    /// Exactly as parsed.
    pub text_raw: String,
    /// Inline tags replaced by `⟦n⟧` placeholders; lead tags removed.
    pub text_masked: String,
    /// Whole-line leading override block, e.g. `{\an8\pos(10,20)}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead_tags: Option<String>,
    /// Placeholder `n` → original tag.
    #[serde(default)]
    pub inline_tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    /// ASS Name/Actor field when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
    /// False if no letters remain after tag removal.
    pub translatable: bool,
}

/// A parsed subtitle document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Document {
    pub format: SubtitleFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format_meta: Option<FormatMeta>,
    pub cues: Vec<Cue>,
}

/// Deterministic quality flags attached to a translation (`docs/DATA_MODEL.md` §5).
///
/// Serialized as snake_case strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityFlag {
    Empty,
    PlaceholderMismatch,
    Untranslated,
    ZawgyiSuspect,
    BoundaryDrift,
    GlossaryMiss,
    /// The target uses digits of the system *not* chosen in `[burmese].digits`
    /// (placeholder contents excluded) (`docs/DATA_MODEL.md` §5, ADR-049(e)).
    DigitStyle,
    TooLong,
    TooFast,
    /// A provider refused the batch on moderation/guardrail grounds (OpenRouter
    /// 403 → [`ErrorClass::ContentRefused`]); the batch fails but the run
    /// continues (`docs/DATA_MODEL.md` §5, `docs/LLM.md` §10.6, ADR-051e).
    ///
    /// [`ErrorClass::ContentRefused`]: crate::llm::client::ErrorClass::ContentRefused
    Refused,
    Failed,
}

/// The digit style the Burmese output requests (`[burmese].digits`, ADR-049(e)).
///
/// A pure domain type so the config, prompt, quality, and pipeline layers can all
/// share it without crossing the dependency direction (`docs/ARCHITECTURE.md` §3).
/// The default is [`DigitStyle::Myanmar`] (`docs/CONFIG.md` §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DigitStyle {
    /// Myanmar digits ၀–၉ (U+1040–U+1049).
    #[default]
    Myanmar,
    /// Western digits 0–9 (U+0030–U+0039).
    Western,
}

impl DigitStyle {
    /// The canonical label (`myanmar`/`western`) matching `[burmese].digits` and the
    /// value recorded in `project.translation_digits` (`docs/DATA_MODEL.md` §2,
    /// ADR-050).
    pub fn label(self) -> &'static str {
        match self {
            DigitStyle::Myanmar => "myanmar",
            DigitStyle::Western => "western",
        }
    }
}

/// How a trailing ။ at the end of a cue is handled at export
/// (`[burmese].cue_final_stop`, `docs/CONFIG.md` §6, ADR-049(e), ADR-050).
///
/// A pure domain type (like [`DigitStyle`]) so `config` can read the policy without
/// depending on `pipeline` (`docs/ARCHITECTURE.md` §3: nothing depends on
/// `pipeline`). The output stage applies it deterministically at export, never the
/// LLM. The default is [`CueFinalStop::Strip`] (`docs/CONFIG.md` §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CueFinalStop {
    /// Remove one trailing ။ (and trailing whitespace) at the end of the cue.
    #[default]
    Strip,
    /// Leave the cue text unchanged.
    Keep,
}
