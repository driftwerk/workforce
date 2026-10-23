//! Git inside a coding sandbox, for the helpers that run around a round
//! (quality gate, rebase, staleness, stash): the same hardened invocation the
//! hub uses on its own checkouts, dispatched wherever the round's workspace
//! is, so a helper never assumes the files are on this host.

use std::path::{Path, PathBuf};

use eyre::{eyre, Result};
use tracing::warn;

use crate::claude_code::remote_git_credential_env;
use crate::coding_sandbox::{CodingSandbox, EntryKind, ExecOutcome, SandboxCommand};
use crate::git_hardening::{hardened_git_args, HARDENED_GIT_ENV};

/// One repository checkout in a workspace, by the alias its reports carry and
/// its path relative to the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoDir {
    pub alias: String,
    pub path: PathBuf,
}

async fn has_git(sandbox: &dyn CodingSandbox, dir: &Path) -> bool {
    sandbox
        .list(dir)
        .await
        .map(|entries| entries.iter().any(|e| e.name == ".git"))
        .unwrap_or(false)
}

/// The checkouts in the workspace: the workspace itself when it is one, else
/// each immediate subdirectory that is one, in name order.
pub async fn repo_dirs(sandbox: &dyn CodingSandbox) -> Vec<RepoDir> {
    let root = Path::new(".");
    if has_git(sandbox, root).await {
        return vec![RepoDir {
            alias: super::repo_alias_from_path(sandbox.workdir()),
            path: root.to_path_buf(),
        }];
    }
    let entries = match sandbox.list(root).await {
        Ok(entries) => entries,
        Err(e) => {
            warn!(workdir = %sandbox.workdir().display(), error = %e, "workspace could not be listed");
            return Vec::new();
        }
    };
    let mut repos = Vec::new();
    for entry in entries.into_iter().filter(|e| e.kind == EntryKind::Dir) {
        let path = PathBuf::from(&entry.name);
        if has_git(sandbox, &path).await {
            repos.push(RepoDir {
                alias: entry.name,
                path,
            });
        }
    }
    repos.sort_by(|a, b| a.alias.cmp(&b.alias));
    repos
}

/// The environment that lets git in the workspace reach GitHub with the
/// round's token, or nothing when there is none.
pub fn auth_env(gh_token: Option<&str>) -> Vec<(String, String)> {
    gh_token
        .map(|token| {
            std::iter::once(("GH_TOKEN".to_string(), token.to_string()))
                .chain(
                    remote_git_credential_env()
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v.to_string())),
                )
                .collect()
        })
        .unwrap_or_default()
}

/// Run a hardened `git <args>` in `repo`.
pub async fn git(
    sandbox: &dyn CodingSandbox,
    repo: &Path,
    args: &[&str],
    env: &[(String, String)],
) -> Result<ExecOutcome> {
    let mut command = SandboxCommand::new(
        "git",
        hardened_git_args()
            .into_iter()
            .chain(args.iter().map(|a| a.to_string())),
    )
    .cwd(repo);
    command.env = HARDENED_GIT_ENV
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .chain(env.iter().cloned())
        .collect();
    sandbox.exec(command).await
}

/// `git <args>`'s trimmed stdout, or its stderr as the error.
pub async fn capture(
    sandbox: &dyn CodingSandbox,
    repo: &Path,
    args: &[&str],
    env: &[(String, String)],
) -> Result<String> {
    let out = git(sandbox, repo, args, env).await?;
    if out.exit_code() != 0 {
        return Err(eyre!(
            "git {} failed: {}",
            args.join(" "),
            out.stderr.trim()
        ));
    }
    Ok(out.stdout.trim().to_string())
}

/// Where one checkout stood before a round: its HEAD, `None` before its
/// first commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    pub repo: RepoDir,
    pub head: Option<String>,
}

/// Where every checkout in the workspace stands now.
pub async fn revisions(sandbox: &dyn CodingSandbox) -> Vec<Revision> {
    let mut out = Vec::new();
    for repo in repo_dirs(sandbox).await {
        let head = capture(
            sandbox,
            &repo.path,
            &["rev-parse", "--verify", "-q", "HEAD"],
            &[],
        )
        .await
        .ok()
        .filter(|head| !head.is_empty());
        out.push(Revision { repo, head });
    }
    out
}

