//! What a session's folder holds, read with a hardened git that runs no
//! hook or fsmonitor and fetches nothing: what this computer knows of its
//! remotes is what counts. Closing reads what a session would leave behind
//! from it ([`Folder`], [`held`]), and so does what the boss sees inside a
//! session, so the two never disagree.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a git command in a session's folder may take.
const GIT_WITHIN: Duration = Duration::from_secs(10);

/// What a session's folder holds that closing it would leave behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Folder {
    /// Nothing to lose: no repository there (or no folder at all).
    Nothing,
    /// A repository with everything committed: its HEAD, and how many of
    /// its commits are on no remote.
    Clean { head: Option<String>, unpushed: u32 },
    /// Uncommitted files, this many.
    Uncommitted(usize),
    /// It couldn't be read: kept open.
    Unread,
}

/// A session's folder as git reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Workspace {
    /// No repository there (or no folder at all).
    Nothing,
    /// It couldn't be read.
    Unread,
    Repo(Repo),
}

/// A repository's work tree, as this computer knows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Repo {
    /// Its branch; none when its HEAD is detached.
    pub branch: Option<String>,
    /// Its HEAD commit, whole; none before its first commit.
    pub head: Option<String>,
    /// The branch it tracks (`origin/main`), when it tracks one.
    pub upstream: Option<String>,
    /// How many commits it is ahead of and behind its upstream, when that
    /// upstream is there to count against.
    pub ahead_behind: Option<(u32, u32)>,
    /// How many of its commits are on no remote; none when that couldn't be
    /// counted.
    pub unpushed: Option<u32>,
    /// The paths it changed and hasn't committed, Finder's own files left
    /// out.
    pub uncommitted: Vec<String>,
    /// Its newest commits, newest first.
    pub recent: Vec<Commit>,
    /// `origin`'s GitHub repository (`owner/name`), when it has one.
    pub origin: Option<String>,
    /// Whether it is a linked worktree of another checkout.
    pub worktree: bool,
}

/// One commit, as `git log` says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Commit {
    pub short: String,
    /// When it was committed, as git writes it (ISO 8601).
    pub at: String,
    /// Its subject, as its author wrote it.
    pub subject: String,
}

impl Workspace {
    /// What closing it would leave behind.
    pub(crate) fn folder(&self) -> Folder {
        match self {
            Self::Nothing => Folder::Nothing,
            Self::Unread => Folder::Unread,
            Self::Repo(r) if !r.uncommitted.is_empty() => Folder::Uncommitted(r.uncommitted.len()),
            Self::Repo(Repo { head: None, .. }) => Folder::Clean {
                head: None,
                unpushed: 0,
            },
            Self::Repo(Repo {
                head: Some(head),
                unpushed: Some(unpushed),
                ..
            }) => Folder::Clean {
                head: Some(head.clone()),
                unpushed: *unpushed,
            },
            Self::Repo(_) => Folder::Unread,
        }
    }
}

/// Why `folder` keeps a session open, in a few words; none when nothing
/// would be left behind. Commits on no remote count as pushed when a
/// merged pull request's head is its HEAD (its branch deleted on merge).
pub(crate) fn held(folder: &Folder, merged_heads: &[String]) -> Option<String> {
    match folder {
        Folder::Nothing => None,
        Folder::Clean { unpushed: 0, .. } => None,
        Folder::Clean { head: Some(h), .. } if merged_heads.iter().any(|m| m == h) => None,
        Folder::Clean { unpushed: 1, .. } => Some("1 commit not pushed".into()),
        Folder::Clean { unpushed, .. } => Some(format!("{unpushed} commits not pushed")),
        Folder::Uncommitted(1) => Some("1 uncommitted file".into()),
        Folder::Uncommitted(n) => Some(format!("{n} uncommitted files")),
        Folder::Unread => Some("its folder couldn't be checked".into()),
    }
}

