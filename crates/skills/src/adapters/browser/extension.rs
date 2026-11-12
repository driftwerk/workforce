//! The person's Chrome, through Desk's Chrome extension: the only browser
//! Desk has. The hub installs an [`ExtensionLink`] (its end of the
//! extension's native messaging, relayed by Desk Helper); each
//! `(agent, person)` gets a tab in Desk's own Chrome window
//! ([`ExtensionCell`]), opened by its first call and closed on Stop, at a
//! turn's end and after its idle time. A page opened *for* the person
//! (`browser.open_for_me`) is handed over instead: its tab moves to the
//! front of their own window, and Desk lets go of it.
//!
//! Every tool reads and acts as before: page text fenced as untrusted, the
//! person's own steps held back below unbound, the site list kept, what was
//! done recorded with a picture (never of a page showing a secret or of a
//! key page), and a call decided for one origin acting there or not at all.
//! The extension only runs code shipped in it, in its own tabs: it has no
//! way to reach any other tab, and no way to run a script it's sent.

use std::sync::Mutex as StdMutex;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use async_trait::async_trait;
use eyre::{eyre, Result};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use workforce_agent::runner::{CURRENT_SESSION_ID, CURRENT_SESSION_KEY};

use super::client::{
    host_of, no_screenshot, path_only, signing, stage_screenshot_artifact, take_decided,
    undecided_write, wait_budget_ms, Decided, SessionId, SIGNED_IN, SIGN_IN,
};
use super::config::BrowserSkillConfig;
use super::hooks::{self, Done, OnPage};
use super::keys;
use super::untrusted::{self, Seen};

/// The hub's end of Desk's extension.
#[async_trait]
pub trait ExtensionLink: Send + Sync {
    /// Asks the extension to do `cmd` with `args` (the protocol's call,
    /// less its id); its answer, or the sentence it failed with.
    async fn call(&self, cmd: &str, args: Value) -> Result<Value>;
    /// Whether a confirmed extension is connected now.
    fn connected(&self) -> bool;
}

static LINK: OnceLock<Arc<dyn ExtensionLink>> = OnceLock::new();

/// Installs the hub's end of the extension, once. False when one was
/// installed already.
pub fn install_link(link: Arc<dyn ExtensionLink>) -> bool {
    LINK.set(link).is_ok()
}

/// What every browser tool says where there is no browser: a hub with no
/// screen, a job, or Desk's extension not connected.
pub const NO_BROWSER: &str = "There's no browser here: Desk browses only in the person's own Chrome, through Desk's Chrome extension, and only in a turn they're in. Tell them what you would do there.";

/// What a call says when the extension isn't connected now.
pub const NOT_CONNECTED: &str = "The person's Chrome isn't connected to Desk now: Chrome may be closed, or Desk's extension not added yet (Settings → Browser). Ask them to open Chrome, then try again.";

pub(super) fn link() -> Result<&'static Arc<dyn ExtensionLink>> {
    let link = LINK.get().ok_or_else(|| eyre!(NO_BROWSER))?;
    if !link.connected() {
        return Err(eyre!(NOT_CONNECTED));
    }
    Ok(link)
}

/// Whether the person's Chrome may show Desk `url`: the web and a blank
/// page only, and a site the person's list allows.
pub(super) fn may_show(url: &str) -> bool {
    use super::address::kind;
    let host = host_of(url);
    super::address::opens_in_chrome(&kind(url))
        && !super::address::on_own_port(url)
        && hooks::settings().sites.allows(host.as_deref())
}

/// A selector on a key page that could test what a field holds is refused:
/// whether it matched would read the key.
fn selector_ok(url: &str, selector: Option<&str>) -> Result<()> {
    match selector {
        Some(s) if keys::is_key_page(url) && keys::probes_value(s) => {
            Err(eyre!(keys::PROBES_VALUE))
        }
        _ => Ok(()),
    }
}

pub(super) use workforce_core::computer::OPEN_FOR_ME;

