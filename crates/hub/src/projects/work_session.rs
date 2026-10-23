//! Per-scope multi-repo workspace for non-CC agent tools.
//!
//! Where `cc_session` materialises a project via `git clone --local` (each
//! claude-code session gets a fully independent object store), this module
//! materialises via `git worktree add` over a per-project bare base. The
//! base is shared across every scope — workflow runs, DM sessions, etc. —
//! so concurrent agent tasks against the same project pay the disk cost
//! once instead of N times.
//!
//! On-disk layout:
//!
//! ```text
//! {project}/work/
//!   bases/{alias}.git              # one bare clone per repo, shared
//!   session/{scope_id}/{alias}/    # one worktree per (scope, repo)
//! ```
//!
//! `ensure(scope_id)` returns the parent dir `session/{scope_id}/` —
//! every project repo is a sibling subdir under it, so an agent tool dropped
//! there can `cd rails && cargo test`, `cd frontend && npm install`, etc.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use eyre::{eyre, Result};
use tracing::{debug, warn};
use workforce_projects::{Project, RepoSource};

use super::git_ops::{github_credential_helper, github_https_url, run_git};
use super::layout::{self, WorkRoots};
use super::{touch_liveness, LIVENESS_FILE};
use crate::git_hardening::lock_down_config;

/// `true` if a work-session worktree has already been materialised for
/// `(agent, project, scope_id)`. Callers that persist a scope id across
/// restarts (e.g. `coding.session`) use this to detect when the on-disk
/// worktree is gone — a redeploy wiped it, or the scope predates this backend
/// — so they can start the scope fresh instead of resuming into an empty tree.
pub fn session_exists(
    roots: &WorkRoots,
    agent_id: &str,
    project_name: &str,
    scope_id: &str,
) -> bool {
    layout::work_session_dir(roots, agent_id, project_name, scope_id).exists()
}

/// Resolve a `{github_owner -> PAT}` map covering every repo in `project`,
/// layering per-user overrides, the agent vault, and the global vault (see
/// [`crate::user_config::LayeredUserVault`]). Must be called in a task where
/// `CURRENT_USER_ID` is in scope so user-scoped overrides resolve. Owners with
/// no resolvable token are omitted (anonymous clone — public repos only).
pub fn resolve_github_tokens(
    project: &Project,
    agent_vault: Option<&std::sync::Arc<crate::vault::Vault>>,
    global_vault: Option<&std::sync::Arc<crate::vault::Vault>>,
) -> HashMap<String, String> {
    let Some(agent) = agent_vault else {
        return HashMap::new();
    };
    let layered = crate::user_config::LayeredUserVault::new(
        std::sync::Arc::clone(agent),
        global_vault.cloned(),
    );
    project
        .repos
        .iter()
        .filter_map(|r| {
            let RepoSource::Github { owner, .. } = &r.source;
            layered.get_github_pat(owner).map(|t| (owner.clone(), t))
        })
        .collect()
}

/// Materialise `work/session/{scope_id}/` for `project`: one worktree per
/// repo, or an empty persistent scratch dir for a project with no repos.
/// Idempotent: already-materialised scopes return their path without
/// re-creating any worktrees. Atomic-or-clean: a partial failure mid-way
/// removes the incomplete session dir before returning so a retry gets a
/// fresh attempt.
///
/// `tokens_by_owner` is a pre-resolved map of `{github_owner -> PAT}` —
/// usually built by `ProjectsToolsContext::resolve_github_tokens_for_project`
/// at the call site. Empty map = anonymous clones (public repos only).
pub async fn ensure(
    roots: &WorkRoots,
    agent_id: &str,
    project: &Project,
    scope_id: &str,
    tokens_by_owner: &HashMap<String, String>,
) -> Result<PathBuf> {
    let session_root = layout::work_session_dir(roots, agent_id, &project.name, scope_id);
    if session_root.exists() {
        touch_liveness(&session_root).await;
        return Ok(session_root);
    }
    tokio::fs::create_dir_all(&session_root).await?;

    match materialise(roots, agent_id, project, scope_id, tokens_by_owner).await {
        Ok(()) => {
            touch_liveness(&session_root).await;
            Ok(session_root)
        }
        Err(e) => {
            let _ = tokio::fs::remove_dir_all(&session_root).await;
            Err(e)
        }
    }
}

/// Tear down scope `scope_id` wherever it lives — both the per-repo worktrees
/// (so git's worktree registry is cleaned up) and the on-disk directory. A
/// scope materialised before a hot root was configured is found at its old
/// location. No-op if nothing is there.
pub async fn cleanup(
    roots: &WorkRoots,
    agent_id: &str,
    project_name: &str,
    scope_id: &str,
) -> Result<()> {
    for work_root in layout::work_roots_for_teardown(roots, agent_id, project_name, scope_id) {
        let session_root = layout::sessions_dir_of(&work_root).join(scope_id);
        remove_session_dir(&work_root, &session_root).await?;
    }
    Ok(())
}

