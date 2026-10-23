//! `aws.sts.get_caller_identity` — the account, ARN and userId of the
//! assumed reader role. Confirms which identity the infra tools run as when
//! debugging access (an AccessDenied is far quicker to read once you know
//! the principal).

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde_json::{json, Value};

use workforce_agent::runner::SessionContext;
use workforce_agent::tools::{Tool, ToolResult};

use super::backend::InfraBackend;
use super::finish;

pub struct AwsStsGetCallerIdentityTool {
    backend: Arc<dyn InfraBackend>,
}

impl AwsStsGetCallerIdentityTool {
    pub fn new(backend: Arc<dyn InfraBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Tool for AwsStsGetCallerIdentityTool {
    fn name(&self) -> &str {
        "aws.sts.get_caller_identity"
    }

    fn description(&self) -> &str {
        "Return the AWS account, ARN and userId the infra tools are calling as (the assumed \
         atlas-infra-reader role). Takes no arguments."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let ctx = SessionContext::capture();
        let result = self.backend.get_caller_identity(&ctx).await;
        Ok(finish(
            "aws.sts.get_caller_identity",
            String::new(),
            start,
            result,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::infra::backend::mock::MockInfraBackend;
    use crate::aws::infra::backend::CallerIdentity;

    #[tokio::test]
    async fn get_caller_identity_serialises_arn() {
        let backend = MockInfraBackend {
            caller_identity: parking_lot::Mutex::new(Some(Ok(CallerIdentity {
                account: Some("123456789012".into()),
                arn: Some(
                    "arn:aws:sts::123456789012:assumed-role/atlas-infra-reader/hub-atlas-abc"
                        .into(),
                ),
                user_id: Some("AROAEXAMPLE:hub-atlas-abc".into()),
            }))),
            ..Default::default()
        };
        let tool = AwsStsGetCallerIdentityTool::new(Arc::new(backend));
        let res = tool.execute(json!({})).await.unwrap();
        assert!(res.success);
        assert_eq!(res.output["account"], "123456789012");
        assert!(res.output["arn"]
            .as_str()
            .unwrap()
            .contains("atlas-infra-reader"));
    }
}
