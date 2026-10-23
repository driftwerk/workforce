//! Xero report (P&L / Balance Sheet / Trial Balance) → a structured model.
//!
//! Xero returns these reports as a recursively-nested `Rows` tree of typed rows
//! (`Header` / `Section` / `Row` / `SummaryRow`) whose amount cells are JSON
//! *strings*. [`parse_report`] flattens that tree into an ordered [`ReportModel`]
//! whose rows carry the statement structure (section, depth, role) and numeric
//! values — the shape both rendering and consolidation build on. The parse is
//! total: a missing or unexpected field degrades to a blank cell, never a panic.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use workforce_agent::export::{parse_numeric, Cell, ColFormat, Column, RowStyle, Sheet, Table};

/// The role a flattened row plays in the statement. Derived from the Xero
/// `RowType` *and* whether the row carries an account id: Xero emits computed
/// statement totals (Gross/Net Profit, Net Assets) as an attribute-less `Row`,
/// not a `SummaryRow`, so `RowType` alone can't tell a line from a total.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowRole {
    SectionHeader,
    Line,
    Subtotal,
    GrandTotal,
}

/// One flattened statement row: its label, the nearest enclosing section title,
/// nesting depth, role, and period values aligned to [`ReportModel::periods`].
#[derive(Debug, Clone, PartialEq)]
pub struct ReportRow {
    pub label: String,
    pub section: String,
    pub depth: u8,
    pub role: RowRole,
    pub values: Vec<Cell>,
}

/// A parsed Xero report: its title lines, period column headers, and ordered
/// statement rows. Row order is Xero's order, which *is* the reading order of
/// the statement (Income → … → Net Profit; Assets → … → Equity).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReportModel {
    pub titles: Vec<String>,
    pub periods: Vec<String>,
    pub rows: Vec<ReportRow>,
}

/// Flatten a Xero report response into a [`ReportModel`]. Accepts either the
/// `{ "Reports": [ … ] }` envelope or a bare report object; anything else yields
/// an empty model.
pub fn parse_report(value: &Value) -> ReportModel {
    let Some(report) = locate_report(value) else {
        return ReportModel::default();
    };
    let rows_json = report.get("Rows").and_then(Value::as_array);
    let header_periods = rows_json
        .and_then(|rows| rows.iter().find(|&r| row_type(r) == "Header"))
        .map(period_labels)
        .unwrap_or_default();

    let mut rows = Vec::new();
    if let Some(rows_json) = rows_json {
        flatten(rows_json, "", 0, &mut rows);
    }

    let width = rows.iter().map(|r| r.values.len()).max().unwrap_or(0);
    ReportModel {
        titles: string_array(report.get("ReportTitles")),
        periods: pad_periods(header_periods, width),
        rows,
    }
}

/// Find the report object in either the standard envelope or a bare object.
fn locate_report(value: &Value) -> Option<&Value> {
    value
        .get("Reports")
        .and_then(Value::as_array)
        .and_then(|reports| reports.first())
        .filter(|report| report.is_object())
        .or_else(|| value.get("Rows").is_some().then_some(value))
}

/// Walk the `Rows` tree in order, emitting one [`ReportRow`] per data/summary
/// row and one `SectionHeader` per titled section. Empty-title sections are
/// transparent — their rows (Xero's grand totals) surface at the current depth.
fn flatten(rows: &[Value], section: &str, depth: u8, out: &mut Vec<ReportRow>) {
    for row in rows {
        match row_type(row) {
            "Header" => {}
            "Section" => {
                let title = str_field(row, "Title");
                let children = row.get("Rows").and_then(Value::as_array);
                if title.is_empty() {
                    if let Some(children) = children {
                        flatten(children, section, depth, out);
                    }
                } else {
                    out.push(ReportRow {
                        label: title.clone(),
                        section: title.clone(),
                        depth,
                        role: RowRole::SectionHeader,
                        values: Vec::new(),
                    });
                    if let Some(children) = children {
                        flatten(children, &title, depth.saturating_add(1), out);
                    }
                }
            }
            rtype => {
                if let Some(cells) = row.get("Cells").and_then(Value::as_array) {
                    out.push(data_row(rtype, cells, section, depth));
                } else if let Some(children) = row.get("Rows").and_then(Value::as_array) {
                    flatten(children, section, depth, out);
                }
            }
        }
    }
}

fn data_row(rtype: &str, cells: &[Value], section: &str, depth: u8) -> ReportRow {
    let role = match rtype {
        "SummaryRow" => RowRole::Subtotal,
        _ if has_account(cells) => RowRole::Line,
        _ => RowRole::GrandTotal,
    };
    ReportRow {
        label: cells.first().map(cell_value).unwrap_or_default(),
        section: section.to_string(),
        depth,
        role,
        values: cells.iter().skip(1).map(amount).collect(),
    }
}

