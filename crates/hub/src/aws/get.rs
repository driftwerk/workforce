//! `aws.s3.get` — download object body, route by content type.
//!
//! A `head_object` pre-check sizes the object: at or below `max_get_bytes` the
//! body is fetched and inlined; above it the object is streamed straight to a
//! downloadable file (never buffered in memory) and delivered out-of-band, up
//! to `MAX_DOWNLOAD_BYTES` past which even a download is refused. The inline
//! read enforces `max_get_bytes` as belt-and-braces against a lying
//! `Content-Length`, and the streamed download enforces the ceiling the same
//! way. `max_get_bytes == 0` disables the inline cap entirely (operator opt-in;
//! OOM is then on them). CSV and spreadsheet bodies are decoded into rows so the
//! existing tabular-spill machinery (`agent_runtime::spill::spill_tool_output`)
//! can stage them under the artifact store, keeping PII rows out of the raw
//! transcript.
//! Note that text/JSON bodies inline into the transcript and do NOT spill, so
//! the cap is the only thing protecting the context window for those kinds.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::info;

use workforce_agent::artifacts::{
    download_filename, download_marker, global_store, stage_file, staging_tempfile,
};
use workforce_agent::runner::SessionContext;
use workforce_agent::tools::image_gen::GENERATED_IMAGES_KEY;
use workforce_agent::tools::{Tool, ToolResult};

use super::client::{Allowlist, GetResponse, HeadResponse, S3Backend};
use super::content::{classify, parse_csv, parse_spreadsheet, ContentKind};

/// Objects larger than `max_get_bytes` are streamed to a downloadable file
/// rather than inlined — up to this hard ceiling, past which even a download is
/// refused so a runaway object can't fill the disk.
const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Deserialize)]
struct GetInput {
    bucket: String,
    key: String,
    #[serde(default)]
    sheet: Option<String>,
}

pub struct AwsS3GetTool {
    backend: Arc<dyn S3Backend>,
    allowlist: Arc<Allowlist>,
    max_get_bytes: u64,
}

impl AwsS3GetTool {
    pub fn new(backend: Arc<dyn S3Backend>, allowlist: Arc<Allowlist>, max_get_bytes: u64) -> Self {
        Self {
            backend,
            allowlist,
            max_get_bytes,
        }
    }
}

#[async_trait]
impl Tool for AwsS3GetTool {
    fn name(&self) -> &str {
        "aws.s3.get"
    }

    fn description(&self) -> &str {
        "Download an object from an allowlisted S3 bucket and return its parsed body. \
         CSV and xlsx/xlsm workbooks (one sheet per call, `sheet` selects which) \
         → rows (staged into the data plane for `data.query` when large); \
         JSON/text → inline; other binary content is refused. \
         Objects larger than `max_get_bytes` are streamed to a download without inlining."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "bucket": { "type": "string", "description": "Bucket name (must be allowlisted)." },
                "key": { "type": "string", "description": "Object key." },
                "sheet": { "type": "string", "description": "Sheet name to read (spreadsheet objects only). Defaults to the first sheet; the result lists all sheet names." }
            },
            "required": ["bucket", "key"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: GetInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        if !self.allowlist.contains(&params.bucket) {
            return Ok(ToolResult::failure(
                self.allowlist.deny_message(&params.bucket),
                elapsed(start),
            ));
        }
        let ctx = SessionContext::capture();
        let head = match self.backend.head(&ctx, &params.bucket, &params.key).await {
            Ok(h) => h,
            Err(err) => return Ok(self.log_error("head", &params, &err, start)),
        };
        if self.max_get_bytes != 0 && head.size > 0 && (head.size as u64) > self.max_get_bytes {
            return Ok(self.deliver_download(&ctx, &params, &head, start).await);
        }
        let kind_pre = classify(head.content_type.as_deref(), &params.key);
        if kind_pre == ContentKind::Binary {
            let msg = binary_message(&head);
            log_binary(&params, &head, elapsed(start));
            return Ok(ToolResult::failure(msg, elapsed(start)));
        }
        let resp = match self
            .backend
            .get(&ctx, &params.bucket, &params.key, self.max_get_bytes)
            .await
        {
            Ok(r) => r,
            Err(err) => return Ok(self.log_error("get", &params, &err, start)),
        };
        let GetResponse {
            body,
            content_type,
            size: _,
            etag: _,
            last_modified: _,
        } = resp;
        let kind = classify(content_type.as_deref(), &params.key);
        let duration_ms = elapsed(start);
        let bytes_out = body.len();
        let ct_log = content_type.clone().unwrap_or_default();
        let output = match decode_body(kind, &params, content_type, body) {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), duration_ms)),
        };
        info!(
            target: "aws.s3",
            op = "get",
            bucket = %params.bucket,
            key = %params.key,
            bytes_out,
            content_type = %ct_log,
            duration_ms,
            outcome = "ok",
            "S3 get",
        );
        Ok(ToolResult::success(output, duration_ms))
    }
}

