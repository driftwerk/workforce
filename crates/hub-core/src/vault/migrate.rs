//! Moving vaults between the file and Postgres backends
//!: `migrate` imports every vault
//! file behind a write freeze, `export` writes Postgres back to the files
//! (rollback), `verify` compares both sides, `audit` reads either backend's
//! history, and `rotate_key` / `retire_key` manage the Postgres key.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use eyre::{bail, eyre, Result, WrapErr};
use sha2::{Digest, Sha256};
use workforce_common::settings::{AgentVaultConfig, VaultEntry, WorkforceConfig};
use workforce_common::subscription::SubscriptionKind;

use super::file::{audit_log_path, frozen_marker, load, write_atomic, Sealer};
use super::open::{postgres_store, uses_postgres};
use super::postgres::{Import, PgStore};
use super::targets::{
    agent_target, canonical_file, configured_agent_target, global_target, VaultTarget,
    DEFAULT_AGENT,
};
use super::Vault;
use crate::hub_core::vault_audit::{AuditEntry, VaultAuditLog};

pub use super::postgres::{RotationSummary, ScopeState};

const IMPORT_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, Copy)]
pub struct MigrateOptions {
    /// Decrypt and count everything, change nothing.
    pub dry_run: bool,
    /// Wait after freezing so writes already in flight land first.
    pub settle: Duration,
}

/// What happened to one vault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Imported {
        entries: usize,
        audit: usize,
    },
    WouldImport {
        entries: usize,
        audit: usize,
    },
    AlreadyOnPostgres,
    NoFile,
    Exported {
        entries: usize,
        audit: usize,
    },
    /// Not imported: a hub created this vault empty in Postgres and has
    /// stored secrets in it since, which an import would replace.
    Diverged {
        entries: usize,
    },
}

#[derive(Debug, Clone)]
pub struct ScopeReport {
    pub scope: String,
    pub file: PathBuf,
    pub outcome: Outcome,
}

#[derive(Debug, Clone)]
pub struct MigrationReport {
    pub scopes: Vec<ScopeReport>,
    /// Files several agents shared; each of those agents now has its own copy.
    pub shared_files: Vec<(PathBuf, Vec<String>)>,
}

#[derive(Debug, Clone)]
pub struct ExportReport {
    pub scopes: Vec<ScopeReport>,
    /// Postgres scopes no configured vault uses; they stay in Postgres.
    pub unclaimed: Vec<String>,
}

/// Paths whose entries differ between a file and its Postgres copy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Differences {
    pub only_in_file: Vec<String>,
    pub only_in_postgres: Vec<String>,
    pub changed: Vec<String>,
}

/// One vault as `workforce vault verify` sees it.
#[derive(Debug, Clone)]
pub struct Check {
    pub scope: String,
    pub file: PathBuf,
    pub file_exists: bool,
    pub file_frozen: bool,
    /// A frozen file whose bytes differ from the ones imported: a write the
    /// freeze missed.
    pub changed_since_import: bool,
    /// The file exists but was never imported: Postgres lacks the vault (hubs
    /// refuse it) or a hub started it empty.
    pub unmigrated: bool,
    pub postgres: Option<ScopeState>,
    pub differences: Option<Differences>,
}

#[derive(Debug, Clone, Copy)]
pub struct AuditQuery<'a> {
    /// An agent's own vault; `None` for the global vault.
    pub agent: Option<&'a str>,
    pub since: Option<DateTime<Utc>>,
    pub limit: usize,
}

enum Step {
    Done(Outcome),
    Import { since: Option<DateTime<Utc>> },
}

/// Import every vault file into Postgres. Files are frozen first — hubs on
/// the file backend then refuse writes — and after `settle` each is imported
/// in a transaction that commits only if the file is still unchanged. Every
/// file is decrypted before anything is frozen, and re-running skips the
/// vaults already on Postgres.
pub async fn migrate(config: &WorkforceConfig, options: MigrateOptions) -> Result<MigrationReport> {
    migrate_with(&*postgres_store(config).await?, config, options).await
}

/// Write every Postgres vault back to its file and unfreeze the files
/// (rollback). The Postgres scopes are frozen first, so hubs still on
/// Postgres refuse writes; audit records written in Postgres are appended to
/// each file's log.
pub async fn export(config: &WorkforceConfig, settle: Duration) -> Result<ExportReport> {
    export_with(&*postgres_store(config).await?, config, settle).await
}

/// Compare every vault's file with its Postgres copy — paths only, never
/// values.
pub async fn verify(config: &WorkforceConfig) -> Result<Vec<Check>> {
    verify_with(&*postgres_store(config).await?, config).await
}

/// Audit records for the global vault or one agent's, newest first, from
/// whichever backend `[vault] backend` selects.
pub async fn audit(
    config: &WorkforceConfig,
    query: AuditQuery<'_>,
) -> Result<Vec<(String, AuditEntry<'static>)>> {
    let target = match query.agent {
        Some(agent_id) => configured_agent_target(config, agent_id)?
            .ok_or_else(|| eyre!("agent `{agent_id}` has no vault"))?,
        None => global_target(config).ok_or_else(|| eyre!("the vault is disabled"))?,
    };
    if uses_postgres(config)? {
        return postgres_store(config)
            .await?
            .audit_records(
                Some(&target.scope),
                query.since,
                i64::try_from(query.limit)?,
            )
            .await;
    }
    Ok(VaultAuditLog::read(&audit_log_path(&target.file))?
        .into_iter()
        .rev()
        .filter(|record| query.since.is_none_or(|since| record.ts >= since))
        .take(query.limit)
        .map(|record| (target.scope.clone(), record))
        .collect())
}