fn row_type(row: &Value) -> &str {
    row.get("RowType").and_then(Value::as_str).unwrap_or("")
}

fn str_field(row: &Value, key: &str) -> String {
    row.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// A report cell's `Value` as trimmed text (used for row labels and titles).
fn cell_value(cell: &Value) -> String {
    cell.get("Value")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// A line carries an account id in some cell's `Attributes`; computed totals do
/// not — the only reliable line-vs-total signal Xero gives.
fn has_account(cells: &[Value]) -> bool {
    cells.iter().any(|cell| {
        cell.get("Attributes")
            .and_then(Value::as_array)
            .is_some_and(|attrs| !attrs.is_empty())
    })
}

/// A report amount cell → a typed [`Cell`]. Xero sends amounts as strings, so
/// numeric text becomes a real number (summable in Excel), blanks become empty,
/// and anything genuinely non-numeric is preserved as text.
fn amount(cell: &Value) -> Cell {
    match cell.get("Value") {
        Some(Value::String(s)) => match s.trim() {
            "" => Cell::Empty,
            t => parse_numeric(t)
                .filter(|v| v.is_finite())
                .map(Cell::Number)
                .unwrap_or_else(|| Cell::Text(s.clone())),
        },
        Some(Value::Number(n)) => n
            .as_f64()
            .filter(|v| v.is_finite())
            .map(Cell::Number)
            .unwrap_or(Cell::Empty),
        Some(other) => Cell::from_json(other),
        None => Cell::Empty,
    }
}

fn period_labels(header: &Value) -> Vec<String> {
    header
        .get("Cells")
        .and_then(Value::as_array)
        .map(|cells| cells.iter().skip(1).map(cell_value).collect())
        .unwrap_or_default()
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Period headers must cover the widest data row; synthesise names for any
/// value column a report leaves unnamed (e.g. a report with no `Header` row).
fn pad_periods(mut labels: Vec<String>, width: usize) -> Vec<String> {
    labels.extend((labels.len()..width).map(|i| format!("Period {}", i + 1)));
    labels
}

/// Output shape for [`render`]. `Statement` reads top-to-bottom like the
/// financial statement (bold section headers + subtotals, indented lines, a
/// blank line after each subtotal); `Analytical` is a flat, pivot-ready table of
/// the line items only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportLayout {
    Statement,
    Analytical,
}

/// Render a [`ReportModel`] into a named [`Sheet`] ready for `build_xlsx`.
pub fn render(model: &ReportModel, name: impl Into<String>, layout: ReportLayout) -> Sheet {
    match layout {
        ReportLayout::Statement => render_statement(model, name.into()),
        ReportLayout::Analytical => render_analytical(model, name.into()),
    }
}

fn render_statement(model: &ReportModel, name: String) -> Sheet {
    let mut columns = vec![Column::new("label", "", ColFormat::General)];
    columns.extend(period_columns(model));
    let mut rows = Vec::new();
    let mut styles = Vec::new();
    let mut after_total = false;
    for row in &model.rows {
        if after_total {
            rows.push(vec![Cell::Empty; columns.len()]);
            styles.push(RowStyle::default());
        }
        let mut cells = Vec::with_capacity(columns.len());
        cells.push(label_cell(&row.label));
        cells.extend(padded_values(&row.values, model.periods.len()));
        rows.push(cells);
        styles.push(statement_style(row));
        after_total = matches!(row.role, RowRole::Subtotal | RowRole::GrandTotal);
    }
    Sheet::styled(name, Table { columns, rows }, styles)
}

fn render_analytical(model: &ReportModel, name: String) -> Sheet {
    let mut columns = vec![
        Column::new("section", "Section", ColFormat::General),
        Column::new("line", "Line Item", ColFormat::General),
    ];
    columns.extend(period_columns(model));
    let rows = model
        .rows
        .iter()
        .filter(|row| row.role == RowRole::Line)
        .map(|row| {
            let mut cells = vec![label_cell(&row.section), label_cell(&row.label)];
            cells.extend(padded_values(&row.values, model.periods.len()));
            cells
        })
        .collect();
    Sheet::new(name, Table { columns, rows })
}

fn period_columns(model: &ReportModel) -> impl Iterator<Item = Column> + '_ {
    model
        .periods
        .iter()
        .enumerate()
        .map(|(i, period)| Column::new(format!("v{i}"), period.clone(), ColFormat::Currency))
}

fn statement_style(row: &ReportRow) -> RowStyle {
    match row.role {
        RowRole::SectionHeader => RowStyle {
            bold: true,
            indent: row.depth,
            top_border: false,
        },
        RowRole::Line => RowStyle {
            bold: false,
            indent: row.depth,
            top_border: false,
        },
        RowRole::Subtotal | RowRole::GrandTotal => RowStyle {
            bold: true,
            indent: row.depth,
            top_border: true,
        },
    }
}

fn label_cell(label: &str) -> Cell {
    if label.is_empty() {
        Cell::Empty
    } else {
        Cell::Text(label.to_string())
    }
}

fn padded_values(values: &[Cell], width: usize) -> Vec<Cell> {
    let mut out: Vec<Cell> = values.iter().take(width).cloned().collect();
    out.resize(width, Cell::Empty);
    out
}

/// Combine several entities' reports into one by summing matching rows. Rows are
/// matched on `(section, label, role)` — so lines, subtotals and grand totals
/// stay in separate buckets and never merge — and period values are aligned by
/// period label. Row order follows the first model, with rows unique to later
/// models appended. This is the first-cut "sum by matching label"; a
/// template-driven account map is the higher-fidelity path.
pub fn consolidate(models: &[ReportModel]) -> ReportModel {
    let periods = models
        .first()
        .map(|m| m.periods.clone())
        .unwrap_or_default();
    let mut order: Vec<(String, String, RowRole)> = Vec::new();
    let mut acc: HashMap<(String, String, RowRole), ReportRow> = HashMap::new();
    for model in models {
        let columns: HashMap<&str, usize> = model
            .periods
            .iter()
            .enumerate()
            .map(|(i, period)| (period.as_str(), i))
            .collect();
        for row in &model.rows {
            let key = (row.section.clone(), row.label.clone(), row.role);
            let entry = acc.entry(key.clone()).or_insert_with(|| {
                order.push(key.clone());
                ReportRow {
                    label: row.label.clone(),
                    section: row.section.clone(),
                    depth: row.depth,
                    role: row.role,
                    values: vec![Cell::Empty; periods.len()],
                }
            });
            for (j, period) in periods.iter().enumerate() {
                let Some(&src) = columns.get(period.as_str()) else {
                    continue;
                };
                if let Some(Cell::Number(n)) = row.values.get(src) {
                    let base = match entry.values[j] {
                        Cell::Number(x) => x,
                        _ => 0.0,
                    };
                    entry.values[j] = Cell::Number(round2(base + n));
                }
            }
        }
    }
    let rows = order
        .into_iter()
        .filter_map(|key| acc.remove(&key))
        .collect();
    ReportModel {
        titles: Vec::new(),
        periods,
        rows,
    }
}

/// Round to 2dp so summed money doesn't surface f64 drift (e.g. `0.1 + 0.2`).
fn round2(n: f64) -> f64 {
    (n * 100.0).round() / 100.0
}

/// How a [`TemplateLine`] behaves in the consolidated output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TemplateLineKind {
    Header,
    Line,
    Subtotal,
}

