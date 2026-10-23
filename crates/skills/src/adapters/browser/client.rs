use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant, SystemTime};

use base64::Engine;
use chromiumoxide::browser::{BrowserConfig, BrowserConfigBuilder};
use chromiumoxide::cdp::browser_protocol::dom::SetFileInputFilesParams;
use chromiumoxide::cdp::browser_protocol::log::EventEntryAdded;
use chromiumoxide::cdp::browser_protocol::network::EventRequestWillBeSent;
use chromiumoxide::cdp::browser_protocol::page::PrintToPdfParams;
use chromiumoxide::cdp::js_protocol::runtime::EventConsoleApiCalled;
use chromiumoxide::handler::viewport::Viewport;
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide::{Browser, Handler, Page};
use eyre::{eyre, Result, WrapErr};
use futures::StreamExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tracing::{debug, warn};

use workforce_agent::artifacts::store::global_store;
use workforce_agent::artifacts::types::{ArtifactBody, ArtifactDescriptor, ContentType};
use workforce_agent::runner::{CURRENT_AGENT_ID, CURRENT_USER_ID, SYSTEM_USER_ID};
use workforce_agent::tools::image_gen::GENERATED_IMAGES_KEY;
use workforce_common::settings::SecretStore;

use crate::adapters::credential_resolver::resolve_vault_refs;

use super::config::{BrowserSkillConfig, MAX_WAIT_MS};

const VAULT_PREFIX: &str = "browser_creds/";
const MIN_SCREENSHOT_BYTES: usize = 200;
const SIZE_DELTA_WARN_RATIO: f64 = 0.5;

const RESPAWN_WINDOW_MS: u64 = 60_000;
const RESPAWN_LIMIT: usize = 3;
const POLL_BACKOFF_MS: u64 = 100;
/// Consecutive timed-out calls after which a session is torn down. A CDP
/// connection that has silently died answers nothing, so every call times out;
/// two in a row is the signal, while a single slow page is not.
const TIMEOUT_RECYCLE_LIMIT: usize = 2;

/// The polling budget a `browser.wait` call gets: the requested `timeout_ms`,
/// else the configured call timeout, never more than [`MAX_WAIT_MS`].
fn wait_budget_ms(requested: Option<u64>, default_ms: u64) -> u64 {
    requested.unwrap_or(default_ms).min(MAX_WAIT_MS)
}

const DEFAULT_AGENT: &str = "default";
const PROFILE_ROOT_NAME: &str = "workforce-browser";
const LEGACY_PROFILE_DIR: &str = "chromiumoxide-runner";

/// Stable, filesystem-safe handle for one `(agent, user)` browser session. The
/// digest both keys the in-memory cell registry and names the on-disk Chromium
/// profile directory, so login state is physically partitioned per user.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SessionId(String);

impl SessionId {
    fn derive(agent: &str, user: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(agent.as_bytes());
        hasher.update([0x1f]); // unit separator so ("a","bc") != ("ab","c")
        hasher.update(user.as_bytes());
        SessionId(lower_hex(&hasher.finalize()))
    }
}

fn lower_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Identity of the in-scope request, falling back to the system actor when no
/// user is scoped (only system/isolated contexts reach the browser unscoped;
/// real users always carry a uid) and to a fixed agent segment outside an agent
/// context (legacy construction, tests).
fn current_session_id() -> SessionId {
    let agent = CURRENT_AGENT_ID
        .try_with(|a| a.clone())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_AGENT.to_string());
    let user = CURRENT_USER_ID
        .try_with(|u| u.clone())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| SYSTEM_USER_ID.to_string());
    SessionId::derive(&agent, &user)
}

/// Snapshot of a cell taken under the registry lock, fed to the pure eviction
/// decisions. `live` = has a running Chromium; `inflight` = a call is in flight
/// or the cell is mid-launch (never evict these).
#[derive(Clone)]
struct CellStat {
    id: SessionId,
    live: bool,
    inflight: bool,
    last_used: Instant,
}

fn select_idle_evictions(stats: &[CellStat], now: Instant, idle_ttl: Duration) -> Vec<SessionId> {
    stats
        .iter()
        .filter(|s| !s.inflight && now.saturating_duration_since(s.last_used) >= idle_ttl)
        .map(|s| s.id.clone())
        .collect()
}

fn select_lru_evictions(stats: &[CellStat], overflow: usize) -> Vec<SessionId> {
    if overflow == 0 {
        return Vec::new();
    }
    let mut evictable: Vec<&CellStat> = stats.iter().filter(|s| s.live && !s.inflight).collect();
    evictable.sort_by_key(|s| s.last_used);
    evictable
        .into_iter()
        .take(overflow)
        .map(|s| s.id.clone())
        .collect()
}

fn should_reap(last: Instant, now: Instant, interval: Duration) -> bool {
    now.saturating_duration_since(last) >= interval
}

fn reap_targets(
    entries: &[(PathBuf, SystemTime)],
    live: &HashSet<PathBuf>,
    now: SystemTime,
    retention: Duration,
) -> Vec<PathBuf> {
    entries
        .iter()
        .filter(|(path, _)| !live.contains(path))
        .filter(|(_, mtime)| {
            now.duration_since(*mtime)
                .map(|age| age >= retention)
                .unwrap_or(false)
        })
        .map(|(path, _)| path.clone())
        .collect()
}

/// RAII counter that marks a cell as having a call in flight so concurrent
/// eviction never tears its Chromium down mid-op.
struct InflightGuard<'a>(&'a AtomicUsize);

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// One Chromium subprocess + chromiumoxide connection + the single tab it drives.
///
/// All tools share `call_lock` so that browser-state-mutating ops are serialised
/// per session. Event listener tasks push into bounded ring buffers exposed by
/// the `network_requests` and `console_messages` tools. Dropping `Active`
/// drops `_browser`, which signals chromiumoxide to terminate the Chromium
/// child it spawned.
struct Active {
    page: Page,
    _browser: Browser,
    _handler_task: JoinHandle<()>,
    _network_task: JoinHandle<()>,
    _console_log_task: JoinHandle<()>,
    _console_api_task: JoinHandle<()>,
    network_log: Arc<Mutex<VecDeque<Value>>>,
    console_log: Arc<Mutex<VecDeque<Value>>>,
    call_lock: Arc<Mutex<()>>,
    last_screenshot_size: Arc<Mutex<Option<usize>>>,
}

/// Lazy, drop-safe handle to one user's headless-Chromium session: a single
/// Chromium lifecycle bound to an isolated on-disk `profile_dir`.
struct SessionCell {
    config: Arc<BrowserSkillConfig>,
    profile_dir: PathBuf,
    inner: Mutex<Option<Active>>,
    respawn: Mutex<RespawnBudget>,
    inflight: AtomicUsize,
    last_used: StdMutex<Instant>,
    consecutive_timeouts: AtomicUsize,
}

