//! `aws.ecs.*` — read-only ECS control-plane tools: cluster/service
//! discovery, service deployment state, task stop reasons / exit codes,
//! task-definition shape, and container-instance (host) scheduling health.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use workforce_agent::runner::SessionContext;
use workforce_agent::tools::{Tool, ToolResult};

use super::backend::{AwsError, EcsContainerInstance, InfraBackend};
use super::{elapsed, finish};

const DEFAULT_MAX_TASKS: i32 = 100;
const HARD_MAX_TASKS: i32 = 100;
const DEFAULT_MAX_SERVICES: i32 = 100;
const HARD_MAX_SERVICES: i32 = 100;

pub struct AwsEcsListClustersTool {
    backend: Arc<dyn InfraBackend>,
}

impl AwsEcsListClustersTool {
    pub fn new(backend: Arc<dyn InfraBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Tool for AwsEcsListClustersTool {
    fn name(&self) -> &str {
        "aws.ecs.list_clusters"
    }

    fn description(&self) -> &str {
        "List the ECS clusters in the account: name, ARN, status, and registered container \
         instance / running task / pending task / active service counts. The starting point when \
         you don't yet know a cluster name — feed one into aws.ecs.list_services or \
         aws.ecs.describe_services. Takes no arguments."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let ctx = SessionContext::capture();
        let result = self.backend.list_clusters(&ctx).await;
        Ok(finish(
            "aws.ecs.list_clusters",
            String::new(),
            start,
            result,
        ))
    }
}

#[derive(Deserialize)]
struct DescribeServicesInput {
    cluster: String,
    services: Vec<String>,
}

pub struct AwsEcsDescribeServicesTool {
    backend: Arc<dyn InfraBackend>,
}

impl AwsEcsDescribeServicesTool {
    pub fn new(backend: Arc<dyn InfraBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Tool for AwsEcsDescribeServicesTool {
    fn name(&self) -> &str {
        "aws.ecs.describe_services"
    }

    fn description(&self) -> &str {
        "Describe ECS services: running/desired/pending counts, the deployments[] with \
         rolloutState (IN_PROGRESS/COMPLETED/FAILED) and PRIMARY vs ACTIVE task definitions, and \
         recent service events. The first stop for 'did the deploy roll forward?'."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "cluster": { "type": "string", "description": "Cluster name or ARN." },
                "services": {
                    "type": "array", "items": { "type": "string" },
                    "description": "Service names or ARNs (at least one)."
                }
            },
            "required": ["cluster", "services"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let p: DescribeServicesInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        if p.services.is_empty() {
            return Ok(ToolResult::failure(
                "provide at least one service name in `services`",
                elapsed(start),
            ));
        }
        let ctx = SessionContext::capture();
        let detail = format!("{} [{}]", p.cluster, p.services.join(","));
        let result = self
            .backend
            .describe_services(&ctx, &p.cluster, &p.services)
            .await;
        Ok(finish("aws.ecs.describe_services", detail, start, result))
    }
}

#[derive(Deserialize)]
struct ListServicesInput {
    cluster: String,
    #[serde(default)]
    max: Option<i32>,
}

pub struct AwsEcsListServicesTool {
    backend: Arc<dyn InfraBackend>,
}

impl AwsEcsListServicesTool {
    pub fn new(backend: Arc<dyn InfraBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Tool for AwsEcsListServicesTool {
    fn name(&self) -> &str {
        "aws.ecs.list_services"
    }

    fn description(&self) -> &str {
        "List ECS service ARNs in a cluster. Discovers the auto-generated service names you can't \
         guess; pass the ARNs straight into aws.ecs.describe_services (which accepts names or \
         ARNs)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "cluster": { "type": "string", "description": "Cluster name or ARN." },
                "max": { "type": "integer", "minimum": 1, "maximum": HARD_MAX_SERVICES, "description": "Max service ARNs (default 100)." }
            },
            "required": ["cluster"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let p: ListServicesInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        let max = p
            .max
            .unwrap_or(DEFAULT_MAX_SERVICES)
            .clamp(1, HARD_MAX_SERVICES);
        let ctx = SessionContext::capture();
        let detail = p.cluster.clone();
        let result = self
            .backend
            .list_services(&ctx, &p.cluster, max)
            .await
            .map(|service_arns| json!({ "service_arns": service_arns }));
        Ok(finish("aws.ecs.list_services", detail, start, result))
    }
}

#[derive(Deserialize)]
struct ListTasksInput {
    cluster: String,
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    desired_status: Option<String>,
    #[serde(default)]
    max: Option<i32>,
}

pub struct AwsEcsListTasksTool {
    backend: Arc<dyn InfraBackend>,
}

impl AwsEcsListTasksTool {
    pub fn new(backend: Arc<dyn InfraBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Tool for AwsEcsListTasksTool {
    fn name(&self) -> &str {
        "aws.ecs.list_tasks"
    }

    fn description(&self) -> &str {
        "List ECS task ARNs in a cluster, optionally filtered by service and desired_status \
         (RUNNING / STOPPED / PENDING). Pair with aws.ecs.describe_tasks to inspect stop reasons \
         and exit codes."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "cluster": { "type": "string", "description": "Cluster name or ARN." },
                "service": { "type": "string", "description": "Optional service name filter." },
                "desired_status": {
                    "type": "string", "enum": ["RUNNING", "STOPPED", "PENDING"],
                    "description": "Optional desired-status filter. STOPPED surfaces crashed tasks."
                },
                "max": { "type": "integer", "minimum": 1, "maximum": HARD_MAX_TASKS, "description": "Max ARNs (default 100)." }
            },
            "required": ["cluster"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let p: ListTasksInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        let max = p.max.unwrap_or(DEFAULT_MAX_TASKS).clamp(1, HARD_MAX_TASKS);
        let ctx = SessionContext::capture();
        let detail = p.service.clone().unwrap_or_else(|| p.cluster.clone());
        let result = self
            .backend
            .list_tasks(
                &ctx,
                &p.cluster,
                p.service.as_deref(),
                p.desired_status.as_deref(),
                max,
            )
            .await
            .map(|task_arns| json!({ "task_arns": task_arns }));
        Ok(finish("aws.ecs.list_tasks", detail, start, result))
    }
}

#[derive(Deserialize)]
struct DescribeTasksInput {
    cluster: String,
    task_arns: Vec<String>,
}

pub struct AwsEcsDescribeTasksTool {
    backend: Arc<dyn InfraBackend>,
}

impl AwsEcsDescribeTasksTool {
    pub fn new(backend: Arc<dyn InfraBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Tool for AwsEcsDescribeTasksTool {
    fn name(&self) -> &str {
        "aws.ecs.describe_tasks"
    }

    fn description(&self) -> &str {
        "Describe ECS tasks by ARN: lastStatus, stopCode + stoppedReason, per-container exitCode \
         (137 = OOM-killed), resolved image + imageDigest, and the task definition. The answer to \
         'why did the container die?'."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "cluster": { "type": "string", "description": "Cluster name or ARN." },
                "task_arns": {
                    "type": "array", "items": { "type": "string" },
                    "description": "Task ARNs from aws.ecs.list_tasks (at least one)."
                }
            },
            "required": ["cluster", "task_arns"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let p: DescribeTasksInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        if p.task_arns.is_empty() {
            return Ok(ToolResult::failure(
                "provide at least one task ARN in `task_arns`",
                elapsed(start),
            ));
        }
        let ctx = SessionContext::capture();
        let detail = format!("{} ({} task(s))", p.cluster, p.task_arns.len());
        let result = self
            .backend
            .describe_tasks(&ctx, &p.cluster, &p.task_arns)
            .await;
        Ok(finish("aws.ecs.describe_tasks", detail, start, result))
    }
}

#[derive(Deserialize)]
struct DescribeTaskDefinitionInput {
    task_definition: String,
}

pub struct AwsEcsDescribeTaskDefinitionTool {
    backend: Arc<dyn InfraBackend>,
}

impl AwsEcsDescribeTaskDefinitionTool {
    pub fn new(backend: Arc<dyn InfraBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Tool for AwsEcsDescribeTaskDefinitionTool {
    fn name(&self) -> &str {
        "aws.ecs.describe_task_definition"
    }

    fn description(&self) -> &str {
        "Describe an ECS task definition (family:revision or ARN): revision, cpu/memory, and per \
         container image + cpu/memory/essential. Environment and secret KEYS only are returned — \
         never their values."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_definition": { "type": "string", "description": "family:revision, family (latest ACTIVE), or full ARN." }
            },
            "required": ["task_definition"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let p: DescribeTaskDefinitionInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        let ctx = SessionContext::capture();
        let detail = p.task_definition.clone();
        let result = self
            .backend
            .describe_task_definition(&ctx, &p.task_definition)
            .await;
        Ok(finish(
            "aws.ecs.describe_task_definition",
            detail,
            start,
            result,
        ))
    }
}

#[derive(Deserialize)]
struct DescribeContainerInstancesInput {
    cluster: String,
    #[serde(default)]
    container_instance_arns: Option<Vec<String>>,
}

pub struct AwsEcsDescribeContainerInstancesTool {
    backend: Arc<dyn InfraBackend>,
}

impl AwsEcsDescribeContainerInstancesTool {
    pub fn new(backend: Arc<dyn InfraBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Tool for AwsEcsDescribeContainerInstancesTool {
    fn name(&self) -> &str {
        "aws.ecs.describe_container_instances"
    }

    fn description(&self) -> &str {
        "Inspect ECS container-instance (host) scheduling health: agentConnected, status, and \
         registered/remaining CPU + memory and running/pending task counts. Lists the cluster's \
         instances when container_instance_arns is omitted. Use to spot a wedged or resource \
         starved host."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "cluster": { "type": "string", "description": "Cluster name or ARN." },
                "container_instance_arns": {
                    "type": "array", "items": { "type": "string" },
                    "description": "Optional explicit instance ARNs; all cluster instances when omitted."
                }
            },
            "required": ["cluster"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let p: DescribeContainerInstancesInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        let ctx = SessionContext::capture();
        let detail = p.cluster.clone();
        let result = container_instances(
            self.backend.as_ref(),
            &ctx,
            &p.cluster,
            p.container_instance_arns,
        )
        .await;
        Ok(finish(
            "aws.ecs.describe_container_instances",
            detail,
            start,
            result,
        ))
    }
}

async fn container_instances(
    backend: &dyn InfraBackend,
    ctx: &SessionContext,
    cluster: &str,
    arns: Option<Vec<String>>,
) -> Result<Vec<EcsContainerInstance>, AwsError> {
    let arns = match arns {
        Some(a) if !a.is_empty() => a,
        _ => backend.list_container_instances(ctx, cluster).await?,
    };
    backend
        .describe_container_instances(ctx, cluster, &arns)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::infra::backend::mock::MockInfraBackend;
    use crate::aws::infra::backend::{EcsClusterSummary, EcsServiceSummary, EcsTask};

    fn svc() -> EcsServiceSummary {
        EcsServiceSummary {
            service_name: Some("workforce-prod".into()),
            status: Some("ACTIVE".into()),
            desired_count: 1,
            running_count: 1,
            pending_count: 0,
            task_definition: Some("workforce:42".into()),
            deployments: Vec::new(),
            events: Vec::new(),
        }
    }

    #[tokio::test]
    async fn list_clusters_serialises_summaries() {
        let backend = MockInfraBackend {
            clusters: parking_lot::Mutex::new(Some(Ok(vec![EcsClusterSummary {
                name: Some("core-api-ec2-prod-cluster".into()),
                arn: Some("arn:aws:ecs:eu-west-2:1:cluster/core-api-ec2-prod-cluster".into()),
                status: Some("ACTIVE".into()),
                registered_container_instances_count: 1,
                running_tasks_count: 3,
                pending_tasks_count: 0,
                active_services_count: 2,
            }]))),
            ..Default::default()
        };
        let tool = AwsEcsListClustersTool::new(Arc::new(backend));
        let res = tool.execute(json!({})).await.unwrap();
        assert!(res.success);
        assert_eq!(res.output[0]["name"], "core-api-ec2-prod-cluster");
        assert_eq!(res.output[0]["active_services_count"], 2);
    }

    #[tokio::test]
    async fn list_clusters_maps_error_to_failure() {
        let backend = MockInfraBackend {
            clusters: parking_lot::Mutex::new(Some(Err(AwsError::AccessDenied))),
            ..Default::default()
        };
        let tool = AwsEcsListClustersTool::new(Arc::new(backend));
        let res = tool.execute(json!({})).await.unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("access denied"));
    }

    #[tokio::test]
    async fn list_services_returns_arns() {
        let backend = MockInfraBackend {
            service_arns: parking_lot::Mutex::new(Some(Ok(vec![
                "arn:aws:ecs:eu-west-2:1:service/c/workforce-prod-service".into(),
            ]))),
            ..Default::default()
        };
        let tool = AwsEcsListServicesTool::new(Arc::new(backend));
        let res = tool.execute(json!({ "cluster": "c" })).await.unwrap();
        assert!(res.success);
        assert_eq!(
            res.output["service_arns"][0],
            "arn:aws:ecs:eu-west-2:1:service/c/workforce-prod-service"
        );
    }

    #[tokio::test]
    async fn list_services_requires_cluster() {
        let backend = Arc::new(MockInfraBackend::default());
        let tool = AwsEcsListServicesTool::new(backend);
        let res = tool.execute(json!({})).await.unwrap();
        assert!(!res.success);
    }

    #[tokio::test]
    async fn list_services_maps_error_to_failure() {
        let backend = MockInfraBackend {
            service_arns: parking_lot::Mutex::new(Some(Err(AwsError::AccessDenied))),
            ..Default::default()
        };
        let tool = AwsEcsListServicesTool::new(Arc::new(backend));
        let res = tool.execute(json!({ "cluster": "c" })).await.unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("access denied"));
    }

    #[tokio::test]
    async fn describe_services_rejects_empty_services() {
        let backend = Arc::new(MockInfraBackend::default());
        let tool = AwsEcsDescribeServicesTool::new(backend);
        let res = tool
            .execute(json!({ "cluster": "c", "services": [] }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("at least one service"));
    }

    #[tokio::test]
    async fn describe_services_serialises_backend_result() {
        let backend = MockInfraBackend {
            services: parking_lot::Mutex::new(Some(Ok(vec![svc()]))),
            ..Default::default()
        };
        let tool = AwsEcsDescribeServicesTool::new(Arc::new(backend));
        let res = tool
            .execute(json!({ "cluster": "c", "services": ["workforce-prod"] }))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output[0]["service_name"], "workforce-prod");
        assert_eq!(res.output[0]["running_count"], 1);
    }

    #[tokio::test]
    async fn describe_tasks_rejects_empty_arns() {
        let backend = Arc::new(MockInfraBackend::default());
        let tool = AwsEcsDescribeTasksTool::new(backend);
        let res = tool
            .execute(json!({ "cluster": "c", "task_arns": [] }))
            .await
            .unwrap();
        assert!(!res.success);
    }

    #[tokio::test]
    async fn describe_tasks_surfaces_exit_code() {
        let task = EcsTask {
            task_arn: Some("arn:task/1".into()),
            task_definition_arn: Some("workforce:42".into()),
            last_status: Some("STOPPED".into()),
            desired_status: Some("STOPPED".into()),
            health_status: None,
            stop_code: Some("EssentialContainerExited".into()),
            stopped_reason: Some("Essential container in task exited".into()),
            started_at: None,
            stopped_at: None,
            containers: vec![crate::aws::infra::backend::EcsContainer {
                name: Some("workforce-prod".into()),
                last_status: Some("STOPPED".into()),
                exit_code: Some(137),
                reason: None,
                image: Some("repo:latest".into()),
                image_digest: Some("sha256:x".into()),
                health_status: None,
            }],
        };
        let backend = MockInfraBackend {
            tasks: parking_lot::Mutex::new(Some(Ok(vec![task]))),
            ..Default::default()
        };
        let tool = AwsEcsDescribeTasksTool::new(Arc::new(backend));
        let res = tool
            .execute(json!({ "cluster": "c", "task_arns": ["arn:task/1"] }))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output[0]["containers"][0]["exit_code"], 137);
    }

    #[tokio::test]
    async fn list_tasks_maps_error_to_failure() {
        let backend = MockInfraBackend {
            task_arns: parking_lot::Mutex::new(Some(Err(AwsError::AccessDenied))),
            ..Default::default()
        };
        let tool = AwsEcsListTasksTool::new(Arc::new(backend));
        let res = tool.execute(json!({ "cluster": "c" })).await.unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("access denied"));
    }

    #[tokio::test]
    async fn container_instances_lists_when_arns_omitted() {
        let backend = MockInfraBackend {
            container_instance_arns: parking_lot::Mutex::new(Some(Ok(vec!["arn:ci/1".into()]))),
            container_instances: parking_lot::Mutex::new(Some(Ok(vec![EcsContainerInstance {
                container_instance_arn: Some("arn:ci/1".into()),
                ec2_instance_id: Some("i-123".into()),
                status: Some("ACTIVE".into()),
                agent_connected: true,
                running_tasks_count: 3,
                pending_tasks_count: 0,
                registered_cpu: Some(8192),
                remaining_cpu: Some(2048),
                registered_memory: Some(16000),
                remaining_memory: Some(4000),
            }]))),
            ..Default::default()
        };
        let tool = AwsEcsDescribeContainerInstancesTool::new(Arc::new(backend));
        let res = tool.execute(json!({ "cluster": "c" })).await.unwrap();
        assert!(res.success);
        assert_eq!(res.output[0]["agent_connected"], true);
        assert_eq!(res.output[0]["running_tasks_count"], 3);
    }
}