/// Remove one scope directory under `work_root`, unregistering each repo
/// worktree from the base in the same tree.
async fn remove_session_dir(work_root: &Path, session_root: &Path) -> Result<()> {
    if !session_root.exists() {
        return Ok(());
    }

    if let Ok(mut entries) = tokio::fs::read_dir(session_root).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let alias = entry.file_name().to_string_lossy().into_owned();
            let base = layout::base_dir_of(work_root, &alias);
            if base.exists() {
                let _ = run_git(
                    Some(&base),
                    &[
                        "worktree",
                        "remove",
                        "--force",
                        &entry.path().to_string_lossy(),
                    ],
                    None,
                )
                .await;
            }
        }
    }

    tokio::fs::remove_dir_all(session_root).await?;
    Ok(())
}

/// Reap work-session worktrees whose scope id starts with `prefix` for
/// `project`, EXCEPT those `is_active` reports live — used by `coding.fanout` to
/// clear its ephemeral `fanout-*` worktrees orphaned by a process restart (its
/// detached executor dies on redeploy) without touching a concurrent live
/// fan-out's worktrees. `is_active` is queried per entry at inspection time (NOT
/// a pre-scan snapshot): a concurrent fan-out registers a scope before it hits
/// disk, so checking liveness when the entry is observed guarantees a
/// just-created live scope is never reaped. After a restart `is_active` is
/// always false, so every on-disk `prefix*` scope is a true orphan. Best-effort:
/// a missing root is a no-op, and each scope is torn down via [`cleanup`].
pub async fn cleanup_orphans(
    roots: &WorkRoots,
    agent_id: &str,
    project_name: &str,
    prefix: &str,
    is_active: &(dyn Fn(&str) -> bool + Sync),
) -> Result<()> {
    for work_root in layout::work_trees(roots, agent_id, project_name) {
        let Ok(mut entries) = tokio::fs::read_dir(layout::sessions_dir_of(&work_root)).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let scope = entry.file_name().to_string_lossy().into_owned();
            if scope.starts_with(prefix) && !is_active(&scope) {
                let _ = cleanup(roots, agent_id, project_name, &scope).await;
            }
        }
    }
    Ok(())
}

/// What the sweeper should do with one scope directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Liveness {
    /// Used within the ttl — leave it alone.
    Fresh,
    /// Idle for this long; reclaim it.
    Stale(Duration),
    /// No sentinel: predates the sweeper, or was written by a hub build that
    /// didn't stamp. Stamped and kept for one cycle rather than reclaimed —
    /// these hold real worktrees that may carry uncommitted work, and a sweep
    /// firing just after a deploy would otherwise race a live round.
    Unstamped,
}

async fn liveness(scope_dir: &Path, now: SystemTime, ttl: Duration) -> Liveness {
    let Ok(meta) = tokio::fs::metadata(scope_dir.join(LIVENESS_FILE)).await else {
        return Liveness::Unstamped;
    };
    // An unreadable or future-dated stamp keeps the workspace: never delete
    // real work over a clock that disagrees. A future stamp then pins the
    // scope until wall time catches up, so make that visible.
    match meta.modified().map(|m| now.duration_since(m)) {
        Ok(Ok(age)) if age > ttl => Liveness::Stale(age),
        Ok(Ok(_)) => Liveness::Fresh,
        Ok(Err(e)) => {
            warn!(
                path = %scope_dir.display(),
                ahead_secs = e.duration().as_secs(),
                "work-session sweeper: liveness stamp is in the future; workspace kept until wall time passes it"
            );
            Liveness::Fresh
        }
        Err(_) => Liveness::Fresh,
    }
}

/// Reclaim session scopes idle past `ttl`. The persistent tree
/// (`{data_dir}/agents/*/projects/*/work/session/*`) is shared by every
/// instance and swept only when `include_shared`; the hot tree
/// (`{hot}/*/*/session/*`) is this instance's own and always swept. Returns
/// how many were removed.
///
/// Unregisters each worktree from its base rather than just removing the
/// directory: deleting one without unregistering leaves a phantom entry that
/// breaks a later `worktree add` for the same scope.
pub async fn sweep_stale(roots: &WorkRoots, ttl: Duration, include_shared: bool) -> Result<usize> {
    let now = SystemTime::now();
    let mut removed = 0usize;
    if include_shared {
        let agents_root = roots.data_dir.join("agents");
        removed += sweep_tree(
            &agents_root,
            |agent_dir| agent_dir.join("projects"),
            |project_dir| project_dir.join("work"),
            now,
            ttl,
        )
        .await?;
    }
    if let Some(hot) = &roots.hot {
        removed += sweep_tree(hot, Path::to_path_buf, Path::to_path_buf, now, ttl).await?;
    }
    Ok(removed)
}

