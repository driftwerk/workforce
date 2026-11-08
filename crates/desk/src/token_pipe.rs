//! Handing a Claude Code session its account's token on a descriptor, never
//! in an environment.
//!
//! Claude Code reads its OAuth token from the descriptor named in
//! `CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR` (read once, up to a newline or
//! the end of the stream, and kept in memory; checked against Claude Code
//! 2.1.317, whose `auth status` signs in by a token on a pipe). So the only
//! variable a session gets is the descriptor's number, `3`.
//!
//! A session runs on a PTY, in the hub or in the PTY holder, which can't hand
//! a child an extra descriptor. So the session starts as a one-line shell
//! that opens a FIFO as descriptor 3 and execs Claude Code in its place
//! (same process, same PTY). The hub writes the token into the FIFO once
//! the shell has opened it, then removes the FIFO and its folder (`0700`,
//! named at random). Once Claude Code has read it the pipe is empty: what the
//! session runs inherits descriptor 3 at its end, and no token.
//!
//! Claude Code (2.1.318) doesn't read descriptor 3 itself: it opens
//! `/proc/self/fd/3` (or `/dev/fd/3`). On Linux that opens the FIFO afresh,
//! which waits for a writer: one the hub had already closed left Claude
//! Code waiting there for good, drawing nothing. So the hub keeps its end
//! open until the pipe is read empty, and only then closes it, which ends
//! the stream for Claude Code.
//!
//! A one-off run (`claude auth status`) gets it the same way through its
//! standard input, which the shell moves to descriptor 3.

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eyre::{Context, Result};
use uuid::Uuid;

use crate::engine_accounts::AccountToken;
use crate::pty::Spawn;

/// The variable naming the descriptor Claude Code reads its token from.
pub const FD_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR";
/// The descriptor: the first after the standard streams.
pub const FD: &str = "3";
/// How long the hub waits for a session's shell to open its FIFO.
const OPEN_WITHIN: Duration = Duration::from_secs(30);
/// How long the hub keeps the token's pipe open for Claude Code to read it,
/// at most: it stops sooner when the session has gone (holding descriptor 3
/// for its life, so its end is the reader's).
const READ_WITHIN: Duration = Duration::from_secs(10 * 60);
/// How often the hub looks whether the token was read.
const READ_EVERY: Duration = Duration::from_millis(20);

/// Opens the FIFO as descriptor 3, removes it, and runs the command.
const SESSION: &str = r#"exec 3<"$1" || exit 126; rm -f -- "$1"; shift; exec "$@""#;
/// Moves standard input to descriptor 3, and runs the command.
const ONE_OFF: &str = r#"exec 3<&0 0</dev/null; exec "$@""#;

/// Makes `run_dir` (where a session's token passes, and a sign-in's browser
/// is made) this user's alone: made `0700` when it isn't there; refused when
/// it is a link, not a folder, or someone else's; closed to everyone else
/// when it is this user's but open to them. Only then can no one else swap
/// a folder made in it before what goes into that folder is written.
pub fn private_run_dir(run_dir: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    if let Some(parent) = run_dir.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("making {}", parent.display()))?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(run_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("making {}", run_dir.display())),
    }
    let meta = std::fs::symlink_metadata(run_dir)
        .with_context(|| format!("reading {}", run_dir.display()))?;
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    if !meta.file_type().is_dir() || meta.uid() != me {
        eyre::bail!("{} isn't a folder of this user's own", run_dir.display());
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(run_dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("closing {} to others", run_dir.display()))?;
    }
    Ok(())
}

/// A FIFO waiting for a session's shell, in a folder of its own.
pub struct TokenFifo {
    dir: PathBuf,
    path: PathBuf,
}

