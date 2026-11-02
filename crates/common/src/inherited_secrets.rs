//! Secrets a supervisor hands the hub on an inherited file descriptor
//! instead of in its environment.
//!
//! A process's starting environment is readable by every other process of
//! the same user (`KERN_PROCARGS2` on macOS, `/proc/<pid>/environ` on Linux),
//! and whatever the hub starts inherits it. So a supervisor that runs the
//! hub on someone's own computer (Desk Helper) writes the hub's secrets to a
//! pipe instead, and names the pipe's descriptor in [`FD_ENV`]. The hub
//! reads it once, at the top of `main` before any thread exists
//! ([`take_from_env`]), closes it, removes [`FD_ENV`], and keeps the secrets
//! in this process only.
//!
//! The descriptor must be a pipe. The payload is its length (4 bytes,
//! big-endian), then one JSON object of variable names to string values,
//! e.g. `{"WORKFORCE_VAULT_KEY": "…"}`. Names have the shape of environment
//! variable names. The few base-config fields a supervisor fills with them
//! (`crate::config_expand::INHERITED_SECRET_FIELDS`) and the hub's own key
//! reads resolve against them first ([`var`]); nothing else does, and with
//! no [`FD_ENV`] nothing changes.
//!
//! Values never reach a log or an error: errors name a variable or a byte
//! position, never what was read.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use eyre::{bail, Result};

use crate::secret::Secret;

/// The variable naming the descriptor the secrets are read from.
pub const FD_ENV: &str = "WORKFORCE_SECRETS_FD";

/// The most a payload may hold: a handful of keys and tokens fit many times
/// over, and a wrong descriptor can't make the hub read without bound.
pub const MAX_BYTES: usize = 64 * 1024;
/// The most secrets one payload may carry.
pub const MAX_SECRETS: usize = 64;
/// How long the hub waits for its supervisor's payload before it gives up.
pub const READ_WITHIN: std::time::Duration = std::time::Duration::from_secs(10);

/// Secrets read from a descriptor, by variable name. `Debug` shows the
/// names only.
#[derive(Default)]
pub struct InheritedSecrets(BTreeMap<String, Secret<String>>);

impl std::fmt::Debug for InheritedSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.0.keys()).finish()
    }
}

impl InheritedSecrets {
    /// Parses a payload: one JSON object of names to non-empty strings.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_BYTES {
            bail!("the inherited secrets are over {MAX_BYTES} bytes");
        }
        // serde_json's messages can quote what they read, so only the
        // position is kept.
        let raw: BTreeMap<String, String> = serde_json::from_slice(bytes).map_err(|e| {
            eyre::eyre!(
                "the inherited secrets aren't a JSON object of strings (line {}, column {})",
                e.line(),
                e.column()
            )
        })?;
        if raw.len() > MAX_SECRETS {
            bail!(
                "the inherited secrets hold {} entries, more than the {MAX_SECRETS} allowed",
                raw.len()
            );
        }
        let mut secrets = BTreeMap::new();
        for (name, value) in raw {
            if !is_var_name(&name) {
                bail!("inherited secret name {name:?} is not [A-Z_][A-Z0-9_]*");
            }
            if value.is_empty() {
                bail!("inherited secret {name} is empty");
            }
            secrets.insert(name, Secret::new(value));
        }
        Ok(Self(secrets))
    }

    /// Reads a framed payload from the pipe `fd` and closes it, giving up
    /// after [`READ_WITHIN`].
    #[cfg(unix)]
    pub fn read(fd: std::os::fd::OwnedFd) -> Result<Self> {
        Self::read_within(fd, READ_WITHIN)
    }

    /// [`Self::read`] with its own deadline. `fd` must be a pipe. The
    /// payload's length comes first (4 bytes, big-endian), so the read ends
    /// once it is in, whoever else still holds the write end; a supervisor
    /// that never writes is an error at the deadline, never a hang.
    #[cfg(unix)]
    pub fn read_within(fd: std::os::fd::OwnedFd, within: std::time::Duration) -> Result<Self> {
        use std::os::fd::AsRawFd;
        // SAFETY: `fstat` fills a `stat` this function owns, for an open
        // descriptor it owns.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } == -1 {
            bail!(
                "the inherited secrets' descriptor can't be read: {}",
                std::io::Error::last_os_error()
            );
        }
        if st.st_mode & libc::S_IFMT != libc::S_IFIFO {
            bail!("the inherited secrets' descriptor isn't a pipe");
        }
        let deadline = std::time::Instant::now() + within;
        let mut header = [0u8; 4];
        read_exact_by(&fd, &mut header, deadline, within)?;
        let len = u32::from_be_bytes(header) as usize;
        if len > MAX_BYTES {
            bail!("the inherited secrets are over {MAX_BYTES} bytes");
        }
        let mut body = vec![0u8; len];
        read_exact_by(&fd, &mut body, deadline, within)?;
        Self::parse(&body)
    }

    /// The secret named `name`.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(|s| s.expose_secret().as_str())
    }

    /// The names held, in order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Fills `buf` from `fd`, waiting at most until `deadline`.
