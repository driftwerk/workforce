//! Desk's browser on a hub with `[desk]`: what the browser skill is told
//! (whether an agent is unbound, what on a page is the person's to do) and
//! what it reports back (every navigation and change, recorded in the
//! agent's Desk activity with its page and a screenshot), and the routes
//! Desk's "Desk is using the browser" indicator reads and stops.
//!
//! - `GET /api/desk/browser`: the browsers open now, with the page each is
//!   on.
//! - `POST /api/desk/browser/stop` (`{ "agent"?: … }`): closes them now,
//!   stops the turns using them, and drops every "Allow for this session"
//!   a browser holds.
//! - `GET /api/desk/browser/shots/{file}`: one screenshot an activity row
//!   names.
//! - `GET`/`PUT /api/desk/browser/settings`: whether Desk uses the
//!   person's Chrome, the sites it may and may not go to, and the Chrome
//!   profiles Desk's extension connects from. Kept in
//!   `…/Desk/browser/settings.json`, which no agent's tools reach.
//! - `POST /api/desk/browser/chrome/{id}/confirm`, `…/choose`, and
//!   `DELETE /api/desk/browser/chrome/{id}`: the person confirms a Chrome
//!   profile asking to connect, chooses the one Desk uses, or forgets one.
//! - `GET /ws/desk/chrome`: Desk's extension, through Desk Helper
//!   (`desk_chrome`).
//!
//! When a turn ends, Desk closes the tab it used in the person's Chrome.
//!
//! Below unbound, what Desk's never-list holds (paying, signing in or out,
//! a credential's file) is refused on the page with a sentence for the
//! person; unbound, nothing is.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tracing::warn;
use uuid::Uuid;
use workforce_common::settings::{AgentWorkspaceConfig, WorkforceConfig};
use workforce_desk::computer::{Activity, Computer};
use workforce_skills::adapters::browser::{self, BrowserHooks, Done, OnPage, Settings, Which};

use crate::desk_chrome::Chrome;

use crate::gateway::{resolve_caller_context, AuthFailure, GatewayState};

/// What Desk tells the browser, and keeps of what it did.
struct DeskHooks {
    state: Arc<GatewayState>,
    config: WorkforceConfig,
    computer: Computer,
    /// `…/Desk/browser/shots`.
    shots: PathBuf,
    settings: Arc<SettingsFile>,
}

/// The person's browser settings, kept in a file of Desk's own and read
/// again when it changes.
pub(crate) struct SettingsFile {
    path: PathBuf,
    read: parking_lot::Mutex<Option<(Option<std::time::SystemTime>, Settings)>>,
}

/// The most sites a list holds.
const MAX_SITES: usize = 200;

impl SettingsFile {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            read: Default::default(),
        }
    }

    /// What the person set; Desk's defaults (on, any site) where they set
    /// nothing. One that can't be read is the browser off, said in the log.
    pub(crate) fn get(&self) -> Settings {
        let at = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        if let Some((seen, s)) = self.read.lock().as_ref() {
            if *seen == at && at.is_some() {
                return s.clone();
            }
        }
        let s = match std::fs::read(&self.path) {
            Err(_) => Settings::default(),
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                warn!(error = %e, "Desk's browser settings can't be read; the browser is off");
                Settings {
                    on: false,
                    ..Settings::default()
                }
            }),
        };
        *self.read.lock() = Some((at, s.clone()));
        s
    }

    fn set(&self, settings: &Settings) -> eyre::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(settings)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &self.path)?;
        *self.read.lock() = None;
        Ok(())
    }
}

/// `settings` as the person may keep them, or what is wrong, in words.
fn checked(mut settings: Settings) -> Result<Settings, String> {
    for (what, list) in [
        ("allowed", &mut settings.sites.allow),
        ("kept away from", &mut settings.sites.block),
    ] {
        list.iter_mut()
            .for_each(|s| *s = s.trim().to_ascii_lowercase());
        list.retain(|s| !s.is_empty());
        list.dedup();
        if list.len() > MAX_SITES {
            return Err(format!("At most {MAX_SITES} sites can be {what}."));
        }
        if let Some(bad) = list.iter().find(|s| {
            s.len() > 253
                || !s
                    .trim_start_matches("*.")
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        }) {
            return Err(format!(
                "\"{bad}\" isn't a site: write its host, like example.com."
            ));
        }
    }
    Ok(settings)
}

