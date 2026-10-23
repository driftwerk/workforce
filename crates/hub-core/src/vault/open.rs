//! Opening vaults from config: the backend `[vault] backend` selects, and
//! one shared `Arc<Vault>` per vault per process.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Weak};

use eyre::{bail, Result, WrapErr};
use parking_lot::Mutex;
use workforce_common::settings::{AgentVaultConfig, WorkforceConfig};

use super::file::audit_log_path;
use super::targets::{
    agent_target, canonical_file, configured_agent_target, global_target, VaultTarget,
};
use super::{FileVault, Vault, VaultError};
use crate::hub_core::vault_audit::VaultAuditLog;

#[cfg(feature = "postgres")]
use {
    super::postgres::{build_pool, PgSettings, PgStore},
    eyre::eyre,
    std::time::Duration,
    tokio::sync::OnceCell,
    workforce_common::backend::connect_with_retry,
};

enum BackendKind {
    File,
    Postgres,
}

fn backend_kind(config: &WorkforceConfig) -> Result<BackendKind> {
    match config.vault.backend.as_str() {
        "" | "file" => Ok(BackendKind::File),
        "postgres" => Ok(BackendKind::Postgres),
        other => bail!("unknown [vault] backend `{other}` — expected \"file\" or \"postgres\""),
    }
}

/// Open the hub-level global vault — the single shared vault whose
/// `users/{uid}/...` entries `LayeredUserVault` layers under each per-agent
/// vault (tiers 2/4). `Ok(None)` when the vault is disabled. The file backend
/// also returns `Ok(None)` (with a warning) when the file can't be opened, so
/// callers fall back to the per-agent vault alone; Postgres fails closed.
pub async fn open_global(config: &WorkforceConfig) -> Result<Option<Arc<Vault>>> {
    let Some(target) = global_target(config) else {
        return Ok(None);
    };
    match backend_kind(config)? {
        BackendKind::File => Ok(open_global_file(&target)),
        BackendKind::Postgres => open_postgres_target(config, &target).await.map(Some),
    }
}

/// Open an agent's own vault; `Ok(None)` when it is disabled — or, on
/// Postgres, when the agent's file was never imported: that agent then runs
/// without its own secrets instead of stopping the whole hub.
pub async fn open_agent(
    config: &WorkforceConfig,
    agent_id: &str,
    agent_vault: Option<&AgentVaultConfig>,
    workspace: &Path,
) -> Result<Option<Arc<Vault>>> {
    match agent_target(config, agent_id, agent_vault, workspace) {
        Some(target) => open_agent_target(config, agent_id, &target).await,
        None => Ok(None),
    }
}

/// Open a configured agent's own vault by id (the CLI's `--agent`).
pub async fn open_configured_agent(
    config: &WorkforceConfig,
    agent_id: &str,
) -> Result<Option<Arc<Vault>>> {
    match configured_agent_target(config, agent_id)? {
        Some(target) => open_agent_target(config, agent_id, &target).await,
        None => Ok(None),
    }
}

/// The global vault and, when `agent_id` names one, that agent's own, as a
/// command run beside a live hub reads them: what is there, and nothing
/// else. No vault file, key file, audit log, schema, keyring or scope is
/// created, nothing is migrated or pruned, and on Postgres every
/// transaction is read-only. A vault that does not exist yet is `None`.
/// The vaults are this caller's own copies as of now: a write through one
/// is refused by Postgres, and must never be made to a file.
pub async fn open_readers(
    config: &WorkforceConfig,
    agent_id: Option<&str>,
) -> Result<(Option<Arc<Vault>>, Option<Arc<Vault>>)> {
    let global = global_target(config);
    let agent = match agent_id {
        Some(agent_id) => configured_agent_target(config, agent_id)?,
        None => None,
    };
    match backend_kind(config)? {
        BackendKind::File => Ok((
            agent.as_ref().map(read_file).transpose()?.flatten(),
            global.as_ref().map(read_file).transpose()?.flatten(),
        )),
        BackendKind::Postgres => read_postgres(config, agent.as_ref(), global.as_ref()).await,
    }
}

/// `target`'s file as it stands, when it and its key exist.
fn read_file(target: &VaultTarget) -> Result<Option<Arc<Vault>>> {
    if !target.file.is_file() {
        return Ok(None);
    }
    let Some(key) = Vault::read_master_key(target.key_file.as_deref(), &target.data_dir)? else {
        bail!(
            "the vault file {} has no key: set WORKFORCE_VAULT_KEY or [vault] key_file",
            target.file.display()
        );
    };
    let file = FileVault::open(&target.file, &key)?;
    Ok(Some(Arc::new(Vault::from(file))))
}

