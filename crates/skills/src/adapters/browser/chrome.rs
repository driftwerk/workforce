//! "My Chrome": the person's own running Chrome, reached over the Chrome
//! DevTools Protocol after Chrome's own consent prompt.
//!
//! Chrome 144 and later turn remote debugging on at
//! `chrome://inspect/#remote-debugging`, and then write `DevToolsActivePort`
//! (its port and browser socket path) into their user data folder. Desk
//! reads that file, connects, and asks Chrome for a window of its own
//! (`Target.createTarget` with `newWindow`). Chrome's protocol has no tab
//! groups, so that window is Desk's tab group: Desk works only in the tab it
//! opened there and closes only that tab.
//!
//! This is a small client of its own, not `chromiumoxide`'s: that one
//! discovers and attaches to every tab a browser has, sets their viewport,
//! and closes a tab whose set-up times out, none of which may happen to the
//! person's tabs. Here nothing is sent to any target but Desk's own.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use eyre::{eyre, Result, WrapErr};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

/// The oldest Chrome whose remote-debugging toggle Desk relies on.
pub const MIN_CHROME: u32 = 144;

/// Where Chrome keeps its user data, by platform: `DevToolsActivePort` is
/// written there while remote debugging is on.
pub fn default_user_data_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if cfg!(target_os = "macos") {
        home.map(|h| h.join("Library/Application Support/Google/Chrome"))
    } else if cfg!(target_os = "windows") {
        std::env::var_os("LOCALAPPDATA").map(|l| PathBuf::from(l).join("Google/Chrome/User Data"))
    } else {
        home.map(|h| h.join(".config/google-chrome"))
    }
}

/// The browser socket `DevToolsActivePort` names: its first line is the
/// port, its second the socket's path.
pub fn socket_of(port_file: &str) -> Option<String> {
    let mut lines = port_file.lines().map(str::trim);
    let port: u16 = lines.next()?.parse().ok()?;
    let path = lines
        .next()
        .filter(|p| p.starts_with("/devtools/browser/"))?;
    (port > 0).then(|| format!("ws://127.0.0.1:{port}{path}"))
}

/// Whether remote debugging is on in the Chrome whose user data is `dir`.
pub fn available(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("DevToolsActivePort"))
        .ok()
        .and_then(|f| socket_of(&f))
        .is_some()
}

/// Chrome's major version from `Browser.getVersion`'s product
/// (`Chrome/144.0.7559.3`, `HeadlessChrome/157.0…`).
pub fn major_of(product: &str) -> Option<u32> {
    product.split_once('/')?.1.split('.').next()?.parse().ok()
}

type Pending = Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

/// Desk's tab in the person's Chrome, attached.
pub struct MyChrome {
    out: mpsc::UnboundedSender<Message>,
    pending: Pending,
    next: AtomicU64,
    target: String,
    session: String,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
    pub(super) network: Arc<Mutex<VecDeque<Value>>>,
    pub(super) console: Arc<Mutex<VecDeque<Value>>>,
    call_timeout: Duration,
    /// Tabs Desk's tab opened (popups): closed with it.
    popups: Arc<std::sync::Mutex<Vec<String>>>,
}

/// Ids for what the reader sends of its own accord, apart from calls'.
/// Chrome reads an id as a 32-bit integer, so they stay below 2^31.
const OWN_IDS: u64 = 1_000_000_000;

/// The user id this process runs as.
#[cfg(unix)]
fn my_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    let probe = tempfile::NamedTempFile::new().ok()?;
    probe.as_file().metadata().ok().map(|m| m.uid())
}

