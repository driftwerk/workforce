//! Scopes: one job's workspace, materialised as clones of the golden
//! datasets inside one toolbox container, with a lifetime of its own.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use eyre::{bail, Context, Result};
use parking_lot::Mutex;
use tracing::{info, warn};
use workforce_agent::tools::exec_runner::{
    run_setup, validate_overrides, ExecOutcome, ExecRequest, ExecSummary, HomeSpec, JobId, JobLink,
    McpBinding, PrepareRequest, Prepared, RefusalReason, RepoSpec, ScopeId, ServerFrame,
    SetupReport, WorkerHealth,
};

use crate::bridge::Bridge;
use crate::config::{dataset_safe, GoldenConfig, WorkerConfig, HOME_GOLDEN};
use crate::jobs::{LiveJob, JOBS_ROOT};
use crate::ports::{
    Cloud, ContainerId, ContainerSpec, Containers, Dataset, ExecSpec, Mount, Repos, Snapshot, Zfs,
    REPO_ENV_PREFIX,
};
use crate::state::{Home, JobRecord, Scope, State, Store};

/// Label every scope container carries, valued with its scope id.
pub const SCOPE_LABEL: &str = "workforce.scope";
/// The pool child holding keyed homes.
pub const HOMES: &str = "homes";
pub use workforce_agent::tools::exec_runner::{
    SCOPE_BENCH as BENCH_DIR, SCOPE_HOME as AGENT_HOME, SCOPE_WORK_ROOT as WORK_ROOT,
};
pub const AGENT_USER: &str = "agent";
/// The toolbox's `agent` user and `workforce` group, as the hub image creates them.
pub const AGENT_UID: u32 = 1001;
pub const AGENT_GID: u32 = 1000;
pub use workforce_agent::tools::exec_runner::SCOPE_MCP_CONFIG_PATH as MCP_CONFIG_PATH;

const READY_TIMEOUT: Duration = Duration::from_secs(120);
const READY_POLL: Duration = Duration::from_secs(1);
const CHOWN_TIMEOUT_SECS: u64 = 60;
const GIT_TIMEOUT_SECS: u64 = 600;
const SETUP_TIMEOUT_SECS: u64 = 1800;

/// Shared caches every scope mounts, as (dataset name, path in the scope).
const CACHES: &[(&str, &str)] = &[
    ("gradle", "/home/agent/.gradle"),
    ("pnpm", "/home/agent/.local/share/pnpm"),
];

const SCOPE_ENV: &[(&str, &str)] = &[
    ("BUNDLE_PATH", "vendor/bundle"),
    ("GRADLE_USER_HOME", "/home/agent/.gradle"),
    ("RDS_HOSTNAME", "127.0.0.1"),
];

pub struct Worker {
    config: Arc<WorkerConfig>,
    zfs: Arc<dyn Zfs>,
    pub(crate) containers: Arc<dyn Containers>,
    cloud: Arc<dyn Cloud>,
    repos: Arc<dyn Repos>,
    image: String,
    pool_mount: PathBuf,
    store: Store,
    saving: Mutex<()>,
    pub(crate) scopes: Mutex<BTreeMap<ScopeId, Scope>>,
    homes: Mutex<BTreeMap<String, Home>>,
    running: Mutex<HashMap<ScopeId, HashSet<String>>>,
    prepares: tokio::sync::Mutex<()>,
    pub(crate) jobs: Mutex<BTreeMap<JobId, JobRecord>>,
    pub(crate) live: Mutex<HashMap<JobId, LiveJob>>,
    pub(crate) starts: tokio::sync::Mutex<()>,
}

impl Worker {
    pub async fn new(
        config: Arc<WorkerConfig>,
        zfs: Arc<dyn Zfs>,
        containers: Arc<dyn Containers>,
        cloud: Arc<dyn Cloud>,
        repos: Arc<dyn Repos>,
    ) -> Result<Self> {
        let image = config.toolbox_image()?;
        let pool = Dataset(config.pool.clone());
        let pool_mount = zfs.mountpoint(&pool).await?;
        let datasets = CACHES
            .iter()
            .map(|(name, _)| pool.child("caches").child(name))
            .chain([pool.child(HOMES)]);
        for dataset in datasets {
            if !zfs.exists(&dataset).await? {
                zfs.create(&dataset, &[]).await?;
            }
        }
        let store = Store::new(&config.state_dir);
        let state = store.load()?;
        let scopes = state
            .scopes
            .into_iter()
            .map(|scope| (scope.id.clone(), scope))
            .collect();
        let homes = state
            .homes
            .into_iter()
            .map(|home| (home.key.clone(), home))
            .collect();
        let jobs = state
            .jobs
            .into_iter()
            .map(|job| (job.id.clone(), job))
            .collect();
        Ok(Self {
            config,
            zfs,
            containers,
            cloud,
            repos,
            image,
            pool_mount,
            store,
            saving: Mutex::new(()),
            scopes: Mutex::new(scopes),
            homes: Mutex::new(homes),
            running: Mutex::new(HashMap::new()),
            prepares: tokio::sync::Mutex::new(()),
            jobs: Mutex::new(jobs),
            live: Mutex::new(HashMap::new()),
            starts: tokio::sync::Mutex::new(()),
        })
    }

    pub fn config(&self) -> &WorkerConfig {
        &self.config
    }

    pub fn zfs(&self) -> &dyn Zfs {
        self.zfs.as_ref()
    }

    pub fn containers(&self) -> &dyn Containers {
        self.containers.as_ref()
    }

    pub fn cloud(&self) -> &dyn Cloud {
        self.cloud.as_ref()
    }

    pub fn repos(&self) -> &dyn Repos {
        self.repos.as_ref()
    }

    /// The value of a `[worker.secrets.env]` entry, read from the cloud now.
    pub(crate) async fn secret(&self, name: &str) -> Result<String> {
        let parameter = self
            .config
            .secrets
            .env
            .get(name)
            .ok_or_else(|| eyre::eyre!("{name} is not in [worker.secrets.env]"))?;
        self.cloud.parameter(parameter).await
    }

    pub fn image(&self) -> &str {
        &self.image
    }

    pub fn pool(&self) -> Dataset {
        Dataset(self.config.pool.clone())
    }

    pub fn golden_dataset(&self, name: &str) -> Dataset {
        self.pool().child("golden").child(name)
    }

    pub fn work_dataset(&self, scope: &ScopeId) -> Dataset {
        self.pool().child("work").child(scope.as_str())
    }

    pub fn home_dataset(&self, key: &str) -> Dataset {
        self.pool().child(HOMES).child(key)
    }

    pub fn homes(&self) -> Vec<Home> {
        self.homes.lock().values().cloned().collect()
    }

    /// Where a dataset under the pool is mounted on the host.
    pub fn host_path(&self, dataset: &Dataset) -> PathBuf {
        let relative = dataset
            .0
            .strip_prefix(&self.config.pool)
            .and_then(|rest| rest.strip_prefix('/'))
            .unwrap_or("");
        self.pool_mount.join(relative)
    }

    pub async fn newest_snapshot(&self, dataset: &Dataset) -> Result<Option<Snapshot>> {
        if !self.zfs.exists(dataset).await? {
            return Ok(None);
        }
        Ok(self
            .zfs
            .snapshots(dataset)
            .await?
            .pop()
            .map(|info| info.snapshot))
    }

    pub fn scopes(&self) -> Vec<Scope> {
        self.scopes.lock().values().cloned().collect()
    }

    pub(crate) fn mount(&self, dataset: &Dataset, inside: &str) -> Mount {
        Mount {
            host: self.host_path(dataset),
            container: inside.to_string(),
            read_only: false,
        }
    }

    /// Write what is remembered. Serialised so a slower writer can never
    /// replace a newer state with the older one it read first.
    pub(crate) fn save(&self) -> Result<()> {
        let _one_at_a_time = self.saving.lock();
        self.store.save(&State {
            scopes: self.scopes(),
            homes: self.homes(),
            jobs: self.jobs.lock().values().cloned().collect(),
        })
    }

