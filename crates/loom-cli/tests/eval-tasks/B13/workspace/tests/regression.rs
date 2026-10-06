use loom_eval_b13::compact_tool_schema;
use serde_json::json;

#[test]
fn definitions_are_inlined_and_the_table_dropped() {
    let schema = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "scope": { "allOf": [{ "$ref": "#/definitions/Scope" }] }
        },
        "definitions": {
            "Scope": { "type": "string", "enum": ["check", "test"] }
        }
    });
    let compacted = compact_tool_schema(&schema);
    let scope = &compacted["properties"]["scope"];
    assert_eq!(scope, &json!({ "type": "string", "enum": ["check", "test"] }));
    assert!(compacted.get("definitions").is_none());
    assert!(scope.get("$ref").is_none());
    assert!(scope.get("allOf").is_none());
}

#[test]
fn redundant_annotations_are_stripped() {
    let schema = json!({
        "type": "object",
        "properties": {
            "offset": {
                "type": ["integer", "null"],
                "format": "uint",
                "default": null,
                "minimum": 0.0
            }
        }
    });
    let compacted = compact_tool_schema(&schema);
    let offset = &compacted["properties"]["offset"];
    assert!(offset.get("default").is_none());
    assert!(offset.get("format").is_none());
}

#[test]
fn integral_minimums_lose_the_float_tail() {
    let schema = json!({ "type": "integer", "minimum": 1.0 });
    let compacted = compact_tool_schema(&schema);
    assert_eq!(
        serde_json::to_string(&compacted).unwrap(),
        r#"{"minimum":1,"type":"integer"}"#
    );
}

#[test]
fn singleton_oneof_branches_collapse() {
    let schema = json!({
        "oneOf": [
            { "type": "string", "enum": ["check"] },
            { "type": "string", "enum": ["test"] }
        ]
    });
    let compacted = compact_tool_schema(&schema);
    assert_eq!(
        compacted,
        json!({ "enum": ["check", "test"] })
    );
}

#[test]
fn root_schema_and_title_are_dropped() {
    let schema = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": "ReadFile",
        "type": "object"
    });
    let compacted = compact_tool_schema(&schema);
    assert_eq!(compacted, json!({ "type": "object" }));
}

#[test]
fn descriptions_required_and_types_survive() {
    let schema = json!({
        "type": "object",
        "description": "Reads a file from the workspace.",
        "required": ["path"],
        "properties": {
            "path": {
                "type": "string",
                "description": "Workspace-relative path to read."
            }
        },
        "additionalProperties": false
    });
    assert_eq!(compact_tool_schema(&schema), schema);
}

#[test]
fn meaningful_formats_survive() {
    let schema = json!({
        "type": "string",
        "format": "date-time"
    });
    assert_eq!(compact_tool_schema(&schema), schema);
}

#[test]
fn non_integral_bounds_survive() {
    let schema = json!({ "type": "number", "minimum": 0.5, "maximum": 10.0 });
    assert_eq!(compact_tool_schema(&schema), schema);
}

#[test]
fn oneof_with_multi_value_branches_survives() {
    let schema = json!({
        "oneOf": [
            { "type": "string", "enum": ["read", "write"] },
            { "type": "string", "enum": ["test"] }
        ]
    });
    assert_eq!(compact_tool_schema(&schema), schema);
}

#[test]
fn a_plain_enum_survives() {
    let schema = json!({ "type": "string", "enum": ["check", "test"] });
    assert_eq!(compact_tool_schema(&schema), schema);
}
