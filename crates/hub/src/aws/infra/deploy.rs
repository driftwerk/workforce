//! `aws.deploy.status` — the one composite tool. Answers "what version is
//! running and did the deploy roll forward" without lying: the configured
//! task-definition image is often a moving tag, so the authoritative signal
//! is the resolved digest of the running tasks, cross-referenced against ECR.
//!
//! The async `gather` orchestrates the read calls; every verdict decision is
//! a pure function over the gathered data, so the logic is exhaustively
//! unit-testable with no SDK or mock.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use workforce_agent::runner::SessionContext;
use workforce_agent::tools::{Tool, ToolResult};

use super::backend::{
    AwsError, EcrImage, EcsServiceSummary, EcsTask, EcsTaskDefinition, ImageFilter, InfraBackend,
};
use super::{elapsed, finish};

#[derive(Deserialize)]
struct Input {
    cluster: String,
    service: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EcrImageRef {
    pub registry: String,
    pub repository: String,
    pub tag: Option<String>,
    pub digest: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunningImage {
    pub digest: String,
    pub tags: Vec<String>,
    pub pushed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeployStatus {
    pub cluster: String,
    pub service: String,
    pub rollout_state: Option<String>,
    pub rollout_state_reason: Option<String>,
    pub desired_count: i32,
    pub running_count: i32,
    pub pending_count: i32,
    pub deployment_count: usize,
    pub configured_image: Option<String>,
    pub configured_repository: Option<String>,
    pub configured_tag: Option<String>,
    pub configured_digest: Option<String>,
    pub running: Vec<RunningImage>,
    pub tag_current_digest: Option<String>,
    pub running_matches_tag: Option<bool>,
    pub mid_rollout: bool,
    pub rolled_forward: bool,
    pub summary: String,
}

pub struct AwsDeployStatusTool {
    backend: Arc<dyn InfraBackend>,
}

impl AwsDeployStatusTool {
    pub fn new(backend: Arc<dyn InfraBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Tool for AwsDeployStatusTool {
    fn name(&self) -> &str {
        "aws.deploy.status"
    }

    fn description(&self) -> &str {
        "Report whether an ECS service's deployment rolled forward and what image version is \
         actually running. Reads the PRIMARY deployment's rolloutState, the running tasks' \
         resolved image digests (authoritative — not the configured tag), and cross-references \
         ECR for the tag + push time. `running_matches_tag=false` is the 'rolled back to :latest' \
         signal ONLY when `mid_rollout=false`; during a rollout a transient mismatch (old task \
         still draining) is normal — trust the `rolled_forward` and `summary` fields."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "cluster": { "type": "string", "description": "ECS cluster name or ARN." },
                "service": { "type": "string", "description": "ECS service name." }
            },
            "required": ["cluster", "service"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let p: Input = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        let ctx = SessionContext::capture();
        let detail = format!("{}/{}", p.cluster, p.service);
        let result = gather(self.backend.as_ref(), &ctx, &p.cluster, &p.service).await;
        Ok(finish("aws.deploy.status", detail, start, result))
    }
}

async fn gather(
    backend: &dyn InfraBackend,
    ctx: &SessionContext,
    cluster: &str,
    service: &str,
) -> Result<DeployStatus, AwsError> {
    let services = backend
        .describe_services(ctx, cluster, &[service.to_string()])
        .await?;
    let svc = services
        .iter()
        .find(|s| s.service_name.as_deref() == Some(service))
        .or_else(|| services.first())
        .cloned()
        .ok_or(AwsError::NotFound)?;

    let primary = svc.deployments.iter().find(|d| is_primary(d));
    let task_def_ref = primary
        .and_then(|d| d.task_definition.clone())
        .or_else(|| svc.task_definition.clone());
    let task_def = match task_def_ref {
        Some(ref td) => backend.describe_task_definition(ctx, td).await.ok(),
        None => None,
    };

    let arns = backend
        .list_tasks(ctx, cluster, Some(service), Some("RUNNING"), 100)
        .await
        .unwrap_or_default();
    let tasks = backend
        .describe_tasks(ctx, cluster, &arns)
        .await
        .unwrap_or_default();

    let primary_name = task_def.as_ref().and_then(primary_container_name);
    let configured_image = task_def
        .as_ref()
        .and_then(|td| configured_image(td, primary_name.as_deref()));
    let parsed = configured_image.as_deref().and_then(parse_image_ref);
    let digests = running_digests(primary_name.as_deref(), &tasks);

    let (tag_current_digest, digest_images) = match &parsed {
        Some(r) => (
            tag_current_digest(backend, ctx, r).await,
            resolve_digest_images(backend, ctx, &r.repository, &digests).await,
        ),
        None => (None, Vec::new()),
    };

    Ok(synthesize(
        cluster,
        service,
        &svc,
        configured_image,
        parsed,
        &digests,
        tag_current_digest,
        &digest_images,
    ))
}

async fn tag_current_digest(
    backend: &dyn InfraBackend,
    ctx: &SessionContext,
    r: &EcrImageRef,
) -> Option<String> {
    let tag = r.tag.clone()?;
    backend
        .describe_images(ctx, &r.repository, ImageFilter::Tag(tag))
        .await
        .ok()?
        .into_iter()
        .next()
        .and_then(|i| i.image_digest)
}

async fn resolve_digest_images(
    backend: &dyn InfraBackend,
    ctx: &SessionContext,
    repository: &str,
    digests: &[String],
) -> Vec<EcrImage> {
    let mut out = Vec::new();
    for digest in digests {
        if let Ok(images) = backend
            .describe_images(ctx, repository, ImageFilter::Digest(digest.clone()))
            .await
        {
            out.extend(images.into_iter().next());
        }
    }
    out
}

fn is_primary(d: &super::backend::EcsDeployment) -> bool {
    d.status.as_deref() == Some("PRIMARY")
}

pub(crate) fn primary_container_name(td: &EcsTaskDefinition) -> Option<String> {
    td.containers
        .iter()
        .find(|c| c.essential == Some(true))
        .or_else(|| td.containers.first())
        .and_then(|c| c.name.clone())
}

fn configured_image(td: &EcsTaskDefinition, primary_name: Option<&str>) -> Option<String> {
    td.containers
        .iter()
        .find(|c| primary_name.is_some() && c.name.as_deref() == primary_name)
        .or_else(|| td.containers.first())
        .and_then(|c| c.image.clone())
}

/// Distinct resolved digests of the primary container across running tasks,
/// in first-seen order. When the primary container name is unknown, falls
/// back to each task's first container.
pub(crate) fn running_digests(primary_name: Option<&str>, tasks: &[EcsTask]) -> Vec<String> {
    let mut seen = Vec::new();
    for task in tasks {
        let container = task
            .containers
            .iter()
            .find(|c| primary_name.is_some() && c.name.as_deref() == primary_name)
            .or_else(|| task.containers.first());
        if let Some(digest) = container.and_then(|c| c.image_digest.clone()) {
            if !seen.contains(&digest) {
                seen.push(digest);
            }
        }
    }
    seen
}

/// Parse an ECR image ref into registry/repository/tag/digest. Returns `None`
/// for non-ECR refs (Docker Hub, bare names) so callers skip ECR enrichment.
pub(crate) fn parse_image_ref(image: &str) -> Option<EcrImageRef> {
    let (registry, rest) = image.split_once('/')?;
    if !registry.contains(".dkr.ecr.") {
        return None;
    }
    if let Some((repository, digest)) = rest.split_once('@') {
        return Some(EcrImageRef {
            registry: registry.to_string(),
            repository: repository.to_string(),
            tag: None,
            digest: Some(digest.to_string()),
        });
    }
    match rest.rsplit_once(':') {
        Some((repository, tag)) => Some(EcrImageRef {
            registry: registry.to_string(),
            repository: repository.to_string(),
            tag: Some(tag.to_string()),
            digest: None,
        }),
        None => Some(EcrImageRef {
            registry: registry.to_string(),
            repository: rest.to_string(),
            tag: None,
            digest: None,
        }),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn synthesize(
    cluster: &str,
    service: &str,
    svc: &EcsServiceSummary,
    configured_image: Option<String>,
    parsed: Option<EcrImageRef>,
    running_digests: &[String],
    tag_current_digest: Option<String>,
    digest_images: &[EcrImage],
) -> DeployStatus {
    let primary = svc.deployments.iter().find(|d| is_primary(d));
    let rollout_state = primary.and_then(|d| d.rollout_state.clone());
    let rollout_state_reason = primary.and_then(|d| d.rollout_state_reason.clone());
    let deployment_count = svc.deployments.len();

    let running_matches_tag = match (&tag_current_digest, running_digests.is_empty()) {
        (Some(tag_digest), false) => Some(running_digests.iter().all(|d| d == tag_digest)),
        _ => None,
    };
    let mid_rollout = running_digests.len() > 1;
    let rolled_forward = rollout_state.as_deref() == Some("COMPLETED")
        && svc.pending_count == 0
        && svc.desired_count > 0
        && svc.running_count == svc.desired_count
        && deployment_count == 1
        && running_matches_tag != Some(false);

    let running: Vec<RunningImage> = running_digests
        .iter()
        .map(|digest| {
            let image = digest_images
                .iter()
                .find(|i| i.image_digest.as_deref() == Some(digest.as_str()));
            RunningImage {
                digest: digest.clone(),
                tags: image.map(|i| i.image_tags.clone()).unwrap_or_default(),
                pushed_at: image.and_then(|i| i.pushed_at.clone()),
            }
        })
        .collect();

    let summary = summarise(
        service,
        rollout_state.as_deref(),
        rolled_forward,
        mid_rollout,
        running_matches_tag,
        parsed.as_ref().and_then(|r| r.tag.as_deref()),
        &running,
    );

    DeployStatus {
        cluster: cluster.to_string(),
        service: service.to_string(),
        rollout_state,
        rollout_state_reason,
        desired_count: svc.desired_count,
        running_count: svc.running_count,
        pending_count: svc.pending_count,
        deployment_count,
        configured_image,
        configured_repository: parsed.as_ref().map(|r| r.repository.clone()),
        configured_tag: parsed.as_ref().and_then(|r| r.tag.clone()),
        configured_digest: parsed.and_then(|r| r.digest),
        running,
        tag_current_digest,
        running_matches_tag,
        mid_rollout,
        rolled_forward,
        summary,
    }
}

fn summarise(
    service: &str,
    rollout_state: Option<&str>,
    rolled_forward: bool,
    mid_rollout: bool,
    running_matches_tag: Option<bool>,
    tag: Option<&str>,
    running: &[RunningImage],
) -> String {
    let running_desc = match running.first() {
        Some(img) if !img.tags.is_empty() => format!("running {}", img.tags.join(", ")),
        Some(img) => format!("running {}", short_digest(&img.digest)),
        None => "no running tasks".to_string(),
    };

    if mid_rollout || rollout_state == Some("IN_PROGRESS") {
        return format!("{service}: rollout in progress — {running_desc}.");
    }
    if running_matches_tag == Some(false) {
        let tag = tag.unwrap_or("the configured tag");
        return format!(
            "{service}: {running_desc} does NOT match what '{tag}' now points to — likely rolled back / did not roll forward."
        );
    }
    if rolled_forward {
        return format!("{service}: rolled forward — rollout COMPLETED, {running_desc}.");
    }
    if rollout_state == Some("FAILED") {
        return format!("{service}: rollout FAILED — {running_desc}.");
    }
    format!(
        "{service}: rollout {} — {running_desc}.",
        rollout_state.unwrap_or("unknown")
    )
}

fn short_digest(digest: &str) -> String {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    format!("sha256:{}", &hex[..hex.len().min(12)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::infra::backend::mock::MockInfraBackend;
    use crate::aws::infra::backend::{EcsContainer, EcsContainerDef, EcsDeployment};

    fn parsed_ref() -> EcrImageRef {
        parse_image_ref("111122223333.dkr.ecr.eu-west-2.amazonaws.com/workforce-prod:latest")
            .unwrap()
    }

    #[test]
    fn parse_image_ref_extracts_tag() {
        let r = parsed_ref();
        assert_eq!(r.repository, "workforce-prod");
        assert_eq!(r.tag.as_deref(), Some("latest"));
        assert!(r.digest.is_none());
    }

    #[test]
    fn parse_image_ref_extracts_digest() {
        let r = parse_image_ref(
            "111122223333.dkr.ecr.eu-west-2.amazonaws.com/workforce-prod@sha256:abc123",
        )
        .unwrap();
        assert_eq!(r.repository, "workforce-prod");
        assert_eq!(r.digest.as_deref(), Some("sha256:abc123"));
        assert!(r.tag.is_none());
    }

    #[test]
    fn parse_image_ref_handles_nested_repository_path() {
        let r = parse_image_ref("111122223333.dkr.ecr.eu-west-2.amazonaws.com/team/workforce:v2")
            .unwrap();
        assert_eq!(r.repository, "team/workforce");
        assert_eq!(r.tag.as_deref(), Some("v2"));
    }

    #[test]
    fn parse_image_ref_rejects_non_ecr() {
        assert!(parse_image_ref("nginx:latest").is_none());
        assert!(parse_image_ref("docker.io/library/redis:7").is_none());
        assert!(parse_image_ref("bare-name").is_none());
    }

    fn deployment(status: &str, rollout: &str) -> EcsDeployment {
        EcsDeployment {
            id: Some("ecs-svc/1".into()),
            status: Some(status.into()),
            task_definition: Some("workforce:42".into()),
            desired_count: 1,
            running_count: 1,
            pending_count: 0,
            rollout_state: Some(rollout.into()),
            rollout_state_reason: None,
            created_at: None,
            updated_at: None,
        }
    }

    fn service(
        deployments: Vec<EcsDeployment>,
        running: i32,
        desired: i32,
        pending: i32,
    ) -> EcsServiceSummary {
        EcsServiceSummary {
            service_name: Some("workforce-prod".into()),
            status: Some("ACTIVE".into()),
            desired_count: desired,
            running_count: running,
            pending_count: pending,
            task_definition: Some("workforce:42".into()),
            deployments,
            events: Vec::new(),
        }
    }

    fn image(digest: &str, tags: &[&str]) -> EcrImage {
        EcrImage {
            image_digest: Some(digest.into()),
            image_tags: tags.iter().map(|s| s.to_string()).collect(),
            pushed_at: Some("2026-06-12T00:00:00Z".into()),
            size_bytes: Some(1024),
        }
    }

    #[test]
    fn synthesize_reports_rolled_forward_when_complete_and_matching() {
        let svc = service(vec![deployment("PRIMARY", "COMPLETED")], 1, 1, 0);
        let digests = vec!["sha256:new".to_string()];
        let status = synthesize(
            "prod",
            "workforce-prod",
            &svc,
            Some("repo:latest".into()),
            Some(parsed_ref()),
            &digests,
            Some("sha256:new".into()),
            &[image("sha256:new", &["latest"])],
        );
        assert!(status.rolled_forward);
        assert_eq!(status.running_matches_tag, Some(true));
        assert_eq!(status.running[0].tags, vec!["latest".to_string()]);
        assert!(status.summary.contains("rolled forward"));
    }

    #[test]
    fn synthesize_detects_rollback_when_running_digest_differs_from_tag() {
        let svc = service(vec![deployment("PRIMARY", "COMPLETED")], 1, 1, 0);
        let digests = vec!["sha256:old".to_string()];
        let status = synthesize(
            "prod",
            "workforce-prod",
            &svc,
            Some("repo:latest".into()),
            Some(parsed_ref()),
            &digests,
            Some("sha256:new".into()),
            &[image("sha256:old", &[])],
        );
        assert!(
            !status.rolled_forward,
            "digest != tag target must not be rolled forward"
        );
        assert_eq!(status.running_matches_tag, Some(false));
        assert!(status.summary.to_lowercase().contains("rolled back"));
    }

    #[test]
    fn synthesize_flags_failed_rollout() {
        let svc = service(vec![deployment("PRIMARY", "FAILED")], 1, 1, 0);
        let status = synthesize(
            "prod",
            "workforce-prod",
            &svc,
            None,
            None,
            &["sha256:x".to_string()],
            None,
            &[],
        );
        assert!(!status.rolled_forward);
        assert_eq!(status.rollout_state.as_deref(), Some("FAILED"));
        assert!(status.summary.contains("FAILED"));
    }

    #[test]
    fn synthesize_flags_mid_rollout_on_multiple_digests() {
        let mut primary = deployment("PRIMARY", "IN_PROGRESS");
        primary.running_count = 1;
        let active = deployment("ACTIVE", "COMPLETED");
        let svc = service(vec![primary, active], 2, 2, 1);
        let digests = vec!["sha256:a".to_string(), "sha256:b".to_string()];
        let status = synthesize(
            "prod",
            "workforce-prod",
            &svc,
            Some("repo:latest".into()),
            Some(parsed_ref()),
            &digests,
            None,
            &[],
        );
        assert!(status.mid_rollout);
        assert!(!status.rolled_forward, "two deployments present");
        assert_eq!(status.running.len(), 2);
    }

    #[test]
    fn synthesize_mid_rollout_with_resolved_tag_is_not_labelled_rollback() {
        // A healthy rolling deploy (new task up, old draining) where the tag
        // already points at the new digest: running spans both digests, so
        // running_matches_tag is Some(false) — but this is in progress, NOT a
        // rollback. The summary must not cry "rolled back".
        let mut primary = deployment("PRIMARY", "IN_PROGRESS");
        primary.running_count = 1;
        let svc = service(vec![primary], 1, 2, 0);
        let digests = vec!["sha256:new".to_string(), "sha256:old".to_string()];
        let status = synthesize(
            "prod",
            "workforce-prod",
            &svc,
            Some("repo:latest".into()),
            Some(parsed_ref()),
            &digests,
            Some("sha256:new".into()),
            &[image("sha256:new", &["latest"])],
        );
        assert_eq!(status.running_matches_tag, Some(false));
        assert!(status.mid_rollout);
        assert!(!status.rolled_forward);
        let summary = status.summary.to_lowercase();
        assert!(summary.contains("in progress"), "{}", status.summary);
        assert!(!summary.contains("rolled back"), "{}", status.summary);
    }

    #[test]
    fn synthesize_zero_running_tasks() {
        let svc = service(vec![deployment("PRIMARY", "IN_PROGRESS")], 0, 1, 1);
        let status = synthesize(
            "prod",
            "workforce-prod",
            &svc,
            Some("repo:latest".into()),
            Some(parsed_ref()),
            &[],
            Some("sha256:new".into()),
            &[],
        );
        assert_eq!(status.running_matches_tag, None);
        assert!(!status.rolled_forward, "nothing running");
        assert!(status.running.is_empty());
        assert!(
            status.summary.contains("no running tasks"),
            "{}",
            status.summary
        );
    }

    #[test]
    fn synthesize_non_ecr_image_skips_tag_comparison() {
        let svc = service(vec![deployment("PRIMARY", "COMPLETED")], 1, 1, 0);
        let status = synthesize(
            "prod",
            "workforce-prod",
            &svc,
            Some("nginx:latest".into()),
            None,
            &["sha256:z".to_string()],
            None,
            &[],
        );
        assert_eq!(status.running_matches_tag, None);
        assert!(
            status.rolled_forward,
            "no tag to mismatch, otherwise complete"
        );
        assert!(status.configured_repository.is_none());
    }

    fn task(digest: &str) -> EcsTask {
        EcsTask {
            task_arn: Some("arn:task/1".into()),
            task_definition_arn: Some("workforce:42".into()),
            last_status: Some("RUNNING".into()),
            desired_status: Some("RUNNING".into()),
            health_status: Some("HEALTHY".into()),
            stop_code: None,
            stopped_reason: None,
            started_at: None,
            stopped_at: None,
            containers: vec![EcsContainer {
                name: Some("workforce-prod".into()),
                last_status: Some("RUNNING".into()),
                exit_code: None,
                reason: None,
                image: Some("repo:latest".into()),
                image_digest: Some(digest.into()),
                health_status: Some("HEALTHY".into()),
            }],
        }
    }

    fn task_def() -> EcsTaskDefinition {
        EcsTaskDefinition {
            family: Some("workforce".into()),
            revision: 42,
            task_definition_arn: Some("workforce:42".into()),
            cpu: Some("1024".into()),
            memory: Some("2048".into()),
            containers: vec![EcsContainerDef {
                name: Some("workforce-prod".into()),
                image: Some(
                    "111122223333.dkr.ecr.eu-west-2.amazonaws.com/workforce-prod:latest".into(),
                ),
                cpu: Some(1024),
                memory: Some(2048),
                essential: Some(true),
                environment_keys: vec!["WORKFORCE_LOG_LEVEL".into()],
                secret_keys: vec!["ANTHROPIC_API_KEY".into()],
            }],
        }
    }

    #[test]
    fn running_digests_dedupes_by_primary_container() {
        let tasks = vec![task("sha256:same"), task("sha256:same")];
        let digests = running_digests(Some("workforce-prod"), &tasks);
        assert_eq!(digests, vec!["sha256:same".to_string()]);
    }

    #[tokio::test]
    async fn gather_resolves_rolled_forward_end_to_end() {
        let backend = MockInfraBackend {
            services: parking_lot::Mutex::new(Some(Ok(vec![service(
                vec![deployment("PRIMARY", "COMPLETED")],
                1,
                1,
                0,
            )]))),
            task_arns: parking_lot::Mutex::new(Some(Ok(vec!["arn:task/1".to_string()]))),
            tasks: parking_lot::Mutex::new(Some(Ok(vec![task("sha256:new")]))),
            task_definition: parking_lot::Mutex::new(Some(Ok(task_def()))),
            ecr_images: parking_lot::Mutex::new(vec![image("sha256:new", &["latest"])]),
            ..Default::default()
        };
        let ctx = SessionContext::capture();
        let status = gather(&backend, &ctx, "prod", "workforce-prod")
            .await
            .unwrap();
        assert!(status.rolled_forward);
        assert_eq!(status.running_matches_tag, Some(true));
        assert_eq!(
            status.configured_repository.as_deref(),
            Some("workforce-prod")
        );
        assert_eq!(status.running[0].tags, vec!["latest".to_string()]);
    }

    #[tokio::test]
    async fn gather_detects_rollback_end_to_end() {
        let backend = MockInfraBackend {
            services: parking_lot::Mutex::new(Some(Ok(vec![service(
                vec![deployment("PRIMARY", "COMPLETED")],
                1,
                1,
                0,
            )]))),
            task_arns: parking_lot::Mutex::new(Some(Ok(vec!["arn:task/1".to_string()]))),
            tasks: parking_lot::Mutex::new(Some(Ok(vec![task("sha256:old")]))),
            task_definition: parking_lot::Mutex::new(Some(Ok(task_def()))),
            ecr_images: parking_lot::Mutex::new(vec![
                image("sha256:new", &["latest"]),
                image("sha256:old", &[]),
            ]),
            ..Default::default()
        };
        let ctx = SessionContext::capture();
        let status = gather(&backend, &ctx, "prod", "workforce-prod")
            .await
            .unwrap();
        assert_eq!(status.running_matches_tag, Some(false));
        assert!(!status.rolled_forward);
        assert!(status.summary.to_lowercase().contains("rolled back"));
    }
}
