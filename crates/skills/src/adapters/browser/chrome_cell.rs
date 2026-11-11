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

use workforce_agent::runner::CURRENT_SESSION_KEY;

use super::chrome::MyChrome;
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
    pub(super) last_session: StdMutex<Option<String>>,
    pub(super) last_used: StdMutex<Instant>,
    pub(super) inner: Mutex<Option<MyChrome>>,
    dir: PathBuf,
}

impl Drop for ChromeCell {
    fn drop(&mut self) {
        untrusted::forget(&self.key);
    }
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
            last_used: StdMutex::new(Instant::now()),
            inner: Mutex::new(None),
            dir,
        }
    }

    fn touch(&self) {
        *self.last_used.lock().unwrap() = Instant::now();
        if let Some(session) = CURRENT_SESSION_KEY
            .try_with(|k| k.clone())
            .ok()
            .filter(|k| !k.is_empty())
        {
            *self.last_session.lock().unwrap() = Some(session);
        }
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
            *guard = Some(attached);
        }
        let chrome = guard.as_ref().expect("just attached");
        let out = op(chrome).await;
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

    async fn hold(
        &self,
        c: &MyChrome,
        tool: &str,
        target: Option<&str>,
        selector: Option<&str>,
        form: bool,
        file: Option<&str>,
    ) -> Result<()> {
        let Some(hooks) = hooks::hooks() else {
            return Ok(());
        };
        if hooks.unbound(&self.agent) {
            return Ok(());
        }
        let shown = c
            .evaluate(&shown_expression(selector, form))
            .await
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let what = OnPage {
            tool: tool.to_string(),
            url: self.url_of(c).await,
            target: target.map(str::to_string),
            shown,
            file: file.map(str::to_string),
        };
        match hooks.holds(&self.agent, &what) {
            Some(why) => Err(eyre!("{why}")),
            None => Ok(()),
        }
    }

    async fn done(&self, c: &MyChrome, tool: &str, target: Option<&str>) {
        let Some(hooks) = hooks::hooks() else {
            return;
        };
        let url = self.url_of(c).await;
        let screenshot = c.screenshot().await.ok();
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
        // their files or the browser's own pages.
        let scheme = url
            .trim()
            .split(':')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !matches!(scheme.as_str(), "http" | "https" | "about") {
            return Err(eyre!(
                "In your Chrome Desk opens web pages only, not `{scheme}`; use Desk's own browser (`browser: \"desk\"`) for a file or an inline page."
            ));
        }
        let host = host_of(url).filter(|_| !url.starts_with("about:"));
        if !hooks::settings().sites.allows(host.as_deref()) {
            return Err(kept_away(host.as_deref().unwrap_or(url)));
        }
        self.with(async |c| {
            c.navigate(url).await?;
            self.keep_to_sites(c).await?;
            let title = c.evaluate("document.title").await.unwrap_or_default();
            let final_url = self.url_of(c).await;
            self.done(c, "browser.navigate", Some(url)).await;
            let mut seen = Seen::at(&final_url);
            let title = seen.text(title.as_str().unwrap_or_default());
            Ok(seen.into_output(
                &self.key,
                json!({ "url": final_url, "title": title, "browser": "chrome" }),
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
            self.hold(
                c,
                "browser.click",
                Some(selector),
                Some(selector),
                true,
                None,
            )
            .await?;
            c.click(selector).await?;
            self.keep_to_sites(c).await?;
            self.done(c, "browser.click", Some(selector)).await;
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
                None,
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
                None,
            )
            .await?;
            let url = self.url_of(c).await;
            let resolved = resolve(text, host_of(&url).as_deref())?;
            c.focus(selector).await?;
            c.insert_text(&resolved).await?;
            self.done(c, "browser.type", Some(selector)).await;
            Ok(json!({ "typed_chars": resolved.chars().count() }))
        })
        .await
    }

    pub(super) async fn press_key(&self, key: &str, selector: Option<&str>) -> Result<Value> {
        self.with(async |c| {
            self.hold(c, "browser.press_key", Some(key), selector, true, None)
                .await?;
            if let Some(sel) = selector {
                c.focus(sel).await?;
            }
            c.press(key).await?;
            self.keep_to_sites(c).await?;
            self.done(c, "browser.press_key", Some(key)).await;
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
                None,
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
            self.keep_to_sites(c).await?;
            self.done(c, "browser.evaluate", None).await;
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
    ) -> Result<Value> {
        self.with(async |c| {
            self.hold(
                c,
                "browser.upload_file",
                Some(selector),
                Some(selector),
                false,
                Some(name),
            )
            .await?;
            c.set_file(selector, path).await?;
            self.done(c, "browser.upload_file", Some(selector)).await;
            Ok(json!({ "uploaded": name, "selector": selector }))
        })
        .await
    }
}
