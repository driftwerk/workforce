//! Controlled environment for agent-invoked commands.
//!
//! The load-bearing security property of the privilege-separated executor is
//! that the child process is launched from a **clean** environment, never an
//! inherited copy of the hub's: inheriting it would hand an agent command the
//! vault decryption key, gateway/owner token, and JWT signing secret straight
//! through `env`/`printenv`. So the policy starts from nothing and copies
//! forward only an explicit allowlist (PATH/locale/toolchain dirs) plus the
//! agent-writable cache dirs the toolchains need to function.

use std::collections::BTreeMap;

/// Variables copied from the source environment when present. These carry no
/// secrets and are needed for commands to find binaries and behave with a sane
/// locale: `PATH` and the toolchain home dirs locate executables; the `LANG`/
/// `LC_*` family and `TZ` keep output stable; `HOME`/`CARGO_HOME`/the npm cache
/// point at agent-writable dirs (cargo/git/npm write their caches there).
const ALLOWED_VARS: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    "TERM",
    "TMPDIR",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "NPM_CONFIG_CACHE",
    "NODE_PATH",
    "GIT_CONFIG_GLOBAL",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

/// Caps on the hub-supplied environment. Generous for a credential set and a
/// handful of settings, small enough that a malformed request cannot make the
/// daemon allocate without bound.
pub const MAX_OVERRIDE_VARS: usize = 32;
pub const MAX_OVERRIDE_BYTES: usize = 128 * 1024;

/// Whether `name` is a POSIX-shaped environment variable name. Anything else —
/// lowercase, punctuation, an embedded `=` — is rejected rather than passed to
/// the child, where the shape is the only thing standing between a name and an
/// injected assignment.
fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Validate the hub-supplied environment before it reaches a child.
///
/// The daemon is the security boundary, so it checks rather than trusting the
/// hub to have checked. Errors name the offending variable but never its value.
pub fn validate_overrides(env: &[(String, String)]) -> Result<(), String> {
    if env.len() > MAX_OVERRIDE_VARS {
        return Err(format!(
            "environment carries {} variables, more than the {MAX_OVERRIDE_VARS} allowed",
            env.len()
        ));
    }
    let bytes: usize = env.iter().map(|(k, v)| k.len() + v.len()).sum();
    if bytes > MAX_OVERRIDE_BYTES {
        return Err(format!(
            "environment totals {bytes} bytes, more than the {MAX_OVERRIDE_BYTES} allowed"
        ));
    }
    env.iter()
        .find(|(name, _)| !is_valid_name(name))
        .map_or(Ok(()), |(name, _)| {
            Err(format!(
                "environment variable name {name:?} is not [A-Z_][A-Z0-9_]*"
            ))
        })
}

/// A fallback `PATH` when the source environment carries none, so the child can
/// still resolve `sh`, `git`, and the standard toolchain dirs.
const FALLBACK_PATH: &str = "/usr/local/cargo/bin:/usr/local/bin:/usr/bin:/bin";

/// Toolchain dirs that must always be on `PATH`, and the rustup home the
/// toolchain proxies resolve against. Injected unconditionally because the
/// runner launches under `setpriv --reset-env`, whose `PATH` lacks the cargo bin
/// and which carries no `RUSTUP_HOME` — copy-forward alone would leave `cargo`
/// unresolvable and toolchain-less in production.
const TOOLCHAIN_BIN_DIRS: &[&str] = &["/usr/local/cargo/bin", "/usr/local/bin"];
const RUSTUP_HOME_DEFAULT: &str = "/usr/local/rustup";

fn ensure_dirs_on_path(path: &str, dirs: &[&str]) -> String {
    let present: Vec<&str> = path.split(':').filter(|s| !s.is_empty()).collect();
    let missing: Vec<&str> = dirs
        .iter()
        .copied()
        .filter(|d| !present.contains(d))
        .collect();
    match (missing.is_empty(), path.is_empty()) {
        (true, _) => path.to_string(),
        (false, true) => missing.join(":"),
        (false, false) => format!("{}:{}", missing.join(":"), path),
    }
}

/// Builds a clean environment for a runner-spawned child from an explicit
/// source map. Taking the source map as a parameter (rather than reading
/// `std::env` directly) keeps the policy a pure, deterministically testable
/// function.
pub struct EnvPolicy;

impl EnvPolicy {
    /// Build the allowlisted environment from `source`. The result contains
    /// only allowlisted variables plus a guaranteed `PATH`; everything else in
    /// `source` (secrets included) is dropped.
    pub fn build(source: &BTreeMap<String, String>) -> Vec<(String, String)> {
        let mut env: BTreeMap<String, String> = ALLOWED_VARS
            .iter()
            .filter_map(|&var| source.get(var).map(|val| (var.to_string(), val.clone())))
            .collect();

        let path = match env.get("PATH") {
            Some(p) => ensure_dirs_on_path(p, TOOLCHAIN_BIN_DIRS),
            None => FALLBACK_PATH.to_string(),
        };
        env.insert("PATH".to_string(), path);

        let home = env
            .get("HOME")
            .cloned()
            .unwrap_or_else(|| "/home/agent".to_string());
        env.entry("RUSTUP_HOME".to_string())
            .or_insert_with(|| RUSTUP_HOME_DEFAULT.to_string());
        env.entry("CARGO_HOME".to_string())
            .or_insert_with(|| format!("{home}/.cargo"));
        env.entry("NPM_CONFIG_CACHE".to_string())
            .or_insert_with(|| format!("{home}/.npm"));

        env.into_iter().collect()
    }

