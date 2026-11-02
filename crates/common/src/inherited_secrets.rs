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
//! The payload is one JSON object of variable names to string values, e.g.
//! `{"WORKFORCE_VAULT_KEY": "…"}`. Names have the shape of environment
//! variable names, so `${NAME}` in `workforce.toml` resolves against them
//! first ([`var`]); everything else still reads the environment, and with no
//! [`FD_ENV`] nothing changes.
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

    /// Reads a payload from `fd` to its end and closes it.
    #[cfg(unix)]
    pub fn read(fd: std::os::fd::OwnedFd) -> Result<Self> {
        use std::io::Read;
        let mut bytes = Vec::new();
        // One byte over the cap, so an oversized payload is told apart from
        // one exactly at it.
        std::fs::File::from(fd)
            .take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| eyre::eyre!("reading the inherited secrets: {e}"))?;
        Self::parse(&bytes)
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

/// `[A-Z_][A-Z0-9_]*`, the shape `${NAME}` expansion accepts.
fn is_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

static INHERITED: OnceLock<InheritedSecrets> = OnceLock::new();

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
    let secrets = read_named(&raw.to_string_lossy())?;
    let count = secrets.0.len();
    if INHERITED.set(secrets).is_err() {
        bail!("the inherited secrets were already read");
    }
    Ok(count)
}

#[cfg(unix)]
fn read_named(raw: &str) -> Result<InheritedSecrets> {
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
    InheritedSecrets::read(owned)
}

#[cfg(not(unix))]
fn read_named(_raw: &str) -> Result<InheritedSecrets> {
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Write;

    fn through_pipe(payload: &[u8]) -> Result<InheritedSecrets> {
        let (reader, mut writer) = std::io::pipe().unwrap();
        writer.write_all(payload).unwrap();
        drop(writer);
        InheritedSecrets::read(reader.into())
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
            .write_all(br#"{"WORKFORCE_GATEWAY_TOKEN":"gt-fixture"}"#)
            .unwrap();
        drop(writer);
        let fd = std::os::fd::OwnedFd::from(reader).into_raw_fd();
        let secrets = read_named(&fd.to_string()).unwrap();
        assert_eq!(secrets.get("WORKFORCE_GATEWAY_TOKEN"), Some("gt-fixture"));
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
        let big = format!(r#"{{"BIG":"{}"}}"#, "x".repeat(MAX_BYTES));
        // Written from a thread: the pipe's buffer is smaller than the payload.
        let (reader, mut writer) = std::io::pipe().unwrap();
        let w = std::thread::spawn(move || {
            let _ = writer.write_all(big.as_bytes());
        });
        let err = InheritedSecrets::read(reader.into()).unwrap_err();
        assert!(err.to_string().contains("bytes"), "{err}");
        w.join().unwrap();
    }

    #[test]
    fn a_standard_stream_or_closed_descriptor_is_refused() {
        assert!(read_named("1").is_err());
        assert!(read_named("not-a-number").is_err());
        // A high number nothing in the test process has open.
        assert!(read_named("987654").is_err());
    }
}