    pub async fn prepare(&self, request: PrepareRequest) -> Result<ServerFrame> {
        let _one_at_a_time = self.prepares.lock().await;
        if !valid_scope_id(&request.scope) {
            bail!("scope id {:?} is not a dataset-safe name", request.scope);
        }
        if let HomeSpec::Keyed { key, .. } = &request.home {
            if !valid_home_key(key) {
                bail!("home key {key:?} is not a dataset-safe name");
            }
        }
        if let Some(existing) = self.touch(&request.scope) {
            let home_reused = existing.home.is_some();
            return Ok(ServerFrame::Prepared(prepared(
                &existing,
                true,
                home_reused,
            )));
        }
        if self.scopes.lock().len() as u32 >= self.config.max_scopes {
            return Ok(refused(RefusalReason::AtCapacity));
        }
        if self.zfs.free_bytes(&self.config.pool).await? < self.config.pool_free_floor_bytes() {
            // A refresh pruning at the same moment can take a snapshot out
            // from under this; whatever was freed still counts below.
            if let Err(error) = crate::refresh::reclaim(self).await {
                warn!(%error, "reclaiming pool space before a prepare failed");
            }
            if self.zfs.free_bytes(&self.config.pool).await? < self.config.pool_free_floor_bytes() {
                return Ok(refused(RefusalReason::PoolFull));
            }
        }
        validate_overrides(&request.env).map_err(|reason| eyre::eyre!(reason))?;

        let root = self.work_dataset(&request.scope);
        let mut started: Option<ContainerId> = None;
        match self.materialise(&request, &root, &mut started).await {
            Ok((scope, home_reused)) => {
                let frame = ServerFrame::Prepared(prepared(&scope, false, home_reused));
                self.scopes.lock().insert(scope.id.clone(), scope);
                self.save()?;
                Ok(frame)
            }
            Err(error) => {
                if let Some(container) = started {
                    let _ = self.containers.stop(&container).await;
                }
                if self.zfs.exists(&root).await.unwrap_or(false) {
                    let _ = self.zfs.destroy(&root).await;
                }
                Err(error)
            }
        }
    }

    /// Build the scope; the bool says whether a keyed home was found rather
    /// than created.
    async fn materialise(
        &self,
        request: &PrepareRequest,
        root: &Dataset,
        started: &mut Option<ContainerId>,
    ) -> Result<(Scope, bool)> {
        let scope_env = self.scope_env(&request.repos).await?;
        self.zfs.create(root, &[]).await?;
        let mut mounts = Vec::new();
        let mut repos = BTreeMap::new();
        let mut cloned = HashSet::new();
        let mut fresh = Vec::new();
        for repo in &request.repos {
            let dataset = root.child(&repo.alias);
            let inside = format!("{WORK_ROOT}/{}", repo.alias);
            let golden = self.config.golden_for_slug(&repo.slug);
            let from_snapshot = match golden {
                Some(golden) => {
                    self.clone_or_create(&self.golden_dataset(&golden.name), &dataset)
                        .await?
                }
                None => {
                    self.zfs.create(&dataset, &[]).await?;
                    false
                }
            };
            if from_snapshot {
                cloned.insert(repo.alias.clone());
            } else {
                fresh.push(inside.clone());
            }
            mounts.push(self.mount(&dataset, &inside));
            repos.insert(repo.alias.clone(), inside);
        }
        // With no repo mounted under it the work root would not exist, and
        // every exec into a scope working there fails before it starts.
        if request.repos.is_empty() {
            let work = root.child("work");
            self.zfs.create(&work, &[]).await?;
            fresh.push(WORK_ROOT.to_string());
            mounts.push(self.mount(&work, WORK_ROOT));
        }
        let claimed_at = Utc::now();
        let (home, home_key, home_reused) = match &request.home {
            HomeSpec::Ephemeral => (root.child(HOME_GOLDEN), None, false),
            HomeSpec::Keyed { key, .. } => (
                self.home_dataset(key),
                Some(key.clone()),
                self.touch_home(key, claimed_at),
            ),
        };
        if !home_reused {
            if !self
                .clone_or_create(&self.golden_dataset(HOME_GOLDEN), &home)
                .await?
            {
                fresh.push(AGENT_HOME.to_string());
            }
            if let HomeSpec::Keyed { key, ttl_secs } = &request.home {
                self.remember_home(key, &home, *ttl_secs, claimed_at)?;
            }
        }
        mounts.push(self.mount(&home, AGENT_HOME));
        for database in &self.config.database {
            let dataset = root.child(&database.name);
            self.clone_or_create(&self.golden_dataset(&database.name), &dataset)
                .await?;
            mounts.push(self.mount(&dataset, database.kind.mount_path()));
        }
        let docker = root.child("docker");
        self.zfs.create(&docker, &[]).await?;
        mounts.push(self.mount(&docker, "/var/lib/docker"));
        let jobs = root.child("jobs");
        self.zfs.create(&jobs, &[]).await?;
        mounts.push(self.mount(&jobs, JOBS_ROOT));
        for (name, inside) in CACHES {
            mounts.push(self.mount(&self.pool().child("caches").child(name), inside));
        }

        let env: Vec<(String, String)> = base_env(&self.config)
            .into_iter()
            .chain([("TOOLBOX_SERVICES".to_string(), scope_services(&self.config))])
            .chain(scope_env)
            .chain(request.env.iter().cloned())
            .collect();

        let container = self
            .containers
            .run(&ContainerSpec {
                name: container_name(&request.scope),
                image: self.image.clone(),
                label: (SCOPE_LABEL.to_string(), request.scope.as_str().to_string()),
                mounts,
                env,
                memory_gib: self.config.scope_memory_gib,
                cpus: self.config.scope_cpus,
            })
            .await?;
        *started = Some(container.clone());
        self.await_ready(&container).await?;

        if !fresh.is_empty() {
            self.chown_agent_in(&container, &fresh).await?;
        }
        for repo in &request.repos {
            self.check_out(&container, repo, cloned.contains(&repo.alias))
                .await?;
        }
        let setup = self.run_setup(&container, &request.repos).await;
        let bench = request.repos.is_empty() && home_key.is_some();
        if bench {
            self.make_bench_in(&container).await?;
        }

        let now = Utc::now();
        Ok((
            Scope {
                id: request.scope.clone(),
                container,
                dataset: root.clone(),
                repos,
                setup,
                created_at: now,
                last_seen: now,
                ttl_secs: request.ttl_secs,
                home: home_key,
                bench,
                jobs: true,
            },
            home_reused,
        ))
    }

    /// What the goldens behind `repos` give a scope: each one's secrets, read
    /// now and once however many repos need them, and each repo's settings
    /// under its alias.
    async fn scope_env(&self, repos: &[RepoSpec]) -> Result<Vec<(String, String)>> {
        let backed: Vec<(&RepoSpec, &GoldenConfig)> = repos
            .iter()
            .filter_map(|repo| {
                self.config
                    .golden_for_slug(&repo.slug)
                    .map(|golden| (repo, golden))
            })
            .collect();
        let mut wanted: BTreeMap<&str, &str> = BTreeMap::new();
        for (repo, golden) in &backed {
            for name in &golden.secrets {
                wanted.entry(name).or_insert(&repo.slug);
            }
        }
        let mut secrets = Vec::with_capacity(wanted.len());
        for (name, slug) in wanted {
            let value = self.secret(name).await.map_err(|e| {
                eyre::eyre!(
                    "scope secret {name} for {slug} unreadable: {}",
                    first_line(&e.to_string())
                )
            })?;
            secrets.push((name.to_string(), value));
        }
        let mut settings: BTreeMap<String, (&str, String)> = BTreeMap::new();
        for (repo, (key, value)) in backed
            .iter()
            .filter_map(|(repo, golden)| repo_env(&repo.alias, golden).map(|env| (repo, env)))
        {
            if let Some((other, _)) = settings.get(&key) {
                bail!("repos {other} and {} would both set {key}", repo.alias);
            }
            settings.insert(key, (&repo.alias, value));
        }
        Ok(secrets
            .into_iter()
            .chain(settings.into_iter().map(|(key, (_, value))| (key, value)))
            .collect())
    }

    /// Mark a keyed home used now; false when the worker keeps no such home.
    fn touch_home(&self, key: &str, now: DateTime<Utc>) -> bool {
        self.homes
            .lock()
            .get_mut(key)
            .map(|home| home.last_used = now)
            .is_some()
    }

    /// Remember a keyed home the moment it exists, before any scope mounts
    /// it, so a prepare that fails later still leaves it findable and a sweep
    /// in the meantime cannot reclaim it.
    fn remember_home(
        &self,
        key: &str,
        dataset: &Dataset,
        ttl_secs: u64,
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.homes.lock().insert(
            key.to_string(),
            Home {
                key: key.to_string(),
                dataset: dataset.clone(),
                created_at: now,
                last_used: now,
                ttl_secs,
            },
        );
        self.save()
    }

    /// Make the bench directory in a keyed home, as the agent user so the
    /// round can write into it.
    async fn make_bench_in(&self, container: &ContainerId) -> Result<()> {
        let outcome = self
            .run_in(
                container,
                AGENT_USER,
                AGENT_HOME,
                &format!("mkdir -p {BENCH_DIR}"),
                Vec::new(),
                GIT_TIMEOUT_SECS,
            )
            .await?;
        if !outcome.success() {
            bail!("making the bench failed: {}", outcome.stderr.trim());
        }
        Ok(())
    }

    /// Clone the golden's newest snapshot into `target`, or create `target`
    /// empty when the golden has none; true when it was cloned.
    async fn clone_or_create(&self, golden: &Dataset, target: &Dataset) -> Result<bool> {
        match self.newest_snapshot(golden).await? {
            Some(snapshot) => {
                self.zfs.clone_snapshot(&snapshot, target, &[]).await?;
                Ok(true)
            }
            None => {
                self.zfs.create(target, &[]).await?;
                Ok(false)
            }
        }
    }

