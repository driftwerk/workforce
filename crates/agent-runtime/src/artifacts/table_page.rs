//! One filtered, sorted page of a staged tabular artifact, for a client's
//! artifact-backed `wf-table` and a `wf-form` field's `options_ref` typeahead.
//! Semantics match the renderer's inline table (`@workforce/rich` `spec.ts`)
//! so a small inline table and a large server-backed one behave identically.
//! Row objects are keyed by column name (`ArtifactDescriptor::schema` carries
//! the order); a page materialises only its own cells.

use std::cmp::Ordering;

use serde::Deserialize;
use serde_json::{json, Value};

use super::store::ArtifactStore;
use super::tabular::rows_to_cells;
use super::types::{ArtifactBody, ArtifactDescriptor, ArtifactId, ContentType};

/// Largest staged table a client may page over; beyond this the model should
/// deliver a spreadsheet export instead of a browsable table.
pub const MAX_TABLE_ROWS: usize = 50_000;
/// Page-size ceiling and default for a page request.
pub const MAX_TABLE_PAGE: usize = 200;
pub const DEFAULT_TABLE_PAGE: usize = 25;

/// A client's page request, as the renderer's `tableQueryString` sends it.
/// `filters` is a JSON array `[{"col","op","value"}]` with
/// `op ∈ contains|gte|lte`.
#[derive(Debug, Default, Deserialize)]
pub struct PageRequest {
    #[serde(default)]
    pub offset: usize,
    pub limit: Option<usize>,
    pub sort: Option<String>,
    pub dir: Option<String>,
    pub q: Option<String>,
    pub filters: Option<String>,
}

/// Why a page could not be served.
#[derive(Debug, PartialEq, Eq)]
pub enum PageRefusal {
    /// Not a staged table: missing, gone, or some other kind of artifact.
    NotFound,
    /// More rows than a client may browse.
    TooLarge,
    /// The store could not be read.
    Unavailable,
}

/// One page of the staged table `id` as the renderer's `TablePage` reads it:
/// `{columns: [{key,label,type}], rows, total, offset, limit}`. Whose table it
/// is is the caller's to check, against `descriptor`.
pub fn table_page(
    store: &dyn ArtifactStore,
    agent_id: &str,
    id: &ArtifactId,
    descriptor: &ArtifactDescriptor,
    request: &PageRequest,
) -> Result<Value, PageRefusal> {
    let Some(schema) = descriptor
        .schema
        .as_deref()
        .filter(|_| descriptor.mime == ContentType::ApplicationJson)
    else {
        return Err(PageRefusal::NotFound);
    };
    if descriptor.row_count.is_some_and(|n| n > MAX_TABLE_ROWS) {
        return Err(PageRefusal::TooLarge);
    }
    let rows = match store
        .read(agent_id, id)
        .map_err(|_| PageRefusal::Unavailable)?
    {
        ArtifactBody::Json(Value::Array(rows)) => rows,
        _ => return Err(PageRefusal::NotFound),
    };
    if rows.len() > MAX_TABLE_ROWS {
        return Err(PageRefusal::TooLarge);
    }
    let types = infer_types(schema, &rows);
    let query = resolve(schema, &types, request);
    let page = query_rows(schema, &types, &rows, &query);
    let columns: Vec<Value> = schema
        .iter()
        .zip(&types)
        .map(|(key, ty)| json!({ "key": key, "label": key, "type": ty.as_str() }))
        .collect();
    Ok(json!({
        "columns": columns,
        "rows": page.rows,
        "total": page.total,
        "offset": query.offset,
        "limit": query.limit,
    }))
}

/// Inferred per-column type, driving comparators, filter widgets, and alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Number,
    Date,
    Text,
}

impl ColumnType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Number => "number",
            Self::Date => "date",
            Self::Text => "text",
        }
    }
}

/// One resolved per-column predicate (column already resolved to an index).
#[derive(Debug)]
pub enum Filter {
    /// Case-insensitive substring on the cell's display text (needle lowercased).
    Contains { col: usize, needle: String },
    /// Numeric/date value ≥ bound (dates as epoch millis).
    Min { col: usize, bound: f64 },
    /// Numeric/date value ≤ bound.
    Max { col: usize, bound: f64 },
}

/// A resolved page request. Columns/values are already validated against the
/// schema; `search`/`Contains` needles are lowercased.
#[derive(Debug, Default)]
pub struct TableQuery {
    pub offset: usize,
    pub limit: usize,
    /// `(column index, descending)`.
    pub sort: Option<(usize, bool)>,
    pub search: Option<String>,
    pub filters: Vec<Filter>,
}

/// One page of cells (schema order) plus the total row count after filtering.
#[derive(Debug)]
pub struct QueriedPage {
    pub rows: Vec<Vec<Value>>,
    pub total: usize,
}