#[cfg(feature = "postgres")]
async fn read_postgres(
    config: &WorkforceConfig,
    agent: Option<&VaultTarget>,
    global: Option<&VaultTarget>,
) -> Result<(Option<Arc<Vault>>, Option<Arc<Vault>>)> {
    if agent.is_none() && global.is_none() {
        return Ok((None, None));
    }
    let pg = config
        .postgres
        .as_ref()
        .filter(|pg| pg.enabled)
        .ok_or_else(|| {
            eyre!("[vault] backend = \"postgres\" requires [postgres] enabled = true")
        })?;
    let passphrase = Vault::read_master_key(
        config.vault.key_file.as_deref().map(Path::new),
        Path::new(&config.hub.data_dir),
    )?
    .ok_or_else(|| {
        eyre!(
            "[vault] backend = \"postgres\" needs the vault passphrase: set \
             WORKFORCE_VAULT_KEY or [vault] key_file"
        )
    })?;
    let pool = super::postgres::build_read_only_pool(pg)?;
    let settings = PgSettings::from_config(&config.vault);
    let store = PgStore::connect_read_only(pool, passphrase, settings).await?;
    let mut opened = Vec::new();
    for target in [agent, global] {
        opened.push(match target {
            Some(target) if store.scope_initialised(&target.scope).await? => {
                Some(store.vault(&target.scope).await?)
            }
            _ => None,
        });
    }
    let global = opened.pop().flatten();
    Ok((opened.pop().flatten(), global))
}

#[cfg(not(feature = "postgres"))]
async fn read_postgres(
    _config: &WorkforceConfig,
    _agent: Option<&VaultTarget>,
    _global: Option<&VaultTarget>,
) -> Result<(Option<Arc<Vault>>, Option<Arc<Vault>>)> {
    bail!("[vault] backend = \"postgres\" but workforce-hub was built without the postgres feature")
}

/// Whether `[vault] backend` selects Postgres.
#[cfg(feature = "postgres")]
pub fn uses_postgres(config: &WorkforceConfig) -> Result<bool> {
    Ok(matches!(backend_kind(config)?, BackendKind::Postgres))
}

async fn open_agent_target(
    config: &WorkforceConfig,
    agent_id: &str,
    target: &VaultTarget,
) -> Result<Option<Arc<Vault>>> {
    match backend_kind(config)? {
        BackendKind::File => open_file(target).map(Some),
        BackendKind::Postgres => {
            skip_unmigrated(agent_id, open_postgres_target(config, target).await)
        }
    }
}

fn skip_unmigrated(agent_id: &str, opened: Result<Arc<Vault>>) -> Result<Option<Arc<Vault>>> {
    match opened {
        Err(e)
            if matches!(
                e.downcast_ref::<VaultError>(),
                Some(VaultError::Unmigrated { .. })
            ) =>
        {
            tracing::error!(
                agent = agent_id,
                error = %e,
                "Agent vault not opened; the agent runs without its own secrets"
            );
            Ok(None)
        }
        opened => opened.map(Some),
    }
}

fn open_global_file(target: &VaultTarget) -> Option<Arc<Vault>> {
    // A missing file is correct on first boot, but a silent trap for a
    // subprocess that resolved `data_dir` against the wrong CWD (see
    // `WorkforceConfig::anchor_relative_paths`): flag the create loudly.
    if !target.file.is_file() {
        tracing::warn!(
            vault_path = %target.file.display(),
            cwd = ?std::env::current_dir().ok(),
            "Global vault file not found — creating a NEW empty vault here. \
             If this is not first boot, data_dir likely resolved against the wrong \
             directory and per-user credentials will be missing."
        );
    }
    open_file(target)
        .inspect_err(|e| tracing::warn!(error = %e, "Global vault unavailable"))
        .ok()
}

static FILE_VAULTS: LazyLock<Mutex<HashMap<PathBuf, Weak<Vault>>>> =
    LazyLock::new(Default::default);

/// The live vault for `target`'s file, or a fresh one registered for every
/// later opener in this process. A file opened twice would be two in-memory
/// copies each rewriting the whole file over the other's writes.
fn open_file(target: &VaultTarget) -> Result<Arc<Vault>> {
    let identity = file_identity(&target.file)?;
    let mut live = FILE_VAULTS.lock();
    live.retain(|_, vault| vault.strong_count() > 0);
    if let Some(vault) = live.get(&identity).and_then(Weak::upgrade) {
        return Ok(vault);
    }
    let file = FileVault::open(&target.file, &target.master_key()?)?;
    let vault = Arc::new(Vault::from(with_audit_log(file)));
    live.insert(identity, Arc::downgrade(&vault));
    Ok(vault)
}

fn file_identity(file: &Path) -> Result<PathBuf> {
    if let Some(dir) = file.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .wrap_err_with(|| format!("Failed to create vault directory {}", dir.display()))?;
    }
    Ok(canonical_file(file))
}

