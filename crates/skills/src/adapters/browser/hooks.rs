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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(from = "Kept")]
pub struct Settings {
    /// Whether Desk uses the person's Chrome at all.
    pub on: bool,
    /// Where the person's Chrome may go.
    pub sites: Sites,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            on: true,
            sites: Sites::default(),
        }
    }
}

/// The settings as kept, and as the old Chromium skill kept them (`use`:
/// off, desk, chrome or both); only `off` meant off.
#[derive(serde::Deserialize)]
struct Kept {
    on: Option<bool>,
    #[serde(rename = "use")]
    uses: Option<String>,
    #[serde(default)]
    sites: Sites,
}

impl From<Kept> for Settings {
    fn from(k: Kept) -> Self {
        Self {
            on: k.on.unwrap_or_else(|| k.uses.as_deref() != Some("off")),
            sites: k.sites,
        }
    }
}

/// A new key the extension read off a key page, for
/// [`BrowserHooks::keep_key`].
pub struct KeyRead<'a> {
    pub user: &'a str,
    pub agent: &'a str,
    /// The conversation's session key.
    pub conversation: &'a str,
    pub connector: &'a str,
    /// Where the extension says it read it, read in the same call.
    pub origin: &'a str,
    pub path: &'a str,
    pub key: &'a str,
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
    /// Whether `agent` may be on `origin` in the person's Chrome, now: a
    /// page can take itself to a site the visit wasn't allowed for. `Err`
    /// says why not. A hub with no say lets it be.
    fn may_visit(&self, _agent: &str, _origin: &str) -> Result<(), String> {
        Ok(())
    }
    /// The key page `user`'s `agent` may read a new key of `connector` off
    /// in `conversation` (its session key): the page of the setup of
    /// `connector` that conversation is running. `None` when no setup there
    /// is waiting on a key, and nothing is read. A hub with no connections
    /// has none.
    fn key_page(
        &self,
        _user: &str,
        _agent: &str,
        _conversation: &str,
        _connector: &str,
    ) -> Option<super::keys::KeyPage> {
        None
    }
    /// Holds a new key `read` says was read, for the setup [`Self::key_page`]
    /// gave its page for. What the setup says next (`ready: …`, or what it
    /// still needs); `Err` says why it wasn't kept. Nothing of the key is in
    /// either. A hub with no connections keeps nothing.
    fn keep_key(&self, _read: KeyRead<'_>) -> Result<String, String> {
        Err("This hub doesn't set up connections from the browser.".into())
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

/// The settings in force: the hub's, or, with no hooks, the defaults (no
/// browser is reached without hooks anyway).
pub(super) fn settings() -> Settings {
    hooks().map(|h| h.settings()).unwrap_or_default()
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
    fn settings_read_as_kept_and_as_the_old_skill_kept_them() {
        let s: Settings =
            serde_json::from_str(r#"{"on":false,"sites":{"block":["bank.test"]}}"#).unwrap();
        assert!(!s.on);
        assert_eq!(s.sites.block, vec!["bank.test"]);
        assert!(serde_json::from_str::<Settings>("{}").unwrap().on);
        for (old, on) in [
            ("off", false),
            ("desk", true),
            ("chrome", true),
            ("both", true),
        ] {
            let s: Settings = serde_json::from_str(&format!(r#"{{"use":"{old}"}}"#)).unwrap();
            assert_eq!(s.on, on, "{old}");
        }
        let round: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(round, s);
    }
}
