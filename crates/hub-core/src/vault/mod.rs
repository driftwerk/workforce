//! Secrets vault: one [`Vault`] facade over its storage backends — an
//! AES-256-GCM file per vault ([`FileVault`]) or hub-encrypted rows in the
//! shared Postgres.

#[cfg(feature = "postgres")]
mod crypto;
mod file;
#[cfg(feature = "postgres")]
pub mod migrate;
mod open;
#[cfg(feature = "postgres")]
mod postgres;
mod targets;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::{bail, Result, WrapErr};
use rand::RngCore;
use workforce_common::secret_lock::{LocalRefreshLock, RefreshLock};
use workforce_common::settings::{VaultActor, VaultEntry};

use crate::hub_core::vault_audit::{value_fingerprint, AuditAction, AuditEntry};

pub use file::FileVault;
#[cfg(feature = "postgres")]
pub use open::uses_postgres;
pub use open::{open_agent, open_configured_agent, open_global, open_readers};

const MASTER_KEY_LEN: usize = 32;

/// Vault failures callers branch on; everything else is a plain report.
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("vault `{scope}` is read-only while it moves to a new store")]
    Frozen { scope: String },
    #[error(
        "vault `{scope}` still lives in {}, which was never imported into Postgres — run `workforce vault migrate`",
        .file.display()
    )]
    Unmigrated { scope: String, file: PathBuf },
}

/// One mutation in a [`commit`].
#[derive(Debug, Clone, Copy)]
pub enum VaultOp<'a> {
    Set { path: &'a str, value: &'a str },
    Delete { path: &'a str },
}

impl<'a> VaultOp<'a> {
    pub fn path(&self) -> &'a str {
        match *self {
            VaultOp::Set { path, .. } | VaultOp::Delete { path } => path,
        }
    }
}

/// A [`VaultOp`] addressed to one vault.
#[derive(Clone, Copy)]
pub struct VaultWrite<'a> {
    pub vault: &'a Vault,
    pub op: VaultOp<'a>,
}

/// The secrets vault — encrypted at rest, decrypted in memory.
pub struct Vault {
    backend: Backend,
}

enum Backend {
    File(FileVault),
    #[cfg(feature = "postgres")]
    Postgres(postgres::PgScope),
}

impl From<FileVault> for Vault {
    fn from(file: FileVault) -> Self {
        Self {
            backend: Backend::File(file),
        }
    }
}

impl workforce_common::settings::VaultResolver for Vault {
    fn resolve(&self, path: &str) -> Option<String> {
        Vault::get(self, path)
    }
}

impl workforce_common::settings::SecretStore for Vault {
    fn get(&self, path: &str) -> Option<String> {
        Vault::get(self, path)
    }

    fn set(&self, path: &str, value: &str) -> Result<()> {
        Vault::set(self, path, value)
    }

    fn delete(&self, path: &str) -> Result<bool> {
        Vault::delete(self, path)
    }

    fn list(&self, prefix: Option<&str>) -> Vec<String> {
        Vault::list(self, prefix)
    }

    fn set_with_actor(&self, path: &str, value: &str, actor: VaultActor<'_>) -> Result<()> {
        Vault::set_with_actor(self, path, value, actor)
    }

    fn get_entry(&self, path: &str) -> Option<VaultEntry> {
        Vault::get_entry(self, path)
    }

    fn set_many(&self, entries: &[(&str, &str)], actor: VaultActor<'_>) -> Result<()> {
        let ops: Vec<_> = entries
            .iter()
            .map(|&(path, value)| VaultOp::Set { path, value })
            .collect();
        self.apply(&ops, actor).map(drop)
    }

    fn write_location(&self, path: &str) -> String {
        format!("{}\0{path}", self.location())
    }

    fn sync(&self) -> Result<()> {
        Vault::sync(self)
    }

    fn refresh_lock(&self) -> Arc<dyn RefreshLock> {
        match &self.backend {
            Backend::File(_) => LocalRefreshLock::shared(),
            #[cfg(feature = "postgres")]
            Backend::Postgres(scope) => scope.refresh_lock(),
        }
    }
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault")
            .field("location", &self.location())
            .field("secrets_count", &self.read(HashMap::len))
            .finish()
    }
}

impl Vault {
    /// Open an existing vault file or create an empty one.
    pub fn open(vault_path: &Path, master_key: &str) -> Result<Self> {
        FileVault::open(vault_path, master_key).map(Self::from)
    }

