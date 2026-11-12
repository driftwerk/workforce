//! The folders a person may pick for a session: `GET /api/desk/folders`,
//! and one made for it: `POST /api/desk/folders`.
//!
//! Folders only, never files, and only inside the hub user's home or a live
//! session's folder. A path is followed through symlinks before it is
//! checked, so `..` and a link that leads out are refused alike. A folder is
//! made only inside the home, from a path with no `.` or `..` in it.

use std::path::{Path, PathBuf};

use serde::Serialize;

/// Most folders one answer carries.
pub const MAX_FOLDERS: usize = 500;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum FolderError {
    #[error("that folder is outside the home folder and the running sessions' folders")]
    Outside,
    #[error("no such folder")]
    NotFound,
    #[error("that is not a folder")]
    NotAFolder,
    #[error("the hub has no home folder to start from")]
    NoHome,
    #[error("the folder could not be read")]
    Unreadable,
    #[error("a folder's path can't hold . or .., or be empty")]
    BadName,
    #[error("only a folder inside the home folder can be made")]
    OutsideHome,
    #[error("the folder could not be made")]
    NotMade,
}

/// Most folders one make may add below the deepest one already there.
pub const MAX_MADE: usize = 8;

/// Makes the folder `requested` (`~` and `~/x` are the home; anything else
/// must be absolute) and any folders above it that aren't there yet, all
/// inside `home`; one already there is its answer too. What is already
/// there is followed through symlinks and must be inside the home, so a link
/// that leads out is never made into. Returns the folder, links resolved.
pub fn make(requested: &str, home: Option<&Path>) -> Result<String, FolderError> {
    use std::path::Component;
    let home = home
        .and_then(|h| h.canonicalize().ok())
        // A home that is the whole disk is no home to make folders in.
        .filter(|h| h.parent().is_some())
        .ok_or(FolderError::NoHome)?;
    let r = requested.trim();
    // As typed: a path drops a `.` inside it as it is read.
    if r.is_empty() || r.contains('\0') || r.split('/').any(|p| p == "." || p == "..") {
        return Err(FolderError::BadName);
    }
    let wanted = match expand_tilde(r) {
        Some(rest) if r.starts_with('~') => home.join(rest),
        Some(abs) if abs.starts_with('/') => PathBuf::from(abs),
        // `~user`, or a path relative to nothing said.
        _ => return Err(FolderError::OutsideHome),
    };
    if wanted
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(FolderError::BadName);
    }
    // The deepest folder already there, and the names to make below it.
    let mut there = wanted.clone();
    let mut names = Vec::new();
    while std::fs::symlink_metadata(&there).is_err() {
        let Some(name) = there.file_name().map(|n| n.to_os_string()) else {
            return Err(FolderError::OutsideHome);
        };
        names.push(name);
        if !there.pop() || names.len() > MAX_MADE {
            return Err(FolderError::BadName);
        }
    }
    let base = there.canonicalize().map_err(|_| FolderError::Unreadable)?;
    if !base.starts_with(&home) {
        return Err(FolderError::OutsideHome);
    }
    if !base.is_dir() {
        return Err(FolderError::NotAFolder);
    }
    let mut made = base;
    for name in names.into_iter().rev() {
        made.push(name);
        match std::fs::create_dir(&made) {
            Ok(()) => {}
            // Made meanwhile: a folder of its own only (a link is refused).
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let meta = std::fs::symlink_metadata(&made).map_err(|_| FolderError::NotMade)?;
                if !meta.is_dir() {
                    return Err(FolderError::NotAFolder);
                }
            }
            Err(_) => return Err(FolderError::NotMade),
        }
    }
    // What was made, as the disk has it now, is still inside the home.
    let real = made.canonicalize().map_err(|_| FolderError::NotMade)?;
    if !real.starts_with(&home) {
        return Err(FolderError::OutsideHome);
    }
    Ok(real.to_string_lossy().into_owned())
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Folder {
    pub name: String,
    pub path: String,
    /// It holds a `.git`: a repository (or a worktree of one).
    pub git: bool,
    /// Where its repository comes from (its `origin`, else its first
    /// remote), as `host/owner/repo`: never a user name or password.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    pub hidden: bool,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Listing {
    /// The folder listed, with symlinks resolved.
    pub path: String,
    /// The folder above it, when that is still allowed.
    pub parent: Option<String>,
    pub folders: Vec<Folder>,
    /// There were more than [`MAX_FOLDERS`]; the first are shown.
    pub truncated: bool,
    /// The folder listed is in a git repository: a separate copy of it can
    /// be made.
    pub git: bool,
    /// Its repository's remote, as [`Folder::remote`] says one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
}

