//! In-memory ports and a ready-made worker for the crate's tests.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use eyre::{bail, Result};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use workforce_agent::tools::exec_runner::{ExecSummary, ServerFrame};

use crate::config::WorkerConfig;
use crate::ports::{
    Cloud, ContainerId, ContainerSpec, Containers, Dataset, DetachedSpec, ExecSpec, Repos,
    Snapshot, SnapshotInfo, Zfs,
};
use crate::scopes::Worker;

#[derive(Default)]
struct DatasetState {
    snapshots: Vec<SnapshotInfo>,
    origin: Option<String>,
}

#[derive(Default)]
struct ZfsState {
    datasets: BTreeMap<String, DatasetState>,
    /// Bytes destroying a snapshot gives back to the pool, by `dataset@name`.
    sizes: BTreeMap<String, i64>,
    replications: Vec<(Option<String>, String, String)>,
}

pub struct FakeZfs {
    state: Mutex<ZfsState>,
    free: AtomicI64,
    clock: AtomicI64,
    root: PathBuf,
}

impl FakeZfs {
    /// A pool whose datasets mount under `root`, so what a test writes to a
    /// host path is really written.
    pub fn rooted(root: PathBuf) -> Self {
        let fake = Self {
            state: Mutex::new(ZfsState::default()),
            free: AtomicI64::new(100 << 30),
            clock: AtomicI64::new(1_700_000_000),
            root,
        };
        for dataset in [
            "tank",
            "tank/golden",
            "tank/work",
            "tank/caches",
            "tank/state",
            "tank/docker",
            "backup",
        ] {
            fake.add_dataset(dataset);
        }
        fake
    }

    pub fn add_dataset(&self, name: &str) {
        self.state
            .lock()
            .datasets
            .entry(name.to_string())
            .or_default();
    }

    fn tick(&self) -> DateTime<Utc> {
        let secs = self.clock.fetch_add(1, Ordering::SeqCst);
        Utc.timestamp_opt(secs, 0).single().unwrap()
    }

    pub fn add_snapshot(&self, dataset: &str, name: &str) {
        let at = self.tick();
        self.add_snapshot_at(dataset, name, at);
    }

    pub fn add_snapshot_at(&self, dataset: &str, name: &str, created: DateTime<Utc>) {
        let mut state = self.state.lock();
        let entry = state.datasets.entry(dataset.to_string()).or_default();
        entry.snapshots.push(SnapshotInfo {
            snapshot: Dataset(dataset.to_string()).snapshot(name),
            created,
            clones: 0,
        });
        entry.snapshots.sort_by_key(|s| s.created);
    }

    /// A snapshot whose destruction frees `bytes`.
    pub fn add_sized_snapshot_at(
        &self,
        dataset: &str,
        name: &str,
        created: DateTime<Utc>,
        bytes: i64,
    ) {
        self.add_snapshot_at(dataset, name, created);
        self.state
            .lock()
            .sizes
            .insert(format!("{dataset}@{name}"), bytes);
    }

    pub fn add_clone(&self, snapshot: &str, dataset: &str) {
        let mut state = self.state.lock();
        state.datasets.insert(
            dataset.to_string(),
            DatasetState {
                snapshots: Vec::new(),
                origin: Some(snapshot.to_string()),
            },
        );
        Self::recount(&mut state);
    }

    fn recount(state: &mut ZfsState) {
        let origins: Vec<String> = state
            .datasets
            .values()
            .filter_map(|d| d.origin.clone())
            .collect();
        for dataset in state.datasets.values_mut() {
            for info in &mut dataset.snapshots {
                let name = info.snapshot.to_string();
                info.clones = origins.iter().filter(|o| **o == name).count() as u32;
            }
        }
    }

    pub fn origin(&self, dataset: &str) -> Option<String> {
        self.state
            .lock()
            .datasets
            .get(dataset)
            .and_then(|d| d.origin.clone())
    }

    pub fn exists_sync(&self, dataset: &str) -> bool {
        self.state.lock().datasets.contains_key(dataset)
    }

