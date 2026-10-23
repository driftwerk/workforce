//! Per-claude-code-session full-project copy.
//!
//! Each /cc session bound to a project gets its own dir under
//! `cc-sessions/{cc_session_uuid}/` containing every project repo as a
//! sibling working clone. We materialise via `git clone --local` from the
//! project's `work/clones/{alias}` source: same-filesystem hardlinks objects
//! (fast + disk-cheap), cross-filesystem falls back to copies (still fast)
//! — and each session ends up with its own object store, so destroying the
//! session dir never affects the source clones the agent works against
//! outside /cc.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use eyre::{eyre, Result};
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};
use workforce_projects::Project;

use super::{layout, sandbox_git, touch_liveness, LIVENESS_FILE};
use crate::coding_sandbox::CodingSandbox;
use crate::git_hardening::hardened_git;

/// Materialise `cc-sessions/{scope}/` for `project`: one local clone per repo,
/// or an empty scratch workspace for a project with no repos (see
/// [`super::workspace_scope`] for what `scope` is keyed on). Idempotent: if the
/// directory already exists (a prior turn already set it up), returns its
/// path without re-cloning. Atomic-or-clean: a partial failure mid-way
/// removes the (now-incomplete) session dir before returning so a retry
/// gets a fresh attempt instead of short-circuiting on the stale state.
///
/// Every call (re-)touches a `.lastseen` sentinel file at the session
/// root so the TTL sweeper can tell active sessions from orphans.
pub async fn ensure(
    data_dir: &Path,
    agent_id: &str,
    project: &Project,
    scope: &str,
) -> Result<PathBuf> {
    let session_root = layout::cc_session_dir(data_dir, agent_id, &project.name, scope);
    if session_root.exists() {
        touch_liveness(&session_root).await;
        return Ok(session_root);
    }
    tokio::fs::create_dir_all(&session_root).await?;

    match materialise_repos(data_dir, agent_id, project, &session_root).await {
        Ok(()) => {
            add_github_remotes(data_dir, agent_id, project, &session_root).await;
            touch_liveness(&session_root).await;
            Ok(session_root)
        }
        Err(e) => {
            let _ = tokio::fs::remove_dir_all(&session_root).await;
            Err(e)
        }
    }
}

/// Mirror each repo's authenticated GitHub `origin` (URL + credential helper,
/// set on the base clone by the fetcher) onto the session clone as a `github`
/// remote, so a `/cc` session can `git push github …` + `gh pr create`. `origin`
/// stays the local base — preserving the per-session object store — so this only
/// adds an authenticated path to GitHub, it doesn't change isolation.
/// Best-effort: a repo whose base has no GitHub origin (e.g. tests) is skipped.
async fn add_github_remotes(
    data_dir: &Path,
    agent_id: &str,
    project: &Project,
    session_root: &Path,
) {
    for repo in &project.repos {
        let base = layout::work_clone_dir(data_dir, agent_id, &project.name, &repo.alias);
        let dest = session_root.join(&repo.alias);
        if !dest.join(".git").exists() {
            continue;
        }
        let Some(url_out) = run_git_or_warn(
            &base,
            &["config", "--get", "remote.origin.url"],
            &repo.alias,
        )
        .await
        else {
            continue;
        };
        let url = String::from_utf8_lossy(&url_out.stdout).trim().to_string();
        if !url.contains("github.com") {
            continue;
        }
        run_git_or_warn(&dest, &["remote", "add", "github", &url], &repo.alias).await;
        if let Some(helper_out) = run_git_or_warn(
            &base,
            &["config", "--get", "credential.helper"],
            &repo.alias,
        )
        .await
        {
            let helper = String::from_utf8_lossy(&helper_out.stdout)
                .trim()
                .to_string();
            if !helper.is_empty() {
                run_git_or_warn(
                    &dest,
                    &["config", "credential.helper", &helper],
                    &repo.alias,
                )
                .await;
            }
        }
    }
}

