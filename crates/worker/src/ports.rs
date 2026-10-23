//! The side-effect ports the worker's logic is written against: the pool,
//! the container runtime, and the cloud calls a refresh or backup needs.

use std::path::PathBuf;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use eyre::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use workforce_agent::tools::exec_runner::{ExecSummary, ServerFrame};

/// A full dataset name, `pool/path/to/dataset`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Dataset(pub String);

impl Dataset {
    pub fn child(&self, name: &str) -> Dataset {
        Dataset(format!("{}/{name}", self.0))
    }

    pub fn name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    pub fn parent(&self) -> Option<Dataset> {
        self.0
            .rsplit_once('/')
            .map(|(parent, _)| Dataset(parent.to_string()))
    }

    pub fn snapshot(&self, name: &str) -> Snapshot {
        Snapshot {
            dataset: self.clone(),
            name: name.to_string(),
        }
    }
}

impl std::fmt::Display for Dataset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Snapshot {
    pub dataset: Dataset,
    pub name: String,
}

impl std::fmt::Display for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.dataset, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotInfo {
    pub snapshot: Snapshot,
    pub created: DateTime<Utc>,
    pub clones: u32,
}

#[async_trait]
pub trait Zfs: Send + Sync {
    async fn exists(&self, dataset: &Dataset) -> Result<bool>;
    async fn create(&self, dataset: &Dataset, properties: &[(String, String)]) -> Result<()>;
    /// Children one level down, excluding `dataset` itself.
    async fn children(&self, dataset: &Dataset) -> Result<Vec<Dataset>>;
    async fn mountpoint(&self, dataset: &Dataset) -> Result<PathBuf>;
    async fn snapshot(&self, dataset: &Dataset, name: &str) -> Result<Snapshot>;
    /// Snapshots of `dataset`, oldest first.
    async fn snapshots(&self, dataset: &Dataset) -> Result<Vec<SnapshotInfo>>;
    async fn clone_snapshot(
        &self,
        from: &Snapshot,
        to: &Dataset,
        properties: &[(String, String)],
    ) -> Result<()>;
    async fn destroy(&self, dataset: &Dataset) -> Result<()>;
    async fn destroy_snapshot(&self, snapshot: &Snapshot) -> Result<()>;
    async fn free_bytes(&self, pool: &str) -> Result<u64>;
    /// Replicate `to` into `into`, incrementally from `from` when given.
    async fn send_receive(
        &self,
        from: Option<&Snapshot>,
        to: &Snapshot,
        into: &Dataset,
    ) -> Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContainerId(pub String);

impl std::fmt::Display for ContainerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub host: PathBuf,
    pub container: String,
    pub read_only: bool,
}

/// Prefix of the variables carrying one repo's settings as `KEY=value` lines:
/// the only container variables whose values may span lines.
pub const REPO_ENV_PREFIX: &str = "TOOLBOX_REPO_ENV_";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    pub label: (String, String),
    pub mounts: Vec<Mount>,
    pub env: Vec<(String, String)>,
    pub memory_gib: u32,
    pub cpus: u32,
}

/// One command inside a container, run as the agent user under a recorded
/// session so the whole tree can be ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecSpec {
    pub job: String,
    pub user: String,
    pub workdir: String,
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub stdin: Option<String>,
    pub timeout_secs: u64,
}

/// A durable job to start inside a container, detached from the caller: it
/// reads `stdin` and writes its streams and exit status under `dir`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachedSpec {
    pub job: String,
    pub user: String,
    pub workdir: String,
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// The job's directory as the container sees it.
    pub dir: String,
}

#[async_trait]
pub trait Containers: Send + Sync {
    async fn run(&self, spec: &ContainerSpec) -> Result<ContainerId>;
    /// Whether the container's init has marked the scope ready.
    async fn ready(&self, container: &ContainerId) -> Result<bool>;
    /// Run `spec` to completion, streaming output to `sink`.
    async fn exec(
        &self,
        container: &ContainerId,
        spec: ExecSpec,
        sink: mpsc::Sender<ServerFrame>,
    ) -> Result<ExecSummary>;
    /// Start `spec` and return once it is running; it outlives the call.
    async fn start_detached(&self, container: &ContainerId, spec: DetachedSpec) -> Result<()>;
    /// Whether the session `job` leads is still running in the container.
    async fn job_alive(&self, container: &ContainerId, job: &str) -> Result<bool>;
    /// End the process tree of `job`, gracefully then forcibly.
    async fn kill(&self, container: &ContainerId, job: &str) -> Result<()>;
    async fn stop(&self, container: &ContainerId) -> Result<()>;
    /// Start a stopped container again, its mounts and files as it left them.
    async fn start(&self, container: &ContainerId) -> Result<()>;
    /// Containers carrying `label`, with the label's value.
    async fn list(&self, label: &str) -> Result<Vec<(ContainerId, String)>>;
    async fn has_image(&self, image: &str) -> Result<bool>;
    async fn pull(&self, image: &str) -> Result<()>;
}

/// Reads from the repositories the goldens are cloned from.
#[async_trait]
pub trait Repos: Send + Sync {
    /// The blob sha of the file at `path` on `branch` of `slug` (`owner/name`).
    async fn blob_sha(
        &self,
        slug: &str,
        path: &str,
        branch: &str,
        token: Option<&str>,
    ) -> Result<String>;
}

#[async_trait]
pub trait Cloud: Send + Sync {
    /// A decrypted secret: an SSM parameter under `aws`, a Secret Manager
    /// secret version under `gcp`.
    async fn parameter(&self, name: &str) -> Result<String>;
    /// Stream a snapshot to an object at `uri`.
    async fn store_snapshot(&self, snapshot: &Snapshot, uri: &str) -> Result<()>;
}
