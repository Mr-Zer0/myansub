//! Deterministic quality-check tests (M6-AC4).
//!
//! Each check has a positive and a negative case, using valid Unicode Burmese
//! samples and Zawgyi-encoded samples. Everything here is offline and pure.
//!
//! ## Burmese / Zawgyi samples
//!
//! Burmese Unicode stores a syllable as `consonant + vowel`, so the vowel sign
//! U+1031 ( "ေ") appears *after* its consonant: "ကေ" = U+1000 U+1031. Zawgyi, a
//! non-Unicode legacy encoding, stores that same vowel *before* the consonant,
//! so it lands at the start of a word: "ေက" = U+1031 U+1000. Zawgyi also reuses
//! the U+1060–U+1097 range for stacked/variant glyphs that Unicode encodes with
//! the virama U+1039. The samples below are built from explicit code points so
//! the test is deterministic and needs no copyrighted material.

use myansub::domain::{Cue, DigitStyle, QualityFlag};
use myansub::quality::checks::{
    self, AnalysisContext, GlossaryTerm, Limits, check_batch_drift, check_cue,
};

const OPEN: char = '\u{27E6}'; // ⟦
const CLOSE: char = '\u{27E7}'; // ⟧

fn limits() -> Limits {
    Limits {
        max_graphemes_per_line: 42,
        max_lines: 2,
        max_gps: 20.0,
        drift_ratio: 3.0,
    }
}

/// Builds a cue with the given masked text and timing. `translatable` defaults to
/// true via [`cue`]; use [`cue_timed`] for timing-sensitive checks.
fn cue(text_masked: &str) -> Cue {
    cue_full(text_masked, true, 0, 2000)
}

fn cue_timed(text_masked: &str, start_ms: u64, end_ms: u64) -> Cue {
    cue_full(text_masked, true, start_ms, end_ms)
}

fn cue_full(text_masked: &str, translatable: bool, start_ms: u64, end_ms: u64) -> Cue {
    Cue {
        idx: 1,
        start_ms,
        end_ms,
        text_raw: text_masked.to_string(),
        text_masked: text_masked.to_string(),
        lead_tags: None,
        inline_tags: Vec::new(),
        style: None,
        speaker: None,
        translatable,
    }
}

/// A placeholder token `⟦n⟧`.
fn ph(n: u32) -> String {
    format!("{OPEN}{n}{CLOSE}")
}

// --- empty -----------------------------------------------------------------

// AC: M6-AC4
#[test]
fn empty_flags_translatable_cue_with_blank_target() {
    let src = cue("Hello");
    assert!(checks::empty_target(&src, ""));
    assert!(checks::empty_target(&src, "   \n  "));
    assert_eq!(
        check_cue(&src, "", &ctx_empty(), &limits()),
        vec![QualityFlag::Empty]
    );
}

// AC: M6-AC4
#[test]
fn empty_not_flagged_for_nonempty_target() {
    let src = cue("Hello");
    assert!(!checks::empty_target(&src, "မင်္ဂလာပါ"));
}

// AC: M6-AC4
#[test]
fn empty_not_flagged_for_nontranslatable_cue() {
    let src = cue_full("♪", false, 0, 2000);
    assert!(!checks::empty_target(&src, ""));
    assert!(check_cue(&src, "", &ctx_empty(), &limits()).is_empty());
}

// --- placeholder_mismatch --------------------------------------------------

// AC: M6-AC4
#[test]
fn placeholder_mismatch_flags_missing_placeholder() {
    let src = cue(&format!("{} hi {}", ph(0), ph(1)));
    let target = format!("{} ဟိုင်း", ph(0)); // ⟦1⟧ dropped
    assert!(checks::placeholder_mismatch(&src, &target));
}

// AC: M6-AC4
#[test]
fn placeholder_mismatch_order_independent() {
    let src = cue(&format!("{} hi {}", ph(0), ph(1)));
    let target = format!("{} ဟိုင်း {}", ph(1), ph(0)); // both, reordered
    assert!(!checks::placeholder_mismatch(&src, &target));
}

