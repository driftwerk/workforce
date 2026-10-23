//! Platform-native secret storage.
//!
//! Exposes a single [`LocalSecretStore`] with the same public surface on
//! every platform (`new`, `probe`, plus `workforce_common::settings::SecretStore`
//! for `get` / `set` / `delete` / `list`). What varies is the backing store:
//!
//! - **macOS / Windows**: the OS keychain via the `keyring` crate
//!   (`apple-native` / `windows-native` features). Items live under the
//!   service name `workforce-local-mcp` with the dotted path as the account.
//!
//! - **Linux (incl. WSL)**: a mode-0600 TOML file at
//!   `$XDG_DATA_HOME/workforce-local-mcp/secrets.toml` (default
//!   `~/.local/share/workforce-local-mcp/secrets.toml`). D-Bus Secret Service
//!   isn't reliably present on WSL or headless Linux boxes, and requiring
//!   `gnome-keyring-daemon` would push that papercut onto every dev — the
//!   file-backed store Just Works. Override the path with
//!   `WORKFORCE_LOCAL_MCP_SECRETS` for sandboxing / alternate locations.
//!
//! `list()` is intentionally a no-op on the keyring backends (the crate has
//! no native enumeration). Enumeration of configured workspaces happens via
//! an explicit JSON index entry maintained by the CLI — see
//! [`LocalSecretStore::get_string_list`] and the
//! `skills/<adapter>/workspaces_index` convention in `cli.rs`.

use eyre::Result;
use workforce_common::settings::SecretStore;

/// Richer result for `config show`-style probes that need to distinguish
/// "never set" from "set but this binary isn't on the ACL". The plain
/// [`SecretStore::get`] can't express that — it only returns `Option<String>`
/// — so the MCP runtime falls back to "adapter disabled" on any error, which
/// looks identical to "no creds" from the user's side.
#[derive(Debug)]
pub enum EntryStatus {
    Present(String),
    NotSet,
    /// The entry exists but the current binary can't read it. On macOS this
    /// happens when a differently-signed binary originally wrote the item —
    /// e.g. an unsigned dev build writes, then a signed brew-installed build
    /// reads. The keychain ACL lists the original trusted identities only.
    /// Never constructed by the Linux file-backed backend (no ACL to deny),
    /// but kept in the enum so `cli::show_config` can match exhaustively
    /// regardless of target.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    AccessDenied,
    Error(String),
}

// ── macOS / Windows: OS keychain via `keyring` ──────────────────────────────

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod backend {
    use super::{EntryStatus, Result, SecretStore};
    use eyre::WrapErr;
    use keyring::Entry;

    const SERVICE: &str = "workforce-local-mcp";

    #[derive(Default)]
    pub struct LocalSecretStore;

    impl LocalSecretStore {
        pub fn new() -> Self {
            Self
        }

        pub fn probe(&self, path: &str) -> EntryStatus {
            let entry = match Entry::new(SERVICE, path) {
                Ok(e) => e,
                Err(keyring::Error::NoEntry) => return EntryStatus::NotSet,
                Err(e) => return EntryStatus::Error(format!("opening {path}: {e}")),
            };
            match entry.get_password() {
                Ok(v) => EntryStatus::Present(v),
                Err(keyring::Error::NoEntry) => EntryStatus::NotSet,
                Err(keyring::Error::NoStorageAccess(_)) => EntryStatus::AccessDenied,
                Err(e) => EntryStatus::Error(format!("reading {path}: {e}")),
            }
        }
    }

    impl SecretStore for LocalSecretStore {
        fn get(&self, path: &str) -> Option<String> {
            Entry::new(SERVICE, path).ok()?.get_password().ok()
        }

        fn set(&self, path: &str, value: &str) -> Result<()> {
            let entry = Entry::new(SERVICE, path)
                .wrap_err_with(|| format!("opening keychain entry {path}"))?;
            entry
                .set_password(value)
                .map_err(|e| keychain_write_error(path, e))
        }

        fn delete(&self, path: &str) -> Result<bool> {
            match Entry::new(SERVICE, path) {
                Ok(entry) => match entry.delete_credential() {
                    Ok(()) => Ok(true),
                    Err(keyring::Error::NoEntry) => Ok(false),
                    Err(e) => Err(e).wrap_err_with(|| format!("deleting keychain entry {path}")),
                },
                Err(keyring::Error::NoEntry) => Ok(false),
                Err(e) => Err(e).wrap_err_with(|| format!("opening keychain entry {path}")),
            }
        }

        fn list(&self, _prefix: Option<&str>) -> Vec<String> {
            Vec::new()
        }
    }

    /// Wrap a keyring write error with a hint about the most common cause: macOS
    /// prompts for permission whenever the binary's code signature changes (which
    /// happens on every `cargo build`), and denying the prompt surfaces here as a
    /// "platform secure storage failure".
    fn keychain_write_error(path: &str, err: keyring::Error) -> eyre::Report {
        eyre::eyre!(
            "keychain write failed for {path}: {err}. \
             On macOS, click \"Always Allow\" on the Keychain prompt so subsequent \
             writes don't re-prompt after each rebuild. If you already denied, \
             re-run this command."
        )
    }
}

