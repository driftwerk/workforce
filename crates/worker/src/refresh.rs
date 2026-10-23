//! Golden refresh: bring each golden checkout to its branch head with its
//! dependencies installed, reload the database goldens from them, and
//! snapshot. A golden that fails keeps its previous snapshot as the newest and
//! is remembered as failed until a later refresh of it succeeds.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use eyre::{bail, Result};
use tracing::{info, warn};

use crate::config::{DatabaseConfig, GoldenConfig, WorkerConfig, HOME_GOLDEN};
use crate::ports::{Cloud, ContainerId, ContainerSpec, Dataset, Mount, Repos, Snapshot};
use crate::scopes::{
    first_line, repo_env, Worker, AGENT_GID, AGENT_HOME, AGENT_UID, AGENT_USER, WORK_ROOT,
};
use crate::state::{GoldenFailure, GoldenStamp, GoldenStore};

/// Label every refresh container carries, valued with its golden's name, so
/// the scope reconcile never mistakes one for a stray scope.
pub const REFRESH_LABEL: &str = "workforce.refresh";
const STEP_TIMEOUT_SECS: u64 = 3 * 3600;
const SERVICE_STOP_SECS: u32 = 30;
const REDACTED: &str = "[redacted]";

/// Which goldens a refresh brings to their branch heads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection<'a> {
    All,
    Named(&'a str),
    /// The goldens whose watched paths moved since their last refresh, and
    /// the databases seeded from the ones that refreshed.
    Changed,
}

