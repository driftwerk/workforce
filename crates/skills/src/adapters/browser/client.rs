//! The browser client: each `(agent, person)`'s tab in the person's Chrome
//! ([`ExtensionCell`]), what each call was decided for, Sign in for me, and
//! what Desk sees and stops of them.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use base64::Engine;
use eyre::{eyre, Result, WrapErr};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use workforce_agent::artifacts::store::global_store;
use workforce_agent::artifacts::types::{ArtifactBody, ArtifactDescriptor, ContentType};
use workforce_agent::runner::{CURRENT_AGENT_ID, CURRENT_USER_ID, SYSTEM_USER_ID};
use workforce_agent::tools::image_gen::GENERATED_IMAGES_KEY;
use workforce_common::settings::SecretStore;

use crate::adapters::credential_resolver::resolve_vault_refs;

use super::config::{BrowserSkillConfig, MAX_WAIT_MS};
use super::extension::ExtensionCell;
use super::hooks;
use super::untrusted;

const VAULT_PREFIX: &str = "browser_creds/";
const DEFAULT_AGENT: &str = "default";

/// How long a `browser.wait` polls, by default the call's own timeout; at
/// most [`MAX_WAIT_MS`].
pub(super) fn wait_budget_ms(requested: Option<u64>, default_ms: u64) -> u64 {
    requested.unwrap_or(default_ms).min(MAX_WAIT_MS)
}

/// Every browser client in this process, so Desk can see and stop them.
static CLIENTS: std::sync::LazyLock<StdMutex<Vec<std::sync::Weak<BrowserClient>>>> =
    std::sync::LazyLock::new(Default::default);

/// Keeps `client` where [`live`] and [`close`] find it, and closes its idle
/// tabs on a clock.
pub(super) fn register(client: &Arc<BrowserClient>) {
    let mut clients = CLIENTS.lock().unwrap();
    clients.retain(|c| c.strong_count() > 0);
    clients.push(Arc::downgrade(client));
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let weak = Arc::downgrade(client);
        let every = Duration::from_millis(client.config.session_idle_ttl_ms.clamp(1_000, 60_000));
        handle.spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let Some(client) = weak.upgrade() else { return };
                client.close_idle().await;
            }
        });
    }
}

fn clients() -> Vec<Arc<BrowserClient>> {
    CLIENTS
        .lock()
        .unwrap()
        .iter()
        .filter_map(std::sync::Weak::upgrade)
        .collect()
}

/// A tab Desk has open now, as Desk's indicator shows it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Live {
    pub agent: String,
    pub user: String,
    /// The session (its id) its last call came from.
    pub session: Option<String>,
    pub url: String,
    pub host: Option<String>,
    /// Seconds since its last call.
    pub idle_secs: u64,
}

/// Every tab Desk has open now, most recently used first.
pub async fn live() -> Vec<Live> {
    let mut out = Vec::new();
    for client in clients() {
        let cells: Vec<Arc<ExtensionCell>> = client.cells.lock().await.values().cloned().collect();
        for cell in cells.into_iter().filter(|c| *c.open.lock().unwrap()) {
            let url = untrusted::state(&cell.key)
                .map(|s| s.url)
                .unwrap_or_default();
            out.push(Live {
                agent: cell.agent.clone(),
                user: cell.user.clone(),
                session: cell.last_session.lock().unwrap().clone(),
                host: host_of(&url).filter(|_| !url.starts_with("about:")),
                url,
                idle_secs: cell.last_used.lock().unwrap().elapsed().as_secs(),
            });
        }
    }
    out.sort_by_key(|l| l.idle_secs);
    out
}

