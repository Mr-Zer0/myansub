//! Domain type tests: format parsing/display/extension and flag serde.

use std::str::FromStr;

use myansub::domain::{
    Cue, Document, FormatMeta, ModelRef, ParseModelRefError, Provider, QualityFlag, SubtitleFormat,
};

// AC: M13-AC1 — a model reference with an explicit `provider:` prefix routes to
// that provider and strips the prefix from the wire model id.
#[test]
fn model_ref_parses_explicit_provider_prefix() {
    let r = ModelRef::parse("openrouter:anthropic/claude-sonnet-5.5", Provider::Gemini).unwrap();
    assert_eq!(r.provider, Provider::OpenRouter);
    assert_eq!(r.model, "anthropic/claude-sonnet-5.5");
    assert_eq!(r.as_ref_string(), "openrouter:anthropic/claude-sonnet-5.5");

    let g = ModelRef::parse("gemini:gemini-flash-latest", Provider::OpenRouter).unwrap();
    assert_eq!(g.provider, Provider::Gemini);
    assert_eq!(g.model, "gemini-flash-latest");
}

// AC: M13-AC1 — a reference with no provider prefix uses the default provider,
// and an id that merely contains a `/` (an OpenRouter-style id) is not mistaken
// for a prefix.
#[test]
fn model_ref_without_prefix_uses_default() {
    let r = ModelRef::parse("gemini-flash-latest", Provider::Gemini).unwrap();
    assert_eq!(r.provider, Provider::Gemini);
    assert_eq!(r.model, "gemini-flash-latest");

    let d = ModelRef::parse("anthropic/claude-sonnet-5.5", Provider::OpenRouter).unwrap();
    assert_eq!(d.provider, Provider::OpenRouter);
    assert_eq!(d.model, "anthropic/claude-sonnet-5.5");
}

// AC: M13-AC1 — an OpenRouter id with a `:free` suffix keeps its colon and is a
// bare model under the default provider, not a provider claim.
#[test]
fn model_ref_free_suffix_is_not_a_provider_prefix() {
    let r = ModelRef::parse(
        "meta-llama/llama-3.1-8b-instruct:free",
        Provider::OpenRouter,
    )
    .unwrap();
    assert_eq!(r.provider, Provider::OpenRouter);
    assert_eq!(r.model, "meta-llama/llama-3.1-8b-instruct:free");

    // provider prefix + a model that itself carries a :free suffix
    let p = ModelRef::parse(
        "openrouter:meta-llama/llama-3.1-8b-instruct:free",
        Provider::Gemini,
    )
    .unwrap();
    assert_eq!(p.provider, Provider::OpenRouter);
    assert_eq!(p.model, "meta-llama/llama-3.1-8b-instruct:free");
}

// AC: M13-AC1 — an unknown provider prefix is rejected (main maps it to exit 2).
#[test]
fn model_ref_rejects_unknown_provider() {
    let err = ModelRef::parse("bogus:some-model", Provider::Gemini).unwrap_err();
    assert_eq!(
        err,
        ParseModelRefError::UnknownProvider("bogus".to_string())
    );
}

// AC: M13-AC1 — provider labels parse and display canonically.
#[test]
fn provider_round_trips() {
    assert_eq!("gemini".parse::<Provider>().unwrap(), Provider::Gemini);
    assert_eq!(
        "openrouter".parse::<Provider>().unwrap(),
        Provider::OpenRouter
    );
    assert_eq!(Provider::OpenRouter.label(), "openrouter");
    assert!("nope".parse::<Provider>().is_err());
}

// AC: M13-AC4 — the `refused` quality flag round-trips as snake_case.
#[test]
fn refused_flag_serializes_as_snake_case() {
    let json = serde_json::to_string(&QualityFlag::Refused).unwrap();
    assert_eq!(json, "\"refused\"");
    let back: QualityFlag = serde_json::from_str("\"refused\"").unwrap();
    assert_eq!(back, QualityFlag::Refused);
}

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
