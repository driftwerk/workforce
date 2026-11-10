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
}

impl Drop for MyChrome {
    fn drop(&mut self) {
        // Closing Desk's own tab is asked for on the way out; the socket
        // then goes, and with it Chrome's "being controlled" bar.
        let close = json!({
            "id": self.next.fetch_add(1, Ordering::Relaxed),
            "method": "Target.closeTarget",
            "params": { "targetId": self.target },
        });
        let _ = self.out.send(Message::Text(close.to_string().into()));
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
        let reader = {
            let pending = Arc::clone(&pending);
            let network = Arc::clone(&network);
            let console = Arc::clone(&console);
            let session_slot = Arc::clone(&session_slot);
            tokio::spawn(async move {
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
                    // Events: only Desk's own tab's are kept.
                    let ours = session_slot.lock().unwrap().clone();
                    if ours.is_none() || v["sessionId"].as_str() != ours.as_deref() {
                        continue;
                    }
                    let p = &v["params"];
                    match v["method"].as_str() {
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
            return Err(eyre!(
                "{}",
                e["exception"]["description"]
                    .as_str()
                    .or(e["text"].as_str())
                    .unwrap_or("script failed")
            ));
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
}