/// Whether what listens on `port` here is a process of this user's: any
/// local process can open a port, and Desk speaks only to the person's own
/// Chrome. Read from the system's socket table (Linux) or `lsof` (macOS).
#[cfg(unix)]
pub fn listener_is_ours(port: u16) -> bool {
    let Some(me) = my_uid() else {
        return false;
    };
    #[cfg(target_os = "linux")]
    {
        let want = format!("{port:04X}");
        ["/proc/net/tcp", "/proc/net/tcp6"].iter().any(|table| {
            std::fs::read_to_string(table).is_ok_and(|t| {
                t.lines().skip(1).any(|line| {
                    let cols: Vec<&str> = line.split_whitespace().collect();
                    cols.len() > 7
                        && cols[3] == "0A"
                        && cols[1].rsplit(':').next() == Some(want.as_str())
                        && cols[7].parse::<u32>() == Ok(me)
                })
            })
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::process::Command::new("lsof")
            .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fu"])
            .output()
            .is_ok_and(|o| {
                let out = String::from_utf8_lossy(&o.stdout).to_string();
                let uids: Vec<u32> = out
                    .lines()
                    .filter_map(|l| l.strip_prefix('u')?.parse().ok())
                    .collect();
                !uids.is_empty() && uids.iter().all(|u| *u == me)
            })
    }
}

#[cfg(not(unix))]
pub fn listener_is_ours(_port: u16) -> bool {
    true
}

impl Drop for MyChrome {
    fn drop(&mut self) {
        // Closing Desk's own tab is asked for on the way out; the socket
        // then goes, and with it Chrome's "being controlled" bar.
        // And every tab it opened, which are Desk's too.
        let popups = std::mem::take(&mut *self.popups.lock().unwrap());
        for target in popups.iter().chain([&self.target]) {
            let close = json!({
                "id": self.next.fetch_add(1, Ordering::Relaxed),
                "method": "Target.closeTarget",
                "params": { "targetId": target },
            });
            let _ = self.out.send(Message::Text(close.to_string().into()));
        }
        let _ = self.out.send(Message::Close(None));
        let reader = self.reader.abort_handle();
        let writer = self.writer.abort_handle();
        // Let the writer flush the close before both stop.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                writer.abort();
                reader.abort();
            });
        } else {
            writer.abort();
            reader.abort();
        }
    }
}

fn push(log: &mut VecDeque<Value>, capacity: usize, entry: Value) {
    if capacity == 0 {
        return;
    }
    while log.len() >= capacity {
        log.pop_front();
    }
    log.push_back(entry);
}

