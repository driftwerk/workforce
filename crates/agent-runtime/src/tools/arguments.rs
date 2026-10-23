//! Checking a tool call's arguments against the tool's input schema before
//! it runs, and telling the model exactly what to fix when they don't match.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, LazyLock};

use jsonschema::Validator;
use parking_lot::RwLock;
use serde_json::{Map, Value};

use crate::llm::tool_schema::sanitize_tool_schema;

pub use workforce_common::settings::ArgumentValidation;

/// At most this many problems are reported; the rest usually share a cause.
const MAX_PROBLEMS: usize = 5;

/// Distinct schemas are few (one per tool definition); a cache this large
/// only fills when schemas are generated per call, and then starts over.
const MAX_CACHED_SCHEMAS: usize = 4096;

/// One thing wrong with a call's arguments.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ArgumentProblem {
    /// JSON pointer to the offending value; empty for the arguments as a whole.
    pub path: String,
    pub message: String,
}

/// What checking a call's arguments found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArgumentReport {
    /// Violations of the schema: at most [`MAX_PROBLEMS`], in path order,
    /// without repeats. Messages never quote the values sent.
    pub problems: Vec<ArgumentProblem>,
    /// Top-level keys the schema does not declare, which a plain JSON Schema
    /// allows.
    pub unknown_keys: Vec<String>,
}

