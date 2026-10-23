//! Encrypted-file backend.
//!
//! Format v2: `magic(4)="WFV\x02" || salt(16) || nonce(12) || ciphertext+tag`;
//! the plaintext is a `serde_json` map of path → [`VaultEntry`]. v1 files (no
//! magic prefix, plaintext path → value) open with `legacy` metadata and are
//! written as v2 on the next write. Every write rewrites the whole file. A
//! write first reloads a file another process replaced, but two processes
//! writing at once can still lose one's change, so a file must have one
//! writing process.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use argon2::Argon2;
use chrono::{DateTime, Utc};
use eyre::{bail, Result, WrapErr};
use parking_lot::{Mutex, RwLock};
use rand::RngCore;
use sha2::{Digest, Sha256};
use workforce_common::settings::{VaultActor, VaultEntry};
use zeroize::Zeroizing;

use super::{plan, with_owner, VaultError, VaultOp};
use crate::hub_core::vault_audit::{AuditEntry, VaultAuditLog};

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
const MAGIC_V2: &[u8; 4] = b"WFV\x02";

/// One encrypted vault file, decrypted into memory at open.
pub struct FileVault {
    path: PathBuf,
    entries: RwLock<HashMap<String, VaultEntry>>,
    /// Held across every write, reload and key rotation.
    disk: Mutex<OnDisk>,
    audit: Option<Arc<VaultAuditLog>>,
}

/// The file as this process last read or wrote it.
struct OnDisk {
    passphrase: Zeroizing<String>,
    sealer: Sealer,
    sha256: [u8; 32],
}

impl FileVault {
    /// Open an existing vault file or create an empty one.
    pub fn open(path: &Path, master_key: &str) -> Result<Self> {
        let (entries, sealer, data) = if path.is_file() {
            load(path, master_key)?
        } else {
            let sealer = Sealer::generate(master_key)?;
            let data = sealer.seal(&HashMap::new())?;
            write_atomic(path, &data)?;
            (HashMap::new(), sealer, data)
        };
        Ok(Self {
            path: path.to_path_buf(),
            entries: RwLock::new(entries),
            disk: Mutex::new(OnDisk {
                passphrase: Zeroizing::new(master_key.to_owned()),
                sealer,
                sha256: sha256(&data),
            }),
            audit: None,
        })
    }

