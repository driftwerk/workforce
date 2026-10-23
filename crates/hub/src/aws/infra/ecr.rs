//! `aws.ecr.{list_repositories,describe_images}` — repository discovery, then
//! image digests, tags and push times for a repository; the "what versions
//! exist and when were they pushed" half of the deployed-version question.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use workforce_agent::runner::SessionContext;
use workforce_agent::tools::{Tool, ToolResult};

use super::backend::{ImageFilter, InfraBackend};
use super::{elapsed, finish};

const DEFAULT_MAX_IMAGES: i32 = 50;
const HARD_MAX_IMAGES: i32 = 1000;
const DEFAULT_MAX_REPOS: i32 = 100;
const HARD_MAX_REPOS: i32 = 1000;

#[derive(Deserialize)]
struct ListRepositoriesInput {
    #[serde(default)]
    max: Option<i32>,
}

pub struct AwsEcrListRepositoriesTool {
    backend: Arc<dyn InfraBackend>,
}

impl AwsEcrListRepositoriesTool {
    pub fn new(backend: Arc<dyn InfraBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Tool for AwsEcrListRepositoriesTool {
    fn name(&self) -> &str {
        "aws.ecr.list_repositories"
    }

    fn description(&self) -> &str {
        "List ECR repositories in the account: name, URI, ARN and creation time. Discovers the \
         repository name to feed into aws.ecr.describe_images when you don't already know it."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "max": { "type": "integer", "minimum": 1, "maximum": HARD_MAX_REPOS, "description": "Max repositories (default 100)." }
            }
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let p: ListRepositoriesInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        let max = p.max.unwrap_or(DEFAULT_MAX_REPOS).clamp(1, HARD_MAX_REPOS);
        let ctx = SessionContext::capture();
        let result = self.backend.list_repositories(&ctx, max).await;
        Ok(finish(
            "aws.ecr.list_repositories",
            String::new(),
            start,
            result,
        ))
    }
}

#[derive(Deserialize)]
struct Input {
    repository: String,
    #[serde(default)]
    image_tag: Option<String>,
    #[serde(default)]
    image_digest: Option<String>,
    #[serde(default)]
    max: Option<i32>,
}

pub struct AwsEcrDescribeImagesTool {
    backend: Arc<dyn InfraBackend>,
}

impl AwsEcrDescribeImagesTool {
    pub fn new(backend: Arc<dyn InfraBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Tool for AwsEcrDescribeImagesTool {
    fn name(&self) -> &str {
        "aws.ecr.describe_images"
    }

    fn description(&self) -> &str {
        "List ECR images for a repository (digest, tags, pushedAt, size), newest first. Pass \
         image_tag or image_digest to resolve a single image — e.g. the digest a tag currently \
         points to, or the tags + push time of a running digest."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repository": { "type": "string", "description": "ECR repository name." },
                "image_tag": { "type": "string", "description": "Resolve the single image behind this tag." },
                "image_digest": { "type": "string", "description": "Resolve the single image with this sha256 digest." },
                "max": { "type": "integer", "minimum": 1, "maximum": HARD_MAX_IMAGES, "description": "Max images when unfiltered (default 50)." }
            },
            "required": ["repository"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let p: Input = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        let filter = match (p.image_digest, p.image_tag) {
            (Some(digest), _) => ImageFilter::Digest(digest),
            (None, Some(tag)) => ImageFilter::Tag(tag),
            (None, None) => ImageFilter::Recent {
                max: p
                    .max
                    .unwrap_or(DEFAULT_MAX_IMAGES)
                    .clamp(1, HARD_MAX_IMAGES),
            },
        };
        let ctx = SessionContext::capture();
        let detail = p.repository.clone();
        let result = self
            .backend
            .describe_images(&ctx, &p.repository, filter)
            .await;
        Ok(finish("aws.ecr.describe_images", detail, start, result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::infra::backend::mock::MockInfraBackend;
    use crate::aws::infra::backend::{AwsError, EcrImage, EcrRepository};

    fn backend_with(images: Vec<EcrImage>) -> Arc<MockInfraBackend> {
        Arc::new(MockInfraBackend {
            ecr_images: parking_lot::Mutex::new(images),
            ..Default::default()
        })
    }

    fn image(digest: &str, tags: &[&str]) -> EcrImage {
        EcrImage {
            image_digest: Some(digest.into()),
            image_tags: tags.iter().map(|s| s.to_string()).collect(),
            pushed_at: Some("2026-06-12T00:00:00Z".into()),
            size_bytes: Some(1024),
        }
    }

    #[tokio::test]
    async fn list_repositories_serialises_result() {
        let backend = MockInfraBackend {
            repositories: parking_lot::Mutex::new(Some(Ok(vec![EcrRepository {
                name: Some("workforce-prod".into()),
                uri: Some("1.dkr.ecr.eu-west-2.amazonaws.com/workforce-prod".into()),
                arn: Some("arn:aws:ecr:eu-west-2:1:repository/workforce-prod".into()),
                created_at: Some("2026-06-12T00:00:00Z".into()),
            }]))),
            ..Default::default()
        };
        let tool = AwsEcrListRepositoriesTool::new(Arc::new(backend));
        let res = tool.execute(json!({})).await.unwrap();
        assert!(res.success);
        assert_eq!(res.output[0]["name"], "workforce-prod");
    }

    #[tokio::test]
    async fn list_repositories_maps_error_to_failure() {
        let backend = MockInfraBackend {
            repositories: parking_lot::Mutex::new(Some(Err(AwsError::AccessDenied))),
            ..Default::default()
        };
        let tool = AwsEcrListRepositoriesTool::new(Arc::new(backend));
        let res = tool.execute(json!({})).await.unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("access denied"));
    }

    #[tokio::test]
    async fn describe_images_filters_by_tag() {
        let backend = backend_with(vec![
            image("sha256:a", &["latest"]),
            image("sha256:b", &["v1"]),
        ]);
        let tool = AwsEcrDescribeImagesTool::new(backend);
        let res = tool
            .execute(json!({ "repository": "workforce-prod", "image_tag": "v1" }))
            .await
            .unwrap();
        assert!(res.success);
        let arr = res.output.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["image_digest"], "sha256:b");
    }

    #[tokio::test]
    async fn describe_images_returns_all_when_unfiltered() {
        let backend = backend_with(vec![
            image("sha256:a", &["latest"]),
            image("sha256:b", &["v1"]),
        ]);
        let tool = AwsEcrDescribeImagesTool::new(backend);
        let res = tool
            .execute(json!({ "repository": "workforce-prod" }))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output.as_array().unwrap().len(), 2);
    }
}
