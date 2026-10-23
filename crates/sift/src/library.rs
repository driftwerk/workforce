//! Built-in functions and list methods that need nothing from the host.
//!
//! Functions that take a callback (`group_by`, `sort_by`, `join`) and the
//! list methods apply it in the interpreter, which may run tool calls inside
//! it; the grouping, sorting and joining over the computed keys is here.

use crate::value::{ScalarKey, Value};
use std::collections::HashMap;

/// A library error: the message names the function, what it got and what it
/// needed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LibError {
    pub message: String,
    pub help: Option<String>,
}

impl LibError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            help: None,
        }
    }

    fn help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }
}

type Lib<T> = Result<T, LibError>;

fn needs(function: &str, what: &str, got: &Value) -> LibError {
    LibError::new(format!("`{function}` needs {what}; got {}", got.describe()))
}

#[derive(Clone, Copy)]
pub(crate) enum Builtin {
    Unary(fn(&Value) -> Lib<Value>),
    Binary(fn(&Value, &Value) -> Lib<Value>),
    GroupBy,
    SortBy {
        descending: bool,
    },
    Join,
    All,
    /// Host-backed: run through the host like a tool call.
    ArtifactRead,
    ArtifactWrite,
}

impl Builtin {
    pub(crate) fn arity(self) -> usize {
        match self {
            Builtin::Unary(_) | Builtin::All | Builtin::ArtifactRead => 1,
            Builtin::Binary(_)
            | Builtin::GroupBy
            | Builtin::SortBy { .. }
            | Builtin::ArtifactWrite => 2,
            Builtin::Join => 4,
        }
    }
}

pub(crate) struct Function {
    pub name: &'static str,
    pub signature: &'static str,
    pub builtin: Builtin,
    /// Walks its arguments all the way down (to text, or comparing), so it
    /// costs their deep size rather than their length.
    pub walks: bool,
}

pub(crate) const FUNCTIONS: &[Function] = &[
    Function {
        name: "one",
        signature: "one(xs)",
        builtin: Builtin::Unary(one),
        walks: false,
    },
    Function {
        name: "first",
        signature: "first(xs)",
        builtin: Builtin::Unary(first),
        walks: false,
    },
    Function {
        name: "count",
        signature: "count(xs)",
        builtin: Builtin::Unary(count),
        walks: false,
    },
    Function {
        name: "sum",
        signature: "sum(xs)",
        builtin: Builtin::Unary(sum),
        walks: false,
    },
    Function {
        name: "min",
        signature: "min(xs)",
        builtin: Builtin::Unary(min),
        walks: false,
    },
    Function {
        name: "max",
        signature: "max(xs)",
        builtin: Builtin::Unary(max),
        walks: false,
    },
    Function {
        name: "keys",
        signature: "keys(record)",
        builtin: Builtin::Unary(keys),
        walks: false,
    },
    Function {
        name: "values",
        signature: "values(record)",
        builtin: Builtin::Unary(values),
        walks: false,
    },
    Function {
        name: "len",
        signature: "len(list_string_or_record)",
        builtin: Builtin::Unary(len),
        walks: false,
    },
    Function {
        name: "str",
        signature: "str(x)",
        builtin: Builtin::Unary(str),
        walks: true,
    },
    Function {
        name: "int",
        signature: "int(x)",
        builtin: Builtin::Unary(int),
        walks: false,
    },
    Function {
        name: "float",
        signature: "float(x)",
        builtin: Builtin::Unary(float),
        walks: false,
    },
    Function {
        name: "round",
        signature: "round(x, digits)",
        builtin: Builtin::Binary(round),
        walks: false,
    },
    Function {
        name: "contains",
        signature: "contains(haystack, needle)",
        builtin: Builtin::Binary(contains),
        walks: true,
    },
    Function {
        name: "lower",
        signature: "lower(s)",
        builtin: Builtin::Unary(lower),
        walks: false,
    },
    Function {
        name: "upper",
        signature: "upper(s)",
        builtin: Builtin::Unary(upper),
        walks: false,
    },
    Function {
        name: "split",
        signature: "split(s, sep)",
        builtin: Builtin::Binary(split),
        walks: false,
    },
    Function {
        name: "join_str",
        signature: "join_str(xs, sep)",
        builtin: Builtin::Binary(join_str),
        walks: false,
    },
    Function {
        name: "group_by",
        signature: "group_by(xs, x => key)",
        builtin: Builtin::GroupBy,
        walks: false,
    },
    Function {
        name: "sort_by",
        signature: "sort_by(xs, x => key)",
        builtin: Builtin::SortBy { descending: false },
        walks: false,
    },
    Function {
        name: "sort_by_desc",
        signature: "sort_by_desc(xs, x => key)",
        builtin: Builtin::SortBy { descending: true },
        walks: false,
    },
    Function {
        name: "join",
        signature: "join(left, right, l => key, r => key)",
        builtin: Builtin::Join,
        walks: false,
    },
    Function {
        name: "table",
        signature: "table(rows)",
        builtin: Builtin::Unary(table),
        walks: false,
    },
    Function {
        name: "all",
        signature: "all([calls]) or all(xs.map(x => call))",
        builtin: Builtin::All,
        walks: false,
    },
    Function {
        name: "artifact_read",
        signature: "artifact_read(artifact_id)",
        builtin: Builtin::ArtifactRead,
        walks: false,
    },
    Function {
        name: "artifact_write",
        signature: "artifact_write(filename, rows)",
        builtin: Builtin::ArtifactWrite,
        walks: true,
    },
];

