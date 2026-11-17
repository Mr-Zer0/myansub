//! Canonical JSON determinism (M3-AC6, `docs/LLM.md` §2/§4).
//!
//! Equal documents must serialize to byte-identical strings regardless of how
//! they were constructed, since this is the basis of the local cache key.

use myansub::llm::canonical_json;
use serde_json::json;

// AC: M3-AC6
#[test]
fn equal_documents_with_different_key_order_serialize_to_identical_bytes() {
    let a = json!({
        "model": "gemini-flash-latest",
        "prompt_version": 1,
        "generationConfig": { "maxOutputTokens": 320, "responseMimeType": "application/json" },
        "contents": [ { "role": "user", "text": "sheet" } ]
    });
    // Same logical document, every object's keys written in a different order.
    let b = json!({
        "contents": [ { "text": "sheet", "role": "user" } ],
        "prompt_version": 1,
        "generationConfig": { "responseMimeType": "application/json", "maxOutputTokens": 320 },
        "model": "gemini-flash-latest"
    });

    assert_eq!(canonical_json(&a), canonical_json(&b));
}

// AC: M3-AC6
#[test]
fn nested_objects_and_arrays_have_keys_sorted_recursively() {
    let value = json!({
        "z": 1,
        "a": { "y": 2, "b": 3 },
        "list": [ { "d": 4, "c": 5 } ]
    });

    let out = canonical_json(&value);

    assert_eq!(out, r#"{"a":{"b":3,"y":2},"list":[{"c":5,"d":4}],"z":1}"#);
}

// AC: M3-AC6
#[test]
fn array_element_order_is_preserved() {
    // Arrays are ordered; only object keys are sorted.
    let value = json!({ "xs": [3, 1, 2] });
    assert_eq!(canonical_json(&value), r#"{"xs":[3,1,2]}"#);
}

// AC: M3-AC6
#[test]
fn output_is_stable_across_repeated_calls() {
    let value = json!({ "b": [1, 2], "a": { "n": 7 } });
    let first = canonical_json(&value);
    let second = canonical_json(&value);
    assert_eq!(first, second);
    assert_eq!(first, r#"{"a":{"n":7},"b":[1,2]}"#);
}
