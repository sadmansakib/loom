//! Generated tool input schemas.
//!
//! Each tool derives `JsonSchema` on the same struct it deserialises
//! `call` input into, and [`parameters`] renders that struct's schema
//! into the `ToolSpec.parameters` value the model sees. Because both
//! sides come from one struct, the schema and the parser cannot drift
//! (ADR-005 addendum 1, V19).

use schemars::JsonSchema;

/// The model-facing JSON Schema for a tool's input struct.
///
/// `$schema` and `title` are dropped: they carry nothing the model needs
/// and the hand-written schemas this replaces did not have them. The
/// rest (properties, descriptions from doc comments, `required`,
/// `additionalProperties`) comes straight from the struct.
pub(crate) fn parameters<T: JsonSchema>() -> serde_json::Value {
    // A `RootSchema` is always JSON-representable, so the fallback is
    // unreachable; it keeps this path panic-free under the workspace lint.
    let mut value =
        serde_json::to_value(schemars::schema_for!(T)).unwrap_or(serde_json::Value::Null);
    if let Some(object) = value.as_object_mut() {
        object.remove("$schema");
        object.remove("title");
    }
    value
}

/// Schema↔parser agreement checks (V19).
///
/// The schema the model receives and the parser that reads its reply come
/// from one struct, but they are still two mechanisms: serde decides what
/// parses, `schemars` decides what is advertised. These helpers fail if the
/// two disagree, so a struct edit cannot leave the model with a schema that
/// describes an input the tool then rejects (or secretly accepts).
#[cfg(test)]
pub(crate) mod agreement {
    use schemars::JsonSchema;
    use serde::de::DeserializeOwned;
    use serde_json::Value;

    /// Asserts the schema for `T` matches how `T` parses, using `valid` as a
    /// minimal accepted instance (it must contain every required key).
    pub(crate) fn check<T: JsonSchema + DeserializeOwned>(valid: Value) {
        let schema = super::parameters::<T>();
        let object = schema.as_object().expect("object schema");
        assert_eq!(
            object.get("type"),
            Some(&Value::String("object".to_owned())),
            "generated schema is not an object"
        );
        assert_eq!(
            object.get("additionalProperties"),
            Some(&Value::Bool(false)),
            "unknown fields are not rejected by the schema"
        );
        let properties = object
            .get("properties")
            .and_then(Value::as_object)
            .expect("properties");
        let required: Vec<String> = object
            .get("required")
            .and_then(Value::as_array)
            .expect("required")
            .iter()
            .map(|name| name.as_str().expect("required name").to_owned())
            .collect();
        assert!(
            !required.is_empty(),
            "a tool input needs at least one field"
        );
        for (name, property) in properties {
            assert!(
                property.get("description").is_some(),
                "property {name} has no description for the model"
            );
        }

        // The sample parses.
        assert!(
            serde_json::from_value::<T>(valid.clone()).is_ok(),
            "the valid sample is rejected by the parser"
        );
        // Every advertised required key really is mandatory.
        for name in &required {
            let mut missing = valid.clone();
            missing.as_object_mut().expect("object sample").remove(name);
            assert!(
                serde_json::from_value::<T>(missing).is_err(),
                "{name} is advertised as required but the parser accepts it missing"
            );
        }
        // `additionalProperties: false` really means the parser rejects extras.
        let mut unknown = valid.clone();
        unknown
            .as_object_mut()
            .expect("object sample")
            .insert("__unexpected".to_owned(), Value::Bool(true));
        assert!(
            serde_json::from_value::<T>(unknown).is_err(),
            "the schema rejects unknown fields but the parser accepts them"
        );
    }
}