async fn materialise_repos(
    data_dir: &Path,
    agent_id: &str,
    project: &Project,
    session_root: &Path,
) -> Result<()> {
    // Pre-check sources before spawning so a missing base surfaces as a
    // single clean error instead of N parallel clone failures racing each
    // other for which one's stderr the user sees.
    for repo in &project.repos {
        let source = layout::work_clone_dir(data_dir, agent_id, &project.name, &repo.alias);
        if !source.exists() {
            return Err(eyre!(
                "project '{}' repo '{}' is not cloned at {} — run projects.update first",
                project.name,
                repo.alias,
                source.display()
            ));
        }
    }

    let timeout = clone_timeout();
    let semaphore = Arc::new(Semaphore::new(clone_concurrency()));
    let mut handles = Vec::with_capacity(project.repos.len());
    for repo in &project.repos {
        let source = layout::work_clone_dir(data_dir, agent_id, &project.name, &repo.alias);
        let dest = session_root.join(&repo.alias);
        let alias = repo.alias.clone();
        let permit = Arc::clone(&semaphore);
        handles.push(tokio::spawn(async move {
            // `acquire_owned` returns Err only when the semaphore is closed,
            // which we never do — treat the panic-free path and the close
            // path the same way: bubble up as a clone failure.
            let _p = permit
                .acquire_owned()
                .await
                .map_err(|e| eyre!("cc-session clone semaphore closed: {e}"))?;
            clone_one_repo(&source, &dest, &alias, timeout).await
        }));
    }

    // Drain every handle before returning so a fast failure doesn't leave
    // sibling clones writing into the (about-to-be-removed) session dir.
    let mut first_err: Option<eyre::Report> = None;
    for handle in handles {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
            Err(join_err) => {
                if first_err.is_none() {
                    first_err = Some(eyre!("cc-session clone task panicked: {join_err}"));
                }
            }
        }
    }

    match first_err {
        None => Ok(()),
        Some(e) => Err(e),
    }
}

/// One per-repo stash made by [`stash_dirty_repos`]. The Slack failure
/// card renders these so the user can recover their in-flight edits via
/// `git stash apply <stash_ref>` if the next /cc turn doesn't continue
/// in the same worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WipStash {
    pub repo: String,
    pub stash_ref: String,
    pub sha: String,
}

/// Stash uncommitted work in every checkout of the sandbox that has any,
/// labelled so the user can find it; a clean checkout is left alone.
pub async fn stash_dirty_repos(sandbox: &dyn CodingSandbox, label: &str) -> Vec<WipStash> {
    let mut entries = Vec::new();
    for repo in sandbox_git::repo_dirs(sandbox).await {
        if let Some(entry) = stash_one_repo(sandbox, &repo, label).await {
            entries.push(entry);
        }
    }
    entries
}

async fn stash_one_repo(
    sandbox: &dyn CodingSandbox,
    repo: &sandbox_git::RepoDir,
    label: &str,
) -> Option<WipStash> {
    let porcelain = stash_git(sandbox, repo, &["status", "--porcelain"]).await?;
    if porcelain.is_empty() {
        return None;
    }
    stash_git(sandbox, repo, &["stash", "push", "-u", "-m", label]).await?;
    let sha = stash_git(sandbox, repo, &["rev-parse", "stash@{0}"])
        .await
        .unwrap_or_default();
    Some(WipStash {
        repo: repo.alias.clone(),
        stash_ref: "stash@{0}".to_string(),
        sha,
    })
}

/// A git step of the stash, logged and dropped when it fails: the stash is
/// best effort around a crash that already has the user's attention.
async fn stash_git(
    sandbox: &dyn CodingSandbox,
    repo: &sandbox_git::RepoDir,
    args: &[&str],
) -> Option<String> {
    match sandbox_git::capture(sandbox, &repo.path, args, &[]).await {
        Ok(out) => Some(out),
        Err(e) => {
            warn!(repo = %repo.alias, error = %e, "cc-session stash: git command failed");
            None
        }
    }
}

/// Run `git <args>` in `repo_dir` on this host, returning `Some(output)` on
/// success and `None` (with a warn-level log carrying `alias`) on spawn or
/// non-zero exit.
async fn run_git_or_warn(
    repo_dir: &Path,
    args: &[&str],
    alias: &str,
) -> Option<std::process::Output> {
    match hardened_git()
        .args(args)
        .current_dir(repo_dir)
        .output()
        .await
    {
        Ok(out) if out.status.success() => Some(out),
        Ok(out) => {
            warn!(
                repo = %alias,
                args = ?args,
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "cc-session: git command exited non-zero"
            );
            None
        }
        Err(e) => {
            warn!(
                repo = %alias,
                args = ?args,
                error = %e,
                "cc-session: git command spawn failed"
            );
            None
        }
    }
}