/// `owner/name` of a GitHub remote's URL (`https://github.com/o/r.git`,
/// `git@github.com:o/r.git`, `ssh://git@github.com/o/r`); none for
/// another host.
pub(crate) fn github_repo(url: &str) -> Option<String> {
    let u = url.trim();
    let rest = u
        .strip_prefix("git@github.com:")
        .or_else(|| u.strip_prefix("ssh://git@github.com/"))
        .or_else(|| u.strip_prefix("https://github.com/"))
        .or_else(|| u.strip_prefix("http://github.com/"))
        .or_else(|| u.strip_prefix("git://github.com/"))?;
    let mut parts = rest.trim_end_matches('/').splitn(3, '/');
    let owner = parts.next()?;
    let name = parts.next()?.trim_end_matches(".git");
    let ok = |p: &str| {
        !p.is_empty()
            && p != "."
            && p != ".."
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (ok(owner) && ok(name)).then(|| format!("{owner}/{name}"))
}

/// The repository and number of a pull request an account names: by its
/// URL, as `owner/name#n`, or as `#n` of the folder's own repository
/// (`origin`).
pub(crate) fn pull_ref(id: &str, url: Option<&str>, origin: Option<&str>) -> Option<(String, u64)> {
    if let Some(u) = url {
        if let Some((repo, n)) = u.split_once("/pull/") {
            let n: u64 = n
                .split(|c: char| !c.is_ascii_digit())
                .next()?
                .parse()
                .ok()?;
            return github_repo(repo).map(|r| (r, n));
        }
    }
    let id = id.trim();
    let (repo, n) = match id.split_once('#') {
        Some((r, n)) if !r.trim().is_empty() => {
            (github_repo(&format!("https://github.com/{}", r.trim()))?, n)
        }
        Some((_, n)) => (origin?.to_string(), n),
        None => (origin?.to_string(), id),
    };
    n.trim().parse().ok().map(|n| (repo, n))
}

/// Runs git in `cwd`, hardened (no hook or fsmonitor runs), its trimmed
/// output; `Err(true)` when `cwd` is no repository, `Err(false)` when it
/// couldn't be run.
async fn git(cwd: &Path, args: &[&str]) -> Result<String, bool> {
    let mut cmd = crate::git_hardening::hardened_git();
    cmd.args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = match tokio::time::timeout(GIT_WITHIN, cmd.output()).await {
        Ok(Ok(out)) => out,
        _ => return Err(false),
    };
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    let err = String::from_utf8_lossy(&out.stderr).to_lowercase();
    Err(err.contains("not a git repository"))
}

/// What closing `cwd`'s session would leave behind.
pub(crate) async fn folder(cwd: &str) -> Folder {
    workspace_of(cwd).await.folder()
}

/// The folder `cwd` as git reads it now.
pub(crate) async fn workspace_of(cwd: &str) -> Workspace {
    let dir = Path::new(cwd);
    if cwd.trim().is_empty() || !dir.is_dir() {
        return Workspace::Nothing;
    }
    let found = match git(
        dir,
        &[
            "rev-parse",
            "--is-inside-work-tree",
            "--git-dir",
            "--git-common-dir",
        ],
    )
    .await
    {
        Ok(found) => found,
        Err(true) => return Workspace::Nothing,
        Err(false) => return Workspace::Unread,
    };
    let mut found = found.lines();
    // A bare repository or `.git` itself: nothing of the session's.
    if found.next() != Some("true") {
        return Workspace::Nothing;
    }
    let worktree = match (found.next(), found.next()) {
        (Some(own), Some(common)) => !same_dir(dir, own, common),
        _ => false,
    };
    let Ok(status) = git(
        dir,
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            "--untracked-files=normal",
        ],
    )
    .await
    else {
        return Workspace::Unread;
    };
    let mut repo = status_of(&status);
    repo.worktree = worktree;
    // The worktrees Desk keeps in a checkout are nobody's work; anything
    // else in their folder is.
    if repo.uncommitted.iter().any(|p| p == DESK_WORKTREES) && only_worktrees(dir).await {
        repo.uncommitted.retain(|p| p != DESK_WORKTREES);
    }
    if repo.head.is_some() {
        repo.unpushed = git(dir, &["rev-list", "--count", "HEAD", "--not", "--remotes"])
            .await
            .ok()
            .and_then(|n| n.parse().ok());
        repo.recent = git(dir, &["log", "-10", "--format=%h%x09%cI%x09%s"])
            .await
            .map(|log| log.lines().filter_map(commit_of).collect())
            .unwrap_or_default();
    }
    repo.origin = origin_of(cwd).await;
    Workspace::Repo(repo)
}

