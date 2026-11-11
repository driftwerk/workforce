//! One `(agent, user)`'s tab in the person's Chrome ([`MyChrome`]),
//! attached lazily on its first call and let go on Stop, at the end of the
//! turn, and after its idle time. Every tool reads and acts as in Desk's
//! own browser: page text fenced as untrusted, the person's own steps held
//! back below unbound, what was done recorded. And the person's site list
//! is kept: a blocked site is never opened, and a click that lands on one
//! is taken back to a blank page.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use eyre::{eyre, Result};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use workforce_agent::runner::{CURRENT_SESSION_ID, CURRENT_SESSION_KEY};

use super::chrome::MyChrome;
use super::client::Decided;
use super::client::{
    fill_expression, host_of, select_option_expression, shown_expression,
    stage_screenshot_artifact, wait_budget_ms, POLL_BACKOFF_MS,
};
use super::config::BrowserSkillConfig;
use super::hooks::{self, Done, OnPage};
use super::untrusted::{self, Seen};

pub(super) struct ChromeCell {
    config: Arc<BrowserSkillConfig>,
    pub(super) key: String,
    pub(super) agent: String,
    pub(super) user: String,
    /// The session (id) its last call came from.
    pub(super) last_session: StdMutex<Option<String>>,
    /// That conversation's key, which its allowances are kept under.
    pub(super) last_key: StdMutex<Option<String>>,
    pub(super) last_used: StdMutex<Instant>,
    /// The site a call the person allowed opened: it may show without the
    /// gate allowing a visit again.
    visited: StdMutex<Option<String>>,
    /// The site the call being made now was allowed to open.
    pending_visit: StdMutex<Option<String>>,
    pub(super) inner: Mutex<Option<MyChrome>>,
    dir: PathBuf,
}

impl Drop for ChromeCell {
    fn drop(&mut self) {
        untrusted::forget(&self.key);
    }
}

/// Whether the person's Chrome may show Desk `url`: the web and a blank
/// page only, and a site the person's list allows.
pub(super) fn may_show(url: &str) -> bool {
    use super::address::{kind, openable, In};
    let host = host_of(url);
    openable(&kind(url), In::Chrome) && hooks::settings().sites.allows(host.as_deref())
}

/// Where the person's Chrome may not be, as a refusal.
fn kept_away(host: &str) -> eyre::Report {
    eyre!("Desk keeps away from {host} in your Chrome (Settings → Browser), so it wasn't opened.")
}

