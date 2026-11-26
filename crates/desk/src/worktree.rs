//! A fresh git worktree for a session, under the repo's own `.worktrees/`.

use std::path::{Path, PathBuf};

use eyre::{Context, Result};
use tokio::process::Command;

async fn git(dir: &Path, args: &[&str]) -> Result<String> {
    git_with(dir, args, &[]).await
}

/// [`git`], with `env` set for it.
async fn git_with(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .envs(env.iter().copied())
        .kill_on_drop(true)
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
    create_from(cwd, branch, None).await
}

/// Creates `<repo>/.worktrees/<slug>` on a new `branch` from `start` (a
/// commit), as a handoff to a new worktree does: from the source's HEAD,
/// whatever the main checkout's is. Refused when `branch` exists.
pub async fn create_at(cwd: &Path, branch: &str, start: &str) -> Result<PathBuf> {
    create_from(cwd, branch, Some(start)).await
}

/// Where a folder's repository stands: its branch, its HEAD, and what is
/// uncommitted (`git status --porcelain`). Nothing when it isn't one.
pub async fn state_of(cwd: &Path) -> (Option<String>, Option<String>, Vec<String>) {
    let branch = git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).await.ok();
    let head = git(cwd, &["rev-parse", "--short=12", "HEAD"]).await.ok();
    let dirty = match head {
        Some(_) => git(cwd, &["status", "--porcelain"])
            .await
            .map(|s| s.lines().map(str::to_string).collect())
            .unwrap_or_default(),
        None => Vec::new(),
    };
    (branch.filter(|b| b != "HEAD"), head, dirty)
}

async fn create_from(cwd: &Path, branch: &str, start: Option<&str>) -> Result<PathBuf> {
    let name = slug(branch);
    if name.is_empty() {
        eyre::bail!("branch name {branch:?} has no usable characters");
    }
    git(cwd, &["check-ref-format", "--branch", branch]).await?;
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
    match (exists, start) {
        (true, Some(_)) => eyre::bail!("a branch called {branch} already exists"),
        (true, None) => {
            git(&root, &["worktree", "add", &path_arg, branch]).await?;
        }
        (false, None) => {
            git(&root, &["worktree", "add", "-b", branch, &path_arg]).await?;
        }
        (false, Some(start)) => {
            git(&root, &["worktree", "add", "-b", branch, &path_arg, start]).await?;
        }
    }
    Ok(path)
}

/// Creates `<repo>/.worktrees/<slug>` for `name`, detached at `commit`, in
/// the main checkout of `cwd`'s repository, never inside a linked
/// worktree: what a review reads its pull request's head in. A `commit`
/// the repository hasn't got is fetched from `origin`'s `from` into
/// `refs/remotes/origin/`, so it is seen as pushed; never waiting on a
/// password or a host key, and for at most [`FETCH_WITHIN`]. One already
/// there, detached at `commit`, is found again; anything else there is
/// refused. `true` beside its path: this call made it.
pub async fn detached(
    cwd: &Path,
    name: &str,
    commit: &str,
    from: &str,
) -> Result<(PathBuf, bool)> {
    let slugged = slug(name);
    if slugged.is_empty() {
        eyre::bail!("worktree name {name:?} has no usable characters");
    }
    let root = main_checkout(cwd).await?;
    let path = root.join(".worktrees").join(&slugged);
    if path.exists() {
        let at = git(&path, &["rev-parse", "--show-toplevel", "HEAD"]).await?;
        let here = std::fs::canonicalize(&path).context("reading the worktree")?;
        let on = git(&path, &["branch", "--show-current"]).await?;
        return match at.lines().collect::<Vec<_>>()[..] {
            [top, head] if Path::new(top) == here && head == commit && on.is_empty() => {
                Ok((path, false))
            }
            _ => eyre::bail!(
                "{} already exists, not detached at {commit}",
                path.display()
            ),
        };
    }
    if git(&root, &["cat-file", "-e", &format!("{commit}^{{commit}}")])
        .await
        .is_err()
    {
        let into = format!(
            "+{from}:refs/remotes/origin/{}",
            from.trim_start_matches("refs/")
        );
        let own_ssh = git(&root, &["config", "core.sshCommand"]).await.is_ok()
            || std::env::var_os("GIT_SSH_COMMAND").is_some();
        let unasked: &[(&str, &str)] = match own_ssh {
            true => &[("GIT_TERMINAL_PROMPT", "0")],
            false => &[
                ("GIT_TERMINAL_PROMPT", "0"),
                ("GIT_SSH_COMMAND", "ssh -o BatchMode=yes"),
            ],
        };
        let args = [
            "-c",
            "credential.interactive=never",
            "fetch",
            "-q",
            "origin",
            &into,
        ];
        let fetch = git_with(&root, &args, unasked);
        tokio::time::timeout(FETCH_WITHIN, fetch)
            .await
            .map_err(|_| eyre::eyre!("fetching it took over {}s", FETCH_WITHIN.as_secs()))
            .and_then(|r| r)
            .with_context(|| format!("{commit} isn't here and couldn't be fetched"))?;
    }
    git(
        &root,
        &[
            "worktree",
            "add",
            "--detach",
            &path.to_string_lossy(),
            commit,
        ],
    )
    .await?;
    Ok((path, true))
}

