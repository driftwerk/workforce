//! `aws.s3.collect` — fetch a *set* of CSV or spreadsheet objects with bounded
//! concurrency, concatenate their rows with per-file metadata columns prepended, and stream
//! the union to one capped CSV download. The bytes never enter the agent
//! context; only a summary (rows, byte total, and a per-file skip/mismatch
//! report) returns. Built on the shared [`export_csv`] streaming kernel, so the
//! output is memory-safe by construction rather than by inlining.
//!
//! Two input modes resolve to the same `(metadata_columns, targets)` shape via
//! [`super::expand`]: explicit `keys`, or a `template` crossed with
//! `dimensions`. There is no listing here — discovery is `aws.s3.list`'s job
//! (use its `delimiter` to map the hierarchy first); `collect` only fetches
//! keys it is handed.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::{eyre, Result};
use futures::stream::{self, StreamExt};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::info;

use workforce_agent::artifacts::{
    download_filename, download_marker, export_csv, global_store, stage_table, staged_to_output,
    zip_table, ArtifactStore, Flow, RowProducer, RowSink, StreamCaps, DEFAULT_INLINE_THRESHOLD,
};
use workforce_agent::runner::SessionContext;
use workforce_agent::tools::image_gen::GENERATED_IMAGES_KEY;
use workforce_agent::tools::{Tool, ToolResult};

use super::client::{Allowlist, S3Backend};
use super::content::{classify, parse_csv, parse_spreadsheet, ContentKind};
use super::expand::{expand, Target, MAX_COLLECT_KEYS};

/// In-flight GETs at once. S3 sustains far more; this caps resident bodies at
/// `COLLECT_CONCURRENCY × max_get_bytes` (when a per-file cap is set) and keeps
/// the fan-out polite.
const COLLECT_CONCURRENCY: usize = 16;

/// Cap on matching rows held in memory by the inline (filter) path. Exceeding
/// it stops the scan early and marks the result truncated — a search that hit
/// this is inconclusive, not "not found".
const MAX_INLINE_ROWS: usize = 50_000;

/// A row predicate pushed into the streaming fetch (filter mode). Resolved by
/// column NAME against each file's parsed row, so it tolerates the delta-vs-main
/// schema difference. Matches the SOURCE columns only — not the injected
/// `file_*` metadata columns.
#[derive(Debug, Clone, Deserialize)]
struct RowFilter {
    column: String,
    values: Vec<String>,
}

impl RowFilter {
    fn matches(&self, row: &Value) -> bool {
        row.get(&self.column)
            .and_then(Value::as_str)
            .is_some_and(|cell| self.values.iter().any(|v| cell_matches(cell, v)))
    }
}

/// Equality after normalising the Excel text marker and (numeric only) leading
/// zeros — so `'0626051698`, `0626051698` and `626051698` all match, while
/// non-numeric codes like `0526` vs `526` stay distinct.
fn cell_matches(cell: &str, value: &str) -> bool {
    let c = cell.strip_prefix('\'').unwrap_or(cell).trim();
    let v = value.strip_prefix('\'').unwrap_or(value).trim();
    if c == v {
        return true;
    }
    let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    numeric(c) && numeric(v) && c.trim_start_matches('0') == v.trim_start_matches('0')
}

/// What to do when a single object cannot be fetched-and-parsed.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
enum OnError {
    /// Record the failure and carry on — robust for large harvests.
    #[default]
    Skip,
    /// Abort the whole export on the first failure; nothing is staged.
    Fail,
}

#[derive(Deserialize)]
struct CollectInput {
    bucket: String,
    #[serde(default)]
    keys: Option<Vec<String>>,
    #[serde(default)]
    path_columns: Vec<String>,
    #[serde(default)]
    template: Option<String>,
    #[serde(default)]
    dimensions: HashMap<String, Vec<String>>,
    #[serde(default)]
    on_error: OnError,
    #[serde(default)]
    max_rows: Option<usize>,
    #[serde(default)]
    filter: Option<RowFilter>,
    #[serde(default)]
    sheet: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct Skipped {
    key: String,
    reason: String,
}

#[derive(Debug, Clone, Serialize)]
struct Mismatch {
    key: String,
    columns: Vec<String>,
}

/// What the producer observed, lifted out of `produce` (which takes `&self`)
/// via the producer's mutex so the tool can report it after the export.
#[derive(Debug, Default)]
struct CollectReport {
    included: usize,
    skipped: Vec<Skipped>,
    mismatches: Vec<Mismatch>,
    /// Whether the filter column appeared in any scanned file's header. A
    /// filtered run with this `false` is a column/schema mismatch, not a clean
    /// "0 matches" — surfaced distinctly so the agent never reports a false
    /// "not found".
    filter_column_found: bool,
    /// The source data columns the header was established from (sans the
    /// `file_*` metadata) — shown to the agent when the filter column is absent.
    data_columns: Vec<String>,
}

pub struct AwsS3CollectTool {
    backend: Arc<dyn S3Backend>,
    allowlist: Arc<Allowlist>,
    max_get_bytes: u64,
}

impl AwsS3CollectTool {
    pub fn new(backend: Arc<dyn S3Backend>, allowlist: Arc<Allowlist>, max_get_bytes: u64) -> Self {
        Self {
            backend,
            allowlist,
            max_get_bytes,
        }
    }
}

#[async_trait]
impl Tool for AwsS3CollectTool {
    fn name(&self) -> &str {
        "aws.s3.collect"
    }

