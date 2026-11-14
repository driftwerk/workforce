//! The GitHub CLI's own sign-in on this computer, for a GitHub setup to
//! offer: when `gh` is signed in to github.com, the person can use that
//! sign-in with one click instead of making a token.
//!
//! Only hub code runs `gh`. What it is asked first (`gh api -i user`) says
//! whose account it is and the token's scopes, never the token; the token
//! itself (`gh auth token`) is read only once the person said to use it,
//! straight into a [`Secret`], and goes only to the vault. Nothing `gh` says
//! is logged. Tokens in the hub's environment are never used: only the
//! sign-in `gh` keeps.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use workforce_common::secret::Secret;

/// The longest `gh` is waited for.
const WAIT: Duration = Duration::from_secs(10);
/// The most of its answer that is read.
const MOST: usize = 256 * 1024;

/// Where `gh` signed in, as a setup's card shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Login {
    pub account: String,
    /// The token's scopes, as GitHub lists them; empty where it said none.
    pub scopes: Vec<String>,
}

/// The `gh` binary a hub asks, if any.
#[derive(Debug, Clone, Default)]
pub struct GhCli {
    bin: Option<PathBuf>,
}

impl GhCli {
    /// `gh` from the hub's `PATH`, else where Homebrew and the installers
    /// put it: a hub started from the Dock has no shell `PATH`.
    pub fn live() -> Self {
        // Whole paths only: never one relative to where the hub runs.
        let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|p| {
                std::env::split_paths(&p)
                    .filter(|d| d.is_absolute())
                    .collect()
            })
            .unwrap_or_default();
        for d in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
            dirs.push(PathBuf::from(d));
        }
        let name = if cfg!(windows) { "gh.exe" } else { "gh" };
        Self {
            bin: dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file()),
        }
    }

    /// A hub that offers no `gh` sign-in.
    pub fn none() -> Self {
        Self { bin: None }
    }

    /// `gh` at `bin`: a test's stand-in.
    pub fn at(bin: impl Into<PathBuf>) -> Self {
        Self {
            bin: Some(bin.into()),
        }
    }

    async fn run(&self, args: &[&str]) -> Option<String> {
        let bin = self.bin.as_ref()?;
        let mut cmd = tokio::process::Command::new(bin);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            // Its own sign-in only, never a token the hub was started with.
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .env_remove("GH_ENTERPRISE_TOKEN")
            .env_remove("GITHUB_ENTERPRISE_TOKEN")
            .env_remove("GH_HOST")
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("GH_PAGER", "cat")
            .env("NO_COLOR", "1");
        let out = tokio::time::timeout(WAIT, cmd.output()).await.ok()?.ok()?;
        if !out.status.success() || out.stdout.len() > MOST {
            return None;
        }
        String::from_utf8(out.stdout).ok()
    }

    /// Whom `gh` is signed in to github.com as, checked with GitHub: `None`
    /// where there is no `gh`, it isn't signed in, or GitHub refused it.
    pub async fn login(&self) -> Option<Login> {
        let said = self
            .run(&["api", "--hostname", "github.com", "-i", "user"])
            .await?;
        parse_login(&said)
    }

    /// The token `gh` signs in with, once the person said to use it.
    pub async fn token(&self) -> Option<Secret<String>> {
        let said = self
            .run(&["auth", "token", "--hostname", "github.com"])
            .await?;
        let token = said.trim();
        (!token.is_empty() && !token.contains(char::is_whitespace))
            .then(|| Secret::new(token.to_string()))
    }
}

/// `gh api -i user`'s answer: its headers, a blank line, GitHub's body.
fn parse_login(said: &str) -> Option<Login> {
    let said = said.replace("\r\n", "\n");
    let (head, body) = said.split_once("\n\n")?;
    let account = serde_json::from_str::<Value>(body.trim())
        .ok()?
        .get("login")?
        .as_str()
        .filter(|l| {
            !l.is_empty()
                && l.len() <= 39
                && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })?
        .to_string();
    let scopes = head
        .lines()
        .find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("x-oauth-scopes")
                .then(|| value.to_string())
        })
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| {
                    !s.is_empty()
                        && s.len() <= 40
                        && s.chars()
                            .all(|c| c.is_ascii_alphanumeric() || ":_-".contains(c))
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Login { account, scopes })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn its_answer_is_read_for_the_account_and_scopes_alone() {
        let said = "HTTP/2.0 200 OK\r\nContent-Type: application/json\r\nX-Oauth-Scopes: gist, read:org, repo, workflow\r\n\r\n{\"login\":\"sarah\",\"id\":1}";
        assert_eq!(
            parse_login(said),
            Some(Login {
                account: "sarah".into(),
                scopes: vec![
                    "gist".into(),
                    "read:org".into(),
                    "repo".into(),
                    "workflow".into()
                ],
            })
        );
        assert_eq!(
            parse_login("HTTP/2.0 200 OK\n\n{\"login\":\"sarah\"}").map(|l| l.scopes),
            Some(vec![])
        );
        assert_eq!(
            parse_login("HTTP/2.0 401\n\n{\"message\":\"Bad credentials\"}"),
            None
        );
        assert_eq!(parse_login("{\"login\":\"<script>\"}"), None);
    }

    #[tokio::test]
    async fn without_gh_nothing_is_offered() {
        assert_eq!(GhCli::none().login().await, None);
        assert!(GhCli::none().token().await.is_none());
        assert_eq!(GhCli::at("/nonexistent/gh").login().await, None);
    }
}