fn with_audit_log(file: FileVault) -> FileVault {
    let path = audit_log_path(file.path());
    match VaultAuditLog::open(path.clone()) {
        Ok(log) => file.with_audit_log(Arc::new(log)),
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "Failed to open vault audit log; continuing without audit"
            );
            file
        }
    }
}

#[cfg(feature = "postgres")]
async fn open_postgres_target(
    config: &WorkforceConfig,
    target: &VaultTarget,
) -> Result<Arc<Vault>> {
    open_postgres(&postgres_store(config).await?, target).await
}

#[cfg(not(feature = "postgres"))]
async fn open_postgres_target(
    _config: &WorkforceConfig,
    _target: &VaultTarget,
) -> Result<Arc<Vault>> {
    bail!("[vault] backend = \"postgres\" but workforce-hub was built without the postgres feature")
}

#[cfg(feature = "postgres")]
static PG_STORE: OnceCell<Arc<PgStore>> = OnceCell::const_new();

/// The process-wide Postgres vault store, connected on first use. Fails
/// closed: it never falls back to files and never generates a passphrase.
#[cfg(feature = "postgres")]
pub(super) async fn postgres_store(config: &WorkforceConfig) -> Result<Arc<PgStore>> {
    PG_STORE
        .get_or_try_init(|| async {
            let pg = config
                .postgres
                .as_ref()
                .filter(|pg| pg.enabled)
                .ok_or_else(|| {
                    eyre!("[vault] backend = \"postgres\" requires [postgres] enabled = true")
                })?;
            let passphrase = Vault::read_master_key(
                config.vault.key_file.as_deref().map(Path::new),
                Path::new(&config.hub.data_dir),
            )?
            .ok_or_else(|| {
                eyre!(
                    "[vault] backend = \"postgres\" needs the vault passphrase: set \
                     WORKFORCE_VAULT_KEY or [vault] key_file (postgres mode never generates one)"
                )
            })?;
            let pool = connect_with_retry(
                "vault-postgres",
                Duration::from_secs(pg.connect_timeout_secs),
                Duration::from_secs(5),
                || async {
                    let pool = build_pool(pg)?;
                    pool.ping().await?;
                    Ok(pool)
                },
            )
            .await?;
            PgStore::connect(pool, passphrase, PgSettings::from_config(&config.vault)).await
        })
        .await
        .cloned()
}