/// How long [`detached`] waits on fetching a head it hasn't got.
pub const FETCH_WITHIN: std::time::Duration = std::time::Duration::from_secs(60);

/// Removes the worktree [`detached`] made at `path` for `commit`: refused,
/// and kept, while it holds changes of its own or is no longer at it.
pub async fn remove(path: &Path, commit: &str) -> Result<()> {
    let head = git(path, &["rev-parse", "HEAD"]).await?;
    if head != commit {
        eyre::bail!("{} moved on from {commit} to {head}", path.display());
    }
    let root = main_checkout(path).await?;
    git(&root, &["worktree", "remove", &path.to_string_lossy()])
        .await
        .map(drop)
}

/// The main checkout of `cwd`'s repository, from a linked worktree too.
async fn main_checkout(cwd: &Path) -> Result<PathBuf> {
    let common =
        std::fs::canonicalize(cwd.join(git(cwd, &["rev-parse", "--git-common-dir"]).await?))
            .context("finding the repository")?;
    match (common.file_name(), common.parent()) {
        (Some(n), Some(parent)) if n == ".git" => Ok(parent.to_path_buf()),
        _ => Ok(PathBuf::from(
            git(cwd, &["rev-parse", "--show-toplevel"]).await?,
        )),
    }
}

/// The most names [`create_fresh`] tries.
pub const FRESH_TRIES: usize = 50;

