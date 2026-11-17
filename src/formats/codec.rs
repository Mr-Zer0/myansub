//! Subtitle codec trait, format detection, and the parse selection chain.
//!
//! The codec layer operates on *already-decoded* UTF-8 text (see
//! `docs/ARCHITECTURE.md` §3; byte decoding lives in [`super::encoding`]). It
//! never writes a subtitle parser by hand: the primary codec adapts the
//! `subtitler` 2.x crate and the fallback adapts `rsubs-lib` 0.4.x
//! (`docs/DECISIONS.md` ADR-001).
//!
//! The selection chain tries the primary codec first and, on a
//! [`FormatError`], falls back to the secondary — except for `SBV`, which has no
//! fallback and returns a typed error instead. The chain is generic over two
//! [`SubtitleCodec`] implementations ([`parse_with_codecs`]) so tests can inject
//! a deliberately failing primary; [`parse_document`] wires the real codecs.
//!
//! For ASS/SSA, [`parse_document`] additionally applies a deterministic
//! text-fidelity overlay (`docs/DECISIONS.md` ADR-024): `subtitler` 2.13 drops
//! the space after commas in dialogue text, so the faithful `rsubs-lib` text is
//! spliced back in when the two parses agree structurally.

use thiserror::Error;

use super::markup;
use crate::domain::{Cue, Document, SubtitleFormat};

pub use super::rsubs::RsubsCodec;
pub use super::subtitler::SubtitlerCodec;

/// Errors produced while parsing or writing a subtitle document.
#[derive(Debug, Error)]
pub enum FormatError {
    /// Neither the content nor the extension hint identified a known format.
    #[error("could not detect subtitle format from content")]
    UnknownFormat,
    /// A codec failed to parse the input for the given format.
    #[error("failed to parse {format} subtitle: {message}")]
    Parse {
        format: SubtitleFormat,
        message: String,
    },
    /// A codec failed to serialize a document for the given format.
    #[error("failed to write {format} subtitle: {message}")]
    Write {
        format: SubtitleFormat,
        message: String,
    },
    /// The primary codec failed and no fallback exists for this format
    /// (`docs/DECISIONS.md` ADR-001: SBV has no fallback).
    #[error("no fallback codec available for {format}")]
    NoFallback { format: SubtitleFormat },
    /// This codec does not support the requested format.
    #[error("{format} is not supported by this codec")]
    Unsupported { format: SubtitleFormat },
}

/// A subtitle codec: parses decoded text into a [`Document`] and serializes a
/// [`Document`] back to a string in a given format.
///
/// Implementations operate on a *known* [`SubtitleFormat`]; format detection is
/// a separate concern ([`detect_format`]).
pub trait SubtitleCodec {
    /// Parse `text` as `format` into a [`Document`].
    fn parse(&self, text: &str, format: SubtitleFormat) -> Result<Document, FormatError>;

    /// Serialize `doc` to a string in `format`.
    fn write(&self, doc: &Document, format: SubtitleFormat) -> Result<String, FormatError>;
}

/// Detects the subtitle format from `text` content, using `ext_hint` only as a
/// tie-breaker (`docs/REQUIREMENTS.md` FR-1).
///
/// Content detection cannot distinguish ASS from SSA (they share a syntax), so
/// when the content looks like ASS the extension hint decides between the two.
/// When content detection fails entirely, the extension hint is the last resort.
pub fn detect_format(text: &str, ext_hint: Option<&str>) -> Option<SubtitleFormat> {
    let from_content = subtitler::detect_format(text.as_bytes()).and_then(map_subtitler_format);
    match from_content {
        Some(SubtitleFormat::Ass) if ext_hint_is(ext_hint, "ssa") => Some(SubtitleFormat::Ssa),
        Some(format) => Some(format),
        None => ext_hint
            .map(normalize_ext)
            .and_then(|e| e.parse::<SubtitleFormat>().ok()),
    }
}

/// Strips a leading dot and surrounding whitespace from an extension hint, so
/// `".vtt"`, `"vtt"`, and `" VTT "` all normalize to a parseable label.
fn normalize_ext(ext: &str) -> &str {
    ext.trim().trim_start_matches('.')
}

fn ext_hint_is(ext_hint: Option<&str>, want: &str) -> bool {
    ext_hint.is_some_and(|e| normalize_ext(e).eq_ignore_ascii_case(want))
}

/// Parses `text` as `format` through the real codec chain: `subtitler` primary,
/// `rsubs-lib` fallback (ADR-001).
///
/// For ASS/SSA the faithful dialogue text from `rsubs-lib` is overlaid onto the
/// `subtitler` structure (ADR-024); see [`apply_ass_text_fidelity_overlay`].
pub fn parse_document(text: &str, format: SubtitleFormat) -> Result<Document, FormatError> {
    let mut doc = parse_with_codecs(&SubtitlerCodec, &RsubsCodec, text, format)?;
    if matches!(format, SubtitleFormat::Ass | SubtitleFormat::Ssa) {
        let _ = apply_ass_text_fidelity_overlay(&mut doc, text, format);
    }
    Ok(doc)
}