/// Whether `agent` is unbound in the browser for this call: the crew says
/// so (Act fully anywhere, every switch on, its budget not spent), and the
/// turn is one the person whose Desk this is started and is in: no job,
/// schedule or other person's turn.
fn unbound_now(state: &Arc<GatewayState>, agent: &str) -> bool {
    let Some(service) = state.registry.get(agent) else {
        return false;
    };
    let o = match AgentWorkspaceConfig::load(&service.workspace) {
        Ok(Some(config)) => match config.oversight() {
            Ok(o) => o,
            Err(_) => return false,
        },
        _ => return false,
    };
    if !o.enabled {
        return false;
    }
    let Some(crew) = crate::desk_crew::for_service(&service) else {
        return false;
    };
    crew.unbound_given(&service, &o) && attended_by_holder(state)
}

/// Whether this turn may be unbound in the browser: the Desk holder's
/// own, and either one they started (no automated origin: no schedule,
/// heartbeat, monitor, workflow or job) or the overseer's own wake-up by
/// the crew ([`CrewWake`]), which the person asked to watch and act.
fn attended_by_holder(state: &Arc<GatewayState>) -> bool {
    use workforce_agent::runner::CURRENT_USER_ID;
    let user = CURRENT_USER_ID
        .try_with(|u| u.clone())
        .ok()
        .filter(|u| !u.is_empty());
    let holder =
        crate::desk_floor::holds_the_hub(state.identity_store.as_ref(), true, user.as_deref());
    attended(
        crate::hub_core::origins::automated_origin().as_deref(),
        crew_wake_now(),
        holder,
    )
}

/// The rule [`attended_by_holder`] applies.
fn attended(automated: Option<&str>, crew_wake: bool, holder: bool) -> bool {
    holder && (crew_wake || automated.is_none())
}

/// The sessions the crew is running an overseer's own wake-up in now (its
/// watcher's check-ins, its plans, lines and handovers): named here as the
/// crew starts the turn, not guessed from a session key.
static CREW_WAKES: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(Default::default);

/// A crew wake-up in progress in one session; it ends when this drops.
pub(crate) struct CrewWake(String);

impl CrewWake {
    pub(crate) fn begin(session: &str) -> Self {
        CREW_WAKES.lock().insert(session.to_string());
        Self(session.to_string())
    }
}

impl Drop for CrewWake {
    fn drop(&mut self) {
        CREW_WAKES.lock().remove(&self.0);
    }
}

/// Whether the turn in scope is a crew wake-up.
fn crew_wake_now() -> bool {
    workforce_agent::runner::CURRENT_SESSION_ID
        .try_with(|id| CREW_WAKES.lock().contains(id))
        .unwrap_or(false)
}

/// What a turn's own Stop reaches, set when Desk's browser is.
static STOPPING: std::sync::OnceLock<Computer> = std::sync::OnceLock::new();

/// A turn's own Stop: the browser its session was using closes now, and
/// the "Allow for this session" answers that conversation held for it go.
pub async fn stopped(session_id: &str) {
    let kept = browser::stop_session(session_id).await;
    let Some(computer) = STOPPING.get() else {
        return;
    };
    for (agent, key) in kept {
        if let Err(e) = computer
            .store()
            .forget_browser_grants_in(&agent, &key)
            .await
        {
            warn!(error = %format!("{e:#}"), "forgetting browser allowances on Stop failed");
        }
    }
}

/// Erases what Desk's browser kept of `person`: the screenshots their
/// activity names, then that activity and their grants. A screenshot
/// already gone is fine; any other failure stops before the rows go, so a
/// retry finds them and finishes.
pub async fn erase_person(computer: &Computer, shots: &Path, person: &str) -> eyre::Result<u64> {
    for name in computer.store().browser_shots_of(person).await? {
        let Some(name) = shot_name(&name) else {
            continue;
        };
        match std::fs::remove_file(shots.join(name)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(eyre::eyre!("couldn't erase screenshot {name}: {e}"));
            }
            _ => {}
        }
    }
    computer.store().erase_access_person(person).await
}

/// Desk's access activity as the identity lifecycle reaches it, on a hub
/// running Desk (`[desk] enabled`); none on a hub without it.
pub fn lifecycle_activity(
    config: &WorkforceConfig,
) -> Option<Arc<dyn crate::identity_lifecycle::DeskActivityStore>> {
    config.desk.enabled.then(|| {
        Arc::new(LifecycleActivity::with(
            || crate::desk_driver::desk().map(|desk| desk.computer()),
            shots_dir(config),
        )) as Arc<dyn crate::identity_lifecycle::DeskActivityStore>
    })
}

/// Where the browser keeps its screenshots: beside its profiles in Desk's
/// folder, or beside the hub's data where that isn't Desk's.
fn shots_dir(config: &WorkforceConfig) -> PathBuf {
    browser_root(config).join("shots")
}

fn browser_root(config: &WorkforceConfig) -> PathBuf {
    crate::desk_folder::browser_profiles(config)
        .unwrap_or_else(|| PathBuf::from(&config.hub.data_dir).join("browser"))
}

