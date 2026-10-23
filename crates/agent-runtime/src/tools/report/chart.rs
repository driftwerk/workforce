//! Report `ChartIntent` → the shared `wf-chart` renderer. The intent is the
//! report IR's row-oriented shape (data rows plus which fields are x and y);
//! lowering it to a `ChartSpec` means one renderer draws charts in reports,
//! Slack and email alike.
//!
//! `render` errors only when there is no usable numeric data; the caller then
//! degrades the block to a table.

use eyre::{eyre, Result};
use serde_json::Value;

use super::ir::{ChartIntent, ChartKind, Orientation};
use super::theme::Theme;
use crate::export::parse_numeric;
use crate::tools::chart::spec::{ChartSpec, Data, Kind, Series};

pub(crate) use crate::tools::chart::render::xml_escape;

pub fn render(intent: &ChartIntent, theme: &Theme) -> Result<String> {
    Ok(crate::tools::chart::render::render(&lower(intent)?, theme))
}

fn lower(intent: &ChartIntent) -> Result<ChartSpec> {
    let points = extract_points(intent);
    if points.is_empty() {
        return Err(eyre!("chart has no usable numeric data points"));
    }
    let (labels, values): (Vec<String>, Vec<f64>) = points.into_iter().unzip();
    Ok(ChartSpec {
        kind: match intent.kind {
            ChartKind::Bar => Kind::Bar,
            ChartKind::Line => Kind::Line,
            ChartKind::Area => Kind::Area,
            ChartKind::Donut => Kind::Pie,
        },
        title: None,
        labels: Some(labels),
        series: vec![Series {
            name: None,
            data: Data::Values(values),
        }],
        stacked: false,
        horizontal: intent.kind == ChartKind::Bar && intent.orientation == Orientation::Horizontal,
        x_label: None,
        y_label: None,
        unit: intent
            .value_suffix
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    })
}

type Point = (String, f64);

fn extract_points(intent: &ChartIntent) -> Vec<Point> {
    let x = intent.x.as_deref();
    let y = intent.y.as_deref();
    intent
        .data
        .iter()
        .filter_map(|row| {
            let obj = row.as_object()?;
            // The declared y field if it yields a number, else the first numeric
            // field that isn't x — so a mis-named or omitted y still charts
            // instead of silently degrading to a table.
            let value = y
                .and_then(|k| obj.get(k))
                .and_then(as_f64)
                .or_else(|| {
                    obj.iter()
                        .filter(|(k, _)| Some(k.as_str()) != x)
                        .find_map(|(_, v)| as_f64(v))
                })
                .filter(|v| v.is_finite())?;
            let label = x
                .and_then(|k| obj.get(k))
                .or_else(|| obj.values().next())
                .map(value_label)
                .unwrap_or_default();
            Some((label, value))
        })
        .collect()
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => parse_numeric(s),
        _ => None,
    }
}

fn value_label(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => crate::tools::chart::render::fmt_num(n.as_f64().unwrap_or_default()),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::report::theme;
    use serde_json::json;

    fn intent(kind: &str, data: Value) -> ChartIntent {
        serde_json::from_value(json!({ "kind": kind, "x": "k", "y": "v", "data": data })).unwrap()
    }

    #[test]
    fn extract_points_reads_fields_and_skips_non_numeric() {
        let pts = extract_points(&intent(
            "bar",
            json!([{ "k": "A", "v": 5 }, { "k": "B", "v": "x" }, { "k": "C", "v": "1,200" }]),
        ));
        assert_eq!(pts, vec![("A".into(), 5.0), ("C".into(), 1200.0)]);
    }

    #[test]
    fn extract_points_handles_formatted_values_and_misnamed_fields() {
        let pts = extract_points(&intent(
            "bar",
            json!([{ "k": "Rev", "v": "£3.1M" }, { "k": "Mkt", "v": "$138B" }, { "k": "NPS", "v": "94%" }]),
        ));
        assert_eq!(
            pts,
            vec![
                ("Rev".into(), 3_100_000.0),
                ("Mkt".into(), 138e9),
                ("NPS".into(), 94.0)
            ]
        );

        let mismatched: ChartIntent = serde_json::from_value(json!({
            "kind": "bar", "x": "month", "y": "count",
            "data": [{ "month": "Jan", "value": 120 }, { "month": "Feb", "value": 165 }]
        }))
        .unwrap();
        assert_eq!(
            extract_points(&mismatched),
            vec![("Jan".into(), 120.0), ("Feb".into(), 165.0)]
        );
    }

    #[test]
    fn lowering_maps_kind_orientation_and_suffix() {
        let bar = lower(&intent("bar", json!([{ "k": "A", "v": 1 }]))).unwrap();
        assert_eq!(bar.kind, Kind::Bar);
        assert!(bar.horizontal, "report bars default to horizontal");
        assert_eq!(bar.labels.as_deref(), Some(&["A".to_string()][..]));

        let donut: ChartIntent = serde_json::from_value(json!({
            "kind": "donut", "x": "k", "y": "v", "value_suffix": "%", "orientation": "vertical",
            "data": [{ "k": "A", "v": 1 }]
        }))
        .unwrap();
        let pie = lower(&donut).unwrap();
        assert_eq!(pie.kind, Kind::Pie);
        assert!(!pie.horizontal);
        assert_eq!(pie.unit.as_deref(), Some("%"));
    }

    #[test]
    fn empty_or_non_numeric_data_errors() {
        assert!(render(&intent("bar", json!([])), theme::resolve(None)).is_err());
        assert!(render(
            &intent("line", json!([{ "k": "A", "v": "nope" }])),
            theme::resolve(None)
        )
        .is_err());
    }

    #[test]
    fn every_kind_renders_well_formed_svg() {
        let t = theme::resolve(None);
        for kind in ["bar", "line", "area", "donut"] {
            let data = json!([{ "k": "A", "v": 10 }, { "k": "B", "v": 20 }, { "k": "C", "v": 5 }]);
            let out = render(&intent(kind, data), t).unwrap();
            assert!(out.starts_with("<svg") && out.ends_with("</svg>"), "{kind}");
        }
    }

    #[test]
    fn a_word_suffix_follows_the_value() {
        let bags: ChartIntent = serde_json::from_value(json!({
            "kind": "bar", "x": "k", "y": "v", "value_suffix": "bags",
            "data": [{ "k": "A", "v": 12 }]
        }))
        .unwrap();
        let out = render(&bags, theme::resolve(None)).unwrap();
        assert!(out.contains(">12 bags<"), "{out}");
    }

    #[test]
    fn donut_with_no_positive_values_falls_back_to_bars() {
        let out = render(
            &intent(
                "donut",
                json!([{ "k": "A", "v": 0 }, { "k": "B", "v": -2 }]),
            ),
            theme::resolve(None),
        )
        .unwrap();
        assert!(out.contains("barfill"), "expected a bar fallback");
    }
}
