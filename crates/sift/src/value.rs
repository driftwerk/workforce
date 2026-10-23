//! Runtime values, and their conversion to and from JSON at the host
//! boundary.

use crate::interp::Closure;
use std::cmp::Ordering;
use std::fmt::Write as _;
use std::ops::Deref;
use std::sync::Arc;

#[derive(Clone)]
pub(crate) enum Value {
    Null,
    Bool(bool),
    Int(i64),
    /// Always finite: arithmetic that would leave the finite range fails.
    Float(f64),
    Str(Arc<str>),
    List(Arc<List>),
    Record(Arc<Record>),
    Function(Arc<Closure>),
}

/// What walking a value costs: its size counts every node reached, shared
/// children once per reference, so it bounds converting it to JSON or text
/// and comparing it. Strings count a unit per 32 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Extent {
    pub size: u64,
    pub depth: u32,
}

impl Extent {
    fn scalar(size: u64) -> Self {
        Self { size, depth: 0 }
    }

    fn container<'v>(children: impl Iterator<Item = &'v Value>) -> Self {
        children.fold(Self { size: 1, depth: 1 }, |extent, child| {
            let inner = child.extent();
            Self {
                size: extent.size.saturating_add(inner.size),
                depth: extent.depth.max(inner.depth.saturating_add(1)),
            }
        })
    }
}

/// A list with its extent, computed once when it is built.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct List {
    items: Vec<Value>,
    extent: Extent,
}

impl List {
    fn new(items: Vec<Value>) -> Self {
        let extent = Extent::container(items.iter());
        Self { items, extent }
    }
}

impl Deref for List {
    type Target = [Value];

    fn deref(&self) -> &[Value] {
        &self.items
    }
}

/// Fields in the order they were written or received, with their extent.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Record {
    fields: Vec<(String, Value)>,
    extent: Extent,
}

impl Record {
    pub(crate) fn new(fields: Vec<(String, Value)>) -> Self {
        let extent = Extent::container(fields.iter().map(|(_, v)| v));
        Self { fields, extent }
    }

    pub(crate) fn get(&self, key: &str) -> Option<&Value> {
        self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub(crate) fn fields(&self) -> &[(String, Value)] {
        &self.fields
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = &str> {
        self.fields.iter().map(|(k, _)| k.as_str())
    }
}

/// Why a value could not become JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotData {
    Function,
    NonFinite,
}

impl NotData {
    pub(crate) fn describe(self) -> &'static str {
        match self {
            NotData::Function => "a function",
            NotData::NonFinite => "a non-finite number",
        }
    }
}

/// A scalar that can key a group or a join; equal keys are `==`-equal.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ScalarKey {
    Null,
    Bool(bool),
    Int(i64),
    Float(u64),
    Str(Arc<str>),
}

const SHORT_LIMIT: usize = 60;

impl Value {
    pub(crate) fn str(text: impl Into<Arc<str>>) -> Self {
        Value::Str(text.into())
    }

    pub(crate) fn list(items: Vec<Value>) -> Self {
        Value::List(Arc::new(List::new(items)))
    }

