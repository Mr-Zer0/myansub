//! Domain type tests: format parsing/display/extension and flag serde.

use std::str::FromStr;

use myansub::domain::{Cue, Document, FormatMeta, QualityFlag, SubtitleFormat};

#[test]
fn subtitle_format_round_trips_through_str() {
    for (label, fmt) in [
        ("srt", SubtitleFormat::Srt),
        ("vtt", SubtitleFormat::Vtt),
        ("ass", SubtitleFormat::Ass),
        ("ssa", SubtitleFormat::Ssa),
        ("sbv", SubtitleFormat::Sbv),
    ] {
        assert_eq!(SubtitleFormat::from_str(label).unwrap(), fmt);
        assert_eq!(fmt.to_string(), label);
        assert_eq!(fmt.extension(), label);
    }
}

#[test]
fn subtitle_format_parsing_is_case_insensitive_with_aliases() {
    assert_eq!(
        SubtitleFormat::from_str("SRT").unwrap(),
        SubtitleFormat::Srt
    );
    assert_eq!(
        SubtitleFormat::from_str("WebVTT").unwrap(),
        SubtitleFormat::Vtt
    );
    assert!(SubtitleFormat::from_str("mkv").is_err());
}

#[test]
fn quality_flags_serialize_as_snake_case() {
    assert_eq!(
        serde_json::to_string(&QualityFlag::PlaceholderMismatch).unwrap(),
        "\"placeholder_mismatch\""
    );
    assert_eq!(
        serde_json::to_string(&QualityFlag::ZawgyiSuspect).unwrap(),
        "\"zawgyi_suspect\""
    );
    let back: QualityFlag = serde_json::from_str("\"too_fast\"").unwrap();
    assert_eq!(back, QualityFlag::TooFast);
}

#[test]
fn document_round_trips_through_json() {
    let doc = Document {
        format: SubtitleFormat::Ass,
        format_meta: Some(FormatMeta {
            script_info: Some("[Script Info]".to_string()),
            styles: None,
        }),
        cues: vec![Cue {
            idx: 1,
            start_ms: 1000,
            end_ms: 2500,
            text_raw: "{\\an8}Hello ⟦0⟧".to_string(),
            text_masked: "Hello ⟦0⟧".to_string(),
            lead_tags: Some("{\\an8}".to_string()),
            inline_tags: vec!["<i>".to_string()],
            style: None,
            speaker: Some("c1".to_string()),
            translatable: true,
        }],
    };
    let json = serde_json::to_string(&doc).unwrap();
    let back: Document = serde_json::from_str(&json).unwrap();
    assert_eq!(doc, back);
}
