//! Codec round-trip and selection-chain tests (milestone M1-B).
//!
//! Fixtures live in `tests/fixtures/formats/` and are small, original samples
//! (`docs/TESTING.md` §5). Round-trip comparisons are structural (cue count,
//! timings, `text_raw`) rather than byte-exact, because the codecs may reorder
//! `[Script Info]` keys or re-emit headers; what must survive is the data the
//! domain model records.

use std::cell::Cell;

use myansub::domain::{Cue, Document, SubtitleFormat};
use myansub::formats::codec::{
    FormatError, SubtitleCodec, SubtitlerCodec, detect_format, parse_document, parse_with_codecs,
    write_document,
};

const SRT: &str = include_str!("fixtures/formats/sample.srt");
const VTT: &str = include_str!("fixtures/formats/sample.vtt");
const ASS: &str = include_str!("fixtures/formats/sample.ass");
const SSA: &str = include_str!("fixtures/formats/sample.ssa");
const SBV: &str = include_str!("fixtures/formats/sample.sbv");

fn fixture(format: SubtitleFormat) -> &'static str {
    match format {
        SubtitleFormat::Srt => SRT,
        SubtitleFormat::Vtt => VTT,
        SubtitleFormat::Ass => ASS,
        SubtitleFormat::Ssa => SSA,
        SubtitleFormat::Sbv => SBV,
    }
}

/// The identity we require across a parse → write → parse round-trip.
fn timing_and_text(doc: &Document) -> Vec<(u64, u64, String)> {
    doc.cues
        .iter()
        .map(|c| (c.start_ms, c.end_ms, c.text_raw.clone()))
        .collect()
}

// AC: M1-AC1
#[test]
fn round_trip_preserves_timings_and_text_for_every_format() {
    for format in [
        SubtitleFormat::Srt,
        SubtitleFormat::Vtt,
        SubtitleFormat::Ass,
        SubtitleFormat::Ssa,
        SubtitleFormat::Sbv,
    ] {
        let codec = SubtitlerCodec;
        let parsed = codec
            .parse(fixture(format), format)
            .unwrap_or_else(|e| panic!("parse {format} failed: {e}"));
        assert!(
            !parsed.cues.is_empty(),
            "{format}: expected at least one cue"
        );

        let written = codec
            .write(&parsed, format)
            .unwrap_or_else(|e| panic!("write {format} failed: {e}"));
        let reparsed = codec
            .write(&parsed, format)
            .and_then(|s| codec.parse(&s, format))
            .unwrap_or_else(|e| panic!("reparse {format} failed: {e}"));

        assert!(!written.is_empty(), "{format}: wrote empty output");
        assert_eq!(
            timing_and_text(&parsed),
            timing_and_text(&reparsed),
            "{format}: round-trip changed timings or text_raw"
        );
    }
}

// AC: M1-AC1
#[test]
fn first_cue_matches_absolute_fixture_values_for_each_format() {
    // Anchor the parse against the known fixture content so a uniform parse bug
    // (e.g. a constant timing offset) cannot hide behind self-consistent
    // round-trips. Every fixture's first cue is "Good morning, traveller." at
    // 1.000 → 3.000. Parsed via `parse_document` so ASS/SSA get the text-fidelity
    // overlay (ADR-024) that restores the post-comma space subtitler drops.
    for format in [
        SubtitleFormat::Vtt,
        SubtitleFormat::Ass,
        SubtitleFormat::Ssa,
        SubtitleFormat::Sbv,
    ] {
        let doc = parse_document(fixture(format), format)
            .unwrap_or_else(|e| panic!("parse {format}: {e}"));
        let first = &doc.cues[0];
        assert_eq!(first.start_ms, 1000, "{format}: start_ms");
        assert_eq!(first.end_ms, 3000, "{format}: end_ms");
        assert_eq!(
            first.text_raw, "Good morning, traveller.",
            "{format}: text_raw"
        );
    }
}

