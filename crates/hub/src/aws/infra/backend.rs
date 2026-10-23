//! SDK boundary for the read-only infra tools: the [`InfraBackend`] trait,
//! the owned domain structs the tools serialise, and the production
//! [`SdkInfraBackend`] that maps each AWS SDK call onto them.
//!
//! Splitting the SDK behind a trait (as the S3 surface does) lets the tools
//! and the `deploy.status` orchestration be unit-tested with one
//! hand-written mock, with no SDK mocking machinery. Domain structs are
//! deliberately narrow — only the fields that answer a diagnostic question —
//! so tool output stays small and SDK types never cross the module boundary.

use std::sync::Arc;

use async_trait::async_trait;
use aws_sdk_ecs::error::ProvideErrorMetadata;
use eyre::Result;
use serde::Serialize;

use workforce_agent::runner::SessionContext;

use crate::aws::client::SdkConfigFactory;

/// Typed failure surfaced to the tools. A distinct vocabulary from the S3
/// object-store errors (which keep their own classifier), recognising the
/// control-plane exception codes and throttling these services return.
#[derive(Debug, Clone, thiserror::Error)]
pub enum AwsError {
    #[error("access denied")]
    AccessDenied,
    #[error("not found")]
    NotFound,
    #[error("throttled by AWS; retry shortly")]
    Throttled,
    #[error("AWS error: {0}")]
    Other(String),
}

