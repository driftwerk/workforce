//! A fresh git worktree for a session, under the repo's own `.worktrees/`.

use std::path::{Path, PathBuf};

use eyre::{Context, Result};
use tokio::process::Command;

async fn git(dir: &Path, args: &[&str]) -> Result<String> {
    // git's own words, untranslated: [`unmade`] reads them.
    let out = Command::new("git")
        .env("LC_ALL", "C")
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

/// What answers a start whose worktree belongs to something else.
pub const TAKEN: &str = "worktree_taken";

/// Why [`create`] has no worktree to give.
#[derive(Debug, thiserror::Error)]
pub enum Unmade {
    /// Its folder, or its branch, is held by something else: no retry
    /// changes that until someone moves it.
    #[error("{0}")]
    Taken(String),
    #[error(transparent)]
    Failed(#[from] eyre::Report),
}

/// `branch`'s worktree, `<repo>/.worktrees/<slug>`: the one there already
/// when it is that branch's, as git lists it (its uncommitted work kept);
/// else made (new from HEAD, or the existing branch), over git's entry for
/// a folder deleted by hand. A folder there that isn't, a locked entry, or
/// the branch checked out elsewhere, is [`Unmade::Taken`].
pub async fn create(cwd: &Path, branch: &str) -> Result<PathBuf, Unmade> {
    let (root, path) = located(cwd, branch).await?;
    let list: Vec<Listed> = worktrees(&root)
        .await?
        .into_iter()
        .map(|w| Listed {
            gone: !w.path.exists(),
            path: canonical(&w.path),
            ..w
        })
        .collect();
    let force = match standing(&list, &canonical(&path), path.exists(), branch) {
        Standing::Own => return Ok(path),
        Standing::Taken(why) => return Err(Unmade::Taken(why)),
        Standing::Free => false,
        Standing::Stale => true,
    };
    add(&root, &path, branch, None, force)
        .await
        .map_err(unmade)?;
    Ok(path)
}

/// `root`'s worktrees as git lists them: NUL-separated where git can
/// (2.36 and later), else by line.
async fn worktrees(root: &Path) -> Result<Vec<Listed>> {
    match git(root, &["worktree", "list", "--porcelain", "-z"]).await {
        Ok(out) => Ok(listed(&out, '\0')),
        Err(_) => git(root, &["worktree", "list", "--porcelain"])
            .await
            .map(|out| listed(&out, '\n')),
    }
}

/// A worktree as `git worktree list --porcelain` says it: its folder, its
/// branch (none when detached), and whether it is locked; and whether its
/// folder is gone, as [`create`] finds it.
#[derive(Debug, PartialEq)]
struct Listed {
    path: PathBuf,
    branch: Option<String>,
    locked: bool,
    gone: bool,
}

/// The worktrees of a porcelain listing whose fields end in `sep`, each
/// worktree's fields closed by an empty one.
fn listed(porcelain: &str, sep: char) -> Vec<Listed> {
    porcelain
        .split(sep)
        .collect::<Vec<_>>()
        .split(|f| f.is_empty())
        .filter_map(|fields| {
            let path = fields.iter().find_map(|f| f.strip_prefix("worktree "))?;
            let branch = fields
                .iter()
                .find_map(|f| f.strip_prefix("branch "))
                .map(|b| b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
            Some(Listed {
                path: PathBuf::from(path),
                branch,
                locked: fields
                    .iter()
                    .any(|f| *f == "locked" || f.starts_with("locked ")),
                gone: false,
            })
        })
        .collect()
}

/// `path` with its links resolved, or as it is when it can't be.
pub fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// A failed `git worktree add`: [`Unmade::Taken`] when what is in its way
/// came about since the worktrees were read.
fn unmade(e: eyre::Report) -> Unmade {
    let said = e.to_string();
    match [
        "already exists",
        "is already checked out",
        "is already used by worktree",
    ]
    .iter()
    .any(|s| said.contains(s))
    {
        true => Unmade::Taken(said),
        false => Unmade::Failed(e),
    }
}

/// What `path`, `branch`'s worktree folder, is now.
#[derive(Debug, PartialEq)]
enum Standing {
    Free,
    /// Its entry, or `branch`'s elsewhere, is of a folder gone and not
    /// locked: made again over it.
    Stale,
    Own,
    Taken(String),
}

fn standing(list: &[Listed], path: &Path, there: bool, branch: &str) -> Standing {
    let at = list.iter().find(|w| w.path == path);
    let elsewhere = list
        .iter()
        .find(|w| w.path != path && w.branch.as_deref() == Some(branch));
    let shown = path.display();
    match (there, at, elsewhere) {
        (true, Some(w), _) if w.branch.as_deref() == Some(branch) => Standing::Own,
        (true, Some(Listed { branch: Some(other), .. }), _) => Standing::Taken(format!(
            "{shown} is there already on {other}, not {branch}: move it away, or check out {branch} in it"
        )),
        (true, Some(_), _) => Standing::Taken(format!(
            "{shown} is there already with no branch checked out, not {branch}: move it away, or check out {branch} in it"
        )),
        (true, None, _) => Standing::Taken(format!(
            "{shown} is there already and isn't a worktree of its repository: move it away"
        )),
        (false, Some(w), _) if w.locked => Standing::Taken(format!(
            "{shown} is gone, but git keeps it as a locked worktree: unlock it (git worktree unlock) or remove it"
        )),
        (false, _, Some(w)) if w.gone && w.locked => Standing::Taken(format!(
            "{branch} is checked out in {}, which is gone, but git keeps it as a locked worktree: unlock it (git worktree unlock) or remove it",
            w.path.display()
        )),
        (false, _, Some(w)) if w.gone => Standing::Stale,
        (false, _, Some(w)) => Standing::Taken(format!(
            "{branch} is checked out already in {}: Desk works on it only in {shown}",
            w.path.display()
        )),
        (false, Some(_), None) => Standing::Stale,
        (false, None, None) => Standing::Free,
    }
}

/// Creates `<repo>/.worktrees/<slug>` on a new `branch` from `start` (a
/// commit), as a handoff to a new worktree does: from the source's HEAD,
/// whatever the main checkout's is. Refused when `branch` exists.
pub async fn create_at(cwd: &Path, branch: &str, start: &str) -> Result<PathBuf> {
    create_from(cwd, branch, start).await
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

/// `branch`'s worktree folder in `cwd`'s repository: its root, and
/// `<root>/.worktrees/<slug>`.
pub(crate) async fn located(cwd: &Path, branch: &str) -> Result<(PathBuf, PathBuf)> {
    let name = slug(branch);
    if name.is_empty() {
        eyre::bail!("branch name {branch:?} has no usable characters");
    }
    git(cwd, &["check-ref-format", "--branch", branch]).await?;
    let root = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"]).await?);
    let path = root.join(".worktrees").join(&name);
    Ok((root, path))
}

/// Adds the worktree `path` on `branch`: the existing branch, or a new one
/// from `start` (HEAD when none); `force` over git's entry for it, its
/// folder gone. A new one asked of an existing branch is refused.
async fn add(
    root: &Path,
    path: &Path,
    branch: &str,
    start: Option<&str>,
    force: bool,
) -> Result<()> {
    let exists = git(
        root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .await
    .is_ok();
    if exists && start.is_some() {
        eyre::bail!("a branch called {branch} already exists");
    }
    let path_arg = path.to_string_lossy();
    let args: Vec<&str> = ["worktree", "add"]
        .into_iter()
        .chain(force.then_some("--force"))
        .chain((!exists).then_some(["-b", branch]).into_iter().flatten())
        .chain([path_arg.as_ref()])
        .chain(exists.then_some(branch))
        .chain(start)
        .collect();
    git(root, &args).await.map(drop)
}

async fn create_from(cwd: &Path, branch: &str, start: &str) -> Result<PathBuf> {
    let (root, path) = located(cwd, branch).await?;
    if path.exists() {
        eyre::bail!("{} already exists", path.display());
    }
    add(&root, &path, branch, Some(start), false).await?;
    Ok(path)
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
        let path = create_from(cwd, &branch, "HEAD").await?;
        return Ok((path, branch));
    }
    eyre::bail!("every name from {base} to {base}-{FRESH_TRIES} is taken")
}

/// Whether `cwd` is in a git repository: only one can have a worktree.
pub async fn in_repo(cwd: &Path) -> bool {
    git(cwd, &["rev-parse", "--show-toplevel"]).await.is_ok()
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

/// A repository at `dir` with one commit on `main`, for tests.
#[cfg(test)]
pub(crate) async fn test_repo(dir: &Path) {
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
        git(dir, &args).await.unwrap();
    }
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
        test_repo(repo).await;
        let wt = create(repo, "desk/auth-fix").await.unwrap();
        assert!(wt.ends_with(".worktrees/desk-auth-fix"));
        assert_eq!(
            git(&wt, &["branch", "--show-current"]).await.unwrap(),
            "desk/auth-fix"
        );
        assert!(create(repo, "bad..name").await.is_err());
    }

    fn taken(e: Unmade) -> String {
        match e {
            Unmade::Taken(why) => why,
            Unmade::Failed(e) => panic!("failed, not taken: {e:#}"),
        }
    }

    /// A branch's own worktree is started in again as it was left: its
    /// uncommitted work is kept.
    #[tokio::test]
    async fn a_branchs_own_worktree_is_reused_with_its_work() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        test_repo(repo).await;
        let first = create(repo, "man-515").await.unwrap();
        std::fs::write(first.join("half-done.rs"), "fn wip() {}").unwrap();
        let again = create(repo, "man-515").await.unwrap();
        assert_eq!(again, first);
        assert_eq!(
            std::fs::read_to_string(again.join("half-done.rs")).unwrap(),
            "fn wip() {}"
        );
        assert_eq!(
            git(&again, &["branch", "--show-current"]).await.unwrap(),
            "man-515"
        );
    }

    /// What isn't the branch's own worktree is never started in: a plain
    /// folder at its place, a worktree there on another branch or on none,
    /// and the branch checked out elsewhere.
    #[tokio::test]
    async fn a_folder_that_isnt_the_branchs_worktree_is_taken() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        test_repo(repo).await;
        let root = PathBuf::from(git(repo, &["rev-parse", "--show-toplevel"]).await.unwrap());
        std::fs::create_dir_all(root.join(".worktrees/man-1")).unwrap();
        let plain = taken(create(repo, "man-1").await.unwrap_err());
        assert!(plain.contains("isn't a worktree"), "{plain}");

        let other = root.join(".worktrees/man-2");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-b",
                "someone-else",
                &other.to_string_lossy(),
            ],
        )
        .await
        .unwrap();
        let on_other = taken(create(repo, "man-2").await.unwrap_err());
        assert!(
            on_other.contains("on someone-else, not man-2"),
            "{on_other}"
        );

        let detached = root.join(".worktrees/man-3");
        git(
            &root,
            &["worktree", "add", "--detach", &detached.to_string_lossy()],
        )
        .await
        .unwrap();
        let none = taken(create(repo, "man-3").await.unwrap_err());
        assert!(none.contains("no branch checked out"), "{none}");

        let elsewhere = root.join("elsewhere");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-b",
                "man-4",
                &elsewhere.to_string_lossy(),
            ],
        )
        .await
        .unwrap();
        let checked_out = taken(create(repo, "man-4").await.unwrap_err());
        assert!(
            checked_out.contains("man-4 is checked out already in"),
            "{checked_out}"
        );
        assert!(!root.join(".worktrees/man-4").exists());
    }

    /// A worktree whose folder was deleted but that git still lists is made
    /// again on its branch.
    #[tokio::test]
    async fn a_deleted_worktree_still_registered_is_made_again() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        test_repo(repo).await;
        let first = create(repo, "man-9").await.unwrap();
        std::fs::remove_dir_all(&first).unwrap();
        let again = create(repo, "man-9").await.unwrap();
        assert_eq!(again, first);
        assert_eq!(
            git(&again, &["branch", "--show-current"]).await.unwrap(),
            "man-9"
        );
    }

    /// A worktree git keeps locked, its folder gone, is never made over.
    #[tokio::test]
    async fn a_locked_worktree_whose_folder_is_gone_is_taken() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        test_repo(repo).await;
        let first = create(repo, "man-8").await.unwrap();
        git(repo, &["worktree", "lock", &first.to_string_lossy()])
            .await
            .unwrap();
        std::fs::remove_dir_all(&first).unwrap();
        let locked = taken(create(repo, "man-8").await.unwrap_err());
        assert!(locked.contains("locked worktree"), "{locked}");
        assert!(!first.exists());
    }

    /// A branch checked out in a worktree elsewhere whose folder is gone,
    /// not locked, is made again in its own place; locked, it is taken.
    #[tokio::test]
    async fn a_branch_in_a_gone_worktree_elsewhere_is_made_again_here() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        test_repo(repo).await;
        for (branch, locked) in [("man-6", false), ("man-7", true)] {
            let elsewhere = repo.join(format!("elsewhere-{branch}"));
            let at = elsewhere.to_string_lossy().into_owned();
            git(repo, &["worktree", "add", "-b", branch, &at])
                .await
                .unwrap();
            if locked {
                git(repo, &["worktree", "lock", &at]).await.unwrap();
            }
            std::fs::remove_dir_all(&elsewhere).unwrap();
            match locked {
                false => {
                    let made = create(repo, branch).await.unwrap();
                    assert!(made.ends_with(format!(".worktrees/{branch}")));
                    assert_eq!(
                        git(&made, &["branch", "--show-current"]).await.unwrap(),
                        branch
                    );
                }
                true => {
                    let why = taken(create(repo, branch).await.unwrap_err());
                    assert!(
                        why.contains("which is gone") && why.contains("locked"),
                        "{why}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_worktree_list_says_each_folder_its_branch_and_its_lock() {
        let fields = [
            "worktree /dev/atlas",
            "HEAD 1111",
            "branch refs/heads/main",
            "",
            "worktree /dev/atlas/.worktrees/man-5",
            "HEAD 2222",
            "detached",
            "",
            "worktree /gone/man-6",
            "HEAD 3333",
            "branch refs/heads/man-6",
            "prunable gitdir file points to non-existent location",
            "",
            "worktree /dev/atlas/.worktrees/man-7",
            "HEAD 4444",
            "branch refs/heads/man-7",
            "locked on a stick",
            "",
        ];
        let by_line = listed(fields.join("\n").trim_end(), '\n');
        let by_nul = listed(&fields.join("\0"), '\0');
        assert_eq!(by_line, by_nul);
        let list = by_nul;
        assert_eq!(
            list,
            vec![
                Listed {
                    path: "/dev/atlas".into(),
                    branch: Some("main".into()),
                    locked: false,
                    gone: false,
                },
                Listed {
                    path: "/dev/atlas/.worktrees/man-5".into(),
                    branch: None,
                    locked: false,
                    gone: false,
                },
                Listed {
                    path: "/gone/man-6".into(),
                    branch: Some("man-6".into()),
                    locked: false,
                    gone: false,
                },
                Listed {
                    path: "/dev/atlas/.worktrees/man-7".into(),
                    branch: Some("man-7".into()),
                    locked: true,
                    gone: false,
                },
            ]
        );
        let at = Path::new("/dev/atlas/.worktrees/man-6");
        assert!(matches!(
            standing(&list, at, false, "man-6"),
            Standing::Taken(why) if why.contains("checked out already in /gone/man-6")
        ));
        assert_eq!(standing(&list, at, false, "man-9"), Standing::Free);
        let own = Path::new("/dev/atlas/.worktrees/man-5");
        assert!(matches!(
            standing(&list, own, true, "man-5"),
            Standing::Taken(why) if why.contains("no branch checked out")
        ));
        assert_eq!(standing(&list, own, false, "man-5"), Standing::Stale);
        let locked = Path::new("/dev/atlas/.worktrees/man-7");
        assert!(matches!(
            standing(&list, locked, false, "man-7"),
            Standing::Taken(why) if why.contains("locked")
        ));
        assert_eq!(standing(&list, locked, true, "man-7"), Standing::Own);
    }

    /// What git finds in a worktree's way after the list was read is
    /// taken; anything else failed.
    #[test]
    fn a_worktree_git_finds_in_the_way_is_taken() {
        for said in [
            "git worktree add: fatal: '/r/.worktrees/x' already exists",
            "git worktree add: fatal: 'x' is already checked out at '/r/y'",
            "git worktree add: fatal: 'x' is already used by worktree at '/r/y'",
        ] {
            assert!(matches!(unmade(eyre::eyre!("{said}")), Unmade::Taken(w) if w == said));
        }
        assert!(matches!(
            unmade(eyre::eyre!("git worktree add: fatal: not a git repository")),
            Unmade::Failed(_)
        ));
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
        test_repo(repo).await;
        // Someone's own branch of that name, with no copy.
        git(repo, &["branch", "desk/fix-login"]).await.unwrap();
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