    /// Snapshot the current process environment as a source map for [`build`].
    ///
    /// [`build`]: EnvPolicy::build
    pub fn current_env() -> BTreeMap<String, String> {
        std::env::vars().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn drops_secrets_keeps_allowlisted() {
        let src = source(&[
            ("PATH", "/usr/bin"),
            ("HOME", "/home/agent"),
            ("WORKFORCE_VAULT_KEY", "super-secret"),
            ("WORKFORCE_GATEWAY_TOKEN", "owner-token"),
            ("ANTHROPIC_API_KEY", "sk-ant-leak"),
        ]);
        let built: BTreeMap<String, String> = EnvPolicy::build(&src).into_iter().collect();

        let path = built.get("PATH").expect("PATH present");
        assert!(path.split(':').any(|d| d == "/usr/bin"));
        assert!(path.split(':').any(|d| d == "/usr/local/cargo/bin"));
        assert_eq!(built.get("HOME").map(String::as_str), Some("/home/agent"));
        // The secrets must be absent — this is the executor's core invariant.
        assert!(!built.contains_key("WORKFORCE_VAULT_KEY"));
        assert!(!built.contains_key("WORKFORCE_GATEWAY_TOKEN"));
        assert!(!built.contains_key("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn synthesises_path_when_source_has_none() {
        let src = source(&[("HOME", "/home/agent")]);
        let built: BTreeMap<String, String> = EnvPolicy::build(&src).into_iter().collect();
        assert_eq!(built.get("PATH").map(String::as_str), Some(FALLBACK_PATH));
    }

    #[test]
    fn injects_toolchain_defaults_for_reset_env() {
        // The runner launches under `setpriv --reset-env`: PATH is present but
        // lacks the cargo bin, and RUSTUP_HOME/CARGO_HOME are absent. The policy
        // must inject them so `cargo` resolves and writes to agent-writable dirs.
        let src = source(&[("HOME", "/home/agent"), ("PATH", "/usr/bin:/bin")]);
        let built: BTreeMap<String, String> = EnvPolicy::build(&src).into_iter().collect();

        let path = built.get("PATH").expect("PATH present");
        assert!(path.split(':').any(|d| d == "/usr/local/cargo/bin"));
        assert_eq!(
            built.get("RUSTUP_HOME").map(String::as_str),
            Some("/usr/local/rustup")
        );
        assert_eq!(
            built.get("CARGO_HOME").map(String::as_str),
            Some("/home/agent/.cargo")
        );
        assert_eq!(
            built.get("NPM_CONFIG_CACHE").map(String::as_str),
            Some("/home/agent/.npm")
        );
    }

    #[test]
    fn carries_toolchain_cache_dirs() {
        let src = source(&[
            ("CARGO_HOME", "/home/agent/.cargo"),
            ("RUSTUP_HOME", "/home/agent/.rustup"),
            ("NPM_CONFIG_CACHE", "/home/agent/.npm"),
        ]);
        let built: BTreeMap<String, String> = EnvPolicy::build(&src).into_iter().collect();
        assert_eq!(
            built.get("CARGO_HOME").map(String::as_str),
            Some("/home/agent/.cargo")
        );
        assert_eq!(
            built.get("RUSTUP_HOME").map(String::as_str),
            Some("/home/agent/.rustup")
        );
        assert_eq!(
            built.get("NPM_CONFIG_CACHE").map(String::as_str),
            Some("/home/agent/.npm")
        );
    }
}

#[cfg(test)]
mod override_tests {
    use super::*;

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn accepts_posix_names() {
        assert!(validate_overrides(&pairs(&[
            ("CLAUDE_CODE_OAUTH_TOKEN", "t"),
            ("_UNDERSCORE", "x"),
            ("WITH9DIGITS", "x"),
        ]))
        .is_ok());
    }

    #[test]
    fn rejects_malformed_names() {
        for bad in ["lowercase", "9LEADING", "HAS-DASH", "HAS=EQUALS", ""] {
            assert!(
                validate_overrides(&pairs(&[(bad, "v")])).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn enforces_the_count_cap() {
        let many: Vec<(String, String)> = (0..=MAX_OVERRIDE_VARS)
            .map(|i| (format!("VAR_{i}"), "v".to_string()))
            .collect();
        assert!(validate_overrides(&many).is_err());
    }

    #[test]
    fn enforces_the_size_cap() {
        let big = pairs(&[("BIG", "")])
            .into_iter()
            .map(|(k, _)| (k, "x".repeat(MAX_OVERRIDE_BYTES)))
            .collect::<Vec<_>>();
        assert!(validate_overrides(&big).is_err());
    }

    #[test]
    fn an_error_never_repeats_the_value() {
        let err = validate_overrides(&pairs(&[("bad name", "super-secret")])).unwrap_err();
        assert!(!err.contains("super-secret"));
    }
}