// AC: M1-AC1
#[test]
fn round_trip_preserves_multiline_cue_text() {
    // Cue 2 of the SRT fixture spans two lines; the newline must survive.
    let codec = SubtitlerCodec;
    let parsed = codec.parse(SRT, SubtitleFormat::Srt).expect("parse srt");
    let multiline = &parsed.cues[1];
    assert!(
        multiline.text_raw.contains('\n'),
        "expected a multiline cue in the fixture, got {:?}",
        multiline.text_raw
    );

    let reparsed = codec
        .write(&parsed, SubtitleFormat::Srt)
        .and_then(|s| codec.parse(&s, SubtitleFormat::Srt))
        .expect("round-trip srt");
    assert_eq!(parsed.cues[1].text_raw, reparsed.cues[1].text_raw);
}

// AC: M1-AC2
#[test]
fn ass_round_trip_preserves_script_info_and_styles_via_format_meta() {
    let codec = SubtitlerCodec;
    let parsed = codec.parse(ASS, SubtitleFormat::Ass).expect("parse ass");

    let meta = parsed
        .format_meta
        .as_ref()
        .expect("ASS document must carry format_meta");
    let script_info = meta.script_info.as_deref().expect("script_info present");
    let styles = meta.styles.as_deref().expect("styles present");
    assert!(
        script_info.contains("The Old Harbour"),
        "script info lost the Title: {script_info}"
    );
    assert!(
        styles.contains("Default") && styles.contains("Sign"),
        "style table lost a style name: {styles}"
    );

    // The same metadata must survive a parse → write → parse cycle, and the
    // serialized output must carry both style definitions and the title.
    let written = codec
        .write(&parsed, SubtitleFormat::Ass)
        .expect("write ass");
    assert!(written.contains("Style: Default,"), "missing Default style");
    assert!(written.contains("Style: Sign,"), "missing Sign style");
    assert!(written.contains("The Old Harbour"), "missing script title");

    let reparsed = codec.parse(&written, SubtitleFormat::Ass).expect("reparse");
    let meta2 = reparsed.format_meta.as_ref().expect("format_meta survives");
    let styles2 = meta2.styles.as_deref().unwrap();
    assert!(
        styles2.contains("Default") && styles2.contains("Sign"),
        "styles lost after round-trip: {styles2}"
    );
}

// AC: M1-AC2
#[test]
fn ass_cue_captures_style_and_speaker() {
    let codec = SubtitlerCodec;
    let parsed = codec.parse(ASS, SubtitleFormat::Ass).expect("parse ass");
    // First cue uses the Default style with actor "Guide".
    assert_eq!(parsed.cues[0].style.as_deref(), Some("Default"));
    assert_eq!(parsed.cues[0].speaker.as_deref(), Some("Guide"));
    // Second cue carries a lead override {\an8} and an inline {\i1} tag.
    assert_eq!(parsed.cues[1].lead_tags.as_deref(), Some("{\\an8}"));
    assert!(!parsed.cues[1].inline_tags.is_empty());
}

// --- ADR-024: ASS/SSA text-fidelity overlay ---------------------------------

const LEGACY_V4_SSA: &str = include_str!("fixtures/formats/legacy_v4.ssa");

/// Collects every cue's `text_raw` for order-sensitive comparisons.
fn cue_text_values(doc: &Document) -> Vec<String> {
    doc.cues.iter().map(|c| c.text_raw.clone()).collect()
}