// AC: M6-AC4
#[test]
fn placeholder_mismatch_flags_duplicate_as_multiset() {
    let src = cue(&format!("{} hi {}", ph(0), ph(1)));
    let target = format!("{} {} hi {}", ph(0), ph(0), ph(1)); // ⟦0⟧ twice
    assert!(checks::placeholder_mismatch(&src, &target));
}

// --- untranslated ----------------------------------------------------------

// AC: M6-AC4
#[test]
fn untranslated_flags_latin_only_target() {
    let src = cue("Where are you going?");
    assert!(checks::untranslated(
        &src,
        "Where are you going?",
        &ctx_empty()
    ));
}

// AC: M6-AC4
#[test]
fn untranslated_not_flagged_for_burmese_target() {
    let src = cue("Where are you going?");
    assert!(!checks::untranslated(&src, "ဘယ်သွားမလို့လဲ", &ctx_empty()));
}

// AC: M6-AC4
#[test]
fn untranslated_not_flagged_for_keep_glossary_term() {
    let src = cue("Tokyo");
    let glossary = [GlossaryTerm {
        term: "Tokyo",
        target: "Tokyo",
        keep: true,
    }];
    let ctx = AnalysisContext {
        glossary: &glossary,
        names: &[],
        name_targets: &[],
        digits: DigitStyle::Myanmar,
    };
    assert!(!checks::untranslated(&src, "Tokyo", &ctx));
}

// AC: M6-AC4
#[test]
fn untranslated_not_flagged_for_known_character_name() {
    let src = cue("Aoi");
    let names = ["Aoi", "Yuki"];
    let ctx = AnalysisContext {
        glossary: &[],
        names: &names,
        name_targets: &[],
        digits: DigitStyle::Myanmar,
    };
    assert!(!checks::untranslated(&src, "Aoi", &ctx));
}

// --- zawgyi_suspect --------------------------------------------------------

// AC: M6-AC4
#[test]
fn zawgyi_suspect_flags_leading_vowel_sign() {
    // "ေက" — U+1031 before its consonant (Zawgyi storage order).
    let zawgyi = "\u{1031}\u{1000}";
    assert!(checks::zawgyi_suspect(zawgyi));
}

// AC: M6-AC4
#[test]
fn zawgyi_suspect_not_flagged_for_valid_unicode_equivalent() {
    // "ကေ" — U+1000 then U+1031 (correct Unicode order).
    let unicode = "\u{1000}\u{1031}";
    assert!(!checks::zawgyi_suspect(unicode));
}

// AC: M6-AC4
#[test]
fn zawgyi_suspect_flags_reused_codepoint_range() {
    // U+1060 inside Burmese text: Zawgyi reuses U+1060–U+1097.
    let zawgyi = "\u{1000}\u{1060}\u{1001}";
    assert!(checks::zawgyi_suspect(zawgyi));
    // Unicode stacks with the virama U+1039 instead — not flagged.
    let unicode = "\u{1000}\u{1039}\u{1001}";
    assert!(!checks::zawgyi_suspect(unicode));
}

// AC: M6-AC4
#[test]
fn zawgyi_suspect_signal2_requires_burmese_core() {
    // The "otherwise Burmese" qualifier is real: a U+1060–1097 band code point
    // amidst Burmese-CORE text trips Signal 2 ...
    let amid_core = "\u{1000}\u{1060}";
    assert!(checks::zawgyi_suspect(amid_core));
    // ... but a stray band code point with no Burmese-core context does not
    // (the band overlaps the Myanmar block, so this must not auto-fire).
    let stray = "abc \u{1060} def";
    assert!(!checks::zawgyi_suspect(stray));
}

// --- boundary_drift --------------------------------------------------------