pub(crate) fn function(name: &str) -> Option<&'static Function> {
    FUNCTIONS.iter().find(|f| f.name == name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    Map,
    Filter,
    FlatMap,
    Find,
    Any,
    Every,
    Take,
}

pub(crate) const METHODS: &[(&str, Method, &str)] = &[
    ("map", Method::Map, "xs.map(x => value)"),
    ("filter", Method::Filter, "xs.filter(x => bool)"),
    ("flat_map", Method::FlatMap, "xs.flat_map(x => list)"),
    ("find", Method::Find, "xs.find(x => bool)"),
    ("any", Method::Any, "xs.any(x => bool)"),
    ("every", Method::Every, "xs.every(x => bool)"),
    ("take", Method::Take, "xs.take(n)"),
];

pub(crate) fn method(name: &str) -> Option<(Method, &'static str)> {
    METHODS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, m, signature)| (*m, *signature))
}

pub(crate) fn list<'v>(function: &str, value: &'v Value) -> Lib<&'v [Value]> {
    match value {
        Value::List(items) => Ok(items),
        other => Err(needs(function, "a list", other)),
    }
}

fn string<'v>(function: &str, what: &str, value: &'v Value) -> Lib<&'v str> {
    match value {
        Value::Str(s) => Ok(s),
        other => Err(needs(function, what, other)),
    }
}

fn one(xs: &Value) -> Lib<Value> {
    match list("one", xs)? {
        [item] => Ok(item.clone()),
        [] => {
            Err(needs("one", "exactly one element", xs)
                .help("use `first(xs)` when there may be none"))
        }
        _ => {
            Err(needs("one", "exactly one element", xs)
                .help("narrow the search, or take `first(xs)`"))
        }
    }
}

fn first(xs: &Value) -> Lib<Value> {
    Ok(list("first", xs)?.first().cloned().unwrap_or(Value::Null))
}

fn count(xs: &Value) -> Lib<Value> {
    Ok(Value::Int(list("count", xs)?.len() as i64))
}

fn non_number(function: &str, index: usize, element: &Value) -> LibError {
    let error = LibError::new(format!(
        "`{function}` needs a list of numbers; element {index} is {}",
        element.describe()
    ));
    match element {
        Value::Null => error.help("drop nulls first: `xs.filter(x => x != null)`"),
        Value::Str(_) => error.help("convert first: `xs.map(x => float(x))`"),
        _ => error,
    }
}

fn sum(xs: &Value) -> Lib<Value> {
    list("sum", xs)?
        .iter()
        .enumerate()
        .try_fold(Value::Int(0), |total, (index, item)| {
            match (&total, item, total.as_number(), item.as_number()) {
                (Value::Int(a), Value::Int(b), _, _) => {
                    a.checked_add(*b).map(Value::Int).ok_or_else(|| {
                        LibError::new("`sum` overflowed a 64-bit int")
                            .help("sum floats instead: `sum(xs.map(x => float(x)))`")
                    })
                }
                (_, _, Some(a), Some(b)) => finite("sum", a + b),
                _ => Err(non_number("sum", index, item)),
            }
        })
}

