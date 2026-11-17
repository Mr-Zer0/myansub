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
//! valid in both (`type`, `properties`, `items`, `required`, `enum`, `maxItems`,
//! `minItems`, `maxLength`), so the modeled value is compatible either way. See
//! `docs/DECISIONS.md` ADR-025.

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

/// The analysis document schema (`docs/DATA_MODEL.md` §3), including the array
/// caps (characters ≤ 40, address_matrix ≤ 80, glossary ≤ 120, scenes ≤ 120) and
/// the string length limits expressible as `maxLength`.
pub fn analysis_response_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "schema_version": { "type": "integer" },
            "premise": { "type": "string", "maxLength": 500 },
            "tone": { "type": "string", "maxLength": 160 },
            "characters": {
                "type": "array",
                "maxItems": 40,
                "items": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "name": { "type": "string", "maxLength": 120 },
                        "aliases": {
                            "type": "array",
                            "maxItems": 8,
                            "items": { "type": "string", "maxLength": 80 }
                        },
                        "gender": { "type": "string", "enum": ["m", "f", "unknown"] },
                        "age_group": {
                            "type": "string",
                            "enum": ["child", "young", "adult", "elder", "unknown"]
                        },
                        "role": { "type": "string", "maxLength": 260 },
                        "speech_style": { "type": "string", "maxLength": 180 }
                    },
                    "required": ["id", "name", "gender", "age_group"]
                }
            },
            "address_matrix": {
                "type": "array",
                "maxItems": 80,
                "items": {
                    "type": "object",
                    "properties": {
                        "from": { "type": "string" },
                        "to": { "type": "string" },
                        "relation": { "type": "string", "maxLength": 160 },
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
                "maxItems": 120,
                "items": {
                    "type": "object",
                    "properties": {
                        "term": { "type": "string", "maxLength": 120 },
                        "target": { "type": "string", "maxLength": 120 },
                        "keep": { "type": "boolean" },
                        "note": { "type": "string", "maxLength": 180 }
                    },
                    "required": ["term", "target", "keep"]
                }
            },
            "scenes": {
                "type": "array",
                "maxItems": 120,
                "items": {
                    "type": "object",
                    "properties": {
                        "cue_range": {
                            "type": "array",
                            "items": { "type": "integer" },
                            "minItems": 2,
                            "maxItems": 2
                        },
                        "summary": { "type": "string", "maxLength": 300 },
                        "speakers": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "mood": { "type": "string", "maxLength": 80 }
                    },
                    "required": ["cue_range", "summary"]
                }
            },
            "style_rules": {
                "type": "array",
                "maxItems": 12,
                "items": { "type": "string", "maxLength": 180 }
            },
            "unresolved_ambiguities": {
                "type": "array",
                "maxItems": 12,
                "items": { "type": "string", "maxLength": 220 }
            }
        },
        "required": ["schema_version", "premise", "characters", "scenes"]
    })
}
