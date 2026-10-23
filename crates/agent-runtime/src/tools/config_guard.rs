//! Writes an agent's tools make inside their roots, kept off the files that
//! configure the agent ([`workforce_common::config_files`]).
//!
//! The tools run as the hub, which may write those files, so the refusal is
//! decided here rather than by the filesystem. It holds against a tree the
//! agent uid rearranges while the write is under way: the entry is opened
//! through a handle on its directory, never by path again, and what was opened
//! is compared with the configuration files themselves. That also catches a
//! hard link to one of them, and a name that a case-insensitive filesystem
//! folds onto one.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Component, Path};

use workforce_common::config_files;

use super::paths::{resolve_real, CanonicalRoots};

/// Why a write did not happen.
#[derive(Debug)]
pub enum WriteError {
    /// The target configures the agent.
    Config,
    Io(io::Error),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config => f.write_str(config_files::WRITE_REFUSED),
            Self::Io(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for WriteError {}

impl From<io::Error> for WriteError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Whether `real`, a location [`CanonicalRoots::resolve`] returned, is or lies
/// beneath one of the agent's configuration files.
pub fn is_config(roots: &CanonicalRoots, real: &Path) -> bool {
    roots
        .workspace()
        .and_then(|workspace| real.strip_prefix(workspace).ok())
        .and_then(|rest| rest.components().next())
        .is_some_and(|first| match first {
            Component::Normal(name) => config_files::is_agent_config_name(name),
            _ => false,
        })
}

/// Whether `path`, wherever the links along it lead, is or lies beneath the
/// configuration of the agent whose workspace is `workspace`: for a
/// destination the hub creates, moves or clones into by a name.
pub fn reaches_config(workspace: &Path, path: &Path) -> bool {
    let roots = CanonicalRoots::new(std::slice::from_ref(&workspace.to_path_buf()));
    is_config(&roots, &resolve_real(path))
}

/// Refuse `real` when it is one of the agent's configuration files.
pub fn check(roots: &CanonicalRoots, real: &Path) -> Result<(), WriteError> {
    match is_config(roots, real) {
        true => Err(WriteError::Config),
        false => Ok(()),
    }
}

/// Create `dir` and its missing parents.
pub fn create_dirs(roots: &CanonicalRoots, dir: &Path) -> Result<(), WriteError> {
    check(roots, dir)?;
    std::fs::create_dir_all(dir).map_err(WriteError::Io)
}

/// Create or replace the file at `real` with `content`.
pub fn write(roots: &CanonicalRoots, real: &Path, content: &[u8]) -> Result<(), WriteError> {
    write_from(roots, real, &mut &content[..])
}

/// Create or replace the file at `real` with everything `content` yields.
pub fn write_from(
    roots: &CanonicalRoots,
    real: &Path,
    content: &mut dyn io::Read,
) -> Result<(), WriteError> {
    check(roots, real)?;
    imp::write(roots, real, content, Mode::Replace)
}

/// Create the file at `real` with `content`, failing when it already exists.
pub fn create_new(roots: &CanonicalRoots, real: &Path, content: &[u8]) -> Result<(), WriteError> {
    check(roots, real)?;
    imp::write(roots, real, &mut &content[..], Mode::CreateNew)
}

/// Add `content` to the end of the file at `real`, creating it when absent.
pub fn append(roots: &CanonicalRoots, real: &Path, content: &[u8]) -> Result<(), WriteError> {
    check(roots, real)?;
    imp::write(roots, real, &mut &content[..], Mode::Append)
}

/// How a write treats a file already there.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Replace,
    CreateNew,
    Append,
}

/// Move the file at `source`, which the agent does not name, to `real`.
/// Fails without moving anything when the two are on different filesystems.
pub fn rename_into(roots: &CanonicalRoots, source: &Path, real: &Path) -> Result<(), WriteError> {
    check(roots, real)?;
    imp::rename_into(roots, source, real)
}

/// Remove the file at `real`.
pub fn remove(roots: &CanonicalRoots, real: &Path) -> Result<(), WriteError> {
    check(roots, real)?;
    imp::remove(roots, real)
}

fn fill(mut file: File, content: &mut dyn io::Read, mode: Mode) -> io::Result<()> {
    if mode != Mode::Append {
        file.set_len(0)?;
    }
    io::copy(content, &mut file)?;
    file.flush()
}

#[cfg(unix)]
mod imp {
    use std::ffi::{CString, OsStr};
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::Path;