/// The `.git` folder that holds `dir`'s repository's config: its own, or
/// for a worktree the repository it was made from. Looked for in `dir`
/// (and, with `up`, the folders above it) while `may` allows them; every
/// path it is led to (a `.git` link, a worktree's `gitdir:`, its
/// `commondir`) is followed to where it really is and must be one `may`
/// allows too, so nothing outside is ever read.
fn git_dir(dir: &Path, up: bool, may: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    let allowed = |p: PathBuf| p.canonicalize().ok().filter(|r| may(r));
    let mut at = Some(dir);
    while let Some(d) = at.filter(|d| may(d)) {
        let dot = d.join(".git");
        if std::fs::symlink_metadata(&dot).is_ok() {
            let dot = allowed(dot)?;
            if dot.is_dir() {
                return Some(dot);
            }
            // A worktree: `gitdir: <repo>/.git/worktrees/<name>`, whose
            // `commondir` names the repository's own `.git`.
            let text = std::fs::read_to_string(&dot).ok()?;
            let linked = allowed(d.join(text.trim().strip_prefix("gitdir:")?.trim()))?;
            return match std::fs::read_to_string(linked.join("commondir")) {
                Ok(common) => allowed(linked.join(common.trim())),
                Err(_) => Some(linked),
            };
        }
        if !up {
            return None;
        }
        at = d.parent();
    }
    None
}

/// Where `dir`'s repository comes from, if it is in one with a remote:
/// looked for above it with `up`, and nowhere `may` doesn't allow.
pub fn remote_of(dir: &Path, up: bool, may: &dyn Fn(&Path) -> bool) -> Option<String> {
    let config = std::fs::read_to_string(git_dir(dir, up, may)?.join("config")).ok()?;
    let mut section = String::new();
    let mut first: Option<String> = None;
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            section = line.to_string();
            continue;
        }
        let Some(url) = line
            .strip_prefix("url")
            .map(str::trim_start)
            .and_then(|l| l.strip_prefix('='))
            .map(str::trim)
        else {
            continue;
        };
        if !section.starts_with("[remote ") {
            continue;
        }
        if section == "[remote \"origin\"]" {
            return shown_remote(url);
        }
        first.get_or_insert_with(|| url.to_string());
    }
    first.as_deref().and_then(shown_remote)
}

/// A remote's URL as a person reads it: `host/owner/repo`, with no scheme,
/// user, token, password, query, fragment or `.git`.
pub fn shown_remote(url: &str) -> Option<String> {
    let u = url.trim();
    // Never a query or a fragment: they can carry a token too.
    let u = u.split(['?', '#']).next().unwrap_or_default();
    let shown = match u.split_once("://") {
        // `scheme://[user[:password]@]host[:port]/path`: whoever it signs
        // in as goes, up to the last `@`, whatever the password holds.
        Some((_, rest)) => {
            let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
            let (host, path) = rest.split_once('/')?;
            format!("{host}/{path}")
        }
        // `[user@]host:owner/repo`
        None => {
            let rest = u.rsplit_once('@').map_or(u, |(_, r)| r);
            match rest.split_once(':') {
                Some((host, path)) if !host.is_empty() && !host.contains('/') => {
                    format!("{host}/{}", path.trim_start_matches('/'))
                }
                _ => return None,
            }
        }
    };
    let shown = shown
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_string();
    let host = shown.split('/').next().unwrap_or_default();
    (shown.len() > 2
        && !host.is_empty()
        && shown.contains('/')
        && !shown.chars().any(|c| c.is_control() || c.is_whitespace()))
    .then_some(shown)
}

