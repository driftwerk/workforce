//! The agent-callable `database.*` tools. Each implements the internal
//! [`DbTool`] trait; a blanket [`Tool`] impl supplies the shared timing and
//! error-to-[`ToolResult`] wrapper so no tool repeats that boilerplate.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::{eyre, Result};
use serde_json::{json, Value};

use crate::artifacts::tabular::staged_to_output;
use crate::artifacts::{
    global_store, normalize_columns, stage_table, zip_table, DEFAULT_INLINE_THRESHOLD,
};
use crate::runner::{CURRENT_AGENT_ID, CURRENT_USER_ID};
use crate::security::external_content::wrap_external_content;
use crate::tools::{Tool, ToolResult};

use super::backend::{classify_read_only, Backend, Introspect, QueryResult};
use super::config::{clamp_limit, InstanceConfig};
use super::{export, AdminCheck, DatabaseRuntime};

/// Internal trait every `database.*` tool implements; the blanket `Tool` impl
/// below adds timing and uniform error handling.
#[async_trait]
trait DbTool: Send + Sync {
    fn tool_name(&self) -> &'static str;
    fn tool_description(&self) -> &'static str;
    fn tool_schema(&self) -> Value;
    async fn run(&self, input: Value) -> Result<Value>;
}

#[async_trait]
impl<T: DbTool> Tool for T {
    fn name(&self) -> &str {
        self.tool_name()
    }
    fn description(&self) -> &str {
        self.tool_description()
    }
    fn input_schema(&self) -> Value {
        self.tool_schema()
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = move || start.elapsed().as_millis() as u64;
        Ok(match self.run(input).await {
            Ok(output) => ToolResult::success(output, elapsed()),
            Err(e) => ToolResult::failure(e.to_string(), elapsed()),
        })
    }
}

// ── shared helpers ──────────────────────────────────────────────────

fn instance_arg(input: &Value) -> Option<&str> {
    input["instance"].as_str().filter(|s| !s.is_empty())
}

fn sql_arg(input: &Value) -> Result<&str> {
    input["sql"]
        .as_str()
        .map(|s| s.trim().trim_end_matches(';').trim())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| eyre!("`sql` is required and must be a non-empty string"))
}

fn str_arg<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input[key].as_str().map(str::trim).filter(|s| !s.is_empty())
}

/// Validate an identifier embedded as a SQL string literal in introspection
/// queries. Rejecting quotes/backslashes/semicolons/NUL keeps the
/// literal-building injection-safe across engines (MySQL treats `\` as an
/// escape), so the introspection tools stay introspection-only.
fn ident_arg<'a>(input: &'a Value, key: &str) -> Result<Option<&'a str>> {
    match str_arg(input, key) {
        None => Ok(None),
        Some(s) if s.contains(['\'', '\\', ';', '\0']) => Err(eyre!(
            "invalid {key} '{s}': identifiers may not contain quotes, backslashes, or semicolons"
        )),
        Some(s) => Ok(Some(s)),
    }
}

fn current_agent_id() -> Option<String> {
    CURRENT_AGENT_ID
        .try_with(|a| a.clone())
        .ok()
        .filter(|a| !a.is_empty())
}

/// Emit one audit record per database operation, after it runs, recording the
/// actor, instance, the SQL (or operation), row count, and success — a failed
/// call is distinguishable from a successful one.
fn audit(tool: &str, instance: &str, sql: &str, rows: Option<u64>, ok: bool) {
    let actor = CURRENT_USER_ID
        .try_with(|u| u.clone())
        .ok()
        .filter(|u| !u.is_empty());
    tracing::info!(
        target: "sentinel.audit",
        tool = %tool,
        instance = %instance,
        actor = ?actor,
        sql = %sql,
        rows = ?rows,
        ok = ok,
        "database access"
    );
}

fn instance_schema_prop() -> Value {
    json!({
        "type": "string",
        "description": "Database instance to target (e.g. harbor-rails-db-prod). \
                        Omit to use the default. See database.list_instances. Databases are a \
                        multi-instance skill that environments pin: with an environment active the \
                        instance is locked to its pin and a disagreeing value here is refused; with \
                        no environment active any configured instance is selectable by name."
    })
}