impl AwsS3GetTool {
    /// Stream an object too large to inline straight to a downloadable file and
    /// deliver it out-of-band, instead of refusing. The bytes never enter the
    /// agent context. Objects past [`MAX_DOWNLOAD_BYTES`] are still refused.
    async fn deliver_download(
        &self,
        ctx: &SessionContext,
        params: &GetInput,
        head: &HeadResponse,
        start: Instant,
    ) -> ToolResult {
        if (head.size as u64) > MAX_DOWNLOAD_BYTES {
            log_too_large(params, head, MAX_DOWNLOAD_BYTES, elapsed(start));
            return ToolResult::failure(over_ceiling_message(head.size), elapsed(start));
        }
        let Some(agent_id) = ctx.agent_id.clone().filter(|s| !s.is_empty()) else {
            return ToolResult::failure(
                "no agent context in scope; cannot stage the download",
                elapsed(start),
            );
        };
        let Some(store) = global_store() else {
            return ToolResult::failure(
                "artifact store is not configured; cannot stage the download",
                elapsed(start),
            );
        };
        let temp = match staging_tempfile(store.as_ref(), &agent_id, "wf-s3-", ".bin") {
            Ok(t) => t.into_temp_path(),
            Err(e) => {
                return ToolResult::failure(
                    format!("cannot create download file: {e}"),
                    elapsed(start),
                )
            }
        };
        if let Err(e) = self
            .backend
            .get_to_path(ctx, &params.bucket, &params.key, &temp, MAX_DOWNLOAD_BYTES)
            .await
        {
            return self.log_error("get", params, &e, start);
        }
        let path = match temp.keep() {
            Ok(path) => path,
            Err(e) => {
                return ToolResult::failure(
                    format!("cannot retain download file: {e}"),
                    elapsed(start),
                )
            }
        };
        let (id, bytes) = match stage_file(store.as_ref(), &agent_id, "aws.s3.get", &path, None) {
            Ok(staged) => staged,
            Err(e) => {
                return ToolResult::failure(format!("cannot stage download: {e}"), elapsed(start))
            }
        };
        let filename = download_basename(&params.key);
        let media_type = head
            .content_type
            .clone()
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let duration_ms = elapsed(start);
        info!(
            target: "aws.s3",
            op = "get",
            bucket = %params.bucket,
            key = %params.key,
            bytes_out = bytes,
            duration_ms,
            outcome = "download",
            "S3 get streamed to download",
        );
        ToolResult::success(
            json!({
                GENERATED_IMAGES_KEY: [download_marker(&id, &media_type, &filename, bytes)],
                "bucket": params.bucket,
                "key": params.key,
                "kind": "download",
                "bytes": bytes,
                "note": format!(
                    "{} ({bytes} bytes) was too large to inline, so it was streamed to a \
                     downloadable file and delivered to this conversation — it happens outside your \
                     context, so you will NOT see its contents. Tell the user the file is ready; do \
                     not try to read or re-upload it.",
                    params.key
                ),
            }),
            duration_ms,
        )
    }

    fn log_error(
        &self,
        op: &'static str,
        params: &GetInput,
        err: &super::client::S3Error,
        start: Instant,
    ) -> ToolResult {
        let duration_ms = elapsed(start);
        info!(
            target: "aws.s3",
            op = op,
            bucket = %params.bucket,
            key = %params.key,
            duration_ms,
            outcome = "error",
            error = %err,
            "S3 get failed",
        );
        ToolResult::failure(err.to_string(), duration_ms)
    }
}

