//! `aws.logs.*` + `aws.cloudwatch.*` — read-only CloudWatch Logs and metrics
//! tools for services whose logs do not reach Datadog, and for the
//! platform metrics (ECS CPU/memory, ALB 5xx, RDS connections) behind an
//! incident.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::{json, Value};
use workforce_agent::runner::SessionContext;
use workforce_agent::tools::{Tool, ToolResult};

use super::backend::{InfraBackend, LogQuery, MetricQuery};
use super::{elapsed, finish};

const MAX_GROUPS: i32 = 50;
const DEFAULT_MAX_EVENTS: i32 = 100;
const HARD_MAX_EVENTS: i32 = 500;
const DEFAULT_MINUTES: i32 = 60;
const HARD_MAX_MINUTES: i32 = 7 * 24 * 60;
const DEFAULT_PERIOD_SECS: i32 = 300;
const MIN_PERIOD_SECS: i32 = 60;

pub struct AwsLogsDescribeLogGroupsTool {
    backend: Arc<dyn InfraBackend>,
}

impl AwsLogsDescribeLogGroupsTool {
    pub fn new(backend: Arc<dyn InfraBackend>) -> Self {
        Self { backend }
    }
}

#[derive(Deserialize)]
struct DescribeLogGroupsInput {
    prefix: Option<String>,
    max: Option<i32>,
}

#[async_trait]
impl Tool for AwsLogsDescribeLogGroupsTool {
    fn name(&self) -> &str {
        "aws.logs.describe_log_groups"
    }

    fn description(&self) -> &str {
        "List CloudWatch log groups (name, stored bytes, retention days, creation time), \
         optionally by name prefix such as /ecs/ or /aws/lambda/. The starting point when you \
         don't know a service's log group — feed one into aws.logs.filter_events."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prefix": { "type": "string", "description": "Optional log group name prefix." },
                "max": { "type": "integer", "minimum": 1, "maximum": MAX_GROUPS, "description": "Groups to return (default 50)." }
            }
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let p: DescribeLogGroupsInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        let max = p.max.unwrap_or(MAX_GROUPS).clamp(1, MAX_GROUPS);
        let prefix = given(p.prefix);
        let ctx = SessionContext::capture();
        let detail = prefix.clone().unwrap_or_else(|| "*".to_string());
        let result = self
            .backend
            .describe_log_groups(&ctx, prefix.as_deref(), max)
            .await;
        Ok(finish(
            "aws.logs.describe_log_groups",
            detail,
            start,
            result,
        ))
    }
}

pub struct AwsLogsFilterEventsTool {
    backend: Arc<dyn InfraBackend>,
}

impl AwsLogsFilterEventsTool {
    pub fn new(backend: Arc<dyn InfraBackend>) -> Self {
        Self { backend }
    }
}

#[derive(Deserialize)]
struct FilterEventsInput {
    log_group: String,
    pattern: Option<String>,
    stream_prefix: Option<String>,
    minutes: Option<i32>,
    max: Option<i32>,
}

#[async_trait]
impl Tool for AwsLogsFilterEventsTool {
    fn name(&self) -> &str {
        "aws.logs.filter_events"
    }

    fn description(&self) -> &str {
        "Search a CloudWatch log group over the last `minutes` (default 60, max 7 days) with an \
         optional CloudWatch filter pattern (e.g. `ERROR`, `\"connection refused\"`, \
         `{ $.status = 500 }`) and stream-name prefix. Returns up to `max` events (default 100) \
         oldest first with timestamp, stream and message. `truncated: true` means the search \
         stopped before covering the whole window (not that there were no more matches) — \
         narrow the pattern or the window and search again."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "log_group": { "type": "string", "description": "Log group name, e.g. /ecs/task-manager-prod." },
                "pattern": { "type": "string", "description": "Optional CloudWatch filter pattern." },
                "stream_prefix": { "type": "string", "description": "Optional log stream name prefix." },
                "minutes": { "type": "integer", "minimum": 1, "maximum": HARD_MAX_MINUTES, "description": "Lookback window ending now (default 60)." },
                "max": { "type": "integer", "minimum": 1, "maximum": HARD_MAX_EVENTS, "description": "Events to return (default 100)." }
            },
            "required": ["log_group"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let p: FilterEventsInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        let query = log_query(p);
        let ctx = SessionContext::capture();
        let detail = query.log_group.clone();
        let result = self.backend.filter_log_events(&ctx, &query).await;
        Ok(finish("aws.logs.filter_events", detail, start, result))
    }
}

