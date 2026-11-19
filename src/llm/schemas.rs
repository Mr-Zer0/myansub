//! Gemini response schemas as [`serde_json::Value`] (`docs/DATA_MODEL.md` §3/§4).
//!
//! # `responseSchema` vs `responseJsonSchema` ([verify], `docs/LLM.md` §1)
//!
//! Verified against the live Gemini `generateContent` API reference
//! (2026, Gemini 3.x Flash era): `generationConfig.responseJsonSchema` is the
//! recommended field (full JSON Schema), and the older `responseSchema` is marked
//! "an internal detail. Use responseJsonSchema rather than this field." The
//! request wiring (which field name to send) is `gemini.rs`'s job (M3-B); it
//! should use `responseJsonSchema`. The schemas built here use only keywords
//! valid in both (`type`, `properties`, `items`, `required`, `enum`, `minItems`,
//! `maxItems`), so the modeled value is compatible either way. See
//! `docs/DECISIONS.md` ADR-025.
//!
//! # Why the analysis schema carries no bounds (ADR-044)
//!
//! Gemini's constrained-decoding state machine rejects a schema whose array-length
//! states exceed a serving ceiling (HTTP 400 "The specified schema produces a
//! constraint that has too many states for serving"). The necessary and sufficient
//! trigger was the analysis document's large `maxItems` array bounds (`characters`
//! 40, `address_matrix` 80, `glossary` 120, `scenes` 120): those array-length limits
//! alone are what overflowed the ceiling. The `maxLength`/`minLength` string caps the
//! schema also carried contributed NOTHING — verified against Gemini's live
//! structured-output docs, those keywords are ignored during constrained decoding, so
//! they added no character-counting states. [`analysis_response_schema`] therefore
//! sends only the document SHAPE (`type`, `properties`, `required`, small `enum`s, and
//! `cue_range`'s fixed 2-element bound). Dropping the big `maxItems` is the fix;
//! removing the (server-ignored) `maxLength` alongside it is harmless cleanup. The
//! `docs/DATA_MODEL.md` §3 array caps and per-field string-length caps are instead
//! enforced in Rust after parsing, in `pipeline::analysis::validate_full` — which, for
//! the string-length caps, closed a latent gap: because Gemini ignored `maxLength` and
//! `validate_full` did not previously check string lengths, over-length strings on the
//! LLM output path had been validated by nothing. The small
//! [`translation_response_schema`] never had such bounds and is unaffected.

use serde_json::{Value, json};

/// The translation response schema (`docs/DATA_MODEL.md` §4):
/// `{"lines":[{"id":int,"t":string}]}`.
pub fn translation_response_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "lines": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "integer" },
                        "t": { "type": "string" }
                    },
                    "required": ["id", "t"]
                }
            }
        },
        "required": ["lines"]
    })
}

/// The analysis document schema (`docs/DATA_MODEL.md` §3). It describes only the
/// document SHAPE — `type`, `properties`, `required`, small `enum`s, and
/// `cue_range`'s fixed 2-element bound. The `docs/DATA_MODEL.md` §3 array caps
/// (characters ≤ 40, address_matrix ≤ 80, glossary ≤ 120, scenes ≤ 120, and the
/// small-list caps) and per-field string-length caps are deliberately NOT in the
/// wire schema: the large `maxItems` array-length bounds alone overflowed Gemini's
/// constrained-decoding serving ceiling (HTTP 400 "too many states for serving",
/// ADR-044). The `maxLength` string caps were dropped too, but only as cleanup —
/// Gemini ignores `maxLength` during constrained decoding, so it never added any
/// states. Those caps are enforced after parsing, in
/// `pipeline::analysis::validate_full`.
pub fn analysis_response_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "schema_version": { "type": "integer" },
            "premise": { "type": "string" },
            "tone": { "type": "string" },
            "characters": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "name": { "type": "string" },
                        "aliases": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "gender": { "type": "string", "enum": ["m", "f", "unknown"] },
                        "age_group": {
                            "type": "string",
                            "enum": ["child", "young", "adult", "elder", "unknown"]
                        },
                        "role": { "type": "string" },
                        "speech_style": { "type": "string" }
                    },
                    "required": ["id", "name", "gender", "age_group"]
                }
            },
            "address_matrix": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "from": { "type": "string" },
                        "to": { "type": "string" },
                        "relation": { "type": "string" },
                        "self_ref": { "type": "string" },
                        "addressee_ref": { "type": "string" },
                        "register": {
                            "type": "string",
                            "enum": ["colloquial", "polite", "formal"]
                        },
                        "particle": { "type": "string" }
                    },
                    "required": ["from", "to", "register"]
                }
            },
            "glossary": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "term": { "type": "string" },
                        "target": { "type": "string" },
                        "keep": { "type": "boolean" },
                        "note": { "type": "string" }
                    },
                    "required": ["term", "target", "keep"]
                }
            },
            "scenes": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "cue_range": {
                            "type": "array",
                            "items": { "type": "integer" },
                            "minItems": 2,
                            "maxItems": 2
                        },
                        "summary": { "type": "string" },
                        "speakers": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "mood": { "type": "string" }
                    },
                    "required": ["cue_range", "summary"]
                }
            },
            "style_rules": {
                "type": "array",
                "items": { "type": "string" }
            },
            "unresolved_ambiguities": {
                "type": "array",
                "items": { "type": "string" }
            }
        },
        "required": ["schema_version", "premise", "characters", "scenes"]
    })
}
