//! Tool-schema sanitization for Anthropic-shaped tool validators.
//!
//! AWS Bedrock, the direct Anthropic Messages API and Anthropic-bound MCP
//! clients reject `oneOf` / `allOf` / `anyOf` at the top level of a tool's
//! `input_schema` (`input_schema does not support oneOf, allOf, or anyOf at
//! the top level`) and accept them anywhere below it. [`sanitize_tool_schema`]
//! folds the top-level ones into the root so the model still sees what they
//! said: branch properties join the root's, `allOf` requirements join its
//! `required`, and `oneOf` / `anyOf` alternatives are spelled out in its
//! `description`. Nested combinators pass through untouched.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

const COMBINATORS: [&str; 3] = ["allOf", "anyOf", "oneOf"];

const OPAQUE_SHAPES: &str =
    "Accepts several input shapes; call the tool and any error will show them.";

/// Fold top-level `oneOf` / `allOf` / `anyOf` into the root of a JSON Schema.
pub fn sanitize_tool_schema(value: Value) -> Value {
    match value {
        Value::Object(root) => Value::Object(hoist_combinators(root)),
        other => other,
    }
}

fn hoist_combinators(mut root: Map<String, Value>) -> Map<String, Value> {
    let removed: Vec<(&str, Value)> = COMBINATORS
        .iter()
        .filter_map(|key| root.remove(*key).map(|branches| (*key, branches)))
        .collect();
    let notes: Vec<String> = removed
        .into_iter()
        .flat_map(|(key, branches)| fold(&mut root, key, branches))
        .collect();
    if !notes.is_empty() {
        let description = root
            .get("description")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|existing| !existing.is_empty())
            .into_iter()
            .map(str::to_string)
            .chain(notes)
            .collect::<Vec<_>>()
            .join(" ");
        root.insert("description".to_string(), Value::String(description));
    }
    root
}

fn fold(root: &mut Map<String, Value>, key: &str, branches: Value) -> Vec<String> {
    let Value::Array(branches) = branches else {
        return Vec::new();
    };
    let (foldable, opaque): (Vec<Value>, Vec<Value>) = branches
        .into_iter()
        .partition(|branch| is_foldable(branch) && !shape_of(branch).is_empty());
    if !foldable.is_empty() {
        root.entry("type")
            .or_insert_with(|| Value::String("object".to_string()));
    }
    merge_properties(root, key, &foldable);
    let alternatives = if key == "allOf" {
        merge_required(root, foldable.iter().flat_map(required_of));
        None
    } else {
        alternatives_note(key, &foldable)
    };
    alternatives
        .into_iter()
        .chain((!opaque.is_empty()).then(|| OPAQUE_SHAPES.to_string()))
        .collect()
}

fn is_foldable(branch: &Value) -> bool {
    branch.as_object().is_some_and(|map| {
        !map.contains_key("$ref")
            && map.get("type").is_none_or(|kind| kind == "object")
            && !COMBINATORS.iter().any(|key| map.contains_key(*key))
    })
}

/// Adds every branch property the root does not declare. Branches that give
/// the same property different schemas keep them all, under a nested
/// combinator: `allOf` for an `allOf`, `anyOf` for a `oneOf` / `anyOf`, whose
/// branches each describe one alternative value.
fn merge_properties(root: &mut Map<String, Value>, key: &str, branches: &[Value]) {
    let by_name = branches
        .iter()
        .filter_map(|branch| branch.get("properties").and_then(Value::as_object))
        .flatten()
        .fold(
            BTreeMap::<&String, Vec<&Value>>::new(),
            |mut by_name, (name, schema)| {
                let schemas = by_name.entry(name).or_default();
                if !schemas.contains(&schema) {
                    schemas.push(schema);
                }
                by_name
            },
        );
    if by_name.is_empty() {
        return;
    }
    let nested = if key == "allOf" { "allOf" } else { "anyOf" };
    if let Value::Object(target) = root
        .entry("properties")
        .or_insert_with(|| Value::Object(Map::new()))
    {
        by_name.into_iter().for_each(|(name, schemas)| {
            target
                .entry(name.clone())
                .or_insert_with(|| match schemas.as_slice() {
                    [only] => (*only).clone(),
                    many => Value::Object(Map::from_iter([(
                        nested.to_string(),
                        Value::Array(many.iter().map(|schema| (*schema).clone()).collect()),
                    )])),
                });
        });
    }
}

