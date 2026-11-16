//! Tag masking and lead-tag extraction over cue text strings.
//!
//! This is *not* a subtitle-file parser; it operates on the text of a single cue
//! (see `docs/ARCHITECTURE.md` §3 and `docs/DATA_MODEL.md` §1). Inline tags are
//! replaced with `⟦n⟧` placeholders numbered per cue, 0-based.

/// The result of masking a cue's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Masked {
    /// Whole-line leading override block, e.g. `{\an8\pos(10,20)}`, if present.
    pub lead_tags: Option<String>,
    /// Text with the lead block removed and inline tags replaced by `⟦n⟧`.
    pub text_masked: String,
    /// Placeholder `n` → the original tag text it replaced.
    pub inline_tags: Vec<String>,
    /// True if at least one letter remains after tag removal.
    pub translatable: bool,
}

const PLACEHOLDER_OPEN: char = '\u{27E6}'; // ⟦
const PLACEHOLDER_CLOSE: char = '\u{27E7}'; // ⟧

/// Returns the length of a tag starting at byte 0 of `s`, including delimiters,
/// or `None` if `s` does not start with a complete `{...}` or `<...>` tag.
fn leading_tag_len(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let close = match bytes.first()? {
        b'{' => b'}',
        b'<' => {
            // Only treat `<` as markup when it plausibly opens a tag: the next
            // byte is an ASCII letter (`<i>`, `<font ...>`) or `/` (`</i>`).
            // A `<` before a space or other char is literal prose.
            match bytes.get(1) {
                Some(b) if b.is_ascii_alphabetic() || *b == b'/' => b'>',
                _ => return None,
            }
        }
        _ => return None,
    };
    // Byte position of the closing delimiter. Delimiters are ASCII, so byte
    // offsets land on char boundaries.
    let rel = bytes.iter().position(|&b| b == close)?;
    Some(rel + 1)
}

/// Masks the inline tags in `text` and extracts a leading override block.
pub fn mask(text: &str) -> Masked {
    // 1. Pull off any run of consecutive leading `{...}` override blocks as lead
    //    tags. HTML-style `<...>` tags are never lead tags; they are inline.
    let mut lead_end = 0;
    while text[lead_end..].starts_with('{') {
        match leading_tag_len(&text[lead_end..]) {
            Some(len) => lead_end += len,
            None => break,
        }
    }
    let lead_tags = if lead_end > 0 {
        Some(text[..lead_end].to_string())
    } else {
        None
    };
    let body = &text[lead_end..];

    // 2. Mask remaining inline tags; collect literal text to judge translatability.
    let mut text_masked = String::with_capacity(body.len());
    let mut inline_tags: Vec<String> = Vec::new();
    let mut plain = String::new();
    let mut rest = body;
    while !rest.is_empty() {
        if let Some(len) = leading_tag_len(rest) {
            let n = inline_tags.len();
            inline_tags.push(rest[..len].to_string());
            text_masked.push(PLACEHOLDER_OPEN);
            text_masked.push_str(&n.to_string());
            text_masked.push(PLACEHOLDER_CLOSE);
            rest = &rest[len..];
        } else {
            // Not a complete tag: emit one char. Placeholder-bracket literals
            // are escaped by doubling so `unmask` can tell them from real
            // placeholders (keeps round-trip identity for all inputs).
            let ch = rest.chars().next().unwrap_or('\0');
            if ch == PLACEHOLDER_OPEN {
                text_masked.push(PLACEHOLDER_OPEN);
                text_masked.push(PLACEHOLDER_OPEN);
            } else if ch == PLACEHOLDER_CLOSE {
                text_masked.push(PLACEHOLDER_CLOSE);
                text_masked.push(PLACEHOLDER_CLOSE);
            } else {
                text_masked.push(ch);
            }
            plain.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }

    let translatable = plain.chars().any(char::is_alphabetic);

    Masked {
        lead_tags,
        text_masked,
        inline_tags,
        translatable,
    }
}

/// Restores `⟦n⟧` placeholders in `masked` using `inline_tags`.
///
/// Unknown placeholder indices are left untouched. This does not re-attach the
/// lead block, which the caller stores and re-applies separately.
pub fn unmask(masked: &str, inline_tags: &[String]) -> String {
    let chars: Vec<char> = masked.chars().collect();
    let mut out = String::with_capacity(masked.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == PLACEHOLDER_OPEN {
            // A doubled open bracket is an escaped literal `⟦`.
            if chars.get(i + 1) == Some(&PLACEHOLDER_OPEN) {
                out.push(PLACEHOLDER_OPEN);
                i += 2;
                continue;
            }
            // Otherwise try to read a `⟦<digits>⟧` placeholder.
            let mut j = i + 1;
            let mut digits = String::new();
            while j < chars.len() && chars[j].is_ascii_digit() {
                digits.push(chars[j]);
                j += 1;
            }
            if !digits.is_empty()
                && chars.get(j) == Some(&PLACEHOLDER_CLOSE)
                && let Ok(n) = digits.parse::<usize>()
                && n < inline_tags.len()
            {
                out.push_str(&inline_tags[n]);
                i = j + 1;
                continue;
            }
            // Not a known placeholder: keep the open bracket verbatim.
            out.push(PLACEHOLDER_OPEN);
            i += 1;
        } else if c == PLACEHOLDER_CLOSE {
            // A doubled close bracket is an escaped literal `⟧`.
            if chars.get(i + 1) == Some(&PLACEHOLDER_CLOSE) {
                i += 1;
            }
            out.push(PLACEHOLDER_CLOSE);
            i += 1;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// Returns true if `text` contains at least one letter after all tags are
/// removed (the "translatable cue" rule from `docs/REQUIREMENTS.md`).
pub fn is_translatable(text: &str) -> bool {
    mask(text).translatable
}