/// A scope's vault. Refuses a scope whose legacy file exists on this host but
/// was never imported: opening it empty would silently hide those secrets.
#[cfg(feature = "postgres")]
async fn open_postgres(store: &Arc<PgStore>, target: &VaultTarget) -> Result<Arc<Vault>> {
    if !store.scope_initialised(&target.scope).await? {
        if target.file.exists() {
            return Err(VaultError::Unmigrated {
                scope: target.scope.clone(),
                file: target.file.clone(),
            }
            .into());
        }
        tracing::warn!(
            scope = %target.scope,
            file = %target.file.display(),
            "Creating an empty Postgres vault: no vault file exists here to import. If this \
             deployment keeps vault files on another host, run `workforce vault migrate` there \
             before anything writes to this vault"
        );
        store.init_scope(&target.scope).await?;
    }
    store.vault(&target.scope).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(data_dir: &Path, vault_enabled: bool) -> WorkforceConfig {
        let mut config = WorkforceConfig::default();
        config.hub.data_dir = data_dir.to_string_lossy().into_owned();
        config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        config.vault.enabled = vault_enabled;
        config
    }

    #[tokio::test]
    async fn open_global_returns_none_when_vault_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(open_global(&config_with(tmp.path(), false))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn open_global_resolves_a_per_user_secret_from_the_hub_vault() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path();
        let key = Vault::resolve_master_key(None, data_dir).unwrap();
        Vault::open(&data_dir.join("vault.enc"), &key)
            .unwrap()
            .set("users/u1/skills/jira/api_token", "tok-123")
            .unwrap();

        let opened = open_global(&config_with(data_dir, true))
            .await
            .unwrap()
            .expect("global vault should open");
        assert_eq!(
            opened.get("users/u1/skills/jira/api_token").as_deref(),
            Some("tok-123")
        );
    }

    #[tokio::test]
    async fn a_reader_creates_no_file_key_or_log_and_reads_what_a_hub_wrote() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with(tmp.path(), true);
        let files = || std::fs::read_dir(tmp.path()).unwrap().count();

        let (agent, global) = open_readers(&config, Some("default")).await.unwrap();
        assert!(agent.is_none() && global.is_none());
        assert_eq!(files(), 0, "nothing there, nothing created");

        let key = Vault::resolve_master_key(None, tmp.path()).unwrap();
        Vault::open(&tmp.path().join("vault.enc"), &key)
            .unwrap()
            .set("llm/kev", "k-1")
            .unwrap();
        let before = files();
        let (agent, global) = open_readers(&config, Some("default")).await.unwrap();
        assert!(agent.is_none(), "the agent has no vault file yet");
        assert_eq!(global.unwrap().get("llm/kev").as_deref(), Some("k-1"));
        assert_eq!(files(), before, "no audit log, no agent directory");
        assert!(open_readers(&config, Some("nobody")).await.is_err());
    }

    #[tokio::test]
    async fn unknown_backend_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = config_with(tmp.path(), true);
        config.vault.backend = "mystery".into();
        assert!(open_global(&config).await.is_err());
        assert!(open_agent(&config, "a", None, tmp.path()).await.is_err());
    }

    #[tokio::test]
    async fn postgres_without_a_postgres_section_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = config_with(tmp.path(), true);
        config.vault.backend = "postgres".into();
        assert!(open_global(&config).await.is_err());
    }

    #[tokio::test]
    async fn one_file_is_one_vault_per_process() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with(tmp.path(), true);
        let workspace = tmp.path().join("agents/a");

        let first = open_agent(&config, "a", None, &workspace)
            .await
            .unwrap()
            .unwrap();
        let second = open_agent(&config, "a", None, &workspace)
            .await
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&first, &second));

        first.set("k", "v").unwrap();
        drop((first, second));
        let reopened = open_agent(&config, "a", None, &workspace)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reopened.get("k").as_deref(), Some("v"));
    }

    #[tokio::test]
    async fn agent_on_the_global_file_gets_the_global_vault() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with(tmp.path(), true);
        let agent = AgentVaultConfig {
            path: Some(tmp.path().join("vault.enc").to_string_lossy().into_owned()),
            ..AgentVaultConfig::default()
        };

        let global = open_global(&config).await.unwrap().unwrap();
        let aliased = open_agent(&config, "a", Some(&agent), tmp.path())
            .await
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&global, &aliased));
    }

    #[tokio::test]
    async fn every_file_vault_gets_an_audit_log() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with(tmp.path(), true);
        open_global(&config)
            .await
            .unwrap()
            .unwrap()
            .set("k", "v")
            .unwrap();
        assert!(tmp.path().join("vault-audit.jsonl").is_file());
    }

    #[tokio::test]
    async fn the_implicit_default_agent_opens_by_id() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with(tmp.path(), true);
        open_configured_agent(&config, "default")
            .await
            .unwrap()
            .unwrap()
            .set("k", "v")
            .unwrap();
        assert!(tmp.path().join("agents/default/vault.enc").is_file());
        assert!(open_configured_agent(&config, "nobody").await.is_err());
    }

    #[test]
    fn only_an_unmigrated_agent_vault_is_skipped() {
        let unmigrated = VaultError::Unmigrated {
            scope: "agent:a".into(),
            file: "/data/agents/a/vault.enc".into(),
        };
        assert!(skip_unmigrated("a", Err(unmigrated.into()))
            .unwrap()
            .is_none());
        assert!(skip_unmigrated("a", Err(eyre::eyre!("database unreachable"))).is_err());
    }

    #[cfg(feature = "postgres")]
    mod postgres_backend {
        use super::*;
        use workforce_postgres::test_support::with_test_pool;

        fn target(dir: &Path, scope: &str) -> VaultTarget {
            VaultTarget {
                scope: scope.into(),
                file: dir.join("vault.enc"),
                key_file: None,
                data_dir: dir.to_path_buf(),
            }
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_file_that_was_never_imported_blocks_its_scope() {
            with_test_pool(build_pool, |pool| async move {
                let store = PgStore::connect(
                    pool,
                    "passphrase".into(),
                    PgSettings {
                        sync_interval: Duration::from_millis(50),
                        audit_retention_days: 30,
                    },
                )
                .await
                .unwrap();
                let tmp = tempfile::tempdir().unwrap();
                let target = target(tmp.path(), "agent:a");
                std::fs::write(&target.file, b"legacy").unwrap();

                let err = open_postgres(&store, &target).await.err().unwrap();
                assert!(matches!(
                    err.downcast_ref::<VaultError>(),
                    Some(VaultError::Unmigrated { .. })
                ));

                std::fs::remove_file(&target.file).unwrap();
                let vault = open_postgres(&store, &target).await.unwrap();
                vault.set("k", "v").unwrap();
                std::fs::write(&target.file, b"stale copy").unwrap();
                let reopened = open_postgres(&store, &target).await.unwrap();
                assert!(Arc::ptr_eq(&vault, &reopened));
                assert_eq!(reopened.get("k").as_deref(), Some("v"));
            })
            .await;
        }
    }
}