impl SessionCell {
    fn new(config: Arc<BrowserSkillConfig>, profile_dir: PathBuf, now: Instant) -> Self {
        Self {
            config,
            profile_dir,
            inner: Mutex::new(None),
            respawn: Mutex::new(RespawnBudget::new(RESPAWN_WINDOW_MS, RESPAWN_LIMIT)),
            inflight: AtomicUsize::new(0),
            last_used: StdMutex::new(now),
            consecutive_timeouts: AtomicUsize::new(0),
        }
    }

    fn stat(&self, id: SessionId) -> CellStat {
        let (live, busy) = match self.inner.try_lock() {
            Ok(guard) => (guard.is_some(), false),
            Err(_) => (true, true),
        };
        CellStat {
            id,
            live,
            inflight: busy || self.inflight.load(Ordering::Relaxed) > 0,
            last_used: *self.last_used.lock().unwrap(),
        }
    }

    fn touch(&self, now: Instant) {
        *self.last_used.lock().unwrap() = now;
    }

    fn call_timeout(&self) -> Duration {
        Duration::from_millis(self.config.call_timeout_ms)
    }

    async fn ensure_active(&self) -> Result<tokio::sync::MutexGuard<'_, Option<Active>>> {
        let mut guard = self.inner.lock().await;
        // The handler stream ends when Chromium exits or the CDP socket drops.
        // Its task finishing is therefore the one reliable "browser is gone"
        // signal; without this check a dead browser stayed resident and every
        // later call failed or timed out until the pod restarted.
        if guard
            .as_ref()
            .is_some_and(|active| active._handler_task.is_finished())
        {
            warn!(target: "browser", "cdp handler ended; relaunching browser session");
            *guard = None;
        }
        if guard.is_none() {
            self.respawn.lock().await.record()?;
            let active = self.start().await?;
            *guard = Some(active);
        }
        Ok(guard)
    }

    async fn start(&self) -> Result<Active> {
        ensure_profile_dir(&self.profile_dir)?;
        let (browser, mut handler) = launch_chromium(&self.config, &self.profile_dir).await?;

        let handler_task = tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                if let Err(e) = event {
                    debug!(target: "browser", error = ?e, "cdp handler event");
                }
            }
        });

        let page = browser
            .new_page("about:blank")
            .await
            .wrap_err("failed to open initial browser page")?;

        if let Some(ua) = self.config.user_agent.as_deref().filter(|s| !s.is_empty()) {
            if let Err(e) = page.set_user_agent(ua).await {
                warn!(target: "browser", error = %e, "failed to set user agent");
            }
        }

        let network_log = Arc::new(Mutex::new(VecDeque::with_capacity(
            self.config.network_log_capacity,
        )));
        let console_log = Arc::new(Mutex::new(VecDeque::with_capacity(
            self.config.console_log_capacity,
        )));

        let network_task =
            spawn_network_listener(&page, network_log.clone(), self.config.network_log_capacity)
                .await?;
        let (console_log_task, console_api_task) =
            spawn_console_listeners(&page, console_log.clone(), self.config.console_log_capacity)
                .await?;

        Ok(Active {
            page,
            _browser: browser,
            _handler_task: handler_task,
            _network_task: network_task,
            _console_log_task: console_log_task,
            _console_api_task: console_api_task,
            network_log,
            console_log,
            call_lock: Arc::new(Mutex::new(())),
            last_screenshot_size: Arc::new(Mutex::new(None)),
        })
    }

    async fn with_page<F, Fut, T>(&self, op: F) -> Result<T>
    where
        F: FnOnce(Page, Arc<Mutex<VecDeque<Value>>>, Arc<Mutex<VecDeque<Value>>>) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        self.inflight.fetch_add(1, Ordering::Relaxed);
        let _inflight = InflightGuard(&self.inflight);
        self.touch(Instant::now());
        let timeout_dur = self.call_timeout();
        let (page, network_log, console_log, call_lock) = {
            let mut guard = self.ensure_active().await?;
            let active = guard.as_mut().expect("just ensured present");
            (
                active.page.clone(),
                active.network_log.clone(),
                active.console_log.clone(),
                active.call_lock.clone(),
            )
        };
        let _serial = call_lock.lock().await;
        match timeout(timeout_dur, op(page, network_log, console_log)).await {
            Ok(Ok(v)) => {
                self.consecutive_timeouts.store(0, Ordering::Relaxed);
                Ok(v)
            }
            Ok(Err(e)) => {
                self.consecutive_timeouts.store(0, Ordering::Relaxed);
                self.maybe_recycle(&e).await;
                Err(e)
            }
            Err(_) => {
                if timeouts_exhausted(&self.consecutive_timeouts, TIMEOUT_RECYCLE_LIMIT) {
                    warn!(
                        target: "browser",
                        limit = TIMEOUT_RECYCLE_LIMIT,
                        "tearing down browser session after consecutive call timeouts"
                    );
                    *self.inner.lock().await = None;
                }
                Err(eyre!(
                    "browser tool call timed out after {}ms",
                    timeout_dur.as_millis()
                ))
            }
        }
    }

    async fn maybe_recycle(&self, err: &eyre::Report) {
        if is_fatal_browser_error(err) {
            warn!(target: "browser", error = %err, "tearing down browser session after fatal error");
            *self.inner.lock().await = None;
        }
    }

    async fn navigate(&self, url: &str) -> Result<Value> {
        let url = url.to_string();
        self.with_page(|page, _n, _c| async move {
            page.goto(url.as_str())
                .await
                .wrap_err("navigation failed")?;
            let title = page
                .evaluate("document.title")
                .await
                .ok()
                .and_then(|r| r.into_value::<String>().ok())
                .unwrap_or_default();
            let final_url = page.url().await.ok().flatten().unwrap_or(url);
            Ok(json!({ "url": final_url, "title": title }))
        })
        .await
    }

    async fn snapshot(&self) -> Result<Value> {
        self.with_page(|page, _n, _c| async move {
            let title = page
                .evaluate("document.title")
                .await
                .ok()
                .and_then(|r| r.into_value::<String>().ok())
                .unwrap_or_default();
            let body_text = page
                .evaluate("document.body ? document.body.innerText : ''")
                .await
                .ok()
                .and_then(|r| r.into_value::<String>().ok())
                .unwrap_or_default();
            let url = page.url().await.ok().flatten().unwrap_or_default();
            Ok(json!({ "url": url, "title": title, "body_text": body_text }))
        })
        .await
    }

    async fn screenshot(&self) -> Result<Value> {
        let last_size = {
            let guard = self.inner.lock().await;
            guard.as_ref().map(|a| a.last_screenshot_size.clone())
        };
        self.with_page(move |page, _n, _c| async move {
            let bytes = page
                .screenshot(ScreenshotParams::default())
                .await
                .wrap_err("screenshot failed")?;
            let size = bytes.len();
            if size < MIN_SCREENSHOT_BYTES {
                warn!(
                    target: "browser",
                    size_bytes = size,
                    "screenshot returned implausibly small payload"
                );
                return Err(eyre!(
                    "screenshot returned only {size} bytes — likely chromium / chromiumoxide state corruption; retry will recycle the browser"
                ));
            }
            if let Some(slot) = last_size.as_ref() {
                let mut prev = slot.lock().await;
                if let Some(p) = *prev {
                    let delta = (size as f64 - p as f64).abs() / p as f64;
                    if delta > SIZE_DELTA_WARN_RATIO {
                        warn!(
                            target: "browser",
                            prev_bytes = p,
                            curr_bytes = size,
                            delta_ratio = delta,
                            "screenshot size deviates sharply from previous in this session"
                        );
                    }
                }
                *prev = Some(size);
            }
            stage_screenshot_artifact(bytes, size)
        })
        .await
    }

    async fn pdf(&self, filename: &str) -> Result<Value> {
        let filename = filename.to_string();
        self.with_page(|page, _n, _c| async move {
            let bytes = page
                .pdf(PrintToPdfParams::default())
                .await
                .wrap_err("pdf generation failed")?;
            stage_pdf_artifact(bytes, &filename)
        })
        .await
    }

    async fn click(&self, selector: &str) -> Result<Value> {
        let selector = selector.to_string();
        self.with_page(|page, _n, _c| async move {
            let element = page
                .find_element(selector.as_str())
                .await
                .wrap_err_with(|| format!("element not found for selector `{selector}`"))?;
            element.click().await.wrap_err("click failed")?;
            Ok(json!({ "clicked": selector }))
        })
        .await
    }

    async fn press_key(&self, key: &str, selector: Option<&str>) -> Result<Value> {
        let key = key.to_string();
        let selector = selector.map(str::to_string);
        self.with_page(|page, _n, _c| async move {
            match selector {
                Some(sel) => {
                    let element = page
                        .find_element(sel.as_str())
                        .await
                        .wrap_err_with(|| format!("element not found for selector `{sel}`"))?;
                    element
                        .press_key(key.as_str())
                        .await
                        .wrap_err("press_key failed")?;
                }
                None => {
                    page.evaluate(format!(
                        "(() => {{ const e = document.activeElement || document.body; \
                          e.dispatchEvent(new KeyboardEvent('keydown', {{ key: {key:?}, bubbles: true }})); \
                          e.dispatchEvent(new KeyboardEvent('keyup', {{ key: {key:?}, bubbles: true }})); \
                          return true; }})()"
                    ))
                    .await
                    .wrap_err("press_key (no selector) failed")?;
                }
            }
            Ok(json!({ "pressed": key }))
        })
        .await
    }

    async fn select_option(&self, selector: &str, value: &str) -> Result<Value> {
        let expr = select_option_expression(selector, value);
        self.with_page(|page, _n, _c| async move {
            let ok = page
                .evaluate(expr)
                .await
                .wrap_err("select_option failed")?
                .into_value::<bool>()
                .unwrap_or(false);
            if !ok {
                return Err(eyre!("element or option not found"));
            }
            Ok(json!({ "selected": true }))
        })
        .await
    }

    async fn evaluate(&self, expression: &str) -> Result<Value> {
        let expression = expression.to_string();
        self.with_page(|page, _n, _c| async move {
            let result = page
                .evaluate(expression.as_str())
                .await
                .wrap_err("evaluate failed")?;
            let value = result.into_value::<Value>().unwrap_or(Value::Null);
            Ok(json!({ "result": value }))
        })
        .await
    }

    async fn wait_for_selector(&self, selector: &str, timeout_ms: Option<u64>) -> Result<Value> {
        let selector = selector.to_string();
        let budget_ms = wait_budget_ms(timeout_ms, self.config.call_timeout_ms);
        let budget = Duration::from_millis(budget_ms);
        self.with_page(|page, _n, _c| async move {
            let deadline = Instant::now() + budget;
            loop {
                if page.find_element(selector.as_str()).await.is_ok() {
                    return Ok(json!({ "found": selector }));
                }
                if Instant::now() >= deadline {
                    return Err(eyre!(
                        "selector `{selector}` not present after {budget_ms}ms"
                    ));
                }
                sleep(Duration::from_millis(POLL_BACKOFF_MS)).await;
            }
        })
        .await
    }

    async fn get_content(&self, format: &str) -> Result<Value> {
        let expr = if format == "text" {
            "document.body ? document.body.innerText : ''"
        } else {
            "document.documentElement ? document.documentElement.outerHTML : ''"
        };
        let format = format.to_string();
        self.with_page(|page, _n, _c| async move {
            let content = page
                .evaluate(expr)
                .await
                .wrap_err("get_content failed")?
                .into_value::<String>()
                .unwrap_or_default();
            let len = content.len();
            Ok(json!({ "format": format, "content": content, "size_bytes": len }))
        })
        .await
    }

    async fn network_requests(&self) -> Result<Value> {
        self.with_page(|_p, network_log, _c| async move {
            let log = network_log.lock().await;
            let entries: Vec<Value> = log.iter().cloned().collect();
            Ok(json!({ "count": entries.len(), "requests": entries }))
        })
        .await
    }

    async fn console_messages(&self) -> Result<Value> {
        self.with_page(|_p, _n, console_log| async move {
            let log = console_log.lock().await;
            let entries: Vec<Value> = log.iter().cloned().collect();
            Ok(json!({ "count": entries.len(), "messages": entries }))
        })
        .await
    }
}

