//! How far a checkout's branch has drifted from its base: commits behind and
//! ahead of `origin/<base>`, and the files both sides touched, which is what
//! turns a rebase into a conflict. Read through the round's sandbox with
//! local git only; nothing here fetches.

use std::path::Path;

use eyre::Result;

use super::sandbox_git::capture;
use crate::coding_sandbox::CodingSandbox;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StalenessReport {
    pub current_branch: String,
    pub base_branch: String,
    pub commits_behind: u32,
    pub commits_ahead: u32,
    pub overlapping_files: Vec<String>,
}

pub const COMMIT_BEHIND_WARN_THRESHOLD: u32 = 50;

impl StalenessReport {
    pub fn warrants_warning(&self) -> bool {
        !self.overlapping_files.is_empty() || self.commits_behind >= COMMIT_BEHIND_WARN_THRESHOLD
    }
}

pub async fn analyse_in(sandbox: &dyn CodingSandbox, repo: &Path) -> Result<StalenessReport> {
    let git = |args: &'static [&'static str]| capture(sandbox, repo, args, &[]);
    let current_branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]).await?;
    let Some(base_branch) = detect_base_branch_in(sandbox, repo).await? else {
        return Ok(StalenessReport {
            current_branch,
            ..Default::default()
        });
    };
    if current_branch == base_branch {
        return Ok(StalenessReport {
            current_branch,
            base_branch,
            ..Default::default()
        });
    }
    let base_ref = format!("origin/{base_branch}");
    let commits_behind = count_revs(sandbox, repo, &format!("HEAD..{base_ref}")).await;
    let commits_ahead = count_revs(sandbox, repo, &format!("{base_ref}..HEAD")).await;
    let overlapping_files = if commits_behind > 0 && commits_ahead > 0 {
        overlap(sandbox, repo, &base_ref).await.unwrap_or_default()
    } else {
        Vec::new()
    };
    Ok(StalenessReport {
        current_branch,
        base_branch,
        commits_behind,
        commits_ahead,
        overlapping_files,
    })
}

/// The base branch: what `origin/HEAD` points at, else `main` or `master`
/// when the remote has one.
pub async fn detect_base_branch_in(
    sandbox: &dyn CodingSandbox,
    repo: &Path,
) -> Result<Option<String>> {
    if let Ok(s) = capture(
        sandbox,
        repo,
        &["symbolic-ref", "refs/remotes/origin/HEAD"],
        &[],
    )
    .await
    {
        if let Some(name) = s.rsplit('/').next().filter(|s| !s.is_empty()) {
            return Ok(Some(name.to_string()));
        }
    }
    for name in ["main", "master"] {
        let verified = capture(
            sandbox,
            repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("origin/{name}"),
            ],
            &[],
        )
        .await;
        if verified.is_ok() {
            return Ok(Some(name.to_string()));
        }
    }
    Ok(None)
}

