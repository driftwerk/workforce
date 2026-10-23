use async_nats::jetstream::stream::{Config as StreamConfig, RetentionPolicy};
use async_nats::jetstream::Context;
use eyre::Result;
use std::time::Duration;
use tracing::info;

const AGENTS_SUBJECTS: &str = "workforce.agents.>";
const TEAMS_SUBJECTS: &str = "workforce.teams.>";
const SYSTEM_SUBJECTS: &str = "workforce.system.>";
const EVENTS_SUBJECTS: &str = "workforce.events.>";
const TOOLS_SUBJECTS: &str = "workforce.tools.>";
const JOBS_SUBJECTS: &str = "workforce.jobs.>";

/// Every subject filter a workforce stream captures. A subject under one of
/// these is acknowledged by JetStream, so request/reply subjects must avoid
/// them all.
pub const STREAMED_SUBJECTS: [&str; 6] = [
    AGENTS_SUBJECTS,
    TEAMS_SUBJECTS,
    SYSTEM_SUBJECTS,
    EVENTS_SUBJECTS,
    TOOLS_SUBJECTS,
    JOBS_SUBJECTS,
];

/// Manages JetStream streams for the workforce system.
///
/// Streams provide durable message storage so that messages survive
/// restarts and can be replayed if consumers come online later.
pub struct StreamManager {
    context: Context,
    /// JetStream replica count for every workforce stream. 1 on a single
    /// NATS server (which rejects higher values); 3 on a 3-node cluster so
    /// streams survive a node loss.
    replicas: usize,
}

impl StreamManager {
    pub fn new(context: Context, replicas: usize) -> Self {
        Self { context, replicas }
    }

    /// Ensure all required workforce streams exist.
    ///
    /// Creates them if they don't exist, updates config if they do.
    pub async fn ensure_all_streams(&self) -> Result<()> {
        self.ensure_agents_stream().await?;
        self.ensure_teams_stream().await?;
        self.ensure_system_stream().await?;
        self.ensure_events_stream().await?;
        self.ensure_tools_stream().await?;
        self.ensure_jobs_stream().await?;
        info!("All JetStream streams ready");
        Ok(())
    }

    /// Stream for agent messages (inbox, status, tool invocations).
    async fn ensure_agents_stream(&self) -> Result<()> {
        self.ensure_stream(
            "WORKFORCE_AGENTS",
            &[AGENTS_SUBJECTS],
            "Agent messages — inbox, status, tool invocations",
            Duration::from_secs(7 * 24 * 3600), // 7 days
        )
        .await
    }

    /// Stream for team messages (channels, tasks, events).
    async fn ensure_teams_stream(&self) -> Result<()> {
        self.ensure_stream(
            "WORKFORCE_TEAMS",
            &[TEAMS_SUBJECTS],
            "Team messages — channels, tasks, events",
            Duration::from_secs(7 * 24 * 3600), // 7 days
        )
        .await
    }

    /// Stream for system messages (policy, orchestrator, nodes).
    async fn ensure_system_stream(&self) -> Result<()> {
        self.ensure_stream(
            "WORKFORCE_SYSTEM",
            &[SYSTEM_SUBJECTS],
            "System messages — policy, orchestrator, node health",
            Duration::from_secs(3 * 24 * 3600), // 3 days
        )
        .await
    }

    /// Stream for inbound events from event sources.
    async fn ensure_events_stream(&self) -> Result<()> {
        self.ensure_stream(
            "WORKFORCE_EVENTS",
            &[EVENTS_SUBJECTS],
            "Inbound events from event sources",
            Duration::from_secs(3 * 24 * 3600), // 3 days
        )
        .await
    }

    /// Stream for plugin tool invocations.
    async fn ensure_tools_stream(&self) -> Result<()> {
        self.ensure_stream(
            "WORKFORCE_TOOLS",
            &[TOOLS_SUBJECTS],
            "Plugin tool invocations",
            Duration::from_secs(3 * 24 * 3600), // 3 days
        )
        .await
    }

    /// Stream for async job notifications.
    ///
    /// Job completion notifications are published under workforce.jobs.{agent_id}.>
    /// and consumed by durable pull consumers for reliable delivery.
    async fn ensure_jobs_stream(&self) -> Result<()> {
        let config = StreamConfig {
            name: "WORKFORCE_JOBS".to_string(),
            subjects: vec![JOBS_SUBJECTS.to_string()],
            description: Some("Async job notifications — completion, progress".to_string()),
            retention: RetentionPolicy::WorkQueue,
            max_messages: 10_000,
            max_bytes: 64 * 1024 * 1024,             // 64MB
            max_age: Duration::from_secs(24 * 3600), // 24h
            num_replicas: self.replicas,
            ..Default::default()
        };

        self.ensure(config).await
    }

    /// Create or update a single stream.
    async fn ensure_stream(
        &self,
        name: &str,
        subjects: &[&str],
        description: &str,
        max_age: Duration,
    ) -> Result<()> {
        let config = StreamConfig {
            name: name.to_string(),
            subjects: subjects.iter().map(|s| s.to_string()).collect(),
            description: Some(description.to_string()),
            retention: RetentionPolicy::Limits,
            max_messages: 100_000,
            max_bytes: 512 * 1024 * 1024, // 512MB
            max_age,
            num_replicas: self.replicas,
            ..Default::default()
        };

        self.ensure(config).await
    }

    /// Get-or-create the stream, then reconcile the replica count.
    /// `get_or_create_stream` silently ignores the passed config when the
    /// stream already exists, so a replica bump (1 → 3 during a NATS-cluster
    /// migration) would otherwise never apply.
    async fn ensure(&self, config: StreamConfig) -> Result<()> {
        let name = config.name.clone();
        let stream = self
            .context
            .get_or_create_stream(config.clone())
            .await
            .map_err(|e| eyre::eyre!("Failed to ensure stream '{}': {}", name, e))?;

        let current = stream.cached_info().config.num_replicas;
        if current != self.replicas {
            self.context.update_stream(&config).await.map_err(|e| {
                eyre::eyre!(
                    "Failed to update stream '{}' from {} to {} replicas: {}",
                    name,
                    current,
                    self.replicas,
                    e
                )
            })?;
            info!(
                stream = %name,
                from = current,
                to = self.replicas,
                "Stream replicas updated"
            );
        }

        info!(stream = %name, "Stream ready");
        Ok(())
    }
}