/// `~` and `~/x` mean the hub user's home: the part after the `~/` (empty
/// for a bare `~`) is returned to be joined to home. `~user` is `None`: other
/// people's homes are never listed. Any other path comes back unchanged.
fn expand_tilde(r: &str) -> Option<&str> {
    match r.strip_prefix('~') {
        None => Some(r),
        Some("") => Some(""),
        Some(rest) => rest.strip_prefix('/').map(|x| x.trim_start_matches('/')),
    }
}

/// Lists the folders in `requested` (none or empty: `home`; a leading `~`
/// is `home`). `sessions` are
/// the folders live sessions run in, which are allowed as well as `home`.
pub fn list(
    requested: Option<&str>,
    hidden: bool,
    home: Option<&Path>,
    sessions: &[PathBuf],
) -> Result<Listing, FolderError> {
    let home = home
        .and_then(|h| h.canonicalize().ok())
        .ok_or(FolderError::NoHome)?;
    let mut roots = vec![home.clone()];
    roots.extend(sessions.iter().filter_map(|s| s.canonicalize().ok()));
    let inside = |p: &Path| roots.iter().any(|r| p.starts_with(r));

    let wanted = match requested.map(str::trim).filter(|r| !r.is_empty()) {
        None => home.clone(),
        Some(r) => match expand_tilde(r) {
            Some(rest) => home.join(rest),
            None => return Err(FolderError::Outside), // `~user`
        },
    };
    let dir = wanted.canonicalize().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => FolderError::NotFound,
        std::io::ErrorKind::NotADirectory => FolderError::NotAFolder,
        _ => FolderError::Unreadable,
    })?;
    // Checked before anything is read, so a path outside is never probed.
    if !inside(&dir) {
        return Err(FolderError::Outside);
    }
    if !dir.is_dir() {
        return Err(FolderError::NotAFolder);
    }

    let mut found = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(|_| FolderError::Unreadable)? {
        let Ok(entry) = entry else { continue };
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let is_hidden = name.starts_with('.');
        if is_hidden && !hidden {
            continue;
        }
        // A link is followed: it is a folder only if its target is, and is
        // left out unless the target is somewhere that may be listed.
        let Ok(real) = entry.path().canonicalize() else {
            continue;
        };
        if !real.is_dir() || !inside(&real) {
            continue;
        }
        let git = real.join(".git").exists();
        found.push(Folder {
            remote: git.then(|| remote_of(&real, false, &inside)).flatten(),
            git,
            path: real.to_string_lossy().into_owned(),
            name,
            hidden: is_hidden,
        });
    }
    found.sort_by_cached_key(|f| (f.name.to_lowercase(), f.name.clone()));
    let truncated = found.len() > MAX_FOLDERS;
    found.truncate(MAX_FOLDERS);
    Ok(Listing {
        parent: dir
            .parent()
            .filter(|p| inside(p))
            .map(|p| p.to_string_lossy().into_owned()),
        path: dir.to_string_lossy().into_owned(),
        folders: found,
        truncated,
        git: git_dir(&dir, true, &inside).is_some(),
        remote: remote_of(&dir, true, &inside),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_is_made_in_the_home_and_nowhere_else() {
        let t = tree();
        let home = Some(t.home.as_path());
        // One level, and several: `mkdir -p`, inside the home.
        let one = make("~/dagr", home).unwrap();
        assert_eq!(one, t.home.join("dagr").to_string_lossy());
        assert!(t.home.join("dagr").is_dir());
        let deep = t.home.join("src/new/app");
        assert_eq!(
            make(&deep.to_string_lossy(), home).unwrap(),
            deep.to_string_lossy()
        );
        assert!(deep.is_dir());
        // There already: its answer, nothing made.
        assert_eq!(make("~/dagr", home).unwrap(), one);
        // Never outside the home, nor through `..`, `.`, a link out, or `~user`.
        let out = t.outside.join("x");
        assert_eq!(
            make(&out.to_string_lossy(), home),
            Err(FolderError::OutsideHome)
        );
        assert!(!out.exists());
        assert_eq!(make("~/../escape", home), Err(FolderError::BadName));
        assert_eq!(make("~/./a", home), Err(FolderError::BadName));
        assert_eq!(make("~root/x", home), Err(FolderError::OutsideHome));
        assert_eq!(make("relative/x", home), Err(FolderError::OutsideHome));
        assert_eq!(make("  ", home), Err(FolderError::BadName));
        std::os::unix::fs::symlink(&t.outside, t.home.join("out-link")).unwrap();
        assert_eq!(make("~/out-link/x", home), Err(FolderError::OutsideHome));
        assert!(!t.outside.join("x").exists());
        // Under a file: not a folder.
        std::fs::write(t.home.join("a-file"), "x").unwrap();
        assert_eq!(make("~/a-file/x", home), Err(FolderError::NotAFolder));
        // Too deep at once.
        assert_eq!(make("~/1/2/3/4/5/6/7/8/9", home), Err(FolderError::BadName));
        assert_eq!(make("~/x", None), Err(FolderError::NoHome));
        // A home that resolves to `/`: nothing is made anywhere.
        assert_eq!(
            make("/tmp/desk-made-x", Some(Path::new("/"))),
            Err(FolderError::NoHome)
        );
        assert!(!Path::new("/tmp/desk-made-x").exists());
    }

    fn names(l: &Listing) -> Vec<&str> {
        l.folders.iter().map(|f| f.name.as_str()).collect()
    }

    struct Tree {
        _tmp: tempfile::TempDir,
        home: PathBuf,
        outside: PathBuf,
    }

    fn tree() -> Tree {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = root.join("home");
        let outside = root.join("outside");
        for d in ["app/.git", "app/src", "docs", ".config", "Zed"] {
            std::fs::create_dir_all(home.join(d)).unwrap();
        }
        std::fs::create_dir_all(outside.join("secret")).unwrap();
        std::fs::write(home.join("notes.txt"), "x").unwrap();
        Tree {
            _tmp: tmp,
            home,
            outside,
        }
    }

    #[test]
    fn it_defaults_to_home_lists_folders_only_and_marks_repositories() {
        let t = tree();
        let l = list(None, false, Some(&t.home), &[]).unwrap();
        assert_eq!(l.path, t.home.to_string_lossy());
        assert_eq!(names(&l), ["app", "docs", "Zed"]);
        assert!(l.folders[0].git && !l.folders[1].git);
        assert_eq!(list(Some(""), false, Some(&t.home), &[]).unwrap(), l);
    }

    #[test]
    fn hidden_folders_come_only_when_asked() {
        let t = tree();
        let l = list(None, true, Some(&t.home), &[]).unwrap();
        assert_eq!(names(&l), [".config", "app", "docs", "Zed"]);
        assert!(l.folders[0].hidden);
    }

    #[test]
    fn a_relative_path_is_read_from_home_and_the_parent_stops_at_home() {
        let t = tree();
        let l = list(Some("app"), false, Some(&t.home), &[]).unwrap();
        assert_eq!(names(&l), ["src"]);
        assert_eq!(l.parent.as_deref(), Some(t.home.to_str().unwrap()));
        let top = list(None, false, Some(&t.home), &[]).unwrap();
        assert_eq!(top.parent, None);
    }

    #[test]
    fn dotdot_and_absolute_paths_elsewhere_are_refused() {
        let t = tree();
        for p in [
            "..",
            "../outside",
            "app/../../outside",
            t.outside.to_str().unwrap(),
            "/",
            "/etc",
        ] {
            assert_eq!(
                list(Some(p), false, Some(&t.home), &[]),
                Err(FolderError::Outside),
                "{p}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_link_out_is_refused_and_left_out_of_the_listing() {
        let t = tree();
        std::os::unix::fs::symlink(&t.outside, t.home.join("escape")).unwrap();
        std::os::unix::fs::symlink(t.home.join("docs"), t.home.join("shortcut")).unwrap();
        assert_eq!(
            list(Some("escape"), false, Some(&t.home), &[]),
            Err(FolderError::Outside)
        );
        assert_eq!(
            list(Some("escape/secret"), false, Some(&t.home), &[]),
            Err(FolderError::Outside)
        );
        let l = list(None, false, Some(&t.home), &[]).unwrap();
        assert_eq!(names(&l), ["app", "docs", "shortcut", "Zed"]);
    }

    #[test]
    fn a_live_sessions_folder_is_allowed_though_it_is_outside_home() {
        let t = tree();
        let sessions = [t.outside.clone()];
        let l = list(t.outside.to_str(), false, Some(&t.home), &sessions).unwrap();
        assert_eq!(names(&l), ["secret"]);
        assert_eq!(l.parent, None);
        assert_eq!(
            list(Some("/etc"), false, Some(&t.home), &sessions),
            Err(FolderError::Outside)
        );
    }

    #[test]
    fn files_missing_paths_and_a_missing_home_are_told_apart() {
        let t = tree();
        assert_eq!(
            list(Some("notes.txt"), false, Some(&t.home), &[]),
            Err(FolderError::NotAFolder)
        );
        assert_eq!(
            list(Some("nope"), false, Some(&t.home), &[]),
            Err(FolderError::NotFound)
        );
        assert_eq!(list(None, false, None, &[]), Err(FolderError::NoHome));
    }

    #[test]
    fn a_leading_tilde_means_home_and_other_users_homes_are_refused() {
        let t = tree();
        let home = list(None, false, Some(&t.home), &[]).unwrap();
        for p in ["~", "~/", " ~ "] {
            assert_eq!(
                list(Some(p), false, Some(&t.home), &[]).unwrap(),
                home,
                "{p}"
            );
        }
        let app = list(Some("app"), false, Some(&t.home), &[]).unwrap();
        assert_eq!(list(Some("~/app"), false, Some(&t.home), &[]).unwrap(), app);
        assert_eq!(
            list(Some("~/nope"), false, Some(&t.home), &[]),
            Err(FolderError::NotFound)
        );
        for p in ["~root", "~root/x", "~/../outside", "~/app/../../outside"] {
            assert_eq!(
                list(Some(p), false, Some(&t.home), &[]),
                Err(FolderError::Outside),
                "{p}"
            );
        }
        // Only a leading tilde is special.
        assert_eq!(
            list(Some("app/~"), false, Some(&t.home), &[]),
            Err(FolderError::NotFound)
        );
    }

    #[test]
    fn more_than_the_cap_is_cut_and_said() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..MAX_FOLDERS + 3 {
            std::fs::create_dir(tmp.path().join(format!("d{i:04}"))).unwrap();
        }
        let l = list(None, false, Some(tmp.path()), &[]).unwrap();
        assert_eq!(l.folders.len(), MAX_FOLDERS);
        assert!(l.truncated);
        assert_eq!(l.folders[0].name, "d0000");
    }
}

#[cfg(test)]
mod remote_tests {
    use super::*;

    #[test]
    fn a_remote_is_shown_without_who_signs_in() {
        for (url, shown) in [
            ("git@github.com:acme/web.git", "github.com/acme/web"),
            ("https://github.com/acme/web.git", "github.com/acme/web"),
            (
                "https://sam:hunter2@gitlab.example/acme/web",
                "gitlab.example/acme/web",
            ),
            (
                "ssh://git@git.example:2222/acme/web.git/",
                "git.example:2222/acme/web",
            ),
        ] {
            assert_eq!(shown_remote(url).as_deref(), Some(shown), "{url}");
        }
        assert_eq!(shown_remote("/srv/git/web.git"), None);
        assert_eq!(shown_remote("https://host-only"), None);
    }

    /// Nothing a repository points to outside the home is read: not a
    /// `.git` link, an absolute or `..` `gitdir:`, nor a `commondir` that
    /// leads out.
    #[test]
    fn a_repository_that_points_outside_is_never_read() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = root.join("home");
        let outside = root.join("outside");
        std::fs::create_dir_all(outside.join("repo/.git/worktrees/x")).unwrap();
        std::fs::write(
            outside.join("repo/.git/config"),
            "[remote \"origin\"]\n\turl = git@secret.example:them/private\n",
        )
        .unwrap();
        let make = |name: &str| {
            let d = home.join(name);
            std::fs::create_dir_all(&d).unwrap();
            d
        };
        // A `.git` that is a link out.
        let linked = make("linked");
        std::os::unix::fs::symlink(outside.join("repo/.git"), linked.join(".git")).unwrap();
        // An absolute `gitdir:` out, and a `..` one.
        let absolute = make("absolute");
        std::fs::write(
            absolute.join(".git"),
            format!(
                "gitdir: {}\n",
                outside.join("repo/.git/worktrees/x").display()
            ),
        )
        .unwrap();
        let dotdot = make("dotdot");
        std::fs::write(dotdot.join(".git"), "gitdir: ../../outside/repo/.git\n").unwrap();
        // A worktree inside whose `commondir` leads out.
        let common = make("common");
        std::fs::create_dir_all(home.join("inner/.git/worktrees/c")).unwrap();
        std::fs::write(
            common.join(".git"),
            format!(
                "gitdir: {}\n",
                home.join("inner/.git/worktrees/c").display()
            ),
        )
        .unwrap();
        std::fs::write(
            home.join("inner/.git/worktrees/c/commondir"),
            format!("{}\n", outside.join("repo/.git").display()),
        )
        .unwrap();
        let listing = list(None, false, Some(&home), &[]).unwrap();
        for f in &listing.folders {
            assert_eq!(f.remote, None, "{} read outside", f.name);
        }
        for name in ["linked", "absolute", "dotdot", "common"] {
            let l = list(Some(&format!("~/{name}")), false, Some(&home), &[]).unwrap();
            assert_eq!((l.git, l.remote.as_deref()), (false, None), "{name}");
        }
    }

    #[test]
    fn a_listing_says_its_repository_and_remote_and_a_copy_s_too() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let repo = home.join("web");
        std::fs::create_dir_all(repo.join(".git/worktrees/fix")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(
            repo.join(".git/config"),
            "[core]\n\tbare = false\n[remote \"upstream\"]\n\turl = https://github.com/up/web\n[remote \"origin\"]\n\turl = git@github.com:acme/web.git\n",
        )
        .unwrap();
        // A separate copy: its `.git` is a file pointing back.
        let copy = repo.join(".worktrees/fix");
        std::fs::create_dir_all(&copy).unwrap();
        std::fs::write(
            copy.join(".git"),
            format!("gitdir: {}\n", repo.join(".git/worktrees/fix").display()),
        )
        .unwrap();
        std::fs::write(repo.join(".git/worktrees/fix/commondir"), "../..\n").unwrap();
        let top = list(None, false, Some(&home), &[]).unwrap();
        assert_eq!(
            top.folders[0].remote.as_deref(),
            Some("github.com/acme/web")
        );
        assert!(!top.git, "home itself is no repository");
        let inner = list(Some("~/web/src"), false, Some(&home), &[]).unwrap();
        assert!(inner.git);
        assert_eq!(inner.remote.as_deref(), Some("github.com/acme/web"));
        let c = list(Some("~/web/.worktrees/fix"), false, Some(&home), &[]).unwrap();
        assert_eq!(c.remote.as_deref(), Some("github.com/acme/web"));
    }
}