/// Seal every Postgres secret under a new key, optionally moving to a new
/// passphrase (the old one keeps working until `retire_key`).
pub async fn rotate_key(
    config: &WorkforceConfig,
    new_passphrase: Option<String>,
) -> Result<RotationSummary> {
    postgres_only(config, "rotate-key")?;
    postgres_store(config).await?.rotate(new_passphrase).await
}

/// Stop every passphrase but the configured one opening the Postgres vault.
pub async fn retire_key(config: &WorkforceConfig) -> Result<usize> {
    postgres_only(config, "retire-key")?;
    postgres_store(config).await?.retire_foreign_wraps().await
}

/// The key commands manage the Postgres keyring, so they refuse a config
/// whose vaults are files.
fn postgres_only(config: &WorkforceConfig, command: &str) -> Result<()> {
    if uses_postgres(config)? {
        return Ok(());
    }
    bail!(
        "`workforce vault {command} --config` manages the Postgres vault key, but this config \
         keeps its vaults in files ([vault] backend = \"file\"); a vault file's key is rotated \
         with `workforce vault rotate-key --data-dir`"
    )
}

async fn migrate_with(
    store: &PgStore,
    config: &WorkforceConfig,
    options: MigrateOptions,
) -> Result<MigrationReport> {
    let targets = all_targets(config)?;
    let states = store.scope_states().await?;
    let steps: Vec<(&VaultTarget, Step)> = distinct_scopes(&targets)
        .into_iter()
        .map(|target| (target, step(target, states.get(&target.scope))))
        .collect();
    let previews: HashMap<&str, Outcome> = steps
        .iter()
        .filter_map(|(target, step)| match step {
            Step::Import { since } => Some((*target, *since)),
            Step::Done(_) => None,
        })
        .map(|(target, since)| Ok((target.scope.as_str(), preview(target, since)?)))
        .collect::<Result<_>>()?;
    refuse_shared_sign_ins(
        &targets,
        steps
            .iter()
            .filter(|(_, step)| matches!(step, Step::Import { .. }))
            .map(|(target, _)| *target),
    )?;

    if !options.dry_run && !previews.is_empty() {
        steps
            .iter()
            .filter(|(_, step)| matches!(step, Step::Import { .. }))
            .try_for_each(|(target, _)| freeze(&target.file))?;
        tokio::time::sleep(options.settle).await;
    }

    let mut scopes = Vec::with_capacity(steps.len());
    for (target, step) in &steps {
        let outcome = match step {
            Step::Done(outcome) => outcome.clone(),
            Step::Import { .. } if options.dry_run => previews[target.scope.as_str()].clone(),
            Step::Import { since } => import(store, target, *since, options.settle).await?,
        };
        scopes.push(ScopeReport {
            scope: target.scope.clone(),
            file: target.file.clone(),
            outcome,
        });
    }
    Ok(MigrationReport {
        scopes,
        shared_files: shared_files(&targets),
    })
}

/// A scope hubs write is left alone unless a hub created it empty and it is
/// still empty; a missing or frozen scope with a file is (re)imported.
fn step(target: &VaultTarget, state: Option<&ScopeState>) -> Step {
    let has_file = target.file.is_file();
    match state {
        Some(state) if state.writable() && (state.import_sha256.is_some() || !has_file) => {
            Step::Done(Outcome::AlreadyOnPostgres)
        }
        Some(state) if state.writable() && state.entries > 0 => Step::Done(Outcome::Diverged {
            entries: state.entries,
        }),
        _ if !has_file => Step::Done(Outcome::NoFile),
        state => Step::Import {
            since: state.and_then(|state| state.frozen_at),
        },
    }
}

fn preview(target: &VaultTarget, since: Option<DateTime<Utc>>) -> Result<Outcome> {
    let (entries, _) = read_snapshot(target)?;
    Ok(Outcome::WouldImport {
        entries: entries.len(),
        audit: audit_since(&target.file, since)?.len(),
    })
}

async fn import(
    store: &PgStore,
    target: &VaultTarget,
    since: Option<DateTime<Utc>>,
    settle: Duration,
) -> Result<Outcome> {
    for _ in 0..IMPORT_ATTEMPTS {
        let (entries, sha256) = read_snapshot(target)?;
        let audit = audit_since(&target.file, since)?;
        let unchanged = || Ok(file_sha256(&target.file)? == sha256);
        match store
            .import_scope(&target.scope, &entries, &audit, &sha256, unchanged)
            .await?
        {
            Import::Imported => {
                return Ok(Outcome::Imported {
                    entries: entries.len(),
                    audit: audit.len(),
                })
            }
            Import::OnPostgres => return Ok(Outcome::AlreadyOnPostgres),
            Import::Diverged { entries } => return Ok(Outcome::Diverged { entries }),
            Import::FileChanged => tokio::time::sleep(settle).await,
        }
    }
    bail!(
        "{} kept changing during the import — a hub without the write freeze may still be \
         running: deploy the current image everywhere, then run the migration again",
        target.file.display()
    )
}

