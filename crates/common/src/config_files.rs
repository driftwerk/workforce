//! Files the hub reads as configuration or policy, kept out of the agent's
//! reach.
//!
//! They decide what an agent may do and see, for everyone who uses it, so they
//! change only through the API, where the owner/admin rule applies. Agent tools
//! never write them. The in-process tools refuse
//! (`workforce_agent::tools::config_guard`). Under privilege separation the
//! agent uid that runs every dispatched command cannot either: each file is
//! writable by its owner alone, and the directory holding it carries the sticky
//! bit, so a member of the shared group can neither rewrite, rename nor remove
//! an entry the hub made. It can still create one where none exists, which is
//! why a file that uid made is never read. Every other owner is trusted: an
//! operator copying files in as root or as their own user, or a volume whose
//! files a host account owns, is the install's own doing.

use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::Path;

/// The agent's own settings, layered over the install config.
pub const WORKSPACE_CONFIG: &str = "config.toml";

/// The identity files the system prompt is assembled from.
pub const IDENTITY_FILES: [&str; 6] = [
    "SOUL.md",
    "AGENTS.md",
    "IDENTITY.md",
    "TOOLS.md",
    "HEARTBEAT.md",
    "WELCOME.md",
];

/// The model-tier and pricing tables an agent workspace may carry.
pub const TABLE_FILES: [&str; 3] = [
    "vision_tiers.toml",
    "transcription_tiers.toml",
    "pricing.toml",
];

/// Directories in an agent workspace whose entries the hub acts on unattended,
/// some under the identity they name: heartbeat checks, learning plans,
/// background jobs and scheduled jobs. The hub keeps them closed to itself.
pub const AGENT_CONFIG_DIRS: [&str; 4] = ["heartbeats", "learning", ".local", "scheduler"];

/// The variable naming the user every command an agent dispatches runs as.
/// `workforce-init` sets it for the hub when it starts the runner as that
/// user; unset, commands run as the hub and no file is told apart by owner.
pub const RUNNER_USER_ENV: &str = "WORKFORCE_RUNNER_USER";

/// What an agent is told when it tries to write any of these.
pub const WRITE_REFUSED: &str =
    "An agent's configuration is changed in its settings, not by the agent.";

/// The configuration files at the root of an agent workspace.
pub fn agent_config_files() -> impl Iterator<Item = &'static str> {
    std::iter::once(WORKSPACE_CONFIG)
        .chain(IDENTITY_FILES)
        .chain(TABLE_FILES)
}

/// Whether `name`, directly inside an agent workspace, is configuration: one
/// of its files, or one of its directories.
pub fn is_agent_config_name(name: &OsStr) -> bool {
    agent_config_files()
        .chain(AGENT_CONFIG_DIRS)
        .any(|config| OsStr::new(config) == name)
}

/// The uid of the user [`RUNNER_USER_ENV`] names, `None` where it names none,
/// or the hub's own uid or root's: no command runs apart from the hub.
pub fn runner_uid() -> Option<u32> {
    static RUNNER: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    *RUNNER.get_or_init(|| {
        std::env::var(RUNNER_USER_ENV)
            .ok()
            .and_then(|user| imp::uid_of(&user))
            .filter(|&uid| uid != 0 && uid != imp::effective_uid())
    })
}

/// The text of the file at `path`, `None` when it is absent or the runner
/// made it. A file the agent uid planted where none existed is left out, with
/// a warning, exactly as though it were absent; one an older replica left
/// group-writable is closed before it is read.
pub fn read(path: &Path) -> io::Result<Option<String>> {
    read_unless_planted_by(path, runner_uid())
}

fn read_unless_planted_by(path: &Path, runner: Option<u32>) -> io::Result<Option<String>> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
        Ok(_) => {}
    }
    if planted(path, runner) {
        tracing::warn!(
            path = %path.display(),
            "Ignoring configuration an agent's own commands wrote; change it through the API"
        );
        return Ok(None);
    }
    if !std::fs::metadata(path).is_ok_and(|meta| meta.is_file()) {
        tracing::warn!(path = %path.display(), "Ignoring configuration that is not a file");
        return Ok(None);
    }
    close_file(path);
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        other => other.map(Some),
    }
}

