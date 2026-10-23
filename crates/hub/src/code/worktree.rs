//! Git worktree manager for per-session repo isolation.
//!
//! Provides isolated git working directories for concurrent workflow instances
//! and DM sessions, preventing conflicts when multiple agents operate on the
//! same repository simultaneously.

use eyre::{eyre, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::git_hardening::{hardened_git, lock_down_config};

/// Hex digits of a checkout's digest in its base's name.
const BASE_KEY_LEN: usize = 16;

/// Metadata for a tracked worktree.
struct WorktreeEntry {
    path: PathBuf,
    last_access: Instant,
}

/// Manages git worktrees for per-session repo isolation.
///
/// Each workflow instance or DM session gets its own working directory,
/// created from a shared bare base clone. This allows unlimited concurrent
/// operations on the same repo without git conflicts.
pub struct WorktreeManager {
    bases_dir: PathBuf,
    worktrees_dir: PathBuf,
    /// scope_id → (repo → WorktreeEntry)
    overrides: RwLock<HashMap<String, HashMap<String, WorktreeEntry>>>,
    /// Serialize worktree create/remove to avoid git lock contention on the base.
    create_lock: tokio::sync::Mutex<()>,
}

/// Information about an active worktree.
#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub scope_id: String,
    pub repo: String,
    pub path: PathBuf,
}

/// Whether `repo` is a name a worktree may be keyed by: one or more plain path
/// segments, as `owner/repo` is.
///
/// The name is joined onto the bases directory, the worktrees directory and a
/// workspace's `repos/`, and it arrives from a model — a `code.*` argument, a
/// workflow step's `target_repo`. An absolute path replaces the directory it
/// is joined to and `..` climbs out of it, so either would key a worktree to
/// another session's tree or to any repository the hub can read.
pub fn is_repo_name(repo: &str) -> bool {
    !repo.is_empty()
        && !repo.contains(['\\', '\0'])
        && repo
            .split('/')
            .all(|segment| !matches!(segment, "" | "." | ".."))
}

fn checked_repo_name(repo: &str) -> Result<&str> {
    match is_repo_name(repo) {
        true => Ok(repo),
        false => Err(eyre!("'{repo}' is not a repository name")),
    }
}

/// Resolve the directory that holds ephemeral worktree working trees, matching
/// [`WorktreeManager::new_with_worktree_root`]. Callers that need this path
/// without owning a manager — the path-access guard for `code.*`/exec tools and
/// stale-worktree cleanup — use this so every site agrees on one location.
/// `worktree_root` overrides `data_dir` when set; the persistent
/// `worktree-bases/` mirrors always stay under `data_dir`.
pub fn worktrees_dir(data_dir: &Path, worktree_root: Option<&Path>) -> PathBuf {
    worktree_root.unwrap_or(data_dir).join("worktrees")
}

/// Create `dir` (if absent) and mark it as a shared, group-writable tree the hub
/// (`workforce`) and the agent runner (`agent`, same group) both write into
/// (audit F7): setgid (so descendants inherit the `workforce` group without a
/// recursive chown) + `g+rwX`, i.e. mode `0o2775`. The hub creates the dir under
/// its own `workforce` egid, so it is already group `workforce` — only the mode
/// needs widening, which needs no chgrp syscall (and no extra crate).
///
/// The mode is applied unconditionally after `create_dir_all`, so a dir left
/// from a prior boot is re-tightened to setgid every start (`create_dir_all` is
/// a no-op on an existing dir and would otherwise keep its old mode). On non-Unix
/// or when the process lacks permission (dev/macOS), the chmod is best-effort and
/// never fails startup — the dispatched-executor model is Linux/container-only.
pub fn ensure_shared_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| eyre!("create shared dir {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // setgid (0o2000) + rwxrwxr-x (0o0775), keeping a sticky bit that
        // protects the hub's entries (an agent workspace). Best-effort: a dir
        // on a host mount the hub does not own can't be chmod'd, and that is
        // fine.
        let sticky = std::fs::metadata(dir).map_or(0, |meta| meta.permissions().mode() & 0o1000);
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o2775 | sticky));
    }
    Ok(())
}

impl WorktreeManager {
    /// Create a new WorktreeManager. Creates base and worktree dirs if missing.
    ///
    /// Canonicalises `data_dir` to an absolute path. If left relative, the
    /// `wt_path` arg passed to `git worktree add` (run with
    /// `current_dir(&base_path)`) would resolve under the bare repo dir, hiding
    /// worktrees from later `Command::spawn` calls that look for them at the
    /// configured absolute path.
    pub fn new(data_dir: &Path) -> Result<Self> {
        Self::new_with_worktree_root(data_dir, None)
    }

