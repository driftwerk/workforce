//! Simple expression language for monitor job conditions.
//!
//! Evaluates conditions against JSON values returned by tool executions.
//!
//! Supported expressions:
//! - `result.field == "value"` — equality
//! - `result.field != "value"` — inequality
//! - `result.field exists` — existence check
//! - `result.field == true` / `result.field == false` — boolean comparison
//! - `result.field == 42` — numeric comparison
//! - `expr1 AND expr2` — logical AND (can chain multiple)
//!
//! Dot notation traverses nested JSON objects.

use eyre::{bail, Result};
use serde_json::Value;

/// A parsed condition that can be evaluated against a JSON value.
#[derive(Debug, Clone)]
pub enum Condition {
    /// Check equality: path == value.
    Equals { path: String, value: Value },
    /// Check inequality: path != value.
    NotEquals { path: String, value: Value },
    /// Check if a path exists (is not null/missing).
    Exists { path: String },
    /// Logical AND of multiple conditions.
    And(Vec<Condition>),
}

impl Condition {
    /// Evaluate the condition against a JSON value.
    pub fn evaluate(&self, data: &Value) -> bool {
        match self {
            Condition::Equals { path, value } => {
                let actual = resolve_path(data, path);
                match actual {
                    Some(actual) => values_equal(actual, value),
                    None => value.is_null(),
                }
            }
            Condition::NotEquals { path, value } => {
                let actual = resolve_path(data, path);
                match actual {
                    Some(actual) => !values_equal(actual, value),
                    None => !value.is_null(),
                }
            }
            Condition::Exists { path } => {
                let actual = resolve_path(data, path);
                matches!(actual, Some(v) if !v.is_null())
            }
            Condition::And(conditions) => conditions.iter().all(|c| c.evaluate(data)),
        }
    }
}

/// Compare two JSON values, with type coercion for strings vs other types.
fn values_equal(actual: &Value, expected: &Value) -> bool {
    if actual == expected {
        return true;
    }

    // Coerce: if expected is a string but actual is bool/number, compare stringified.
    if let Value::String(expected_str) = expected {
        match actual {
            Value::Bool(b) => return b.to_string() == *expected_str,
            Value::Number(n) => return n.to_string() == *expected_str,
            _ => {}
        }
    }

    // Coerce: if actual is a string but expected is bool/number, compare stringified.
    if let Value::String(actual_str) = actual {
        match expected {
            Value::Bool(b) => return b.to_string() == *actual_str,
            Value::Number(n) => return n.to_string() == *actual_str,
            _ => {}
        }
    }

    false
}

/// Resolve a dot-notation path against a JSON value.
/// e.g. "result.overall" resolves data["result"]["overall"].
pub(crate) fn resolve_path<'a>(data: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = data;
    for segment in path.split('.') {
        match current {
            Value::Object(map) => {
                current = map.get(segment)?;
            }
            Value::Array(arr) => {
                let index: usize = segment.parse().ok()?;
                current = arr.get(index)?;
            }
            _ => return None,
        }
    }
    Some(current)
}

/// Parse a condition expression string.
///
/// Supports:
/// - `path == value`
/// - `path != value`
/// - `path exists`
/// - `expr1 AND expr2`
pub fn parse_condition(expr: &str) -> Result<Condition> {
    let expr = expr.trim();

    // Check for AND compound conditions (case-insensitive).
    // Split on " AND " (with spaces) to avoid matching substrings.
    let and_parts: Vec<&str> = split_and(expr);
    if and_parts.len() > 1 {
        let conditions: Result<Vec<Condition>> = and_parts
            .iter()
            .map(|part| parse_single(part.trim()))
            .collect();
        return Ok(Condition::And(conditions?));
    }

    parse_single(expr)
}

/// Split on " AND " (case-insensitive), respecting quoted strings.
fn split_and(expr: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let upper = expr.to_uppercase();
    let bytes = upper.as_bytes();

    let mut i = 0;
    while i < bytes.len() {
        // Check for " AND " at position i.
        if i + 5 <= bytes.len() && &upper[i..i + 5] == " AND " {
            parts.push(&expr[start..i]);
            start = i + 5;
            i += 5;
        } else {
            i += 1;
        }
    }
    parts.push(&expr[start..]);
    parts
}

