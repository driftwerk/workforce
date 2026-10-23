//! The `wf-chart` wire contract, server side.
//!
//! A line-for-line port of the widget's `validateChart` (`spec.ts`): the same
//! kinds, the same caps, the same lenient coercions (numeric strings become
//! numbers) and the same error wording, so a spec that draws in the widget
//! draws here and one that fails there fails here with the same message. Held
//! together by a golden corpus rather than by description — see
//! `fixtures/charts/` and `goldens.rs`.

use serde::Serialize;
use serde_json::Value;

pub const MAX_SERIES: usize = 8;
pub const MAX_POINTS: usize = 1000;

const KINDS: [&str; 5] = ["pie", "bar", "line", "area", "scatter"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Pie,
    Bar,
    Line,
    Area,
    Scatter,
}

impl Kind {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "pie" => Some(Self::Pie),
            "bar" => Some(Self::Bar),
            "line" => Some(Self::Line),
            "area" => Some(Self::Area),
            "scatter" => Some(Self::Scatter),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Data {
    Values(Vec<f64>),
    Pairs(Vec<(f64, f64)>),
}

impl Data {
    pub fn len(&self) -> usize {
        match self {
            Self::Values(v) => v.len(),
            Self::Pairs(p) => p.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The y values of a category series; empty for scatter pairs.
    pub fn values(&self) -> &[f64] {
        match self {
            Self::Values(v) => v,
            Self::Pairs(_) => &[],
        }
    }

    /// The `[x, y]` points of a scatter series; empty for category values.
    pub fn pairs(&self) -> &[(f64, f64)] {
        match self {
            Self::Values(_) => &[],
            Self::Pairs(p) => p,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Series {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub data: Data,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChartSpec {
    pub kind: Kind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
    pub series: Vec<Series>,
    pub stacked: bool,
    pub horizontal: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

impl ChartSpec {
    /// Category names along the x axis: the declared labels, else `1..=n`
    /// for the longest series, as the widget numbers unlabelled points.
    pub fn categories(&self) -> Vec<String> {
        self.labels.clone().unwrap_or_else(|| {
            let n = self.series.iter().map(|s| s.data.len()).max().unwrap_or(0);
            (1..=n).map(|i| i.to_string()).collect()
        })
    }
}

pub fn validate(value: &Value) -> Result<ChartSpec, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "payload must be a JSON object".to_string())?;
    let kind = obj
        .get("kind")
        .and_then(Value::as_str)
        .and_then(Kind::parse)
        .ok_or_else(|| format!("kind must be one of {}", KINDS.join(", ")))?;
    let raw_series = obj
        .get("series")
        .and_then(Value::as_array)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "series must be a non-empty array".to_string())?;
    if raw_series.len() > MAX_SERIES {
        return Err(format!("too many series (max {MAX_SERIES})"));
    }
    let scatter = kind == Kind::Scatter;
    let series = raw_series
        .iter()
        .enumerate()
        .map(|(i, s)| to_series(s, i, scatter))
        .collect::<Result<Vec<_>, _>>()?;
    let labels = obj
        .get("labels")
        .map(|v| {
            v.as_array()
                .filter(|a| a.iter().all(is_cell))
                .map(|a| a.iter().map(cell_string).collect::<Vec<_>>())
                .ok_or_else(|| "labels must be an array of names".to_string())
        })
        .transpose()?;
    if kind == Kind::Pie {
        if series.len() != 1 {
            return Err("a pie chart takes exactly one series".into());
        }
        let labels = labels
            .as_ref()
            .ok_or_else(|| "a pie chart needs labels for its slices".to_string())?;
        if labels.len() != series[0].data.len() {
            return Err("pie labels and data must be the same length".into());
        }
    } else if let (false, Some(labels)) = (scatter, &labels) {
        if let Some(i) = series.iter().position(|s| s.data.len() != labels.len()) {
            return Err(format!("series {} length does not match labels", i + 1));
        }
    }
    let flag = |key: &str| obj.get(key) == Some(&Value::Bool(true));
    Ok(ChartSpec {
        kind,
        title: optional_string(obj.get("title")),
        labels: if scatter { None } else { labels },
        series,
        stacked: flag("stacked") && kind != Kind::Pie && kind != Kind::Scatter,
        horizontal: flag("horizontal") && kind == Kind::Bar,
        x_label: optional_string(obj.get("x_label")),
        y_label: optional_string(obj.get("y_label")),
        unit: optional_string(obj.get("unit")),
    })
}

fn to_series(v: &Value, index: usize, scatter: bool) -> Result<Series, String> {
    let n = index + 1;
    let data = v
        .as_object()
        .and_then(|o| o.get("data"))
        .and_then(Value::as_array)
        .ok_or_else(|| format!("series {n} must be an object with a data array"))?;
    if data.is_empty() {
        return Err(format!("series {n} has no data"));
    }
    if data.len() > MAX_POINTS {
        return Err(format!("series {n} exceeds {MAX_POINTS} points"));
    }
    let name = optional_string(v.get("name"));
    let data = if scatter {
        Data::Pairs(
            data.iter()
                .map(|p| {
                    let pair = p
                        .as_array()
                        .filter(|a| a.len() == 2)
                        .ok_or_else(|| format!("series {n}: scatter points must be [x,y] pairs"))?;
                    match (to_finite_number(&pair[0]), to_finite_number(&pair[1])) {
                        (Some(x), Some(y)) => Ok((x, y)),
                        _ => Err(format!("series {n}: scatter points must be numeric")),
                    }
                })
                .collect::<Result<_, _>>()?,
        )
    } else {
        Data::Values(
            data.iter()
                .map(|p| {
                    to_finite_number(p).ok_or_else(|| format!("series {n}: data must be numbers"))
                })
                .collect::<Result<_, _>>()?,
        )
    };
    Ok(Series { name, data })
}

/// Numbers pass through; numeric strings (an LLM habit) coerce; all else fails.
fn to_finite_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64().filter(|f| f.is_finite()),
        Value::String(s) => js_number_from(s.trim()).filter(|f| f.is_finite()),
        _ => None,
    }
}

/// JavaScript's `Number(string)` for a trimmed, non-empty string: decimal
/// literals as Rust parses them, plus the unsigned `0x`/`0o`/`0b` forms.
fn js_number_from(s: &str) -> Option<f64> {
    if s.is_empty() {
        return None;
    }
    let radix = match s.get(..2).map(str::to_ascii_lowercase).as_deref() {
        Some("0x") => Some(16),
        Some("0o") => Some(8),
        Some("0b") => Some(2),
        _ => None,
    };
    match radix {
        Some(radix) => u64::from_str_radix(&s[2..], radix).ok().map(|n| n as f64),
        None => s
            .parse::<f64>()
            .ok()
            .filter(|_| !s.contains(['i', 'n', 'N', 'I']) || s.eq_ignore_ascii_case("infinity")),
    }
}

fn is_cell(v: &Value) -> bool {
    matches!(
        v,
        Value::Null | Value::String(_) | Value::Number(_) | Value::Bool(_)
    )
}

/// `String(cell ?? "")` — a number prints as JavaScript prints it.
fn cell_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.as_f64().map(js_number).unwrap_or_default(),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

/// JavaScript's `String(number)`: shortest round-trip digits, in plain
/// decimal up to 1e21 and down to 1e-6, exponent form beyond.
fn js_number(f: f64) -> String {
    if f == 0.0 {
        return "0".into();
    }
    if (1e-6..1e21).contains(&f.abs()) {
        return format!("{f}");
    }
    let exp = format!("{f:e}");
    match exp.split_once('e') {
        Some((mantissa, power)) if !power.starts_with('-') => format!("{mantissa}e+{power}"),
        _ => exp,
    }
}

pub(crate) fn optional_string(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(v: Value) -> ChartSpec {
        validate(&v).expect("valid")
    }

    #[test]
    fn coerces_numeric_strings_and_rejects_non_numeric() {
        let s = spec(json!({ "kind": "bar", "series": [{ "data": ["42", 7, " 1e3 "] }] }));
        assert_eq!(s.series[0].data, Data::Values(vec![42.0, 7.0, 1000.0]));
        assert_eq!(
            validate(&json!({ "kind": "bar", "series": [{ "data": ["nope"] }] })).unwrap_err(),
            "series 1: data must be numbers"
        );
        assert!(validate(&json!({ "kind": "bar", "series": [{ "data": [null] }] })).is_err());
    }

    #[test]
    fn scopes_stacked_and_horizontal_to_the_kinds_that_support_them() {
        let pie = spec(json!({
            "kind": "pie", "labels": ["a"], "series": [{ "data": [1] }],
            "stacked": true, "horizontal": true
        }));
        assert!(!pie.stacked && !pie.horizontal);
        let line = spec(
            json!({ "kind": "line", "series": [{ "data": [1] }], "stacked": true, "horizontal": true }),
        );
        assert!(line.stacked && !line.horizontal);
        let bar = spec(json!({ "kind": "bar", "series": [{ "data": [1] }], "horizontal": true }));
        assert!(bar.horizontal);
    }

    #[test]
    fn pie_needs_exactly_one_series_with_matching_labels() {
        let err = |v: Value| validate(&v).unwrap_err();
        assert_eq!(
            err(json!({ "kind": "pie", "series": [{ "data": [1] }] })),
            "a pie chart needs labels for its slices"
        );
        assert_eq!(
            err(
                json!({ "kind": "pie", "labels": ["a"], "series": [{ "data": [1] }, { "data": [2] }] })
            ),
            "a pie chart takes exactly one series"
        );
        assert_eq!(
            err(json!({ "kind": "pie", "labels": ["a", "b"], "series": [{ "data": [1] }] })),
            "pie labels and data must be the same length"
        );
    }

    #[test]
    fn scatter_drops_labels_and_requires_pairs() {
        let s = spec(
            json!({ "kind": "scatter", "labels": ["x"], "series": [{ "data": [[1, 2], ["3", 4]] }] }),
        );
        assert_eq!(s.labels, None);
        assert_eq!(s.series[0].data, Data::Pairs(vec![(1.0, 2.0), (3.0, 4.0)]));
        assert_eq!(
            validate(&json!({ "kind": "scatter", "series": [{ "data": [[1]] }] })).unwrap_err(),
            "series 1: scatter points must be [x,y] pairs"
        );
    }

    #[test]
    fn numeric_strings_coerce_as_javascript_number_does() {
        for (text, expected) in [
            ("0x10", Some(16.0)),
            ("0B11", Some(3.0)),
            ("0o17", Some(15.0)),
            ("+5", Some(5.0)),
            (".5", Some(0.5)),
            ("5.", Some(5.0)),
            ("1E2", Some(100.0)),
            ("-0x10", None),
            ("0x", None),
            ("1_000", None),
            ("nan", None),
            ("Infinity", None),
            ("inf", None),
        ] {
            assert_eq!(to_finite_number(&json!(text)), expected, "{text}");
        }
    }

    #[test]
    fn numbers_print_as_javascript_prints_them() {
        for (value, expected) in [
            (1e21, "1e+21"),
            (1e-7, "1e-7"),
            (-0.0, "0"),
            (0.1, "0.1"),
            (123456789012345680000.0, "123456789012345680000"),
            (1e-6, "0.000001"),
            (1.5e300, "1.5e+300"),
            (-2.5, "-2.5"),
            (2024.0, "2024"),
        ] {
            assert_eq!(js_number(value), expected, "{value}");
        }
    }

    #[test]
    fn labels_print_like_javascript_and_null_labels_fail_like_the_widget() {
        let s = spec(
            json!({ "kind": "bar", "labels": [2024, 2.5, true, null], "series": [{ "data": [1, 2, 3, 4] }] }),
        );
        assert_eq!(s.labels.unwrap(), vec!["2024", "2.5", "true", ""]);
        assert_eq!(
            validate(&json!({ "kind": "bar", "labels": null, "series": [{ "data": [1] }] }))
                .unwrap_err(),
            "labels must be an array of names"
        );
    }

    #[test]
    fn categories_number_unlabelled_points_from_one() {
        let s =
            spec(json!({ "kind": "line", "series": [{ "data": [5, 6] }, { "data": [1, 2, 3] }] }));
        assert_eq!(s.categories(), vec!["1", "2", "3"]);
    }

    #[test]
    fn serialises_like_the_widget_spec_object() {
        let s = spec(json!({ "kind": "bar", "title": "  ", "series": [{ "data": [1] }] }));
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            json!({ "kind": "bar", "series": [{ "data": [1.0] }], "stacked": false, "horizontal": false })
        );
    }
}
