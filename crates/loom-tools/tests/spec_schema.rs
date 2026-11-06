//! The schemas the model receives are generated from the same structs the
//! tools parse into (ADR-005 addendum 1, V19).
//!
//! These assertions run over the specs of the shipped tool structs, so a tool
//! cannot ship a schema that is not a closed, fully described object. The
//! schema↔parser agreement itself is checked next to each private input
//! struct (`crate::schema::agreement`), where the parser is reachable.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use loom_kernel::tool::Tool;
use serde_json::Value;

/// Every shipped tool, by the name it registers under.
fn tools() -> Vec<(&'static str, Box<dyn Tool>)> {
    vec![
        ("read_file", Box::new(loom_tools::ReadFileTool::default())),
        ("list_dir", Box::new(loom_tools::ListDirTool)),
        ("search", Box::new(loom_tools::SearchTool)),
        ("write_file", Box::new(loom_tools::WriteFileTool::default())),
        ("edit_file", Box::new(loom_tools::EditFileTool::default())),
        ("run_command", Box::new(loom_tools::RunCommandTool::new())),
        ("verify", Box::new(loom_tools::VerifyTool::new())),
        (
            "artifact_read",
            Box::new(loom_tools::ArtifactReadTool::default()),
        ),
    ]
}

#[test]
fn every_tool_schema_is_a_closed_described_object() {
    for (name, tool) in tools() {
        let spec = tool.spec();
        assert_eq!(
            spec.name, name,
            "spec name drifted from the registration name"
        );
        assert!(!spec.description.is_empty(), "{name} has no description");

        let schema = &spec.parameters;
        let object = schema
            .as_object()
            .unwrap_or_else(|| panic!("{name}: parameters is not an object"));
        assert_eq!(
            object.get("type"),
            Some(&Value::String("object".to_owned())),
            "{name}: parameters is not an object schema"
        );
        assert_eq!(
            object.get("additionalProperties"),
            Some(&Value::Bool(false)),
            "{name}: unknown fields are not rejected"
        );
        // Generated schemas carry boilerplate the hand-written ones did not;
        // the model has no use for either.
        assert!(object.get("$schema").is_none(), "{name}: leaks `$schema`");
        assert!(object.get("title").is_none(), "{name}: leaks `title`");

        let required = object
            .get("required")
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("{name}: no `required` list"));
        assert!(!required.is_empty(), "{name}: no required field");

        let properties = object
            .get("properties")
            .and_then(Value::as_object)
            .unwrap_or_else(|| panic!("{name}: no `properties`"));
        assert!(!properties.is_empty(), "{name}: no properties");
        for (property, definition) in properties {
            assert!(
                definition.get("description").is_some(),
                "{name}.{property}: no description for the model"
            );
        }
        for key in required {
            let key = key.as_str().expect("required entry is a string");
            assert!(
                properties.contains_key(key),
                "{name}: required `{key}` is not a declared property"
            );
        }
    }
}

/// `2K`: the serialized specs carry no `schemars` noise.
///
/// `schema::parameters` strips the keys that repeat on every request without
/// changing what a tool accepts (`default`, `format`, and the
/// `definitions`/`$ref` indirection for a named enum). The byte *budget*
/// lives in `measure_specs.rs`; here we only assert the noise is gone, so the
/// two tests do not duplicate a number that has to be bumped in lockstep.
#[test]
fn tool_specs_are_compact_and_noise_free() {
    for (name, tool) in tools() {
        let json = serde_json::to_string(&tool.spec()).expect("spec serializes");
        for banned in ["\"$ref\"", "\"definitions\"", "\"default\"", "\"format\""] {
            assert!(
                !json.contains(banned),
                "{name}: spec still carries {banned}"
            );
        }
    }
}
