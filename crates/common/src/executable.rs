//! Executables written whole, in one step, and only put in place once
//! nothing has them open for writing.

use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Writes `contents` to `path` as an executable (`0o755`). It is written
/// under a fresh name beside `path`, closed, and renamed into place once
/// no process has it open for writing: a program forked while it was
/// being written holds it open until that program starts, and starting it
/// meanwhile fails as busy (`ETXTBSY`). A run of the one there before
/// never finds it open for writing either, and a run of the new one never
/// finds it half written.
pub fn write(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
    let mut file = tempfile::Builder::new()
        .prefix(".executable-")
        .tempfile_in(dir.unwrap_or(Path::new(".")))?;
    file.write_all(contents.as_ref())?;
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o755))?;
    let (_, temp) = file.into_parts();
    no_writers(&temp)?;
    temp.persist(path).map_err(|e| e.error)
}

/// Waits until no process has `path` open for writing: a read lease is
/// granted only then (`fcntl(2)`). Where leases aren't kept, as on some
/// filesystems, it is taken as written.
#[cfg(target_os = "linux")]
fn no_writers(path: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let file = std::fs::File::open(path)?;
    let fd = file.as_raw_fd();
    let started = std::time::Instant::now();
    loop {
        // SAFETY: `fd` is open for as long as `file` is.
        if unsafe { libc::fcntl(fd, libc::F_SETLEASE, libc::F_RDLCK) } == 0 {
            // SAFETY: as above; the lease just taken is let go.
            unsafe { libc::fcntl(fd, libc::F_SETLEASE, libc::F_UNLCK) };
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EAGAIN) {
            return Ok(());
        }
        if started.elapsed() > std::time::Duration::from_secs(10) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "an executable stayed open for writing",
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[cfg(not(target_os = "linux"))]
fn no_writers(_: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn an_executable_rewritten_while_it_keeps_starting_is_never_busy_or_half_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cli");
        write(&path, "#!/bin/sh\necho 0\n").unwrap();
        let done = AtomicBool::new(false);
        let said = std::thread::scope(|s| {
            let runs = s.spawn(|| {
                let mut said = Vec::new();
                while !done.load(Ordering::Relaxed) {
                    let out = std::process::Command::new(&path).output().unwrap();
                    said.push(String::from_utf8(out.stdout).unwrap());
                }
                said
            });
            for n in 1..=300 {
                write(&path, format!("#!/bin/sh\necho {n}\n")).unwrap();
            }
            done.store(true, Ordering::Relaxed);
            runs.join().unwrap()
        });
        assert!(!said.is_empty());
        assert!(
            said.iter().all(|s| s.trim().parse::<u32>().is_ok()),
            "{said:?}"
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        let left = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(left, 1, "no temporary file left beside it");
    }
}