async fn export_with(
    store: &PgStore,
    config: &WorkforceConfig,
    settle: Duration,
) -> Result<ExportReport> {
    let states = store.scope_states().await?;
    let targets: Vec<VaultTarget> = all_targets(config)?
        .into_iter()
        .filter(|target| states.contains_key(&target.scope))
        .collect();
    if let Some((file, scopes)) = shared_files(&targets).into_iter().next() {
        bail!(
            "{} holds the vaults of {} — give each agent its own vault path, then export again",
            file.display(),
            scopes.join(" and ")
        );
    }
    let exporting = distinct_scopes(&targets);
    let scopes: Vec<String> = exporting
        .iter()
        .map(|target| target.scope.clone())
        .collect();
    store.freeze_scopes(&scopes).await?;
    tokio::time::sleep(settle).await;

    let mut exported = Vec::with_capacity(exporting.len());
    for target in exporting {
        let (entries, audit) = store.export_scope(&target.scope).await?;
        write_atomic(
            &target.file,
            &Sealer::generate(&existing_master_key(target)?)?.seal(&entries)?,
        )?;
        let appended = append_audit(&target.file, audit)?;
        thaw(&target.file)?;
        exported.push(ScopeReport {
            scope: target.scope.clone(),
            file: target.file.clone(),
            outcome: Outcome::Exported {
                entries: entries.len(),
                audit: appended,
            },
        });
    }
    Ok(ExportReport {
        scopes: exported,
        unclaimed: states
            .into_keys()
            .filter(|scope| !scopes.contains(scope))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
    })
}

async fn verify_with(store: &PgStore, config: &WorkforceConfig) -> Result<Vec<Check>> {
    let states = store.scope_states().await?;
    let targets = all_targets(config)?;
    let mut checks = Vec::new();
    for target in distinct_scopes(&targets) {
        let state = states.get(&target.scope).cloned();
        let imported = state.as_ref().filter(|state| state.imported_at.is_some());
        let file = target
            .file
            .is_file()
            .then(|| read_snapshot(target))
            .transpose()?;
        let file_frozen = frozen_marker(&target.file).exists();
        let changed_since_import = file_frozen
            && matches!(
                (&file, imported),
                (Some((_, sha256)), Some(ScopeState { import_sha256: Some(at_import), .. }))
                    if sha256 != at_import
            );
        let differences = match (&file, imported) {
            (Some((entries, _)), Some(_)) => {
                Some(diff(entries, &store.scope_entries(&target.scope).await?))
            }
            _ => None,
        };
        checks.push(Check {
            scope: target.scope.clone(),
            file: target.file.clone(),
            file_exists: file.is_some(),
            file_frozen,
            changed_since_import,
            unmigrated: file.is_some()
                && state
                    .as_ref()
                    .is_none_or(|state| state.import_sha256.is_none()),
            postgres: state,
            differences,
        });
    }
    Ok(checks)
}

/// Every vault this deployment holds: the global vault, each configured
/// agent's (the implicit `default` agent's when none are configured), and any
/// agent vault under `{data_dir}/agents` whose agent is no longer configured,
/// so no secret is stranded.
fn all_targets(config: &WorkforceConfig) -> Result<Vec<VaultTarget>> {
    let agent_ids: Vec<&str> = if config.agents.is_empty() {
        vec![DEFAULT_AGENT]
    } else {
        config
            .agents
            .iter()
            .map(|agent| agent.id.as_str())
            .collect()
    };
    let agents = agent_ids
        .into_iter()
        .map(|agent_id| configured_agent_target(config, agent_id))
        .collect::<Result<Vec<_>>>()?;
    let known: Vec<VaultTarget> = global_target(config)
        .into_iter()
        .chain(agents.into_iter().flatten())
        .collect();
    let claimed: HashSet<PathBuf> = known
        .iter()
        .map(|target| canonical_file(&target.file))
        .collect();
    Ok(known
        .into_iter()
        .chain(leftover_agent_targets(config, &claimed))
        .collect())
}

fn leftover_agent_targets(
    config: &WorkforceConfig,
    claimed: &HashSet<PathBuf>,
) -> Vec<VaultTarget> {
    let enabled = AgentVaultConfig {
        enabled: Some(true),
        ..AgentVaultConfig::default()
    };
    let mut leftovers: Vec<VaultTarget> =
        std::fs::read_dir(Path::new(&config.hub.data_dir).join("agents"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let agent_id = entry.file_name().into_string().ok()?;
                agent_target(config, &agent_id, Some(&enabled), &entry.path())
            })
            .filter(|target| {
                target.file.is_file() && !claimed.contains(&canonical_file(&target.file))
            })
            .collect();
    leftovers.sort_by(|a, b| a.scope.cmp(&b.scope));
    leftovers
}

/// One target per scope — an agent on the global file is the global vault.
fn distinct_scopes(targets: &[VaultTarget]) -> Vec<&VaultTarget> {
    targets.iter().fold(Vec::new(), |mut distinct, target| {
        if !distinct
            .iter()
            .any(|seen: &&VaultTarget| seen.scope == target.scope)
        {
            distinct.push(target);
        }
        distinct
    })
}