/// Which tabs [`close`] closes.
#[derive(Debug, Clone, Copy)]
pub enum Which<'a> {
    All,
    Agent(&'a str),
    /// Those whose last call came from this session (its id).
    Session(&'a str),
}

/// Closes the tabs `which` names now, and forgets what they read. The
/// person stopped them: a hostile page they read holds them back no longer.
/// How many were open.
pub async fn close(which: Which<'_>) -> usize {
    close_where(which, true).await
}

async fn close_where(which: Which<'_>, stopped: bool) -> usize {
    let mut n = 0;
    for client in clients() {
        let taken: Vec<Arc<ExtensionCell>> = {
            let mut cells = client.cells.lock().await;
            let ids: Vec<SessionId> = cells
                .iter()
                .filter(|(_, cell)| match which {
                    Which::All => true,
                    Which::Agent(agent) => cell.agent == agent,
                    Which::Session(session) => {
                        cell.last_session.lock().unwrap().as_deref() == Some(session)
                    }
                })
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter().filter_map(|id| cells.remove(id)).collect()
        };
        for cell in taken {
            if stopped {
                untrusted::calm(&cell.key);
            }
            if cell.close().await {
                n += 1;
            }
        }
    }
    n
}

/// The person closed a tab of Desk's in Chrome, or pressed Stop in the
/// extension: its cell lets go, as Stop does.
pub async fn tab_closed(key: &str) {
    for client in clients() {
        let cell = {
            let mut cells = client.cells.lock().await;
            let id = cells.keys().find(|id| id.0 == key).cloned();
            id.and_then(|id| cells.remove(&id))
        };
        if let Some(cell) = cell {
            *cell.open.lock().unwrap() = false;
            untrusted::calm(&cell.key);
            untrusted::forget(&cell.key);
        }
    }
}

/// At the end of a turn: closes the tab the turn's session (its id) was
/// using.
pub async fn turn_ended(session: &str) -> usize {
    close_where(Which::Session(session), false).await
}

/// Closes the tabs whose last call came from session `id` now, and says,
/// for each, its agent and the conversation's key (what its allowances are
/// kept under).
pub async fn stop_session(id: &str) -> Vec<(String, String)> {
    let mut keys = Vec::new();
    for client in clients() {
        let cells = client.cells.lock().await;
        for cell in cells.values() {
            if cell.last_session.lock().unwrap().as_deref() == Some(id) {
                if let Some(key) = cell.last_key.lock().unwrap().clone() {
                    keys.push((cell.agent.clone(), key));
                }
            }
        }
    }
    close(Which::Session(id)).await;
    keys
}

/// What a call was decided for, carried from the gate's decision to the
/// call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Decided {
    /// Acts on this origin (`None`: a page with none), or not at all.
    Origin(Option<String>),
    /// Opens this origin, which the person allowed for this call (`None`:
    /// a page with none).
    Visit(Option<String>),
}

/// What each call was decided for, by the identity making it and the call
/// itself ([`call_key`]): taken by that call alone.
static EXPECTED: std::sync::LazyLock<StdMutex<HashMap<(SessionId, String), Decided>>> =
    std::sync::LazyLock::new(Default::default);

tokio::task_local! {
    /// The call being made, as [`call_key`] names it.
    pub(super) static CALL_KEY: Call;
}

/// The call being made: its key, and whether it changes the page.
#[derive(Debug, Clone)]
pub(super) struct Call {
    pub(super) key: String,
    pub(super) write: bool,
}

/// The fields of a call's input that name what it acts on: the gate and
/// the call read them alike. What is typed (`value`, `text`) is left out,
/// since a credential may be put in for it between the two.
const KEYED: &[&str] = &[
    "url",
    "selector",
    "key",
    "artifact_id",
    "filename",
    "format",
    "connector",
    "timeout_ms",
];

/// Which call this is, as both the gate's decision and the call itself name
/// it: the tool and its input, less what a credential may be put in for
/// between the two (`value`, `text`).
pub(super) fn call_key(tool: &str, input: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(tool.as_bytes());
    for field in KEYED {
        hasher.update([0x1f]);
        hasher.update(field.as_bytes());
        hasher.update([0x1e]);
        match &input[field] {
            Value::Null => {}
            Value::String(v) => hasher.update(v.as_bytes()),
            other => hasher.update(other.to_string().as_bytes()),
        }
    }
    lower_hex(&hasher.finalize())
}

/// Notes what the call `key`, about to be made by the identity in scope,
/// was decided for; `None` clears it.
pub(super) fn expect(key: &str, decided: Option<Decided>) {
    let mut expected = EXPECTED.lock().unwrap();
    let at = (current_session_id(), key.to_string());
    match decided {
        Some(d) => expected.insert(at, d),
        None => expected.remove(&at),
    };
}

/// Where Desk's gate decides every browser call, a write it decided nothing
/// for isn't made: the page or the call changed since.
pub(super) fn undecided_write(decided: &Option<Decided>) -> Result<()> {
    let gated = hooks::hooks().is_some_and(|h| h.gate_decides());
    let write = CALL_KEY.try_with(|c| c.write).unwrap_or(false);
    match (gated, write, decided) {
        (true, true, None) => Err(eyre!(
            "The page or the call changed since this was decided, so nothing was done. Call again."
        )),
        _ => Ok(()),
    }
}

/// Takes what the call in scope was decided for, if it was.
pub(super) fn take_decided() -> Option<Decided> {
    let key = CALL_KEY.try_with(|c| c.key.clone()).ok()?;
    EXPECTED
        .lock()
        .unwrap()
        .remove(&(current_session_id(), key))
}

