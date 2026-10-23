//! Read-only SQL engine over canonical tabular data.
//!
//! Each call builds a throwaway in-memory SQLite database, ingests the input
//! tables (untyped columns — every cell keeps its natural bound type, so messy
//! real data needs no schema inference), runs ONE read-only statement under an
//! authorizer sandbox, and returns canonical row objects. Pure — no artifact
//! store, no task-locals — so the whole engine is unit-testable; the
//! `data.query` tool owns resolution, scoping, and staging.

use std::time::{Duration, Instant};

use eyre::{eyre, Result};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::Connection;
use serde_json::Value;

use crate::artifacts::tabular::{normalize_columns, rows_to_cells, zip_table};

pub const MAX_INPUTS: usize = 8;
pub const MAX_SQL_BYTES: usize = 100_000;
pub const MAX_INPUT_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_RESULT_ROWS: usize = 1_000_000;
pub const MAX_RESULT_BYTES: usize = 64 * 1024 * 1024;
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

const PROGRESS_OPS_INTERVAL: i32 = 4096;

/// One staged table presented to the SQL layer as `alias`.
#[derive(Debug)]
pub struct TableInput {
    pub alias: String,
    pub columns: Vec<String>,
    pub rows: Vec<Value>,
}

/// Canonical query output: row objects plus the (normalized) result column
/// order — `SELECT a.id, b.id` yields `id`, `id_2`.
#[derive(Debug)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Value>,
}

/// Cheap serialized-size estimate, used for the input and result byte caps.
/// The timeout can't catch a single huge allocation (e.g. `printf` building a
/// multi-MB string per cell), so byte accounting is the actual memory guard.
pub fn estimate_json_bytes(value: &Value) -> usize {
    match value {
        Value::Null => 4,
        Value::Bool(_) => 5,
        Value::Number(_) => 12,
        Value::String(s) => s.len() + 2,
        Value::Array(items) => 2 + items.iter().map(estimate_json_bytes).sum::<usize>(),
        Value::Object(fields) => {
            2 + fields
                .iter()
                .map(|(k, v)| k.len() + 4 + estimate_json_bytes(v))
                .sum::<usize>()
        }
    }
}

pub fn run_query(
    inputs: Vec<TableInput>,
    sql: &str,
    timeout: Duration,
    max_result_rows: usize,
    max_result_bytes: usize,
) -> Result<QueryResult> {
    validate(sql, &inputs)?;
    let aliases: Vec<String> = inputs.iter().map(|i| i.alias.clone()).collect();
    let conn = Connection::open_in_memory()?;
    for input in inputs {
        ingest(&conn, input)?;
    }
    assert_single_statement(&conn, sql, &aliases)?;
    install_sandbox(&conn);
    let deadline = Instant::now() + timeout;
    conn.progress_handler(
        PROGRESS_OPS_INTERVAL,
        Some(move || Instant::now() >= deadline),
    );

    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| explain_sql_error(e, &aliases))?;
    if !stmt.readonly() {
        return Err(eyre!(
            "only read-only SELECT statements are allowed (statement would write)"
        ));
    }
    let columns = normalize_columns(stmt.column_names().iter().map(|s| s.to_string()));
    if columns.is_empty() {
        return Err(eyre!("statement returns no columns; provide a SELECT"));
    }

    let mut cells: Vec<Vec<Value>> = Vec::new();
    let mut result_bytes = 0usize;
    let mut rows = stmt.query([]).map_err(|e| explain_sql_error(e, &aliases))?;
    loop {
        match rows.next() {
            Ok(Some(row)) => {
                if cells.len() >= max_result_rows {
                    return Err(eyre!(
                        "result exceeded {max_result_rows} rows; aggregate or add a LIMIT"
                    ));
                }
                let record = (0..columns.len())
                    .map(|i| Ok(sql_to_json(row.get_ref(i)?)))
                    .collect::<Result<Vec<Value>>>()?;
                result_bytes += record.iter().map(estimate_json_bytes).sum::<usize>();
                if result_bytes > max_result_bytes {
                    return Err(eyre!(
                        "result exceeded {} MB; aggregate or add a LIMIT",
                        max_result_bytes / (1024 * 1024)
                    ));
                }
                cells.push(record);
            }
            Ok(None) => break,
            Err(e) if is_interrupt(&e) => {
                return Err(eyre!("query timed out after {}s", timeout.as_secs()));
            }
            Err(e) => return Err(explain_sql_error(e, &aliases)),
        }
    }
    Ok(QueryResult {
        rows: zip_table(&columns, cells),
        columns,
    })
}