fn extreme(function: &str, xs: &Value, keep: std::cmp::Ordering) -> Lib<Value> {
    let items = list(function, xs)?;
    let Some(head) = items.first() else {
        return Ok(Value::Null);
    };
    let comparable = |v: &Value| matches!(v, Value::Int(_) | Value::Float(_) | Value::Str(_));
    if !comparable(head) {
        return Err(needs(function, "a list of numbers or of strings", head)
            .help("pick the field to compare: `max(xs.map(x => x.n))`"));
    }
    items
        .iter()
        .enumerate()
        .skip(1)
        .try_fold(head, |best, (index, item)| match item.compare(best) {
            Some(order) if order == keep => Ok(item),
            Some(_) => Ok(best),
            None => Err(LibError::new(format!(
                "`{function}` needs a list of numbers or of strings; element 0 is {} but element {index} is {}",
                head.type_name(),
                item.describe()
            ))),
        })
        .cloned()
}

fn min(xs: &Value) -> Lib<Value> {
    extreme("min", xs, std::cmp::Ordering::Less)
}

fn max(xs: &Value) -> Lib<Value> {
    extreme("max", xs, std::cmp::Ordering::Greater)
}

fn keys(r: &Value) -> Lib<Value> {
    match r {
        Value::Record(record) => Ok(Value::list(record.keys().map(Value::str).collect())),
        other => Err(needs("keys", "a record", other)),
    }
}

fn values(r: &Value) -> Lib<Value> {
    match r {
        Value::Record(record) => Ok(Value::list(
            record.fields().iter().map(|(_, v)| v.clone()).collect(),
        )),
        other => Err(needs("values", "a record", other)),
    }
}

fn len(x: &Value) -> Lib<Value> {
    let n = match x {
        Value::List(items) => items.len(),
        Value::Str(s) => s.chars().count(),
        Value::Record(record) => record.fields().len(),
        other => return Err(needs("len", "a list, string or record", other)),
    };
    Ok(Value::Int(n as i64))
}

fn str(x: &Value) -> Lib<Value> {
    match x {
        Value::Function(_) => Err(needs("str", "data", x)),
        Value::Str(_) => Ok(x.clone()),
        other => Ok(Value::str(other.display())),
    }
}

fn int(x: &Value) -> Lib<Value> {
    match x {
        Value::Int(_) => Ok(x.clone()),
        Value::Float(f) => {
            let truncated = f.trunc();
            if (i64::MIN as f64..-(i64::MIN as f64)).contains(&truncated) {
                Ok(Value::Int(truncated as i64))
            } else {
                Err(needs("int", "a float within the 64-bit int range", x))
            }
        }
        Value::Str(s) => s.trim().parse::<i64>().map(Value::Int).map_err(|_| {
            needs("int", "a string holding an integer", x)
                .help("for a decimal string use `int(float(s))`")
        }),
        other => Err(needs("int", "an int, float or numeric string", other)),
    }
}

fn float(x: &Value) -> Lib<Value> {
    match x {
        Value::Int(n) => Ok(Value::Float(*n as f64)),
        Value::Float(_) => Ok(x.clone()),
        Value::Str(s) => s
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .map(Value::Float)
            .ok_or_else(|| needs("float", "a string holding a number", x)),
        other => Err(needs("float", "an int, float or numeric string", other)),
    }
}

fn finite(function: &str, f: f64) -> Lib<Value> {
    if f.is_finite() {
        Ok(Value::Float(f))
    } else {
        Err(LibError::new(format!(
            "`{function}` overflowed the float range"
        )))
    }
}

fn round(x: &Value, digits: &Value) -> Lib<Value> {
    let digits = match digits {
        Value::Int(d @ 0..=12) => *d as i32,
        other => return Err(needs("round", "a digit count from 0 to 12", other)),
    };
    match x {
        Value::Int(_) => Ok(x.clone()),
        Value::Float(f) => {
            let scale = 10f64.powi(digits);
            let scaled = f * scale;
            Ok(Value::Float(if scaled.is_finite() {
                scaled.round() / scale
            } else {
                *f
            }))
        }
        other => Err(needs("round", "a number", other)),
    }
}