/// Stage a query result: large results become a `tabular_artifact` envelope,
/// small ones inline; rows are wrapped as untrusted external content.
fn deliver(source: &'static str, qr: QueryResult) -> Result<Value> {
    let columns = normalize_columns(qr.columns);
    let truncated = qr.truncated;
    let row_count = qr.rows.len();
    let objects = zip_table(&columns, qr.rows);
    let mut out = match (global_store(), current_agent_id()) {
        (Some(store), Some(agent_id)) => staged_to_output(
            stage_table(
                store.as_ref(),
                &agent_id,
                source,
                Some(columns),
                objects,
                DEFAULT_INLINE_THRESHOLD,
            )?,
            source,
        ),
        _ => {
            let rendered = serde_json::to_string(&objects).unwrap_or_default();
            json!({
                "kind": "rows",
                "columns": columns,
                "row_count": row_count,
                "rows_wrapped": wrap_external_content(&rendered, source).wrapped_content,
            })
        }
    };
    if let Some(obj) = out.as_object_mut() {
        obj.entry("row_count").or_insert(json!(row_count));
        obj.insert("truncated".to_string(), json!(truncated));
    }
    Ok(out)
}

/// Shared introspection path: run the read, audit it, stage and annotate.
async fn run_introspect(
    cfg: &InstanceConfig,
    backend: &dyn Backend,
    source: &'static str,
    op: &str,
    scope: Introspect<'_>,
) -> Result<Value> {
    let result = backend.introspect(scope, cfg.statement_timeout).await;
    audit(
        source,
        &cfg.name,
        op,
        result.as_ref().ok().map(|q| q.rows.len() as u64),
        result.is_ok(),
    );
    let mut out = deliver(source, result?)?;
    annotate(&mut out, &cfg.name);
    Ok(out)
}

// ── database.list_instances ─────────────────────────────────────────

pub struct ListInstancesTool {
    rt: Arc<DatabaseRuntime>,
}

impl ListInstancesTool {
    pub fn new(rt: Arc<DatabaseRuntime>) -> Self {
        Self { rt }
    }
}

#[async_trait]
impl DbTool for ListInstancesTool {
    fn tool_name(&self) -> &'static str {
        "database.list_instances"
    }
    fn tool_description(&self) -> &'static str {
        "List configured database instances (name, engine, writable). No connection details are \
         exposed. When none are configured the result includes a `setup` field describing how an \
         operator adds one — credentials live in the vault `databases/*` namespace and are set \
         from the host; the agent cannot configure instances itself. Database instances are a \
         multi-instance skill that environments pin like any other (member name `database`): with \
         an environment active the agent is locked to its pinned instance (a disagreeing `instance` \
         is refused); with none active any configured instance is selectable by name. Call \
         `env.status` for the live pin."
    }
    fn tool_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn run(&self, _input: Value) -> Result<Value> {
        let instances: Vec<Value> = self
            .rt
            .instances()
            .iter()
            .map(InstanceConfig::summary)
            .collect();
        if instances.is_empty() {
            // The one orientation tool an agent reaches when asked "can you
            // see our database?" — so when nothing is configured, hand back
            // the *correct* setup procedure instead of a bare empty list.
            // The agent cannot do this itself: the `databases/*` vault
            // namespace is blocked from the vault.* tools, so configuration
            // is an operator action on the host.
            return Ok(json!({
                "instances": [],
                "count": 0,
                "setup": "No database instances are configured, and the agent cannot set one up \
                    itself — credentials live in the vault `databases/*` namespace, which is \
                    off-limits to the vault.* tools. An operator configures an instance from the \
                    host (not from chat): `workforce vault set \
                    databases/instances/<name>/dsn '<connection string>'`. Example read-only \
                    Postgres DSN: `postgres://ro_user:PASSWORD@host:5432/dbname?sslmode=require`. \
                    The engine (postgres/mysql/redshift) is inferred from the DSN scheme. \
                    Optional follow-ups: `databases/instances/<name>/allow_writes` set to 'true' \
                    to permit database.execute (default is read-only), and \
                    `databases/default_instance` set to '<name>' when more than one instance \
                    exists. Relay these steps to the user; never ask them to paste a password \
                    into chat — they store it on the host."
            }));
        }
        Ok(json!({ "instances": instances, "count": instances.len() }))
    }
}

// ── database.schemas / tables / columns ─────────────────────────────

pub struct SchemasTool {
    rt: Arc<DatabaseRuntime>,
}

impl SchemasTool {
    pub fn new(rt: Arc<DatabaseRuntime>) -> Self {
        Self { rt }
    }
}

#[async_trait]
impl DbTool for SchemasTool {
    fn tool_name(&self) -> &'static str {
        "database.schemas"
    }
    fn tool_description(&self) -> &'static str {
        "List the (non-system) schemas/databases on a database instance."
    }
    fn tool_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "instance": instance_schema_prop() }
        })
    }
    async fn run(&self, input: Value) -> Result<Value> {
        let (cfg, backend) = self.rt.resolve(instance_arg(&input)).await?;
        run_introspect(
            &cfg,
            backend.as_ref(),
            "database.schemas",
            "schemas",
            Introspect::Schemas,
        )
        .await
    }
}