    fn description(&self) -> &str {
        "Fetch many CSV or xlsx/xlsm workbook objects at once across S3 keys, prepending per-file columns (`file_path` plus any you name) so every row is traceable to its source. Workbooks contribute one sheet each (`sheet` selects which; default first visible). \
         Supply the objects either as an explicit `keys` list (with optional `path_columns` naming the key's path segments) or as a `template` like \"{date}/{trust}/main.csv\" crossed with `dimensions` value lists — no listing needed. \
         TO SEARCH (the common case — 'find value X across these files'), pass a `filter`: only matching rows are returned INLINE and made queryable via `data.query`, nothing large is delivered, and matching is robust to Excel `'` markers and leading zeros. The filter matches the files' OWN columns (e.g. `Request Id`), NOT the injected `file_*` metadata — narrow trust/date via `keys`/`template` instead. \
         WITHOUT a filter the full concatenation is delivered out-of-band as a downloadable CSV (you never see its contents); use this only when the human wants the whole file. Both modes report which files were skipped and whether the result was `truncated`."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "bucket": { "type": "string", "description": "Bucket name (must be allowlisted)." },
                "keys": { "type": "array", "items": { "type": "string" }, "description": "Explicit object keys to fetch. Mutually exclusive with `template`." },
                "path_columns": { "type": "array", "items": { "type": "string" }, "description": "Names for the leading '/'-separated segments of each key, added as metadata columns (keys mode)." },
                "template": { "type": "string", "description": "Key template with {placeholder} tokens, e.g. \"{file_date}/{file_region}/main.csv\". Mutually exclusive with `keys`." },
                "dimensions": { "type": "object", "additionalProperties": { "type": "array", "items": { "type": "string" } }, "description": "Value lists per template placeholder; keys are the Cartesian product. Each placeholder becomes a metadata column." },
                "filter": {
                    "type": "object",
                    "description": "Search mode: keep only rows whose `column` equals one of `values` (leading-zero/quote-insensitive). Matches the files' source columns, not the `file_*` metadata. Matches are returned inline + queryable.",
                    "properties": {
                        "column": { "type": "string", "description": "Source column name to match on, e.g. \"Request Id\"." },
                        "values": { "type": "array", "items": { "type": "string" }, "description": "Values to match (any). Must be non-empty." }
                    },
                    "required": ["column", "values"]
                },
                "sheet": { "type": "string", "description": "Sheet name to read from each spreadsheet object (ignored for CSV). Defaults to the first sheet; a file missing the named sheet is skipped with its sheet inventory as the reason." },
                "on_error": { "type": "string", "enum": ["skip", "fail"], "description": "Per-file failure handling: skip-and-report (default) or fail the whole run." },
                "max_rows": { "type": "integer", "minimum": 1, "description": "Optional output row cap (clamped to the mode's default)." }
            },
            "required": ["bucket"]
        })
    }

    /// Tell an approver the scope of THIS call: how many objects one approval
    /// authorizes reading from which bucket. Counted cheaply from the raw input
    /// (this runs on every call, before the policy check) — no expansion, no
    /// fetch. The over-cap case is flagged rather than counted, since that is
    /// the highest-scope request an approver would want surfaced.
    fn approval_summary(&self, input: &Value) -> Option<String> {
        let bucket = input.get("bucket").and_then(Value::as_str)?;
        Some(match collect_object_count(input) {
            Some(n) if n > MAX_COLLECT_KEYS => {
                format!("read over {MAX_COLLECT_KEYS} objects from bucket `{bucket}`")
            }
            Some(n) if n > 0 => format!("read up to {n} object(s) from bucket `{bucket}`"),
            _ => format!("read objects from bucket `{bucket}`"),
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: CollectInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        if !self.allowlist.contains(&params.bucket) {
            return Ok(ToolResult::failure(
                self.allowlist.deny_message(&params.bucket),
                elapsed(start),
            ));
        }
        let expansion = match expand(
            params.keys,
            params.path_columns,
            params.template,
            params.dimensions,
        ) {
            Ok(e) => e,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        let files_total = expansion.targets.len();

        let ctx = SessionContext::capture();
        let Some(agent_id) = ctx.agent_id.clone().filter(|s| !s.is_empty()) else {
            return Ok(ToolResult::failure(
                "no agent context in scope; cannot stage the collected CSV",
                elapsed(start),
            ));
        };
        let Some(store) = global_store() else {
            return Ok(ToolResult::failure(
                "artifact store is not configured; cannot stage the collected CSV",
                elapsed(start),
            ));
        };

        if let Some(f) = &params.filter {
            if f.column.trim().is_empty() || f.values.is_empty() {
                return Ok(ToolResult::failure(
                    "filter requires a non-empty `column` and at least one value",
                    elapsed(start),
                ));
            }
        }
        let filtering = params.filter.is_some();
        let producer = CollectProducer {
            backend: Arc::clone(&self.backend),
            ctx: Arc::new(ctx),
            bucket: params.bucket.clone(),
            targets: expansion.targets,
            metadata_columns: expansion.metadata_columns,
            per_file_cap: self.max_get_bytes,
            on_error: params.on_error,
            filter: params.filter,
            sheet: params.sheet,
            report: Mutex::new(CollectReport::default()),
        };
        if filtering {
            let cap = params
                .max_rows
                .map_or(MAX_INLINE_ROWS, |m| m.clamp(1, MAX_INLINE_ROWS));
            Ok(self
                .deliver_filtered(producer, store.as_ref(), &agent_id, files_total, cap, start)
                .await)
        } else {
            let default_rows = StreamCaps::default().max_rows;
            let max_rows = params
                .max_rows
                .map_or(default_rows, |m| m.clamp(1, default_rows));
            Ok(self
                .deliver_download(
                    producer,
                    store.as_ref(),
                    &agent_id,
                    files_total,
                    max_rows,
                    start,
                )
                .await)
        }
    }
}

impl AwsS3CollectTool {
    /// Full-concatenation mode: stream to disk via the export kernel and deliver
    /// a download for the human. Memory-safe by streaming; the bytes never enter
    /// the agent context.
    async fn deliver_download(
        &self,
        producer: CollectProducer,
        store: &dyn ArtifactStore,
        agent_id: &str,
        files_total: usize,
        max_rows: usize,
        start: Instant,
    ) -> ToolResult {
        let bucket = producer.bucket.clone();
        let caps = StreamCaps {
            max_rows,
            ..StreamCaps::default()
        };
        let export = match export_csv(store, agent_id, self.name(), caps, &producer).await {
            Ok(e) => e,
            Err(err) => return ToolResult::failure(err.to_string(), elapsed(start)),
        };
        let report = std::mem::take(&mut *producer.report.lock());
        let duration_ms = elapsed(start);
        info!(
            target: "aws.s3",
            op = "collect",
            bucket = %bucket,
            files_total,
            files_included = report.included,
            files_skipped = report.skipped.len(),
            rows = export.rows,
            bytes = export.bytes,
            truncated = export.truncated,
            duration_ms,
            "S3 collect",
        );
        let files_not_reached = files_total.saturating_sub(report.included + report.skipped.len());
        let mut output = json!({
            "bucket": bucket,
            "kind": "collect",
            "rows": export.rows,
            "bytes": export.bytes,
            "truncated": export.truncated,
            "files_total": files_total,
            "files_included": report.included,
            "files_not_reached": files_not_reached,
            "files_skipped": serde_json::to_value(&report.skipped).unwrap_or_default(),
            "schema_mismatches": serde_json::to_value(&report.mismatches).unwrap_or_default(),
        });
        let map = output.as_object_mut().expect("json object");
        if export.rows > 0 {
            let filename = download_filename(&format!("{bucket}-collect"), "csv");
            map.insert(
                GENERATED_IMAGES_KEY.to_string(),
                json!([download_marker(
                    &export.artifact_id,
                    "text/csv",
                    &filename,
                    export.bytes
                )]),
            );
            map.insert("note".to_string(), json!(collect_note(export.truncated)));
        } else {
            map.insert(
                "note".to_string(),
                json!("0 rows collected; see `files_skipped` for why. No file was delivered."),
            );
        }
        ToolResult::success(output, duration_ms)
    }

    /// Search mode: keep only matching rows in memory (capped), then stage them
    /// via the data-plane kernel so they return inline (small) or as a queryable
    /// `tabular_artifact` (large) — never as a re-ingestible download.
    async fn deliver_filtered(
        &self,
        producer: CollectProducer,
        store: &dyn ArtifactStore,
        agent_id: &str,
        files_total: usize,
        cap: usize,
        start: Instant,
    ) -> ToolResult {
        let bucket = producer.bucket.clone();
        let mut sink = InlineSink::new(cap);
        if let Err(err) = producer.produce(&mut sink).await {
            return ToolResult::failure(err.to_string(), elapsed(start));
        }
        let report = std::mem::take(&mut *producer.report.lock());
        let truncated = sink.truncated;
        let columns = sink.columns;
        let objects = zip_table(&columns, sink.rows);
        let matched = objects.len();
        // No header is established when every file was skipped or empty; pass
        // `None` so staging derives an empty schema rather than erroring on an
        // empty column list — the run is a clean 0-match, not a failure.
        let col_arg = (!columns.is_empty()).then_some(columns);
        let staged = match stage_table(
            store,
            agent_id,
            self.name(),
            col_arg,
            objects,
            DEFAULT_INLINE_THRESHOLD,
        ) {
            Ok(s) => s,
            Err(err) => return ToolResult::failure(err.to_string(), elapsed(start)),
        };
        let result = staged_to_output(staged, self.name());
        let files_not_reached = files_total.saturating_sub(report.included + report.skipped.len());
        let duration_ms = elapsed(start);
        info!(
            target: "aws.s3",
            op = "collect",
            mode = "filter",
            bucket = %bucket,
            files_total,
            files_included = report.included,
            matched,
            column_found = report.filter_column_found,
            truncated,
            duration_ms,
            "S3 collect (filter)",
        );
        ToolResult::success(
            json!({
                "bucket": bucket,
                "kind": "collect",
                "matched_rows": matched,
                "filter_column_found": report.filter_column_found,
                "result": result,
                "files_total": files_total,
                "files_included": report.included,
                "files_not_reached": files_not_reached,
                "files_skipped": serde_json::to_value(&report.skipped).unwrap_or_default(),
                "schema_mismatches": serde_json::to_value(&report.mismatches).unwrap_or_default(),
                "truncated": truncated,
                "note": filter_note(&report, matched, truncated),
            }),
            duration_ms,
        )
    }
}

/// The agent-facing reading of a filtered run — distinguishing the three
/// false-negative traps (column absent, truncated/inconclusive, genuine zero)
/// from a real hit so an existence query is never reported as a lie.
fn filter_note(report: &CollectReport, matched: usize, truncated: bool) -> String {
    let scanned = report.included;
    if scanned == 0 {
        format!(
            "No files could be read ({} skipped — see files_skipped); the search did not actually \
             run, so this is NOT a 'not found'. Fix the keys/template and re-run.",
            report.skipped.len()
        )
    } else if !report.filter_column_found {
        format!(
            "FILTER COLUMN NOT FOUND in any of the {scanned} files scanned — a column-name/schema \
             mismatch, NOT a 'not found'. Source columns are {:?}. The filter matches source \
             columns, not the file_* metadata; narrow trust/date via keys/template.",
            report.data_columns
        )
    } else if truncated {
        format!(
            "TRUNCATED at {matched} matching rows (the cap) — files past it were not scanned, so a \
             non-match here is INCONCLUSIVE. Narrow the filter (fewer values, one trust/month) and re-run."
        )
    } else if matched == 0 {
        format!(
            "The filter column was present across {scanned} files but 0 rows matched — genuinely not found."
        )
    } else {
        format!("{matched} matching row(s) across {scanned} files; inline and queryable via data.query.")
    }
}

fn collect_note(truncated: bool) -> &'static str {
    if truncated {
        "The collected CSV was delivered out-of-band, but it hit the output cap and is TRUNCATED. Narrow the request (e.g. one month at a time) and re-run to capture the rest. You will NOT see its contents; tell the user the (partial) file is ready."
    } else {
        "The collected CSV was streamed to a downloadable file and delivered out-of-band — it happens outside your context, so you will NOT see its contents. Tell the user the file is ready; do not try to read or re-upload it."
    }
}

/// Drives the bounded-concurrency fetch and feeds parsed rows into the sink.
struct CollectProducer {
    backend: Arc<dyn S3Backend>,
    ctx: Arc<SessionContext>,
    bucket: String,
    targets: Vec<Target>,
    metadata_columns: Vec<String>,
    per_file_cap: u64,
    on_error: OnError,
    filter: Option<RowFilter>,
    sheet: Option<String>,
    report: Mutex<CollectReport>,
}

#[async_trait]
impl RowProducer for CollectProducer {
    async fn produce(&self, sink: &mut (dyn RowSink + Send)) -> Result<()> {
        let mut fetches = stream::iter(0..self.targets.len())
            .map(|idx| {
                let backend = Arc::clone(&self.backend);
                let ctx = Arc::clone(&self.ctx);
                let bucket = self.bucket.clone();
                let key = self.targets[idx].key.clone();
                let cap = self.per_file_cap;
                async move {
                    let res = backend.get(ctx.as_ref(), &bucket, &key, cap).await;
                    (idx, res)
                }
            })
            .buffered(COLLECT_CONCURRENCY);

        let mut included = 0usize;
        let mut skipped: Vec<Skipped> = Vec::new();
        let mut mismatches: Vec<Mismatch> = Vec::new();
        let mut data_columns: Vec<String> = Vec::new();
        let mut header_set = false;
        let mut filter_column_found = false;

        'consume: while let Some((idx, res)) = fetches.next().await {
            let target = &self.targets[idx];
            let parsed = match res {
                Ok(resp) => match classify(resp.content_type.as_deref(), &target.key) {
                    ContentKind::Spreadsheet => {
                        parse_spreadsheet(&resp.body, self.sheet.as_deref())
                            .map(|p| (p.columns, p.rows))
                    }
                    _ => parse_csv(&resp.body),
                }
                .map_err(|e| e.to_string()),
                Err(err) => Err(err.to_string()),
            };
            let (columns, rows) = match parsed {
                Ok(parsed) => parsed,
                Err(reason) => {
                    if self.on_error == OnError::Fail {
                        return Err(eyre!("aws.s3.collect failed on {}: {reason}", target.key));
                    }
                    skipped.push(Skipped {
                        key: target.key.clone(),
                        reason,
                    });
                    continue;
                }
            };
            included += 1;
            if let Some(f) = &self.filter {
                if columns.iter().any(|c| c == &f.column) {
                    filter_column_found = true;
                }
            }
            if columns.is_empty() {
                continue;
            }
            if !header_set {
                data_columns = columns;
                sink.columns(&header(&self.metadata_columns, &data_columns))?;
                header_set = true;
            } else if columns != data_columns {
                mismatches.push(Mismatch {
                    key: target.key.clone(),
                    columns,
                });
            }
            for row in &rows {
                if self.filter.as_ref().is_some_and(|f| !f.matches(row)) {
                    continue;
                }
                if sink.push(cells(&target.metadata, row, &data_columns))? == Flow::Stop {
                    break 'consume;
                }
            }
        }

        *self.report.lock() = CollectReport {
            included,
            skipped,
            mismatches,
            filter_column_found,
            data_columns,
        };
        Ok(())
    }
}