impl MyChrome {
    /// Attaches to the Chrome whose user data is `dir`: connects (Chrome
    /// asks the person first, so this waits up to `connect_timeout`),
    /// checks its version, opens Desk's window and attaches to its tab.
    pub async fn attach(
        dir: &Path,
        connect_timeout: Duration,
        call_timeout: Duration,
        log_capacity: usize,
    ) -> Result<Self> {
        let file = std::fs::read_to_string(dir.join("DevToolsActivePort")).map_err(|_| {
            eyre!(
                "Your Chrome isn't open to Desk. In Chrome, open chrome://inspect/#remote-debugging and turn remote debugging on, then try again."
            )
        })?;
        let socket =
            socket_of(&file).ok_or_else(|| eyre!("Chrome's DevToolsActivePort can't be read"))?;
        let port: u16 = file
            .lines()
            .next()
            .and_then(|p| p.trim().parse().ok())
            .unwrap_or(0);
        if !listener_is_ours(port) {
            eyre::bail!(
                "What answers on Chrome's debugging port isn't a program of yours, so Desk didn't connect."
            );
        }
        let (ws, _) =
            tokio::time::timeout(connect_timeout, tokio_tungstenite::connect_async(socket))
                .await
                .map_err(|_| {
                    eyre!("Chrome didn't let Desk in: the person didn't allow it in time")
                })?
                .wrap_err("connecting to Chrome failed")?;
        let (mut sink, mut stream) = ws.split();
        let (out, mut rx) = mpsc::unbounded_channel::<Message>();
        let writer = tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                let close = matches!(m, Message::Close(_));
                if sink.send(m).await.is_err() || close {
                    break;
                }
            }
        });
        let pending: Pending = Default::default();
        let network = Arc::new(Mutex::new(VecDeque::with_capacity(log_capacity)));
        let console = Arc::new(Mutex::new(VecDeque::with_capacity(log_capacity)));
        let session_slot: Arc<std::sync::Mutex<Option<String>>> = Default::default();
        let target_slot: Arc<std::sync::Mutex<Option<String>>> = Default::default();
        let popups: Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let reader = {
            let target_slot = Arc::clone(&target_slot);
            let popups = Arc::clone(&popups);
            let own = out.clone();
            let own_ids = AtomicU64::new(OWN_IDS);
            let send_own = move |session: &str, method: &str, params: Value| {
                let msg = json!({
                    "id": own_ids.fetch_add(1, Ordering::Relaxed),
                    "method": method,
                    "params": params,
                    "sessionId": session,
                });
                let _ = own.send(Message::Text(msg.to_string().into()));
            };
            let pending = Arc::clone(&pending);
            let network = Arc::clone(&network);
            let console = Arc::clone(&console);
            let session_slot = Arc::clone(&session_slot);
            tokio::spawn(async move {
                // Desk's tab's main frame, and the page it last committed.
                let mut main: Option<(String, String)> = None;
                while let Some(Ok(msg)) = stream.next().await {
                    let Message::Text(text) = msg else { continue };
                    let Ok(v) = serde_json::from_str::<Value>(&text) else {
                        continue;
                    };
                    if let Some(id) = v["id"].as_u64() {
                        if let Some(tx) = pending.lock().unwrap().remove(&id) {
                            let result = match v.get("error") {
                                Some(e) => Err(eyre!(
                                    "{}",
                                    e["message"].as_str().unwrap_or("chrome error")
                                )),
                                None => Ok(v["result"].clone()),
                            };
                            let _ = tx.send(result);
                        }
                        continue;
                    }
                    // A tab Desk's opened is Desk's, to close with it.
                    if v["method"] == "Target.targetCreated" {
                        let info = &v["params"]["targetInfo"];
                        let mine = target_slot.lock().unwrap().clone();
                        if mine.is_some() && info["openerId"].as_str() == mine.as_deref() {
                            if let Some(id) = info["targetId"].as_str() {
                                popups.lock().unwrap().push(id.to_string());
                            }
                        }
                        continue;
                    }
                    // Events: only Desk's own tab's are kept.
                    let ours = session_slot.lock().unwrap().clone();
                    let Some(ours) = ours.filter(|o| v["sessionId"].as_str() == Some(o.as_str()))
                    else {
                        continue;
                    };
                    let p = &v["params"];
                    match v["method"].as_str() {
                        // Wherever the page takes itself, a page the person's
                        // Chrome may not show Desk is left at once: as it
                        // commits, and again once it has loaded (a navigation
                        // asked for mid-commit can be overtaken by it).
                        Some("Page.frameNavigated") if p["frame"]["parentId"].is_null() => {
                            let url = p["frame"]["url"].as_str().unwrap_or_default().to_string();
                            let frame = p["frame"]["id"].as_str().unwrap_or_default().to_string();
                            if !super::chrome_cell::may_show(&url) {
                                send_own(&ours, "Page.navigate", json!({ "url": "about:blank" }));
                            }
                            main = Some((frame, url));
                        }
                        Some("Page.frameStoppedLoading") => {
                            let stopped = p["frameId"].as_str().unwrap_or_default();
                            if let Some((frame, url)) = main.as_ref() {
                                if frame == stopped && !super::chrome_cell::may_show(url) {
                                    send_own(
                                        &ours,
                                        "Page.navigate",
                                        json!({ "url": "about:blank" }),
                                    );
                                }
                            }
                        }
                        // An alert, confirm or prompt is dismissed, never
                        // answered for the person, and noted.
                        Some("Page.javascriptDialogOpening") => {
                            send_own(
                                &ours,
                                "Page.handleJavaScriptDialog",
                                json!({ "accept": false }),
                            );
                            push(
                                &mut *console.lock().await,
                                log_capacity,
                                json!({
                                    "source": "dialog",
                                    "level": p["type"],
                                    "text": p["message"],
                                    "url": p["url"],
                                    "dismissed": true,
                                }),
                            );
                        }
                        Some("Network.requestWillBeSent") => push(
                            &mut *network.lock().await,
                            log_capacity,
                            json!({
                                "request_id": p["requestId"],
                                "url": p["request"]["url"],
                                "method": p["request"]["method"],
                                "resource_type": p["type"],
                                "timestamp": p["timestamp"],
                            }),
                        ),
                        Some("Runtime.consoleAPICalled") => push(
                            &mut *console.lock().await,
                            log_capacity,
                            json!({
                                "source": "console",
                                "level": p["type"],
                                "args": p["args"].as_array().map(|a| a.iter().map(|x| x.get("value").cloned().unwrap_or(Value::Null)).collect::<Vec<_>>()),
                                "timestamp": p["timestamp"],
                            }),
                        ),
                        Some("Log.entryAdded") => push(
                            &mut *console.lock().await,
                            log_capacity,
                            json!({
                                "source": "log",
                                "level": p["entry"]["level"],
                                "text": p["entry"]["text"],
                                "url": p["entry"]["url"],
                                "timestamp": p["entry"]["timestamp"],
                            }),
                        ),
                        _ => {}
                    }
                }
                // The socket closed: every waiting call fails.
                for (_, tx) in pending.lock().unwrap().drain() {
                    let _ = tx.send(Err(eyre!("Chrome closed the connection")));
                }
            })
        };
        let mut me = Self {
            out,
            pending,
            next: AtomicU64::new(1),
            target: String::new(),
            session: String::new(),
            reader,
            writer,
            network,
            console,
            call_timeout,
            popups,
        };
        // Chrome asks the person before the first command is answered.
        let version = tokio::time::timeout(
            connect_timeout,
            me.send(None, "Browser.getVersion", json!({})),
        )
        .await
        .map_err(|_| eyre!("Chrome didn't let Desk in: the person didn't allow it in time"))??;
        let product = version["product"].as_str().unwrap_or_default();
        match major_of(product) {
            Some(major) if major >= MIN_CHROME => {}
            _ => eyre::bail!(
                "Desk needs Chrome {MIN_CHROME} or later to use your Chrome; this one is {product}. Update Chrome, or use Desk's own browser."
            ),
        }
        let created = me
            .send(
                None,
                "Target.createTarget",
                json!({ "url": "about:blank", "newWindow": true }),
            )
            .await
            .wrap_err("Chrome wouldn't open a window for Desk")?;
        me.target = created["targetId"].as_str().unwrap_or_default().to_string();
        *target_slot.lock().unwrap() = Some(me.target.clone());
        // To learn of the tabs Desk's opens; no other tab is attached to.
        me.send(
            None,
            "Target.setDiscoverTargets",
            json!({ "discover": true }),
        )
        .await
        .wrap_err("Chrome wouldn't say which tabs Desk opens")?;
        let attached = me
            .send(
                None,
                "Target.attachToTarget",
                json!({ "targetId": me.target, "flatten": true }),
            )
            .await
            .wrap_err("attaching to Desk's tab failed")?;
        me.session = attached["sessionId"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        *session_slot.lock().unwrap() = Some(me.session.clone());
        for domain in [
            "Page.enable",
            "Runtime.enable",
            "Network.enable",
            "Log.enable",
        ] {
            me.call(domain, json!({})).await?;
        }
        Ok(me)
    }

    async fn send(&self, session: Option<&str>, method: &str, params: Value) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        self.out
            .send(Message::Text(msg.to_string().into()))
            .map_err(|_| eyre!("Chrome closed the connection"))?;
        rx.await
            .map_err(|_| eyre!("Chrome closed the connection"))?
    }

    /// A command to Desk's own tab, never another.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        tokio::time::timeout(
            self.call_timeout,
            self.send(Some(&self.session), method, params),
        )
        .await
        .map_err(|_| eyre!("Chrome didn't answer {method} in time"))?
    }

    /// Whether the connection is still up.
    pub fn alive(&self) -> bool {
        !self.reader.is_finished()
    }

    /// The value of `expression` in Desk's tab.
    pub async fn evaluate(&self, expression: &str) -> Result<Value> {
        let r = self
            .call(
                "Runtime.evaluate",
                json!({ "expression": expression, "returnByValue": true, "awaitPromise": true }),
            )
            .await?;
        if let Some(e) = r.get("exceptionDetails") {
            // What a page's script throws is the page's words: fenced.
            let thrown = e["exception"]["description"]
                .as_str()
                .or(e["text"].as_str())
                .unwrap_or("script failed");
            let mut seen = super::untrusted::Seen::at("");
            return Err(eyre!("script failed: {}", seen.text(thrown)));
        }
        Ok(r["result"]["value"].clone())
    }

    pub async fn url(&self) -> String {
        self.evaluate("location.href")
            .await
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default()
    }

    /// Goes to `url` and waits for the page to load.
    pub async fn navigate(&self, url: &str) -> Result<()> {
        let r = self.call("Page.navigate", json!({ "url": url })).await?;
        if let Some(e) = r["errorText"].as_str().filter(|e| !e.is_empty()) {
            return Err(eyre!("navigation failed: {e}"));
        }
        self.settle().await;
        Ok(())
    }

    /// Waits, briefly, for the page to finish loading.
    pub async fn settle(&self) {
        let until = Instant::now() + self.call_timeout.min(Duration::from_secs(15));
        while Instant::now() < until {
            if self
                .evaluate("document.readyState")
                .await
                .is_ok_and(|s| s == "complete")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// A real click at the middle of what `selector` names.
    pub async fn click(&self, selector: &str) -> Result<()> {
        let sel = serde_json::to_string(selector)?;
        let at = self
            .evaluate(&format!(
                "(() => {{ const el = document.querySelector({sel}); if (!el) return null; \
                  el.scrollIntoView({{block: 'center', inline: 'center'}}); \
                  const r = el.getBoundingClientRect(); \
                  return [r.left + r.width / 2, r.top + r.height / 2]; }})()"
            ))
            .await?;
        let (Some(x), Some(y)) = (at[0].as_f64(), at[1].as_f64()) else {
            return Err(eyre!("element not found for selector `{selector}`"));
        };
        for kind in ["mousePressed", "mouseReleased"] {
            self.call(
                "Input.dispatchMouseEvent",
                json!({ "type": kind, "x": x, "y": y, "button": "left", "clickCount": 1 }),
            )
            .await?;
        }
        Ok(())
    }

    /// Focuses what `selector` names.
    pub async fn focus(&self, selector: &str) -> Result<()> {
        let sel = serde_json::to_string(selector)?;
        let ok = self
            .evaluate(&format!(
                "(() => {{ const el = document.querySelector({sel}); if (!el) return false; el.focus(); return true; }})()"
            ))
            .await?;
        if ok != Value::Bool(true) {
            return Err(eyre!("element not found for selector `{selector}`"));
        }
        Ok(())
    }

    /// Types `text` into what has focus, as a person typing would.
    pub async fn insert_text(&self, text: &str) -> Result<()> {
        self.call("Input.insertText", json!({ "text": text }))
            .await?;
        Ok(())
    }

    /// Presses `key` on what has focus.
    pub async fn press(&self, key: &str) -> Result<()> {
        let text = match key {
            "Enter" => Some("\r"),
            "Tab" => Some("\t"),
            k if k.chars().count() == 1 => Some(k),
            _ => None,
        };
        for kind in ["keyDown", "keyUp"] {
            let mut params = json!({ "type": kind, "key": key });
            if let (Some(t), "keyDown") = (text, kind) {
                params["text"] = json!(t);
            }
            self.call("Input.dispatchKeyEvent", params).await?;
        }
        Ok(())
    }

    pub async fn screenshot(&self) -> Result<Vec<u8>> {
        let r = self
            .call("Page.captureScreenshot", json!({ "format": "png" }))
            .await?;
        base64::engine::general_purpose::STANDARD
            .decode(r["data"].as_str().unwrap_or_default())
            .wrap_err("screenshot wasn't an image")
    }

    /// The value of `expression` in a world of Desk's own in its tab, which
    /// the page's scripts can't reach or reshape.
    pub async fn evaluate_isolated(&self, expression: &str) -> Result<Value> {
        let tree = self.call("Page.getFrameTree", json!({})).await?;
        let frame = tree["frameTree"]["frame"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let world = self
            .call(
                "Page.createIsolatedWorld",
                json!({ "frameId": frame, "worldName": "desk-browser" }),
            )
            .await?;
        let r = self
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "contextId": world["executionContextId"],
                    "returnByValue": true,
                }),
            )
            .await?;
        Ok(r["result"]["value"].clone())
    }

    /// The node `selector` names, by the DOM's own lookup.
    async fn node(&self, selector: &str) -> Result<i64> {
        let doc = self.call("DOM.getDocument", json!({ "depth": 0 })).await?;
        let root = doc["root"]["nodeId"].as_i64().unwrap_or_default();
        let found = self
            .call(
                "DOM.querySelector",
                json!({ "nodeId": root, "selector": selector }),
            )
            .await?;
        found["nodeId"]
            .as_i64()
            .filter(|n| *n > 0)
            .ok_or_else(|| eyre!("element not found for selector `{selector}`"))
    }

    /// Where Desk would press `selector`: the middle of its content box, by
    /// the DOM's own geometry, and only where the element shown there is
    /// that one (or inside it), not something laid over it.
    pub async fn press_point(&self, selector: &str) -> Result<(f64, f64)> {
        let node = self.node(selector).await?;
        let _ = self
            .call("DOM.scrollIntoViewIfNeeded", json!({ "nodeId": node }))
            .await;
        let quads = self
            .call("DOM.getContentQuads", json!({ "nodeId": node }))
            .await
            .wrap_err("the element has no box on the page")?;
        let q: Vec<f64> = quads["quads"][0]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_f64).collect())
            .unwrap_or_default();
        if q.len() != 8 {
            return Err(eyre!("the element has no box on the page"));
        }
        let x = (q[0] + q[2] + q[4] + q[6]) / 4.0;
        let y = (q[1] + q[3] + q[5] + q[7]) / 4.0;
        let sel = serde_json::to_string(selector)?;
        let hit = self
            .evaluate_isolated(&format!(
                "(() => {{ const el = document.querySelector({sel}); \
                   const at = document.elementFromPoint({x}, {y}); \
                   return !!el && !!at && (el === at || el.contains(at)); }})()"
            ))
            .await?;
        if hit != Value::Bool(true) {
            return Err(eyre!(
                "Something on the page covers `{selector}`, so nothing was pressed."
            ));
        }
        Ok((x, y))
    }

    /// A real click at `(x, y)`.
    pub async fn click_at(&self, x: f64, y: f64) -> Result<()> {
        for kind in ["mousePressed", "mouseReleased"] {
            self.call(
                "Input.dispatchMouseEvent",
                json!({ "type": kind, "x": x, "y": y, "button": "left", "clickCount": 1 }),
            )
            .await?;
        }
        Ok(())
    }

    /// Whether what has focus is what `selector` names: keys go nowhere
    /// else.
    pub async fn focused_is(&self, selector: &str) -> Result<bool> {
        let sel = serde_json::to_string(selector)?;
        Ok(self
            .evaluate_isolated(&format!(
                "document.activeElement === document.querySelector({sel})"
            ))
            .await?
            == Value::Bool(true))
    }

    /// Puts `file` in the file input `selector` names.
    pub async fn set_file(&self, selector: &str, file: &str) -> Result<()> {
        let doc = self.call("DOM.getDocument", json!({ "depth": 0 })).await?;
        let root = doc["root"]["nodeId"].as_i64().unwrap_or_default();
        let found = self
            .call(
                "DOM.querySelector",
                json!({ "nodeId": root, "selector": selector }),
            )
            .await?;
        let node = found["nodeId"]
            .as_i64()
            .filter(|n| *n > 0)
            .ok_or_else(|| eyre!("element not found for selector `{selector}`"))?;
        self.call(
            "DOM.setFileInputFiles",
            json!({ "files": [file], "nodeId": node }),
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_port_file_names_the_browser_socket() {
        assert_eq!(
            socket_of("9222\n/devtools/browser/abc-123\n").as_deref(),
            Some("ws://127.0.0.1:9222/devtools/browser/abc-123")
        );
        for bad in [
            "",
            "9222",
            "x\n/devtools/browser/a",
            "0\n/devtools/browser/a",
            "9222\n/devtools/page/a",
        ] {
            assert_eq!(socket_of(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn chromes_version_is_read_from_its_product() {
        assert_eq!(major_of("Chrome/144.0.7559.3"), Some(144));
        assert_eq!(major_of("HeadlessChrome/157.0.8103.50"), Some(157));
        assert_eq!(major_of("Chrome"), None);
    }

    #[test]
    fn remote_debugging_is_on_only_with_a_readable_port_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!available(dir.path()));
        std::fs::write(
            dir.path().join("DevToolsActivePort"),
            "9222\n/devtools/browser/x\n",
        )
        .unwrap();
        assert!(available(dir.path()));
    }

    #[cfg(unix)]
    #[test]
    fn a_port_this_user_listens_on_is_ours_and_a_closed_one_is_not() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(listener_is_ours(port));
        drop(listener);
        assert!(!listener_is_ours(port));
    }
}