    /// Like [`new`](Self::new), but routes the ephemeral `worktrees/` working
    /// trees to `worktree_root` (intended to be fast instance-local disk),
    /// while the persistent `worktree-bases/` git mirrors stay under
    /// `data_dir`. When `worktree_root` is `None`, both live under `data_dir`
    /// (legacy behaviour).
    ///
    /// Build I/O in a worktree is dominated by small-file latency, so keeping
    /// the working trees off an EFS/NFS `data_dir` is the dominant lever on
    /// coding-session build times. `worktree_root` is canonicalised for the
    /// same reason `data_dir` is (see [`new`](Self::new)).
    pub fn new_with_worktree_root(data_dir: &Path, worktree_root: Option<&Path>) -> Result<Self> {
        std::fs::create_dir_all(data_dir)
            .map_err(|e| eyre!("create data_dir {}: {e}", data_dir.display()))?;
        let data_dir = std::fs::canonicalize(data_dir)
            .map_err(|e| eyre!("canonicalize data_dir {}: {e}", data_dir.display()))?;
        let worktree_root = worktree_root
            .map(|root| {
                std::fs::create_dir_all(root)
                    .map_err(|e| eyre!("create worktree_root {}: {e}", root.display()))?;
                std::fs::canonicalize(root)
                    .map_err(|e| eyre!("canonicalize worktree_root {}: {e}", root.display()))
            })
            .transpose()?;
        let bases_dir = data_dir.join("worktree-bases");
        let worktrees_dir = worktrees_dir(&data_dir, worktree_root.as_deref());
        // Create both as shared, group-writable+setgid trees so the agent runner
        // (dispatched git/cargo) can write `.git`/objects/`target/` the hub
        // created, and vice versa (audit F7). This perms the RESOLVED
        // `worktrees_dir` — under `worktree_root` when configured — so a custom
        // `worktree_root_path` is covered without the init script hardcoding
        // `/app/data/worktrees` (audit S1).
        ensure_shared_dir(&bases_dir)?;
        ensure_shared_dir(&worktrees_dir)?;
        Ok(Self {
            bases_dir,
            worktrees_dir,
            overrides: RwLock::new(HashMap::new()),
            create_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Extract a scope_id from a session key.
    ///
    /// - `"workflow:{id}:{step}"` → `"wf-{id}"`
    /// - `"slack:{ws}:{ch}:{ts}"` → `"dm-{short_hash}"`
    /// - Other → `"ses-{short_hash}"`
    pub fn scope_id_for_session(session_key: &str) -> String {
        if let Some(rest) = session_key.strip_prefix("workflow:") {
            // workflow:{instance_id}:{step_id} → wf-{instance_id}
            if let Some(id) = rest.split(':').next() {
                return format!("wf-{}", id);
            }
        }
        // DM/channel sessions: hash the key for a stable, filesystem-safe identifier
        let mut hasher = Sha256::new();
        hasher.update(session_key.as_bytes());
        let hash = hex::encode(hasher.finalize());
        format!("dm-{}", &hash[..12])
    }

    /// The bare base that worktrees of `repo`, checked out at `source`, are
    /// made from: `bases_dir/owner/repo.<checkout>.git`. It is keyed by the
    /// checkout as well as the name, because two agents' checkouts of one name
    /// may hold different content, and a base carries the credentials of the
    /// checkout it was made for.
    fn base_path(&self, repo: &str, source: &Path) -> PathBuf {
        let source = std::fs::canonicalize(source).unwrap_or_else(|_| source.to_path_buf());
        let checkout = hex::encode(Sha256::digest(source.as_os_str().as_encoded_bytes()));
        self.bases_dir
            .join(format!("{repo}.{}.git", &checkout[..BASE_KEY_LEN]))
    }

    /// The base `wt_path` was made from, whichever layout named it. The
    /// worktree's `.git` file says, but the agent can rewrite it, so it is
    /// believed only when it names a worktree of a base under `bases_dir` that
    /// names this worktree back; git never runs where the agent points it.
    fn base_of(&self, wt_path: &Path) -> Option<PathBuf> {
        let pointer = std::fs::read_to_string(wt_path.join(".git")).ok()?;
        let admin = wt_path
            .join(pointer.strip_prefix("gitdir:")?.trim())
            .canonicalize()
            .ok()?;
        let worktrees = admin.parent()?;
        let base = worktrees.parent()?;
        let names_back = std::fs::read_to_string(admin.join("gitdir"))
            .ok()
            .and_then(|back| Path::new(back.trim()).parent()?.canonicalize().ok())
            .is_some_and(|back| Some(back) == wt_path.canonicalize().ok());
        (worktrees.file_name()? == "worktrees"
            && base.extension()? == "git"
            && base.starts_with(self.bases_dir.canonicalize().ok()?)
            && names_back)
            .then(|| base.to_path_buf())
    }

    /// Ensure the bare base clone `base_path` of `repo` exists.
    /// If missing, clones from the provided source (an existing agent workspace clone).
    /// Always fetches origin to stay up to date.
    async fn ensure_base(
        &self,
        base_path: PathBuf,
        repo: &str,
        source: &Path,
        github_token: Option<&str>,
    ) -> Result<PathBuf> {
        if !base_path.exists() {
            info!(repo = %repo, base = %base_path.display(), source = %source.display(),
                  "Creating bare base clone for worktrees");

            if let Some(parent) = base_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            let output = hardened_git()
                .args(["clone", "--bare"])
                .arg(source)
                .arg(&base_path)
                .output()
                .await?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(eyre!("Failed to create bare base: {}", stderr));
            }

            // The bare clone's origin points to the local source path, but we
            // need it to point to the real GitHub remote so worktrees can push.
            // Copy the remote URL from the source repo to the bare base.
            if let Ok(url_output) = hardened_git()
                .args(["config", "--get", "remote.origin.url"])
                .current_dir(source)
                .output()
                .await
            {
                let url = String::from_utf8_lossy(&url_output.stdout)
                    .trim()
                    .to_string();
                if !url.is_empty() && !url.starts_with('/') {
                    // Only override if it's a real remote URL, not a local path
                    let _ = hardened_git()
                        .args(["remote", "set-url", "origin", &url])
                        .current_dir(&base_path)
                        .output()
                        .await;
                }
            }
        }

        // Re-apply on every call, not just initial creation: pre-existing clones
        // and rotated tokens both leave the base without a working helper.
        Self::ensure_credential_helper(&base_path, source, github_token).await;

        // `git clone --bare` does not configure a fetch refspec, so the
        // subsequent `git fetch origin` would not populate
        // `refs/remotes/origin/*` (which `detect_default_ref` relies on).
        // Set it idempotently on every call so bare clones missing the
        // refspec self-heal.
        let _ = hardened_git()
            .args([
                "config",
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*",
            ])
            .current_dir(&base_path)
            .output()
            .await;

        let output = hardened_git()
            .args(["fetch", "origin"])
            .current_dir(&base_path)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!(repo = %repo, error = %stderr, "Failed to fetch origin on base (non-fatal)");
        }

        lock_down_config(&base_path);

        Ok(base_path)
    }

    /// Ensure `target` has a working `credential.helper`.
    ///
    /// Resolution order:
    /// 1. `vault_token` — if provided, build a fresh helper from it (preferred).
    ///    Also back-propagates to `source` so future non-vault callers
    ///    (e.g. DM sessions) pick up the rotated token.
    /// 2. Fall back to copying the helper from `source` (legacy behaviour).
    async fn ensure_credential_helper(target: &Path, source: &Path, vault_token: Option<&str>) {
        if let Some(token) = vault_token.filter(|t| !t.is_empty()) {
            let fresh_helper = format!(
                "!f() {{ echo username=x-access-token; echo password={}; }}; f",
                token
            );

            Self::set_credential_helper(target, &fresh_helper).await;

            if Self::read_credential_helper(source).await != fresh_helper {
                Self::set_credential_helper(source, &fresh_helper).await;
            }

            return;
        }

        let source_helper = Self::read_credential_helper(source).await;

        if source_helper.is_empty() {
            warn!(
                source = %source.display(),
                target = %target.display(),
                "Source clone has no credential.helper and no vault token available — \
                 git fetch/push from worktrees will fail. Set skills/github/token in \
                 the vault or re-clone via the github_local skill."
            );
            return;
        }

        let existing = Self::read_credential_helper(target).await;
        if existing == source_helper {
            return;
        }

        Self::set_credential_helper(target, &source_helper).await;
    }

    async fn read_credential_helper(dir: &Path) -> String {
        hardened_git()
            .args(["config", "--local", "credential.helper"])
            .current_dir(dir)
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    }

    async fn set_credential_helper(dir: &Path, helper: &str) {
        let _ = hardened_git()
            .args(["config", "--local", "credential.helper", helper])
            .current_dir(dir)
            .output()
            .await;
    }

    /// Resolve a working tree's git config dir. For a linked worktree, `.git` is
    /// a file pointing at `gitdir: <commondir>/worktrees/<name>`; its own
    /// `config.worktree` lives there while shared `config` lives in the bare
    /// base. We lock the base config (shared, governs filters on checkout) in
    /// `ensure_base`; here we additionally lock the per-worktree `config.worktree`
    /// when an `extensions.worktreeConfig` setup isolates it.
    async fn worktree_git_dir(wt_path: &Path) -> Option<PathBuf> {
        let output = hardened_git()
            .args(["rev-parse", "--git-dir"])
            .current_dir(wt_path)
            .output()
            .await
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if raw.is_empty() {
            return None;
        }
        let p = PathBuf::from(&raw);
        Some(if p.is_absolute() { p } else { wt_path.join(p) })
    }

    /// Detect the default branch/ref to use when creating worktrees from the bare base.
    /// Tries origin/HEAD → origin/main → origin/master → HEAD (for local-only repos).
    async fn detect_default_ref(base_path: &Path) -> String {
        if let Ok(output) = hardened_git()
            .args(["symbolic-ref", "refs/remotes/origin/HEAD"])
            .current_dir(base_path)
            .output()
            .await
        {
            if output.status.success() {
                let refname = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if let Some(branch) = refname.strip_prefix("refs/remotes/origin/") {
                    return format!("origin/{}", branch);
                }
            }
        }

        for branch in &["origin/main", "origin/master"] {
            if let Ok(output) = hardened_git()
                .args(["rev-parse", "--verify", branch])
                .current_dir(base_path)
                .output()
                .await
            {
                if output.status.success() {
                    return branch.to_string();
                }
            }
        }

        // Fallback to HEAD (works for bare repos cloned from local repos without a remote)
        "HEAD".to_string()
    }

    /// Create a worktree for the given scope and repo.
    ///
    /// The `source` path is an existing clone (e.g., from an agent's workspace)
    /// used to bootstrap the bare base if it doesn't exist yet.
    pub async fn create_worktree(
        &self,
        scope_id: &str,
        repo: &str,
        source: &Path,
        github_token: Option<&str>,
    ) -> Result<PathBuf> {
        let repo = checked_repo_name(repo)?;
        let _lock = self.create_lock.lock().await;

        // Worktree path: worktrees_dir/{scope_id}/{owner}/{repo}
        let wt_path = self.worktrees_dir.join(scope_id).join(repo);
        let existing_base = wt_path.exists().then(|| self.base_of(&wt_path)).flatten();
        let base_path = self
            .ensure_base(
                existing_base.unwrap_or_else(|| self.base_path(repo, source)),
                repo,
                source,
                github_token,
            )
            .await?;

        if wt_path.exists() {
            self.register_override(scope_id, repo, wt_path.clone());
            return Ok(wt_path);
        }

        if let Some(parent) = wt_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let default_ref = Self::detect_default_ref(&base_path).await;

        let output = hardened_git()
            .args(["worktree", "add", "--detach"])
            .arg(&wt_path)
            .arg(&default_ref)
            .current_dir(&base_path)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(eyre!(
                "Failed to create worktree for {} (scope {}): {}",
                repo,
                scope_id,
                stderr
            ));
        }

        // Reapply per-worktree: `extensions.worktreeConfig` setups isolate the
        // credential helper from the bare's config, so inheritance is not enough.
        Self::ensure_credential_helper(&wt_path, &base_path, github_token).await;

        // The worktree's own `config.worktree` lives under the base, not beside
        // the worktree, so both need locking.
        lock_down_config(&base_path);
        if let Some(git_dir) = Self::worktree_git_dir(&wt_path).await {
            lock_down_config(&git_dir);
        }

        info!(scope = %scope_id, repo = %repo, path = %wt_path.display(), "Created worktree");

        self.register_override(scope_id, repo, wt_path.clone());

        Ok(wt_path)
    }

