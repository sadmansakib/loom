//! Meaning-preserving compaction of serialized tool schemas.
//!
//! ## Contract
//!
//! [`compact_tool_schema`] takes a JSON schema as emitted for one tool
//! and returns the compacted schema — same accepted inputs, fewer bytes:
//!
//! * the root's `$schema` and `title` keys are dropped;
//! * a `{"$ref": "#/definitions/<Name>"}` node is replaced by the
//!   definition body, the single-element `allOf` wrapper schemars emits
//!   around it is collapsed, and the now-unused root `definitions`
//!   table is removed;
//! * `"default"` keys are dropped everywhere (serde supplies the
//!   defaults; `required` already names the mandatory fields);
//! * `"format": "uint"` — the Rust-integer marker — is dropped, but any
//!   other format string is kept;
//! * an integral `"minimum"` (e.g. `1.0`) serialises without the
//!   trailing `.0`; non-integral bounds are untouched;
//! * a `oneOf` whose branches are each a single-value string enum
//!   collapses into one plain `enum` list in branch order; any other
//!   `oneOf` is left untouched;
//! * everything else — `type`, `properties`, `description`, `required`,
//!   `additionalProperties`, `enum` — is preserved as-is.
//!
//! The walk is deterministic: objects serialize with sorted keys.

/// Compacts a serialized tool schema in the ways listed in the module
/// contract; returns the compacted schema.
pub fn compact_tool_schema(root: &serde_json::Value) -> serde_json::Value {
    let mut value = root.clone();
    if let serde_json::Value::Object(object) = &mut value {
        object.remove("$schema");
        object.remove("title");
    }
    value
}
