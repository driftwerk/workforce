//! What the hub tells the browser, and what the browser tells it back, on
//! a hub that installs [`BrowserHooks`] (Desk's): whether an agent is
//! unbound, what on a page is the person's to do, and a record of every
//! navigation and change with a screenshot. A hub that installs none runs
//! the browser as before: nothing is held back, nothing recorded here.

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

/// The hub's side.
pub trait BrowserHooks: Send + Sync {
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
}

static HOOKS: OnceLock<Arc<dyn BrowserHooks>> = OnceLock::new();

/// Installs the hub's hooks, once. False when some were already installed.
pub fn install_hooks(hooks: Arc<dyn BrowserHooks>) -> bool {
    HOOKS.set(hooks).is_ok()
}

pub(super) fn hooks() -> Option<&'static Arc<dyn BrowserHooks>> {
    HOOKS.get()
}
