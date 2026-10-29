//! A process's working directory, read from the system: `proc_pidinfo` on
//! macOS (no shell integration needed, so a plain `cd` is seen), `/proc` on
//! Linux.

/// `pid`'s current working directory, when it can be read.
#[cfg(target_os = "macos")]
pub fn of_pid(pid: u32) -> Option<String> {
    let pid = libc::c_int::try_from(pid).ok()?;
    // SAFETY: `info` is a plain C struct the kernel fills; the size passed is
    // its size, and the result says how much was written.
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_vnodepathinfo>()).ok()?;
    let wrote = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            size,
        )
    };
    if wrote != size {
        return None;
    }
    // `vip_path` is a NUL-terminated MAXPATHLEN buffer, split for old rustc.
    let bytes: Vec<u8> = info
        .pvi_cdir
        .vip_path
        .iter()
        .flatten()
        .map(|&c| c as u8)
        .take_while(|&b| b != 0)
        .collect();
    (!bytes.is_empty())
        .then(|| String::from_utf8(bytes).ok())
        .flatten()
}

#[cfg(not(target_os = "macos"))]
pub fn of_pid(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_childs_working_directory_is_read_from_the_system() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("5")
            .current_dir(dir.path())
            .spawn()
            .unwrap();
        let got = of_pid(child.id());
        let _ = child.kill();
        let _ = child.wait();
        let want = dir.path().canonicalize().unwrap();
        assert_eq!(got.map(std::path::PathBuf::from), Some(want));
        assert_eq!(of_pid(u32::MAX), None);
    }
}
