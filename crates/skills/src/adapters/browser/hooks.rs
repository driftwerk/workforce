//! What the hub tells the browser, and what the browser tells it back, on
//! a hub that installs [`BrowserHooks`] (Desk's): whether an agent is
//! unbound, what on a page is the person's to do, and a record of every
//! navigation and change with a screenshot. A hub that installs none runs
//! the browser as before: Desk's own browser only, nothing held back,
//! nothing recorded here.

use std::sync::{Arc, OnceLock};

/// What a browser call presses or types into, as the page shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OnPage {
    /// The tool (`browser.click`).
    pub tool: String,
    /// The page it is on.
    pub url: String,
    /// The selector, key or file it was given.
    pub target: Option<String>,
    /// What the page shows of the element: its text, value, label, title,
    /// type, name and autocomplete hint, and its form's action.
    pub shown: String,
    /// The names of a file it uploads: the staged file's own, and the one
    /// the site is told.
    pub files: Vec<String>,
}

/// One navigation or change a browser made, for the agent's activity.
#[derive(Debug, Clone, Default)]
pub struct Done {
    pub agent: String,
    pub user: String,
    /// The session it was made in (its id).
    pub session: Option<String>,
    pub tool: String,
    /// Which browser: `desk` (Desk's own) or `chrome` (the person's).
    pub backend: &'static str,
    /// The page after it.
    pub url: String,
    pub target: Option<String>,
    /// The page as it looked after, PNG.
    pub screenshot: Option<Vec<u8>>,
}

/// Which browsers Desk uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Uses {
    /// None: every browser call is refused.
    Off,
    /// Desk's own browser only.
    Desk,
    /// The person's Chrome only.
    Chrome,
    /// Either: an unbound overseer reaches for the person's Chrome, every
    /// other agent for Desk's own, and a call may name one.
    #[default]
    Both,
}

/// Which sites the person's Chrome may be taken to. A site is a host
/// (`example.com`, which takes in its subdomains; `*.example.com` says so
/// too). A blocked site is never visited; with any allowed, only those are.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Sites {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub block: Vec<String>,
}

fn matches(site: &str, host: &str) -> bool {
    let site = site
        .trim()
        .trim_start_matches("*.")
        .trim_start_matches('.')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let host = host.trim_end_matches('.');
    !site.is_empty() && (host == site || host.ends_with(&format!(".{site}")))
}

impl Sites {
    /// Whether the person's Chrome may be on `host`. A page with none
    /// (`about:blank`) always may.
    pub fn allows(&self, host: Option<&str>) -> bool {
        let Some(host) = host.map(str::to_ascii_lowercase).filter(|h| !h.is_empty()) else {
            return true;
        };
        !self.block.iter().any(|s| matches(s, &host))
            && (self.allow.is_empty() || self.allow.iter().any(|s| matches(s, &host)))
    }
}

/// Desk's browser settings, as the person set them.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    #[serde(default, rename = "use")]
    pub uses: Uses,
    /// Show Desk's own browser on screen; unset, as `[skills.browser]` says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show: Option<bool>,
    /// Where the person's Chrome may go.
    #[serde(default)]
    pub sites: Sites,
}

/// The hub's side.
pub trait BrowserHooks: Send + Sync {
    /// The person's browser settings.
    fn settings(&self) -> Settings {
        Settings::default()
    }
    /// Whether `agent` is unbound: what [`Self::holds`] holds back, it does.
    fn unbound(&self, agent: &str) -> bool;
    /// Why `agent` may not do `what` on its own, if it may not: it is the
    /// person's (paying, signing in or out, a credential's file).
    fn holds(&self, agent: &str, what: &OnPage) -> Option<String>;
    /// Keeps what was done. Must neither block nor fail the call.
    fn record(&self, done: Done);
    /// Whether `agent` may have the file at `path` open, now: a page can
    /// move to another file after the first was allowed. `Err` says why
    /// not. A hub with no say lets it be.
    fn may_open_file(&self, _agent: &str, _path: &std::path::Path) -> Result<(), String> {
        Ok(())
    }
    /// Whether `agent` may be on `origin` in the person's Chrome, now: a
    /// page can take itself to a site the visit wasn't allowed for. `Err`
    /// says why not. A hub with no say lets it be.
    fn may_visit(&self, _agent: &str, _origin: &str) -> Result<(), String> {
        Ok(())
    }
    /// Keeps `user`'s own Google OAuth client, made in Cloud Console, where
    /// Desk's Google sign-in reads it. `Err` says why not. A hub with no
    /// say keeps nothing.
    fn keep_google_client(
        &self,
        _user: &str,
        _client_id: &str,
        _client_secret: &str,
    ) -> Result<(), String> {
        Err("This hub doesn't keep a Google OAuth client for Desk.".into())
    }
    /// Whether the hub's gate decides every browser call before it is made
    /// (Desk's does): a write it decided nothing for is then refused.
    fn gate_decides(&self) -> bool {
        false
    }
}

static HOOKS: OnceLock<Arc<dyn BrowserHooks>> = OnceLock::new();

/// Installs the hub's hooks, once. False when some were already installed.
pub fn install_hooks(hooks: Arc<dyn BrowserHooks>) -> bool {
    HOOKS.set(hooks).is_ok()
}

pub(super) fn hooks() -> Option<&'static Arc<dyn BrowserHooks>> {
    HOOKS.get()
}

/// The settings in force: the hub's, or, with no hooks, Desk's own browser
/// only, as before there was a choice.
pub(super) fn settings() -> Settings {
    match hooks() {
        Some(h) => h.settings(),
        None => Settings {
            uses: Uses::Desk,
            ..Settings::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_site_takes_in_its_subdomains_and_blocks_win() {
        let sites = Sites {
            allow: vec!["example.com".into(), "*.docs.test".into()],
            block: vec!["admin.example.com".into()],
        };
        assert!(sites.allows(Some("example.com")));
        assert!(sites.allows(Some("shop.example.com")));
        assert!(sites.allows(Some("a.docs.test")));
        let dotted = Sites {
            allow: vec![],
            block: vec![".bank.test".into()],
        };
        assert!(
            !dotted.allows(Some("bank.test")),
            "a leading dot is the same site"
        );
        assert!(!dotted.allows(Some("www.bank.test")));
        assert!(!dotted.allows(Some("bank.test.")), "a trailing dot is too");
        assert!(!sites.allows(Some("admin.example.com")));
        assert!(!sites.allows(Some("x.admin.example.com")));
        assert!(!sites.allows(Some("notexample.com")));
        assert!(
            !sites.allows(Some("bank.test")),
            "with any allowed, only those"
        );
        assert!(sites.allows(None), "about:blank");
        let open = Sites {
            allow: vec![],
            block: vec!["bank.test".into()],
        };
        assert!(open.allows(Some("news.test")));
        assert!(!open.allows(Some("www.bank.test")));
    }

    #[test]
    fn settings_read_as_the_person_wrote_them() {
        let s: Settings =
            serde_json::from_str(r#"{"use":"chrome","sites":{"block":["bank.test"]}}"#).unwrap();
        assert_eq!(s.uses, Uses::Chrome);
        assert_eq!(s.sites.block, vec!["bank.test"]);
        assert_eq!(
            serde_json::from_str::<Settings>("{}").unwrap().uses,
            Uses::Both
        );
    }
}