impl Selection<'_> {
    fn picks(self, name: &str) -> bool {
        match self {
            Self::All => true,
            Self::Named(wanted) => wanted == name,
            Self::Changed => false,
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct RefreshReport {
    pub refreshed: Vec<String>,
    pub failed: Vec<(String, String)>,
    pub pruned: u32,
    /// Whether a [`Selection::Changed`] refresh stood aside for one already
    /// running, which brings those goldens to their heads itself.
    pub busy: bool,
}

struct Step {
    user: &'static str,
    workdir: String,
    script: String,
    env: Vec<(String, String)>,
}

fn step(user: &'static str, workdir: impl Into<String>, script: impl Into<String>) -> Step {
    Step {
        user,
        workdir: workdir.into(),
        script: script.into(),
        env: Vec::new(),
    }
}

/// Refresh the goldens `selection` picks, one refresh on the host at a time,
/// then prune snapshots past retention that nothing depends on. A
/// [`Selection::Changed`] refresh does nothing while another runs; any other
/// waits for it.
pub async fn refresh(
    worker: &Worker,
    selection: Selection<'_>,
    now: DateTime<Utc>,
) -> Result<RefreshReport> {
    let config = worker.config();
    let store = GoldenStore::new(&config.state_dir);
    let wait = selection != Selection::Changed;
    let lock = tokio::task::spawn_blocking({
        let store = store.clone();
        move || store.lock(wait)
    })
    .await??;
    let Some(_lock) = lock else {
        info!("another refresh is running; leaving the changed goldens to it");
        return Ok(RefreshReport {
            busy: true,
            ..RefreshReport::default()
        });
    };
    let secrets = Secrets::resolve(worker).await?;
    let records = store.load()?;
    let mut report = RefreshReport::default();
    let stamp = now.format("%Y%m%dT%H%M%SZ").to_string();
    let outcomes = Outcomes {
        store: &store,
        secrets: &secrets,
        now,
    };

    for golden in &config.golden {
        let stamped = records
            .get(&golden.name)
            .and_then(|record| record.stamp.as_ref());
        let Some(watched) = due(worker, golden, selection, stamped, &secrets).await else {
            continue;
        };
        let outcome = refresh_repo(worker, golden, &secrets, &stamp)
            .await
            .map(|snapshot| GoldenStamp { snapshot, watched });
        outcomes.record(&mut report, &golden.name, outcome)?;
    }
    let databases: Vec<&DatabaseConfig> = config
        .database
        .iter()
        .filter(|database| match selection {
            Selection::Changed => database
                .seed_from
                .as_ref()
                .is_some_and(|from| report.refreshed.contains(from)),
            _ => selection.picks(&database.name),
        })
        .collect();
    for database in databases {
        let outcome = refresh_database(worker, database, &secrets, &stamp).await;
        outcomes.record(&mut report, &database.name, outcome.map(unwatched))?;
    }
    if selection.picks(HOME_GOLDEN) {
        let outcome = refresh_home(worker, &stamp).await;
        outcomes.record(&mut report, HOME_GOLDEN, outcome.map(unwatched))?;
    }
    report.pruned = prune(worker, now).await?;
    Ok(report)
}

/// The watched shas to stamp `golden` with when `selection` wants it
/// refreshed. Under [`Selection::Changed`] that is only when every watched
/// path was read and one differs from the last stamp; a path that cannot be
/// read skips the golden, since the next prepare reconciles its checkout
/// anyway.
async fn due(
    worker: &Worker,
    golden: &GoldenConfig,
    selection: Selection<'_>,
    stamped: Option<&GoldenStamp>,
    secrets: &Secrets,
) -> Option<BTreeMap<String, String>> {
    let wanted = match selection {
        Selection::Changed => !golden.watch.is_empty(),
        _ => selection.picks(&golden.name),
    };
    if !wanted {
        return None;
    }
    let (watched, unread) =
        watched_shas(worker.repos(), golden, secrets.github_token.as_deref()).await;
    if !unread.is_empty() {
        warn!(
            golden = golden.name,
            unread = %secrets.redact(&unread.join("; ")),
            "watched paths unreadable"
        );
    }
    match selection {
        Selection::Changed if !unread.is_empty() => None,
        Selection::Changed if stamped.is_some_and(|stamp| stamp.watched == watched) => {
            info!(golden = golden.name, "watched paths unchanged");
            None
        }
        _ => Some(watched),
    }
}

/// The blob sha of each path `golden` watches on its branch, read before any
/// checkout so a change landing meanwhile is never stamped as built, and a
/// `path: error` line for each path that could not be read.
pub(crate) async fn watched_shas(
    repos: &dyn Repos,
    golden: &GoldenConfig,
    token: Option<&str>,
) -> (BTreeMap<String, String>, Vec<String>) {
    let mut shas = BTreeMap::new();
    let mut unread = Vec::new();
    for path in &golden.watch {
        match repos
            .blob_sha(&golden.slug, path, &golden.branch, token)
            .await
        {
            Ok(sha) => {
                shas.insert(path.clone(), sha);
            }
            Err(e) => unread.push(format!("{path}: {}", first_line(&e.to_string()))),
        }
    }
    (shas, unread)
}

fn unwatched(snapshot: Snapshot) -> GoldenStamp {
    GoldenStamp {
        snapshot,
        watched: BTreeMap::new(),
    }
}

/// Where each golden's outcome is reported and remembered.
struct Outcomes<'a> {
    store: &'a GoldenStore,
    secrets: &'a Secrets,
    now: DateTime<Utc>,
}

impl Outcomes<'_> {
    fn record(
        &self,
        report: &mut RefreshReport,
        name: &str,
        outcome: Result<GoldenStamp>,
    ) -> Result<()> {
        match outcome {
            Ok(stamp) => {
                info!(golden = name, snapshot = %stamp.snapshot, "golden refreshed");
                report.refreshed.push(name.to_string());
                self.store.record(name, Ok(stamp))
            }
            Err(e) => {
                let error = self.secrets.redact(&e.to_string());
                warn!(golden = name, error = %error, "golden refresh failed; previous snapshot stays newest");
                self.store.record(
                    name,
                    Err(GoldenFailure {
                        at: self.now,
                        error: first_line(&error).to_string(),
                    }),
                )?;
                report.failed.push((name.to_string(), error));
                Ok(())
            }
        }
    }
}

/// The GitHub token `[worker.secrets]` names, read from the cloud now.
pub(crate) async fn github_token(
    config: &WorkerConfig,
    cloud: &dyn Cloud,
) -> Result<Option<String>> {
    match &config.secrets.github_token {
        Some(parameter) => cloud.parameter(parameter).await.map(Some),
        None => Ok(None),
    }
}