/// Whether the ASS/SSA text-fidelity overlay spliced faithful text in or was
/// skipped (`docs/DECISIONS.md` ADR-024).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverlayOutcome {
    /// The two parses aligned; faithful text was merged (a no-op for cues whose
    /// text already matched).
    Applied,
    /// The overlay was skipped; `subtitler`'s text is left unchanged.
    Skipped,
}

/// Repairs ASS/SSA dialogue text in `doc` using a second, faithful parse by the
/// `rsubs-lib` codec (`docs/DECISIONS.md` ADR-024).
///
/// `subtitler` 2.13 is the authoritative structural parser (timings, styles,
/// speaker, `format_meta`) but it strips the space after commas in dialogue
/// text, which corrupts the *source* text the translator sees. `rsubs-lib`
/// preserves it. This overlay re-parses the same input with `rsubs-lib` and
/// merges its faithful text via [`merge_faithful_text`].
///
/// It never fails the parse: if `rsubs-lib` errors or the parses do not align,
/// the `subtitler` text is kept and a `warn` is emitted. Returns the outcome so
/// callers (and tests) can observe whether the overlay applied.
fn apply_ass_text_fidelity_overlay(
    doc: &mut Document,
    text: &str,
    format: SubtitleFormat,
) -> OverlayOutcome {
    let outcome = match RsubsCodec.parse(text, format) {
        Ok(faithful) => merge_faithful_text(doc, &faithful.cues),
        Err(_) => OverlayOutcome::Skipped,
    };
    if outcome == OverlayOutcome::Skipped {
        warn_overlay_skipped();
    }
    outcome
}

/// Splices the faithful `text_raw` from `faithful` into `doc`, re-deriving the
/// masked fields for every cue whose text changes — but only when the two parses
/// align (same cue count AND identical per-cue `(start_ms, end_ms)`).
///
/// The alignment guard is the safety basis of ADR-024: without it, a structural
/// disagreement between the two parsers could pair the wrong texts. On any
/// misalignment this is a no-op and returns [`OverlayOutcome::Skipped`], leaving
/// every `subtitler` text untouched (no partial splice).
pub(crate) fn merge_faithful_text(doc: &mut Document, faithful: &[Cue]) -> OverlayOutcome {
    let aligned = faithful.len() == doc.cues.len()
        && doc
            .cues
            .iter()
            .zip(faithful)
            .all(|(a, b)| a.start_ms == b.start_ms && a.end_ms == b.end_ms);
    if !aligned {
        return OverlayOutcome::Skipped;
    }

    for (cue, faithful_cue) in doc.cues.iter_mut().zip(faithful) {
        if cue.text_raw == faithful_cue.text_raw {
            continue;
        }
        let masked = markup::mask(&faithful_cue.text_raw);
        cue.text_raw = faithful_cue.text_raw.clone();
        cue.text_masked = masked.text_masked;
        cue.lead_tags = masked.lead_tags;
        cue.inline_tags = masked.inline_tags;
        cue.translatable = masked.translatable;
    }
    OverlayOutcome::Applied
}

/// Emits the overlay-skipped warning once per skip (per parse call).
///
/// The message carries no dialogue text (`docs/ARCHITECTURE.md` §9).
fn warn_overlay_skipped() {
    tracing::warn!(
        "ASS/SSA text-fidelity overlay skipped; dialogue spacing after commas may be lost"
    );
}

/// Serializes `doc` as `format` using the primary codec.
///
/// `subtitler` supports every format this crate accepts, so writing needs no
/// fallback; the fallback exists only to recover a [`Document`] when the primary
/// parser rejects otherwise-usable input.
pub fn write_document(doc: &Document, format: SubtitleFormat) -> Result<String, FormatError> {
    SubtitlerCodec.write(doc, format)
}

/// The codec selection chain, generic over the primary and fallback codecs.
///
/// This is the injection seam for testing (AC M1-AC3): a test supplies a
/// deliberately failing `primary` and a working `fallback` to prove the chain
/// falls back, and proves that `SBV` does *not* fall back.
///
/// Behavior: try `primary`. On success, return it. On a [`FormatError`], fall
/// back to `fallback` — unless `format` is [`SubtitleFormat::Sbv`], which has no
/// fallback (ADR-001) and yields [`FormatError::NoFallback`].
///
/// When both codecs fail on a non-SBV format, the primary (`subtitler`) error is
/// surfaced: it is the richer parser and its diagnostics are more actionable
/// than the fallback's.
pub fn parse_with_codecs<P, F>(
    primary: &P,
    fallback: &F,
    text: &str,
    format: SubtitleFormat,
) -> Result<Document, FormatError>
where
    P: SubtitleCodec,
    F: SubtitleCodec,
{
    match primary.parse(text, format) {
        Ok(doc) => Ok(doc),
        Err(_) if format == SubtitleFormat::Sbv => Err(FormatError::NoFallback { format }),
        Err(primary_err) => fallback
            .parse(text, format)
            .map_err(|_fallback_err| primary_err),
    }
}

