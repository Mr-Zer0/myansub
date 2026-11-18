//! Markup masking tests (M1-AC5).

use myansub::formats::markup::{self, Masked};

/// Reconstruct the original cue text from a mask result.
fn reconstruct(m: &Masked) -> String {
    let mut s = m.lead_tags.clone().unwrap_or_default();
    s.push_str(&markup::unmask(&m.text_masked, &m.inline_tags));
    s
}

/// Representative cue texts covering the tag styles in the fixtures:
/// ASS override blocks incl. lead `{\an8}`, SRT/VTT `<i>`/`<b>`/`<font>`,
/// and `{\i1}...{\i0}` toggles.
const FIXTURES: &[&str] = &[
    "Plain line with no tags",
    "{\\an8}Positioned line",
    "{\\an8\\pos(10,20)}Lead with multiple tags",
    "{\\i1}italic toggle{\\i0}",
    "Mixed {\\i1}emphasis{\\i0} in the middle",
    "<i>Hello</i> world",
    "<b>Bold</b> and <font color=\"red\">red</font>",
    "Line with <i>nested <b>styles</b></i> here",
    "{\\an8}{\\pos(1,2)}Two leading blocks",
    "Café — “quoted” text",
    "မင်္ဂလာပါ ကမ္ဘာ",
    "{\\an5}",
    "♪",
    "No close { brace stays literal",
];

// AC: M1-AC5
#[test]
fn mask_then_reconstruct_is_identity_for_all_fixtures() {
    for original in FIXTURES {
        let m = markup::mask(original);
        assert_eq!(
            &reconstruct(&m),
            original,
            "round-trip failed for {original:?}"
        );
    }
}

// AC: M1-AC5
#[test]
fn round_trip_identity_with_literal_placeholder_brackets() {
    // Source text that itself contains placeholder-shaped literals must still
    // round-trip exactly (no corruption of the literal into a tag).
    let cases = [
        "⟦0⟧<i>x</i>",
        "⟦0⟧ plain ⟦1⟧",
        "a lone ⟦ bracket",
        "a lone ⟧ bracket",
        "⟦⟧ empty pair",
        "{\\an8}⟦0⟧ lead plus literal <i>y</i>",
    ];
    for original in cases {
        let m = markup::mask(original);
        assert_eq!(
            &reconstruct(&m),
            original,
            "round-trip failed for {original:?}"
        );
    }
}

// AC: M1-AC5
#[test]
fn angle_bracket_prose_is_not_masked() {
    let m = markup::mask("a < b > c");
    assert_eq!(m.text_masked, "a < b > c");
    assert!(m.inline_tags.is_empty());
    assert!(m.translatable);

    // Genuine markup is still recognized.
    let tag = markup::mask("<i>hi</i>");
    assert_eq!(tag.text_masked, "⟦0⟧hi⟦1⟧");
    assert_eq!(tag.inline_tags, vec!["<i>".to_string(), "</i>".to_string()]);
}

// AC: M1-AC5
#[test]
fn placeholder_count_matches_inline_tag_count() {
    for original in FIXTURES {
        let m = markup::mask(original);
        let placeholders = m.text_masked.matches('\u{27E6}').count();
        assert_eq!(
            placeholders,
            m.inline_tags.len(),
            "placeholder/inline-tag count mismatch for {original:?}"
        );
    }
}

// AC: M1-AC5
#[test]
fn lead_block_is_extracted_and_removed_from_masked_text() {
    let m = markup::mask("{\\an8\\pos(10,20)}Hello");
    assert_eq!(m.lead_tags.as_deref(), Some("{\\an8\\pos(10,20)}"));
    assert_eq!(m.text_masked, "Hello");
    assert!(m.inline_tags.is_empty());
}

// AC: M1-AC5
#[test]
fn consecutive_leading_blocks_are_all_treated_as_lead() {
    let m = markup::mask("{\\an8}{\\pos(1,2)}Text");
    assert_eq!(m.lead_tags.as_deref(), Some("{\\an8}{\\pos(1,2)}"));
    assert_eq!(m.text_masked, "Text");
    assert!(m.inline_tags.is_empty());
}

// AC: M1-AC5
#[test]
fn inline_tags_are_numbered_zero_based_per_cue() {
    let m = markup::mask("<i>Hello</i> world");
    assert_eq!(m.lead_tags, None);
    assert_eq!(m.text_masked, "⟦0⟧Hello⟦1⟧ world");
    assert_eq!(m.inline_tags, vec!["<i>".to_string(), "</i>".to_string()]);
}

// AC: M1-AC5
#[test]
fn trailing_toggle_after_lead_is_masked_inline() {
    let m = markup::mask("{\\i1}emph{\\i0}");
    assert_eq!(m.lead_tags.as_deref(), Some("{\\i1}"));
    assert_eq!(m.text_masked, "emph⟦0⟧");
    assert_eq!(m.inline_tags, vec!["{\\i0}".to_string()]);
}

// AC: M1-AC5
#[test]
fn unmask_restores_placeholders_in_order() {
    let tags = vec!["<i>".to_string(), "</i>".to_string()];
    assert_eq!(markup::unmask("⟦0⟧Hi⟦1⟧", &tags), "<i>Hi</i>");
}

// AC: M1-AC5
#[test]
fn unmask_leaves_unknown_placeholders_untouched() {
    let tags = vec!["<i>".to_string()];
    assert_eq!(markup::unmask("⟦0⟧x⟦5⟧", &tags), "<i>x⟦5⟧");
}

// AC: M1-AC5
#[test]
fn translatable_true_when_letters_remain() {
    assert!(markup::is_translatable("Hello"));
    assert!(markup::is_translatable("<i>Hello</i>"));
    assert!(markup::is_translatable("မင်္ဂလာပါ"));
    assert!(markup::is_translatable("Café"));
}

// AC: M1-AC5
#[test]
fn translatable_false_without_letters() {
    assert!(!markup::is_translatable("..."));
    assert!(!markup::is_translatable("♪"));
    assert!(!markup::is_translatable("{\\i1}"));
    assert!(!markup::is_translatable("{\\an8}"));
    assert!(!markup::is_translatable("123"));
    assert!(!markup::is_translatable(""));
}

// AC: M1-AC5
#[test]
fn mask_sets_translatable_flag() {
    assert!(markup::mask("Hello {\\i1}world{\\i0}").translatable);
    assert!(!markup::mask("{\\an8}").translatable);
    assert!(!markup::mask("<i></i>").translatable);
}