pub(crate) struct LifecycleActivity {
    /// The hub's Desk, once it is up.
    computer: Box<dyn Fn() -> Option<Computer> + Send + Sync>,
    shots: PathBuf,
}

impl LifecycleActivity {
    fn with(
        computer: impl Fn() -> Option<Computer> + Send + Sync + 'static,
        shots: PathBuf,
    ) -> Self {
        Self {
            computer: Box::new(computer),
            shots,
        }
    }

    #[cfg(test)]
    pub(crate) fn of(computer: Computer, shots: PathBuf) -> Self {
        Self::with(move || Some(computer.clone()), shots)
    }

    /// The hub's Desk: on a Desk hub whose Desk hasn't started, the
    /// category fails, to be run again, rather than report nothing held.
    fn computer(&self) -> eyre::Result<Computer> {
        (self.computer)().ok_or_else(|| eyre::eyre!("Desk isn't up yet"))
    }
}

#[async_trait::async_trait]
impl crate::identity_lifecycle::DeskActivityStore for LifecycleActivity {
    async fn people(&self) -> eyre::Result<Vec<(String, u64)>> {
        self.computer()?.store().access_people().await
    }

    async fn held(&self, name: &str) -> eyre::Result<u64> {
        self.computer()?.store().access_of(name).await
    }

    async fn activity(&self, name: &str) -> eyre::Result<u64> {
        self.computer()?.store().activity_of(name).await
    }

    async fn reassign(&self, from: &str, into: &str) -> eyre::Result<()> {
        self.computer()?
            .store()
            .move_activity_person(from, into)
            .await?;
        Ok(())
    }

    async fn erase(&self, name: &str) -> eyre::Result<()> {
        erase_person(&self.computer()?, &self.shots, name).await?;
        Ok(())
    }
}

/// The sentence a browser call the never-list holds is refused with.
fn persons(never: workforce_desk::never::Never) -> String {
    format!(
        "Not done: this is the person's to do, since it {}. Tell them what you would do, or ask them to do it in the browser.",
        never.why()
    )
}

impl BrowserHooks for DeskHooks {
    fn settings(&self) -> Settings {
        self.settings.get()
    }

    fn unbound(&self, agent: &str) -> bool {
        unbound_now(&self.state, agent)
    }

    fn gate_decides(&self) -> bool {
        true
    }

    fn holds(&self, agent: &str, what: &OnPage) -> Option<String> {
        if self.unbound(agent) {
            return None;
        }
        let file = what
            .files
            .iter()
            .find_map(|f| workforce_desk::never::path(Path::new(f)));
        file.or_else(|| workforce_desk::never::on_page(&what.shown))
            .map(persons)
    }

    /// A page in the person's Chrome that took itself to another site is
    /// held to that site's visit as the gate decides one: it stays only
    /// where this session may be, without asking.
    fn may_visit(&self, agent: &str, origin: &str) -> Result<(), String> {
        use workforce_core::computer::Verdict;
        let Some(service) = self.state.registry.get(agent) else {
            return Err("Desk couldn't check that site for this agent.".into());
        };
        let subject = crate::serve::ComputerSubject {
            agent: agent.to_string(),
            name: None,
            workspace: service.workspace.clone(),
        };
        let gate = crate::desk_computer::HubComputerGate::for_agent(
            self.computer.clone(),
            &self.config,
            &subject,
        );
        let effect = workforce_core::computer::Effect::Connector {
            connector: format!("chrome-visit:{origin}"),
            write: false,
        };
        use workforce_agent::tools::computer::ComputerGate;
        match gate.verdict("browser.snapshot", &effect) {
            Verdict::Run { .. } => Ok(()),
            Verdict::Refuse { refusal } => Err(refusal.sentence),
            _ => Err("The page took itself to a site this session hasn't been allowed in your Chrome; open it with browser.navigate so you're asked.".into()),
        }
    }

    fn record(&self, done: Done) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let computer = self.computer.clone();
        let shots = self.shots.clone();
        handle.spawn(async move {
            let shot = match done.screenshot.as_deref() {
                Some(png) => match keep_shot(&shots, png) {
                    Ok(name) => Some(name),
                    Err(e) => {
                        warn!(error = %format!("{e:#}"), "keeping a browser screenshot failed");
                        None
                    }
                },
                None => None,
            };
            let activity = activity_of(&done, shot.as_deref());
            if let Err(e) = computer.store().record_activity(&activity).await {
                warn!(error = %format!("{e:#}"), tool = %done.tool, "recording browser activity failed");
            }
        });
    }
}

/// Writes `png` under `shots`, owner-only, and names it.
fn keep_shot(shots: &Path, png: &[u8]) -> eyre::Result<String> {
    std::fs::create_dir_all(shots)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(shots, std::fs::Permissions::from_mode(0o700))?;
    }
    let name = format!("{}.png", Uuid::new_v4());
    let path = shots.join(&name);
    std::fs::write(&path, png)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(name)
}

