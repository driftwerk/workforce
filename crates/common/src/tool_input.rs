//! Shrink tool-use input payloads for context budgeting.
//!
//! Walks a JSON value and elides bulky array/string/object fields, keeping
//! small fields verbatim so the round remains debuggable. The Anthropic API
//! requires `tool_use.input` to be a JSON object — every shrink output
//! satisfies that invariant, including for non-object inputs.
//!
//! The shrink is idempotent: marker strings produced by an earlier pass are
//! small enough that re-running on the output is a no-op.

use serde_json::{Map, Value};

const ARRAY_MARKER_PREFIX: &str = "<array:";
const STRING_MARKER_PREFIX: &str = "<string:";
const OBJECT_MARKER_PREFIX: &str = "<object:";
const NON_OBJECT_KEY: &str = "_truncated";
const OVERFLOW_KEY: &str = "_overflow_keys";

/// Result of a single [`shrink_tool_input`] call. Callers use the byte deltas
/// to update token estimates after mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShrinkOutcome {
    pub shrunk: bool,
    pub bytes_before: usize,
    pub bytes_after: usize,
}

impl ShrinkOutcome {
    fn unchanged(bytes: usize) -> Self {
        Self {
            shrunk: false,
            bytes_before: bytes,
            bytes_after: bytes,
        }
    }
}

/// Shrink a tool-use input payload in place. Always leaves `input` as a JSON
/// object so the Anthropic API contract holds. `max_field_bytes` caps any
/// individual field's serialised size; `max_total_bytes` caps the whole
/// object. Both clamp to a minimum of 1 byte.
pub fn shrink_tool_input(
    input: &mut Value,
    max_field_bytes: usize,
    max_total_bytes: usize,
) -> ShrinkOutcome {
    let max_field_bytes = max_field_bytes.max(1);
    let max_total_bytes = max_total_bytes.max(1);

    let bytes_before = input.to_string().len();

    let Some(obj) = input.as_object_mut() else {
        *input = Value::Object(non_object_stub(bytes_before));
        return ShrinkOutcome {
            shrunk: true,
            bytes_before,
            bytes_after: input.to_string().len(),
        };
    };

    if bytes_before <= max_total_bytes {
        return ShrinkOutcome::unchanged(bytes_before);
    }

    shrink_object(obj, max_field_bytes);
    if input.to_string().len() > max_total_bytes {
        if let Some(obj) = input.as_object_mut() {
            cap_object_total(obj, max_total_bytes);
        }
    }

    let bytes_after = input.to_string().len();
    ShrinkOutcome {
        shrunk: bytes_after != bytes_before,
        bytes_before,
        bytes_after,
    }
}

fn shrink_object(obj: &mut Map<String, Value>, max_field_bytes: usize) {
    for (_, value) in obj.iter_mut() {
        shrink_value(value, max_field_bytes);
    }
}

fn shrink_value(value: &mut Value, max_field_bytes: usize) {
    let serialized = value.to_string().len();
    if serialized <= max_field_bytes {
        return;
    }
    match value {
        Value::Array(items) => {
            let marker = format!(
                "{ARRAY_MARKER_PREFIX} {} items, {serialized} bytes elided>",
                items.len()
            );
            *value = Value::String(marker);
        }
        Value::String(s) => {
            let marker = format!("{STRING_MARKER_PREFIX} {} bytes elided>", s.len());
            *value = Value::String(marker);
        }
        Value::Object(nested) => {
            shrink_object(nested, max_field_bytes);
            let recomputed = Value::Object(nested.clone()).to_string().len();
            if recomputed > max_field_bytes {
                let marker = format!(
                    "{OBJECT_MARKER_PREFIX} {} keys, {recomputed} bytes elided>",
                    nested.len()
                );
                *value = Value::String(marker);
            }
        }
        Value::Number(_) | Value::Bool(_) | Value::Null => {}
    }
}

fn cap_object_total(obj: &mut Map<String, Value>, max_total_bytes: usize) {
    const MAX_OVERFLOW_SAMPLE: usize = 8;

    let entries: Vec<(String, Value)> = obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect();

    let mut kept = Map::new();
    let mut overflow_sample: Vec<String> = Vec::new();
    let mut overflow_count: usize = 0;
    let mut running = 2;

    for (k, v) in entries {
        let entry_bytes = k.len() + v.to_string().len() + 4;
        if running + entry_bytes <= max_total_bytes {
            running += entry_bytes;
            kept.insert(k, v);
        } else {
            if overflow_sample.len() < MAX_OVERFLOW_SAMPLE {
                overflow_sample.push(k);
            }
            overflow_count += 1;
        }
    }

    if overflow_count > 0 {
        let marker = format!(
            "<{} keys elided; sample: {}>",
            overflow_count,
            overflow_sample.join(", ")
        );
        kept.insert(OVERFLOW_KEY.to_string(), Value::String(marker));
    }

    *obj = kept;
}