/// Walk `agents_root/{agent}` then `projects_of(agent_dir)/{project}` and sweep
/// the scopes under `work_root_of(project_dir)`.
async fn sweep_tree(
    agents_root: &Path,
    projects_of: impl Fn(&Path) -> PathBuf,
    work_root_of: impl Fn(&Path) -> PathBuf,
    now: SystemTime,
    ttl: Duration,
) -> Result<usize> {
    if !agents_root.exists() {
        return Ok(0);
    }
    let mut removed = 0usize;
    let mut agent_iter = match tokio::fs::read_dir(agents_root).await {
        Ok(d) => d,
        Err(e) => return Err(eyre!("read_dir({}): {e}", agents_root.display())),
    };
    while let Some(agent_entry) = agent_iter.next_entry().await? {
        let agent_id = agent_entry.file_name().to_string_lossy().into_owned();
        let projects_dir = projects_of(&agent_entry.path());
        if !projects_dir.is_dir() {
            continue;
        }
        let mut project_iter = match tokio::fs::read_dir(&projects_dir).await {
            Ok(d) => d,
            Err(e) => {
                warn!(path = %projects_dir.display(), error = %e, "work-session sweeper: unreadable projects dir");
                continue;
            }
        };
        loop {
            let project_entry = match project_iter.next_entry().await {
                Ok(Some(e)) => e,
                Ok(None) => break,
                Err(e) => {
                    warn!(path = %projects_dir.display(), error = %e, "work-session sweeper: projects walk ended early");
                    break;
                }
            };
            let project = project_entry.file_name().to_string_lossy().into_owned();
            // Per-project so one unreadable tree cannot end the pass for
            // every agent after it — on the volume this exists to protect,
            // a silent abort is the failure that matters.
            match sweep_work_root(&work_root_of(&project_entry.path()), now, ttl).await {
                Ok(n) => removed += n,
                Err(e) => {
                    warn!(agent = %agent_id, project = %project, error = %e, "work-session sweeper: project pass failed");
                }
            }
        }
    }
    Ok(removed)
}

async fn sweep_work_root(work_root: &Path, now: SystemTime, ttl: Duration) -> Result<usize> {
    let sessions_root = layout::sessions_dir_of(work_root);
    if !sessions_root.is_dir() {
        return Ok(0);
    }
    let mut removed = 0usize;
    let mut scope_iter = tokio::fs::read_dir(&sessions_root).await?;
    while let Some(scope_entry) = scope_iter.next_entry().await? {
        let path = scope_entry.path();
        if !path.is_dir() {
            continue;
        }
        match liveness(&path, now, ttl).await {
            Liveness::Fresh => {}
            Liveness::Unstamped => touch_liveness(&path).await,
            Liveness::Stale(age) => match remove_session_dir(work_root, &path).await {
                Ok(()) => {
                    removed += 1;
                    debug!(
                        path = %path.display(),
                        age_secs = age.as_secs(),
                        "work-session sweeper: reclaimed idle workspace"
                    );
                }
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "work-session sweeper: cleanup failed");
                }
            },
        }
    }
    Ok(removed)
}

/// Spawn a background task running [`sweep_stale`] every `interval`. Lives for
/// the process lifetime; a failed pass logs and continues so a transient FS
/// error cannot disable the sweeper.
///
/// Leadership is consulted **each tick**, not sampled at spawn: instances share
/// the data volume, so exactly one should sweep it, and a gate read once at
/// boot would leave a demoted instance sweeping forever while the new leader
/// never starts. The hot root is this instance's own, so every instance sweeps
/// it regardless.
pub fn spawn_sweeper(
    roots: WorkRoots,
    interval: Duration,
    ttl: Duration,
    leader_gate: Arc<crate::leader_gate::LeaderGate>,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        // First tick fires immediately in `Burst` mode — skip it so startup
        // doesn't race other init work.
        tick.tick().await;
        loop {
            tick.tick().await;
            match sweep_stale(&roots, ttl, leader_gate.is_leader()).await {
                Ok(n) if n > 0 => debug!(removed = n, "work-session sweeper pass"),
                Ok(_) => {}
                Err(e) => warn!(error = %e, "work-session sweeper pass failed"),
            }
        }
    });
}

async fn materialise(
    roots: &WorkRoots,
    agent_id: &str,
    project: &Project,
    scope_id: &str,
    tokens_by_owner: &HashMap<String, String>,
) -> Result<()> {
    for repo in &project.repos {
        let RepoSource::Github { owner, repo: name } = &repo.source;
        let base = layout::work_base_dir_for(roots, agent_id, &project.name, scope_id, &repo.alias);
        let branch = project.effective_default_branch(&repo.alias).to_string();
        let token = tokens_by_owner.get(owner).map(String::as_str);
        ensure_bare_base(&base, owner, name, &branch, token)
            .await
            .map_err(|e| enrich_clone_error(e, owner, name, token.is_some()))?;

        let wt =
            layout::work_session_repo_dir(roots, agent_id, &project.name, scope_id, &repo.alias);
        run_git(
            Some(&base),
            &[
                "worktree",
                "add",
                "--detach",
                &wt.to_string_lossy(),
                &format!("origin/{branch}"),
            ],
            None,
        )
        .await?;
    }
    Ok(())
}

/// Turn an opaque clone/fetch failure for a repo with no resolved token into
/// an actionable error naming the vault path the operator must populate.
/// Without this the agent sees git's raw "repository not found" (private repo,
/// anonymous clone) and confabulates a cause; with a token present the
/// original error is returned untouched.
fn enrich_clone_error(err: eyre::Report, owner: &str, repo: &str, had_token: bool) -> eyre::Report {
    if had_token {
        return err;
    }
    eyre!(
        "could not access {owner}/{repo}: no GitHub token resolved for owner `{owner}`. \
         Set a write-scoped PAT at vault path `skills/github/workspaces/{owner}/token` \
         (or the shared `skills/github/token`). Original error: {err}"
    )
}