#[derive(Debug, Deserialize)]
struct RawFilter {
    col: String,
    op: String,
    value: Value,
}

/// Validate a page request against the table's schema and types: clamp the
/// page, resolve the sort and filter columns by name (unknown ones dropped),
/// and parse filter bounds per column type. A malformed `filters` yields none.
pub fn resolve(schema: &[String], types: &[ColumnType], request: &PageRequest) -> TableQuery {
    let column = |name: &str| schema.iter().position(|c| c == name);
    let limit = request
        .limit
        .unwrap_or(DEFAULT_TABLE_PAGE)
        .clamp(1, MAX_TABLE_PAGE);
    let sort = request
        .sort
        .as_deref()
        .and_then(column)
        .map(|col| (col, request.dir.as_deref() == Some("desc")));
    let search = request
        .q
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase);
    let filters = request
        .filters
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Vec<RawFilter>>(raw).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|f| {
            let col = column(&f.col)?;
            match f.op.as_str() {
                "contains" if types[col] == ColumnType::Text => {
                    let needle = filter_text(&f.value)?.to_lowercase();
                    (!needle.is_empty()).then_some(Filter::Contains { col, needle })
                }
                "gte" if types[col] != ColumnType::Text => Some(Filter::Min {
                    col,
                    bound: filter_bound(&f.value, types[col])?,
                }),
                "lte" if types[col] != ColumnType::Text => Some(Filter::Max {
                    col,
                    bound: filter_bound(&f.value, types[col])?,
                }),
                _ => None,
            }
        })
        .collect();
    TableQuery {
        offset: request.offset,
        limit,
        sort,
        search,
        filters,
    }
}

/// A filter value as text (JSON string or number).
fn filter_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A numeric filter bound under the column type: a number, a numeric string,
/// or for a date column an ISO date string (the renderer sends epoch millis).
fn filter_bound(value: &Value, ty: ColumnType) -> Option<f64> {
    match (ty, value) {
        (ColumnType::Text, _) => None,
        (_, Value::Number(n)) => n.as_f64(),
        (ColumnType::Number, Value::String(s)) => {
            s.trim().parse::<f64>().ok().filter(|f| f.is_finite())
        }
        (ColumnType::Date, Value::String(s)) => as_date(s),
        _ => None,
    }
}

/// The cell value for `key`, treating JSON null, a missing key, and the empty
/// string all as "no value" — matching the renderer's `cellValue`.
fn value<'a>(row: &'a Value, key: &str) -> Option<&'a Value> {
    match row.get(key) {
        Some(Value::Null) | None => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(v) => Some(v),
    }
}

/// A number or a finite numeric string (an LLM habit), else `None`.
fn as_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
        _ => None,
    }
}

/// Epoch millis (UTC) for an ISO date or datetime string, else `None`. Parsing
/// is uniform (no host timezone), so ordering is self-consistent.
fn as_date(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return d
            .and_hms_opt(0, 0, 0)
            .map(|dt| dt.and_utc().timestamp_millis() as f64);
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_millis() as f64);
    }
    [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
    ]
    .iter()
    .find_map(|fmt| chrono::NaiveDateTime::parse_from_str(s, fmt).ok())
    .map(|ndt| ndt.and_utc().timestamp_millis() as f64)
}

/// The cell's display text: null → "", bool → `true`/`false`, else its scalar
/// text. Used for search and `Contains`.
fn display(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// The numeric key for a range filter under the column's type.
fn range_value(v: &Value, ty: ColumnType) -> Option<f64> {
    match ty {
        ColumnType::Number => as_number(v),
        ColumnType::Date => v.as_str().and_then(as_date),
        ColumnType::Text => None,
    }
}

/// Per-column type: `Number` if every present cell is numeric, `Date` if every
/// present cell is an ISO date, else `Text` (all-empty ⇒ `Text`).
pub fn infer_types(schema: &[String], rows: &[Value]) -> Vec<ColumnType> {
    schema
        .iter()
        .map(|key| {
            let mut seen = 0usize;
            let mut nums = 0usize;
            let mut dates = 0usize;
            for row in rows {
                let Some(v) = value(row, key) else { continue };
                seen += 1;
                if as_number(v).is_some() {
                    nums += 1;
                } else if v.as_str().and_then(as_date).is_some() {
                    dates += 1;
                }
            }
            if seen == 0 || nums != seen && dates != seen {
                ColumnType::Text
            } else if nums == seen {
                ColumnType::Number
            } else {
                ColumnType::Date
            }
        })
        .collect()
}

/// Sort key for one cell: numeric for `Number`/`Date`, text otherwise; `None`
/// (empty/unparseable) sorts last in both directions.
enum SortKey {
    Num(f64),
    Text(String),
}

fn sort_key(row: &Value, key: &str, ty: ColumnType) -> Option<SortKey> {
    let v = value(row, key)?;
    match ty {
        ColumnType::Number => as_number(v).map(SortKey::Num),
        ColumnType::Date => v.as_str().and_then(as_date).map(SortKey::Num),
        ColumnType::Text => Some(SortKey::Text(display(v))),
    }
}

fn cmp_keys(a: &SortKey, b: &SortKey) -> Ordering {
    match (a, b) {
        (SortKey::Num(x), SortKey::Num(y)) => x.total_cmp(y),
        (SortKey::Text(x), SortKey::Text(y)) => x.cmp(y),
        _ => Ordering::Equal,
    }
}

fn ordering(a: Option<&SortKey>, b: Option<&SortKey>, descending: bool) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) if descending => cmp_keys(x, y).reverse(),
        (Some(x), Some(y)) => cmp_keys(x, y),
    }
}