impl ChromeCell {
    pub(super) fn new(
        config: Arc<BrowserSkillConfig>,
        (key, agent, user): (String, String, String),
        dir: PathBuf,
    ) -> Self {
        Self {
            config,
            key,
            agent,
            user,
            last_session: StdMutex::new(None),
            last_key: StdMutex::new(None),
            last_used: StdMutex::new(Instant::now()),
            visited: StdMutex::new(None),
            pending_visit: StdMutex::new(None),
            inner: Mutex::new(None),
            dir,
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

    /// Before every call: the page Desk's tab is on now is one it may be
    /// on (the web or blank, a site the list allows, one this session may
    /// visit), else it is blanked and nothing is done; and a call decided
    /// for one origin acts there or not at all.
    async fn keep_page(&self, c: &MyChrome, decided: Option<Decided>) -> Result<()> {
        let url = self.url_of(c).await;
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
            let _ = c.navigate("about:blank").await;
            untrusted::on(&self.key, "about:blank");
            return Err(eyre!("{why} Desk's tab is blank now."));
        }
        if let Some(Decided::Origin(decided)) = decided {
            if super::address::origin_of(&url) != decided {
                return Err(eyre!(
                    "The page moved since this was decided, so nothing was done. Call again."
                ));
            }
        }
        Ok(())
    }

    /// Whether it is attached now.
    pub(super) fn attached(&self) -> bool {
        match self.inner.try_lock() {
            Ok(g) => g.as_ref().is_some_and(MyChrome::alive),
            Err(_) => true,
        }
    }

    /// Lets the person's Chrome go: Desk's tab closes, the connection ends.
    pub(super) async fn detach(&self) -> bool {
        self.inner.lock().await.take().is_some()
    }

    /// Runs `op` on Desk's tab, attaching first where it isn't.
    async fn with<T>(&self, op: impl AsyncFnOnce(&MyChrome) -> Result<T>) -> Result<T> {
        self.touch();
        let mut guard = self.inner.lock().await;
        if guard.as_ref().is_some_and(|c| !c.alive()) {
            *guard = None;
        }
        if guard.is_none() {
            let attached = MyChrome::attach(
                &self.dir,
                Duration::from_millis(self.config.chrome_connect_timeout_ms),
                Duration::from_millis(self.config.call_timeout_ms),
                self.config
                    .network_log_capacity
                    .max(self.config.console_log_capacity),
            )
            .await?;
            attached.scan_into(&self.key);
            *guard = Some(attached);
        }
        let chrome = guard.as_ref().expect("just attached");
        let decided = super::client::take_decided(super::client::Backend::Chrome);
        self.keep_page(chrome, decided.clone()).await?;
        // The site the person allowed this very call to open: it stays
        // allowed only if the call lands there ([`Self::navigate`]).
        *self.pending_visit.lock().unwrap() = match decided {
            Some(Decided::Visit(origin)) => origin,
            _ => None,
        };
        let out = op(chrome).await;
        *self.pending_visit.lock().unwrap() = None;
        if out.is_err() && !chrome.alive() {
            *guard = None;
        }
        out
    }

    async fn url_of(&self, c: &MyChrome) -> String {
        let url = c.url().await;
        untrusted::on(&self.key, &url);
        url
    }

    /// After a change: a page the site list keeps Desk from is left at once.
    async fn keep_to_sites(&self, c: &MyChrome) -> Result<()> {
        c.settle().await;
        let url = self.url_of(c).await;
        let host = host_of(&url).filter(|_| !url.starts_with("about:"));
        if hooks::settings().sites.allows(host.as_deref()) {
            return Ok(());
        }
        let _ = c.navigate("about:blank").await;
        Err(kept_away(host.as_deref().unwrap_or(&url)))
    }

    /// Holds back what is the person's to do (paying, signing in or out,
    /// a credential's file), read from what the page shows in a world of
    /// Desk's own; for a click, also where it lands, which must be the
    /// element read and not something laid over it.
    async fn hold(
        &self,
        c: &MyChrome,
        tool: &str,
        target: Option<&str>,
        selector: Option<&str>,
        form: bool,
        files: &[&str],
    ) -> Result<Option<(f64, f64)>> {
        let at = match (tool, selector) {
            ("browser.click", Some(sel)) => Some(c.press_point(sel).await?),
            _ => None,
        };
        let Some(hooks) = hooks::hooks() else {
            return Ok(at);
        };
        if hooks.unbound(&self.agent) {
            return Ok(at);
        }
        let shown = c
            .evaluate_isolated(&shown_expression(selector, form))
            .await
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let what = OnPage {
            tool: tool.to_string(),
            url: self.url_of(c).await,
            target: target.map(str::to_string),
            shown,
            files: files.iter().map(|f| f.to_string()).collect(),
        };
        match hooks.holds(&self.agent, &what) {
            Some(why) => Err(eyre!("{why}")),
            None => Ok(at),
        }
    }

    /// Keys go only where `selector` has focus.
    async fn focus_on(&self, c: &MyChrome, selector: &str) -> Result<()> {
        c.focus(selector).await?;
        if !c.focused_is(selector).await? {
            return Err(eyre!(
                "`{selector}` didn't take the focus, so nothing was typed."
            ));
        }
        Ok(())
    }

    async fn done(&self, c: &MyChrome, tool: &str, target: Option<&str>) {
        let Some(hooks) = hooks::hooks() else {
            return;
        };
        let url = self.url_of(c).await;
        let screenshot = match tool {
            "browser.fill" | "browser.type" => None,
            _ => c.screenshot().await.ok(),
        };
        hooks.record(Done {
            agent: self.agent.clone(),
            user: self.user.clone(),
            session: self.last_session.lock().unwrap().clone(),
            tool: tool.to_string(),
            backend: "chrome",
            url,
            target: target.map(str::to_string),
            screenshot,
        });
    }

    pub(super) async fn navigate(&self, url: &str) -> Result<Value> {
        // In the person's Chrome: web pages and a blank page only, never
        // their files, an inline page or the browser's own pages.
        if !super::address::openable(&super::address::kind(url), super::address::In::Chrome) {
            return Err(eyre!(
                "In your Chrome Desk opens web pages and about:blank only; use Desk's own browser (`browser: \"desk\"`) for a file or an inline page."
            ));
        }
        let host = host_of(url).filter(|_| !url.starts_with("about:"));
        if !hooks::settings().sites.allows(host.as_deref()) {
            return Err(kept_away(host.as_deref().unwrap_or(url)));
        }
        self.with(async |c| {
            c.navigate(url).await?;
            // Opened, by the call the person allowed, exactly the site it was
            // allowed to: it may stay.
            let landed = super::address::origin_of(&c.url().await);
            if landed.is_some() && landed == *self.pending_visit.lock().unwrap() {
                *self.visited.lock().unwrap() = landed;
            }
            self.done(c, "browser.navigate", Some(url)).await;
            self.keep_to_sites(c).await?;
            let title = c.evaluate("document.title").await.unwrap_or_default();
            let final_url = self.url_of(c).await;
            let mut seen = Seen::at(&final_url);
            let title = seen.text(title.as_str().unwrap_or_default());
            let shown_url = seen.text(&final_url);
            Ok(seen.into_output(
                &self.key,
                json!({ "url": shown_url, "title": title, "browser": "chrome" }),
            ))
        })
        .await
    }

    pub(super) async fn snapshot(&self) -> Result<Value> {
        self.with(async |c| {
            let title = c.evaluate("document.title").await.unwrap_or_default();
            let body = c
                .evaluate("document.body ? document.body.innerText : ''")
                .await
                .unwrap_or_default();
            let url = self.url_of(c).await;
            let mut seen = Seen::at(&url);
            let title = seen.text(title.as_str().unwrap_or_default());
            let body = seen.text(body.as_str().unwrap_or_default());
            let url = seen.text(&url);
            Ok(seen.into_output(
                &self.key,
                json!({ "url": url, "title": title, "body_text": body, "browser": "chrome" }),
            ))
        })
        .await
    }

    pub(super) async fn screenshot(&self) -> Result<Value> {
        self.with(async |c| {
            let png = c.screenshot().await?;
            let size = png.len();
            stage_screenshot_artifact(png, size)
        })
        .await
    }

    pub(super) async fn click(&self, selector: &str) -> Result<Value> {
        self.with(async |c| {
            let at = self
                .hold(
                    c,
                    "browser.click",
                    Some(selector),
                    Some(selector),
                    true,
                    &[],
                )
                .await?;
            match at {
                Some((x, y)) => c.click_at(x, y).await?,
                None => c.click(selector).await?,
            }
            self.done(c, "browser.click", Some(selector)).await;
            self.keep_to_sites(c).await?;
            Ok(json!({ "clicked": selector }))
        })
        .await
    }

    /// Sets a field's value; `resolve` turns its markers into what is typed,
    /// against the page's host.
    pub(super) async fn fill(
        &self,
        selector: &str,
        value: &str,
        resolve: impl FnOnce(&str, Option<&str>) -> Result<String>,
    ) -> Result<Value> {
        self.with(async move |c| {
            self.hold(
                c,
                "browser.fill",
                Some(selector),
                Some(selector),
                false,
                &[],
            )
            .await?;
            let url = self.url_of(c).await;
            let resolved = resolve(value, host_of(&url).as_deref())?;
            let ok = c.evaluate(&fill_expression(selector, &resolved)).await?;
            if ok != Value::Bool(true) {
                return Err(eyre!("element not found for selector"));
            }
            self.done(c, "browser.fill", Some(selector)).await;
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
        self.with(async move |c| {
            self.hold(
                c,
                "browser.type",
                Some(selector),
                Some(selector),
                false,
                &[],
            )
            .await?;
            let url = self.url_of(c).await;
            let resolved = resolve(text, host_of(&url).as_deref())?;
            self.focus_on(c, selector).await?;
            c.insert_text(&resolved).await?;
            self.done(c, "browser.type", Some(selector)).await;
            Ok(json!({ "typed_chars": resolved.chars().count() }))
        })
        .await
    }

    pub(super) async fn press_key(&self, key: &str, selector: Option<&str>) -> Result<Value> {
        self.with(async |c| {
            self.hold(c, "browser.press_key", Some(key), selector, true, &[])
                .await?;
            if let Some(sel) = selector {
                self.focus_on(c, sel).await?;
            }
            c.press(key).await?;
            self.done(c, "browser.press_key", Some(key)).await;
            self.keep_to_sites(c).await?;
            Ok(json!({ "pressed": key }))
        })
        .await
    }

    pub(super) async fn select_option(&self, selector: &str, value: &str) -> Result<Value> {
        self.with(async |c| {
            self.hold(
                c,
                "browser.select_option",
                Some(selector),
                Some(selector),
                false,
                &[],
            )
            .await?;
            let ok = c
                .evaluate(&select_option_expression(selector, value))
                .await?;
            if ok != Value::Bool(true) {
                return Err(eyre!("element or option not found"));
            }
            self.done(c, "browser.select_option", Some(selector)).await;
            Ok(json!({ "selected": true }))
        })
        .await
    }

    pub(super) async fn evaluate(&self, expression: &str) -> Result<Value> {
        self.with(async |c| {
            let value = c.evaluate(expression).await?;
            self.done(c, "browser.evaluate", None).await;
            self.keep_to_sites(c).await?;
            let url = self.url_of(c).await;
            let mut seen = Seen::at(&url);
            let value = seen.value(value);
            Ok(seen.into_output(&self.key, json!({ "result": value })))
        })
        .await
    }

    pub(super) async fn wait_for_selector(
        &self,
        selector: &str,
        timeout_ms: Option<u64>,
    ) -> Result<Value> {
        let budget_ms = wait_budget_ms(timeout_ms, self.config.call_timeout_ms);
        self.with(async |c| {
            let sel = serde_json::to_string(selector)?;
            let deadline = Instant::now() + Duration::from_millis(budget_ms);
            loop {
                if c.evaluate(&format!("document.querySelector({sel}) !== null"))
                    .await
                    .is_ok_and(|v| v == Value::Bool(true))
                {
                    return Ok(json!({ "found": selector }));
                }
                if Instant::now() >= deadline {
                    return Err(eyre!(
                        "selector `{selector}` not present after {budget_ms}ms"
                    ));
                }
                tokio::time::sleep(Duration::from_millis(POLL_BACKOFF_MS)).await;
            }
        })
        .await
    }

    pub(super) async fn get_content(&self, format: &str) -> Result<Value> {
        let expr = if format == "text" {
            "document.body ? document.body.innerText : ''"
        } else {
            "document.documentElement ? document.documentElement.outerHTML : ''"
        };
        self.with(async |c| {
            let content = c.evaluate(expr).await?;
            let content = content.as_str().unwrap_or_default();
            let url = self.url_of(c).await;
            let mut seen = Seen::at(&url);
            let len = content.len();
            let content = seen.text(content);
            Ok(seen.into_output(
                &self.key,
                json!({ "format": format, "content": content, "size_bytes": len }),
            ))
        })
        .await
    }

    pub(super) async fn network_requests(&self) -> Result<Value> {
        self.with(async |c| {
            let entries: Vec<Value> = c.network.lock().await.iter().cloned().collect();
            let url = self.url_of(c).await;
            let mut seen = Seen::at(&url);
            let count = entries.len();
            let requests = seen.value(Value::Array(entries));
            Ok(seen.into_output(&self.key, json!({ "count": count, "requests": requests })))
        })
        .await
    }

    pub(super) async fn console_messages(&self) -> Result<Value> {
        self.with(async |c| {
            let entries: Vec<Value> = c.console.lock().await.iter().cloned().collect();
            let url = self.url_of(c).await;
            let mut seen = Seen::at(&url);
            let count = entries.len();
            let messages = seen.value(Value::Array(entries));
            Ok(seen.into_output(&self.key, json!({ "count": count, "messages": messages })))
        })
        .await
    }

    pub(super) async fn upload_file(
        &self,
        selector: &str,
        path: &str,
        name: &str,
        names: &[&str],
    ) -> Result<Value> {
        self.with(async |c| {
            self.hold(
                c,
                "browser.upload_file",
                Some(selector),
                Some(selector),
                false,
                names,
            )
            .await?;
            c.set_file(selector, path).await?;
            self.done(c, "browser.upload_file", Some(selector)).await;
            Ok(json!({ "uploaded": name, "selector": selector }))
        })
        .await
    }
}