// ── Linux (incl. WSL): mode-0600 TOML file ──────────────────────────────────

#[cfg(target_os = "linux")]
mod backend {
    use super::{EntryStatus, Result, SecretStore};
    // `Context` in eyre 0.6 provides both `.context()` and `.wrap_err_with()`;
    // no separate `WrapErr` import needed.
    use eyre::Context;
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    const ENV_OVERRIDE: &str = "WORKFORCE_LOCAL_MCP_SECRETS";

    /// On-disk format is a plain top-level TOML table of `path = "value"`
    /// pairs. TOML quoted keys tolerate `/` separators cleanly, so paths like
    /// `"skills/jira/email"` round-trip without any escaping.
    type Secrets = BTreeMap<String, String>;

    pub struct LocalSecretStore {
        path: PathBuf,
        /// Serializes read-modify-write from concurrent CLI invocations on the
        /// same host. Not a cross-process lock, but the CLI is interactive and
        /// the MCP server only reads; a simple in-process Mutex is enough to
        /// avoid losing writes from two `config` subcommands running back to
        /// back in a single shell.
        lock: Mutex<()>,
    }

    impl LocalSecretStore {
        pub fn new() -> Self {
            Self {
                path: resolve_path(),
                lock: Mutex::new(()),
            }
        }

        /// Test-only constructor that bypasses env-var resolution so parallel
        /// tests can each point at their own tempfile without racing on the
        /// process-global `WORKFORCE_LOCAL_MCP_SECRETS`.
        #[cfg(test)]
        pub fn with_path(path: PathBuf) -> Self {
            Self {
                path,
                lock: Mutex::new(()),
            }
        }

        pub fn probe(&self, path: &str) -> EntryStatus {
            let _g = self.lock.lock().unwrap();
            match load(&self.path) {
                Ok(mut s) => match s.remove(path) {
                    Some(v) => EntryStatus::Present(v),
                    None => EntryStatus::NotSet,
                },
                Err(e) => EntryStatus::Error(e.to_string()),
            }
        }
    }

    impl SecretStore for LocalSecretStore {
        fn get(&self, path: &str) -> Option<String> {
            let _g = self.lock.lock().unwrap();
            load(&self.path).ok()?.remove(path)
        }

        fn set(&self, path: &str, value: &str) -> Result<()> {
            let _g = self.lock.lock().unwrap();
            let mut s = load(&self.path).unwrap_or_default();
            s.insert(path.to_string(), value.to_string());
            save(&self.path, &s)
        }

        fn delete(&self, path: &str) -> Result<bool> {
            let _g = self.lock.lock().unwrap();
            let mut s = load(&self.path).unwrap_or_default();
            let removed = s.remove(path).is_some();
            if removed {
                save(&self.path, &s)?;
            }
            Ok(removed)
        }

        fn list(&self, prefix: Option<&str>) -> Vec<String> {
            let _g = self.lock.lock().unwrap();
            let Ok(s) = load(&self.path) else {
                return Vec::new();
            };
            s.keys()
                .filter(|k| prefix.is_none_or(|p| k.starts_with(p)))
                .cloned()
                .collect()
        }
    }

    fn resolve_path() -> PathBuf {
        if let Ok(p) = std::env::var(ENV_OVERRIDE) {
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
        let base = std::env::var("XDG_DATA_HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".local/share")))
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        base.join("workforce-local-mcp").join("secrets.toml")
    }

    fn load(path: &Path) -> Result<Secrets> {
        if !path.exists() {
            return Ok(Secrets::default());
        }
        let data = fs::read_to_string(path)
            .wrap_err_with(|| format!("reading secrets file {}", path.display()))?;
        toml::from_str(&data).wrap_err_with(|| format!("parsing secrets file {}", path.display()))
    }

    fn save(path: &Path, s: &Secrets) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .wrap_err_with(|| format!("creating {}", parent.display()))?;
            // Tighten the parent to 0700 in case the caller's umask left it
            // world-traversable. Non-fatal if the fs doesn't support chmod
            // (e.g. a bind-mounted NTFS path under WSL) — the file itself is
            // the actual secret-bearing surface.
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
        let body = toml::to_string_pretty(s).context("serializing secrets")?;
        let tmp = path.with_extension("toml.tmp");
        {
            let mut f = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .wrap_err_with(|| format!("opening {}", tmp.display()))?;
            f.write_all(body.as_bytes())
                .wrap_err_with(|| format!("writing {}", tmp.display()))?;
            f.sync_all().ok();
        }
        fs::rename(&tmp, path)
            .wrap_err_with(|| format!("renaming {} into {}", tmp.display(), path.display()))?;
        Ok(())
    }
}

pub use backend::LocalSecretStore;

// ── Shared helpers on top of the backend-agnostic SecretStore surface ───────

/// JSON array of strings stored at a single secret-store path. Used by the
/// CLI to maintain the `skills/<adapter>/workspaces_index` entry — Keychain
/// has no native enumeration, so we keep an explicit index that both writes
/// (CLI) and reads (adapter loader) share.
impl LocalSecretStore {
    /// Read a path whose value is a JSON array of strings. Returns an empty
    /// Vec if the entry is unset; a malformed payload surfaces as an error
    /// so callers can decide whether to reset or bail.
    pub fn get_string_list(&self, path: &str) -> Result<Vec<String>> {
        use eyre::WrapErr;
        let Some(raw) = self.get(path) else {
            return Ok(Vec::new());
        };
        serde_json::from_str::<Vec<String>>(&raw)
            .wrap_err_with(|| format!("parsing JSON string list at {path}"))
    }

