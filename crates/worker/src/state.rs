//! What the worker remembers between requests and across restarts: the live
//! scopes, and what each golden's last refresh left behind. Rewritten
//! atomically on every change so a crash leaves either the previous or the
//! next state, never a torn one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use workforce_agent::tools::exec_runner::{ExecSummary, JobId, ScopeId, SetupReport};

use crate::bridge::BridgeAddress;
use crate::ports::{ContainerId, Dataset, Snapshot};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    pub id: ScopeId,
    pub container: ContainerId,
    pub dataset: Dataset,
    pub repos: BTreeMap<String, String>,
    pub setup: Vec<SetupReport>,
    pub created_at: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub ttl_secs: u64,
    /// The keyed home mounted in this scope, which outlives it.
    #[serde(default)]
    pub home: Option<String>,
    /// Whether the scope works in a bench inside its keyed home rather than
    /// in checkouts under the work root.
    #[serde(default)]
    pub bench: bool,
    /// Whether the scope mounts the dataset durable jobs write to.
    #[serde(default)]
    pub jobs: bool,
}

impl Scope {
    pub fn expired_at(&self, now: DateTime<Utc>) -> bool {
        expired(self.last_seen, self.ttl_secs, now)
    }
}

/// A home dataset kept across scopes under a key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Home {
    pub key: String,
    pub dataset: Dataset,
    pub created_at: DateTime<Utc>,
    pub last_used: DateTime<Utc>,
    pub ttl_secs: u64,
}

impl Home {
    pub fn expired_at(&self, now: DateTime<Utc>) -> bool {
        expired(self.last_used, self.ttl_secs, now)
    }
}

fn expired(since: DateTime<Utc>, ttl_secs: u64, now: DateTime<Utc>) -> bool {
    since + chrono::Duration::seconds(ttl_secs as i64) <= now
}

/// A durable job, from its start until a client forgets it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: JobId,
    pub scope: ScopeId,
    pub started_at: DateTime<Utc>,
    pub deadline: DateTime<Utc>,
    /// Where the job reaches its MCP endpoint, kept so a restarted daemon
    /// serves it at the address the job was given.
    #[serde(default)]
    pub bridge: Option<BridgeAddress>,
    #[serde(default)]
    pub stopped: Option<Stop>,
    #[serde(default)]
    pub finished: Option<Finished>,
}

/// Why the worker ended a job before it ended by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stop {
    Cancelled,
    Deadline,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finished {
    pub at: DateTime<Utc>,
    pub summary: ExecSummary,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StateFile {
    scopes: Vec<Scope>,
    #[serde(default)]
    homes: Vec<Home>,
    #[serde(default)]
    jobs: Vec<JobRecord>,
}

/// Everything the daemon remembers between restarts.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct State {
    pub scopes: Vec<Scope>,
    pub homes: Vec<Home>,
    pub jobs: Vec<JobRecord>,
}

pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join("scopes.json"),
        }
    }

    pub fn load(&self) -> Result<State> {
        let file: StateFile = read_json(&self.path)?;
        Ok(State {
            scopes: file.scopes,
            homes: file.homes,
            jobs: file.jobs,
        })
    }

    pub fn save(&self, state: &State) -> Result<()> {
        write_json(
            &self.path,
            &StateFile {
                scopes: state.scopes.clone(),
                homes: state.homes.clone(),
                jobs: state.jobs.clone(),
            },
        )
    }
}

/// What a golden's last successful refresh made: its snapshot, and the blob
/// sha of each watched path as read before the checkout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoldenStamp {
    pub snapshot: Snapshot,
    #[serde(default)]
    pub watched: BTreeMap<String, String>,
}

/// A refresh that failed, remembered until a later one succeeds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoldenFailure {
    pub at: DateTime<Utc>,
    pub error: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoldenRecord {
    #[serde(default)]
    pub stamp: Option<GoldenStamp>,
    #[serde(default)]
    pub failure: Option<GoldenFailure>,
}

/// Every golden's record, by golden name, beside the lock that keeps two
/// refreshes from running at once.
#[derive(Debug, Clone)]
pub struct GoldenStore {
    path: PathBuf,
}

impl GoldenStore {
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join("goldens.json"),
        }
    }

    pub fn load(&self) -> Result<BTreeMap<String, GoldenRecord>> {
        read_json(&self.path)
    }

    /// Remember how a golden's refresh ended: a success replaces its stamp
    /// and clears any failure; a failure keeps the stamp of the snapshot that
    /// stays newest.
    pub fn record(
        &self,
        name: &str,
        outcome: std::result::Result<GoldenStamp, GoldenFailure>,
    ) -> Result<()> {
        let mut records = self.load()?;
        let record = records.entry(name.to_string()).or_default();
        match outcome {
            Ok(stamp) => {
                *record = GoldenRecord {
                    stamp: Some(stamp),
                    failure: None,
                }
            }
            Err(failure) => record.failure = Some(failure),
        }
        write_json(&self.path, &records)
    }

    /// Take the refresh lock, held until the returned file is dropped:
    /// waiting for another refresh to finish, or, unless `wait`, answering
    /// `None` while one runs.
    pub fn lock(&self, wait: bool) -> Result<Option<std::fs::File>> {
        let path = self.path.with_file_name("refresh.lock");
        create_parent(&path)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let locked = if wait {
            file.lock().map(|()| true)
        } else {
            match file.try_lock() {
                Ok(()) => Ok(true),
                Err(std::fs::TryLockError::WouldBlock) => Ok(false),
                Err(std::fs::TryLockError::Error(e)) => Err(e),
            }
        };
        Ok(locked
            .with_context(|| format!("locking {}", path.display()))?
            .then_some(file))
    }
}