    /// Where this vault is stored.
    pub fn location(&self) -> String {
        match &self.backend {
            Backend::File(file) => file.path().display().to_string(),
            #[cfg(feature = "postgres")]
            Backend::Postgres(scope) => format!("postgres:{}", scope.scope()),
        }
    }

    /// Pick up other processes' writes now: Postgres reloads a changed scope,
    /// a file vault reloads a file another process replaced.
    pub fn sync(&self) -> Result<()> {
        match &self.backend {
            Backend::File(file) => file.sync(),
            #[cfg(feature = "postgres")]
            Backend::Postgres(scope) => scope.sync(),
        }
    }

    /// Get a secret value by dot-notation path.
    pub fn get(&self, path: &str) -> Option<String> {
        self.read(|entries| entries.get(path).map(|entry| entry.value.clone()))
    }

    /// Get a full entry (value plus audit metadata) by dot-notation path.
    pub fn get_entry(&self, path: &str) -> Option<VaultEntry> {
        self.read(|entries| entries.get(path).cloned())
    }

    /// Set a secret using a default actor inferred from the calling
    /// context (current task-local user identity, or `system:unknown`
    /// when no identity is active). Prefer [`Vault::set_with_actor`] at
    /// call sites that know the writer.
    pub fn set(&self, path: &str, value: &str) -> Result<()> {
        let uid = current_user_id();
        self.set_with_actor(path, value, default_actor(uid.as_deref()))
    }

    /// Set a secret, recording `actor` in the entry's audit metadata.
    pub fn set_with_actor(&self, path: &str, value: &str, actor: VaultActor<'_>) -> Result<()> {
        self.apply(&[VaultOp::Set { path, value }], actor).map(drop)
    }

    /// Delete a secret. Returns whether the key existed. Records the
    /// removal in the audit log (when one is attached) using the same
    /// default-actor resolution as [`Vault::set`].
    pub fn delete(&self, path: &str) -> Result<bool> {
        let uid = current_user_id();
        self.delete_with_actor(path, default_actor(uid.as_deref()))
    }

    /// Delete a secret, recording `actor` in the audit log. Returns
    /// whether the key existed.
    pub fn delete_with_actor(&self, path: &str, actor: VaultActor<'_>) -> Result<bool> {
        self.apply(&[VaultOp::Delete { path }], actor)
            .map(|existed| existed == [true])
    }

    /// Apply `ops` to this vault as one unit; see [`commit`].
    pub fn apply(&self, ops: &[VaultOp<'_>], actor: VaultActor<'_>) -> Result<Vec<bool>> {
        let writes: Vec<_> = ops
            .iter()
            .map(|&op| VaultWrite { vault: self, op })
            .collect();
        commit(&writes, actor)
    }

    /// List secret paths (never values). Optionally filtered by prefix.
    pub fn list(&self, prefix: Option<&str>) -> Vec<String> {
        let mut paths: Vec<String> = self.read(|entries| {
            entries
                .keys()
                .filter(|path| prefix.is_none_or(|p| path.starts_with(p)))
                .cloned()
                .collect()
        });
        paths.sort();
        paths
    }

    /// The `api_key` of the first `llm/accounts/*` entry whose stored
    /// `provider` matches `provider`. The single credential source shared by
    /// chat-provider and image-provider resolution, so a key added via
    /// `system.add_account` is visible to both.
    pub fn find_account_api_key(&self, provider: &str) -> Option<String> {
        self.find_account(provider)
            .and_then(|(_, record)| record["api_key"].as_str().map(str::to_string))
    }

    /// The path and record of the first `llm/accounts/*` entry whose stored
    /// `provider` matches `provider` and that holds an `api_key`: the one
    /// [`Self::find_account_api_key`] reads.
    pub fn find_account(&self, provider: &str) -> Option<(String, serde_json::Value)> {
        self.list(Some("llm/accounts/"))
            .into_iter()
            .find_map(|path| {
                let parsed: serde_json::Value = serde_json::from_str(&self.get(&path)?).ok()?;
                (parsed["provider"].as_str()? == provider && parsed["api_key"].is_string())
                    .then_some((path, parsed))
            })
    }

    /// Every `llm/accounts/*` path whose record names `provider`.
    pub fn account_paths(&self, provider: &str) -> Vec<String> {
        self.list(Some("llm/accounts/"))
            .into_iter()
            .filter(|path| {
                self.get(path)
                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                    .is_some_and(|record| record["provider"].as_str() == Some(provider))
            })
            .collect()
    }

    /// The master key from the `WORKFORCE_VAULT_KEY` env var or an existing
    /// key file; `None` when neither exists. Never creates a key.
    pub fn read_master_key(key_file: Option<&Path>, data_dir: &Path) -> Result<Option<String>> {
        if let Some(key) = std::env::var("WORKFORCE_VAULT_KEY")
            .ok()
            .filter(|key| !key.is_empty())
        {
            return Ok(Some(key));
        }
        let path = key_file_path(key_file, data_dir);
        if !path.is_file() {
            return Ok(None);
        }
        let key = std::fs::read_to_string(&path)
            .wrap_err_with(|| format!("Failed to read vault key from {}", path.display()))?
            .trim()
            .to_string();
        if key.is_empty() {
            bail!("Vault key file {} is empty", path.display());
        }
        // Tighten on every read so key files written by pre-0600 deployments
        // (or restored from a permissive backup) get re-secured.
        ensure_owner_only(&path)?;
        Ok(Some(key))
    }

    /// Resolve the master key: [`Vault::read_master_key`], otherwise
    /// generate a new key and save it to the key file.
    pub fn resolve_master_key(key_file: Option<&Path>, data_dir: &Path) -> Result<String> {
        if let Some(key) = Self::read_master_key(key_file, data_dir)? {
            return Ok(key);
        }
        let path = key_file_path(key_file, data_dir);
        let key = Self::generate_master_key();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, &key)
            .wrap_err_with(|| format!("Failed to write vault key to {}", path.display()))?;
        ensure_owner_only(&path)?;
        tracing::info!(
            path = %path.display(),
            mode = "0600",
            "Generated new vault master key",
        );
        Ok(key)
    }