/// What a hand-over that failed says: the agent tells the person.
const NOT_SHOWN: &str = "The page wasn't put in front of the person, so tell them it wasn't shown.";

/// What a hand-over says when the extension doesn't know how.
pub const EXTENSION_TOO_OLD: &str = "Desk's Chrome extension is older than Desk and can't put a page in front of the person, so it wasn't shown. Tell them, and that updating Desk's extension in Chrome fixes it.";

/// What a hand-over says when Chrome took the tab but didn't confirm it is
/// in front.
pub const NOT_IN_FRONT: &str = "The page is open as a tab in the person's Chrome, but Chrome didn't confirm it's in front of them. Tell them it's there and to switch to Chrome; don't say it's showing.";

/// What a call on "the page" says once that page was handed to the person.
pub const HANDED_ALREADY: &str = "The page you opened for the person is theirs now, so you can't read or act on it. To browse yourself, open a page with browser.navigate.";

/// What a hand-over tells the model once the page is in front.
pub const HANDED_OVER: &str = "The page is in front of the person, in their own Chrome window, and is theirs now: it stays open after this turn, and you can't read or act on it. To browse yourself, use browser.navigate, which opens your own tab.";

/// Where the person's Chrome may not be, as a refusal.
fn kept_away(host: &str) -> eyre::Report {
    eyre!("Desk keeps away from {host} in your Chrome (Settings → Browser), so it wasn't opened.")
}

/// Clears the site a call was opening when the call ends, any way it ends.
struct PendingVisit<'a>(&'a StdMutex<Option<String>>);

impl Drop for PendingVisit<'_> {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.0.lock() {
            *pending = None;
        }
    }
}

/// One `(agent, person)`'s tab in Desk's Chrome window.
pub(super) struct ExtensionCell {
    config: Arc<BrowserSkillConfig>,
    /// Its [`SessionId`]: keys what it has read, and names its tab to the
    /// extension.
    pub(super) key: String,
    pub(super) agent: String,
    pub(super) user: String,
    /// The session (id) its last call came from.
    pub(super) last_session: StdMutex<Option<String>>,
    /// That conversation's key, which its allowances are kept under.
    pub(super) last_key: StdMutex<Option<String>>,
    pub(super) last_used: StdMutex<Instant>,
    /// Whether its tab is open.
    pub(super) open: StdMutex<bool>,
    /// The site a call the person allowed opened: it may show without the
    /// gate allowing a visit again.
    visited: StdMutex<Option<String>>,
    /// The site the call being made now was allowed to open.
    pending_visit: StdMutex<Option<String>>,
    /// One call at a time in its tab.
    serial: Mutex<()>,
    /// Its last page was handed to the person, and no page of its own has
    /// been opened since: a call on "the page" would find a blank tab.
    handed: StdMutex<bool>,
}

impl Drop for ExtensionCell {
    fn drop(&mut self) {
        untrusted::forget(&self.key);
    }
}

impl ExtensionCell {
    pub(super) fn new(
        config: Arc<BrowserSkillConfig>,
        id: &SessionId,
        agent: &str,
        user: &str,
    ) -> Self {
        Self {
            config,
            key: id.0.clone(),
            agent: agent.to_string(),
            user: user.to_string(),
            last_session: StdMutex::new(None),
            last_key: StdMutex::new(None),
            last_used: StdMutex::new(Instant::now()),
            open: StdMutex::new(false),
            visited: StdMutex::new(None),
            pending_visit: StdMutex::new(None),
            serial: Mutex::new(()),
            handed: StdMutex::new(false),
        }
    }