fn read_json<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match std::fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    create_parent(path)?;
    let mut temp =
        tempfile::NamedTempFile::new_in(path.parent().unwrap_or_else(|| Path::new(".")))?;
    serde_json::to_writer_pretty(&mut temp, value)?;
    temp.persist(path)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn create_parent(path: &Path) -> Result<()> {
    match path.parent() {
        Some(dir) => {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))
        }
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(id: &str) -> Scope {
        Scope {
            id: ScopeId::new(id),
            container: ContainerId("c1".into()),
            dataset: Dataset("tank/work/a".into()),
            repos: BTreeMap::from([("rails".to_string(), "/work/rails".to_string())]),
            setup: Vec::new(),
            created_at: Utc::now(),
            last_seen: Utc::now(),
            ttl_secs: 60,
            home: None,
            bench: false,
            jobs: true,
        }
    }

    #[test]
    fn a_scope_and_a_state_from_before_durable_jobs_still_load() {
        let old = r#"{"scopes":[{"id":"a","container":"c1","dataset":"tank/work/a","repos":{},
            "setup":[],"created_at":"2026-09-01T00:00:00Z","last_seen":"2026-09-01T00:00:00Z",
            "ttl_secs":60}]}"#;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        std::fs::write(dir.path().join("scopes.json"), old).unwrap();
        let state = store.load().unwrap();
        assert!(!state.scopes[0].jobs);
        assert!(state.jobs.is_empty());
    }

    #[test]
    fn scopes_and_homes_survive_a_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(&dir.path().join("state"));
        assert_eq!(store.load().unwrap(), State::default());
        let home = Home {
            key: "thread-1".into(),
            dataset: Dataset("tank/homes/thread-1".into()),
            created_at: Utc::now(),
            last_used: Utc::now(),
            ttl_secs: 604_800,
        };
        let job = JobRecord {
            id: JobId::new("round-1"),
            scope: ScopeId::new("a"),
            started_at: Utc::now(),
            deadline: Utc::now(),
            bridge: Some(BridgeAddress {
                port: 41001,
                token: "t".into(),
            }),
            stopped: Some(Stop::Cancelled),
            finished: None,
        };
        let state = State {
            scopes: vec![scope("a"), scope("b")],
            homes: vec![home.clone()],
            jobs: vec![job.clone()],
        };
        store.save(&state).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.scopes.len(), 2);
        assert_eq!(loaded.scopes[0].id, ScopeId::new("a"));
        assert_eq!(loaded.homes, vec![home]);
        assert_eq!(loaded.jobs, vec![job]);
    }

    #[test]
    fn a_state_file_from_before_homes_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scopes.json");
        std::fs::write(&path, r#"{"scopes":[]}"#).unwrap();
        let store = Store::new(dir.path());
        assert!(store.load().unwrap().homes.is_empty());
    }

    #[test]
    fn a_golden_failure_keeps_the_stamp_and_a_later_success_clears_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = GoldenStore::new(&dir.path().join("state"));
        assert!(store.load().unwrap().is_empty());
        let stamp = GoldenStamp {
            snapshot: Dataset("tank/golden/rails".into()).snapshot("a"),
            watched: BTreeMap::from([("Gemfile.lock".to_string(), "sha-1".to_string())]),
        };
        let failure = GoldenFailure {
            at: Utc::now(),
            error: "bundle install failed".into(),
        };
        store.record("rails", Ok(stamp.clone())).unwrap();
        store.record("rails", Err(failure.clone())).unwrap();
        store.record("mysql", Err(failure.clone())).unwrap();
        let records = store.load().unwrap();
        assert_eq!(
            records["rails"],
            GoldenRecord {
                stamp: Some(stamp.clone()),
                failure: Some(failure.clone()),
            }
        );
        assert_eq!(records["mysql"].stamp, None);

        let newer = GoldenStamp {
            snapshot: Dataset("tank/golden/rails".into()).snapshot("b"),
            ..stamp
        };
        store.record("rails", Ok(newer.clone())).unwrap();
        assert_eq!(
            store.load().unwrap()["rails"],
            GoldenRecord {
                stamp: Some(newer),
                failure: None,
            }
        );
    }

    #[test]
    fn only_one_refresh_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let store = GoldenStore::new(dir.path());
        let held = store.lock(true).unwrap();
        assert!(held.is_some());
        assert!(store.lock(false).unwrap().is_none());
        drop(held);
        assert!(store.lock(false).unwrap().is_some());
    }

    #[test]
    fn a_scope_expires_only_after_its_ttl_from_last_seen() {
        let mut scope = scope("a");
        let now = Utc::now();
        scope.last_seen = now - chrono::Duration::seconds(59);
        assert!(!scope.expired_at(now));
        scope.last_seen = now - chrono::Duration::seconds(60);
        assert!(scope.expired_at(now));
    }
}
