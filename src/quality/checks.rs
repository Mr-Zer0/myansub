//! Deterministic quality checks (`docs/LLM.md` §8).
//!
//! Pure functions over `(source cue, translated text, analysis context, limits)`.
//! Every per-cue check is infallible and returns a `bool`; [`check_cue`] runs the
//! whole set and returns the [`QualityFlag`]s that fired. The batch-level
//! [`check_batch_drift`] inspects a window of cues for `boundary_drift`.
//!
//! The `failed` flag is **not** produced here; the pipeline sets it when a batch
//! exhausts its content retries (`docs/LLM.md` §6).

use unicode_segmentation::UnicodeSegmentation;

use crate::domain::{Cue, QualityFlag};

const PLACEHOLDER_OPEN: char = '\u{27E6}'; // ⟦
const PLACEHOLDER_CLOSE: char = '\u{27E7}'; // ⟧

/// Configurable thresholds for the length and timing checks (`[quality]` in
/// `docs/CONFIG.md`). Kept independent of [`crate::config`] so this module's only
/// crate dependency is `domain` (pure/reusable per ADR-010). The config→`Limits`
/// mapping lives on the config side (`config::Quality`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub max_graphemes_per_line: u32,
    pub max_lines: u32,
    pub max_gps: f64,
    pub drift_ratio: f64,
}

/// A glossary entry, reduced to the fields the checks need (`docs/DATA_MODEL.md`
/// §3). Borrowed so callers can build a context without copying the analysis.
#[derive(Debug, Clone, Copy)]
pub struct GlossaryTerm<'a> {
    pub term: &'a str,
    pub target: &'a str,
    pub keep: bool,
}

/// The slice of analysis a cue check consults: glossary terms and the set of
/// character names/aliases that may legitimately stay untranslated.
#[derive(Debug, Clone, Copy, Default)]
pub struct AnalysisContext<'a> {
    pub glossary: &'a [GlossaryTerm<'a>],
    /// Character names and aliases (`docs/DATA_MODEL.md` §3 `characters`).
    pub names: &'a [&'a str],
}

/// True when `c` is a Myanmar code point in any of the three ranges used for
/// Burmese: main (U+1000–U+109F), Extended-A (U+AA60–U+AA7F), Extended-B
/// (U+A9E0–U+A9FF) (`docs/LLM.md` §8).
fn is_myanmar(c: char) -> bool {
    matches!(c as u32,
        0x1000..=0x109F | 0xAA60..=0xAA7F | 0xA9E0..=0xA9FF)
}

fn grapheme_count(s: &str) -> usize {
    s.graphemes(true).count()
}