async fn count_revs(sandbox: &dyn CodingSandbox, repo: &Path, range: &str) -> u32 {
    capture(sandbox, repo, &["rev-list", "--count", range], &[])
        .await
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

async fn overlap(sandbox: &dyn CodingSandbox, repo: &Path, base_ref: &str) -> Result<Vec<String>> {
    let ours = capture(
        sandbox,
        repo,
        &["diff", "--name-only", &format!("{base_ref}...HEAD")],
        &[],
    )
    .await?;
    let theirs = capture(
        sandbox,
        repo,
        &["diff", "--name-only", &format!("HEAD...{base_ref}")],
        &[],
    )
    .await?;
    let our_set: std::collections::HashSet<&str> = ours.lines().collect();
    let mut files: Vec<String> = theirs
        .lines()
        .filter(|f| our_set.contains(f) && !f.is_empty())
        .map(String::from)
        .collect();
    files.sort();
    files.dedup();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::process::Command;

    use crate::coding_sandbox::adopt_workdir;

    async fn analyse(dir: &Path) -> Result<StalenessReport> {
        analyse_in(
            adopt_workdir(dir.to_path_buf(), Vec::new(), None).as_ref(),
            Path::new("."),
        )
        .await
    }

    /// Build a self-contained "origin" bare repo + a working clone with a
    /// configurable amount of divergence. Returns the path to the working
    /// clone. Both sides commit through real `git` so the analyser is
    /// exercised end-to-end.
    async fn make_diverged_repo(
        ahead_files: &[(&str, &str)],
        behind_files: &[(&str, &str)],
    ) -> TempDir {
        let tmp = TempDir::new().unwrap();
        let bare = tmp.path().join("origin.git");
        let work = tmp.path().join("work");

        // `git init --bare <path>` creates the bare in `<path>` regardless
        // of cwd, so we don't have to pre-create the dir or chdir into it.
        run(
            tmp.path(),
            &[
                "init",
                "--bare",
                "--initial-branch=main",
                &bare.to_string_lossy(),
            ],
        )
        .await;
        // Seed the bare via a throwaway clone so we can push an initial commit.
        let seed = tmp.path().join("seed");
        run(
            tmp.path(),
            &["clone", &bare.to_string_lossy(), &seed.to_string_lossy()],
        )
        .await;
        configure(&seed).await;
        tokio::fs::write(seed.join("README"), "init").await.unwrap();
        run(&seed, &["add", "."]).await;
        run(&seed, &["commit", "-m", "init"]).await;
        run(&seed, &["push", "origin", "main"]).await;

        // Working clone — start a feature branch off `main`, diverge it.
        run(
            tmp.path(),
            &["clone", &bare.to_string_lossy(), &work.to_string_lossy()],
        )
        .await;
        configure(&work).await;
        run(&work, &["checkout", "-b", "feature"]).await;
        for (path, contents) in ahead_files {
            tokio::fs::write(work.join(path), contents).await.unwrap();
            run(&work, &["add", path]).await;
            run(&work, &["commit", "-m", &format!("feature: {path}")]).await;
        }

        // Now produce some commits on origin/main behind the feature branch.
        // Reuse the seed clone so we don't taint `work`'s view.
        run(&seed, &["checkout", "main"]).await;
        for (path, contents) in behind_files {
            tokio::fs::write(seed.join(path), contents).await.unwrap();
            run(&seed, &["add", path]).await;
            run(&seed, &["commit", "-m", &format!("main: {path}")]).await;
        }
        run(&seed, &["push", "origin", "main"]).await;
        // Pull origin/main into the working clone's remote-tracking ref so
        // the analyser sees the divergence without needing network IO.
        run(&work, &["fetch", "origin", "main"]).await;

        tmp
    }

    async fn run(cwd: &Path, args: &[&str]) {
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
        run(repo, &["config", "user.email", "test@example.com"]).await;
        run(repo, &["config", "user.name", "Test"]).await;
    }

    #[tokio::test]
    async fn no_divergence_on_base_branch_itself() {
        let tmp = make_diverged_repo(&[], &[]).await;
        let work = tmp.path().join("work");
        // Switch back to main so current==base.
        run(&work, &["checkout", "main"]).await;
        let report = analyse(&work).await.unwrap();
        assert_eq!(report.current_branch, "main");
        assert_eq!(report.commits_behind, 0);
        assert_eq!(report.commits_ahead, 0);
        assert!(report.overlapping_files.is_empty());
        assert!(!report.warrants_warning());
    }

    #[tokio::test]
    async fn detects_ahead_and_behind_counts() {
        let tmp = make_diverged_repo(
            &[("a.txt", "ours-1"), ("b.txt", "ours-2")],
            &[
                ("c.txt", "theirs-1"),
                ("d.txt", "theirs-2"),
                ("e.txt", "theirs-3"),
            ],
        )
        .await;
        let work = tmp.path().join("work");
        let report = analyse(&work).await.unwrap();
        assert_eq!(report.current_branch, "feature");
        assert_eq!(report.base_branch, "main");
        assert_eq!(report.commits_ahead, 2);
        assert_eq!(report.commits_behind, 3);
        assert!(report.overlapping_files.is_empty());
        // Below threshold + no overlap → no warning.
        assert!(!report.warrants_warning());
    }

    #[tokio::test]
    async fn flags_overlapping_files_as_conflict_risk() {
        let tmp = make_diverged_repo(
            &[("shared.txt", "our version"), ("ours-only.txt", "x")],
            &[("shared.txt", "their version"), ("theirs-only.txt", "y")],
        )
        .await;
        let work = tmp.path().join("work");
        let report = analyse(&work).await.unwrap();
        assert_eq!(report.overlapping_files, vec!["shared.txt".to_string()]);
        assert!(report.warrants_warning());
    }

    #[tokio::test]
    async fn warrants_warning_when_far_behind_even_without_overlap() {
        let report = StalenessReport {
            commits_behind: 60,
            ..Default::default()
        };
        assert!(report.warrants_warning());
    }

    #[tokio::test]
    async fn missing_origin_returns_empty_report_not_error() {
        let tmp = TempDir::new().unwrap();
        let work = tmp.path().join("work");
        tokio::fs::create_dir_all(&work).await.unwrap();
        run(&work, &["init", "--initial-branch=main"]).await;
        configure(&work).await;
        tokio::fs::write(work.join("README"), "x").await.unwrap();
        run(&work, &["add", "."]).await;
        run(&work, &["commit", "-m", "init"]).await;
        let report = analyse(&work).await.unwrap();
        assert_eq!(report.current_branch, "main");
        assert!(report.base_branch.is_empty());
    }
}
