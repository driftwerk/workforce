//! `aws.s3.list` — paginated `ListObjectsV2` on an allowlisted bucket.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::info;

use workforce_agent::runner::SessionContext;
use workforce_agent::tools::{Tool, ToolResult};

use super::client::{Allowlist, S3Backend};

const DEFAULT_MAX_KEYS: u16 = 100;
const HARD_MAX_KEYS: u16 = 1000;

#[derive(Deserialize)]
struct ListInput {
    bucket: String,
    #[serde(default)]
    prefix: Option<String>,
    #[serde(default)]
    delimiter: Option<String>,
    #[serde(default)]
    continuation_token: Option<String>,
    #[serde(default)]
    max_keys: Option<u32>,
}

pub struct AwsS3ListTool {
    backend: Arc<dyn S3Backend>,
    allowlist: Arc<Allowlist>,
}

impl AwsS3ListTool {
    pub fn new(backend: Arc<dyn S3Backend>, allowlist: Arc<Allowlist>) -> Self {
        Self { backend, allowlist }
    }
}

#[async_trait]
impl Tool for AwsS3ListTool {
    fn name(&self) -> &str {
        "aws.s3.list"
    }

    fn description(&self) -> &str {
        "List objects in an allowlisted S3 bucket (paginated; up to 1000 keys per call). Pass `prefix` to scope, and `continuation_token` from a prior call's `next_continuation_token` to page through. Pass `delimiter` (usually \"/\") to enumerate one hierarchy level at a time: the response's `common_prefixes` then holds the \"subdirectories\" under `prefix` instead of every leaf key — the cheap way to discover structure without paging through it."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "bucket": { "type": "string", "description": "Bucket name (must be allowlisted)." },
                "prefix": { "type": "string", "description": "Optional key prefix filter." },
                "delimiter": { "type": "string", "description": "Group keys by this separator (usually \"/\"); returns one-level `common_prefixes` instead of recursing into every key." },
                "continuation_token": { "type": "string", "description": "Page token from a prior call." },
                "max_keys": { "type": "integer", "minimum": 1, "maximum": HARD_MAX_KEYS, "description": "Max keys to return (default 100)." }
            },
            "required": ["bucket"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: ListInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        if !self.allowlist.contains(&params.bucket) {
            return Ok(ToolResult::failure(
                self.allowlist.deny_message(&params.bucket),
                elapsed(start),
            ));
        }
        let max_keys = params
            .max_keys
            .unwrap_or(DEFAULT_MAX_KEYS as u32)
            .clamp(1, HARD_MAX_KEYS as u32) as u16;
        let ctx = SessionContext::capture();
        let result = self
            .backend
            .list(
                &ctx,
                &params.bucket,
                params.prefix.as_deref(),
                params.delimiter.as_deref(),
                params.continuation_token.as_deref(),
                max_keys,
            )
            .await;
        let duration_ms = elapsed(start);
        match result {
            Ok(resp) => {
                info!(
                    target: "aws.s3",
                    op = "list",
                    bucket = %params.bucket,
                    prefix = params.prefix.as_deref().unwrap_or(""),
                    delimiter = params.delimiter.as_deref().unwrap_or(""),
                    returned = resp.keys.len(),
                    prefixes = resp.common_prefixes.len(),
                    truncated = resp.is_truncated,
                    duration_ms,
                    "S3 list",
                );
                Ok(ToolResult::success(
                    json!({
                        "bucket": params.bucket,
                        "keys": resp.keys,
                        "common_prefixes": resp.common_prefixes,
                        "is_truncated": resp.is_truncated,
                        "next_continuation_token": resp.next_continuation_token,
                    }),
                    duration_ms,
                ))
            }
            Err(err) => {
                info!(
                    target: "aws.s3",
                    op = "list",
                    bucket = %params.bucket,
                    duration_ms,
                    outcome = "error",
                    error = %err,
                    "S3 list failed",
                );
                Ok(ToolResult::failure(err.to_string(), duration_ms))
            }
        }
    }
}

