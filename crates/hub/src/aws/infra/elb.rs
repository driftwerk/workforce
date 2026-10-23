//! `aws.elbv2.describe_target_health` — per-target health for an ALB/NLB
//! target group; the answer to "why is the load balancer 5xx-ing / why does
//! the healthcheck fail".

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use workforce_agent::runner::SessionContext;
use workforce_agent::tools::{Tool, ToolResult};

use super::backend::InfraBackend;
use super::{elapsed, finish};

#[derive(Deserialize)]
struct Input {
    target_group_arn: String,
}

pub struct AwsElbDescribeTargetHealthTool {
    backend: Arc<dyn InfraBackend>,
}

impl AwsElbDescribeTargetHealthTool {
    pub fn new(backend: Arc<dyn InfraBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Tool for AwsElbDescribeTargetHealthTool {
    fn name(&self) -> &str {
        "aws.elbv2.describe_target_health"
    }

    fn description(&self) -> &str {
        "Describe the health of every target in an ELBv2 target group: target id, port, state \
         (healthy/unhealthy/draining/…), and the reason + description when unhealthy."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "target_group_arn": { "type": "string", "description": "Target group ARN." }
            },
            "required": ["target_group_arn"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let p: Input = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        let ctx = SessionContext::capture();
        let detail = p.target_group_arn.clone();
        let result = self
            .backend
            .describe_target_health(&ctx, &p.target_group_arn)
            .await;
        Ok(finish(
            "aws.elbv2.describe_target_health",
            detail,
            start,
            result,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::infra::backend::mock::MockInfraBackend;
    use crate::aws::infra::backend::{AwsError, TargetHealth};

    #[tokio::test]
    async fn describe_target_health_serialises_state_and_reason() {
        let backend = MockInfraBackend {
            target_health: parking_lot::Mutex::new(Some(Ok(vec![TargetHealth {
                target_id: Some("10.20.0.7".into()),
                port: Some(8090),
                state: Some("unhealthy".into()),
                reason: Some("Target.FailedHealthChecks".into()),
                description: Some("Health checks failed".into()),
            }]))),
            ..Default::default()
        };
        let tool = AwsElbDescribeTargetHealthTool::new(Arc::new(backend));
        let res = tool
            .execute(json!({ "target_group_arn": "arn:tg/1" }))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output[0]["state"], "unhealthy");
        assert_eq!(res.output[0]["port"], 8090);
    }

    #[tokio::test]
    async fn describe_target_health_maps_not_found() {
        let backend = MockInfraBackend {
            target_health: parking_lot::Mutex::new(Some(Err(AwsError::NotFound))),
            ..Default::default()
        };
        let tool = AwsElbDescribeTargetHealthTool::new(Arc::new(backend));
        let res = tool
            .execute(json!({ "target_group_arn": "arn:tg/missing" }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("not found"));
    }
}
