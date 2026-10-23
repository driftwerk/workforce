//! Datadog agent-callable tools.
//!
//! Each tool accepts an optional `account` parameter that selects which
//! Datadog account/region to use. Time-range parameters accept Unix epoch
//! seconds, ISO-8601 strings, or relative shorthands like `now-1h`.
//!
//! User-authored content (monitor names/messages, dashboard titles, log
//! messages, event text, incident summaries) is passed through
//! `wrap_external_content` so prompt-injection attempts in Datadog data
//! can't escape the security boundary.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use eyre::Result;
use serde_json::{json, Value};
use tracing::{info, warn};

use workforce_agent::artifacts::{
    global_store, stage_table, staged_to_output, ArtifactBody, ArtifactDescriptor, ContentType,
    DEFAULT_INLINE_THRESHOLD,
};
use workforce_agent::data::{estimate_json_bytes, MAX_RESULT_BYTES};
use workforce_agent::export::{
    build_xlsx, xlsx_filename, Cell, ColFormat, Column, Sheet, Table, XLSX_MIME,
};
use workforce_agent::runner::CURRENT_AGENT_ID;
use workforce_agent::security::external_content::wrap_external_content;
use workforce_agent::tools::image_gen::GENERATED_IMAGES_KEY;
use workforce_agent::tools::{Tool, ToolResult};

use crate::adapters::utils::{coerce_to_i64, coerce_to_u64, instance_choices};

use super::client::DatadogClient;

// ──────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────

/// Wrap a JSON string field with security boundary markers if it contains content.
fn wrap_field(value: &Value, source: &str) -> Value {
    match value.as_str() {
        Some(s) if !s.is_empty() => Value::String(wrap_external_content(s, source).wrapped_content),
        _ => value.clone(),
    }
}

/// Resolve which Datadog account to use for a given tool input.
///
/// Precedence:
/// 1. Explicit `account` parameter (errors if unknown).
/// 2. Fall back to `default_account`.
fn resolve_client(
    clients: &HashMap<String, Arc<DatadogClient>>,
    default_account: &str,
    input: &Value,
) -> Result<Arc<DatadogClient>> {
    let account = workforce_agent::llm::environment::resolve_instance(
        "datadog",
        input.get("account").and_then(|v| v.as_str()),
        default_account,
        clients,
    )
    .map_err(|d| eyre::eyre!("{}", d.message))?;
    clients.get(&account).cloned().ok_or_else(|| {
        eyre::eyre!(
            "Unknown Datadog account: '{account}'. Available: {:?}",
            clients.keys().collect::<Vec<_>>()
        )
    })
}

/// Build a dynamic description for the `account` parameter, listing the
/// available accounts so the LLM knows what's valid.
fn account_description(
    clients: &HashMap<String, Arc<DatadogClient>>,
    default_account: &str,
) -> String {
    if clients.len() <= 1 {
        return "Datadog account name. Uses the default if omitted.".to_string();
    }
    format!(
        "Datadog account name. {}",
        instance_choices(clients, default_account, "account")
    )
}

/// Required string field extractor with a clean error message.
fn required_str<'a>(input: &'a Value, key: &str) -> std::result::Result<&'a str, String> {
    input
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("Missing required parameter '{key}'"))
}

/// Parse a time-range value into Unix epoch seconds.
///
/// Accepts:
/// - Integer / float Unix epoch seconds
/// - Numeric string ("1700000000")
/// - Relative shorthand: "now", "now-15m", "now-1h", "now-2d", "now-1w"
///
/// ISO-8601 strings are NOT converted here — callers that need ISO (e.g.
/// v2 logs search) should use [`parse_time_string`] which preserves them.
fn parse_time_seconds(value: &Value) -> std::result::Result<i64, String> {
    if let Some(n) = coerce_to_i64(value) {
        return Ok(n);
    }
    let Some(s) = value.as_str() else {
        return Err("Expected epoch seconds or relative time string".to_string());
    };
    parse_relative_time(s)
}

/// Parse a time-range value for endpoints that accept ISO-8601 directly
/// (Datadog v2 logs/events search). Returns the original string for ISO
/// inputs, otherwise an integer epoch seconds string.
fn parse_time_string(value: &Value) -> std::result::Result<String, String> {
    if let Some(n) = coerce_to_i64(value) {
        return Ok(n.to_string());
    }
    let Some(s) = value.as_str() else {
        return Err("Expected epoch seconds, ISO-8601, or relative time string".to_string());
    };
    if s.starts_with("now") {
        return parse_relative_time(s).map(|n| n.to_string());
    }
    Ok(s.to_string())
}

/// Resolve a relative time string like `now-1h` against the current clock.
fn parse_relative_time(s: &str) -> std::result::Result<i64, String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .map_err(|e| format!("System clock error: {e}"))?;

    let trimmed = s.trim();
    if trimmed == "now" {
        return Ok(now);
    }
    if let Some(rest) = trimmed.strip_prefix("now-") {
        let offset = parse_duration_seconds(rest)?;
        return Ok(now - offset);
    }
    if let Some(rest) = trimmed.strip_prefix("now+") {
        let offset = parse_duration_seconds(rest)?;
        return Ok(now + offset);
    }
    // Last resort: try parsing as a bare integer string.
    trimmed
        .parse::<i64>()
        .map_err(|_| format!("Unrecognised time value: '{s}'"))
}

/// Parse a duration shorthand like `15m`, `1h`, `2d`, `1w` into seconds.
fn parse_duration_seconds(s: &str) -> std::result::Result<i64, String> {
    let trimmed = s.trim();
    if trimmed.len() < 2 {
        return Err(format!("Invalid duration: '{s}'"));
    }
    let (num_str, unit) = trimmed.split_at(trimmed.len() - 1);
    let n: i64 = num_str
        .parse()
        .map_err(|_| format!("Invalid duration number in '{s}'"))?;
    let multiplier: i64 = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 60 * 60 * 24,
        "w" => 60 * 60 * 24 * 7,
        other => return Err(format!("Unknown duration unit: '{other}'")),
    };
    Ok(n * multiplier)
}

macro_rules! resolve_or_fail {
    ($self:ident, $input:ident, $start:ident) => {{
        match resolve_client(&$self.clients, &$self.default_account, &$input) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("{e}"),
                    $start.elapsed().as_millis() as u64,
                ))
            }
        }
    }};
}

macro_rules! dd_tool_struct {
    ($name:ident) => {
        pub struct $name {
            clients: Arc<HashMap<String, Arc<DatadogClient>>>,
            default_account: String,
        }

        impl $name {
            pub fn new(
                clients: Arc<HashMap<String, Arc<DatadogClient>>>,
                default_account: String,
            ) -> Self {
                Self {
                    clients,
                    default_account,
                }
            }
        }
    };
}

// ──────────────────────────────────────────────────────────────────────
// Metrics
// ──────────────────────────────────────────────────────────────────────

dd_tool_struct!(DatadogMetricsQueryTool);