/// Whether git's own directory and its common one, as `rev-parse` says
/// them from `dir`, are one: a checkout of its own, not a linked worktree.
fn same_dir(dir: &Path, own: &str, common: &str) -> bool {
    let at = |p: &str| std::fs::canonicalize(dir.join(p)).ok();
    match (at(own), at(common)) {
        (Some(own), Some(common)) => own == common,
        _ => own == common,
    }
}

/// Where Desk keeps a checkout's worktrees, as `git status` names it.
const DESK_WORKTREES: &str = ".worktrees/";

/// Whether everything in the `.worktrees/` of `dir`'s checkout is one of
/// its worktrees, as git lists them.
async fn only_worktrees(dir: &Path) -> bool {
    let (Ok(top), Ok(listed)) = (
        git(dir, &["rev-parse", "--show-toplevel"]).await,
        git(dir, &["worktree", "list", "--porcelain"]).await,
    ) else {
        return false;
    };
    let real = |p: &Path| std::fs::canonicalize(p).ok();
    let listed: HashSet<PathBuf> = listed
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .filter_map(|p| real(Path::new(p)))
        .collect();
    let Ok(children) = std::fs::read_dir(Path::new(top.trim()).join(DESK_WORKTREES)) else {
        return false;
    };
    children
        .map(|c| c.ok().and_then(|c| real(&c.path())))
        .all(|c| c.is_some_and(|c| listed.contains(&c)))
}

/// A repository's branch, HEAD, upstream and changed paths from
/// `git status --porcelain=v2 -z --branch`.
fn status_of(status: &str) -> Repo {
    let entries: Vec<&str> = status.split('\0').collect();
    let header = |key: &str| {
        entries
            .iter()
            .find_map(|e| e.strip_prefix("# ")?.strip_prefix(key)?.strip_prefix(' '))
    };
    let ahead_behind = header("branch.ab").and_then(|ab| {
        let mut counts = ab
            .split_whitespace()
            .map(|c| c.trim_start_matches(['+', '-']).parse::<u32>().ok());
        Some((counts.next()??, counts.next()??))
    });
    Repo {
        branch: header("branch.head")
            .filter(|h| *h != "(detached)")
            .map(str::to_string),
        head: header("branch.oid")
            .filter(|h| *h != "(initial)")
            .map(str::to_string),
        upstream: header("branch.upstream").map(str::to_string),
        ahead_behind,
        uncommitted: entries
            .iter()
            // A rename's or copy's own path is followed by the one it had.
            .scan(false, |was_renamed, e| {
                let orig = *was_renamed;
                *was_renamed = !orig && e.starts_with("2 ");
                Some((!orig).then_some(*e))
            })
            .flatten()
            .filter_map(changed_path)
            // Finder's own files are nobody's work (the person's global
            // ignore isn't read by a hardened git).
            .filter(|p| !p.ends_with(".DS_Store"))
            .map(str::to_string)
            .collect(),
        ..Repo::default()
    }
}