/// Whether `name` names an entry inside a directory and nothing else: not
/// empty, not `.` or `..`, and free of separators. For identifiers a tool
/// joins onto a directory to make a path.
pub fn is_plain_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

/// Replace the file at `path` with `content`, atomically and writable by the
/// hub alone whatever the umask. The text goes to a temporary file created
/// exclusively under a fresh name beside it, so nothing planted in the
/// directory can redirect the write, and is then renamed into place.
pub fn replace(path: &Path, content: &[u8]) -> io::Result<()> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} names no file", path.display()),
        ));
    };
    let mut prefix = OsStr::new(".").to_os_string();
    prefix.push(name);
    prefix.push(".");
    let mut staged = tempfile::Builder::new().prefix(&prefix).tempfile_in(dir)?;
    staged.write_all(content)?;
    staged.as_file().sync_all()?;
    set_mode(staged.path(), CONFIG_MODE)?;
    staged.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Close an agent workspace's configuration files to the agent uid, and make
/// the workspace sticky. Never fails the agent's load: a position that cannot
/// be closed is logged.
pub fn close_agent_workspace(workspace: &Path) {
    agent_config_files().for_each(|name| close(&workspace.join(name)));
    make_sticky(workspace);
}

/// Make the file at `path` writable by its owner alone, when the hub made it.
pub fn close(path: &Path) {
    if std::fs::symlink_metadata(path).is_ok() && !planted(path, runner_uid()) {
        close_file(path);
    }
}

/// Take the write bits for group and others off everything the hub made in
/// `dir`, which stays readable: for a tree of configuration the agent uid may
/// read but must not change. Links are not followed.
pub fn close_tree(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    entries.flatten().for_each(|entry| {
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            close_tree(&path);
        }
    });
    close_entry(dir);
    if let Ok(entries) = std::fs::read_dir(dir) {
        entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .for_each(|entry| close_entry(&entry.path()));
    }
}

/// Set the sticky bit on `dir` when the hub owns it, so the agent uid can
/// rename or remove only the entries it made itself.
pub fn make_sticky(dir: &Path) {
    imp::make_sticky(dir);
}

fn close_file(path: &Path) {
    if std::fs::metadata(path).is_ok_and(|meta| meta.is_file()) {
        close_entry(path);
    }
}

fn close_entry(path: &Path) {
    if let Err(e) = imp::drop_write_bits(path) {
        tracing::warn!(path = %path.display(), error = %e, "Could not close configuration to the agent uid");
    }
}

fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    imp::set_mode(path, mode)
}

/// Whether the entry at `path`, the directory holding it or, for a link, its
/// target belongs to `runner`, so the agent's own commands could have put it
/// there. With no runner every command runs as the hub, and ownership tells
/// nothing apart.
fn planted(path: &Path, runner: Option<u32>) -> bool {
    runner.is_some_and(|runner| made_by(imp::owners(path), runner))
}

/// Whether any of `owners` is `runner`, or cannot be known.
fn made_by(mut owners: impl Iterator<Item = Option<u32>>, runner: u32) -> bool {
    owners.any(|owner| owner.is_none_or(|owner| owner == runner))
}

#[cfg(unix)]
const CONFIG_MODE: u32 = 0o644;

#[cfg(not(unix))]
const CONFIG_MODE: u32 = 0;

#[cfg(unix)]
mod imp {
    use std::io;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::Path;

    const GROUP_OTHER_WRITE: u32 = 0o022;
    const STICKY: u32 = 0o1000;

