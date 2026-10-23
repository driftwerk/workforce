//! Writing one structural edit: every file it changes, or none.
//!
//! Each file is written in place, as `code.edit` writes, so it keeps its
//! owner, mode and links. Every file is first opened for writing and held to
//! the text the edit was computed from, so a file that changed or cannot be
//! written refuses the whole edit before anything is touched; a write that
//! then fails puts back the files already written.
//!
//! A path is checked and then opened, and the agent's user can swap what is
//! at it in between. Every open here refuses a symlink in the same call that
//! opens the file, so the hub never reads or writes through one planted
//! after the check.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use eyre::{eyre, Result};

/// One file's text before and after an edit, at a path already confined to
/// the agent's roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: PathBuf,
    pub before: String,
    pub after: String,
}

/// Write every change, or none of them.
pub fn apply(changes: &[FileChange]) -> Result<()> {
    let mut files = changes.iter().map(open).collect::<Result<Vec<File>>>()?;
    for at in 0..changes.len() {
        if let Err(e) = overwrite(&mut files[at], &changes[at].after) {
            let stuck: Vec<String> = changes[..=at]
                .iter()
                .zip(&mut files)
                .filter_map(|(done, file)| {
                    overwrite(file, &done.before)
                        .err()
                        .map(|_| done.path.display().to_string())
                })
                .collect();
            let path = changes[at].path.display();
            return Err(match stuck.is_empty() {
                true => eyre!("{path}: {e}; nothing was changed"),
                false => eyre!(
                    "{path}: {e}; these files could not be put back: {}",
                    stuck.join(", ")
                ),
            });
        }
    }
    Ok(())
}

/// Open the regular file at `path` itself: never the target of a symlink
/// there, and never a pipe or device, which is not waited on either.
pub fn open_regular(path: &Path, write: bool) -> std::io::Result<File> {
    let not_regular = || std::io::Error::other("not a regular file");
    let mut options = File::options();
    options.read(true).write(write);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|e| match e.raw_os_error() {
        #[cfg(unix)]
        Some(libc::ELOOP) => not_regular(),
        _ => e,
    })?;
    match file.metadata()?.is_file() {
        true => Ok(file),
        false => Err(not_regular()),
    }
}

/// The text of the regular file at `path`, opened as [`open_regular`] opens.
pub fn read_regular(path: &Path) -> std::io::Result<String> {
    let mut text = String::new();
    open_regular(path, false)?.read_to_string(&mut text)?;
    Ok(text)
}

/// The file open for writing, still holding `change.before`.
fn open(change: &FileChange) -> Result<File> {
    let path = change.path.display();
    let mut file =
        open_regular(&change.path, true).map_err(|e| eyre!("{path}: {e}; nothing was changed"))?;
    let mut current = String::new();
    let unchanged = file
        .read_to_string(&mut current)
        .is_ok_and(|_| current == change.before);
    match unchanged {
        true => Ok(file),
        false => Err(eyre!(
            "{path} changed while the edit was being prepared; nothing was changed"
        )),
    }
}

fn overwrite(file: &mut File, text: &str) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(path: &Path, after: &str) -> FileChange {
        FileChange {
            path: path.to_path_buf(),
            before: std::fs::read_to_string(path).unwrap(),
            after: after.into(),
        }
    }

    #[test]
    fn every_file_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.rs"), dir.path().join("b.rs"));
        std::fs::write(&a, "fn a() {}").unwrap();
        std::fs::write(&b, "fn b() {}").unwrap();
        apply(&[change(&a, "fn x() {}"), change(&b, "fn y() {}")]).unwrap();
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "fn x() {}");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "fn y() {}");
    }

    #[test]
    fn a_file_changed_since_the_edit_was_computed_refuses_all_of_it() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.rs"), dir.path().join("b.rs"));
        std::fs::write(&a, "fn a() {}").unwrap();
        std::fs::write(&b, "fn b() {}").unwrap();
        let changes = [change(&a, "fn x() {}"), change(&b, "fn y() {}")];
        std::fs::write(&b, "fn b() { 1 }").unwrap();
        let err = apply(&changes).unwrap_err();
        assert!(err.to_string().contains("changed while"), "{err}");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "fn a() {}");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "fn b() { 1 }");
    }

    #[cfg(unix)]
    #[test]
    fn a_file_that_cannot_be_written_leaves_every_file_as_it_was() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.rs"), dir.path().join("b.rs"));
        std::fs::write(&a, "fn a() {}").unwrap();
        std::fs::write(&b, "fn b() {}").unwrap();
        std::fs::set_permissions(&b, std::fs::Permissions::from_mode(0o444)).unwrap();
        let err = apply(&[change(&a, "fn x() {}"), change(&b, "fn y() {}")]).unwrap_err();
        assert!(err.to_string().contains("nothing was changed"), "{err}");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "fn a() {}");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "fn b() {}");
    }

    /// Written in place: under privilege separation a file replaced by a
    /// new one would pass from the agent's user to the hub's.
    #[cfg(unix)]
    #[test]
    fn a_written_file_is_the_same_file_with_the_same_mode() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.rs");
        std::fs::write(&a, "fn a() { a_long_body_to_shrink(); }").unwrap();
        std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o664)).unwrap();
        let before = std::fs::metadata(&a).unwrap();
        apply(&[change(&a, "fn x() {}")]).unwrap();
        let after = std::fs::metadata(&a).unwrap();
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "fn x() {}");
        assert_eq!(after.ino(), before.ino());
        assert_eq!(after.uid(), before.uid());
        assert_eq!(after.permissions().mode() & 0o777, 0o664);
        let siblings = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(siblings, 1, "nothing is staged beside it");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_never_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.rs");
        std::fs::write(&target, "fn a() {}").unwrap();
        let link = dir.path().join("link.rs");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = apply(&[change(&link, "fn x() {}")]).unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "fn a() {}");
    }

    /// The guard is in the open itself, so a link swapped in after a path
    /// was validated is neither read through nor waited on.
    #[cfg(unix)]
    #[test]
    fn a_symlink_or_a_pipe_is_never_opened() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("private.jsonl");
        std::fs::write(&target, "secret").unwrap();
        let link = dir.path().join("lib.rs");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = read_regular(&link).unwrap_err();
        assert_eq!(err.to_string(), "not a regular file");
        assert_eq!(read_regular(&target).unwrap(), "secret");

        let pipe = dir.path().join("pipe.rs");
        let c_path = std::ffi::CString::new(pipe.to_str().unwrap()).unwrap();
        // SAFETY: mkfifo reads a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) }, 0);
        let err = read_regular(&pipe).unwrap_err();
        assert_eq!(err.to_string(), "not a regular file");
    }
}