// AC: M6-AC4
#[test]
fn boundary_drift_flags_outlier_next_to_short_neighbor() {
    let sources: Vec<Cue> = (0..5).map(|_| cue("aaaaaaaaaa")).collect(); // 10 each
    let targets = [
        "bbbbbbbbbb", // ratio 1.0
        "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc\
         cccccccccccccccccccccccccccccccccccc", // ratio ~10
        "d",          // ratio 0.1 (short)
        "eeeeeeeeee", // ratio 1.0
        "ffffffffff", // ratio 1.0
    ];
    let batch: Vec<(&Cue, &str)> = sources.iter().zip(targets).collect();
    let drift = check_batch_drift(&batch, &limits());
    assert_eq!(drift, vec![false, true, false, false, false]);
}

// AC: M6-AC4
#[test]
fn boundary_drift_not_flagged_for_uniform_batch() {
    let sources: Vec<Cue> = (0..5).map(|_| cue("aaaaaaaaaa")).collect();
    let targets = [
        "bbbbbbbbbb",
        "cccccccccc",
        "dddddddddd",
        "eeeeeeeeee",
        "ffffffffff",
    ];
    let batch: Vec<(&Cue, &str)> = sources.iter().zip(targets).collect();
    let drift = check_batch_drift(&batch, &limits());
    assert_eq!(drift, vec![false; 5]);
}

// AC: M6-AC4
#[test]
fn boundary_drift_empty_batch_returns_empty() {
    assert_eq!(check_batch_drift(&[], &limits()), Vec::<bool>::new());
}

