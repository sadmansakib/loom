//! Canonical JSON serialization for call identity (kernel-internal).
//!
//! `serde_json`'s map iteration order depends on the build graph: with
//! the crate's default features objects iterate in sorted key order,
//! but any package in the resolution enabling the `preserve_order`
//! feature flips every map in the build to insertion order — and the
//! workspace's `agent-client-protocol` dependency does exactly that
//! under unified (`--workspace`) builds. Call identity must not depend
//! on which packages a build happens to include, so the failure ledger
//! and the analytics repeat stats serialize object keys byte-sorted at
//! every nesting depth; array order is semantic and is preserved.

use serde_json::Value;

/// Serializes `value` compactly with object keys byte-sorted
/// recursively.
///
/// Byte-identical to `serde_json::to_string` for any value whose maps
/// serde_json itself keeps sorted (default builds), and identical for
/// two values that differ only in object key insertion order.
pub(crate) fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(bool) => out.push_str(if *bool { "true" } else { "false" }),
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(string) => {
            out.push_str(&string_escape(string));
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort_unstable();
            out.push('{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&string_escape(key));
                out.push(':');
                write_canonical(&map[*key], out);
            }
            out.push('}');
        }
    }
}

/// Serializes one JSON string scalar. `to_string` cannot fail for a
/// `&str`/`&String`; the fallback keeps this panic-free anyway.
fn string_escape(string: &str) -> String {
    serde_json::to_string(string).unwrap_or_else(|_| "\"\"".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn object_keys_are_sorted_at_every_depth() {
        let value = json!({"z": 1, "a": {"y": [true, null, {"b": 2, "a": 1}], "b": "x"}});
        assert_eq!(
            canonical_json(&value),
            r#"{"a":{"b":"x","y":[true,null,{"a":1,"b":2}]},"z":1}"#
        );
    }

    #[test]
    fn byte_identical_to_serde_json_for_sorted_maps() {
        // In a default build `json!` already stores sorted keys, so the
        // canonical form must equal serde_json's own serialization.
        let value = json!({"a": 2, "b": {"c": [1, 2.5, "s"]}, "d": null});
        assert_eq!(
            canonical_json(&value),
            serde_json::to_string(&value).unwrap()
        );
    }

    #[test]
    fn scalars_and_arrays_round_trip() {
        assert_eq!(canonical_json(&json!(null)), "null");
        assert_eq!(canonical_json(&json!(true)), "true");
        assert_eq!(canonical_json(&json!(7)), "7");
        assert_eq!(canonical_json(&json!("he\"llo")), r#""he\"llo""#);
        assert_eq!(
            canonical_json(&json!([1, {"b": 1, "a": 0}])),
            r#"[1,{"a":0,"b":1}]"#
        );
    }
}
