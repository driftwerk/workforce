//! Engine-agnostic database backend: the [`Backend`] trait plus the shared
//! row model, text→JSON decoding, and read-only statement classification.
//!
//! Two backends implement the trait — [`postgres::PgBackend`] for the Postgres
//! family (Postgres, Redshift, Cloud SQL-Postgres, AlloyDB) and
//! [`mysql::MyBackend`] for MySQL / Cloud SQL-MySQL. Engine-specific logic is
//! confined to connection, raw row streaming, and the read-only/timeout SQL
//! dialect; introspection and `EXPLAIN` are shared default methods.

pub mod mysql;
pub mod postgres;

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use eyre::{eyre, Result};
use regex::Regex;
use serde_json::{json, Value};

use super::config::STAGED_MAX_QUERY_ROWS;

/// A supported database engine. [`Engine::Redshift`] speaks the Postgres wire
/// protocol but is flagged so the backend can prefer the simple/text query path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Postgres,
    Redshift,
    Mysql,
}

impl Engine {
    pub fn as_str(&self) -> &'static str {
        match self {
            Engine::Postgres => "postgres",
            Engine::Redshift => "redshift",
            Engine::Mysql => "mysql",
        }
    }

    pub fn parse(raw: &str) -> Option<Engine> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" | "pg" | "cloudsql-postgres" | "alloydb" => {
                Some(Engine::Postgres)
            }
            "redshift" => Some(Engine::Redshift),
            "mysql" | "mariadb" | "cloudsql-mysql" => Some(Engine::Mysql),
            _ => None,
        }
    }

    /// Infer the engine from a DSN scheme (`postgres://`, `mysql://`, …).
    pub fn infer_from_dsn(dsn: &str) -> Option<Engine> {
        let scheme = dsn.split("://").next().unwrap_or("").to_ascii_lowercase();
        match scheme.as_str() {
            "postgres" | "postgresql" => Some(Engine::Postgres),
            "redshift" => Some(Engine::Redshift),
            "mysql" | "mariadb" => Some(Engine::Mysql),
            _ => None,
        }
    }

    /// Whether this engine uses the Postgres driver family.
    pub fn is_postgres_family(&self) -> bool {
        matches!(self, Engine::Postgres | Engine::Redshift)
    }
}

/// A capped, materialized read result (the `database.query` path).
#[derive(Debug, Default, Clone)]
pub struct QueryResult {
    pub columns: Vec<String>,
    /// Row cells in column order; values are JSON scalars.
    pub rows: Vec<Vec<Value>>,
    /// Whether more rows existed beyond the cap.
    pub truncated: bool,
}

/// Outcome of a write (`database.execute`).
#[derive(Debug, Default, Clone)]
pub struct ExecOutcome {
    pub rows_affected: u64,
    pub statements: usize,
}

pub use crate::artifacts::stream::{Flow, RowSink};

/// A [`RowSink`] that materializes up to `cap` rows and stops, recording
/// whether more existed. Backs the shared [`Backend::query`] default so each
/// engine only implements streaming [`Backend::export`].
pub struct CollectSink {
    cap: usize,
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
    truncated: bool,
}

impl CollectSink {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            columns: Vec::new(),
            rows: Vec::new(),
            truncated: false,
        }
    }

    pub fn finish(self) -> QueryResult {
        QueryResult {
            columns: self.columns,
            rows: self.rows,
            truncated: self.truncated,
        }
    }
}

impl RowSink for CollectSink {
    fn columns(&mut self, names: &[String]) -> Result<()> {
        self.columns = names.to_vec();
        Ok(())
    }

    fn push(&mut self, cells: Vec<Value>) -> Result<Flow> {
        if self.rows.len() < self.cap {
            self.rows.push(cells);
            Ok(Flow::Continue)
        } else {
            self.truncated = true;
            Ok(Flow::Stop)
        }
    }
}

/// Introspection scope for the shared [`Backend::introspect`] default.
#[derive(Debug, Clone, Copy)]
pub enum Introspect<'a> {
    Schemas,
    Tables {
        schema: Option<&'a str>,
    },
    Columns {
        schema: Option<&'a str>,
        table: &'a str,
    },
}

#[async_trait]
pub trait Backend: Send + Sync {
    fn engine(&self) -> Engine;
    fn allow_writes(&self) -> bool;

    /// Liveness probe.
    async fn ping(&self) -> Result<()>;

    /// Stream a read-only statement to `sink` without materializing the whole
    /// result, returning the number of rows pushed. Enforced read-only at the
    /// transaction level by the implementation.
    async fn export(
        &self,
        sql: &str,
        timeout: Duration,
        sink: &mut (dyn RowSink + Send),
    ) -> Result<usize>;