/// Which images `describe_images` should return: a bounded recent page, or
/// the single image behind a tag / digest.
#[derive(Debug, Clone)]
pub enum ImageFilter {
    Recent { max: i32 },
    Tag(String),
    Digest(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct EcsClusterSummary {
    pub name: Option<String>,
    pub arn: Option<String>,
    pub status: Option<String>,
    pub registered_container_instances_count: i32,
    pub running_tasks_count: i32,
    pub pending_tasks_count: i32,
    pub active_services_count: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct EcsDeployment {
    pub id: Option<String>,
    pub status: Option<String>,
    pub task_definition: Option<String>,
    pub desired_count: i32,
    pub running_count: i32,
    pub pending_count: i32,
    pub rollout_state: Option<String>,
    pub rollout_state_reason: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EcsServiceEvent {
    pub created_at: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EcsServiceSummary {
    pub service_name: Option<String>,
    pub status: Option<String>,
    pub desired_count: i32,
    pub running_count: i32,
    pub pending_count: i32,
    pub task_definition: Option<String>,
    pub deployments: Vec<EcsDeployment>,
    pub events: Vec<EcsServiceEvent>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EcsContainer {
    pub name: Option<String>,
    pub last_status: Option<String>,
    pub exit_code: Option<i32>,
    pub reason: Option<String>,
    pub image: Option<String>,
    pub image_digest: Option<String>,
    pub health_status: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EcsTask {
    pub task_arn: Option<String>,
    pub task_definition_arn: Option<String>,
    pub last_status: Option<String>,
    pub desired_status: Option<String>,
    pub health_status: Option<String>,
    pub stop_code: Option<String>,
    pub stopped_reason: Option<String>,
    pub started_at: Option<String>,
    pub stopped_at: Option<String>,
    pub containers: Vec<EcsContainer>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EcsContainerDef {
    pub name: Option<String>,
    pub image: Option<String>,
    pub cpu: Option<i32>,
    pub memory: Option<i32>,
    pub essential: Option<bool>,
    pub environment_keys: Vec<String>,
    pub secret_keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EcsTaskDefinition {
    pub family: Option<String>,
    pub revision: i32,
    pub task_definition_arn: Option<String>,
    pub cpu: Option<String>,
    pub memory: Option<String>,
    pub containers: Vec<EcsContainerDef>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EcsContainerInstance {
    pub container_instance_arn: Option<String>,
    pub ec2_instance_id: Option<String>,
    pub status: Option<String>,
    pub agent_connected: bool,
    pub running_tasks_count: i32,
    pub pending_tasks_count: i32,
    pub registered_cpu: Option<i32>,
    pub remaining_cpu: Option<i32>,
    pub registered_memory: Option<i32>,
    pub remaining_memory: Option<i32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EcrRepository {
    pub name: Option<String>,
    pub uri: Option<String>,
    pub arn: Option<String>,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EcrImage {
    pub image_digest: Option<String>,
    pub image_tags: Vec<String>,
    pub pushed_at: Option<String>,
    pub size_bytes: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TargetHealth {
    pub target_id: Option<String>,
    pub port: Option<i32>,
    pub state: Option<String>,
    pub reason: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DbClusterMember {
    pub instance_id: Option<String>,
    pub is_writer: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DbCluster {
    pub identifier: Option<String>,
    pub status: Option<String>,
    pub engine: Option<String>,
    pub engine_version: Option<String>,
    pub endpoint: Option<String>,
    pub reader_endpoint: Option<String>,
    pub multi_az: bool,
    pub members: Vec<DbClusterMember>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RdsEvent {
    pub source_identifier: Option<String>,
    pub source_type: Option<String>,
    pub message: Option<String>,
    pub date: Option<String>,
    pub categories: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CallerIdentity {
    pub account: Option<String>,
    pub arn: Option<String>,
    pub user_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogGroup {
    pub name: Option<String>,
    pub stored_bytes: Option<i64>,
    pub retention_days: Option<i32>,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogEvent {
    pub timestamp: Option<String>,
    pub stream: Option<String>,
    pub message: Option<String>,
}

/// Matching events; `truncated` means the search stopped before covering
/// the whole window (`max` reached, or the page budget spent on a large
/// group), so the caller should narrow the pattern or the window rather
/// than treat the list as complete.
#[derive(Debug, Clone, Serialize)]
pub struct LogEvents {
    pub events: Vec<LogEvent>,
    pub truncated: bool,
}

/// What `filter_log_events` searches: a window ending now, optionally one
/// stream prefix and a CloudWatch filter pattern.
#[derive(Debug)]
pub struct LogQuery {
    pub log_group: String,
    pub pattern: Option<String>,
    pub stream_prefix: Option<String>,
    pub minutes: i32,
    pub max: i32,
}

/// One metric statistic over a window ending now.
#[derive(Debug)]
pub struct MetricQuery {
    pub namespace: String,
    pub metric_name: String,
    pub dimensions: Vec<(String, String)>,
    pub stat: String,
    pub period_secs: i32,
    pub minutes: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricPoint {
    pub timestamp: String,
    pub value: f64,
}

/// Every read-only AWS operation the infra tools need, one method each.
#[async_trait]
pub trait InfraBackend: Send + Sync {
    async fn list_clusters(&self, ctx: &SessionContext)
        -> Result<Vec<EcsClusterSummary>, AwsError>;

    async fn list_services(
        &self,
        ctx: &SessionContext,
        cluster: &str,
        max: i32,
    ) -> Result<Vec<String>, AwsError>;

    async fn describe_services(
        &self,
        ctx: &SessionContext,
        cluster: &str,
        services: &[String],
    ) -> Result<Vec<EcsServiceSummary>, AwsError>;

    async fn list_tasks(
        &self,
        ctx: &SessionContext,
        cluster: &str,
        service: Option<&str>,
        desired_status: Option<&str>,
        max: i32,
    ) -> Result<Vec<String>, AwsError>;

    async fn describe_tasks(
        &self,
        ctx: &SessionContext,
        cluster: &str,
        task_arns: &[String],
    ) -> Result<Vec<EcsTask>, AwsError>;

    async fn describe_task_definition(
        &self,
        ctx: &SessionContext,
        task_definition: &str,
    ) -> Result<EcsTaskDefinition, AwsError>;

    async fn list_container_instances(
        &self,
        ctx: &SessionContext,
        cluster: &str,
    ) -> Result<Vec<String>, AwsError>;

    async fn describe_container_instances(
        &self,
        ctx: &SessionContext,
        cluster: &str,
        arns: &[String],
    ) -> Result<Vec<EcsContainerInstance>, AwsError>;

    async fn list_repositories(
        &self,
        ctx: &SessionContext,
        max: i32,
    ) -> Result<Vec<EcrRepository>, AwsError>;

    async fn describe_images(
        &self,
        ctx: &SessionContext,
        repository: &str,
        filter: ImageFilter,
    ) -> Result<Vec<EcrImage>, AwsError>;

    async fn describe_target_health(
        &self,
        ctx: &SessionContext,
        target_group_arn: &str,
    ) -> Result<Vec<TargetHealth>, AwsError>;

    async fn describe_db_clusters(
        &self,
        ctx: &SessionContext,
        identifier: Option<&str>,
    ) -> Result<Vec<DbCluster>, AwsError>;

    async fn describe_events(
        &self,
        ctx: &SessionContext,
        source_identifier: Option<&str>,
        source_type: Option<&str>,
        duration_minutes: i32,
    ) -> Result<Vec<RdsEvent>, AwsError>;

    async fn get_caller_identity(&self, ctx: &SessionContext) -> Result<CallerIdentity, AwsError>;

    async fn describe_log_groups(
        &self,
        ctx: &SessionContext,
        prefix: Option<&str>,
        max: i32,
    ) -> Result<Vec<LogGroup>, AwsError>;

    async fn filter_log_events(
        &self,
        ctx: &SessionContext,
        query: &LogQuery,
    ) -> Result<LogEvents, AwsError>;

    async fn get_metric_data(
        &self,
        ctx: &SessionContext,
        query: &MetricQuery,
    ) -> Result<Vec<MetricPoint>, AwsError>;
}

/// Production backend: typed SDK clients built per session from the shared
/// assume-role [`SdkConfigFactory`].
pub struct SdkInfraBackend {
    factory: Arc<SdkConfigFactory>,
}

impl SdkInfraBackend {
    pub fn new(factory: Arc<SdkConfigFactory>) -> Self {
        Self { factory }
    }

    async fn ecs(&self, ctx: &SessionContext) -> Result<aws_sdk_ecs::Client, AwsError> {
        Ok(aws_sdk_ecs::Client::new(&self.config(ctx).await?))
    }

    async fn ecr(&self, ctx: &SessionContext) -> Result<aws_sdk_ecr::Client, AwsError> {
        Ok(aws_sdk_ecr::Client::new(&self.config(ctx).await?))
    }

    async fn elb(
        &self,
        ctx: &SessionContext,
    ) -> Result<aws_sdk_elasticloadbalancingv2::Client, AwsError> {
        Ok(aws_sdk_elasticloadbalancingv2::Client::new(
            &self.config(ctx).await?,
        ))
    }

    async fn rds(&self, ctx: &SessionContext) -> Result<aws_sdk_rds::Client, AwsError> {
        Ok(aws_sdk_rds::Client::new(&self.config(ctx).await?))
    }

    async fn sts(&self, ctx: &SessionContext) -> Result<aws_sdk_sts::Client, AwsError> {
        Ok(aws_sdk_sts::Client::new(&self.config(ctx).await?))
    }

    async fn logs(&self, ctx: &SessionContext) -> Result<aws_sdk_cloudwatchlogs::Client, AwsError> {
        Ok(aws_sdk_cloudwatchlogs::Client::new(
            &self.config(ctx).await?,
        ))
    }

    async fn cloudwatch(
        &self,
        ctx: &SessionContext,
    ) -> Result<aws_sdk_cloudwatch::Client, AwsError> {
        Ok(aws_sdk_cloudwatch::Client::new(&self.config(ctx).await?))
    }

    async fn config(&self, ctx: &SessionContext) -> Result<aws_config::SdkConfig, AwsError> {
        self.factory
            .config_for(ctx)
            .await
            .map_err(|e| AwsError::Other(e.to_string()))
    }
}

#[async_trait]
impl InfraBackend for SdkInfraBackend {
    async fn list_clusters(
        &self,
        ctx: &SessionContext,
    ) -> Result<Vec<EcsClusterSummary>, AwsError> {
        let ecs = self.ecs(ctx).await?;
        let arns = ecs
            .list_clusters()
            .send()
            .await
            .map_err(map_sdk_err)?
            .cluster_arns()
            .to_vec();
        // DescribeClusters with an empty set assumes the *default* cluster, so
        // an account with no clusters must short-circuit, not describe nothing.
        if arns.is_empty() {
            return Ok(Vec::new());
        }
        let out = ecs
            .describe_clusters()
            .set_clusters(Some(arns))
            .send()
            .await
            .map_err(map_sdk_err)?;
        Ok(out.clusters().iter().map(map_cluster).collect())
    }

    async fn list_services(
        &self,
        ctx: &SessionContext,
        cluster: &str,
        max: i32,
    ) -> Result<Vec<String>, AwsError> {
        let out = self
            .ecs(ctx)
            .await?
            .list_services()
            .cluster(cluster)
            .max_results(max)
            .send()
            .await
            .map_err(map_sdk_err)?;
        Ok(out.service_arns().to_vec())
    }

    async fn describe_services(
        &self,
        ctx: &SessionContext,
        cluster: &str,
        services: &[String],
    ) -> Result<Vec<EcsServiceSummary>, AwsError> {
        let out = self
            .ecs(ctx)
            .await?
            .describe_services()
            .cluster(cluster)
            .set_services(Some(services.to_vec()))
            .send()
            .await
            .map_err(map_sdk_err)?;
        Ok(out.services().iter().map(map_service).collect())
    }

    async fn list_tasks(
        &self,
        ctx: &SessionContext,
        cluster: &str,
        service: Option<&str>,
        desired_status: Option<&str>,
        max: i32,
    ) -> Result<Vec<String>, AwsError> {
        let mut req = self
            .ecs(ctx)
            .await?
            .list_tasks()
            .cluster(cluster)
            .max_results(max);
        if let Some(s) = service {
            req = req.service_name(s);
        }
        if let Some(status) = desired_status.and_then(parse_desired_status) {
            req = req.desired_status(status);
        }
        let out = req.send().await.map_err(map_sdk_err)?;
        Ok(out.task_arns().to_vec())
    }

    async fn describe_tasks(
        &self,
        ctx: &SessionContext,
        cluster: &str,
        task_arns: &[String],
    ) -> Result<Vec<EcsTask>, AwsError> {
        if task_arns.is_empty() {
            return Ok(Vec::new());
        }
        let out = self
            .ecs(ctx)
            .await?
            .describe_tasks()
            .cluster(cluster)
            .set_tasks(Some(task_arns.to_vec()))
            .send()
            .await
            .map_err(map_sdk_err)?;
        Ok(out.tasks().iter().map(map_task).collect())
    }

    async fn describe_task_definition(
        &self,
        ctx: &SessionContext,
        task_definition: &str,
    ) -> Result<EcsTaskDefinition, AwsError> {
        let out = self
            .ecs(ctx)
            .await?
            .describe_task_definition()
            .task_definition(task_definition)
            .send()
            .await
            .map_err(map_sdk_err)?;
        out.task_definition()
            .map(map_task_definition)
            .ok_or(AwsError::NotFound)
    }

    async fn list_container_instances(
        &self,
        ctx: &SessionContext,
        cluster: &str,
    ) -> Result<Vec<String>, AwsError> {
        let out = self
            .ecs(ctx)
            .await?
            .list_container_instances()
            .cluster(cluster)
            .send()
            .await
            .map_err(map_sdk_err)?;
        Ok(out.container_instance_arns().to_vec())
    }

    async fn describe_container_instances(
        &self,
        ctx: &SessionContext,
        cluster: &str,
        arns: &[String],
    ) -> Result<Vec<EcsContainerInstance>, AwsError> {
        if arns.is_empty() {
            return Ok(Vec::new());
        }
        let out = self
            .ecs(ctx)
            .await?
            .describe_container_instances()
            .cluster(cluster)
            .set_container_instances(Some(arns.to_vec()))
            .send()
            .await
            .map_err(map_sdk_err)?;
        Ok(out
            .container_instances()
            .iter()
            .map(map_container_instance)
            .collect())
    }

    async fn list_repositories(
        &self,
        ctx: &SessionContext,
        max: i32,
    ) -> Result<Vec<EcrRepository>, AwsError> {
        let out = self
            .ecr(ctx)
            .await?
            .describe_repositories()
            .max_results(max)
            .send()
            .await
            .map_err(map_sdk_err)?;
        Ok(out.repositories().iter().map(map_repository).collect())
    }

    async fn describe_images(
        &self,
        ctx: &SessionContext,
        repository: &str,
        filter: ImageFilter,
    ) -> Result<Vec<EcrImage>, AwsError> {
        let mut req = self
            .ecr(ctx)
            .await?
            .describe_images()
            .repository_name(repository);
        match &filter {
            ImageFilter::Recent { max } => req = req.max_results(*max),
            ImageFilter::Tag(tag) => {
                req = req.image_ids(
                    aws_sdk_ecr::types::ImageIdentifier::builder()
                        .image_tag(tag)
                        .build(),
                )
            }
            ImageFilter::Digest(digest) => {
                req = req.image_ids(
                    aws_sdk_ecr::types::ImageIdentifier::builder()
                        .image_digest(digest)
                        .build(),
                )
            }
        }
        let out = req.send().await.map_err(map_sdk_err)?;
        let mut images: Vec<EcrImage> = out.image_details().iter().map(map_image).collect();
        images.sort_by(|a, b| b.pushed_at.cmp(&a.pushed_at));
        Ok(images)
    }

    async fn describe_target_health(
        &self,
        ctx: &SessionContext,
        target_group_arn: &str,
    ) -> Result<Vec<TargetHealth>, AwsError> {
        let out = self
            .elb(ctx)
            .await?
            .describe_target_health()
            .target_group_arn(target_group_arn)
            .send()
            .await
            .map_err(map_sdk_err)?;
        Ok(out
            .target_health_descriptions()
            .iter()
            .map(map_target_health)
            .collect())
    }

    async fn describe_db_clusters(
        &self,
        ctx: &SessionContext,
        identifier: Option<&str>,
    ) -> Result<Vec<DbCluster>, AwsError> {
        let mut req = self.rds(ctx).await?.describe_db_clusters();
        if let Some(id) = identifier {
            req = req.db_cluster_identifier(id);
        }
        let out = req.send().await.map_err(map_sdk_err)?;
        Ok(out.db_clusters().iter().map(map_db_cluster).collect())
    }

    async fn describe_events(
        &self,
        ctx: &SessionContext,
        source_identifier: Option<&str>,
        source_type: Option<&str>,
        duration_minutes: i32,
    ) -> Result<Vec<RdsEvent>, AwsError> {
        let mut req = self
            .rds(ctx)
            .await?
            .describe_events()
            .duration(duration_minutes);
        if let Some(id) = source_identifier {
            req = req.source_identifier(id);
        }
        if let Some(t) = source_type {
            req = req.source_type(aws_sdk_rds::types::SourceType::from(t));
        }
        let out = req.send().await.map_err(map_sdk_err)?;
        Ok(out.events().iter().map(map_rds_event).collect())
    }

    async fn get_caller_identity(&self, ctx: &SessionContext) -> Result<CallerIdentity, AwsError> {
        let out = self
            .sts(ctx)
            .await?
            .get_caller_identity()
            .send()
            .await
            .map_err(map_sdk_err)?;
        Ok(CallerIdentity {
            account: out.account().map(str::to_string),
            arn: out.arn().map(str::to_string),
            user_id: out.user_id().map(str::to_string),
        })
    }

    async fn describe_log_groups(
        &self,
        ctx: &SessionContext,
        prefix: Option<&str>,
        max: i32,
    ) -> Result<Vec<LogGroup>, AwsError> {
        let mut req = self.logs(ctx).await?.describe_log_groups().limit(max);
        if let Some(prefix) = prefix {
            req = req.log_group_name_prefix(prefix);
        }
        let out = req.send().await.map_err(map_sdk_err)?;
        Ok(out
            .log_groups()
            .iter()
            .map(|g| LogGroup {
                name: g.log_group_name().map(str::to_string),
                stored_bytes: g.stored_bytes(),
                retention_days: g.retention_in_days(),
                created_at: g.creation_time().map(millis_to_string),
            })
            .collect())
    }

    async fn filter_log_events(
        &self,
        ctx: &SessionContext,
        query: &LogQuery,
    ) -> Result<LogEvents, AwsError> {
        let (start, end) = window_millis(query.minutes);
        let client = self.logs(ctx).await?;
        let mut events = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..MAX_LOG_PAGES {
            let req = client
                .filter_log_events()
                .log_group_name(&query.log_group)
                .start_time(start)
                .end_time(end)
                .limit(query.max - events.len() as i32)
                .set_next_token(token.take())
                .set_filter_pattern(query.pattern.clone())
                .set_log_stream_name_prefix(query.stream_prefix.clone());
            let out = req.send().await.map_err(map_sdk_err)?;
            events.extend(out.events().iter().map(|e| LogEvent {
                timestamp: e.timestamp().map(millis_to_string),
                stream: e.log_stream_name().map(str::to_string),
                message: e.message().map(str::to_string),
            }));
            token = out.next_token().map(str::to_string);
            if token.is_none() || events.len() as i32 >= query.max {
                break;
            }
        }
        Ok(LogEvents {
            events,
            truncated: token.is_some(),
        })
    }

    async fn get_metric_data(
        &self,
        ctx: &SessionContext,
        query: &MetricQuery,
    ) -> Result<Vec<MetricPoint>, AwsError> {
        use aws_sdk_cloudwatch::primitives::DateTime;
        use aws_sdk_cloudwatch::types::{Dimension, Metric, MetricDataQuery, MetricStat, ScanBy};

        let (start, end) = window_millis(query.minutes);
        let metric = Metric::builder()
            .namespace(&query.namespace)
            .metric_name(&query.metric_name)
            .set_dimensions(Some(
                query
                    .dimensions
                    .iter()
                    .map(|(name, value)| Dimension::builder().name(name).value(value).build())
                    .collect(),
            ))
            .build();
        let stat = MetricStat::builder()
            .metric(metric)
            .period(query.period_secs)
            .stat(&query.stat)
            .build();
        let data_query = MetricDataQuery::builder()
            .id("m")
            .metric_stat(stat)
            .return_data(true)
            .build();
        let out = self
            .cloudwatch(ctx)
            .await?
            .get_metric_data()
            .metric_data_queries(data_query)
            .start_time(DateTime::from_millis(start))
            .end_time(DateTime::from_millis(end))
            .scan_by(ScanBy::TimestampAscending)
            .send()
            .await
            .map_err(map_sdk_err)?;
        Ok(out
            .metric_data_results()
            .iter()
            .flat_map(|r| r.timestamps().iter().zip(r.values()))
            .map(|(t, v)| MetricPoint {
                timestamp: t.to_string(),
                value: *v,
            })
            .collect())
    }
}

/// Pages a log search may fetch before reporting `truncated`: CloudWatch
/// returns partially filled or empty pages on a large group, so one page
/// alone would read as "no matches".
const MAX_LOG_PAGES: usize = 5;

/// Epoch milliseconds for a window of `minutes` ending now.
fn window_millis(minutes: i32) -> (i64, i64) {
    let end = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    (end - i64::from(minutes) * 60_000, end)
}

fn millis_to_string(millis: i64) -> String {
    aws_sdk_cloudwatchlogs::primitives::DateTime::from_millis(millis).to_string()
}

fn parse_desired_status(status: &str) -> Option<aws_sdk_ecs::types::DesiredStatus> {
    match status.to_ascii_uppercase().as_str() {
        "RUNNING" => Some(aws_sdk_ecs::types::DesiredStatus::Running),
        "STOPPED" => Some(aws_sdk_ecs::types::DesiredStatus::Stopped),
        "PENDING" => Some(aws_sdk_ecs::types::DesiredStatus::Pending),
        _ => None,
    }
}

fn map_cluster(c: &aws_sdk_ecs::types::Cluster) -> EcsClusterSummary {
    EcsClusterSummary {
        name: c.cluster_name().map(str::to_string),
        arn: c.cluster_arn().map(str::to_string),
        status: c.status().map(str::to_string),
        registered_container_instances_count: c.registered_container_instances_count(),
        running_tasks_count: c.running_tasks_count(),
        pending_tasks_count: c.pending_tasks_count(),
        active_services_count: c.active_services_count(),
    }
}

fn map_service(s: &aws_sdk_ecs::types::Service) -> EcsServiceSummary {
    EcsServiceSummary {
        service_name: s.service_name().map(str::to_string),
        status: s.status().map(str::to_string),
        desired_count: s.desired_count(),
        running_count: s.running_count(),
        pending_count: s.pending_count(),
        task_definition: s.task_definition().map(str::to_string),
        deployments: s.deployments().iter().map(map_deployment).collect(),
        events: s
            .events()
            .iter()
            .take(10)
            .map(|e| EcsServiceEvent {
                created_at: e.created_at().map(ToString::to_string),
                message: e.message().map(str::to_string),
            })
            .collect(),
    }
}

fn map_deployment(d: &aws_sdk_ecs::types::Deployment) -> EcsDeployment {
    EcsDeployment {
        id: d.id().map(str::to_string),
        status: d.status().map(str::to_string),
        task_definition: d.task_definition().map(str::to_string),
        desired_count: d.desired_count(),
        running_count: d.running_count(),
        pending_count: d.pending_count(),
        rollout_state: d.rollout_state().map(|s| s.as_str().to_string()),
        rollout_state_reason: d.rollout_state_reason().map(str::to_string),
        created_at: d.created_at().map(ToString::to_string),
        updated_at: d.updated_at().map(ToString::to_string),
    }
}

fn map_task(t: &aws_sdk_ecs::types::Task) -> EcsTask {
    EcsTask {
        task_arn: t.task_arn().map(str::to_string),
        task_definition_arn: t.task_definition_arn().map(str::to_string),
        last_status: t.last_status().map(str::to_string),
        desired_status: t.desired_status().map(str::to_string),
        health_status: t.health_status().map(|h| h.as_str().to_string()),
        stop_code: t.stop_code().map(|c| c.as_str().to_string()),
        stopped_reason: t.stopped_reason().map(str::to_string),
        started_at: t.started_at().map(ToString::to_string),
        stopped_at: t.stopped_at().map(ToString::to_string),
        containers: t
            .containers()
            .iter()
            .map(|c| EcsContainer {
                name: c.name().map(str::to_string),
                last_status: c.last_status().map(str::to_string),
                exit_code: c.exit_code(),
                reason: c.reason().map(str::to_string),
                image: c.image().map(str::to_string),
                image_digest: c.image_digest().map(str::to_string),
                health_status: c.health_status().map(|h| h.as_str().to_string()),
            })
            .collect(),
    }
}

fn map_task_definition(t: &aws_sdk_ecs::types::TaskDefinition) -> EcsTaskDefinition {
    EcsTaskDefinition {
        family: t.family().map(str::to_string),
        revision: t.revision(),
        task_definition_arn: t.task_definition_arn().map(str::to_string),
        cpu: t.cpu().map(str::to_string),
        memory: t.memory().map(str::to_string),
        containers: t
            .container_definitions()
            .iter()
            .map(|c| EcsContainerDef {
                name: c.name().map(str::to_string),
                image: c.image().map(str::to_string),
                cpu: Some(c.cpu()),
                memory: c.memory(),
                essential: c.essential(),
                environment_keys: c
                    .environment()
                    .iter()
                    .filter_map(|kv| kv.name().map(str::to_string))
                    .collect(),
                secret_keys: c.secrets().iter().map(|s| s.name().to_string()).collect(),
            })
            .collect(),
    }
}

fn map_container_instance(c: &aws_sdk_ecs::types::ContainerInstance) -> EcsContainerInstance {
    let find_int = |resources: &[aws_sdk_ecs::types::Resource], name: &str| -> Option<i32> {
        resources
            .iter()
            .find(|r| r.name() == Some(name))
            .map(|r| r.integer_value())
    };
    EcsContainerInstance {
        container_instance_arn: c.container_instance_arn().map(str::to_string),
        ec2_instance_id: c.ec2_instance_id().map(str::to_string),
        status: c.status().map(str::to_string),
        agent_connected: c.agent_connected(),
        running_tasks_count: c.running_tasks_count(),
        pending_tasks_count: c.pending_tasks_count(),
        registered_cpu: find_int(c.registered_resources(), "CPU"),
        remaining_cpu: find_int(c.remaining_resources(), "CPU"),
        registered_memory: find_int(c.registered_resources(), "MEMORY"),
        remaining_memory: find_int(c.remaining_resources(), "MEMORY"),
    }
}

fn map_repository(r: &aws_sdk_ecr::types::Repository) -> EcrRepository {
    EcrRepository {
        name: r.repository_name().map(str::to_string),
        uri: r.repository_uri().map(str::to_string),
        arn: r.repository_arn().map(str::to_string),
        created_at: r.created_at().map(ToString::to_string),
    }
}

fn map_image(i: &aws_sdk_ecr::types::ImageDetail) -> EcrImage {
    EcrImage {
        image_digest: i.image_digest().map(str::to_string),
        image_tags: i.image_tags().to_vec(),
        pushed_at: i.image_pushed_at().map(ToString::to_string),
        size_bytes: i.image_size_in_bytes(),
    }
}

fn map_target_health(
    d: &aws_sdk_elasticloadbalancingv2::types::TargetHealthDescription,
) -> TargetHealth {
    TargetHealth {
        target_id: d.target().and_then(|t| t.id()).map(str::to_string),
        port: d.target().and_then(|t| t.port()),
        state: d
            .target_health()
            .and_then(|h| h.state())
            .map(|s| s.as_str().to_string()),
        reason: d
            .target_health()
            .and_then(|h| h.reason())
            .map(|r| r.as_str().to_string()),
        description: d
            .target_health()
            .and_then(|h| h.description())
            .map(str::to_string),
    }
}

fn map_db_cluster(c: &aws_sdk_rds::types::DbCluster) -> DbCluster {
    DbCluster {
        identifier: c.db_cluster_identifier().map(str::to_string),
        status: c.status().map(str::to_string),
        engine: c.engine().map(str::to_string),
        engine_version: c.engine_version().map(str::to_string),
        endpoint: c.endpoint().map(str::to_string),
        reader_endpoint: c.reader_endpoint().map(str::to_string),
        multi_az: c.multi_az().unwrap_or(false),
        members: c
            .db_cluster_members()
            .iter()
            .map(|m| DbClusterMember {
                instance_id: m.db_instance_identifier().map(str::to_string),
                is_writer: m.is_cluster_writer().unwrap_or(false),
            })
            .collect(),
    }
}

fn map_rds_event(e: &aws_sdk_rds::types::Event) -> RdsEvent {
    RdsEvent {
        source_identifier: e.source_identifier().map(str::to_string),
        source_type: e.source_type().map(|t| t.as_str().to_string()),
        message: e.message().map(str::to_string),
        date: e.date().map(ToString::to_string),
        categories: e.event_categories().to_vec(),
    }
}

/// Map any service SDK error onto [`AwsError`]; falls back to the error's
/// display text when no canonical service code is present (timeouts, DNS).
fn map_sdk_err<E, R>(err: aws_sdk_ecs::error::SdkError<E, R>) -> AwsError
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
    R: Send + Sync + 'static,
{
    if let Some(code) = err.as_service_error().and_then(ProvideErrorMetadata::code) {
        return classify_code(code);
    }
    classify_message(&err.to_string())
}

/// Map a canonical control-plane exception code to [`AwsError`]. Unknown
/// codes surface as `Other` carrying the raw code.
pub(crate) fn classify_code(code: &str) -> AwsError {
    match code {
        "ResourceNotFoundException"
        | "ClusterNotFoundException"
        | "ServiceNotFoundException"
        | "RepositoryNotFoundException"
        | "ImageNotFoundException"
        | "TaskDefinitionNotFoundException"
        | "TargetGroupNotFound"
        | "LoadBalancerNotFound"
        | "DBClusterNotFoundFault"
        | "DBInstanceNotFound" => AwsError::NotFound,
        "AccessDenied"
        | "AccessDeniedException"
        | "UnauthorizedException"
        | "UnauthorizedOperation" => AwsError::AccessDenied,
        "ThrottlingException"
        | "Throttling"
        | "RequestLimitExceeded"
        | "TooManyRequestsException"
        | "RequestThrottled" => AwsError::Throttled,
        other => AwsError::Other(format!("AWS service error: {other}")),
    }
}

/// Fallback classifier for non-service errors. Anchored on canonical code
/// spellings; bare status digits are intentionally not matched.
pub(crate) fn classify_message(msg: &str) -> AwsError {
    let lower = msg.to_lowercase();
    if lower.contains("notfound") || lower.contains("not found") {
        AwsError::NotFound
    } else if lower.contains("accessdenied")
        || lower.contains("access denied")
        || lower.contains("unauthorized")
    {
        AwsError::AccessDenied
    } else if lower.contains("throttl") || lower.contains("too many requests") {
        AwsError::Throttled
    } else {
        AwsError::Other(msg.to_string())
    }
}

#[cfg(test)]
pub(crate) mod mock {
    use super::*;
    use parking_lot::Mutex;

    /// Hand-written backend for tool + orchestration tests. Each operation
    /// returns its configured response; unset operations return
    /// `Other("not stubbed: …")` so a test that exercises an unexpected path
    /// fails loudly rather than silently. `describe_images` filters the
    /// canned `ecr_images` by tag/digest, mirroring the real service so the
    /// `deploy.status` orchestration is tested end-to-end.
    #[derive(Default)]
    pub struct MockInfraBackend {
        pub clusters: Mutex<Option<Result<Vec<EcsClusterSummary>, AwsError>>>,
        pub service_arns: Mutex<Option<Result<Vec<String>, AwsError>>>,
        pub repositories: Mutex<Option<Result<Vec<EcrRepository>, AwsError>>>,
        pub services: Mutex<Option<Result<Vec<EcsServiceSummary>, AwsError>>>,
        pub task_arns: Mutex<Option<Result<Vec<String>, AwsError>>>,
        pub tasks: Mutex<Option<Result<Vec<EcsTask>, AwsError>>>,
        pub task_definition: Mutex<Option<Result<EcsTaskDefinition, AwsError>>>,
        pub container_instance_arns: Mutex<Option<Result<Vec<String>, AwsError>>>,
        pub container_instances: Mutex<Option<Result<Vec<EcsContainerInstance>, AwsError>>>,
        pub ecr_images: Mutex<Vec<EcrImage>>,
        pub target_health: Mutex<Option<Result<Vec<TargetHealth>, AwsError>>>,
        pub db_clusters: Mutex<Option<Result<Vec<DbCluster>, AwsError>>>,
        pub events: Mutex<Option<Result<Vec<RdsEvent>, AwsError>>>,
        pub caller_identity: Mutex<Option<Result<CallerIdentity, AwsError>>>,
        pub log_groups: Mutex<Option<Result<Vec<LogGroup>, AwsError>>>,
        pub log_events: Mutex<Option<Result<LogEvents, AwsError>>>,
        pub metric_points: Mutex<Option<Result<Vec<MetricPoint>, AwsError>>>,
    }

    fn taken<T: Clone>(slot: &Mutex<Option<Result<T, AwsError>>>, op: &str) -> Result<T, AwsError> {
        slot.lock()
            .clone()
            .unwrap_or_else(|| Err(AwsError::Other(format!("not stubbed: {op}"))))
    }

    #[async_trait]
    impl InfraBackend for MockInfraBackend {
        async fn list_clusters(
            &self,
            _ctx: &SessionContext,
        ) -> Result<Vec<EcsClusterSummary>, AwsError> {
            taken(&self.clusters, "list_clusters")
        }

        async fn list_services(
            &self,
            _ctx: &SessionContext,
            _cluster: &str,
            _max: i32,
        ) -> Result<Vec<String>, AwsError> {
            taken(&self.service_arns, "list_services")
        }

        async fn describe_services(
            &self,
            _ctx: &SessionContext,
            _cluster: &str,
            _services: &[String],
        ) -> Result<Vec<EcsServiceSummary>, AwsError> {
            taken(&self.services, "describe_services")
        }

        async fn list_tasks(
            &self,
            _ctx: &SessionContext,
            _cluster: &str,
            _service: Option<&str>,
            _desired_status: Option<&str>,
            _max: i32,
        ) -> Result<Vec<String>, AwsError> {
            taken(&self.task_arns, "list_tasks")
        }

        async fn describe_tasks(
            &self,
            _ctx: &SessionContext,
            _cluster: &str,
            _task_arns: &[String],
        ) -> Result<Vec<EcsTask>, AwsError> {
            taken(&self.tasks, "describe_tasks")
        }

        async fn describe_task_definition(
            &self,
            _ctx: &SessionContext,
            _task_definition: &str,
        ) -> Result<EcsTaskDefinition, AwsError> {
            taken(&self.task_definition, "describe_task_definition")
        }

        async fn list_container_instances(
            &self,
            _ctx: &SessionContext,
            _cluster: &str,
        ) -> Result<Vec<String>, AwsError> {
            taken(&self.container_instance_arns, "list_container_instances")
        }

        async fn describe_container_instances(
            &self,
            _ctx: &SessionContext,
            _cluster: &str,
            _arns: &[String],
        ) -> Result<Vec<EcsContainerInstance>, AwsError> {
            taken(&self.container_instances, "describe_container_instances")
        }

        async fn list_repositories(
            &self,
            _ctx: &SessionContext,
            _max: i32,
        ) -> Result<Vec<EcrRepository>, AwsError> {
            taken(&self.repositories, "list_repositories")
        }

        async fn describe_images(
            &self,
            _ctx: &SessionContext,
            _repository: &str,
            filter: ImageFilter,
        ) -> Result<Vec<EcrImage>, AwsError> {
            let images = self.ecr_images.lock().clone();
            Ok(match filter {
                ImageFilter::Recent { .. } => images,
                ImageFilter::Tag(tag) => images
                    .into_iter()
                    .filter(|i| i.image_tags.contains(&tag))
                    .collect(),
                ImageFilter::Digest(digest) => images
                    .into_iter()
                    .filter(|i| i.image_digest.as_deref() == Some(digest.as_str()))
                    .collect(),
            })
        }

        async fn describe_target_health(
            &self,
            _ctx: &SessionContext,
            _target_group_arn: &str,
        ) -> Result<Vec<TargetHealth>, AwsError> {
            taken(&self.target_health, "describe_target_health")
        }

        async fn describe_db_clusters(
            &self,
            _ctx: &SessionContext,
            _identifier: Option<&str>,
        ) -> Result<Vec<DbCluster>, AwsError> {
            taken(&self.db_clusters, "describe_db_clusters")
        }

        async fn describe_events(
            &self,
            _ctx: &SessionContext,
            _source_identifier: Option<&str>,
            _source_type: Option<&str>,
            _duration_minutes: i32,
        ) -> Result<Vec<RdsEvent>, AwsError> {
            taken(&self.events, "describe_events")
        }

        async fn get_caller_identity(
            &self,
            _ctx: &SessionContext,
        ) -> Result<CallerIdentity, AwsError> {
            taken(&self.caller_identity, "get_caller_identity")
        }

        async fn describe_log_groups(
            &self,
            _ctx: &SessionContext,
            _prefix: Option<&str>,
            _max: i32,
        ) -> Result<Vec<LogGroup>, AwsError> {
            taken(&self.log_groups, "describe_log_groups")
        }

        async fn filter_log_events(
            &self,
            _ctx: &SessionContext,
            _query: &LogQuery,
        ) -> Result<LogEvents, AwsError> {
            taken(&self.log_events, "filter_log_events")
        }

        async fn get_metric_data(
            &self,
            _ctx: &SessionContext,
            _query: &MetricQuery,
        ) -> Result<Vec<MetricPoint>, AwsError> {
            taken(&self.metric_points, "get_metric_data")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_code_maps_not_found_family() {
        for code in [
            "ServiceNotFoundException",
            "ClusterNotFoundException",
            "RepositoryNotFoundException",
            "DBClusterNotFoundFault",
        ] {
            assert!(matches!(classify_code(code), AwsError::NotFound), "{code}");
        }
    }

    #[test]
    fn classify_code_maps_access_and_throttle() {
        assert!(matches!(
            classify_code("AccessDeniedException"),
            AwsError::AccessDenied
        ));
        assert!(matches!(
            classify_code("ThrottlingException"),
            AwsError::Throttled
        ));
    }

    #[test]
    fn classify_code_unknown_falls_through_with_raw_code() {
        match classify_code("InternalServerError") {
            AwsError::Other(m) => assert!(m.contains("InternalServerError"), "{m}"),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn classify_message_does_not_match_naked_digits() {
        for s in ["timeout after 503 ms", "connection reset (429)"] {
            assert!(
                matches!(classify_message(s), AwsError::Other(_)),
                "naked digits in '{s}' must not classify",
            );
        }
    }

    #[test]
    fn classify_message_recognises_canonical_text() {
        assert!(matches!(
            classify_message("ResourceNotFoundException: nope"),
            AwsError::NotFound
        ));
        assert!(matches!(
            classify_message("AccessDenied: nope"),
            AwsError::AccessDenied
        ));
        assert!(matches!(
            classify_message("Rate exceeded (throttling)"),
            AwsError::Throttled
        ));
    }
}