/// Make sure `{project}/work/bases/{alias}.git` exists and is up to date.
/// First call clones; subsequent calls just `git fetch origin` to refresh.
/// Token-bearing URL is used only at clone time and is then rewritten to
/// the bare HTTPS form, with a local credential helper installed for
/// later fetches.
async fn ensure_bare_base(
    base: &Path,
    owner: &str,
    repo: &str,
    branch: &str,
    token: Option<&str>,
) -> Result<()> {
    let (token_url, public_url) = github_https_url(owner, repo, token);
    if !base.exists() {
        if let Some(parent) = base.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        run_git(
            None,
            &[
                "clone",
                "--bare",
                "--branch",
                branch,
                &token_url,
                &base.to_string_lossy(),
            ],
            token,
        )
        .await?;
    }

    provision_base_config(base, &public_url, token).await?;

    // Make `origin/<branch>` resolvable for the `git worktree add` in
    // `materialise` — see [`ensure_remote_tracking`].
    ensure_remote_tracking(base, token).await
}

/// Re-assert the token-free remote URL and (re)install the credential helper on
/// EVERY call — not just the cold clone — so a base created by the fetcher, or
/// by a prior run whose token has since rotated or expired, still ends up with
/// fresh, write-capable credentials. Keeping the token out of the persisted URL
/// means auth always comes from the helper.
///
/// The config is locked here rather than at the end of provisioning: the base is
/// group-writable so the agent can commit from its worktrees, and a later step
/// failing must not leave the config it just wrote open to editing.
async fn provision_base_config(base: &Path, public_url: &str, token: Option<&str>) -> Result<()> {
    run_git(
        Some(base),
        &["remote", "set-url", "origin", public_url],
        token,
    )
    .await?;
    if let Some(t) = token {
        install_credential_helper(base, t).await?;
    }
    lock_down_config(base);
    Ok(())
}

/// (Re)install the per-clone credential helper, single-valued via
/// `--replace-all` so repeated `ensure` calls replace the token instead of
/// accumulating one stale `credential.helper` entry per call.
async fn install_credential_helper(base: &Path, token: &str) -> Result<()> {
    run_git(
        Some(base),
        &[
            "config",
            "--local",
            "--replace-all",
            "credential.helper",
            &github_credential_helper(token),
        ],
        Some(token),
    )
    .await
    .map(|_| ())
}