#[cfg(unix)]
fn read_exact_by(
    fd: &std::os::fd::OwnedFd,
    buf: &mut [u8],
    deadline: std::time::Instant,
    within: std::time::Duration,
) -> Result<()> {
    use std::os::fd::AsRawFd;
    let mut filled = 0;
    while filled < buf.len() {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            bail!("the inherited secrets didn't arrive within {within:?}");
        }
        let mut pfd = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = libc::c_int::try_from(left.as_millis().max(1)).unwrap_or(libc::c_int::MAX);
        // SAFETY: one `pollfd` this function owns.
        let ready = unsafe { libc::poll(&mut pfd, 1, ms) };
        if ready == -1 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            bail!("waiting for the inherited secrets: {e}");
        }
        if ready == 0 {
            continue;
        }
        let rest = &mut buf[filled..];
        // SAFETY: reads at most `rest.len()` bytes into `rest`.
        let n = unsafe { libc::read(fd.as_raw_fd(), rest.as_mut_ptr().cast(), rest.len()) };
        if n == -1 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            bail!("reading the inherited secrets: {e}");
        }
        if n == 0 {
            bail!("the inherited secrets ended before their length");
        }
        filled += n as usize;
    }
    Ok(())
}

/// `[A-Z_][A-Z0-9_]*`, the shape `${NAME}` expansion accepts.
fn is_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

static INHERITED: OnceLock<InheritedSecrets> = OnceLock::new();
/// The descriptor the inherited secrets were read from.
static TAKEN_FROM: OnceLock<i32> = OnceLock::new();

/// The descriptor [`take_from_env`] read the secrets from, if it read any.
/// A supervisor also names it on the command line (`--secrets-fd`), which a
/// hub too old to read it refuses at once.
pub fn taken_from() -> Option<i32> {
    TAKEN_FROM.get().copied()
}

/// Checks `--secrets-fd` against what [`take_from_env`] read: the flag names
/// the descriptor [`FD_ENV`] named.
pub fn check_flag(flag: Option<i32>) -> Result<()> {
    match (flag, taken_from()) {
        (None, _) => Ok(()),
        (Some(fd), Some(read)) if fd == read => Ok(()),
        (Some(fd), Some(read)) => {
            bail!("--secrets-fd {fd} doesn't match {FD_ENV}, which named descriptor {read}")
        }
        (Some(fd), None) => bail!("--secrets-fd {fd} was given without {FD_ENV}"),
    }
}

/// Reads the secrets from the descriptor [`FD_ENV`] names, if it names one,
/// closes the descriptor and removes [`FD_ENV`], so nothing the hub starts
/// inherits either. Returns how many secrets were read.
///
/// Call it first thing in `main`, before any thread exists: it changes the
/// process's environment. A named descriptor that can't be read, or holds a
/// malformed payload, is an error rather than a hub that starts without its
/// secrets.
pub fn take_from_env() -> Result<usize> {
    let Some(raw) = std::env::var_os(FD_ENV) else {
        return Ok(0);
    };
    std::env::remove_var(FD_ENV);
    let (fd, secrets) = read_named(&raw.to_string_lossy())?;
    let count = secrets.0.len();
    let _ = TAKEN_FROM.set(fd);
    if INHERITED.set(secrets).is_err() {
        bail!("the inherited secrets were already read");
    }
    Ok(count)
}