/// Files that more than one scope reads.
fn shared_files(targets: &[VaultTarget]) -> Vec<(PathBuf, Vec<String>)> {
    targets
        .iter()
        .fold(
            BTreeMap::<PathBuf, Vec<String>>::new(),
            |mut by_file, target| {
                let scopes = by_file.entry(canonical_file(&target.file)).or_default();
                if !scopes.contains(&target.scope) {
                    scopes.push(target.scope.clone());
                }
                by_file
            },
        )
        .into_iter()
        .filter(|(_, scopes)| scopes.len() > 1)
        .collect()
}

/// Refuse to import a file several agents share while it holds a ChatGPT
/// sign-in: each agent's copy would spend the same refresh token, and the
/// second refresh would fail for good.
fn refuse_shared_sign_ins<'a>(
    targets: &[VaultTarget],
    importing: impl Iterator<Item = &'a VaultTarget>,
) -> Result<()> {
    let shared = shared_files(targets);
    importing
        .filter_map(|target| {
            shared
                .iter()
                .find(|(file, _)| *file == canonical_file(&target.file))
                .map(|(file, scopes)| (target, file, scopes))
        })
        .try_for_each(|(target, file, scopes)| {
            let (entries, _) = read_snapshot(target)?;
            let mut sign_ins: Vec<&str> = entries
                .keys()
                .map(String::as_str)
                .filter(|key| SubscriptionKind::of_vault_key(key) == Some(SubscriptionKind::ChatGpt))
                .collect();
            sign_ins.sort_unstable();
            match sign_ins.is_empty() {
                true => Ok(()),
                false => Err(eyre!(
                    "{} holds the vaults of {}, and a ChatGPT sign-in works in one place only: move \
                     {} into one agent's own vault, or delete it, then migrate again",
                    file.display(),
                    scopes.join(" and "),
                    sign_ins.join(", ")
                )),
            }
        })
}

fn existing_master_key(target: &VaultTarget) -> Result<String> {
    Vault::read_master_key(target.key_file.as_deref(), &target.data_dir)?.ok_or_else(|| {
        eyre!(
            "no vault key for {}: set WORKFORCE_VAULT_KEY or its key file",
            target.file.display()
        )
    })
}

fn read_snapshot(target: &VaultTarget) -> Result<(HashMap<String, VaultEntry>, Vec<u8>)> {
    let (entries, _, bytes) = load(&target.file, &existing_master_key(target)?)?;
    Ok((entries, Sha256::digest(&bytes).to_vec()))
}

fn file_sha256(path: &Path) -> Result<Vec<u8>> {
    let bytes = std::fs::read(path)
        .wrap_err_with(|| format!("Failed to read vault at {}", path.display()))?;
    Ok(Sha256::digest(&bytes).to_vec())
}

fn freeze(file: &Path) -> Result<()> {
    let marker =
        serde_json::json!({ "frozen_at": Utc::now(), "reason": "workforce vault migrate" });
    std::fs::write(frozen_marker(file), serde_json::to_vec_pretty(&marker)?)
        .wrap_err_with(|| format!("Failed to freeze {}", file.display()))
}

fn thaw(file: &Path) -> Result<()> {
    match std::fs::remove_file(frozen_marker(file)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).wrap_err_with(|| format!("Failed to unfreeze {}", file.display()))
        }
        _ => Ok(()),
    }
}

/// The file's audit records newer than `since` (all of them when `None`).
fn audit_since(file: &Path, since: Option<DateTime<Utc>>) -> Result<Vec<AuditEntry<'static>>> {
    Ok(VaultAuditLog::read(&audit_log_path(file))?
        .into_iter()
        .filter(|record| since.is_none_or(|since| record.ts > since))
        .collect())
}

/// Append Postgres-era audit records to the file's log, skipping any the log
/// already holds, so exporting twice appends nothing new.
fn append_audit(file: &Path, records: Vec<AuditEntry<'static>>) -> Result<usize> {
    let path = audit_log_path(file);
    let newest = VaultAuditLog::read(&path)?.last().map(|record| record.ts);
    let fresh: Vec<_> = records
        .into_iter()
        .filter(|record| newest.is_none_or(|newest| record.ts > newest))
        .collect();
    let log = VaultAuditLog::open(path)?;
    fresh.iter().for_each(|record| log.record(record.clone()));
    Ok(fresh.len())
}

