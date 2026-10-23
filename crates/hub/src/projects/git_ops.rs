//! Shared git command runner + GitHub URL helpers used by `fetcher`,
//! `work_session`, and any other module that shells out to `git`.
//!
//! Token-bearing URLs are used only at clone time and rewritten to the
//! bare HTTPS form afterwards (see `github_https_url`); a per-clone
//! credential helper (see `github_credential_helper`) keeps subsequent
//! `git fetch` working without exposing the PAT in the remote URL. Stderr
//! containing the token is sanitised in error messages.

use std::path::Path;
use std::process::Output;
use std::sync::OnceLock;
use std::time::Duration;

use eyre::{eyre, Result};

use crate::git_hardening::hardened_git;

/// Backstop timeout for any single `git` invocation. Generous because a clone
/// or fetch of a large repo over a slow link can legitimately take minutes,
/// while read-only ops (rev-parse, status) never approach it. Override with
/// the `WORKFORCE_GIT_TIMEOUT_SECS` env var.
const DEFAULT_GIT_TIMEOUT_SECS: u64 = 600;

fn resolve_git_timeout(var: Option<&str>) -> Duration {
    let secs = var
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(DEFAULT_GIT_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

fn git_timeout() -> Duration {
    static TIMEOUT: OnceLock<Duration> = OnceLock::new();
    *TIMEOUT.get_or_init(|| {
        resolve_git_timeout(std::env::var("WORKFORCE_GIT_TIMEOUT_SECS").ok().as_deref())
    })
}

/// Run a `git` command under [`git_timeout`]. Stderr is sanitised against
/// `token` so a leaked PAT can't reach logs or error reports.
pub(super) async fn run_git(
    cwd: Option<&Path>,
    args: &[&str],
    token: Option<&str>,
) -> Result<Output> {
    let mut cmd = hardened_git();
    cmd.args(args);
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    let timeout = git_timeout();
    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| eyre!("git {} timed out after {}s", args[0], timeout.as_secs()))??;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let sanitised = match token {
            Some(t) => stderr.replace(t, "***"),
            None => stderr,
        };
        return Err(eyre!("git {} failed: {}", args[0], sanitised.trim()));
    }
    Ok(output)
}

/// Build the (token-bearing, bare-https) URL pair for a GitHub repo.
/// Callers clone with the first and rewrite the remote to the second so the
/// PAT never lives in the on-disk config.
pub(super) fn github_https_url(owner: &str, repo: &str, token: Option<&str>) -> (String, String) {
    let public = format!("https://github.com/{owner}/{repo}.git");
    let with_token = match token {
        Some(t) => format!("https://x-access-token:{t}@github.com/{owner}/{repo}.git"),
        None => public.clone(),
    };
    (with_token, public)
}

/// `credential.helper` value that injects a fresh PAT for `git fetch` /
/// `git push` without storing it on disk in cleartext outside the (root-
/// readable) git config.
pub(super) fn github_credential_helper(token: &str) -> String {
    format!("!f() {{ echo username=x-access-token; echo password={token}; }}; f")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_https_url_with_token_keeps_public_url_clean() {
        let (with_tok, public) = github_https_url("acme", "widgets", Some("ghp_xxx"));
        assert_eq!(public, "https://github.com/acme/widgets.git");
        assert!(with_tok.contains("x-access-token:ghp_xxx"));
        assert!(with_tok.contains("github.com/acme/widgets.git"));
    }

    #[test]
    fn github_https_url_without_token_uses_anonymous_url() {
        let (with_tok, public) = github_https_url("acme", "widgets", None);
        assert_eq!(public, "https://github.com/acme/widgets.git");
        assert_eq!(with_tok, public);
    }

    #[test]
    fn github_credential_helper_injects_token() {
        let helper = github_credential_helper("ghp_xxx");
        assert!(helper.contains("ghp_xxx"));
        assert!(helper.contains("x-access-token"));
    }

    #[test]
    fn git_timeout_falls_back_to_default_when_unset_or_invalid() {
        let default = Duration::from_secs(DEFAULT_GIT_TIMEOUT_SECS);
        assert_eq!(resolve_git_timeout(None), default);
        assert_eq!(resolve_git_timeout(Some("not-a-number")), default);
        assert_eq!(resolve_git_timeout(Some("0")), default);
    }

    #[test]
    fn git_timeout_honours_a_valid_override() {
        assert_eq!(resolve_git_timeout(Some("300")), Duration::from_secs(300));
    }
}