/// Extracts the placeholder indices (`⟦n⟧`) in order of appearance, honoring the
/// doubled-bracket escape used by `formats::markup`.
fn placeholders(s: &str) -> Vec<u32> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            PLACEHOLDER_OPEN => {
                if chars.get(i + 1) == Some(&PLACEHOLDER_OPEN) {
                    // Escaped literal `⟦`.
                    i += 2;
                    continue;
                }
                let mut j = i + 1;
                let mut digits = String::new();
                while j < chars.len() && chars[j].is_ascii_digit() {
                    digits.push(chars[j]);
                    j += 1;
                }
                if !digits.is_empty()
                    && chars.get(j) == Some(&PLACEHOLDER_CLOSE)
                    && let Ok(n) = digits.parse::<u32>()
                {
                    out.push(n);
                    i = j + 1;
                    continue;
                }
                i += 1;
            }
            PLACEHOLDER_CLOSE => {
                if chars.get(i + 1) == Some(&PLACEHOLDER_CLOSE) {
                    // Escaped literal `⟧`.
                    i += 1;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    out
}

/// `empty`: a translatable cue whose target is blank (`docs/LLM.md` §8). A
/// non-translatable cue is never flagged for an empty target.
pub fn empty_target(source: &Cue, target: &str) -> bool {
    source.translatable && target.trim().is_empty()
}

/// `placeholder_mismatch`: the multiset of `⟦n⟧` placeholders in the target
/// differs from the source (order-independent, count-sensitive).
pub fn placeholder_mismatch(source: &Cue, target: &str) -> bool {
    let mut src = placeholders(&source.text_masked);
    let mut tgt = placeholders(target);
    src.sort_unstable();
    tgt.sort_unstable();
    src != tgt
}

/// `untranslated`: a translatable cue whose target contains no Myanmar code
/// point, unless the source is a `keep` glossary term or a known character
/// name/alias (`docs/LLM.md` §8).
pub fn untranslated(source: &Cue, target: &str, ctx: &AnalysisContext) -> bool {
    if !source.translatable {
        return false;
    }
    if target.chars().any(is_myanmar) {
        return false;
    }
    let src = source.text_masked.trim();
    if ctx
        .glossary
        .iter()
        .any(|g| g.keep && g.term.eq_ignore_ascii_case(src))
    {
        return false;
    }
    if ctx.names.iter().any(|n| n.eq_ignore_ascii_case(src)) {
        return false;
    }
    true
}

/// `zawgyi_suspect`: the heuristic from `docs/LLM.md` §8 / ADR-021. Flags
/// U+1031 at a line start or after whitespace (Zawgyi stores the vowel before
/// its consonant), or any code point in U+1060–U+1097 within otherwise-Burmese
/// text (Zawgyi reuses that range for stacked/variant forms).
pub fn zawgyi_suspect(target: &str) -> bool {
    // Signal 1: U+1031 at the start of a line or after whitespace.
    let mut at_line_start = true;
    for ch in target.chars() {
        if ch == '\u{1031}' && at_line_start {
            return true;
        }
        at_line_start = ch.is_whitespace();
    }

    // Signal 2: U+1060–U+1097 present *in otherwise-Burmese text* (`docs/LLM.md`
    // §8). The band overlaps the Myanmar block, so "otherwise Burmese" is
    // measured over Burmese-CORE code points *excluding* the band itself —
    // otherwise the qualifier would be a no-op (any band char would self-satisfy
    // it). A stray band code point in non-Burmese text does not fire Signal 2.
    let has_burmese_core = target.chars().any(is_burmese_core);
    let has_zawgyi_range = target.chars().any(is_zawgyi_band);
    has_burmese_core && has_zawgyi_range
}

/// The Zawgyi-reused band U+1060–U+1097 (`docs/LLM.md` §8 Signal 2).
fn is_zawgyi_band(c: char) -> bool {
    ('\u{1060}'..='\u{1097}').contains(&c)
}

/// A Myanmar code point that is *not* in the Zawgyi band — the "otherwise
/// Burmese" context used to qualify Signal 2.
fn is_burmese_core(c: char) -> bool {
    is_myanmar(c) && !is_zawgyi_band(c)
}

/// `glossary_miss`: the source contains a glossary term (case-insensitive) but
/// the target lacks that entry's `target` (`docs/LLM.md` §8).
pub fn glossary_miss(source: &Cue, target: &str, ctx: &AnalysisContext) -> bool {
    let src = source.text_masked.to_lowercase();
    let tgt = target.to_lowercase();
    ctx.glossary.iter().any(|g| {
        !g.term.is_empty()
            && !g.target.is_empty()
            && src.contains(&g.term.to_lowercase())
            && !tgt.contains(&g.target.to_lowercase())
    })
}

/// `too_long`: a line exceeds `max_graphemes_per_line`, or the target has more
/// than `max_lines` lines. Lengths are grapheme clusters, not chars/bytes, so
/// multi-code-point Burmese clusters count once (`docs/LLM.md` §8).
pub fn too_long(target: &str, limits: &Limits) -> bool {
    let lines: Vec<&str> = target.split('\n').collect();
    if lines.len() as u32 > limits.max_lines {
        return true;
    }
    lines
        .iter()
        .any(|line| grapheme_count(line) as u32 > limits.max_graphemes_per_line)
}

/// `too_fast`: grapheme clusters per second exceed `max_gps`. A zero or negative
/// duration is treated as "cannot compute" and never flagged (no divide-by-zero).
pub fn too_fast(source: &Cue, target: &str, limits: &Limits) -> bool {
    if source.end_ms <= source.start_ms {
        return false;
    }
    let secs = (source.end_ms - source.start_ms) as f64 / 1000.0;
    if secs <= 0.0 {
        return false;
    }
    let gps = grapheme_count(target) as f64 / secs;
    gps > limits.max_gps
}

/// Runs every per-cue check and returns the flags that fired. An empty target
/// short-circuits to `[Empty]` (the other checks would only add noise).
pub fn check_cue(
    source: &Cue,
    target: &str,
    ctx: &AnalysisContext,
    limits: &Limits,
) -> Vec<QualityFlag> {
    if empty_target(source, target) {
        return vec![QualityFlag::Empty];
    }
    let mut flags = Vec::new();
    if placeholder_mismatch(source, target) {
        flags.push(QualityFlag::PlaceholderMismatch);
    }
    if untranslated(source, target, ctx) {
        flags.push(QualityFlag::Untranslated);
    }
    if zawgyi_suspect(target) {
        flags.push(QualityFlag::ZawgyiSuspect);
    }
    if glossary_miss(source, target, ctx) {
        flags.push(QualityFlag::GlossaryMiss);
    }
    if too_long(target, limits) {
        flags.push(QualityFlag::TooLong);
    }
    if too_fast(source, target, limits) {
        flags.push(QualityFlag::TooFast);
    }
    flags
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let n = values.len();
    if n % 2 == 1 {
        Some(values[n / 2])
    } else {
        Some((values[n / 2 - 1] + values[n / 2]) / 2.0)
    }
}

/// `boundary_drift`: over a batch of `(source cue, target)` pairs, flags a cue
/// whose target/source grapheme-length ratio is far outside the batch median
/// (by `drift_ratio`) while an adjacent cue is abnormally short — the signature
/// of text that drifted across a cue boundary (`docs/LLM.md` §8).
///
/// Returns one `bool` per input index (`true` ⇒ [`QualityFlag::BoundaryDrift`]).
/// Cues with an empty source are never flagged and are excluded from the median.
pub fn check_batch_drift(batch: &[(&Cue, &str)], limits: &Limits) -> Vec<bool> {
    let n = batch.len();
    let ratios: Vec<Option<f64>> = batch
        .iter()
        .map(|(cue, target)| {
            let src = grapheme_count(&cue.text_masked);
            if src == 0 {
                None
            } else {
                Some(grapheme_count(target) as f64 / src as f64)
            }
        })
        .collect();

    let present: Vec<f64> = ratios.iter().flatten().copied().collect();
    let Some(med) = median(present) else {
        return vec![false; n];
    };
    if med <= 0.0 {
        return vec![false; n];
    }

    let high = med * limits.drift_ratio;
    let low = med / limits.drift_ratio;
    let is_short = |i: usize| ratios.get(i).and_then(|r| *r).is_some_and(|r| r < low);

    (0..n)
        .map(|i| {
            let Some(r) = ratios[i] else {
                return false;
            };
            let far = r > high || r < low;
            if !far {
                return false;
            }
            let left_short = i > 0 && is_short(i - 1);
            let right_short = i + 1 < n && is_short(i + 1);
            left_short || right_short
        })
        .collect()
}