/// The host of `url`, for the line a person reads.
fn host(url: &str) -> String {
    url.split_once("://")
        .map(|(_, rest)| rest)
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .map(|a| a.rsplit('@').next().unwrap_or(a).to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| url.to_string())
}

/// What a person reads of `done`: "Clicked #save on shop.example".
fn doing(done: &Done) -> String {
    let verb = match done.tool.as_str() {
        "browser.navigate" | "browser.open_for_me" => "Opened",
        "browser.click" => "Clicked",
        "browser.fill" | "browser.type" => "Typed into",
        "browser.press_key" => "Pressed",
        "browser.select_option" => "Chose in",
        "browser.upload_file" => "Uploaded a file to",
        _ => "Used",
    };
    let line = match (done.tool.as_str(), done.target.as_deref()) {
        ("browser.open_for_me", _) => format!("{verb} {} for you", host(&done.url)),
        ("browser.navigate", _) | (_, None) => {
            format!("{verb} {}", host(&done.url))
        }
        (_, Some(target)) => format!("{verb} {target} on {}", host(&done.url)),
    };
    let line = workforce_common::redact::redact_secrets(&line);
    match line.char_indices().nth(300) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line,
    }
}

/// The activity row for `done`: its page, its action and target, and the
/// screenshot kept for it. Never what was typed.
fn activity_of(done: &Done, shot: Option<&str>) -> Activity {
    Activity {
        hub: "local".into(),
        agent: done.agent.clone(),
        person: Some(done.user.clone()).filter(|u| u != workforce_agent::runner::SYSTEM_USER_ID),
        session_id: done.session.clone(),
        request_id: None,
        kind: "browser".into(),
        doing: doing(done),
        folder: None,
        connector: Some("browser".into()),
        tool: done.tool.clone(),
        vault_source: None,
        decided_by: "rule".into(),
        outcome: json!({ "outcome": "ran" }),
        reason: None,
        exit_code: None,
        duration_ms: None,
        arguments: json!({
            "tool": done.tool,
            "url": workforce_common::redact::redact_secrets(&done.url),
            "target": done.target.as_deref().map(workforce_common::redact::redact_secrets),
            "backend": done.backend,
            "screenshot": shot,
        }),
        device: None,
    }
}

struct Routes {
    state: Arc<GatewayState>,
    computer: Computer,
    shots: PathBuf,
    settings: Arc<SettingsFile>,
    chrome: Arc<Chrome>,
}

type S = State<Arc<Routes>>;

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

#[derive(Deserialize, Default)]
struct StopBody {
    /// One agent's browser; every one where absent.
    agent: Option<String>,
}

fn refusal(status: StatusCode, error: &str) -> Response {
    (status, Json(json!({ "error": error }))).into_response()
}

/// Only the person whose Desk this is.
async fn holder(routes: &Routes, headers: &HeaderMap, token: Option<&str>) -> Result<(), Response> {
    let caller = resolve_caller_context(&routes.state, headers, token).await;
    match caller.auth_failure {
        Some(AuthFailure::InvalidCredentials) => {
            return Err(refusal(StatusCode::UNAUTHORIZED, "not allowed"))
        }
        Some(AuthFailure::StoreError) => {
            return Err(refusal(
                StatusCode::SERVICE_UNAVAILABLE,
                "the hub could not check that token",
            ))
        }
        None => {}
    }
    if !crate::desk_floor::holds_the_hub(
        routes.state.identity_store.as_ref(),
        true,
        caller.identity_id.as_deref(),
    ) {
        return Err(refusal(
            StatusCode::FORBIDDEN,
            "Only the person whose Desk this is sees and stops its browser",
        ));
    }
    Ok(())
}

macro_rules! holder {
    ($r:expr, $h:expr, $q:expr) => {
        if let Err(refused) = holder(&$r, &$h, $q.token.as_deref()).await {
            return refused;
        }
    };
}

async fn list(State(r): S, h: HeaderMap, Query(q): Query<TokenQuery>) -> Response {
    holder!(r, h, q);
    Json(json!({ "live": browser::live().await })).into_response()
}

/// Stop: the turns using the browser end, Desk's tabs close now, and every
/// "Allow for this session" a browser holds is dropped.
async fn stop(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    body: Option<Json<StopBody>>,
) -> Response {
    holder!(r, h, q);
    let agent = body.and_then(|b| b.0.agent).filter(|a| !a.is_empty());
    match stop_now(&r.state, &r.computer, agent.as_deref()).await {
        Ok((closed, turns, forgotten)) => Json(json!({ "closed": closed, "turns_stopped": turns, "allowances_dropped": forgotten }))
            .into_response(),
        Err(()) => refusal(
            StatusCode::SERVICE_UNAVAILABLE,
            "The browser closed, but Desk couldn't drop its allowances for this session. Try Stop again.",
        ),
    }
}

