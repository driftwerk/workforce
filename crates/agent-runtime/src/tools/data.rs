//! `data.query` / `data.inspect` — the data plane's compute layer.
//!
//! `data.query` runs one read-only SQLite statement over staged tabular
//! artifacts (handle → handle: results above the inline threshold are staged
//! right back), so cross-source joins, filters, and aggregations never pass
//! rows through the model. `data.inspect` answers "what shape is this
//! dataset?" with schema, per-column stats, and a wrapped sample. Resolution
//! and session enforcement happen on the async side; ingest + query run in
//! `spawn_blocking` with the session context re-entered for result staging.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::{eyre, Result};
use serde_json::{json, Map, Value};

use super::{Tool, ToolResult};
use crate::artifacts::resolve::{resolve_table_artifact, ResolvedTable};
use crate::artifacts::store::{global_store, ArtifactStore, DEFAULT_INLINE_THRESHOLD};
use crate::artifacts::tabular::{stage_table, staged_to_output, StagedTable};
use crate::artifacts::ArtifactId;
use crate::data::{
    estimate_json_bytes, run_query, TableInput, MAX_INPUTS, MAX_INPUT_BYTES, MAX_RESULT_BYTES,
    MAX_RESULT_ROWS, QUERY_TIMEOUT,
};
use crate::runner::{SessionContext, CURRENT_AGENT_ID};
use crate::security::external_content::wrap_external_content;

pub const DATA_QUERY_TOOL_NAME: &str = "data.query";
pub const DATA_INSPECT_TOOL_NAME: &str = "data.inspect";

const STATS_ROW_CAP: usize = 100_000;
const DISTINCT_CAP: usize = 10_000;
const DEFAULT_SAMPLE_ROWS: usize = 5;
const MAX_SAMPLE_ROWS: usize = 20;

const QUERY_DESCRIPTION: &str = "Run ONE read-only SQLite SELECT over staged tabular artifacts \
    (the `tabular_artifact` envelopes other tools return). `inputs` maps table aliases to \
    artifact ids: {\"a\": \"art_...\"} makes the data queryable as `a`. Use this to filter, \
    join, aggregate, dedupe, or reshape datasets WITHOUT pulling rows into your context — \
    e.g. consolidate two exports with a JOIN/UNION, or cross-reference an uploaded list \
    against a query result. Results stay out of context too: small results return inline, \
    large ones return a new tabular_artifact envelope you can chain into another data.query \
    or export_xlsx. To reason about a big dataset, probe with small-result queries \
    (GROUP BY counts, LIMIT 20 peeks) instead of reading it. Prefer filtering at the source \
    (SQL/query parameters of the producing tool) when possible; use this for what the source \
    can't do. SQLite dialect: booleans are 0/1, nested JSON cells query via json_extract, \
    quote \"column names\" with spaces. One statement only; writes/PRAGMA/ATTACH are blocked.";

const INSPECT_DESCRIPTION: &str = "Inspect a staged tabular artifact without reading it: \
    returns row count, columns with per-column stats (type, nulls, distinct count, min/max), \
    and a small sample. Use this to understand a dataset's shape before writing a data.query \
    or export_xlsx layout. Never page through a staged dataset to learn its shape — this tool \
    answers that in one call.";

pub struct DataQueryTool;

#[async_trait]
impl Tool for DataQueryTool {
    fn name(&self) -> &str {
        DATA_QUERY_TOOL_NAME
    }

    fn description(&self) -> &str {
        QUERY_DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "sql": {
                    "type": "string",
                    "description": "One read-only SQLite SELECT. Input tables are available under their alias names."
                },
                "inputs": {
                    "type": "object",
                    "description": format!("Alias → artifact_id map (1-{MAX_INPUTS} entries). Aliases must match [A-Za-z_][A-Za-z0-9_]* and become SQL table names."),
                    "additionalProperties": { "type": "string" }
                }
            },
            "required": ["sql", "inputs"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = |t: Instant| t.elapsed().as_millis() as u64;
        match query_impl(&input).await {
            Ok(output) => Ok(ToolResult::success(output, elapsed(start))),
            Err(e) => Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        }
    }
}