    /// Give the agent user the empty datasets it must write into, which the
    /// pool creates root-owned.
    async fn chown_agent_in(&self, container: &ContainerId, paths: &[String]) -> Result<()> {
        let script = format!("chown {AGENT_UID}:{AGENT_GID} {}", paths.join(" "));
        let outcome = self
            .run_in(
                container,
                "root",
                "/",
                &script,
                Vec::new(),
                CHOWN_TIMEOUT_SECS,
            )
            .await?;
        if !outcome.success() {
            bail!("{script} failed: {}", outcome.stderr.trim());
        }
        Ok(())
    }

    /// Whether a scope's container is ready, once started again if it had
    /// stopped. A container that cannot be asked or started is reported, not
    /// an error: its scope, and the work in it, must outlive a failed check.
    async fn revive(&self, container: &ContainerId) -> bool {
        match self.containers.ready(container).await {
            Ok(true) => return true,
            Ok(false) => {}
            Err(e) => {
                warn!(container = %container.0, error = %e, "asking whether a scope's container is ready");
                return false;
            }
        }
        let revived = match self.containers.start(container).await {
            Ok(()) => self.await_ready(container).await,
            Err(e) => Err(e),
        };
        revived
            .inspect_err(|e| warn!(container = %container.0, error = %e, "starting a scope's container again"))
            .is_ok()
    }

    pub(crate) async fn await_ready(&self, container: &ContainerId) -> Result<()> {
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        while !self.containers.ready(container).await? {
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "scope services did not become ready within {}s",
                    READY_TIMEOUT.as_secs()
                );
            }
            tokio::time::sleep(READY_POLL).await;
        }
        Ok(())
    }

    async fn check_out(
        &self,
        container: &ContainerId,
        repo: &RepoSpec,
        from_snapshot: bool,
    ) -> Result<()> {
        let inside = format!("{WORK_ROOT}/{}", repo.alias);
        let script = match (from_snapshot, repo.default_branch.as_str()) {
            (true, "") => "git fetch -q origin HEAD && git checkout -q --detach FETCH_HEAD".to_string(),
            (true, branch) => {
                format!("git fetch -q origin {branch} && git checkout -q --detach FETCH_HEAD")
            }
            (false, "") => format!(
                "git clone -q https://github.com/{slug} . && git checkout -q --detach",
                slug = repo.slug
            ),
            (false, branch) => format!(
                "git clone -q --branch {branch} https://github.com/{slug} . && git checkout -q --detach",
                slug = repo.slug
            ),
        };
        let outcome = self
            .run_in(
                container,
                AGENT_USER,
                &inside,
                &script,
                git_auth(repo.token.as_deref()),
                GIT_TIMEOUT_SECS,
            )
            .await?;
        if !outcome.success() {
            bail!(
                "checking out {} at {} failed: {}",
                repo.slug,
                Some(repo.default_branch.as_str())
                    .filter(|b| !b.is_empty())
                    .unwrap_or("the default branch"),
                outcome.stderr.trim()
            );
        }
        Ok(())
    }

    async fn run_setup(&self, container: &ContainerId, repos: &[RepoSpec]) -> Vec<SetupReport> {
        let commands: Vec<(String, String)> = repos
            .iter()
            .filter_map(|repo| {
                repo.setup
                    .clone()
                    .or_else(|| {
                        self.config
                            .golden_for_slug(&repo.slug)
                            .and_then(|golden| golden.install.clone())
                    })
                    .map(|command| (repo.alias.clone(), command))
            })
            .collect();
        run_setup(&commands, |alias, command| async move {
            let inside = format!("{WORK_ROOT}/{alias}");
            self.run_in(
                container,
                AGENT_USER,
                &inside,
                &command,
                Vec::new(),
                SETUP_TIMEOUT_SECS,
            )
            .await
        })
        .await
    }

    /// Run a shell command inside a container as `user`, collecting its output.
    pub(crate) async fn run_in(
        &self,
        container: &ContainerId,
        user: &str,
        workdir: &str,
        script: &str,
        env: Vec<(String, String)>,
        timeout_secs: u64,
    ) -> Result<ExecOutcome> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let drain = tokio::spawn(async move {
            let (mut stdout, mut stderr) = (String::new(), String::new());
            while let Some(frame) = rx.recv().await {
                match frame {
                    ServerFrame::Stdout { chunk } => stdout.push_str(&chunk),
                    ServerFrame::Stderr { chunk } => stderr.push_str(&chunk),
                    _ => {}
                }
            }
            (stdout, stderr)
        });
        let summary = self
            .containers
            .exec(
                container,
                ExecSpec {
                    job: uuid::Uuid::new_v4().to_string(),
                    user: user.to_string(),
                    workdir: workdir.to_string(),
                    program: "sh".to_string(),
                    args: vec!["-lc".to_string(), script.to_string()],
                    env,
                    stdin: None,
                    timeout_secs,
                },
                tx,
            )
            .await?;
        let (stdout, stderr) = drain.await.unwrap_or_default();
        Ok(ExecOutcome {
            stdout,
            stderr,
            summary,
        })
    }

    /// Mark a scope used now, on disk as well: a restarted daemon sweeps by
    /// the lifetime it reads back.
    pub(crate) fn scope(&self, scope: &ScopeId) -> Option<Scope> {
        self.scopes.lock().get(scope).cloned()
    }

    pub(crate) fn touch(&self, scope: &ScopeId) -> Option<Scope> {
        let touched = {
            let mut scopes = self.scopes.lock();
            let entry = scopes.get_mut(scope)?;
            entry.last_seen = Utc::now();
            entry.clone()
        };
        if let Err(e) = self.save() {
            warn!(scope = %scope, error = %e, "remembering when a scope was last used");
        }
        Some(touched)
    }

    pub async fn exec(&self, request: ExecRequest, link: Arc<JobLink>) -> Result<ServerFrame> {
        let Some(scope_id) = request.scope.clone() else {
            return Ok(refused(RefusalReason::ProtocolMismatch));
        };
        let Some(scope) = self.touch(&scope_id) else {
            return Ok(refused(RefusalReason::UnknownScope));
        };
        let workdir = checked_workdir(&request)?;
        validate_overrides(&request.env).map_err(|reason| eyre::eyre!(reason))?;
        let mut job = RunningJob::start(self, &scope, request.mcp.is_some());

        let _bridge = match request.mcp.as_ref() {
            Some(binding) => Some(self.bind_mcp(&scope, binding, Arc::clone(&link)).await?),
            None => None,
        };
        let spec = ExecSpec {
            job: job.id.clone(),
            user: AGENT_USER.to_string(),
            workdir,
            program: request.program,
            args: request.args,
            env: request.env,
            stdin: request.stdin,
            timeout_secs: request.timeout_secs,
        };
        let exec = self.containers.exec(&scope.container, spec, link.sink());
        tokio::pin!(exec);
        tokio::select! {
            biased;
            _ = link.cancelled() => {
                job.kill().await;
                Ok(ServerFrame::Done(ExecSummary::killed()))
            }
            summary = &mut exec => {
                if !matches!(&summary, Ok(done) if done.timed_out) {
                    job.ended();
                }
                summary.map(ServerFrame::Done)
            }
        }
    }

    fn end_job(&self, scope: &ScopeId, job: &str) {
        if let Some(jobs) = self.running.lock().get_mut(scope) {
            jobs.remove(job);
        }
        self.touch(scope);
    }

    /// The dataset mounted at the scope's home: its keyed home, or the clone
    /// made with it.
    fn home_dataset_of(&self, scope: &Scope) -> Dataset {
        scope
            .home
            .as_deref()
            .map(|key| self.home_dataset(key))
            .unwrap_or_else(|| scope.dataset.child(HOME_GOLDEN))
    }

    pub(crate) fn mcp_config_host_path(&self, scope: &Scope) -> PathBuf {
        self.home_host_path(scope, MCP_CONFIG_PATH)
    }

    /// Where a path under the scope's home lives on the host.
    fn home_host_path(&self, scope: &Scope, inside: &str) -> PathBuf {
        let inside = inside.strip_prefix(AGENT_HOME).unwrap_or(inside);
        self.host_path(&self.home_dataset_of(scope))
            .join(inside.trim_start_matches('/'))
    }

    async fn bind_mcp(
        &self,
        scope: &Scope,
        binding: &McpBinding,
        link: Arc<JobLink>,
    ) -> Result<Bridge> {
        let bridge = Bridge::start(link).await?;
        self.write_mcp_config(scope, binding, &bridge)?;
        Ok(bridge)
    }

    /// Point the scope's MCP config, and Codex's when the binding asks, at
    /// `bridge`.
    pub(crate) fn write_mcp_config(
        &self,
        scope: &Scope,
        binding: &McpBinding,
        bridge: &Bridge,
    ) -> Result<()> {
        let path = self.mcp_config_host_path(scope);
        write_agent_file(&path, &bridge.config(&binding.server_name), false)?;
        if let Some(codex) = binding.codex_config.as_ref() {
            if !codex.path.starts_with(AGENT_HOME) {
                bail!(
                    "codex config path {} is outside the scope home {AGENT_HOME}",
                    codex.path
                );
            }
            write_agent_file(
                &self.home_host_path(scope, &codex.path),
                &bridge.codex_config(&binding.server_name, codex.tool_timeout_secs),
                true,
            )?;
        }
        Ok(())
    }

    pub async fn release(&self, scope: &ScopeId) -> Result<ServerFrame> {
        let removed = self.scopes.lock().remove(scope);
        if let Some(removed) = removed {
            self.drop_jobs_of(scope);
            self.save()?;
            self.running.lock().remove(scope);
            let _ = std::fs::remove_file(self.mcp_config_host_path(&removed));
            if let Err(e) = self.containers.stop(&removed.container).await {
                warn!(scope = %scope, error = %e, "stopping a released scope's container");
            }
            if self.zfs.exists(&removed.dataset).await? {
                self.zfs.destroy(&removed.dataset).await?;
            }
            if let Some(key) = &removed.home {
                if let Some(home) = self.homes.lock().get_mut(key) {
                    home.last_used = Utc::now();
                }
                self.save()?;
            }
            info!(scope = %scope, "released");
        }
        Ok(ServerFrame::Done(ExecSummary::exited(0)))
    }

    /// Release every scope past its lifetime that has nothing running.
    pub async fn sweep(&self, now: DateTime<Utc>) -> Result<Vec<ScopeId>> {
        let idle: Vec<ScopeId> = {
            let running = self.running.lock();
            self.scopes
                .lock()
                .values()
                .filter(|scope| scope.expired_at(now))
                .filter(|scope| running.get(&scope.id).is_none_or(HashSet::is_empty))
                .filter(|scope| !self.jobs_running_in(&scope.id))
                .map(|scope| scope.id.clone())
                .collect()
        };
        for scope in &idle {
            self.release(scope).await?;
        }
        self.sweep_jobs(now)?;
        self.sweep_homes(now).await?;
        Ok(idle)
    }

    /// Destroy every keyed home past its lifetime that no live scope mounts.
    pub async fn sweep_homes(&self, now: DateTime<Utc>) -> Result<Vec<String>> {
        let expired: Vec<Home> = {
            let scopes = self.scopes.lock();
            let in_use: HashSet<&str> = scopes.values().filter_map(|s| s.home.as_deref()).collect();
            self.homes
                .lock()
                .values()
                .filter(|home| home.expired_at(now) && !in_use.contains(home.key.as_str()))
                .cloned()
                .collect()
        };
        for home in &expired {
            self.zfs.destroy(&home.dataset).await?;
            self.homes.lock().remove(&home.key);
            info!(home = %home.key, "home reclaimed");
        }
        if !expired.is_empty() {
            self.save()?;
        }
        Ok(expired.into_iter().map(|h| h.key).collect())
    }

    /// Make the host agree with the state file: adopt every scope whose
    /// container is present, starting it again when it stopped (as a Docker
    /// restart leaves it), and destroy containers and datasets nothing
    /// remembers. An adopted scope starts a fresh lifetime, since a job
    /// started before the restart may still be running in it.
    pub async fn reconcile(&self) -> Result<()> {
        let live: HashMap<String, ContainerId> = self
            .containers
            .list(SCOPE_LABEL)
            .await?
            .into_iter()
            .map(|(id, scope)| (scope, id))
            .collect();
        let remembered = self.scopes();
        let revived = futures::future::join_all(remembered.iter().map(|scope| async {
            match live.get(scope.id.as_str()) {
                Some(container) => Some(self.revive(container).await),
                None => None,
            }
        }))
        .await;
        for (scope, revived) in remembered.iter().zip(revived) {
            match revived {
                Some(true) => {}
                Some(false) => warn!(
                    scope = %scope.id,
                    "scope's container is not ready; keeping it, and its work, for its lifetime"
                ),
                None => {
                    warn!(scope = %scope.id, "scope has no container; forgetting it");
                    self.release(&scope.id).await?;
                }
            }
        }
        let adopted_at = Utc::now();
        self.scopes
            .lock()
            .values_mut()
            .for_each(|scope| scope.last_seen = adopted_at);
        for (scope, container) in &live {
            if !remembered.iter().any(|s| s.id.as_str() == scope) {
                warn!(scope, "container has no scope; removing it");
                let _ = self.containers.stop(container).await;
            }
        }
        let known: HashSet<String> = self
            .scopes()
            .into_iter()
            .map(|s| s.id.as_str().to_string())
            .collect();
        for dataset in self.zfs.children(&self.pool().child("work")).await? {
            if !known.contains(dataset.name()) {
                warn!(dataset = %dataset, "dataset has no scope; destroying it");
                self.zfs.destroy(&dataset).await?;
            }
        }
        for home in self.homes() {
            if !self.zfs.exists(&home.dataset).await? {
                warn!(home = %home.key, "home dataset is gone; forgetting it");
                self.homes.lock().remove(&home.key);
            }
        }
        let kept: HashSet<String> = self.homes.lock().keys().cloned().collect();
        for dataset in self.zfs.children(&self.pool().child(HOMES)).await? {
            if !kept.contains(dataset.name()) {
                warn!(dataset = %dataset, "home dataset is remembered by nothing; destroying it");
                self.zfs.destroy(&dataset).await?;
            }
        }
        self.save()
    }

    pub async fn health(&self) -> Result<WorkerHealth> {
        let mut goldens = Vec::new();
        let names = self
            .config
            .golden
            .iter()
            .map(|g| g.name.as_str())
            .chain(self.config.database.iter().map(|d| d.name.as_str()))
            .chain([HOME_GOLDEN]);
        for name in names {
            let dataset = self.golden_dataset(name);
            if self.zfs.exists(&dataset).await? {
                if let Some(info) = self.zfs.snapshots(&dataset).await?.pop() {
                    goldens.push((name.to_string(), info.created));
                }
            }
        }
        let scopes = self.scopes.lock().len() as u32;
        let pool_free_bytes = self.zfs.free_bytes(&self.config.pool).await?;
        Ok(WorkerHealth {
            scopes,
            homes: self.homes.lock().len() as u32,
            capacity: self.config.max_scopes,
            pool_free_bytes,
            goldens,
            version: env!("CARGO_PKG_VERSION").to_string(),
            jobs: self.running_jobs(),
        })
    }
}

