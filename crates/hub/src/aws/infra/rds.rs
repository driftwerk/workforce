//! `aws.rds.*` — Aurora/RDS cluster status and recent events (failovers,
//! storage, restarts).

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

const DEFAULT_EVENT_MINUTES: i32 = 120;
const HARD_MAX_EVENT_MINUTES: i32 = 20_160;

#[derive(Deserialize)]
struct ClustersInput {
    #[serde(default)]
    db_cluster_identifier: Option<String>,
}

pub struct AwsRdsDescribeDbClustersTool {
    backend: Arc<dyn InfraBackend>,
}

impl AwsRdsDescribeDbClustersTool {
    pub fn new(backend: Arc<dyn InfraBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Tool for AwsRdsDescribeDbClustersTool {
    fn name(&self) -> &str {
        "aws.rds.describe_db_clusters"
    }

    fn description(&self) -> &str {
        "Describe RDS/Aurora DB clusters: status, engine + version, writer/reader endpoints, \
         members and multi-AZ. Omit db_cluster_identifier to list all clusters."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "db_cluster_identifier": { "type": "string", "description": "Optional cluster identifier; all clusters when omitted." }
            }
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let p: ClustersInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        let ctx = SessionContext::capture();
        let detail = p
            .db_cluster_identifier
            .clone()
            .unwrap_or_else(|| "*".to_string());
        let result = self
            .backend
            .describe_db_clusters(&ctx, p.db_cluster_identifier.as_deref())
            .await;
        Ok(finish(
            "aws.rds.describe_db_clusters",
            detail,
            start,
            result,
        ))
    }
}

#[derive(Deserialize)]
struct EventsInput {
    #[serde(default)]
    source_identifier: Option<String>,
    #[serde(default)]
    source_type: Option<String>,
    #[serde(default)]
    duration_minutes: Option<i32>,
}

pub struct AwsRdsDescribeEventsTool {
    backend: Arc<dyn InfraBackend>,
}

impl AwsRdsDescribeEventsTool {
    pub fn new(backend: Arc<dyn InfraBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Tool for AwsRdsDescribeEventsTool {
    fn name(&self) -> &str {
        "aws.rds.describe_events"
    }

    fn description(&self) -> &str {
        "List recent RDS events (failovers, storage, restarts, parameter changes) over the last \
         duration_minutes (default 120). Scope with source_identifier and source_type \
         (db-cluster / db-instance / …)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source_identifier": { "type": "string", "description": "Optional source id (cluster/instance identifier)." },
                "source_type": {
                    "type": "string",
                    "enum": ["db-instance", "db-cluster", "db-parameter-group", "db-security-group", "db-snapshot", "db-cluster-snapshot"],
                    "description": "Optional source type filter."
                },
                "duration_minutes": { "type": "integer", "minimum": 1, "maximum": HARD_MAX_EVENT_MINUTES, "description": "Lookback window in minutes (default 120)." }
            }
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let p: EventsInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        let duration = p
            .duration_minutes
            .unwrap_or(DEFAULT_EVENT_MINUTES)
            .clamp(1, HARD_MAX_EVENT_MINUTES);
        let ctx = SessionContext::capture();
        let detail = p
            .source_identifier
            .clone()
            .unwrap_or_else(|| "*".to_string());
        let result = self
            .backend
            .describe_events(
                &ctx,
                p.source_identifier.as_deref(),
                p.source_type.as_deref(),
                duration,
            )
            .await;
        Ok(finish("aws.rds.describe_events", detail, start, result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::infra::backend::mock::MockInfraBackend;
    use crate::aws::infra::backend::{DbCluster, DbClusterMember};

    #[tokio::test]
    async fn describe_db_clusters_serialises_status_and_members() {
        let backend = MockInfraBackend {
            db_clusters: parking_lot::Mutex::new(Some(Ok(vec![DbCluster {
                identifier: Some("core-api-ec2-prod".into()),
                status: Some("available".into()),
                engine: Some("aurora-postgresql".into()),
                engine_version: Some("16.4".into()),
                endpoint: Some("writer.rds".into()),
                reader_endpoint: Some("reader.rds".into()),
                multi_az: true,
                members: vec![DbClusterMember {
                    instance_id: Some("core-api-ec2-prod-1".into()),
                    is_writer: true,
                }],
            }]))),
            ..Default::default()
        };
        let tool = AwsRdsDescribeDbClustersTool::new(Arc::new(backend));
        let res = tool.execute(json!({})).await.unwrap();
        assert!(res.success);
        assert_eq!(res.output[0]["status"], "available");
        assert_eq!(res.output[0]["members"][0]["is_writer"], true);
    }

    #[tokio::test]
    async fn describe_events_handles_empty_input() {
        let backend = MockInfraBackend {
            events: parking_lot::Mutex::new(Some(Ok(Vec::new()))),
            ..Default::default()
        };
        let tool = AwsRdsDescribeEventsTool::new(Arc::new(backend));
        let res = tool.execute(json!({})).await.unwrap();
        assert!(res.success);
        assert_eq!(res.output.as_array().unwrap().len(), 0);
    }
}