impl TokenFifo {
    /// A new FIFO under `run_dir`, only this user can open.
    pub fn new(run_dir: &Path) -> Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        private_run_dir(run_dir)?;
        let dir = run_dir.join(format!("t-{}", Uuid::new_v4().simple()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("making {}", dir.display()))?;
        let path = dir.join("token");
        let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        // SAFETY: `c` is a valid NUL-terminated path.
        if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
            let e = std::io::Error::last_os_error();
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e).context("making a FIFO for a session's token");
        }
        Ok(Self { dir, path })
    }

    /// `spec` started through the shell that opens this FIFO as descriptor
    /// 3, with Claude Code told to read its token there.
    pub fn wrap(&self, spec: Spawn) -> Spawn {
        let mut args = vec![
            "-c".to_string(),
            SESSION.to_string(),
            "desk-session".to_string(),
            self.path.to_string_lossy().into_owned(),
            spec.program.to_string_lossy().into_owned(),
        ];
        args.extend(spec.args);
        let mut env = spec.env;
        env.insert(FD_ENV.into(), FD.into());
        Spawn {
            program: PathBuf::from("/bin/sh"),
            args,
            cwd: spec.cwd,
            env,
        }
    }

    /// Writes `token` once the session's shell has opened the FIFO, then
    /// removes it. Blocking: run it off the runtime's threads.
    pub fn hand(self, token: &AccountToken) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let started = Instant::now();
        let result = loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&self.path)
            {
                Ok(mut f) => {
                    let mut line = token.expose().as_bytes().to_vec();
                    line.push(b'\n');
                    if let Err(e) = f.write_all(&line) {
                        break Err(e).context("handing a session its token");
                    }
                    // Open until read: Claude Code opens the FIFO again.
                    break match wait_read(&f, READ_WITHIN) {
                        Read::All => Ok(()),
                        Read::ReaderGone => {
                            Err(eyre::eyre!("the session ended before it read its token"))
                        }
                        Read::TimedOut => Err(eyre::eyre!(
                            "the session hadn't read its token after {} minutes",
                            READ_WITHIN.as_secs() / 60
                        )),
                    };
                }
                // No reader yet.
                Err(e) if e.raw_os_error() == Some(libc::ENXIO) => {
                    if started.elapsed() > OPEN_WITHIN {
                        break Err(eyre::eyre!("the session never opened its token's pipe"));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => break Err(e).context("opening a session's token pipe"),
            }
        };
        let _ = std::fs::remove_dir_all(&self.dir);
        result
    }
}

impl Drop for TokenFifo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// How many bytes are still in the pipe `end` is an end of; none when the
/// system won't say, and then the hub's end closes at once, as it did
/// before. (macOS needs no wait: there `/dev/fd/3` duplicates the open
/// descriptor rather than opening the FIFO again.)
fn unread(end: &impl std::os::fd::AsRawFd) -> usize {
    let mut n: libc::c_int = 0;
    // SAFETY: FIONREAD writes one int through a valid pointer.
    if unsafe { libc::ioctl(end.as_raw_fd(), libc::FIONREAD, &mut n) } == 0 {
        n.max(0) as usize
    } else {
        0
    }
}

/// Whether the pipe whose write end is `end` has no reader left (`POLLERR`).
fn reader_gone(end: &impl std::os::fd::AsRawFd) -> bool {
    let mut p = libc::pollfd {
        fd: end.as_raw_fd(),
        events: 0,
        revents: 0,
    };
    // SAFETY: one valid pollfd, and no waiting.
    let ready = unsafe { libc::poll(&mut p, 1, 0) };
    ready == 1 && p.revents & libc::POLLERR != 0
}

/// What came of waiting for what was written to a pipe to be read.
#[derive(Debug, PartialEq, Eq)]
enum Read {
    /// All of it.
    All,
    /// Not all, and no one is left to read it.
    ReaderGone,
    /// Not all, and the wait ran out.
    TimedOut,
}

/// Waits, at most `within`, until what was written to the pipe whose write
/// end is `end` has all been read, or until no reader is left. Blocking.
fn wait_read(end: &impl std::os::fd::AsRawFd, within: Duration) -> Read {
    let started = Instant::now();
    loop {
        if unread(end) == 0 {
            return Read::All;
        }
        if reader_gone(end) {
            return Read::ReaderGone;
        }
        if started.elapsed() >= within {
            return Read::TimedOut;
        }
        std::thread::sleep(READ_EVERY);
    }
}