/// The path one entry of `git status --porcelain=v2 -z` names: an
/// ordinary change, a rename or copy (its new path), a conflict, or a file
/// git doesn't track.
fn changed_path(entry: &str) -> Option<&str> {
    let field = |n: usize| entry.splitn(n, ' ').nth(n - 1);
    match entry.split_once(' ')?.0 {
        "1" => field(9),
        "2" => field(10),
        "u" => field(11),
        "?" => field(2),
        _ => None,
    }
    .filter(|p| !p.is_empty())
}

/// One line of `git log --format=%h%x09%cI%x09%s`.
fn commit_of(line: &str) -> Option<Commit> {
    let mut parts = line.splitn(3, '\t');
    Some(Commit {
        short: parts.next()?.to_string(),
        at: parts.next()?.to_string(),
        subject: parts.next().unwrap_or_default().to_string(),
    })
}

/// `origin`'s GitHub repository of the folder `cwd`, when it has one.
pub(crate) async fn origin_of(cwd: &str) -> Option<String> {
    let dir = Path::new(cwd);
    if !dir.is_dir() {
        return None;
    }
    // As configured: a repository's own `insteadOf` doesn't move it.
    git(dir, &["config", "--get", "remote.origin.url"])
        .await
        .ok()
        .and_then(|u| github_repo(&u))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncommitted_or_unpushed_work_keeps_it_open_unless_its_merged_head_is_head() {
        assert_eq!(held(&Folder::Nothing, &[]), None);
        let clean = |unpushed| Folder::Clean {
            head: Some("abc".into()),
            unpushed,
        };
        assert_eq!(held(&clean(0), &[]), None);
        assert_eq!(held(&clean(1), &[]), Some("1 commit not pushed".into()));
        assert_eq!(held(&clean(3), &[]), Some("3 commits not pushed".into()));
        // Its branch deleted on merge: what merged is its HEAD.
        assert_eq!(held(&clean(3), &["abc".into()]), None);
        assert_eq!(
            held(&clean(3), &["def".into()]),
            Some("3 commits not pushed".into())
        );
        assert_eq!(
            held(&Folder::Uncommitted(1), &["abc".into()]),
            Some("1 uncommitted file".into())
        );
        assert_eq!(
            held(&Folder::Uncommitted(4), &[]),
            Some("4 uncommitted files".into())
        );
        assert!(
            held(&Folder::Unread, &[]).is_some(),
            "what can't be read is kept"
        );
    }

    #[test]
    fn a_github_remote_and_an_accounts_pr_are_read_as_owner_name_and_number() {
        for url in [
            "https://github.com/driftwerk/workforce.git",
            "https://github.com/driftwerk/workforce",
            "git@github.com:driftwerk/workforce.git",
            "ssh://git@github.com/driftwerk/workforce",
        ] {
            assert_eq!(
                github_repo(url).as_deref(),
                Some("driftwerk/workforce"),
                "{url}"
            );
        }
        assert_eq!(github_repo("https://gitlab.com/a/b.git"), None);
        assert_eq!(github_repo("https://github.com/a"), None);
        assert_eq!(github_repo("https://github.com/../b"), None);
        let origin = Some("acme/web");
        assert_eq!(
            pull_ref("#624", None, origin),
            Some(("acme/web".into(), 624))
        );
        assert_eq!(
            pull_ref("624", None, origin),
            Some(("acme/web".into(), 624))
        );
        assert_eq!(
            pull_ref("driftwerk/workforce#434", None, origin),
            Some(("driftwerk/workforce".into(), 434))
        );
        assert_eq!(
            pull_ref(
                "#1",
                Some("https://github.com/driftwerk/workforce-desk/pull/307/files"),
                origin
            ),
            Some(("driftwerk/workforce-desk".into(), 307))
        );
        assert_eq!(
            pull_ref("#624", None, None),
            None,
            "no repository to read it in"
        );
        assert_eq!(pull_ref("the fix", None, origin), None);
    }

    async fn sh(dir: &Path, args: &[&str]) {
        let ok = tokio::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .await
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    #[tokio::test]
    async fn a_folder_is_read_for_what_closing_would_leave_behind() {
        let t = tempfile::tempdir().unwrap();
        let none = t.path().join("gone");
        assert_eq!(folder(none.to_str().unwrap()).await, Folder::Nothing);
        let plain = t.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(folder(plain.to_str().unwrap()).await, Folder::Nothing);

        let remote = t.path().join("remote.git");
        let work = t.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        sh(
            t.path(),
            &["init", "-q", "--bare", remote.to_str().unwrap()],
        )
        .await;
        sh(&work, &["init", "-q"]).await;
        let w = work.to_str().unwrap();
        // Nothing committed yet, nothing changed: nothing to lose.
        assert_eq!(
            folder(w).await,
            Folder::Clean {
                head: None,
                unpushed: 0
            }
        );
        std::fs::write(work.join("a.txt"), "a").unwrap();
        std::fs::write(work.join(".DS_Store"), "x").unwrap();
        assert_eq!(
            folder(w).await,
            Folder::Uncommitted(1),
            "Finder's file isn't work"
        );
        std::fs::create_dir_all(work.join(".worktrees/notes")).unwrap();
        std::fs::write(work.join(".worktrees/notes/x.ts"), "x").unwrap();
        assert_eq!(
            folder(w).await,
            Folder::Uncommitted(2),
            "a folder in .worktrees that is no worktree is work"
        );
        std::fs::remove_dir_all(work.join(".worktrees/notes")).unwrap();
        sh(&work, &["add", "a.txt"]).await;
        sh(&work, &["commit", "-q", "-m", "a"]).await;
        std::fs::remove_file(work.join(".DS_Store")).unwrap();
        sh(
            &work,
            &["worktree", "add", "-q", "--detach", ".worktrees/review-7", "HEAD"],
        )
        .await;
        assert!(matches!(
            folder(w).await,
            Folder::Clean {
                unpushed: 1,
                head: Some(_)
            }
        ));
        sh(
            &work,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        )
        .await;
        sh(&work, &["push", "-q", "origin", "HEAD:refs/heads/main"]).await;
        assert!(matches!(
            folder(w).await,
            Folder::Clean {
                unpushed: 0,
                head: Some(_)
            }
        ));
        // A planted fsmonitor never runs.
        let planted = t.path().join("ran");
        sh(
            &work,
            &[
                "config",
                "core.fsmonitor",
                &format!("touch {}", planted.display()),
            ],
        )
        .await;
        let _ = folder(w).await;
        assert!(
            !planted.exists(),
            "the repository's own config ran a command"
        );
    }

    #[tokio::test]
    async fn a_workspace_says_its_branch_upstream_counts_changes_commits_and_worktree() {
        let t = tempfile::tempdir().unwrap();
        let remote = t.path().join("remote.git");
        let work = t.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        sh(
            t.path(),
            &["init", "-q", "--bare", remote.to_str().unwrap()],
        )
        .await;
        sh(&work, &["init", "-q", "-b", "main"]).await;
        let w = work.to_str().unwrap();
        let Workspace::Repo(empty) = workspace_of(w).await else {
            panic!("a repository");
        };
        assert_eq!(empty.branch.as_deref(), Some("main"));
        assert_eq!((empty.head, empty.unpushed), (None, None));
        assert!(empty.recent.is_empty() && !empty.worktree);

        std::fs::write(work.join("a.txt"), "a").unwrap();
        sh(&work, &["add", "a.txt"]).await;
        sh(&work, &["commit", "-q", "-m", "First\tof all"]).await;
        sh(
            &work,
            &[
                "remote",
                "add",
                "origin",
                "git@github.com:driftwerk/workforce.git",
            ],
        )
        .await;
        sh(
            &work,
            &[
                "push",
                "-q",
                remote.to_str().unwrap(),
                "main:refs/heads/main",
            ],
        )
        .await;
        sh(
            &work,
            &[
                "fetch",
                "-q",
                remote.to_str().unwrap(),
                "+refs/heads/main:refs/remotes/origin/main",
            ],
        )
        .await;
        sh(&work, &["branch", "-q", "--set-upstream-to=origin/main"]).await;
        for n in ["b", "c"] {
            std::fs::write(work.join(n), n).unwrap();
            sh(&work, &["add", n]).await;
            sh(&work, &["commit", "-q", "-m", &format!("Add {n}")]).await;
        }
        std::fs::write(work.join("a.txt"), "changed").unwrap();
        std::fs::write(work.join("new file.txt"), "new").unwrap();
        std::fs::write(work.join(".DS_Store"), "x").unwrap();

        let Workspace::Repo(r) = workspace_of(w).await else {
            panic!("a repository");
        };
        assert_eq!(r.branch.as_deref(), Some("main"));
        assert_eq!(r.head.as_ref().map(String::len), Some(40));
        assert_eq!(r.upstream.as_deref(), Some("origin/main"));
        assert_eq!(r.ahead_behind, Some((2, 0)));
        assert_eq!(r.unpushed, Some(2));
        assert_eq!(r.uncommitted, ["a.txt", "new file.txt"]);
        assert_eq!(
            r.recent
                .iter()
                .map(|c| c.subject.as_str())
                .collect::<Vec<_>>(),
            ["Add c", "Add b", "First\tof all"]
        );
        assert!(r
            .recent
            .iter()
            .all(|c| c.short.len() >= 7 && c.at.contains('T')));
        assert_eq!(r.origin.as_deref(), Some("driftwerk/workforce"));
        assert!(!r.worktree);
        assert_eq!(
            Workspace::Repo(r.clone()).folder(),
            Folder::Uncommitted(2),
            "its leftovers are closing's"
        );

        let linked = t.path().join("linked");
        sh(
            &work,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "side",
                linked.to_str().unwrap(),
            ],
        )
        .await;
        let Workspace::Repo(side) = workspace_of(linked.to_str().unwrap()).await else {
            panic!("a repository");
        };
        assert!(side.worktree);
        assert_eq!(side.branch.as_deref(), Some("side"));
        assert_eq!(side.upstream, None);
        assert_eq!(side.ahead_behind, None);
        assert_eq!(side.head, r.head);

        sh(&linked, &["checkout", "-q", "--detach"]).await;
        let Workspace::Repo(detached) = workspace_of(linked.to_str().unwrap()).await else {
            panic!("a repository");
        };
        assert_eq!(detached.branch, None);
        assert!(detached.head.is_some());
    }

    #[test]
    fn porcelain_v2_entries_give_their_paths() {
        let status = [
            "# branch.oid 1234",
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +3 -1",
            "1 .M N... 100644 100644 100644 aaa bbb src/a b.rs",
            "2 R. N... 100644 100644 100644 aaa bbb R100 new name.rs",
            "2 R. N... 100644 100644 100644 aaa bbb R100 an old name",
            "? after.rs",
            "u UU N... 100644 100644 100644 100644 aaa bbb ccc conflict.rs",
            "? untracked/",
            "? sub/.DS_Store",
            "! ignored.log",
            "",
        ]
        .join("\0");
        let r = status_of(&status);
        assert_eq!(
            r.uncommitted,
            [
                "src/a b.rs",
                "new name.rs",
                "after.rs",
                "conflict.rs",
                "untracked/"
            ],
            "a rename's old path is never a change of its own"
        );
        assert_eq!(r.ahead_behind, Some((3, 1)));
        assert_eq!(r.head.as_deref(), Some("1234"));
        let fresh = status_of("# branch.oid (initial)\0# branch.head (detached)\0");
        assert_eq!((fresh.head, fresh.branch), (None, None));
    }
}