// AC: M6-AC4
#[test]
fn boundary_drift_single_element_batch_never_flags() {
    let src = cue("aaaaaaaaaa");
    let batch: Vec<(&Cue, &str)> = vec![(&src, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")];
    assert_eq!(check_batch_drift(&batch, &limits()), vec![false]);
}

// AC: M6-AC4
#[test]
fn boundary_drift_excludes_empty_source_from_median() {
    // A cue with an empty source (text_masked == "") sits between the short cue
    // and a normal cue. Its ratio is undefined and must be excluded: the cue is
    // never flagged and does not join the median. A regression that instead fed
    // `target_len / 0 = inf` into the computation would flag the empty cue
    // (inf is "far" and it neighbors the short cue), flipping index 2 to true.
    let long = cue("aaaaaaaaaa"); // src 10, target 100 → ratio 10 (outlier)
    let short = cue("aaaaaaaaaa"); // src 10, target 1 → ratio 0.1 (short)
    let empty_src = cue(""); // excluded
    let a = cue("aaaaaaaaaa"); // ratio 1.0
    let b = cue("aaaaaaaaaa"); // ratio 1.0
    let long_target = "c".repeat(100);
    let batch: Vec<(&Cue, &str)> = vec![
        (&long, long_target.as_str()),
        (&short, "d"),
        (&empty_src, "zzzzzzzzzz"),
        (&a, "eeeeeeeeee"),
        (&b, "ffffffffff"),
    ];
    let drift = check_batch_drift(&batch, &limits());
    // Only the long outlier (index 0, neighbor of the short cue) is flagged; the
    // empty-source cue at index 2 stays false.
    assert_eq!(drift, vec![true, false, false, false, false]);
    // Baseline without the empty cue: the real cues flag identically.
    let baseline: Vec<(&Cue, &str)> = vec![
        (&long, long_target.as_str()),
        (&short, "d"),
        (&a, "eeeeeeeeee"),
        (&b, "ffffffffff"),
    ];
    assert_eq!(
        check_batch_drift(&baseline, &limits()),
        vec![true, false, false, false]
    );
}

// --- glossary_miss ---------------------------------------------------------

// AC: M6-AC4
#[test]
fn glossary_miss_flags_missing_target_term() {
    let src = cue("The Demon King appears.");
    let glossary = [GlossaryTerm {
        term: "Demon King",
        target: "နတ်ဆိုးဘုရင်",
        keep: false,
    }];
    let ctx = AnalysisContext {
        glossary: &glossary,
        names: &[],
        name_targets: &[],
        digits: DigitStyle::Myanmar,
    };
    // Target omits the glossary target entirely.
    assert!(checks::glossary_miss(&src, "ဘုရင်ပေါ်လာတယ်", &ctx));
}

// AC: M6-AC4
#[test]
fn glossary_miss_not_flagged_when_target_present() {
    let src = cue("The Demon King appears.");
    let glossary = [GlossaryTerm {
        term: "Demon King",
        target: "နတ်ဆိုးဘုရင်",
        keep: false,
    }];
    let ctx = AnalysisContext {
        glossary: &glossary,
        names: &[],
        name_targets: &[],
        digits: DigitStyle::Myanmar,
    };
    assert!(!checks::glossary_miss(&src, "နတ်ဆိုးဘုရင် ပေါ်လာတယ်", &ctx));
}

// AC: M12-AC23 — the source cue names a character but the target lacks the matching
// `name_target` spelling → `glossary_miss` fires (name coverage, ADR-049(d)).
#[test]
fn glossary_miss_flags_name_without_name_target() {
    let src = cue("Aoi runs away.");
    let name_targets = [("Aoi".to_string(), "အာအိုအိ".to_string())];
    let ctx = AnalysisContext {
        glossary: &[],
        names: &[],
        name_targets: &name_targets,
        digits: DigitStyle::Myanmar,
    };
    // Target translates the sentence but omits the name's Burmese spelling.
    assert!(checks::glossary_miss(&src, "ထွက်ပြေးသွားတယ်", &ctx));
}

// AC: M12-AC23 — the target contains the matching `name_target` spelling → no flag.
#[test]
fn glossary_miss_not_flagged_when_name_target_present() {
    let src = cue("Aoi runs away.");
    let name_targets = [("Aoi".to_string(), "အာအိုအိ".to_string())];
    let ctx = AnalysisContext {
        glossary: &[],
        names: &[],
        name_targets: &name_targets,
        digits: DigitStyle::Myanmar,
    };
    assert!(!checks::glossary_miss(&src, "အာအိုအိ ထွက်ပြေးသွားတယ်", &ctx));
}

// AC: M12-AC23 — name coverage also applies to an alias → its `aliases_target`
// spelling must appear in the target.
#[test]
fn glossary_miss_flags_alias_without_alias_target() {
    let src = cue("X-man waves.");
    let name_targets = [("X-man".to_string(), "အိတ်စ်မန်".to_string())];
    let ctx = AnalysisContext {
        glossary: &[],
        names: &[],
        name_targets: &name_targets,
        digits: DigitStyle::Myanmar,
    };
    assert!(checks::glossary_miss(&src, "လက်ပြတယ်", &ctx));
    assert!(!checks::glossary_miss(&src, "အိတ်စ်မန် လက်ပြတယ်", &ctx));
}

// --- digit_style -----------------------------------------------------------

// AC: M12-AC12 — with `digits = myanmar`, a Western digit in the target fires
// `digit_style`; a target using Myanmar digits does not (ADR-049(e), LLM.md §8).
#[test]
fn digit_style_flags_western_digit_when_myanmar_chosen() {
    // Target contains Western "3" — the non-chosen system.
    assert!(checks::digit_style("အခန်း 3 မှာ", DigitStyle::Myanmar));
    // Target uses the Myanmar digit ၃ (U+1043) — the chosen system.
    assert!(!checks::digit_style("အခန်း ၃ မှာ", DigitStyle::Myanmar));
    // No digits at all — never flagged.
    assert!(!checks::digit_style("အခန်းထဲမှာ", DigitStyle::Myanmar));
}

// AC: M12-AC12 — with `digits = western`, a Myanmar digit in the target fires
// `digit_style`; a Western-digit target does not (ADR-049(e), LLM.md §8).
#[test]
fn digit_style_flags_myanmar_digit_when_western_chosen() {
    assert!(checks::digit_style("chapter ၃", DigitStyle::Western));
    assert!(!checks::digit_style("chapter 3", DigitStyle::Western));
}

// AC: M12-AC12 — digits *inside* a `⟦n⟧` placeholder do NOT trigger the check: the
// placeholder index is masked markup, not translated content (ADR-049(e)).
#[test]
fn digit_style_ignores_digits_inside_placeholders() {
    // `⟦0⟧` holds a Western "0", but it is placeholder markup, so with myanmar
    // chosen and no other Western digit, the check must not fire.
    let target = format!("{} အခန်း ၃", ph(0));
    assert!(!checks::digit_style(&target, DigitStyle::Myanmar));
    // A Western digit OUTSIDE the placeholder still fires.
    let target_bad = format!("{} အခန်း 3", ph(0));
    assert!(checks::digit_style(&target_bad, DigitStyle::Myanmar));
}

// AC: M12-AC12 — `digit_style` joins the flags returned by `check_cue` when the
// target uses the non-chosen digit system (ADR-049(e), LLM.md §8).
#[test]
fn check_cue_includes_digit_style_flag() {
    let src = cue("Room 3");
    let ctx = AnalysisContext {
        glossary: &[],
        names: &[],
        name_targets: &[],
        digits: DigitStyle::Myanmar,
    };
    let flags = check_cue(&src, "အခန်း 3", &ctx, &limits());
    assert!(
        flags.contains(&QualityFlag::DigitStyle),
        "check_cue should surface digit_style, got: {flags:?}"
    );
}

// --- too_long --------------------------------------------------------------

// AC: M6-AC4
#[test]
fn too_long_counts_grapheme_clusters_not_chars() {
    // "က်" = U+1000 + U+103A (asat) is ONE grapheme cluster of two code points.
    let cluster = "\u{1000}\u{103A}";
    // 42 clusters = 42 graphemes but 84 chars: within the limit (proves grapheme
    // counting; a char/byte count would wrongly flag this).
    let within = cluster.repeat(42);
    assert!(!checks::too_long(&within, &limits()));
    // 43 clusters = 43 graphemes: over the limit.
    let over = cluster.repeat(43);
    assert!(checks::too_long(&over, &limits()));
}

// AC: M6-AC4
#[test]
fn too_long_flags_excess_line_count() {
    let three_lines = "တစ်\nနှစ်\nသုံး"; // 3 lines > max_lines (2)
    assert!(checks::too_long(three_lines, &limits()));
    let two_lines = "တစ်\nနှစ်";
    assert!(!checks::too_long(two_lines, &limits()));
}

// --- too_fast --------------------------------------------------------------

// AC: M6-AC4
#[test]
fn too_fast_flags_high_grapheme_rate() {
    let src = cue_timed("x", 0, 1000); // 1 second window
    let target = "က".repeat(30); // 30 graphemes / 1s = 30 gps > 20
    assert!(checks::too_fast(&src, &target, &limits()));
}

// AC: M6-AC4
#[test]
fn too_fast_not_flagged_for_comfortable_timing() {
    let src = cue_timed("x", 0, 1000);
    let target = "က".repeat(10); // 10 gps < 20
    assert!(!checks::too_fast(&src, &target, &limits()));
}

// AC: M6-AC4
#[test]
fn too_fast_handles_zero_and_one_ms_duration_without_panic() {
    let zero = cue_timed("x", 5000, 5000); // zero duration
    assert!(!checks::too_fast(&zero, &"က".repeat(10), &limits()));
    let one_ms = cue_timed("x", 0, 1); // 1 ms, extreme rate but must not panic
    let _ = checks::too_fast(&one_ms, &"က".repeat(10), &limits());
}

fn ctx_empty() -> AnalysisContext<'static> {
    AnalysisContext::default()
}