struct Secrets {
    github_token: Option<String>,
    env: Vec<(String, String)>,
}

impl Secrets {
    async fn resolve(worker: &Worker) -> Result<Self> {
        let github_token = github_token(worker.config(), worker.cloud()).await?;
        let mut env = Vec::with_capacity(worker.config().secrets.env.len());
        for key in worker.config().secrets.env.keys() {
            match worker.secret(key).await {
                Ok(value) => env.push((key.clone(), value)),
                Err(e) => warn!(key, error = %e, "secret unavailable; refreshing without it"),
            }
        }
        Ok(Self { github_token, env })
    }

    fn git_auth(&self) -> Vec<(String, String)> {
        crate::scopes::git_auth(self.github_token.as_deref())
    }

    /// `text` with every secret value this refresh holds masked, so what is
    /// logged and remembered never carries one.
    fn redact(&self, text: &str) -> String {
        self.github_token
            .iter()
            .chain(self.env.iter().map(|(_, value)| value))
            .filter(|value| !value.is_empty())
            .fold(text.to_string(), |text, value| {
                text.replace(value, REDACTED)
            })
    }
}

async fn refresh_repo(
    worker: &Worker,
    golden: &GoldenConfig,
    secrets: &Secrets,
    stamp: &str,
) -> Result<Snapshot> {
    let dataset = ensure(worker, &golden.name).await?;
    let inside = format!("{WORK_ROOT}/{}", golden.name);
    let checkout = format!(
        "if [ -d .git ]; then git fetch -q origin {branch} && git checkout -q -B {branch} FETCH_HEAD; \
         else git clone -q --branch {branch} https://github.com/{slug} .; fi",
        branch = golden.branch,
        slug = golden.slug
    );
    let mut steps = vec![
        step(
            "root",
            WORK_ROOT,
            format!("chown {AGENT_UID}:{AGENT_GID} {inside}"),
        ),
        Step {
            env: secrets.git_auth(),
            ..step(AGENT_USER, &inside, checkout)
        },
    ];
    if let Some(install) = &golden.install {
        steps.push(step(AGENT_USER, &inside, install.clone()));
    }
    run_refresh(
        worker,
        &golden.name,
        vec![worker.mount(&dataset, &inside)],
        Vec::new(),
        refresh_env(secrets, Some(golden)),
        steps,
        stamp,
    )
    .await
}

async fn refresh_database(
    worker: &Worker,
    database: &DatabaseConfig,
    secrets: &Secrets,
    stamp: &str,
) -> Result<Snapshot> {
    let dataset = ensure(worker, &database.name).await?;
    let mut mounts = vec![worker.mount(&dataset, database.kind.mount_path())];
    let mut steps = Vec::new();
    if let (Some(from), Some(seed)) = (&database.seed_from, &database.seed) {
        let repo = worker.golden_dataset(from);
        if worker.newest_snapshot(&repo).await?.is_none() {
            bail!(
                "database {} seeds from {from}, which has not been refreshed yet",
                database.name
            );
        }
        let inside = format!("{WORK_ROOT}/{from}");
        mounts.push(worker.mount(&repo, &inside));
        steps.push(step(AGENT_USER, &inside, seed.clone()));
    }
    steps.push(step(
        "root",
        "/",
        format!(
            "docker stop -t {SERVICE_STOP_SECS} {}",
            database.kind.service()
        ),
    ));
    let seeded_from = database
        .seed_from
        .as_deref()
        .and_then(|from| worker.config().golden(from));
    run_refresh(
        worker,
        &database.name,
        mounts,
        vec![database.kind.service()],
        refresh_env(secrets, seeded_from),
        steps,
        stamp,
    )
    .await
}

/// A refresh container's secrets, plus the settings of the golden checked out
/// at `/work/<name>` when there is one, as a scope would see them.
fn refresh_env(secrets: &Secrets, golden: Option<&GoldenConfig>) -> Vec<(String, String)> {
    secrets
        .env
        .iter()
        .cloned()
        .chain(golden.and_then(|golden| repo_env(&golden.name, golden)))
        .collect()
}