    pub(crate) fn record(fields: Vec<(String, Value)>) -> Self {
        Value::Record(Arc::new(Record::new(fields)))
    }

    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Str(_) => "string",
            Value::List(_) => "list",
            Value::Record(_) => "record",
            Value::Function(_) => "function",
        }
    }

    /// Type and a short rendering, for "got ..." in error messages.
    pub(crate) fn describe(&self) -> String {
        match self {
            Value::Null => "null".to_string(),
            Value::Function(_) => "a function".to_string(),
            Value::List(items) if items.is_empty() => "an empty list".to_string(),
            Value::List(items) => format!(
                "a list of {} element{}: {}",
                items.len(),
                plural(items.len()),
                self.short()
            ),
            _ => format!("{} {}", self.type_name(), self.short()),
        }
    }

    /// The rendering, cut to a few dozen characters.
    pub(crate) fn short(&self) -> String {
        let mut text = String::new();
        self.write_data(&mut text, true, SHORT_LIMIT);
        text
    }

    /// Text for `str(x)` and interpolation: strings as they are, everything
    /// else as compact JSON-like data.
    pub(crate) fn display(&self) -> String {
        match self {
            Value::Str(s) => s.to_string(),
            other => {
                let mut text = String::new();
                other.write_data(&mut text, true, usize::MAX);
                text
            }
        }
    }

    /// Write the value as data; stop adding once `limit` is passed, then
    /// end with `…`.
    fn write_data(&self, out: &mut String, top: bool, limit: usize) {
        if out.len() > limit {
            return;
        }
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => {
                let _ = write!(out, "{b}");
            }
            Value::Int(n) => {
                let _ = write!(out, "{n}");
            }
            Value::Float(n) => {
                let _ = write!(out, "{n}");
            }
            Value::Str(s) => {
                let _ = write!(out, "{}", serde_json::Value::String(s.to_string()));
            }
            Value::List(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if out.len() > limit {
                        break;
                    }
                    if i > 0 {
                        out.push_str(", ");
                    }
                    item.write_data(out, false, limit);
                }
                out.push(']');
            }
            Value::Record(record) => {
                out.push('{');
                for (i, (key, value)) in record.fields().iter().enumerate() {
                    if out.len() > limit {
                        break;
                    }
                    out.push_str(if i > 0 { ", " } else { " " });
                    let _ = write!(out, "{}: ", serde_json::Value::String(key.clone()));
                    value.write_data(out, false, limit);
                }
                out.push_str(if record.fields().is_empty() {
                    "}"
                } else {
                    " }"
                });
            }
            Value::Function(_) => out.push_str("<function>"),
        }
        if top && out.len() > limit {
            let cut = (0..=limit)
                .rev()
                .find(|&i| out.is_char_boundary(i))
                .unwrap_or(0);
            out.truncate(cut);
            out.push('…');
        }
    }

    pub(crate) fn extent(&self) -> Extent {
        match self {
            Value::List(list) => list.extent,
            Value::Record(record) => record.extent,
            Value::Str(s) => Extent::scalar(1 + (s.len() / 32) as u64),
            Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Function(_) => {
                Extent::scalar(1)
            }
        }
    }

    /// How much bulk work a library call does over this argument, in steps.
    pub(crate) fn weight(&self) -> u64 {
        match self {
            Value::List(items) => items.len() as u64,
            Value::Str(s) => (s.len() / 32) as u64,
            Value::Record(record) => record.fields().len() as u64,
            _ => 0,
        }
    }

    pub(crate) fn as_number(&self) -> Option<f64> {
        match self {
            Value::Int(n) => Some(*n as f64),
            Value::Float(n) => Some(*n),
            _ => None,
        }
    }

    /// Structural equality. Numbers compare by value across int and float;
    /// functions are never equal.
    pub(crate) fn equals(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::List(a), Value::List(b)) => {
                a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.equals(y))
            }
            (Value::Record(a), Value::Record(b)) => {
                a.fields().len() == b.fields().len()
                    && a.fields()
                        .iter()
                        .all(|(k, v)| b.get(k).is_some_and(|w| v.equals(w)))
            }
            (a, b) => matches!(a.compare(b), Some(Ordering::Equal)),
        }
    }

    /// Ordering between two numbers or two strings; None otherwise.
    pub(crate) fn compare(&self, other: &Value) -> Option<Ordering> {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
            (Value::Str(a), Value::Str(b)) => Some(a.cmp(b)),
            (a, b) => Some(a.as_number()?.total_cmp(&b.as_number()?)),
        }
    }

    pub(crate) fn scalar_key(&self) -> Option<ScalarKey> {
        Some(match self {
            Value::Null => ScalarKey::Null,
            Value::Bool(b) => ScalarKey::Bool(*b),
            Value::Int(n) => ScalarKey::Int(*n),
            Value::Float(f) if f.fract() == 0.0 && f.abs() < 9.2e18 => ScalarKey::Int(*f as i64),
            Value::Float(f) => ScalarKey::Float(f.to_bits()),
            Value::Str(s) => ScalarKey::Str(s.clone()),
            Value::List(_) | Value::Record(_) | Value::Function(_) => return None,
        })
    }

    pub(crate) fn to_json(&self) -> Result<serde_json::Value, NotData> {
        Ok(match self {
            Value::Null => serde_json::Value::Null,
            Value::Bool(b) => serde_json::Value::Bool(*b),
            Value::Int(n) => serde_json::Value::from(*n),
            Value::Float(n) => serde_json::Number::from_f64(*n)
                .map(serde_json::Value::Number)
                .ok_or(NotData::NonFinite)?,
            Value::Str(s) => serde_json::Value::String(s.to_string()),
            Value::List(items) => serde_json::Value::Array(
                items.iter().map(Value::to_json).collect::<Result<_, _>>()?,
            ),
            Value::Record(record) => serde_json::Value::Object(
                record
                    .fields()
                    .iter()
                    .map(|(k, v)| Ok((k.clone(), v.to_json()?)))
                    .collect::<Result<_, _>>()?,
            ),
            Value::Function(_) => return Err(NotData::Function),
        })
    }

    pub(crate) fn from_json(json: serde_json::Value) -> Self {
        match json {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(b),
            serde_json::Value::Number(n) => match n.as_i64() {
                Some(i) => Value::Int(i),
                None => n
                    .as_f64()
                    .filter(|f| f.is_finite())
                    .map_or(Value::Null, Value::Float),
            },
            serde_json::Value::String(s) => Value::str(s),
            serde_json::Value::Array(items) => {
                Value::list(items.into_iter().map(Value::from_json).collect())
            }
            serde_json::Value::Object(fields) => Value::record(
                fields
                    .into_iter()
                    .map(|(k, v)| (k, Value::from_json(v)))
                    .collect(),
            ),
        }
    }
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut text = String::new();
        self.write_data(&mut text, true, usize::MAX);
        f.write_str(&text)
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.equals(other)
    }
}