/// One row of a consolidation template's canonical statement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemplateLine {
    pub label: String,
    pub kind: TemplateLineKind,
    /// For a `Subtotal`, the canonical `Line` labels it sums.
    #[serde(default)]
    pub components: Vec<String>,
}

/// A consolidation template: the canonical statement lines (in order, with their
/// structure) plus a per-entity map from each entity's source row labels to a
/// canonical line. Install-agnostic — every workspace defines its own.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ConsolidationTemplate {
    #[serde(default)]
    pub lines: Vec<TemplateLine>,
    #[serde(default)]
    pub map: HashMap<String, HashMap<String, String>>,
}

/// Consolidate entities' reports into the template's canonical shape: each
/// entity's `Line` rows sum into the canonical line their account maps to,
/// subtotals are recomputed from their components, and any source account with
/// no mapping is surfaced under an "Unmapped" section (never silently dropped).
pub fn consolidate_mapped(
    entities: &[(String, ReportModel)],
    template: &ConsolidationTemplate,
) -> ReportModel {
    let periods = entities
        .first()
        .map(|(_, m)| m.periods.clone())
        .unwrap_or_default();
    let width = periods.len();

    let mut totals: HashMap<&str, Vec<f64>> = template
        .lines
        .iter()
        .filter(|l| l.kind == TemplateLineKind::Line)
        .map(|l| (l.label.as_str(), vec![0.0; width]))
        .collect();
    let mut unmapped_order: Vec<String> = Vec::new();
    let mut unmapped: HashMap<String, Vec<f64>> = HashMap::new();

    for (entity, model) in entities {
        let columns: HashMap<&str, usize> = model
            .periods
            .iter()
            .enumerate()
            .map(|(i, p)| (p.as_str(), i))
            .collect();
        let entity_map = template.map.get(entity);
        for row in model.rows.iter().filter(|r| r.role == RowRole::Line) {
            let canonical = entity_map
                .and_then(|m| m.get(&row.label))
                .map(String::as_str)
                .filter(|c| totals.contains_key(c));
            let target = match canonical {
                Some(canon) => totals.get_mut(canon).expect("checked by filter"),
                None => unmapped.entry(row.label.clone()).or_insert_with(|| {
                    unmapped_order.push(row.label.clone());
                    vec![0.0; width]
                }),
            };
            add_aligned(target, &row.values, &columns, &periods);
        }
    }

    // `computed` holds every emitted line/subtotal value so a subtotal can sum
    // its components from earlier *subtotals* too, not just lines (real
    // statements nest: Net Profit includes the Gross Profit subtotal).
    let mut rows: Vec<ReportRow> = Vec::with_capacity(template.lines.len() + unmapped_order.len());
    let mut computed: HashMap<&str, Vec<f64>> = HashMap::new();
    for line in &template.lines {
        let row = match line.kind {
            TemplateLineKind::Header => {
                template_row(&line.label, RowRole::SectionHeader, Vec::new())
            }
            TemplateLineKind::Line => {
                let values = totals
                    .get(line.label.as_str())
                    .cloned()
                    .unwrap_or_else(|| vec![0.0; width]);
                computed.insert(line.label.as_str(), values.clone());
                template_row(
                    &line.label,
                    RowRole::Line,
                    number_cells(Some(&values), width),
                )
            }
            TemplateLineKind::Subtotal => {
                let mut sum = vec![0.0; width];
                for component in &line.components {
                    if let Some(values) = computed.get(component.as_str()) {
                        for (s, v) in sum.iter_mut().zip(values) {
                            *s = round2(*s + v);
                        }
                    }
                }
                computed.insert(line.label.as_str(), sum.clone());
                template_row(
                    &line.label,
                    RowRole::Subtotal,
                    number_cells(Some(&sum), width),
                )
            }
        };
        rows.push(row);
    }
    if !unmapped_order.is_empty() {
        rows.push(template_row("Unmapped", RowRole::SectionHeader, Vec::new()));
        for label in &unmapped_order {
            rows.push(template_row(
                label,
                RowRole::Line,
                number_cells(unmapped.get(label), width),
            ));
        }
    }

    ReportModel {
        titles: Vec::new(),
        periods,
        rows,
    }
}