// ADR-024: subtitler drops post-comma spaces; the overlay restores them for ASS.
#[test]
fn ass_dialogue_commas_and_tags_survive_via_overlay() {
    let doc = parse_document(ASS, SubtitleFormat::Ass).expect("parse ass");
    let texts = cue_text_values(&doc);
    assert!(
        texts.iter().any(|t| t == "Hello, world, and goodbye"),
        "multi-comma dialogue not preserved: {texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|t| t == "{\\i1}Wait{\\i0}, where are you going?"),
        "inline-tagged comma dialogue not preserved: {texts:?}"
    );

    // The faithful text must also survive parse → write → parse.
    let written = write_document(&doc, SubtitleFormat::Ass).expect("write ass");
    let reparsed = parse_document(&written, SubtitleFormat::Ass).expect("reparse ass");
    assert_eq!(
        cue_text_values(&doc),
        cue_text_values(&reparsed),
        "round-trip altered dialogue text"
    );
}

// ADR-024: after a splice, the derived masked fields must be re-derived from the
// corrected text_raw — not left as subtitler's stale masked output.
#[test]
fn overlay_rederives_masked_fields_from_corrected_text() {
    let doc = parse_document(ASS, SubtitleFormat::Ass).expect("parse ass");
    let cue = doc
        .cues
        .iter()
        .find(|c| c.text_raw == "{\\i1}Wait{\\i0}, where are you going?")
        .expect("inline-tag + comma cue present with faithful text");

    // The leading override block is the lead tag; the inner {\i0} is inline.
    assert_eq!(cue.lead_tags.as_deref(), Some("{\\i1}"));
    assert_eq!(cue.inline_tags, vec!["{\\i0}".to_string()]);
    // The restored post-comma space must appear in the masked text, and the
    // inline tag is replaced by the ⟦0⟧ placeholder.
    assert_eq!(
        cue.text_masked,
        "Wait\u{27E6}0\u{27E7}, where are you going?"
    );
    assert!(cue.translatable);
}

// ADR-024: the overlay also repairs the SSA format path.
#[test]
fn ssa_dialogue_commas_and_tags_survive_via_overlay() {
    let doc = parse_document(SSA, SubtitleFormat::Ssa).expect("parse ssa");
    let texts = cue_text_values(&doc);
    assert!(
        texts.iter().any(|t| t == "Hello, world, and goodbye"),
        "multi-comma dialogue not preserved: {texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|t| t == "{\\i1}Wait{\\i0}, where are you going?"),
        "inline-tagged comma dialogue not preserved: {texts:?}"
    );

    let written = write_document(&doc, SubtitleFormat::Ssa).expect("write ssa");
    let reparsed = parse_document(&written, SubtitleFormat::Ssa).expect("reparse ssa");
    assert_eq!(cue_text_values(&doc), cue_text_values(&reparsed));
}

// ADR-024: when the overlay cannot align (rsubs rejects legacy v4 events) the
// parse still succeeds with subtitler's text — no panic, no error.
#[test]
fn overlay_gracefully_keeps_subtitler_text_when_rsubs_cannot_parse() {
    // Sanity: rsubs really does reject this legacy-v4 SSA, so the overlay's
    // fallible second parse is exercised.
    assert!(
        myansub::formats::codec::RsubsCodec
            .parse(LEGACY_V4_SSA, SubtitleFormat::Ssa)
            .is_err(),
        "fixture must be unparseable by rsubs to exercise the graceful path"
    );

    let doc = parse_document(LEGACY_V4_SSA, SubtitleFormat::Ssa)
        .expect("parse must succeed despite overlay skip");
    // subtitler's text is kept, including its comma-space corruption.
    assert_eq!(doc.cues[0].text_raw, "Good morning,traveller.");
    assert_eq!(doc.cues[0].start_ms, 1000);
    assert_eq!(doc.cues[0].end_ms, 3000);
}

// ADR-024 must not disturb M1-AC2: format_meta still comes from subtitler.
#[test]
fn overlay_preserves_subtitler_format_meta() {
    let doc = parse_document(ASS, SubtitleFormat::Ass).expect("parse ass");
    let meta = doc.format_meta.as_ref().expect("format_meta present");
    assert!(
        meta.script_info
            .as_deref()
            .unwrap()
            .contains("The Old Harbour")
    );
    assert!(meta.styles.as_deref().unwrap().contains("Sign"));
}