    /// Record every write in an append-only JSONL audit log.
    pub fn with_audit_log(self, audit: Arc<VaultAuditLog>) -> Self {
        Self {
            audit: Some(audit),
            ..self
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Re-encrypt the file under a new master key.
    pub fn rotate_key(&self, new_master_key: &str) -> Result<()> {
        let mut disk = self.disk.lock();
        self.ensure_writable()?;
        self.catch_up(&mut disk)?;
        let sealer = Sealer::generate(new_master_key)?;
        let data = sealer.seal(&self.entries.read())?;
        write_atomic(&self.path, &data)?;
        *disk = OnDisk {
            passphrase: Zeroizing::new(new_master_key.to_owned()),
            sealer,
            sha256: sha256(&data),
        };
        Ok(())
    }

    pub(super) fn read<T>(&self, f: impl FnOnce(&HashMap<String, VaultEntry>) -> T) -> T {
        f(&self.entries.read())
    }

    /// Reload the file if another process replaced it.
    pub(super) fn sync(&self) -> Result<()> {
        self.catch_up(&mut self.disk.lock())
    }

    /// Apply `ops` in one persist; memory changes only once the file has.
    pub(super) fn apply(&self, ops: &[VaultOp<'_>], actor: VaultActor<'_>) -> Result<Vec<bool>> {
        let mut disk = self.disk.lock();
        self.ensure_writable()?;
        self.catch_up(&mut disk)?;
        let plan = plan(
            self.entries.read().clone(),
            ops,
            &actor.audit_string(),
            Utc::now(),
        );
        if plan.audit.is_empty() {
            return Ok(plan.existed);
        }
        let persisted = disk
            .sealer
            .seal(&plan.entries)
            .and_then(|data| write_atomic(&self.path, &data).map(|()| sha256(&data)));
        self.record(plan.audit, persisted.is_ok());
        disk.sha256 = persisted?;
        *self.entries.write() = plan.entries;
        Ok(plan.existed)
    }

    /// Reload the file when another process replaced it since this one last
    /// read or wrote it, so the next write keeps that process's changes. The
    /// check and the write that follows are not atomic across processes.
    fn catch_up(&self, disk: &mut OnDisk) -> Result<()> {
        let data = match std::fs::read(&self.path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            read => {
                read.wrap_err_with(|| format!("Failed to read vault at {}", self.path.display()))?
            }
        };
        let current = sha256(&data);
        if current == disk.sha256 {
            return Ok(());
        }
        let (entries, sealer) = unseal(
            &data,
            |salt| disk.sealer.for_salt(&disk.passphrase, salt),
            file_mtime(&self.path).unwrap_or_else(Utc::now),
        )?;
        tracing::info!(
            path = %self.path.display(),
            "Reloaded a vault file another process replaced"
        );
        *self.entries.write() = entries;
        disk.sealer = sealer;
        disk.sha256 = current;
        Ok(())
    }

    fn ensure_writable(&self) -> Result<()> {
        if frozen_marker(&self.path).exists() {
            return Err(VaultError::Frozen {
                scope: self.path.display().to_string(),
            }
            .into());
        }
        Ok(())
    }

    fn record(&self, audit: Vec<AuditEntry<'static>>, ok: bool) {
        if let Some(log) = &self.audit {
            audit
                .into_iter()
                .for_each(|entry| log.record(AuditEntry { ok, ..entry }));
        }
    }
}

/// Marker file whose presence makes the vault file at `path` refuse writes.
pub fn frozen_marker(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".frozen");
    path.with_file_name(name)
}

/// The JSONL audit log kept beside a vault file, named after it:
/// `vault.enc` logs to `vault-audit.jsonl`, `sarah.enc` to `sarah-audit.jsonl`.
pub(super) fn audit_log_path(vault_file: &Path) -> PathBuf {
    let stem = vault_file.file_stem().unwrap_or_default().to_string_lossy();
    vault_file.with_file_name(format!("{stem}-audit.jsonl"))
}

/// A vault file's entries, the key sealing it, and the exact bytes read.
pub(super) fn load(
    path: &Path,
    master_key: &str,
) -> Result<(HashMap<String, VaultEntry>, Sealer, Vec<u8>)> {
    let data = std::fs::read(path)
        .wrap_err_with(|| format!("Failed to read vault at {}", path.display()))?;
    let (entries, sealer) = unseal(
        &data,
        |salt| Sealer::derive(master_key, salt),
        file_mtime(path).unwrap_or_else(Utc::now),
    )?;
    Ok((entries, sealer, data))
}

/// Salt and Argon2id-derived key a file is sealed with.
#[derive(Clone)]
pub(super) struct Sealer {
    salt: [u8; SALT_LEN],
    key: [u8; KEY_LEN],
}

impl Sealer {
    pub(super) fn generate(passphrase: &str) -> Result<Self> {
        let mut salt = [0u8; SALT_LEN];
        rand::thread_rng().fill_bytes(&mut salt);
        Self::derive(passphrase, salt)
    }

    fn derive(passphrase: &str, salt: [u8; SALT_LEN]) -> Result<Self> {
        let mut key = [0u8; KEY_LEN];
        Argon2::default()
            .hash_password_into(passphrase.as_bytes(), &salt, &mut key)
            .map_err(|e| eyre::eyre!("Key derivation failed: {e}"))?;
        Ok(Self { salt, key })
    }

    /// The sealer for a file salted with `salt`: this one when the salt is
    /// unchanged, otherwise the passphrase derived over the new salt.
    fn for_salt(&self, passphrase: &str, salt: [u8; SALT_LEN]) -> Result<Self> {
        if salt == self.salt {
            Ok(self.clone())
        } else {
            Self::derive(passphrase, salt)
        }
    }

    fn cipher(&self) -> Result<Aes256Gcm> {
        Aes256Gcm::new_from_slice(&self.key).map_err(|e| eyre::eyre!("Cipher init failed: {e}"))
    }

    /// Always emits v2.
    pub(super) fn seal(&self, entries: &HashMap<String, VaultEntry>) -> Result<Vec<u8>> {
        let plaintext = serde_json::to_vec(entries)?;
        let mut nonce = [0u8; NONCE_LEN];
        rand::thread_rng().fill_bytes(&mut nonce);
        let ciphertext = self
            .cipher()?
            .encrypt(Nonce::from_slice(&nonce), plaintext.as_ref())
            .map_err(|e| eyre::eyre!("Encryption failed: {e}"))?;
        Ok([MAGIC_V2.as_slice(), &self.salt, &nonce, &ciphertext].concat())
    }
}

/// Decrypt a v1 or v2 file with the sealer `sealer_for` gives for its salt;
/// v1 entries get `legacy` metadata dated `fallback_ts`. Per-user entries get
/// the owner rule applied.
fn unseal(
    data: &[u8],
    sealer_for: impl FnOnce([u8; SALT_LEN]) -> Result<Sealer>,
    fallback_ts: DateTime<Utc>,
) -> Result<(HashMap<String, VaultEntry>, Sealer)> {
    let (header, is_v2) = if data.starts_with(MAGIC_V2) {
        (MAGIC_V2.len(), true)
    } else {
        (0, false)
    };
    if data.len() < header + SALT_LEN + NONCE_LEN + 1 {
        bail!("Vault file too short — corrupted or empty");
    }
    let (salt, rest) = data[header..].split_at(SALT_LEN);
    let (nonce, ciphertext) = rest.split_at(NONCE_LEN);
    let sealer = sealer_for(salt.try_into()?)?;
    let plaintext = sealer
        .cipher()?
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .map_err(|_| eyre::eyre!("Decryption failed — wrong key or corrupted vault"))?;
    let entries: HashMap<String, VaultEntry> = if is_v2 {
        serde_json::from_slice(&plaintext)
            .wrap_err("Failed to parse decrypted v2 vault contents")?
    } else {
        serde_json::from_slice::<HashMap<String, String>>(&plaintext)
            .wrap_err("Failed to parse decrypted v1 vault contents")?
            .into_iter()
            .map(|(path, value)| (path, legacy_entry(value, fallback_ts)))
            .collect()
    };
    let entries = entries
        .into_iter()
        .map(|(path, entry)| {
            let entry = with_owner(&path, entry);
            (path, entry)
        })
        .collect();
    Ok((entries, sealer))
}

/// Write `data` to `path` through a uniquely named 0600 temp file that is
/// fsynced before the rename, then fsync the directory: readers only ever see
/// the old file or the new one, and concurrent writers never share a temp.
/// Once the rename lands the new file is what every reader sees, so a failed
/// directory fsync is logged rather than reported as a failed write.
pub(super) fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)
        .wrap_err_with(|| format!("Failed to create vault directory {}", dir.display()))?;
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(
        ".{}.{:016x}.tmp",
        std::process::id(),
        rand::random::<u64>()
    ));
    let tmp = path.with_file_name(tmp_name);
    let written = write_synced(&tmp, data).and_then(|()| {
        std::fs::rename(&tmp, path)
            .wrap_err_with(|| format!("Failed to rename temp vault to {}", path.display()))
    });
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written?;
    if let Err(e) = sync_dir(dir) {
        tracing::warn!(
            error = %e,
            "Vault file replaced, but a crash before its directory reaches disk could undo it"
        );
    }
    Ok(())
}