/// What Stop does, for one agent or all: cancels the turns using the
/// browser, closes Desk's tabs, and drops the browser's session allowances.
async fn stop_now(
    state: &Arc<GatewayState>,
    computer: &Computer,
    agent: Option<&str>,
) -> Result<(usize, usize, u64), ()> {
    let open = browser::live().await;
    let mut turns = 0;
    for l in open.iter().filter(|l| agent.is_none_or(|a| a == l.agent)) {
        let (Some(session), Some(service)) = (l.session.as_deref(), state.registry.get(&l.agent))
        else {
            continue;
        };
        if crate::gateway::steer(
            state.nats.as_ref(),
            &service,
            session,
            workforce_nats::chat::TurnControl::Cancel,
        )
        .await
        .applied()
        {
            turns += 1;
        }
    }
    let closed = browser::close(match agent {
        Some(a) => Which::Agent(a),
        None => Which::All,
    })
    .await;
    match computer.store().forget_browser_grants(agent).await {
        Ok(n) => Ok((closed, turns, n)),
        Err(e) => {
            warn!(error = %format!("{e:#}"), "forgetting browser allowances failed");
            Err(())
        }
    }
}

/// What Stop reaches from the extension's own Stop button.
static STOP_WITH: std::sync::OnceLock<(Arc<GatewayState>, Computer)> = std::sync::OnceLock::new();

/// The person pressed Stop in Desk's extension: as Desk's own Stop does,
/// for every agent.
pub async fn stop_everything() {
    if let Some((state, computer)) = STOP_WITH.get() {
        let _ = stop_now(state, computer, None).await;
    }
}

/// The settings, and how Desk's extension stands: whether the Chrome Desk
/// uses is connected now, every profile it connects from, and where to get
/// the extension.
async fn settings_view(r: &Routes) -> serde_json::Value {
    let store_url = (!crate::desk_chrome::STORE_ID.is_empty()).then(|| {
        format!(
            "https://chromewebstore.google.com/detail/{}",
            crate::desk_chrome::STORE_ID
        )
    });
    json!({
        "settings": r.settings.get(),
        "chrome": {
            "connected": r.chrome.connected(),
            "profiles": r.chrome.view(),
            "store_url": store_url,
        },
    })
}

/// The person answers about a Chrome profile: confirms it, chooses it, or
/// forgets it.
async fn profile(
    State(r): S,
    h: HeaderMap,
    UrlPath((id, what)): UrlPath<(String, String)>,
    Query(q): Query<TokenQuery>,
) -> Response {
    holder!(r, h, q);
    let done = match what.as_str() {
        "confirm" => r.chrome.confirm(&id),
        "choose" => r.chrome.choose(&id),
        _ => return refusal(StatusCode::NOT_FOUND, "no such answer"),
    };
    if !done {
        return refusal(StatusCode::NOT_FOUND, "That Chrome isn't asking any more.");
    }
    Json(settings_view(&r).await).into_response()
}

async fn forget_profile(
    State(r): S,
    h: HeaderMap,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<TokenQuery>,
) -> Response {
    holder!(r, h, q);
    r.chrome.forget(&id);
    browser::close(Which::All).await;
    Json(settings_view(&r).await).into_response()
}

async fn chrome_socket(
    State(r): S,
    ws: axum::extract::ws::WebSocketUpgrade,
    h: HeaderMap,
) -> Response {
    crate::desk_chrome::handler(ws, h, Arc::clone(&r.state)).await
}

async fn get_settings(State(r): S, h: HeaderMap, Query(q): Query<TokenQuery>) -> Response {
    holder!(r, h, q);
    Json(settings_view(&r).await).into_response()
}

/// The person changes them: turned off, Desk's tabs close now.
async fn put_settings(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Json(next): Json<Settings>,
) -> Response {
    holder!(r, h, q);
    let next = match checked(next) {
        Ok(s) => s,
        Err(why) => return refusal(StatusCode::BAD_REQUEST, &why),
    };
    if let Err(e) = r.settings.set(&next) {
        warn!(error = %format!("{e:#}"), "saving Desk's browser settings failed");
        return refusal(
            StatusCode::SERVICE_UNAVAILABLE,
            "Desk couldn't save the browser settings. Try again.",
        );
    }
    if !next.on {
        browser::close(Which::All).await;
    }
    Json(settings_view(&r).await).into_response()
}

/// Whether `file` names a kept screenshot: a UUID and `.png`, nothing else.
fn shot_name(file: &str) -> Option<&str> {
    let id = file.strip_suffix(".png")?;
    Uuid::parse_str(id).ok().map(|_| file)
}