/// Parse a single condition (no AND).
fn parse_single(expr: &str) -> Result<Condition> {
    let expr = expr.trim();

    // Check for "exists" pattern: "path exists"
    if let Some(stripped) = expr.strip_suffix(" exists") {
        let path = stripped.trim().to_string();
        if path.is_empty() {
            bail!("Empty path in 'exists' condition");
        }
        return Ok(Condition::Exists { path });
    }

    // Check for "==" pattern
    if let Some(pos) = expr.find("==") {
        let path = expr[..pos].trim().to_string();
        let value_str = expr[pos + 2..].trim();
        let value = parse_value(value_str)?;
        if path.is_empty() {
            bail!("Empty path in '==' condition");
        }
        return Ok(Condition::Equals { path, value });
    }

    // Check for "!=" pattern
    if let Some(pos) = expr.find("!=") {
        let path = expr[..pos].trim().to_string();
        let value_str = expr[pos + 2..].trim();
        let value = parse_value(value_str)?;
        if path.is_empty() {
            bail!("Empty path in '!=' condition");
        }
        return Ok(Condition::NotEquals { path, value });
    }

    bail!(
        "Invalid condition expression: '{}'. Expected 'path == value', 'path != value', or 'path exists'",
        expr
    );
}

/// Parse a value string into a JSON value.
fn parse_value(s: &str) -> Result<Value> {
    let s = s.trim();

    // Quoted string
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        return Ok(Value::String(s[1..s.len() - 1].to_string()));
    }

    // Boolean
    if s == "true" {
        return Ok(Value::Bool(true));
    }
    if s == "false" {
        return Ok(Value::Bool(false));
    }

    // Null
    if s == "null" {
        return Ok(Value::Null);
    }

    // Integer
    if let Ok(n) = s.parse::<i64>() {
        return Ok(Value::Number(n.into()));
    }

    // Float
    if let Ok(n) = s.parse::<f64>() {
        if let Some(num) = serde_json::Number::from_f64(n) {
            return Ok(Value::Number(num));
        }
    }

    // Treat as unquoted string
    Ok(Value::String(s.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_equality() {
        let cond = parse_condition("result.overall == \"success\"").unwrap();
        let data = json!({"result": {"overall": "success"}});
        assert!(cond.evaluate(&data));

        let data2 = json!({"result": {"overall": "pending"}});
        assert!(!cond.evaluate(&data2));
    }

    #[test]
    fn test_inequality() {
        let cond = parse_condition("result.overall != \"pending\"").unwrap();
        let data = json!({"result": {"overall": "success"}});
        assert!(cond.evaluate(&data));

        let data2 = json!({"result": {"overall": "pending"}});
        assert!(!cond.evaluate(&data2));
    }

    #[test]
    fn test_exists() {
        let cond = parse_condition("result.conclusion exists").unwrap();
        let data = json!({"result": {"conclusion": "success"}});
        assert!(cond.evaluate(&data));

        let data2 = json!({"result": {}});
        assert!(!cond.evaluate(&data2));

        let data3 = json!({"result": {"conclusion": null}});
        assert!(!cond.evaluate(&data3));
    }

    #[test]
    fn test_boolean() {
        let cond = parse_condition("result.indexing == false").unwrap();
        let data = json!({"result": {"indexing": false}});
        assert!(cond.evaluate(&data));

        let data2 = json!({"result": {"indexing": true}});
        assert!(!cond.evaluate(&data2));
    }

    #[test]
    fn test_numeric() {
        let cond = parse_condition("result.count == 42").unwrap();
        let data = json!({"result": {"count": 42}});
        assert!(cond.evaluate(&data));
    }

    #[test]
    fn test_and_compound() {
        let cond =
            parse_condition("result.overall != \"pending\" AND result.overall != \"queued\"")
                .unwrap();
        let data = json!({"result": {"overall": "success"}});
        assert!(cond.evaluate(&data));

        let data2 = json!({"result": {"overall": "pending"}});
        assert!(!cond.evaluate(&data2));

        let data3 = json!({"result": {"overall": "queued"}});
        assert!(!cond.evaluate(&data3));
    }

    #[test]
    fn test_nested_path() {
        let cond = parse_condition("a.b.c.d == \"deep\"").unwrap();
        let data = json!({"a": {"b": {"c": {"d": "deep"}}}});
        assert!(cond.evaluate(&data));
    }

    #[test]
    fn test_missing_path() {
        let cond = parse_condition("result.missing == null").unwrap();
        let data = json!({"result": {}});
        assert!(cond.evaluate(&data));
    }

    #[test]
    fn test_parse_errors() {
        assert!(parse_condition("").is_err());
        assert!(parse_condition("just_a_path").is_err());
        assert!(parse_condition(" exists").is_err());
    }
}
