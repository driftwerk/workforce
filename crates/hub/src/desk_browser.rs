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
//! - `GET`/`PUT /api/desk/browser/settings`: which browsers Desk uses (Off,
//!   Desk's own, My Chrome, Both), whether Desk's own shows on screen, the
//!   sites My Chrome may and may not go to, and whether the person's Chrome
//!   has remote debugging on and Desk is connected to it. Kept in
//!   `…/Desk/browser/settings.json`, which no agent's tools reach.
//!
//! When a turn ends, Desk lets go of the person's Chrome there.
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

use crate::gateway::{resolve_caller_context, AuthFailure, GatewayState};

/// What Desk tells the browser, and keeps of what it did.
struct DeskHooks {
    state: Arc<GatewayState>,
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

    /// What the person set; Desk's defaults (both browsers) where they set
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
                    uses: browser::Uses::Off,
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
        let Some(service) = self.state.registry.get(agent) else {
            return false;
        };
        match AgentWorkspaceConfig::load(&service.workspace) {
            Ok(Some(config)) => config
                .oversight()
                .is_ok_and(|o| crate::desk_computer::unbound(&o)),
            _ => false,
        }
    }

    fn holds(&self, agent: &str, what: &OnPage) -> Option<String> {
        if self.unbound(agent) {
            return None;
        }
        let file = what
            .file
            .as_deref()
            .and_then(|f| workforce_desk::never::path(Path::new(f)));
        file.or_else(|| workforce_desk::never::on_page(&what.shown))
            .map(persons)
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
        "browser.navigate" => "Opened",
        "browser.click" => "Clicked",
        "browser.fill" | "browser.type" => "Typed into",
        "browser.press_key" => "Pressed",
        "browser.select_option" => "Chose in",
        "browser.evaluate" => "Ran a script on",
        "browser.upload_file" => "Uploaded a file to",
        _ => "Used",
    };
    let line = match (done.tool.as_str(), done.target.as_deref()) {
        ("browser.navigate", _) | ("browser.evaluate", _) | (_, None) => {
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
    /// Where the person's Chrome writes `DevToolsActivePort`.
    chrome_dir: Option<PathBuf>,
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

/// Stop: the turns using the browser end, the browser closes now, and
/// every "Allow for this session" a browser holds is dropped.
async fn stop(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    body: Option<Json<StopBody>>,
) -> Response {
    holder!(r, h, q);
    let agent = body.and_then(|b| b.0.agent).filter(|a| !a.is_empty());
    let open = browser::live().await;
    let mut turns = 0;
    for l in open
        .iter()
        .filter(|l| agent.as_deref().is_none_or(|a| a == l.agent))
    {
        let (Some(session), Some(service)) = (l.session.as_deref(), r.state.registry.get(&l.agent))
        else {
            continue;
        };
        if crate::gateway::steer(
            r.state.nats.as_ref(),
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
    let closed = browser::close(match agent.as_deref() {
        Some(a) => Which::Agent(a),
        None => Which::All,
    })
    .await;
    let forgotten = match r
        .computer
        .store()
        .forget_browser_grants(agent.as_deref())
        .await
    {
        Ok(n) => n,
        Err(e) => {
            warn!(error = %format!("{e:#}"), "forgetting browser allowances failed");
            return refusal(
                StatusCode::SERVICE_UNAVAILABLE,
                "The browser closed, but Desk couldn't drop its allowances for this session. Try Stop again.",
            );
        }
    };
    Json(json!({ "closed": closed, "turns_stopped": turns, "allowances_dropped": forgotten }))
        .into_response()
}

/// The settings, and how the person's Chrome stands: remote debugging on
/// (`available`) and Desk attached to it now (`connected`).
async fn settings_view(r: &Routes) -> serde_json::Value {
    let connected = browser::live().await.iter().any(|l| l.backend == "chrome");
    json!({
        "settings": r.settings.get(),
        "chrome": {
            "available": r.chrome_dir.as_deref().is_some_and(browser::chrome_available),
            "connected": connected,
            "min_version": browser::MIN_CHROME,
            "turn_on": "chrome://inspect/#remote-debugging",
        },
    })
}

async fn get_settings(State(r): S, h: HeaderMap, Query(q): Query<TokenQuery>) -> Response {
    holder!(r, h, q);
    Json(settings_view(&r).await).into_response()
}

/// The person changes them: a browser turned off closes, and My Chrome
/// turned off is let go, now.
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
    use browser::{Backend, Uses};
    match next.uses {
        Uses::Off => {
            browser::close(Which::All).await;
        }
        Uses::Desk => {
            browser::close_backend(Backend::Chrome).await;
        }
        Uses::Chrome => {
            browser::close_backend(Backend::Desk).await;
        }
        Uses::Both => {}
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
    let root = crate::desk_folder::browser_profiles(config)
        .unwrap_or_else(|| PathBuf::from(&config.hub.data_dir).join("browser"));
    let shots = root.join("shots");
    let settings = Arc::new(SettingsFile::new(root.join("settings.json")));
    let chrome_dir = config
        .skills
        .browser
        .chrome_user_data_dir
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(browser::chrome_user_data_dir);
    let hooks = Arc::new(DeskHooks {
        state: Arc::clone(state),
        computer: computer.clone(),
        shots: shots.clone(),
        settings: Arc::clone(&settings),
    });
    // A turn's end lets go of the person's Chrome it was using.
    workforce_hub_core::hub_core::live_turns::on_turn_end(Box::new(|session: &str| {
        let session = session.to_string();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                browser::turn_ended(&session).await;
            });
        }
    }));
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
        .with_state(Arc::new(Routes {
            state: Arc::clone(state),
            computer,
            shots,
            settings,
            chrome_dir,
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn settings_default_to_both_and_keep_what_was_set() {
        let dir = tempfile::tempdir().unwrap();
        let file = SettingsFile::new(dir.path().join("browser/settings.json"));
        assert_eq!(file.get().uses, browser::Uses::Both, "nothing set: both on");
        let set = Settings {
            uses: browser::Uses::Chrome,
            show: Some(false),
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
        assert_eq!(file.get().uses, browser::Uses::Off, "unreadable: off");
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
}
