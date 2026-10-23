//! `/cc-rebase`: bring every checkout in the workspace up to date with its
//! base branch, through the round's sandbox. A conflict is aborted so the
//! worktree is clean on return and the files that clashed are reported.

use std::path::Path;

use tracing::warn;

use super::sandbox_git::{auth_env, capture, git, repo_dirs};
use super::staleness;
use crate::coding_sandbox::CodingSandbox;

#[derive(Debug, Clone)]
pub struct RepoRebaseReport {
    pub repo: String,
    pub outcome: RebaseOutcome,
}

#[derive(Debug, Clone)]
pub enum RebaseOutcome {
    Skipped {
        reason: String,
    },
    AlreadyUpToDate,
    Advanced {
        from: String,
        to: String,
        base: String,
    },
    Conflict {
        files: Vec<String>,
        base: String,
    },
    Error {
        stage: &'static str,
        message: String,
    },
}

/// Rebase every checkout in the sandbox; `gh_token` lets the fetch reach a
/// private origin where the checkout carries no credential of its own.
pub async fn rebase_in(
    sandbox: &dyn CodingSandbox,
    gh_token: Option<&str>,
) -> Vec<RepoRebaseReport> {
    let env = auth_env(gh_token);
    let mut reports = Vec::new();
    for repo in repo_dirs(sandbox).await {
        reports.push(RepoRebaseReport {
            repo: repo.alias,
            outcome: rebase_one_repo(sandbox, &repo.path, &env).await,
        });
    }
    reports
}

