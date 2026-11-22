//! Shared test fixtures for adapter unit tests.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use tempfile::TempDir;
use workforce_agent::artifacts::{set_global_store, FsArtifactStore};
use workforce_common::settings::{SecretStore, VaultSource};

/// Backing TempDir for the hub-wide artifact store, installed once for
/// the whole test binary. Tests namespace their writes by `agent_id`
/// (via [`fresh_agent_id`]) to stay isolated under the shared store.
static ARTIFACT_STORE_DIR: LazyLock<TempDir> = LazyLock::new(|| {
    let tmp = TempDir::new().expect("tempdir");
    let store = FsArtifactStore::new(tmp.path().to_path_buf()).expect("artifact store");
    set_global_store(store);
    tmp
});
static AGENT_SEQ: AtomicUsize = AtomicUsize::new(0);

/// Mint a fresh `agent_id` namespaced for one test, ensuring the global
/// artifact store is installed.
pub fn fresh_agent_id(prefix: &str) -> String {
    LazyLock::force(&ARTIFACT_STORE_DIR);
    format!("{prefix}-{}", AGENT_SEQ.fetch_add(1, Ordering::SeqCst))
}

/// Assert an adapter's `READ_TOOLS`/`WRITE_TOOLS` partition its registered
/// tools exactly — disjoint, and together covering every name. Catches a tool
/// omitted from both lists (the `datadog.logs_export` failure mode).
pub fn assert_read_write_partition(tool_names: Vec<&str>, read: &[&str], write: &[&str]) {
    let classified: HashSet<&str> = read.iter().chain(write).copied().collect();
    assert_eq!(
        classified.len(),
        read.len() + write.len(),
        "READ_TOOLS and WRITE_TOOLS overlap"
    );
    assert_eq!(
        classified,
        tool_names.into_iter().collect::<HashSet<&str>>(),
        "every registered tool must be classified read or write"
    );
}

/// Minimal in-memory `SecretStore` seeded from a slice of `(path, value)`
/// pairs. Implements only `get`/`set`/`delete`/`list`; per-user overrides
/// fall through to the trait's `None` default (use a custom impl when a
/// test needs `LayeredUserVault`-style layering).
pub struct InMem(Mutex<HashMap<String, String>>);

impl InMem {
    pub fn from(seed: &[(&str, &str)]) -> Arc<Self> {
        Arc::new(Self(Mutex::new(
            seed.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )))
    }
}

impl SecretStore for InMem {
    fn get(&self, p: &str) -> Option<String> {
        self.0.lock().unwrap().get(p).cloned()
    }
    fn set(&self, p: &str, v: &str) -> eyre::Result<()> {
        self.0.lock().unwrap().insert(p.into(), v.into());
        Ok(())
    }
    fn delete(&self, p: &str) -> eyre::Result<bool> {
        Ok(self.0.lock().unwrap().remove(p).is_some())
    }
    fn list(&self, _: Option<&str>) -> Vec<String> {
        Vec::new()
    }
}

/// In-memory `SecretStore` that simulates `LayeredUserVault`'s
/// user-tier-then-agent-tier behaviour for a single test user.
pub struct LayeredInMem {
    agent: Mutex<HashMap<String, String>>,
    user: Mutex<HashMap<String, String>>,
}

impl LayeredInMem {
    pub fn new(agent: &[(&str, &str)], user: &[(&str, &str)]) -> Arc<Self> {
        let map = |seed: &[(&str, &str)]| {
            Mutex::new(
                seed.iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            )
        };
        Arc::new(Self {
            agent: map(agent),
            user: map(user),
        })
    }
}