/// Per-agent registry of [`SessionCell`]s, one live Chromium per `(agent, user)`
/// identity. Cells are created lazily by [`BrowserClient::acquire`], bounded by
/// idle-TTL and a soft live-process cap, and their on-disk profiles reaped on a
/// retention window. The shared `store` resolves `[[vault:...]]` / `browser_creds/`
/// markers in `fill` / `type`, unchanged by keying.
pub struct BrowserClient {
    config: Arc<BrowserSkillConfig>,
    profile_root: PathBuf,
    sessions: Mutex<HashMap<SessionId, Arc<SessionCell>>>,
    last_reap: StdMutex<Instant>,
    store: Option<Arc<dyn SecretStore>>,
}

impl BrowserClient {
    pub fn new(config: BrowserSkillConfig) -> Self {
        Self::build(config, None)
    }

    pub fn with_store(config: BrowserSkillConfig, store: Arc<dyn SecretStore>) -> Self {
        Self::build(config, Some(store))
    }

    fn build(config: BrowserSkillConfig, store: Option<Arc<dyn SecretStore>>) -> Self {
        let profile_root = config
            .profile_root
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join(PROFILE_ROOT_NAME));
        let now = Instant::now();
        let last_reap = now
            .checked_sub(Duration::from_millis(config.reap_interval_ms))
            .unwrap_or(now);
        Self {
            config: Arc::new(config),
            profile_root,
            sessions: Mutex::new(HashMap::new()),
            last_reap: StdMutex::new(last_reap),
            store,
        }
    }

    /// Resolve credential markers in a fill/type value at the current page.
    /// Inside a workflow run a `[[vault:<scope>:<key>]]` marker resolves through
    /// the run's secret policy (deny-by-default, per-user gate, curated prefix,
    /// and `host`-based egress); outside one, the legacy `browser_creds/` path
    /// applies. `host` is the page's current host, for egress enforcement.
    fn resolve_credential(&self, raw: &str, host: Option<&str>) -> Result<String> {
        let Some(store) = self.store.as_ref() else {
            return if raw.contains("[[vault:") {
                Err(eyre!(
                    "browser tool received `[[vault:...]]` marker but the skill was constructed without a vault store"
                ))
            } else {
                Ok(raw.to_string())
            };
        };
        let after_policy =
            workforce_common::secret_policy::resolve_secret_markers(raw, store, host)?;
        resolve_vault_refs(&after_policy, store, VAULT_PREFIX)
    }

    /// Resolve the in-scope identity to its session cell, creating it if needed,
    /// after pruning idle cells and enforcing the live-process cap. Never holds
    /// the registry lock across a Chromium launch: launches happen lazily on the
    /// returned cell, and evicted Chromium is dropped after the lock is released.
    async fn acquire(&self) -> Result<Arc<SessionCell>> {
        let target = current_session_id();
        let now = Instant::now();
        let idle_ttl = Duration::from_millis(self.config.session_idle_ttl_ms);
        let cap = self.config.max_live_sessions.max(1);

        let (cell, evicted, live_dirs, reap) = {
            let mut sessions = self.sessions.lock().await;

            let stats: Vec<CellStat> = sessions
                .iter()
                .filter(|(id, _)| **id != target)
                .map(|(id, cell)| cell.stat(id.clone()))
                .collect();

            let idle = select_idle_evictions(&stats, now, idle_ttl);
            let idle_set: HashSet<&SessionId> = idle.iter().collect();
            let remaining: Vec<CellStat> = stats
                .iter()
                .filter(|s| !idle_set.contains(&s.id))
                .cloned()
                .collect();
            let live_after_idle = remaining.iter().filter(|s| s.live).count();
            let overflow = (live_after_idle + 1).saturating_sub(cap);
            let lru = select_lru_evictions(&remaining, overflow);
            if lru.len() < overflow {
                warn!(
                    target: "browser",
                    cap,
                    over = overflow - lru.len(),
                    "all live browser sessions busy; tolerating temporary over-cap"
                );
            }

            let mut evicted = Vec::with_capacity(idle.len() + lru.len());
            for id in idle.into_iter().chain(lru) {
                if let Some(cell) = sessions.remove(&id) {
                    evicted.push(cell);
                }
            }

            let cell = sessions
                .entry(target.clone())
                .or_insert_with(|| {
                    Arc::new(SessionCell::new(
                        self.config.clone(),
                        self.profile_root.join(&target.0),
                        now,
                    ))
                })
                .clone();
            cell.touch(now);

            let live_dirs: HashSet<PathBuf> =
                sessions.values().map(|c| c.profile_dir.clone()).collect();

            let reap = {
                let mut last = self.last_reap.lock().unwrap();
                let due = should_reap(
                    *last,
                    now,
                    Duration::from_millis(self.config.reap_interval_ms),
                );
                if due {
                    *last = now;
                }
                due
            };

            (cell, evicted, live_dirs, reap)
        };

        drop(evicted);
        if reap {
            self.spawn_reap(live_dirs);
        }
        Ok(cell)
    }

    fn spawn_reap(&self, live_dirs: HashSet<PathBuf>) {
        let root = self.profile_root.clone();
        let retention = Duration::from_millis(self.config.profile_retention_ms);
        tokio::task::spawn_blocking(move || reap_blocking(&root, &live_dirs, retention));
    }

    pub(super) async fn navigate(&self, url: &str) -> Result<Value> {
        self.acquire().await?.navigate(url).await
    }

    pub(super) async fn snapshot(&self) -> Result<Value> {
        self.acquire().await?.snapshot().await
    }

    pub(super) async fn screenshot(&self) -> Result<Value> {
        self.acquire().await?.screenshot().await
    }

    pub(super) async fn pdf(&self, filename: &str) -> Result<Value> {
        self.acquire().await?.pdf(filename).await
    }

    pub(super) async fn click(&self, selector: &str) -> Result<Value> {
        self.acquire().await?.click(selector).await
    }

    pub(super) async fn fill(&self, selector: &str, value: &str) -> Result<Value> {
        let cell = self.acquire().await?;
        let selector = selector.to_string();
        let value = value.to_string();
        cell.with_page(move |page, _n, _c| async move {
            // Resolve the credential against the page we're about to fill, so the
            // egress allowlist is checked against the real destination host.
            let host = page.url().await.ok().flatten().and_then(|u| host_of(&u));
            let resolved = self.resolve_credential(&value, host.as_deref())?;
            let expr = fill_expression(&selector, &resolved);
            let ok = page
                .evaluate(expr)
                .await
                .wrap_err("fill failed")?
                .into_value::<bool>()
                .unwrap_or(false);
            if !ok {
                return Err(eyre!("element not found for selector"));
            }
            Ok(json!({ "filled": true }))
        })
        .await
    }

    pub(super) async fn type_text(&self, selector: &str, text: &str) -> Result<Value> {
        let cell = self.acquire().await?;
        let selector = selector.to_string();
        let text = text.to_string();
        cell.with_page(move |page, _n, _c| async move {
            let host = page.url().await.ok().flatten().and_then(|u| host_of(&u));
            let resolved = self.resolve_credential(&text, host.as_deref())?;
            let element = page
                .find_element(selector.as_str())
                .await
                .wrap_err_with(|| format!("element not found for selector `{selector}`"))?;
            element.focus().await.wrap_err("focus failed")?;
            element
                .type_str(resolved.as_str())
                .await
                .wrap_err("type failed")?;
            Ok(json!({ "typed_chars": resolved.chars().count() }))
        })
        .await
    }

    pub(super) async fn select_option(&self, selector: &str, value: &str) -> Result<Value> {
        self.acquire().await?.select_option(selector, value).await
    }

    pub(super) async fn evaluate(&self, expression: &str) -> Result<Value> {
        self.acquire().await?.evaluate(expression).await
    }

    pub(super) async fn wait_for_selector(
        &self,
        selector: &str,
        timeout_ms: Option<u64>,
    ) -> Result<Value> {
        self.acquire()
            .await?
            .wait_for_selector(selector, timeout_ms)
            .await
    }

    pub(super) async fn get_content(&self, format: &str) -> Result<Value> {
        self.acquire().await?.get_content(format).await
    }

    pub(super) async fn press_key(&self, key: &str, selector: Option<&str>) -> Result<Value> {
        self.acquire().await?.press_key(key, selector).await
    }

    pub(super) async fn network_requests(&self) -> Result<Value> {
        self.acquire().await?.network_requests().await
    }

    pub(super) async fn console_messages(&self) -> Result<Value> {
        self.acquire().await?.console_messages().await
    }

    /// Upload a staged artifact to a file `<input>` on the current page. The
    /// bytes are read from the artifact store (same-session scoped) and written
    /// to a temp file, because CDP `DOM.setFileInputFiles` needs a real path
    /// Chromium can read — the model never handles the bytes.
    pub(super) async fn upload_file(
        &self,
        selector: &str,
        artifact_id: &str,
        filename: Option<&str>,
    ) -> Result<Value> {
        let agent_id = CURRENT_AGENT_ID
            .try_with(|v| v.clone())
            .ok()
            .unwrap_or_default();
        let staged = crate::adapters::utils::resolve_staged_file(&agent_id, artifact_id)
            .wrap_err_with(|| format!("could not read artifact {artifact_id}"))?;
        let name = filename
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or(staged.filename);
        // The file must outlive this call (Chromium reads it at form-submit
        // time, after the later browser.click), so it is intentionally left on
        // disk; the OS temp sweep reclaims it.
        let path = write_upload_temp(&name, &staged.bytes)?;
        let path_str = path.to_string_lossy().to_string();

        let selector = selector.to_string();
        let cell = self.acquire().await?;
        cell.with_page(move |page, _n, _c| async move {
            let element = page
                .find_element(selector.as_str())
                .await
                .wrap_err_with(|| format!("element not found for selector `{selector}`"))?;
            let params = SetFileInputFilesParams::builder()
                .files(vec![path_str.clone()])
                .backend_node_id(element.backend_node_id)
                .build()
                .map_err(|e| eyre!("setFileInputFiles params: {e}"))?;
            page.execute(params)
                .await
                .wrap_err("setFileInputFiles failed (is the selector an <input type=\"file\">?)")?;
            Ok(json!({ "uploaded": name, "selector": selector }))
        })
        .await
    }
}