/// Creates a separate copy of `cwd`'s repository on a new branch named from
/// `base`: the first of `base`, `base-2`, `base-3`, … that is neither a
/// branch nor a copy yet, from HEAD. Returns its path and branch. A folder
/// outside any repository says so plainly.
pub async fn create_fresh(cwd: &Path, base: &str) -> Result<(PathBuf, String)> {
    if !in_repo(cwd).await {
        eyre::bail!(
            "{} isn't in a git repository, so Desk can't make a separate copy of it",
            cwd.display()
        );
    }
    let base = base.trim().trim_end_matches('/');
    if slug(base).is_empty() {
        eyre::bail!("branch name {base:?} has no usable characters");
    }
    let root = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"]).await?);
    for n in 1..=FRESH_TRIES {
        let branch = match n {
            1 => base.to_string(),
            n => format!("{base}-{n}"),
        };
        let taken = root.join(".worktrees").join(slug(&branch)).exists()
            || git(
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
        if taken {
            continue;
        }
        let path = create_from(cwd, &branch, Some("HEAD")).await?;
        return Ok((path, branch));
    }
    eyre::bail!("every name from {base} to {base}-{FRESH_TRIES} is taken")
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

    /// A review's worktree is detached at the commit it reads, in the
    /// main checkout even from a linked worktree, found again there at that
    /// commit, and refused at another.
    #[tokio::test]
    async fn a_detached_worktree_is_made_at_its_commit_in_the_main_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let repo = std::fs::canonicalize(dir.path()).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]).await.unwrap();
        for m in ["one", "two"] {
            git(
                &repo,
                &[
                    "-c",
                    "user.email=d@e",
                    "-c",
                    "user.name=d",
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    m,
                ],
            )
            .await
            .unwrap();
        }
        let first = git(&repo, &["rev-parse", "HEAD~1"]).await.unwrap();
        let second = git(&repo, &["rev-parse", "HEAD"]).await.unwrap();
        let ticket = create(&repo, "des-2").await.unwrap();

        let (made, fresh) = detached(&ticket, "review 7 abc", &first, PULL)
            .await
            .unwrap();
        assert!(fresh, "made by this call");
        assert_eq!(made, repo.join(".worktrees/review-7-abc"));
        assert_eq!(git(&made, &["rev-parse", "HEAD"]).await.unwrap(), first);
        assert_eq!(
            git(&made, &["branch", "--show-current"]).await.unwrap(),
            "",
            "detached"
        );
        assert_eq!(
            detached(&repo, "review 7 abc", &first, PULL).await.unwrap(),
            (made.clone(), false),
            "found again, not made"
        );
        assert!(detached(&repo, "review 7 abc", &second, PULL)
            .await
            .is_err());
        assert!(
            detached(&repo, "des 2", &second, PULL).await.is_err(),
            "a ticket's own worktree, on its branch at that commit"
        );
        std::fs::create_dir_all(repo.join(".worktrees/plain")).unwrap();
        assert!(
            detached(&repo, "plain", &second, PULL).await.is_err(),
            "a folder that isn't its worktree"
        );
        assert!(detached(&repo, "review 8", &"0".repeat(40), PULL)
            .await
            .is_err());

        std::fs::write(made.join("notes.md"), "x").unwrap();
        assert!(remove(&made, &first).await.is_err(), "it holds changes");
        std::fs::remove_file(made.join("notes.md")).unwrap();
        git(&made, &["checkout", "-q", "--detach", &second])
            .await
            .unwrap();
        assert!(
            remove(&made, &first).await.is_err(),
            "no longer at the head it was made for"
        );
        assert!(made.exists());
        remove(&made, &second).await.unwrap();
        assert!(!made.exists());
    }

    const PULL: &str = "refs/pull/7/head";

    /// A head pushed from elsewhere is fetched from `origin` before its
    /// worktree is made, and kept under `refs/remotes/origin/`.
    #[tokio::test]
    async fn a_head_the_repository_hasnt_got_is_fetched_from_origin() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (origin, repo, other) = (
            base.join("origin.git"),
            base.join("web"),
            base.join("other"),
        );
        git(&base, &["init", "-q", "--bare", &origin.to_string_lossy()])
            .await
            .unwrap();
        let commit = |at: PathBuf, m: &'static str| async move {
            git(
                &at,
                &[
                    "-c",
                    "user.email=d@e",
                    "-c",
                    "user.name=d",
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    m,
                ],
            )
            .await
            .unwrap();
        };
        for at in [&repo, &other] {
            git(
                &base,
                &[
                    "clone",
                    "-q",
                    &origin.to_string_lossy(),
                    &at.to_string_lossy(),
                ],
            )
            .await
            .unwrap();
        }
        commit(repo.clone(), "one").await;
        git(&repo, &["push", "-q", "origin", "HEAD:main"])
            .await
            .unwrap();
        git(&other, &["pull", "-q", "origin", "main"])
            .await
            .unwrap();
        commit(other.clone(), "from elsewhere").await;
        git(&other, &["push", "-q", "origin", &format!("HEAD:{PULL}")])
            .await
            .unwrap();
        let head = git(&other, &["rev-parse", "HEAD"]).await.unwrap();

        let (made, _) = detached(&repo, "review 7", &head, PULL).await.unwrap();
        assert_eq!(git(&made, &["rev-parse", "HEAD"]).await.unwrap(), head);
        assert_eq!(
            git(&repo, &["rev-parse", "refs/remotes/origin/pull/7/head"])
                .await
                .unwrap(),
            head,
            "seen as pushed"
        );
        assert!(
            detached(&repo, "review 8", &"1".repeat(40), "refs/pull/8/head")
                .await
                .is_err(),
            "a head origin hasn't got either"
        );
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

    /// A separate copy is a new branch every time: a name taken by a branch
    /// or a copy goes on to the next, and a folder outside any repository
    /// says so.
    #[tokio::test]
    async fn a_fresh_copy_takes_the_first_name_no_branch_or_copy_has() {
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
            // Someone's own branch of that name, with no copy.
            vec!["branch", "desk/fix-login"],
        ] {
            git(repo, &args).await.unwrap();
        }
        let (first, branch) = create_fresh(repo, "desk/fix-login").await.unwrap();
        assert_eq!(branch, "desk/fix-login-2");
        assert!(first.ends_with(".worktrees/desk-fix-login-2"));
        let (_, again) = create_fresh(repo, "desk/fix-login").await.unwrap();
        assert_eq!(again, "desk/fix-login-3");
        let elsewhere = tempfile::tempdir().unwrap();
        let refused = create_fresh(elsewhere.path(), "desk/x").await.unwrap_err();
        assert!(
            refused.to_string().contains("isn't in a git repository"),
            "{refused}"
        );
    }
}
