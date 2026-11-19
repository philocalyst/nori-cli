//! The cache key under which a tool call's id is remembered between the hook and the permission
//! callback.

use std::collections::BTreeMap;

/// Generate a stable cache key from JSON value
///
/// JSON serialization order is not guaranteed to be stable.
/// This function canonicalizes the JSON by sorting object keys using BTreeMap,
/// ensuring identical content always produces the same cache key.
pub fn stable_cache_key(tool_input: &serde_json::Value) -> String {
    fn canonicalize(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => {
                // Use BTreeMap to ensure keys are sorted
                let sorted: BTreeMap<_, _> = map
                    .iter()
                    .map(|(k, v)| (k.clone(), canonicalize(v)))
                    .collect();
                serde_json::Value::Object(sorted.into_iter().collect())
            }
            serde_json::Value::Array(arr) => {
                serde_json::Value::Array(arr.iter().map(canonicalize).collect())
            }
            other => other.clone(),
        }
    }
    canonicalize(tool_input).to_string()
}