    /// Serialize `items` as a compact JSON array and write it to `path`.
    /// Writing an empty list is allowed and semantically equivalent to
    /// "entry present, no workspaces" — distinct from "entry missing".
    pub fn set_string_list(&self, path: &str, items: &[String]) -> Result<()> {
        let encoded = serde_json::to_string(items).expect("Vec<String> always serializes");
        self.set(path, &encoded)
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::*;

    fn unique_path(tag: &str) -> String {
        format!(
            "test/{tag}/{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    #[test]
    fn set_get_delete_round_trip() {
        let store = LocalSecretStore::new();
        let path = unique_path("round_trip");

        store.set(&path, "supersecret").unwrap();
        assert_eq!(store.get(&path).as_deref(), Some("supersecret"));

        assert!(store.delete(&path).unwrap());
        assert_eq!(store.get(&path), None);
        assert!(!store.delete(&path).unwrap(), "second delete is a no-op");
    }

    #[test]
    fn list_is_no_op() {
        let store = LocalSecretStore::new();
        assert!(store.list(None).is_empty());
        assert!(store.list(Some("skills/")).is_empty());
    }

    #[test]
    fn probe_unset_path_is_not_set() {
        let store = LocalSecretStore::new();
        let path = unique_path("probe_not_set");
        assert!(matches!(store.probe(&path), EntryStatus::NotSet));
    }

    #[test]
    fn probe_present_after_set() {
        let store = LocalSecretStore::new();
        let path = unique_path("probe_present");
        store.set(&path, "value").unwrap();
        match store.probe(&path) {
            EntryStatus::Present(v) => assert_eq!(v, "value"),
            other => panic!("expected Present, got {:?}", other),
        }
        store.delete(&path).unwrap();
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    //! The Linux backend is file-based so these tests run fully hermetically
    //! — no keychain, no daemon. Each test uses its own tempfile via the
    //! `with_path` test constructor, avoiding any dependency on the
    //! process-global `WORKFORCE_LOCAL_MCP_SECRETS` env var (which would race
    //! under cargo's default parallel-test execution).

    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn with_tmp_store<F: FnOnce(&LocalSecretStore, &std::path::Path)>(f: F) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.toml");
        let store = LocalSecretStore::with_path(path.clone());
        f(&store, &path);
    }

    #[test]
    fn round_trip_persists_to_disk() {
        with_tmp_store(|store, path| {
            store.set("skills/jira/email", "me@x.com").unwrap();
            store.set("skills/jira/api_token", "abc").unwrap();

            // A fresh store instance pointing at the same file must see the
            // writes (verifies the on-disk format, not just in-memory state).
            let store2 = LocalSecretStore::with_path(path.to_path_buf());
            assert_eq!(store2.get("skills/jira/email").as_deref(), Some("me@x.com"));
            assert_eq!(store2.get("skills/jira/api_token").as_deref(), Some("abc"));

            // File should be mode 0600.
            let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "secrets file must be user-read/write only");
        });
    }

    #[test]
    fn delete_is_idempotent() {
        with_tmp_store(|store, _| {
            store.set("skills/linear/api_key", "lin_xxx").unwrap();
            assert!(store.delete("skills/linear/api_key").unwrap());
            assert!(!store.delete("skills/linear/api_key").unwrap());
            assert_eq!(store.get("skills/linear/api_key"), None);
        });
    }

    #[test]
    fn list_respects_prefix() {
        with_tmp_store(|store, _| {
            store.set("skills/jira/email", "a").unwrap();
            store.set("skills/jira/site", "b").unwrap();
            store.set("skills/linear/api_key", "c").unwrap();

            let mut all = store.list(None);
            all.sort();
            assert_eq!(
                all,
                vec![
                    "skills/jira/email".to_string(),
                    "skills/jira/site".to_string(),
                    "skills/linear/api_key".to_string(),
                ]
            );

            let mut jira = store.list(Some("skills/jira/"));
            jira.sort();
            assert_eq!(
                jira,
                vec![
                    "skills/jira/email".to_string(),
                    "skills/jira/site".to_string(),
                ]
            );
        });
    }

    #[test]
    fn probe_matches_status() {
        with_tmp_store(|store, _| {
            assert!(matches!(
                store.probe("skills/jira/email"),
                EntryStatus::NotSet
            ));
            store.set("skills/jira/email", "me@x.com").unwrap();
            match store.probe("skills/jira/email") {
                EntryStatus::Present(v) => assert_eq!(v, "me@x.com"),
                other => panic!("expected Present, got {other:?}"),
            }
        });
    }
}
