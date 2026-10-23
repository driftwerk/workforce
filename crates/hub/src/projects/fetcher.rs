//! Git operations for a single project repo: clone, pull, status.
//!
//! Working clone (with checkout). For bare clones used as a worktree
//! source, see `work_session::ensure_bare_base`. Both modules share the
//! GitHub URL + credential-helper plumbing in `git_ops`.

use std::path::Path;

use eyre::{eyre, Result};
use serde::Serialize;

use workforce_projects::RepoSource;

use super::git_ops::{github_credential_helper, github_https_url, run_git};

/// Outcome of `pull` for a single repo.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum PullOutcome {
    UpToDate,
    Updated { from: String, to: String },
    Cloned,
}

/// Snapshot of an on-disk repo's git state. Returned by the `projects.show`
/// tool; missing fields mean the directory wasn't a clone yet.
#[derive(Debug, Clone, Serialize, Default)]
pub struct RepoStatus {
    pub cloned: bool,
    pub current_branch: Option<String>,
    pub head_commit: Option<String>,
    pub dirty: bool,
}

pub struct Fetcher {
    /// GitHub PAT used at clone time and embedded in the per-clone credential
    /// helper. `None` means anonymous (public repos only).
    token: Option<String>,
    /// Blobless partial clone (`--single-branch --filter=blob:none`): keeps
    /// full commit/tree history — so incremental `git diff --name-status`
    /// still works — but fetches file contents on demand, which keeps the
    /// initial clone fast on large repos. Only the knowledge indexer sets it;
    /// the /cc base clones stay full because they are re-materialised with
    /// `git clone --local`, which does not carry a partial clone's promisor.
    blobless: bool,
}

impl Fetcher {
    /// Full clone. Used by the /cc flow, whose base clones are `git clone
    /// --local` sources.
    pub fn new(token: Option<String>) -> Self {
        Self {
            token,
            blobless: false,
        }
    }

    /// Blobless partial clone for the indexer's private checkouts (see the
    /// [`blobless`](Self::blobless) field).
    pub fn new_blobless(token: Option<String>) -> Self {
        Self {
            token,
            blobless: true,
        }
    }

    /// Clone `source` into `dest` (which must not yet exist). After the
    /// clone the remote is rewritten to drop the token, and a local
    /// credential helper is installed so subsequent `pull`s still
    /// authenticate.
    pub async fn clone_repo(&self, source: &RepoSource, dest: &Path, branch: &str) -> Result<()> {
        if dest.exists() {
            return Err(eyre!(
                "clone destination already exists: {}",
                dest.display()
            ));
        }
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let RepoSource::Github { owner, repo: name } = source;
        let (token_url, public_url) = github_https_url(owner, name, self.token.as_deref());

        let dest_str = dest.to_string_lossy();
        let argv = clone_args(branch, self.blobless, &token_url, &dest_str);
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        run_git(None, &argv, self.token.as_deref()).await?;

        run_git(
            Some(dest),
            &["remote", "set-url", "origin", &public_url],
            self.token.as_deref(),
        )
        .await?;

        // Disable implicit `git gc` on this clone. Each /cc session
        // materialises a child clone via `git clone --local`, which
        // hardlinks objects from this base. An auto-gc here can prune
        // unreachable objects that those child clones still reference,
        // producing silent corruption that surfaces hours later as
        // "object not found" mid-/cc-turn. Disabling gc.auto is cheap
        // (these clones never grow unbounded — pulls reset --hard) and
        // removes the foot-gun outright.
        run_git(
            Some(dest),
            &["config", "--local", "gc.auto", "0"],
            self.token.as_deref(),
        )
        .await?;

        self.install_credential_helper(dest).await?;

        Ok(())
    }

    /// (Re)install the credential helper from the configured token, single-valued
    /// via `--replace-all` so the reuse path (`pull_repo`) self-heals a rotated
    /// token instead of authenticating a `fetch` with the one baked at clone
    /// time. No-op for anonymous (token-less) fetchers.
    async fn install_credential_helper(&self, dest: &Path) -> Result<()> {
        if let Some(t) = &self.token {
            run_git(
                Some(dest),
                &[
                    "config",
                    "--local",
                    "--replace-all",
                    "credential.helper",
                    &github_credential_helper(t),
                ],
                self.token.as_deref(),
            )
            .await?;
        }
        Ok(())
    }

