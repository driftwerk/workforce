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

/// Opens the FIFO as descriptor 3, removes it, and runs the command.
const SESSION: &str = r#"exec 3<"$1" || exit 126; rm -f -- "$1"; shift; exec "$@""#;
/// Moves standard input to descriptor 3, and runs the command.
const ONE_OFF: &str = r#"exec 3<&0 0</dev/null; exec "$@""#;

/// A FIFO waiting for a session's shell, in a folder of its own.
pub struct TokenFifo {
    dir: PathBuf,
    path: PathBuf,
}

impl TokenFifo {
    /// A new FIFO under `run_dir`, only this user can open.
    pub fn new(run_dir: &Path) -> Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        let dir = run_dir.join(format!("t-{}", Uuid::new_v4().simple()));
        std::fs::DirBuilder::new()
            .recursive(false)
            .mode(0o700)
            .create(&dir)
            .or_else(|e| match e.kind() {
                std::io::ErrorKind::NotFound => {
                    std::fs::create_dir_all(run_dir)?;
                    std::fs::DirBuilder::new().mode(0o700).create(&dir)
                }
                _ => Err(e),
            })
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
                    break f.write_all(&line).context("handing a session its token");
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
