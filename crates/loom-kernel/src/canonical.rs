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
/// `serde_json` itself keeps sorted (default builds), and identical for
/// two values that differ only in object key insertion order.
pub(crate) fn canonical_json(value: &Value) -> String {
    // `serde_json::to_writer` writes to `io::Write`; use one shared
    // byte buffer for the whole tree rather than allocating a `String`
    // for every scalar/key and copying it into its parent.
    let mut out = Vec::new();
    write_canonical(value, &mut out);
    // serde_json always emits UTF-8; retain a panic-free fallback if a
    // future serializer change violates that invariant.
    String::from_utf8(out).unwrap_or_default()
}

fn write_canonical(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(bool) => out.extend_from_slice(if *bool { b"true" } else { b"false" }),
        Value::Number(number) => {
            // Serialize directly into the shared output buffer instead
            // of allocating a temporary `String` per scalar (review
            // item 3.2). `Number` has no serializer failure mode.
            let _ = serde_json::to_writer(out, number);
        }
        Value::String(string) => write_string(string, out),
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_canonical(item, out);
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort_unstable();
            out.push(b'{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_string(key, out);
                out.push(b':');
                write_canonical(&map[*key], out);
            }
            out.push(b'}');
        }
    }
}

/// Serializes one JSON string scalar directly into the shared buffer:
/// the serializer writes quotes and escaping without allocating an
/// intermediate `String` per key/value.
fn write_string(string: &str, out: &mut Vec<u8>) {
    let _ = serde_json::to_writer(out, string);
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