fn non_object_stub(bytes_before: usize) -> Map<String, Value> {
    let mut stub = Map::new();
    stub.insert(
        NON_OBJECT_KEY.to_string(),
        Value::String(format!("<non-object input: {bytes_before} bytes elided>")),
    );
    stub
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shrink_returns_object_for_non_object_input() {
        for mut input in [json!(null), json!("hello world"), json!(42), json!(true)] {
            let outcome = shrink_tool_input(&mut input, 256, 1024);
            assert!(outcome.shrunk, "expected shrink for {input}");
            assert!(input.is_object(), "expected object, got {input}");
            assert!(input.get(NON_OBJECT_KEY).is_some(), "missing marker key");
        }
    }

    #[test]
    fn shrink_is_noop_for_small_input() {
        let mut input = json!({"sheet_id": "abc", "range": "Sheet1!A1"});
        let before = input.clone();
        let outcome = shrink_tool_input(&mut input, 256, 1024);
        assert!(!outcome.shrunk);
        assert_eq!(input, before);
        assert_eq!(outcome.bytes_before, outcome.bytes_after);
    }

    #[test]
    fn shrink_replaces_large_array_field() {
        let rows: Vec<Value> = (0..1000).map(|i| json!([format!("row{i}"), i])).collect();
        let mut input = json!({
            "sheet_id": "abc",
            "range": "Sheet1!A1",
            "values": rows,
        });

        let outcome = shrink_tool_input(&mut input, 256, 1024);

        assert!(outcome.shrunk);
        assert_eq!(input["sheet_id"], "abc");
        assert_eq!(input["range"], "Sheet1!A1");
        let marker = input["values"].as_str().expect("array elided to string");
        assert!(
            marker.starts_with(ARRAY_MARKER_PREFIX),
            "got marker: {marker}"
        );
        assert!(marker.contains("1000 items"), "marker: {marker}");
    }

    #[test]
    fn shrink_replaces_large_string_field() {
        let huge = "x".repeat(10_000);
        let mut input = json!({"body": huge});
        let outcome = shrink_tool_input(&mut input, 256, 1024);
        assert!(outcome.shrunk);
        let marker = input["body"].as_str().expect("string elided");
        assert!(marker.starts_with(STRING_MARKER_PREFIX), "marker: {marker}");
        assert!(marker.contains("10000 bytes"), "marker: {marker}");
    }

    #[test]
    fn shrink_recurses_into_nested_objects() {
        let inner: Vec<Value> = (0..500).map(|i| json!(i)).collect();
        let mut input = json!({
            "filter": {
                "where": {
                    "in": inner,
                }
            }
        });
        let outcome = shrink_tool_input(&mut input, 256, 1024);
        assert!(outcome.shrunk);
        let inner_marker = input["filter"]["where"]["in"]
            .as_str()
            .expect("inner array elided");
        assert!(inner_marker.starts_with(ARRAY_MARKER_PREFIX));
    }

    #[test]
    fn shrink_handles_pathological_many_small_keys() {
        let mut obj = Map::new();
        for i in 0..500 {
            obj.insert(format!("k{i:03}"), json!(i));
        }
        let mut input = Value::Object(obj);

        let outcome = shrink_tool_input(&mut input, 256, 256);

        assert!(outcome.shrunk);
        assert!(input.is_object());
        let overflow = input
            .get(OVERFLOW_KEY)
            .and_then(|v| v.as_str())
            .expect("overflow marker recorded");
        assert!(overflow.contains("keys elided"), "marker: {overflow}");
        assert!(
            input.to_string().len() <= 512,
            "after shrink: {} bytes",
            input.to_string().len()
        );
    }

    #[test]
    fn shrink_is_idempotent() {
        let rows: Vec<Value> = (0..1000).map(|i| json!([format!("row{i}"), i])).collect();
        let mut input = json!({
            "sheet_id": "abc",
            "values": rows,
        });
        let first = shrink_tool_input(&mut input, 256, 1024);
        let snapshot = input.clone();
        let second = shrink_tool_input(&mut input, 256, 1024);
        assert!(first.shrunk);
        assert!(!second.shrunk);
        assert_eq!(input, snapshot);
    }

    #[test]
    fn shrink_outcome_reports_correct_bytes() {
        let rows: Vec<Value> = (0..200).map(|i| json!([i])).collect();
        let mut input = json!({"values": rows});
        let before = input.to_string().len();
        let outcome = shrink_tool_input(&mut input, 256, 1024);
        let after = input.to_string().len();
        assert_eq!(outcome.bytes_before, before);
        assert_eq!(outcome.bytes_after, after);
        assert!(outcome.shrunk);
        assert!(after < before);
    }

    #[test]
    fn shrink_clamps_min_one_byte() {
        let mut input = json!({"a": 1});
        let outcome = shrink_tool_input(&mut input, 0, 0);
        assert!(outcome.shrunk);
        assert!(input.is_object());
    }
}