fn contains(haystack: &Value, needle: &Value) -> Lib<Value> {
    match haystack {
        Value::Str(s) => Ok(Value::Bool(s.contains(string(
            "contains",
            "a string needle for a string haystack",
            needle,
        )?))),
        Value::List(items) => Ok(Value::Bool(items.iter().any(|item| item.equals(needle)))),
        other => Err(needs("contains", "a string or list to search", other)),
    }
}

fn lower(s: &Value) -> Lib<Value> {
    Ok(Value::str(string("lower", "a string", s)?.to_lowercase()))
}

fn upper(s: &Value) -> Lib<Value> {
    Ok(Value::str(string("upper", "a string", s)?.to_uppercase()))
}

fn split(s: &Value, sep: &Value) -> Lib<Value> {
    let text = string("split", "a string to split", s)?;
    let separator = string("split", "a string separator", sep)?;
    if separator.is_empty() {
        return Err(needs("split", "a non-empty separator", sep));
    }
    Ok(Value::list(text.split(separator).map(Value::str).collect()))
}

fn join_str(xs: &Value, sep: &Value) -> Lib<Value> {
    let separator = string("join_str", "a string separator", sep)?;
    let parts = list("join_str", xs)?
        .iter()
        .enumerate()
        .map(|(index, item)| match item {
            Value::Str(s) => Ok(s.as_ref()),
            other => Err(LibError::new(format!(
                "`join_str` needs a list of strings; element {index} is {}",
                other.describe()
            ))
            .help("convert first: `join_str(xs.map(x => str(x)), sep)`")),
        })
        .collect::<Lib<Vec<&str>>>()?;
    Ok(Value::str(parts.join(separator)))
}

fn table(rows: &Value) -> Lib<Value> {
    let items = list("table", rows)?;
    match items
        .iter()
        .position(|row| !matches!(row, Value::Record(_)))
    {
        None => Ok(rows.clone()),
        Some(index) => Err(LibError::new(format!(
            "`table` needs a list of records; row {index} is {}",
            items[index].describe()
        ))),
    }
}

pub(crate) fn take(xs: &[Value], n: &Value) -> Lib<Value> {
    match n {
        Value::Int(n) if *n >= 0 => Ok(Value::list(
            xs.iter()
                .take(usize::try_from(*n).unwrap_or(usize::MAX))
                .cloned()
                .collect(),
        )),
        other => Err(needs("take", "a count of zero or more", other)),
    }
}

/// Keys computed by a key function, checked to be scalars.
fn scalar_keys(function: &str, keys: &[Value]) -> Lib<Vec<ScalarKey>> {
    keys.iter()
        .enumerate()
        .map(|(index, key)| {
            key.scalar_key().ok_or_else(|| {
                LibError::new(format!(
                    "`{function}`'s key function must return a string, number, bool or null; for element {index} it returned {}",
                    key.describe()
                ))
                .help("combine fields into one string: `x => \"${x.a}|${x.b}\"`")
            })
        })
        .collect()
}

/// Groups in order of first appearance: `[{ key, items }]`.
pub(crate) fn group(items: &[Value], keys: Vec<Value>) -> Lib<Value> {
    let scalars = scalar_keys("group_by", &keys)?;
    let mut index: HashMap<ScalarKey, usize> = HashMap::new();
    let mut groups: Vec<(Value, Vec<Value>)> = Vec::new();
    for ((item, key), scalar) in items.iter().zip(keys).zip(scalars) {
        let slot = *index.entry(scalar).or_insert_with(|| {
            groups.push((key, Vec::new()));
            groups.len() - 1
        });
        groups[slot].1.push(item.clone());
    }
    Ok(Value::list(
        groups
            .into_iter()
            .map(|(key, members)| {
                Value::record(vec![
                    ("key".to_string(), key),
                    ("items".to_string(), Value::list(members)),
                ])
            })
            .collect(),
    ))
}