fn decode_body(
    kind: ContentKind,
    params: &GetInput,
    content_type: Option<String>,
    body: bytes::Bytes,
) -> Result<Value> {
    match kind {
        ContentKind::Csv => {
            let (columns, rows) = parse_csv(&body)?;
            let row_count = rows.len();
            Ok(json!({
                "bucket": params.bucket,
                "key": params.key,
                "content_type": content_type.unwrap_or_else(|| "text/csv".to_string()),
                "kind": "csv",
                "row_count": row_count,
                "columns": columns,
                "rows": rows,
            }))
        }
        ContentKind::Json => {
            let parsed: Value = serde_json::from_slice(&body)
                .map_err(|e| eyre::eyre!("json body parse failed: {e}"))?;
            Ok(json!({
                "bucket": params.bucket,
                "key": params.key,
                "content_type": content_type,
                "kind": "json",
                "body": parsed,
            }))
        }
        ContentKind::Text => {
            let text = String::from_utf8(body.to_vec())
                .map_err(|e| eyre::eyre!("text body is not valid UTF-8: {e}"))?;
            let bytes = text.len();
            Ok(json!({
                "bucket": params.bucket,
                "key": params.key,
                "content_type": content_type,
                "kind": "text",
                "bytes": bytes,
                "body": text,
            }))
        }
        ContentKind::Spreadsheet => {
            let parsed = parse_spreadsheet(&body, params.sheet.as_deref())?;
            Ok(json!({
                "bucket": params.bucket,
                "key": params.key,
                "content_type": content_type.unwrap_or_else(|| {
                    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet".to_string()
                }),
                "kind": "spreadsheet",
                "sheet": parsed.sheet,
                "sheets": parsed.sheets,
                "row_count": parsed.rows.len(),
                "columns": parsed.columns,
                "rows": parsed.rows,
            }))
        }
        ContentKind::Binary => Err(eyre::eyre!("binary content cannot be inlined")),
    }
}

fn over_ceiling_message(size: i64) -> String {
    format!(
        "object is {size} bytes, over the {MAX_DOWNLOAD_BYTES}-byte download ceiling; inspect it via the AWS console rather than downloading."
    )
}

/// The object key's basename, sanitised into a safe download filename.
fn download_basename(key: &str) -> String {
    let name = key
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("download");
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => download_filename(stem, ext),
        _ => download_filename(name, "bin"),
    }
}

fn binary_message(head: &HeadResponse) -> String {
    let ct = head.content_type.as_deref().unwrap_or("unknown");
    format!("binary content (content_type={ct}); inspect via the AWS console rather than inlining.")
}

fn log_too_large(params: &GetInput, head: &HeadResponse, cap: u64, duration_ms: u64) {
    info!(
        target: "aws.s3",
        op = "get",
        bucket = %params.bucket,
        key = %params.key,
        size = head.size,
        cap,
        duration_ms,
        outcome = "too_large",
        "S3 get refused at head: over cap",
    );
}

fn log_binary(params: &GetInput, head: &HeadResponse, duration_ms: u64) {
    info!(
        target: "aws.s3",
        op = "get",
        bucket = %params.bucket,
        key = %params.key,
        content_type = head.content_type.as_deref().unwrap_or(""),
        duration_ms,
        outcome = "binary_refused",
        "S3 get refused at head: binary content",
    );
}