fn add_aligned(
    target: &mut [f64],
    values: &[Cell],
    columns: &HashMap<&str, usize>,
    periods: &[String],
) {
    for (j, period) in periods.iter().enumerate() {
        if let Some(&src) = columns.get(period.as_str()) {
            if let Some(Cell::Number(n)) = values.get(src) {
                target[j] = round2(target[j] + n);
            }
        }
    }
}

fn number_cells(values: Option<&Vec<f64>>, width: usize) -> Vec<Cell> {
    match values {
        Some(v) => v.iter().map(|&n| Cell::Number(round2(n))).collect(),
        None => vec![Cell::Empty; width],
    }
}

fn template_row(label: &str, role: RowRole, values: Vec<Cell>) -> ReportRow {
    ReportRow {
        label: label.to_string(),
        section: String::new(),
        depth: u8::from(role == RowRole::Line),
        role,
        values,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn consolidate_mapped_maps_accounts_sums_subtotals_and_surfaces_unmapped() {
        let uk = parse_report(&json!({ "Rows": [
            { "RowType": "Section", "Title": "Income", "Rows": [
                { "RowType": "Row", "Cells": [ {"Value":"UK Sales","Attributes":[{"Value":"a","Id":"account"}]}, {"Value":"100.00"} ] },
                { "RowType": "Row", "Cells": [ {"Value":"Consulting","Attributes":[{"Value":"b","Id":"account"}]}, {"Value":"40.00"} ] }
            ]}
        ]}));
        let us = parse_report(&json!({ "Rows": [
            { "RowType": "Section", "Title": "Income", "Rows": [
                { "RowType": "Row", "Cells": [ {"Value":"Product Revenue","Attributes":[{"Value":"c","Id":"account"}]}, {"Value":"200.00"} ] },
                { "RowType": "Row", "Cells": [ {"Value":"Misc","Attributes":[{"Value":"d","Id":"account"}]}, {"Value":"5.00"} ] }
            ]}
        ]}));
        let line = |label: &str| TemplateLine {
            label: label.into(),
            kind: TemplateLineKind::Line,
            components: vec![],
        };
        let template = ConsolidationTemplate {
            lines: vec![
                TemplateLine {
                    label: "Income".into(),
                    kind: TemplateLineKind::Header,
                    components: vec![],
                },
                line("Revenue"),
                line("Services"),
                TemplateLine {
                    label: "Total Income".into(),
                    kind: TemplateLineKind::Subtotal,
                    components: vec!["Revenue".into(), "Services".into()],
                },
            ],
            map: HashMap::from([
                (
                    "uk".to_string(),
                    HashMap::from([
                        ("UK Sales".to_string(), "Revenue".to_string()),
                        ("Consulting".to_string(), "Services".to_string()),
                    ]),
                ),
                (
                    "us".to_string(),
                    HashMap::from([("Product Revenue".to_string(), "Revenue".to_string())]),
                ),
            ]),
        };

        let c = consolidate_mapped(&[("uk".into(), uk), ("us".into(), us)], &template);
        assert_eq!(find(&c, "Revenue").values[0], Cell::Number(300.0));
        assert_eq!(find(&c, "Services").values[0], Cell::Number(40.0));
        let total = find(&c, "Total Income");
        assert_eq!(total.values[0], Cell::Number(340.0));
        assert_eq!(total.role, RowRole::Subtotal);
        assert_eq!(find(&c, "Income").role, RowRole::SectionHeader);
        assert_eq!(find(&c, "Misc").values[0], Cell::Number(5.0));
        assert_eq!(find(&c, "Unmapped").role, RowRole::SectionHeader);
    }

    #[test]
    fn consolidate_mapped_supports_nested_subtotals() {
        let model = parse_report(&json!({ "Rows": [
            { "RowType": "Row", "Cells": [ {"Value":"S","Attributes":[{"Value":"a","Id":"account"}]}, {"Value":"100.00"} ] },
            { "RowType": "Row", "Cells": [ {"Value":"C","Attributes":[{"Value":"b","Id":"account"}]}, {"Value":"-30.00"} ] },
            { "RowType": "Row", "Cells": [ {"Value":"E","Attributes":[{"Value":"d","Id":"account"}]}, {"Value":"-20.00"} ] }
        ]}));
        let line = |label: &str| TemplateLine {
            label: label.into(),
            kind: TemplateLineKind::Line,
            components: vec![],
        };
        let subtotal = |label: &str, components: Vec<&str>| TemplateLine {
            label: label.into(),
            kind: TemplateLineKind::Subtotal,
            components: components.into_iter().map(String::from).collect(),
        };
        let template = ConsolidationTemplate {
            lines: vec![
                line("Revenue"),
                line("COGS"),
                subtotal("Gross Profit", vec!["Revenue", "COGS"]),
                line("Expenses"),
                subtotal("Net Profit", vec!["Gross Profit", "Expenses"]),
            ],
            map: HashMap::from([(
                "e".to_string(),
                HashMap::from([
                    ("S".to_string(), "Revenue".to_string()),
                    ("C".to_string(), "COGS".to_string()),
                    ("E".to_string(), "Expenses".to_string()),
                ]),
            )]),
        };
        let c = consolidate_mapped(&[("e".into(), model)], &template);
        assert_eq!(find(&c, "Gross Profit").values[0], Cell::Number(70.0));
        // Net Profit sums the Gross Profit *subtotal* (70) + Expenses (-20).
        assert_eq!(find(&c, "Net Profit").values[0], Cell::Number(50.0));
    }

    #[test]
    fn consolidation_template_json_round_trips() {
        let template = ConsolidationTemplate {
            lines: vec![
                TemplateLine {
                    label: "Income".into(),
                    kind: TemplateLineKind::Header,
                    components: vec![],
                },
                TemplateLine {
                    label: "Revenue".into(),
                    kind: TemplateLineKind::Line,
                    components: vec![],
                },
                TemplateLine {
                    label: "Total Income".into(),
                    kind: TemplateLineKind::Subtotal,
                    components: vec!["Revenue".into()],
                },
            ],
            map: HashMap::from([(
                "uk".to_string(),
                HashMap::from([("Sales".to_string(), "Revenue".to_string())]),
            )]),
        };
        let json = serde_json::to_string(&template).unwrap();
        assert!(json.contains("\"kind\":\"header\""));
        let back: ConsolidationTemplate = serde_json::from_str(&json).unwrap();
        assert_eq!(template, back);
    }

    #[test]
    fn consolidate_sums_matching_rows_and_appends_unmatched() {
        let a = parse_report(&json!({ "Rows": [
            { "RowType": "Section", "Title": "Income", "Rows": [
                { "RowType": "Row", "Cells": [ {"Value":"Sales","Attributes":[{"Value":"x","Id":"account"}]}, {"Value":"100.00"} ] },
                { "RowType": "SummaryRow", "Cells": [ {"Value":"Total Income"}, {"Value":"100.00"} ] }
            ]}
        ]}));
        let b = parse_report(&json!({ "Rows": [
            { "RowType": "Section", "Title": "Income", "Rows": [
                { "RowType": "Row", "Cells": [ {"Value":"Sales","Attributes":[{"Value":"y","Id":"account"}]}, {"Value":"50.00"} ] },
                { "RowType": "Row", "Cells": [ {"Value":"Grants","Attributes":[{"Value":"z","Id":"account"}]}, {"Value":"25.00"} ] },
                { "RowType": "SummaryRow", "Cells": [ {"Value":"Total Income"}, {"Value":"75.00"} ] }
            ]}
        ]}));
        let c = consolidate(&[a, b]);
        assert_eq!(find(&c, "Sales").values[0], Cell::Number(150.0));
        assert_eq!(find(&c, "Sales").role, RowRole::Line);
        assert_eq!(find(&c, "Total Income").values[0], Cell::Number(175.0));
        assert_eq!(find(&c, "Grants").values[0], Cell::Number(25.0));
        assert_eq!(find(&c, "Income").role, RowRole::SectionHeader);
    }

    #[test]
    fn consolidate_aligns_period_columns_by_label() {
        let a = parse_report(&json!({ "Rows": [
            { "RowType": "Header", "Cells": [ {"Value":""}, {"Value":"Jan"}, {"Value":"Feb"} ] },
            { "RowType": "Row", "Cells": [ {"Value":"X","Attributes":[{"Value":"a","Id":"account"}]}, {"Value":"1.00"}, {"Value":"2.00"} ] }
        ]}));
        let b = parse_report(&json!({ "Rows": [
            { "RowType": "Header", "Cells": [ {"Value":""}, {"Value":"Feb"}, {"Value":"Jan"} ] },
            { "RowType": "Row", "Cells": [ {"Value":"X","Attributes":[{"Value":"a","Id":"account"}]}, {"Value":"20.00"}, {"Value":"10.00"} ] }
        ]}));
        let c = consolidate(&[a, b]);
        assert_eq!(c.periods, vec!["Jan", "Feb"]);
        let x = find(&c, "X");
        assert_eq!(x.values[0], Cell::Number(11.0));
        assert_eq!(x.values[1], Cell::Number(22.0));
    }

    #[test]
    fn consolidate_rounds_to_two_decimals() {
        let a = parse_report(&json!({ "Rows": [
            { "RowType": "Row", "Cells": [ {"Value":"X","Attributes":[{"Value":"a","Id":"account"}]}, {"Value":"0.1"} ] }
        ]}));
        let b = parse_report(&json!({ "Rows": [
            { "RowType": "Row", "Cells": [ {"Value":"X","Attributes":[{"Value":"a","Id":"account"}]}, {"Value":"0.2"} ] }
        ]}));
        assert_eq!(
            find(&consolidate(&[a, b]), "X").values[0],
            Cell::Number(0.3)
        );
    }

    fn find<'a>(model: &'a ReportModel, label: &str) -> &'a ReportRow {
        model
            .rows
            .iter()
            .find(|r| r.label == label)
            .unwrap_or_else(|| panic!("row {label:?} not found"))
    }

    fn pnl() -> Value {
        json!({ "Reports": [{
            "ReportType": "ProfitAndLoss",
            "ReportTitles": ["Profit and Loss", "HARBOR T/A Harbor", "1 April 2026 to 31 May 2026"],
            "Rows": [
                { "RowType": "Header", "Cells": [ {"Value":""}, {"Value":"Apr 2026"}, {"Value":"May 2026"}, {"Value":"YTD"} ] },
                { "RowType": "Section", "Title": "Income", "Rows": [
                    { "RowType": "Row", "Cells": [
                        {"Value":"Agency Income","Attributes":[{"Value":"acc-1","Id":"account"}]},
                        {"Value":"35000.00"}, {"Value":"0.00"}, {"Value":"35000.00"} ] },
                    { "RowType": "SummaryRow", "Cells": [ {"Value":"Total Income"}, {"Value":"35000.00"}, {"Value":"0.00"}, {"Value":"35000.00"} ] }
                ] },
                { "RowType": "Section", "Title": "Less Operating Expenses", "Rows": [
                    { "RowType": "Row", "Cells": [
                        {"Value":"Bank Fees","Attributes":[{"Value":"acc-9","Id":"account"}]},
                        {"Value":"-1234.56"}, {"Value":"2,000.00"}, {"Value":""} ] },
                    { "RowType": "SummaryRow", "Cells": [ {"Value":"Total Operating Expenses"}, {"Value":"-1234.56"}, {"Value":"2000.00"}, {"Value":"765.44"} ] }
                ] },
                { "RowType": "Section", "Title": "", "Rows": [
                    { "RowType": "Row", "Cells": [ {"Value":"Net Profit"}, {"Value":"33765.44"}, {"Value":"-2000.00"}, {"Value":"34234.56"} ] }
                ] }
            ]
        }]})
    }

    fn balance_sheet() -> Value {
        json!({ "Reports": [{
            "ReportType": "BalanceSheet",
            "ReportTitles": ["Balance Sheet", "BEACON Enterprise Ltd", "As at 31 May 2026"],
            "Rows": [
                { "RowType": "Header", "Cells": [ {"Value":""}, {"Value":"31 May 2026"}, {"Value":"31 May 2025"} ] },
                { "RowType": "Section", "Title": "Assets", "Rows": [] },
                { "RowType": "Section", "Title": "Bank", "Rows": [
                    { "RowType": "Row", "Cells": [
                        {"Value":"Business Account","Attributes":[{"Value":"b1","Id":"account"}]},
                        {"Value":"5000.00"}, {"Value":"4000.00"} ] },
                    { "RowType": "SummaryRow", "Cells": [ {"Value":"Total Bank"}, {"Value":"5000.00"}, {"Value":"4000.00"} ] }
                ] },
                { "RowType": "Section", "Title": "", "Rows": [
                    { "RowType": "SummaryRow", "Cells": [ {"Value":"Total Assets"}, {"Value":"5000.00"}, {"Value":"4000.00"} ] }
                ] },
                { "RowType": "Section", "Title": "", "Rows": [
                    { "RowType": "Row", "Cells": [ {"Value":"Net Assets"}, {"Value":"5000.00"}, {"Value":"4000.00"} ] }
                ] }
            ]
        }]})
    }

    #[test]
    fn pnl_periods_and_titles() {
        let m = parse_report(&pnl());
        assert_eq!(m.periods, vec!["Apr 2026", "May 2026", "YTD"]);
        assert_eq!(m.titles[1], "HARBOR T/A Harbor");
    }

    #[test]
    fn section_header_has_no_values_and_depth_zero() {
        let m = parse_report(&pnl());
        let income = find(&m, "Income");
        assert_eq!(income.role, RowRole::SectionHeader);
        assert_eq!(income.depth, 0);
        assert!(income.values.is_empty());
    }

    #[test]
    fn account_line_is_nested_and_numeric() {
        let m = parse_report(&pnl());
        let line = find(&m, "Agency Income");
        assert_eq!(line.role, RowRole::Line);
        assert_eq!(line.section, "Income");
        assert_eq!(line.depth, 1);
        assert_eq!(line.values[0], Cell::Number(35000.0));
        assert_eq!(line.values[1], Cell::Number(0.0));
    }

    #[test]
    fn summary_row_is_subtotal_within_its_section() {
        let m = parse_report(&pnl());
        let total = find(&m, "Total Income");
        assert_eq!(total.role, RowRole::Subtotal);
        assert_eq!(total.section, "Income");
        assert_eq!(total.depth, 1);
    }

    #[test]
    fn negatives_thousands_and_blanks_parse() {
        let m = parse_report(&pnl());
        let fees = find(&m, "Bank Fees");
        assert_eq!(fees.values[0], Cell::Number(-1234.56));
        assert_eq!(fees.values[1], Cell::Number(2000.0));
        assert_eq!(fees.values[2], Cell::Empty);
    }

    #[test]
    fn bare_row_is_grand_total_in_empty_section() {
        let m = parse_report(&pnl());
        let net = find(&m, "Net Profit");
        assert_eq!(net.role, RowRole::GrandTotal);
        assert_eq!(net.section, "");
        assert_eq!(net.depth, 0);
        assert_eq!(net.values[1], Cell::Number(-2000.0));
    }

    #[test]
    fn balance_sheet_periods_and_pure_header_section() {
        let m = parse_report(&balance_sheet());
        assert_eq!(m.periods, vec!["31 May 2026", "31 May 2025"]);
        let assets = find(&m, "Assets");
        assert_eq!(assets.role, RowRole::SectionHeader);
        assert!(assets.values.is_empty());
    }

    #[test]
    fn balance_sheet_subsection_and_grand_totals() {
        let m = parse_report(&balance_sheet());
        assert_eq!(find(&m, "Business Account").section, "Bank");
        assert_eq!(find(&m, "Business Account").depth, 1);
        assert_eq!(find(&m, "Total Bank").role, RowRole::Subtotal);
        let total_assets = find(&m, "Total Assets");
        assert_eq!(total_assets.role, RowRole::Subtotal);
        assert_eq!(total_assets.depth, 0);
        assert_eq!(find(&m, "Net Assets").role, RowRole::GrandTotal);
    }

    #[test]
    fn missing_report_yields_empty_model() {
        assert_eq!(parse_report(&json!({})), ReportModel::default());
        assert_eq!(
            parse_report(&json!({ "Reports": [] })),
            ReportModel::default()
        );
    }

    #[test]
    fn header_absent_synthesises_period_names() {
        let bare = json!({ "Rows": [
            { "RowType": "Row", "Cells": [
                {"Value":"X","Attributes":[{"Value":"a","Id":"account"}]},
                {"Value":"1"}, {"Value":"2"} ] }
        ]});
        let m = parse_report(&bare);
        assert_eq!(m.periods, vec!["Period 1", "Period 2"]);
        assert_eq!(find(&m, "X").role, RowRole::Line);
        assert_eq!(
            find(&m, "X").values,
            vec![Cell::Number(1.0), Cell::Number(2.0)]
        );
    }

    #[test]
    fn unknown_row_type_with_account_is_a_line() {
        let report = json!({ "Rows": [
            { "RowType": "Mystery", "Cells": [
                {"Value":"Surprise","Attributes":[{"Value":"x","Id":"account"}]},
                {"Value":"7.00"} ] }
        ]});
        let m = parse_report(&report);
        assert_eq!(find(&m, "Surprise").role, RowRole::Line);
        assert_eq!(find(&m, "Surprise").values[0], Cell::Number(7.0));
    }

    #[test]
    fn non_numeric_amount_preserved_as_text() {
        let report = json!({ "Rows": [
            { "RowType": "Row", "Cells": [ {"Value":"Note"}, {"Value":"n/a"} ] }
        ]});
        let m = parse_report(&report);
        assert_eq!(find(&m, "Note").values[0], Cell::Text("n/a".to_string()));
    }

    #[test]
    fn nested_subsections_increment_depth() {
        let report = json!({ "Rows": [
            { "RowType": "Section", "Title": "Assets", "Rows": [
                { "RowType": "Section", "Title": "Bank", "Rows": [
                    { "RowType": "Row", "Cells": [
                        {"Value":"Account","Attributes":[{"Value":"a","Id":"account"}]},
                        {"Value":"10.00"} ] },
                    { "RowType": "SummaryRow", "Cells": [ {"Value":"Total Bank"}, {"Value":"10.00"} ] }
                ] }
            ] }
        ]});
        let m = parse_report(&report);
        assert_eq!(find(&m, "Assets").depth, 0);
        assert_eq!(find(&m, "Bank").depth, 1);
        let account = find(&m, "Account");
        assert_eq!(account.depth, 2);
        assert_eq!(account.section, "Bank");
        assert_eq!(find(&m, "Total Bank").depth, 2);
    }

    #[test]
    fn summary_row_with_attributes_is_still_subtotal() {
        let report = json!({ "Rows": [
            { "RowType": "SummaryRow", "Cells": [
                {"Value":"Total","Attributes":[{"Value":"x","Id":"account"}]},
                {"Value":"5.00"} ] }
        ]});
        assert_eq!(
            find(&parse_report(&report), "Total").role,
            RowRole::Subtotal
        );
    }

    #[test]
    fn account_attribute_on_value_cell_marks_a_line() {
        let report = json!({ "Rows": [
            { "RowType": "Row", "Cells": [
                {"Value":"Sales"},
                {"Value":"9.00","Attributes":[{"Value":"a","Id":"account"}]} ] }
        ]});
        assert_eq!(find(&parse_report(&report), "Sales").role, RowRole::Line);
    }

    #[test]
    fn non_finite_amount_is_not_a_number() {
        let report = json!({ "Rows": [
            { "RowType": "Row", "Cells": [ {"Value":"Bad"}, {"Value":"Infinity"} ] }
        ]});
        assert_eq!(
            find(&parse_report(&report), "Bad").values[0],
            Cell::Text("Infinity".to_string())
        );
    }

    #[test]
    fn statement_render_headers_and_styles() {
        let sheet = render(&parse_report(&pnl()), "Harbor P&L", ReportLayout::Statement);
        assert_eq!(sheet.name, "Harbor P&L");
        let headers: Vec<&str> = sheet
            .table
            .columns
            .iter()
            .map(|c| c.header.as_str())
            .collect();
        assert_eq!(headers, vec!["", "Apr 2026", "May 2026", "YTD"]);
        assert_eq!(sheet.table.rows.len(), sheet.styles.len());

        let style_of = |label: &str| {
            let i = sheet
                .table
                .rows
                .iter()
                .position(|r| r[0] == Cell::Text(label.into()))
                .unwrap();
            sheet.styles[i]
        };
        assert!(style_of("Income").bold && !style_of("Income").top_border);
        assert!(!style_of("Agency Income").bold);
        assert_eq!(style_of("Agency Income").indent, 1);
        assert!(style_of("Total Income").bold && style_of("Total Income").top_border);
    }

    #[test]
    fn statement_inserts_blank_after_each_subtotal() {
        let sheet = render(&parse_report(&pnl()), "x", ReportLayout::Statement);
        assert!(sheet
            .table
            .rows
            .iter()
            .any(|r| r.iter().all(|c| *c == Cell::Empty)));
    }

    #[test]
    fn analytical_render_is_lines_only() {
        let sheet = render(&parse_report(&pnl()), "x", ReportLayout::Analytical);
        assert!(sheet.styles.is_empty());
        let headers: Vec<&str> = sheet
            .table
            .columns
            .iter()
            .map(|c| c.header.as_str())
            .collect();
        assert_eq!(&headers[..2], &["Section", "Line Item"]);
        let agency = sheet
            .table
            .rows
            .iter()
            .find(|r| r[1] == Cell::Text("Agency Income".into()))
            .unwrap();
        assert_eq!(agency[0], Cell::Text("Income".into()));
        assert!(!sheet.table.rows.iter().any(|r| {
            r[1] == Cell::Text("Total Income".into()) || r[1] == Cell::Text("Net Profit".into())
        }));
    }
}