    /// Register a worktree override in the in-memory map.
    fn register_override(&self, scope_id: &str, repo: &str, path: PathBuf) {
        let entry = WorktreeEntry {
            path,
            last_access: Instant::now(),
        };
        // Use try_write to avoid blocking. If contended (very rare), silently skip —
        // the next resolve_for_session call will find the worktree on disk and re-register.
        if let Ok(mut overrides) = self.overrides.try_write() {
            overrides
                .entry(scope_id.to_string())
                .or_default()
                .insert(repo.to_string(), entry);
        }
    }

    /// Remove all worktrees for the given scope.
    pub async fn remove_worktree(&self, scope_id: &str) -> Result<()> {
        let _lock = self.create_lock.lock().await;

        let entries: Vec<(String, PathBuf)> = {
            let mut overrides = self.overrides.write().await;
            if let Some(repos) = overrides.remove(scope_id) {
                repos
                    .into_iter()
                    .map(|(repo, entry)| (repo, entry.path))
                    .collect()
            } else {
                vec![]
            }
        };

        for (repo, wt_path) in &entries {
            let Some(base_path) = self.base_of(wt_path) else {
                let _ = std::fs::remove_dir_all(wt_path);
                continue;
            };
            let output = hardened_git()
                .args(["worktree", "remove", "--force"])
                .arg(wt_path)
                .current_dir(&base_path)
                .output()
                .await;

            match output {
                Ok(o) if !o.status.success() => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    warn!(scope = %scope_id, repo = %repo, error = %stderr,
                          "git worktree remove failed, cleaning up manually");
                    let _ = std::fs::remove_dir_all(wt_path);
                }
                Err(e) => {
                    warn!(scope = %scope_id, repo = %repo, error = %e,
                          "Failed to run git worktree remove, cleaning up manually");
                    let _ = std::fs::remove_dir_all(wt_path);
                }
                _ => {}
            }
        }

        let scope_dir = self.worktrees_dir.join(scope_id);
        if scope_dir.exists() {
            let _ = std::fs::remove_dir_all(&scope_dir);
        }

        if !entries.is_empty() {
            info!(scope = %scope_id, count = entries.len(), "Removed worktrees");
        }