pub struct AwsCloudwatchGetMetricDataTool {
    backend: Arc<dyn InfraBackend>,
}

impl AwsCloudwatchGetMetricDataTool {
    pub fn new(backend: Arc<dyn InfraBackend>) -> Self {
        Self { backend }
    }
}

#[derive(Deserialize)]
struct Dimension {
    name: String,
    value: String,
}

#[derive(Deserialize)]
struct GetMetricDataInput {
    namespace: String,
    metric_name: String,
    #[serde(default)]
    dimensions: Vec<Dimension>,
    stat: Option<String>,
    period_secs: Option<i32>,
    minutes: Option<i32>,
}

#[async_trait]
impl Tool for AwsCloudwatchGetMetricDataTool {
    fn name(&self) -> &str {
        "aws.cloudwatch.get_metric_data"
    }

    fn description(&self) -> &str {
        "Read one CloudWatch metric as a time series over the last `minutes` (default 60, max 7 \
         days): namespace + metric_name + dimensions (e.g. AWS/ECS CPUUtilization with \
         ClusterName and ServiceName; AWS/ApplicationELB HTTPCode_Target_5XX_Count with \
         LoadBalancer; AWS/RDS DatabaseConnections with DBInstanceIdentifier), one statistic \
         (Average, Sum, Maximum, Minimum, SampleCount or a percentile like p99) per `period_secs` \
         (default 300, rounded up to a multiple of 60). Returns [{timestamp, value}] oldest \
         first."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "namespace": { "type": "string", "description": "Metric namespace, e.g. AWS/ECS." },
                "metric_name": { "type": "string", "description": "Metric name, e.g. CPUUtilization." },
                "dimensions": {
                    "type": "array",
                    "items": { "type": "object", "properties": { "name": { "type": "string" }, "value": { "type": "string" } }, "required": ["name", "value"] },
                    "description": "Dimensions identifying the resource."
                },
                "stat": { "type": "string", "description": "Statistic (default Average)." },
                "period_secs": { "type": "integer", "minimum": MIN_PERIOD_SECS, "description": "Aggregation period in seconds (default 300, rounded up to a multiple of 60)." },
                "minutes": { "type": "integer", "minimum": 1, "maximum": HARD_MAX_MINUTES, "description": "Lookback window ending now (default 60)." }
            },
            "required": ["namespace", "metric_name"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let p: GetMetricDataInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        let query = metric_query(p);
        let ctx = SessionContext::capture();
        let detail = format!("{}/{}", query.namespace, query.metric_name);
        let result = self.backend.get_metric_data(&ctx, &query).await;
        Ok(finish(
            "aws.cloudwatch.get_metric_data",
            detail,
            start,
            result,
        ))
    }
}

/// An optional text input, absent when blank.
fn given(value: Option<String>) -> Option<String> {
    value.filter(|s| !s.trim().is_empty())
}

fn log_query(p: FilterEventsInput) -> LogQuery {
    LogQuery {
        log_group: p.log_group,
        pattern: given(p.pattern),
        stream_prefix: given(p.stream_prefix),
        minutes: p
            .minutes
            .unwrap_or(DEFAULT_MINUTES)
            .clamp(1, HARD_MAX_MINUTES),
        max: p
            .max
            .unwrap_or(DEFAULT_MAX_EVENTS)
            .clamp(1, HARD_MAX_EVENTS),
    }
}