/// Maps a `subtitler` format tag to this crate's [`SubtitleFormat`], or `None`
/// for formats this crate does not accept.
fn map_subtitler_format(format: subtitler::Format) -> Option<SubtitleFormat> {
    match format {
        subtitler::Format::Srt => Some(SubtitleFormat::Srt),
        subtitler::Format::Vtt => Some(SubtitleFormat::Vtt),
        subtitler::Format::Ass => Some(SubtitleFormat::Ass),
        subtitler::Format::Ssa => Some(SubtitleFormat::Ssa),
        subtitler::Format::Sbv => Some(SubtitleFormat::Sbv),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A subtitler-shaped cue with corrupted (comma-space-stripped) text.
    fn subtitler_cue(idx: u32, start_ms: u64, end_ms: u64, text: &str) -> Cue {
        Cue {
            idx,
            start_ms,
            end_ms,
            text_raw: text.to_string(),
            text_masked: text.to_string(),
            lead_tags: None,
            inline_tags: Vec::new(),
            style: None,
            speaker: None,
            translatable: true,
        }
    }

    fn two_cue_subtitler_doc() -> Document {
        Document {
            format: SubtitleFormat::Ass,
            format_meta: None,
            cues: vec![
                subtitler_cue(1, 1000, 3000, "Hello,world"),
                subtitler_cue(2, 4000, 6000, "Bye,now"),
            ],
        }
    }

    // ADR-024 safety guard: a differing cue count must skip the overlay entirely,
    // leaving every subtitler text in place (no partial splice).
    #[test]
    fn merge_skips_on_cue_count_mismatch() {
        let mut doc = two_cue_subtitler_doc();
        let faithful = vec![subtitler_cue(1, 1000, 3000, "Hello, world")];
        let outcome = merge_faithful_text(&mut doc, &faithful);
        assert_eq!(outcome, OverlayOutcome::Skipped);
        assert_eq!(doc.cues[0].text_raw, "Hello,world");
        assert_eq!(doc.cues[1].text_raw, "Bye,now");
    }

    // ADR-024 safety guard: same count but a single differing timing must skip
    // the overlay entirely — no cue's text may be spliced.
    #[test]
    fn merge_skips_on_timing_mismatch() {
        let mut doc = two_cue_subtitler_doc();
        let faithful = vec![
            // First cue aligns; second cue's end differs by 1ms.
            subtitler_cue(1, 1000, 3000, "Hello, world"),
            subtitler_cue(2, 4000, 6001, "Bye, now"),
        ];
        let outcome = merge_faithful_text(&mut doc, &faithful);
        assert_eq!(outcome, OverlayOutcome::Skipped);
        assert_eq!(doc.cues[0].text_raw, "Hello,world");
        assert_eq!(doc.cues[1].text_raw, "Bye,now");
    }

    // When aligned, faithful text is spliced and masked fields are re-derived.
    #[test]
    fn merge_applies_and_rederives_masked_fields_when_aligned() {
        let mut doc = two_cue_subtitler_doc();
        let faithful = vec![
            subtitler_cue(1, 1000, 3000, "Hello, world"),
            subtitler_cue(2, 4000, 6000, "Bye, now"),
        ];
        let outcome = merge_faithful_text(&mut doc, &faithful);
        assert_eq!(outcome, OverlayOutcome::Applied);
        assert_eq!(doc.cues[0].text_raw, "Hello, world");
        assert_eq!(doc.cues[0].text_masked, "Hello, world");
        assert_eq!(doc.cues[1].text_raw, "Bye, now");
    }

    // Graceful path (#3): legacy v4 SSA that rsubs cannot parse reports Skipped
    // (which is exactly when the warn fires), and leaves subtitler's text intact.
    #[test]
    fn overlay_reports_skipped_when_rsubs_cannot_parse() {
        let legacy = include_str!("../../tests/fixtures/formats/legacy_v4.ssa");
        let mut doc = SubtitlerCodec
            .parse(legacy, SubtitleFormat::Ssa)
            .expect("subtitler parses legacy v4 ssa");
        let before = doc.cues[0].text_raw.clone();
        let outcome = apply_ass_text_fidelity_overlay(&mut doc, legacy, SubtitleFormat::Ssa);
        assert_eq!(outcome, OverlayOutcome::Skipped);
        assert_eq!(doc.cues[0].text_raw, before);
    }
}