/// A stable sort by precomputed keys, which must all be numbers or all
/// strings.
pub(crate) fn sort(
    function: &str,
    items: &[Value],
    keys: Vec<Value>,
    descending: bool,
) -> Lib<Value> {
    let numeric = |v: &Value| matches!(v, Value::Int(_) | Value::Float(_));
    let textual = |v: &Value| matches!(v, Value::Str(_));
    let kind: fn(&Value) -> bool = match keys.first() {
        Some(k) if numeric(k) => numeric,
        _ => textual,
    };
    if let Some(index) = keys.iter().position(|k| !kind(k)) {
        return Err(LibError::new(format!(
            "`{function}`'s keys must be all numbers or all strings; element {index}'s key is {}",
            keys[index].describe()
        ))
        .help("map missing keys to a default: `x => x.n ?? 0`"));
    }
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by(|&a, &b| {
        let ordering = keys[a]
            .compare(&keys[b])
            .unwrap_or(std::cmp::Ordering::Equal);
        if descending {
            ordering.reverse()
        } else {
            ordering
        }
    });
    Ok(Value::list(
        order.into_iter().map(|i| items[i].clone()).collect(),
    ))
}

/// An inner join on `==`-equal keys, indexed so the number of pairs is
/// known before any is built.
pub(crate) struct JoinIndex {
    left: Vec<ScalarKey>,
    right: HashMap<ScalarKey, Vec<usize>>,
}

impl JoinIndex {
    pub(crate) fn new(left_keys: &[Value], right_keys: &[Value]) -> Lib<Self> {
        let left = scalar_keys("join", left_keys)?;
        let mut right: HashMap<ScalarKey, Vec<usize>> = HashMap::new();
        for (index, key) in scalar_keys("join", right_keys)?.into_iter().enumerate() {
            right.entry(key).or_default().push(index);
        }
        Ok(Self { left, right })
    }

    fn matches(&self, key: &ScalarKey) -> &[usize] {
        self.right.get(key).map_or(&[], Vec::as_slice)
    }

    pub(crate) fn pairs(&self) -> u64 {
        self.left
            .iter()
            .map(|key| self.matches(key).len() as u64)
            .fold(0, u64::saturating_add)
    }

    /// `[{ left, right }]` in left-then-right order.
    pub(crate) fn build(&self, left: &[Value], right: &[Value]) -> Value {
        let pairs = left
            .iter()
            .zip(&self.left)
            .flat_map(|(l, key)| {
                self.matches(key).iter().map(move |&r| {
                    Value::record(vec![
                        ("left".to_string(), l.clone()),
                        ("right".to_string(), right[r].clone()),
                    ])
                })
            })
            .collect();
        Value::list(pairs)
    }
}

