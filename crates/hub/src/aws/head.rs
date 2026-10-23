//! `aws.s3.head` — cheap `HeadObject` metadata probe.

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

#[derive(Deserialize)]
struct HeadInput {
    bucket: String,
    key: String,
}

pub struct AwsS3HeadTool {
    backend: Arc<dyn S3Backend>,
    allowlist: Arc<Allowlist>,
}

impl AwsS3HeadTool {
    pub fn new(backend: Arc<dyn S3Backend>, allowlist: Arc<Allowlist>) -> Self {
        Self { backend, allowlist }
    }
}

#[async_trait]
impl Tool for AwsS3HeadTool {
    fn name(&self) -> &str {
        "aws.s3.head"
    }

    fn description(&self) -> &str {
        "Fetch an object's metadata (size, content-type, last-modified, ETag, user metadata) without downloading the body. Use before `aws.s3.get` to confirm size and type."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "bucket": { "type": "string", "description": "Bucket name (must be allowlisted)." },
                "key": { "type": "string", "description": "Object key." }
            },
            "required": ["bucket", "key"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: HeadInput = match serde_json::from_value(input) {
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
        let result = self.backend.head(&ctx, &params.bucket, &params.key).await;
        let duration_ms = elapsed(start);
        match result {
            Ok(resp) => {
                info!(
                    target: "aws.s3",
                    op = "head",
                    bucket = %params.bucket,
                    key = %params.key,
                    size = resp.size,
                    content_type = resp.content_type.as_deref().unwrap_or(""),
                    duration_ms,
                    "S3 head",
                );
                Ok(ToolResult::success(
                    json!({
                        "bucket": params.bucket,
                        "key": params.key,
                        "size": resp.size,
                        "content_type": resp.content_type,
                        "last_modified": resp.last_modified,
                        "etag": resp.etag,
                        "metadata": resp.metadata,
                    }),
                    duration_ms,
                ))
            }
            Err(err) => {
                info!(
                    target: "aws.s3",
                    op = "head",
                    bucket = %params.bucket,
                    key = %params.key,
                    duration_ms,
                    outcome = "error",
                    error = %err,
                    "S3 head failed",
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
    use crate::aws::client::{HeadResponse, S3Error};
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    struct MockBackend {
        head_calls: Mutex<Vec<(String, String)>>,
        response: Mutex<Option<Result<HeadResponse, S3Error>>>,
    }

    impl MockBackend {
        fn new(response: Result<HeadResponse, S3Error>) -> Self {
            Self {
                head_calls: Mutex::new(Vec::new()),
                response: Mutex::new(Some(response)),
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
            bucket: &str,
            key: &str,
        ) -> Result<HeadResponse, S3Error> {
            self.head_calls
                .lock()
                .unwrap()
                .push((bucket.to_string(), key.to_string()));
            self.response
                .lock()
                .unwrap()
                .take()
                .expect("response consumed twice")
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

    fn head_response(size: i64, ct: &str) -> HeadResponse {
        HeadResponse {
            size,
            content_type: Some(ct.to_string()),
            last_modified: Some("2026-05-01T00:00:00Z".into()),
            etag: Some("abc".into()),
            metadata: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn head_refuses_unlisted_bucket() {
        let backend = Arc::new(MockBackend::new(Ok(head_response(1, "text/csv"))));
        let tool = AwsS3HeadTool::new(backend.clone(), allowlist(&["allowed"]));
        let res = tool
            .execute(json!({ "bucket": "denied", "key": "k" }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(backend.head_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn head_returns_metadata_fields() {
        let backend = Arc::new(MockBackend::new(Ok(head_response(4096, "text/csv"))));
        let tool = AwsS3HeadTool::new(backend, allowlist(&["b"]));
        let res = tool
            .execute(json!({ "bucket": "b", "key": "trust/main.csv" }))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["size"], 4096);
        assert_eq!(res.output["content_type"], "text/csv");
        assert_eq!(res.output["etag"], "abc");
    }

    #[tokio::test]
    async fn head_surfaces_not_found_as_clear_message() {
        let backend = Arc::new(MockBackend::new(Err(S3Error::NotFound)));
        let tool = AwsS3HeadTool::new(backend, allowlist(&["b"]));
        let res = tool
            .execute(json!({ "bucket": "b", "key": "missing" }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("not found"));
    }
}
