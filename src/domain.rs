//! Pure domain types shared across the crate.
//!
//! This module depends on nothing else in the crate (see
//! `docs/ARCHITECTURE.md` §3). Types here mirror `docs/DATA_MODEL.md` §1 and §5.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

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