async fn shot(
    State(r): S,
    h: HeaderMap,
    UrlPath(file): UrlPath<String>,
    Query(q): Query<TokenQuery>,
) -> Response {
    holder!(r, h, q);
    let Some(name) = shot_name(&file) else {
        return refusal(StatusCode::NOT_FOUND, "no such screenshot");
    };
    match tokio::fs::read(r.shots.join(name)).await {
        Ok(png) => ([(header::CONTENT_TYPE, "image/png")], png).into_response(),
        Err(_) => refusal(StatusCode::NOT_FOUND, "no such screenshot"),
    }
}

/// Screenshots are kept as long as the activity naming them.
fn prune_shots(shots: &Path) {
    let Ok(entries) = std::fs::read_dir(shots) else {
        return;
    };
    let kept = std::time::Duration::from_secs(
        workforce_desk::computer::ACTIVITY_KEPT_DAYS as u64 * 24 * 60 * 60,
    );
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > kept);
        if old && shot_name(&entry.file_name().to_string_lossy()).is_some() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Gives `service` the browser, if it hasn't it: an overseer reaches for it.
pub fn equip(service: &crate::serve::AgentService) {
    if crate::skill_loader::skill_tools_registered("browser", &service.tools) {
        return;
    }
    let Some(vault) = service.vault.as_ref() else {
        return;
    };
    if let Err(e) = crate::skill_loader::load_and_track(
        "browser",
        Arc::clone(vault),
        service.global_vault.clone(),
        &service.config,
        &service.tools,
        &service.skill_state,
        "desk",
    ) {
        warn!(agent = %service.agent_id, error = %format!("{e:#}"), "giving the overseer the browser failed");
    }
}