async fn query_impl(input: &Value) -> Result<Value> {
    let sql = input["sql"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| eyre!("`sql` is required"))?
        .to_string();
    let inputs_map = input["inputs"]
        .as_object()
        .filter(|m| !m.is_empty())
        .ok_or_else(|| eyre!("`inputs` must map at least one alias to an artifact_id"))?;

    let (store, agent_id) = store_and_agent()?;
    let mut tables = Vec::with_capacity(inputs_map.len());
    let mut total_bytes = 0usize;
    for (alias, id_value) in inputs_map {
        let id_str = id_value
            .as_str()
            .ok_or_else(|| eyre!("inputs.{alias} must be an artifact_id string"))?;
        let id = ArtifactId::parse(id_str)?;
        let ResolvedTable { rows, columns, .. } =
            resolve_table_artifact(store.as_ref(), &agent_id, &id)
                .map_err(|e| eyre!("input `{alias}`: {e}"))?;
        total_bytes += rows.iter().map(estimate_json_bytes).sum::<usize>();
        if total_bytes > MAX_INPUT_BYTES {
            return Err(eyre!(
                "combined inputs exceed {} MB",
                MAX_INPUT_BYTES / (1024 * 1024)
            ));
        }
        tables.push(TableInput {
            alias: alias.clone(),
            columns,
            rows,
        });
    }

    let ctx = SessionContext::capture();
    let staged = tokio::task::spawn_blocking(move || {
        ctx.sync_scope(|| -> Result<StagedTable> {
            let result = run_query(
                tables,
                &sql,
                QUERY_TIMEOUT,
                MAX_RESULT_ROWS,
                MAX_RESULT_BYTES,
            )?;
            stage_table(
                store.as_ref(),
                &agent_id,
                DATA_QUERY_TOOL_NAME,
                Some(result.columns),
                result.rows,
                DEFAULT_INLINE_THRESHOLD,
            )
        })
    })
    .await
    .map_err(|e| eyre!("query task failed: {e}"))??;

    Ok(staged_to_output(staged, DATA_QUERY_TOOL_NAME))
}

pub struct DataInspectTool;

#[async_trait]
impl Tool for DataInspectTool {
    fn name(&self) -> &str {
        DATA_INSPECT_TOOL_NAME
    }

    fn description(&self) -> &str {
        INSPECT_DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "artifact_id": {
                    "type": "string",
                    "description": "Id of a staged tabular artifact (art_...)."
                },
                "sample_rows": {
                    "type": "integer",
                    "description": format!("Sample rows to include (0-{MAX_SAMPLE_ROWS}, default {DEFAULT_SAMPLE_ROWS}).")
                }
            },
            "required": ["artifact_id"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = |t: Instant| t.elapsed().as_millis() as u64;
        match inspect_impl(&input) {
            Ok(output) => Ok(ToolResult::success(output, elapsed(start))),
            Err(e) => Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        }
    }
}

fn inspect_impl(input: &Value) -> Result<Value> {
    let id_str = input["artifact_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| eyre!("`artifact_id` is required"))?;
    let sample_rows = input["sample_rows"]
        .as_u64()
        .map(|n| (n as usize).min(MAX_SAMPLE_ROWS))
        .unwrap_or(DEFAULT_SAMPLE_ROWS);

    let (store, agent_id) = store_and_agent()?;
    let id = ArtifactId::parse(id_str)?;
    let ResolvedTable { rows, columns, .. } =
        resolve_table_artifact(store.as_ref(), &agent_id, &id)?;

    let scanned = rows.len().min(STATS_ROW_CAP);
    let stats = column_stats(&rows[..scanned], &columns);
    let sample = &rows[..rows.len().min(sample_rows)];
    let rendered = serde_json::to_string(sample).unwrap_or_default();
    let wrapped = wrap_external_content(&rendered, DATA_INSPECT_TOOL_NAME).wrapped_content;

    Ok(json!({
        "artifact_id": id.as_str(),
        "rows": rows.len(),
        "columns": stats,
        "sample_wrapped": wrapped,
        "stats_sampled": scanned < rows.len(),
        "note": "Query this dataset with data.query (alias it in `inputs`) or export it with export_xlsx(artifact_id=...).",
    }))
}

fn store_and_agent() -> Result<(Arc<dyn ArtifactStore>, String)> {
    let store = global_store().ok_or_else(|| eyre!("artifact store is not configured"))?;
    let agent_id = CURRENT_AGENT_ID
        .try_with(|a| a.clone())
        .ok()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| eyre!("no agent context in scope"))?;
    Ok((store, agent_id))
}

#[derive(Default)]
struct ColumnAccumulator {
    nulls: usize,
    types: HashMap<&'static str, usize>,
    distinct: std::collections::HashSet<String>,
    distinct_capped: bool,
    num_min: Option<f64>,
    num_max: Option<f64>,
    text_min: Option<String>,
    text_max: Option<String>,
}