/// Forgets what `agent`'s call of `tool` with `input` for `user` was
/// decided for: it was refused or declined, so it never comes.
pub fn forget_decision(agent: &str, user: &str, tool: &str, input: &Value) {
    let key = call_key(tool, input);
    EXPECTED
        .lock()
        .unwrap()
        .remove(&(SessionId::derive(agent, user), key));
}

/// The origin the identity in scope's page was last seen on.
pub(super) fn origin_now() -> Option<String> {
    let url = untrusted::state(&current_session_id().0)?.url;
    super::address::origin_of(&url)
}

/// What the tab of `agent` for `user` has read, if it has a page: the
/// hub's gate reads it.
pub fn page_state(agent: &str, user: &str) -> Option<untrusted::PageState> {
    untrusted::state(&SessionId::derive(agent, user).0)
}

/// Stable handle for one `(agent, person)` tab: keys the cell registry and
/// names the tab to the extension.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct SessionId(pub(super) String);

impl SessionId {
    pub(super) fn derive(agent: &str, user: &str) -> Self {
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

/// The tab of the identity in scope.
fn current_session_id() -> SessionId {
    let (agent, user) = current_identity();
    SessionId::derive(&agent, &user)
}

/// The `(agent, user)` in scope, falling back to the system actor and a
/// fixed agent outside one.
fn current_identity() -> (String, String) {
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
    (agent, user)
}

pub use workforce_core::computer::{SIGNED_IN, SIGN_IN};

/// "Sign in for me": the tabs whose person is signing in now, and since
/// when. While one is, its tab takes no other call and isn't closed as idle;
/// it ends when they say they signed in, decline, or the card's wait is
/// long over.
static SIGNING: std::sync::LazyLock<StdMutex<HashMap<SessionId, Instant>>> =
    std::sync::LazyLock::new(Default::default);

/// How long a sign-in holds the tab at most: the card's five minutes, and a
/// margin.
const SIGN_IN_HOLD: Duration = Duration::from_secs(6 * 60);

/// Whether the person is signing in in tab `id` now.
pub(super) fn signing(id: &SessionId) -> bool {
    let mut held = SIGNING.lock().unwrap();
    match held.get(id) {
        Some(since) if since.elapsed() < SIGN_IN_HOLD => true,
        Some(_) => {
            held.remove(id);
            false
        }
        None => false,
    }
}

pub(super) fn start_signing(id: &SessionId) {
    SIGNING.lock().unwrap().insert(id.clone(), Instant::now());
}

pub(super) fn end_signing(id: &SessionId) {
    SIGNING.lock().unwrap().remove(id);
}

/// Whether the person is signing in in the tab in scope.
pub(super) fn signing_now() -> bool {
    signing(&current_session_id())
}

/// The person declined the sign-in card of `agent`'s tab for `user`, or
/// nobody answered it: the tab takes calls again.
pub fn sign_in_ended(agent: &str, user: &str) {
    end_signing(&SessionId::derive(agent, user));
}

/// What a call made while the person is signing in is told.
pub const SIGNING_IN_NOW: &str = "The person is signing in to a site in their Chrome, so nothing else is done in the browser until they finish. Call browser.signed_in to wait for them.";

/// What `browser.sign_in` tells the model to do next.
pub const SIGN_IN_NEXT: &str = "The page is in front of the person, in Desk's window in their Chrome. Call browser.signed_in now: it shows them a card and waits while they sign in themselves, then says where the page is. Never ask for or type their password.";

/// What a picture of a page showing a secret, or a key page, is refused
/// with.
pub const SHOWS_A_SECRET: &str = "The page shows a secret or a new key, so Desk didn't take a picture of it. Read it with browser.snapshot, which hides it.";

/// `url`'s origin and path, without its query or fragment, which may carry
/// a sign-in's codes.
pub(super) fn path_only(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut u) if matches!(u.scheme(), "http" | "https") => {
            u.set_query(None);
            u.set_fragment(None);
            u.to_string()
        }
        _ => super::address::origin_of(url).unwrap_or_default(),
    }
}

/// Whether a call's activity row keeps no picture: what was typed, a page
/// the person signs in to, and a key being read.
pub(super) fn no_screenshot(tool: &str) -> bool {
    matches!(
        tool,
        "browser.fill" | "browser.type" | SIGN_IN | SIGNED_IN | super::keys::CAPTURE_KEY
    )
}

