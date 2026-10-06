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
///
/// The schema is then passed through [`compact`], which removes the
/// `schemars` annotations that do not affect what the tool accepts. This is
/// a pure size cut: the tool spec is the dominant fixed cost of a short
/// task's context (`2K`), and these keys are re-sent on every request.
pub(crate) fn parameters<T: JsonSchema>() -> serde_json::Value {
    // A `RootSchema` is always JSON-representable, so the fallback is
    // unreachable; it keeps this path panic-free under the workspace lint.
    let mut value =
        serde_json::to_value(schemars::schema_for!(T)).unwrap_or(serde_json::Value::Null);
    if let Some(object) = value.as_object_mut() {
        object.remove("$schema");
        object.remove("title");
    }
    inline_definitions(&mut value);
    compact(&mut value);
    value
}

/// Replaces `{"$ref": "#/definitions/<Name>"}` nodes with the definition body
/// and drops the now-unused `definitions` table.
///
/// `schemars` emits a named enum (e.g. `verify`'s `Scope`) as a top-level
/// `definitions` entry referenced by `$ref`. The model only ever sees one
/// tool at a time, so the indirection buys nothing and costs bytes; inlining
/// also lets [`compact`] see through the copy. A schemars `$ref` is wrapped in
/// a single-element `allOf`, so that wrapper is collapsed too.
fn inline_definitions(root: &mut serde_json::Value) {
    let Some(definitions) = root
        .get("definitions")
        .and_then(serde_json::Value::as_object)
    else {
        return;
    };
    let definitions = definitions.clone();
    resolve_refs(root, &definitions);
    if let Some(object) = root.as_object_mut() {
        object.remove("definitions");
    }
}

fn resolve_refs(
    value: &mut serde_json::Value,
    definitions: &serde_json::Map<String, serde_json::Value>,
) {
    match value {
        serde_json::Value::Object(map) => {
            let replacement = map
                .get("$ref")
                .and_then(serde_json::Value::as_str)
                .and_then(|r| r.strip_prefix("#/definitions/"))
                .and_then(|name| definitions.get(name))
                .cloned();
            if let Some(mut resolved) = replacement {
                resolve_refs(&mut resolved, definitions);
                *value = resolved;
                return;
            }
            for child in map.values_mut() {
                resolve_refs(child, definitions);
            }
            // Collapse `{"allOf": [<one schema>]}` left by a resolved `$ref`,
            // keeping the parent's own keys (e.g. its `description`).
            if let Some(serde_json::Value::Array(items)) = map.get("allOf")
                && items.len() == 1
                && let serde_json::Value::Object(inner) = items[0].clone()
            {
                map.remove("allOf");
                for (key, child) in inner {
                    map.entry(key).or_insert(child);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                resolve_refs(item, definitions);
            }
        }
        _ => {}
    }
}

/// Recursively drops schema keys the model does not need.
///
/// `schemars` annotates every `Option` field with `"default": null` and every
/// `usize` with `"format": "uint"`. Neither changes what the tool accepts:
/// `required` already names the mandatory fields and serde supplies the
/// defaults, so both are pure repetition in every request. `format` is only
/// dropped when it is the Rust-integer marker, so a future schema can keep a
/// meaningful format; other keys (`description`, `type`, `minimum`, enums,
/// `additionalProperties`) are preserved. The walk is deterministic because
/// `serde_json` maps serialize with sorted keys (`preserve_order` off).
///
/// Two more rewrites happen on the way through, both meaning-preserving:
///
/// * `"minimum": 1.0` becomes `"minimum": 1` — `schemars` serialises integer
///   bounds as floats, and the trailing `.0` is re-sent on every request.
/// * A `oneOf` whose branches are each a single-value string enum (how
///   `schemars` renders a field-less Rust `enum`) collapses to one plain
///   `enum` list. `{"oneOf":[{"enum":["check"]},{"enum":["test"]}]}` and
///   `"enum": ["check","test"]` accept exactly the same values; the branch
///   descriptions duplicate the field's own description, so they go too.
fn compact(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.remove("default");
            if map.get("format").and_then(serde_json::Value::as_str) == Some("uint") {
                map.remove("format");
            }
            if let Some(bound) = map.get("minimum").and_then(serde_json::Value::as_f64)
                && bound.fract() == 0.0
            {
                map.insert("minimum".to_owned(), serde_json::Value::from(bound as i64));
            }
            for child in map.values_mut() {
                compact(child);
            }
            collapse_singleton_enum(map);
        }
        serde_json::Value::Array(items) => {
            for item in items {
                compact(item);
            }
        }
        _ => {}
    }
}

/// Rewrites a `oneOf` of single-value string enums into one `enum` list, in
/// place. Anything else is left untouched.
fn collapse_singleton_enum(map: &mut serde_json::Map<String, serde_json::Value>) {
    let Some(serde_json::Value::Array(branches)) = map.get("oneOf") else {
        return;
    };
    if branches.is_empty() || map.contains_key("enum") {
        return;
    }
    let mut values: Vec<serde_json::Value> = Vec::with_capacity(branches.len());
    for branch in branches {
        let Some(object) = branch.as_object() else {
            return;
        };
        let Some(variants) = object.get("enum").and_then(serde_json::Value::as_array) else {
            return;
        };
        if variants.len() != 1
            || object.get("type") != Some(&serde_json::Value::String("string".to_owned()))
        {
            return;
        }
        values.push(variants[0].clone());
    }
    map.remove("oneOf");
    map.insert("enum".to_owned(), serde_json::Value::Array(values));
    map.insert(
        "type".to_owned(),
        serde_json::Value::String("string".to_owned()),
    );
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