/// Write `bytes` to a uniquely-named temp dir, keeping `name` as the basename
/// so the form sees a real filename. The basename is sanitised to block path
/// traversal from a model-supplied name.
fn write_upload_temp(name: &str, bytes: &[u8]) -> Result<PathBuf> {
    let base = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("upload.bin");
    let unique = format!(
        "wf-upload-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let dir = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&dir).wrap_err("failed to create upload temp dir")?;
    let path = dir.join(base);
    std::fs::write(&path, bytes).wrap_err("failed to write upload temp file")?;
    Ok(path)
}

fn ensure_profile_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).wrap_err("failed to create browser profile dir")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .wrap_err("failed to set browser profile dir permissions")?;
    }
    Ok(())
}

fn reap_blocking(profile_root: &Path, live: &HashSet<PathBuf>, retention: Duration) {
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join(LEGACY_PROFILE_DIR));
    let Ok(entries) = std::fs::read_dir(profile_root) else {
        return;
    };
    let now = SystemTime::now();
    let candidates: Vec<(PathBuf, SystemTime)> = entries
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_dir() {
                return None;
            }
            let mtime = entry.metadata().ok()?.modified().ok()?;
            Some((path, mtime))
        })
        .collect();
    for target in reap_targets(&candidates, live, now, retention) {
        let _ = std::fs::remove_dir_all(&target);
    }
}