fn header(metadata_columns: &[String], data_columns: &[String]) -> Vec<String> {
    metadata_columns
        .iter()
        .chain(data_columns)
        .cloned()
        .collect()
}

/// Build one output record: the file's metadata cells, then its data cells
/// projected onto the established `data_columns` (missing → null).
fn cells(metadata: &[Value], row: &Value, data_columns: &[String]) -> Vec<Value> {
    let mut out = metadata.to_vec();
    match row {
        Value::Object(map) => out.extend(
            data_columns
                .iter()
                .map(|col| map.get(col).cloned().unwrap_or(Value::Null)),
        ),
        _ => out.extend(data_columns.iter().map(|_| Value::Null)),
    }
    out
}

/// Accumulates the producer's (already filtered) rows in memory for the search
/// path, capped at `cap` — at which point it stops the scan and records
/// truncation. Positional cells align to `columns` (the full header).
struct InlineSink {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
    cap: usize,
    truncated: bool,
}

impl InlineSink {
    fn new(cap: usize) -> Self {
        Self {
            columns: Vec::new(),
            rows: Vec::new(),
            cap,
            truncated: false,
        }
    }
}

impl RowSink for InlineSink {
    fn columns(&mut self, names: &[String]) -> Result<()> {
        self.columns = names.to_vec();
        Ok(())
    }