fn validate(sql: &str, inputs: &[TableInput]) -> Result<()> {
    if sql.trim().is_empty() {
        return Err(eyre!("sql must not be empty"));
    }
    if sql.len() > MAX_SQL_BYTES {
        return Err(eyre!("sql exceeds {MAX_SQL_BYTES} bytes"));
    }
    if inputs.is_empty() {
        return Err(eyre!("provide at least one input table"));
    }
    if inputs.len() > MAX_INPUTS {
        return Err(eyre!("at most {MAX_INPUTS} input tables are supported"));
    }
    let mut seen = std::collections::HashSet::new();
    for input in inputs {
        if !valid_alias(&input.alias) {
            return Err(eyre!(
                "alias `{}` is invalid: use [A-Za-z_][A-Za-z0-9_]*, max 64 chars",
                input.alias
            ));
        }
        if !seen.insert(input.alias.as_str()) {
            return Err(eyre!("duplicate input alias `{}`", input.alias));
        }
        if input.columns.is_empty() {
            return Err(eyre!(
                "input `{}` has no columns (empty dataset with no schema?)",
                input.alias
            ));
        }
    }
    Ok(())
}

fn valid_alias(alias: &str) -> bool {
    let mut chars = alias.chars();
    alias.len() <= 64
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn ingest(conn: &Connection, input: TableInput) -> Result<()> {
    let table = quote_ident(&input.alias);
    let quoted: Vec<String> = input.columns.iter().map(|c| quote_ident(c)).collect();
    conn.execute_batch(&format!("CREATE TABLE {table} ({})", quoted.join(", ")))?;
    let placeholders = vec!["?"; quoted.len()].join(", ");
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare(&format!("INSERT INTO {table} VALUES ({placeholders})"))?;
        for cells in rows_to_cells(&input.rows, &input.columns) {
            stmt.execute(rusqlite::params_from_iter(cells.iter().map(json_to_sql)))?;
        }
    }
    tx.commit()?;
    Ok(())
}

fn install_sandbox(conn: &Connection) {
    conn.authorizer(Some(|ctx: AuthContext<'_>| match ctx.action {
        AuthAction::Select
        | AuthAction::Read { .. }
        | AuthAction::Function { .. }
        | AuthAction::Recursive => Authorization::Allow,
        _ => Authorization::Deny,
    }));
}

/// Reject inputs holding more than one SQL statement. Prepared via
/// [`rusqlite::Batch`] (prepare-only, nothing executes) before the
/// sandbox is installed so even denied statement kinds count cleanly.
fn assert_single_statement(conn: &Connection, sql: &str, aliases: &[String]) -> Result<()> {
    let mut batch = rusqlite::Batch::new(conn, sql);
    let mut count = 0usize;
    while batch
        .next()
        .map_err(|e| explain_sql_error(e, aliases))?
        .is_some()
    {
        count += 1;
        if count > 1 {
            return Err(eyre!("provide exactly one SQL statement"));
        }
    }
    if count == 0 {
        return Err(eyre!("sql contains no statement"));
    }
    Ok(())
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn json_to_sql(value: &Value) -> SqlValue {
    match value {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(*b as i64),
        Value::Number(n) => n
            .as_i64()
            .map(SqlValue::Integer)
            .or_else(|| n.as_f64().map(SqlValue::Real))
            .unwrap_or(SqlValue::Null),
        Value::String(s) => SqlValue::Text(s.clone()),
        other => SqlValue::Text(other.to_string()),
    }
}

fn sql_to_json(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::from(i),
        ValueRef::Real(f) => serde_json::Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => {
            use base64::Engine;
            Value::String(base64::engine::general_purpose::STANDARD.encode(b))
        }
    }
}

