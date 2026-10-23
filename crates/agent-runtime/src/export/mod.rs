//! Generic tabular export — skill-agnostic.
//!
//! Three reusable pieces, none of which know about any specific adapter:
//!   * [`Table`] — an in-memory grid of typed [`Cell`]s with named [`Column`]s.
//!   * [`Layout`] + [`apply_layout`] — a declarative reshape (column
//!     select/rename/reorder, group-by aggregation, sort, totals row). This is
//!     the "custom shape" engine: a tool fills a [`Layout`] from the user's
//!     words or from an example workbook's headers, and the same code applies
//!     it regardless of where the rows came from.
//!   * [`build_xlsx`] — write one or more [`Sheet`]s to `.xlsx` bytes.
//!
//! A producer (e.g. the Xero bulk-export tool) flattens its API rows into a
//! [`Table`], optionally reshapes it with a [`Layout`], and hands the result to
//! [`build_xlsx`]; the bytes are then staged as an artifact and delivered to the
//! channel by the runner's existing attachment path. See

use std::cmp::Ordering;

use eyre::Result;
use serde_json::Value;

/// The xlsx MIME type, for tagging the delivered file so Slack infers `.xlsx`.
pub const XLSX_MIME: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

/// Excel's hard per-sheet data-row limit (1,048,576 rows minus the header).
/// [`build_xlsx`] never writes more than this per sheet, so no producer can
/// emit a workbook Excel refuses to open.
pub const EXCEL_MAX_ROWS: usize = 1_048_575;

/// A single typed spreadsheet cell. Numbers stay numeric so the recipient can
/// sum / pivot in Excel; everything else is text.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Empty,
    Text(String),
    Number(f64),
    Bool(bool),
}

impl Cell {
    /// Convert an arbitrary JSON value to a cell: numbers stay numeric, bools
    /// stay boolean, null/empty-string become [`Cell::Empty`], everything else
    /// is text (arrays/objects compact-serialised).
    pub fn from_json(value: &Value) -> Cell {
        match value {
            Value::Null => Cell::Empty,
            Value::Bool(b) => Cell::Bool(*b),
            Value::Number(n) => n.as_f64().map(Cell::Number).unwrap_or(Cell::Empty),
            Value::String(s) if s.is_empty() => Cell::Empty,
            Value::String(s) => Cell::Text(s.clone()),
            other => Cell::Text(other.to_string()),
        }
    }

    /// Inverse of [`Cell::from_json`], for producers that shape a [`Table`]
    /// and then stage canonical row objects instead of writing a workbook.
    pub fn to_json(&self) -> Value {
        match self {
            Cell::Empty => Value::Null,
            Cell::Text(s) => Value::String(s.clone()),
            Cell::Number(n) => serde_json::Number::from_f64(*n)
                .map(Value::Number)
                .unwrap_or(Value::Null),
            Cell::Bool(b) => Value::Bool(*b),
        }
    }

    /// The numeric value, for aggregation/totals. `None` for non-numeric cells.
    fn as_number(&self) -> Option<f64> {
        match self {
            Cell::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// A stable string used as a group-by key and for non-numeric sorting.
    fn group_key(&self) -> String {
        match self {
            Cell::Empty => String::new(),
            Cell::Text(s) => s.clone(),
            Cell::Number(n) => n.to_string(),
            Cell::Bool(b) => b.to_string(),
        }
    }
}

/// Parse a number that may be "dressed up" — currency symbols, `%`, thousands
/// separators, and a K/M/B magnitude suffix (`£3.1M`, `$138B`, `12.4k`, `94%`,
/// `2,280`). Returns `None` when there's no number to read. Shared by the report
/// chart extractor and the Xero report flattener so both read amounts alike.
pub fn parse_numeric(s: &str) -> Option<f64> {
    let cleaned: String = s
        .chars()
        .filter(|c| !matches!(c, ',' | '$' | '£' | '€' | '¥' | '%') && !c.is_whitespace())
        .collect();
    let (body, mult) = match cleaned.chars().last() {
        Some('k' | 'K') => (&cleaned[..cleaned.len() - 1], 1e3),
        Some('m' | 'M') => (&cleaned[..cleaned.len() - 1], 1e6),
        Some('b' | 'B') => (&cleaned[..cleaned.len() - 1], 1e9),
        _ => (cleaned.as_str(), 1.0),
    };
    body.parse::<f64>().ok().map(|n| n * mult)
}

/// How a column's cells render in Excel. `Currency` applies a 2dp numeric
/// format; `General` writes numbers as-is and text as text; `Date` marks a
/// column whose values are pre-formatted date strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColFormat {
    General,
    Currency,
    Date,
}

/// A named output column. `key` is the stable handle a [`Layout`] addresses the
/// column by; `header` is what's written to the header row.
#[derive(Debug, Clone)]
pub struct Column {
    pub key: String,
    pub header: String,
    pub format: ColFormat,
}

impl Column {
    pub fn new(key: impl Into<String>, header: impl Into<String>, format: ColFormat) -> Self {
        Self {
            key: key.into(),
            header: header.into(),
            format,
        }
    }
}

/// An in-memory table: columns plus rows-of-cells aligned to the columns by
/// index. Rows may be shorter than `columns` (missing trailing cells are
/// treated as [`Cell::Empty`]).
#[derive(Debug, Clone)]
pub struct Table {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Cell>>,
}

impl Table {
    fn col_index(&self, key: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.key == key)
    }