    /// Run a read-only statement and return up to `cap` rows, flagging
    /// [`QueryResult::truncated`] when more existed. Shared across engines via
    /// the streaming [`Backend::export`] path.
    async fn query(&self, sql: &str, cap: usize, timeout: Duration) -> Result<QueryResult> {
        let mut sink = CollectSink::new(cap);
        self.export(sql, timeout, &mut sink).await?;
        Ok(sink.finish())
    }

    /// Run a write statement (DDL/DML). Only reachable when [`allow_writes`]
    /// is set and the caller cleared the admin + approval gates.
    async fn execute(&self, sql: &str, timeout: Duration) -> Result<ExecOutcome>;

    /// System schemas to exclude from introspection listings.
    fn system_schemas(&self) -> &'static [&'static str];

    async fn explain(&self, sql: &str, timeout: Duration) -> Result<QueryResult> {
        self.query(&format!("EXPLAIN {sql}"), STAGED_MAX_QUERY_ROWS, timeout)
            .await
    }

    async fn introspect(&self, scope: Introspect<'_>, timeout: Duration) -> Result<QueryResult> {
        self.query(
            &self.introspection_sql(scope),
            STAGED_MAX_QUERY_ROWS,
            timeout,
        )
        .await
    }

    fn introspection_sql(&self, scope: Introspect<'_>) -> String {
        match scope {
            Introspect::Schemas => {
                let excluded = quote_list(self.system_schemas());
                format!(
                    "SELECT schema_name FROM information_schema.schemata \
                     WHERE schema_name NOT IN ({excluded}) ORDER BY schema_name"
                )
            }
            Introspect::Tables { schema } => {
                let mut sql = String::from(
                    "SELECT table_schema, table_name, table_type \
                     FROM information_schema.tables WHERE ",
                );
                match schema {
                    Some(s) => sql.push_str(&format!("table_schema = {}", quote_literal(s))),
                    None => sql.push_str(&format!(
                        "table_schema NOT IN ({})",
                        quote_list(self.system_schemas())
                    )),
                }
                sql.push_str(" ORDER BY table_schema, table_name");
                sql
            }
            Introspect::Columns { schema, table } => {
                let mut sql = format!(
                    "SELECT column_name, data_type, is_nullable, ordinal_position \
                     FROM information_schema.columns WHERE table_name = {}",
                    quote_literal(table)
                );
                if let Some(s) = schema {
                    sql.push_str(&format!(" AND table_schema = {}", quote_literal(s)));
                }
                sql.push_str(" ORDER BY ordinal_position");
                sql
            }
        }
    }
}

/// Escape a value for use as a single-quoted SQL string literal.
pub fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn quote_list(values: &[&str]) -> String {
    values
        .iter()
        .map(|v| quote_literal(v))
        .collect::<Vec<_>>()
        .join(", ")
}

static DECIMAL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^-?\d+\.\d+([eE][+-]?\d+)?$").expect("valid regex"));

/// Decode a text-protocol cell into a JSON scalar. Integers and decimals that
/// round-trip exactly become JSON numbers (so `data.query` can aggregate
/// without casts); everything else — including codes with leading zeros,
/// timestamps, and oversized integers — stays a string.
pub fn text_to_json(cell: Option<&str>) -> Value {
    let Some(s) = cell else {
        return Value::Null;
    };
    if let Ok(i) = s.parse::<i64>() {
        if i.to_string() == s {
            return json!(i);
        }
    }
    if DECIMAL_RE.is_match(s) {
        if let Some(n) = s
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .and_then(serde_json::Number::from_f64)
        {
            // Only emit a JSON number when it reserializes to the input exactly,
            // so a high-precision NUMERIC (or a trailing-zero value like
            // "100.10") stays a lossless string rather than being silently
            // rounded by the f64 round-trip.
            if n.to_string() == s {
                return Value::Number(n);
            }
        }
    }
    Value::String(s.to_string())
}

/// Strip leading SQL comments/whitespace and return the first keyword lowercased.
fn leading_keyword(sql: &str) -> Option<String> {
    let mut rest = sql.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map_or("", |(_, r)| r).trim_start();
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, r)| r).trim_start();
        } else {
            break;
        }
    }
    rest.split(|c: char| c.is_whitespace() || c == '(')
        .find(|w| !w.is_empty())
        .map(|w| w.to_ascii_lowercase())
}

const READ_KEYWORDS: &[&str] = &["select", "with", "explain", "show", "values", "table"];