async fn refresh_home(worker: &Worker, stamp: &str) -> Result<Snapshot> {
    let dataset = ensure(worker, HOME_GOLDEN).await?;
    let steps = vec![step(
        "root",
        "/",
        format!("chown {AGENT_UID}:{AGENT_GID} {AGENT_HOME} && install -d -o {AGENT_UID} -g {AGENT_GID} {AGENT_HOME}/.workforce"),
    )];
    run_refresh(
        worker,
        HOME_GOLDEN,
        vec![worker.mount(&dataset, AGENT_HOME)],
        Vec::new(),
        Vec::new(),
        steps,
        stamp,
    )
    .await
}

async fn ensure(worker: &Worker, name: &str) -> Result<Dataset> {
    let dataset = worker.golden_dataset(name);
    if !worker.zfs().exists(&dataset).await? {
        worker.zfs().create(&dataset, &[]).await?;
    }
    Ok(dataset)
}

/// Run `steps` in a fresh toolbox container over `mounts`, stop it, and
/// snapshot the golden. A container an earlier, interrupted refresh left
/// behind is removed first; this one is stopped whatever happens.
async fn run_refresh(
    worker: &Worker,
    name: &str,
    mounts: Vec<Mount>,
    services: Vec<&str>,
    extra_env: Vec<(String, String)>,
    steps: Vec<Step>,
    stamp: &str,
) -> Result<Snapshot> {
    let env: Vec<(String, String)> = crate::scopes::base_env(worker.config())
        .into_iter()
        .chain([("TOOLBOX_SERVICES".to_string(), services.join(","))])
        .chain(extra_env)
        .collect();
    for (leftover, _) in worker
        .containers()
        .list(REFRESH_LABEL)
        .await?
        .into_iter()
        .filter(|(_, golden)| golden == name)
    {
        warn!(golden = name, container = %leftover, "removing a container an earlier refresh left behind");
        worker.containers().stop(&leftover).await?;
    }
    let container = worker
        .containers()
        .run(&ContainerSpec {
            name: format!("wf-refresh-{name}"),
            image: worker.image().to_string(),
            label: (REFRESH_LABEL.to_string(), name.to_string()),
            mounts,
            env,
            memory_gib: worker.config().scope_memory_gib,
            cpus: worker.config().scope_cpus,
        })
        .await?;
    let outcome = run_steps(worker, &container, steps).await;
    if let Err(e) = worker.containers().stop(&container).await {
        warn!(golden = name, error = %e, "stopping the refresh container");
    }
    outcome?;
    worker
        .zfs()
        .snapshot(&worker.golden_dataset(name), stamp)
        .await
}

async fn run_steps(worker: &Worker, container: &ContainerId, steps: Vec<Step>) -> Result<()> {
    worker.await_ready(container).await?;
    for step in steps {
        let outcome = worker
            .run_in(
                container,
                step.user,
                &step.workdir,
                &step.script,
                step.env,
                STEP_TIMEOUT_SECS,
            )
            .await?;
        if !outcome.success() {
            bail!(
                "`{}` in {} failed (exit {}): {}",
                step.script,
                step.workdir,
                outcome.exit_code(),
                outcome.stderr.trim()
            );
        }
    }
    Ok(())
}

/// Destroy golden and cache snapshots past retention that nothing depends
/// on, keeping each dataset's newest whatever its age; then, if the pool is
/// still under its free-space floor, [`reclaim`] younger ones too.
async fn prune(worker: &Worker, now: DateTime<Utc>) -> Result<u32> {
    let retention = chrono::Duration::days(worker.config().snapshot_retention_days as i64);
    let mut pruned = 0;
    for dataset in snapshotted_datasets(worker).await? {
        let mut snapshots = worker.zfs().snapshots(&dataset).await?;
        snapshots.pop();
        for info in snapshots {
            if info.clones == 0 && info.created + retention <= now {
                worker.zfs().destroy_snapshot(&info.snapshot).await?;
                pruned += 1;
            }
        }
    }
    Ok(pruned + reclaim(worker).await?)
}