impl ColumnAccumulator {
    fn observe(&mut self, value: &Value) {
        let kind = match value {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        };
        *self.types.entry(kind).or_default() += 1;
        if value.is_null() {
            self.nulls += 1;
            return;
        }
        let rendered = value.to_string();
        if self.distinct.len() < DISTINCT_CAP {
            self.distinct.insert(rendered);
        } else if !self.distinct.contains(&rendered) {
            self.distinct_capped = true;
        }
        match value {
            Value::Number(n) => {
                if let Some(f) = n.as_f64() {
                    self.num_min = Some(self.num_min.map_or(f, |m| m.min(f)));
                    self.num_max = Some(self.num_max.map_or(f, |m| m.max(f)));
                }
            }
            Value::String(s) => {
                if self.text_min.as_deref().is_none_or(|m| s.as_str() < m) {
                    self.text_min = Some(s.clone());
                }
                if self.text_max.as_deref().is_none_or(|m| s.as_str() > m) {
                    self.text_max = Some(s.clone());
                }
            }
            _ => {}
        }
    }

    fn into_json(self, name: &str, total: usize) -> Value {
        let dominant = self
            .types
            .iter()
            .filter(|(kind, _)| **kind != "null")
            .max_by_key(|(_, count)| **count)
            .map(|(kind, _)| *kind)
            .unwrap_or("null");
        let mixed = self.types.keys().filter(|k| **k != "null").count() > 1;
        let mut out = Map::new();
        out.insert("name".into(), json!(name));
        out.insert("type".into(), json!(dominant));
        if mixed {
            out.insert("mixed".into(), json!(true));
        }
        out.insert("nulls".into(), json!(self.nulls));
        out.insert("distinct".into(), json!(self.distinct.len()));
        if self.distinct_capped {
            out.insert("distinct_capped".into(), json!(true));
        }
        match dominant {
            "number" => {
                out.insert("min".into(), json!(self.num_min));
                out.insert("max".into(), json!(self.num_max));
            }
            "string" => {
                out.insert("min".into(), json!(self.text_min));
                out.insert("max".into(), json!(self.text_max));
            }
            _ => {}
        }
        out.insert("present".into(), json!(total - self.nulls));
        Value::Object(out)
    }
}