    pub(super) fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }

    pub(super) fn effective_uid() -> u32 {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }

    pub(super) fn uid_of(user: &str) -> Option<u32> {
        let name = std::ffi::CString::new(user).ok()?;
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut buf = vec![0 as libc::c_char; 16 * 1024];
        let mut found = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the call, `buf` is as long as
        // passed, and `entry` is read only when `found` says it was filled.
        let rc = unsafe {
            libc::getpwnam_r(
                name.as_ptr(),
                entry.as_mut_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
                &mut found,
            )
        };
        (rc == 0 && !found.is_null()).then(|| unsafe { entry.assume_init() }.pw_uid)
    }

    /// The owners of the directory holding `path`, of the entry and, through
    /// a link, of its target; `None` for one that cannot be read, a link
    /// leading nowhere included.
    pub(super) fn owners(path: &Path) -> impl Iterator<Item = Option<u32>> {
        let dir = path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        [
            std::fs::metadata(dir).ok().map(|meta| meta.uid()),
            std::fs::symlink_metadata(path).ok().map(|meta| meta.uid()),
            std::fs::metadata(path).ok().map(|meta| meta.uid()),
        ]
        .into_iter()
    }

    /// Only what the hub owns, and never through a link.
    pub(super) fn drop_write_bits(path: &Path) -> io::Result<()> {
        let meta = std::fs::symlink_metadata(path)?;
        let mode = meta.permissions().mode() & 0o7777;
        if meta.file_type().is_symlink()
            || meta.uid() != effective_uid()
            || mode & GROUP_OTHER_WRITE == 0
        {
            return Ok(());
        }
        set_mode(path, mode & !GROUP_OTHER_WRITE)
    }

    pub(super) fn make_sticky(dir: &Path) {
        let Ok(meta) = std::fs::metadata(dir) else {
            return;
        };
        let mode = meta.permissions().mode() & 0o7777;
        if mode & STICKY != 0 || meta.uid() != effective_uid() {
            return;
        }
        if let Err(e) = set_mode(dir, mode | STICKY) {
            tracing::warn!(path = %dir.display(), error = %e, "Could not make a directory holding configuration sticky");
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use std::io;
    use std::path::Path;

    pub(super) fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
        Ok(())
    }

    pub(super) fn effective_uid() -> u32 {
        0
    }

    pub(super) fn uid_of(_user: &str) -> Option<u32> {
        None
    }

    pub(super) fn owners(_path: &Path) -> impl Iterator<Item = Option<u32>> {
        std::iter::empty()
    }

    pub(super) fn drop_write_bits(_path: &Path) -> io::Result<()> {
        Ok(())
    }

    pub(super) fn make_sticky(_dir: &Path) {}
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    }

    fn widen(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn own_uid() -> u32 {
        imp::effective_uid()
    }

    /// The test owns every file it makes, so naming its own uid as the
    /// runner's stands for a file the agent uid made, and any other uid for
    /// one it did not.
    #[test]
    fn configuration_the_runner_made_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(WORKSPACE_CONFIG);
        std::fs::write(&path, "[scope]").unwrap();

        assert_eq!(
            read_unless_planted_by(&path, Some(own_uid())).unwrap(),
            None
        );
        assert_eq!(
            read_unless_planted_by(&path, Some(own_uid() + 1))
                .unwrap()
                .as_deref(),
            Some("[scope]")
        );
        assert_eq!(
            read_unless_planted_by(&path, None).unwrap().as_deref(),
            Some("[scope]")
        );
    }

    /// Rows are (directory owner, entry owner, link target owner, made by
    /// the runner). Only the runner's uid is distrusted: root, the hub and
    /// any other account, such as an operator copying files in, are not.
    #[test]
    fn only_the_runner_plants_configuration() {
        const HUB: u32 = 1000;
        const RUNNER: u32 = 1001;
        const OPERATOR: u32 = 501;
        for (dir, entry, target, want) in [
            (HUB, HUB, HUB, false),
            (HUB, 0, 0, false),
            (HUB, OPERATOR, OPERATOR, false),
            (0, OPERATOR, OPERATOR, false),
            (HUB, RUNNER, RUNNER, true),
            // A link the runner made, to a file the hub made.
            (HUB, RUNNER, HUB, true),
            // A link the hub made, to a file the runner made.
            (HUB, HUB, RUNNER, true),
            // The runner renamed the workspace away and made its own.
            (RUNNER, HUB, HUB, true),
        ] {
            assert_eq!(
                made_by([Some(dir), Some(entry), Some(target)].into_iter(), RUNNER),
                want,
                "dir {dir}, entry {entry}, target {target}"
            );
        }
        assert!(made_by([Some(HUB), Some(HUB), None].into_iter(), RUNNER));
    }

    #[test]
    fn a_link_leading_nowhere_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let dangling = dir.path().join("AGENTS.md");
        std::os::unix::fs::symlink(dir.path().join("missing.md"), &dangling).unwrap();
        for runner in [None, Some(own_uid() + 1)] {
            assert_eq!(read_unless_planted_by(&dangling, runner).unwrap(), None);
        }
    }

    #[test]
    fn the_runner_is_never_the_hub_or_root() {
        assert!(runner_uid().is_none_or(|uid| uid != 0 && uid != own_uid()));
    }

    #[test]
    fn a_replaced_file_is_the_owners_alone_whatever_the_umask() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(WORKSPACE_CONFIG);
        std::fs::write(&path, "old").unwrap();
        widen(&path, 0o664);

        replace(&path, b"new").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(mode(&path), 0o644);
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
        assert_eq!(left.len(), 1, "a staging file was left behind");
    }

    /// A link planted where the file will go is replaced, not written through.
    #[test]
    fn a_replace_never_writes_through_a_planted_link() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("vault.enc");
        std::fs::write(&victim, "ciphertext").unwrap();
        let path = dir.path().join("WELCOME.md");
        std::os::unix::fs::symlink(&victim, &path).unwrap();

        replace(&path, b"hello").unwrap();

        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "ciphertext");
        assert!(!std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn a_read_closes_a_group_writable_file_and_skips_what_is_not_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(WORKSPACE_CONFIG);
        std::fs::write(&path, "[scope]").unwrap();
        widen(&path, 0o664);

        assert_eq!(
            read_unless_planted_by(&path, None).unwrap().as_deref(),
            Some("[scope]")
        );
        assert_eq!(mode(&path), 0o644);

        let soul = dir.path().join("SOUL.md");
        std::fs::create_dir(&soul).unwrap();
        assert_eq!(read_unless_planted_by(&soul, None).unwrap(), None);
        assert_eq!(
            read_unless_planted_by(&dir.path().join("AGENTS.md"), None).unwrap(),
            None
        );
    }

    #[test]
    fn closing_a_workspace_closes_its_configuration_and_makes_it_sticky() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        widen(ws, 0o2775);
        for name in [WORKSPACE_CONFIG, "SOUL.md", "notes.md"] {
            std::fs::write(ws.join(name), "x").unwrap();
            widen(&ws.join(name), 0o664);
        }

        close_agent_workspace(ws);

        assert_eq!(mode(ws), 0o3775);
        assert_eq!(mode(&ws.join(WORKSPACE_CONFIG)), 0o644);
        assert_eq!(mode(&ws.join("SOUL.md")), 0o644);
        assert_eq!(mode(&ws.join("notes.md")), 0o664);
    }

    #[test]
    fn closing_a_tree_drops_write_for_others_and_follows_no_link() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let shared = outside.path().join("shared.txt");
        std::fs::write(&shared, "x").unwrap();
        widen(&shared, 0o666);
        let skills = dir.path().join("skills");
        std::fs::create_dir_all(skills.join("deploy/scripts")).unwrap();
        std::fs::write(skills.join("deploy/SKILL.md"), "x").unwrap();
        std::fs::write(skills.join("deploy/scripts/run.sh"), "x").unwrap();
        widen(&skills.join("deploy/scripts/run.sh"), 0o775);
        std::os::unix::fs::symlink(&shared, skills.join("deploy/link")).unwrap();
        for dir in [
            &skills,
            &skills.join("deploy"),
            &skills.join("deploy/scripts"),
        ] {
            widen(dir, 0o2775);
        }
        widen(&skills.join("deploy/SKILL.md"), 0o664);

        close_tree(&skills);

        assert_eq!(mode(&skills), 0o2755);
        assert_eq!(mode(&skills.join("deploy/scripts")), 0o2755);
        assert_eq!(mode(&skills.join("deploy/SKILL.md")), 0o644);
        assert_eq!(mode(&skills.join("deploy/scripts/run.sh")), 0o755);
        assert_eq!(mode(&shared), 0o666);
    }

    #[test]
    fn a_plain_name_stays_in_its_directory() {
        for name in ["session-1", "slack:C1:171.2", ".github", "a.b"] {
            assert!(is_plain_name(name), "{name}");
        }
        for name in ["", ".", "..", "../config", "a/b", "a\\b", "x\0y"] {
            assert!(!is_plain_name(name), "{name:?}");
        }
    }
}
