//! A fresh git worktree for a session, under the repo's own `.worktrees/`.

use std::path::{Path, PathBuf};

use eyre::{Context, Result};
use tokio::process::Command;

async fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .context("running git")?;
    if !out.status.success() {
        eyre::bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn slug(branch: &str) -> String {
    let s: String = branch
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    s.split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Creates `<repo>/.worktrees/<slug>` on `branch` (new from HEAD, or the
/// existing branch) and returns its path.
pub async fn create(cwd: &Path, branch: &str) -> Result<PathBuf> {
    let name = slug(branch);
    if name.is_empty() {
        eyre::bail!("branch name {branch:?} has no usable characters");
    }
    git(Path::new("."), &["check-ref-format", "--branch", branch]).await?;
    let root = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"]).await?);
    let path = root.join(".worktrees").join(&name);
    if path.exists() {
        eyre::bail!("{} already exists", path.display());
    }
    let path_arg = path.to_string_lossy().into_owned();
    let exists = git(
        &root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .await
    .is_ok();
    if exists {
        git(&root, &["worktree", "add", &path_arg, branch]).await?;
    } else {
        git(&root, &["worktree", "add", "-b", branch, &path_arg]).await?;
    }
    Ok(path)
}

/// Whether `cwd` is in a git repository: only one can have a worktree.
pub async fn in_repo(cwd: &Path) -> bool {
    git(cwd, &["rev-parse", "--show-toplevel"]).await.is_ok()
}

/// The worktree [`create`] made for `branch` in `cwd`'s repo, when there
/// is one: what a start cut off after making it starts in.
pub async fn existing(cwd: &Path, branch: &str) -> Option<PathBuf> {
    let name = slug(branch);
    if name.is_empty() {
        return None;
    }
    let root = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"]).await.ok()?);
    let path = root.join(".worktrees").join(&name);
    path.is_dir().then_some(path)
}

/// The project a session's folder belongs to: the repo for one of Desk's
/// own worktrees (`<repo>/.worktrees/<slug>`, as [`create`] makes them),
/// else the folder itself.
pub fn project_of(cwd: &str) -> &str {
    match cwd.rfind("/.worktrees/") {
        Some(at)
            if at > 0
                && !cwd[at + "/.worktrees/".len()..]
                    .trim_matches('/')
                    .contains('/') =>
        {
            &cwd[..at]
        }
        _ => cwd,
    }
}

/// The project a folder belongs to, by git: the root of its repository,
/// a linked worktree's main repository (through its `.git` file's `gitdir`
/// and that folder's `commondir`); else [`project_of`]. Reads a few small
/// files, never runs git.
pub fn git_project(cwd: &str) -> String {
    let mut at = Some(std::path::Path::new(cwd));
    while let Some(dir) = at {
        let dot = dir.join(".git");
        if dot.is_dir() {
            return dir.to_string_lossy().into_owned();
        }
        if dot.is_file() {
            return main_repo(dir, &dot).unwrap_or_else(|| dir.to_string_lossy().into_owned());
        }
        at = dir.parent();
    }
    project_of(cwd).to_string()
}

/// A linked worktree's main repository, from its `.git` file.
fn main_repo(dir: &std::path::Path, dot: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(dot).ok()?;
    let gitdir = text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
    let gitdir = dir.join(gitdir);
    let common = std::fs::read_to_string(gitdir.join("commondir")).ok()?;
    let common = std::fs::canonicalize(gitdir.join(common.trim())).ok()?;
    let root = match common.file_name().is_some_and(|n| n == ".git") {
        true => common.parent()?.to_path_buf(),
        false => common,
    };
    Some(root.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_linked_worktree_is_its_main_repositorys_project() {
        let dir = tempfile::tempdir().unwrap();
        let main = std::fs::canonicalize(dir.path()).unwrap().join("atlas");
        std::fs::create_dir_all(main.join(".git/worktrees/fix-1")).unwrap();
        std::fs::create_dir_all(main.join("src/deep")).unwrap();
        std::fs::write(main.join(".git/worktrees/fix-1/commondir"), "../..\n").unwrap();
        let tree = dir.path().join("elsewhere/fix-1");
        std::fs::create_dir_all(tree.join("crates")).unwrap();
        std::fs::write(
            tree.join(".git"),
            format!("gitdir: {}\n", main.join(".git/worktrees/fix-1").display()),
        )
        .unwrap();
        let main_s = main.to_string_lossy().into_owned();
        assert_eq!(
            git_project(&main.join("src/deep").to_string_lossy()),
            main_s
        );
        assert_eq!(git_project(&tree.join("crates").to_string_lossy()), main_s);
        assert_eq!(
            git_project("/nowhere/at/all"),
            "/nowhere/at/all",
            "not in a repository"
        );
    }

    #[test]
    fn slugs_are_path_safe() {
        assert_eq!(slug("desk/Auth Fix"), "desk-auth-fix");
        assert_eq!(slug("--x--"), "x");
    }

    #[tokio::test]
    async fn creates_a_worktree_on_a_new_branch() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec![
                "-c",
                "user.email=d@e",
                "-c",
                "user.name=d",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        ] {
            git(repo, &args).await.unwrap();
        }
        let wt = create(repo, "desk/auth-fix").await.unwrap();
        assert!(wt.ends_with(".worktrees/desk-auth-fix"));
        assert_eq!(
            git(&wt, &["branch", "--show-current"]).await.unwrap(),
            "desk/auth-fix"
        );
        assert!(create(repo, "desk/auth-fix").await.is_err());
        assert!(create(repo, "bad..name").await.is_err());
    }

    #[test]
    fn a_desk_worktree_is_its_repos_project() {
        assert_eq!(
            super::project_of("/dev/acme-web/.worktrees/login-race"),
            "/dev/acme-web"
        );
        assert_eq!(
            super::project_of("/dev/acme-web/.worktrees/login-race/"),
            "/dev/acme-web"
        );
        assert_eq!(
            super::project_of("/dev/acme-web/.worktrees/x/crates"),
            "/dev/acme-web/.worktrees/x/crates"
        );
        assert_eq!(super::project_of("/dev/atlas"), "/dev/atlas");
    }
}