        Ok(())
    }

    /// Resolve a worktree path for the given session key and repo.
    /// Returns `None` if no worktree exists for this session/repo.
    pub fn resolve_for_session(&self, session_key: &str, repo: &str) -> Option<PathBuf> {
        if !is_repo_name(repo) {
            return None;
        }
        let scope_id = Self::scope_id_for_session(session_key);

        if let Ok(mut overrides) = self.overrides.try_write() {
            if let Some(repos) = overrides.get_mut(&scope_id) {
                if let Some(entry) = repos.get_mut(repo) {
                    entry.last_access = Instant::now();
                    return Some(entry.path.clone());
                }
            }
        } else if let Ok(overrides) = self.overrides.try_read() {
            // Fallback: read-only access (skip timestamp update)
            if let Some(repos) = overrides.get(&scope_id) {
                if let Some(entry) = repos.get(repo) {
                    return Some(entry.path.clone());
                }
            }
        }

        // Check if a worktree directory exists on disk (e.g., after restart)
        let wt_path = self.worktrees_dir.join(&scope_id).join(repo);
        if wt_path.join(".git").exists() {
            self.register_override(&scope_id, repo, wt_path.clone());
            return Some(wt_path);
        }

        None
    }

    /// Resolve any active worktree root for the given session, regardless of repo.
    ///
    /// Returns the first worktree path if one exists for this session's scope.
    /// Used by `validate_path` to resolve relative file paths within the correct
    /// worktree so that `code.read README.md` goes to the worktree, not the workspace.
    pub fn resolve_session_root(&self, session_key: &str) -> Option<PathBuf> {
        let scope_id = Self::scope_id_for_session(session_key);

        // Check in-memory overrides first (fast path)
        if let Ok(overrides) = self.overrides.try_read() {
            if let Some(repos) = overrides.get(&scope_id) {
                if let Some(entry) = repos.values().next() {
                    return Some(entry.path.clone());
                }
            }
        }

        // Check disk (e.g. after container restart)
        let scope_dir = self.worktrees_dir.join(&scope_id);
        if scope_dir.exists() {
            // Walk scope_dir/{owner}/{repo} looking for a .git marker
            if let Ok(owners) = std::fs::read_dir(&scope_dir) {
                for owner in owners.flatten() {
                    if let Ok(repos) = std::fs::read_dir(owner.path()) {
                        for repo in repos.flatten() {
                            if repo.path().join(".git").exists() {
                                return Some(repo.path());
                            }
                        }
                    }
                }
            }
        }

        None
    }

    /// Ensure a worktree exists for the given session, creating one lazily if needed.
    /// Used for DM/channel sessions where worktrees are created on first repo access.
    pub async fn ensure_for_session(
        &self,
        session_key: &str,
        repo: &str,
        agent_workspace: &Path,
        github_token: Option<&str>,
    ) -> Result<PathBuf> {
        let repo = checked_repo_name(repo)?;
        let scope_id = Self::scope_id_for_session(session_key);

        if let Some(path) = self.resolve_for_session(session_key, repo) {
            if path.exists() {
                return Ok(path);
            }
        }

        let source = agent_workspace.join("repos").join(repo);
        if !source.join(".git").exists() {
            return Err(eyre!("Source repo not found at {}", source.display()));
        }

        self.create_worktree(&scope_id, repo, &source, github_token)
            .await
    }

    /// Remove stale worktrees: workflow worktrees not in the active set,
    /// and DM worktrees that exceed the TTL.
    ///
    /// `grace_period` controls how long terminal-state workflow worktrees are
    /// preserved after their last filesystem modification, allowing post-mortem
    /// debugging of failed workflows.
    pub async fn cleanup_stale(
        &self,
        active_workflow_ids: &HashSet<String>,
        ttl: Duration,
        grace_period: Duration,
    ) -> Result<usize> {
        let mut removed = 0;

        let entries = match std::fs::read_dir(&self.worktrees_dir) {
            Ok(e) => e,
            Err(_) => return Ok(0),
        };

        let mut stale_scopes = vec![];

        for entry in entries.flatten() {
            let scope_id = entry.file_name().to_string_lossy().to_string();

            if let Some(wf_id) = scope_id.strip_prefix("wf-") {
                // Workflow worktree: preserve if the instance is active (running/paused)
                if active_workflow_ids.contains(wf_id) {
                    continue;
                }
                // Not active — check grace period before removing
                let path = entry.path();
                let past_grace = match std::fs::metadata(&path).and_then(|m| m.modified()) {
                    Ok(modified) => modified.elapsed().unwrap_or(Duration::MAX) > grace_period,
                    Err(_) => true, // Can't read mtime — treat as stale
                };
                if past_grace {
                    info!(scope = %scope_id, "Cleaning up stale workflow worktree (not active, past grace period)");
                    stale_scopes.push(scope_id);
                } else {
                    info!(scope = %scope_id, "Preserving workflow worktree (within grace period)");
                }
            } else if scope_id.starts_with("dm-") {
                // DM worktree: check last-access TTL
                let should_remove = {
                    let overrides = self.overrides.read().await;
                    if let Some(repos) = overrides.get(&scope_id) {
                        repos.values().all(|e| e.last_access.elapsed() > ttl)
                    } else {
                        // Not tracked in memory — check filesystem mtime
                        let path = entry.path();
                        match std::fs::metadata(&path).and_then(|m| m.modified()) {
                            Ok(modified) => modified.elapsed().unwrap_or(Duration::MAX) > ttl,
                            Err(_) => true,
                        }
                    }
                };
                if should_remove {
                    info!(scope = %scope_id, "Cleaning up stale DM worktree (past TTL)");
                    stale_scopes.push(scope_id);
                }
            }
        }

        for scope_id in stale_scopes {
            if let Err(e) = self.remove_worktree(&scope_id).await {
                warn!(scope = %scope_id, error = %e, "Failed to clean up stale worktree");
            } else {
                removed += 1;
            }
        }

        // Prune dangling worktree refs in bare bases (bases_dir/{owner}/{repo}[.{checkout}].git)
        if let Ok(owners) = std::fs::read_dir(&self.bases_dir) {
            for owner_entry in owners.flatten() {
                if let Ok(repos) = std::fs::read_dir(owner_entry.path()) {
                    for repo_entry in repos.flatten() {
                        let base_path = repo_entry.path();
                        if base_path.extension().is_some_and(|e| e == "git") {
                            let _ = hardened_git()
                                .args(["worktree", "prune"])
                                .current_dir(&base_path)
                                .output()
                                .await;
                        }
                    }
                }
            }
        }

        if removed > 0 {
            info!(count = removed, "Cleaned up stale worktrees");
        }

        Ok(removed)
    }

    /// Spawn a periodic background task that cleans up stale worktrees.
    ///
    /// `active_ids_fn` is called each tick to query the current set of active
    /// (running + paused) workflow instance IDs. Worktrees belonging to those
    /// instances are preserved. Terminal-state worktrees are preserved for
    /// `grace_period` after their last modification. In a clustered
    /// deployment only the leader sweeps, and only when the preserve-set is
    /// backed by a fleet-shared store (`preserve_set_is_shared`) — a
    /// per-instance preserve-set cannot see peers' active workflows and would
    /// delete their live worktrees.
    pub fn spawn_cleanup_task(
        self: &std::sync::Arc<Self>,
        interval: Duration,
        ttl: Duration,
        grace_period: Duration,
        active_ids_fn: Arc<dyn Fn() -> HashSet<String> + Send + Sync + 'static>,
        leader_gate: Arc<crate::leader_gate::LeaderGate>,
        preserve_set_is_shared: bool,
    ) {
        let mgr = std::sync::Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            loop {
                tick.tick().await;
                if leader_gate.is_clustered() && !preserve_set_is_shared {
                    continue; // logged once by the startup sweep
                }
                if !leader_gate.is_leader() {
                    continue;
                }
                let active = active_ids_fn();
                if let Err(e) = mgr.cleanup_stale(&active, ttl, grace_period).await {
                    warn!(error = %e, "Worktree cleanup task failed");
                }
            }
        });
    }

    /// List all active worktrees.
    pub async fn list_worktrees(&self) -> Vec<WorktreeInfo> {
        let overrides = self.overrides.read().await;
        let mut result = Vec::new();
        for (scope_id, repos) in overrides.iter() {
            for (repo, entry) in repos {
                result.push(WorktreeInfo {
                    scope_id: scope_id.clone(),
                    repo: repo.clone(),
                    path: entry.path.clone(),
                });
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;
    use tempfile::TempDir;
    use tokio::process::Command;

    /// An agent workspace is made shared at every load, after it was made
    /// sticky: the sticky bit stays.
    #[cfg(unix)]
    #[test]
    fn a_shared_dir_keeps_its_sticky_bit() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let mode = |dir: &Path| std::fs::metadata(dir).unwrap().permissions().mode() & 0o7777;
        let (plain, sticky) = (tmp.path().join("plain"), tmp.path().join("sticky"));
        ensure_shared_dir(&plain).unwrap();
        std::fs::create_dir(&sticky).unwrap();
        std::fs::set_permissions(&sticky, std::fs::Permissions::from_mode(0o1755)).unwrap();
        ensure_shared_dir(&sticky).unwrap();
        assert_eq!(mode(&plain), 0o2775);
        assert_eq!(mode(&sticky), 0o3775);
    }

    fn has_git() -> bool {
        StdCommand::new("git").arg("--version").output().is_ok()
    }

    #[test]
    fn worktree_root_routes_trees_off_data_dir_but_keeps_bases() {
        let temp = TempDir::new().unwrap();
        let data = temp.path().join("data");
        let scratch = temp.path().join("scratch");
        let mgr = WorktreeManager::new_with_worktree_root(&data, Some(&scratch)).unwrap();

        // Working trees live under the (fast-local) worktree_root; the
        // persistent bare mirrors stay under data_dir.
        let scratch_canon = std::fs::canonicalize(&scratch).unwrap();
        let data_canon = std::fs::canonicalize(&data).unwrap();
        assert!(mgr.worktrees_dir.starts_with(&scratch_canon));
        assert!(mgr.bases_dir.starts_with(&data_canon));

        // The free-fn resolver (used by allowed-path guards + cleanup) agrees.
        assert_eq!(
            worktrees_dir(&data, Some(&scratch)),
            scratch.join("worktrees")
        );
        assert_eq!(worktrees_dir(&data, None), data.join("worktrees"));
    }

    #[test]
    fn worktree_root_unset_keeps_legacy_layout() {
        let temp = TempDir::new().unwrap();
        let data = temp.path().join("data");
        let mgr = WorktreeManager::new(&data).unwrap();
        let data_canon = std::fs::canonicalize(&data).unwrap();
        assert!(mgr.worktrees_dir.starts_with(&data_canon));
        assert!(mgr.bases_dir.starts_with(&data_canon));
    }

    /// The created worktree/base dirs must be setgid + group-writable (mode
    /// `0o2775`) so the dispatched agent runner and the hub (shared group) both
    /// write the same `.git`/`target/` trees (audit F7). This must hold whether
    /// or not `worktree_root` is configured (audit S1: a custom root still gets
    /// permed because the hub creates the resolved path, not the init script).
    #[cfg(unix)]
    #[test]
    fn created_worktree_dirs_are_setgid_group_writable() {
        use std::os::unix::fs::PermissionsExt;

        let assert_shared = |dir: &Path| {
            let mode = std::fs::metadata(dir).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o2000,
                0o2000,
                "{} must have the setgid bit (mode {:o})",
                dir.display(),
                mode
            );
            assert_eq!(
                mode & 0o070,
                0o070,
                "{} must be group rwx (mode {:o})",
                dir.display(),
                mode
            );
        };

        // worktree_root unset → both under data_dir.
        let temp = TempDir::new().unwrap();
        let data = temp.path().join("data");
        let mgr = WorktreeManager::new(&data).unwrap();
        assert_shared(&mgr.worktrees_dir);
        assert_shared(&mgr.bases_dir);

        // worktree_root set → resolved worktrees_dir still permed (S1).
        let data2 = temp.path().join("data2");
        let scratch = temp.path().join("scratch");
        let mgr2 = WorktreeManager::new_with_worktree_root(&data2, Some(&scratch)).unwrap();
        assert_shared(&mgr2.worktrees_dir);
        assert_shared(&mgr2.bases_dir);
    }

    fn create_test_repo(dir: &Path) {
        StdCommand::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir)
            .output()
            .unwrap();
        std::fs::write(dir.join("README.md"), "# Test").unwrap();
        StdCommand::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir)
            .output()
            .unwrap();
    }

    #[test]
    fn scope_id_extraction() {
        assert_eq!(
            WorktreeManager::scope_id_for_session("workflow:abc123:implement"),
            "wf-abc123"
        );
        assert_eq!(
            WorktreeManager::scope_id_for_session("workflow:def456:review"),
            "wf-def456"
        );

        // DM sessions get hashed
        let dm1 = WorktreeManager::scope_id_for_session("slack:ws1:C123:ts456");
        assert!(dm1.starts_with("dm-"));
        assert_eq!(dm1.len(), 15); // "dm-" + 12 hex chars

        // Same session key → same scope
        let dm2 = WorktreeManager::scope_id_for_session("slack:ws1:C123:ts456");
        assert_eq!(dm1, dm2);

        // Different session key → different scope
        let dm3 = WorktreeManager::scope_id_for_session("slack:ws1:C123:ts789");
        assert_ne!(dm1, dm3);
    }

    #[tokio::test]
    async fn create_and_resolve_worktree() {
        if !has_git() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let data_dir = temp.path().join("data");
        let workspace = temp.path().join("workspace");

        // Create a source repo
        let source_repo = workspace.join("repos").join("test-org").join("test-repo");
        std::fs::create_dir_all(&source_repo).unwrap();
        create_test_repo(&source_repo);

        let mgr = WorktreeManager::new(&data_dir).unwrap();

        // Create a worktree for a workflow
        let wt_path = mgr
            .create_worktree("wf-inst1", "test-org/test-repo", &source_repo, None)
            .await
            .unwrap();

        assert!(wt_path.exists());
        assert!(wt_path.join("README.md").exists());

        // Resolve should find it
        let resolved = mgr.resolve_for_session("workflow:inst1:implement", "test-org/test-repo");
        assert_eq!(resolved, Some(wt_path.clone()));

        // Different workflow instance should not resolve
        let resolved2 = mgr.resolve_for_session("workflow:inst2:implement", "test-org/test-repo");
        assert_eq!(resolved2, None);

        // Create a second worktree for a different instance
        let wt_path2 = mgr
            .create_worktree("wf-inst2", "test-org/test-repo", &source_repo, None)
            .await
            .unwrap();

        assert!(wt_path2.exists());
        assert_ne!(wt_path, wt_path2);

        // Both should resolve independently
        let r1 = mgr.resolve_for_session("workflow:inst1:implement", "test-org/test-repo");
        let r2 = mgr.resolve_for_session("workflow:inst2:implement", "test-org/test-repo");
        assert_eq!(r1, Some(wt_path.clone()));
        assert_eq!(r2, Some(wt_path2.clone()));
    }

    #[test]
    fn a_repo_name_is_plain_segments_and_nothing_that_moves_a_join() {
        for name in [
            "api",
            "acme/api",
            "acme/.github",
            "group/sub/api",
            "Acme/API",
        ] {
            assert!(is_repo_name(name), "{name}");
        }
        for name in [
            "",
            "/srv/agents/other/workspace/repos/acme/api",
            "..",
            "../dm-0123456789ab/acme/api",
            "acme/../../other",
            "acme//api",
            "acme/api/",
            "./api",
            "acme\\api",
            "acme/a\0pi",
        ] {
            assert!(!is_repo_name(name), "{name:?}");
        }
    }

    /// A `..` in the repo name would otherwise walk from this session's scope
    /// directory into another's and hand back its working tree.
    #[tokio::test]
    async fn one_session_cannot_reach_another_sessions_worktree_through_the_repo_name() {
        if !has_git() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("workspace/repos/acme/api");
        std::fs::create_dir_all(&source).unwrap();
        create_test_repo(&source);
        let mgr = WorktreeManager::new(&temp.path().join("data")).unwrap();

        let theirs = mgr
            .ensure_for_session(
                "slack:ws:C1:theirs",
                "acme/api",
                &temp.path().join("workspace"),
                None,
            )
            .await
            .unwrap();
        let their_scope = WorktreeManager::scope_id_for_session("slack:ws:C1:theirs");
        let my_scope = mgr
            .worktrees_dir
            .join(WorktreeManager::scope_id_for_session("slack:ws:C1:mine"));
        std::fs::create_dir_all(&my_scope).unwrap();
        let climbing = format!("../{their_scope}/acme/api");
        assert!(my_scope.join(&climbing).join(".git").exists());

        assert_eq!(mgr.resolve_for_session("slack:ws:C1:mine", &climbing), None);
        assert_eq!(
            mgr.resolve_for_session("slack:ws:C1:theirs", "acme/api"),
            Some(theirs)
        );
    }

    /// An absolute path replaces every directory it is joined to, so it would
    /// make the "worktree" the named repository itself, wherever it is.
    #[tokio::test]
    async fn a_repository_outside_the_workspace_is_never_made_a_worktree() {
        if !has_git() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let outside = temp
            .path()
            .join("another-agent/workspace/repos/acme/secret");
        std::fs::create_dir_all(&outside).unwrap();
        create_test_repo(&outside);
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("repos")).unwrap();
        let mgr = WorktreeManager::new(&temp.path().join("data")).unwrap();
        let absolute = outside.to_string_lossy().into_owned();

        assert!(mgr
            .ensure_for_session("slack:ws:C1:mine", &absolute, &workspace, None)
            .await
            .is_err());
        assert!(mgr
            .create_worktree("dm-mine", &absolute, &outside, None)
            .await
            .is_err());
        assert_eq!(mgr.resolve_for_session("slack:ws:C1:mine", &absolute), None);
        assert!(!outside.with_extension("git").exists());
        assert!(mgr.list_worktrees().await.is_empty());
    }

    async fn read_helper(dir: &Path) -> String {
        Command::new("git")
            .args(["config", "--local", "credential.helper"])
            .current_dir(dir)
            .output()
            .await
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn credential_helper_propagates_from_source_to_base_and_worktree() {
        if !has_git() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let workspace = temp.path().join("workspace");
        let source_repo = workspace.join("repos").join("test-org").join("test-repo");
        std::fs::create_dir_all(&source_repo).unwrap();
        create_test_repo(&source_repo);

        let helper = "!f() { echo username=x; echo password=secret; }; f";
        StdCommand::new("git")
            .args(["config", "--local", "credential.helper", helper])
            .current_dir(&source_repo)
            .output()
            .unwrap();

        let mgr = WorktreeManager::new(&temp.path().join("data")).unwrap();
        let wt_path = mgr
            .create_worktree("wf-x", "test-org/test-repo", &source_repo, None)
            .await
            .unwrap();

        let base_path = mgr.base_path("test-org/test-repo", &source_repo);
        assert_eq!(read_helper(&base_path).await, helper);
        assert_eq!(read_helper(&wt_path).await, helper);
    }

    #[tokio::test]
    async fn credential_helper_refreshes_when_source_rotates() {
        if !has_git() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let source_repo = temp.path().join("workspace/repos/o/r");
        std::fs::create_dir_all(&source_repo).unwrap();
        create_test_repo(&source_repo);

        let old = "!f() { echo password=old; }; f";
        StdCommand::new("git")
            .args(["config", "--local", "credential.helper", old])
            .current_dir(&source_repo)
            .output()
            .unwrap();

        let mgr = WorktreeManager::new(&temp.path().join("data")).unwrap();
        mgr.create_worktree("wf-1", "o/r", &source_repo, None)
            .await
            .unwrap();

        let new = "!f() { echo password=new; }; f";
        StdCommand::new("git")
            .args(["config", "--local", "credential.helper", new])
            .current_dir(&source_repo)
            .output()
            .unwrap();

        mgr.create_worktree("wf-2", "o/r", &source_repo, None)
            .await
            .unwrap();

        let base_path = mgr.base_path("o/r", &source_repo);
        assert_eq!(read_helper(&base_path).await, new);
    }

    #[tokio::test]
    async fn missing_source_helper_logs_warning_but_does_not_fail() {
        if !has_git() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let source_repo = temp.path().join("workspace/repos/o/r");
        std::fs::create_dir_all(&source_repo).unwrap();
        create_test_repo(&source_repo);

        let mgr = WorktreeManager::new(&temp.path().join("data")).unwrap();
        let wt_path = mgr
            .create_worktree("wf-x", "o/r", &source_repo, None)
            .await
            .unwrap();
        assert!(wt_path.exists());
        assert!(read_helper(&wt_path).await.is_empty());
    }

    #[tokio::test]
    async fn vault_token_overrides_stale_source_helper() {
        if !has_git() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let source_repo = temp.path().join("workspace/repos/o/r");
        std::fs::create_dir_all(&source_repo).unwrap();
        create_test_repo(&source_repo);

        let stale = "!f() { echo username=x-access-token; echo password=ghp_STALE; }; f";
        StdCommand::new("git")
            .args(["config", "--local", "credential.helper", stale])
            .current_dir(&source_repo)
            .output()
            .unwrap();

        let mgr = WorktreeManager::new(&temp.path().join("data")).unwrap();

        let wt_path = mgr
            .create_worktree("wf-1", "o/r", &source_repo, Some("ghp_FRESH"))
            .await
            .unwrap();

        let expected = "!f() { echo username=x-access-token; echo password=ghp_FRESH; }; f";
        let base_path = mgr.base_path("o/r", &source_repo);
        assert_eq!(read_helper(&wt_path).await, expected);
        assert_eq!(read_helper(&base_path).await, expected);
        assert_eq!(read_helper(&source_repo).await, expected);
    }

    #[tokio::test]
    async fn remove_worktree_cleans_up() {
        if !has_git() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let data_dir = temp.path().join("data");
        let workspace = temp.path().join("workspace");

        let source_repo = workspace.join("repos").join("test-org").join("test-repo");
        std::fs::create_dir_all(&source_repo).unwrap();
        create_test_repo(&source_repo);

        let mgr = WorktreeManager::new(&data_dir).unwrap();

        let wt_path = mgr
            .create_worktree("wf-inst1", "test-org/test-repo", &source_repo, None)
            .await
            .unwrap();

        assert!(wt_path.exists());

        // Remove it
        mgr.remove_worktree("wf-inst1").await.unwrap();

        assert!(!wt_path.exists());
        assert_eq!(
            mgr.resolve_for_session("workflow:inst1:implement", "test-org/test-repo"),
            None
        );
    }

    /// Two agents' checkouts of one name are worked on from bases of their
    /// own: neither sees the other's content or carries its credentials.
    #[tokio::test]
    async fn checkouts_of_one_name_in_two_workspaces_have_bases_of_their_own() {
        if !has_git() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let checkout = |agent: &str, content: &str, token: &str| {
            let source = temp.path().join(agent).join("repos/o/r");
            std::fs::create_dir_all(&source).unwrap();
            create_test_repo(&source);
            std::fs::write(source.join("README.md"), content).unwrap();
            StdCommand::new("git")
                .args(["commit", "-qam", content])
                .current_dir(&source)
                .output()
                .unwrap();
            let helper = format!("!f() {{ echo password={token}; }}; f");
            StdCommand::new("git")
                .args(["config", "--local", "credential.helper", &helper])
                .current_dir(&source)
                .output()
                .unwrap();
            (source, helper)
        };
        let (ours, our_helper) = checkout("ours", "ours", "ghp_OURS");
        let (theirs, their_helper) = checkout("theirs", "theirs", "ghp_THEIRS");
        let mgr = WorktreeManager::new(&temp.path().join("data")).unwrap();

        let our_tree = mgr
            .create_worktree("wf-1", "o/r", &ours, None)
            .await
            .unwrap();
        let their_tree = mgr
            .create_worktree("wf-2", "o/r", &theirs, None)
            .await
            .unwrap();

        assert_ne!(mgr.base_path("o/r", &ours), mgr.base_path("o/r", &theirs));
        assert_eq!(
            std::fs::read_to_string(our_tree.join("README.md")).unwrap(),
            "ours"
        );
        assert_eq!(
            std::fs::read_to_string(their_tree.join("README.md")).unwrap(),
            "theirs"
        );
        assert_eq!(read_helper(&mgr.base_path("o/r", &ours)).await, our_helper);
        assert_eq!(read_helper(&our_tree).await, our_helper);
        assert_eq!(read_helper(&ours).await, our_helper);
        assert_eq!(read_helper(&their_tree).await, their_helper);
    }

    /// A worktree whose `.git` the agent pointed somewhere else is removed
    /// as a plain directory: no git runs where it points.
    #[tokio::test]
    async fn a_worktree_pointing_outside_the_bases_is_removed_without_git() {
        if !has_git() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("workspace/repos/o/r");
        std::fs::create_dir_all(&source).unwrap();
        create_test_repo(&source);
        let mgr = WorktreeManager::new(&temp.path().join("data")).unwrap();
        let tree = mgr
            .create_worktree("wf-1", "o/r", &source, None)
            .await
            .unwrap();
        let elsewhere = temp.path().join("planted");
        std::fs::create_dir_all(elsewhere.join("worktrees/r")).unwrap();
        std::fs::write(
            elsewhere.join("worktrees/r/gitdir"),
            tree.join(".git").display().to_string(),
        )
        .unwrap();
        std::fs::write(
            tree.join(".git"),
            format!("gitdir: {}", elsewhere.join("worktrees/r").display()),
        )
        .unwrap();
        assert_eq!(mgr.base_of(&tree), None);

        mgr.remove_worktree("wf-1").await.unwrap();

        assert!(!tree.exists());
        assert!(elsewhere.join("worktrees/r/gitdir").exists(), "untouched");
    }

    /// A worktree made from a base of the layout keyed by name alone keeps
    /// working from that base, and is still unregistered from it and removed.
    #[tokio::test]
    async fn a_worktree_of_a_base_keyed_by_name_alone_keeps_it_and_is_still_removed() {
        if !has_git() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("workspace/repos/o/r");
        std::fs::create_dir_all(&source).unwrap();
        create_test_repo(&source);
        let mgr = WorktreeManager::new(&temp.path().join("data")).unwrap();
        let legacy = mgr.bases_dir.join("o/r.git");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let out = StdCommand::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).to_string()
        };
        git(
            temp.path(),
            &[
                "clone",
                "-q",
                "--bare",
                source.to_str().unwrap(),
                legacy.to_str().unwrap(),
            ],
        );
        let tree = mgr.worktrees_dir.join("dm-legacy/o/r");
        git(
            &legacy,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                tree.to_str().unwrap(),
                "HEAD",
            ],
        );
        assert_eq!(
            mgr.create_worktree("dm-legacy", "o/r", &source, None)
                .await
                .unwrap(),
            tree
        );
        assert!(
            !mgr.base_path("o/r", &source).exists(),
            "a worktree that exists keeps its base; none is cloned for it"
        );

        mgr.remove_worktree("dm-legacy").await.unwrap();

        assert!(!tree.exists());
        assert!(
            !git(&legacy, &["worktree", "list", "--porcelain"]).contains("dm-legacy"),
            "the old base no longer lists it"
        );
    }

    #[tokio::test]
    async fn worktrees_are_isolated() {
        if !has_git() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let data_dir = temp.path().join("data");
        let workspace = temp.path().join("workspace");

        let source_repo = workspace.join("repos").join("test-org").join("test-repo");
        std::fs::create_dir_all(&source_repo).unwrap();
        create_test_repo(&source_repo);

        let mgr = WorktreeManager::new(&data_dir).unwrap();

        let wt1 = mgr
            .create_worktree("wf-inst1", "test-org/test-repo", &source_repo, None)
            .await
            .unwrap();
        let wt2 = mgr
            .create_worktree("wf-inst2", "test-org/test-repo", &source_repo, None)
            .await
            .unwrap();

        // Write different files in each worktree
        std::fs::write(wt1.join("file1.txt"), "from instance 1").unwrap();
        std::fs::write(wt2.join("file2.txt"), "from instance 2").unwrap();

        // Verify isolation: each worktree only has its own file
        assert!(wt1.join("file1.txt").exists());
        assert!(!wt1.join("file2.txt").exists());
        assert!(wt2.join("file2.txt").exists());
        assert!(!wt2.join("file1.txt").exists());
    }

    #[tokio::test]
    async fn ensure_for_session_lazy_creation() {
        if !has_git() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let data_dir = temp.path().join("data");
        let workspace = temp.path().join("workspace");

        let source_repo = workspace.join("repos").join("test-org").join("test-repo");
        std::fs::create_dir_all(&source_repo).unwrap();
        create_test_repo(&source_repo);

        let mgr = WorktreeManager::new(&data_dir).unwrap();

        // Should not exist yet
        assert!(mgr
            .resolve_for_session("slack:ws1:C123:ts456", "test-org/test-repo")
            .is_none());

        // Lazy creation
        let path = mgr
            .ensure_for_session(
                "slack:ws1:C123:ts456",
                "test-org/test-repo",
                &workspace,
                None,
            )
            .await
            .unwrap();

        assert!(path.exists());
        assert!(path.join("README.md").exists());

        // Should resolve now
        assert_eq!(
            mgr.resolve_for_session("slack:ws1:C123:ts456", "test-org/test-repo"),
            Some(path.clone())
        );

        // Calling again returns the same path
        let path2 = mgr
            .ensure_for_session(
                "slack:ws1:C123:ts456",
                "test-org/test-repo",
                &workspace,
                None,
            )
            .await
            .unwrap();
        assert_eq!(path, path2);
    }

    #[tokio::test]
    async fn cleanup_stale_removes_expired() {
        if !has_git() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let data_dir = temp.path().join("data");
        let workspace = temp.path().join("workspace");

        let source_repo = workspace.join("repos").join("test-org").join("test-repo");
        std::fs::create_dir_all(&source_repo).unwrap();
        create_test_repo(&source_repo);

        let mgr = WorktreeManager::new(&data_dir).unwrap();

        // Create a workflow worktree
        mgr.create_worktree("wf-inst1", "test-org/test-repo", &source_repo, None)
            .await
            .unwrap();

        // Cleanup with inst1 NOT in active set and zero grace → should be removed
        let active = HashSet::new();
        let removed = mgr
            .cleanup_stale(&active, Duration::from_secs(86400), Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(removed, 1);

        // Should no longer resolve
        assert!(mgr
            .resolve_for_session("workflow:inst1:implement", "test-org/test-repo")
            .is_none());
    }

    #[tokio::test]
    async fn cleanup_preserves_active_workflow_worktrees() {
        if !has_git() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let data_dir = temp.path().join("data");
        let workspace = temp.path().join("workspace");

        let source_repo = workspace.join("repos").join("test-org").join("test-repo");
        std::fs::create_dir_all(&source_repo).unwrap();
        create_test_repo(&source_repo);

        let mgr = WorktreeManager::new(&data_dir).unwrap();

        // Create two workflow worktrees
        let wt1 = mgr
            .create_worktree("wf-running1", "test-org/test-repo", &source_repo, None)
            .await
            .unwrap();
        let wt2 = mgr
            .create_worktree("wf-paused1", "test-org/test-repo", &source_repo, None)
            .await
            .unwrap();
        let _wt3 = mgr
            .create_worktree("wf-terminal1", "test-org/test-repo", &source_repo, None)
            .await
            .unwrap();

        // Active set includes running and paused, NOT terminal
        let mut active = HashSet::new();
        active.insert("running1".to_string());
        active.insert("paused1".to_string());

        // Cleanup with zero grace — terminal should be removed, active preserved
        let removed = mgr
            .cleanup_stale(&active, Duration::from_secs(86400), Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(removed, 1); // Only terminal1 removed

        // Active worktrees still exist
        assert!(wt1.exists());
        assert!(wt2.exists());
    }

    #[tokio::test]
    async fn cleanup_grace_period_preserves_recent_terminal() {
        if !has_git() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let data_dir = temp.path().join("data");
        let workspace = temp.path().join("workspace");

        let source_repo = workspace.join("repos").join("test-org").join("test-repo");
        std::fs::create_dir_all(&source_repo).unwrap();
        create_test_repo(&source_repo);

        let mgr = WorktreeManager::new(&data_dir).unwrap();

        // Create a workflow worktree for a terminal instance
        let wt = mgr
            .create_worktree("wf-done1", "test-org/test-repo", &source_repo, None)
            .await
            .unwrap();

        // Not in active set, but with a long grace period — should be preserved
        let active = HashSet::new();
        let removed = mgr
            .cleanup_stale(
                &active,
                Duration::from_secs(86400),
                Duration::from_secs(3600),
            )
            .await
            .unwrap();
        assert_eq!(removed, 0); // Preserved by grace period (just created)
        assert!(wt.exists());

        // With zero grace — should be removed
        let removed = mgr
            .cleanup_stale(&active, Duration::from_secs(86400), Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(removed, 1);
        assert!(!wt.exists());
    }

    /// A relative `data_dir` must be canonicalised so derived paths are
    /// absolute. Otherwise `git worktree add` (invoked with
    /// `current_dir(&base_path)` and a relative `wt_path`) plants worktrees
    /// under the bare repo dir, and the next `chdir` fails with ENOENT.
    #[test]
    fn new_canonicalises_relative_data_dir() {
        // target/ is workspace-relative and gitignored — safe to leave debris.
        let unique = format!("target/wt-test-relpath-{}", std::process::id());
        let rel_path = PathBuf::from(&unique);
        assert!(rel_path.is_relative(), "test fixture must be relative");

        let mgr = WorktreeManager::new(&rel_path).unwrap();

        assert!(
            mgr.bases_dir.is_absolute(),
            "bases_dir must be absolute, got: {}",
            mgr.bases_dir.display()
        );
        assert!(
            mgr.worktrees_dir.is_absolute(),
            "worktrees_dir must be absolute, got: {}",
            mgr.worktrees_dir.display()
        );
        assert!(mgr.bases_dir.ends_with("worktree-bases"));
        assert!(mgr.worktrees_dir.ends_with("worktrees"));

        let _ = std::fs::remove_dir_all(&unique);
    }
}