pub struct BrowserClient {
    config: Arc<BrowserSkillConfig>,
    cells: Mutex<HashMap<SessionId, Arc<ExtensionCell>>>,
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
        Self {
            config: Arc::new(config),
            cells: Mutex::new(HashMap::new()),
            store,
        }
    }

    /// Resolves credential markers in a fill or type value at the current
    /// page. In a workflow run a `[[vault:<scope>:<key>]]` marker resolves
    /// through the run's secret policy (deny by default, per-user gate,
    /// curated prefix, `host`-based egress); outside one, under
    /// `browser_creds/`.
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

    /// The tab of the identity in scope.
    async fn cell(&self) -> Arc<ExtensionCell> {
        let (agent, user) = current_identity();
        let id = SessionId::derive(&agent, &user);
        self.cells
            .lock()
            .await
            .entry(id.clone())
            .or_insert_with(|| {
                Arc::new(ExtensionCell::new(self.config.clone(), &id, &agent, &user))
            })
            .clone()
    }

    /// Closes the tabs idle past their time; never one the person is
    /// signing in in.
    pub(super) async fn close_idle(&self) -> usize {
        let idle = Duration::from_millis(self.config.session_idle_ttl_ms);
        let cells: Vec<Arc<ExtensionCell>> = self.cells.lock().await.values().cloned().collect();
        let mut n = 0;
        for cell in cells {
            let id = SessionId(cell.key.clone());
            let open = *cell.open.lock().unwrap();
            if open
                && !signing(&id)
                && cell.last_used.lock().unwrap().elapsed() >= idle
                && cell.close().await
            {
                n += 1;
            }
        }
        n
    }

    pub(super) async fn navigate(&self, url: &str) -> Result<Value> {
        self.cell().await.navigate(url).await
    }

    pub(super) async fn open_for_person(&self, url: &str) -> Result<Value> {
        self.cell().await.open_for_person(url).await
    }

    pub(super) async fn sign_in(&self, url: &str) -> Result<Value> {
        self.cell().await.sign_in(url).await
    }

    pub(super) async fn signed_in(&self) -> Result<Value> {
        self.cell().await.signed_in().await
    }

    pub(super) async fn snapshot(&self) -> Result<Value> {
        self.cell().await.snapshot().await
    }

    pub(super) async fn screenshot(&self) -> Result<Value> {
        self.cell().await.screenshot().await
    }

    pub(super) async fn click(&self, selector: &str) -> Result<Value> {
        self.cell().await.click(selector).await
    }

    pub(super) async fn fill(&self, selector: &str, value: &str) -> Result<Value> {
        self.cell()
            .await
            .fill(selector, value, |raw, host| {
                self.resolve_credential(raw, host)
            })
            .await
    }

    pub(super) async fn type_text(&self, selector: &str, text: &str) -> Result<Value> {
        self.cell()
            .await
            .type_text(selector, text, |raw, host| {
                self.resolve_credential(raw, host)
            })
            .await
    }

    pub(super) async fn select_option(&self, selector: &str, value: &str) -> Result<Value> {
        self.cell().await.select_option(selector, value).await
    }

    pub(super) async fn wait_for_selector(
        &self,
        selector: &str,
        timeout_ms: Option<u64>,
    ) -> Result<Value> {
        self.cell()
            .await
            .wait_for_selector(selector, timeout_ms)
            .await
    }

    pub(super) async fn get_content(&self, format: &str) -> Result<Value> {
        self.cell().await.get_content(format).await
    }

    pub(super) async fn press_key(&self, key: &str, selector: Option<&str>) -> Result<Value> {
        self.cell().await.press_key(key, selector).await
    }

    pub(super) async fn capture_key(&self, selector: &str, connector: &str) -> Result<Value> {
        self.cell().await.capture_key(selector, connector).await
    }

    /// Uploads a staged artifact to a file field on the page: the bytes go
    /// from the artifact store to the extension, never through the model.
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
        if staged.bytes.len() > MAX_UPLOAD_BYTES {
            return Err(eyre!(
                "That file is too big for Desk to hand to Chrome ({} KB at most).",
                MAX_UPLOAD_BYTES / 1000
            ));
        }
        let staged_name = staged.filename.clone();
        let name = filename
            .filter(|s| !s.is_empty())
            .map(sanitised)
            .unwrap_or_else(|| sanitised(&staged.filename));
        self.cell()
            .await
            .upload_file(
                selector,
                &staged.bytes,
                &name,
                &[staged_name.as_str(), name.as_str()],
            )
            .await
    }
}