    fn push(&mut self, cells: Vec<Value>) -> Result<Flow> {
        if self.rows.len() >= self.cap {
            self.truncated = true;
            return Ok(Flow::Stop);
        }
        self.rows.push(cells);
        Ok(Flow::Continue)
    }
}

/// The number of objects a collect call would target, derived cheaply from the
/// raw input without expanding the key set: the `keys` array length, or the
/// product of the `dimensions` value-list lengths (template mode). `None` when
/// the count isn't determinable; arithmetic overflow saturates to `usize::MAX`
/// (an over-cap request the caller flags rather than prints).
fn collect_object_count(input: &Value) -> Option<usize> {
    if let Some(keys) = input.get("keys").and_then(Value::as_array) {
        return Some(keys.len());
    }
    let dims = input.get("dimensions").and_then(Value::as_object)?;
    let lengths: Vec<usize> = dims
        .values()
        .map(|v| v.as_array().map_or(0, Vec::len))
        .collect();
    if lengths.is_empty() || lengths.contains(&0) {
        return None;
    }
    Some(
        lengths
            .into_iter()
            .try_fold(1usize, |acc, n| acc.checked_mul(n))
            .unwrap_or(usize::MAX),
    )
}

fn elapsed(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::client::{GetResponse, HeadResponse, ListResponse, S3Error};
    use bytes::Bytes;
    use workforce_agent::artifacts::{ArtifactBody, ArtifactStore};

    struct MockBackend {
        objects: HashMap<String, Result<(Bytes, String), S3Error>>,
    }

    impl MockBackend {
        fn new(objects: &[(&str, Result<&str, S3Error>)]) -> Self {
            Self {
                objects: objects
                    .iter()
                    .map(|(k, v)| {
                        let body = v
                            .clone()
                            .map(|s| (Bytes::from(s.to_string()), "text/csv".to_string()));
                        (k.to_string(), body)
                    })
                    .collect(),
            }
        }

        /// Add a raw (binary) object — spreadsheet fixtures are served the way
        /// S3 serves them in the wild: `application/octet-stream`.
        fn with_raw(mut self, key: &str, body: Bytes) -> Self {
            self.objects.insert(
                key.to_string(),
                Ok((body, "application/octet-stream".to_string())),
            );
            self
        }
    }

    #[async_trait]
    impl S3Backend for MockBackend {
        async fn list(
            &self,
            _ctx: &SessionContext,
            _bucket: &str,
            _prefix: Option<&str>,
            _delimiter: Option<&str>,
            _token: Option<&str>,
            _max_keys: u16,
        ) -> Result<ListResponse, S3Error> {
            unreachable!()
        }

        async fn head(
            &self,
            _ctx: &SessionContext,
            _bucket: &str,
            _key: &str,
        ) -> Result<HeadResponse, S3Error> {
            unreachable!()
        }

        async fn get(
            &self,
            _ctx: &SessionContext,
            _bucket: &str,
            key: &str,
            max_bytes: u64,
        ) -> Result<GetResponse, S3Error> {
            match self.objects.get(key) {
                None => Err(S3Error::NotFound),
                Some(Err(e)) => Err(e.clone()),
                Some(Ok((body, content_type))) => {
                    let observed = body.len() as u64;
                    if max_bytes != 0 && observed > max_bytes {
                        return Err(S3Error::TooLarge {
                            observed,
                            cap: max_bytes,
                        });
                    }
                    Ok(GetResponse {
                        body: body.clone(),
                        content_type: Some(content_type.clone()),
                        size: observed as i64,
                        etag: None,
                        last_modified: None,
                    })
                }
            }
        }

        async fn get_to_path(
            &self,
            _ctx: &SessionContext,
            _bucket: &str,
            _key: &str,
            _dest: &std::path::Path,
            _cap: u64,
        ) -> Result<u64, S3Error> {
            unreachable!()
        }
    }

    fn install_store() -> Arc<dyn ArtifactStore> {
        use std::sync::LazyLock;
        static DIR: LazyLock<tempfile::TempDir> = LazyLock::new(|| {
            let dir = tempfile::TempDir::new().expect("tempdir");
            workforce_agent::artifacts::set_global_store(
                workforce_agent::artifacts::FsArtifactStore::new(dir.path().to_path_buf())
                    .expect("artifact store"),
            );
            dir
        });
        LazyLock::force(&DIR);
        global_store().expect("store installed")
    }

    fn allowlist(names: &[&str]) -> Arc<Allowlist> {
        Arc::new(Allowlist::new(names.iter().map(|s| s.to_string())))
    }

    async fn run(tool: &AwsS3CollectTool, input: Value) -> ToolResult {
        workforce_agent::runner::CURRENT_AGENT_ID
            .scope("collect-agent".to_string(), tool.execute(input))
            .await
            .unwrap()
    }

    fn staged_csv(store: &dyn ArtifactStore, marker: &Value) -> String {
        use workforce_agent::artifacts::ArtifactId;
        let id = ArtifactId::parse(marker["artifact_id"].as_str().unwrap()).unwrap();
        match store.read("collect-agent", &id).unwrap() {
            ArtifactBody::Bytes(b) => String::from_utf8(b).unwrap(),
            other => panic!("expected bytes, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn collect_concatenates_with_metadata_columns_in_key_order() {
        let store = install_store();
        let backend = Arc::new(MockBackend::new(&[
            (
                "2026-02-01/NORTH/Sales/main.csv",
                Ok("name,role\nann,clerk\n"),
            ),
            (
                "2026-02-01/SOUTH/Support/main.csv",
                Ok("name,role\nbo,manager\n"),
            ),
        ]));
        let tool = AwsS3CollectTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = run(
            &tool,
            json!({
                "bucket": "b",
                "keys": [
                    "2026-02-01/NORTH/Sales/main.csv",
                    "2026-02-01/SOUTH/Support/main.csv"
                ],
                "path_columns": ["file_date", "file_region", "file_team"]
            }),
        )
        .await;
        assert!(res.success);
        assert_eq!(res.output["rows"], 2);
        assert_eq!(res.output["files_total"], 2);
        assert_eq!(res.output["files_included"], 2);
        assert!(res.output["files_skipped"].as_array().unwrap().is_empty());
        let marker = &res.output[GENERATED_IMAGES_KEY][0];
        assert_eq!(
            staged_csv(store.as_ref(), marker),
            "file_path,file_date,file_region,file_team,name,role\n\
             2026-02-01/NORTH/Sales/main.csv,2026-02-01,NORTH,Sales,ann,clerk\n\
             2026-02-01/SOUTH/Support/main.csv,2026-02-01,SOUTH,Support,bo,manager\n"
        );
    }

    #[tokio::test]
    async fn collect_template_mode_expands_and_labels() {
        let store = install_store();
        let backend = Arc::new(MockBackend::new(&[
            ("d1/NORTH/main.csv", Ok("v\n1\n")),
            ("d1/SOUTH/main.csv", Ok("v\n2\n")),
        ]));
        let tool = AwsS3CollectTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = run(
            &tool,
            json!({
                "bucket": "b",
                "template": "{file_date}/{file_region}/main.csv",
                "dimensions": { "file_date": ["d1"], "file_region": ["NORTH", "SOUTH"] }
            }),
        )
        .await;
        assert!(res.success);
        assert_eq!(res.output["rows"], 2);
        let marker = &res.output[GENERATED_IMAGES_KEY][0];
        assert_eq!(
            staged_csv(store.as_ref(), marker),
            "file_path,file_date,file_region,v\n\
             d1/NORTH/main.csv,d1,NORTH,1\n\
             d1/SOUTH/main.csv,d1,SOUTH,2\n"
        );
    }

    #[tokio::test]
    async fn collect_skips_missing_keys_and_reports_them() {
        install_store();
        let backend = Arc::new(MockBackend::new(&[("a.csv", Ok("c\n1\n"))]));
        let tool = AwsS3CollectTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = run(
            &tool,
            json!({ "bucket": "b", "keys": ["a.csv", "missing.csv"] }),
        )
        .await;
        assert!(res.success);
        assert_eq!(res.output["rows"], 1);
        assert_eq!(res.output["files_total"], 2);
        assert_eq!(res.output["files_included"], 1);
        let skipped = res.output["files_skipped"].as_array().unwrap();
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0]["key"], "missing.csv");
        assert!(skipped[0]["reason"].as_str().unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn collect_fail_mode_aborts_on_first_error() {
        install_store();
        let backend = Arc::new(MockBackend::new(&[("a.csv", Ok("c\n1\n"))]));
        let tool = AwsS3CollectTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = run(
            &tool,
            json!({ "bucket": "b", "keys": ["a.csv", "missing.csv"], "on_error": "fail" }),
        )
        .await;
        assert!(!res.success);
        assert!(res.error.unwrap().contains("missing.csv"));
    }

    #[tokio::test]
    async fn collect_header_only_first_file_defines_schema() {
        let store = install_store();
        let backend = Arc::new(MockBackend::new(&[
            ("header_only.csv", Ok("a,b\n")),
            ("real.csv", Ok("a,b\n1,2\n")),
        ]));
        let tool = AwsS3CollectTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = run(
            &tool,
            json!({ "bucket": "b", "keys": ["header_only.csv", "real.csv"] }),
        )
        .await;
        assert!(res.success);
        assert_eq!(res.output["rows"], 1);
        assert_eq!(res.output["files_included"], 2);
        assert!(res.output["schema_mismatches"]
            .as_array()
            .unwrap()
            .is_empty());
        let marker = &res.output[GENERATED_IMAGES_KEY][0];
        assert_eq!(
            staged_csv(store.as_ref(), marker),
            "file_path,a,b\nreal.csv,1,2\n",
            "a header-only file is a valid definer; the next file's row survives"
        );
    }

    #[tokio::test]
    async fn collect_empty_first_file_does_not_poison_schema() {
        let store = install_store();
        let backend = Arc::new(MockBackend::new(&[
            ("empty.csv", Ok("")),
            ("real.csv", Ok("a,b\n1,2\n")),
        ]));
        let tool = AwsS3CollectTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = run(
            &tool,
            json!({ "bucket": "b", "keys": ["empty.csv", "real.csv"] }),
        )
        .await;
        assert!(res.success);
        assert_eq!(res.output["rows"], 1);
        assert_eq!(res.output["files_included"], 2, "empty file is included");
        let marker = &res.output[GENERATED_IMAGES_KEY][0];
        assert_eq!(
            staged_csv(store.as_ref(), marker),
            "file_path,a,b\nreal.csv,1,2\n",
            "the real file's columns survive — not projected onto an empty header"
        );
    }

    #[tokio::test]
    async fn collect_projects_divergent_schema_and_reports_mismatch() {
        let store = install_store();
        let backend = Arc::new(MockBackend::new(&[
            ("first.csv", Ok("a,b\n1,2\n")),
            ("second.csv", Ok("a,c\n9,8\n")),
        ]));
        let tool = AwsS3CollectTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = run(
            &tool,
            json!({ "bucket": "b", "keys": ["first.csv", "second.csv"] }),
        )
        .await;
        assert!(res.success);
        assert_eq!(res.output["rows"], 2);
        let mismatches = res.output["schema_mismatches"].as_array().unwrap();
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0]["key"], "second.csv");
        let marker = &res.output[GENERATED_IMAGES_KEY][0];
        assert_eq!(
            staged_csv(store.as_ref(), marker),
            "file_path,a,b\nfirst.csv,1,2\nsecond.csv,9,\n",
            "second row keeps shared col a, drops unknown c, nulls missing b"
        );
    }

    #[tokio::test]
    async fn collect_skips_object_over_per_file_cap() {
        install_store();
        let backend = Arc::new(MockBackend::new(&[
            ("small.csv", Ok("c\n1\n")),
            ("big.csv", Ok("c\nxxxxxxxxxxxxxxxxxxxx\n")),
        ]));
        let tool = AwsS3CollectTool::new(backend, allowlist(&["b"]), 10);
        let res = run(
            &tool,
            json!({ "bucket": "b", "keys": ["small.csv", "big.csv"] }),
        )
        .await;
        assert!(res.success);
        let skipped = res.output["files_skipped"].as_array().unwrap();
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0]["key"], "big.csv");
        assert!(skipped[0]["reason"]
            .as_str()
            .unwrap()
            .contains("max_get_bytes"));
    }

    #[tokio::test]
    async fn collect_zero_rows_emits_no_download_marker() {
        install_store();
        let backend = Arc::new(MockBackend::new(&[]));
        let tool = AwsS3CollectTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = run(&tool, json!({ "bucket": "b", "keys": ["gone.csv"] })).await;
        assert!(res.success);
        assert_eq!(res.output["rows"], 0);
        assert!(res.output.get(GENERATED_IMAGES_KEY).is_none());
        assert_eq!(res.output["files_skipped"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn collect_truncates_at_row_cap() {
        let store = install_store();
        let backend = Arc::new(MockBackend::new(&[("a.csv", Ok("c\n1\n2\n3\n4\n5\n"))]));
        let tool = AwsS3CollectTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = run(
            &tool,
            json!({ "bucket": "b", "keys": ["a.csv"], "max_rows": 2 }),
        )
        .await;
        assert!(res.success);
        assert_eq!(res.output["rows"], 2);
        assert_eq!(res.output["truncated"], true);
        let marker = &res.output[GENERATED_IMAGES_KEY][0];
        assert_eq!(
            staged_csv(store.as_ref(), marker),
            "file_path,c\na.csv,1\na.csv,2\n"
        );
    }

    #[tokio::test]
    async fn collect_truncation_reports_unreached_files() {
        install_store();
        let backend = Arc::new(MockBackend::new(&[
            ("a.csv", Ok("c\n1\n2\n3\n")),
            ("b.csv", Ok("c\n4\n")),
            ("c.csv", Ok("c\n5\n")),
        ]));
        let tool = AwsS3CollectTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = run(
            &tool,
            json!({ "bucket": "b", "keys": ["a.csv", "b.csv", "c.csv"], "max_rows": 2 }),
        )
        .await;
        assert!(res.success);
        assert_eq!(res.output["truncated"], true);
        assert_eq!(res.output["files_total"], 3);
        assert_eq!(res.output["files_included"], 1, "only a.csv was reached");
        assert_eq!(
            res.output["files_not_reached"], 2,
            "b.csv and c.csv were never fetched once the cap was hit"
        );
        assert!(res.output["files_skipped"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn collect_refuses_unlisted_bucket_without_io() {
        install_store();
        let backend = Arc::new(MockBackend::new(&[("a.csv", Ok("c\n1\n"))]));
        let tool = AwsS3CollectTool::new(backend, allowlist(&["allowed"]), 1_000_000);
        let res = run(&tool, json!({ "bucket": "denied", "keys": ["a.csv"] })).await;
        assert!(!res.success);
        assert!(res.error.unwrap().contains("not allowlisted"));
    }

    #[tokio::test]
    async fn collect_rejects_both_input_modes() {
        install_store();
        let backend = Arc::new(MockBackend::new(&[]));
        let tool = AwsS3CollectTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = run(
            &tool,
            json!({ "bucket": "b", "keys": ["a.csv"], "template": "{x}/main.csv" }),
        )
        .await;
        assert!(!res.success);
    }

    #[tokio::test]
    async fn collect_requires_agent_context() {
        install_store();
        let backend = Arc::new(MockBackend::new(&[("a.csv", Ok("c\n1\n"))]));
        let tool = AwsS3CollectTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = tool
            .execute(json!({ "bucket": "b", "keys": ["a.csv"] }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("no agent context"));
    }

    #[test]
    fn cell_matches_normalizes_quote_and_numeric_leading_zero() {
        assert!(cell_matches("'0626051698", "626051698"));
        assert!(cell_matches("0626051698", "626051698"));
        assert!(cell_matches("626051698", "0626051698"));
        assert!(cell_matches("626051698", "626051698"));
        assert!(!cell_matches("626051699", "626051698"));
        // leading-zero equivalence is numeric-only — non-numeric codes stay distinct
        assert!(!cell_matches("0526", "526a"));
        assert!(cell_matches("ABC", "ABC"));
        assert!(!cell_matches("0ABC", "ABC"));
    }

    #[tokio::test]
    async fn collect_filter_returns_matching_rows_inline_not_a_download() {
        install_store();
        let backend = Arc::new(MockBackend::new(&[
            (
                "2026-06-05/NORTH/main.csv",
                Ok("Request Id,Status\n626051698,UnFilled\n626051699,Filled\n"),
            ),
            (
                "2026-06-06/NORTH/main.csv",
                Ok("Request Id,Status\n626051700,UnFilled\n"),
            ),
        ]));
        let tool = AwsS3CollectTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = run(
            &tool,
            json!({
                "bucket": "b",
                "keys": ["2026-06-05/NORTH/main.csv", "2026-06-06/NORTH/main.csv"],
                "path_columns": ["file_date", "file_region"],
                "filter": { "column": "Request Id", "values": ["626051698"] }
            }),
        )
        .await;
        assert!(res.success);
        assert_eq!(res.output["matched_rows"], 1);
        assert_eq!(res.output["filter_column_found"], true);
        assert_eq!(res.output["files_included"], 2);
        assert!(
            res.output.get(GENERATED_IMAGES_KEY).is_none(),
            "filter mode never delivers a re-ingestible download"
        );
        let rendered = res.output["result"].to_string();
        assert!(
            rendered.contains("626051698"),
            "matching row is returned: {rendered}"
        );
        assert!(
            rendered.contains("2026-06-05"),
            "with its file metadata: {rendered}"
        );
        assert!(
            !rendered.contains("626051699"),
            "non-matching rows excluded"
        );
    }

    #[tokio::test]
    async fn collect_filter_matches_quote_and_leading_zero_variants() {
        install_store();
        let backend = Arc::new(MockBackend::new(&[(
            "f.csv",
            Ok("Request Id,Status\n'0626051698,UnFilled\n"),
        )]));
        let tool = AwsS3CollectTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = run(
            &tool,
            json!({ "bucket": "b", "keys": ["f.csv"],
                    "filter": { "column": "Request Id", "values": ["626051698"] } }),
        )
        .await;
        assert!(res.success);
        assert_eq!(
            res.output["matched_rows"], 1,
            "quote+leading-zero cell still matches"
        );
    }

    #[tokio::test]
    async fn collect_filter_column_absent_is_distinct_from_zero_matches() {
        install_store();
        let backend = Arc::new(MockBackend::new(&[(
            "f.csv",
            Ok("Request Id,Status\n626051698,UnFilled\n"),
        )]));
        let tool = AwsS3CollectTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = run(
            &tool,
            json!({ "bucket": "b", "keys": ["f.csv"],
                    "filter": { "column": "file_region", "values": ["NORTH"] } }),
        )
        .await;
        assert!(res.success);
        assert_eq!(res.output["matched_rows"], 0);
        assert_eq!(res.output["filter_column_found"], false);
        assert!(res.output["note"].as_str().unwrap().contains("NOT FOUND"));
    }

    #[tokio::test]
    async fn collect_filter_genuine_zero_match_is_not_found() {
        install_store();
        let backend = Arc::new(MockBackend::new(&[(
            "f.csv",
            Ok("Request Id,Status\n626051698,UnFilled\n"),
        )]));
        let tool = AwsS3CollectTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = run(
            &tool,
            json!({ "bucket": "b", "keys": ["f.csv"],
                    "filter": { "column": "Request Id", "values": ["999999999"] } }),
        )
        .await;
        assert!(res.success);
        assert_eq!(res.output["matched_rows"], 0);
        assert_eq!(res.output["filter_column_found"], true);
        assert!(res.output["note"]
            .as_str()
            .unwrap()
            .contains("genuinely not found"));
    }

    #[tokio::test]
    async fn collect_filter_resolves_column_per_file() {
        install_store();
        // A delta-style file lacking the column contributes no matches but
        // does not error; the main file with the column does.
        let backend = Arc::new(MockBackend::new(&[
            ("delta.csv", Ok("Other\nx\n")),
            ("main.csv", Ok("Request Id\n626051698\n")),
        ]));
        let tool = AwsS3CollectTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = run(
            &tool,
            json!({ "bucket": "b", "keys": ["delta.csv", "main.csv"],
                    "filter": { "column": "Request Id", "values": ["626051698"] } }),
        )
        .await;
        assert!(res.success);
        assert_eq!(res.output["matched_rows"], 1);
        assert_eq!(res.output["filter_column_found"], true);
    }

    #[tokio::test]
    async fn collect_filter_truncation_is_inconclusive() {
        install_store();
        let backend = Arc::new(MockBackend::new(&[(
            "f.csv",
            Ok("Request Id\n100\n100\n100\n"),
        )]));
        let tool = AwsS3CollectTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = run(
            &tool,
            json!({ "bucket": "b", "keys": ["f.csv"], "max_rows": 2,
                    "filter": { "column": "Request Id", "values": ["100"] } }),
        )
        .await;
        assert!(res.success);
        assert_eq!(res.output["matched_rows"], 2);
        assert_eq!(res.output["truncated"], true);
        let note = res.output["note"].as_str().unwrap();
        assert!(note.contains("INCONCLUSIVE"));
        assert!(
            note.contains("at 2 matching"),
            "note reports the actual cap reached (2), not the hardcoded max: {note}"
        );
    }

    #[tokio::test]
    async fn collect_filter_all_files_skipped_is_clean_zero_not_an_error() {
        install_store();
        // Every key 404s (e.g. a wrong template) → no header established. Must
        // return a clean 0-match result, not a leaked staging error.
        let backend = Arc::new(MockBackend::new(&[]));
        let tool = AwsS3CollectTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = run(
            &tool,
            json!({ "bucket": "b", "keys": ["gone1.csv", "gone2.csv"],
                    "filter": { "column": "Request Id", "values": ["1"] } }),
        )
        .await;
        assert!(res.success, "all-skipped must not error: {:?}", res.error);
        assert_eq!(res.output["matched_rows"], 0);
        assert_eq!(res.output["files_included"], 0);
        assert_eq!(res.output["files_skipped"].as_array().unwrap().len(), 2);
        assert!(res.output["note"]
            .as_str()
            .unwrap()
            .contains("did not actually"));
    }

    #[tokio::test]
    async fn collect_filter_rejects_empty_values() {
        install_store();
        let backend = Arc::new(MockBackend::new(&[("f.csv", Ok("Request Id\n1\n"))]));
        let tool = AwsS3CollectTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = run(
            &tool,
            json!({ "bucket": "b", "keys": ["f.csv"],
                    "filter": { "column": "Request Id", "values": [] } }),
        )
        .await;
        assert!(!res.success);
        assert!(res.error.unwrap().contains("non-empty"));
    }

    fn report_workbook(sheet: &str) -> Bytes {
        let sheet = sheet.to_string();
        crate::aws::test_workbook(move |wb| {
            let ws = wb.add_worksheet();
            ws.set_name(&sheet).unwrap();
            ws.write_string(0, 0, "Request Id").unwrap();
            ws.write_string(0, 1, "Status").unwrap();
            ws.write_number(1, 0, 826281843.0).unwrap();
            ws.write_string(1, 1, "UnFilled").unwrap();
        })
    }

    #[tokio::test]
    async fn collect_mixes_csv_and_xlsx_files() {
        let store = install_store();
        let backend = Arc::new(
            MockBackend::new(&[("d1/main.csv", Ok("Request Id,Status\n111,Filled\n"))])
                .with_raw("d2/report.xlsx", report_workbook("Sheet1")),
        );
        let tool = AwsS3CollectTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = run(
            &tool,
            json!({ "bucket": "b", "keys": ["d1/main.csv", "d2/report.xlsx"],
                    "path_columns": ["file_date"] }),
        )
        .await;
        assert!(res.success, "{:?}", res.error);
        assert_eq!(res.output["rows"], 2);
        assert_eq!(res.output["files_included"], 2);
        assert!(res.output["schema_mismatches"]
            .as_array()
            .unwrap()
            .is_empty());
        let marker = &res.output[GENERATED_IMAGES_KEY][0];
        assert_eq!(
            staged_csv(store.as_ref(), marker),
            "file_path,file_date,Request Id,Status\n\
             d1/main.csv,d1,111,Filled\n\
             d2/report.xlsx,d2,826281843,UnFilled\n"
        );
    }

    #[tokio::test]
    async fn collect_filter_finds_numeric_xlsx_cell_by_leading_zero_search() {
        // The archive search end to end: the ID is stored as a NUMBER in the
        // workbook (Excel dropped the leading zero); the human searches with it.
        install_store();
        let backend = Arc::new(
            MockBackend::new(&[]).with_raw("raw/chunk-00.xlsx", report_workbook("Sheet1")),
        );
        let tool = AwsS3CollectTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = run(
            &tool,
            json!({ "bucket": "b", "keys": ["raw/chunk-00.xlsx"],
                    "filter": { "column": "Request Id", "values": ["0826281843"] } }),
        )
        .await;
        assert!(res.success, "{:?}", res.error);
        assert_eq!(res.output["matched_rows"], 1);
        assert_eq!(res.output["filter_column_found"], true);
        assert!(res.output["result"].to_string().contains("826281843"));
    }

    #[tokio::test]
    async fn collect_missing_named_sheet_skips_the_file_with_its_inventory() {
        install_store();
        let backend = Arc::new(
            MockBackend::new(&[])
                .with_raw("a.xlsx", report_workbook("Unfilled"))
                .with_raw("b.xlsx", report_workbook("Data")),
        );
        let tool = AwsS3CollectTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = run(
            &tool,
            json!({ "bucket": "b", "keys": ["a.xlsx", "b.xlsx"], "sheet": "Unfilled" }),
        )
        .await;
        assert!(res.success);
        assert_eq!(res.output["rows"], 1);
        assert_eq!(res.output["files_included"], 1);
        let skipped = res.output["files_skipped"].as_array().unwrap();
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0]["key"], "b.xlsx");
        let reason = skipped[0]["reason"].as_str().unwrap();
        assert!(reason.contains("\"Unfilled\" not found"), "{reason}");
        assert!(reason.contains("Data"), "{reason}");
    }

    fn summary_tool() -> AwsS3CollectTool {
        AwsS3CollectTool::new(
            Arc::new(MockBackend::new(&[])),
            allowlist(&["b"]),
            1_000_000,
        )
    }

    #[test]
    fn approval_summary_keys_mode_reports_count_and_bucket() {
        let s = summary_tool()
            .approval_summary(&json!({ "bucket": "b", "keys": ["x.csv", "y.csv", "z.csv"] }))
            .unwrap();
        assert_eq!(s, "read up to 3 object(s) from bucket `b`");
    }

    #[test]
    fn approval_summary_template_mode_reports_cartesian_product() {
        let s = summary_tool()
            .approval_summary(&json!({
                "bucket": "b",
                "template": "{d}/{t}/main.csv",
                "dimensions": { "d": ["d1", "d2"], "t": ["NORTH", "SOUTH", "EAST"] }
            }))
            .unwrap();
        assert_eq!(s, "read up to 6 object(s) from bucket `b`");
    }

    #[test]
    fn approval_summary_flags_over_cap_request() {
        let many = |n: usize| Value::Array((0..n).map(|i| json!(i.to_string())).collect());
        let s = summary_tool()
            .approval_summary(&json!({
                "bucket": "b",
                "template": "{a}/{b}",
                "dimensions": { "a": many(300), "b": many(200) }
            }))
            .unwrap();
        assert_eq!(s, "read over 50000 objects from bucket `b`");
    }

    #[test]
    fn approval_summary_without_determinable_scope_is_generic() {
        let s = summary_tool()
            .approval_summary(&json!({ "bucket": "b" }))
            .unwrap();
        assert_eq!(s, "read objects from bucket `b`");
    }

    #[test]
    fn approval_summary_requires_a_bucket() {
        assert!(summary_tool()
            .approval_summary(&json!({ "keys": ["x.csv"] }))
            .is_none());
    }

    #[test]
    fn collect_object_count_handles_keys_template_and_empty_dimension() {
        assert_eq!(
            collect_object_count(&json!({ "keys": ["a", "b"] })),
            Some(2)
        );
        assert_eq!(
            collect_object_count(&json!({ "dimensions": { "a": ["1", "2"], "b": ["x"] } })),
            Some(2)
        );
        assert_eq!(
            collect_object_count(&json!({ "dimensions": { "a": [] } })),
            None,
            "an empty dimension is not a zero-object request, it's undeterminable"
        );
        assert_eq!(collect_object_count(&json!({ "bucket": "b" })), None);
    }
}