    fn cell(row: &[Cell], idx: usize) -> &Cell {
        row.get(idx).unwrap_or(&Cell::Empty)
    }
}

/// Sanitise an `.xlsx` filename: strip path components and control chars, and
/// ensure the `.xlsx` extension. Falls back to `{default_stem}.xlsx` when none
/// is supplied.
pub fn xlsx_filename(requested: Option<&str>, default_stem: &str) -> String {
    let raw = requested
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{default_stem}.xlsx"));
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(&raw);
    let cleaned: String = base
        .chars()
        .map(|c| if c.is_control() { '_' } else { c })
        .collect();
    if cleaned.to_ascii_lowercase().ends_with(".xlsx") {
        cleaned
    } else {
        format!("{cleaned}.xlsx")
    }
}

/// Build a [`Table`] from a slice of JSON row objects. Columns are the union of
/// the rows' object keys (key == header, [`ColFormat::General`]); serde_json
/// orders object keys alphabetically, so pass a [`Layout`] with explicit
/// `columns` to control order / subset / rename. Non-object rows and missing
/// keys yield empty cells rather than erroring.
pub fn table_from_objects(rows: &[Value]) -> Table {
    let mut columns: Vec<Column> = Vec::new();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for obj in rows.iter().filter_map(Value::as_object) {
        for key in obj.keys() {
            if seen.insert(key.as_str()) {
                columns.push(Column::new(key.clone(), key.clone(), ColFormat::General));
            }
        }
    }
    let cells = rows
        .iter()
        .map(|row| {
            columns
                .iter()
                .map(|c| Cell::from_json(row.get(c.key.as_str()).unwrap_or(&Value::Null)))
                .collect()
        })
        .collect();
    Table {
        columns,
        rows: cells,
    }
}

/// Reorder a table's columns to `order` (keys present in the table, in the
/// given order) followed by any remaining columns in their existing order;
/// row cells are permuted to match. Used to honor an artifact's schema
/// order, since `table_from_objects` sees alphabetical object keys.
pub fn reorder_columns(table: Table, order: &[String]) -> Table {
    let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut perm: Vec<usize> = order
        .iter()
        .filter_map(|key| table.col_index(key))
        .filter(|i| seen.insert(*i))
        .collect();
    perm.extend((0..table.columns.len()).filter(|i| !seen.contains(i)));
    if perm
        .iter()
        .enumerate()
        .all(|(position, idx)| position == *idx)
    {
        return table;
    }
    let columns = perm.iter().map(|&i| table.columns[i].clone()).collect();
    let rows = table
        .rows
        .iter()
        .map(|row| perm.iter().map(|&i| Table::cell(row, i).clone()).collect())
        .collect();
    Table { columns, rows }
}

/// Per-row visual styling, parallel to a [`Sheet`]'s table rows. The default
/// (no bold, no indent, no border) renders exactly like an unstyled row, so a
/// producer that sets no styles is unaffected.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RowStyle {
    pub bold: bool,
    pub indent: u8,
    pub top_border: bool,
}

/// A named worksheet within a workbook. `styles`, when non-empty, is aligned to
/// `table.rows` and drives per-row formatting (bold, indent, top border) plus
/// auto-fit column widths; an empty `styles` renders exactly as before.
pub struct Sheet {
    pub name: String,
    pub table: Table,
    pub styles: Vec<RowStyle>,
}