pub struct TablesTool {
    rt: Arc<DatabaseRuntime>,
}

impl TablesTool {
    pub fn new(rt: Arc<DatabaseRuntime>) -> Self {
        Self { rt }
    }
}

#[async_trait]
impl DbTool for TablesTool {
    fn tool_name(&self) -> &'static str {
        "database.tables"
    }
    fn tool_description(&self) -> &'static str {
        "List tables on a database instance, optionally filtered to one schema."
    }
    fn tool_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "instance": instance_schema_prop(),
                "schema": { "type": "string", "description": "Restrict to this schema/database." }
            }
        })
    }
    async fn run(&self, input: Value) -> Result<Value> {
        let schema = ident_arg(&input, "schema")?;
        let (cfg, backend) = self.rt.resolve(instance_arg(&input)).await?;
        run_introspect(
            &cfg,
            backend.as_ref(),
            "database.tables",
            "tables",
            Introspect::Tables { schema },
        )
        .await
    }
}

pub struct ColumnsTool {
    rt: Arc<DatabaseRuntime>,
}

impl ColumnsTool {
    pub fn new(rt: Arc<DatabaseRuntime>) -> Self {
        Self { rt }
    }
}

#[async_trait]
impl DbTool for ColumnsTool {
    fn tool_name(&self) -> &'static str {
        "database.columns"
    }
    fn tool_description(&self) -> &'static str {
        "List a table's columns (name, type, nullability) on a database instance."
    }
    fn tool_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "instance": instance_schema_prop(),
                "schema": { "type": "string", "description": "Schema/database the table lives in." },
                "table": { "type": "string", "description": "Table name." }
            },
            "required": ["table"]
        })
    }
    async fn run(&self, input: Value) -> Result<Value> {
        let table = ident_arg(&input, "table")?.ok_or_else(|| eyre!("`table` is required"))?;
        let schema = ident_arg(&input, "schema")?;
        let (cfg, backend) = self.rt.resolve(instance_arg(&input)).await?;
        run_introspect(
            &cfg,
            backend.as_ref(),
            "database.columns",
            "columns",
            Introspect::Columns { schema, table },
        )
        .await
    }
}

// ── database.query ──────────────────────────────────────────────────

pub struct QueryTool {
    rt: Arc<DatabaseRuntime>,
}

impl QueryTool {
    pub fn new(rt: Arc<DatabaseRuntime>) -> Self {
        Self { rt }
    }
}

#[async_trait]
impl DbTool for QueryTool {
    fn tool_name(&self) -> &'static str {
        "database.query"
    }
    fn tool_description(&self) -> &'static str {
        "Run a read-only SQL query against a database instance and stage the result. \
         Do the searching/filtering/aggregating IN SQL (WHERE / GROUP BY / JOIN / LIMIT) — \
         the database scans all the rows and only the (small) result returns, so this works \
         over tables of any size. Refine the staged result with data.query, inspect shape \
         with data.inspect, download with export_xlsx. For a bulk dump of many rows to a \
         file, use database.export instead."
    }
    fn tool_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "sql": {
                    "type": "string",
                    "description": "A single read-only statement (SELECT / WITH / EXPLAIN / SHOW). \
                                    Filter and aggregate here; never SELECT * then filter locally."
                },
                "instance": instance_schema_prop(),
                "limit": {
                    "type": "integer",
                    "description": "Max rows (default 2000, hard cap 50000). Large results stage as artifacts."
                }
            },
            "required": ["sql"]
        })
    }
    async fn run(&self, input: Value) -> Result<Value> {
        let sql = sql_arg(&input)?;
        classify_read_only(sql)?;
        let (cfg, backend) = self.rt.resolve(instance_arg(&input)).await?;
        let cap = clamp_limit(
            input["limit"].as_u64().map(|n| n as usize),
            cfg.max_query_rows,
        );
        let result = backend.query(sql, cap, cfg.statement_timeout).await;
        audit(
            "database.query",
            &cfg.name,
            sql,
            result.as_ref().ok().map(|q| q.rows.len() as u64),
            result.is_ok(),
        );
        let mut out = deliver("database.query", result?)?;
        if let Some(obj) = out.as_object_mut() {
            obj.insert("instance".to_string(), json!(cfg.name));
            obj.insert("row_cap".to_string(), json!(cap));
        }
        Ok(out)
    }
}