    use workforce_common::config_files;

    use super::{fill, CanonicalRoots, Mode, WriteError};

    /// A file's identity on disk, whatever name reaches it.
    #[derive(Clone, Copy, PartialEq, Eq)]
    struct FileId {
        dev: u64,
        ino: u64,
    }

    impl FileId {
        fn of(meta: &std::fs::Metadata) -> Self {
            Self {
                dev: meta.dev(),
                ino: meta.ino(),
            }
        }
    }

    /// The directory holding `real`, opened, and the entry's name in it.
    struct Entry {
        dir: File,
        name: CString,
    }

    impl Entry {
        /// Refused when the directory actually opened, wherever a link swapped
        /// in since `real` was resolved led, holds configuration there.
        fn of(roots: &CanonicalRoots, real: &Path) -> Result<Self, WriteError> {
            let (Some(parent), Some(name)) = (real.parent(), real.file_name()) else {
                return Err(WriteError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} names no file", real.display()),
                )));
            };
            let dir = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY)
                .open(parent)?;
            let opened = opened_path(&dir, parent)?.join(name);
            if !roots.holds(&opened) {
                return Err(WriteError::Io(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{} led outside the allowed directories", real.display()),
                )));
            }
            super::check(roots, &opened)?;
            Ok(Self {
                dir,
                name: c_name(name)?,
            })
        }

        /// The entry is reached through the opened directory, never by path,
        /// and a link in its place is not followed.
        fn open(&self, flags: libc::c_int) -> io::Result<File> {
            // SAFETY: `dir` is an open directory and `name` a NUL-terminated
            // name; on success the descriptor is owned by the returned File.
            let fd = unsafe {
                libc::openat(
                    self.dir.as_raw_fd(),
                    self.name.as_ptr(),
                    flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    0o666 as libc::c_uint,
                )
            };
            match fd {
                -1 => Err(io::Error::last_os_error()),
                fd => Ok(unsafe { File::from_raw_fd(fd) }),
            }
        }

        fn id(&self) -> io::Result<FileId> {
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: `stat` is written in full when fstatat succeeds, and
            // only read then.
            let stat = unsafe {
                match libc::fstatat(
                    self.dir.as_raw_fd(),
                    self.name.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                ) {
                    0 => stat.assume_init(),
                    _ => return Err(io::Error::last_os_error()),
                }
            };
            #[allow(clippy::unnecessary_cast)]
            Ok(FileId {
                dev: stat.st_dev as u64,
                ino: stat.st_ino as u64,
            })
        }

        fn unlink(&self) -> io::Result<()> {
            // SAFETY: as for `open`.
            match unsafe { libc::unlinkat(self.dir.as_raw_fd(), self.name.as_ptr(), 0) } {
                0 => Ok(()),
                _ => Err(io::Error::last_os_error()),
            }
        }
    }

    /// Where the open directory `dir` sits now.
    #[cfg(target_os = "linux")]
    fn opened_path(dir: &File, _parent: &Path) -> io::Result<std::path::PathBuf> {
        std::fs::read_link(format!("/proc/self/fd/{}", dir.as_raw_fd()))
    }

    #[cfg(target_os = "macos")]
    fn opened_path(dir: &File, _parent: &Path) -> io::Result<std::path::PathBuf> {
        use std::os::unix::ffi::OsStringExt;
        let mut buf = vec![0u8; libc::PATH_MAX as usize];
        // SAFETY: F_GETPATH writes at most PATH_MAX bytes, NUL-terminated,
        // into `buf`, which is that long.
        match unsafe { libc::fcntl(dir.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) } {
            -1 => Err(io::Error::last_os_error()),
            _ => {
                let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                buf.truncate(len);
                Ok(std::ffi::OsString::from_vec(buf).into())
            }
        }
    }

    /// Where nothing reports an open directory's location, the path it was
    /// opened by; the inode comparison still holds.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn opened_path(_dir: &File, parent: &Path) -> io::Result<std::path::PathBuf> {
        Ok(parent.to_path_buf())
    }

    fn c_name(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes()).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
    }

    /// Whether `id` is one of the agent's configuration files.
    fn configures(roots: &CanonicalRoots, id: FileId) -> bool {
        roots.workspace().is_some_and(|workspace| {
            config_files::agent_config_files()
                .filter_map(|name| std::fs::metadata(workspace.join(name)).ok())
                .any(|meta| FileId::of(&meta) == id)
        })
    }

    pub(super) fn write(
        roots: &CanonicalRoots,
        real: &Path,
        content: &mut dyn io::Read,
        mode: Mode,
    ) -> Result<(), WriteError> {
        let entry = Entry::of(roots, real)?;
        let created = entry.open(libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL);
        let existing = match mode {
            Mode::Append => libc::O_WRONLY | libc::O_APPEND,
            _ => libc::O_WRONLY,
        };
        let (file, created) = match created {
            Ok(file) => (file, true),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && mode != Mode::CreateNew => {
                (entry.open(existing)?, false)
            }
            Err(e) => return Err(e.into()),
        };
        if configures(roots, FileId::of(&file.metadata()?)) {
            if created {
                entry.unlink()?;
            }
            return Err(WriteError::Config);
        }
        fill(file, content, mode).map_err(WriteError::Io)
    }

    pub(super) fn rename_into(
        roots: &CanonicalRoots,
        source: &Path,
        real: &Path,
    ) -> Result<(), WriteError> {
        let entry = Entry::of(roots, real)?;
        match entry.id() {
            Ok(id) if configures(roots, id) => return Err(WriteError::Config),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let source = c_name(source.as_os_str())?;
        // SAFETY: both names are NUL-terminated and `dir` is open.
        match unsafe {
            libc::renameat(
                libc::AT_FDCWD,
                source.as_ptr(),
                entry.dir.as_raw_fd(),
                entry.name.as_ptr(),
            )
        } {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error().into()),
        }
    }

    pub(super) fn remove(roots: &CanonicalRoots, real: &Path) -> Result<(), WriteError> {
        let entry = Entry::of(roots, real)?;
        if configures(roots, entry.id()?) {
            return Err(WriteError::Config);
        }
        entry.unlink().map_err(WriteError::Io)
    }
}