/// The receiver of a list method.
pub(crate) fn method_receiver<'v>(name: &str, value: &'v Value) -> Lib<&'v [Value]> {
    match value {
        Value::List(items) => Ok(items),
        other => Err(LibError::new(format!(
            "`.{name}` needs a list; got {}",
            other.describe()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn v(json: serde_json::Value) -> Value {
        Value::from_json(json)
    }

    fn call(name: &str, args: &[Value]) -> Lib<Value> {
        match (function(name).map(|f| f.builtin), args) {
            (Some(Builtin::Unary(f)), [a]) => f(a),
            (Some(Builtin::Binary(f)), [a, b]) => f(a, b),
            _ => panic!("{name} is not a plain function of {} args", args.len()),
        }
    }

    fn ok(name: &str, args: &[Value]) -> serde_json::Value {
        call(name, args)
            .unwrap_or_else(|e| panic!("{name}: {e:?}"))
            .to_json()
            .unwrap_or_else(|e| panic!("{e:?}"))
    }

    fn err(name: &str, args: &[Value]) -> LibError {
        call(name, args).expect_err(name)
    }

    #[test]
    fn one_and_first() {
        assert_eq!(ok("one", &[v(json!([7]))]), json!(7));
        assert_eq!(
            err("one", &[v(json!([1, 2, 3]))]).message,
            "`one` needs exactly one element; got a list of 3 elements: [1, 2, 3]"
        );
        let none = err("one", &[v(json!([]))]);
        assert_eq!(
            none.message,
            "`one` needs exactly one element; got an empty list"
        );
        assert!(none.help.is_some_and(|h| h.contains("first")));
        assert_eq!(
            err("one", &[v(json!({ "a": 1 }))]).message,
            "`one` needs a list; got record { \"a\": 1 }"
        );
        assert_eq!(ok("first", &[v(json!([]))]), json!(null));
        assert_eq!(ok("first", &[v(json!([4, 5]))]), json!(4));
    }

    #[test]
    fn numbers() {
        assert_eq!(ok("count", &[v(json!([1, 2]))]), json!(2));
        assert_eq!(ok("sum", &[v(json!([1, 2, 3]))]), json!(6));
        assert_eq!(ok("sum", &[v(json!([1, 2.5]))]), json!(3.5));
        assert_eq!(ok("sum", &[v(json!([]))]), json!(0));
        let e = err("sum", &[v(json!([1, null]))]);
        assert_eq!(
            e.message,
            "`sum` needs a list of numbers; element 1 is null"
        );
        assert!(e.help.is_some());
        assert!(err("sum", &[v(json!([i64::MAX, 1]))])
            .message
            .contains("overflowed"));
        assert_eq!(ok("min", &[v(json!([3, 1.5, 2]))]), json!(1.5));
        assert_eq!(ok("max", &[v(json!(["b", "c", "a"]))]), json!("c"));
        assert_eq!(ok("max", &[v(json!([]))]), json!(null));
        assert!(err("max", &[v(json!([1, "a"]))])
            .message
            .contains("element 1 is string \"a\""));
        assert!(err("min", &[v(json!([{ "a": 1 }]))])
            .message
            .starts_with("`min` needs a list of numbers or of strings"));
        assert_eq!(
            ok("round", &[Value::Float(2.345), Value::Int(2)]),
            json!(2.35)
        );
        assert_eq!(
            ok("round", &[Value::Float(-2.5), Value::Int(0)]),
            json!(-3.0)
        );
        assert_eq!(ok("round", &[Value::Int(7), Value::Int(1)]), json!(7));
        assert_eq!(
            ok("round", &[Value::Float(1e300), Value::Int(12)]),
            json!(1e300)
        );
        assert!(err("round", &[Value::Float(1.0), Value::Int(-1)])
            .message
            .contains("0 to 12"));
    }

    #[test]
    fn conversions() {
        assert_eq!(ok("int", &[v(json!(" 42 "))]), json!(42));
        assert_eq!(ok("int", &[Value::Float(-3.9)]), json!(-3));
        assert_eq!(ok("int", &[Value::Int(5)]), json!(5));
        assert_eq!(
            err("int", &[v(json!("3.5"))]).message,
            "`int` needs a string holding an integer; got string \"3.5\""
        );
        assert!(err("int", &[Value::Float(1e19)]).message.contains("range"));
        assert!(err("int", &[Value::Bool(true)])
            .message
            .contains("got bool true"));
        assert_eq!(ok("float", &[v(json!("2.5"))]), json!(2.5));
        assert_eq!(ok("float", &[Value::Int(2)]), json!(2.0));
        assert!(err("float", &[v(json!("inf"))])
            .message
            .contains("holding a number"));
        assert_eq!(ok("str", &[v(json!([1, "a"]))]), json!("[1, \"a\"]"));
        assert_eq!(ok("str", &[Value::Float(2.5)]), json!("2.5"));
        assert_eq!(ok("str", &[Value::Null]), json!("null"));
    }

    #[test]
    fn records_and_sizes() {
        let r = v(json!({ "a": 1, "b": [2] }));
        let mut k = ok("keys", std::slice::from_ref(&r))
            .as_array()
            .cloned()
            .unwrap_or_default();
        k.sort_by_key(|x| x.to_string());
        assert_eq!(k, vec![json!("a"), json!("b")]);
        assert_eq!(
            ok("values", std::slice::from_ref(&r))
                .as_array()
                .map(Vec::len),
            Some(2)
        );
        assert_eq!(ok("len", &[r]), json!(2));
        assert_eq!(ok("len", &[v(json!("héllo"))]), json!(5));
        assert_eq!(ok("len", &[v(json!([1, 2, 3]))]), json!(3));
        assert_eq!(
            err("len", &[Value::Int(1)]).message,
            "`len` needs a list, string or record; got int 1"
        );
        assert_eq!(
            err("keys", &[v(json!([]))]).message,
            "`keys` needs a record; got an empty list"
        );
    }

    #[test]
    fn strings() {
        assert_eq!(
            ok("contains", &[v(json!("hello")), v(json!("ell"))]),
            json!(true)
        );
        assert_eq!(
            ok(
                "contains",
                &[v(json!([1, { "a": 2 }])), v(json!({ "a": 2.0 }))]
            ),
            json!(true)
        );
        assert!(err("contains", &[v(json!("a")), Value::Int(1)])
            .message
            .contains("string needle"));
        assert!(err("contains", &[Value::Int(1), Value::Int(1)])
            .message
            .contains("string or list"));
        assert_eq!(ok("lower", &[v(json!("AbC"))]), json!("abc"));
        assert_eq!(ok("upper", &[v(json!("AbC"))]), json!("ABC"));
        assert_eq!(
            ok("split", &[v(json!("a,b,,c")), v(json!(","))]),
            json!(["a", "b", "", "c"])
        );
        assert!(err("split", &[v(json!("a")), v(json!(""))])
            .message
            .contains("non-empty"));
        assert_eq!(
            ok("join_str", &[v(json!(["a", "b"])), v(json!("-"))]),
            json!("a-b")
        );
        let e = err("join_str", &[v(json!(["a", 1])), v(json!("-"))]);
        assert_eq!(
            e.message,
            "`join_str` needs a list of strings; element 1 is int 1"
        );
        assert!(e.help.is_some_and(|h| h.contains("str(x)")));
    }

    #[test]
    fn table_passes_records_through() {
        assert_eq!(ok("table", &[v(json!([{ "a": 1 }]))]), json!([{ "a": 1 }]));
        assert_eq!(
            err("table", &[v(json!([{ "a": 1 }, 2]))]).message,
            "`table` needs a list of records; row 1 is int 2"
        );
    }

    #[test]
    fn grouping_sorting_joining() {
        let items = v(json!(["a1", "b1", "a2"]));
        let Value::List(items) = items else { panic!() };
        let keys = vec![Value::str("a"), Value::str("b"), Value::str("a")];
        assert_eq!(
            group(&items, keys).map(|g| g.to_json()).ok(),
            Some(Ok(json!([
                { "key": "a", "items": ["a1", "a2"] },
                { "key": "b", "items": ["b1"] }
            ])))
        );
        let bad =
            group(&items, vec![v(json!({})), Value::Null, Value::Null]).expect_err("record key");
        assert!(bad.message.contains("for element 0 it returned record {}"));

        let sorted = sort(
            "sort_by",
            &items,
            vec![Value::Int(2), Value::Float(1.5), Value::Int(2)],
            false,
        );
        assert_eq!(
            sorted.map(|s| s.to_json()).ok(),
            Some(Ok(json!(["b1", "a1", "a2"])))
        );
        let desc = sort(
            "sort_by_desc",
            &items,
            vec![Value::Int(2), Value::Int(1), Value::Int(2)],
            true,
        );
        assert_eq!(
            desc.map(|s| s.to_json()).ok(),
            Some(Ok(json!(["a1", "a2", "b1"])))
        );
        let mixed = sort(
            "sort_by",
            &items,
            vec![Value::Int(1), Value::Null, Value::Int(2)],
            false,
        )
        .expect_err("mixed keys");
        assert_eq!(
            mixed.message,
            "`sort_by`'s keys must be all numbers or all strings; element 1's key is null"
        );

        let left = [Value::Int(1), Value::Int(2), Value::Int(3)];
        let right = [Value::str("x"), Value::str("y"), Value::str("z")];
        let index = JoinIndex::new(&left, &[Value::Int(2), Value::Float(1.0), Value::Int(2)])
            .expect("scalar keys");
        assert_eq!(index.pairs(), 3);
        assert_eq!(
            Some(index.build(&left, &right).to_json()),
            Some(Ok(json!([
                { "left": 1, "right": "y" },
                { "left": 2, "right": "x" },
                { "left": 2, "right": "z" }
            ])))
        );
    }

    #[test]
    fn take_counts() {
        let xs = [Value::Int(1), Value::Int(2)];
        assert_eq!(
            take(&xs, &Value::Int(1)).map(|t| t.to_json()).ok(),
            Some(Ok(json!([1])))
        );
        assert_eq!(
            take(&xs, &Value::Int(9)).map(|t| t.to_json()).ok(),
            Some(Ok(json!([1, 2])))
        );
        assert!(take(&xs, &Value::Int(-1)).is_err());
    }

    #[test]
    fn tables_are_consistent() {
        for f in FUNCTIONS {
            assert!(f.signature.starts_with(f.name), "{}", f.name);
        }
        for (name, _, signature) in METHODS {
            assert!(signature.starts_with(&format!("xs.{name}(")), "{name}");
        }
    }
}
