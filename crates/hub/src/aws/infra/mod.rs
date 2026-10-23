//! `aws.{ecs,ecr,elbv2,rds,sts,logs,cloudwatch}.*` + `aws.deploy.status` —
//! read-only infra self-diagnosis tools, gated by `[aws_infra]` and executed
//! under the dedicated `atlas-infra-reader` assume-role.

pub mod backend;
pub mod cloudwatch;
pub mod deploy;
pub mod ecr;
pub mod ecs;
pub mod elb;
pub mod rds;
pub mod sts;

use std::time::Instant;

use serde::Serialize;
use tracing::info;

use workforce_agent::tools::ToolResult;
use workforce_policy::rules::{PolicyRule, PolicyScope, RuleAction};

pub use backend::{InfraBackend, SdkInfraBackend};
pub use cloudwatch::{
    AwsCloudwatchGetMetricDataTool, AwsLogsDescribeLogGroupsTool, AwsLogsFilterEventsTool,
};
pub use deploy::AwsDeployStatusTool;
pub use ecr::{AwsEcrDescribeImagesTool, AwsEcrListRepositoriesTool};
pub use ecs::{
    AwsEcsDescribeContainerInstancesTool, AwsEcsDescribeServicesTool,
    AwsEcsDescribeTaskDefinitionTool, AwsEcsDescribeTasksTool, AwsEcsListClustersTool,
    AwsEcsListServicesTool, AwsEcsListTasksTool,
};
pub use elb::AwsElbDescribeTargetHealthTool;
pub use rds::{AwsRdsDescribeDbClustersTool, AwsRdsDescribeEventsTool};
pub use sts::AwsStsGetCallerIdentityTool;

/// Every tool this module registers. The source of truth for the
/// allow-default constitutional policy and the registration log line.
pub const TOOL_NAMES: &[&str] = &[
    "aws.ecs.list_clusters",
    "aws.ecs.describe_services",
    "aws.ecs.list_services",
    "aws.ecs.list_tasks",
    "aws.ecs.describe_tasks",
    "aws.ecs.describe_task_definition",
    "aws.ecs.describe_container_instances",
    "aws.ecr.list_repositories",
    "aws.ecr.describe_images",
    "aws.elbv2.describe_target_health",
    "aws.rds.describe_db_clusters",
    "aws.rds.describe_events",
    "aws.sts.get_caller_identity",
    "aws.deploy.status",
    "aws.logs.describe_log_groups",
    "aws.logs.filter_events",
    "aws.cloudwatch.get_metric_data",
];

/// All infra tools default to `Allow`: they return non-PII control-plane
/// metadata, and the IAM role's read-only action scope is the authorisation
/// boundary, so a per-call human prompt adds friction without a control the
/// admin does not already hold.
pub fn default_policies() -> Vec<PolicyRule> {
    TOOL_NAMES
        .iter()
        .map(|tool| PolicyRule {
            name: format!("{tool}-allow-default"),
            scope: PolicyScope::Global,
            tool: (*tool).to_string(),
            action: RuleAction::Allow,
            conditions: Vec::new(),
            priority: 10,
            constitutional: true,
        })
        .collect()
}

pub(crate) fn elapsed(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

/// Shared tail for every tool's `execute`: serialise the domain value on
/// success, map [`backend::AwsError`] to a failure on error, and emit one
/// structured `aws.infra` log line either way.
pub(crate) fn finish<T: Serialize>(
    op: &'static str,
    detail: String,
    started: Instant,
    result: Result<T, backend::AwsError>,
) -> ToolResult {
    let duration_ms = elapsed(started);
    match result {
        Ok(value) => match serde_json::to_value(value) {
            Ok(output) => {
                info!(target: "aws.infra", op, detail = %detail, duration_ms, outcome = "ok", "infra tool");
                ToolResult::success(output, duration_ms)
            }
            Err(e) => ToolResult::failure(format!("serialise {op} output: {e}"), duration_ms),
        },
        Err(err) => {
            info!(target: "aws.infra", op, detail = %detail, duration_ms, outcome = "error", error = %err, "infra tool failed");
            ToolResult::failure(err.to_string(), duration_ms)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policies_allow_every_tool() {
        let rules = default_policies();
        assert_eq!(rules.len(), TOOL_NAMES.len());
        for rule in &rules {
            assert_eq!(rule.action, RuleAction::Allow);
            assert!(rule.constitutional);
            assert_eq!(rule.priority, 10);
        }
    }
}