#[cfg(not(unix))]
mod imp {
    use std::path::Path;

    use super::{fill, CanonicalRoots, Mode, WriteError};

    pub(super) fn write(
        _roots: &CanonicalRoots,
        real: &Path,
        content: &mut dyn std::io::Read,
        mode: Mode,
    ) -> Result<(), WriteError> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .append(mode == Mode::Append)
            .create(mode != Mode::CreateNew)
            .create_new(mode == Mode::CreateNew)
            .open(real)?;
        fill(file, content, mode).map_err(WriteError::Io)
    }

    pub(super) fn rename_into(
        _roots: &CanonicalRoots,
        source: &Path,
        real: &Path,
    ) -> Result<(), WriteError> {
        std::fs::rename(source, real).map_err(WriteError::Io)
    }

    pub(super) fn remove(_roots: &CanonicalRoots, real: &Path) -> Result<(), WriteError> {
        std::fs::remove_file(real).map_err(WriteError::Io)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::PathBuf;

    use super::*;

    struct Workspace {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        roots: CanonicalRoots,
    }

    fn workspace() -> Workspace {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(root.join("config.toml"), "[access]\nmode = \"allowlist\"\n").unwrap();
        std::fs::write(root.join("SOUL.md"), "soul").unwrap();
        let roots = CanonicalRoots::new(std::slice::from_ref(&root));
        Workspace {
            _tmp: tmp,
            root,
            roots,
        }
    }

    fn real(ws: &Workspace, raw: &str) -> PathBuf {
        ws.roots.resolve(&ws.root.join(raw)).unwrap()
    }

    fn refused(result: Result<(), WriteError>) -> bool {
        matches!(result, Err(WriteError::Config))
    }

    #[test]
    fn every_configuration_file_and_directory_is_refused() {
        let ws = workspace();
        for raw in config_files::agent_config_files()
            .map(str::to_string)
            .chain(
                config_files::AGENT_CONFIG_DIRS
                    .iter()
                    .map(|dir| format!("{dir}/planted.md")),
            )
            .chain(["learning/nested/plan.md".to_string()])
            .chain([".local/jobs/x/manifest.json".to_string()])
        {
            let target = real(&ws, &raw);
            assert!(
                refused(write(&ws.roots, &target, b"x")),
                "{raw} was written"
            );
            assert!(refused(create_new(&ws.roots, &target, b"x")), "{raw}");
            assert!(refused(create_dirs(&ws.roots, &target)), "{raw}");
        }
        assert!(refused(remove(&ws.roots, &real(&ws, "config.toml"))));
        assert!(ws.root.join("config.toml").is_file());
        assert_eq!(
            std::fs::read_to_string(ws.root.join("SOUL.md")).unwrap(),
            "soul"
        );
    }

    #[test]
    fn a_dot_dot_spelling_of_a_configuration_file_is_refused() {
        let ws = workspace();
        std::fs::create_dir(ws.root.join("notes")).unwrap();
        let target = real(&ws, "notes/../config.toml");
        assert!(refused(write(&ws.roots, &target, b"x")));
    }

    #[test]
    fn a_symlink_to_a_configuration_file_is_refused() {
        let ws = workspace();
        std::os::unix::fs::symlink(ws.root.join("config.toml"), ws.root.join("settings")).unwrap();
        let target = real(&ws, "settings");
        assert!(refused(write(&ws.roots, &target, b"x")));
    }

    #[test]
    fn a_hard_link_to_a_configuration_file_is_refused() {
        let ws = workspace();
        std::fs::create_dir(ws.root.join("notes")).unwrap();
        std::fs::hard_link(ws.root.join("config.toml"), ws.root.join("notes/copy")).unwrap();
        let target = real(&ws, "notes/copy");
        assert!(refused(write(&ws.roots, &target, b"x")));
        assert!(refused(remove(&ws.roots, &target)));
        assert!(std::fs::read_to_string(ws.root.join("config.toml"))
            .unwrap()
            .contains("allowlist"));
    }

    /// The agent uid rearranges the tree between the check and the write: a
    /// directory the path was resolved through becomes a link to the root.
    #[test]
    fn a_directory_swapped_for_a_link_after_the_check_is_refused() {
        let ws = workspace();
        std::fs::create_dir(ws.root.join("drafts")).unwrap();
        let target = real(&ws, "drafts/config.toml");
        assert!(check(&ws.roots, &target).is_ok());
        std::fs::remove_dir(ws.root.join("drafts")).unwrap();
        std::os::unix::fs::symlink(&ws.root, ws.root.join("drafts")).unwrap();

        assert!(refused(write(
            &ws.roots,
            &target,
            b"[access]\nmode = \"open\"\n"
        )));
        assert!(refused(remove(&ws.roots, &target)));
        assert!(std::fs::read_to_string(ws.root.join("config.toml"))
            .unwrap()
            .contains("allowlist"));
    }

    /// The same race into a directory of configuration, where no existing
    /// file's identity gives it away: only where the directory really is does.
    #[test]
    fn a_directory_swapped_for_a_link_into_a_configuration_directory_is_refused() {
        let ws = workspace();
        std::fs::create_dir(ws.root.join("learning")).unwrap();
        std::fs::create_dir(ws.root.join("drafts")).unwrap();
        let target = real(&ws, "drafts/plan.md");
        std::fs::remove_dir(ws.root.join("drafts")).unwrap();
        std::os::unix::fs::symlink(ws.root.join("learning"), ws.root.join("drafts")).unwrap();

        assert!(refused(write(
            &ws.roots,
            &target,
            b"run_as_identity: admin"
        )));
        assert!(!ws.root.join("learning/plan.md").exists());
    }

    #[test]
    fn a_directory_swapped_for_a_link_out_of_the_roots_is_refused() {
        let ws = workspace();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(ws.root.join("drafts")).unwrap();
        let target = real(&ws, "drafts/out.txt");
        std::fs::remove_dir(ws.root.join("drafts")).unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.root.join("drafts")).unwrap();

        assert!(matches!(
            write(&ws.roots, &target, b"x"),
            Err(WriteError::Io(e)) if e.kind() == io::ErrorKind::PermissionDenied
        ));
        assert!(!outside.path().join("out.txt").exists());
    }

    #[test]
    fn the_agents_own_files_are_written_replaced_and_removed() {
        let ws = workspace();
        let note = real(&ws, "memory/2026-10-05.md");
        create_dirs(&ws.roots, note.parent().unwrap()).unwrap();
        create_new(&ws.roots, &note, b"first").unwrap();
        assert!(matches!(
            create_new(&ws.roots, &note, b"again"),
            Err(WriteError::Io(e)) if e.kind() == io::ErrorKind::AlreadyExists
        ));
        write(&ws.roots, &note, b"second, and longer").unwrap();
        write(&ws.roots, &note, b"third").unwrap();
        assert_eq!(std::fs::read_to_string(&note).unwrap(), "third");
        let report = real(&ws, "report.md");
        write(&ws.roots, &report, b"done").unwrap();
        remove(&ws.roots, &report).unwrap();
        assert!(!report.exists());
    }

    #[test]
    fn an_append_adds_to_the_agents_file_and_never_reaches_configuration() {
        let ws = workspace();
        let log = real(&ws, "memory/compaction/today.md");
        create_dirs(&ws.roots, log.parent().unwrap()).unwrap();
        append(&ws.roots, &log, b"one\n").unwrap();
        append(&ws.roots, &log, b"two\n").unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "one\ntwo\n");

        let planted = ws.root.join("memory/compaction/planted.md");
        std::os::unix::fs::symlink("../../SOUL.md", &planted).unwrap();
        assert!(append(&ws.roots, &real(&ws, "memory/compaction/planted.md"), b"x").is_err());
        assert!(append(&ws.roots, &planted, b"x").is_err());
        assert_eq!(
            std::fs::read_to_string(ws.root.join("SOUL.md")).unwrap(),
            "soul"
        );
    }

    #[test]
    fn a_destination_reached_through_a_link_into_configuration_is_known() {
        let ws = workspace();
        std::fs::create_dir(ws.root.join("learning")).unwrap();
        std::os::unix::fs::symlink(&ws.root, ws.root.join("repos")).unwrap();
        for raw in [
            "learning",
            "learning/x",
            "repos/learning/x",
            "repos/config.toml",
        ] {
            assert!(reaches_config(&ws.root, &ws.root.join(raw)), "{raw}");
        }
        for raw in ["notes", "repos/org/app", "memory/learning"] {
            assert!(!reaches_config(&ws.root, &ws.root.join(raw)), "{raw}");
        }
    }

    #[test]
    fn a_configuration_name_below_the_root_is_the_agents_own() {
        let ws = workspace();
        let nested = real(&ws, "repos/app/config.toml");
        create_dirs(&ws.roots, nested.parent().unwrap()).unwrap();
        write(&ws.roots, &nested, b"[package]").unwrap();
        assert_eq!(std::fs::read_to_string(&nested).unwrap(), "[package]");
    }
}