/// Check `input` against `schema`, or `None` when the schema does not
/// compile.
///
/// A call is checked the way tools read it: an absent input is an empty
/// object, a `null` optional argument is an absent one, and `format` is
/// descriptive rather than a rule.
pub fn check_arguments(schema: &Value, input: &Value) -> Option<ArgumentReport> {
    let validator = compiled(schema)?;
    let input = as_tools_read_it(schema, input);
    let unknown_keys: Vec<String> = match (input.as_object(), declared_keys(schema)) {
        (Some(arguments), Some(declared)) => arguments
            .keys()
            .filter(|key| !declared.contains(key.as_str()))
            .cloned()
            .collect(),
        _ => Vec::new(),
    };
    let problems = validator
        .iter_errors(&input)
        .map(|error| ArgumentProblem {
            path: error.instance_path().as_str().to_string(),
            message: error.masked().to_string(),
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(MAX_PROBLEMS)
        .collect();
    Some(ArgumentReport {
        problems,
        unknown_keys,
    })
}

static COMPILED: LazyLock<RwLock<HashMap<String, Option<Arc<Validator>>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// The validator for `schema`, compiled once per distinct schema. Keyed by
/// content, so a tool whose schema changes gets a new validator and every
/// registry sharing a tool shares its validator.
fn compiled(schema: &Value) -> Option<Arc<Validator>> {
    let key = schema.to_string();
    if let Some(cached) = COMPILED.read().get(&key) {
        return cached.clone();
    }
    let validator = jsonschema::options()
        .should_validate_formats(false)
        .build(&nullable_as_null_type(schema.clone()))
        .inspect_err(|error| {
            tracing::warn!(%error, "Tool input schema does not compile; its arguments go unchecked")
        })
        .ok()
        .map(Arc::new);
    let mut cache = COMPILED.write();
    if cache.len() >= MAX_CACHED_SCHEMAS {
        cache.clear();
    }
    cache.entry(key).or_insert(validator).clone()
}

/// OpenAPI's `nullable: true`, as MCP servers often write it, spelled as the
/// JSON Schema it means: `null` joins the allowed types.
fn nullable_as_null_type(schema: Value) -> Value {
    match schema {
        Value::Object(map) => {
            let nullable = map.get("nullable") == Some(&Value::Bool(true));
            Value::Object(
                map.into_iter()
                    .map(|(key, value)| match key.as_str() {
                        "type" if nullable => (key, with_null_type(value)),
                        _ => (key, nullable_as_null_type(value)),
                    })
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.into_iter().map(nullable_as_null_type).collect()),
        other => other,
    }
}

fn with_null_type(kind: Value) -> Value {
    let null = Value::String("null".to_string());
    match kind {
        Value::String(_) => Value::Array(vec![kind, null]),
        Value::Array(mut kinds) if !kinds.contains(&null) => {
            kinds.push(null);
            Value::Array(kinds)
        }
        other => other,
    }
}

fn as_tools_read_it(schema: &Value, input: &Value) -> Value {
    let required: BTreeSet<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    match input {
        Value::Null => Value::Object(Map::new()),
        Value::Object(arguments) => Value::Object(
            arguments
                .iter()
                .filter(|(key, value)| !value.is_null() || required.contains(key.as_str()))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The top-level property names the model is shown for this schema (after
/// top-level combinators are folded in), or `None` when it declares none or
/// explicitly allows others.
fn declared_keys(schema: &Value) -> Option<BTreeSet<String>> {
    let open = schema
        .get("additionalProperties")
        .is_some_and(|additional| additional != &Value::Bool(false));
    let offered = sanitize_tool_schema(schema.clone());
    let properties = offered.get("properties")?.as_object()?;
    (!open && !properties.is_empty()).then(|| properties.keys().cloned().collect())
}

/// The message a model gets for a call that did not run because of its
/// arguments, with the tool's full schema: the model may never have been shown
/// it whole (a sanitised schema, a tool reached through `tool_call`).
pub fn render_argument_problems(
    tool_name: &str,
    problems: &[ArgumentProblem],
    full_schema: &Value,
) -> String {
    let lines: String = problems
        .iter()
        .map(|problem| match problem.path.as_str() {
            "" => format!("- {}\n", problem.message),
            path => format!("- `{path}`: {}\n", problem.message),
        })
        .collect();
    let schema = serde_json::to_string_pretty(full_schema)
        .map(|schema| format!("\nIts full input schema:\n```json\n{schema}\n```\n"))
        .unwrap_or_default();
    format!(
        "'{tool_name}' did not run: its arguments don't match its input schema.\n{lines}\
         Fix these and call it again.\n{schema}"
    )
}

/// A one-line summary of the problems, for logs and the call's recorded reason.
pub fn summarise_argument_problems(problems: &[ArgumentProblem]) -> String {
    let summary = problems
        .iter()
        .map(|problem| match problem.path.as_str() {
            "" => problem.message.clone(),
            path => format!("{path}: {}", problem.message),
        })
        .collect::<Vec<_>>()
        .join("; ");
    format!("invalid arguments: {summary}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["remember", "recall"] },
                "importance": { "type": "number" },
                "file_id": { "type": "string" },
                "tags": { "type": "array", "items": { "type": "string" } },
                "when": { "type": "string", "format": "date-time" },
                "note": { "type": "string", "nullable": true },
            },
            "required": ["action"],
        })
    }

    fn check(input: Value) -> ArgumentReport {
        check_arguments(&schema(), &input).unwrap()
    }

    #[test]
    fn a_valid_call_has_no_problems() {
        let report = check(json!({ "action": "recall", "tags": ["a"] }));
        assert_eq!(report, ArgumentReport::default());
    }

    #[test]
    fn each_violation_names_its_path_and_never_the_value_sent() {
        let report =
            check(json!({ "action": "store-this-secret", "importance": "very-high", "tags": [1] }));
        let by_path = |path: &str| {
            report
                .problems
                .iter()
                .find(|problem| problem.path == path)
                .unwrap()
                .message
                .clone()
        };
        assert!(by_path("/action").contains("remember"));
        assert!(by_path("/importance").contains("number"));
        assert!(by_path("/tags/0").contains("string"));
        assert!(report.problems.iter().all(|problem| {
            !problem.message.contains("store-this-secret") && !problem.message.contains("very-high")
        }));
        let missing = check(json!({}));
        assert_eq!(missing.problems.len(), 1);
        assert!(missing.problems[0].message.contains("action"));
    }

    #[test]
    fn a_call_is_read_the_way_tools_read_it() {
        let no_arguments = json!({ "type": "object", "properties": { "a": { "type": "string" } } });
        assert!(check_arguments(&no_arguments, &Value::Null)
            .unwrap()
            .problems
            .is_empty());
        assert!(check(json!({ "action": "recall", "importance": null }))
            .problems
            .is_empty());
        let required_null = check(json!({ "action": null }));
        assert!(!required_null.problems.is_empty());
        assert!(required_null
            .problems
            .iter()
            .all(|problem| problem.path == "/action"));
        assert!(check(json!({ "action": "recall", "when": "tomorrow" }))
            .problems
            .is_empty());
    }

    #[test]
    fn nullable_means_null_is_allowed() {
        let required_note = json!({
            "type": "object",
            "properties": { "note": { "type": "string", "nullable": true } },
            "required": ["note"],
        });
        assert!(check_arguments(&required_note, &json!({ "note": null }))
            .unwrap()
            .problems
            .is_empty());
        assert_eq!(
            check_arguments(&required_note, &json!({ "note": 1 }))
                .unwrap()
                .problems
                .len(),
            1
        );
    }

    #[test]
    fn unknown_keys_are_reported_but_are_not_problems() {
        let report = check(json!({ "action": "recall", "sql": "x" }));
        assert!(report.problems.is_empty());
        assert_eq!(report.unknown_keys, ["sql"]);
    }

    #[test]
    fn keys_a_folded_top_level_combinator_declares_are_known() {
        let schema = json!({
            "type": "object",
            "properties": { "name": { "type": "string" } },
            "oneOf": [
                { "properties": { "content": { "type": "string" } }, "required": ["content"] },
                { "properties": { "artifact_id": { "type": "string" } }, "required": ["artifact_id"] },
            ],
        });
        let report = check_arguments(&schema, &json!({ "name": "n", "artifact_id": "a" })).unwrap();
        assert_eq!(report, ArgumentReport::default());
    }

    #[test]
    fn a_schema_that_allows_other_keys_has_none_unknown() {
        let schema =
            json!({ "type": "object", "properties": { "a": {} }, "additionalProperties": true });
        let report = check_arguments(&schema, &json!({ "b": 1 })).unwrap();
        assert_eq!(report, ArgumentReport::default());
    }

    #[test]
    fn a_schema_that_does_not_compile_is_not_checked() {
        assert!(check_arguments(&json!({ "type": "not-a-type" }), &json!({})).is_none());
    }

    #[test]
    fn a_schema_compiles_once_and_a_changed_schema_compiles_again() {
        let first = compiled(&schema()).unwrap();
        assert!(Arc::ptr_eq(&first, &compiled(&schema()).unwrap()));
        let changed = json!({ "type": "object", "required": ["only"] });
        assert!(!Arc::ptr_eq(&first, &compiled(&changed).unwrap()));
    }

    #[test]
    fn problems_are_capped_ordered_and_unrepeated() {
        let report = check(
            json!({ "action": 1, "importance": [], "file_id": 2, "tags": "x", "when": 3, "note": 4 }),
        );
        assert_eq!(report.problems.len(), MAX_PROBLEMS);
        let mut sorted = report.problems.clone();
        sorted.sort();
        assert_eq!(report.problems, sorted);
    }

    #[test]
    fn the_rendered_message_lists_problems_and_the_full_schema() {
        let problems = vec![
            ArgumentProblem {
                path: "/action".into(),
                message: "value is not one of \"remember\" or \"recall\"".into(),
            },
            ArgumentProblem {
                path: String::new(),
                message: "\"action\" is a required property".into(),
            },
        ];
        let message = render_argument_problems("memory", &problems, &schema());
        assert!(message.starts_with("'memory' did not run"));
        assert!(message.contains("- `/action`: value is not one of"));
        assert!(message.contains("- \"action\" is a required property"));
        assert!(message.contains("Its full input schema:\n```json\n{"));
        assert_eq!(
            summarise_argument_problems(&problems),
            "invalid arguments: /action: value is not one of \"remember\" or \"recall\"; \"action\" is a required property"
        );
    }
}