/// Destroy golden and cache snapshots oldest first, whatever their age, until
/// the pool is back over its free-space floor or nothing more may go.
///
/// Age alone doesn't bound the pool: a week of rebuilt goldens can hold more
/// than the scopes leave free, and then every prepare is refused until the
/// oldest ages out. A snapshot is only ever a rollback point, so under the
/// floor it is worth less than a scope. Spared: each dataset's newest (what
/// scopes clone), anything with clones, and the newest the backup pool also
/// holds, which the next backup sends incrementally from.
pub(crate) async fn reclaim(worker: &Worker) -> Result<u32> {
    let pool = &worker.config().pool;
    let floor = worker.config().pool_free_floor_bytes();
    if worker.zfs().free_bytes(pool).await? >= floor {
        return Ok(0);
    }
    let mut candidates = Vec::new();
    for dataset in snapshotted_datasets(worker).await? {
        let base = crate::backup::backup_base(worker, &dataset).await?;
        let mut snapshots = worker.zfs().snapshots(&dataset).await?;
        snapshots.pop();
        candidates.extend(
            snapshots
                .into_iter()
                .filter(|info| info.clones == 0 && Some(&info.snapshot) != base.as_ref()),
        );
    }
    candidates.sort_by_key(|info| info.created);
    let mut reclaimed = 0;
    for info in candidates {
        if worker.zfs().free_bytes(pool).await? >= floor {
            break;
        }
        worker.zfs().destroy_snapshot(&info.snapshot).await?;
        warn!(snapshot = %info.snapshot, "pool under its free-space floor; destroyed snapshot early");
        reclaimed += 1;
    }
    Ok(reclaimed)
}

async fn snapshotted_datasets(worker: &Worker) -> Result<Vec<Dataset>> {
    let mut datasets = Vec::new();
    for parent in ["golden", "caches"] {
        datasets.extend(worker.zfs().children(&worker.pool().child(parent)).await?);
    }
    Ok(datasets)
}