/// Installs Desk's browser hooks and returns its routes. Where the hub's
/// data isn't Desk's, screenshots go beside the hub's data.
pub fn install(
    state: &Arc<GatewayState>,
    config: &WorkforceConfig,
    computer: Computer,
    cancel: tokio_util::sync::CancellationToken,
) -> Router {
    let root = browser_root(config);
    let shots = shots_dir(config);
    let settings = Arc::new(SettingsFile::new(root.join("settings.json")));
    let chrome = crate::desk_chrome::install(root.join("chrome-profiles.json"));
    // The hub itself is never a page the person's Chrome opens for Desk.
    browser::keep_off_ports(crate::desk_driver::ports_of([
        config.hub.listen.as_str(),
        config.gateway.listen.as_str(),
    ]));
    let _ = STOP_WITH.set((Arc::clone(state), computer.clone()));
    let hooks = Arc::new(DeskHooks {
        state: Arc::clone(state),
        config: config.clone(),
        computer: computer.clone(),
        shots: shots.clone(),
        settings: Arc::clone(&settings),
    });
    // A turn's end closes the tab it used in the person's Chrome.
    workforce_hub_core::hub_core::live_turns::on_turn_end(Box::new(|session: &str| {
        let session = session.to_string();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                browser::turn_ended(&session).await;
            });
        }
    }));
    {
        let state = Arc::clone(state);
        crate::desk_computer::judge_unbound_with(Box::new(move |agent| unbound_now(&state, agent)));
    }
    {
        // Only the person whose Desk this is uses their Chrome, and its
        // cards are theirs alone.
        workforce_core::computer::browser_is_the_persons_chrome();
        let state = Arc::clone(state);
        crate::desk_computer::judge_holder_with(Box::new(move |user| {
            crate::desk_floor::holds_the_hub(state.identity_store.as_ref(), true, user)
        }));
    }
    let _ = STOPPING.set(computer.clone());
    if !browser::install_hooks(hooks) {
        warn!("Desk's browser hooks were installed twice; the first stand");
    }
    {
        let shots = shots.clone();
        tokio::spawn(async move {
            loop {
                let dir = shots.clone();
                let _ = tokio::task::spawn_blocking(move || prune_shots(&dir)).await;
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(24 * 60 * 60)) => {}
                }
            }
        });
    }
    Router::new()
        .route("/api/desk/browser", get(list))
        .route("/api/desk/browser/stop", post(stop))
        .route("/api/desk/browser/shots/{file}", get(shot))
        .route(
            "/api/desk/browser/settings",
            get(get_settings).put(put_settings),
        )
        .route("/api/desk/browser/chrome/{id}/{what}", post(profile))
        .route(
            "/api/desk/browser/chrome/{id}",
            axum::routing::delete(forget_profile),
        )
        .route("/ws/desk/chrome", get(chrome_socket))
        .with_state(Arc::new(Routes {
            state: Arc::clone(state),
            computer,
            shots,
            settings,
            chrome,
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_uuid_and_png_names_a_screenshot() {
        let good = "0f0e0d0c-0b0a-4909-8807-060504030201.png";
        assert_eq!(shot_name(good), Some(good));
        for bad in [
            "../x.png",
            "../0f0e0d0c-0b0a-4909-8807-060504030201.png",
            "0f0e0d0c-0b0a-4909-8807-060504030201.png/..",
            "/etc/passwd",
            "x.png",
            "0f0e0d0c-0b0a-4909-8807-060504030201.jpg",
            "0f0e0d0c-0b0a-4909-8807-060504030201",
            "",
        ] {
            assert_eq!(shot_name(bad), None, "{bad}");
        }
    }

    /// A hub without Desk gives the lifecycle nothing to reach; a Desk hub
    /// whose Desk isn't up yet fails the category rather than say nothing
    /// is held.
    #[tokio::test]
    async fn a_hub_without_desk_has_no_activity_and_one_not_up_yet_fails() {
        use crate::identity_lifecycle::DeskActivityStore as _;
        let config = WorkforceConfig::default();
        assert!(!config.desk.enabled);
        assert!(lifecycle_activity(&config).is_none());

        let waiting = LifecycleActivity::with(|| None, PathBuf::from("shots"));
        for e in [
            waiting.people().await.map(|_| ()),
            waiting.held("sarah").await.map(|_| ()),
            waiting.erase("sarah").await,
        ] {
            assert_eq!(e.unwrap_err().to_string(), "Desk isn't up yet");
        }
    }

    /// Erasing a person takes the screenshots their activity names and
    /// nothing a hostile name points at; one that can't be removed stops
    /// before the rows go, so a retry finds them and finishes.
    #[tokio::test]
    async fn erasing_a_person_takes_their_screenshots_only_and_a_retry_finishes() {
        workforce_postgres::test_support::with_test_pool(
            workforce_desk::store::build_pool,
            |pool| async move {
                let store = Arc::new(workforce_desk::DeskStore::with_pool(pool).await.unwrap());
                let computer = Computer::new(Arc::clone(&store));
                let dir = tempfile::tempdir().unwrap();
                let shots = dir.path().join("shots");
                std::fs::create_dir_all(&shots).unwrap();
                let outside = dir.path().join("x.png");
                std::fs::write(&outside, b"not a screenshot").unwrap();
                let (kept, stuck) = (
                    format!("{}.png", Uuid::new_v4()),
                    format!("{}.png", Uuid::new_v4()),
                );
                std::fs::write(shots.join(&kept), b"png").unwrap();
                // A directory where a screenshot should be: removing it fails.
                std::fs::create_dir_all(shots.join(&stuck).join("inside")).unwrap();
                let mut d = done("browser.click", Some("#save"));
                for shot in [kept.as_str(), "../x.png", stuck.as_str()] {
                    store
                        .record_activity(&activity_of(&d, Some(shot)))
                        .await
                        .unwrap();
                }
                d.user = "tom".into();
                store.record_activity(&activity_of(&d, None)).await.unwrap();

                let failed = erase_person(&computer, &shots, "sarah").await;
                assert!(failed.is_err(), "{failed:?}");
                assert_eq!(
                    store.activity_of("sarah").await.unwrap(),
                    3,
                    "the rows stay for a retry"
                );

                std::fs::remove_dir_all(shots.join(&stuck)).unwrap();
                assert_eq!(erase_person(&computer, &shots, "sarah").await.unwrap(), 3);
                assert_eq!(store.activity_of("sarah").await.unwrap(), 0);
                assert!(!shots.join(&kept).exists());
                assert!(outside.exists(), "a name outside the folder is ignored");
                assert_eq!(store.activity_of("tom").await.unwrap(), 1);
            },
        )
        .await;
    }

    fn done(tool: &str, target: Option<&str>) -> Done {
        Done {
            agent: "allan".into(),
            user: "sarah".into(),
            session: Some("s1".into()),
            tool: tool.into(),
            backend: "desk",
            url: "https://shop.example/cart?x=1".into(),
            target: target.map(str::to_string),
            screenshot: None,
        }
    }

    #[test]
    fn a_row_says_what_was_done_where_and_names_its_screenshot() {
        let row = activity_of(&done("browser.click", Some("#save")), Some("a.png"));
        assert_eq!(row.kind, "browser");
        assert_eq!(
            row.outcome["outcome"], "ran",
            "as Desk's activity list reads it"
        );
        assert_eq!(row.doing, "Clicked #save on shop.example");
        assert_eq!(row.connector.as_deref(), Some("browser"));
        assert_eq!(row.arguments["url"], "https://shop.example/cart?x=1");
        assert_eq!(row.arguments["target"], "#save");
        assert_eq!(row.arguments["screenshot"], "a.png");
        assert_eq!(row.arguments["backend"], "desk");
        assert_eq!(row.person.as_deref(), Some("sarah"));
        assert_eq!(
            activity_of(
                &done("browser.navigate", Some("https://shop.example/")),
                None
            )
            .doing,
            "Opened shop.example"
        );
        assert_eq!(
            activity_of(
                &done("browser.open_for_me", Some("https://staging.example.com/")),
                None
            )
            .doing,
            "Opened shop.example for you"
        );
    }

    #[test]
    fn a_row_never_carries_a_credential() {
        let token = format!("lin_api_{}", "a1".repeat(15));
        let mut d = done("browser.click", Some(&format!("#k-{token}")));
        d.url = format!("https://shop.example/?key={token}");
        let row = activity_of(&d, None);
        let text = serde_json::to_string(&row.arguments).unwrap() + &row.doing;
        assert!(!text.contains(&token), "{text}");
    }

    #[test]
    fn only_a_kept_screenshots_name_is_served() {
        let id = Uuid::new_v4();
        assert!(shot_name(&format!("{id}.png")).is_some());
        for bad in [
            "../x.png",
            "a.png",
            &format!("{id}.png/../../etc"),
            &id.to_string(),
        ] {
            assert!(shot_name(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn screenshots_are_kept_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let shots = dir.path().join("shots");
        let name = keep_shot(&shots, b"png").unwrap();
        assert!(shot_name(&name).is_some());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&shots), 0o700);
            assert_eq!(mode(&shots.join(&name)), 0o600);
        }
    }

    #[test]
    fn the_never_list_reads_what_a_page_shows() {
        use workforce_desk::never::{on_page, Never};
        assert_eq!(on_page("Buy now"), Some(Never::Money));
        assert!(persons(Never::Money).contains("spends or moves money"));
    }

    #[test]
    fn settings_default_to_on_and_keep_what_was_set() {
        let dir = tempfile::tempdir().unwrap();
        let file = SettingsFile::new(dir.path().join("browser/settings.json"));
        assert!(file.get().on, "nothing set: on");
        let set = Settings {
            on: false,
            sites: browser::Sites {
                allow: vec![],
                block: vec!["bank.test".into()],
            },
        };
        file.set(&set).unwrap();
        assert_eq!(file.get(), set);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("browser/settings.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::write(dir.path().join("browser/settings.json"), "{ not json").unwrap();
        // A later mtime may share the second; drop what was read.
        *file.read.lock() = None;
        assert!(!file.get().on, "unreadable: off");
    }

    #[test]
    fn a_site_list_is_hosts_only() {
        let ok = checked(Settings {
            sites: browser::Sites {
                allow: vec![" Example.COM ".into(), "*.docs.test".into(), "".into()],
                block: vec![],
            },
            ..Settings::default()
        })
        .unwrap();
        assert_eq!(ok.sites.allow, vec!["example.com", "*.docs.test"]);
        for bad in ["https://x.test/", "a b", "x.test/path"] {
            let err = checked(Settings {
                sites: browser::Sites {
                    allow: vec![],
                    block: vec![bad.into()],
                },
                ..Settings::default()
            })
            .unwrap_err();
            assert!(err.contains("isn't a site"), "{bad}: {err}");
        }
    }

    /// A schedule, heartbeat, monitor or workflow turn is never unbound in
    /// the browser, even run as the Desk holder; the overseer's own crew
    /// wake-up is; anyone but the holder never is.
    #[tokio::test]
    async fn only_the_holders_own_turns_and_the_overseers_crew_wake_ups_are_unbound() {
        use workforce_agent::runner::{CURRENT_SESSION_ID, CURRENT_SESSION_KEY};
        assert!(attended(None, false, true), "a turn the person started");
        assert!(!attended(None, false, false), "someone else's turn");
        assert!(!attended(Some("scheduler"), false, true));
        assert!(
            attended(Some("scheduler"), true, true),
            "the crew's own wake-up"
        );
        assert!(!attended(None, true, false));
        for key in [
            "scheduler:job-1",
            "heartbeat:allan",
            "workflow-engine:w1",
            "monitor:m1",
        ] {
            let origin = CURRENT_SESSION_KEY
                .scope(key.into(), async {
                    crate::hub_core::origins::automated_origin()
                })
                .await;
            assert!(origin.is_some(), "{key} reads as automated");
            assert!(!attended(origin.as_deref(), false, true), "{key}");
        }
        let id = uuid::Uuid::new_v4().to_string();
        let woken = CURRENT_SESSION_ID
            .scope(id.clone(), async {
                let wake = CrewWake::begin(&id);
                let during = crew_wake_now();
                drop(wake);
                (during, crew_wake_now())
            })
            .await;
        assert_eq!(
            woken,
            (true, false),
            "named while the crew runs it, and only then"
        );
        let other = CURRENT_SESSION_ID
            .scope("another".into(), async { crew_wake_now() })
            .await;
        assert!(!other);
    }
}