    /// Generate a random master key (base64-encoded 32 bytes).
    pub fn generate_master_key() -> String {
        use base64::Engine;
        let mut key = [0u8; MASTER_KEY_LEN];
        rand::thread_rng().fill_bytes(&mut key);
        base64::engine::general_purpose::STANDARD.encode(key)
    }

    #[cfg(feature = "postgres")]
    fn from_postgres(scope: postgres::PgScope) -> Self {
        Self {
            backend: Backend::Postgres(scope),
        }
    }

    fn read<T>(&self, f: impl FnOnce(&HashMap<String, VaultEntry>) -> T) -> T {
        match &self.backend {
            Backend::File(file) => file.read(f),
            #[cfg(feature = "postgres")]
            Backend::Postgres(scope) => scope.read(f),
        }
    }

    fn as_file(&self) -> Option<&FileVault> {
        match &self.backend {
            Backend::File(file) => Some(file),
            #[cfg(feature = "postgres")]
            Backend::Postgres(_) => None,
        }
    }

    #[cfg(feature = "postgres")]
    fn as_postgres(&self) -> Option<&postgres::PgScope> {
        match &self.backend {
            Backend::Postgres(scope) => Some(scope),
            Backend::File(_) => None,
        }
    }
}

/// Apply `writes` as one unit per backend. On Postgres every vault's writes
/// commit in one transaction. On files each vault's writes persist together,
/// vault by vault in first-appearance order — put destination writes before
/// source deletes so a failure between two files leaves a duplicate, never a
/// loss. Returns, for each write, whether its path held a value beforehand.
pub fn commit(writes: &[VaultWrite<'_>], actor: VaultActor<'_>) -> Result<Vec<bool>> {
    let groups = group_by_vault(writes);
    let batches: Vec<(&Vault, Vec<VaultOp<'_>>)> = groups
        .iter()
        .map(|(vault, indices)| {
            (
                *vault,
                indices.iter().map(|&index| writes[index].op).collect(),
            )
        })
        .collect();
    let existed = apply_batches(&batches, actor)?;
    let mut outcomes: Vec<(usize, bool)> = groups
        .into_iter()
        .zip(existed)
        .flat_map(|((_, indices), existed)| indices.into_iter().zip(existed))
        .collect();
    outcomes.sort_unstable_by_key(|&(index, _)| index);
    Ok(outcomes.into_iter().map(|(_, existed)| existed).collect())
}

fn apply_batches(
    batches: &[(&Vault, Vec<VaultOp<'_>>)],
    actor: VaultActor<'_>,
) -> Result<Vec<Vec<bool>>> {
    if let Some(files) = batches
        .iter()
        .map(|(vault, ops)| vault.as_file().map(|file| (file, ops)))
        .collect::<Option<Vec<_>>>()
    {
        return files
            .into_iter()
            .map(|(file, ops)| file.apply(ops, actor))
            .collect();
    }
    #[cfg(feature = "postgres")]
    if let Some(scopes) = batches
        .iter()
        .map(|(vault, ops)| vault.as_postgres().map(|scope| (scope, ops.as_slice())))
        .collect::<Option<Vec<_>>>()
    {
        return postgres::commit(&scopes, actor);
    }
    bail!("one vault commit cannot span storage backends")
}

/// Move `value` from one vault slot to another as one [`commit`]; a no-op
/// when both name the same slot (two tiers backed by one vault).
pub fn move_value(
    from: (&Vault, &str),
    to: (&Vault, &str),
    value: &str,
    actor: VaultActor<'_>,
) -> Result<()> {
    if std::ptr::eq(from.0, to.0) && from.1 == to.1 {
        return Ok(());
    }
    commit(
        &[
            VaultWrite {
                vault: to.0,
                op: VaultOp::Set { path: to.1, value },
            },
            VaultWrite {
                vault: from.0,
                op: VaultOp::Delete { path: from.1 },
            },
        ],
        actor,
    )
    .map(drop)
}

fn group_by_vault<'a>(writes: &[VaultWrite<'a>]) -> Vec<(&'a Vault, Vec<usize>)> {
    writes.iter().enumerate().fold(
        Vec::new(),
        |mut groups: Vec<(&'a Vault, Vec<usize>)>, (index, write)| {
            match groups
                .iter_mut()
                .find(|(vault, _)| std::ptr::eq(*vault, write.vault))
            {
                Some((_, indices)) => indices.push(index),
                None => groups.push((write.vault, vec![index])),
            }
            groups
        },
    )
}

/// A batch folded over the entries it touches.
struct Plan {
    entries: HashMap<String, VaultEntry>,
    existed: Vec<bool>,
    audit: Vec<AuditEntry<'static>>,
}

/// Fold `ops` in order over `entries`, so later ops see earlier ones.
/// Deleting a missing path changes nothing and records nothing.
fn plan(
    entries: HashMap<String, VaultEntry>,
    ops: &[VaultOp<'_>],
    actor: &str,
    now: DateTime<Utc>,
) -> Plan {
    let empty = Plan {
        entries,
        existed: Vec::with_capacity(ops.len()),
        audit: Vec::new(),
    };
    ops.iter()
        .fold(empty, |plan, &op| plan.step(op, actor, now))
}

impl Plan {
    fn step(mut self, op: VaultOp<'_>, actor: &str, now: DateTime<Utc>) -> Self {
        match op {
            VaultOp::Set { path, value } => {
                let prev = self.entries.get(path);
                let entry = match prev {
                    Some(prev) => VaultEntry {
                        value: value.to_owned(),
                        updated_at: now,
                        updated_by: actor.to_owned(),
                        ..prev.clone()
                    },
                    None => VaultEntry {
                        value: value.to_owned(),
                        created_at: now,
                        updated_at: now,
                        created_by: creator(path, actor),
                        updated_by: actor.to_owned(),
                    },
                };
                self.existed.push(prev.is_some());
                self.audit.push(audit_entry(
                    now,
                    actor,
                    AuditAction::Set,
                    path,
                    Some(value),
                    prev.map(|prev| prev.value.as_str()),
                ));
                self.entries.insert(path.to_owned(), entry);
            }
            VaultOp::Delete { path } => {
                let prev = self.entries.remove(path);
                self.existed.push(prev.is_some());
                self.audit.extend(prev.map(|prev| {
                    audit_entry(
                        now,
                        actor,
                        AuditAction::Delete,
                        path,
                        None,
                        Some(&prev.value),
                    )
                }));
            }
        }
        self
    }
}

fn audit_entry(
    ts: DateTime<Utc>,
    actor: &str,
    action: AuditAction,
    path: &str,
    value: Option<&str>,
    prev: Option<&str>,
) -> AuditEntry<'static> {
    AuditEntry {
        ts,
        actor: actor.to_owned().into(),
        action,
        path: path.to_owned().into(),
        ok: true,
        value_sha256_first8: value.map(value_fingerprint),
        prev_value_sha256_first8: prev.map(value_fingerprint),
    }
}

/// The uid a per-user path (`users/<uid>/…`) belongs to.
pub fn owner_of(path: &str) -> Option<&str> {
    path.strip_prefix("users/")?
        .split_once('/')
        .map(|(uid, _)| uid)
        .filter(|uid| !uid.is_empty())
}

/// `created_by` for a new entry: a per-user entry names its owner (the UI
/// addresses per-user entries through it), any other names its writer.
fn creator(path: &str, actor: &str) -> String {
    owner_of(path).map_or_else(
        || actor.to_owned(),
        |uid| VaultActor::User(uid).audit_string(),
    )
}

/// Apply the owner rule to an entry stored before the rule existed.
fn with_owner(path: &str, entry: VaultEntry) -> VaultEntry {
    VaultEntry {
        created_by: creator(path, &entry.created_by),
        ..entry
    }
}

fn key_file_path(key_file: Option<&Path>, data_dir: &Path) -> PathBuf {
    key_file
        .map(Path::to_path_buf)
        .unwrap_or_else(|| data_dir.join("vault-key.txt"))
}

/// Tighten `path` to owner-readable only (mode 0o600). No-op on non-Unix
/// targets — Windows uses ACLs, which the hub's user already controls when
/// it created the file.
fn ensure_owner_only(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .wrap_err_with(|| format!("Failed to set 0600 permissions on {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// [`Vault::find_account_api_key`] across the agent vault, then the global
/// vault. An `llm/accounts/*` entry moved to the global tier (gateway
/// "move to global") must stay resolvable at boot and provider hot-reload —
/// neither has a user context, so this is a plain two-tier lookup, not a
/// `LayeredUserVault` read. The agent's own entry wins when both tiers
/// define the same provider.
pub fn find_account_api_key_layered(
    agent: Option<&Vault>,
    global: Option<&Vault>,
    provider: &str,
) -> Option<String> {
    agent
        .and_then(|v| v.find_account_api_key(provider))
        .or_else(|| global.and_then(|v| v.find_account_api_key(provider)))
}

/// The `llm/accounts/*` record [`find_account_api_key_layered`] reads its key
/// from: the agent vault's, then the global vault's.
pub fn find_account_layered(
    agent: Option<&Vault>,
    global: Option<&Vault>,
    provider: &str,
) -> Option<serde_json::Value> {
    agent
        .and_then(|v| v.find_account(provider))
        .or_else(|| global.and_then(|v| v.find_account(provider)))
        .map(|(_, record)| record)
}

/// A speaking user's own voice for `provider`, resolved at request time for
/// the turn's user from `users/{uid}/speech/{provider}/voice` — the path
/// `vault.set` writes with a per-user scope and the my-settings UI manages.
///
/// Tier precedence is global-per-user first, then agent-per-user — the same
/// order as [`crate::user_config::user_override_layered`], so the voice a user
/// saves in Settings → Integrations (the global tier) wins over a stale
/// agent-tier copy, and a Settings "Remove" actually takes effect.
pub fn find_user_speech_voice(
    agent: Option<&Vault>,
    global: Option<&Vault>,
    uid: &str,
    provider: &str,
) -> Option<String> {
    let path = format!("users/{uid}/speech/{provider}/voice");
    global
        .filter(|_| !uid.is_empty())
        .and_then(|v| v.get(&path))
        .or_else(|| agent.filter(|_| !uid.is_empty()).and_then(|v| v.get(&path)))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The per-user voice resolver the `generate_speech` tool calls at request
/// time — the single constructor for the closure all three install sites
/// (boot, config hot-reload, vault-reload hook) pass in.
pub fn speech_user_voice_resolver(
    agent: Option<Arc<Vault>>,
    global: Option<Arc<Vault>>,
) -> workforce_agent::tools::speech_gen::SpeechUserVoice {
    Arc::new(move |uid: &str, provider: &str| {
        find_user_speech_voice(agent.as_deref(), global.as_deref(), uid, provider)
    })
}

fn current_user_id() -> Option<String> {
    workforce_agent::runner::CURRENT_USER_ID
        .try_with(|u| u.clone())
        .ok()
        .filter(|u| !u.is_empty())
}

fn default_actor(uid: Option<&str>) -> VaultActor<'_> {
    match uid {
        Some(u) => VaultActor::User(u),
        None => VaultActor::System("unknown"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub_core::vault_audit::VaultAuditLog;

    fn test_key() -> String {
        "test-vault-passphrase-123".to_string()
    }

    fn open_vault(dir: &Path, name: &str) -> Vault {
        Vault::open(&dir.join(name), &test_key()).unwrap()
    }

    #[test]
    fn set_and_get() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = open_vault(tmp.path(), "vault.enc");

        vault.set("slack.bot_token", "xoxb-123").unwrap();
        assert_eq!(vault.get("slack.bot_token").unwrap(), "xoxb-123");
        assert!(vault.get("nonexistent").is_none());
    }

    #[test]
    fn set_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = open_vault(tmp.path(), "vault.enc");

        vault.set("key", "first").unwrap();
        vault.set("key", "second").unwrap();
        assert_eq!(vault.get("key").unwrap(), "second");
    }

    #[test]
    fn find_account_api_key_matches_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = open_vault(tmp.path(), "vault.enc");

        vault
            .set(
                "llm/accounts/gemini-images",
                r#"{"provider":"gemini","api_key":"AIza-key"}"#,
            )
            .unwrap();
        vault
            .set(
                "llm/accounts/main",
                r#"{"provider":"openrouter","api_key":"or-key"}"#,
            )
            .unwrap();

        assert_eq!(
            vault.find_account_api_key("gemini").as_deref(),
            Some("AIza-key")
        );
        assert_eq!(
            vault.find_account_api_key("openrouter").as_deref(),
            Some("or-key")
        );
        assert!(vault.find_account_api_key("bedrock").is_none());
    }

    #[test]
    fn layered_account_lookup_falls_back_to_global() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = open_vault(tmp.path(), "agent.enc");
        let global = open_vault(tmp.path(), "global.enc");

        global
            .set(
                "llm/accounts/gemini",
                r#"{"provider":"gemini","api_key":"global-key"}"#,
            )
            .unwrap();

        assert_eq!(
            find_account_api_key_layered(Some(&agent), Some(&global), "gemini").as_deref(),
            Some("global-key")
        );
        agent
            .set(
                "llm/accounts/gemini",
                r#"{"provider":"gemini","api_key":"agent-key"}"#,
            )
            .unwrap();
        assert_eq!(
            find_account_api_key_layered(Some(&agent), Some(&global), "gemini").as_deref(),
            Some("agent-key")
        );
        assert!(find_account_api_key_layered(Some(&agent), Some(&global), "zai").is_none());
        assert!(find_account_api_key_layered(None, None, "gemini").is_none());
    }

    #[test]
    fn delete_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = open_vault(tmp.path(), "vault.enc");

        vault.set("key", "value").unwrap();
        assert!(vault.delete("key").unwrap());
        assert!(vault.get("key").is_none());
    }

    #[test]
    fn delete_nonexistent() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = open_vault(tmp.path(), "vault.enc");
        assert!(!vault.delete("nope").unwrap());
    }

    #[test]
    fn list_all() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = open_vault(tmp.path(), "vault.enc");

        vault.set("slack.bot_token", "a").unwrap();
        vault.set("github.pat", "b").unwrap();
        vault.set("slack.app_token", "c").unwrap();

        assert_eq!(
            vault.list(None),
            vec!["github.pat", "slack.app_token", "slack.bot_token"]
        );
    }

    #[test]
    fn list_with_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = open_vault(tmp.path(), "vault.enc");

        vault.set("slack.bot_token", "a").unwrap();
        vault.set("github.pat", "b").unwrap();
        vault.set("slack.app_token", "c").unwrap();

        assert_eq!(
            vault.list(Some("slack.")),
            vec!["slack.app_token", "slack.bot_token"]
        );
    }

    #[test]
    fn set_with_actor_records_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = open_vault(tmp.path(), "vault.enc");

        vault
            .set_with_actor("k", "v", VaultActor::User("kings"))
            .unwrap();

        let entry = vault.get_entry("k").unwrap();
        assert_eq!(entry.value, "v");
        assert_eq!(entry.created_by, "user:kings");
        assert_eq!(entry.updated_by, "user:kings");
        assert_eq!(entry.created_at, entry.updated_at);
    }

    #[test]
    fn updates_preserve_created_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = open_vault(tmp.path(), "vault.enc");

        vault
            .set_with_actor("k", "first", VaultActor::User("kings"))
            .unwrap();
        let original = vault.get_entry("k").unwrap();

        std::thread::sleep(std::time::Duration::from_millis(2));

        vault
            .set_with_actor("k", "second", VaultActor::User("tom"))
            .unwrap();
        let updated = vault.get_entry("k").unwrap();

        assert_eq!(updated.value, "second");
        assert_eq!(updated.created_by, "user:kings");
        assert_eq!(updated.created_at, original.created_at);
        assert_eq!(updated.updated_by, "user:tom");
        assert!(updated.updated_at > original.updated_at);
    }

    #[test]
    fn per_user_entries_name_their_owner_whoever_writes_them() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = open_vault(tmp.path(), "vault.enc");

        vault
            .set_with_actor(
                "users/u1/mcp/v1/hubspot/access_token",
                "tok",
                VaultActor::System("oauth_refresh"),
            )
            .unwrap();

        let entry = vault
            .get_entry("users/u1/mcp/v1/hubspot/access_token")
            .unwrap();
        assert_eq!(entry.created_by, "user:u1");
        assert_eq!(entry.updated_by, "system:oauth_refresh");
    }

    #[test]
    fn owner_of_reads_only_well_formed_per_user_paths() {
        assert_eq!(owner_of("users/u1/skills/jira/token"), Some("u1"));
        assert_eq!(owner_of("users//skills"), None);
        assert_eq!(owner_of("users/u1"), None);
        assert_eq!(owner_of("skills/users/u1/x"), None);
    }

    #[test]
    fn concurrent_writes_keep_one_winning_value() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = Arc::new(open_vault(tmp.path(), "vault.enc"));

        let handles: Vec<_> = (0..16)
            .map(|i| {
                let v = Arc::clone(&vault);
                std::thread::spawn(move || {
                    v.set_with_actor("k", &format!("v{i}"), VaultActor::Cli)
                        .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let entry = vault
            .get_entry("k")
            .expect("key exists after concurrent sets");
        assert!(entry.value.starts_with('v'));
        assert_eq!(entry.created_by, "cli");
        assert_eq!(entry.updated_by, "cli");
    }

    #[test]
    fn audit_log_records_set_and_delete_without_values() {
        let tmp = tempfile::tempdir().unwrap();
        let audit_path = tmp.path().join("vault-audit.jsonl");
        let audit = Arc::new(VaultAuditLog::open(audit_path.clone()).unwrap());
        let vault = Vault::from(
            FileVault::open(&tmp.path().join("vault.enc"), &test_key())
                .unwrap()
                .with_audit_log(audit),
        );

        vault
            .set_with_actor("k", "v1", VaultActor::User("kings"))
            .unwrap();
        vault
            .set_with_actor("k", "v2", VaultActor::User("kings"))
            .unwrap();
        vault.delete_with_actor("k", VaultActor::Cli).unwrap();
        vault.delete_with_actor("missing", VaultActor::Cli).unwrap();

        let raw = std::fs::read_to_string(&audit_path).unwrap();
        assert!(!raw.contains("v1"));
        assert!(!raw.contains("v2"));
        let lines: Vec<_> = raw.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("\"action\":\"set\""));
        assert!(lines[0].contains("user:kings"));
        assert!(lines[1].contains("prev_value_sha256_first8"));
        assert!(lines[2].contains("\"action\":\"delete\""));
        assert!(lines[2].contains("\"actor\":\"cli\""));
    }

    #[test]
    fn apply_folds_ops_in_order_and_reports_prior_presence() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = open_vault(tmp.path(), "vault.enc");
        vault.set("old", "1").unwrap();

        let existed = vault
            .apply(
                &[
                    VaultOp::Set {
                        path: "new",
                        value: "1",
                    },
                    VaultOp::Delete { path: "old" },
                    VaultOp::Delete { path: "new" },
                    VaultOp::Delete { path: "never" },
                ],
                VaultActor::Cli,
            )
            .unwrap();

        assert_eq!(existed, vec![false, true, true, false]);
        assert!(vault.list(None).is_empty());
    }

    #[test]
    fn commit_moves_across_vaults_destination_first() {
        let tmp = tempfile::tempdir().unwrap();
        let source = open_vault(tmp.path(), "agent.enc");
        let destination = open_vault(tmp.path(), "global.enc");
        source.set("skills/x/token", "tok").unwrap();

        let existed = commit(
            &[
                VaultWrite {
                    vault: &destination,
                    op: VaultOp::Set {
                        path: "skills/x/token",
                        value: "tok",
                    },
                },
                VaultWrite {
                    vault: &source,
                    op: VaultOp::Delete {
                        path: "skills/x/token",
                    },
                },
            ],
            VaultActor::Cli,
        )
        .unwrap();

        assert_eq!(existed, vec![false, true]);
        assert!(source.get("skills/x/token").is_none());
        assert_eq!(destination.get("skills/x/token").as_deref(), Some("tok"));
    }

    #[test]
    fn commit_stops_at_the_first_vault_that_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let source = open_vault(tmp.path(), "agent.enc");
        let destination = open_vault(tmp.path(), "global.enc");
        source.set("k", "v").unwrap();
        std::fs::write(file::frozen_marker(&tmp.path().join("global.enc")), "{}").unwrap();

        let result = commit(
            &[
                VaultWrite {
                    vault: &destination,
                    op: VaultOp::Set {
                        path: "k",
                        value: "v",
                    },
                },
                VaultWrite {
                    vault: &source,
                    op: VaultOp::Delete { path: "k" },
                },
            ],
            VaultActor::Cli,
        );

        let err = result.unwrap_err();
        assert!(matches!(
            err.downcast_ref::<VaultError>(),
            Some(VaultError::Frozen { .. })
        ));
        assert_eq!(source.get("k").as_deref(), Some("v"));
        assert!(destination.get("k").is_none());
    }

    #[test]
    fn moving_a_value_onto_its_own_slot_keeps_it() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = open_vault(tmp.path(), "vault.enc");
        vault.set("users/u1/k", "v").unwrap();

        move_value(
            (&vault, "users/u1/k"),
            (&vault, "users/u1/k"),
            "v",
            VaultActor::Cli,
        )
        .unwrap();
        assert_eq!(vault.get("users/u1/k").as_deref(), Some("v"));

        let other = open_vault(tmp.path(), "other.enc");
        move_value(
            (&vault, "users/u1/k"),
            (&other, "users/u1/k"),
            "v",
            VaultActor::Cli,
        )
        .unwrap();
        assert!(vault.get("users/u1/k").is_none());
        assert_eq!(other.get("users/u1/k").as_deref(), Some("v"));
    }

    #[test]
    fn set_many_writes_every_entry_in_one_persist() {
        use workforce_common::settings::SecretStore;
        let tmp = tempfile::tempdir().unwrap();
        let vault = open_vault(tmp.path(), "vault.enc");

        vault
            .set_many(
                &[("a", "1"), ("b", "2")],
                VaultActor::System("oauth_refresh"),
            )
            .unwrap();

        assert_eq!(vault.get("a").as_deref(), Some("1"));
        assert_eq!(vault.get("b").as_deref(), Some("2"));
        let reopened = open_vault(tmp.path(), "vault.enc");
        assert_eq!(reopened.list(None), vec!["a", "b"]);
    }

    #[test]
    fn write_location_names_the_vault_and_path() {
        use workforce_common::settings::SecretStore;
        let tmp = tempfile::tempdir().unwrap();
        let first = open_vault(tmp.path(), "a.enc");
        let second = open_vault(tmp.path(), "b.enc");
        assert_ne!(first.write_location("k"), second.write_location("k"));
        assert_eq!(first.write_location("k"), first.write_location("k"));
    }

    #[test]
    fn resolve_master_key_from_file() {
        let tmp = tempfile::tempdir().unwrap();
        let key_path = tmp.path().join("vault-key.txt");
        std::fs::write(&key_path, "file-key-456\n").unwrap();

        let result = Vault::resolve_master_key(Some(&key_path), tmp.path()).unwrap();
        if std::env::var("WORKFORCE_VAULT_KEY").is_err() {
            assert_eq!(result, "file-key-456");
        }
    }

    #[test]
    fn resolve_master_key_auto_generates() {
        let tmp = tempfile::tempdir().unwrap();
        let key_path = tmp.path().join("vault-key.txt");
        assert!(!key_path.exists());

        let result = Vault::resolve_master_key(Some(&key_path), tmp.path()).unwrap();
        assert!(!result.is_empty());
        let second = Vault::resolve_master_key(Some(&key_path), tmp.path()).unwrap();
        assert_eq!(result, second);
        if std::env::var("WORKFORCE_VAULT_KEY").is_err() {
            assert!(key_path.exists());
        }
    }

    #[test]
    fn read_master_key_never_creates_one() {
        if std::env::var("WORKFORCE_VAULT_KEY").is_ok() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let key_path = tmp.path().join("vault-key.txt");
        assert!(Vault::read_master_key(Some(&key_path), tmp.path())
            .unwrap()
            .is_none());
        assert!(!key_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn auto_generated_key_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        if std::env::var("WORKFORCE_VAULT_KEY").is_ok() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let key_path = tmp.path().join("vault-key.txt");
        Vault::resolve_master_key(Some(&key_path), tmp.path()).unwrap();
        let mode = std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
    }

    #[cfg(unix)]
    #[test]
    fn existing_loose_key_file_is_tightened_on_read() {
        use std::os::unix::fs::PermissionsExt;
        if std::env::var("WORKFORCE_VAULT_KEY").is_ok() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let key_path = tmp.path().join("vault-key.txt");
        std::fs::write(&key_path, "legacy-key\n").unwrap();
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let key = Vault::resolve_master_key(Some(&key_path), tmp.path()).unwrap();
        assert_eq!(key, "legacy-key");

        let mode = std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600 after read, got {mode:o}");
    }

    #[test]
    fn generate_master_key_is_base64() {
        use base64::Engine;
        let key = Vault::generate_master_key();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&key)
            .unwrap();
        assert_eq!(decoded.len(), MASTER_KEY_LEN);
    }
}