/// The files the workspace's checkouts changed since `before`, relative to
/// the workspace: tracked changes against the recorded HEAD, committed or
/// not and deletions included, and untracked files git does not ignore. A
/// checkout with no commit before has everything it tracks now as new; one
/// `before` does not know (cloned meanwhile) counts only its uncommitted
/// changes.
pub async fn changed_since(sandbox: &dyn CodingSandbox, before: &[Revision]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for repo in repo_dirs(sandbox).await {
        let known = before
            .iter()
            .find(|r| r.repo == repo)
            .map(|r| r.head.as_deref());
        let tracked: &[&str] = match known {
            Some(Some(head)) => &["diff", "--name-only", "--no-renames", "-z", head, "--"],
            Some(None) => &["ls-files", "-z", "--cached"],
            None => &["diff", "--name-only", "--no-renames", "-z", "HEAD", "--"],
        };
        let untracked: &[&str] = &["ls-files", "-z", "--others", "--exclude-standard"];
        for args in [tracked, untracked] {
            match git(sandbox, &repo.path, args, &[]).await {
                Ok(listed) if listed.exit_code() == 0 => out.extend(
                    listed
                        .stdout
                        .split('\0')
                        .filter(|name| !name.is_empty())
                        .map(|name| repo.path.join(name)),
                ),
                Ok(listed) => {
                    warn!(repo = %repo.alias, args = %args.join(" "), stderr = %listed.stderr.trim(), "changed files could not be listed")
                }
                Err(e) => {
                    warn!(repo = %repo.alias, args = %args.join(" "), error = %e, "changed files could not be listed")
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coding_sandbox::adopt_workdir;

    async fn init(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let out = tokio::process::Command::new("git")
            .args(["init", "-q", "--initial-branch=main"])
            .current_dir(dir)
            .output()
            .await
            .unwrap();
        assert!(out.status.success());
    }

    #[tokio::test]
    async fn a_workspace_is_one_repo_or_a_row_of_them() {
        let dir = tempfile::tempdir().unwrap();
        let single = dir.path().join("solo");
        init(&single).await;
        let repos = repo_dirs(adopt_workdir(single, Vec::new(), None).as_ref()).await;
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].alias, "solo");
        assert_eq!(repos[0].path, Path::new("."));

        let multi = dir.path().join("session");
        init(&multi.join("beta")).await;
        init(&multi.join("alpha")).await;
        std::fs::create_dir_all(multi.join("notes")).unwrap();
        let repos = repo_dirs(adopt_workdir(multi, Vec::new(), None).as_ref()).await;
        let aliases: Vec<&str> = repos.iter().map(|r| r.alias.as_str()).collect();
        assert_eq!(aliases, ["alpha", "beta"]);
        assert_eq!(repos[0].path, Path::new("alpha"));
    }

    #[tokio::test]
    async fn git_runs_hardened_in_the_repo_and_capture_reports_failures() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path()).await;
        let sandbox = adopt_workdir(dir.path().to_path_buf(), Vec::new(), None);
        let hooks = capture(
            sandbox.as_ref(),
            Path::new("."),
            &["config", "core.hooksPath"],
            &[],
        )
        .await
        .unwrap();
        assert_eq!(hooks, "/dev/null", "the hardening reaches the command");
        let err = capture(
            sandbox.as_ref(),
            Path::new("."),
            &["rev-parse", "nope"],
            &[],
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.starts_with("git rev-parse nope failed"), "{err}");
    }

    fn sh(dir: &Path, script: &str) {
        let out = std::process::Command::new("sh")
            .args(["-c", script])
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(out.status.success(), "{script}: {out:?}");
    }

    #[tokio::test]
    async fn a_round_changes_what_it_committed_edited_deleted_and_created_since_its_start() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("session");
        let alpha = workspace.join("alpha");
        init(&alpha).await;
        sh(
            &alpha,
            "printf 'ignored.txt\\n' > .gitignore && touch a.rs b.rs c.rs keep.rs \
             && git add -A && git commit -qm base",
        );
        let unborn = workspace.join("unborn");
        init(&unborn).await;
        let sandbox = adopt_workdir(workspace.clone(), Vec::new(), None);
        let before = revisions(sandbox.as_ref()).await;
        assert!(before[0].head.is_some());
        assert_eq!(before[1].head, None);

        sh(
            &alpha,
            "echo x > a.rs && echo y > b.rs && git commit -qam 'b' && echo x >> b.rs \
             && git rm -q c.rs && git mv keep.rs kept.rs && touch new.rs ignored.txt",
        );
        sh(&unborn, "touch first.rs && git add first.rs");
        let cloned = workspace.join("cloned");
        init(&cloned).await;
        sh(
            &cloned,
            "touch old.rs && git add -A && git commit -qm base && echo x > old.rs",
        );

        let changed = changed_since(sandbox.as_ref(), &before).await;
        let changed: Vec<String> = changed.iter().map(|p| p.display().to_string()).collect();
        assert_eq!(
            changed,
            [
                "alpha/a.rs",
                "alpha/b.rs",
                "alpha/c.rs",
                "alpha/keep.rs",
                "alpha/kept.rs",
                "alpha/new.rs",
                "cloned/old.rs",
                "unborn/first.rs",
            ]
        );
    }

    #[test]
    fn auth_env_carries_the_token_and_a_helper_that_reads_it() {
        assert!(auth_env(None).is_empty());
        let env = auth_env(Some("ghp_x"));
        assert!(env.contains(&("GH_TOKEN".to_string(), "ghp_x".to_string())));
        assert!(env
            .iter()
            .any(|(k, v)| k == "GIT_CONFIG_KEY_0" && v == "credential.helper"));
    }
}