/// Reject statements that are obviously not a single read. This is a usability
/// guard, NOT the write-prevention guarantee — the read-only transaction and
/// the least-privilege database role are the real boundaries. The driver's
/// single-statement default and the read-only transaction defeat the cases this
/// misses (data-modifying CTEs, multi-statement batches).
pub fn classify_read_only(sql: &str) -> Result<()> {
    match leading_keyword(sql) {
        Some(kw) if READ_KEYWORDS.contains(&kw.as_str()) => Ok(()),
        Some(kw) => Err(eyre!(
            "database.query runs read-only statements; '{kw}' is not allowed. \
             Use database.execute for writes (requires an admin + a writable instance)."
        )),
        None => Err(eyre!("empty SQL statement")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_parse_and_infer() {
        assert_eq!(Engine::parse("postgresql"), Some(Engine::Postgres));
        assert_eq!(Engine::parse("Redshift"), Some(Engine::Redshift));
        assert_eq!(Engine::parse("mariadb"), Some(Engine::Mysql));
        assert_eq!(Engine::parse("oracle"), None);
        assert_eq!(
            Engine::infer_from_dsn("postgres://u@h/db"),
            Some(Engine::Postgres)
        );
        assert_eq!(
            Engine::infer_from_dsn("mysql://u@h/db"),
            Some(Engine::Mysql)
        );
        assert!(Engine::Redshift.is_postgres_family());
        assert!(!Engine::Mysql.is_postgres_family());
    }

    #[test]
    fn text_decoding_preserves_types_and_codes() {
        assert_eq!(text_to_json(None), Value::Null);
        assert_eq!(text_to_json(Some("42")), json!(42));
        assert_eq!(text_to_json(Some("-7")), json!(-7));
        assert_eq!(text_to_json(Some("3.5")), json!(3.5));
        // Decimals stay lossless: trailing zeros and high-precision values that
        // don't round-trip through f64 remain strings.
        assert_eq!(text_to_json(Some("100.10")), json!("100.10"));
        assert_eq!(
            text_to_json(Some("123.456789012345678")),
            json!("123.456789012345678")
        );
        assert_eq!(text_to_json(Some("007")), json!("007"));
        assert_eq!(text_to_json(Some("+5")), json!("+5"));
        assert_eq!(text_to_json(Some("2026-06-14")), json!("2026-06-14"));
        assert_eq!(text_to_json(Some("hello")), json!("hello"));
        // Oversized integers stay strings (lossless).
        assert_eq!(
            text_to_json(Some("123456789012345678901234")),
            json!("123456789012345678901234")
        );
    }

    #[test]
    fn classify_accepts_reads_and_rejects_writes() {
        assert!(classify_read_only("SELECT 1").is_ok());
        assert!(classify_read_only("  with t as (select 1) select * from t").is_ok());
        assert!(classify_read_only("-- a comment\nSELECT 1").is_ok());
        assert!(classify_read_only("/* c */ EXPLAIN SELECT 1").is_ok());
        assert!(classify_read_only("(SELECT 1)").is_ok());
        assert!(classify_read_only("DELETE FROM t").is_err());
        assert!(classify_read_only("update t set x=1").is_err());
        assert!(classify_read_only("DROP TABLE t").is_err());
        assert!(classify_read_only("   ").is_err());
    }

    #[test]
    fn quote_literal_escapes_quotes() {
        assert_eq!(quote_literal("a'b"), "'a''b'");
        assert_eq!(quote_literal("plain"), "'plain'");
    }

    #[test]
    fn introspection_sql_excludes_system_schemas() {
        struct Pg;
        #[async_trait]
        impl Backend for Pg {
            fn engine(&self) -> Engine {
                Engine::Postgres
            }
            fn allow_writes(&self) -> bool {
                false
            }
            async fn ping(&self) -> Result<()> {
                Ok(())
            }
            async fn query(&self, _: &str, _: usize, _: Duration) -> Result<QueryResult> {
                Ok(QueryResult::default())
            }
            async fn export(
                &self,
                _: &str,
                _: Duration,
                _: &mut (dyn RowSink + Send),
            ) -> Result<usize> {
                Ok(0)
            }
            async fn execute(&self, _: &str, _: Duration) -> Result<ExecOutcome> {
                Ok(ExecOutcome::default())
            }
            fn system_schemas(&self) -> &'static [&'static str] {
                &["pg_catalog", "information_schema"]
            }
        }
        let sql = Pg.introspection_sql(Introspect::Columns {
            schema: Some("public"),
            table: "users",
        });
        assert!(sql.contains("table_name = 'users'"));
        assert!(sql.contains("table_schema = 'public'"));
        let schemas = Pg.introspection_sql(Introspect::Schemas);
        assert!(schemas.contains("'pg_catalog'"));
    }
}