impl Sheet {
    /// An unstyled sheet — renders exactly as the engine always has.
    pub fn new(name: impl Into<String>, table: Table) -> Self {
        Self {
            name: name.into(),
            table,
            styles: Vec::new(),
        }
    }

    /// A sheet whose `styles` are aligned to `table.rows`.
    pub fn styled(name: impl Into<String>, table: Table, styles: Vec<RowStyle>) -> Self {
        Self {
            name: name.into(),
            table,
            styles,
        }
    }
}

// ── Layout: the declarative "custom shape" spec (Tier 1 / Tier 2) ──

/// A declarative reshape of a [`Table`]. Every field is optional; a default
/// `Layout` is the identity transform. Parse one from tool input with
/// [`Layout::from_json`].
#[derive(Debug, Clone, Default)]
pub struct Layout {
    /// Output columns (subset / reorder / rename), addressed by source `key`.
    /// Empty → keep all source columns. Ignored when `group_by` is set.
    pub columns: Vec<LayoutColumn>,
    /// Group rows by these column keys. When set, the output is
    /// `[group columns…] ++ [aggregate columns…]` and `columns` is ignored.
    pub group_by: Vec<String>,
    /// Aggregates over each group. Empty (with `group_by` set) → a single
    /// `Count` column.
    pub aggregate: Vec<Aggregate>,
    /// Sort keys, applied in order (primary first).
    pub sort: Vec<SortKey>,
    /// Append a totals row that sums every numeric column.
    pub totals_row: bool,
}