fn prepared(scope: &Scope, reused: bool, home_reused: bool) -> Prepared {
    let workdir = match (scope.bench, scope.repos.len()) {
        (true, _) => BENCH_DIR.to_string(),
        (false, 1) => scope
            .repos
            .values()
            .next()
            .cloned()
            .unwrap_or_else(|| WORK_ROOT.to_string()),
        _ => WORK_ROOT.to_string(),
    };
    Prepared {
        workdir,
        repos: scope
            .repos
            .iter()
            .map(|(alias, path)| (alias.clone(), path.clone()))
            .collect(),
        setup: scope.setup.clone(),
        reused,
        home_reused,
        jobs: scope.jobs,
    }
}

pub(crate) fn refused(reason: RefusalReason) -> ServerFrame {
    ServerFrame::Refused { reason }
}

pub fn container_name(scope: &ScopeId) -> String {
    format!("wf-{scope}")
}

/// Environment every scope and refresh container starts with, so what refresh
/// installed is what a job finds.
pub(crate) fn base_env(config: &WorkerConfig) -> Vec<(String, String)> {
    SCOPE_ENV
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .chain(
            config
                .registry_mirror
                .iter()
                .map(|mirror| ("TOOLBOX_REGISTRY_MIRROR".to_string(), mirror.clone())),
        )
        .collect()
}

/// Every service a scope starts: its databases, then the stateless services.
pub(crate) fn scope_services(config: &WorkerConfig) -> String {
    config
        .database
        .iter()
        .map(|database| database.kind.service())
        .chain(config.services.iter().map(|service| service.name()))
        .collect::<Vec<_>>()
        .join(",")
}

/// A repo's settings as `TOOLBOX_REPO_ENV_<ALIAS>`, the alias upper-cased
/// with anything but letters and digits made `_`; none when its golden has no
/// settings.
pub(crate) fn repo_env(alias: &str, golden: &GoldenConfig) -> Option<(String, String)> {
    (!golden.env.is_empty()).then(|| {
        let suffix: String = alias
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect();
        let lines: Vec<String> = golden
            .env
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        (format!("{REPO_ENV_PREFIX}{suffix}"), lines.join("\n"))
    })
}

pub(crate) fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