async fn rebase_one_repo(
    sandbox: &dyn CodingSandbox,
    repo: &Path,
    env: &[(String, String)],
) -> RebaseOutcome {
    let error = |stage: &'static str| {
        move |e: eyre::Report| RebaseOutcome::Error {
            stage,
            message: e.to_string(),
        }
    };
    let current_branch =
        match capture(sandbox, repo, &["rev-parse", "--abbrev-ref", "HEAD"], env).await {
            Ok(b) => b,
            Err(e) => return error("rev-parse HEAD")(e),
        };
    let base = match staleness::detect_base_branch_in(sandbox, repo).await {
        Ok(Some(b)) => b,
        Ok(None) => {
            return RebaseOutcome::Skipped {
                reason: "no origin/main or origin/master to rebase onto".into(),
            }
        }
        Err(e) => return error("detect base branch")(e),
    };
    if current_branch == base {
        return RebaseOutcome::Skipped {
            reason: format!("already on the base branch `{base}`"),
        };
    }
    let before = match capture(sandbox, repo, &["rev-parse", "HEAD"], env).await {
        Ok(s) => s,
        Err(e) => return error("rev-parse HEAD")(e),
    };
    if let Err(e) = capture(sandbox, repo, &["fetch", "origin", &base], env).await {
        return error("git fetch")(e);
    }
    let base_ref = format!("origin/{base}");
    match git(sandbox, repo, &["rebase", &base_ref], env).await {
        Ok(out) if out.exit_code() == 0 => {
            let after = capture(sandbox, repo, &["rev-parse", "HEAD"], env)
                .await
                .unwrap_or_default();
            if after == before {
                RebaseOutcome::AlreadyUpToDate
            } else {
                RebaseOutcome::Advanced {
                    from: short(&before),
                    to: short(&after),
                    base,
                }
            }
        }
        Ok(_) => {
            let files = capture(
                sandbox,
                repo,
                &["diff", "--name-only", "--diff-filter=U"],
                env,
            )
            .await
            .map(|s| {
                s.lines()
                    .filter(|l| !l.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
            if let Err(e) = git(sandbox, repo, &["rebase", "--abort"], env).await {
                warn!(
                    repo = %repo.display(),
                    error = %e,
                    "cc-rebase: rebase --abort failed; worktree may be in a mid-rebase state"
                );
            }
            RebaseOutcome::Conflict { files, base }
        }
        Err(e) => error("git rebase")(e),
    }
}

fn short(sha: &str) -> String {
    sha.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::process::Command;

    use crate::coding_sandbox::adopt_workdir;

    fn sb(dir: &Path) -> Arc<dyn CodingSandbox> {
        adopt_workdir(dir.to_path_buf(), Vec::new(), None)
    }

    async fn rebase_one_repo(dir: &Path) -> RebaseOutcome {
        super::rebase_one_repo(sb(dir).as_ref(), Path::new("."), &[]).await
    }

    async fn capture(dir: &Path, args: &[&str]) -> Result<String, String> {
        super::capture(sb(dir).as_ref(), Path::new("."), args, &[])
            .await
            .map_err(|e| e.to_string())
    }

    /// Build a working clone whose `feature` branch can fast-forward
    /// onto `origin/main`. Mirrors the divergence helper from
    /// `staleness.rs` but doesn't introduce conflicts.
    async fn make_advance_repo() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let bare = tmp.path().join("origin.git");
        let work = tmp.path().join("work");
        let seed = tmp.path().join("seed");

        run_git(
            tmp.path(),
            &[
                "init",
                "--bare",
                "--initial-branch=main",
                &bare.to_string_lossy(),
            ],
        )
        .await;
        run_git(
            tmp.path(),
            &["clone", &bare.to_string_lossy(), &seed.to_string_lossy()],
        )
        .await;
        configure(&seed).await;
        tokio::fs::write(seed.join("README"), "init").await.unwrap();
        run_git(&seed, &["add", "."]).await;
        run_git(&seed, &["commit", "-m", "init"]).await;
        run_git(&seed, &["push", "origin", "main"]).await;

        run_git(
            tmp.path(),
            &["clone", &bare.to_string_lossy(), &work.to_string_lossy()],
        )
        .await;
        configure(&work).await;
        run_git(&work, &["checkout", "-b", "feature"]).await;
        // Feature commit that lives only on `feature`.
        tokio::fs::write(work.join("ours.txt"), "ours")
            .await
            .unwrap();
        run_git(&work, &["add", "ours.txt"]).await;
        run_git(&work, &["commit", "-m", "ours"]).await;

        // New commit on origin/main that doesn't touch ours.txt.
        run_git(&seed, &["checkout", "main"]).await;
        tokio::fs::write(seed.join("theirs.txt"), "theirs")
            .await
            .unwrap();
        run_git(&seed, &["add", "theirs.txt"]).await;
        run_git(&seed, &["commit", "-m", "theirs"]).await;
        run_git(&seed, &["push", "origin", "main"]).await;

        tmp
    }

    /// Same shape as `make_advance_repo` but both sides change the
    /// same file → rebase produces a real conflict.
    async fn make_conflict_repo() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let bare = tmp.path().join("origin.git");
        let work = tmp.path().join("work");
        let seed = tmp.path().join("seed");

        run_git(
            tmp.path(),
            &[
                "init",
                "--bare",
                "--initial-branch=main",
                &bare.to_string_lossy(),
            ],
        )
        .await;
        run_git(
            tmp.path(),
            &["clone", &bare.to_string_lossy(), &seed.to_string_lossy()],
        )
        .await;
        configure(&seed).await;
        tokio::fs::write(seed.join("shared.txt"), "base\n")
            .await
            .unwrap();
        run_git(&seed, &["add", "."]).await;
        run_git(&seed, &["commit", "-m", "init"]).await;
        run_git(&seed, &["push", "origin", "main"]).await;

        run_git(
            tmp.path(),
            &["clone", &bare.to_string_lossy(), &work.to_string_lossy()],
        )
        .await;
        configure(&work).await;
        run_git(&work, &["checkout", "-b", "feature"]).await;
        tokio::fs::write(work.join("shared.txt"), "ours\n")
            .await
            .unwrap();
        run_git(&work, &["add", "shared.txt"]).await;
        run_git(&work, &["commit", "-m", "ours"]).await;

        run_git(&seed, &["checkout", "main"]).await;
        tokio::fs::write(seed.join("shared.txt"), "theirs\n")
            .await
            .unwrap();
        run_git(&seed, &["add", "shared.txt"]).await;
        run_git(&seed, &["commit", "-m", "theirs"]).await;
        run_git(&seed, &["push", "origin", "main"]).await;

        tmp
    }

    async fn run_git(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} failed in {}: {}",
            args,
            cwd.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    async fn configure(repo: &Path) {
        run_git(repo, &["config", "user.email", "test@example.com"]).await;
        run_git(repo, &["config", "user.name", "Test"]).await;
    }

    #[tokio::test]
    async fn rebase_advances_when_no_conflicts() {
        let tmp = make_advance_repo().await;
        let work = tmp.path().join("work");
        let report = rebase_one_repo(&work).await;
        match report {
            RebaseOutcome::Advanced { base, .. } => assert_eq!(base, "main"),
            other => panic!("expected Advanced, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rebase_aborts_on_conflict_and_leaves_clean_worktree() {
        let tmp = make_conflict_repo().await;
        let work = tmp.path().join("work");

        let report = rebase_one_repo(&work).await;
        match &report {
            RebaseOutcome::Conflict { files, base } => {
                assert_eq!(base, "main");
                assert!(
                    files.iter().any(|f| f == "shared.txt"),
                    "expected shared.txt in conflict list, got {files:?}"
                );
            }
            other => panic!("expected Conflict, got {other:?}"),
        }

        // Worktree must be clean (no merge in progress, no conflicts
        // staged) — the abort branch is the whole reason this helper
        // exists and not a bare `git rebase`.
        let porcelain = capture(&work, &["status", "--porcelain"]).await.unwrap();
        assert!(
            porcelain.is_empty(),
            "worktree must be clean post-abort; got: {porcelain:?}"
        );
        let head_branch = capture(&work, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .unwrap();
        assert_eq!(head_branch, "feature");
    }

    #[tokio::test]
    async fn rebase_skips_when_already_on_base_branch() {
        let tmp = make_advance_repo().await;
        let work = tmp.path().join("work");
        run_git(&work, &["checkout", "main"]).await;
        let report = rebase_one_repo(&work).await;
        assert!(
            matches!(report, RebaseOutcome::Skipped { reason } if reason.contains("base branch")),
            "expected Skipped(base branch), got non-matching outcome"
        );
    }

    #[tokio::test]
    async fn rebase_workdir_iterates_per_repo_subdirs() {
        let tmp_a = make_advance_repo().await;
        let tmp_b = make_advance_repo().await;
        let session = TempDir::new().unwrap();
        let session_root = session.path();

        // Move each `work` clone under session_root as a subdir so the
        // session-shaped path is exercised.
        tokio::fs::rename(tmp_a.path().join("work"), session_root.join("alpha"))
            .await
            .unwrap();
        tokio::fs::rename(tmp_b.path().join("work"), session_root.join("beta"))
            .await
            .unwrap();

        let reports = rebase_in(sb(session_root).as_ref(), None).await;
        assert_eq!(reports.len(), 2);
        let names: Vec<_> = reports.iter().map(|r| r.repo.clone()).collect();
        assert!(names.contains(&"alpha".to_string()));
        assert!(names.contains(&"beta".to_string()));
        for r in &reports {
            assert!(matches!(r.outcome, RebaseOutcome::Advanced { .. }));
        }
    }
}