fn elapsed(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::client::{ListResponse, ListedKey, S3Error};
    use std::sync::Mutex;

    type ListCall = (String, Option<String>, Option<String>, Option<String>, u16);

    struct MockBackend {
        calls: Mutex<Vec<ListCall>>,
        response: Result<ListResponse, S3Error>,
    }

    impl MockBackend {
        fn new(response: Result<ListResponse, S3Error>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                response,
            }
        }
    }

    #[async_trait]
    impl S3Backend for MockBackend {
        async fn list(
            &self,
            _ctx: &SessionContext,
            bucket: &str,
            prefix: Option<&str>,
            delimiter: Option<&str>,
            token: Option<&str>,
            max_keys: u16,
        ) -> Result<ListResponse, S3Error> {
            self.calls.lock().unwrap().push((
                bucket.to_string(),
                prefix.map(String::from),
                delimiter.map(String::from),
                token.map(String::from),
                max_keys,
            ));
            self.response.clone()
        }

        async fn head(
            &self,
            _ctx: &SessionContext,
            _bucket: &str,
            _key: &str,
        ) -> Result<crate::aws::client::HeadResponse, S3Error> {
            unreachable!()
        }

        async fn get(
            &self,
            _ctx: &SessionContext,
            _bucket: &str,
            _key: &str,
            _max_bytes: u64,
        ) -> Result<crate::aws::client::GetResponse, S3Error> {
            unreachable!()
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

    fn allowlist(names: &[&str]) -> Arc<Allowlist> {
        Arc::new(Allowlist::new(names.iter().map(|s| s.to_string())))
    }

    #[tokio::test]
    async fn list_refuses_unlisted_bucket_without_calling_backend() {
        let backend = Arc::new(MockBackend::new(Ok(ListResponse {
            keys: Vec::new(),
            common_prefixes: Vec::new(),
            is_truncated: false,
            next_continuation_token: None,
        })));
        let tool = AwsS3ListTool::new(backend.clone(), allowlist(&["allowed"]));
        let res = tool.execute(json!({ "bucket": "denied" })).await.unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("not allowlisted"));
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_returns_keys_and_pagination_fields() {
        let backend = Arc::new(MockBackend::new(Ok(ListResponse {
            keys: vec![ListedKey {
                key: "trust/main.csv".into(),
                size: 1024,
                last_modified: Some("2026-05-01T00:00:00Z".into()),
                etag: Some("abc".into()),
            }],
            common_prefixes: Vec::new(),
            is_truncated: true,
            next_continuation_token: Some("tok".into()),
        })));
        let tool = AwsS3ListTool::new(backend, allowlist(&["allowed"]));
        let res = tool
            .execute(json!({ "bucket": "allowed", "prefix": "trust/" }))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["keys"][0]["key"], "trust/main.csv");
        assert_eq!(res.output["is_truncated"], true);
        assert_eq!(res.output["next_continuation_token"], "tok");
    }

    #[tokio::test]
    async fn list_clamps_max_keys_to_hard_limit() {
        let backend = Arc::new(MockBackend::new(Ok(ListResponse {
            keys: Vec::new(),
            common_prefixes: Vec::new(),
            is_truncated: false,
            next_continuation_token: None,
        })));
        let tool = AwsS3ListTool::new(backend.clone(), allowlist(&["allowed"]));
        let _ = tool
            .execute(json!({ "bucket": "allowed", "max_keys": 99_999 }))
            .await
            .unwrap();
        assert_eq!(backend.calls.lock().unwrap()[0].4, HARD_MAX_KEYS);
    }

    #[tokio::test]
    async fn list_passes_delimiter_and_returns_common_prefixes() {
        let backend = Arc::new(MockBackend::new(Ok(ListResponse {
            keys: Vec::new(),
            common_prefixes: vec!["2026-02-01/NORTH/".into(), "2026-02-01/SOUTH/".into()],
            is_truncated: false,
            next_continuation_token: None,
        })));
        let tool = AwsS3ListTool::new(backend.clone(), allowlist(&["allowed"]));
        let res = tool
            .execute(json!({ "bucket": "allowed", "prefix": "2026-02-01/", "delimiter": "/" }))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["common_prefixes"][0], "2026-02-01/NORTH/");
        assert_eq!(res.output["common_prefixes"][1], "2026-02-01/SOUTH/");
        assert_eq!(backend.calls.lock().unwrap()[0].2, Some("/".to_string()));
    }

    #[tokio::test]
    async fn list_surfaces_backend_error_as_failure() {
        let backend = Arc::new(MockBackend::new(Err(S3Error::AccessDenied)));
        let tool = AwsS3ListTool::new(backend, allowlist(&["allowed"]));
        let res = tool.execute(json!({ "bucket": "allowed" })).await.unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("access denied"));
    }

    #[tokio::test]
    async fn list_rejects_missing_bucket_with_parse_error() {
        let backend = Arc::new(MockBackend::new(Ok(ListResponse {
            keys: Vec::new(),
            common_prefixes: Vec::new(),
            is_truncated: false,
            next_continuation_token: None,
        })));
        let tool = AwsS3ListTool::new(backend, allowlist(&["allowed"]));
        let res = tool.execute(json!({})).await.unwrap();
        assert!(!res.success);
    }
}
