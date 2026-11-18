//! Byte decoding: BOM → strict UTF-8 → `chardetng` → `encoding_rs`.
//!
//! Implements the detection chain from `docs/DECISIONS.md` ADR-016 and
//! `docs/REQUIREMENTS.md` FR-1. Output is always UTF-8.

use chardetng::{EncodingDetector, Iso2022JpDetection, Utf8Detection};
use thiserror::Error;

/// The result of decoding raw bytes into text.
#[derive(Debug, Clone, PartialEq)]
pub struct Decoded {
    /// Decoded text, always valid UTF-8.
    pub text: String,
    /// Name of the encoding used (e.g. `UTF-8`, `Shift_JIS`, `windows-1252`).
    pub encoding: String,
    /// Detector confidence, when a probabilistic detector was used.
    ///
    /// `chardetng` 1.0 does not expose a numeric confidence, and BOM / strict
    /// UTF-8 / explicit override are deterministic, so this is currently always
    /// `None`. The field is kept for the `encoding_confidence` DB column.
    pub confidence: Option<f32>,
    /// True if decoding inserted U+FFFD replacement characters.
    pub had_replacement: bool,
}

/// Errors produced while decoding bytes to text.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum EncodingError {
    /// The `--encoding` override named an encoding that is not recognized.
    #[error("unknown encoding label: {label}")]
    UnknownLabel { label: String },
}

/// Decodes `bytes` to UTF-8 text.
///
/// - `override_label`: force a named encoding (e.g. from `--encoding`). An
///   unknown label is an error.
/// - `source_lang_hint`: a source-language code used as a weak hint to the
///   charset detector (mapped to a representative TLD; see below).
pub fn decode(
    bytes: &[u8],
    override_label: Option<&str>,
    source_lang_hint: Option<&str>,
) -> Result<Decoded, EncodingError> {
    // 1. Explicit override wins over all detection.
    if let Some(label) = override_label {
        let encoding = encoding_rs::Encoding::for_label(label.as_bytes()).ok_or_else(|| {
            EncodingError::UnknownLabel {
                label: label.to_string(),
            }
        })?;
        let (text, had_replacement) = encoding.decode_without_bom_handling(bytes);
        return Ok(Decoded {
            text: text.into_owned(),
            encoding: encoding.name().to_string(),
            confidence: None,
            had_replacement,
        });
    }

    // 2. A BOM is authoritative; `encoding_rs` strips it for us.
    if let Some((encoding, bom_len)) = encoding_rs::Encoding::for_bom(bytes) {
        let (text, had_replacement) = encoding.decode_without_bom_handling(&bytes[bom_len..]);
        return Ok(Decoded {
            text: text.into_owned(),
            encoding: encoding.name().to_string(),
            confidence: None,
            had_replacement,
        });
    }

    // 3. Strict UTF-8: accept only if the whole input is valid.
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Ok(Decoded {
            text: text.to_string(),
            encoding: encoding_rs::UTF_8.name().to_string(),
            confidence: None,
            had_replacement: false,
        });
    }

    // 4. Statistical detection with an optional source-language hint.
    let mut detector = EncodingDetector::new(Iso2022JpDetection::Deny);
    detector.feed(bytes, true);
    let tld = tld_hint(source_lang_hint);
    // UTF-8 was already ruled out in step 3, so deny it as a guess.
    let encoding = detector.guess(tld, Utf8Detection::Deny);
    let (text, had_replacement) = encoding.decode_without_bom_handling(bytes);
    Ok(Decoded {
        text: text.into_owned(),
        encoding: encoding.name().to_string(),
        confidence: None,
        had_replacement,
    })
}

/// Maps a source-language code to a representative ASCII TLD for `chardetng`.
///
/// `chardetng` 1.0 accepts only a TLD hint, not a language code, so we translate
/// the most common source languages to a plausible TLD. Unknown or Latin-script
/// languages return `None` (generic).
fn tld_hint(source_lang_hint: Option<&str>) -> Option<&'static [u8]> {
    let lang = source_lang_hint?;
    // Use only the primary subtag, lowercased (e.g. "zh-Hans" -> "zh").
    let primary = lang.split(['-', '_']).next().unwrap_or(lang);
    match primary.to_ascii_lowercase().as_str() {
        "ja" | "jpn" => Some(b"jp"),
        "zh" | "zho" | "cmn" => Some(b"cn"),
        "ko" | "kor" => Some(b"kr"),
        "ru" | "rus" => Some(b"ru"),
        "th" | "tha" => Some(b"th"),
        _ => None,
    }
}