/// `program args` as a one-off run whose standard input carries the token
/// to descriptor 3: `/bin/sh` and its arguments.
pub fn one_off(program: &Path, args: &[&str]) -> (PathBuf, Vec<String>) {
    let mut all = vec![
        "-c".to_string(),
        ONE_OFF.to_string(),
        "desk-check".to_string(),
        program.to_string_lossy().into_owned(),
    ];
    all.extend(args.iter().map(|a| a.to_string()));
    (PathBuf::from("/bin/sh"), all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn a_token_never_passes_through_a_folder_someone_else_could_swap() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        // Made new: this user's alone.
        let new = dir.path().join("run");
        drop(TokenFifo::new(&new).unwrap());
        assert_eq!(
            std::fs::metadata(&new).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // This user's but open to others: closed first.
        let open = dir.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        drop(TokenFifo::new(&open).unwrap());
        assert_eq!(
            std::fs::metadata(&open).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // A link to a folder (another user could make one where it goes), or
        // a folder of someone else's (the system's `/`), is never used.
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&open, &link).unwrap();
        assert!(TokenFifo::new(&link).is_err());
        assert!(TokenFifo::new(Path::new("/")).is_err());
        assert!(private_run_dir(Path::new("/")).is_err());
    }

    #[tokio::test]
    async fn a_session_reads_its_token_on_descriptor_3_and_its_environment_and_children_never_see_it(
    ) {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let script = format!(
            "printf 'fd=%s\\n' \"$CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR\" > {o}; \
             read -r t <&3; printf 'read=%s\\n' \"$t\" >> {o}; \
             env >> {o}; sh -c 'cat <&3' > {o}.again; echo done >> {o}",
            o = out.display()
        );
        let fifo = TokenFifo::new(&dir.path().join("run")).unwrap();
        let folder = fifo.dir.clone();
        let spec = fifo.wrap(Spawn {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), script],
            cwd: dir.path().to_path_buf(),
            env: BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]),
        });
        let token = AccountToken::new("sk-ant-oat01-pipe-0123456789abcdefghij").unwrap();
        let mut child = std::process::Command::new(&spec.program)
            .args(&spec.args)
            .env_clear()
            .envs(&spec.env)
            .current_dir(&spec.cwd)
            .spawn()
            .unwrap();
        let t = token.clone();
        tokio::task::spawn_blocking(move || fifo.hand(&t))
            .await
            .unwrap()
            .unwrap();
        child.wait().unwrap();
        let said = std::fs::read_to_string(&out).unwrap();
        assert!(said.contains("fd=3"), "{said}");
        assert!(said.contains(&format!("read={}", token.expose())), "{said}");
        let env_part: String = said.lines().filter(|l| !l.starts_with("read=")).collect();
        assert!(
            !env_part.contains(token.expose()),
            "the environment holds the token"
        );
        assert_eq!(
            std::fs::read_to_string(format!("{}.again", out.display())).unwrap(),
            ""
        );
        assert!(!folder.exists(), "the FIFO's folder is removed");
    }

    /// Whether `child` ends within `within`; it is killed when it doesn't.
    fn ended_within(child: &mut std::process::Child, within: Duration) -> bool {
        let started = Instant::now();
        while started.elapsed() < within {
            if child.try_wait().unwrap().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // The whole group: what it runs too, not left waiting behind it.
        // SAFETY: killpg has no preconditions.
        unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };
        let _ = child.wait();
        false
    }

    #[tokio::test]
    async fn a_session_that_opens_descriptor_3_again_reads_its_token_and_its_end() {
        use std::os::unix::process::CommandExt;
        // As Claude Code 2.1.318 does: `/dev/fd/3` opened afresh, after the
        // hub has written, which on Linux waits for a writer.
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let script = format!("sleep 0.3; cat /dev/fd/3 > {o}", o = out.display());
        let fifo = TokenFifo::new(&dir.path().join("run")).unwrap();
        let spec = fifo.wrap(Spawn {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), script],
            cwd: dir.path().to_path_buf(),
            env: BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]),
        });
        let token = AccountToken::new("sk-ant-oat01-pipe-0123456789abcdefghij").unwrap();
        let mut child = std::process::Command::new(&spec.program)
            .args(&spec.args)
            .env_clear()
            .envs(&spec.env)
            .current_dir(&spec.cwd)
            .process_group(0)
            .spawn()
            .unwrap();
        let t = token.clone();
        let handed = tokio::task::spawn_blocking(move || fifo.hand(&t));
        assert!(
            ended_within(&mut child, Duration::from_secs(10)),
            "the session waited for good on its token's pipe"
        );
        handed.await.unwrap().unwrap();
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            format!("{}\n", token.expose())
        );
    }

    /// A pipe's two ends.
    fn pipe() -> (std::fs::File, std::fs::File) {
        use std::os::fd::FromRawFd;
        let mut fds = [0; 2];
        // SAFETY: two valid ints for pipe to fill.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: both were just opened, and each is owned once.
        unsafe {
            (
                std::fs::File::from_raw_fd(fds[0]),
                std::fs::File::from_raw_fd(fds[1]),
            )
        }
    }

    #[test]
    fn waiting_for_a_read_says_whether_it_came_and_stops_when_no_reader_is_left() {
        use std::io::{Read as _, Write};
        let (mut rd, mut wr) = pipe();
        wr.write_all(b"tok\n").unwrap();
        // Not read: the wait runs out, and says so.
        assert_eq!(wait_read(&wr, Duration::from_millis(100)), Read::TimedOut);
        let mut buf = [0; 4];
        rd.read_exact(&mut buf).unwrap();
        assert_eq!(wait_read(&wr, Duration::from_secs(10)), Read::All);
        // Written, and the reader gone: said at once, not after the wait.
        wr.write_all(b"tok\n").unwrap();
        drop(rd);
        let started = Instant::now();
        assert_eq!(wait_read(&wr, Duration::from_secs(10)), Read::ReaderGone);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn a_session_that_ends_before_reading_its_token_is_said_to_at_once() {
        use std::os::unix::process::CommandExt;
        let dir = tempfile::tempdir().unwrap();
        let fifo = TokenFifo::new(&dir.path().join("run")).unwrap();
        let spec = fifo.wrap(Spawn {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), "sleep 0.3".into()],
            cwd: dir.path().to_path_buf(),
            env: BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]),
        });
        let token = AccountToken::new("sk-ant-oat01-pipe-0123456789abcdefghij").unwrap();
        let mut child = std::process::Command::new(&spec.program)
            .args(&spec.args)
            .env_clear()
            .envs(&spec.env)
            .current_dir(&spec.cwd)
            .process_group(0)
            .spawn()
            .unwrap();
        let started = Instant::now();
        let handed = tokio::task::spawn_blocking(move || fifo.hand(&token))
            .await
            .unwrap();
        assert!(ended_within(&mut child, Duration::from_secs(10)));
        let e = handed.unwrap_err();
        assert!(e.to_string().contains("ended before it read"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_one_off_run_that_opens_descriptor_3_again_reads_its_token_and_its_end() {
        // A pipe, not a FIFO: opened again, it never waits for a writer, so
        // its input may close as soon as the token is written.
        use std::io::Write;
        use std::os::unix::process::CommandExt;
        let (program, args) = one_off(Path::new("/bin/sh"), &["-c", "sleep 0.3; cat /dev/fd/3"]);
        let mut child = std::process::Command::new(program)
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"tok\n").unwrap();
        drop(stdin);
        let mut stdout = child.stdout.take().unwrap();
        assert!(
            ended_within(&mut child, Duration::from_secs(10)),
            "the run waited for good"
        );
        let mut said = String::new();
        std::io::Read::read_to_string(&mut stdout, &mut said).unwrap();
        assert_eq!(said, "tok\n");
    }

    #[test]
    fn a_fifo_is_the_users_alone_and_goes_when_dropped() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fifo = TokenFifo::new(dir.path()).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode(&fifo.dir), mode(&fifo.path)), (0o700, 0o600));
        let folder = fifo.dir.clone();
        drop(fifo);
        assert!(!folder.exists());
    }

    #[test]
    fn a_one_off_run_moves_its_input_to_descriptor_3() {
        use std::io::Write;
        let (program, args) = one_off(Path::new("/bin/sh"), &["-c", "cat <&3"]);
        let mut child = std::process::Command::new(program)
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"tok\n").unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "tok\n");
    }
}
