//! A stopped session's process, held so that what is sent to it never
//! reaches another process that came to use its number.
//!
//! On Linux it is held by a pidfd, opened while the hub still held the
//! session's PTY (its exit not seen). A pidfd names that one process for
//! good: once it has ended, it never names another. Each signal is sent only
//! while it says the process is still there. Then the number, which is also
//! its process group's (it leads its PTY's session), can't have been given
//! to another process. The whole group is signalled, so the tool shells and
//! dev servers it started go too. Elsewhere, without pidfds, the process is
//! asked by number whether it is still there, just before each signal.

use std::os::fd::OwnedFd;

pub struct Leader {
    pid: libc::pid_t,
    fd: Option<OwnedFd>,
}

impl Leader {
    /// The process `pid` that leads its own session (as a PTY's child does),
    /// or `None` when it doesn't, or isn't there: never a stranger.
    pub fn of(pid: u32) -> Option<Self> {
        let pid = libc::pid_t::try_from(pid).ok().filter(|p| *p > 1)?;
        let fd = open_pidfd(pid);
        // SAFETY: getsid has no preconditions.
        if unsafe { libc::getsid(pid) } != pid {
            return None;
        }
        let leader = Self { pid, fd };
        leader.alive().then_some(leader)
    }

    /// Whether the process is still there (a zombie not yet reaped counts:
    /// its number is still its own).
    pub fn alive(&self) -> bool {
        match &self.fd {
            Some(fd) => send_by_pidfd(fd, 0),
            // SAFETY: kill with no signal only asks.
            None => (unsafe { libc::kill(self.pid, 0) }) == 0,
        }
    }

    /// `signal` to the process and its group, while it is still there.
    /// Whether it was sent.
    pub fn signal_group(&self, signal: libc::c_int) -> bool {
        if !self.alive() {
            return false;
        }
        // SAFETY: killpg has no preconditions; the group is the leader's,
        // which is still there.
        unsafe { libc::killpg(self.pid, signal) == 0 }
    }
}

#[cfg(target_os = "linux")]
fn open_pidfd(pid: libc::pid_t) -> Option<OwnedFd> {
    use std::os::fd::FromRawFd;
    // SAFETY: pidfd_open takes a number and flags, and returns a new
    // descriptor or -1.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    // SAFETY: a descriptor just opened, owned here alone.
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

#[cfg(not(target_os = "linux"))]
fn open_pidfd(_pid: libc::pid_t) -> Option<OwnedFd> {
    None
}

#[cfg(target_os = "linux")]
fn send_by_pidfd(fd: &OwnedFd, signal: libc::c_int) -> bool {
    use std::os::fd::AsRawFd;
    // SAFETY: a valid pidfd, a signal number, no siginfo, no flags.
    unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        ) == 0
    }
}

#[cfg(not(target_os = "linux"))]
fn send_by_pidfd(_fd: &OwnedFd, _signal: libc::c_int) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    #[test]
    fn a_leader_is_signalled_with_its_group_and_never_once_it_has_ended() {
        // A session leader with a child of its own in its group.
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 300 & echo $!; wait"])
            .stdout(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        // `process_group` makes a group, not a session: not a PTY's child.
        assert!(Leader::of(child.id()).is_none(), "not a session leader");
        let _ = unsafe { libc::killpg(child.id() as i32, libc::SIGKILL) };
        child.wait().unwrap();

        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 300 & echo $!; wait"])
            .stdout(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null())
            .spawn_session()
            .unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.stdout.take().unwrap()),
            &mut line,
        )
        .unwrap();
        let below: i32 = line.trim().parse().unwrap();
        let leader = Leader::of(child.id()).expect("a session leader");
        assert!(leader.signal_group(libc::SIGKILL));
        child.wait().unwrap();
        // Its child went with it.
        let gone = (0..100).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            // SAFETY: kill with no signal only asks.
            (unsafe { libc::kill(below, 0) }) != 0
        });
        assert!(gone, "the group's other process is still there");
        // Ended and reaped: nothing more is sent.
        assert!(!leader.alive());
        assert!(!leader.signal_group(libc::SIGKILL));
    }

    trait SpawnSession {
        fn spawn_session(&mut self) -> std::io::Result<std::process::Child>;
    }

    impl SpawnSession for std::process::Command {
        fn spawn_session(&mut self) -> std::io::Result<std::process::Child> {
            // SAFETY: setsid is async-signal-safe.
            unsafe {
                self.pre_exec(|| {
                    if libc::setsid() < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                })
            }
            .spawn()
        }
    }
}