async fn launch_chromium(
    config: &BrowserSkillConfig,
    profile_dir: &Path,
) -> Result<(Browser, Handler)> {
    let builder: BrowserConfigBuilder = BrowserConfig::builder()
        .no_sandbox()
        .user_data_dir(profile_dir)
        .viewport(Viewport {
            width: config.viewport_width,
            height: config.viewport_height,
            ..Default::default()
        })
        .launch_timeout(Duration::from_millis(config.launch_timeout_ms))
        .args([
            "--disable-dev-shm-usage",
            "--disable-gpu",
            "--disable-software-rasterizer",
            "--disable-background-networking",
            "--disable-default-apps",
            "--no-first-run",
            "--no-default-browser-check",
        ]);
    let builder = match config.chrome_binary.as_deref().filter(|s| !s.is_empty()) {
        Some(path) => builder.chrome_executable(path),
        None => builder,
    };
    let cfg = builder
        .build()
        .map_err(|e| eyre!("invalid chromium config: {e}"))?;
    Browser::launch(cfg)
        .await
        .wrap_err("failed to launch chromium")
}

struct RespawnBudget {
    window_ms: u64,
    limit: usize,
    timestamps: Vec<std::time::Instant>,
}

/// Whether `err` means the browser connection itself is gone, judged over the
/// whole cause chain: tool code wraps CDP failures ("navigation failed") and
/// the transport error that matters sits underneath.
fn is_fatal_browser_error(err: &eyre::Report) -> bool {
    err.chain().any(|cause| {
        let msg = cause.to_string().to_lowercase();
        msg.contains("websocket")
            || msg.contains("connection closed")
            || msg.contains("channel closed")
            || msg.contains("io error")
            || msg.contains("implausibly small")
            || msg.contains("state corruption")
    })
}

/// Record one timed-out call; true when `limit` have now happened in a row,
/// at which point the counter resets for the relaunched session.
fn timeouts_exhausted(counter: &AtomicUsize, limit: usize) -> bool {
    let seen = counter.fetch_add(1, Ordering::Relaxed) + 1;
    if seen >= limit {
        counter.store(0, Ordering::Relaxed);
        true
    } else {
        false
    }
}

impl RespawnBudget {
    fn new(window_ms: u64, limit: usize) -> Self {
        Self {
            window_ms,
            limit,
            timestamps: Vec::with_capacity(limit + 1),
        }
    }

    fn record(&mut self) -> Result<()> {
        let now = std::time::Instant::now();
        let window = std::time::Duration::from_millis(self.window_ms);
        self.timestamps.retain(|t| now.duration_since(*t) < window);
        if self.timestamps.len() >= self.limit {
            return Err(eyre!(
                "chromium has crashed {limit} times in the last {seconds}s; refusing to respawn",
                limit = self.limit,
                seconds = self.window_ms / 1000,
            ));
        }
        self.timestamps.push(now);
        Ok(())
    }
}

fn push_bounded(log: &mut VecDeque<Value>, capacity: usize, entry: Value) {
    if capacity == 0 {
        return;
    }
    while log.len() >= capacity {
        log.pop_front();
    }
    log.push_back(entry);
}