    /// Fast-forward `dest` to `origin/{branch}`. If the directory doesn't
    /// exist yet, perform an initial clone instead — callers asking to pull
    /// generally just want "make this match upstream". Returns `Cloned` when
    /// the dir was missing, `UpToDate` when nothing changed, otherwise
    /// `Updated { from, to }` with short SHAs.
    pub async fn pull_repo(
        &self,
        source: &RepoSource,
        dest: &Path,
        branch: &str,
    ) -> Result<PullOutcome> {
        if !dest.exists() {
            self.clone_repo(source, dest, branch).await?;
            return Ok(PullOutcome::Cloned);
        }
        // Reinstall the credential helper before fetching so a rotated token
        // self-heals on the reuse path — the clone-time helper would otherwise
        // keep authenticating with the stale token until the base is wiped.
        self.install_credential_helper(dest).await?;
        let before = head_sha(dest).await?;
        run_git(Some(dest), &["fetch", "origin"], self.token.as_deref()).await?;
        run_git(
            Some(dest),
            &["reset", "--hard", &format!("origin/{branch}")],
            self.token.as_deref(),
        )
        .await?;
        let after = head_sha(dest).await?;
        if before == after {
            Ok(PullOutcome::UpToDate)
        } else {
            Ok(PullOutcome::Updated {
                from: short(&before),
                to: short(&after),
            })
        }
    }
}