fn write_synced(path: &Path, data: &[u8]) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .wrap_err_with(|| format!("Failed to create temp vault at {}", path.display()))?;
    file.write_all(data)
        .and_then(|()| file.sync_all())
        .wrap_err_with(|| format!("Failed to write temp vault at {}", path.display()))
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> Result<()> {
    std::fs::File::open(dir)
        .and_then(|dir| dir.sync_all())
        .wrap_err_with(|| format!("Failed to sync vault directory {}", dir.display()))
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> Result<()> {
    Ok(())
}

fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

fn file_mtime(path: &Path) -> Option<DateTime<Utc>> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let secs = modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    DateTime::<Utc>::from_timestamp(secs, 0)
}

fn legacy_entry(value: String, ts: DateTime<Utc>) -> VaultEntry {
    let actor = VaultActor::Legacy.audit_string();
    VaultEntry {
        value,
        created_at: ts,
        updated_at: ts,
        created_by: actor.clone(),
        updated_by: actor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::{Vault, VaultError};

    fn test_key() -> String {
        "test-vault-passphrase-123".to_string()
    }

    /// Encode `secrets` in the legacy v1 format (no magic prefix, plaintext
    /// is `HashMap<String, String>`).
    fn encode_v1_blob(
        secrets: &HashMap<String, String>,
        salt: [u8; SALT_LEN],
        passphrase: &str,
    ) -> Vec<u8> {
        let sealer = Sealer::derive(passphrase, salt).unwrap();
        let plaintext = serde_json::to_vec(secrets).unwrap();
        let mut nonce = [0u8; NONCE_LEN];
        rand::thread_rng().fill_bytes(&mut nonce);
        let ciphertext = sealer
            .cipher()
            .unwrap()
            .encrypt(Nonce::from_slice(&nonce), plaintext.as_ref())
            .unwrap();
        [salt.as_slice(), &nonce, &ciphertext].concat()
    }

    fn random_salt() -> [u8; SALT_LEN] {
        let mut salt = [0u8; SALT_LEN];
        rand::thread_rng().fill_bytes(&mut salt);
        salt
    }

    fn set(vault: &FileVault, path: &str, value: &str) -> Result<Vec<bool>> {
        vault.apply(&[VaultOp::Set { path, value }], VaultActor::Cli)
    }

    #[test]
    fn open_creates_empty_vault_file() {
        let tmp = tempfile::tempdir().unwrap();
        let vault_path = tmp.path().join("vault.enc");
        let vault = FileVault::open(&vault_path, &test_key()).unwrap();
        assert!(vault_path.is_file());
        assert!(vault.read(HashMap::is_empty));
    }

    #[test]
    fn round_trip_persist_and_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let vault_path = tmp.path().join("vault.enc");

        let vault = FileVault::open(&vault_path, &test_key()).unwrap();
        set(&vault, "slack.bot_token", "xoxb-secret").unwrap();
        set(&vault, "github.pat", "ghp-secret").unwrap();
        drop(vault);

        let vault = Vault::open(&vault_path, &test_key()).unwrap();
        assert_eq!(vault.get("slack.bot_token").unwrap(), "xoxb-secret");
        assert_eq!(vault.get("github.pat").unwrap(), "ghp-secret");
        assert_eq!(vault.list(None).len(), 2);
    }

    #[test]
    fn empty_vault_persist_and_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let vault_path = tmp.path().join("vault.enc");
        drop(FileVault::open(&vault_path, &test_key()).unwrap());
        let vault = FileVault::open(&vault_path, &test_key()).unwrap();
        assert!(vault.read(HashMap::is_empty));
    }

    #[test]
    fn metadata_survives_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let vault_path = tmp.path().join("vault.enc");

        Vault::open(&vault_path, &test_key())
            .unwrap()
            .set_with_actor("secret", "value", VaultActor::System("oauth_callback"))
            .unwrap();

        let vault = Vault::open(&vault_path, &test_key()).unwrap();
        let entry = vault.get_entry("secret").unwrap();
        assert_eq!(entry.value, "value");
        assert_eq!(entry.created_by, "system:oauth_callback");
        assert_eq!(entry.updated_by, "system:oauth_callback");
    }

    #[test]
    fn wrong_key_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let vault_path = tmp.path().join("vault.enc");
        set(
            &FileVault::open(&vault_path, "correct-key").unwrap(),
            "secret",
            "value",
        )
        .unwrap();

        let err = FileVault::open(&vault_path, "wrong-key").err().unwrap();
        assert!(err.to_string().contains("wrong key or corrupted"));
    }

    #[test]
    fn rotate_key() {
        let tmp = tempfile::tempdir().unwrap();
        let vault_path = tmp.path().join("vault.enc");

        let vault = FileVault::open(&vault_path, "old-key").unwrap();
        set(&vault, "secret", "value").unwrap();
        vault.rotate_key("new-key").unwrap();
        set(&vault, "after", "rotation").unwrap();
        drop(vault);

        assert!(FileVault::open(&vault_path, "old-key").is_err());
        let vault = Vault::open(&vault_path, "new-key").unwrap();
        assert_eq!(vault.get("secret").unwrap(), "value");
        assert_eq!(vault.get("after").unwrap(), "rotation");
    }

    #[cfg(unix)]
    #[test]
    fn persisted_vault_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let vault_path = tmp.path().join("vault.enc");
        set(&FileVault::open(&vault_path, "test-key").unwrap(), "k", "v").unwrap();
        let mode = std::fs::metadata(&vault_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
    }

    #[test]
    fn v1_file_opens_with_legacy_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let vault_path = tmp.path().join("vault.enc");
        let secrets = HashMap::from([("slack.bot_token".to_string(), "xoxb-old".to_string())]);
        std::fs::write(
            &vault_path,
            encode_v1_blob(&secrets, random_salt(), &test_key()),
        )
        .unwrap();

        let vault = Vault::open(&vault_path, &test_key()).unwrap();
        assert_eq!(vault.get("slack.bot_token").unwrap(), "xoxb-old");
        let entry = vault.get_entry("slack.bot_token").unwrap();
        assert_eq!(entry.created_by, "legacy");
        assert_eq!(entry.updated_by, "legacy");
    }

    #[test]
    fn v1_file_persists_as_v2_after_first_write() {
        let tmp = tempfile::tempdir().unwrap();
        let vault_path = tmp.path().join("vault.enc");
        let secrets = HashMap::from([("legacy_key".to_string(), "legacy_value".to_string())]);
        std::fs::write(
            &vault_path,
            encode_v1_blob(&secrets, random_salt(), &test_key()),
        )
        .unwrap();
        assert!(!std::fs::read(&vault_path).unwrap().starts_with(MAGIC_V2));

        set(
            &FileVault::open(&vault_path, &test_key()).unwrap(),
            "new_key",
            "new_value",
        )
        .unwrap();
        assert!(std::fs::read(&vault_path).unwrap().starts_with(MAGIC_V2));

        let vault = Vault::open(&vault_path, &test_key()).unwrap();
        assert_eq!(vault.get("legacy_key").unwrap(), "legacy_value");
        assert_eq!(vault.get("new_key").unwrap(), "new_value");
        assert_eq!(vault.get_entry("legacy_key").unwrap().created_by, "legacy");
        assert_eq!(vault.get_entry("new_key").unwrap().created_by, "cli");
    }

    #[test]
    fn v1_salt_colliding_with_magic_fails_decryption_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        let vault_path = tmp.path().join("vault.enc");
        let mut salt = random_salt();
        salt[..MAGIC_V2.len()].copy_from_slice(MAGIC_V2);
        std::fs::write(
            &vault_path,
            encode_v1_blob(&HashMap::new(), salt, &test_key()),
        )
        .unwrap();

        let err = FileVault::open(&vault_path, &test_key())
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("wrong key or corrupted") || err.contains("too short"),
            "expected clean decryption-failed error, got: {err}"
        );
    }

    #[test]
    fn load_names_the_owner_of_legacy_per_user_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let vault_path = tmp.path().join("vault.enc");
        let now = Utc::now();
        let stored = HashMap::from([(
            "users/u1/mcp/v1/hubspot/refresh_token".to_string(),
            VaultEntry {
                value: "rt".into(),
                created_at: now,
                updated_at: now,
                created_by: "system:unknown".into(),
                updated_by: "system:unknown".into(),
            },
        )]);
        let sealer = Sealer::generate(&test_key()).unwrap();
        std::fs::write(&vault_path, sealer.seal(&stored).unwrap()).unwrap();

        let vault = Vault::open(&vault_path, &test_key()).unwrap();
        let entry = vault
            .get_entry("users/u1/mcp/v1/hubspot/refresh_token")
            .unwrap();
        assert_eq!(entry.created_by, "user:u1");
        assert_eq!(entry.updated_by, "system:unknown");
    }

    #[test]
    fn frozen_vault_refuses_writes_and_keeps_serving_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let vault_path = tmp.path().join("vault.enc");
        let vault = FileVault::open(&vault_path, &test_key()).unwrap();
        set(&vault, "k", "before").unwrap();
        std::fs::write(frozen_marker(&vault_path), "{}").unwrap();

        let err = set(&vault, "k", "after").unwrap_err();
        assert!(matches!(
            err.downcast_ref::<VaultError>(),
            Some(VaultError::Frozen { .. })
        ));
        assert!(vault.rotate_key("other").is_err());
        assert_eq!(
            vault.read(|entries| entries.get("k").map(|e| e.value.clone())),
            Some("before".to_string())
        );

        std::fs::remove_file(frozen_marker(&vault_path)).unwrap();
        set(&vault, "k", "after").unwrap();
    }

    #[test]
    fn failed_persist_leaves_memory_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("vaults");
        let vault_path = dir.join("vault.enc");
        let vault = FileVault::open(&vault_path, &test_key()).unwrap();
        set(&vault, "k", "persisted").unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::write(&dir, "not a directory").unwrap();

        assert!(set(&vault, "k", "lost").is_err());
        assert_eq!(
            vault.read(|entries| entries.get("k").map(|e| e.value.clone())),
            Some("persisted".to_string())
        );
    }

    #[test]
    fn writes_leave_no_temp_files_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = FileVault::open(&tmp.path().join("vault.enc"), &test_key()).unwrap();
        (0..5).for_each(|i| {
            set(&vault, &format!("k{i}"), "v").unwrap();
        });
        let names: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("vault.enc")]);
    }

    #[test]
    fn two_instances_on_one_file_never_fail_or_corrupt_it() {
        let tmp = tempfile::tempdir().unwrap();
        let vault_path = tmp.path().join("vault.enc");
        let instances: Vec<_> = (0..2)
            .map(|_| Arc::new(FileVault::open(&vault_path, &test_key()).unwrap()))
            .collect();

        let handles: Vec<_> = instances
            .iter()
            .enumerate()
            .flat_map(|(n, vault)| {
                (0..4).map(move |t| {
                    let vault = Arc::clone(vault);
                    std::thread::spawn(move || {
                        (0..25)
                            .try_for_each(|i| set(&vault, &format!("i{n}t{t}k{i}"), "v").map(drop))
                    })
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }

        FileVault::open(&vault_path, &test_key()).unwrap();
    }

    fn two_processes(vault_path: &Path) -> (FileVault, FileVault) {
        (
            FileVault::open(vault_path, &test_key()).unwrap(),
            FileVault::open(vault_path, &test_key()).unwrap(),
        )
    }

    #[test]
    fn a_write_keeps_what_another_process_wrote_since() {
        let tmp = tempfile::tempdir().unwrap();
        let vault_path = tmp.path().join("vault.enc");
        let (a, b) = two_processes(&vault_path);

        set(&a, "from-a", "1").unwrap();
        set(&b, "from-b", "2").unwrap();
        set(&a, "from-a-again", "3").unwrap();

        assert_eq!(
            Vault::open(&vault_path, &test_key()).unwrap().list(None),
            vec!["from-a", "from-a-again", "from-b"]
        );
    }

    #[test]
    fn sync_picks_up_a_file_another_process_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let vault_path = tmp.path().join("vault.enc");
        let (a, b) = two_processes(&vault_path);
        set(&b, "k", "v").unwrap();

        assert!(a.read(|entries| !entries.contains_key("k")));
        a.sync().unwrap();
        assert!(a.read(|entries| entries.contains_key("k")));
    }

    #[test]
    fn a_file_rekeyed_by_another_process_is_never_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let vault_path = tmp.path().join("vault.enc");
        let (a, b) = two_processes(&vault_path);
        set(&a, "k", "v").unwrap();
        b.rotate_key("another-key").unwrap();

        assert!(set(&a, "k", "clobbered").is_err());
        let vault = Vault::open(&vault_path, "another-key").unwrap();
        assert_eq!(vault.get("k").as_deref(), Some("v"));
    }

    #[test]
    fn each_vault_file_has_its_own_audit_log() {
        assert_eq!(
            audit_log_path(Path::new("/data/vault.enc")),
            Path::new("/data/vault-audit.jsonl")
        );
        assert_eq!(
            audit_log_path(Path::new("/data/sarah.enc")),
            Path::new("/data/sarah-audit.jsonl")
        );
    }
}