// --- AC3: fallback injection via the generic chain seam ---------------------

/// A codec that always fails to parse — used as a stand-in primary.
struct AlwaysFails;

impl SubtitleCodec for AlwaysFails {
    fn parse(&self, _text: &str, format: SubtitleFormat) -> Result<Document, FormatError> {
        Err(FormatError::Parse {
            format,
            message: "injected primary failure".into(),
        })
    }
    fn write(&self, _doc: &Document, format: SubtitleFormat) -> Result<String, FormatError> {
        Err(FormatError::Write {
            format,
            message: "injected primary failure".into(),
        })
    }
}

/// A codec that records how many times it was asked to parse and always
/// succeeds with a single marker cue.
struct CountingOk<'a> {
    calls: &'a Cell<u32>,
}

impl SubtitleCodec for CountingOk<'_> {
    fn parse(&self, _text: &str, format: SubtitleFormat) -> Result<Document, FormatError> {
        self.calls.set(self.calls.get() + 1);
        Ok(Document {
            format,
            format_meta: None,
            cues: vec![Cue {
                idx: 1,
                start_ms: 0,
                end_ms: 1000,
                text_raw: "from fallback".into(),
                text_masked: "from fallback".into(),
                lead_tags: None,
                inline_tags: Vec::new(),
                style: None,
                speaker: None,
                translatable: true,
            }],
        })
    }
    fn write(&self, _doc: &Document, _format: SubtitleFormat) -> Result<String, FormatError> {
        Ok(String::new())
    }
}

// AC: M1-AC3
#[test]
fn chain_falls_back_to_secondary_when_primary_fails() {
    let calls = Cell::new(0);
    let fallback = CountingOk { calls: &calls };
    let doc = parse_with_codecs(&AlwaysFails, &fallback, SRT, SubtitleFormat::Srt)
        .expect("fallback should succeed");
    assert_eq!(calls.get(), 1, "fallback must be invoked exactly once");
    assert_eq!(doc.cues[0].text_raw, "from fallback");
}

// AC: M1-AC3
#[test]
fn chain_does_not_fall_back_for_sbv() {
    let calls = Cell::new(0);
    let fallback = CountingOk { calls: &calls };
    let err = parse_with_codecs(&AlwaysFails, &fallback, SBV, SubtitleFormat::Sbv)
        .expect_err("SBV must not fall back");
    assert!(
        matches!(err, FormatError::NoFallback { format } if format == SubtitleFormat::Sbv),
        "expected NoFallback for SBV, got {err:?}"
    );
    assert_eq!(calls.get(), 0, "fallback must not be called for SBV");
}

// AC: M1-AC3
#[test]
fn real_rsubs_fallback_parses_srt_when_primary_fails() {
    use myansub::formats::codec::RsubsCodec;
    // Prove the production fallback (rsubs-lib) really recovers SRT content.
    let doc = parse_with_codecs(&AlwaysFails, &RsubsCodec, SRT, SubtitleFormat::Srt)
        .expect("rsubs fallback should parse srt");
    assert_eq!(doc.cues.len(), 3);
    assert_eq!(doc.cues[0].text_raw, "Good morning, traveller.");
}

#[test]
fn parse_document_uses_primary_for_all_formats() {
    // The public entry parses every accepted format via the primary codec.
    for format in [
        SubtitleFormat::Srt,
        SubtitleFormat::Vtt,
        SubtitleFormat::Ass,
        SubtitleFormat::Ssa,
        SubtitleFormat::Sbv,
    ] {
        let doc = parse_document(fixture(format), format)
            .unwrap_or_else(|e| panic!("parse_document {format} failed: {e}"));
        assert_eq!(doc.format, format);
        assert!(!doc.cues.is_empty());
    }
}