#[cfg(unix)]
fn read_named(raw: &str) -> Result<(i32, InheritedSecrets)> {
    use std::os::fd::{FromRawFd, OwnedFd, RawFd};
    let fd: RawFd = raw
        .trim()
        .parse()
        .map_err(|_| eyre::eyre!("{FD_ENV} is not a file descriptor number"))?;
    // 0–2 are the standard streams: never a secrets pipe.
    if fd < 3 {
        bail!("{FD_ENV} names descriptor {fd}, one of the standard streams");
    }
    // SAFETY: `fcntl(F_GETFD)` only asks whether the descriptor is open.
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
        bail!("{FD_ENV} names descriptor {fd}, which isn't open");
    }
    // SAFETY: the descriptor is open, was handed to this process for this
    // alone, and nothing else in the process owns it this early in `main`.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    Ok((fd, InheritedSecrets::read(owned)?))
}

#[cfg(not(unix))]
fn read_named(_raw: &str) -> Result<(i32, InheritedSecrets)> {
    bail!("{FD_ENV} is only supported on Unix")
}

/// The variable `name`: from the inherited secrets when they hold it, else
/// from the environment.
pub fn var(name: &str) -> Option<String> {
    var_in(INHERITED.get(), name)
}

/// [`var`] against `secrets` rather than the process's own.
pub(crate) fn var_in(secrets: Option<&InheritedSecrets>, name: &str) -> Option<String> {
    secrets
        .and_then(|s| s.get(name))
        .map(str::to_owned)
        .or_else(|| std::env::var(name).ok())
}