fn elapsed(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::client::{HeadResponse, S3Error};
    use bytes::Bytes;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    enum Stub {
        Head(Result<HeadResponse, S3Error>),
        Get(Result<GetResponse, S3Error>),
    }

    #[derive(Default)]
    struct MockBackend {
        head_calls: Mutex<usize>,
        get_calls: Mutex<usize>,
        stubs: Mutex<Vec<Stub>>,
    }

    impl MockBackend {
        fn with(stubs: Vec<Stub>) -> Self {
            Self {
                head_calls: Mutex::new(0),
                get_calls: Mutex::new(0),
                stubs: Mutex::new(stubs),
            }
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
        ) -> Result<crate::aws::client::ListResponse, S3Error> {
            unreachable!()
        }

        async fn head(
            &self,
            _ctx: &SessionContext,
            _bucket: &str,
            _key: &str,
        ) -> Result<HeadResponse, S3Error> {
            *self.head_calls.lock().unwrap() += 1;
            match self.stubs.lock().unwrap().remove(0) {
                Stub::Head(r) => r,
                Stub::Get(_) => panic!("expected head stub first"),
            }
        }

        async fn get(
            &self,
            _ctx: &SessionContext,
            _bucket: &str,
            _key: &str,
            max_bytes: u64,
        ) -> Result<GetResponse, S3Error> {
            *self.get_calls.lock().unwrap() += 1;
            match self.stubs.lock().unwrap().remove(0) {
                Stub::Get(Ok(resp)) => {
                    let observed = resp.body.len() as u64;
                    if max_bytes != 0 && observed > max_bytes {
                        Err(S3Error::TooLarge {
                            observed,
                            cap: max_bytes,
                        })
                    } else {
                        Ok(resp)
                    }
                }
                Stub::Get(Err(e)) => Err(e),
                Stub::Head(_) => panic!("expected get stub next"),
            }
        }

        async fn get_to_path(
            &self,
            _ctx: &SessionContext,
            _bucket: &str,
            _key: &str,
            dest: &std::path::Path,
            cap: u64,
        ) -> Result<u64, S3Error> {
            *self.get_calls.lock().unwrap() += 1;
            match self.stubs.lock().unwrap().remove(0) {
                Stub::Get(Ok(resp)) => {
                    let observed = resp.body.len() as u64;
                    if cap != 0 && observed > cap {
                        return Err(S3Error::TooLarge { observed, cap });
                    }
                    std::fs::write(dest, &resp.body).map_err(|e| S3Error::Other(e.to_string()))?;
                    Ok(observed)
                }
                Stub::Get(Err(e)) => Err(e),
                Stub::Head(_) => panic!("expected get stub next"),
            }
        }
    }

    fn install_store() -> std::sync::Arc<dyn workforce_agent::artifacts::ArtifactStore> {
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
        workforce_agent::artifacts::global_store().expect("store installed")
    }

    fn allowlist(names: &[&str]) -> Arc<Allowlist> {
        Arc::new(Allowlist::new(names.iter().map(|s| s.to_string())))
    }

    fn head(size: i64, ct: &str) -> HeadResponse {
        HeadResponse {
            size,
            content_type: Some(ct.to_string()),
            last_modified: None,
            etag: None,
            metadata: BTreeMap::new(),
        }
    }

    fn body(bytes: &'static [u8], ct: &str, size: i64) -> GetResponse {
        GetResponse {
            body: Bytes::from_static(bytes),
            content_type: Some(ct.to_string()),
            size,
            etag: None,
            last_modified: None,
        }
    }

    #[tokio::test]
    async fn get_refuses_unlisted_bucket_without_io() {
        let backend = Arc::new(MockBackend::with(vec![]));
        let tool = AwsS3GetTool::new(backend.clone(), allowlist(&["allowed"]), 1024);
        let res = tool
            .execute(json!({ "bucket": "nope", "key": "k" }))
            .await
            .unwrap();
        assert!(!res.success);
        assert_eq!(*backend.head_calls.lock().unwrap(), 0);
        assert_eq!(*backend.get_calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn get_streams_over_cap_object_to_a_download() {
        use workforce_agent::artifacts::{ArtifactBody, ArtifactId};
        let store = install_store();
        let big = vec![b'x'; 10_000];
        let backend = Arc::new(MockBackend::with(vec![
            Stub::Head(Ok(head(10_000, "application/octet-stream"))),
            Stub::Get(Ok(GetResponse {
                body: Bytes::from(big.clone()),
                content_type: Some("application/octet-stream".to_string()),
                size: 10_000,
                etag: None,
                last_modified: None,
            })),
        ]));
        let tool = AwsS3GetTool::new(backend.clone(), allowlist(&["b"]), 1_000);
        let res = workforce_agent::runner::CURRENT_AGENT_ID
            .scope(
                "s3-agent".to_string(),
                tool.execute(json!({ "bucket": "b", "key": "exports/big.bin" })),
            )
            .await
            .unwrap();
        assert!(res.success, "over-cap object is delivered as a download");
        assert_eq!(res.output["kind"], "download");
        assert_eq!(res.output["bytes"], 10_000);
        let marker = &res.output["generated_images"][0];
        assert_eq!(marker["filename"], "big.bin");
        assert_eq!(marker["media_type"], "application/octet-stream");
        let id = ArtifactId::parse(marker["artifact_id"].as_str().unwrap()).unwrap();
        let ArtifactBody::Bytes(bytes) = store.read("s3-agent", &id).unwrap() else {
            panic!("expected bytes artifact");
        };
        assert_eq!(bytes, big, "the full object streamed to the artifact");
    }

    #[tokio::test]
    async fn get_refuses_object_over_the_download_ceiling() {
        let backend = Arc::new(MockBackend::with(vec![Stub::Head(Ok(head(
            600 * 1024 * 1024,
            "application/octet-stream",
        )))]));
        let tool = AwsS3GetTool::new(backend.clone(), allowlist(&["b"]), 1_000);
        let res = tool
            .execute(json!({ "bucket": "b", "key": "huge.bin" }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("ceiling"));
        assert_eq!(
            *backend.get_calls.lock().unwrap(),
            0,
            "no body fetch past the ceiling"
        );
    }

    #[tokio::test]
    async fn get_with_cap_zero_skips_head_cap_and_fetches() {
        // A head claiming a large size must NOT be refused when the cap is
        // disabled (0); the body is fetched and decoded.
        let csv = b"name,role\nalice,clerk\n";
        let backend = Arc::new(MockBackend::with(vec![
            Stub::Head(Ok(head(50_000_000, "text/csv"))),
            Stub::Get(Ok(body(csv, "text/csv", csv.len() as i64))),
        ]));
        let tool = AwsS3GetTool::new(backend.clone(), allowlist(&["b"]), 0);
        let res = tool
            .execute(json!({ "bucket": "b", "key": "big.csv" }))
            .await
            .unwrap();
        assert!(res.success, "cap=0 disables the head ceiling");
        assert_eq!(res.output["kind"], "csv");
        assert_eq!(*backend.get_calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn get_refuses_binary_at_head_without_fetching_body() {
        let backend = Arc::new(MockBackend::with(vec![Stub::Head(Ok(head(
            500,
            "image/png",
        )))]));
        let tool = AwsS3GetTool::new(backend.clone(), allowlist(&["b"]), 100_000);
        let res = tool
            .execute(json!({ "bucket": "b", "key": "x.png" }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("binary content"));
        assert_eq!(*backend.get_calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn get_decodes_csv_to_rows_for_spill() {
        let csv = b"name,role\nalice,clerk\nbob,manager\n";
        let backend = Arc::new(MockBackend::with(vec![
            Stub::Head(Ok(head(csv.len() as i64, "text/csv"))),
            Stub::Get(Ok(body(csv, "text/csv", csv.len() as i64))),
        ]));
        let tool = AwsS3GetTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = tool
            .execute(json!({ "bucket": "b", "key": "trust/main.csv" }))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["kind"], "csv");
        assert_eq!(res.output["row_count"], 2);
        assert_eq!(res.output["columns"], json!(["name", "role"]));
        let rows = res.output["rows"].as_array().unwrap();
        assert_eq!(rows[0]["name"], "alice");
        assert_eq!(rows[1]["role"], "manager");
    }

    fn xlsx_fixture() -> Bytes {
        crate::aws::test_workbook(|wb| {
            let ws = wb.add_worksheet();
            ws.set_name("Unfilled").unwrap();
            ws.write_string(0, 0, "Request Id").unwrap();
            ws.write_string(0, 1, "Status").unwrap();
            ws.write_number(1, 0, 826281843.0).unwrap();
            ws.write_string(1, 1, "UnFilled").unwrap();
            let ws = wb.add_worksheet();
            ws.set_name("Notes").unwrap();
            ws.write_string(0, 0, "note").unwrap();
            ws.write_string(1, 0, "chunk 0 of 2").unwrap();
        })
    }

    #[tokio::test]
    async fn get_decodes_octet_stream_xlsx_to_rows() {
        // The archive case that used to be refused as binary: a real workbook
        // served as application/octet-stream, routed by its key suffix.
        let bytes = xlsx_fixture();
        let backend = Arc::new(MockBackend::with(vec![
            Stub::Head(Ok(head(bytes.len() as i64, "application/octet-stream"))),
            Stub::Get(Ok(GetResponse {
                body: bytes.clone(),
                content_type: Some("application/octet-stream".to_string()),
                size: bytes.len() as i64,
                etag: None,
                last_modified: None,
            })),
        ]));
        let tool = AwsS3GetTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = tool
            .execute(json!({ "bucket": "b", "key": "raw/T081042Z__chunk-00.xlsx" }))
            .await
            .unwrap();
        assert!(res.success, "{:?}", res.error);
        assert_eq!(res.output["kind"], "spreadsheet");
        assert_eq!(res.output["sheet"], "Unfilled");
        assert_eq!(res.output["sheets"], json!(["Unfilled", "Notes"]));
        assert_eq!(res.output["row_count"], 1);
        assert_eq!(res.output["columns"], json!(["Request Id", "Status"]));
        assert_eq!(res.output["rows"][0]["Request Id"], "826281843");
        assert_eq!(res.output["rows"][0]["Status"], "UnFilled");
    }

    #[tokio::test]
    async fn get_honors_the_sheet_parameter() {
        let bytes = xlsx_fixture();
        let backend = Arc::new(MockBackend::with(vec![
            Stub::Head(Ok(head(bytes.len() as i64, "application/octet-stream"))),
            Stub::Get(Ok(GetResponse {
                body: bytes.clone(),
                content_type: Some("application/octet-stream".to_string()),
                size: bytes.len() as i64,
                etag: None,
                last_modified: None,
            })),
        ]));
        let tool = AwsS3GetTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = tool
            .execute(json!({ "bucket": "b", "key": "r.xlsx", "sheet": "Notes" }))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["sheet"], "Notes");
        assert_eq!(res.output["rows"][0]["note"], "chunk 0 of 2");
    }

    #[tokio::test]
    async fn get_decodes_json_inline() {
        let json_body = br#"{"hello": "world"}"#;
        let backend = Arc::new(MockBackend::with(vec![
            Stub::Head(Ok(head(json_body.len() as i64, "application/json"))),
            Stub::Get(Ok(body(
                json_body,
                "application/json",
                json_body.len() as i64,
            ))),
        ]));
        let tool = AwsS3GetTool::new(backend, allowlist(&["b"]), 1_000_000);
        let res = tool
            .execute(json!({ "bucket": "b", "key": "x.json" }))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["kind"], "json");
        assert_eq!(res.output["body"]["hello"], "world");
    }

    #[tokio::test]
    async fn get_decodes_text_inline() {
        let txt = b"hello world";
        let backend = Arc::new(MockBackend::with(vec![
            Stub::Head(Ok(head(txt.len() as i64, "text/plain"))),
            Stub::Get(Ok(body(txt, "text/plain", txt.len() as i64))),
        ]));
        let tool = AwsS3GetTool::new(backend, allowlist(&["b"]), 1_000);
        let res = tool
            .execute(json!({ "bucket": "b", "key": "notes.txt" }))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["kind"], "text");
        assert_eq!(res.output["body"], "hello world");
        assert_eq!(res.output["bytes"], 11);
    }

    #[tokio::test]
    async fn get_enforces_cap_when_body_exceeds_head_claim() {
        let csv = b"a\n1\n2\n3\n4\n5\n6\n7\n8\n9\n";
        let backend = Arc::new(MockBackend::with(vec![
            Stub::Head(Ok(head(5, "text/csv"))),
            Stub::Get(Ok(body(csv, "text/csv", 5))),
        ]));
        let tool = AwsS3GetTool::new(backend, allowlist(&["b"]), 5);
        let res = tool
            .execute(json!({ "bucket": "b", "key": "small.csv" }))
            .await
            .unwrap();
        assert!(!res.success, "head said 5 but body is bigger → reject");
        assert!(res.error.unwrap().contains("max_get_bytes"));
    }

    #[tokio::test]
    async fn get_surfaces_not_found() {
        let backend = Arc::new(MockBackend::with(vec![Stub::Head(Err(S3Error::NotFound))]));
        let tool = AwsS3GetTool::new(backend, allowlist(&["b"]), 1_000);
        let res = tool
            .execute(json!({ "bucket": "b", "key": "missing" }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("not found"));
    }
}