pub fn describe(report: &RefreshReport) -> String {
    if report.busy {
        return "another refresh is running; nothing refreshed".to_string();
    }
    let mut lines = vec![format!(
        "refreshed {} golden(s), pruned {} snapshot(s)",
        report.refreshed.len(),
        report.pruned
    )];
    lines.extend(
        report
            .failed
            .iter()
            .map(|(name, error)| format!("failed {name}: {error}")),
    );
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::Harness;
    use crate::state::GoldenRecord;

    fn at(hour: u32) -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(2026, 9, 14, hour, 0, 0).unwrap()
    }

    fn with_kotlin(config: &mut WorkerConfig) {
        config.golden.push(GoldenConfig {
            name: "kotlin".into(),
            slug: "acme/kotlin".into(),
            branch: "main".into(),
            install: None,
            secrets: Vec::new(),
            env: BTreeMap::new(),
            watch: vec!["gradle/libs.versions.toml".into()],
        });
    }

    fn records(h: &Harness) -> BTreeMap<String, GoldenRecord> {
        GoldenStore::new(&h.worker.config().state_dir)
            .load()
            .unwrap()
    }

    #[tokio::test]
    async fn a_refresh_checks_out_installs_and_snapshots_each_golden() {
        let h = Harness::new().await;
        let report = refresh(&h.worker, Selection::All, Utc::now())
            .await
            .unwrap();
        assert_eq!(report.failed, Vec::<(String, String)>::new());
        assert_eq!(report.refreshed, vec!["rails", "mysql", "pg", "home"]);
        for golden in ["rails", "mysql", "pg", "home"] {
            assert_eq!(
                h.zfs.snapshot_names(&format!("tank/golden/{golden}")).len(),
                1
            );
        }
        let execs = h.containers.execs();
        let rails_install = execs
            .iter()
            .find(|e| e.args[1] == "bundle install")
            .unwrap();
        assert_eq!(rails_install.user, "agent");
        assert_eq!(rails_install.workdir, "/work/rails");
        let checkout = execs
            .iter()
            .find(|e| e.args[1].contains("git clone"))
            .unwrap();
        assert!(checkout
            .env
            .iter()
            .any(|(k, v)| k == "GIT_CONFIG_KEY_0" && v.contains("gh-token")));
        let seed = execs
            .iter()
            .find(|e| e.args[1] == "bin/rails db:schema:load")
            .unwrap();
        assert_eq!(seed.workdir, "/work/rails");
        let runs = h.containers.runs();
        let run_of = |name: &str| runs.iter().find(|r| r.name == name).cloned().unwrap();
        let mysql_run = run_of("wf-refresh-mysql");
        assert!(mysql_run
            .env
            .iter()
            .any(|(k, v)| k == "TOOLBOX_SERVICES" && v == "mysql"));
        assert!(mysql_run
            .env
            .iter()
            .any(|(k, v)| k == "RAILS_MASTER_KEY" && v == "dev-key"));
        assert!(mysql_run
            .env
            .iter()
            .any(|(k, v)| k == "TOOLBOX_REPO_ENV_RAILS" && v == "TZ=Europe/London"));
        assert!(mysql_run
            .mounts
            .iter()
            .any(|m| m.container == "/var/lib/toolbox/mysql"));
        assert!(run_of("wf-refresh-rails")
            .env
            .iter()
            .any(|(k, v)| k == "TOOLBOX_REPO_ENV_RAILS" && v == "TZ=Europe/London"));
        assert!(!run_of("wf-refresh-home")
            .env
            .iter()
            .any(|(k, _)| k.starts_with("TOOLBOX_REPO_ENV_")));
        assert_eq!(h.containers.stopped().len(), 4);
        assert!(h
            .containers
            .runs()
            .iter()
            .all(|run| run.label.0 == REFRESH_LABEL));

        let stamp = records(&h)["rails"].stamp.clone().unwrap();
        assert_eq!(stamp.snapshot.dataset.0, "tank/golden/rails");
        assert_eq!(
            stamp.watched,
            BTreeMap::from([("Gemfile.lock".to_string(), "lock-1".to_string())])
        );
        assert_eq!(h.repos.tokens(), vec![Some("gh-token".to_string())]);
    }

    #[tokio::test]
    async fn a_container_left_by_an_interrupted_refresh_is_removed_first() {
        let h = Harness::new().await;
        h.containers
            .add_stray("wf-refresh-rails", REFRESH_LABEL, "rails");
        let report = refresh(&h.worker, Selection::Named("rails"), Utc::now())
            .await
            .unwrap();
        assert_eq!(report.refreshed, vec!["rails"]);
        assert_eq!(h.containers.stopped().len(), 2);
        assert_eq!(h.containers.runs().len(), 1);
    }

    #[tokio::test]
    async fn a_failed_golden_keeps_its_previous_snapshot_newest_and_others_still_refresh() {
        let h = Harness::new().await;
        h.zfs.add_snapshot("tank/golden/rails", "previous");
        h.containers.fail_next_exec();
        let report = refresh(&h.worker, Selection::All, Utc::now())
            .await
            .unwrap();
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, "rails");
        assert_eq!(h.zfs.snapshot_names("tank/golden/rails"), vec!["previous"]);
        assert!(report.refreshed.contains(&"pg".to_string()));
        assert!(report.refreshed.contains(&"home".to_string()));
    }

    #[tokio::test]
    async fn a_failed_golden_is_remembered_by_its_first_line_until_a_refresh_succeeds() {
        let h = Harness::new().await;
        h.containers.fail_next_exec();
        refresh(&h.worker, Selection::Named("rails"), at(2))
            .await
            .unwrap();
        let failure = records(&h)["rails"].failure.clone().unwrap();
        assert_eq!(failure.at, at(2));
        assert_eq!(
            failure.error,
            "`chown 1001:1000 /work/rails` in /work failed (exit 1): boom"
        );

        refresh(&h.worker, Selection::Named("rails"), at(3))
            .await
            .unwrap();
        let record = records(&h)["rails"].clone();
        assert_eq!(record.failure, None);
        assert_eq!(record.stamp.unwrap().snapshot.name, "20260914T030000Z");
    }

    #[test]
    fn what_a_refresh_logs_and_remembers_never_carries_a_secret() {
        let secrets = Secrets {
            github_token: Some("gh-token".into()),
            env: vec![
                ("RAILS_MASTER_KEY".into(), "dev-key".into()),
                ("EMPTY".into(), String::new()),
            ],
        };
        assert_eq!(
            secrets.redact("fetch with gh-token failed\nkey dev-key rejected"),
            "fetch with [redacted] failed\nkey [redacted] rejected"
        );
    }

    #[tokio::test]
    async fn a_database_cannot_seed_from_a_golden_that_was_never_refreshed() {
        let h = Harness::new().await;
        let report = refresh(&h.worker, Selection::Named("mysql"), Utc::now())
            .await
            .unwrap();
        assert!(
            report.failed[0].1.contains("has not been refreshed"),
            "{:?}",
            report.failed
        );
    }

    #[tokio::test]
    async fn refresh_changed_refreshes_only_goldens_whose_watched_lockfile_moved() {
        let h = Harness::with(with_kotlin).await;
        h.repos
            .set_sha("acme/kotlin", "gradle/libs.versions.toml", "versions-1");
        refresh(&h.worker, Selection::All, at(2)).await.unwrap();
        let runs = h.containers.runs().len();

        let report = refresh(&h.worker, Selection::Changed, at(3)).await.unwrap();
        assert!(report.refreshed.is_empty() && report.failed.is_empty());
        assert_eq!(h.containers.runs().len(), runs);

        h.repos.set_sha("acme/rails", "Gemfile.lock", "lock-2");
        let report = refresh(&h.worker, Selection::Changed, at(4)).await.unwrap();
        assert_eq!(report.refreshed, vec!["rails", "mysql"]);
        assert_eq!(
            records(&h)["rails"].stamp.clone().unwrap().watched["Gemfile.lock"],
            "lock-2"
        );
        assert_eq!(
            records(&h)["kotlin"].stamp.clone().unwrap().snapshot.name,
            "20260914T020000Z"
        );
    }

    #[tokio::test]
    async fn refresh_changed_skips_a_golden_whose_sha_cannot_be_read() {
        let h = Harness::with(with_kotlin).await;
        h.repos.remove("acme/rails", "Gemfile.lock");
        let report = refresh(&h.worker, Selection::Changed, at(2)).await.unwrap();
        assert!(report.refreshed.is_empty(), "{report:?}");
        assert!(report.failed.is_empty(), "{report:?}");
        assert!(h.containers.runs().is_empty());
        assert!(!records(&h).contains_key("kotlin"));

        h.repos.set_sha("acme/rails", "Gemfile.lock", "lock-1");
        h.repos
            .set_sha("acme/kotlin", "gradle/libs.versions.toml", "versions-1");
        let report = refresh(&h.worker, Selection::Changed, at(3)).await.unwrap();
        assert_eq!(
            report.refreshed,
            vec!["rails", "kotlin", "mysql"],
            "a golden never stamped counts as changed"
        );
    }

    #[tokio::test]
    async fn refresh_changed_stands_aside_while_another_refresh_runs() {
        let h = Harness::new().await;
        h.repos.set_sha("acme/rails", "Gemfile.lock", "lock-2");
        let store = GoldenStore::new(&h.worker.config().state_dir);
        let held = store.lock(true).unwrap();
        let report = refresh(&h.worker, Selection::Changed, at(2)).await.unwrap();
        assert_eq!(
            report,
            RefreshReport {
                busy: true,
                ..RefreshReport::default()
            }
        );
        assert_eq!(
            describe(&report),
            "another refresh is running; nothing refreshed"
        );
        assert!(h.containers.runs().is_empty());

        drop(held);
        let report = refresh(&h.worker, Selection::Changed, at(3)).await.unwrap();
        assert!(!report.busy);
        assert_eq!(report.refreshed, vec!["rails", "mysql"]);
    }

    #[tokio::test]
    async fn a_normal_refresh_stamps_what_it_could_read_and_still_refreshes() {
        let h = Harness::new().await;
        h.repos.remove("acme/rails", "Gemfile.lock");
        let report = refresh(&h.worker, Selection::Named("rails"), at(2))
            .await
            .unwrap();
        assert_eq!(report.refreshed, vec!["rails"]);
        assert!(records(&h)["rails"]
            .stamp
            .clone()
            .unwrap()
            .watched
            .is_empty());
    }

    #[tokio::test]
    async fn prune_keeps_the_newest_and_anything_with_clones() {
        let h = Harness::new().await;
        let old = Utc::now() - chrono::Duration::days(30);
        h.zfs.add_snapshot_at("tank/golden/rails", "ancient", old);
        h.zfs.add_snapshot_at("tank/golden/rails", "pinned", old);
        h.zfs
            .add_snapshot_at("tank/golden/rails", "recent", Utc::now());
        h.zfs
            .add_clone("tank/golden/rails@pinned", "tank/work/x/rails");
        h.zfs.add_snapshot_at("tank/caches/pnpm", "stale", old);
        h.zfs
            .add_snapshot_at("tank/caches/pnpm", "latest", Utc::now());
        let pruned = prune(&h.worker, Utc::now()).await.unwrap();
        assert_eq!(pruned, 2);
        assert_eq!(
            h.zfs.snapshot_names("tank/golden/rails"),
            vec!["pinned", "recent"]
        );
        assert_eq!(h.zfs.snapshot_names("tank/caches/pnpm"), vec!["latest"]);
    }

    #[tokio::test]
    async fn under_the_floor_reclaim_destroys_oldest_first_until_back_over_it() {
        const GIB: i64 = 1 << 30;
        let h = Harness::new().await;
        h.zfs.set_free_bytes(0);
        let day = |n| Utc::now() - chrono::Duration::days(n);
        h.zfs
            .add_sized_snapshot_at("tank/caches/pnpm", "pinned", day(6), 9 * GIB);
        h.zfs
            .add_clone("tank/caches/pnpm@pinned", "tank/work/x/pnpm");
        h.zfs
            .add_sized_snapshot_at("tank/caches/pnpm", "latest", day(0), 9 * GIB);
        h.zfs
            .add_sized_snapshot_at("tank/golden/rails", "s1", day(5), GIB / 2);
        h.zfs
            .add_sized_snapshot_at("tank/golden/rails", "s2", day(4), 9 * GIB);
        h.zfs
            .add_sized_snapshot_at("tank/golden/rails", "s3", day(3), 2 * GIB);
        h.zfs
            .add_sized_snapshot_at("tank/golden/rails", "s4", day(2), GIB);
        h.zfs
            .add_sized_snapshot_at("tank/golden/rails", "s5", day(1), 9 * GIB);
        h.zfs.add_dataset("backup/golden/rails");
        h.zfs.add_snapshot_at("backup/golden/rails", "s2", day(4));

        let reclaimed = reclaim(&h.worker).await.unwrap();

        // s1 frees too little, s2 is the backup's incremental base, s3 gets
        // the pool over its 1 GiB floor, so s4 survives; s5 is the newest.
        assert_eq!(reclaimed, 2);
        assert_eq!(
            h.zfs.snapshot_names("tank/golden/rails"),
            vec!["s2", "s4", "s5"]
        );
        assert_eq!(
            h.zfs.snapshot_names("tank/caches/pnpm"),
            vec!["pinned", "latest"]
        );
    }

    #[tokio::test]
    async fn over_the_floor_reclaim_leaves_young_snapshots_alone() {
        let h = Harness::new().await;
        h.zfs
            .add_sized_snapshot_at("tank/golden/rails", "young", Utc::now(), 1 << 30);
        h.zfs
            .add_snapshot_at("tank/golden/rails", "newest", Utc::now());
        assert_eq!(reclaim(&h.worker).await.unwrap(), 0);
        assert_eq!(
            h.zfs.snapshot_names("tank/golden/rails"),
            vec!["young", "newest"]
        );
    }
}