/// Idempotently assert the remote-tracking fetch refspec on a bare `base` and
/// fetch, so `origin/<branch>` resolves for `git worktree add`. `git clone
/// --bare` configures no such refspec, and a base pre-created by the project
/// fetcher lacks it too — so on the reuse path a bare `git fetch origin` only
/// updates FETCH_HEAD and leaves `origin/<branch>` unresolvable ("fatal:
/// invalid reference: origin/..."), which previously broke `coding.session` /
/// `open_workspace` provisioning against a fetcher-created base. `--replace-all`
/// keeps it idempotent regardless of any pre-existing refspec values.
async fn ensure_remote_tracking(base: &Path, token: Option<&str>) -> Result<()> {
    run_git(
        Some(base),
        &[
            "config",
            "--replace-all",
            "remote.origin.fetch",
            "+refs/heads/*:refs/remotes/origin/*",
        ],
        token,
    )
    .await?;
    run_git(Some(base), &["fetch", "origin"], token).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::process::Command;
    use workforce_projects::{ProjectRepo, RepoSource};

    /// Stand up a real bare repo + initial commit so we can clone into a
    /// `bases/{alias}.git` from a `file://` URL.
    async fn local_bare(tmp: &Path, name: &str) -> PathBuf {
        let bare = tmp.join(format!("{name}.git"));
        tokio::fs::create_dir(&bare).await.unwrap();
        Command::new("git")
            .args(["init", "--bare", "--initial-branch=main"])
            .current_dir(&bare)
            .output()
            .await
            .unwrap();

        let seed = tmp.join(format!("{name}-seed"));
        Command::new("git")
            .args(["clone", &bare.to_string_lossy(), &seed.to_string_lossy()])
            .output()
            .await
            .unwrap();
        for args in [
            &["config", "user.email", "test@example.com"][..],
            &["config", "user.name", "Test"][..],
        ] {
            Command::new("git")
                .args(args)
                .current_dir(&seed)
                .output()
                .await
                .unwrap();
        }
        tokio::fs::write(seed.join("README"), name).await.unwrap();
        for args in [
            &["add", "."][..],
            &["commit", "-m", "init"][..],
            &["push", "origin", "main"][..],
        ] {
            Command::new("git")
                .args(args)
                .current_dir(&seed)
                .output()
                .await
                .unwrap();
        }
        bare
    }

    fn roots(tmp: &TempDir) -> WorkRoots {
        WorkRoots::persistent(tmp.path())
    }

    /// Mirror `ensure`'s materialisation but plug in a local bare URL
    /// (`file://...`) instead of going through GitHub.
    async fn ensure_local(
        roots: &WorkRoots,
        agent_id: &str,
        project: &Project,
        scope_id: &str,
        sources: &[(&str, &Path)],
    ) -> Result<PathBuf> {
        let session_root = layout::work_session_dir(roots, agent_id, &project.name, scope_id);
        if session_root.exists() {
            return Ok(session_root);
        }
        tokio::fs::create_dir_all(&session_root).await?;
        for repo in &project.repos {
            let base =
                layout::work_base_dir_for(roots, agent_id, &project.name, scope_id, &repo.alias);
            let source_url = sources
                .iter()
                .find(|(a, _)| *a == repo.alias)
                .map(|(_, p)| p.to_string_lossy().to_string())
                .ok_or_else(|| eyre!("no test source for {}", repo.alias))?;
            if !base.exists() {
                if let Some(parent) = base.parent() {
                    tokio::fs::create_dir_all(parent).await.unwrap();
                }
                run_git(
                    None,
                    &[
                        "clone",
                        "--bare",
                        "--branch",
                        "main",
                        &source_url,
                        &base.to_string_lossy(),
                    ],
                    None,
                )
                .await?;
            }
            ensure_remote_tracking(&base, None).await?;
            let wt = layout::work_session_repo_dir(
                roots,
                agent_id,
                &project.name,
                scope_id,
                &repo.alias,
            );
            run_git(
                Some(&base),
                &[
                    "worktree",
                    "add",
                    "--detach",
                    &wt.to_string_lossy(),
                    "origin/main",
                ],
                None,
            )
            .await?;
        }
        Ok(session_root)
    }

    fn project_with_repos(name: &str, aliases: &[&str]) -> Project {
        let mut p = Project::new("allan", name);
        for alias in aliases {
            p.repos.push(ProjectRepo {
                alias: (*alias).into(),
                source: RepoSource::Github {
                    owner: "acme".into(),
                    repo: (*alias).into(),
                },
                default_branch: None,
                setup: None,
            });
        }
        p
    }

    #[tokio::test]
    async fn ensure_remote_tracking_makes_origin_branch_resolvable_on_reused_base() {
        // Regression: a base pre-created by the project fetcher is a bare clone
        // WITHOUT a remote-tracking refspec, so `origin/main` doesn't resolve and
        // `git worktree add origin/main` fails ("invalid reference: origin/main")
        // — the failure that broke coding.session in prod. The reuse path must
        // re-assert the refspec. (It slipped through before because the test
        // path was a fork that never exercised the real reuse logic.)
        let tmp = TempDir::new().unwrap();
        let src = local_bare(tmp.path(), "repo").await;
        let base = tmp.path().join("base.git");

        // Simulate the fetcher: a bare clone with NO fetch refspec configured.
        run_git(
            None,
            &[
                "clone",
                "--bare",
                "--branch",
                "main",
                &src.to_string_lossy(),
                &base.to_string_lossy(),
            ],
            None,
        )
        .await
        .unwrap();
        // origin/main is unresolvable on a bare clone with no refspec.
        assert!(run_git(Some(&base), &["rev-parse", "origin/main"], None)
            .await
            .is_err());

        ensure_remote_tracking(&base, None).await.unwrap();

        // Now it resolves — so `git worktree add origin/main` would succeed.
        assert!(run_git(Some(&base), &["rev-parse", "origin/main"], None)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn ensure_materialises_empty_scratch_dir_for_repoless_project() {
        let tmp = TempDir::new().unwrap();
        let project = Project::new("allan", "skills");
        let root = ensure(&roots(&tmp), "allan", &project, "wf-1", &HashMap::new())
            .await
            .unwrap();
        assert!(root.is_dir());
        // Empty but for the liveness sentinel the sweeper keys on.
        let entries: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from(LIVENESS_FILE)]);

        // Idempotent: files created in the scratch workspace survive re-ensure.
        tokio::fs::write(root.join("notes.md"), "kept")
            .await
            .unwrap();
        let again = ensure(&roots(&tmp), "allan", &project, "wf-1", &HashMap::new())
            .await
            .unwrap();
        assert_eq!(again, root);
        assert!(root.join("notes.md").exists());
    }

    #[tokio::test]
    async fn cleanup_removes_scratch_dir_with_plain_files() {
        let tmp = TempDir::new().unwrap();
        let project = Project::new("allan", "skills");
        let root = ensure(&roots(&tmp), "allan", &project, "wf-1", &HashMap::new())
            .await
            .unwrap();
        tokio::fs::create_dir(root.join("skill-src")).await.unwrap();
        tokio::fs::write(root.join("skill-src").join("lib.rs"), "fn main() {}")
            .await
            .unwrap();

        cleanup(&roots(&tmp), "allan", "skills", "wf-1")
            .await
            .unwrap();
        assert!(!root.exists());
    }

    #[tokio::test]
    async fn materialises_one_worktree_per_repo_under_session_dir() {
        let tmp = TempDir::new().unwrap();
        let rails_bare = local_bare(tmp.path(), "rails").await;
        let frontend_bare = local_bare(tmp.path(), "frontend").await;
        let project = project_with_repos("harbor", &["rails", "frontend"]);

        let session_root = ensure_local(
            &roots(&tmp),
            "allan",
            &project,
            "wf-1",
            &[("rails", &rails_bare), ("frontend", &frontend_bare)],
        )
        .await
        .unwrap();

        assert!(session_root.join("rails").join("README").exists());
        assert!(session_root.join("frontend").join("README").exists());
        assert!(layout::work_base_dir(tmp.path(), "allan", "harbor", "rails").exists());
        assert!(layout::work_base_dir(tmp.path(), "allan", "harbor", "frontend").exists());
    }

    #[tokio::test]
    async fn each_scope_gets_its_own_session_dir_sharing_bases() {
        let tmp = TempDir::new().unwrap();
        let rails_bare = local_bare(tmp.path(), "rails").await;
        let project = project_with_repos("harbor", &["rails"]);

        let a = ensure_local(
            &roots(&tmp),
            "allan",
            &project,
            "wf-1",
            &[("rails", &rails_bare)],
        )
        .await
        .unwrap();
        let b = ensure_local(
            &roots(&tmp),
            "allan",
            &project,
            "wf-2",
            &[("rails", &rails_bare)],
        )
        .await
        .unwrap();

        assert_ne!(a, b);
        assert!(a.join("rails").join(".git").exists());
        assert!(b.join("rails").join(".git").exists());
        // Single bare base shared across both scopes.
        let bases = layout::work_bases_dir(tmp.path(), "allan", "harbor");
        let entries: Vec<_> = std::fs::read_dir(&bases)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1);

        // Scope isolation: a write in `a` doesn't appear in `b`.
        tokio::fs::write(a.join("rails").join("MARKER"), "a")
            .await
            .unwrap();
        assert!(!b.join("rails").join("MARKER").exists());
    }

    #[tokio::test]
    async fn ensure_stamps_liveness_on_creation_and_on_reuse() {
        let tmp = TempDir::new().unwrap();
        let project = Project::new("allan", "skills");
        let root = ensure(&roots(&tmp), "allan", &project, "wf-1", &HashMap::new())
            .await
            .unwrap();
        let sentinel = root.join(LIVENESS_FILE);
        assert!(sentinel.exists(), "a fresh workspace must be stamped");

        tokio::fs::remove_file(&sentinel).await.unwrap();
        ensure(&roots(&tmp), "allan", &project, "wf-1", &HashMap::new())
            .await
            .unwrap();
        assert!(
            sentinel.exists(),
            "the reuse path must re-stamp, or an active workspace ages out"
        );
    }

    #[tokio::test]
    async fn liveness_classifies_by_sentinel_age() {
        let tmp = TempDir::new().unwrap();
        let scope = tmp.path().join("scope");
        tokio::fs::create_dir_all(&scope).await.unwrap();
        let ttl = Duration::from_secs(7 * 24 * 3600);

        assert_eq!(
            liveness(&scope, SystemTime::now(), ttl).await,
            Liveness::Unstamped
        );

        touch_liveness(&scope).await;
        assert_eq!(
            liveness(&scope, SystemTime::now(), ttl).await,
            Liveness::Fresh
        );

        // Age is injected rather than slept for.
        let later = SystemTime::now() + ttl + Duration::from_secs(60);
        assert!(matches!(
            liveness(&scope, later, ttl).await,
            Liveness::Stale(_)
        ));
    }

    #[tokio::test]
    async fn sweep_gives_unstamped_scopes_one_grace_pass_then_reclaims_them() {
        let tmp = TempDir::new().unwrap();
        let project = Project::new("allan", "skills");
        let legacy = ensure(&roots(&tmp), "allan", &project, "legacy", &HashMap::new())
            .await
            .unwrap();
        tokio::fs::write(legacy.join("work.rs"), "unfinished")
            .await
            .unwrap();
        // Predates the sweeper: no sentinel.
        tokio::fs::remove_file(legacy.join(LIVENESS_FILE))
            .await
            .unwrap();

        // Zero ttl would reclaim anything stamped, so the grace pass is the
        // only reason this survives — and it must survive.
        assert_eq!(
            sweep_stale(&roots(&tmp), Duration::from_secs(0), true)
                .await
                .unwrap(),
            0
        );
        assert!(legacy.join("work.rs").exists());
        assert!(legacy.join(LIVENESS_FILE).exists(), "grace pass stamps it");

        // Now that it carries a stamp, the next pass past the ttl takes it.
        assert_eq!(
            sweep_stale(&roots(&tmp), Duration::from_secs(0), true)
                .await
                .unwrap(),
            1
        );
        assert!(!legacy.exists());
    }

    #[test]
    fn work_session_ttl_defaults_to_a_week() {
        let projects = workforce_common::settings::ProjectsConfig::default();
        assert_eq!(projects.work_session_ttl_secs, 7 * 24 * 60 * 60);
    }

    #[tokio::test]
    async fn sweep_keeps_fresh_scopes() {
        let tmp = TempDir::new().unwrap();
        let project = Project::new("allan", "skills");
        let fresh = ensure(&roots(&tmp), "allan", &project, "live", &HashMap::new())
            .await
            .unwrap();

        assert_eq!(
            sweep_stale(&roots(&tmp), Duration::from_secs(7 * 24 * 3600), true)
                .await
                .unwrap(),
            0
        );
        assert!(fresh.exists());
    }

    #[tokio::test]
    async fn sweep_unregisters_worktrees_so_the_scope_can_be_remade() {
        let tmp = TempDir::new().unwrap();
        let rails_bare = local_bare(tmp.path(), "rails").await;
        let project = project_with_repos("harbor", &["rails"]);
        let sources = [("rails", &rails_bare as &Path)];

        let root = ensure_local(&roots(&tmp), "allan", &project, "wf-1", &sources)
            .await
            .unwrap();
        touch_liveness(&root).await;
        assert!(root.join("rails").join(".git").exists());

        // Two passes: the first stamps nothing (already stamped) and reclaims.
        assert_eq!(
            sweep_stale(&roots(&tmp), Duration::from_secs(0), true)
                .await
                .unwrap(),
            1
        );
        assert!(!root.exists());

        // Re-materialising the same scope proves the worktree registration
        // went with the directory — a raw remove would leave a phantom entry
        // and `git worktree add` would refuse.
        let again = ensure_local(&roots(&tmp), "allan", &project, "wf-1", &sources)
            .await
            .unwrap();
        assert!(again.join("rails").join(".git").exists());
    }

    #[tokio::test]
    async fn cleanup_removes_session_dir_and_unregisters_worktrees() {
        let tmp = TempDir::new().unwrap();
        let rails_bare = local_bare(tmp.path(), "rails").await;
        let project = project_with_repos("harbor", &["rails"]);

        ensure_local(
            &roots(&tmp),
            "allan",
            &project,
            "wf-1",
            &[("rails", &rails_bare)],
        )
        .await
        .unwrap();
        let session_root = layout::work_session_dir(&roots(&tmp), "allan", "harbor", "wf-1");
        assert!(session_root.exists());

        cleanup(&roots(&tmp), "allan", "harbor", "wf-1")
            .await
            .unwrap();
        assert!(!session_root.exists());

        // Re-creating the same scope should succeed (no leftover worktree
        // registration would block `git worktree add`).
        ensure_local(
            &roots(&tmp),
            "allan",
            &project,
            "wf-1",
            &[("rails", &rails_bare)],
        )
        .await
        .unwrap();
        assert!(session_root.exists());
    }

    #[tokio::test]
    async fn cleanup_is_noop_when_session_missing() {
        let tmp = TempDir::new().unwrap();
        cleanup(&roots(&tmp), "allan", "harbor", "missing")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cleanup_orphans_reaps_prefixed_except_active() {
        let tmp = TempDir::new().unwrap();
        let rails_bare = local_bare(tmp.path(), "rails").await;
        let project = project_with_repos("harbor", &["rails"]);
        for scope in ["fanout-aaa", "fanout-bbb", "wf-keep"] {
            ensure_local(
                &roots(&tmp),
                "allan",
                &project,
                scope,
                &[("rails", &rails_bare)],
            )
            .await
            .unwrap();
        }
        cleanup_orphans(&roots(&tmp), "allan", "harbor", "fanout-", &|scope| {
            scope == "fanout-bbb"
        })
        .await
        .unwrap();

        let dir = |scope| layout::work_session_dir(&roots(&tmp), "allan", "harbor", scope);
        assert!(
            !dir("fanout-aaa").exists(),
            "an orphaned fan-out scope is reaped"
        );
        assert!(dir("fanout-bbb").exists(), "an active scope is spared");
        assert!(dir("wf-keep").exists(), "a non-fan-out scope is untouched");
    }

    fn hot_roots(tmp: &TempDir) -> WorkRoots {
        WorkRoots::new(tmp.path().join("data"), Some(&tmp.path().join("fast")))
    }

    #[tokio::test]
    async fn a_hot_root_holds_repo_scopes_with_their_bases_while_scratch_stays_persistent() {
        let tmp = TempDir::new().unwrap();
        let rails_bare = local_bare(tmp.path(), "rails").await;
        let roots = hot_roots(&tmp);
        let project = project_with_repos("harbor", &["rails"]);

        let root = ensure_local(&roots, "allan", &project, "wf-1", &[("rails", &rails_bare)])
            .await
            .unwrap();
        assert!(root.starts_with(tmp.path().join("fast")));
        assert!(root.join("rails").join("README").exists());
        assert!(layout::base_dir_of(&tmp.path().join("fast/work/allan/harbor"), "rails").exists());
        assert!(!layout::work_base_dir(&roots.data_dir, "allan", "harbor", "rails").exists());

        let bench = Project::new("allan", "skills");
        let scratch = ensure(&roots, "allan", &bench, "scratch-abc", &HashMap::new())
            .await
            .unwrap();
        assert!(scratch.starts_with(&roots.data_dir));
    }

    #[tokio::test]
    async fn a_scope_from_before_the_hot_root_is_missing_but_still_torn_down() {
        let tmp = TempDir::new().unwrap();
        let rails_bare = local_bare(tmp.path(), "rails").await;
        let project = project_with_repos("harbor", &["rails"]);
        let persistent = WorkRoots::persistent(tmp.path().join("data"));
        let legacy = ensure_local(
            &persistent,
            "allan",
            &project,
            "wf-1",
            &[("rails", &rails_bare)],
        )
        .await
        .unwrap();

        let roots = hot_roots(&tmp);
        assert!(!session_exists(&roots, "allan", "harbor", "wf-1"));
        cleanup(&roots, "allan", "harbor", "wf-1").await.unwrap();
        assert!(!legacy.exists());
        // The persistent base no longer registers the removed worktree.
        ensure_local(
            &persistent,
            "allan",
            &project,
            "wf-1",
            &[("rails", &rails_bare)],
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn cleanup_orphans_reaps_across_both_roots() {
        let tmp = TempDir::new().unwrap();
        let rails_bare = local_bare(tmp.path(), "rails").await;
        let project = project_with_repos("harbor", &["rails"]);
        let persistent = WorkRoots::persistent(tmp.path().join("data"));
        let roots = hot_roots(&tmp);
        let old = ensure_local(
            &persistent,
            "allan",
            &project,
            "fanout-old",
            &[("rails", &rails_bare)],
        )
        .await
        .unwrap();
        let new = ensure_local(
            &roots,
            "allan",
            &project,
            "fanout-new",
            &[("rails", &rails_bare)],
        )
        .await
        .unwrap();

        cleanup_orphans(&roots, "allan", "harbor", "fanout-", &|_| false)
            .await
            .unwrap();
        assert!(!old.exists());
        assert!(!new.exists());
    }

    #[tokio::test]
    async fn a_follower_sweeps_only_its_own_hot_root() {
        let tmp = TempDir::new().unwrap();
        let rails_bare = local_bare(tmp.path(), "rails").await;
        let project = project_with_repos("harbor", &["rails"]);
        let persistent = WorkRoots::persistent(tmp.path().join("data"));
        let roots = hot_roots(&tmp);
        let shared = ensure_local(
            &persistent,
            "allan",
            &project,
            "old",
            &[("rails", &rails_bare)],
        )
        .await
        .unwrap();
        let local = ensure_local(&roots, "allan", &project, "mine", &[("rails", &rails_bare)])
            .await
            .unwrap();
        touch_liveness(&shared).await;
        touch_liveness(&local).await;

        assert_eq!(
            sweep_stale(&roots, Duration::from_secs(0), false)
                .await
                .unwrap(),
            1
        );
        assert!(!local.exists(), "the instance's own hot tree is swept");
        assert!(
            shared.exists(),
            "a follower must leave the shared tree alone"
        );

        assert_eq!(
            sweep_stale(&roots, Duration::from_secs(0), true)
                .await
                .unwrap(),
            1
        );
        assert!(!shared.exists());
    }

    /// A bare base as `git clone --bare` leaves it: `origin` already present,
    /// which is what `provision_base_config` re-points.
    async fn bare_base_with_origin(tmp: &Path) -> PathBuf {
        let base = tmp.join("base.git");
        run_git(None, &["init", "--bare", &base.to_string_lossy()], None)
            .await
            .unwrap();
        run_git(
            Some(&base),
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/acme/seed.git",
            ],
            None,
        )
        .await
        .unwrap();
        base
    }

    /// The base is group-writable so the agent can commit from its worktrees.
    /// Its config carries the credential helper, and a hub-side `git` reads it
    /// on the next `ensure` — so provisioning must leave it unwritable by the
    /// group, on the cold path and on reuse alike.
    #[tokio::test]
    async fn provisioning_leaves_the_base_config_group_read_only() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let tmp = TempDir::new().unwrap();
            let base = bare_base_with_origin(tmp.path()).await;
            let url = format!("file://{}", tmp.path().join("upstream.git").display());
            let mode = || {
                std::fs::metadata(base.join("config"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777
            };

            std::fs::set_permissions(base.join("config"), std::fs::Permissions::from_mode(0o664))
                .unwrap();
            provision_base_config(&base, &url, Some("tok"))
                .await
                .unwrap();
            assert_eq!(mode(), 0o640, "cold provisioning left the config writable");

            provision_base_config(&base, &url, Some("tok-rotated"))
                .await
                .unwrap();
            assert_eq!(mode(), 0o640, "reuse left the config writable");

            let out = run_git(
                Some(&base),
                &["config", "--local", "--get", "credential.helper"],
                None,
            )
            .await
            .unwrap();
            assert!(
                String::from_utf8_lossy(&out.stdout).contains("tok-rotated"),
                "locking the config must not stop the hub rewriting it"
            );
        }
    }

    /// A token-less project (public repos) writes no helper, and the lock must
    /// still apply — the config still carries the remotes hub-side git reads.
    #[tokio::test]
    async fn provisioning_without_a_token_still_locks_the_config() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let tmp = TempDir::new().unwrap();
            let base = bare_base_with_origin(tmp.path()).await;
            std::fs::set_permissions(base.join("config"), std::fs::Permissions::from_mode(0o666))
                .unwrap();

            provision_base_config(&base, "https://github.com/acme/widgets.git", None)
                .await
                .unwrap();

            let mode = std::fs::metadata(base.join("config"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o640);
        }
    }

    /// Reinstalling the credential helper (what every `ensure` now does) must
    /// replace the entry, not append: a second call with a rotated token leaves
    /// exactly one helper carrying the new token. Without `--replace-all` the
    /// entries would accumulate and stale tokens would shadow the fresh one.
    #[tokio::test]
    async fn install_credential_helper_is_single_valued_across_reuse() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("base.git");
        run_git(None, &["init", "--bare", &base.to_string_lossy()], None)
            .await
            .unwrap();

        install_credential_helper(&base, "tok-stale").await.unwrap();
        install_credential_helper(&base, "tok-fresh").await.unwrap();

        let out = run_git(
            Some(&base),
            &["config", "--local", "--get-all", "credential.helper"],
            None,
        )
        .await
        .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            stdout.lines().count(),
            1,
            "credential.helper must stay single-valued across reuse, got: {stdout:?}"
        );
        assert!(stdout.contains("tok-fresh"), "the fresh token must win");
        assert!(
            !stdout.contains("tok-stale"),
            "the stale token must be gone"
        );
    }
}