impl SecretStore for LayeredInMem {
    fn get(&self, path: &str) -> Option<String> {
        self.get_user_override(path)
            .or_else(|| self.agent.lock().unwrap().get(path).cloned())
    }
    fn set(&self, p: &str, v: &str) -> eyre::Result<()> {
        self.agent.lock().unwrap().insert(p.into(), v.into());
        Ok(())
    }
    fn delete(&self, p: &str) -> eyre::Result<bool> {
        Ok(self.agent.lock().unwrap().remove(p).is_some())
    }
    fn list(&self, _: Option<&str>) -> Vec<String> {
        Vec::new()
    }
    fn get_user_override(&self, path: &str) -> Option<String> {
        self.user.lock().unwrap().get(path).cloned()
    }
    fn inspect(&self, path: &str) -> Vec<(VaultSource, String)> {
        [
            (VaultSource::AgentPerUser, &self.user),
            (VaultSource::AgentSpecific, &self.agent),
        ]
        .into_iter()
        .filter_map(|(tier, map)| map.lock().unwrap().get(path).map(|v| (tier, v.clone())))
        .collect()
    }
}

/// One agent shared by several people, as a hub's `LayeredUserVault` is:
/// shared keys (`AgentSpecific`) plus each person's own (`AgentPerUser`),
/// the latter visible only while that person is `CURRENT_USER_ID`.
#[derive(Default)]
pub struct People {
    shared: Mutex<HashMap<String, String>>,
    own: Mutex<HashMap<String, HashMap<String, String>>>,
}

impl People {
    pub fn new(shared: &[(&str, &str)]) -> Arc<Self> {
        let people = Self::default();
        for (k, v) in shared {
            people
                .shared
                .lock()
                .unwrap()
                .insert(k.to_string(), v.to_string());
        }
        Arc::new(people)
    }

    /// Give `person` their own `path`.
    pub fn own(&self, person: &str, path: &str, value: &str) -> &Self {
        self.own
            .lock()
            .unwrap()
            .entry(person.to_string())
            .or_default()
            .insert(path.to_string(), value.to_string());
        self
    }

    fn mine(&self) -> HashMap<String, String> {
        workforce_agent::runner::CURRENT_USER_ID
            .try_with(|u| u.clone())
            .ok()
            .and_then(|u| self.own.lock().unwrap().get(&u).cloned())
            .unwrap_or_default()
    }
}

impl SecretStore for People {
    fn get(&self, path: &str) -> Option<String> {
        self.get_user_override(path)
            .or_else(|| self.shared.lock().unwrap().get(path).cloned())
    }
    fn set(&self, p: &str, v: &str) -> eyre::Result<()> {
        self.shared.lock().unwrap().insert(p.into(), v.into());
        Ok(())
    }
    fn delete(&self, p: &str) -> eyre::Result<bool> {
        Ok(self.shared.lock().unwrap().remove(p).is_some())
    }
    fn list(&self, prefix: Option<&str>) -> Vec<String> {
        let p = prefix.unwrap_or("");
        let mut paths: Vec<String> = self
            .mine()
            .into_keys()
            .chain(self.shared.lock().unwrap().keys().cloned())
            .filter(|k| k.starts_with(p))
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }
    fn get_user_override(&self, path: &str) -> Option<String> {
        self.mine().get(path).cloned()
    }
    fn inspect(&self, path: &str) -> Vec<(VaultSource, String)> {
        let mut tiers = Vec::new();
        if let Some(v) = self.mine().get(path) {
            tiers.push((VaultSource::AgentPerUser, v.clone()));
        }
        if let Some(v) = self.shared.lock().unwrap().get(path) {
            tiers.push((VaultSource::AgentSpecific, v.clone()));
        }
        tiers
    }
}

/// A synthetic `reqwest::Response` for exercising status-mapping paths
/// without HTTP.
#[cfg(feature = "reqwest")]
pub(crate) fn canned_response(
    status: u16,
    retry_after: Option<&str>,
    body: &'static str,
) -> reqwest::Response {
    let mut builder = http::Response::builder().status(status);
    if let Some(secs) = retry_after {
        builder = builder.header("Retry-After", secs);
    }
    reqwest::Response::from(builder.body(body).unwrap())
}

/// What the code under test logs, at every level, while its guard is held.
#[derive(Clone, Default)]
pub struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Logs;
    fn make_writer(&'a self) -> Logs {
        self.clone()
    }
}

impl Logs {
    pub fn capture() -> (Self, tracing::subscriber::DefaultGuard) {
        let logs = Self::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        (logs, tracing::subscriber::set_default(subscriber))
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}