/// Git authentication for a token, carried in the environment rather than
/// the remote URL so it never appears in a process list.
pub(crate) fn git_auth(token: Option<&str>) -> Vec<(String, String)> {
    token
        .map(|token| {
            vec![
                ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
                (
                    "GIT_CONFIG_KEY_0".to_string(),
                    format!("url.https://x-access-token:{token}@github.com/.insteadOf"),
                ),
                (
                    "GIT_CONFIG_VALUE_0".to_string(),
                    "https://github.com/".to_string(),
                ),
            ]
        })
        .unwrap_or_default()
}

pub fn valid_scope_id(scope: &ScopeId) -> bool {
    valid_home_key(scope.as_str())
}

pub fn valid_home_key(key: &str) -> bool {
    key.len() <= 64 && dataset_safe(key)
}

/// The working directory a job asks for, defaulting to the work root, once it
/// is known to be inside the scope.
pub(crate) fn checked_workdir(request: &ExecRequest) -> Result<String> {
    let workdir = request
        .workdir
        .clone()
        .unwrap_or_else(|| WORK_ROOT.to_string());
    if !workdir_allowed(&workdir) {
        bail!("working directory {workdir:?} is outside the scope");
    }
    Ok(workdir)
}

pub fn workdir_allowed(workdir: &str) -> bool {
    let inside = |root: &str| workdir == root || workdir.starts_with(&format!("{root}/"));
    (inside(WORK_ROOT) || inside(AGENT_HOME)) && !workdir.split('/').any(|part| part == "..")
}

/// Write (or append) a file the agent user reads, creating its directory.
fn write_agent_file(path: &Path, content: &str, append: bool) -> Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(path)
        .and_then(|mut file| file.write_all(content.as_bytes()))
        .with_context(|| format!("writing {}", path.display()))?;
    chown_agent(path)
}

fn chown_agent(path: &std::path::Path) -> Result<()> {
    std::os::unix::fs::chown(path, Some(AGENT_UID), Some(AGENT_GID))
        .or_else(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                Ok(())
            } else {
                Err(e)
            }
        })
        .with_context(|| format!("chown {}", path.display()))
}

/// One exec's membership of a scope's running set. Ends the job on every
/// route out of `exec`, including the task being aborted when the client
/// goes away, so a scope never stays unsweepable behind a job that is gone.
struct RunningJob<'a> {
    worker: &'a Worker,
    scope: ScopeId,
    container: ContainerId,
    id: String,
    mcp_config: Option<PathBuf>,
    alive: bool,
}

impl<'a> RunningJob<'a> {
    fn start(worker: &'a Worker, scope: &Scope, mcp: bool) -> Self {
        let id = uuid::Uuid::new_v4().to_string();
        worker
            .running
            .lock()
            .entry(scope.id.clone())
            .or_default()
            .insert(id.clone());
        Self {
            worker,
            scope: scope.id.clone(),
            container: scope.container.clone(),
            id,
            mcp_config: mcp.then(|| worker.mcp_config_host_path(scope)),
            alive: true,
        }
    }

    fn ended(&mut self) {
        self.alive = false;
    }

    async fn kill(&mut self) {
        if let Err(e) = self.worker.containers.kill(&self.container, &self.id).await {
            warn!(scope = %self.scope, error = %e, "ending a job");
        }
        self.ended();
    }
}

