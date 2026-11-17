//! Canonical JSON serialization: recursively sorted object keys and fixed,
//! compact formatting so that two equal documents serialize to identical bytes.
//!
//! This is the basis of the local cache key (`docs/LLM.md` §4) and the stable
//! cached prefix (`docs/LLM.md` §2). A unit test asserts byte-identical output
//! for equal documents (M3-AC6).

/// Serializes `value` to a canonical string: every object's keys are sorted
/// lexicographically (recursively) and the output is compact (no insignificant
/// whitespace). Equal [`serde_json::Value`]s always produce identical bytes.
pub fn canonical_json(value: &serde_json::Value) -> String {
    // Serializing a `Value` cannot fail (map keys are always strings); on the
    // impossible error path fall back to an empty string rather than panic.
    serde_json::to_string(&canonicalize(value)).unwrap_or_default()
}

/// Rebuilds `value` with every nested object's keys sorted, so serialization is
/// order-independent regardless of how the input was constructed.
fn canonicalize(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut sorted = serde_json::Map::new();
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for key in keys {
                // Each key came from `map`, so the lookup always succeeds.
                if let Some(child) = map.get(key) {
                    sorted.insert(key.clone(), canonicalize(child));
                }
            }
            serde_json::Value::Object(sorted)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonicalize).collect())
        }
        other => other.clone(),
    }
}