fn merge_required(root: &mut Map<String, Value>, names: impl Iterator<Item = String>) {
    let merged = names.fold(names_in(root.get("required")), |mut all, name| {
        if !all.contains(&name) {
            all.push(name);
        }
        all
    });
    if !merged.is_empty() {
        root.insert(
            "required".to_string(),
            Value::Array(merged.into_iter().map(Value::String).collect()),
        );
    }
}

fn required_of(schema: &Value) -> Vec<String> {
    names_in(schema.get("required"))
}

fn names_in(list: Option<&Value>) -> Vec<String> {
    list.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn property_names(schema: &Value) -> Vec<String> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|properties| properties.keys().cloned())
        .collect()
}

/// The names a branch is recognised by: its required properties, or else
/// every property it declares. Empty for a branch that only constrains.
fn shape_of(branch: &Value) -> Vec<String> {
    match required_of(branch) {
        required if required.is_empty() => property_names(branch),
        required => required,
    }
}

fn alternatives_note(key: &str, branches: &[Value]) -> Option<String> {
    let shapes: Vec<String> = branches
        .iter()
        .map(shape_of)
        .map(|names| format!("{{{}}}", names.join(", ")))
        .collect();
    let quantity = if key == "oneOf" {
        "exactly one"
    } else {
        "at least one"
    };
    (!shapes.is_empty()).then(|| format!("Provide {quantity} of: {}.", shapes.join(" | ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn has_top_level_combinator(value: &Value) -> bool {
        COMBINATORS.iter().any(|key| value.get(*key).is_some())
    }

    #[test]
    fn one_of_required_alternatives_become_a_description_note() {
        let out = sanitize_tool_schema(json!({
            "type": "object",
            "description": "Upload a file.",
            "properties": {
                "content": { "type": "string" },
                "artifact_id": { "type": "string" },
            },
            "oneOf": [{ "required": ["content"] }, { "required": ["artifact_id"] }],
        }));
        assert!(!has_top_level_combinator(&out));
        assert_eq!(
            out["description"],
            json!("Upload a file. Provide exactly one of: {content} | {artifact_id}.")
        );
        assert_eq!(out["properties"]["content"], json!({ "type": "string" }));
        assert!(out.get("required").is_none());
    }

    #[test]
    fn any_of_branch_properties_join_the_root_and_the_root_wins_a_clash() {
        let out = sanitize_tool_schema(json!({
            "type": "object",
            "properties": { "id": { "type": "string", "description": "root" } },
            "anyOf": [
                { "properties": { "id": { "type": "integer" }, "email": { "type": "string" } } },
                { "properties": { "phone": { "type": "string" } }, "required": ["phone"] },
            ],
        }));
        assert!(!has_top_level_combinator(&out));
        assert_eq!(
            out["properties"]["id"],
            json!({ "type": "string", "description": "root" })
        );
        assert_eq!(out["properties"]["email"], json!({ "type": "string" }));
        assert_eq!(out["properties"]["phone"], json!({ "type": "string" }));
        assert_eq!(
            out["description"],
            json!("Provide at least one of: {email, id} | {phone}.")
        );
    }

    #[test]
    fn all_of_merges_properties_and_required_without_duplicates() {
        let out = sanitize_tool_schema(json!({
            "type": "object",
            "properties": { "a": { "type": "string" } },
            "required": ["a"],
            "allOf": [
                { "properties": { "b": { "type": "number" } }, "required": ["a", "b"] },
                { "required": ["c"] },
            ],
        }));
        assert!(!has_top_level_combinator(&out));
        assert_eq!(out["required"], json!(["a", "b", "c"]));
        assert_eq!(out["properties"]["b"], json!({ "type": "number" }));
        assert!(out.get("description").is_none());
    }

    #[test]
    fn branches_that_disagree_on_a_property_keep_every_alternative() {
        let out = sanitize_tool_schema(json!({
            "type": "object",
            "oneOf": [
                { "properties": { "action": { "const": "add" }, "ids": { "type": "array" } }, "required": ["action", "ids"] },
                { "properties": { "action": { "const": "clear" } }, "required": ["action"] },
            ],
        }));
        assert_eq!(
            out["properties"]["action"],
            json!({ "anyOf": [{ "const": "add" }, { "const": "clear" }] })
        );
        assert_eq!(out["properties"]["ids"], json!({ "type": "array" }));
        let all_of = sanitize_tool_schema(json!({
            "allOf": [
                { "properties": { "n": { "type": "integer" } } },
                { "properties": { "n": { "minimum": 1 } } },
            ],
        }));
        assert_eq!(
            all_of["properties"]["n"],
            json!({ "allOf": [{ "type": "integer" }, { "minimum": 1 }] })
        );
    }

    #[test]
    fn folding_gives_a_typeless_root_its_object_type() {
        let out = sanitize_tool_schema(json!({
            "anyOf": [{ "type": "object", "properties": { "a": { "type": "string" } } }],
        }));
        assert_eq!(out["type"], json!("object"));
        assert_eq!(
            sanitize_tool_schema(json!({ "type": "object", "oneOf": [{ "$ref": "#/x" }] }))["type"],
            json!("object")
        );
    }

    #[test]
    fn a_branch_that_only_constrains_counts_as_opaque() {
        let out = sanitize_tool_schema(json!({
            "type": "object",
            "oneOf": [{ "not": { "required": ["a"] } }, { "minProperties": 1 }],
        }));
        assert_eq!(out["description"], json!(OPAQUE_SHAPES));
    }

    #[test]
    fn a_branch_that_cannot_be_folded_leaves_an_opaque_note() {
        let out = sanitize_tool_schema(json!({
            "type": "object",
            "oneOf": [{ "$ref": "#/defs/a" }, { "type": "string" }, { "required": ["x"] }],
        }));
        assert!(!has_top_level_combinator(&out));
        assert_eq!(
            out["description"],
            json!(format!("Provide exactly one of: {{x}}. {OPAQUE_SHAPES}"))
        );
    }

    #[test]
    fn every_top_level_combinator_is_folded_in_a_fixed_order() {
        let out = sanitize_tool_schema(json!({
            "type": "object",
            "oneOf": [{ "required": ["a"] }, { "required": ["b"] }],
            "anyOf": [{ "required": ["c"] }],
            "allOf": [{ "required": ["d"] }],
        }));
        assert!(!has_top_level_combinator(&out));
        assert_eq!(out["required"], json!(["d"]));
        assert_eq!(
            out["description"],
            json!("Provide at least one of: {c}. Provide exactly one of: {a} | {b}.")
        );
    }

    #[test]
    fn nested_combinators_pass_through_at_every_depth() {
        let input = json!({
            "type": "object",
            "properties": {
                "operation": {
                    "anyOf": [
                        { "type": "object", "properties": { "add": { "type": "array" } } },
                        { "type": "object", "properties": { "remove": { "type": "array" } } },
                    ],
                },
                "items": {
                    "type": "array",
                    "items": { "type": "object", "oneOf": [{ "required": ["a"] }] },
                },
                "deep": {
                    "type": "object",
                    "properties": {
                        "inner": { "allOf": [{ "type": "object" }] },
                    },
                },
            },
        });
        assert_eq!(sanitize_tool_schema(input.clone()), input);
    }

    #[test]
    fn a_schema_without_top_level_combinators_is_unchanged() {
        let input = json!({
            "type": "object",
            "description": "Accepts oneOf the listed inputs",
            "properties": {
                "name": { "type": "string", "description": "user name" },
                "age": { "type": "integer", "minimum": 0 },
            },
            "required": ["name"],
            "additionalProperties": false,
        });
        assert_eq!(sanitize_tool_schema(input.clone()), input);
    }

    #[test]
    fn a_non_object_root_is_unchanged() {
        assert_eq!(sanitize_tool_schema(Value::Null), Value::Null);
        assert_eq!(sanitize_tool_schema(json!("hello")), json!("hello"));
        assert_eq!(sanitize_tool_schema(json!(42)), json!(42));
        assert_eq!(sanitize_tool_schema(json!([1, 2])), json!([1, 2]));
    }

    #[test]
    fn sanitising_twice_changes_nothing_more() {
        let once = sanitize_tool_schema(json!({
            "type": "object",
            "description": "d",
            "oneOf": [{ "required": ["a"] }, { "properties": { "b": { "type": "string" } } }],
            "allOf": [{ "required": ["c"] }],
        }));
        assert_eq!(sanitize_tool_schema(once.clone()), once);
    }

    #[test]
    fn a_combinator_that_is_not_an_array_is_dropped_without_a_note() {
        let out = sanitize_tool_schema(json!({ "type": "object", "oneOf": "nonsense" }));
        assert_eq!(out, json!({ "type": "object" }));
    }
}