/// Remove the on-disk dir for a single /cc session. No-op if missing.
pub async fn cleanup(
    data_dir: &Path,
    agent_id: &str,
    project_name: &str,
    cc_session_uuid: &str,
) -> Result<()> {
    let dir = layout::cc_session_dir(data_dir, agent_id, project_name, cc_session_uuid);
    if dir.exists() {
        tokio::fs::remove_dir_all(&dir).await?;
    }
    Ok(())
}

/// `git clone --local <source> <dest>` — uses hardlinks for objects when
/// source and dest are on the same filesystem, otherwise falls back to
/// regular copies. Either way the clone has its own working tree and its
/// own object store, so a `git gc` in the source can never orphan objects
/// in the dest (the trade-off `--shared` would force).
///
/// Errors include the repo `alias` and source path so users staring at
/// "git clone --local timed out" can tell which of the project's N repos
/// is the slow one without grepping the hub logs.
async fn clone_one_repo(source: &Path, dest: &Path, alias: &str, timeout: Duration) -> Result<()> {
    let mut cmd = hardened_git();
    cmd.args([
        "clone",
        "--local",
        &source.to_string_lossy(),
        &dest.to_string_lossy(),
    ]);
    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| {
            eyre!(
                "git clone --local timed out after {}s for repo '{alias}' (source: {})",
                timeout.as_secs(),
                source.display()
            )
        })?
        .map_err(|e| eyre!("git clone --local for repo '{alias}' failed to spawn: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!(
            "git clone --local failed for repo '{alias}': {}",
            stderr.trim()
        ));
    }
    Ok(())
}

/// Per-repo clone timeout. Sized for a multi-GB monorepo on networked
/// storage where `--local` falls back to per-object copy. Override via
/// `WORKFORCE_CC_CLONE_TIMEOUT_SECS`. Values below 10s are ignored —
/// they're almost always typos and would spuriously fail every clone.
const DEFAULT_CLONE_TIMEOUT_SECS: u64 = 300;

/// Concurrent clones inside a single project's `materialise_repos`.
/// Bounded so disk-saturated environments don't thrash; 4 fits typical
/// NVMe/EBS gp3 sweet spots and is a clean ~5x speedup over sequential
/// for the harbor-scale 7-repo project. Override via
/// `WORKFORCE_CC_CLONE_CONCURRENCY` (must be ≥ 1).
const DEFAULT_CLONE_CONCURRENCY: usize = 4;