fn diff(file: &HashMap<String, VaultEntry>, postgres: &HashMap<String, VaultEntry>) -> Differences {
    let sorted = |mut paths: Vec<String>| {
        paths.sort();
        paths
    };
    let same = |a: &VaultEntry, b: &VaultEntry| {
        (
            &a.value,
            a.created_at,
            a.updated_at,
            &a.created_by,
            &a.updated_by,
        ) == (
            &b.value,
            b.created_at,
            b.updated_at,
            &b.created_by,
            &b.updated_by,
        )
    };
    Differences {
        only_in_file: sorted(
            file.keys()
                .filter(|path| !postgres.contains_key(*path))
                .cloned()
                .collect(),
        ),
        only_in_postgres: sorted(
            postgres
                .keys()
                .filter(|path| !file.contains_key(*path))
                .cloned()
                .collect(),
        ),
        changed: sorted(
            file.iter()
                .filter(|(path, entry)| {
                    postgres.get(*path).is_some_and(|other| !same(entry, other))
                })
                .map(|(path, _)| path.clone())
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::postgres::{build_pool, PgSettings};
    use crate::vault::FileVault;
    use std::sync::Arc;
    use workforce_common::settings::{AgentConfig, VaultActor};
    use workforce_postgres::test_support::with_test_pool;
    use workforce_postgres::PostgresPool;

    struct Install {
        dir: tempfile::TempDir,
        config: WorkforceConfig,
        key: String,
    }

    impl Install {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let key_file = dir.path().join("vault-key.txt");
            std::fs::write(&key_file, "migration-test-passphrase").unwrap();
            let mut config = WorkforceConfig::default();
            config.hub.data_dir = dir.path().to_string_lossy().into_owned();
            config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
                .join("memory.db")
                .to_string_lossy()
                .into_owned();
            config.vault.key_file = Some(key_file.to_string_lossy().into_owned());
            let key = Vault::read_master_key(Some(&key_file), dir.path())
                .unwrap()
                .unwrap();
            Self { dir, config, key }
        }

        fn with_agents(mut self, agents: Vec<AgentConfig>) -> Self {
            self.config.agents = agents;
            self
        }

        fn path(&self, relative: &str) -> PathBuf {
            self.dir.path().join(relative)
        }

        /// Write entries through an audited file vault, as a hub would.
        fn seed(&self, relative: &str, entries: &[(&str, &str)]) {
            let file = self.path(relative);
            let log = Arc::new(VaultAuditLog::open(audit_log_path(&file)).unwrap());
            let vault = Vault::from(
                FileVault::open(&file, &self.key)
                    .unwrap()
                    .with_audit_log(log),
            );
            entries.iter().for_each(|(path, value)| {
                vault.set_with_actor(path, value, VaultActor::Cli).unwrap();
            });
        }

        fn file_vault(&self, relative: &str) -> Vault {
            Vault::from(
                FileVault::open(&self.path(relative), &self.key)
                    .unwrap()
                    .with_audit_log(Arc::new(
                        VaultAuditLog::open(audit_log_path(&self.path(relative))).unwrap(),
                    )),
            )
        }

        async fn store(&self, pool: &PostgresPool) -> Arc<PgStore> {
            connect(pool, &self.key).await
        }
    }

    async fn connect(pool: &PostgresPool, passphrase: &str) -> Arc<PgStore> {
        PgStore::connect(pool.clone(), passphrase.into(), settings())
            .await
            .unwrap()
    }

    fn settings() -> PgSettings {
        PgSettings {
            sync_interval: Duration::from_millis(50),
            audit_retention_days: 365,
        }
    }

    fn agent(id: &str, vault_file: Option<&Path>) -> AgentConfig {
        AgentConfig {
            id: id.into(),
            vault: vault_file.map(|file| AgentVaultConfig {
                path: Some(file.to_string_lossy().into_owned()),
                ..AgentVaultConfig::default()
            }),
            ..AgentConfig::default()
        }
    }

    fn run() -> MigrateOptions {
        MigrateOptions {
            dry_run: false,
            settle: Duration::ZERO,
        }
    }

    fn outcome(reports: &[ScopeReport], scope: &str) -> Outcome {
        reports
            .iter()
            .find(|report| report.scope == scope)
            .unwrap_or_else(|| panic!("no report for {scope}"))
            .outcome
            .clone()
    }

    async fn count(pool: &PostgresPool, query: &str) -> i64 {
        pool.get()
            .await
            .unwrap()
            .query_one(query, &[])
            .await
            .unwrap()
            .get(0)
    }

    async fn active_key(pool: &PostgresPool) -> String {
        pool.get()
            .await
            .unwrap()
            .query_one("SELECT active_key_id::text FROM vault_keyring", &[])
            .await
            .unwrap()
            .get(0)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn every_vault_is_imported_once() {
        with_test_pool(build_pool, |pool| async move {
            let install = Install::new().with_agents(vec![agent("sarah", None)]);
            install.seed(
                "vault.enc",
                &[("slack/bot_token", "xoxb"), ("users/u1/jira/token", "jt")],
            );
            install.seed("agents/sarah/vault.enc", &[("github/pat", "ghp")]);
            install.seed("agents/ghost/vault.enc", &[("old/secret", "s")]);
            let store = install.store(&pool).await;

            let report = migrate_with(&store, &install.config, run()).await.unwrap();
            assert_eq!(
                outcome(&report.scopes, "global"),
                Outcome::Imported {
                    entries: 2,
                    audit: 2
                }
            );
            assert_eq!(
                outcome(&report.scopes, "agent:sarah"),
                Outcome::Imported {
                    entries: 1,
                    audit: 1
                }
            );
            assert_eq!(
                outcome(&report.scopes, "agent:ghost"),
                Outcome::Imported {
                    entries: 1,
                    audit: 1
                }
            );
            assert!(report.shared_files.is_empty());
            assert!(frozen_marker(&install.path("vault.enc")).exists());

            let global = store.vault("global").await.unwrap();
            assert_eq!(global.get("slack/bot_token").as_deref(), Some("xoxb"));
            assert_eq!(
                global.get_entry("users/u1/jira/token").unwrap().created_by,
                "user:u1"
            );
            let ghost = store.vault("agent:ghost").await.unwrap();
            assert_eq!(ghost.get("old/secret").as_deref(), Some("s"));
            assert_eq!(
                store
                    .audit_records(Some("agent:sarah"), None, 10)
                    .await
                    .unwrap()
                    .len(),
                1
            );

            let again = migrate_with(&store, &install.config, run()).await.unwrap();
            assert!(again
                .scopes
                .iter()
                .all(|report| report.outcome == Outcome::AlreadyOnPostgres));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_dry_run_changes_nothing() {
        with_test_pool(build_pool, |pool| async move {
            let install = Install::new();
            install.seed("vault.enc", &[("a", "1")]);
            let store = install.store(&pool).await;

            let report = migrate_with(
                &store,
                &install.config,
                MigrateOptions {
                    dry_run: true,
                    settle: Duration::ZERO,
                },
            )
            .await
            .unwrap();
            assert_eq!(
                outcome(&report.scopes, "global"),
                Outcome::WouldImport {
                    entries: 1,
                    audit: 1
                }
            );
            assert_eq!(outcome(&report.scopes, "agent:default"), Outcome::NoFile);
            assert!(!frozen_marker(&install.path("vault.enc")).exists());
            assert!(store.scope_states().await.unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_that_decrypts_with_no_key_stops_the_migration_before_any_freeze() {
        with_test_pool(build_pool, |pool| async move {
            let install = Install::new().with_agents(vec![agent("sarah", None)]);
            install.seed("vault.enc", &[("a", "1")]);
            let foreign = install.path("agents/sarah/vault.enc");
            std::fs::create_dir_all(foreign.parent().unwrap()).unwrap();
            Vault::open(&foreign, "some other key")
                .unwrap()
                .set("b", "2")
                .unwrap();
            let store = install.store(&pool).await;

            assert!(migrate_with(&store, &install.config, run()).await.is_err());
            assert!(!frozen_marker(&install.path("vault.enc")).exists());
            assert!(store.scope_states().await.unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_import_commits_only_while_the_file_is_unchanged() {
        with_test_pool(build_pool, |pool| async move {
            let install = Install::new();
            let store = install.store(&pool).await;
            let entry = VaultEntry {
                value: "v".into(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
                created_by: "cli".into(),
                updated_by: "cli".into(),
            };
            let entries = HashMap::from([("k".to_string(), entry)]);

            let outcome = store
                .import_scope("global", &entries, &[], b"sha", || Ok(false))
                .await
                .unwrap();
            assert!(matches!(outcome, Import::FileChanged));
            assert!(store.scope_states().await.unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_shared_file_holding_a_chatgpt_sign_in_is_not_migrated() {
        with_test_pool(build_pool, |pool| async move {
            let install = Install::new();
            let shared = install.path("shared/vault.enc");
            let install =
                install.with_agents(vec![agent("a", Some(&shared)), agent("b", Some(&shared))]);
            install.seed(
                "shared/vault.enc",
                &[
                    ("s", "2"),
                    ("users/u1/codex/auth_json", "{}"),
                    ("claude_code/oauth_token", "sk-ant-oat01-shared"),
                ],
            );
            let store = install.store(&pool).await;

            for options in [
                MigrateOptions {
                    dry_run: true,
                    ..run()
                },
                run(),
            ] {
                let refused = migrate_with(&store, &install.config, options)
                    .await
                    .unwrap_err()
                    .to_string();
                assert!(
                    refused.contains("users/u1/codex/auth_json")
                        && refused.contains("agent:a")
                        && !refused.contains("oauth_token"),
                    "{refused}"
                );
            }
            assert!(!frozen_marker(&shared).exists(), "nothing was frozen");
            assert!(store.scope_states().await.unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agents_sharing_a_file_each_get_their_own_copy() {
        with_test_pool(build_pool, |pool| async move {
            let install = Install::new();
            let shared = install.path("shared/vault.enc");
            let global = install.path("vault.enc");
            let install = install.with_agents(vec![
                agent("a", Some(&shared)),
                agent("b", Some(&shared)),
                agent("c", Some(&global)),
            ]);
            install.seed("vault.enc", &[("g", "1")]);
            install.seed("shared/vault.enc", &[("s", "2")]);
            let store = install.store(&pool).await;

            let report = migrate_with(&store, &install.config, run()).await.unwrap();
            assert_eq!(
                report.shared_files,
                vec![(
                    canonical_file(&shared),
                    vec!["agent:a".to_string(), "agent:b".to_string()]
                )]
            );
            assert!(report.scopes.iter().all(|r| r.scope != "agent:c"));
            for scope in ["agent:a", "agent:b"] {
                assert_eq!(
                    store.vault(scope).await.unwrap().get("s").as_deref(),
                    Some("2")
                );
            }
            assert_eq!(
                store.vault("global").await.unwrap().get("g").as_deref(),
                Some("1")
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn export_rolls_back_and_a_later_migrate_reimports() {
        with_test_pool(build_pool, |pool| async move {
            let install = Install::new();
            install.seed("vault.enc", &[("a", "1")]);
            let store = install.store(&pool).await;
            migrate_with(&store, &install.config, run()).await.unwrap();
            let global = store.vault("global").await.unwrap();
            global
                .set_with_actor("b", "2", VaultActor::User("u1"))
                .unwrap();

            let exported = export_with(&store, &install.config, Duration::ZERO)
                .await
                .unwrap();
            assert_eq!(
                outcome(&exported.scopes, "global"),
                Outcome::Exported {
                    entries: 2,
                    audit: 1
                }
            );
            assert!(!frozen_marker(&install.path("vault.enc")).exists());
            assert!(
                global.set("c", "3").is_err(),
                "Postgres is frozen after an export"
            );
            let again = export_with(&store, &install.config, Duration::ZERO)
                .await
                .unwrap();
            assert_eq!(
                outcome(&again.scopes, "global"),
                Outcome::Exported {
                    entries: 2,
                    audit: 0
                }
            );

            let file = install.file_vault("vault.enc");
            assert_eq!(file.get("b").as_deref(), Some("2"));
            file.set_with_actor("c", "3", VaultActor::Cli).unwrap();

            let reimported = migrate_with(&store, &install.config, run()).await.unwrap();
            assert_eq!(
                outcome(&reimported.scopes, "global"),
                Outcome::Imported {
                    entries: 3,
                    audit: 1
                }
            );
            global.sync().unwrap();
            assert_eq!(global.get("c").as_deref(), Some("3"));
            global.set("d", "4").unwrap();
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_vault_left_open_across_an_export_keeps_the_export() {
        with_test_pool(build_pool, |pool| async move {
            let install = Install::new();
            install.seed("vault.enc", &[("a", "1")]);
            let held = install.file_vault("vault.enc");
            let store = install.store(&pool).await;
            migrate_with(&store, &install.config, run()).await.unwrap();
            store.vault("global").await.unwrap().set("b", "2").unwrap();

            export_with(&store, &install.config, Duration::ZERO)
                .await
                .unwrap();
            held.set_with_actor("c", "3", VaultActor::Cli).unwrap();

            assert_eq!(
                install.file_vault("vault.enc").list(None),
                vec!["a", "b", "c"]
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn verify_flags_a_write_that_slipped_past_the_freeze() {
        with_test_pool(build_pool, |pool| async move {
            let install = Install::new();
            install.seed("vault.enc", &[("a", "1")]);
            let store = install.store(&pool).await;
            migrate_with(&store, &install.config, run()).await.unwrap();

            let checks = verify_with(&store, &install.config).await.unwrap();
            let global = checks.iter().find(|check| check.scope == "global").unwrap();
            assert!(global.file_frozen && !global.changed_since_import && !global.unmigrated);
            assert_eq!(global.differences, Some(Differences::default()));

            let file = install.path("vault.enc");
            thaw(&file).unwrap();
            install.file_vault("vault.enc").set("late", "x").unwrap();
            freeze(&file).unwrap();

            let checks = verify_with(&store, &install.config).await.unwrap();
            let global = checks.iter().find(|check| check.scope == "global").unwrap();
            assert!(global.changed_since_import);
            assert_eq!(
                global.differences.as_ref().unwrap().only_in_file,
                vec!["late".to_string()]
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_vault_a_hub_created_empty_is_imported_while_still_empty() {
        with_test_pool(build_pool, |pool| async move {
            let install = Install::new();
            install.seed("vault.enc", &[("a", "1")]);
            let store = install.store(&pool).await;
            store.init_scope("global").await.unwrap();

            let report = migrate_with(&store, &install.config, run()).await.unwrap();
            assert_eq!(
                outcome(&report.scopes, "global"),
                Outcome::Imported {
                    entries: 1,
                    audit: 1
                }
            );
            assert_eq!(
                store.vault("global").await.unwrap().get("a").as_deref(),
                Some("1")
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_vault_a_hub_created_and_wrote_is_left_for_the_operator() {
        with_test_pool(build_pool, |pool| async move {
            let install = Install::new();
            install.seed("vault.enc", &[("a", "1")]);
            let store = install.store(&pool).await;
            store.init_scope("global").await.unwrap();
            let global = store.vault("global").await.unwrap();
            global.set("b", "2").unwrap();

            let report = migrate_with(&store, &install.config, run()).await.unwrap();
            assert_eq!(
                outcome(&report.scopes, "global"),
                Outcome::Diverged { entries: 1 }
            );
            assert!(!frozen_marker(&install.path("vault.enc")).exists());
            assert!(matches!(
                store
                    .import_scope("global", &HashMap::new(), &[], b"sha", || Ok(true))
                    .await
                    .unwrap(),
                Import::Diverged { entries: 1 }
            ));
            assert!(global.get("a").is_none());
            assert_eq!(global.get("b").as_deref(), Some("2"));

            let checks = verify_with(&store, &install.config).await.unwrap();
            let check = checks.iter().find(|check| check.scope == "global").unwrap();
            assert!(check.unmigrated);
            assert_eq!(
                check.differences.as_ref().unwrap().only_in_file,
                vec!["a".to_string()]
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rotated_audit_logs_are_imported_too() {
        with_test_pool(build_pool, |pool| async move {
            let install = Install::new();
            install.seed("vault.enc", &[("a", "1")]);
            std::fs::rename(
                install.path("vault-audit.jsonl"),
                install.path("vault-audit.1.jsonl"),
            )
            .unwrap();
            install.seed("vault.enc", &[("b", "2")]);
            let store = install.store(&pool).await;

            let report = migrate_with(&store, &install.config, run()).await.unwrap();
            assert_eq!(
                outcome(&report.scopes, "global"),
                Outcome::Imported {
                    entries: 2,
                    audit: 2
                }
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn export_never_mints_a_key_for_a_vault_file() {
        with_test_pool(build_pool, |pool| async move {
            let install = Install::new();
            let key_file = install.path("sarah.key");
            let install = install.with_agents(vec![AgentConfig {
                id: "sarah".into(),
                vault: Some(AgentVaultConfig {
                    key_file: Some(key_file.to_string_lossy().into_owned()),
                    ..AgentVaultConfig::default()
                }),
                ..AgentConfig::default()
            }]);
            let store = install.store(&pool).await;
            store.init_scope("agent:sarah").await.unwrap();
            store
                .vault("agent:sarah")
                .await
                .unwrap()
                .set("k", "v")
                .unwrap();

            assert!(export_with(&store, &install.config, Duration::ZERO)
                .await
                .is_err());
            assert!(!key_file.exists());
        })
        .await;
    }

    #[tokio::test]
    async fn the_file_backend_audit_is_read_newest_first() {
        let install = Install::new();
        install.seed("vault.enc", &[("first", "1"), ("second", "2")]);

        let records = audit(
            &install.config,
            AuditQuery {
                agent: None,
                since: None,
                limit: 1,
            },
        )
        .await
        .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].1.path, "second");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rotation_reseals_secrets_and_peers_keep_working() {
        with_test_pool(build_pool, |pool| async move {
            let install = Install::new();
            let (a, b) = (install.store(&pool).await, install.store(&pool).await);
            a.init_scope("global").await.unwrap();
            let (va, vb) = (
                a.vault("global").await.unwrap(),
                b.vault("global").await.unwrap(),
            );
            va.set("k", "1").unwrap();

            let summary = a.rotate(None).await.unwrap();
            assert_eq!(
                summary,
                RotationSummary {
                    scopes: 1,
                    entries: 1
                }
            );

            vb.set("k2", "2").unwrap();
            va.sync().unwrap();
            assert_eq!(va.get("k").as_deref(), Some("1"));
            assert_eq!(va.get("k2").as_deref(), Some("2"));
            assert_eq!(
                b.audit_records(Some("global"), None, 10)
                    .await
                    .unwrap()
                    .len(),
                2
            );
            assert_eq!(count(&pool, "SELECT count(*) FROM vault_keys").await, 2);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_passphrase_takes_over_once_the_old_one_is_retired() {
        with_test_pool(build_pool, |pool| async move {
            let install = Install::new();
            let old = install.store(&pool).await;
            old.init_scope("global").await.unwrap();
            old.vault("global").await.unwrap().set("k", "1").unwrap();

            old.rotate(Some("the new passphrase".into())).await.unwrap();
            let new = connect(&pool, "the new passphrase").await;
            assert_eq!(
                new.vault("global").await.unwrap().get("k").as_deref(),
                Some("1")
            );
            connect(&pool, &install.key).await;

            assert!(new.retire_foreign_wraps().await.unwrap() > 0);
            assert!(
                PgStore::connect(pool.clone(), install.key.clone(), settings())
                    .await
                    .is_err()
            );
            assert_eq!(
                new.audit_records(Some("global"), None, 10)
                    .await
                    .unwrap()
                    .len(),
                1,
                "history sealed under the retired key still opens"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_plain_rotation_never_locks_out_a_passphrase_being_rolled_out() {
        with_test_pool(build_pool, |pool| async move {
            let install = Install::new();
            let old = install.store(&pool).await;
            old.init_scope("global").await.unwrap();
            old.rotate(Some("the new passphrase".into())).await.unwrap();
            let active = active_key(&pool).await;

            let err = old.rotate(None).await.unwrap_err();
            assert!(err.to_string().contains("retire-key"), "{err}");
            let new = connect(&pool, "the new passphrase").await;
            assert!(new.rotate(None).await.is_err());
            assert_eq!(active_key(&pool).await, active);

            old.rotate(Some("the new passphrase".into())).await.unwrap();
            new.retire_foreign_wraps().await.unwrap();
            new.rotate(None).await.unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn key_commands_refuse_a_config_on_the_file_backend() {
        let install = Install::new();
        for err in [
            rotate_key(&install.config, None).await.unwrap_err(),
            retire_key(&install.config).await.unwrap_err(),
        ] {
            assert!(
                err.to_string().contains("keeps its vaults in files"),
                "{err}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rotation_refuses_while_a_secret_does_not_decrypt() {
        with_test_pool(build_pool, |pool| async move {
            let install = Install::new();
            let store = install.store(&pool).await;
            store.init_scope("global").await.unwrap();
            store.vault("global").await.unwrap().set("k", "1").unwrap();
            pool.get()
                .await
                .unwrap()
                .batch_execute("UPDATE vault_entries SET ciphertext = '\\xdeadbeef'::bytea")
                .await
                .unwrap();

            assert!(store.rotate(None).await.is_err());
            assert_eq!(count(&pool, "SELECT count(*) FROM vault_keys").await, 1);
        })
        .await;
    }
}