impl Drop for RunningJob<'_> {
    fn drop(&mut self) {
        self.worker.end_job(&self.scope, &self.id);
        if let Some(path) = &self.mcp_config {
            let _ = std::fs::remove_file(path);
        }
        if self.alive {
            let containers = Arc::clone(&self.worker.containers);
            let (container, id) = (self.container.clone(), self.id.clone());
            tokio::spawn(async move {
                let _ = containers.kill(&container, &id).await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Service;
    use crate::fakes::{FakeCloud, FakeContainers, FakeZfs, Harness};

    fn spec(scope: &str, repos: &[(&str, &str)]) -> PrepareRequest {
        PrepareRequest {
            scope: ScopeId::new(scope),
            repos: repos
                .iter()
                .map(|(alias, slug)| RepoSpec {
                    alias: alias.to_string(),
                    slug: slug.to_string(),
                    default_branch: "main".into(),
                    setup: None,
                    token: Some("t".into()),
                })
                .collect(),
            ttl_secs: 60,
            env: Vec::new(),
            home: HomeSpec::Ephemeral,
        }
    }

    fn keyed(scope: &str, key: &str, ttl_secs: u64) -> PrepareRequest {
        PrepareRequest {
            home: HomeSpec::Keyed {
                key: key.into(),
                ttl_secs,
            },
            ..spec(scope, &[])
        }
    }

    #[tokio::test]
    async fn prepare_clones_the_newest_golden_and_creates_an_empty_dataset_for_a_stranger() {
        let h = Harness::new().await;
        h.zfs.add_snapshot("tank/golden/rails", "old");
        h.zfs.add_snapshot("tank/golden/rails", "new");
        let frame = h
            .worker
            .prepare(spec(
                "s1",
                &[("rails", "acme/rails"), ("other", "acme/other")],
            ))
            .await
            .unwrap();
        let ServerFrame::Prepared(prepared) = frame else {
            panic!("{frame:?}")
        };
        assert!(!prepared.reused);
        assert_eq!(prepared.workdir, "/work");
        assert_eq!(
            h.zfs.origin("tank/work/s1/rails").as_deref(),
            Some("tank/golden/rails@new")
        );
        assert!(h.zfs.origin("tank/work/s1/other").is_none());
        assert!(h.zfs.exists_sync("tank/work/s1/home"));
        assert!(h.zfs.exists_sync("tank/work/s1/mysql"));
        assert!(h.zfs.exists_sync("tank/work/s1/docker"));
        let run = h.containers.runs()[0].clone();
        assert_eq!(run.label.1, "s1");
        assert!(run
            .env
            .iter()
            .any(|(k, v)| k == "TOOLBOX_SERVICES" && v == "mysql,postgres"));
        assert!(run.mounts.iter().any(|m| m.container == "/work/rails"));
        let execs = h.containers.execs();
        let heads: Vec<(String, String)> = execs
            .iter()
            .map(|e| {
                let head = e.args[1].split(' ').take(2).collect::<Vec<_>>().join(" ");
                (e.user.clone(), head)
            })
            .collect();
        assert_eq!(
            heads,
            [
                ("root", "chown 1001:1000"),
                ("agent", "git fetch"),
                ("agent", "git clone"),
                ("agent", "bundle install"),
            ]
            .map(|(user, head)| (user.to_string(), head.to_string()))
        );
        assert!(
            execs[0].args[1].contains("/work/other") && !execs[0].args[1].contains("/work/rails"),
            "only the stranger's dataset is chowned: {:?}",
            execs[0].args
        );
        assert!(execs[2].env.iter().any(|(k, _)| k == "GIT_CONFIG_KEY_0"));
    }

    #[tokio::test]
    async fn a_golden_without_a_snapshot_is_created_empty_chowned_and_cloned_from_origin() {
        let h = Harness::new().await;
        h.worker
            .prepare(spec("s1", &[("rails", "acme/rails")]))
            .await
            .unwrap();
        let execs = h.containers.execs();
        assert_eq!(execs[0].user, "root");
        assert!(execs[0].args[1].contains("/work/rails"));
        assert!(
            execs[0].args[1].contains("/home/agent"),
            "{:?}",
            execs[0].args
        );
        assert!(
            execs[1].args[1].contains("git clone"),
            "{:?}",
            execs[1].args
        );
    }

    #[tokio::test]
    async fn an_unset_branch_checks_out_the_remotes_default() {
        let h = Harness::new().await;
        h.zfs.add_snapshot("tank/golden/rails", "r");
        let mut request = spec("s1", &[("rails", "acme/rails"), ("other", "acme/other")]);
        for repo in &mut request.repos {
            repo.default_branch.clear();
        }
        h.worker.prepare(request).await.unwrap();
        let scripts: Vec<String> = h
            .containers
            .execs()
            .iter()
            .map(|e| e.args[1].clone())
            .collect();
        assert!(
            scripts
                .iter()
                .any(|s| s.starts_with("git fetch -q origin HEAD ")),
            "{scripts:?}"
        );
        assert!(
            scripts
                .iter()
                .any(|s| s.starts_with("git clone -q https://github.com/acme/other .")),
            "{scripts:?}"
        );
        assert!(
            !scripts.iter().any(|s| s.contains("--branch")),
            "{scripts:?}"
        );
    }

    #[tokio::test]
    async fn prepare_refuses_at_capacity_and_under_the_floor_before_any_side_effect() {
        let h = Harness::new().await;
        h.worker.prepare(spec("a", &[])).await.unwrap();
        h.worker.prepare(spec("b", &[])).await.unwrap();
        let frame = h.worker.prepare(spec("c", &[])).await.unwrap();
        assert_eq!(
            frame,
            ServerFrame::Refused {
                reason: RefusalReason::AtCapacity
            }
        );
        assert!(!h.zfs.exists_sync("tank/work/c"));

        h.worker.release(&ScopeId::new("b")).await.unwrap();
        h.zfs.set_free_bytes(0);
        let frame = h.worker.prepare(spec("c", &[])).await.unwrap();
        assert_eq!(
            frame,
            ServerFrame::Refused {
                reason: RefusalReason::PoolFull
            }
        );
        assert!(!h.zfs.exists_sync("tank/work/c"));
        assert_eq!(h.containers.runs().len(), 2);
    }

    #[tokio::test]
    async fn prepare_under_the_floor_reclaims_old_golden_snapshots_before_refusing() {
        let h = Harness::new().await;
        h.zfs.set_free_bytes(0);
        let old = chrono::Utc::now() - chrono::Duration::days(3);
        h.zfs
            .add_sized_snapshot_at("tank/golden/rails", "old", old, 2 << 30);
        h.zfs
            .add_snapshot_at("tank/golden/rails", "newest", chrono::Utc::now());

        let frame = h.worker.prepare(spec("c", &[])).await.unwrap();

        assert!(matches!(frame, ServerFrame::Prepared(_)), "{frame:?}");
        assert_eq!(h.zfs.snapshot_names("tank/golden/rails"), vec!["newest"]);
    }

    #[tokio::test]
    async fn a_failure_while_materialising_leaves_no_dataset_and_no_container() {
        let h = Harness::new().await;
        h.containers.fail_next_run();
        let err = h
            .worker
            .prepare(spec("s1", &[("rails", "acme/rails")]))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("run failed"), "{err}");
        assert!(!h.zfs.exists_sync("tank/work/s1"));
        assert!(h.worker.scopes().is_empty());

        h.zfs.add_snapshot("tank/golden/rails", "r");
        h.zfs.add_snapshot("tank/golden/home", "h");
        h.containers.fail_next_exec();
        let err = h
            .worker
            .prepare(spec("s2", &[("rails", "acme/rails")]))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("checking out"), "{err}");
        assert!(!h.zfs.exists_sync("tank/work/s2"));
        assert_eq!(h.containers.stopped().len(), 1);
    }

    #[tokio::test]
    async fn a_second_prepare_for_a_live_scope_returns_it_reused() {
        let h = Harness::new().await;
        h.worker
            .prepare(spec("s1", &[("rails", "acme/rails")]))
            .await
            .unwrap();
        let ServerFrame::Prepared(again) = h.worker.prepare(spec("s1", &[])).await.unwrap() else {
            panic!()
        };
        assert!(again.reused);
        assert_eq!(again.workdir, "/work/rails");
        assert_eq!(h.containers.runs().len(), 1);
    }

    #[tokio::test]
    async fn a_keyed_home_outlives_its_scope_and_is_found_by_the_next_one() {
        let h = Harness::new().await;
        h.zfs.add_snapshot("tank/golden/home", "g");
        let ServerFrame::Prepared(first) = h
            .worker
            .prepare(keyed("s1", "thread-1", 3600))
            .await
            .unwrap()
        else {
            panic!()
        };
        assert!(!first.reused && !first.home_reused);
        assert_eq!(
            h.zfs.origin("tank/homes/thread-1").as_deref(),
            Some("tank/golden/home@g")
        );
        assert!(!h.zfs.exists_sync("tank/work/s1/home"));
        let run = h.containers.runs()[0].clone();
        assert!(run
            .mounts
            .iter()
            .any(|m| m.container == "/home/agent" && m.host.ends_with("homes/thread-1")));

        h.worker.release(&ScopeId::new("s1")).await.unwrap();
        assert!(!h.zfs.exists_sync("tank/work/s1"));
        assert!(
            h.zfs.exists_sync("tank/homes/thread-1"),
            "the home outlives the scope"
        );

        let ServerFrame::Prepared(second) = h
            .worker
            .prepare(keyed("s2", "thread-1", 3600))
            .await
            .unwrap()
        else {
            panic!()
        };
        assert!(!second.reused && second.home_reused);
        assert_eq!(h.worker.homes().len(), 1);

        let ServerFrame::Prepared(again) = h
            .worker
            .prepare(keyed("s2", "thread-1", 3600))
            .await
            .unwrap()
        else {
            panic!()
        };
        assert!(again.reused && again.home_reused);

        assert!(h.worker.prepare(keyed("s3", "../etc", 1)).await.is_err());
    }

    #[tokio::test]
    async fn a_keyed_home_made_by_a_failed_prepare_is_kept_for_the_next_one() {
        let h = Harness::new().await;
        h.containers.fail_next_run();
        assert!(h
            .worker
            .prepare(keyed("s1", "thread-1", 3600))
            .await
            .is_err());
        assert!(!h.zfs.exists_sync("tank/work/s1"));
        assert!(h.zfs.exists_sync("tank/homes/thread-1"));
        assert_eq!(h.worker.homes().len(), 1, "remembered before the scope ran");

        let ServerFrame::Prepared(second) = h
            .worker
            .prepare(keyed("s2", "thread-1", 3600))
            .await
            .unwrap()
        else {
            panic!()
        };
        assert!(second.home_reused);
        assert_eq!(h.worker.homes().len(), 1);
    }

    #[tokio::test]
    async fn the_mcp_config_of_a_scope_lives_in_the_home_it_mounts() {
        let h = Harness::new().await;
        h.worker
            .prepare(keyed("s1", "thread-1", 3600))
            .await
            .unwrap();
        h.worker.prepare(spec("s2", &[])).await.unwrap();
        let path_of = |scope: &str| {
            let scope = h
                .worker
                .scopes()
                .into_iter()
                .find(|s| s.id.as_str() == scope)
                .unwrap();
            h.worker.mcp_config_host_path(&scope)
        };
        assert!(
            path_of("s1").starts_with(h.worker.host_path(&Dataset("tank/homes/thread-1".into())))
        );
        assert!(path_of("s2").starts_with(h.worker.host_path(&Dataset("tank/work/s2/home".into()))));
    }

    #[tokio::test]
    async fn a_bench_over_a_keyed_home_works_inside_the_home() {
        let h = Harness::new().await;
        let ServerFrame::Prepared(bench) = h
            .worker
            .prepare(keyed("s1", "thread-1", 3600))
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(bench.workdir, BENCH_DIR);
        assert!(h.containers.execs().iter().any(|e| e.user == AGENT_USER
            && e.args
                .iter()
                .any(|a| a.contains("mkdir -p /home/agent/bench"))));
        let ServerFrame::Prepared(ephemeral) = h.worker.prepare(spec("s2", &[])).await.unwrap()
        else {
            panic!()
        };
        assert_eq!(
            ephemeral.workdir, "/work",
            "an ephemeral bench stays under the work root"
        );
        let run = h.containers.runs().pop().unwrap();
        assert!(
            run.mounts.iter().any(|m| m.container == "/work"),
            "a scope with no repos mounts its own work root, so the workdir exists"
        );
    }

    #[tokio::test]
    async fn expired_homes_are_reclaimed_only_once_no_scope_mounts_them() {
        let h = Harness::new().await;
        h.worker
            .prepare(keyed("s1", "thread-1", 100))
            .await
            .unwrap();
        h.worker
            .prepare(keyed("s2", "thread-2", 100_000))
            .await
            .unwrap();
        let later = Utc::now() + chrono::Duration::seconds(200);
        assert!(
            h.worker.sweep_homes(later).await.unwrap().is_empty(),
            "mounted by a live scope"
        );
        h.worker.release(&ScopeId::new("s1")).await.unwrap();
        let soon = Utc::now() + chrono::Duration::seconds(50);
        assert!(
            h.worker.sweep_homes(soon).await.unwrap().is_empty(),
            "release restarts the lifetime"
        );
        assert_eq!(h.worker.sweep_homes(later).await.unwrap(), vec!["thread-1"]);
        assert!(!h.zfs.exists_sync("tank/homes/thread-1"));
        assert!(h.zfs.exists_sync("tank/homes/thread-2"));
        assert_eq!(h.worker.homes().len(), 1);
    }

    #[tokio::test]
    async fn reconcile_forgets_missing_homes_and_destroys_unremembered_ones() {
        let h = Harness::new().await;
        h.worker
            .prepare(keyed("s1", "thread-1", 100))
            .await
            .unwrap();
        h.worker.release(&ScopeId::new("s1")).await.unwrap();
        h.zfs.add_dataset("tank/homes/stray");
        h.zfs
            .destroy(&Dataset("tank/homes/thread-1".into()))
            .await
            .unwrap();
        h.worker.reconcile().await.unwrap();
        assert!(h.worker.homes().is_empty());
        assert!(!h.zfs.exists_sync("tank/homes/stray"));
    }

    #[tokio::test]
    async fn release_is_idempotent_and_destroys_everything() {
        let h = Harness::new().await;
        h.worker
            .prepare(spec("s1", &[("rails", "acme/rails")]))
            .await
            .unwrap();
        h.worker.release(&ScopeId::new("s1")).await.unwrap();
        assert!(!h.zfs.exists_sync("tank/work/s1"));
        assert_eq!(h.containers.stopped().len(), 1);
        let frame = h.worker.release(&ScopeId::new("s1")).await.unwrap();
        assert!(matches!(frame, ServerFrame::Done(ref s) if s.exit_code == Some(0)));
        assert_eq!(h.containers.stopped().len(), 1);
    }

    #[tokio::test]
    async fn the_sweeper_releases_only_expired_scopes_with_nothing_running() {
        let h = Harness::new().await;
        h.worker.prepare(spec("old", &[])).await.unwrap();
        let mut long_lived = spec("fresh", &[]);
        long_lived.ttl_secs = 3600;
        h.worker.prepare(long_lived).await.unwrap();
        let later = Utc::now() + chrono::Duration::seconds(61);
        h.worker
            .running
            .lock()
            .entry(ScopeId::new("old"))
            .or_default()
            .insert("job".into());
        assert!(h.worker.sweep(later).await.unwrap().is_empty());
        h.worker.running.lock().clear();
        assert_eq!(
            h.worker.sweep(later).await.unwrap(),
            vec![ScopeId::new("old")]
        );
        let remaining: Vec<String> = h
            .worker
            .scopes()
            .into_iter()
            .map(|s| s.id.as_str().to_string())
            .collect();
        assert_eq!(remaining, vec!["fresh"]);
    }

    fn age(worker: &Worker, scope: &str, by: chrono::Duration) {
        if let Some(entry) = worker.scopes.lock().get_mut(&ScopeId::new(scope)) {
            entry.last_seen -= by;
        }
        worker.save().unwrap();
    }

    fn scope_ids(worker: &Worker) -> Vec<String> {
        worker
            .scopes()
            .into_iter()
            .map(|s| s.id.as_str().to_string())
            .collect()
    }

    #[tokio::test]
    async fn a_scope_used_since_its_last_save_survives_a_restart_and_its_first_sweep() {
        let h = Harness::new().await;
        h.worker.prepare(spec("s1", &[])).await.unwrap();
        age(&h.worker, "s1", chrono::Duration::hours(2));
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let mut request = exec_request("true");
        request.scope = Some(ScopeId::new("s1"));
        h.worker
            .exec(request, Arc::new(JobLink::for_tests(tx)))
            .await
            .unwrap();

        let restarted = h.restarted().await;
        let remembered = restarted.scopes().pop().unwrap();
        assert!(Utc::now() - remembered.last_seen < chrono::Duration::seconds(5));
        restarted.reconcile().await.unwrap();
        assert!(restarted.sweep(Utc::now()).await.unwrap().is_empty());
        assert_eq!(scope_ids(&restarted), vec!["s1"]);
    }

    #[tokio::test]
    async fn an_adopted_scope_starts_a_fresh_lifetime() {
        let h = Harness::new().await;
        h.worker.prepare(spec("s1", &[])).await.unwrap();
        age(&h.worker, "s1", chrono::Duration::hours(2));

        let restarted = h.restarted().await;
        restarted.reconcile().await.unwrap();
        let soon = Utc::now() + chrono::Duration::seconds(30);
        assert!(restarted.sweep(soon).await.unwrap().is_empty());
        let past_ttl = Utc::now() + chrono::Duration::seconds(61);
        assert_eq!(
            restarted.sweep(past_ttl).await.unwrap(),
            vec![ScopeId::new("s1")]
        );
    }

    #[tokio::test]
    async fn releasing_a_scope_whose_dataset_is_already_gone_still_forgets_it() {
        let h = Harness::new().await;
        h.worker.prepare(spec("s1", &[])).await.unwrap();
        h.zfs
            .destroy(&Dataset("tank/work/s1".into()))
            .await
            .unwrap();
        h.containers.forget(&container_name(&ScopeId::new("s1")));

        h.worker.reconcile().await.unwrap();
        assert!(h.worker.scopes().is_empty());
        assert!(h.restarted().await.scopes().is_empty());
    }

    #[tokio::test]
    async fn a_scope_whose_container_stopped_is_started_again_with_its_files() {
        let h = Harness::new().await;
        h.worker.prepare(spec("s1", &[])).await.unwrap();
        let container = container_name(&ScopeId::new("s1"));
        h.containers.mark_unready(&container);

        h.worker.reconcile().await.unwrap();

        assert_eq!(h.containers.started(), vec![ContainerId(container)]);
        assert_eq!(h.worker.scopes().len(), 1);
        assert!(h.zfs.exists_sync("tank/work/s1"), "its work is kept");
    }

    #[tokio::test]
    async fn reconcile_adopts_ready_matches_and_destroys_orphans_both_ways() {
        let h = Harness::with(|config| config.max_scopes = 3).await;
        h.worker.prepare(spec("kept", &[])).await.unwrap();
        h.worker.prepare(spec("lost", &[])).await.unwrap();
        h.worker.prepare(spec("halted", &[])).await.unwrap();
        h.containers.forget(&container_name(&ScopeId::new("lost")));
        h.containers
            .mark_dead(&container_name(&ScopeId::new("halted")));
        h.containers.add_stray("wf-stray", SCOPE_LABEL, "stray");
        h.containers
            .add_stray("wf-refresh-rails", "workforce.refresh", "rails");
        h.zfs.add_dataset("tank/work/orphan");

        h.worker.reconcile().await.unwrap();

        let ids: Vec<String> = h
            .worker
            .scopes()
            .into_iter()
            .map(|s| s.id.as_str().to_string())
            .collect();
        assert_eq!(
            ids,
            vec!["halted", "kept"],
            "a container that will not start keeps its work"
        );
        assert!(!h.zfs.exists_sync("tank/work/lost"));
        assert!(h.zfs.exists_sync("tank/work/halted"));
        assert!(!h.zfs.exists_sync("tank/work/orphan"));
        let stopped = h.containers.stopped();
        assert!(stopped.iter().any(|c| c.0 == "wf-stray"));
        assert!(!stopped.iter().any(|c| c.0 == "wf-halted"));
        assert!(!stopped.iter().any(|c| c.0 == "wf-refresh-rails"));
    }

    #[tokio::test]
    async fn exec_refuses_an_unknown_scope_and_a_workdir_outside_the_scope() {
        let h = Harness::new().await;
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let link = Arc::new(JobLink::for_tests(tx));
        let mut request = exec_request("echo");
        request.scope = Some(ScopeId::new("missing"));
        let frame = h
            .worker
            .exec(request.clone(), Arc::clone(&link))
            .await
            .unwrap();
        assert_eq!(
            frame,
            ServerFrame::Refused {
                reason: RefusalReason::UnknownScope
            }
        );

        h.worker.prepare(spec("s1", &[])).await.unwrap();
        request.scope = Some(ScopeId::new("s1"));
        request.workdir = Some("/etc".into());
        let err = h
            .worker
            .exec(request.clone(), Arc::clone(&link))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("outside the scope"), "{err}");

        request.workdir = Some("/work/../etc".into());
        assert!(h
            .worker
            .exec(request.clone(), Arc::clone(&link))
            .await
            .is_err());

        request.workdir = Some("/work".into());
        let frame = h.worker.exec(request, link).await.unwrap();
        assert!(matches!(frame, ServerFrame::Done(ref s) if s.exit_code == Some(0)));
        let last = h.containers.execs().pop().unwrap();
        assert_eq!(last.user, "agent");
        assert_eq!(last.program, "echo");
    }

    #[tokio::test]
    async fn a_cancelled_exec_kills_the_job_and_reports_the_signal() {
        let h = Harness::new().await;
        h.worker.prepare(spec("s1", &[])).await.unwrap();
        h.containers.hang_next_exec();
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let cancel = tokio_util::sync::CancellationToken::new();
        let link = Arc::new(JobLink::for_tests_with_cancel(tx, cancel.clone()));
        let mut request = exec_request("sleep");
        request.scope = Some(ScopeId::new("s1"));
        let run = tokio::spawn({
            let worker = Arc::clone(&h.worker);
            async move { worker.exec(request, link).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
        let frame = run.await.unwrap().unwrap();
        assert_eq!(frame, ServerFrame::Done(ExecSummary::killed()));
        assert_eq!(h.containers.kills().len(), 1);
        assert!(nothing_running(&h.worker, "s1"));
    }

    #[tokio::test]
    async fn a_timed_out_exec_kills_its_job_in_the_scope() {
        let h = Harness::new().await;
        h.worker.prepare(spec("s1", &[])).await.unwrap();
        h.containers.time_out_next_exec();
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let mut request = exec_request("sleep");
        request.scope = Some(ScopeId::new("s1"));
        let frame = h
            .worker
            .exec(request, Arc::new(JobLink::for_tests(tx)))
            .await
            .unwrap();
        assert!(matches!(frame, ServerFrame::Done(ref s) if s.timed_out));
        tokio::task::yield_now().await;
        let job = h.containers.execs().pop().unwrap().job;
        assert_eq!(
            h.containers.kills(),
            vec![(ContainerId(container_name(&ScopeId::new("s1"))), job)]
        );
        assert!(nothing_running(&h.worker, "s1"));
    }

    #[tokio::test]
    async fn an_exec_whose_task_is_aborted_still_ends_its_job() {
        let h = Harness::new().await;
        h.worker.prepare(spec("s1", &[])).await.unwrap();
        h.containers.hang_next_exec();
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let link = Arc::new(JobLink::for_tests(tx));
        let mut request = exec_request("sleep");
        request.scope = Some(ScopeId::new("s1"));
        let run = tokio::spawn({
            let worker = Arc::clone(&h.worker);
            async move { worker.exec(request, link).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!nothing_running(&h.worker, "s1"));
        run.abort();
        let _ = run.await;
        tokio::task::yield_now().await;
        assert!(nothing_running(&h.worker, "s1"));
        assert_eq!(h.containers.kills().len(), 1);
        let later = Utc::now() + chrono::Duration::seconds(61);
        assert_eq!(
            h.worker.sweep(later).await.unwrap(),
            vec![ScopeId::new("s1")]
        );
    }

    fn golden(name: &str, slug: &str) -> GoldenConfig {
        GoldenConfig {
            name: name.into(),
            slug: slug.into(),
            branch: "main".into(),
            install: None,
            secrets: Vec::new(),
            env: BTreeMap::new(),
            watch: Vec::new(),
        }
    }

    fn env_of(run: &ContainerSpec, key: &str) -> Vec<String> {
        run.env
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .collect()
    }

    #[tokio::test]
    async fn a_scope_gets_the_secrets_of_the_goldens_it_checks_out_and_no_others() {
        let h = Harness::with(|config| {
            config.secrets.env.insert(
                "LOCALSTACK_AUTH_TOKEN".into(),
                "/workforce/prod/worker/localstack-token".into(),
            );
            config.golden.push(GoldenConfig {
                secrets: vec!["LOCALSTACK_AUTH_TOKEN".into(), "RAILS_MASTER_KEY".into()],
                ..golden("kotlin", "acme/kotlin")
            });
        })
        .await;
        h.worker
            .prepare(spec(
                "s1",
                &[("rails", "acme/rails"), ("other", "acme/other")],
            ))
            .await
            .unwrap();
        h.worker
            .prepare(spec(
                "s2",
                &[("rails", "acme/rails"), ("kotlin", "acme/kotlin")],
            ))
            .await
            .unwrap();
        let runs = h.containers.runs();
        assert_eq!(env_of(&runs[0], "RAILS_MASTER_KEY"), ["dev-key"]);
        assert!(env_of(&runs[0], "LOCALSTACK_AUTH_TOKEN").is_empty());
        assert_eq!(
            env_of(&runs[1], "RAILS_MASTER_KEY"),
            ["dev-key"],
            "a secret two goldens share is given once"
        );
        assert_eq!(
            env_of(&runs[1], "LOCALSTACK_AUTH_TOKEN"),
            ["localstack-key"]
        );
    }

    #[tokio::test]
    async fn a_scope_secret_that_cannot_be_read_fails_the_prepare_and_names_it() {
        let h = Harness::with(|config| {
            config.secrets.env.insert(
                "RAILS_MASTER_KEY".into(),
                "/workforce/prod/worker/missing".into(),
            );
        })
        .await;
        let error = h
            .worker
            .prepare(spec("s1", &[("rails", "acme/rails")]))
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "scope secret RAILS_MASTER_KEY for acme/rails unreadable: no parameter /workforce/prod/worker/missing"
        );
        assert!(!h.zfs.exists_sync("tank/work/s1"));
        assert!(h.containers.runs().is_empty());
        assert!(h.worker.scopes().is_empty());
    }

    #[tokio::test]
    async fn scope_services_are_the_databases_and_the_configured_services() {
        let h = Harness::with(|config| config.services = vec![Service::Redis, Service::Ministack])
            .await;
        h.worker.prepare(spec("s1", &[])).await.unwrap();
        assert_eq!(
            env_of(&h.containers.runs()[0], "TOOLBOX_SERVICES"),
            ["mysql,postgres,redis,ministack"]
        );
    }

    #[tokio::test]
    async fn the_scope_env_is_base_then_services_then_goldens_then_the_request() {
        let h = Harness::new().await;
        let mut request = spec("s1", &[("rails", "acme/rails")]);
        request.env = vec![("RAILS_MASTER_KEY".into(), "from-the-hub".into())];
        h.worker.prepare(request).await.unwrap();
        let run = &h.containers.runs()[0];
        let keys: Vec<&str> = run.env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "BUNDLE_PATH",
                "GRADLE_USER_HOME",
                "RDS_HOSTNAME",
                "TOOLBOX_REGISTRY_MIRROR",
                "TOOLBOX_SERVICES",
                "RAILS_MASTER_KEY",
                "TOOLBOX_REPO_ENV_RAILS",
                "RAILS_MASTER_KEY",
            ]
        );
        assert_eq!(env_of(run, "RAILS_MASTER_KEY"), ["dev-key", "from-the-hub"]);
    }

    #[tokio::test]
    async fn a_golden_repo_runs_its_install_after_checkout_when_no_setup_is_given() {
        let h = Harness::new().await;
        h.zfs.add_snapshot("tank/golden/rails", "r");
        let mut request = spec("s1", &[("rails", "acme/rails"), ("other", "acme/other")]);
        let setup_of = |frame: ServerFrame| match frame {
            ServerFrame::Prepared(prepared) => prepared
                .setup
                .into_iter()
                .map(|report| (report.alias, report.command))
                .collect::<Vec<_>>(),
            other => panic!("{other:?}"),
        };
        let setup = setup_of(h.worker.prepare(request.clone()).await.unwrap());
        assert_eq!(setup, [("rails".to_string(), "bundle install".to_string())]);
        let scripts: Vec<String> = h
            .containers
            .execs()
            .iter()
            .map(|e| e.args[1].clone())
            .collect();
        let last_checkout = scripts.iter().rposition(|s| s.starts_with("git ")).unwrap();
        let install = scripts.iter().position(|s| s == "bundle install").unwrap();
        assert!(last_checkout < install, "{scripts:?}");

        request.scope = ScopeId::new("s2");
        request.repos[0].setup = Some("bin/setup".into());
        let setup = setup_of(h.worker.prepare(request).await.unwrap());
        assert_eq!(setup, [("rails".to_string(), "bin/setup".to_string())]);
    }

    #[tokio::test]
    async fn repo_env_is_exported_per_alias() {
        let h = Harness::with(|config| {
            config.golden.push(GoldenConfig {
                env: BTreeMap::from([
                    ("NODE_ENV".into(), "test".into()),
                    ("CI".into(), "1".into()),
                ]),
                ..golden("web", "acme/web-app")
            });
        })
        .await;
        h.worker
            .prepare(spec(
                "s1",
                &[
                    ("web-app", "acme/web-app"),
                    ("rails", "acme/rails"),
                    ("other", "acme/other"),
                ],
            ))
            .await
            .unwrap();
        let run = &h.containers.runs()[0];
        let repo_env: Vec<(&str, &str)> = run
            .env
            .iter()
            .filter(|(k, _)| k.starts_with(REPO_ENV_PREFIX))
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            repo_env,
            [
                ("TOOLBOX_REPO_ENV_RAILS", "TZ=Europe/London"),
                ("TOOLBOX_REPO_ENV_WEB_APP", "CI=1\nNODE_ENV=test"),
            ]
        );

        let error = h
            .worker
            .prepare(spec(
                "s2",
                &[("web-app", "acme/web-app"), ("web_app", "acme/web-app")],
            ))
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "repos web-app and web_app would both set TOOLBOX_REPO_ENV_WEB_APP"
        );
    }

    fn nothing_running(worker: &Worker, scope: &str) -> bool {
        worker
            .running
            .lock()
            .get(&ScopeId::new(scope))
            .is_none_or(HashSet::is_empty)
    }

    #[tokio::test]
    async fn scope_ids_and_workdirs_are_checked_for_shape() {
        assert!(valid_scope_id(&ScopeId::new("abc123-def.4")));
        assert!(!valid_scope_id(&ScopeId::new("")));
        assert!(!valid_scope_id(&ScopeId::new("../x")));
        assert!(!valid_scope_id(&ScopeId::new("a/b")));
        assert!(workdir_allowed("/work"));
        assert!(workdir_allowed("/work/rails/sub"));
        assert!(workdir_allowed("/home/agent"));
        assert!(!workdir_allowed("/workspace"));
        assert!(!workdir_allowed("/home/agent/../root"));
        assert!(!workdir_allowed("/"));
    }

    fn exec_request(program: &str) -> ExecRequest {
        ExecRequest {
            program: program.into(),
            args: vec![],
            workdir: None,
            timeout_secs: 30,
            allow_retry: false,
            env: vec![],
            stdin: None,
            scope: None,
            mcp: None,
        }
    }

    #[allow(dead_code)]
    fn ports_are_object_safe(_: &FakeZfs, _: &FakeContainers, _: &FakeCloud) {}
}
