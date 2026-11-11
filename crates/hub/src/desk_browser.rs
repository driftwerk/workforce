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
use workforce_skills::adapters::browser::{self, BrowserHooks, Done, OnPage, Which};

use crate::gateway::{resolve_caller_context, AuthFailure, GatewayState};

/// What Desk tells the browser, and keeps of what it did.
struct DeskHooks {
    state: Arc<GatewayState>,
    computer: Computer,
    /// `…/Desk/browser/shots`.
    shots: PathBuf,
}

/// The sentence a browser call the never-list holds is refused with.
fn persons(never: workforce_desk::never::Never) -> String {
    format!(
        "Not done: this is the person's to do, since it {}. Tell them what you would do, or ask them to do it in the browser.",
        never.why()
    )
}

impl BrowserHooks for DeskHooks {
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
    let shots = crate::desk_folder::browser_profiles(config)
        .unwrap_or_else(|| PathBuf::from(&config.hub.data_dir).join("browser"))
        .join("shots");
    let hooks = Arc::new(DeskHooks {
        state: Arc::clone(state),
        computer: computer.clone(),
        shots: shots.clone(),
    });
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
        .with_state(Arc::new(Routes {
            state: Arc::clone(state),
            computer,
            shots,
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
}