pub(crate) fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_round_trip_keeps_ints_and_floats() {
        let json = json!({ "a": 1, "b": 1.5, "c": [null, true, "x"], "d": { "e": -3 } });
        let value = Value::from_json(json.clone());
        assert!(matches!(value, Value::Record(ref r) if matches!(r.get("a"), Some(Value::Int(1)))));
        assert_eq!(value.to_json(), Ok(json));
        assert!(matches!(Value::from_json(json!(u64::MAX)), Value::Float(_)));
    }

    #[test]
    fn functions_and_non_finite_are_not_data() {
        assert_eq!(
            Value::Float(f64::INFINITY).to_json(),
            Err(NotData::NonFinite)
        );
    }

    #[test]
    fn structural_equality() {
        let a = Value::from_json(json!({ "x": [1, 2.0, "s"], "y": null }));
        let b = Value::from_json(json!({ "y": null, "x": [1.0, 2, "s"] }));
        assert!(a.equals(&b));
        assert!(!a.equals(&Value::from_json(json!({ "x": [1, 2, "s"] }))));
        assert!(!Value::Int(1).equals(&Value::str("1")));
        assert!(!Value::Null.equals(&Value::Bool(false)));
    }

    #[test]
    fn ordering_only_on_numbers_and_strings() {
        assert_eq!(
            Value::Int(1).compare(&Value::Float(1.5)),
            Some(Ordering::Less)
        );
        assert_eq!(
            Value::str("b").compare(&Value::str("a")),
            Some(Ordering::Greater)
        );
        assert_eq!(Value::str("1").compare(&Value::Int(1)), None);
        assert_eq!(Value::Bool(true).compare(&Value::Bool(false)), None);
    }

    #[test]
    fn scalar_keys_follow_equality() {
        assert_eq!(Value::Int(2).scalar_key(), Value::Float(2.0).scalar_key());
        assert_ne!(Value::Int(2).scalar_key(), Value::str("2").scalar_key());
        assert_eq!(Value::list(vec![]).scalar_key(), None);
    }

    #[test]
    fn rendering() {
        let value = Value::from_json(json!({ "name": "Ann \"A\"", "tags": ["x", 1.5, null] }));
        assert_eq!(
            value.display(),
            r#"{ "name": "Ann \"A\"", "tags": ["x", 1.5, null] }"#
        );
        assert_eq!(Value::str("plain").display(), "plain");
        assert_eq!(Value::Float(3.0).display(), "3");
        let long = Value::list((0..100).map(Value::Int).collect());
        let short = long.short();
        assert!(
            short.ends_with('…') && short.chars().count() <= SHORT_LIMIT + 2,
            "{short}"
        );
        assert!(long
            .describe()
            .starts_with("a list of 100 elements: [0, 1,"));
        assert_eq!(Value::list(vec![]).describe(), "an empty list");
        assert_eq!(Value::str("é".repeat(40)).short().chars().last(), Some('…'));
    }
}