    fn touch(&self) {
        *self.last_used.lock().unwrap() = Instant::now();
        let local = |k: &'static tokio::task::LocalKey<String>| {
            k.try_with(|v| v.clone()).ok().filter(|v| !v.is_empty())
        };
        if let Some(id) = local(&CURRENT_SESSION_ID) {
            *self.last_session.lock().unwrap() = Some(id);
        }
        if let Some(key) = local(&CURRENT_SESSION_KEY) {
            *self.last_key.lock().unwrap() = Some(key);
        }
    }

    /// Asks the extension `cmd` in this cell's tab.
    async fn ask(&self, cmd: &str, mut args: Value) -> Result<Value> {
        if let Value::Object(map) = &mut args {
            map.insert("session".into(), Value::String(self.key.clone()));
        }
        let out = link()?.call(cmd, args).await;
        if out.is_ok() && cmd != "close" {
            *self.open.lock().unwrap() = true;
        }
        out
    }

    /// The page's address and title now, noted for the gate.
    async fn where_now(&self) -> Result<(String, String)> {
        let at = self.ask("where", json!({})).await?;
        let url = at["url"].as_str().unwrap_or_default().to_string();
        untrusted::on(&self.key, &url);
        Ok((url, at["title"].as_str().unwrap_or_default().to_string()))
    }

    /// Before every call: the page Desk's tab is on is one it may be on (the
    /// web or blank, a site the list allows, one this session may visit),
    /// else it is blanked and nothing is done; and a call decided for one
    /// origin acts there or not at all.
    async fn keep_page(&self, decided: Option<&Decided>) -> Result<String> {
        let (url, _) = self.where_now().await?;
        let mut refused = (!may_show(&url))
            .then(|| kept_away(&host_of(&url).unwrap_or_else(|| "that page".into())).to_string());
        if refused.is_none() {
            let origin = super::address::origin_of(&url);
            let allowed = {
                let mut visited = self.visited.lock().unwrap();
                // Off the site a call was allowed to open: that allowance goes.
                if *visited != origin {
                    *visited = None;
                }
                origin.is_some() && *visited == origin
            };
            if let (Some(origin), Some(h), false) = (origin, hooks::hooks(), allowed) {
                refused = h.may_visit(&self.agent, &origin).err();
            }
        }
        if let Some(why) = refused {
            let _ = self.ask("navigate", json!({ "url": "about:blank" })).await;
            untrusted::on(&self.key, "about:blank");
            return Err(eyre!("{why} Desk's tab is blank now."));
        }
        if let Some(Decided::Origin(decided)) = decided {
            if &super::address::origin_of(&url) != decided {
                return Err(eyre!(
                    "The page moved since this was decided, so nothing was done. Call again."
                ));
            }
        }
        Ok(url)
    }

    /// Runs `op` in this cell's tab, one call at a time, after the page and
    /// the call's decision are checked.
    async fn with<T>(&self, op: impl AsyncFnOnce(&str) -> Result<T>) -> Result<T> {
        link()?;
        if *self.handed.lock().unwrap() {
            return Err(eyre!(HANDED_ALREADY));
        }
        self.touch();
        let _serial = self.serial.lock().await;
        let decided = take_decided();
        undecided_write(&decided)?;
        let url = self.keep_page(decided.as_ref()).await?;
        // The site the person allowed this very call to open: it stays
        // allowed only if the call lands there ([`Self::navigate`]).
        *self.pending_visit.lock().unwrap() = match decided {
            Some(Decided::Visit(origin)) => origin,
            _ => None,
        };
        let _pending = PendingVisit(&self.pending_visit);
        op(&url).await
    }

    /// After a change: a page the site list keeps Desk from is left at once.
    async fn keep_to_sites(&self) -> Result<()> {
        let (url, _) = self.where_now().await?;
        let host = host_of(&url).filter(|_| !url.starts_with("about:"));
        if hooks::settings().sites.allows(host.as_deref()) {
            return Ok(());
        }
        let _ = self.ask("navigate", json!({ "url": "about:blank" })).await;
        Err(kept_away(host.as_deref().unwrap_or(&url)))
    }

    /// Holds back what is the person's to do (paying, signing in or out, a
    /// credential's file), read from what the page shows in the extension's
    /// own world.
    async fn hold(
        &self,
        url: &str,
        tool: &str,
        target: Option<&str>,
        selector: Option<&str>,
        form: bool,
        files: &[&str],
    ) -> Result<()> {
        let Some(hooks) = hooks::hooks() else {
            return Ok(());
        };
        if hooks.unbound(&self.agent) {
            return Ok(());
        }
        let shown = self
            .ask("shown", json!({ "selector": selector, "form": form }))
            .await
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let what = OnPage {
            tool: tool.to_string(),
            url: url.to_string(),
            target: target.map(str::to_string),
            shown,
            files: files.iter().map(|f| f.to_string()).collect(),
        };
        match hooks.holds(&self.agent, &what) {
            Some(why) => Err(eyre!("{why}")),
            None => Ok(()),
        }
    }

    /// A picture of Desk's tab, as the extension took it: the extension
    /// checks, just before the picture, that the tab it pictures is Desk's
    /// tab and the window's active one, and says that tab's address and
    /// text, read in the same moment. A picture of a key page, of a page
    /// that shows a secret, or one that doesn't say what it pictured, is
    /// thrown away here; `Err` with why none was kept.
    async fn capture(&self) -> Result<Vec<u8>> {
        let shot = self
            .ask("capture", json!({ "refuse": keys::refusals() }))
            .await?;
        if let Some(why) = shot["skipped"].as_str() {
            return Err(eyre!(match why {
                "minimised" => "Desk's Chrome window is minimised, so no picture was taken. Ask the person to bring it back.",
                "not_front" => "Desk's tab isn't the one showing in its Chrome window, so no picture was taken. Call again.",
                "moved" => "The page moved while it was pictured, so the picture was thrown away. Call again.",
                _ => super::client::SHOWS_A_SECRET,
            }));
        }
        let (Some(url), Some(text)) = (shot["url"].as_str(), shot["text"].as_str()) else {
            return Err(eyre!(super::client::SHOWS_A_SECRET));
        };
        if keys::is_key_page(url) || workforce_common::redact::contains_secret(text) {
            return Err(eyre!(super::client::SHOWS_A_SECRET));
        }
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(shot["png_base64"].as_str().unwrap_or_default())
            .map_err(|_| eyre!("Chrome's picture couldn't be read"))
    }

    /// A picture of the page, for the activity row: none of a page that
    /// shows a secret or is a key page, none of a minimised window.
    async fn picture(&self) -> Option<Vec<u8>> {
        self.capture().await.ok()
    }

    /// Records what was done, with a picture of the page after.
    async fn done(&self, tool: &str, target: Option<&str>) {
        if hooks::hooks().is_none() {
            return;
        }
        let url = self.where_now().await.map(|(u, _)| u).unwrap_or_default();
        let signing_in = matches!(tool, SIGN_IN | SIGNED_IN);
        let (url, target) = match signing_in {
            true => (path_only(&url), target.map(path_only)),
            false => (url, target.map(str::to_string)),
        };
        let screenshot = match no_screenshot(tool) {
            true => None,
            false => self.picture().await,
        };
        self.record(tool, target, url, screenshot);
    }

    /// Keeps a row of what was done, as [`Self::done`] does, from what was
    /// read of the page already.
    fn record(&self, tool: &str, target: Option<String>, url: String, screenshot: Option<Vec<u8>>) {
        let Some(hooks) = hooks::hooks() else {
            return;
        };
        hooks.record(Done {
            agent: self.agent.clone(),
            user: self.user.clone(),
            session: self.last_session.lock().unwrap().clone(),
            tool: tool.to_string(),
            backend: "chrome",
            url,
            target,
            screenshot,
        });
    }

    /// Lets go: Desk's tab closes. Whether it was open.
    pub(super) async fn close(&self) -> bool {
        let was = std::mem::replace(&mut *self.open.lock().unwrap(), false);
        if was {
            if let Some(link) = LINK.get() {
                let _ = link.call("close", json!({ "session": self.key })).await;
            }
        }
        untrusted::forget(&self.key);
        was
    }

    pub(super) async fn navigate(&self, url: &str) -> Result<Value> {
        self.checked(url)?;
        *self.handed.lock().unwrap() = false;
        self.with(async |_| {
            let (landed_url, title) = self.open(url, "browser.navigate", true).await?;
            self.keep_to_sites().await?;
            let mut seen = Seen::at(&landed_url);
            let title = seen.text(&title);
            let shown_url = seen.text(&landed_url);
            Ok(seen.into_output(&self.key, json!({ "url": shown_url, "title": title })))
        })
        .await
    }

    /// Opens `url` for the person: through every check [`Self::navigate`]
    /// makes, in this cell's own tab, then hands that tab over. The
    /// extension moves it to the front of the person's current Chrome
    /// window and lets go of it, so it is theirs: never closed at a turn's
    /// end, and never driven by this agent again. Answers only once Chrome
    /// says it is the active tab of the focused window.
    pub(super) async fn open_for_person(&self, url: &str) -> Result<Value> {
        self.checked(url)?;
        *self.handed.lock().unwrap() = false;
        self.with(async |_| {
            let (landed_url, title) = self.open(url, OPEN_FOR_ME, false).await?;
            self.keep_to_sites().await?;
            // The row is kept once the page is the person's, with the
            // picture taken while it is still Desk's tab.
            let shot = match hooks::hooks() {
                Some(_) => self.picture().await,
                None => None,
            };
            // Nothing is asked of this tab after the hand-over: every call
            // names the session, and the extension would open it a new tab.
            let handed = self.ask("hand_over", json!({})).await;
            let handed = match handed {
                Ok(v) => v,
                Err(e) => return Err(eyre!("{NOT_SHOWN} Chrome said: {e}")),
            };
            if handed["handed"].as_bool() != Some(true) {
                // An extension older than this hub answers what it doesn't
                // know with nothing; the tab is still Desk's.
                return Err(eyre!(EXTENSION_TOO_OLD));
            }
            self.record(OPEN_FOR_ME, Some(url.to_string()), landed_url.clone(), shot);
            let in_front = handed["active"].as_bool() == Some(true)
                && handed["focused"].as_bool() == Some(true);
            let mut seen = Seen::at(&landed_url);
            let title = seen.text(&title);
            let shown_url = seen.text(&landed_url);
            let out = seen.into_output(
                &self.key,
                json!({ "url": shown_url, "title": title, "shown": true, "next": HANDED_OVER }),
            );
            self.let_go();
            match in_front {
                true => Ok(out),
                false => Err(eyre!(NOT_IN_FRONT)),
            }
        })
        .await
    }

    /// The tab was handed to the person: it is no longer this cell's. What
    /// it read is forgotten as a closed tab's is (a hostile page still
    /// holds the agent back), and the site it was allowed goes.
    fn let_go(&self) {
        *self.open.lock().unwrap() = false;
        *self.handed.lock().unwrap() = true;
        *self.visited.lock().unwrap() = None;
        untrusted::forget(&self.key);
    }

    /// What opening `url` is refused for before anything is asked of Chrome:
    /// not a web page, or a site the person's list keeps Desk from.
    fn checked(&self, url: &str) -> Result<()> {
        if !super::address::opens_in_chrome(&super::address::kind(url)) {
            return Err(eyre!(
                "Desk opens web pages (https://…) and about:blank only, never files, scripts, inline pages or Chrome's own pages."
            ));
        }
        let host = host_of(url).filter(|_| !url.starts_with("about:"));
        if !hooks::settings().sites.allows(host.as_deref()) {
            return Err(kept_away(host.as_deref().unwrap_or(url)));
        }
        Ok(())
    }

    /// Takes this cell's tab to `url`, inside [`Self::with`]: where it lands
    /// passes the gate and the site list, and what was done is recorded as
    /// `tool`, with a picture, when `record`. Its address and title,
    /// unfenced.
    async fn open(&self, url: &str, tool: &str, record: bool) -> Result<(String, String)> {
        let at = self.ask("navigate", json!({ "url": url })).await?;
        let landed_url = at["url"].as_str().unwrap_or_default().to_string();
        untrusted::on(&self.key, &landed_url);
        // Where it landed passes the gate before anything of it is said
        // or pictured: a redirect to a site the session may not visit
        // is left at once, and its address and title go unsaid.
        let landed = super::address::origin_of(&landed_url);
        let allowed_visit = landed.is_some() && landed == *self.pending_visit.lock().unwrap();
        let refused = match (&landed, hooks::hooks()) {
            _ if !may_show(&landed_url) => Some(
                "It led to a page Desk may not show in your Chrome, so it was left.".to_string(),
            ),
            (Some(origin), Some(h)) if !allowed_visit => h
                .may_visit(&self.agent, origin)
                .err()
                .map(|_| "It led to a site this conversation hasn't been allowed to open, so it was left.".to_string()),
            _ => None,
        };
        if let Some(why) = refused {
            let _ = self.ask("navigate", json!({ "url": "about:blank" })).await;
            untrusted::on(&self.key, "about:blank");
            return Err(eyre!("{why} Desk's tab is blank now."));
        }
        // Opened, by the call the person allowed, exactly the site it was
        // allowed to: it may stay.
        if allowed_visit {
            *self.visited.lock().unwrap() = landed;
        }
        if record {
            self.done(tool, Some(url)).await;
        }
        Ok((
            landed_url,
            at["title"].as_str().unwrap_or_default().to_string(),
        ))
    }

    /// Opens `url` in Desk's tab and brings it to the front of the person's
    /// Chrome, for them to sign in themselves; nothing else is done in the
    /// browser until they say they signed in, or decline.
    pub(super) async fn sign_in(&self, url: &str) -> Result<Value> {
        if !matches!(super::address::kind(url), super::address::Kind::Web) {
            return Err(eyre!("A sign-in page is a web page (https://…)."));
        }
        *self.handed.lock().unwrap() = false;
        let id = SessionId(self.key.clone());
        self.with(async |_| {
            super::client::start_signing(&id);
            let opened: Result<Value> = async {
                let at = self.ask("navigate", json!({ "url": url })).await?;
                self.ask("front", json!({})).await?;
                let landed = at["url"].as_str().unwrap_or_default().to_string();
                untrusted::on(&self.key, &landed);
                self.done(SIGN_IN, Some(url)).await;
                let mut seen = Seen::at(&landed);
                let shown = seen.text(&path_only(&landed));
                let title = seen.text(at["title"].as_str().unwrap_or_default());
                Ok(seen.into_output(
                    &self.key,
                    json!({ "url": shown, "title": title, "next": super::client::SIGN_IN_NEXT }),
                ))
            }
            .await;
            if opened.is_err() {
                super::client::end_signing(&id);
            }
            opened
        })
        .await
    }

    /// Where the page is once the person said they signed in: its origin
    /// and path, never its query or fragment, nothing of what they typed,
    /// and no picture. Ends the sign-in.
    pub(super) async fn signed_in(&self) -> Result<Value> {
        let id = SessionId(self.key.clone());
        if !signing(&id) {
            return Err(eyre!("No sign-in is open: call browser.sign_in first."));
        }
        link()?;
        self.touch();
        let read = async {
            let (url, title) = self.where_now().await?;
            self.done(SIGNED_IN, None).await;
            let mut seen = Seen::at(&url);
            let (url, title) = (seen.text(&path_only(&url)), seen.text(&title));
            Ok(seen.into_output(&self.key, json!({ "url": url, "title": title })))
        }
        .await;
        super::client::end_signing(&id);
        read
    }

    /// The page's text or HTML, read through the extension, with key-shaped
    /// text hidden on a key page.
    async fn read(&self, what: &str) -> Result<(String, String, String)> {
        let page = self.ask("read", json!({ "what": what })).await?;
        let url = page["url"].as_str().unwrap_or_default().to_string();
        untrusted::on(&self.key, &url);
        let body = page[if what == "html" { "html" } else { "text" }]
            .as_str()
            .unwrap_or_default();
        let body = keys::hide_keys(&url, body);
        let title = keys::hide_keys(&url, page["title"].as_str().unwrap_or_default());
        Ok((url, title, body))
    }

    pub(super) async fn snapshot(&self) -> Result<Value> {
        self.with(async |_| {
            let (url, title, text) = self.read("text").await?;
            let mut seen = Seen::at(&url);
            let (shown_url, title, text) = (seen.text(&url), seen.text(&title), seen.text(&text));
            Ok(seen.into_output(
                &self.key,
                json!({ "url": shown_url, "title": title, "body_text": text }),
            ))
        })
        .await
    }

    pub(super) async fn get_content(&self, format: &str) -> Result<Value> {
        let what = if format == "text" { "text" } else { "html" };
        self.with(async |_| {
            let (url, _, content) = self.read(what).await?;
            let mut seen = Seen::at(&url);
            let len = content.len();
            let content = seen.text(&content);
            Ok(seen.into_output(
                &self.key,
                json!({ "format": what, "content": content, "size_bytes": len }),
            ))
        })
        .await
    }

    pub(super) async fn screenshot(&self) -> Result<Value> {
        self.with(async |_| {
            let png = self.capture().await?;
            let size = png.len();
            stage_screenshot_artifact(png, size)
        })
        .await
    }

    pub(super) async fn click(&self, selector: &str) -> Result<Value> {
        self.with(async |url| {
            selector_ok(url, Some(selector))?;
            self.hold(
                url,
                "browser.click",
                Some(selector),
                Some(selector),
                true,
                &[],
            )
            .await?;
            self.ask("click", json!({ "selector": selector })).await?;
            self.done("browser.click", Some(selector)).await;
            self.keep_to_sites().await?;
            Ok(json!({ "clicked": selector }))
        })
        .await
    }

    /// Sets a field's value; `resolve` turns its markers into what is
    /// typed, against the page's host.
    pub(super) async fn fill(
        &self,
        selector: &str,
        value: &str,
        resolve: impl FnOnce(&str, Option<&str>) -> Result<String>,
    ) -> Result<Value> {
        self.with(async move |url| {
            selector_ok(url, Some(selector))?;
            self.hold(
                url,
                "browser.fill",
                Some(selector),
                Some(selector),
                false,
                &[],
            )
            .await?;
            let resolved = resolve(value, host_of(url).as_deref())?;
            // The extension types it only on the origin it was resolved for.
            let origin = super::address::origin_of(url);
            self.ask(
                "fill",
                json!({ "selector": selector, "value": resolved, "origin": origin }),
            )
            .await?;
            self.done("browser.fill", Some(selector)).await;
            Ok(json!({ "filled": true }))
        })
        .await
    }

    pub(super) async fn type_text(
        &self,
        selector: &str,
        text: &str,
        resolve: impl FnOnce(&str, Option<&str>) -> Result<String>,
    ) -> Result<Value> {
        self.with(async move |url| {
            selector_ok(url, Some(selector))?;
            self.hold(
                url,
                "browser.type",
                Some(selector),
                Some(selector),
                false,
                &[],
            )
            .await?;
            let resolved = resolve(text, host_of(url).as_deref())?;
            let origin = super::address::origin_of(url);
            self.ask(
                "type",
                json!({ "selector": selector, "text": resolved, "origin": origin }),
            )
            .await?;
            self.done("browser.type", Some(selector)).await;
            Ok(json!({ "typed_chars": resolved.chars().count() }))
        })
        .await
    }

    pub(super) async fn press_key(&self, key: &str, selector: Option<&str>) -> Result<Value> {
        self.with(async |url| {
            selector_ok(url, selector)?;
            self.hold(url, "browser.press_key", Some(key), selector, true, &[])
                .await?;
            self.ask("press", json!({ "selector": selector, "key": key }))
                .await?;
            self.done("browser.press_key", Some(key)).await;
            self.keep_to_sites().await?;
            Ok(json!({ "pressed": key }))
        })
        .await
    }

    pub(super) async fn select_option(&self, selector: &str, value: &str) -> Result<Value> {
        self.with(async |url| {
            selector_ok(url, Some(selector))?;
            self.hold(
                url,
                "browser.select_option",
                Some(selector),
                Some(selector),
                false,
                &[],
            )
            .await?;
            self.ask("select", json!({ "selector": selector, "value": value }))
                .await?;
            self.done("browser.select_option", Some(selector)).await;
            Ok(json!({ "selected": true }))
        })
        .await
    }

    pub(super) async fn wait_for_selector(
        &self,
        selector: &str,
        timeout_ms: Option<u64>,
    ) -> Result<Value> {
        let budget_ms = wait_budget_ms(timeout_ms, self.config.call_timeout_ms);
        self.with(async |url| {
            selector_ok(url, Some(selector))?;
            self.ask(
                "wait",
                json!({ "selector": selector, "timeout_ms": budget_ms }),
            )
            .await?;
            Ok(json!({ "found": selector }))
        })
        .await
    }

    pub(super) async fn upload_file(
        &self,
        selector: &str,
        bytes: &[u8],
        name: &str,
        names: &[&str],
    ) -> Result<Value> {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        self.with(async |url| {
            selector_ok(url, Some(selector))?;
            self.hold(url, "browser.upload_file", Some(selector), Some(selector), false, names)
                .await?;
            self.ask(
                "upload",
                json!({ "selector": selector, "files": [{ "name": name, "type": "", "base64": encoded }] }),
            )
            .await?;
            self.done("browser.upload_file", Some(selector)).await;
            Ok(json!({ "uploaded": name, "selector": selector }))
        })
        .await
    }

    /// Reads a new key off a key page and hands it to the hub's secret path,
    /// never to the model: what [`keys`] describes.
    pub(super) async fn capture_key(&self, selector: &str, connector: &str) -> Result<Value> {
        let page = keys::page_for(connector).ok_or_else(|| {
            eyre!("Desk reads a new key only for a connector it sets up, not `{connector}`.")
        })?;
        let Some(hooks) = hooks::hooks() else {
            return Err(eyre!(NO_BROWSER));
        };
        self.with(async |url| {
            selector_ok(url, Some(selector))?;
            // A key is kept for the conversation whose setup asked for it.
            let conversation = CURRENT_SESSION_KEY
                .try_with(|k| k.clone())
                .ok()
                .filter(|k| !k.is_empty());
            let Some(conversation) = conversation else {
                return Err(eyre!(
                    "Desk keeps a new key only for the conversation setting the connector up, and this call came from none."
                ));
            };
            if !keys::matches(&page, url) {
                return Err(eyre!(
                    "Desk's tab isn't on {connector}'s key page, so no key was read."
                ));
            }
            let read = self
                .ask(
                    "capture_key",
                    json!({ "selector": selector, "origin": page.origin, "paths": page.paths }),
                )
                .await?;
            let value = read["value"].as_str().unwrap_or_default().to_string();
            if !keys::shaped(&page, &value) {
                return Err(eyre!(
                    "What Desk read there doesn't look like a {connector} key, so it wasn't kept."
                ));
            }
            // Only its length is said of it: nothing of the key itself.
            let length = value.chars().count();
            let test = hooks
                .keep_key(&self.user, &conversation, connector, &value)
                .map_err(|e| eyre!(e))?;
            drop(value);
            self.done(keys::CAPTURE_KEY, None).await;
            Ok(json!({ "kept": true, "connector": connector, "length": length, "test": test }))
        })
        .await
    }
}