/// The most file bytes one upload carries: a message to Chrome is at most
/// 1 MB, and base64 grows by a third.
pub const MAX_UPLOAD_BYTES: usize = 700_000;

/// A file's name as a form sees it: its last part, nothing that climbs.
fn sanitised(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name).trim();
    match base {
        "" | "." | ".." => "upload".into(),
        b => b.to_string(),
    }
}

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

/// Stage a screenshot PNG for auto-delivery, or fall back to inline base64
/// when no artifact store is in scope.
pub(super) fn stage_screenshot_artifact(bytes: Vec<u8>, size: usize) -> Result<Value> {
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

/// The lowercase host of a URL, as Chrome reads it.
pub(super) fn host_of(url: &str) -> Option<String> {
    super::address::host_of(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_budget_defaults_to_the_call_timeout_and_caps_at_max_wait() {
        assert_eq!(wait_budget_ms(None, 30_000), 30_000);
        assert_eq!(wait_budget_ms(Some(5_000), 30_000), 5_000);
        assert_eq!(wait_budget_ms(Some(MAX_WAIT_MS + 1), 30_000), MAX_WAIT_MS);
        assert_eq!(wait_budget_ms(None, MAX_WAIT_MS * 2), MAX_WAIT_MS);
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
        assert_eq!(host_of("about:blank").as_deref(), None);
        assert_eq!(host_of(""), None);
    }

    #[test]
    fn stage_screenshot_falls_back_to_inline_base64_without_agent_or_store() {
        let bytes = vec![0u8; 1024];
        let len = bytes.len();
        let out = stage_screenshot_artifact(bytes, len).unwrap();
        assert_eq!(out["format"], "png");
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
        assert!(out.get("image_base64").is_none());
        let entries = out[GENERATED_IMAGES_KEY].as_array().expect("array");
        assert_eq!(entries[0]["media_type"], "image/png");
        assert_eq!(entries[0]["bytes"].as_u64(), Some(len as u64));
    }

    #[test]
    fn session_id_is_hex_stable_and_separates_identities() {
        let a = SessionId::derive("agent", "alice");
        assert_eq!(a, SessionId::derive("agent", "alice"));
        assert_eq!(a.0.len(), 64);
        assert!(a.0.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(a, SessionId::derive("agent", "bob"));
        assert_ne!(SessionId::derive("a", "bc"), SessionId::derive("ab", "c"));
    }

    #[test]
    fn current_session_id_keys_on_the_scoped_user_and_falls_back() {
        let key = |user: &str| {
            let user = user.to_string();
            CURRENT_AGENT_ID.sync_scope("agent".to_string(), || {
                CURRENT_USER_ID.sync_scope(user, current_session_id)
            })
        };
        assert_ne!(key("alice"), key("bob"));
        assert_eq!(key("alice"), SessionId::derive("agent", "alice"));
        let unscoped = CURRENT_AGENT_ID.sync_scope("agent".to_string(), current_session_id);
        assert_eq!(unscoped, SessionId::derive("agent", SYSTEM_USER_ID));
    }

    #[test]
    fn a_sign_ins_address_keeps_its_origin_and_path_only() {
        assert_eq!(
            path_only("https://login.example/oauth/callback?code=s3cret&state=x#token=t"),
            "https://login.example/oauth/callback"
        );
        assert_eq!(path_only("https://shop.example"), "https://shop.example/");
        assert_eq!(path_only("about:blank"), "");
    }

    #[test]
    fn a_sign_in_holds_its_tab_until_it_ends_or_its_card_is_long_over() {
        let id = SessionId::derive("agent", "signer");
        start_signing(&id);
        assert!(signing(&id));
        sign_in_ended("agent", "signer");
        assert!(!signing(&id));
        SIGNING.lock().unwrap().insert(
            id.clone(),
            Instant::now().checked_sub(SIGN_IN_HOLD).unwrap(),
        );
        assert!(!signing(&id), "a sign-in long past its card ends");
    }

    #[test]
    fn nothing_typed_signed_in_or_keyed_is_pictured() {
        for tool in [
            "browser.fill",
            "browser.type",
            SIGN_IN,
            SIGNED_IN,
            super::super::keys::CAPTURE_KEY,
        ] {
            assert!(no_screenshot(tool), "{tool}");
        }
        assert!(!no_screenshot("browser.click"));
    }

    #[test]
    fn an_uploads_name_never_climbs() {
        assert_eq!(sanitised("../../etc/passwd"), "passwd");
        assert_eq!(sanitised(r"C:\x\report.pdf"), "report.pdf");
        assert_eq!(sanitised(".."), "upload");
    }
}