/// A codec that always fails to parse with a tagged message, so tests can tell
/// which codec's error surfaced.
struct FailsWith(&'static str);

impl SubtitleCodec for FailsWith {
    fn parse(&self, _text: &str, format: SubtitleFormat) -> Result<Document, FormatError> {
        Err(FormatError::Parse {
            format,
            message: self.0.to_string(),
        })
    }
    fn write(&self, _doc: &Document, format: SubtitleFormat) -> Result<String, FormatError> {
        Err(FormatError::Write {
            format,
            message: self.0.to_string(),
        })
    }
}

// Fix #3: when both codecs fail on a non-SBV format, the richer primary
// (subtitler) error is surfaced, not the fallback's.
#[test]
fn chain_prefers_primary_error_when_both_fail() {
    let err = parse_with_codecs(
        &FailsWith("primary-context"),
        &FailsWith("fallback-context"),
        "garbage",
        SubtitleFormat::Srt,
    )
    .expect_err("both codecs fail");
    match err {
        FormatError::Parse { message, .. } => {
            assert_eq!(message, "primary-context", "expected the primary error");
        }
        other => panic!("expected FormatError::Parse, got {other:?}"),
    }
}

#[test]
fn parse_document_returns_typed_error_on_garbage_without_panic() {
    // A malformed VTT timestamp is rejected by both the primary (subtitler) and
    // the fallback (rsubs-lib). The chain must return a typed FormatError::Parse
    // carrying the format (no panic), and — per fix #3 — the surfaced message is
    // the primary's, not the fallback's (rsubs renders as "error at line N: …").
    let malformed = "WEBVTT\n\n1\n99:99 --> 00:00\nhi\n";
    let err = parse_document(malformed, SubtitleFormat::Vtt).expect_err("garbage must not parse");
    match err {
        FormatError::Parse { format, message } => {
            assert_eq!(format, SubtitleFormat::Vtt);
            assert!(
                !message.contains("error at line"),
                "expected the primary (subtitler) error, got fallback error: {message}"
            );
        }
        other => panic!("expected FormatError::Parse for Vtt, got {other:?}"),
    }
}

// --- Fix #1: content-first format detection (FR-1) --------------------------

#[test]
fn detect_prefers_content_over_extension() {
    // SRT content with a misleading or absent extension still detects as Srt.
    assert_eq!(detect_format(SRT, Some("txt")), Some(SubtitleFormat::Srt));
    assert_eq!(detect_format(SRT, Some("vtt")), Some(SubtitleFormat::Srt));
    assert_eq!(detect_format(SRT, None), Some(SubtitleFormat::Srt));
}

#[test]
fn detect_resolves_ass_vs_ssa_by_extension_tiebreak() {
    // ASS-shaped content (V4+ Styles) is ambiguous with an .ssa container: the
    // extension hint breaks the tie.
    assert_eq!(detect_format(ASS, Some("ssa")), Some(SubtitleFormat::Ssa));
    assert_eq!(detect_format(ASS, Some("ass")), Some(SubtitleFormat::Ass));
    assert_eq!(detect_format(ASS, None), Some(SubtitleFormat::Ass));
}

#[test]
fn detect_falls_back_to_extension_when_content_unrecognized() {
    let garbage = "not a subtitle at all\njust some prose\n";
    assert_eq!(
        detect_format(garbage, Some("srt")),
        Some(SubtitleFormat::Srt)
    );
    assert_eq!(
        detect_format(garbage, Some(".vtt")),
        Some(SubtitleFormat::Vtt)
    );
}

#[test]
fn detect_returns_none_for_unknown_content_without_hint() {
    let garbage = "not a subtitle at all\njust some prose\n";
    assert_eq!(detect_format(garbage, None), None);
    // An unknown extension hint cannot rescue unrecognizable content.
    assert_eq!(detect_format(garbage, Some("txt")), None);
}
