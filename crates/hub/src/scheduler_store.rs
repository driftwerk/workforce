//! Persistence backend for the scheduler's job list.
//!
//! The file backend keeps today's per-agent `{workspace}/scheduler/jobs.json`
//! (whole-list atomic rewrite). The Postgres backend stores one row per job
//! and applies only the change each mutation actually made — so two hub
//! instances editing different jobs can never clobber each other, which the
//! whole-file model cannot avoid. Backend selection is `[scheduler_store]
//! backend = "file" (default) | "postgres"` — explicit opt-in, because the
//! Postgres path needs a `migrate-scheduler` backfill first.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use eyre::{bail, Context, Result};
use workforce_common::settings::WorkforceConfig;

use crate::scheduler::ScheduledJob;

#[cfg(feature = "postgres")]
pub mod postgres;

/// What a persist call actually changed. The file backend ignores this and
/// rewrites the full list; the Postgres backend applies exactly this change.
pub enum JobChange<'a> {
    Upserted(&'a ScheduledJob),
    /// The job's definition (name, description, schedule, payload,
    /// identities) refreshed from a snapshot that may be stale on the run
    /// bookkeeping. A shared store keeps the row's own `next_run`,
    /// `last_run`, `run_count`, `delivery_failures` and `enabled`: those
    /// belong to the leader's tick, and writing a follower's boot-time copy
    /// over them re-arms an occurrence the leader has already fired.
    Refreshed(&'a ScheduledJob),
    /// Only the named top-level fields of the job changed. A shared store
    /// writes those and keeps the rest of the row, so a write made from a
    /// snapshot a peer has since moved on from (an edit on a follower, the
    /// leader's bookkeeping after a long run) cannot take the peer's change
    /// back.
    Fields(&'a ScheduledJob, &'a [&'static str]),
    /// As [`Self::Fields`], written only while the stored row's flag (the
    /// last element) is still true: a suspension's hold must not switch off
    /// a job a peer switched off since, nor its release switch on one a
    /// person paused since.
    FieldsWhile(&'a ScheduledJob, &'a [&'static str], &'static str),
    Removed(&'a [String]),
    /// A bulk rewrite (load-time caps backfill): every job in the list is
    /// upserted; nothing is deleted.
    Replaced,
}

#[async_trait]
pub trait JobStore: Send + Sync {
    async fn load_all(&self) -> Result<Vec<ScheduledJob>>;
    async fn persist(&self, all: &[ScheduledJob], change: JobChange<'_>) -> Result<()>;
    /// Whether other hub instances can write this store too — when true the
    /// scheduler periodically refreshes its in-memory list from it.
    fn is_shared(&self) -> bool;
}

/// Today's behaviour: the whole job list as pretty JSON, written to a temp
/// file and renamed into place.
pub struct FileJobStore {
    path: PathBuf,
}

impl FileJobStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

#[async_trait]
impl JobStore for FileJobStore {
    async fn load_all(&self) -> Result<Vec<ScheduledJob>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let content = std::fs::read_to_string(&self.path)
            .with_context(|| format!("reading {}", self.path.display()))?;
        serde_json::from_str(&content).with_context(|| format!("parsing {}", self.path.display()))
    }

    async fn persist(&self, all: &[ScheduledJob], _change: JobChange<'_>) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(all).context("serializing jobs")?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json)
            .and_then(|()| std::fs::rename(&tmp, &self.path))
            .with_context(|| format!("writing {}", self.path.display()))
    }

    fn is_shared(&self) -> bool {
        false
    }
}

/// The part of an encoded job a [`JobChange`] writes over an existing row:
/// its `fields`, or everything but them.
pub(crate) fn part_of(
    definition: &serde_json::Value,
    fields: &[&str],
    keep: bool,
) -> serde_json::Value {
    definition
        .as_object()
        .map(|object| {
            object
                .iter()
                .filter(|(key, _)| fields.contains(&key.as_str()) == keep)
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<serde_json::Map<_, _>>()
        })
        .unwrap_or_default()
        .into()
}

/// Select and build the backend for one agent's scheduler. Fails closed: an
/// unknown backend or an unreachable Postgres is an error, never a silent
/// fall-back to the file.
pub async fn build_job_store(
    config: &WorkforceConfig,
    workspace: &Path,
    agent_id: &str,
) -> Result<Arc<dyn JobStore>> {
    match config.scheduler_store.backend.as_str() {
        "" | "file" => Ok(Arc::new(FileJobStore::new(jobs_file_path(workspace)))),
        "postgres" => build_postgres_job_store(config, agent_id).await,
        other => {
            bail!("unknown [scheduler_store] backend `{other}` — expected \"file\" or \"postgres\"")
        }
    }
}

/// The file backend's location — also the source the `migrate-scheduler`
/// backfill reads.
pub fn jobs_file_path(workspace: &Path) -> PathBuf {
    workspace.join("scheduler").join("jobs.json")
}

#[cfg(feature = "postgres")]
async fn build_postgres_job_store(
    config: &WorkforceConfig,
    agent_id: &str,
) -> Result<Arc<dyn JobStore>> {
    let pg = config
        .postgres
        .as_ref()
        .filter(|p| p.enabled)
        .ok_or_else(|| {
            eyre::eyre!(
                "[scheduler_store] backend = \"postgres\" requires [postgres] enabled = true"
            )
        })?;
    let pool = postgres::shared_pool(pg).await?;
    Ok(Arc::new(postgres::PostgresJobStore::new(
        pool,
        agent_id.to_string(),
    )))
}

#[cfg(not(feature = "postgres"))]
async fn build_postgres_job_store(
    _config: &WorkforceConfig,
    _agent_id: &str,
) -> Result<Arc<dyn JobStore>> {
    bail!("[scheduler_store] backend = \"postgres\" but this build lacks the `postgres` feature")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::{JobPayload, Schedule};

    fn job(id: &str, name: &str) -> ScheduledJob {
        ScheduledJob {
            id: id.to_string(),
            name: name.to_string(),
            description: None,
            schedule: Schedule::Every {
                every_secs: 3600,
                anchor: None,
            },
            payload: JobPayload::SystemEvent {
                text: "test".to_string(),
                channel_id: None,
                thread_id: None,
                user_id: None,
            },
            enabled: true,
            created_at: chrono::Utc::now(),
            last_run: None,
            next_run: None,
            run_count: 0,
            is_reminder: false,
            delivery_failures: 0,
            creator_identity_id: None,
            run_as_identity_id: None,
            bill_to_identity_id: None,
            visibility: None,
            max_runs: None,
            budget_usd: None,
            expires_at: None,
            deliver_to: None,
            edited_by: None,
            paused_by_suspension: false,
        }
    }

    #[tokio::test]
    async fn file_store_round_trips_and_handles_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FileJobStore::new(jobs_file_path(tmp.path()));
        assert!(store.load_all().await.unwrap().is_empty());

        let jobs = vec![job("a", "one"), job("b", "two")];
        store.persist(&jobs, JobChange::Replaced).await.unwrap();
        let loaded = store.load_all().await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, "a");
        assert_eq!(loaded[1].name, "two");
    }

    #[tokio::test]
    async fn file_store_fails_closed_on_corrupt_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = jobs_file_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        assert!(FileJobStore::new(path).load_all().await.is_err());
    }

    #[tokio::test]
    async fn unknown_backend_fails_closed() {
        let mut config = WorkforceConfig::default();
        config.scheduler_store.backend = "mystery".into();
        let tmp = tempfile::tempdir().unwrap();
        assert!(build_job_store(&config, tmp.path(), "a").await.is_err());
    }
}