fn clone_timeout() -> Duration {
    let secs = std::env::var("WORKFORCE_CC_CLONE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|n| *n >= 10)
        .unwrap_or(DEFAULT_CLONE_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

fn clone_concurrency() -> usize {
    std::env::var("WORKFORCE_CC_CLONE_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(DEFAULT_CLONE_CONCURRENCY)
}

/// Walk every `{data_dir}/agents/*/projects/*/cc-sessions/*` directory
/// and remove the ones whose `.lastseen` sentinel is older than `ttl` (or
/// missing — pre-sentinel orphans from earlier hub builds count as
/// stale). Returns the number of dirs removed.
pub async fn sweep_stale(data_dir: &Path, ttl: Duration) -> Result<usize> {
    let agents_root = data_dir.join("agents");
    if !agents_root.exists() {
        return Ok(0);
    }
    let now = SystemTime::now();
    let mut removed = 0usize;

    let mut agent_iter = match tokio::fs::read_dir(&agents_root).await {
        Ok(d) => d,
        Err(e) => return Err(eyre!("read_dir({}): {e}", agents_root.display())),
    };
    while let Some(agent_entry) = agent_iter.next_entry().await? {
        let projects_dir = agent_entry.path().join("projects");
        if !projects_dir.is_dir() {
            continue;
        }
        let mut project_iter = tokio::fs::read_dir(&projects_dir).await?;
        while let Some(project_entry) = project_iter.next_entry().await? {
            let cc_root = project_entry.path().join("cc-sessions");
            if !cc_root.is_dir() {
                continue;
            }
            let mut session_iter = tokio::fs::read_dir(&cc_root).await?;
            while let Some(session_entry) = session_iter.next_entry().await? {
                let path = session_entry.path();
                if !path.is_dir() {
                    continue;
                }
                if let Some(age) = stale_age(&path, now, ttl).await {
                    match tokio::fs::remove_dir_all(&path).await {
                        Ok(()) => {
                            removed += 1;
                            info!(
                                path = %path.display(),
                                age_secs = age.as_secs(),
                                "cc-session sweeper: removed stale dir"
                            );
                        }
                        Err(e) => {
                            warn!(path = %path.display(), error = %e, "cc-session sweeper: remove failed");
                        }
                    }
                }
            }
        }
    }
    Ok(removed)
}

/// Return `Some(age)` when the session at `path` has aged past `ttl`,
/// `None` when it's still fresh enough to keep. The age comes from the
/// `.lastseen` sentinel; a missing sentinel is treated as ancient so
/// orphans from buggy materialisation paths (or older hub builds) get
/// reclaimed instead of lingering forever.
async fn stale_age(path: &Path, now: SystemTime, ttl: Duration) -> Option<Duration> {
    let sentinel = path.join(LIVENESS_FILE);
    let age = match tokio::fs::metadata(&sentinel).await {
        Ok(m) => {
            let mtime = m.modified().unwrap_or(now);
            now.duration_since(mtime).unwrap_or(Duration::ZERO)
        }
        Err(_) => Duration::MAX,
    };
    if age > ttl {
        Some(age)
    } else {
        None
    }
}

/// Spawn a background task that runs [`sweep_stale`] every `interval`. The
/// task lives for the process lifetime; mistakes log-and-continue rather
/// than abort, so a transient FS error doesn't disable the sweeper.
pub fn spawn_sweeper(
    data_dir: PathBuf,
    interval: Duration,
    ttl: Duration,
    leader_gate: Arc<crate::leader_gate::LeaderGate>,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        // First tick fires immediately on `interval`'s default `Burst` mode —
        // skip it so startup doesn't race other init work.
        tick.tick().await;
        loop {
            tick.tick().await;
            if !leader_gate.is_leader() {
                continue;
            }
            match sweep_stale(&data_dir, ttl).await {
                Ok(n) if n > 0 => debug!(removed = n, "cc-session sweeper pass"),
                Ok(_) => {}
                Err(e) => warn!(error = %e, "cc-session sweeper pass failed"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::process::Command;
    use workforce_projects::{ProjectRepo, RepoSource};

    use crate::coding_sandbox::adopt_workdir;

    fn sb(dir: &Path) -> Arc<dyn CodingSandbox> {
        adopt_workdir(dir.to_path_buf(), Vec::new(), None)
    }

    /// Seed a real on-disk source clone at `work/clones/{alias}` so
    /// `ensure` can `git clone --local` from it.
    async fn seed_source_clone(data_dir: &Path, agent_id: &str, project: &str, alias: &str) {
        let source = layout::work_clone_dir(data_dir, agent_id, project, alias);
        tokio::fs::create_dir_all(&source).await.unwrap();
        for args in [
            &["init", "--initial-branch=main"][..],
            &["config", "user.email", "test@example.com"][..],
            &["config", "user.name", "Test"][..],
        ] {
            Command::new("git")
                .args(args)
                .current_dir(&source)
                .output()
                .await
                .unwrap();
        }
        tokio::fs::write(source.join("README"), "hello")
            .await
            .unwrap();
        for args in [&["add", "."][..], &["commit", "-m", "init"][..]] {
            Command::new("git")
                .args(args)
                .current_dir(&source)
                .output()
                .await
                .unwrap();
        }
    }

    fn project_with_repo(name: &str, alias: &str) -> Project {
        let mut p = Project::new("allan", name);
        p.repos.push(ProjectRepo {
            alias: alias.into(),
            source: RepoSource::Github {
                owner: "acme".into(),
                repo: alias.into(),
            },
            default_branch: None,
            setup: None,
        });
        p
    }

    #[tokio::test]
    async fn ensure_materialises_a_scratch_workspace_for_a_repoless_project() {
        let tmp = TempDir::new().unwrap();
        let project = Project::new("allan", "skills");
        let scope = "scratch-dm-abc123";
        let root = ensure(tmp.path(), "allan", &project, scope).await.unwrap();

        assert!(root.is_dir());
        assert!(
            root.join(LIVENESS_FILE).exists(),
            "the sweeper needs a sentinel or it treats the workspace as ancient"
        );

        tokio::fs::write(root.join("lib.rs"), "fn main() {}")
            .await
            .unwrap();
        let again = ensure(tmp.path(), "allan", &project, scope).await.unwrap();
        assert_eq!(again, root);
        assert!(again.join("lib.rs").exists());

        cleanup(tmp.path(), "allan", "skills", scope).await.unwrap();
        assert!(!root.exists());
    }

    #[tokio::test]
    async fn ensure_errors_when_source_clone_missing() {
        let tmp = TempDir::new().unwrap();
        let project = project_with_repo("harbor", "rails");
        let err = ensure(tmp.path(), "allan", &project, "uuid-1")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not cloned"));
    }

    #[tokio::test]
    async fn ensure_materialises_each_repo_into_session_dir() {
        let tmp = TempDir::new().unwrap();
        seed_source_clone(tmp.path(), "allan", "harbor", "rails").await;
        seed_source_clone(tmp.path(), "allan", "harbor", "frontend").await;

        let mut project = project_with_repo("harbor", "rails");
        project.repos.push(ProjectRepo {
            alias: "frontend".into(),
            source: RepoSource::Github {
                owner: "acme".into(),
                repo: "frontend".into(),
            },
            default_branch: None,
            setup: None,
        });

        let session_root = ensure(tmp.path(), "allan", &project, "uuid-1")
            .await
            .unwrap();
        assert!(session_root.join("rails").join(".git").exists());
        assert!(session_root.join("frontend").join(".git").exists());
        assert!(session_root.join("rails").join("README").exists());
    }

    #[tokio::test]
    async fn ensure_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        seed_source_clone(tmp.path(), "allan", "harbor", "rails").await;
        let project = project_with_repo("harbor", "rails");

        let first = ensure(tmp.path(), "allan", &project, "uuid-1")
            .await
            .unwrap();
        // Second call short-circuits because the dir exists; would otherwise
        // fail with "destination already exists" inside git clone.
        let second = ensure(tmp.path(), "allan", &project, "uuid-1")
            .await
            .unwrap();
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn add_github_remotes_mirrors_authenticated_origin_onto_session() {
        let tmp = TempDir::new().unwrap();
        seed_source_clone(tmp.path(), "allan", "harbor", "rails").await;
        // Give the base an authenticated-style GitHub origin + helper, as the
        // fetcher does; the session clone should mirror it as `github` so /cc
        // can push + open PRs.
        let base = layout::work_clone_dir(tmp.path(), "allan", "harbor", "rails");
        for args in [
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/acme/rails.git",
            ][..],
            &["config", "credential.helper", "!f(){ echo x; }; f"][..],
        ] {
            Command::new("git")
                .args(args)
                .current_dir(&base)
                .output()
                .await
                .unwrap();
        }
        let project = project_with_repo("harbor", "rails");
        let session_root = ensure(tmp.path(), "allan", &project, "uuid-gh")
            .await
            .unwrap();

        // `origin` stays the local mirror; `github` points at the real remote.
        let github = Command::new("git")
            .args(["remote", "get-url", "github"])
            .current_dir(session_root.join("rails"))
            .output()
            .await
            .unwrap();
        assert!(
            github.status.success(),
            "session clone must have a `github` remote"
        );
        assert_eq!(
            String::from_utf8_lossy(&github.stdout).trim(),
            "https://github.com/acme/rails.git"
        );
    }

    #[tokio::test]
    async fn cleanup_removes_session_dir() {
        let tmp = TempDir::new().unwrap();
        seed_source_clone(tmp.path(), "allan", "harbor", "rails").await;
        let project = project_with_repo("harbor", "rails");
        let session_root = ensure(tmp.path(), "allan", &project, "uuid-1")
            .await
            .unwrap();
        assert!(session_root.exists());

        cleanup(tmp.path(), "allan", "harbor", "uuid-1")
            .await
            .unwrap();
        assert!(!session_root.exists());
    }

    #[tokio::test]
    async fn cleanup_is_noop_when_dir_missing() {
        let tmp = TempDir::new().unwrap();
        cleanup(tmp.path(), "allan", "harbor", "missing-uuid")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn ensure_cleans_up_partial_state_when_a_repo_fails() {
        let tmp = TempDir::new().unwrap();
        // Seed only one of two repos. The second will fail with "not cloned".
        seed_source_clone(tmp.path(), "allan", "harbor", "rails").await;

        let mut project = project_with_repo("harbor", "rails");
        project.repos.push(ProjectRepo {
            alias: "missing".into(),
            source: RepoSource::Github {
                owner: "acme".into(),
                repo: "missing".into(),
            },
            default_branch: None,
            setup: None,
        });

        let session_dir = layout::cc_session_dir(tmp.path(), "allan", "harbor", "uuid-partial");
        assert!(ensure(tmp.path(), "allan", &project, "uuid-partial")
            .await
            .is_err());
        // Dir is fully removed so a retry doesn't short-circuit on stale state.
        assert!(!session_dir.exists());
    }

    /// Set the mtime of `path` to `now - age` so the sweeper sees it as stale.
    /// Use BSD/POSIX-compatible `touch -t YYYYMMDDhhmm.SS` (works on macOS and
    /// GNU coreutils alike); `touch -d @<unix>` is GNU-only.
    async fn age_dir(path: &Path, age: Duration) {
        use std::time::{SystemTime, UNIX_EPOCH};
        let target = SystemTime::now()
            .checked_sub(age)
            .expect("age must be in the past")
            .duration_since(UNIX_EPOCH)
            .unwrap();
        let secs = target.as_secs() as i64;
        let tm = chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0).unwrap();
        let stamp = tm.format("%Y%m%d%H%M.%S").to_string();
        let out = Command::new("touch")
            .args(["-t", &stamp, &path.to_string_lossy()])
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "touch failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[tokio::test]
    async fn sweep_removes_dirs_older_than_ttl() {
        let tmp = TempDir::new().unwrap();
        seed_source_clone(tmp.path(), "allan", "harbor", "rails").await;
        let project = project_with_repo("harbor", "rails");

        let fresh = ensure(tmp.path(), "allan", &project, "uuid-fresh")
            .await
            .unwrap();
        let stale = ensure(tmp.path(), "allan", &project, "uuid-stale")
            .await
            .unwrap();
        // Age the liveness sentinel — the sweeper checks that file, not the
        // dir itself, because POSIX dir mtime doesn't bubble up changes from
        // deep inside the per-repo subtrees claude code modifies.
        age_dir(&stale.join(LIVENESS_FILE), Duration::from_secs(7200)).await;

        let removed = sweep_stale(tmp.path(), Duration::from_secs(3600))
            .await
            .unwrap();
        assert_eq!(removed, 1);
        assert!(fresh.exists(), "fresh session must survive");
        assert!(!stale.exists(), "stale session must be reclaimed");
    }

    #[tokio::test]
    async fn sweep_reclaims_orphan_with_no_liveness_file() {
        // A dir created without `ensure()` (e.g. left over from an older hub
        // build that didn't write `.lastseen`) is treated as ancient so it
        // doesn't linger forever even if its dir mtime is fresh.
        let tmp = TempDir::new().unwrap();
        let orphan = layout::cc_session_dir(tmp.path(), "allan", "harbor", "uuid-orphan");
        tokio::fs::create_dir_all(&orphan).await.unwrap();

        let removed = sweep_stale(tmp.path(), Duration::from_secs(3600))
            .await
            .unwrap();
        assert_eq!(removed, 1);
        assert!(!orphan.exists());
    }

    #[tokio::test]
    async fn ensure_writes_and_refreshes_liveness_file() {
        let tmp = TempDir::new().unwrap();
        seed_source_clone(tmp.path(), "allan", "harbor", "rails").await;
        let project = project_with_repo("harbor", "rails");

        let root = ensure(tmp.path(), "allan", &project, "uuid-1")
            .await
            .unwrap();
        let sentinel = root.join(LIVENESS_FILE);
        assert!(sentinel.exists(), "first ensure() must create the sentinel");

        // Age the sentinel and re-call ensure — short-circuit path must
        // refresh it.
        age_dir(&sentinel, Duration::from_secs(7200)).await;
        let aged = tokio::fs::metadata(&sentinel)
            .await
            .unwrap()
            .modified()
            .unwrap();
        ensure(tmp.path(), "allan", &project, "uuid-1")
            .await
            .unwrap();
        let refreshed = tokio::fs::metadata(&sentinel)
            .await
            .unwrap()
            .modified()
            .unwrap();
        assert!(refreshed > aged, "second ensure() must bump sentinel mtime");
    }

    #[tokio::test]
    async fn sweep_is_noop_when_data_dir_has_no_agents() {
        let tmp = TempDir::new().unwrap();
        let removed = sweep_stale(tmp.path(), Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(removed, 0);
    }

    #[tokio::test]
    async fn sweep_skips_unrelated_dirs() {
        let tmp = TempDir::new().unwrap();
        // An agent root with no `projects/` subdir at all.
        tokio::fs::create_dir_all(tmp.path().join("agents/loose"))
            .await
            .unwrap();
        // A project root with no `cc-sessions/` subdir.
        tokio::fs::create_dir_all(tmp.path().join("agents/allan/projects/empty"))
            .await
            .unwrap();
        let removed = sweep_stale(tmp.path(), Duration::from_secs(0))
            .await
            .unwrap();
        assert_eq!(removed, 0);
    }

    #[tokio::test]
    async fn each_session_uuid_gets_its_own_dir() {
        let tmp = TempDir::new().unwrap();
        seed_source_clone(tmp.path(), "allan", "harbor", "rails").await;
        let project = project_with_repo("harbor", "rails");

        let a = ensure(tmp.path(), "allan", &project, "uuid-a")
            .await
            .unwrap();
        let b = ensure(tmp.path(), "allan", &project, "uuid-b")
            .await
            .unwrap();
        assert_ne!(a, b);
        // Each has its own working tree.
        tokio::fs::write(a.join("rails").join("MARKER_A"), "a")
            .await
            .unwrap();
        assert!(!b.join("rails").join("MARKER_A").exists());
    }

    /// 7-repo project mirrors a production install (core-api, HarborRails,
    /// HarborApps, keycloak, glue, Harbor_Task_Manager,
    /// workforce). The pre-PR1 sequential implementation hit
    /// `CLONE_TIMEOUT` because each clone serialised behind the previous;
    /// parallelising fans them out concurrently. This test asserts the
    /// "all repos materialised" invariant, regardless of order.
    #[tokio::test]
    async fn parallel_materialise_all_seven_harbor_repos() {
        let tmp = TempDir::new().unwrap();
        let aliases = [
            "core-api",
            "HarborRails",
            "HarborApps",
            "keycloak",
            "glue",
            "Harbor_Task_Manager",
            "workforce",
        ];
        for a in aliases {
            seed_source_clone(tmp.path(), "allan", "harbor", a).await;
        }
        let mut project = Project::new("allan", "harbor");
        for a in aliases {
            project.repos.push(ProjectRepo {
                alias: a.into(),
                source: RepoSource::Github {
                    owner: "harborlabs".into(),
                    repo: a.into(),
                },
                default_branch: None,
                setup: None,
            });
        }

        let session_root = ensure(tmp.path(), "allan", &project, "uuid-all")
            .await
            .unwrap();
        for a in aliases {
            assert!(
                session_root.join(a).join(".git").exists(),
                "expected materialised .git for {a}"
            );
            assert!(
                session_root.join(a).join("README").exists(),
                "expected materialised README for {a}"
            );
        }
    }

    /// `clone_one_repo`'s timeout error must include the repo alias and
    /// the source path. The harbor failure was just "git clone --local
    /// timed out" with no clue which of 7 repos was the slow one;
    /// production debugging needs that label in the error itself.
    #[tokio::test]
    async fn clone_timeout_error_includes_repo_alias_and_source_path() {
        let tmp = TempDir::new().unwrap();
        // Source must exist (clone_one_repo doesn't pre-check that). We
        // force a timeout by setting it to 0; tokio rounds up to the
        // smallest tickable value but the timeout still fires before
        // git can finish, even on the trivial seeded repo below.
        seed_source_clone(tmp.path(), "allan", "harbor", "rails").await;
        let source = layout::work_clone_dir(tmp.path(), "allan", "harbor", "rails");
        let dest = tmp.path().join("dest-rails");
        let err = clone_one_repo(&source, &dest, "rails", Duration::from_nanos(1))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("'rails'"),
            "timeout error must mention repo alias 'rails': {err}"
        );
        assert!(
            err.contains("timed out"),
            "timeout error must say 'timed out': {err}"
        );
    }

    /// End-to-end stash test on a project-shaped session: two repo
    /// subdirs, one dirty, one clean. Only the dirty one should
    /// produce a stash entry.
    #[tokio::test]
    async fn stash_dirty_repos_stashes_only_repos_with_uncommitted_changes() {
        let tmp = TempDir::new().unwrap();
        let session_root = tmp.path().join("cc-session");
        tokio::fs::create_dir_all(&session_root).await.unwrap();

        // Build two real on-disk clones inside session_root so the
        // helper sees them as project-bound subdirs.
        for alias in ["rails", "frontend"] {
            seed_source_clone(tmp.path(), "allan", "harbor", alias).await;
            // Move the seeded clone INTO session_root under its alias.
            let from = layout::work_clone_dir(tmp.path(), "allan", "harbor", alias);
            let to = session_root.join(alias);
            tokio::fs::rename(&from, &to).await.unwrap();
        }

        // Make `rails` dirty by editing a tracked file; leave `frontend` clean.
        tokio::fs::write(session_root.join("rails").join("README"), "edited!\n")
            .await
            .unwrap();

        let stashes = stash_dirty_repos(sb(&session_root).as_ref(), "WIP: test").await;
        assert_eq!(stashes.len(), 1);
        assert_eq!(stashes[0].repo, "rails");
        assert_eq!(stashes[0].stash_ref, "stash@{0}");
        // The recovery SHA should be a 40-char hex.
        assert_eq!(stashes[0].sha.len(), 40);
        assert!(stashes[0].sha.chars().all(|c| c.is_ascii_hexdigit()));

        // The stash actually exists in the rails repo.
        let list = Command::new("git")
            .args(["stash", "list"])
            .current_dir(session_root.join("rails"))
            .output()
            .await
            .unwrap();
        assert!(list.status.success());
        let listed = String::from_utf8_lossy(&list.stdout);
        assert!(
            listed.contains("WIP: test"),
            "expected stash list to contain our label; got: {listed}"
        );

        // The working tree is clean again post-stash.
        let porcelain = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(session_root.join("rails"))
            .output()
            .await
            .unwrap();
        assert!(
            porcelain.stdout.is_empty(),
            "repo should be clean after stash"
        );
    }

    #[tokio::test]
    async fn stash_dirty_repos_handles_repo_bound_workdir() {
        // Repo-bound /cc has the workdir as a single git repo, not a
        // dir of repo subdirs. The helper detects that via `.git` at
        // the workdir root.
        let tmp = TempDir::new().unwrap();
        seed_source_clone(tmp.path(), "allan", "solo", "single").await;
        let workdir = layout::work_clone_dir(tmp.path(), "allan", "solo", "single");
        tokio::fs::write(workdir.join("README"), "edited\n")
            .await
            .unwrap();

        let stashes = stash_dirty_repos(sb(&workdir).as_ref(), "WIP: solo test").await;
        assert_eq!(stashes.len(), 1);
        // For repo-bound the alias is the workdir's basename.
        assert_eq!(stashes[0].repo, "single");
    }

    #[tokio::test]
    async fn stash_dirty_repos_returns_empty_when_nothing_to_stash() {
        let tmp = TempDir::new().unwrap();
        seed_source_clone(tmp.path(), "allan", "solo", "single").await;
        let workdir = layout::work_clone_dir(tmp.path(), "allan", "solo", "single");
        let stashes = stash_dirty_repos(sb(&workdir).as_ref(), "WIP: clean").await;
        assert!(stashes.is_empty());
    }

    #[tokio::test]
    async fn clone_concurrency_clamps_to_at_least_one() {
        // Don't pollute the global env in tests run in parallel — read the
        // current value, set, read, restore. Use a sentinel to detect the
        // bound, not absolute equality (CI may set the var).
        let key = "WORKFORCE_CC_CLONE_CONCURRENCY";
        let prev = std::env::var(key).ok();
        std::env::set_var(key, "0");
        let v = clone_concurrency();
        if let Some(p) = prev {
            std::env::set_var(key, p);
        } else {
            std::env::remove_var(key);
        }
        assert!(
            v >= 1,
            "concurrency must clamp to >= 1 even when env says 0"
        );
    }
}