/// Local-only git status — no network IO. Used by `projects.show`; default
/// fields signal an uncloned / non-git directory.
pub async fn read_status(dest: &Path) -> Result<RepoStatus> {
    if !dest.join(".git").exists() {
        return Ok(RepoStatus::default());
    }
    let current_branch = run_git_capture(dest, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .ok();
    let head_commit = run_git_capture(dest, &["rev-parse", "HEAD"]).await.ok();
    let porcelain = run_git_capture(dest, &["status", "--porcelain"]).await?;
    Ok(RepoStatus {
        cloned: true,
        current_branch,
        head_commit,
        dirty: !porcelain.is_empty(),
    })
}

async fn head_sha(dest: &Path) -> Result<String> {
    run_git_capture(dest, &["rev-parse", "HEAD"]).await
}

async fn run_git_capture(cwd: &Path, args: &[&str]) -> Result<String> {
    run_git(Some(cwd), args, None)
        .await
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn short(sha: &str) -> String {
    sha.chars().take(8).collect()
}

/// Build `git clone` arguments. Blobless mode adds `--single-branch
/// --filter=blob:none` (fast clone of a large repo, full history kept for
/// incremental diffs); full mode is the previous behaviour, preserved for the
/// /cc base clones that get `git clone --local`d.
fn clone_args(branch: &str, blobless: bool, url: &str, dest: &str) -> Vec<String> {
    let mut args = vec![
        "clone".to_string(),
        "--branch".to_string(),
        branch.to_string(),
    ];
    if blobless {
        args.push("--single-branch".to_string());
        args.push("--filter=blob:none".to_string());
    }
    args.push(url.to_string());
    args.push(dest.to_string());
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use tokio::process::Command;

    /// Build a self-contained git source we can clone from. Uses a local
    /// `git init --bare` + an initial commit pushed via a working clone,
    /// so the tests don't touch the network.
    async fn local_git_source(tmp: &Path) -> PathBuf {
        let bare = tmp.join("origin.git");
        tokio::fs::create_dir(&bare).await.unwrap();
        Command::new("git")
            .args(["init", "--bare", "--initial-branch=main"])
            .current_dir(&bare)
            .output()
            .await
            .unwrap();

        let seed = tmp.join("seed");
        Command::new("git")
            .args(["clone", &bare.to_string_lossy(), &seed.to_string_lossy()])
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&seed)
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&seed)
            .output()
            .await
            .unwrap();
        tokio::fs::write(seed.join("README"), "hello")
            .await
            .unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&seed)
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&seed)
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["push", "origin", "main"])
            .current_dir(&seed)
            .output()
            .await
            .unwrap();

        bare
    }

    /// Test fetcher that targets a local bare repo, bypassing the github.com
    /// URL builder.
    async fn clone_local(source: &Path, dest: &Path, branch: &str) -> Result<()> {
        if let Some(p) = dest.parent() {
            tokio::fs::create_dir_all(p).await?;
        }
        run_git(
            None,
            &[
                "clone",
                "--branch",
                branch,
                &source.to_string_lossy(),
                &dest.to_string_lossy(),
            ],
            None,
        )
        .await
        .map(|_| ())
    }

    fn sample_source() -> RepoSource {
        RepoSource::Github {
            owner: "acme".into(),
            repo: "widgets".into(),
        }
    }

    #[tokio::test]
    async fn read_status_on_missing_dir_returns_uncloned() {
        let tmp = TempDir::new().unwrap();
        let s = read_status(&tmp.path().join("missing")).await.unwrap();
        assert!(!s.cloned);
    }

    #[tokio::test]
    async fn pull_reports_up_to_date_then_updated_after_remote_advances() {
        let tmp = TempDir::new().unwrap();
        let bare = local_git_source(tmp.path()).await;
        let dest = tmp.path().join("clones/rails");
        clone_local(&bare, &dest, "main").await.unwrap();

        let f = Fetcher::new(None);

        // Pull against unchanged origin: up-to-date.
        let outcome = f.pull_repo(&sample_source(), &dest, "main").await.unwrap();
        assert!(matches!(outcome, PullOutcome::UpToDate));

        // Push a new commit through a second seed clone.
        let seed = tmp.path().join("seed2");
        Command::new("git")
            .args(["clone", &bare.to_string_lossy(), &seed.to_string_lossy()])
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&seed)
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&seed)
            .output()
            .await
            .unwrap();
        tokio::fs::write(seed.join("README"), "hello v2")
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "-am", "v2"])
            .current_dir(&seed)
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["push", "origin", "main"])
            .current_dir(&seed)
            .output()
            .await
            .unwrap();

        let outcome = f.pull_repo(&sample_source(), &dest, "main").await.unwrap();
        assert!(matches!(outcome, PullOutcome::Updated { .. }));
    }

    #[tokio::test]
    async fn read_status_reports_branch_and_clean_state() {
        let tmp = TempDir::new().unwrap();
        let bare = local_git_source(tmp.path()).await;
        let dest = tmp.path().join("clones/rails");
        clone_local(&bare, &dest, "main").await.unwrap();

        let s = read_status(&dest).await.unwrap();
        assert!(s.cloned);
        assert_eq!(s.current_branch.as_deref(), Some("main"));
        assert!(!s.dirty);
        assert!(s.head_commit.is_some());
    }

    #[tokio::test]
    async fn read_status_flags_dirty_when_working_tree_modified() {
        let tmp = TempDir::new().unwrap();
        let bare = local_git_source(tmp.path()).await;
        let dest = tmp.path().join("clones/rails");
        clone_local(&bare, &dest, "main").await.unwrap();

        tokio::fs::write(dest.join("README"), "modified locally")
            .await
            .unwrap();
        let s = read_status(&dest).await.unwrap();
        assert!(s.dirty);
    }

    #[tokio::test]
    async fn install_credential_helper_is_single_valued_and_refreshes() {
        // Only touches `git config` — no network, no clone.
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("r");
        tokio::fs::create_dir(&repo).await.unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(&repo)
            .output()
            .await
            .unwrap();

        Fetcher::new(Some("ghp_STALE".into()))
            .install_credential_helper(&repo)
            .await
            .unwrap();
        Fetcher::new(Some("ghp_FRESH".into()))
            .install_credential_helper(&repo)
            .await
            .unwrap();

        let out = run_git(
            Some(&repo),
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
        assert!(stdout.contains("ghp_FRESH"), "fresh token must win");
        assert!(!stdout.contains("ghp_STALE"), "stale token must be gone");
    }

    #[test]
    fn clone_args_blobless_adds_partial_clone_flags() {
        let args = clone_args("main", true, "URL", "DEST");
        assert!(args.contains(&"--single-branch".to_string()));
        assert!(args.contains(&"--filter=blob:none".to_string()));
        assert_eq!(args.first().map(String::as_str), Some("clone"));
        assert_eq!(args.last().map(String::as_str), Some("DEST"));
    }

    #[test]
    fn clone_args_full_is_unchanged_and_has_no_partial_flags() {
        let args = clone_args("main", false, "URL", "DEST");
        assert_eq!(args, vec!["clone", "--branch", "main", "URL", "DEST"]);
    }
}