fn metric_query(p: GetMetricDataInput) -> MetricQuery {
    let period = p
        .period_secs
        .unwrap_or(DEFAULT_PERIOD_SECS)
        .max(MIN_PERIOD_SECS);
    MetricQuery {
        namespace: p.namespace,
        metric_name: p.metric_name,
        dimensions: p
            .dimensions
            .into_iter()
            .map(|d| (d.name, d.value))
            .collect(),
        stat: given(p.stat).unwrap_or_else(|| "Average".into()),
        period_secs: (period + MIN_PERIOD_SECS - 1) / MIN_PERIOD_SECS * MIN_PERIOD_SECS,
        minutes: p
            .minutes
            .unwrap_or(DEFAULT_MINUTES)
            .clamp(1, HARD_MAX_MINUTES),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::infra::backend::mock::MockInfraBackend;
    use crate::aws::infra::backend::{AwsError, LogEvent, LogEvents, LogGroup, MetricPoint};

    #[test]
    fn log_inputs_are_clamped_and_blank_options_dropped() {
        let q = log_query(
            serde_json::from_value(json!({
                "log_group": "/ecs/x", "pattern": "  ", "stream_prefix": "", "minutes": 100000, "max": 0
            }))
            .unwrap(),
        );
        assert_eq!((q.pattern, q.stream_prefix), (None, None));
        assert_eq!((q.minutes, q.max), (HARD_MAX_MINUTES, 1));
        let q = log_query(serde_json::from_value(json!({ "log_group": "/ecs/x" })).unwrap());
        assert_eq!((q.minutes, q.max), (DEFAULT_MINUTES, DEFAULT_MAX_EVENTS));
    }

    #[test]
    fn metric_inputs_default_and_round_the_period_to_whole_minutes() {
        let q = metric_query(
            serde_json::from_value(json!({
                "namespace": "AWS/ECS", "metric_name": "CPUUtilization", "stat": "", "period_secs": 90
            }))
            .unwrap(),
        );
        assert_eq!(q.stat, "Average");
        assert_eq!(q.period_secs, 120);
        assert!(q.dimensions.is_empty());
        let q = metric_query(
            serde_json::from_value(json!({
                "namespace": "AWS/ECS", "metric_name": "CPUUtilization", "period_secs": 10,
                "dimensions": [{ "name": "ClusterName", "value": "c" }], "stat": "p99"
            }))
            .unwrap(),
        );
        assert_eq!(q.period_secs, MIN_PERIOD_SECS);
        assert_eq!(
            q.dimensions,
            vec![("ClusterName".to_string(), "c".to_string())]
        );
        assert_eq!(q.stat, "p99");
    }

    #[tokio::test]
    async fn describe_log_groups_serialises_groups_and_accepts_no_input() {
        let backend = MockInfraBackend {
            log_groups: parking_lot::Mutex::new(Some(Ok(vec![LogGroup {
                name: Some("/ecs/task-manager-prod".into()),
                stored_bytes: Some(1024),
                retention_days: Some(30),
                created_at: Some("2026-01-01T00:00:00Z".into()),
            }]))),
            ..Default::default()
        };
        let tool = AwsLogsDescribeLogGroupsTool::new(Arc::new(backend));
        let res = tool.execute(json!({ "prefix": " " })).await.unwrap();
        assert!(res.success);
        assert_eq!(res.output[0]["name"], "/ecs/task-manager-prod");
        assert_eq!(res.output[0]["retention_days"], 30);
    }

    #[tokio::test]
    async fn filter_events_requires_a_log_group_and_reports_truncation() {
        let tool = AwsLogsFilterEventsTool::new(Arc::new(MockInfraBackend::default()));
        let res = tool.execute(json!({ "pattern": "ERROR" })).await.unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("log_group"));

        let backend = MockInfraBackend {
            log_events: parking_lot::Mutex::new(Some(Ok(LogEvents {
                events: vec![LogEvent {
                    timestamp: Some("2026-09-10T05:00:00Z".into()),
                    stream: Some("ecs/task-manager/abc".into()),
                    message: Some("ERROR boom".into()),
                }],
                truncated: true,
            }))),
            ..Default::default()
        };
        let tool = AwsLogsFilterEventsTool::new(Arc::new(backend));
        let res = tool
            .execute(json!({ "log_group": "/ecs/task-manager-prod" }))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["events"][0]["message"], "ERROR boom");
        assert_eq!(res.output["truncated"], true);
    }

    #[tokio::test]
    async fn get_metric_data_maps_denial_to_failure_and_points_to_output() {
        let backend = MockInfraBackend {
            metric_points: parking_lot::Mutex::new(Some(Err(AwsError::AccessDenied))),
            ..Default::default()
        };
        let tool = AwsCloudwatchGetMetricDataTool::new(Arc::new(backend));
        let res = tool
            .execute(json!({ "namespace": "AWS/ECS", "metric_name": "CPUUtilization" }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("access denied"));

        let backend = MockInfraBackend {
            metric_points: parking_lot::Mutex::new(Some(Ok(vec![MetricPoint {
                timestamp: "2026-09-10T05:00:00Z".into(),
                value: 42.5,
            }]))),
            ..Default::default()
        };
        let tool = AwsCloudwatchGetMetricDataTool::new(Arc::new(backend));
        let res = tool
            .execute(json!({ "namespace": "AWS/ECS", "metric_name": "CPUUtilization" }))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output[0]["value"], 42.5);
    }
}