fn is_interrupt(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ffi::ErrorCode::OperationInterrupted,
                ..
            },
            _,
        )
    )
}

fn explain_sql_error(e: rusqlite::Error, aliases: &[String]) -> eyre::Report {
    if is_interrupt(&e) {
        return eyre!("query timed out");
    }
    let detail = e.to_string();
    if detail.contains("not authorized") {
        return eyre!(
            "statement not allowed: only read-only SELECT over the input tables is permitted \
             (no PRAGMA, ATTACH, or writes). Underlying error: {detail}"
        );
    }
    if detail.contains("no such table") {
        return eyre!("{detail}. Available tables: {}", aliases.join(", "));
    }
    eyre!("{detail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn table(alias: &str, columns: &[&str], rows: Vec<Value>) -> TableInput {
        TableInput {
            alias: alias.to_string(),
            columns: columns.iter().map(|s| s.to_string()).collect(),
            rows,
        }
    }

    fn quick(inputs: Vec<TableInput>, sql: &str) -> Result<QueryResult> {
        run_query(
            inputs,
            sql,
            Duration::from_secs(5),
            10_000,
            MAX_RESULT_BYTES,
        )
    }

    #[test]
    fn join_across_two_inputs() {
        let a = table(
            "a",
            &["month", "total"],
            vec![
                json!({"month": "Jan", "total": 10}),
                json!({"month": "Feb", "total": 20}),
            ],
        );
        let b = table(
            "b",
            &["month", "total"],
            vec![
                json!({"month": "Jan", "total": 5}),
                json!({"month": "Feb", "total": 7}),
            ],
        );
        let out = quick(
            vec![a, b],
            "SELECT a.month AS month, a.total + b.total AS combined \
             FROM a JOIN b USING (month) ORDER BY combined",
        )
        .unwrap();
        assert_eq!(out.columns, vec!["month", "combined"]);
        assert_eq!(
            out.rows,
            vec![
                json!({"month": "Jan", "combined": 15}),
                json!({"month": "Feb", "combined": 27}),
            ]
        );
    }

    #[test]
    fn duplicate_result_columns_are_normalized() {
        let a = table("a", &["id"], vec![json!({"id": 1})]);
        let b = table("b", &["id"], vec![json!({"id": 1})]);
        let out = quick(vec![a, b], "SELECT a.id, b.id FROM a JOIN b ON a.id = b.id").unwrap();
        assert_eq!(out.columns, vec!["id", "id_2"]);
        assert_eq!(out.rows, vec![json!({"id": 1, "id_2": 1})]);
    }

    #[test]
    fn bools_are_zero_one_and_nested_json_is_queryable() {
        let a = table(
            "a",
            &["active", "meta"],
            vec![
                json!({"active": true, "meta": {"k": "x"}}),
                json!({"active": false, "meta": {"k": "y"}}),
            ],
        );
        let out = quick(
            vec![a],
            "SELECT json_extract(meta, '$.k') AS k FROM a WHERE active = 1",
        )
        .unwrap();
        assert_eq!(out.rows, vec![json!({"k": "x"})]);
    }

    #[test]
    fn nulls_and_floats_roundtrip() {
        let a = table(
            "a",
            &["x"],
            vec![json!({"x": null}), json!({"x": 1.5}), json!({"x": 3})],
        );
        let out = quick(vec![a], "SELECT x FROM a ORDER BY x").unwrap();
        assert_eq!(
            out.rows,
            vec![json!({"x": null}), json!({"x": 1.5}), json!({"x": 3})]
        );
    }

    #[test]
    fn empty_result_keeps_columns() {
        let a = table("a", &["id"], vec![json!({"id": 1})]);
        let out = quick(vec![a], "SELECT id, id * 2 AS double FROM a WHERE id > 99").unwrap();
        assert_eq!(out.columns, vec!["id", "double"]);
        assert!(out.rows.is_empty());
    }

    #[test]
    fn result_row_cap_errors() {
        let a = table("a", &["id"], (0..50).map(|i| json!({"id": i})).collect());
        let err = run_query(
            vec![a],
            "SELECT t1.id FROM a t1, a t2",
            Duration::from_secs(5),
            100,
            MAX_RESULT_BYTES,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("exceeded 100 rows"));
    }

    #[test]
    fn runaway_query_times_out() {
        let a = table("a", &["id"], vec![json!({"id": 1})]);
        let err = run_query(
            vec![a],
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) \
             SELECT count(*) FROM c",
            Duration::from_millis(100),
            10_000,
            MAX_RESULT_BYTES,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("timed out"), "got: {err}");
    }

    #[test]
    fn writes_and_schema_changes_are_denied() {
        for sql in [
            "INSERT INTO a VALUES (1)",
            "UPDATE a SET id = 2",
            "DELETE FROM a",
            "DROP TABLE a",
            "CREATE TABLE evil (x)",
            "ALTER TABLE a RENAME TO b",
        ] {
            let a = table("a", &["id"], vec![json!({"id": 1})]);
            let err = quick(vec![a], sql).unwrap_err().to_string();
            assert!(
                err.contains("not allowed")
                    || err.contains("read-only")
                    || err.contains("no columns"),
                "{sql} should be rejected, got: {err}"
            );
        }
    }

    #[test]
    fn pragma_and_attach_are_denied() {
        for sql in ["PRAGMA writable_schema = ON", "ATTACH ':memory:' AS x"] {
            let a = table("a", &["id"], vec![json!({"id": 1})]);
            let err = quick(vec![a], sql).unwrap_err().to_string();
            assert!(
                err.contains("not allowed") || err.contains("no columns"),
                "{sql} should be rejected, got: {err}"
            );
        }
    }

    #[test]
    fn multiple_statements_rejected() {
        let a = table("a", &["id"], vec![json!({"id": 1})]);
        let err = quick(vec![a], "SELECT id FROM a; SELECT id FROM a")
            .unwrap_err()
            .to_string();
        assert!(err.contains("exactly one"));
    }

    #[test]
    fn invalid_alias_rejected() {
        for alias in ["1bad", "bad-name", "", "a b", "drop;table"] {
            let t = table(alias, &["id"], vec![json!({"id": 1})]);
            let err = quick(vec![t], "SELECT 1").unwrap_err().to_string();
            assert!(err.contains("invalid"), "alias `{alias}`: {err}");
        }
    }

    #[test]
    fn duplicate_alias_rejected() {
        let t1 = table("a", &["id"], vec![json!({"id": 1})]);
        let t2 = table("a", &["id"], vec![json!({"id": 2})]);
        let err = quick(vec![t1, t2], "SELECT 1").unwrap_err().to_string();
        assert!(err.contains("duplicate input alias"));
    }

    #[test]
    fn unknown_table_lists_available_aliases() {
        let a = table("orders", &["id"], vec![json!({"id": 1})]);
        let err = quick(vec![a], "SELECT * FROM missing")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no such table"));
        assert!(err.contains("orders"));
    }

    #[test]
    fn quoted_identifiers_with_spaces_work() {
        let a = table("a", &["Total Revenue"], vec![json!({"Total Revenue": 100})]);
        let out = quick(vec![a], "SELECT \"Total Revenue\" FROM a").unwrap();
        assert_eq!(out.rows, vec![json!({"Total Revenue": 100})]);
    }

    #[test]
    fn missing_keys_ingest_as_nulls() {
        let a = table(
            "a",
            &["id", "name"],
            vec![json!({"id": 1}), json!({"name": "x", "id": 2})],
        );
        let out = quick(vec![a], "SELECT count(*) AS n FROM a WHERE name IS NULL").unwrap();
        assert_eq!(out.rows, vec![json!({"n": 1})]);
    }

    #[test]
    fn empty_input_table_queries_fine() {
        let a = table("a", &["id"], Vec::new());
        let out = quick(vec![a], "SELECT count(*) AS n FROM a").unwrap();
        assert_eq!(out.rows, vec![json!({"n": 0})]);
    }

    #[test]
    fn blob_results_encode_base64() {
        let a = table("a", &["id"], vec![json!({"id": 1})]);
        let out = quick(vec![a], "SELECT zeroblob(4) AS b FROM a").unwrap();
        assert_eq!(out.rows, vec![json!({"b": "AAAAAA=="})]);
    }
}