#[derive(Debug, Clone)]
pub struct LayoutColumn {
    pub field: String,
    pub header: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Aggregate {
    pub field: String,
    pub func: AggFunc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFunc {
    Sum,
    Count,
    Avg,
    Min,
    Max,
}

impl AggFunc {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "sum" => Some(Self::Sum),
            "count" => Some(Self::Count),
            "avg" | "average" | "mean" => Some(Self::Avg),
            "min" => Some(Self::Min),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Sum => "Sum",
            Self::Count => "Count",
            Self::Avg => "Avg",
            Self::Min => "Min",
            Self::Max => "Max",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SortKey {
    pub field: String,
    pub descending: bool,
}

impl Layout {
    /// True when the layout is the identity transform (parse produced nothing).
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
            && self.group_by.is_empty()
            && self.aggregate.is_empty()
            && self.sort.is_empty()
            && !self.totals_row
    }

    /// Parse a lenient layout from JSON. Unknown keys are ignored and malformed
    /// entries are dropped, so a partially-valid spec still does something
    /// sensible rather than failing the whole export.
    pub fn from_json(v: &Value) -> Self {
        if !v.is_object() {
            return Self::default();
        }
        let columns = v["columns"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|c| {
                        let field = c["field"].as_str()?.to_string();
                        let header = c["header"].as_str().map(str::to_string);
                        Some(LayoutColumn { field, header })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let group_by = v["group_by"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let aggregate = v["aggregate"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| {
                        let field = x["field"].as_str()?.to_string();
                        let func = AggFunc::parse(x["fn"].as_str().or(x["func"].as_str())?)?;
                        Some(Aggregate { field, func })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let sort = v["sort"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| {
                        let field = x["field"].as_str()?.to_string();
                        let descending = matches!(
                            x["dir"].as_str().or(x["direction"].as_str()),
                            Some("desc") | Some("descending")
                        );
                        Some(SortKey { field, descending })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let totals_row = v["totals_row"].as_bool().unwrap_or(false);
        Self {
            columns,
            group_by,
            aggregate,
            sort,
            totals_row,
        }
    }
}

/// Apply a [`Layout`] to a [`Table`], producing a reshaped table. Field keys in
/// the layout that don't match any source column are skipped; if a projection
/// or grouping matches nothing, the source table is kept unchanged (lenient, so
/// a bad spec degrades to "default shape" rather than an empty sheet).
pub fn apply_layout(table: &Table, layout: &Layout) -> Table {
    let mut out = if !layout.group_by.is_empty() {
        aggregate_table(table, layout)
    } else if !layout.columns.is_empty() {
        project_table(table, &layout.columns)
    } else {
        table.clone()
    };
    apply_sort(&mut out, &layout.sort);
    if layout.totals_row {
        append_totals_row(&mut out);
    }
    out
}

fn project_table(table: &Table, cols: &[LayoutColumn]) -> Table {
    let mut columns = Vec::new();
    let mut idxs = Vec::new();
    for lc in cols {
        if let Some(i) = table.col_index(&lc.field) {
            let src = &table.columns[i];
            columns.push(Column {
                key: src.key.clone(),
                header: lc.header.clone().unwrap_or_else(|| src.header.clone()),
                format: src.format,
            });
            idxs.push(i);
        }
    }
    if columns.is_empty() {
        return table.clone();
    }
    let rows = table
        .rows
        .iter()
        .map(|r| idxs.iter().map(|&i| Table::cell(r, i).clone()).collect())
        .collect();
    Table { columns, rows }
}

fn aggregate_table(table: &Table, layout: &Layout) -> Table {
    let group_idxs: Vec<usize> = layout
        .group_by
        .iter()
        .filter_map(|k| table.col_index(k))
        .collect();
    if group_idxs.is_empty() {
        return table.clone();
    }

    // Resolve aggregates to (source column index, func, output column). With no
    // aggregates requested, emit a single per-group row count.
    struct AggCol {
        idx: Option<usize>,
        func: AggFunc,
        column: Column,
    }
    let agg_cols: Vec<AggCol> = if layout.aggregate.is_empty() {
        vec![AggCol {
            idx: None,
            func: AggFunc::Count,
            column: Column::new("__count", "Count", ColFormat::General),
        }]
    } else {
        layout
            .aggregate
            .iter()
            .map(|a| {
                let idx = table.col_index(&a.field);
                let src_header = idx
                    .map(|i| table.columns[i].header.clone())
                    .unwrap_or_else(|| a.field.clone());
                // Count is a plain tally; other aggregates inherit the source
                // column's format so summed money stays money-formatted.
                let format = match a.func {
                    AggFunc::Count => ColFormat::General,
                    _ => idx
                        .map(|i| table.columns[i].format)
                        .unwrap_or(ColFormat::General),
                };
                AggCol {
                    idx,
                    func: a.func,
                    column: Column::new(
                        a.field.clone(),
                        format!("{} of {}", a.func.label(), src_header),
                        format,
                    ),
                }
            })
            .collect()
    };

    // Accumulate per group, preserving first-seen order.
    #[derive(Default)]
    struct Acc {
        sum: f64,
        count: u64,
        numeric_count: u64,
        min: Option<f64>,
        max: Option<f64>,
    }
    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, (Vec<Cell>, Vec<Acc>)> =
        std::collections::HashMap::new();

    for row in &table.rows {
        let key_parts: Vec<String> = group_idxs
            .iter()
            .map(|&i| Table::cell(row, i).group_key())
            .collect();
        let key = key_parts.join("\u{1f}");
        let entry = groups.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            let group_cells = group_idxs
                .iter()
                .map(|&i| Table::cell(row, i).clone())
                .collect();
            (
                group_cells,
                agg_cols.iter().map(|_| Acc::default()).collect(),
            )
        });
        for (slot, ac) in entry.1.iter_mut().zip(agg_cols.iter()) {
            slot.count += 1;
            if let Some(n) = ac.idx.and_then(|i| Table::cell(row, i).as_number()) {
                slot.sum += n;
                slot.numeric_count += 1;
                slot.min = Some(slot.min.map_or(n, |m| m.min(n)));
                slot.max = Some(slot.max.map_or(n, |m| m.max(n)));
            }
        }
    }

    let mut columns: Vec<Column> = group_idxs
        .iter()
        .map(|&i| table.columns[i].clone())
        .collect();
    columns.extend(agg_cols.iter().map(|a| a.column.clone()));

    let rows = order
        .iter()
        .filter_map(|key| {
            let (group_cells, accs) = groups.get(key)?;
            let mut row = group_cells.clone();
            for (acc, ac) in accs.iter().zip(agg_cols.iter()) {
                let value = match ac.func {
                    AggFunc::Sum => acc.sum,
                    AggFunc::Count => acc.count as f64,
                    AggFunc::Avg => {
                        if acc.numeric_count == 0 {
                            0.0
                        } else {
                            acc.sum / acc.numeric_count as f64
                        }
                    }
                    AggFunc::Min => acc.min.unwrap_or(0.0),
                    AggFunc::Max => acc.max.unwrap_or(0.0),
                };
                row.push(Cell::Number(value));
            }
            Some(row)
        })
        .collect();

    Table { columns, rows }
}

fn apply_sort(table: &mut Table, sort: &[SortKey]) {
    if sort.is_empty() {
        return;
    }
    let keys: Vec<(usize, bool)> = sort
        .iter()
        .filter_map(|s| table.col_index(&s.field).map(|i| (i, s.descending)))
        .collect();
    if keys.is_empty() {
        return;
    }
    table.rows.sort_by(|a, b| {
        for (i, desc) in &keys {
            let ord = compare_cells(Table::cell(a, *i), Table::cell(b, *i));
            let ord = if *desc { ord.reverse() } else { ord };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    });
}

fn compare_cells(a: &Cell, b: &Cell) -> Ordering {
    match (a.as_number(), b.as_number()) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        _ => a.group_key().cmp(&b.group_key()),
    }
}

fn append_totals_row(table: &mut Table) {
    let mut totals = vec![Cell::Empty; table.columns.len()];
    let mut labelled = false;
    for (c, slot) in totals.iter_mut().enumerate() {
        let sum: f64 = table
            .rows
            .iter()
            .filter_map(|r| Table::cell(r, c).as_number())
            .sum();
        let has_numeric = table
            .rows
            .iter()
            .any(|r| Table::cell(r, c).as_number().is_some());
        if has_numeric {
            *slot = Cell::Number(sum);
        } else if !labelled {
            *slot = Cell::Text("Total".to_string());
            labelled = true;
        }
    }
    table.rows.push(totals);
}

// ── xlsx writer ──

/// Build an `.xlsx` workbook from one or more sheets, returning the file bytes.
/// Sheet names are sanitised (Excel forbids `[]:*?/\` and caps names at 31
/// chars) and de-duplicated. A sheet with no `styles` renders exactly as it
/// always has; a styled sheet gets per-row formatting and auto-fit columns.
pub fn build_xlsx(sheets: &[Sheet]) -> Result<Vec<u8>> {
    use rust_xlsxwriter::{Format, Workbook};

    let mut workbook = Workbook::new();
    let header_fmt = Format::new().set_bold();
    let money_fmt = Format::new().set_num_format("#,##0.00");

    let mut used_names: Vec<String> = Vec::new();
    for sheet in sheets {
        let ws = workbook.add_worksheet();
        ws.set_name(sanitize_sheet_name(&sheet.name, &mut used_names))?;
        for (col, c) in sheet.table.columns.iter().enumerate() {
            ws.write_with_format(0, col as u16, c.header.as_str(), &header_fmt)?;
        }
        if sheet.styles.is_empty() {
            write_rows(ws, &sheet.table, &money_fmt)?;
        } else {
            write_styled_rows(ws, sheet)?;
            autofit_columns(ws, sheet)?;
        }
    }
    Ok(workbook.save_to_buffer()?)
}

/// Unstyled rows: numbers stay numeric, currency columns get a 2dp format, the
/// rest is written as-is. Capped at Excel's row limit so the workbook opens.
fn write_rows(
    ws: &mut rust_xlsxwriter::Worksheet,
    table: &Table,
    money_fmt: &rust_xlsxwriter::Format,
) -> Result<()> {
    for (r, row) in table.rows.iter().take(EXCEL_MAX_ROWS).enumerate() {
        let row_idx = (r as u32) + 1;
        for (col, cell) in row.iter().enumerate() {
            let format = table
                .columns
                .get(col)
                .map(|c| c.format)
                .unwrap_or(ColFormat::General);
            write_cell(ws, row_idx, col as u16, cell, format, money_fmt)?;
        }
    }
    Ok(())
}

fn write_cell(
    ws: &mut rust_xlsxwriter::Worksheet,
    row: u32,
    col: u16,
    cell: &Cell,
    format: ColFormat,
    money_fmt: &rust_xlsxwriter::Format,
) -> Result<()> {
    match cell {
        Cell::Empty => {}
        Cell::Text(s) => {
            // Always written as a string type — never a formula — so a value
            // like "=cmd()" lands as literal text, not an executed formula.
            ws.write(row, col, s.as_str())?;
        }
        Cell::Bool(b) => {
            ws.write(row, col, *b)?;
        }
        Cell::Number(n) => match format {
            ColFormat::Currency => {
                ws.write_with_format(row, col, *n, money_fmt)?;
            }
            _ => {
                ws.write(row, col, *n)?;
            }
        },
    }
    Ok(())
}

/// Styled rows: each row's [`RowStyle`] composes a cached `Format` (bold,
/// indent on the label column, top border, plus the column's currency format).
/// A bordered row writes its blank cells too, so the rule spans the full width.
fn write_styled_rows(ws: &mut rust_xlsxwriter::Worksheet, sheet: &Sheet) -> Result<()> {
    let mut cache: std::collections::HashMap<FormatKey, rust_xlsxwriter::Format> =
        std::collections::HashMap::new();
    for (r, row) in sheet.table.rows.iter().take(EXCEL_MAX_ROWS).enumerate() {
        let row_idx = (r as u32) + 1;
        let style = sheet.styles.get(r).copied().unwrap_or_default();
        for (col, cell) in row.iter().enumerate() {
            let currency = matches!(
                sheet.table.columns.get(col).map(|c| c.format),
                Some(ColFormat::Currency)
            );
            let key = FormatKey {
                bold: style.bold,
                indent: if col == 0 { style.indent } else { 0 },
                top_border: style.top_border,
                currency,
            };
            let fmt = cache.entry(key).or_insert_with(|| key.build());
            match cell {
                Cell::Empty => {
                    if style.top_border {
                        ws.write_blank(row_idx, col as u16, fmt)?;
                    }
                }
                Cell::Text(s) => {
                    ws.write_with_format(row_idx, col as u16, s.as_str(), fmt)?;
                }
                Cell::Bool(b) => {
                    ws.write_with_format(row_idx, col as u16, *b, fmt)?;
                }
                Cell::Number(n) => {
                    ws.write_with_format(row_idx, col as u16, *n, fmt)?;
                }
            }
        }
    }
    Ok(())
}

/// The distinct format combinations a styled sheet needs, so the workbook builds
/// one `Format` per combination instead of one per cell.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct FormatKey {
    bold: bool,
    indent: u8,
    top_border: bool,
    currency: bool,
}

impl FormatKey {
    fn build(self) -> rust_xlsxwriter::Format {
        use rust_xlsxwriter::{Format, FormatBorder};
        let mut format = Format::new();
        if self.currency {
            format = format.set_num_format("#,##0.00");
        }
        if self.bold {
            format = format.set_bold();
        }
        if self.indent > 0 {
            format = format.set_indent(self.indent);
        }
        if self.top_border {
            format = format.set_border_top(FormatBorder::Thin);
        }
        format
    }
}

/// Widen each column to fit its content (header + cells), with extra room on the
/// label column for its indentation. Clamped so one long label can't run away.
fn autofit_columns(ws: &mut rust_xlsxwriter::Worksheet, sheet: &Sheet) -> Result<()> {
    let max_indent = sheet
        .styles
        .iter()
        .map(|s| s.indent as usize)
        .max()
        .unwrap_or(0);
    for (c, column) in sheet.table.columns.iter().enumerate() {
        let mut width = column.header.chars().count();
        for row in &sheet.table.rows {
            if let Some(cell) = row.get(c) {
                width = width.max(display_len(cell));
            }
        }
        if c == 0 {
            width += max_indent * 2;
        }
        let width = ((width + 2) as f64).clamp(8.0, 60.0);
        ws.set_column_width(c as u16, width)?;
    }
    Ok(())
}

fn display_len(cell: &Cell) -> usize {
    match cell {
        Cell::Empty => 0,
        Cell::Text(s) => s.chars().count(),
        Cell::Bool(_) => 5,
        Cell::Number(n) => format!("{n:.2}").len(),
    }
}

fn sanitize_sheet_name(raw: &str, used: &mut Vec<String>) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| if "[]:*?/\\".contains(c) { '_' } else { c })
        .collect();
    let mut name: String = cleaned.trim().chars().take(31).collect();
    if name.is_empty() {
        name = "Sheet".to_string();
    }
    let base = name.clone();
    let mut i = 2;
    while used.iter().any(|u| u.eq_ignore_ascii_case(&name)) {
        let suffix = format!(" ({i})");
        let keep = 31usize.saturating_sub(suffix.len());
        name = format!("{}{}", base.chars().take(keep).collect::<String>(), suffix);
        i += 1;
    }
    used.push(name.clone());
    name
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Table {
        Table {
            columns: vec![
                Column::new("region", "Region", ColFormat::General),
                Column::new("rep", "Rep", ColFormat::General),
                Column::new("amount", "Amount", ColFormat::Currency),
            ],
            rows: vec![
                vec![
                    Cell::Text("EU".into()),
                    Cell::Text("Ann".into()),
                    Cell::Number(100.0),
                ],
                vec![
                    Cell::Text("EU".into()),
                    Cell::Text("Bob".into()),
                    Cell::Number(50.0),
                ],
                vec![
                    Cell::Text("US".into()),
                    Cell::Text("Cy".into()),
                    Cell::Number(70.0),
                ],
            ],
        }
    }

    #[test]
    fn projection_selects_and_renames() {
        let layout = Layout::from_json(&json!({
            "columns": [
                { "field": "rep" },
                { "field": "amount", "header": "Net" }
            ]
        }));
        let out = apply_layout(&sample(), &layout);
        assert_eq!(out.columns.len(), 2);
        assert_eq!(out.columns[0].key, "rep");
        assert_eq!(out.columns[1].header, "Net");
        assert_eq!(
            out.rows[0],
            vec![Cell::Text("Ann".into()), Cell::Number(100.0)]
        );
    }

    #[test]
    fn projection_skips_unknown_field() {
        let layout = Layout::from_json(&json!({
            "columns": [ { "field": "rep" }, { "field": "nope" } ]
        }));
        let out = apply_layout(&sample(), &layout);
        assert_eq!(out.columns.len(), 1, "unknown field dropped");
    }

    #[test]
    fn group_by_sum_aggregates_per_group() {
        let layout = Layout::from_json(&json!({
            "group_by": ["region"],
            "aggregate": [ { "field": "amount", "fn": "sum" } ]
        }));
        let out = apply_layout(&sample(), &layout);
        assert_eq!(out.columns.len(), 2);
        assert_eq!(out.columns[1].header, "Sum of Amount");
        // First-seen order: EU then US.
        assert_eq!(out.rows[0][0], Cell::Text("EU".into()));
        assert_eq!(out.rows[0][1], Cell::Number(150.0));
        assert_eq!(out.rows[1][1], Cell::Number(70.0));
    }

    #[test]
    fn group_by_without_aggregate_counts() {
        let layout = Layout::from_json(&json!({ "group_by": ["region"] }));
        let out = apply_layout(&sample(), &layout);
        assert_eq!(out.columns[1].header, "Count");
        assert_eq!(out.rows[0][1], Cell::Number(2.0));
        assert_eq!(out.rows[1][1], Cell::Number(1.0));
    }

    #[test]
    fn sort_descending_by_number() {
        let layout =
            Layout::from_json(&json!({ "sort": [ { "field": "amount", "dir": "desc" } ] }));
        let out = apply_layout(&sample(), &layout);
        let amounts: Vec<f64> = out.rows.iter().map(|r| r[2].as_number().unwrap()).collect();
        assert_eq!(amounts, vec![100.0, 70.0, 50.0]);
    }

    #[test]
    fn totals_row_sums_numeric_columns() {
        let layout = Layout::from_json(&json!({ "totals_row": true }));
        let out = apply_layout(&sample(), &layout);
        let last = out.rows.last().unwrap();
        assert_eq!(last[0], Cell::Text("Total".into()));
        assert_eq!(last[2], Cell::Number(220.0));
    }

    #[test]
    fn empty_layout_is_identity() {
        assert!(Layout::from_json(&json!({})).is_empty());
        assert!(Layout::from_json(&Value::Null).is_empty());
        let out = apply_layout(&sample(), &Layout::default());
        assert_eq!(out.rows.len(), 3);
        assert_eq!(out.columns.len(), 3);
    }

    #[test]
    fn build_xlsx_produces_a_valid_zip_workbook() {
        let sheets = vec![Sheet::new("Invoices/2026:Q1", sample())]; // forbidden chars
        let bytes = build_xlsx(&sheets).unwrap();
        // xlsx is a zip — check the PK magic and that it's non-trivial.
        assert_eq!(&bytes[0..2], b"PK");
        assert!(bytes.len() > 500);
    }

    #[test]
    fn table_from_objects_unions_keys_and_fills_missing() {
        let rows = vec![
            json!({ "name": "Ann", "dept": "Eng" }),
            json!({ "name": "Bo", "salary": 50 }), // `salary` only on row 2
        ];
        let table = table_from_objects(&rows);
        // serde_json orders object keys alphabetically: dept, name, salary.
        let keys: Vec<&str> = table.columns.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, vec!["dept", "name", "salary"]);
        // Missing key → empty cell; numbers stay numeric.
        assert_eq!(table.rows[1][0], Cell::Empty); // Bo has no dept
        assert_eq!(table.rows[1][2], Cell::Number(50.0));
    }

    #[test]
    fn reorder_columns_applies_order_dedupes_and_appends_extras() {
        let rows = vec![json!({ "a": 1, "b": 2, "z": 3 })];
        let table = table_from_objects(&rows);
        let order = vec![
            "z".to_string(),
            "b".to_string(),
            "z".to_string(),
            "missing".to_string(),
        ];
        let out = reorder_columns(table, &order);
        let keys: Vec<&str> = out.columns.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, vec!["z", "b", "a"]);
        assert_eq!(
            out.rows[0],
            vec![Cell::Number(3.0), Cell::Number(2.0), Cell::Number(1.0)]
        );
    }

    #[test]
    fn reorder_columns_identity_order_is_noop() {
        let rows = vec![json!({ "a": 1, "b": 2 })];
        let table = table_from_objects(&rows);
        let out = reorder_columns(table, &["a".to_string(), "b".to_string()]);
        let keys: Vec<&str> = out.columns.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, vec!["a", "b"]);
    }

    #[test]
    fn cell_from_json_maps_types() {
        assert_eq!(Cell::from_json(&json!(null)), Cell::Empty);
        assert_eq!(Cell::from_json(&json!("")), Cell::Empty);
        assert_eq!(Cell::from_json(&json!(true)), Cell::Bool(true));
        assert_eq!(Cell::from_json(&json!(3.5)), Cell::Number(3.5));
        assert_eq!(
            Cell::from_json(&json!("=cmd()")),
            Cell::Text("=cmd()".into())
        );
        assert_eq!(Cell::from_json(&json!([1, 2])), Cell::Text("[1,2]".into()));
    }

    #[test]
    fn cell_to_json_round_trips_from_json() {
        for v in [json!(null), json!(true), json!(3.5), json!("text")] {
            assert_eq!(Cell::from_json(&v).to_json(), v);
        }
        assert_eq!(Cell::Number(2.0).to_json(), json!(2.0));
    }

    #[test]
    fn sheet_names_sanitised_and_deduped() {
        let mut used = Vec::new();
        assert_eq!(sanitize_sheet_name("a/b:c", &mut used), "a_b_c");
        assert_eq!(sanitize_sheet_name("dup", &mut used), "dup");
        assert_eq!(sanitize_sheet_name("dup", &mut used), "dup (2)");
        let long = "x".repeat(40);
        assert!(sanitize_sheet_name(&long, &mut used).len() <= 31);
    }

    #[test]
    fn parse_numeric_reads_dressed_up_values() {
        assert_eq!(parse_numeric("£3.1M"), Some(3_100_000.0));
        assert_eq!(parse_numeric("$138B"), Some(138_000_000_000.0));
        assert_eq!(parse_numeric("12.4k"), Some(12_400.0));
        assert_eq!(parse_numeric("94%"), Some(94.0));
        assert_eq!(parse_numeric("2,280"), Some(2280.0));
        assert_eq!(parse_numeric("-4"), Some(-4.0));
        assert_eq!(parse_numeric("hello"), None);
        assert_eq!(parse_numeric("%"), None);
    }

    #[test]
    fn build_xlsx_styled_sheet_is_valid_zip() {
        let table = Table {
            columns: vec![
                Column::new("label", "", ColFormat::General),
                Column::new("v0", "May", ColFormat::Currency),
            ],
            rows: vec![
                vec![Cell::Text("Income".into()), Cell::Empty],
                vec![Cell::Text("Sales".into()), Cell::Number(100.0)],
                vec![Cell::Text("Total".into()), Cell::Number(100.0)],
            ],
        };
        let styles = vec![
            RowStyle {
                bold: true,
                indent: 0,
                top_border: false,
            },
            RowStyle {
                bold: false,
                indent: 1,
                top_border: false,
            },
            RowStyle {
                bold: true,
                indent: 0,
                top_border: true,
            },
        ];
        let bytes = build_xlsx(&[Sheet::styled("P&L", table, styles)]).unwrap();
        assert_eq!(&bytes[0..2], b"PK");
        assert!(bytes.len() > 500);
    }

    #[test]
    fn sheet_new_is_unstyled_and_round_trips() {
        let sheet = Sheet::new("x", sample());
        assert!(sheet.styles.is_empty());
        let bytes = build_xlsx(&[sheet]).unwrap();
        assert_eq!(&bytes[0..2], b"PK");
    }
}