#[async_trait]
impl Tool for DatadogMetricsQueryTool {
    fn name(&self) -> &str {
        "datadog.metrics_query"
    }
    fn description(&self) -> &str {
        "Run a Datadog metrics timeseries query (v2). Returns one or more series with timestamps and values."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "query": { "type": "string", "description": "Datadog query, e.g. 'avg:system.cpu.user{*} by {host}'" },
                "from": { "description": "Start time (epoch seconds, ISO-8601, or 'now-1h')" },
                "to": { "description": "End time (epoch seconds, ISO-8601, or 'now')" }
            },
            "required": ["query", "from", "to"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let query = match required_str(&input, "query") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let from = match parse_time_seconds(&input["from"]) {
            Ok(n) => n,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let to = match parse_time_seconds(&input["to"]) {
            Ok(n) => n,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let body = json!({
            "data": {
                "type": "timeseries_request",
                "attributes": {
                    "from": from * 1000,
                    "to": to * 1000,
                    "queries": [{ "name": "a", "data_source": "metrics", "query": query }]
                }
            }
        });
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.post("/api/v2/query/timeseries", &body).await {
            Ok(resp) => Ok(ToolResult::success(resp, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog metrics query failed"),
                duration_ms,
            )),
        }
    }
}

dd_tool_struct!(DatadogMetricsScalarTool);

#[async_trait]
impl Tool for DatadogMetricsScalarTool {
    fn name(&self) -> &str {
        "datadog.metrics_scalar"
    }
    fn description(&self) -> &str {
        "Run a Datadog scalar metrics query (v2). Returns a single value per group."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "query": { "type": "string", "description": "Datadog query, e.g. 'avg:system.load.5{*}'" },
                "from": { "description": "Start time" },
                "to": { "description": "End time" },
                "aggregator": { "type": "string", "description": "Aggregator (avg/sum/min/max/last). Default: avg" }
            },
            "required": ["query", "from", "to"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let query = match required_str(&input, "query") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let from = match parse_time_seconds(&input["from"]) {
            Ok(n) => n,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let to = match parse_time_seconds(&input["to"]) {
            Ok(n) => n,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let aggregator = input
            .get("aggregator")
            .and_then(|v| v.as_str())
            .unwrap_or("avg");
        let body = json!({
            "data": {
                "type": "scalar_request",
                "attributes": {
                    "from": from * 1000,
                    "to": to * 1000,
                    "queries": [{
                        "name": "a",
                        "data_source": "metrics",
                        "query": query,
                        "aggregator": aggregator
                    }]
                }
            }
        });
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.post("/api/v2/query/scalar", &body).await {
            Ok(resp) => Ok(ToolResult::success(resp, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog scalar query failed"),
                duration_ms,
            )),
        }
    }
}

dd_tool_struct!(DatadogMetricsListTool);

#[async_trait]
impl Tool for DatadogMetricsListTool {
    fn name(&self) -> &str {
        "datadog.metrics_list"
    }
    fn description(&self) -> &str {
        "List metric names that have reported data since a given time."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "from": { "description": "Earliest seen time (epoch seconds or relative). Default: now-1h" }
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let from = parse_time_seconds(&input["from"]).unwrap_or_else(|_| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64 - 3600)
                .unwrap_or(0)
        });
        let from_str = from.to_string();
        let duration_ms = start.elapsed().as_millis() as u64;
        match client
            .get_with_query("/api/v1/metrics", &[("from", from_str.as_str())])
            .await
        {
            Ok(resp) => Ok(ToolResult::success(resp, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog metrics list failed"),
                duration_ms,
            )),
        }
    }
}

dd_tool_struct!(DatadogMetricsMetadataTool);

#[async_trait]
impl Tool for DatadogMetricsMetadataTool {
    fn name(&self) -> &str {
        "datadog.metrics_metadata"
    }
    fn description(&self) -> &str {
        "Get metadata (unit, type, description) for a single Datadog metric."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "metric_name": { "type": "string" }
            },
            "required": ["metric_name"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let name = match required_str(&input, "metric_name") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let path = format!("/api/v1/metrics/{name}");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.get(&path).await {
            Ok(resp) => {
                let out = json!({
                    "name": resp["name"],
                    "type": resp["type"],
                    "unit": resp["unit"],
                    "per_unit": resp["per_unit"],
                    "description": wrap_field(&resp["description"], "datadog.metrics_metadata.description"),
                    "short_name": resp["short_name"],
                    "integration": resp["integration"],
                });
                Ok(ToolResult::success(out, duration_ms))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog metric metadata failed"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Logs
// ──────────────────────────────────────────────────────────────────────

dd_tool_struct!(DatadogLogsSearchTool);

#[async_trait]
impl Tool for DatadogLogsSearchTool {
    fn name(&self) -> &str {
        "datadog.logs_search"
    }
    fn description(&self) -> &str {
        "Search Datadog logs (v2). Supports cursor pagination via the `cursor` parameter."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "query": { "type": "string", "description": "Datadog log query, e.g. 'service:web status:error'" },
                "from": { "description": "Start time (epoch, ISO-8601, or 'now-15m'). Default: now-15m" },
                "to": { "description": "End time. Default: now" },
                "limit": { "type": "integer", "description": "Page size (default 50, max 1000)" },
                "cursor": { "type": "string", "description": "Pagination cursor from a previous response" }
            },
            "required": ["query"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let query = match required_str(&input, "query") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let from = parse_time_string(&input["from"]).unwrap_or_else(|_| "now-15m".to_string());
        let to = parse_time_string(&input["to"]).unwrap_or_else(|_| "now".to_string());
        let limit = coerce_to_u64(&input["limit"]).unwrap_or(50).min(1000);

        let mut body = json!({
            "filter": { "query": query, "from": from, "to": to },
            "page": { "limit": limit }
        });
        if let Some(cursor) = input.get("cursor").and_then(|v| v.as_str()) {
            body["page"]["cursor"] = json!(cursor);
        }

        let duration_ms = start.elapsed().as_millis() as u64;
        match client.post("/api/v2/logs/events/search", &body).await {
            Ok(resp) => {
                let logs: Vec<Value> = resp
                    .get("data")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|l| {
                        let attrs = &l["attributes"];
                        json!({
                            "id": l["id"],
                            "timestamp": attrs["timestamp"],
                            "host": attrs["host"],
                            "service": attrs["service"],
                            "status": attrs["status"],
                            "message": wrap_field(&attrs["message"], "datadog.logs_search.message"),
                            "tags": attrs["tags"],
                        })
                    })
                    .collect();
                info!(count = logs.len(), "Datadog logs search");
                Ok(ToolResult::success(
                    json!({
                        "count": logs.len(),
                        "logs": logs,
                        "cursor": resp.pointer("/meta/page/after"),
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                warn!(error = %e, "Datadog logs search failed");
                Ok(ToolResult::from_report(
                    &e.wrap_err("Datadog logs search failed"),
                    duration_ms,
                ))
            }
        }
    }
}

const LOGS_EXPORT_PAGE_LIMIT: u64 = 1000;
const LOGS_EXPORT_DEFAULT_MAX: usize = 50_000;
const LOGS_EXPORT_HARD_MAX: usize = 250_000;
const LOGS_EXPORT_TOOL_NAME: &str = "datadog.logs_export";
const LOG_EXPORT_COLUMNS: &[&str] = &[
    "timestamp",
    "status",
    "service",
    "host",
    "message",
    "tags",
    "id",
];

/// Slim one Datadog log event to the export row shape. Values stay RAW —
/// injection wrapping happens at the preview/inline boundary, and wrapped
/// markers inside an .xlsx cell would be junk.
fn slim_log_row(log: &Value) -> Value {
    let attrs = &log["attributes"];
    json!({
        "timestamp": attrs["timestamp"],
        "status": attrs["status"],
        "service": attrs["service"],
        "host": attrs["host"],
        "message": attrs["message"],
        "tags": attrs["tags"],
        "id": log["id"],
    })
}

/// Continue/stop decision after one page: keep following the cursor only
/// while pages are non-empty and the record cap is unreached.
enum PageStep {
    Continue(String),
    Stop { capped: bool },
}

fn next_step(rows_len: usize, got: usize, next: Option<String>, max_records: usize) -> PageStep {
    match next {
        Some(cursor) if got > 0 => {
            if rows_len >= max_records {
                PageStep::Stop { capped: true }
            } else {
                PageStep::Continue(cursor)
            }
        }
        _ => PageStep::Stop { capped: false },
    }
}

/// Extract one search response page: slim rows + the next cursor.
fn log_page(resp: &Value) -> (Vec<Value>, Option<String>) {
    let rows = resp
        .get("data")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(slim_log_row).collect())
        .unwrap_or_default();
    let cursor = resp
        .pointer("/meta/page/after")
        .and_then(Value::as_str)
        .map(str::to_string);
    (rows, cursor)
}

fn logs_export_table(rows: &[Value]) -> Table {
    let columns = LOG_EXPORT_COLUMNS
        .iter()
        .map(|c| Column::new(*c, *c, ColFormat::General))
        .collect();
    let cells = rows
        .iter()
        .map(|row| {
            LOG_EXPORT_COLUMNS
                .iter()
                .map(|c| Cell::from_json(row.get(*c).unwrap_or(&Value::Null)))
                .collect()
        })
        .collect();
    Table {
        columns,
        rows: cells,
    }
}

dd_tool_struct!(DatadogLogsExportTool);

#[async_trait]
impl Tool for DatadogLogsExportTool {
    fn name(&self) -> &str {
        LOGS_EXPORT_TOOL_NAME
    }
    fn description(&self) -> &str {
        "Bulk-export Datadog logs server-side: fetches EVERY matching log in the window \
         (cursor pagination handled internally — never page logs_search yourself for bulk \
         work), then either delivers an .xlsx to this conversation (format \"xlsx\", default) \
         or stages the rows as a tabular_artifact (format \"artifact\") for data.query / \
         export_xlsx chaining. Push filters into `query` (e.g. service:web status:error) — \
         the rows do NOT enter your context, so don't re-fetch or verify them."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "query": { "type": "string", "description": "Datadog log query (pushdown filter), e.g. 'service:web status:error'" },
                "from": { "description": "Start time (epoch, ISO-8601, or 'now-24h'). REQUIRED — bound the window explicitly." },
                "to": { "description": "End time. Default: now" },
                "max_records": { "type": "integer", "description": "Cap on exported logs (default 50000, max 250000)." },
                "format": { "type": "string", "enum": ["xlsx", "artifact"], "description": "xlsx (default) delivers a spreadsheet; artifact stages a tabular_artifact for data.query." }
            },
            "required": ["query", "from"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let elapsed = |t: std::time::Instant| t.elapsed().as_millis() as u64;
        let client = resolve_or_fail!(self, input, start);
        let query = match required_str(&input, "query") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, elapsed(start))),
        };
        if input.get("from").is_none_or(Value::is_null) {
            return Ok(ToolResult::failure(
                "`from` is required (e.g. \"now-24h\") — bound the export window explicitly",
                elapsed(start),
            ));
        }
        let from = match parse_time_string(&input["from"]) {
            Ok(s) => s,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("invalid `from`: {e}"),
                    elapsed(start),
                ))
            }
        };
        let to = parse_time_string(&input["to"]).unwrap_or_else(|_| "now".to_string());
        let max_records = coerce_to_u64(&input["max_records"])
            .map(|n| n as usize)
            .filter(|n| *n > 0)
            .unwrap_or(LOGS_EXPORT_DEFAULT_MAX)
            .min(LOGS_EXPORT_HARD_MAX);
        let format = input["format"].as_str().unwrap_or("xlsx");
        if !matches!(format, "xlsx" | "artifact") {
            return Ok(ToolResult::failure(
                "`format` must be \"xlsx\" or \"artifact\"",
                elapsed(start),
            ));
        }
        let agent_id = CURRENT_AGENT_ID
            .try_with(|v| v.clone())
            .ok()
            .filter(|s| !s.is_empty());
        let Some(agent_id) = agent_id else {
            return Ok(ToolResult::failure(
                "no agent context in scope; cannot stage the export",
                elapsed(start),
            ));
        };
        let Some(store) = global_store() else {
            return Ok(ToolResult::failure(
                "artifact store is not configured; cannot stage the export",
                elapsed(start),
            ));
        };

        let mut rows: Vec<Value> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0usize;
        let mut bytes = 0usize;
        let capped = loop {
            let remaining = max_records.saturating_sub(rows.len());
            let limit = (remaining as u64).clamp(1, LOGS_EXPORT_PAGE_LIMIT);
            let mut body = json!({
                "filter": { "query": query, "from": from, "to": to },
                "page": { "limit": limit }
            });
            if let Some(c) = &cursor {
                body["page"]["cursor"] = json!(c);
            }
            let resp = match client.post("/api/v2/logs/events/search", &body).await {
                Ok(r) => r,
                Err(e) => {
                    return Ok(ToolResult::from_report(
                        &e.wrap_err(format!(
                            "Datadog logs export failed after {pages} page(s), {} logs",
                            rows.len()
                        )),
                        elapsed(start),
                    ))
                }
            };
            pages += 1;
            let (page, next) = log_page(&resp);
            let got = page.len();
            bytes += page.iter().map(estimate_json_bytes).sum::<usize>();
            rows.extend(page);
            if rows.len() > max_records {
                rows.truncate(max_records);
                break true;
            }
            if bytes > MAX_RESULT_BYTES {
                break true;
            }
            match next_step(rows.len(), got, next, max_records) {
                PageStep::Continue(c) => cursor = Some(c),
                PageStep::Stop { capped } => break capped,
            }
        };
        info!(
            count = rows.len(),
            pages, capped, format, "Datadog logs export"
        );

        let cap_note = if capped {
            format!(
                " Truncated at {} logs — narrow the window or query to capture the rest.",
                rows.len()
            )
        } else {
            String::new()
        };

        if format == "artifact" {
            let columns = LOG_EXPORT_COLUMNS.iter().map(|s| s.to_string()).collect();
            let staged = match stage_table(
                store.as_ref(),
                &agent_id,
                LOGS_EXPORT_TOOL_NAME,
                Some(columns),
                rows,
                DEFAULT_INLINE_THRESHOLD,
            ) {
                Ok(s) => s,
                Err(e) => {
                    return Ok(ToolResult::failure(
                        format!("failed to stage logs: {e}"),
                        elapsed(start),
                    ))
                }
            };
            let mut output = staged_to_output(staged, LOGS_EXPORT_TOOL_NAME);
            if let Some(obj) = output.as_object_mut() {
                obj.insert(
                    "note".to_string(),
                    json!(format!(
                        "Logs staged out of context. Slice/aggregate with data.query or export \
                         with export_xlsx(artifact_id=...). Do not re-fetch or page through \
                         them.{cap_note}"
                    )),
                );
            }
            return Ok(ToolResult::success(output, elapsed(start)));
        }

        let row_count = rows.len();
        let bytes = match build_xlsx(&[Sheet::new("Logs", logs_export_table(&rows))]) {
            Ok(b) => b,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("failed to build the spreadsheet: {e}"),
                    elapsed(start),
                ))
            }
        };
        let size = bytes.len();
        let filename = xlsx_filename(None, "datadog-logs");
        let descriptor = ArtifactDescriptor {
            row_count: Some(row_count),
            ..ArtifactDescriptor::from_context(
                LOGS_EXPORT_TOOL_NAME,
                ContentType::OctetStream,
                agent_id.clone(),
            )
        };
        let handle = match store.write(&agent_id, descriptor, ArtifactBody::Bytes(bytes)) {
            Ok(h) => h,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("failed to stage the spreadsheet for delivery: {e}"),
                    elapsed(start),
                ))
            }
        };
        Ok(ToolResult::success(
            json!({
                GENERATED_IMAGES_KEY: [ {
                    "artifact_id": handle.id.as_str(),
                    "media_type": XLSX_MIME,
                    "filename": filename,
                    "bytes": size,
                } ],
                "rows": row_count,
                "filename": filename,
                "note": format!(
                    "Spreadsheet generated ({row_count} logs) and being delivered to this \
                     conversation as a downloadable .xlsx automatically — it happens outside \
                     your context, so you will NOT see the file. This is expected, NOT a \
                     failure: do not re-fetch the logs or try to read or verify the file. Just \
                     tell the user it is ready.{cap_note}"
                ),
            }),
            elapsed(start),
        ))
    }
}