// ── database.explain ────────────────────────────────────────────────

pub struct ExplainTool {
    rt: Arc<DatabaseRuntime>,
}

impl ExplainTool {
    pub fn new(rt: Arc<DatabaseRuntime>) -> Self {
        Self { rt }
    }
}

#[async_trait]
impl DbTool for ExplainTool {
    fn tool_name(&self) -> &'static str {
        "database.explain"
    }
    fn tool_description(&self) -> &'static str {
        "Return the query planner's EXPLAIN output for a read-only statement, without running it \
         to completion — use it to estimate cost before a large query."
    }
    fn tool_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "sql": { "type": "string", "description": "The statement to explain." },
                "instance": instance_schema_prop()
            },
            "required": ["sql"]
        })
    }
    async fn run(&self, input: Value) -> Result<Value> {
        let sql = sql_arg(&input)?;
        classify_read_only(sql)?;
        let (cfg, backend) = self.rt.resolve(instance_arg(&input)).await?;
        let result = backend.explain(sql, cfg.statement_timeout).await;
        audit(
            "database.explain",
            &cfg.name,
            sql,
            result.as_ref().ok().map(|q| q.rows.len() as u64),
            result.is_ok(),
        );
        let mut out = deliver("database.explain", result?)?;
        annotate(&mut out, &cfg.name);
        Ok(out)
    }
}

// ── database.export ─────────────────────────────────────────────────

pub struct ExportTool {
    rt: Arc<DatabaseRuntime>,
}

impl ExportTool {
    pub fn new(rt: Arc<DatabaseRuntime>) -> Self {
        Self { rt }
    }
}

#[async_trait]
impl DbTool for ExportTool {
    fn tool_name(&self) -> &'static str {
        "database.export"
    }
    fn tool_description(&self) -> &'static str {
        "Stream a read-only query's full result to a CSV file and deliver it to this conversation \
         as a downloadable attachment — for bulk dumps too large to reason about inline. The file \
         is delivered outside your context; you will not see its contents. Prefer database.query \
         when you need to analyze the data yourself."
    }
    fn tool_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "sql": { "type": "string", "description": "A single read-only SELECT to export." },
                "instance": instance_schema_prop(),
                "filename": { "type": "string", "description": "Optional download filename (\".csv\" appended)." }
            },
            "required": ["sql"]
        })
    }
    async fn run(&self, input: Value) -> Result<Value> {
        let sql = sql_arg(&input)?;
        classify_read_only(sql)?;
        let agent_id = current_agent_id()
            .ok_or_else(|| eyre!("database.export requires an active agent context"))?;
        let store = global_store().ok_or_else(|| eyre!("artifact store is not configured"))?;
        let (cfg, backend) = self.rt.resolve(instance_arg(&input)).await?;
        let result = export::run(
            backend.as_ref(),
            sql,
            cfg.statement_timeout,
            &cfg.name,
            str_arg(&input, "filename").unwrap_or(""),
            &agent_id,
            store.as_ref(),
        )
        .await;
        audit(
            "database.export",
            &cfg.name,
            sql,
            result.as_ref().ok().and_then(|v| v["rows"].as_u64()),
            result.is_ok(),
        );
        result
    }
}

// ── database.execute ────────────────────────────────────────────────

pub struct ExecuteTool {
    rt: Arc<DatabaseRuntime>,
    is_admin: AdminCheck,
}

impl ExecuteTool {
    pub fn new(rt: Arc<DatabaseRuntime>, is_admin: AdminCheck) -> Self {
        Self { rt, is_admin }
    }
}