async fn spawn_network_listener(
    page: &Page,
    log: Arc<Mutex<VecDeque<Value>>>,
    capacity: usize,
) -> Result<JoinHandle<()>> {
    let mut stream = page
        .event_listener::<EventRequestWillBeSent>()
        .await
        .wrap_err("failed to subscribe to network events")?;
    Ok(tokio::spawn(async move {
        while let Some(event) = stream.next().await {
            let entry = json!({
                "request_id": event.request_id.inner(),
                "url": event.request.url,
                "method": event.request.method,
                "resource_type": format!("{:?}", event.r#type),
                "timestamp": event.timestamp.inner(),
            });
            push_bounded(&mut *log.lock().await, capacity, entry);
        }
    }))
}

async fn spawn_console_listeners(
    page: &Page,
    log: Arc<Mutex<VecDeque<Value>>>,
    capacity: usize,
) -> Result<(JoinHandle<()>, JoinHandle<()>)> {
    let mut log_stream = page
        .event_listener::<EventEntryAdded>()
        .await
        .wrap_err("failed to subscribe to log entries")?;
    let log_for_log = log.clone();
    let log_task = tokio::spawn(async move {
        while let Some(event) = log_stream.next().await {
            let entry = json!({
                "source": "log",
                "level": format!("{:?}", event.entry.level),
                "text": event.entry.text,
                "url": event.entry.url,
                "timestamp": event.entry.timestamp.inner(),
            });
            push_bounded(&mut *log_for_log.lock().await, capacity, entry);
        }
    });

    let mut api_stream = page
        .event_listener::<EventConsoleApiCalled>()
        .await
        .wrap_err("failed to subscribe to console api events")?;
    let api_task = tokio::spawn(async move {
        while let Some(event) = api_stream.next().await {
            let args: Vec<Value> = event
                .args
                .iter()
                .map(|a| a.value.clone().unwrap_or(Value::Null))
                .collect();
            let entry = json!({
                "source": "console",
                "level": format!("{:?}", event.r#type),
                "args": args,
                "timestamp": event.timestamp.inner(),
            });
            push_bounded(&mut *log.lock().await, capacity, entry);
        }
    });

    Ok((log_task, api_task))
}

/// Write `bytes` to the artifact store under the current agent and build a
/// `generated_images` delivery entry (the channel the runner's sink resolves to
/// deliver attachments — screenshots, PDFs — to the originating surface without
/// the agent carrying the bytes). Returns `Ok(Err(bytes))` when no agent id or
/// store is in scope, handing the bytes back so the caller can fall back to an
/// inline-base64 payload (keeps the tools usable in tests / out-of-band runs).
fn stage_for_delivery(
    bytes: Vec<u8>,
    producer: &str,
    media_type: &str,
    filename: &str,
) -> Result<std::result::Result<Value, Vec<u8>>> {
    let agent_id = CURRENT_AGENT_ID
        .try_with(|v| v.clone())
        .ok()
        .filter(|s| !s.is_empty());
    let store = global_store();
    let (Some(agent_id), Some(store)) = (agent_id, store) else {
        return Ok(Err(bytes));
    };
    let size = bytes.len();
    let descriptor =
        ArtifactDescriptor::from_context(producer, ContentType::OctetStream, agent_id.clone());
    let handle = store
        .write(&agent_id, descriptor, ArtifactBody::Bytes(bytes))
        .wrap_err("failed to stage artifact for delivery")?;
    Ok(Ok(json!({
        "artifact_id": handle.id.as_str(),
        "media_type": media_type,
        "filename": filename,
        "bytes": size,
    })))
}

/// Stage a screenshot PNG for auto-delivery, or fall back to inline base64 when
/// no artifact store is in scope. See [`stage_for_delivery`].
fn stage_screenshot_artifact(bytes: Vec<u8>, size: usize) -> Result<Value> {
    tracing::info!(target: "browser", size_bytes = size, "screenshot captured");
    match stage_for_delivery(bytes, "browser.screenshot", "image/png", "screenshot.png")? {
        Ok(entry) => Ok(json!({
            GENERATED_IMAGES_KEY: [entry],
            "format": "png",
            "size_bytes": size,
            "note": "Screenshot captured. The system is delivering it to this conversation automatically — it happens outside your context, so you will NOT see the attachment. This is expected, NOT a failure: do not re-upload, attach, verify, or claim it failed. Just briefly tell the user what's on screen.",
        })),
        Err(bytes) => {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            Ok(json!({
                "image_base64": b64,
                "format": "png",
                "size_bytes": size,
                "note": "Image staged inline (no artifact store in scope). The runner will not auto-deliver this screenshot."
            }))
        }
    }
}

/// Stage a rendered PDF for auto-delivery, or fall back to inline base64 when no
/// artifact store is in scope. Mirrors [`stage_screenshot_artifact`] so a PDF
/// reaches the user the same way a screenshot does. See [`stage_for_delivery`].
fn stage_pdf_artifact(bytes: Vec<u8>, filename: &str) -> Result<Value> {
    let size = bytes.len();
    tracing::info!(target: "browser", size_bytes = size, %filename, "pdf rendered");
    match stage_for_delivery(bytes, "browser.pdf", "application/pdf", filename)? {
        Ok(entry) => Ok(json!({
            GENERATED_IMAGES_KEY: [entry],
            "size_bytes": size,
            "note": "PDF rendered. The system is delivering it to this conversation automatically — it happens outside your context, so you will NOT see the file. This is expected, NOT a failure: do not re-upload, attach, verify, or claim it failed. Just briefly tell the user what the PDF contains.",
        })),
        Err(bytes) => {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            Ok(json!({
                "pdf_base64": b64,
                "size_bytes": size,
                "filename": filename,
                "note": "PDF rendered inline (no artifact store in scope). The runner will not auto-deliver this file."
            }))
        }
    }
}

/// Extract the lowercase host from a URL for egress matching. Strips the
/// scheme, any `userinfo@`, the path/query/fragment, and the `:port`. Returns
/// `None` for an empty or scheme-only string.
fn host_of(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

fn fill_expression(selector: &str, value: &str) -> String {
    let sel = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".to_string());
    let val = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        "(() => {{ const el = document.querySelector({sel}); if (!el) return false; \
          el.focus(); el.value = {val}; \
          el.dispatchEvent(new Event('input', {{ bubbles: true }})); \
          el.dispatchEvent(new Event('change', {{ bubbles: true }})); \
          return true; }})()"
    )
}

fn select_option_expression(selector: &str, value: &str) -> String {
    let sel = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".to_string());
    let val = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        "(() => {{ const sel = document.querySelector({sel}); if (!sel) return false; \
          const target = {val}; \
          const opt = Array.from(sel.options).find(o => o.value === target || o.text === target); \
          if (!opt) return false; \
          sel.value = opt.value; \
          sel.dispatchEvent(new Event('change', {{ bubbles: true }})); \
          return true; }})()"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fatal_transport_errors_are_found_beneath_tool_wrapping() {
        let inner = eyre!("channel closed");
        let wrapped = inner.wrap_err("navigation failed");
        assert!(is_fatal_browser_error(&wrapped));
        let plain = eyre!("navigation failed");
        assert!(!is_fatal_browser_error(&plain));
        let selector = eyre!("selector not found").wrap_err("click failed");
        assert!(!is_fatal_browser_error(&selector));
    }

    #[test]
    fn consecutive_timeouts_recycle_at_the_limit_and_reset() {
        let counter = AtomicUsize::new(0);
        assert!(!timeouts_exhausted(&counter, 2));
        assert!(timeouts_exhausted(&counter, 2));
        assert_eq!(counter.load(Ordering::Relaxed), 0);
        assert!(!timeouts_exhausted(&counter, 2));
    }

    #[test]
    fn wait_budget_defaults_to_the_call_timeout_and_caps_at_max_wait() {
        assert_eq!(wait_budget_ms(None, 30_000), 30_000);
        assert_eq!(wait_budget_ms(Some(5_000), 30_000), 5_000);
        assert_eq!(wait_budget_ms(Some(MAX_WAIT_MS), 30_000), MAX_WAIT_MS);
        assert_eq!(wait_budget_ms(Some(MAX_WAIT_MS + 1), 30_000), MAX_WAIT_MS);
        assert_eq!(wait_budget_ms(Some(u64::MAX), 30_000), MAX_WAIT_MS);
        assert_eq!(
            wait_budget_ms(None, MAX_WAIT_MS * 2),
            MAX_WAIT_MS,
            "an oversized configured default is capped too"
        );
    }

    #[test]
    fn push_bounded_drops_oldest() {
        let mut log = VecDeque::new();
        for i in 0..5 {
            push_bounded(&mut log, 3, json!({ "i": i }));
        }
        let observed: Vec<i64> = log.iter().map(|v| v["i"].as_i64().unwrap()).collect();
        assert_eq!(observed, vec![2, 3, 4]);
    }

    #[test]
    fn push_bounded_zero_capacity_is_noop() {
        let mut log = VecDeque::new();
        push_bounded(&mut log, 0, json!({}));
        assert!(log.is_empty());
    }

    #[test]
    fn host_of_extracts_host() {
        assert_eq!(
            host_of("https://App.Example.com/login?x=1").as_deref(),
            Some("app.example.com")
        );
        assert_eq!(
            host_of("https://user:pw@host.example.com:8443/p").as_deref(),
            Some("host.example.com")
        );
        assert_eq!(host_of("about:blank").as_deref(), Some("about"));
        assert_eq!(host_of(""), None);
    }

    #[test]
    fn fill_expression_quotes_strings() {
        let expr = fill_expression("input[name=\"q\"]", "alice's value");
        assert!(expr.contains("input[name=\\\"q\\\"]"));
        assert!(expr.contains("alice's value") || expr.contains("alice\\'s value"));
        assert!(expr.contains("document.querySelector"));
    }

    #[test]
    fn select_option_expression_handles_quote() {
        let expr = select_option_expression("#pick", "two");
        assert!(expr.contains("#pick"));
        assert!(expr.contains("\"two\""));
        assert!(expr.contains("Array.from"));
    }

    #[test]
    fn stage_screenshot_falls_back_to_inline_base64_without_agent_or_store() {
        let bytes = vec![0u8; 1024];
        let len = bytes.len();
        let out = stage_screenshot_artifact(bytes, len).unwrap();
        assert_eq!(out["format"], "png");
        assert_eq!(out["size_bytes"].as_u64(), Some(len as u64));
        assert!(out.get(GENERATED_IMAGES_KEY).is_none());
        assert!(out["image_base64"].as_str().unwrap().len() > 100);
    }

    #[tokio::test]
    async fn stage_screenshot_writes_artifact_and_emits_generated_images() {
        let agent_id = crate::adapters::test_support::fresh_agent_id("browser-screenshot");
        let bytes = vec![0xAB; 4096];
        let len = bytes.len();
        let out = CURRENT_AGENT_ID
            .scope(agent_id, async move {
                stage_screenshot_artifact(bytes, len).unwrap()
            })
            .await;

        assert!(
            out.get("image_base64").is_none(),
            "should not inline bytes when staged"
        );
        let entries = out[GENERATED_IMAGES_KEY].as_array().expect("array");
        assert_eq!(entries.len(), 1);
        assert!(!entries[0]["artifact_id"].as_str().unwrap().is_empty());
        assert_eq!(entries[0]["media_type"], "image/png");
        assert_eq!(entries[0]["bytes"].as_u64(), Some(len as u64));
    }

    #[test]
    fn stage_pdf_falls_back_to_inline_base64_without_agent_or_store() {
        let bytes = vec![0u8; 1024];
        let out = stage_pdf_artifact(bytes, "report.pdf").unwrap();
        assert_eq!(out["filename"], "report.pdf");
        assert!(out.get(GENERATED_IMAGES_KEY).is_none());
        assert!(out["pdf_base64"].as_str().unwrap().len() > 100);
    }

    #[tokio::test]
    async fn stage_pdf_writes_artifact_and_emits_generated_images() {
        let agent_id = crate::adapters::test_support::fresh_agent_id("browser-pdf");
        let bytes = vec![0xCD; 4096];
        let len = bytes.len();
        let out = CURRENT_AGENT_ID
            .scope(agent_id, async move {
                stage_pdf_artifact(bytes, "report.pdf").unwrap()
            })
            .await;

        assert!(
            out.get("pdf_base64").is_none(),
            "should not inline bytes when staged"
        );
        let entries = out[GENERATED_IMAGES_KEY].as_array().expect("array");
        assert_eq!(entries.len(), 1);
        assert!(!entries[0]["artifact_id"].as_str().unwrap().is_empty());
        assert_eq!(entries[0]["media_type"], "application/pdf");
        assert_eq!(entries[0]["filename"], "report.pdf");
        assert_eq!(entries[0]["bytes"].as_u64(), Some(len as u64));
    }

    #[test]
    fn respawn_budget_allows_up_to_limit_then_rejects() {
        let mut budget = RespawnBudget::new(60_000, 3);
        for _ in 0..3 {
            budget.record().unwrap();
        }
        assert!(budget.record().is_err());
    }

    #[test]
    fn respawn_budget_clears_after_window() {
        let mut budget = RespawnBudget::new(1, 1);
        budget.record().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        budget.record().unwrap();
    }

    #[test]
    fn session_id_is_hex_and_stable() {
        let id = SessionId::derive("agent", "alice");
        assert_eq!(id.0.len(), 64);
        assert!(id.0.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(id, SessionId::derive("agent", "alice"));
    }

    #[test]
    fn session_id_distinguishes_user_and_agent() {
        assert_ne!(
            SessionId::derive("agent", "alice"),
            SessionId::derive("agent", "bob")
        );
        assert_ne!(
            SessionId::derive("agent-a", "alice"),
            SessionId::derive("agent-b", "alice")
        );
    }

    #[test]
    fn session_id_domain_separator_prevents_collision() {
        assert_ne!(SessionId::derive("a", "bc"), SessionId::derive("ab", "c"));
    }

    #[test]
    fn current_session_id_keys_on_scoped_user() {
        let key = |user: &str| {
            let user = user.to_string();
            CURRENT_AGENT_ID.sync_scope("agent".to_string(), || {
                CURRENT_USER_ID.sync_scope(user, current_session_id)
            })
        };
        assert_ne!(key("alice"), key("bob"));
        assert_eq!(key("alice"), key("alice"));
        assert_eq!(key("alice"), SessionId::derive("agent", "alice"));
    }

    #[test]
    fn current_session_id_falls_back_to_system_and_default() {
        let unscoped = CURRENT_AGENT_ID.sync_scope("agent".to_string(), current_session_id);
        assert_eq!(unscoped, SessionId::derive("agent", SYSTEM_USER_ID));
        let no_agent = CURRENT_USER_ID.sync_scope("alice".to_string(), current_session_id);
        assert_eq!(no_agent, SessionId::derive(DEFAULT_AGENT, "alice"));
    }

    #[test]
    fn select_idle_evictions_picks_idle_non_inflight() {
        let now = Instant::now();
        let old = now.checked_sub(Duration::from_secs(60)).unwrap();
        let stats = vec![
            CellStat {
                id: SessionId("idle".into()),
                live: true,
                inflight: false,
                last_used: old,
            },
            CellStat {
                id: SessionId("busy".into()),
                live: true,
                inflight: true,
                last_used: old,
            },
            CellStat {
                id: SessionId("fresh".into()),
                live: true,
                inflight: false,
                last_used: now,
            },
        ];
        assert_eq!(
            select_idle_evictions(&stats, now, Duration::from_secs(30)),
            vec![SessionId("idle".into())]
        );
    }

    #[test]
    fn select_lru_evictions_takes_oldest_live_only() {
        let now = Instant::now();
        let at = |secs: u64| now.checked_sub(Duration::from_secs(secs)).unwrap();
        let stats = vec![
            CellStat {
                id: SessionId("old".into()),
                live: true,
                inflight: false,
                last_used: at(90),
            },
            CellStat {
                id: SessionId("mid".into()),
                live: true,
                inflight: false,
                last_used: at(60),
            },
            CellStat {
                id: SessionId("busy".into()),
                live: true,
                inflight: true,
                last_used: at(120),
            },
            CellStat {
                id: SessionId("idle_off".into()),
                live: false,
                inflight: false,
                last_used: at(200),
            },
            CellStat {
                id: SessionId("new".into()),
                live: true,
                inflight: false,
                last_used: at(1),
            },
        ];
        assert_eq!(
            select_lru_evictions(&stats, 2),
            vec![SessionId("old".into()), SessionId("mid".into())]
        );
        assert!(select_lru_evictions(&stats, 0).is_empty());
    }

    #[test]
    fn should_reap_respects_interval() {
        let now = Instant::now();
        assert!(should_reap(
            now.checked_sub(Duration::from_secs(2)).unwrap(),
            now,
            Duration::from_secs(1)
        ));
        assert!(!should_reap(now, now, Duration::from_secs(1)));
    }

    #[test]
    fn reap_targets_excludes_live_and_fresh() {
        let now = SystemTime::now();
        let old = now.checked_sub(Duration::from_secs(100)).unwrap();
        let live: HashSet<PathBuf> = [PathBuf::from("/p/live")].into_iter().collect();
        let entries = vec![
            (PathBuf::from("/p/live"), old),
            (PathBuf::from("/p/stale"), old),
            (PathBuf::from("/p/fresh"), now),
        ];
        assert_eq!(
            reap_targets(&entries, &live, now, Duration::from_secs(50)),
            vec![PathBuf::from("/p/stale")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn ensure_profile_dir_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile");
        ensure_profile_dir(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    fn test_client(profile_root: &Path, idle_ttl_ms: u64) -> BrowserClient {
        BrowserClient::new(BrowserSkillConfig {
            profile_root: Some(profile_root.display().to_string()),
            session_idle_ttl_ms: idle_ttl_ms,
            ..Default::default()
        })
    }

    async fn acquire_as(client: &BrowserClient, user: &str) -> Arc<SessionCell> {
        CURRENT_AGENT_ID
            .scope(
                "agent".to_string(),
                CURRENT_USER_ID.scope(user.to_string(), client.acquire()),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn acquire_reuses_cell_per_user_and_isolates_profile_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let client = test_client(dir.path(), 600_000);
        let alice1 = acquire_as(&client, "alice").await;
        let alice2 = acquire_as(&client, "alice").await;
        let bob = acquire_as(&client, "bob").await;

        assert!(
            Arc::ptr_eq(&alice1, &alice2),
            "same user must reuse one cell"
        );
        assert!(!Arc::ptr_eq(&alice1, &bob));
        assert_ne!(alice1.profile_dir, bob.profile_dir);
        assert!(alice1.profile_dir.starts_with(dir.path()));
    }

    #[tokio::test]
    async fn acquire_evicts_idle_cells() {
        let dir = tempfile::tempdir().unwrap();
        let client = test_client(dir.path(), 0);
        let alice = acquire_as(&client, "alice").await;
        let _bob = acquire_as(&client, "bob").await;
        let alice_again = acquire_as(&client, "alice").await;

        assert!(
            !Arc::ptr_eq(&alice, &alice_again),
            "an idle cell should be evicted from the registry and recreated"
        );
    }

    /// End-to-end: two scoped users hit the same origin; the second must not see
    /// the first's cookie, and the first's persistent cookie survives into a
    /// later turn. Served over localhost http (not file://, where Chromium drops
    /// document.cookie) and the writer reads its own cookie back so a dropped
    /// write fails loudly. Gated on `BROWSER_E2E`.
    ///
    /// Run locally with:
    ///   BROWSER_E2E=1 cargo test -p workforce-skills --features browser \
    ///       -- --ignored browser_e2e_isolates
    #[tokio::test]
    #[ignore]
    async fn browser_e2e_isolates_cookies_per_user() {
        if std::env::var("BROWSER_E2E").is_err() {
            return;
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                // One thread per connection so Chrome's preconnect/favicon
                // sockets never stall the request we care about.
                std::thread::spawn(move || {
                    let mut buf = [0u8; 1024];
                    let _ = stream.read(&mut buf);
                    let body = "<!doctype html><title>iso</title><body>ok</body>";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes());
                });
            }
        });
        let url = format!("http://127.0.0.1:{port}/");
        let dir = tempfile::tempdir().unwrap();
        let client = BrowserClient::new(BrowserSkillConfig {
            profile_root: Some(dir.path().display().to_string()),
            chrome_binary: std::env::var("BROWSER_E2E_CHROMIUM").ok(),
            ..Default::default()
        });

        assert!(
            cookie_after(&client, &url, "alice", true)
                .await
                .contains("iso=alice"),
            "alice must read back her own cookie (the write must not be silently dropped)"
        );
        assert!(
            !cookie_after(&client, &url, "bob", false)
                .await
                .contains("iso=alice"),
            "bob must NOT see alice's cookie — sessions are isolated per user"
        );
        assert!(
            cookie_after(&client, &url, "alice", false)
                .await
                .contains("iso=alice"),
            "alice's persistent cookie should survive into a later turn"
        );
    }

    async fn cookie_after(client: &BrowserClient, url: &str, user: &str, set: bool) -> String {
        let url = url.to_string();
        CURRENT_AGENT_ID
            .scope(
                "agent".to_string(),
                CURRENT_USER_ID.scope(user.to_string(), async {
                    let nav = client.navigate(&url).await.unwrap();
                    assert!(
                        nav["url"]
                            .as_str()
                            .unwrap_or_default()
                            .contains("127.0.0.1"),
                        "navigation must land on the http fixture, got {nav:?}"
                    );
                    if set {
                        client
                            .evaluate("document.cookie = 'iso=alice; path=/; max-age=3600'")
                            .await
                            .unwrap();
                    }
                    client.evaluate("document.cookie").await.unwrap()["result"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string()
                }),
            )
            .await
    }
}