/// Installs a fixed set of inherited secrets for this test process, as
/// `take_from_env` would. Every caller gets the same fixture.
#[cfg(test)]
pub(crate) fn install_test_fixture() -> &'static InheritedSecrets {
    INHERITED.get_or_init(|| {
        InheritedSecrets::parse(
            br#"{"INHERITED_T_PG":"pg-fixture","INHERITED_T_KEY":"key-fixture","INHERITED_T_TOKEN":"token-fixture"}"#,
        )
        .expect("the fixture parses")
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Write;

    /// `payload` as a supervisor frames it: its length, then itself.
    fn framed(payload: &[u8]) -> Vec<u8> {
        let mut out = (payload.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(payload);
        out
    }

    fn through_pipe(payload: &[u8]) -> Result<InheritedSecrets> {
        let (reader, mut writer) = std::io::pipe().unwrap();
        writer.write_all(&framed(payload)).unwrap();
        drop(writer);
        InheritedSecrets::read(reader.into())
    }

    /// A copy of the write end held open elsewhere (another child that
    /// inherited it) can't make the hub wait: it reads the framed length,
    /// not to EOF.
    #[test]
    fn a_write_end_left_open_does_not_hang_the_read() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        writer
            .write_all(&framed(br#"{"DESK_MEMORY_KEY":"mk-fixture"}"#))
            .unwrap();
        let started = std::time::Instant::now();
        let secrets =
            InheritedSecrets::read_within(reader.into(), std::time::Duration::from_secs(5))
                .unwrap();
        assert_eq!(secrets.get("DESK_MEMORY_KEY"), Some("mk-fixture"));
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        drop(writer);
    }

    /// A supervisor that never writes gives a clear error at the deadline,
    /// never a hub that hangs at start.
    #[test]
    fn a_silent_pipe_times_out() {
        let (reader, writer) = std::io::pipe().unwrap();
        let started = std::time::Instant::now();
        let err =
            InheritedSecrets::read_within(reader.into(), std::time::Duration::from_millis(200))
                .unwrap_err()
                .to_string();
        assert!(err.contains("within"), "{err}");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        drop(writer);
    }

    /// A pipe that ends before its framed length is an error.
    #[test]
    fn a_short_payload_is_refused() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        let mut bytes = framed(br#"{"DESK_MEMORY_KEY":"mk-fixture"}"#);
        bytes.truncate(bytes.len() - 3);
        writer.write_all(&bytes).unwrap();
        drop(writer);
        let err = InheritedSecrets::read(reader.into())
            .unwrap_err()
            .to_string();
        assert!(!err.contains("mk-fixture"), "{err}");
    }

    /// Only a pipe is accepted: a regular file (or anything else a wrong
    /// number might name) is refused.
    #[test]
    fn a_descriptor_that_is_not_a_pipe_is_refused() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&framed(br#"{"A":"b"}"#)).unwrap();
        use std::io::Seek;
        file.rewind().unwrap();
        let err = InheritedSecrets::read(file.into()).unwrap_err().to_string();
        assert!(err.contains("pipe"), "{err}");
    }

    #[test]
    fn the_hub_reads_its_secrets_from_the_pipe() {
        let secrets = through_pipe(
            br#"{"DESK_POSTGRES_PASSWORD":"pw-fixture","DESK_MEMORY_KEY":"mk-fixture"}"#,
        )
        .unwrap();
        assert_eq!(secrets.get("DESK_POSTGRES_PASSWORD"), Some("pw-fixture"));
        assert_eq!(secrets.get("DESK_MEMORY_KEY"), Some("mk-fixture"));
        assert_eq!(secrets.get("PATH"), None);
        assert_eq!(
            secrets.names().collect::<Vec<_>>(),
            ["DESK_MEMORY_KEY", "DESK_POSTGRES_PASSWORD"]
        );
    }

    /// What `take_from_env` does with the number in `WORKFORCE_SECRETS_FD`:
    /// the descriptor it names is read, as a supervisor leaves it.
    #[test]
    fn the_descriptor_named_by_number_is_read() {
        use std::os::fd::IntoRawFd;
        let (reader, mut writer) = std::io::pipe().unwrap();
        writer
            .write_all(&framed(br#"{"WORKFORCE_GATEWAY_TOKEN":"gt-fixture"}"#))
            .unwrap();
        drop(writer);
        let fd = std::os::fd::OwnedFd::from(reader).into_raw_fd();
        let (read_from, secrets) = read_named(&fd.to_string()).unwrap();
        assert_eq!(read_from, fd);
        assert_eq!(secrets.get("WORKFORCE_GATEWAY_TOKEN"), Some("gt-fixture"));
    }

    /// `--secrets-fd` without the descriptor having been read is refused:
    /// the flag never stands in for the secrets.
    #[test]
    fn the_flag_needs_the_secrets_it_names() {
        assert!(check_flag(None).is_ok());
        if taken_from().is_none() {
            let err = check_flag(Some(3)).unwrap_err().to_string();
            assert!(err.contains(FD_ENV), "{err}");
        }
    }

    #[test]
    fn debug_shows_names_only() {
        let secrets = through_pipe(br#"{"WORKFORCE_VAULT_KEY":"vk-fixture"}"#).unwrap();
        let shown = format!("{secrets:?}");
        assert!(shown.contains("WORKFORCE_VAULT_KEY"));
        assert!(!shown.contains("vk-fixture"));
    }

    #[test]
    fn malformed_payloads_are_refused_without_repeating_them() {
        for bad in [
            &br#"{"DESK_MEMORY_KEY": 12345678}"#[..],
            br#"["mk-fixture"]"#,
            br#"{"DESK_MEMORY_KEY":"mk-fixture""#,
            b"DESK_MEMORY_KEY=mk-fixture",
        ] {
            let err = through_pipe(bad).unwrap_err().to_string();
            assert!(!err.contains("mk-fixture"), "{err}");
            assert!(!err.contains("12345678"), "{err}");
        }
    }

    #[test]
    fn names_must_be_variable_names_and_values_non_empty() {
        assert!(through_pipe(br#"{"lower":"v"}"#).is_err());
        assert!(through_pipe(br#"{"HAS=EQUALS":"v"}"#).is_err());
        let err = through_pipe(br#"{"EMPTY":""}"#).unwrap_err().to_string();
        assert!(err.contains("EMPTY"));
    }

    #[test]
    fn an_oversized_payload_is_refused() {
        // The length alone is refused, before anything else is read.
        let (reader, mut writer) = std::io::pipe().unwrap();
        writer
            .write_all(&((MAX_BYTES as u32) + 1).to_be_bytes())
            .unwrap();
        let err = InheritedSecrets::read(reader.into()).unwrap_err();
        assert!(err.to_string().contains("bytes"), "{err}");
        drop(writer);
    }

    #[test]
    fn a_standard_stream_or_closed_descriptor_is_refused() {
        assert!(read_named("1").is_err());
        assert!(read_named("not-a-number").is_err());
        // A high number nothing in the test process has open.
        assert!(read_named("987654").is_err());
    }
}