fn column_stats(rows: &[Value], columns: &[String]) -> Vec<Value> {
    let mut accumulators: Vec<ColumnAccumulator> = columns
        .iter()
        .map(|_| ColumnAccumulator::default())
        .collect();
    for row in rows {
        for (column, acc) in columns.iter().zip(accumulators.iter_mut()) {
            acc.observe(row.get(column).unwrap_or(&Value::Null));
        }
    }
    columns
        .iter()
        .zip(accumulators)
        .map(|(name, acc)| acc.into_json(name, rows.len()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::store::{install_test_global_store, FsArtifactStore};
    use crate::artifacts::types::{ArtifactBody, ArtifactDescriptor, ContentType};
    use crate::runner::{CURRENT_AGENT_ID, CURRENT_SESSION_ID};
    use serde_json::json;
    use tempfile::TempDir;

    fn write_table(
        store: &dyn ArtifactStore,
        agent: &str,
        session: Option<&str>,
        rows: Vec<Value>,
    ) -> ArtifactId {
        store
            .write(
                agent,
                ArtifactDescriptor {
                    schema: Some(vec!["id".into(), "name".into()]),
                    row_count: Some(rows.len()),
                    session_id: session.map(str::to_string),
                    ..ArtifactDescriptor::from_context("test", ContentType::ApplicationJson, agent)
                },
                ArtifactBody::Json(Value::Array(rows)),
            )
            .unwrap()
            .id
    }

    async fn execute_scoped(agent: &str, session: &str, input: Value) -> crate::tools::ToolResult {
        CURRENT_AGENT_ID
            .scope(
                agent.to_string(),
                CURRENT_SESSION_ID.scope(session.to_string(), DataQueryTool.execute(input)),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn data_query_stages_large_results_with_session_stamp() {
        let store = install_test_global_store();
        let agent = "data-exec-agent";
        let session = "data-exec-session";
        let rows: Vec<Value> = (0..600)
            .map(|i| json!({"id": i, "name": format!("row-{i}-padding-padding-padding")}))
            .collect();
        let input_id = write_table(store.as_ref(), agent, Some(session), rows);

        let result = execute_scoped(
            agent,
            session,
            json!({
                "sql": "SELECT id, name FROM t ORDER BY id",
                "inputs": { "t": input_id.as_str() },
            }),
        )
        .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.output["kind"], "tabular_artifact");
        assert!(
            result.output["preview"].is_string(),
            "staged preview must be injection-wrapped"
        );

        let out_id = ArtifactId::parse(result.output["artifact_id"].as_str().unwrap()).unwrap();
        let descriptor = store.metadata(agent, &out_id).unwrap().unwrap();
        assert_eq!(
            descriptor.session_id.as_deref(),
            Some(session),
            "staged result must carry the ambient session (sync_scope inside spawn_blocking)"
        );
        assert_eq!(descriptor.producer, DATA_QUERY_TOOL_NAME);
        let ArtifactBody::Json(Value::Array(stored)) = store.read(agent, &out_id).unwrap() else {
            panic!("expected canonical rows");
        };
        assert_eq!(stored.len(), 600);
        assert_eq!(stored[0]["id"], 0);
    }

    #[tokio::test]
    async fn data_query_small_results_inline_and_wrapped() {
        let store = install_test_global_store();
        let agent = "data-exec-agent-inline";
        let session = "data-exec-session-inline";
        let rows: Vec<Value> = (0..50).map(|i| json!({"id": i, "name": "x"})).collect();
        let input_id = write_table(store.as_ref(), agent, Some(session), rows);

        let result = execute_scoped(
            agent,
            session,
            json!({
                "sql": "SELECT count(*) AS n, max(id) AS top FROM t",
                "inputs": { "t": input_id.as_str() },
            }),
        )
        .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.output["kind"], "rows");
        assert_eq!(result.output["row_count"], 1);
        assert_eq!(result.output["columns"], json!(["n", "top"]));
        let wrapped = result.output["rows_wrapped"].as_str().unwrap();
        assert!(wrapped.contains("49"));
        assert!(wrapped.contains(DATA_QUERY_TOOL_NAME));
    }

    #[tokio::test]
    async fn data_query_denies_cross_session_inputs() {
        let store = install_test_global_store();
        let agent = "data-exec-agent-deny";
        let input_id = write_table(
            store.as_ref(),
            agent,
            Some("session-owner"),
            vec![json!({"id": 1, "name": "secret"})],
        );

        let result = execute_scoped(
            agent,
            "session-intruder",
            json!({
                "sql": "SELECT * FROM t",
                "inputs": { "t": input_id.as_str() },
            }),
        )
        .await;
        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(err.contains("not found or expired"), "got: {err}");
        assert!(err.contains("input `t`"), "error names the input: {err}");
    }

    fn stats_for(rows: Vec<Value>, columns: &[&str]) -> Vec<Value> {
        let columns: Vec<String> = columns.iter().map(|s| s.to_string()).collect();
        column_stats(&rows, &columns)
    }

    #[test]
    fn column_stats_counts_nulls_types_and_ranges() {
        let rows = vec![
            json!({"n": 1, "s": "b", "m": 1}),
            json!({"n": 5.5, "s": "a", "m": "x"}),
            json!({"n": null, "s": "c"}),
        ];
        let stats = stats_for(rows, &["n", "s", "m"]);
        assert_eq!(stats[0]["type"], "number");
        assert_eq!(stats[0]["nulls"], 1);
        assert_eq!(stats[0]["distinct"], 2);
        assert_eq!(stats[0]["min"], 1.0);
        assert_eq!(stats[0]["max"], 5.5);
        assert_eq!(stats[1]["type"], "string");
        assert_eq!(stats[1]["min"], "a");
        assert_eq!(stats[1]["max"], "c");
        assert_eq!(stats[2]["mixed"], true);
        assert_eq!(stats[2]["nulls"], 1, "missing key counts as null");
    }

    #[test]
    fn inspect_reports_shape_and_sample() {
        let dir = TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let rows: Vec<Value> = (0..50)
            .map(|i| json!({"id": i, "status": "Unfilled"}))
            .collect();
        let id = store
            .write(
                "agent-a",
                ArtifactDescriptor {
                    schema: Some(vec!["id".into(), "status".into()]),
                    row_count: Some(50),
                    ..ArtifactDescriptor::from_context(
                        "test",
                        ContentType::ApplicationJson,
                        "agent-a",
                    )
                },
                ArtifactBody::Json(Value::Array(rows)),
            )
            .unwrap()
            .id;
        let stats = {
            let table = resolve_table_artifact(store.as_ref(), "agent-a", &id).unwrap();
            column_stats(&table.rows, &table.columns)
        };
        assert_eq!(stats[0]["name"], "id");
        assert_eq!(stats[0]["distinct"], 50);
        assert_eq!(stats[1]["distinct"], 1);
    }
}