fn passes(row: &Value, schema: &[String], types: &[ColumnType], q: &TableQuery) -> bool {
    if let Some(needle) = &q.search {
        let hit = schema
            .iter()
            .filter_map(|k| value(row, k))
            .any(|v| display(v).to_lowercase().contains(needle));
        if !hit {
            return false;
        }
    }
    q.filters.iter().all(|f| match f {
        Filter::Contains { col, needle } => {
            value(row, &schema[*col]).is_some_and(|v| display(v).to_lowercase().contains(needle))
        }
        Filter::Min { col, bound } => value(row, &schema[*col])
            .and_then(|v| range_value(v, types[*col]))
            .is_some_and(|x| x >= *bound),
        Filter::Max { col, bound } => value(row, &schema[*col])
            .and_then(|v| range_value(v, types[*col]))
            .is_some_and(|x| x <= *bound),
    })
}

/// Filter → sort → paginate, materialising only the requested page.
pub fn query_rows(
    schema: &[String],
    types: &[ColumnType],
    rows: &[Value],
    q: &TableQuery,
) -> QueriedPage {
    let mut idx: Vec<usize> = (0..rows.len())
        .filter(|&i| passes(&rows[i], schema, types, q))
        .collect();
    let total = idx.len();
    if let Some((col, descending)) = q.sort {
        let (key, ty) = (&schema[col], types[col]);
        let mut decorated: Vec<(Option<SortKey>, usize)> = idx
            .iter()
            .map(|&i| (sort_key(&rows[i], key, ty), i))
            .collect();
        decorated.sort_by(|(a, _), (b, _)| ordering(a.as_ref(), b.as_ref(), descending));
        idx = decorated.into_iter().map(|(_, i)| i).collect();
    }
    let page: Vec<Value> = idx
        .into_iter()
        .skip(q.offset)
        .take(q.limit)
        .map(|i| rows[i].clone())
        .collect();
    QueriedPage {
        rows: rows_to_cells(&page, schema),
        total,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::store::FsArtifactStore;
    use crate::artifacts::tabular::stage_table;

    fn rows() -> Vec<Value> {
        vec![
            json!({"name": "Acme", "amount": 100, "due": "2026-07-03"}),
            json!({"name": "Birch", "amount": "40", "due": "2026-07-01"}),
            json!({"name": "cedar", "amount": 250, "due": "2026-07-02"}),
            json!({"name": "Delta", "amount": null, "due": ""}),
        ]
    }

    fn schema() -> Vec<String> {
        ["name", "amount", "due"].map(String::from).to_vec()
    }

    fn names(page: &QueriedPage) -> Vec<&str> {
        page.rows.iter().map(|r| r[0].as_str().unwrap()).collect()
    }

    fn run(q: TableQuery) -> QueriedPage {
        let (s, r) = (schema(), rows());
        query_rows(&s, &infer_types(&s, &r), &r, &q)
    }

    fn all() -> TableQuery {
        TableQuery {
            limit: 10,
            ..Default::default()
        }
    }

    #[test]
    fn infers_number_date_text_with_numeric_strings_and_nulls() {
        assert_eq!(
            infer_types(&schema(), &rows()),
            [ColumnType::Text, ColumnType::Number, ColumnType::Date]
        );
        let mixed = [json!({"c": 1}), json!({"c": "x"}), json!({"c": null})];
        assert_eq!(infer_types(&["c".into()], &mixed), [ColumnType::Text]);
        assert_eq!(
            infer_types(&["c".into()], &[json!({"c": ""})]),
            [ColumnType::Text]
        );
    }

    #[test]
    fn sorts_numerically_with_nulls_last_both_directions() {
        let asc = run(TableQuery {
            sort: Some((1, false)),
            ..all()
        });
        assert_eq!(names(&asc), ["Birch", "Acme", "cedar", "Delta"]);
        let desc = run(TableQuery {
            sort: Some((1, true)),
            ..all()
        });
        assert_eq!(names(&desc), ["cedar", "Acme", "Birch", "Delta"]);
    }

    #[test]
    fn sorts_dates_and_text_case_sensitively() {
        let by_date = run(TableQuery {
            sort: Some((2, false)),
            ..all()
        });
        assert_eq!(names(&by_date), ["Birch", "cedar", "Acme", "Delta"]);
        let by_name = run(TableQuery {
            sort: Some((0, false)),
            ..all()
        });
        assert_eq!(names(&by_name)[3], "cedar", "uppercase sorts first");
    }

    #[test]
    fn contains_and_range_filters_and_search() {
        let contains = run(TableQuery {
            filters: vec![Filter::Contains {
                col: 0,
                needle: "c".into(),
            }],
            ..all()
        });
        assert_eq!(contains.total, 3);
        let range = run(TableQuery {
            filters: vec![
                Filter::Min {
                    col: 1,
                    bound: 100.0,
                },
                Filter::Max {
                    col: 1,
                    bound: 200.0,
                },
            ],
            ..all()
        });
        assert_eq!(names(&range), ["Acme"]);
        let search = run(TableQuery {
            search: Some("birch".into()),
            ..all()
        });
        assert_eq!(search.total, 1);
    }

    #[test]
    fn paginates_with_total_reflecting_filter() {
        let page = run(TableQuery {
            offset: 2,
            limit: 1,
            sort: Some((0, false)),
            ..Default::default()
        });
        assert_eq!((page.total, page.rows.len()), (4, 1));
        let past = run(TableQuery {
            offset: 99,
            ..all()
        });
        assert_eq!(past.total, 4);
        assert!(past.rows.is_empty());
    }

    #[test]
    fn missing_cells_fill_null_in_schema_order() {
        let s = ["a", "b"].map(String::from).to_vec();
        let r = [json!({"a": 1})];
        let page = query_rows(&s, &infer_types(&s, &r), &r, &all());
        assert_eq!(page.rows[0], [json!(1), Value::Null]);
    }

    #[test]
    fn a_request_is_clamped_and_validated_against_the_schema() {
        let types = [ColumnType::Text, ColumnType::Number, ColumnType::Date];
        let q = |request: PageRequest| resolve(&schema(), &types, &request);

        let r = q(PageRequest {
            limit: Some(9999),
            sort: Some("amount".into()),
            dir: Some("desc".into()),
            ..Default::default()
        });
        assert_eq!((r.limit, r.sort), (MAX_TABLE_PAGE, Some((1, true))));

        let r = q(PageRequest {
            sort: Some("nope".into()),
            ..Default::default()
        });
        assert_eq!((r.limit, r.sort), (DEFAULT_TABLE_PAGE, None));

        let r = q(PageRequest {
            filters: Some(
                r#"[{"col":"name","op":"contains","value":"AC"},
                    {"col":"amount","op":"gte","value":"100"},
                    {"col":"due","op":"lte","value":"2026-07-10"},
                    {"col":"amount","op":"contains","value":"x"},
                    {"col":"ghost","op":"gte","value":1}]"#
                    .into(),
            ),
            ..Default::default()
        });
        assert_eq!(r.filters.len(), 3);
        assert!(matches!(&r.filters[0], Filter::Contains { col: 0, needle } if needle == "ac"));
        assert!(matches!(r.filters[1], Filter::Min { col: 1, bound } if bound == 100.0));
        assert!(matches!(r.filters[2], Filter::Max { col: 2, .. }));

        let r = q(PageRequest {
            filters: Some("not json".into()),
            q: Some("  ".into()),
            ..Default::default()
        });
        assert!(r.filters.is_empty() && r.search.is_none());
    }

    #[test]
    fn a_staged_table_is_served_a_page_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let rows: Vec<Value> = (0..30)
            .map(|i| json!({"id": i, "name": format!("row {i}")}))
            .collect();
        let crate::artifacts::StagedTable::Staged { artifact_id, .. } = stage_table(
            store.as_ref(),
            "agent",
            "test",
            Some(vec!["id".into(), "name".into()]),
            rows,
            1,
        )
        .unwrap() else {
            panic!("staged")
        };
        let descriptor = store.metadata("agent", &artifact_id).unwrap().unwrap();
        let page = table_page(
            store.as_ref(),
            "agent",
            &artifact_id,
            &descriptor,
            &PageRequest {
                offset: 25,
                sort: Some("id".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(page["total"], 30);
        assert_eq!(page["rows"].as_array().unwrap().len(), 5);
        assert_eq!(page["rows"][0], json!([25, "row 25"]));
        assert_eq!(
            page["columns"],
            json!([
                {"key": "id", "label": "id", "type": "number"},
                {"key": "name", "label": "name", "type": "text"},
            ])
        );
    }
}