    pub fn set_free_bytes(&self, bytes: i64) {
        self.free.store(bytes, Ordering::SeqCst);
    }

    pub fn snapshot_names(&self, dataset: &str) -> Vec<String> {
        self.state
            .lock()
            .datasets
            .get(dataset)
            .map(|d| {
                d.snapshots
                    .iter()
                    .map(|s| s.snapshot.name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn replications(&self) -> Vec<(Option<String>, String, String)> {
        self.state.lock().replications.clone()
    }
}

#[async_trait]
impl Zfs for FakeZfs {
    async fn exists(&self, dataset: &Dataset) -> Result<bool> {
        Ok(self.exists_sync(&dataset.0))
    }

    async fn create(&self, dataset: &Dataset, _properties: &[(String, String)]) -> Result<()> {
        let mut state = self.state.lock();
        if state.datasets.contains_key(&dataset.0) {
            bail!("dataset {dataset} already exists");
        }
        state
            .datasets
            .insert(dataset.0.clone(), DatasetState::default());
        Ok(())
    }

    async fn children(&self, dataset: &Dataset) -> Result<Vec<Dataset>> {
        let prefix = format!("{}/", dataset.0);
        Ok(self
            .state
            .lock()
            .datasets
            .keys()
            .filter(|name| name.starts_with(&prefix) && !name[prefix.len()..].contains('/'))
            .map(|name| Dataset(name.clone()))
            .collect())
    }

    async fn mountpoint(&self, dataset: &Dataset) -> Result<PathBuf> {
        Ok(self.root.join(&dataset.0))
    }

    async fn snapshot(&self, dataset: &Dataset, name: &str) -> Result<Snapshot> {
        if !self.exists_sync(&dataset.0) {
            bail!("dataset {dataset} does not exist");
        }
        self.add_snapshot(&dataset.0, name);
        Ok(dataset.snapshot(name))
    }

    async fn snapshots(&self, dataset: &Dataset) -> Result<Vec<SnapshotInfo>> {
        Ok(self
            .state
            .lock()
            .datasets
            .get(&dataset.0)
            .map(|d| d.snapshots.clone())
            .unwrap_or_default())
    }

    async fn clone_snapshot(
        &self,
        from: &Snapshot,
        to: &Dataset,
        _properties: &[(String, String)],
    ) -> Result<()> {
        let exists = self
            .state
            .lock()
            .datasets
            .get(&from.dataset.0)
            .is_some_and(|d| d.snapshots.iter().any(|s| s.snapshot == *from));
        if !exists {
            bail!("snapshot {from} does not exist");
        }
        self.add_clone(&from.to_string(), &to.0);
        Ok(())
    }

    async fn destroy(&self, dataset: &Dataset) -> Result<()> {
        let mut state = self.state.lock();
        let prefix = format!("{}/", dataset.0);
        if !state
            .datasets
            .keys()
            .any(|name| name == &dataset.0 || name.starts_with(&prefix))
        {
            bail!("cannot open '{dataset}': dataset does not exist");
        }
        state
            .datasets
            .retain(|name, _| name != &dataset.0 && !name.starts_with(&prefix));
        Self::recount(&mut state);
        Ok(())
    }

    async fn destroy_snapshot(&self, snapshot: &Snapshot) -> Result<()> {
        let mut state = self.state.lock();
        let Some(dataset) = state.datasets.get_mut(&snapshot.dataset.0) else {
            bail!("dataset {} does not exist", snapshot.dataset);
        };
        if dataset
            .snapshots
            .iter()
            .any(|s| s.snapshot == *snapshot && s.clones > 0)
        {
            bail!("snapshot {snapshot} has dependent clones");
        }
        dataset.snapshots.retain(|s| s.snapshot != *snapshot);
        if let Some(bytes) = state.sizes.remove(&snapshot.to_string()) {
            self.free.fetch_add(bytes, Ordering::SeqCst);
        }
        Ok(())
    }

    async fn free_bytes(&self, _pool: &str) -> Result<u64> {
        Ok(self.free.load(Ordering::SeqCst).max(0) as u64)
    }

    async fn send_receive(
        &self,
        from: Option<&Snapshot>,
        to: &Snapshot,
        into: &Dataset,
    ) -> Result<()> {
        let created = self.tick();
        let mut state = self.state.lock();
        if into
            .parent()
            .is_some_and(|parent| !state.datasets.contains_key(&parent.0))
        {
            bail!("cannot receive {into}: parent does not exist");
        }
        let target = state.datasets.entry(into.0.clone()).or_default();
        target.snapshots.push(SnapshotInfo {
            snapshot: into.snapshot(&to.name),
            created,
            clones: 0,
        });
        state.replications.push((
            from.map(ToString::to_string),
            to.to_string(),
            into.0.clone(),
        ));
        Ok(())
    }
}

#[derive(Default)]
struct ContainerState {
    runs: Vec<ContainerSpec>,
    execs: Vec<ExecSpec>,
    stopped: Vec<ContainerId>,
    kills: Vec<(ContainerId, String)>,
    detached: Vec<DetachedSpec>,
    ended: BTreeSet<String>,
    live: BTreeMap<String, (String, String)>,
    unready: BTreeSet<String>,
    /// Containers that are present but will not start.
    dead: BTreeSet<String>,
    started: Vec<ContainerId>,
}

pub struct FakeContainers {
    state: Mutex<ContainerState>,
    fail_run: AtomicBool,
    fail_exec: AtomicBool,
    hang_exec: AtomicBool,
    time_out_exec: AtomicBool,
}

impl FakeContainers {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(ContainerState::default()),
            fail_run: AtomicBool::new(false),
            fail_exec: AtomicBool::new(false),
            hang_exec: AtomicBool::new(false),
            time_out_exec: AtomicBool::new(false),
        }
    }

    pub fn fail_next_run(&self) {
        self.fail_run.store(true, Ordering::SeqCst);
    }

    pub fn fail_next_exec(&self) {
        self.fail_exec.store(true, Ordering::SeqCst);
    }

    pub fn hang_next_exec(&self) {
        self.hang_exec.store(true, Ordering::SeqCst);
    }

    pub fn time_out_next_exec(&self) {
        self.time_out_exec.store(true, Ordering::SeqCst);
    }

    pub fn runs(&self) -> Vec<ContainerSpec> {
        self.state.lock().runs.clone()
    }

    pub fn execs(&self) -> Vec<ExecSpec> {
        self.state.lock().execs.clone()
    }

    pub fn stopped(&self) -> Vec<ContainerId> {
        self.state.lock().stopped.clone()
    }

    pub fn kills(&self) -> Vec<(ContainerId, String)> {
        self.state.lock().kills.clone()
    }

    pub fn detached(&self) -> Vec<DetachedSpec> {
        self.state.lock().detached.clone()
    }

    /// Mark a detached job's process gone, as if it ended or its container
    /// lost it.
    pub fn end_detached(&self, job: &str) {
        self.state.lock().ended.insert(job.to_string());
    }

    pub fn forget(&self, name: &str) {
        self.state.lock().live.remove(name);
    }

    pub fn mark_dead(&self, name: &str) {
        let mut state = self.state.lock();
        state.unready.insert(name.to_string());
        state.dead.insert(name.to_string());
    }

    pub fn started(&self) -> Vec<ContainerId> {
        self.state.lock().started.clone()
    }

    pub fn mark_unready(&self, name: &str) {
        self.state.lock().unready.insert(name.to_string());
    }

    pub fn add_stray(&self, name: &str, label: &str, value: &str) {
        self.state
            .lock()
            .live
            .insert(name.to_string(), (label.to_string(), value.to_string()));
    }
}

#[async_trait]
impl Containers for FakeContainers {
    async fn run(&self, spec: &ContainerSpec) -> Result<ContainerId> {
        if self.fail_run.swap(false, Ordering::SeqCst) {
            bail!("run failed");
        }
        let mut state = self.state.lock();
        if state.live.contains_key(&spec.name) {
            bail!("container name {} is already in use", spec.name);
        }
        state.runs.push(spec.clone());
        state.live.insert(spec.name.clone(), spec.label.clone());
        Ok(ContainerId(spec.name.clone()))
    }

    async fn ready(&self, container: &ContainerId) -> Result<bool> {
        Ok(!self.state.lock().unready.contains(&container.0))
    }

    async fn exec(
        &self,
        _container: &ContainerId,
        spec: ExecSpec,
        sink: mpsc::Sender<ServerFrame>,
    ) -> Result<ExecSummary> {
        self.state.lock().execs.push(spec);
        if self.hang_exec.swap(false, Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
        let exit_code = if self.fail_exec.swap(false, Ordering::SeqCst) {
            let _ = sink
                .send(ServerFrame::Stderr {
                    chunk: "boom".into(),
                })
                .await;
            Some(1)
        } else {
            Some(0)
        };
        let timed_out = self.time_out_exec.swap(false, Ordering::SeqCst);
        Ok(ExecSummary {
            exit_code: if timed_out { None } else { exit_code },
            attempts: 1,
            truncated: false,
            timed_out,
        })
    }

    async fn start_detached(&self, _container: &ContainerId, spec: DetachedSpec) -> Result<()> {
        if self.fail_run.swap(false, Ordering::SeqCst) {
            bail!("start failed");
        }
        self.state.lock().detached.push(spec);
        Ok(())
    }

    async fn job_alive(&self, _container: &ContainerId, job: &str) -> Result<bool> {
        Ok(!self.state.lock().ended.contains(job))
    }

    async fn kill(&self, container: &ContainerId, job: &str) -> Result<()> {
        let mut state = self.state.lock();
        state.kills.push((container.clone(), job.to_string()));
        state.ended.insert(job.to_string());
        Ok(())
    }

    async fn stop(&self, container: &ContainerId) -> Result<()> {
        let mut state = self.state.lock();
        state.stopped.push(container.clone());
        state.live.remove(&container.0);
        Ok(())
    }

    async fn start(&self, container: &ContainerId) -> Result<()> {
        let mut state = self.state.lock();
        if state.dead.contains(&container.0) {
            bail!("container {} cannot start", container.0);
        }
        state.unready.remove(&container.0);
        state.started.push(container.clone());
        Ok(())
    }

    async fn list(&self, label: &str) -> Result<Vec<(ContainerId, String)>> {
        Ok(self
            .state
            .lock()
            .live
            .iter()
            .filter(|(_, (key, _))| key == label)
            .map(|(name, (_, value))| (ContainerId(name.clone()), value.clone()))
            .collect())
    }

    async fn has_image(&self, _image: &str) -> Result<bool> {
        Ok(true)
    }

    async fn pull(&self, _image: &str) -> Result<()> {
        Ok(())
    }
}

pub struct FakeCloud {
    parameters: BTreeMap<String, String>,
    stored: Mutex<Vec<(Snapshot, String)>>,
}

impl FakeCloud {
    pub fn new() -> Self {
        Self {
            parameters: BTreeMap::from([
                (
                    "/workforce/prod/worker/github-read-token".to_string(),
                    "gh-token".to_string(),
                ),
                (
                    "/workforce/prod/worker/rails-dev-master-key".to_string(),
                    "dev-key".to_string(),
                ),
                (
                    "/workforce/prod/worker/localstack-token".to_string(),
                    "localstack-key".to_string(),
                ),
            ]),
            stored: Mutex::new(Vec::new()),
        }
    }

    pub fn stored(&self) -> Vec<(Snapshot, String)> {
        self.stored.lock().clone()
    }
}

#[async_trait]
impl Cloud for FakeCloud {
    async fn parameter(&self, name: &str) -> Result<String> {
        self.parameters
            .get(name)
            .cloned()
            .ok_or_else(|| eyre::eyre!("no parameter {name}"))
    }

    async fn store_snapshot(&self, snapshot: &Snapshot, uri: &str) -> Result<()> {
        self.stored.lock().push((snapshot.clone(), uri.to_string()));
        Ok(())
    }
}

/// Files on each repo's branch, by (slug, path), with the token of every read.
pub struct FakeRepos {
    shas: Mutex<BTreeMap<(String, String), String>>,
    tokens: Mutex<Vec<Option<String>>>,
}

impl FakeRepos {
    pub fn new() -> Self {
        let fake = Self {
            shas: Mutex::new(BTreeMap::new()),
            tokens: Mutex::new(Vec::new()),
        };
        fake.set_sha("acme/rails", "Gemfile.lock", "lock-1");
        fake
    }

    pub fn set_sha(&self, slug: &str, path: &str, sha: &str) {
        self.shas
            .lock()
            .insert((slug.to_string(), path.to_string()), sha.to_string());
    }

    pub fn remove(&self, slug: &str, path: &str) {
        self.shas
            .lock()
            .remove(&(slug.to_string(), path.to_string()));
    }

    pub fn tokens(&self) -> Vec<Option<String>> {
        self.tokens.lock().clone()
    }
}

#[async_trait]
impl Repos for FakeRepos {
    async fn blob_sha(
        &self,
        slug: &str,
        path: &str,
        branch: &str,
        token: Option<&str>,
    ) -> Result<String> {
        self.tokens.lock().push(token.map(str::to_string));
        self.shas
            .lock()
            .get(&(slug.to_string(), path.to_string()))
            .cloned()
            .ok_or_else(|| eyre::eyre!("{path} on {slug}@{branch} → 404 Not Found\nbody"))
    }
}

pub struct Harness {
    pub worker: Arc<Worker>,
    config: Arc<WorkerConfig>,
    pub zfs: Arc<FakeZfs>,
    pub containers: Arc<FakeContainers>,
    pub cloud: Arc<FakeCloud>,
    pub repos: Arc<FakeRepos>,
    _dir: tempfile::TempDir,
}

impl Harness {
    pub async fn new() -> Self {
        Self::with(|_| {}).await
    }

    pub async fn with(adjust: impl FnOnce(&mut WorkerConfig)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let image_file = dir.path().join("toolbox-image");
        std::fs::write(&image_file, "registry/toolbox:git_abc\n").unwrap();
        let mut config = WorkerConfig {
            state_dir: dir.path().join("state"),
            toolbox_image_file: image_file,
            ..crate::config::tests::sample()
        };
        adjust(&mut config);
        let config = Arc::new(config);
        let zfs = Arc::new(FakeZfs::rooted(dir.path().join("pool")));
        let containers = Arc::new(FakeContainers::new());
        let cloud = Arc::new(FakeCloud::new());
        let repos = Arc::new(FakeRepos::new());
        let worker = worker_over(&config, &zfs, &containers, &cloud, &repos).await;
        Self {
            worker,
            config,
            zfs,
            containers,
            cloud,
            repos,
            _dir: dir,
        }
    }

    /// A fresh daemon over the same host and state file, as after a restart.
    pub async fn restarted(&self) -> Arc<Worker> {
        worker_over(
            &self.config,
            &self.zfs,
            &self.containers,
            &self.cloud,
            &self.repos,
        )
        .await
    }
}

async fn worker_over(
    config: &Arc<WorkerConfig>,
    zfs: &Arc<FakeZfs>,
    containers: &Arc<FakeContainers>,
    cloud: &Arc<FakeCloud>,
    repos: &Arc<FakeRepos>,
) -> Arc<Worker> {
    let worker = Worker::new(
        Arc::clone(config),
        Arc::clone(zfs) as Arc<dyn Zfs>,
        Arc::clone(containers) as Arc<dyn Containers>,
        Arc::clone(cloud) as Arc<dyn Cloud>,
        Arc::clone(repos) as Arc<dyn Repos>,
    )
    .await
    .unwrap();
    Arc::new(worker)
}