#[async_trait]
impl DbTool for ExecuteTool {
    fn tool_name(&self) -> &'static str {
        "database.execute"
    }
    fn tool_description(&self) -> &'static str {
        "Run a writing statement (INSERT/UPDATE/DELETE/DDL) against a WRITABLE database instance. \
         Requires the SystemAdmin role, an instance with writes enabled, and per-call approval. \
         Use database.query for reads."
    }
    fn tool_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "sql": { "type": "string", "description": "The writing statement to run." },
                "instance": instance_schema_prop()
            },
            "required": ["sql"]
        })
    }
    async fn run(&self, input: Value) -> Result<Value> {
        if !(self.is_admin)() {
            let actor = CURRENT_USER_ID.try_with(|u| u.clone()).ok();
            tracing::warn!(
                target: "sentinel.audit",
                tool = "database.execute",
                actor = ?actor,
                "privileged database tool rejected: caller is not a SystemAdmin"
            );
            return Err(eyre!(
                "database.execute requires the SystemAdmin role — ask a system administrator to run writes."
            ));
        }
        let sql = sql_arg(&input)?;
        let (cfg, backend) = self.rt.resolve(instance_arg(&input)).await?;
        if !cfg.allow_writes {
            return Err(eyre!(
                "database instance '{}' is read-only. An admin enables writes with \
                 vault.set('databases/instances/{}/allow_writes', 'true').",
                cfg.name,
                cfg.name
            ));
        }
        let outcome = backend.execute(sql, cfg.statement_timeout).await;
        audit(
            "database.execute",
            &cfg.name,
            sql,
            outcome.as_ref().ok().map(|o| o.rows_affected),
            outcome.is_ok(),
        );
        let outcome = outcome?;
        Ok(json!({
            "instance": cfg.name,
            "rows_affected": outcome.rows_affected,
            "statements": outcome.statements,
        }))
    }
}

fn annotate(out: &mut Value, instance: &str) {
    if let Some(obj) = out.as_object_mut() {
        obj.insert("instance".to_string(), json!(instance));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deliver_inlines_small_results() {
        let qr = QueryResult {
            columns: vec!["id".into(), "name".into()],
            rows: vec![vec![json!(1), json!("a")], vec![json!(2), json!("b")]],
            truncated: false,
        };
        let out = deliver("database.query", qr).unwrap();
        assert_eq!(out["kind"], "rows");
        assert_eq!(out["row_count"], 2);
        assert_eq!(out["truncated"], false);
        assert_eq!(out["columns"], json!(["id", "name"]));
    }

    #[test]
    fn deliver_marks_truncation() {
        let qr = QueryResult {
            columns: vec!["x".into()],
            rows: vec![vec![json!(1)]],
            truncated: true,
        };
        let out = deliver("database.query", qr).unwrap();
        assert_eq!(out["truncated"], true);
    }

    #[test]
    fn sql_arg_trims_and_requires_non_empty() {
        assert_eq!(sql_arg(&json!({"sql": "SELECT 1;"})).unwrap(), "SELECT 1");
        assert_eq!(
            sql_arg(&json!({"sql": "  SELECT 1 ; "})).unwrap(),
            "SELECT 1"
        );
        assert!(sql_arg(&json!({"sql": "  "})).is_err());
        assert!(sql_arg(&json!({"sql": ";"})).is_err());
        assert!(sql_arg(&json!({})).is_err());
    }

    #[test]
    fn ident_arg_rejects_injection_chars() {
        assert_eq!(
            ident_arg(&json!({"schema": "public"}), "schema").unwrap(),
            Some("public")
        );
        assert_eq!(ident_arg(&json!({}), "schema").unwrap(), None);
        assert!(ident_arg(&json!({"table": "x'; DROP TABLE t"}), "table").is_err());
        assert!(ident_arg(&json!({"table": "a\\b"}), "table").is_err());
    }

    #[test]
    fn instance_arg_filters_empty() {
        assert_eq!(instance_arg(&json!({"instance": "pw"})), Some("pw"));
        assert_eq!(instance_arg(&json!({"instance": ""})), None);
        assert_eq!(instance_arg(&json!({})), None);
    }

    /// When nothing is configured, `list_instances` is the one tool the
    /// agent reaches — so it must return the correct, actionable setup
    /// procedure rather than a bare empty list (the gap that left the
    /// agent confabulating `config.toml`).
    #[tokio::test]
    async fn list_instances_empty_returns_setup_guidance() {
        struct EmptyStore;
        impl workforce_common::settings::SecretStore for EmptyStore {
            fn get(&self, _: &str) -> Option<String> {
                None
            }
            fn set(&self, _: &str, _: &str) -> eyre::Result<()> {
                Ok(())
            }
            fn delete(&self, _: &str) -> eyre::Result<bool> {
                Ok(false)
            }
            fn list(&self, _: Option<&str>) -> Vec<String> {
                Vec::new()
            }
        }

        let rt = DatabaseRuntime::new(Arc::new(EmptyStore));
        let out = ListInstancesTool::new(rt).run(json!({})).await.unwrap();

        assert_eq!(out["count"], 0);
        let setup = out["setup"].as_str().expect("setup guidance present");
        // Points at the real mechanism (operator vault store on the host)…
        assert!(setup.contains("workforce vault set databases/instances"));
        assert!(setup.contains("agent cannot"));
        // …and away from the wrong mechanisms the agent invented.
        assert!(!setup.contains("config.toml"));
    }
}