dd_tool_struct!(DatadogLogsAggregateTool);

#[async_trait]
impl Tool for DatadogLogsAggregateTool {
    fn name(&self) -> &str {
        "datadog.logs_aggregate"
    }
    fn description(&self) -> &str {
        "Aggregate Datadog logs by computed metrics and groupings."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "query": { "type": "string" },
                "from": { "description": "Start time. Default: now-15m" },
                "to": { "description": "End time. Default: now" },
                "compute": { "type": "array", "description": "Aggregation specs, e.g. [{'aggregation':'count'}]" },
                "group_by": { "type": "array", "description": "Group-by facets, e.g. [{'facet':'service','limit':10}]" }
            },
            "required": ["query"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let query = match required_str(&input, "query") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let from = parse_time_string(&input["from"]).unwrap_or_else(|_| "now-15m".to_string());
        let to = parse_time_string(&input["to"]).unwrap_or_else(|_| "now".to_string());
        let compute = input
            .get("compute")
            .cloned()
            .unwrap_or_else(|| json!([{ "aggregation": "count" }]));
        let group_by = input.get("group_by").cloned().unwrap_or_else(|| json!([]));

        let body = json!({
            "filter": { "query": query, "from": from, "to": to },
            "compute": compute,
            "group_by": group_by
        });
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.post("/api/v2/logs/analytics/aggregate", &body).await {
            Ok(resp) => Ok(ToolResult::success(resp, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog logs aggregate failed"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Monitors
// ──────────────────────────────────────────────────────────────────────

dd_tool_struct!(DatadogMonitorsListTool);

#[async_trait]
impl Tool for DatadogMonitorsListTool {
    fn name(&self) -> &str {
        "datadog.monitors_list"
    }
    fn description(&self) -> &str {
        "List Datadog monitors. Optionally filter by name, tags, or monitor_tags."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "name": { "type": "string", "description": "Filter by name substring" },
                "tags": { "type": "string", "description": "Comma-separated scope tags" },
                "monitor_tags": { "type": "string", "description": "Comma-separated monitor tags" },
                "page_size": { "type": "integer", "description": "Default 50" },
                "page": { "type": "integer", "description": "Page index, default 0" }
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let page_size = coerce_to_u64(&input["page_size"]).unwrap_or(50).to_string();
        let page = coerce_to_u64(&input["page"]).unwrap_or(0).to_string();
        let mut q: Vec<(&str, &str)> =
            vec![("page_size", page_size.as_str()), ("page", page.as_str())];
        let name = input.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if !name.is_empty() {
            q.push(("name", name));
        }
        let tags = input.get("tags").and_then(|v| v.as_str()).unwrap_or("");
        if !tags.is_empty() {
            q.push(("tags", tags));
        }
        let mtags = input
            .get("monitor_tags")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !mtags.is_empty() {
            q.push(("monitor_tags", mtags));
        }
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.get_with_query("/api/v1/monitor", &q).await {
            Ok(resp) => {
                let monitors: Vec<Value> = resp
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|m| {
                        json!({
                            "id": m["id"],
                            "name": wrap_field(&m["name"], "datadog.monitors_list.name"),
                            "type": m["type"],
                            "overall_state": m["overall_state"],
                            "tags": m["tags"],
                            "creator": m["creator"]["handle"],
                        })
                    })
                    .collect();
                Ok(ToolResult::success(
                    json!({ "count": monitors.len(), "monitors": monitors }),
                    duration_ms,
                ))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog monitors list failed"),
                duration_ms,
            )),
        }
    }
}

dd_tool_struct!(DatadogMonitorsGetTool);

#[async_trait]
impl Tool for DatadogMonitorsGetTool {
    fn name(&self) -> &str {
        "datadog.monitors_get"
    }
    fn description(&self) -> &str {
        "Get full details for a single Datadog monitor."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "monitor_id": { "type": "integer" }
            },
            "required": ["monitor_id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let id = match coerce_to_i64(&input["monitor_id"]) {
            Some(n) => n,
            None => {
                return Ok(ToolResult::failure(
                    "Missing or invalid 'monitor_id'".to_string(),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let path = format!("/api/v1/monitor/{id}");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.get(&path).await {
            Ok(m) => {
                let out = json!({
                    "id": m["id"],
                    "name": wrap_field(&m["name"], "datadog.monitors_get.name"),
                    "type": m["type"],
                    "query": m["query"],
                    "message": wrap_field(&m["message"], "datadog.monitors_get.message"),
                    "tags": m["tags"],
                    "options": m["options"],
                    "overall_state": m["overall_state"],
                    "created": m["created"],
                    "modified": m["modified"],
                    "creator": m["creator"]["handle"],
                });
                Ok(ToolResult::success(out, duration_ms))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog monitor get failed"),
                duration_ms,
            )),
        }
    }
}

dd_tool_struct!(DatadogMonitorsSearchTool);

#[async_trait]
impl Tool for DatadogMonitorsSearchTool {
    fn name(&self) -> &str {
        "datadog.monitors_search"
    }
    fn description(&self) -> &str {
        "Search Datadog monitors with the monitor search query language."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "query": { "type": "string", "description": "e.g. 'status:alert env:prod'" },
                "page_size": { "type": "integer" },
                "page": { "type": "integer" }
            },
            "required": ["query"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let query = match required_str(&input, "query") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let page_size = coerce_to_u64(&input["page_size"]).unwrap_or(30).to_string();
        let page = coerce_to_u64(&input["page"]).unwrap_or(0).to_string();
        let q: Vec<(&str, &str)> = vec![
            ("query", query.as_str()),
            ("per_page", page_size.as_str()),
            ("page", page.as_str()),
        ];
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.get_with_query("/api/v1/monitor/search", &q).await {
            Ok(resp) => Ok(ToolResult::success(resp, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog monitors search failed"),
                duration_ms,
            )),
        }
    }
}

dd_tool_struct!(DatadogMonitorsCreateTool);

#[async_trait]
impl Tool for DatadogMonitorsCreateTool {
    fn name(&self) -> &str {
        "datadog.monitors_create"
    }
    fn description(&self) -> &str {
        "Create a new Datadog monitor. Use the `options` passthrough for thresholds, notifications, etc."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "type": { "type": "string", "description": "Monitor type (e.g. 'metric alert', 'log alert')" },
                "query": { "type": "string", "description": "Monitor query" },
                "name": { "type": "string" },
                "message": { "type": "string", "description": "Notification message; supports @user mentions" },
                "tags": { "type": "array", "items": { "type": "string" } },
                "priority": { "type": "integer", "description": "1 (highest) – 5 (lowest)" },
                "options": { "type": "object", "description": "Raw options object (thresholds, notify_no_data, etc.)" }
            },
            "required": ["type", "query", "name", "message"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let monitor_type = match required_str(&input, "type") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let query = match required_str(&input, "query") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let name = match required_str(&input, "name") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let message = match required_str(&input, "message") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };

        let mut body = json!({
            "type": monitor_type,
            "query": query,
            "name": name,
            "message": message,
        });
        if let Some(tags) = input.get("tags").and_then(|v| v.as_array()) {
            body["tags"] = json!(tags);
        }
        if let Some(prio) = coerce_to_i64(&input["priority"]) {
            body["priority"] = json!(prio);
        }
        if let Some(opts) = input.get("options") {
            body["options"] = opts.clone();
        }

        let duration_ms = start.elapsed().as_millis() as u64;
        match client.post("/api/v1/monitor", &body).await {
            Ok(resp) => {
                info!(id = ?resp["id"], "Datadog monitor created");
                Ok(ToolResult::success(
                    json!({ "id": resp["id"], "name": resp["name"] }),
                    duration_ms,
                ))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog monitor create failed"),
                duration_ms,
            )),
        }
    }
}

dd_tool_struct!(DatadogMonitorsUpdateTool);

#[async_trait]
impl Tool for DatadogMonitorsUpdateTool {
    fn name(&self) -> &str {
        "datadog.monitors_update"
    }
    fn description(&self) -> &str {
        "Update an existing Datadog monitor. Only the supplied fields are sent."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "monitor_id": { "type": "integer" },
                "name": { "type": "string" },
                "query": { "type": "string" },
                "message": { "type": "string" },
                "tags": { "type": "array", "items": { "type": "string" } },
                "priority": { "type": "integer" },
                "options": { "type": "object" }
            },
            "required": ["monitor_id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let id = match coerce_to_i64(&input["monitor_id"]) {
            Some(n) => n,
            None => {
                return Ok(ToolResult::failure(
                    "Missing or invalid 'monitor_id'".to_string(),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let mut body = serde_json::Map::new();
        for key in &["name", "query", "message"] {
            if let Some(s) = input.get(*key).and_then(|v| v.as_str()) {
                body.insert((*key).to_string(), json!(s));
            }
        }
        if let Some(tags) = input.get("tags").and_then(|v| v.as_array()) {
            body.insert("tags".to_string(), json!(tags));
        }
        if let Some(prio) = coerce_to_i64(&input["priority"]) {
            body.insert("priority".to_string(), json!(prio));
        }
        if let Some(opts) = input.get("options") {
            body.insert("options".to_string(), opts.clone());
        }

        if body.is_empty() {
            return Ok(ToolResult::failure(
                "No fields to update".to_string(),
                start.elapsed().as_millis() as u64,
            ));
        }

        let path = format!("/api/v1/monitor/{id}");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.put(&path, &Value::Object(body)).await {
            Ok(_) => Ok(ToolResult::success(
                json!({ "id": id, "updated": true }),
                duration_ms,
            )),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog monitor update failed"),
                duration_ms,
            )),
        }
    }
}

dd_tool_struct!(DatadogMonitorsDeleteTool);

#[async_trait]
impl Tool for DatadogMonitorsDeleteTool {
    fn name(&self) -> &str {
        "datadog.monitors_delete"
    }
    fn description(&self) -> &str {
        "Delete a Datadog monitor."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "monitor_id": { "type": "integer" }
            },
            "required": ["monitor_id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let id = match coerce_to_i64(&input["monitor_id"]) {
            Some(n) => n,
            None => {
                return Ok(ToolResult::failure(
                    "Missing or invalid 'monitor_id'".to_string(),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let path = format!("/api/v1/monitor/{id}");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.delete(&path).await {
            Ok(_) => Ok(ToolResult::success(
                json!({ "id": id, "deleted": true }),
                duration_ms,
            )),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog monitor delete failed"),
                duration_ms,
            )),
        }
    }
}

dd_tool_struct!(DatadogMonitorsMuteTool);

#[async_trait]
impl Tool for DatadogMonitorsMuteTool {
    fn name(&self) -> &str {
        "datadog.monitors_mute"
    }
    fn description(&self) -> &str {
        "Mute a Datadog monitor, optionally with an end time and scope."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "monitor_id": { "type": "integer" },
                "scope": { "type": "string", "description": "Tag scope to mute, e.g. 'host:i-0abc'" },
                "end": { "description": "Optional unmute time (epoch or relative)" }
            },
            "required": ["monitor_id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let id = match coerce_to_i64(&input["monitor_id"]) {
            Some(n) => n,
            None => {
                return Ok(ToolResult::failure(
                    "Missing or invalid 'monitor_id'".to_string(),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let mut body = json!({});
        if let Some(scope) = input.get("scope").and_then(|v| v.as_str()) {
            body["scope"] = json!(scope);
        }
        if let Some(end) = input.get("end").filter(|v| !v.is_null()) {
            match parse_time_seconds(end) {
                Ok(n) => body["end"] = json!(n),
                Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
            }
        }
        let path = format!("/api/v1/monitor/{id}/mute");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.post(&path, &body).await {
            Ok(_) => Ok(ToolResult::success(
                json!({ "id": id, "muted": true }),
                duration_ms,
            )),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog monitor mute failed"),
                duration_ms,
            )),
        }
    }
}

dd_tool_struct!(DatadogMonitorsUnmuteTool);

#[async_trait]
impl Tool for DatadogMonitorsUnmuteTool {
    fn name(&self) -> &str {
        "datadog.monitors_unmute"
    }
    fn description(&self) -> &str {
        "Unmute a previously muted Datadog monitor."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "monitor_id": { "type": "integer" },
                "scope": { "type": "string" },
                "all_scopes": { "type": "boolean" }
            },
            "required": ["monitor_id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let id = match coerce_to_i64(&input["monitor_id"]) {
            Some(n) => n,
            None => {
                return Ok(ToolResult::failure(
                    "Missing or invalid 'monitor_id'".to_string(),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };
        let mut body = json!({});
        if let Some(scope) = input.get("scope").and_then(|v| v.as_str()) {
            body["scope"] = json!(scope);
        }
        if let Some(all) = input.get("all_scopes").and_then(|v| v.as_bool()) {
            body["all_scopes"] = json!(all);
        }
        let path = format!("/api/v1/monitor/{id}/unmute");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.post(&path, &body).await {
            Ok(_) => Ok(ToolResult::success(
                json!({ "id": id, "unmuted": true }),
                duration_ms,
            )),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog monitor unmute failed"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Dashboards
// ──────────────────────────────────────────────────────────────────────

dd_tool_struct!(DatadogDashboardsListTool);

#[async_trait]
impl Tool for DatadogDashboardsListTool {
    fn name(&self) -> &str {
        "datadog.dashboards_list"
    }
    fn description(&self) -> &str {
        "List Datadog dashboards accessible to the authenticated user."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) }
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.get("/api/v1/dashboard").await {
            Ok(resp) => {
                let dashboards: Vec<Value> = resp
                    .get("dashboards")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|d| {
                        json!({
                            "id": d["id"],
                            "title": wrap_field(&d["title"], "datadog.dashboards_list.title"),
                            "url": d["url"],
                            "layout_type": d["layout_type"],
                            "author_handle": d["author_handle"],
                            "modified_at": d["modified_at"],
                        })
                    })
                    .collect();
                Ok(ToolResult::success(
                    json!({ "count": dashboards.len(), "dashboards": dashboards }),
                    duration_ms,
                ))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog dashboards list failed"),
                duration_ms,
            )),
        }
    }
}

dd_tool_struct!(DatadogDashboardsGetTool);

#[async_trait]
impl Tool for DatadogDashboardsGetTool {
    fn name(&self) -> &str {
        "datadog.dashboards_get"
    }
    fn description(&self) -> &str {
        "Get the full definition of a Datadog dashboard."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "dashboard_id": { "type": "string" }
            },
            "required": ["dashboard_id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let id = match required_str(&input, "dashboard_id") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let path = format!("/api/v1/dashboard/{id}");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.get(&path).await {
            Ok(resp) => Ok(ToolResult::success(resp, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog dashboard get failed"),
                duration_ms,
            )),
        }
    }
}

dd_tool_struct!(DatadogDashboardsCreateTool);

#[async_trait]
impl Tool for DatadogDashboardsCreateTool {
    fn name(&self) -> &str {
        "datadog.dashboards_create"
    }
    fn description(&self) -> &str {
        "Create a Datadog dashboard. Pass widgets and template variables as raw JSON arrays."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "title": { "type": "string" },
                "layout_type": { "type": "string", "enum": ["ordered", "free"] },
                "description": { "type": "string" },
                "widgets": { "type": "array" },
                "template_variables": { "type": "array" },
                "tags": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["title", "layout_type"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let title = match required_str(&input, "title") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let layout_type = match required_str(&input, "layout_type") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let mut body = json!({
            "title": title,
            "layout_type": layout_type,
            "widgets": input.get("widgets").cloned().unwrap_or_else(|| json!([])),
        });
        if let Some(desc) = input.get("description").and_then(|v| v.as_str()) {
            body["description"] = json!(desc);
        }
        if let Some(tv) = input.get("template_variables") {
            body["template_variables"] = tv.clone();
        }
        if let Some(tags) = input.get("tags") {
            body["tags"] = tags.clone();
        }
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.post("/api/v1/dashboard", &body).await {
            Ok(resp) => Ok(ToolResult::success(
                json!({ "id": resp["id"], "url": resp["url"], "title": resp["title"] }),
                duration_ms,
            )),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog dashboard create failed"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Events
// ──────────────────────────────────────────────────────────────────────

dd_tool_struct!(DatadogEventsListTool);

#[async_trait]
impl Tool for DatadogEventsListTool {
    fn name(&self) -> &str {
        "datadog.events_list"
    }
    fn description(&self) -> &str {
        "List Datadog events in a time range, optionally filtered by tags or sources."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "from": { "description": "Start time" },
                "to": { "description": "End time" },
                "priority": { "type": "string", "enum": ["normal", "low"] },
                "sources": { "type": "string", "description": "Comma-separated source names" },
                "tags": { "type": "string", "description": "Comma-separated tags" }
            },
            "required": ["from", "to"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let from = match parse_time_seconds(&input["from"]) {
            Ok(n) => n.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let to = match parse_time_seconds(&input["to"]) {
            Ok(n) => n.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let mut q: Vec<(&str, &str)> = vec![("start", from.as_str()), ("end", to.as_str())];
        let priority = input.get("priority").and_then(|v| v.as_str()).unwrap_or("");
        if !priority.is_empty() {
            q.push(("priority", priority));
        }
        let sources = input.get("sources").and_then(|v| v.as_str()).unwrap_or("");
        if !sources.is_empty() {
            q.push(("sources", sources));
        }
        let tags = input.get("tags").and_then(|v| v.as_str()).unwrap_or("");
        if !tags.is_empty() {
            q.push(("tags", tags));
        }
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.get_with_query("/api/v1/events", &q).await {
            Ok(resp) => {
                let events: Vec<Value> = resp
                    .get("events")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|e| {
                        json!({
                            "id": e["id"],
                            "title": wrap_field(&e["title"], "datadog.events_list.title"),
                            "text": wrap_field(&e["text"], "datadog.events_list.text"),
                            "date_happened": e["date_happened"],
                            "priority": e["priority"],
                            "tags": e["tags"],
                            "source": e["source"],
                        })
                    })
                    .collect();
                Ok(ToolResult::success(
                    json!({ "count": events.len(), "events": events }),
                    duration_ms,
                ))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog events list failed"),
                duration_ms,
            )),
        }
    }
}

dd_tool_struct!(DatadogEventsPostTool);

#[async_trait]
impl Tool for DatadogEventsPostTool {
    fn name(&self) -> &str {
        "datadog.events_post"
    }
    fn description(&self) -> &str {
        "Post a custom event to the Datadog event stream."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "title": { "type": "string" },
                "text": { "type": "string" },
                "tags": { "type": "array", "items": { "type": "string" } },
                "alert_type": { "type": "string", "enum": ["info", "warning", "error", "success"] },
                "priority": { "type": "string", "enum": ["normal", "low"] }
            },
            "required": ["title", "text"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let title = match required_str(&input, "title") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let text = match required_str(&input, "text") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let mut body = json!({ "title": title, "text": text });
        if let Some(tags) = input.get("tags") {
            body["tags"] = tags.clone();
        }
        if let Some(at) = input.get("alert_type").and_then(|v| v.as_str()) {
            body["alert_type"] = json!(at);
        }
        if let Some(p) = input.get("priority").and_then(|v| v.as_str()) {
            body["priority"] = json!(p);
        }
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.post("/api/v1/events", &body).await {
            Ok(resp) => Ok(ToolResult::success(
                json!({ "id": resp["event"]["id"], "url": resp["event"]["url"] }),
                duration_ms,
            )),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog event post failed"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Incidents
// ──────────────────────────────────────────────────────────────────────

dd_tool_struct!(DatadogIncidentsListTool);

#[async_trait]
impl Tool for DatadogIncidentsListTool {
    fn name(&self) -> &str {
        "datadog.incidents_list"
    }
    fn description(&self) -> &str {
        "List Datadog incidents."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "page_size": { "type": "integer" }
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let size = coerce_to_u64(&input["page_size"]).unwrap_or(50).to_string();
        let duration_ms = start.elapsed().as_millis() as u64;
        match client
            .get_with_query("/api/v2/incidents", &[("page[size]", size.as_str())])
            .await
        {
            Ok(resp) => Ok(ToolResult::success(resp, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog incidents list failed"),
                duration_ms,
            )),
        }
    }
}

dd_tool_struct!(DatadogIncidentsGetTool);

#[async_trait]
impl Tool for DatadogIncidentsGetTool {
    fn name(&self) -> &str {
        "datadog.incidents_get"
    }
    fn description(&self) -> &str {
        "Get a single Datadog incident by id."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "incident_id": { "type": "string" }
            },
            "required": ["incident_id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let id = match required_str(&input, "incident_id") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let path = format!("/api/v2/incidents/{id}");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.get(&path).await {
            Ok(resp) => Ok(ToolResult::success(resp, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog incident get failed"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// SLOs
// ──────────────────────────────────────────────────────────────────────

dd_tool_struct!(DatadogSlosListTool);

#[async_trait]
impl Tool for DatadogSlosListTool {
    fn name(&self) -> &str {
        "datadog.slos_list"
    }
    fn description(&self) -> &str {
        "List Datadog SLOs, optionally filtered by query."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "query": { "type": "string", "description": "SLO search query" },
                "limit": { "type": "integer" }
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let limit = coerce_to_u64(&input["limit"]).unwrap_or(50).to_string();
        let mut q: Vec<(&str, &str)> = vec![("limit", limit.as_str())];
        let query = input.get("query").and_then(|v| v.as_str()).unwrap_or("");
        if !query.is_empty() {
            q.push(("query", query));
        }
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.get_with_query("/api/v1/slo", &q).await {
            Ok(resp) => Ok(ToolResult::success(resp, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog SLOs list failed"),
                duration_ms,
            )),
        }
    }
}

dd_tool_struct!(DatadogSlosGetTool);

#[async_trait]
impl Tool for DatadogSlosGetTool {
    fn name(&self) -> &str {
        "datadog.slos_get"
    }
    fn description(&self) -> &str {
        "Get a single Datadog SLO by id."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "slo_id": { "type": "string" }
            },
            "required": ["slo_id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let id = match required_str(&input, "slo_id") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let path = format!("/api/v1/slo/{id}");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.get(&path).await {
            Ok(resp) => Ok(ToolResult::success(resp, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog SLO get failed"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Hosts
// ──────────────────────────────────────────────────────────────────────

dd_tool_struct!(DatadogHostsListTool);

#[async_trait]
impl Tool for DatadogHostsListTool {
    fn name(&self) -> &str {
        "datadog.hosts_list"
    }
    fn description(&self) -> &str {
        "List Datadog hosts, optionally filtered by tag query."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "filter": { "type": "string", "description": "Tag filter, e.g. 'env:prod'" },
                "count": { "type": "integer", "description": "Default 100, max 1000" },
                "start": { "type": "integer", "description": "Pagination offset" }
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start_t = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start_t);
        let count = coerce_to_u64(&input["count"])
            .unwrap_or(100)
            .min(1000)
            .to_string();
        let offset = coerce_to_u64(&input["start"]).unwrap_or(0).to_string();
        let mut q: Vec<(&str, &str)> = vec![("count", count.as_str()), ("start", offset.as_str())];
        let filter = input.get("filter").and_then(|v| v.as_str()).unwrap_or("");
        if !filter.is_empty() {
            q.push(("filter", filter));
        }
        let duration_ms = start_t.elapsed().as_millis() as u64;
        match client.get_with_query("/api/v1/hosts", &q).await {
            Ok(resp) => {
                let hosts: Vec<Value> = resp
                    .get("host_list")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|h| {
                        json!({
                            "name": h["name"],
                            "host_name": h["host_name"],
                            "up": h["up"],
                            "is_muted": h["is_muted"],
                            "last_reported_time": h["last_reported_time"],
                            "tags_by_source": h["tags_by_source"],
                        })
                    })
                    .collect();
                Ok(ToolResult::success(
                    json!({
                        "total_returned": resp["total_returned"],
                        "total_matching": resp["total_matching"],
                        "hosts": hosts
                    }),
                    duration_ms,
                ))
            }
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog hosts list failed"),
                duration_ms,
            )),
        }
    }
}

dd_tool_struct!(DatadogHostsMuteTool);

#[async_trait]
impl Tool for DatadogHostsMuteTool {
    fn name(&self) -> &str {
        "datadog.hosts_mute"
    }
    fn description(&self) -> &str {
        "Mute a Datadog host (silence its monitor alerts)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "host_name": { "type": "string" },
                "message": { "type": "string" },
                "end": { "description": "Optional unmute time (epoch or relative)" },
                "override": { "type": "boolean" }
            },
            "required": ["host_name"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let name = match required_str(&input, "host_name") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let mut body = json!({});
        if let Some(m) = input.get("message").and_then(|v| v.as_str()) {
            body["message"] = json!(m);
        }
        if let Some(end) = input.get("end").filter(|v| !v.is_null()) {
            match parse_time_seconds(end) {
                Ok(n) => body["end"] = json!(n),
                Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
            }
        }
        if let Some(ov) = input.get("override").and_then(|v| v.as_bool()) {
            body["override"] = json!(ov);
        }
        let path = format!("/api/v1/host/{name}/mute");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.post(&path, &body).await {
            Ok(_) => Ok(ToolResult::success(
                json!({ "host_name": name, "muted": true }),
                duration_ms,
            )),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog host mute failed"),
                duration_ms,
            )),
        }
    }
}

dd_tool_struct!(DatadogHostsUnmuteTool);

#[async_trait]
impl Tool for DatadogHostsUnmuteTool {
    fn name(&self) -> &str {
        "datadog.hosts_unmute"
    }
    fn description(&self) -> &str {
        "Unmute a previously muted Datadog host."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "host_name": { "type": "string" }
            },
            "required": ["host_name"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let name = match required_str(&input, "host_name") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let path = format!("/api/v1/host/{name}/unmute");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.post(&path, &json!({})).await {
            Ok(_) => Ok(ToolResult::success(
                json!({ "host_name": name, "unmuted": true }),
                duration_ms,
            )),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog host unmute failed"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Downtimes
// ──────────────────────────────────────────────────────────────────────

dd_tool_struct!(DatadogDowntimesListTool);

#[async_trait]
impl Tool for DatadogDowntimesListTool {
    fn name(&self) -> &str {
        "datadog.downtimes_list"
    }
    fn description(&self) -> &str {
        "List active and scheduled Datadog downtimes."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "current_only": { "type": "boolean" }
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let current_only = input
            .get("current_only")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let path = if current_only {
            "/api/v2/downtime?current_only=true"
        } else {
            "/api/v2/downtime"
        };
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.get(path).await {
            Ok(resp) => Ok(ToolResult::success(resp, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog downtimes list failed"),
                duration_ms,
            )),
        }
    }
}

dd_tool_struct!(DatadogDowntimesCreateTool);

#[async_trait]
impl Tool for DatadogDowntimesCreateTool {
    fn name(&self) -> &str {
        "datadog.downtimes_create"
    }
    fn description(&self) -> &str {
        "Schedule a new Datadog downtime by monitor id or scope."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "scope": { "type": "string", "description": "Tag scope, e.g. 'env:staging'" },
                "monitor_id": { "type": "integer" },
                "message": { "type": "string" },
                "start": { "description": "Start time (epoch or relative)" },
                "end": { "description": "End time (epoch or relative)" }
            },
            "required": ["start", "end"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start_t = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start_t);
        let start_time = match parse_time_seconds(&input["start"]) {
            Ok(n) => n,
            Err(e) => return Ok(ToolResult::failure(e, start_t.elapsed().as_millis() as u64)),
        };
        let end_time = match parse_time_seconds(&input["end"]) {
            Ok(n) => n,
            Err(e) => return Ok(ToolResult::failure(e, start_t.elapsed().as_millis() as u64)),
        };

        let scope = input.get("scope").and_then(|v| v.as_str());
        let monitor_id = coerce_to_i64(&input["monitor_id"]);
        let monitor_identifier = match (monitor_id, scope) {
            (Some(_), Some(_)) => {
                return Ok(ToolResult::failure(
                    "Provide either 'scope' or 'monitor_id', not both".to_string(),
                    start_t.elapsed().as_millis() as u64,
                ))
            }
            (Some(id), None) => json!({ "monitor_id": id }),
            (None, Some(s)) => json!({ "monitor_tags": [s] }),
            (None, None) => {
                return Ok(ToolResult::failure(
                    "Provide either 'scope' or 'monitor_id'".to_string(),
                    start_t.elapsed().as_millis() as u64,
                ))
            }
        };

        let mut attributes = json!({
            "schedule": {
                "start": start_time,
                "end": end_time,
            },
            "monitor_identifier": monitor_identifier,
        });
        if let Some(m) = input.get("message").and_then(|v| v.as_str()) {
            attributes["message"] = json!(m);
        }

        let body = json!({
            "data": { "type": "downtime", "attributes": attributes }
        });
        let duration_ms = start_t.elapsed().as_millis() as u64;
        match client.post("/api/v2/downtime", &body).await {
            Ok(resp) => Ok(ToolResult::success(
                json!({ "id": resp["data"]["id"] }),
                duration_ms,
            )),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog downtime create failed"),
                duration_ms,
            )),
        }
    }
}

dd_tool_struct!(DatadogDowntimesCancelTool);

#[async_trait]
impl Tool for DatadogDowntimesCancelTool {
    fn name(&self) -> &str {
        "datadog.downtimes_cancel"
    }
    fn description(&self) -> &str {
        "Cancel a scheduled or active Datadog downtime."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "downtime_id": { "type": "string" }
            },
            "required": ["downtime_id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let id = match required_str(&input, "downtime_id") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let path = format!("/api/v2/downtime/{id}");
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.delete(&path).await {
            Ok(_) => Ok(ToolResult::success(
                json!({ "id": id, "cancelled": true }),
                duration_ms,
            )),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog downtime cancel failed"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Synthetics
// ──────────────────────────────────────────────────────────────────────

dd_tool_struct!(DatadogSyntheticsListTool);

#[async_trait]
impl Tool for DatadogSyntheticsListTool {
    fn name(&self) -> &str {
        "datadog.synthetics_list"
    }
    fn description(&self) -> &str {
        "List all Datadog Synthetic tests (API and Browser)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) }
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let duration_ms = start.elapsed().as_millis() as u64;
        match client.get("/api/v1/synthetics/tests").await {
            Ok(resp) => Ok(ToolResult::success(resp, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog synthetics list failed"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Escape hatch
// ──────────────────────────────────────────────────────────────────────

dd_tool_struct!(DatadogApiTool);

#[async_trait]
impl Tool for DatadogApiTool {
    fn name(&self) -> &str {
        "datadog.api"
    }
    fn description(&self) -> &str {
        "Make an arbitrary Datadog REST request. Use sparingly — prefer the dedicated tools."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": { "type": "string", "description": account_description(&self.clients, &self.default_account) },
                "method": { "type": "string", "enum": ["GET", "POST", "PUT", "DELETE"] },
                "path": { "type": "string", "description": "Path including the /api/v1 or /api/v2 prefix" },
                "body": { "type": "object" },
                "query": { "type": "object" }
            },
            "required": ["method", "path"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let client = resolve_or_fail!(self, input, start);
        let method = match required_str(&input, "method") {
            Ok(s) => s.to_uppercase(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let path = match required_str(&input, "path") {
            Ok(s) => s.to_string(),
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let empty = json!({});
        let body = input.get("body").unwrap_or(&empty);

        let duration_ms = start.elapsed().as_millis() as u64;
        let result = match method.as_str() {
            "GET" => {
                if let Some(q) = input.get("query").and_then(|v| v.as_object()) {
                    let pairs: Vec<(String, String)> = q
                        .iter()
                        .filter_map(|(k, v)| {
                            v.as_str()
                                .map(String::from)
                                .or_else(|| v.as_i64().map(|n| n.to_string()))
                                .or_else(|| v.as_bool().map(|b| b.to_string()))
                                .map(|s| (k.clone(), s))
                        })
                        .collect();
                    let refs: Vec<(&str, &str)> = pairs
                        .iter()
                        .map(|(k, v)| (k.as_str(), v.as_str()))
                        .collect();
                    client.get_with_query(&path, &refs).await
                } else {
                    client.get(&path).await
                }
            }
            "POST" => client.post(&path, body).await,
            "PUT" => client.put(&path, body).await,
            "DELETE" => client.delete(&path).await,
            other => Err(eyre::eyre!("Unsupported method: {other}")),
        };
        match result {
            Ok(v) => Ok(ToolResult::success(v, duration_ms)),
            Err(e) => Ok(ToolResult::from_report(
                &e.wrap_err("Datadog API request failed"),
                duration_ms,
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    type ClientMap = Arc<HashMap<String, Arc<DatadogClient>>>;

    fn make_client() -> Arc<DatadogClient> {
        Arc::new(DatadogClient::new("api", "app", "us1", None).unwrap())
    }

    fn log_event(i: usize) -> Value {
        json!({
            "id": format!("log-{i}"),
            "attributes": {
                "timestamp": "2026-06-07T00:00:00Z",
                "status": "error",
                "service": "web",
                "host": format!("host-{i}"),
                "message": format!("boom {i}"),
                "tags": ["env:prod"]
            }
        })
    }

    #[test]
    fn next_step_decision_table() {
        let cursor = || Some("c".to_string());
        // Full page + cursor + room left → continue.
        assert!(matches!(
            next_step(1000, 1000, cursor(), 50_000),
            PageStep::Continue(_)
        ));
        // Hit the cap with a cursor still present → stop, capped.
        assert!(matches!(
            next_step(50_000, 1000, cursor(), 50_000),
            PageStep::Stop { capped: true }
        ));
        // Hit the cap with no cursor → stop, NOT capped (we got everything).
        assert!(matches!(
            next_step(50_000, 1000, None, 50_000),
            PageStep::Stop { capped: false }
        ));
        // Empty page with a cursor (Datadog end-of-stream) → stop, not capped.
        assert!(matches!(
            next_step(1000, 0, cursor(), 50_000),
            PageStep::Stop { capped: false }
        ));
        // No cursor mid-stream → stop, not capped.
        assert!(matches!(
            next_step(1000, 1000, None, 50_000),
            PageStep::Stop { capped: false }
        ));
    }

    #[test]
    fn log_page_slims_rows_and_extracts_cursor() {
        let resp = json!({
            "data": [log_event(0), log_event(1)],
            "meta": { "page": { "after": "cursor-abc" } }
        });
        let (rows, cursor) = log_page(&resp);
        assert_eq!(rows.len(), 2);
        assert_eq!(cursor.as_deref(), Some("cursor-abc"));
        assert_eq!(rows[0]["id"], "log-0");
        assert_eq!(
            rows[0]["message"], "boom 0",
            "message stays raw, not wrapped"
        );
        assert_eq!(rows[0]["tags"], json!(["env:prod"]));

        let last = json!({ "data": [], "meta": { "page": {} } });
        let (rows, cursor) = log_page(&last);
        assert!(rows.is_empty());
        assert!(cursor.is_none());
    }

    #[test]
    fn logs_export_table_has_fixed_columns_and_typed_cells() {
        let rows: Vec<Value> = (0..2).map(log_event).map(|l| slim_log_row(&l)).collect();
        let table = logs_export_table(&rows);
        let keys: Vec<&str> = table.columns.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, LOG_EXPORT_COLUMNS);
        assert_eq!(
            table.rows[0][4],
            workforce_agent::export::Cell::Text("boom 0".into())
        );
        assert_eq!(
            table.rows[0][5],
            workforce_agent::export::Cell::Text("[\"env:prod\"]".into()),
            "tags array stringifies for xlsx"
        );
    }

    #[test]
    fn logs_export_artifact_staging_is_canonical() {
        use workforce_agent::artifacts::{
            ArtifactBody, ArtifactId, ArtifactStore, FsArtifactStore,
        };

        let dir = tempfile::TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let rows: Vec<Value> = (0..500).map(log_event).map(|l| slim_log_row(&l)).collect();
        let columns: Vec<String> = LOG_EXPORT_COLUMNS.iter().map(|s| s.to_string()).collect();
        let staged = stage_table(
            store.as_ref(),
            "agent-a",
            LOGS_EXPORT_TOOL_NAME,
            Some(columns),
            rows,
            DEFAULT_INLINE_THRESHOLD,
        )
        .unwrap();
        let output = staged_to_output(staged, LOGS_EXPORT_TOOL_NAME);
        assert_eq!(output["kind"], "tabular_artifact");
        assert_eq!(output["rows"], 500);
        assert_eq!(output["columns"], json!(LOG_EXPORT_COLUMNS));
        let id = ArtifactId::parse(output["artifact_id"].as_str().unwrap()).unwrap();
        let ArtifactBody::Json(Value::Array(stored)) = store.read("agent-a", &id).unwrap() else {
            panic!("expected canonical rows");
        };
        assert_eq!(stored[0]["message"], "boom 0");
    }

    fn fixture_clients() -> ClientMap {
        let clients: HashMap<String, Arc<DatadogClient>> = [("prod-us", "us1"), ("prod-eu", "eu1")]
            .into_iter()
            .map(|(name, _)| (name.to_string(), make_client()))
            .collect();
        Arc::new(clients)
    }

    #[test]
    fn resolve_client_uses_explicit_account() {
        let clients = fixture_clients();
        let input = json!({ "account": "prod-eu" });
        assert!(resolve_client(&clients, "prod-us", &input).is_ok());
    }

    #[test]
    fn resolve_client_explicit_unknown_errors() {
        let clients = fixture_clients();
        let input = json!({ "account": "ghost" });
        assert!(resolve_client(&clients, "prod-us", &input).is_err());
    }

    #[test]
    fn resolve_client_falls_back_to_default() {
        let clients = fixture_clients();
        let input = json!({});
        assert!(resolve_client(&clients, "prod-us", &input).is_ok());
    }

    #[test]
    fn resolve_client_unknown_default_errors() {
        let clients = fixture_clients();
        let input = json!({});
        assert!(resolve_client(&clients, "missing", &input).is_err());
    }

    #[test]
    fn account_description_lists_available_accounts() {
        let clients = fixture_clients();
        let desc = account_description(&clients, "prod-us");
        assert!(desc.contains("prod-us"));
        assert!(desc.contains("prod-eu"));
        assert!(desc.contains("Default: prod-us"));
        assert!(account_description(&clients, "gone").contains("Default: none — pass account"));
    }

    #[test]
    fn account_description_collapses_when_single() {
        let mut map: HashMap<String, Arc<DatadogClient>> = HashMap::new();
        map.insert("only".to_string(), make_client());
        let desc = account_description(&map, "only");
        assert!(desc.contains("default if omitted"));
    }

    #[test]
    fn parse_time_accepts_unix_seconds() {
        assert_eq!(parse_time_seconds(&json!(1700000000)).unwrap(), 1700000000);
        assert_eq!(
            parse_time_seconds(&json!("1700000000")).unwrap(),
            1700000000
        );
    }

    #[test]
    fn parse_time_accepts_now() {
        let now = parse_time_seconds(&json!("now")).unwrap();
        let real_now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!((real_now - now).abs() < 5);
    }

    #[test]
    fn parse_time_accepts_relative_now_minus() {
        let real_now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let one_hour_ago = parse_time_seconds(&json!("now-1h")).unwrap();
        assert!((real_now - one_hour_ago - 3600).abs() < 5);

        let fifteen_min_ago = parse_time_seconds(&json!("now-15m")).unwrap();
        assert!((real_now - fifteen_min_ago - 900).abs() < 5);

        let two_days_ago = parse_time_seconds(&json!("now-2d")).unwrap();
        assert!((real_now - two_days_ago - 172800).abs() < 5);
    }

    #[test]
    fn parse_time_rejects_garbage() {
        assert!(parse_time_seconds(&json!("totally-bogus")).is_err());
        assert!(parse_time_seconds(&json!("now-1x")).is_err());
        assert!(parse_time_seconds(&json!(null)).is_err());
    }

    #[test]
    fn parse_time_string_passes_iso_through() {
        let iso = "2026-01-01T00:00:00Z";
        assert_eq!(parse_time_string(&json!(iso)).unwrap(), iso);
    }

    #[test]
    fn parse_time_string_resolves_relative() {
        let v = parse_time_string(&json!("now")).unwrap();
        // Should be a numeric epoch string.
        assert!(v.parse::<i64>().is_ok());
    }

    #[test]
    fn parse_duration_seconds_known_units() {
        assert_eq!(parse_duration_seconds("30s").unwrap(), 30);
        assert_eq!(parse_duration_seconds("5m").unwrap(), 300);
        assert_eq!(parse_duration_seconds("2h").unwrap(), 7200);
        assert_eq!(parse_duration_seconds("1d").unwrap(), 86400);
        assert_eq!(parse_duration_seconds("1w").unwrap(), 604800);
    }

    #[test]
    fn parse_duration_seconds_rejects_unknown_unit() {
        assert!(parse_duration_seconds("5y").is_err());
        assert!(parse_duration_seconds("abc").is_err());
    }

    #[test]
    fn parse_duration_seconds_rejects_empty_string() {
        assert!(parse_duration_seconds("").is_err());
        assert!(parse_duration_seconds("   ").is_err());
    }

    #[test]
    fn parse_relative_time_rejects_empty_offset() {
        assert!(parse_relative_time("now-").is_err());
        assert!(parse_relative_time("now+").is_err());
    }

    #[test]
    fn required_str_rejects_empty_and_missing() {
        assert!(required_str(&json!({}), "k").is_err());
        assert!(required_str(&json!({ "k": "" }), "k").is_err());
        assert!(required_str(&json!({ "k": "v" }), "k").is_ok());
    }
}
