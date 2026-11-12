//! Desk boards synced with Linear or Jira (`workforce_desk::board_sync`).
//!
//! A board syncs **through a hub that has the integration, as the person**:
//! this hub calls that hub's remote MCP (`/mcp`, `crates/mcp-client`) with
//! the person's `wfr_` token for it, so that hub's permissions, gates and
//! audit apply to every call and no Jira or Linear key is ever copied here.
//! Only where no linked hub offers the system does a board use **this
//! hub's own** `jira.*` / `linear.*` tools, in-process, through the same
//! gate chain the remote MCP server runs (`GatedExecutor`), as the person.
//!
//! The token is kept in this hub's vault only, at
//! `users/<person>/desk/hubs/<hub>/remote_mcp`, which no agent's vault tools
//! can read, list or write (`AgentReadDenial::DeskLink`). It is never in
//! config, Postgres, events or the UI, never logged, and held in memory as a
//! [`Secret`] whose `Debug` and `Display` print `wfr_…redacted`.
//!
//! Routes, the person's whose hub this is only:
//! - `GET /api/desk/sync-hubs`: the hubs a board can sync through, and
//!   whether each has a token. Never a token.
//! - `PUT /api/desk/sync-hubs/{hub}` `{name, url, token}`: checks the token
//!   with `tools/list`, then keeps it (write-only).
//! - `DELETE /api/desk/sync-hubs/{hub}`: forgets the hub and its token; its
//!   boards pause.
//! - `POST /api/desk/boards/link/check` `{system, via, target?, filter?, me?}`:
//!   what the link dialog needs: whether the hub offers the tools (and if
//!   not, what its admin must add), the teams, the states with the map
//!   prefilled, who the person is there (`linear.viewer`, `jira.myself`),
//!   and how many issues the filter matches, counted page by page up to
//!   [`sync::MATCH_CEILING`] (past it, "or more").
//! - `PUT|DELETE /api/desk/boards/{id}/link`: links or unlinks a board.
//! - `POST /api/desk/boards/{id}/pause` `{paused}`: pauses or resumes it.
//! - `POST /api/desk/boards/{id}/me`: asks the hub again who the person
//!   is there, and keeps its answer.
//! - `POST /api/desk/tickets/{id}/take`: Take it. The ticket's issue, no
//!   one's there, is assigned to the person, as themselves, through the
//!   route the board syncs by.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_common::settings::VaultActor;
use workforce_desk::board::{Filter, Paused, System, Via};
use workforce_desk::board_sync::{self as sync, Remote, RemoteError, RemoteState};
use workforce_desk::{Desk, DeskError, LinkBoard};
use workforce_mcp_client::{McpClient, McpError, McpLimits, StaticBearer};

use crate::gateway::{resolve_caller_context, AuthFailure, GatewayState};
use crate::vault::Vault;

/// The longest the link dialog waits on one hub when asking which offers a
/// system.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The longest a call to a hub may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

// ── The token ────────────────────────────────────────────────────────────

/// A person's `wfr_` token for a hub. Never printed, even in part.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(token: &str) -> Option<Self> {
        let t = token.trim();
        (t.starts_with(workforce_identity::remote_mcp_token::TOKEN_PREFIX)
            && t.len() > 20
            && t.len() < 200
            && t.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .then(|| Self(t.to_string()))
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("wfr_…redacted")
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("wfr_…redacted")
    }
}

/// Where `person`'s token for `hub` is kept in this hub's vault.
pub fn token_key(person: &str, hub: &str) -> String {
    format!("users/{person}/desk/hubs/{hub}/remote_mcp")
}

fn read_token(vault: &Vault, person: &str, hub: &str) -> Option<Secret> {
    vault
        .get(&token_key(person, hub))
        .and_then(|t| Secret::new(&t))
}

// ── A hub, through its remote MCP ────────────────────────────────────────

/// Whether `id` can name an agent in a path: what a hub's agent ids are.
pub fn agent_id_ok(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A linked hub's tools, called through its remote MCP as the person: its
/// default agent's (`/mcp`), or one agent's (`/mcp/agents/{id}`).
pub struct HubRemote {
    client: McpClient,
    ready: tokio::sync::OnceCell<()>,
}

impl HubRemote {
    pub fn new(
        url: &str,
        token: &Secret,
        http: Arc<reqwest::Client>,
        agent: Option<&str>,
    ) -> Result<Self, RemoteError> {
        let base = url.trim_end_matches('/');
        let path = match agent {
            Some(a) if agent_id_ok(a) => format!("{base}/mcp/agents/{a}"),
            Some(_) => return Err(RemoteError::Refused("that isn't an agent's id".into())),
            None => format!("{base}/mcp"),
        };
        let endpoint = url::Url::parse(&path)
            .map_err(|_| RemoteError::Offline("the hub's address isn't one".into()))?;
        Ok(Self {
            client: McpClient::new(
                endpoint,
                http,
                Arc::new(StaticBearer::new(token.expose())),
                McpLimits::default(),
            ),
            ready: tokio::sync::OnceCell::new(),
        })
    }

    async fn ready(&self) -> Result<(), RemoteError> {
        self.ready
            .get_or_try_init(|| async {
                self.client.initialize().await.map(drop).map_err(mcp_error)
            })
            .await
            .map(|_| ())
    }
}

/// What a hub's answer means for the sync. Its words are already redacted
/// by the client; they are clipped here too.
fn mcp_error(e: McpError) -> RemoteError {
    let said = |s: &str| s.chars().take(300).collect::<String>();
    match e {
        McpError::Unreachable(m) => RemoteError::Offline(said(&m)),
        McpError::AuthExpired(_) => RemoteError::NoToken,
        McpError::RateLimited {
            retry_after_secs, ..
        } => RemoteError::RateLimited { retry_after_secs },
        McpError::Transport(m) if m.contains("404") => {
            RemoteError::Refused("the hub doesn't offer remote MCP".into())
        }
        McpError::Transport(m) => RemoteError::Offline(said(&m)),
        McpError::Protocol(m) if m.contains("404") => {
            RemoteError::Refused("the hub doesn't offer remote MCP".into())
        }
        McpError::ServerError { message, .. } => RemoteError::Failed(said(&message)),
        other => RemoteError::Failed(said(&other.to_string())),
    }
}

#[async_trait]
impl Remote for HubRemote {
    async fn tools(&self) -> Result<Vec<String>, RemoteError> {
        self.ready().await?;
        let survey = tokio::time::timeout(CALL_TIMEOUT, self.client.list_tool_names())
            .await
            .map_err(|_| RemoteError::Offline("the hub didn't answer in time".into()))?
            .map_err(mcp_error)?;
        Ok(survey.names.into_iter().map(|t| t.name).collect())
    }

    async fn call(&self, tool: &str, args: Value) -> Result<Value, RemoteError> {
        self.ready().await?;
        let wire = tool.replace('.', "_");
        let result = tokio::time::timeout(
            CALL_TIMEOUT,
            self.client
                .tools_call(&wire, args, CancellationToken::new()),
        )
        .await
        .map_err(|_| RemoteError::Offline("the hub didn't answer in time".into()))?
        .map_err(|e| match e {
            // An unknown tool (JSON-RPC "method not found"): not offered.
            McpError::ServerError { code: -32601, .. } => RemoteError::Missing(tool.to_string()),
            e => mcp_error(e),
        })?;
        sync::mcp_answer(&result)
    }
}

/// The agents `token`'s person may reach on the hub at `url`
/// (`GET /mcp/agents`), default first; none from a hub without the route.
pub async fn hub_agents(
    url: &str,
    token: &Secret,
    http: &reqwest::Client,
) -> Result<Vec<String>, RemoteError> {
    let at = format!("{}/mcp/agents", url.trim_end_matches('/'));
    let resp = tokio::time::timeout(
        PROBE_TIMEOUT,
        http.get(&at).bearer_auth(token.expose()).send(),
    )
    .await
    .map_err(|_| RemoteError::Offline("the hub didn't answer in time".into()))?
    .map_err(|e| RemoteError::Offline(e.without_url().to_string()))?;
    match resp.status().as_u16() {
        401 => return Err(RemoteError::NoToken),
        404 | 405 => return Ok(vec![]),
        s if s >= 400 => return Ok(vec![]),
        _ => {}
    }
    let body: Value = resp.json().await.unwrap_or_default();
    let mut agents: Vec<(bool, String)> = body["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let id = a["id"].as_str().filter(|i| agent_id_ok(i))?;
            Some((a["default"].as_bool() != Some(true), id.to_string()))
        })
        .collect();
    agents.sort();
    Ok(agents.into_iter().map(|(_, id)| id).collect())
}

/// Where a board's tools were found: what reaches them, and the agent
/// there that offers them (none: the default, or this hub).
pub struct Reached {
    pub remote: Arc<dyn Remote>,
    pub agent: Option<String>,
}

// ── This hub's own tools ─────────────────────────────────────────────────

/// This hub's own `jira.*` / `linear.*` tools, with the person's keys from
/// its vault, run through the gate chain remote MCP runs, as the person.
pub struct LocalRemote {
    state: Arc<GatewayState>,
    person: String,
    system: System,
}

impl LocalRemote {
    pub fn new(state: Arc<GatewayState>, person: &str, system: System) -> Self {
        Self {
            state,
            person: person.to_string(),
            system,
        }
    }

    fn service(&self) -> Result<Arc<crate::serve::AgentService>, RemoteError> {
        self.state
            .registry
            .default_agent()
            .ok_or_else(|| RemoteError::Refused("this hub runs no agent".into()))
    }

    /// The registry's tools the sync may call for this system.
    fn surface(
        &self,
        service: &crate::serve::AgentService,
    ) -> workforce_agent::tools::registry::ToolRegistry {
        let patterns: Vec<String> = sync::callable(self.system)
            .iter()
            .map(|t| t.to_string())
            .collect();
        crate::mcp::build_mcp_registry(&service.tools, &patterns)
    }
}

#[async_trait]
impl Remote for LocalRemote {
    async fn tools(&self) -> Result<Vec<String>, RemoteError> {
        let service = self.service()?;
        Ok(self.surface(&service).names())
    }

    async fn call(&self, tool: &str, args: Value) -> Result<Value, RemoteError> {
        use crate::mcp::ToolCallExecutor as _;
        use crate::mcp_gate::{ApprovalMode, CallAudit, CallOrigin, CallSite, GatedExecutor};
        let service = self.service()?;
        let surface = Arc::new(self.surface(&service));
        let Some(t) = surface.get(tool) else {
            return Err(RemoteError::Missing(tool.to_string()));
        };
        let ctx = crate::remote_mcp::session_context::session_context(&service, &self.person)
            .await
            .map_err(|e| RemoteError::Failed(format!("no session to run in: {e}")))?;
        let executor = GatedExecutor {
            site: CallSite {
                gates: service.tool_gates(crate::desk_surface::allows()),
                policy: Arc::clone(&service.policy),
                surface: crate::mcp::Surface::Fixed(Arc::clone(&surface)),
                access: service.tool_access_snapshot(),
                spill: service.current_spill_config(),
                audit: CallAudit {
                    origin: CallOrigin::BoardSync,
                    identity_id: self.person.clone(),
                    agent_id: service.agent_id.to_string(),
                },
            },
            ctx,
            router: service.approval_router.read().clone(),
            mode: ApprovalMode::Park {
                pending: Arc::new(crate::mcp_gate::PendingCalls::default()),
                routed: self.state.registry.routed_delivery(),
                bridge: Arc::from(Vec::new()),
            },
        };
        let reply = tokio::time::timeout(CALL_TIMEOUT, executor.call(t, tool, args))
            .await
            .map_err(|_| RemoteError::Offline("the tool didn't answer in time".into()))?
            .map_err(|e| RemoteError::Failed(format!("{e:#}").chars().take(300).collect()))?;
        sync::mcp_answer(&serde_json::to_value(reply).unwrap_or_default())
    }
}

// ── Reaching a board's Jira or Linear ────────────────────────────────────

/// What a board syncs through, and what reaches it.
pub struct BoardSync {
    pub state: Arc<GatewayState>,
    pub desk: Desk,
    http: Arc<reqwest::Client>,
    /// Boards to sync at once (opened, or asked to), and what wakes the
    /// loop for them.
    kicked: std::sync::Mutex<std::collections::HashSet<Uuid>>,
    wake: tokio::sync::Notify,
}

impl BoardSync {
    pub fn new(state: Arc<GatewayState>, desk: Desk) -> Self {
        let http = reqwest::Client::builder()
            .timeout(CALL_TIMEOUT)
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        Self {
            state,
            desk,
            http: Arc::new(http),
            kicked: Default::default(),
            wake: tokio::sync::Notify::new(),
        }
    }

    /// Syncs board `id` at once, not on its next turn.
    pub fn kick(&self, id: Uuid) {
        self.kicked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id);
        self.wake.notify_one();
    }

    /// The loop: every board's turn every [`sync::PULL_EVERY`], a kicked
    /// board's at once, until `cancel`.
    pub fn start(self: &Arc<Self>, cancel: CancellationToken) {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = me.wake.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                }
                me.pass().await;
            }
        });
    }

    /// One pass over every synced board whose turn it is.
    pub async fn pass(&self) {
        let boards = match self.desk.synced_boards().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "synced boards weren't read");
                return;
            }
        };
        let kicked: std::collections::HashSet<Uuid> =
            std::mem::take(&mut *self.kicked.lock().unwrap_or_else(|e| e.into_inner()));
        for id in boards {
            if let Err(e) = self.pass_board(id, kicked.contains(&id)).await {
                tracing::warn!(board = %id, error = %e, "a board's sync pass failed");
            }
        }
    }

    /// Board `id`'s turn: nothing while it is paused by the person, out of
    /// its group or without its hub; else its pull, when due (or `now`).
    pub async fn pass_board(&self, id: Uuid, now: bool) -> Result<(), DeskError> {
        let Some((_, s)) = self.desk.board_sync(id).await? else {
            return Ok(());
        };
        if matches!(
            s.paused,
            Some(Paused::ByYou | Paused::LeftGroup | Paused::HubUnpaired)
        ) {
            return Ok(());
        }
        let due = s
            .pulled_at
            .is_none_or(|at| chrono::Utc::now() - at >= sync::PULL_EVERY);
        let queued = !self.desk.nothing_to_push(id).await?;
        if !(due || now || queued) {
            return Ok(());
        }
        // An agent that stopped offering the tools: the agents there are
        // asked again, and the board goes on through one that offers them.
        let remote = if matches!(s.paused, Some(Paused::Missing { .. })) {
            match self
                .offered(s.system, &s.via, &s.person, s.agent.as_deref())
                .await
            {
                Ok(r) => {
                    let agent = r.agent.clone();
                    self.desk
                        .change_sync(id, move |s| {
                            s.agent = agent;
                            s.paused = None;
                            s.backoff_until = None;
                            s.failures = 0;
                        })
                        .await?;
                    r.remote
                }
                Err(p) => {
                    if s.paused.as_ref() != Some(&p) {
                        self.desk.set_sync_paused(id, Some(p)).await?;
                    }
                    return Ok(());
                }
            }
        } else {
            match self
                .remote(s.system, &s.via, &s.person, s.agent.as_deref())
                .await
            {
                Ok(r) => r,
                Err(p) => {
                    if s.paused.as_ref() != Some(&p) {
                        self.desk.set_sync_paused(id, Some(p)).await?;
                    }
                    return Ok(());
                }
            }
        };
        // What changed here goes out first, so the pull that follows finds
        // its own changes there and takes nothing back.
        let pushed = self.desk.push_board(id, &*remote).await?;
        if pushed.created + pushed.updated + pushed.waiting > 0 {
            tracing::info!(board = %id, created = pushed.created, updated = pushed.updated,
                commented = pushed.commented, waiting = pushed.waiting, "board pushed");
        }
        if !(due || now) {
            return Ok(());
        }
        let pulled = self.desk.pull_board(id, &*remote).await?;
        if pulled.imported + pulled.updated + pulled.detached + pulled.dropped > 0 {
            tracing::info!(board = %id, imported = pulled.imported, updated = pulled.updated,
                detached = pulled.detached, dropped = pulled.dropped, "board pulled");
        }
        Ok(())
    }

    /// What reaches `system` `via` a hub (or this one), as `person`; why
    /// not, as a board's pause says it.
    pub async fn remote(
        &self,
        system: System,
        via: &Via,
        person: &str,
        agent: Option<&str>,
    ) -> Result<Arc<dyn Remote>, Paused> {
        match via {
            Via::ThisHub => Ok(Arc::new(LocalRemote::new(
                Arc::clone(&self.state),
                person,
                system,
            ))),
            Via::Hub { hub } => {
                let h = self
                    .desk
                    .sync_hub(hub)
                    .await
                    .ok()
                    .flatten()
                    .ok_or(Paused::HubUnpaired)?;
                let vault = self.state.vault.as_ref().ok_or(Paused::NoToken)?;
                let token = read_token(vault, person, hub).ok_or(Paused::NoToken)?;
                HubRemote::new(&h.url, &token, Arc::clone(&self.http), agent)
                    .map(|r| Arc::new(r) as Arc<dyn Remote>)
                    .map_err(|e| Paused::Refused {
                        reason: e.to_string(),
                    })
            }
        }
    }

    /// What reaches every tool `system`'s sync needs, and the agent that
    /// offers them: on a hub, `prefer` (the board's agent) first, then its
    /// default agent, then every other agent the person may reach there,
    /// since a hub offers each agent's own tools. None of them: `Missing`.
    pub async fn offered(
        &self,
        system: System,
        via: &Via,
        person: &str,
        prefer: Option<&str>,
    ) -> Result<Reached, Paused> {
        let probe = |agent: Option<String>| async move {
            let remote = self.remote(system, via, person, agent.as_deref()).await?;
            let tools = tokio::time::timeout(PROBE_TIMEOUT * 2, remote.tools())
                .await
                .map_err(|_| Paused::HubOffline {
                    since: chrono::Utc::now(),
                })?
                .map_err(|e| paused_by(&e))?;
            match sync::missing_tools(system, &tools) {
                missing if missing.is_empty() => Ok(Reached { remote, agent }),
                missing => Err(Paused::Missing { tools: missing }),
            }
        };
        let Via::Hub { hub } = via else {
            return probe(None).await;
        };
        // The board's own agent, then the default.
        let mut tried: Vec<Option<String>> = vec![];
        let mut missing = None;
        for agent in prefer
            .map(|a| Some(a.to_string()))
            .into_iter()
            .chain([None])
        {
            if tried.contains(&agent) {
                continue;
            }
            tried.push(agent.clone());
            match probe(agent).await {
                Ok(r) => return Ok(r),
                // A token refused or a hub away: no agent there will do.
                Err(p @ (Paused::NoToken | Paused::HubOffline { .. } | Paused::HubUnpaired)) => {
                    return Err(p)
                }
                Err(p) => missing = missing.or(Some(p)),
            }
        }
        // Every other agent the person may reach there, asked at once.
        let h = self
            .desk
            .sync_hub(hub)
            .await
            .ok()
            .flatten()
            .ok_or(Paused::HubUnpaired)?;
        let vault = self.state.vault.as_ref().ok_or(Paused::NoToken)?;
        let token = read_token(vault, person, hub).ok_or(Paused::NoToken)?;
        let agents = hub_agents(&h.url, &token, &self.http)
            .await
            .map_err(|e| paused_by(&e))?;
        let rest: Vec<String> = agents
            .into_iter()
            .filter(|a| !tried.contains(&Some(a.clone())))
            .collect();
        let found = futures::future::join_all(rest.into_iter().map(|a| probe(Some(a)))).await;
        if let Some(r) = found.into_iter().flatten().next() {
            return Ok(r);
        }
        Err(missing.unwrap_or(Paused::Missing {
            tools: sync::tools_of(system)
                .iter()
                .map(|t| t.to_string())
                .collect(),
        }))
    }

    /// The linked hub that offers `system`'s tools to `person`, if any: a
    /// board then syncs through it rather than with this computer's own
    /// keys. A hub that can't be reached now is passed over.
    pub async fn hub_offering(&self, system: System, person: &str) -> Option<String> {
        let hubs = self.desk.sync_hubs().await.ok()?;
        let probes = hubs.into_iter().map(|h| async move {
            let via = Via::Hub { hub: h.id.clone() };
            let offered =
                tokio::time::timeout(PROBE_TIMEOUT * 4, self.offered(system, &via, person, None))
                    .await;
            matches!(offered, Ok(Ok(_))).then_some(h.name)
        });
        futures::future::join_all(probes)
            .await
            .into_iter()
            .flatten()
            .next()
    }

    /// The name a board's hub goes by, for what the board says.
    pub async fn hub_name(&self, via: &Via) -> String {
        match via {
            Via::ThisHub => "This computer".to_string(),
            Via::Hub { hub } => self
                .desk
                .sync_hub(hub)
                .await
                .ok()
                .flatten()
                .map_or_else(|| hub.clone(), |h| h.name),
        }
    }
}

/// What a failed call pauses a board for; `None`-like errors (a tool that
/// failed on one issue) pause nothing, and are the caller's to handle.
pub fn paused_by(e: &RemoteError) -> Paused {
    match e {
        RemoteError::Offline(_) => Paused::HubOffline {
            since: chrono::Utc::now(),
        },
        RemoteError::NoToken => Paused::NoToken,
        RemoteError::Refused(reason) => Paused::Refused {
            reason: reason.clone(),
        },
        RemoteError::Missing(tool) => Paused::Missing {
            tools: vec![tool.clone()],
        },
        other => Paused::Refused {
            reason: other.to_string(),
        },
    }
}

/// What a hub's admin must do for a board to link through it, in plain
/// words.
fn missing_words(hub: &str, system: System, _tools: &[String]) -> String {
    format!(
        "No agent on {hub} you can reach has {} switched on.",
        system.title()
    )
}

fn paused_words(hub: &str, system: System, p: &Paused) -> String {
    match p {
        Paused::Missing { tools } => missing_words(hub, system, tools),
        other => other.say(hub),
    }
}

// ── Routes ───────────────────────────────────────────────────────────────

struct Routes {
    sync: Arc<BoardSync>,
}

type S = State<Arc<Routes>>;

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

fn refusal(status: StatusCode, error: &str) -> Response {
    (status, Json(json!({ "error": error }))).into_response()
}

/// The person asking: signed in as themselves, holding this hub.
async fn person(r: &Routes, headers: &HeaderMap, token: Option<&str>) -> Result<String, Response> {
    let state = &r.sync.state;
    let caller = resolve_caller_context(state, headers, token).await;
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
    let Some(person) = caller.identity_id else {
        return Err(refusal(
            StatusCode::FORBIDDEN,
            "A board's sync is a person's: ask as yourself, not as the hub",
        ));
    };
    if !crate::desk_floor::holds_the_hub(state.identity_store.as_ref(), true, Some(&person)) {
        return Err(refusal(
            StatusCode::FORBIDDEN,
            "Only the person whose Desk this is links its boards",
        ));
    }
    Ok(person)
}

macro_rules! person {
    ($r:expr, $h:expr, $q:expr) => {
        match person(&$r, &$h, $q.token.as_deref()).await {
            Ok(person) => person,
            Err(refused) => return refused,
        }
    };
}

/// The board sync's routes.
pub fn router(sync: Arc<BoardSync>) -> Router {
    Router::new()
        .route("/api/desk/sync-hubs", get(list_hubs))
        .route("/api/desk/sync-hubs/{hub}", put(put_hub).delete(delete_hub))
        .route("/api/desk/boards/link/check", post(check_link))
        .route(
            "/api/desk/boards/{id}/link",
            put(link_board).delete(unlink_board),
        )
        .route("/api/desk/boards/{id}/pause", post(pause_board))
        .route("/api/desk/boards/{id}/sync", post(sync_board))
        .route("/api/desk/tickets/{id}/take", post(take_ticket))
        .route("/api/desk/boards/{id}/me", post(recheck_me))
        .with_state(Arc::new(Routes { sync }))
}

async fn list_hubs(State(r): S, h: HeaderMap, Query(q): Query<TokenQuery>) -> Response {
    let person = person!(r, h, q);
    let hubs = match r.sync.desk.sync_hubs().await {
        Ok(h) => h,
        Err(e) => return e.into_response(),
    };
    let vault = r.sync.state.vault.as_ref();
    let hubs: Vec<Value> = hubs
        .into_iter()
        .map(|hub| {
            let has = vault.is_some_and(|v| read_token(v, &person, &hub.id).is_some());
            json!({ "id": hub.id, "name": hub.name, "url": hub.url, "hasToken": has })
        })
        .collect();
    Json(json!({ "hubs": hubs })).into_response()
}

#[derive(Deserialize)]
struct HubBody {
    name: String,
    url: String,
    /// The person's `wfr_` token for that hub, given once and never read
    /// back. Without one, a hub kept before keeps its token.
    #[serde(default)]
    token: Option<String>,
}

async fn put_hub(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(hub): Path<String>,
    Json(body): Json<HubBody>,
) -> Response {
    let person = person!(r, h, q);
    if let Err(e) = workforce_desk::check_hub_id(&hub) {
        return e.into_response();
    }
    let url = match workforce_desk::check_hub_url(&body.url) {
        Ok(u) => u,
        Err(e) => return e.into_response(),
    };
    let Some(vault) = r.sync.state.vault.clone() else {
        return refusal(
            StatusCode::SERVICE_UNAVAILABLE,
            "This hub keeps no vault for the token",
        );
    };
    let token = match body.token.as_deref() {
        Some(t) => match Secret::new(t) {
            Some(t) => Some(t),
            None => {
                return refusal(
                    StatusCode::BAD_REQUEST,
                    "That isn't a hub's token: it starts wfr_.",
                )
            }
        },
        None => None,
    };
    // A token goes only to the address it was given for: a hub that moves
    // needs a token checked at its new address.
    let before = match r.sync.desk.sync_hub(&hub).await {
        Ok(h) => h,
        Err(e) => return e.into_response(),
    };
    let moved = before
        .as_ref()
        .is_some_and(|h| h.url != url.as_str().trim_end_matches('/'));
    if moved && token.is_none() {
        return refusal(
            StatusCode::BAD_REQUEST,
            &format!(
                "{} has a new address: paste a token made there.",
                body.name.trim()
            ),
        );
    }
    // A new token is tried before it is kept: one the hub refuses is never
    // kept.
    let offers = match &token {
        Some(t) => {
            let remote = match HubRemote::new(url.as_str(), t, Arc::clone(&r.sync.http), None) {
                Ok(remote) => remote,
                Err(e) => return refusal(StatusCode::BAD_REQUEST, &e.to_string()),
            };
            match remote.tools().await {
                Ok(tools) => Some(tools),
                Err(RemoteError::NoToken) => {
                    return refusal(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        &format!(
                            "{} refused that token; make a new one there.",
                            body.name.trim()
                        ),
                    )
                }
                Err(e) => {
                    return refusal(
                        StatusCode::BAD_GATEWAY,
                        &format!("{} couldn't be reached with it: {e}", body.name.trim()),
                    )
                }
            }
        }
        None => None,
    };
    let kept = match r
        .sync
        .desk
        .put_sync_hub(&hub, &body.name, url.as_str(), &person)
        .await
    {
        Ok(k) => k,
        Err(e) => return e.into_response(),
    };
    if let Some(t) = &token {
        let key = token_key(&person, &hub);
        if let Err(e) = vault.set_with_actor(&key, t.expose(), VaultActor::User(&person)) {
            tracing::error!(error = %format!("{e:#}"), hub = %hub, "a hub's token wasn't kept");
            return refusal(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Desk couldn't keep the token. Try again.",
            );
        }
        tracing::info!(target: "sentinel.audit", hub = %hub, person = %person, "a hub's token for board sync was kept");
    }
    // What it offers through any agent the person may reach there.
    let offers = match offers {
        Some(_) => {
            let via = Via::Hub { hub: hub.clone() };
            let (jira, linear) = futures::join!(
                r.sync.offered(System::Jira, &via, &person, None),
                r.sync.offered(System::Linear, &via, &person, None)
            );
            Some(json!({ "jira": jira.is_ok(), "linear": linear.is_ok() }))
        }
        None => None,
    };
    Json(json!({ "id": kept.id, "name": kept.name, "url": kept.url, "offers": offers }))
        .into_response()
}

async fn delete_hub(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(hub): Path<String>,
) -> Response {
    let person = person!(r, h, q);
    if let Err(e) = workforce_desk::check_hub_id(&hub) {
        return e.into_response();
    }
    if let Some(vault) = r.sync.state.vault.as_ref() {
        if let Err(e) =
            vault.delete_with_actor(&token_key(&person, &hub), VaultActor::User(&person))
        {
            tracing::error!(error = %format!("{e:#}"), hub = %hub, "a hub's token wasn't removed");
            return refusal(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Desk couldn't remove the token. Try again.",
            );
        }
    }
    match r.sync.desk.forget_sync_hub(&hub).await {
        Ok(()) | Err(DeskError::NotFound) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => e.into_response(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckBody {
    system: System,
    via: Via,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    target_id: Option<String>,
    #[serde(default)]
    filter: Option<Filter>,
    #[serde(default)]
    me: Option<String>,
}

/// How many of `target`'s issues `filter` matches, read page by page up to
/// [`sync::MATCH_CEILING`]; and whether there may be more than that.
async fn count_matches(
    remote: &dyn Remote,
    system: System,
    target: &str,
    target_id: Option<&str>,
    filter: &Filter,
    me: Option<&str>,
) -> Result<(usize, bool), RemoteError> {
    let mut n = 0usize;
    let pages = sync::MATCH_CEILING.div_ceil(sync::PAGE);
    let mut page: Option<String> = None;
    for _ in 0..pages {
        let (found, next) = match system {
            System::Jira => {
                let mut args = json!({ "jql": sync::jql(target, filter, None), "max_results": sync::PAGE, "fields": ["summary"] });
                if let Some(p) = &page {
                    args["next_page_token"] = json!(p);
                }
                let a = sync::call(remote, system, "jira.search", args).await?;
                let (issues, next) = sync::jira_search_issues(&a, "");
                let found = issues
                    .into_iter()
                    .filter(|i| sync::key_in_target(&i.key, target))
                    .count();
                (found, next)
            }
            System::Linear => {
                let team = target_id.unwrap_or_default();
                let a = sync::call(
                    remote,
                    system,
                    "linear.issue_list",
                    sync::linear_args(team, filter, me, None, page.as_deref()),
                )
                .await?;
                let issues = sync::linear_issues(&a);
                let paged = sync::linear_pages(&a);
                // A hub whose tool doesn't page answers one page: past a
                // full one there may be more.
                let more = match paged {
                    true => sync::linear_next_page(&a),
                    false if issues.len() >= sync::PAGE => Some(String::new()),
                    false => None,
                };
                let found = issues
                    .into_iter()
                    .filter(|i| sync::key_in_target(&i.key, target))
                    .filter(|i| match filter {
                        Filter::Label(l) => i.labels.iter().any(|x| x.eq_ignore_ascii_case(l)),
                        // What a hub that can't filter by state answered.
                        Filter::All { .. } if !paged => !i.is_closed(&Default::default()),
                        _ => true,
                    })
                    .count();
                if !paged {
                    return Ok((found, more.is_some()));
                }
                (found, more)
            }
        };
        n += found;
        match next {
            Some(p) => page = Some(p),
            None => return Ok((n, false)),
        }
    }
    Ok((n, true))
}

async fn check_link(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Json(body): Json<CheckBody>,
) -> Response {
    let person = person!(r, h, q);
    if let Some(target) = body.target.as_deref() {
        if let Err(e) = sync::check_target(body.system, target) {
            return refusal(StatusCode::BAD_REQUEST, &e);
        }
    }
    // A filter is checked before any call; Linear's "assigned to me" once
    // the hub has said who the person is there.
    if let Some(filter) = &body.filter {
        let asks_me =
            body.system == System::Linear && *filter == Filter::AssignedToMe && body.me.is_none();
        if !asks_me {
            if let Err(e) = sync::check_filter(body.system, filter, body.me.as_deref()) {
                return refusal(StatusCode::BAD_REQUEST, &e);
            }
        }
    }
    if let Some(me) = body.me.as_deref() {
        if let Err(e) = sync::check_me(body.system, me) {
            return refusal(StatusCode::BAD_REQUEST, &e);
        }
    }
    if let Some(id) = body.target_id.as_deref() {
        if let Err(e) = sync::check_linear_id(id) {
            return refusal(StatusCode::BAD_REQUEST, &e);
        }
    }
    let hub = r.sync.hub_name(&body.via).await;
    let reached = match r.sync.offered(body.system, &body.via, &person, None).await {
        Ok(reached) => reached,
        Err(p) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": paused_words(&hub, body.system, &p), "paused": p })),
            )
                .into_response()
        }
    };
    let remote = reached.remote;
    // Which agent there offers the tools: "Jira through Lattice".
    let mut out = json!({
        "offered": true,
        "agent": reached.agent.as_deref().map(|a| json!({ "id": a, "name": workforce_desk::board::agent_title(a) })),
    });
    let said = |e: RemoteError| {
        (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("{hub}: {e}") })),
        )
            .into_response()
    };
    if body.system == System::Linear {
        match sync::call(&*remote, body.system, "linear.teams_list", json!({})).await {
            Ok(teams) => out["teams"] = teams_of(&teams),
            Err(e) => return said(e),
        }
    }
    let Some(target) = body.target.as_deref() else {
        return Json(out).into_response();
    };
    let states = match remote_states(&*remote, body.system, target, body.target_id.as_deref()).await
    {
        Ok(s) => s,
        Err(e) => return said(e),
    };
    out["states"] = json!(states.iter().map(|s| &s.name).collect::<Vec<_>>());
    out["prefill"] = json!(sync::prefill_states(&states));
    // Who the person is there, when the hub can say: "assigned to me" then
    // needs no id typed in.
    // Only the person's own key vouches for them: a shared one says who
    // owns it, so then no one is named, and the dialog says why.
    let me = match body.me.as_deref() {
        Some(id) => Some(workforce_desk::board::Assignee {
            id: id.to_string(),
            name: "You".into(),
        }),
        None => match sync::identify(&*remote, body.system).await {
            Ok(sync::Who::Person(me)) => Some(me),
            Ok(sync::Who::ByEmail(me)) => {
                out["meBy"] = json!("email");
                Some(me)
            }
            Ok(other) => {
                out["meWhy"] = json!(other.why_not(&hub, body.system));
                None
            }
            Err(e) => return said(e),
        },
    };
    out["me"] = json!(me);
    if let Some(filter) = &body.filter {
        // A filter that can't be counted yet (Linear's "assigned to me"
        // on a hub that can't say who the person is) says why, beside what
        // was found, so the dialog can ask for the missing id.
        if let Err(e) = sync::check_filter(body.system, filter, me.as_ref().map(|m| m.id.as_str()))
        {
            out["filterError"] = json!(e);
            return Json(out).into_response();
        }
        let matched = count_matches(
            &*remote,
            body.system,
            target,
            body.target_id.as_deref(),
            filter,
            me.as_ref().map(|m| m.id.as_str()),
        )
        .await;
        match matched {
            Ok((n, more)) => {
                out["matches"] = json!(n);
                out["matchesAtLeast"] = json!(more);
                out["ceiling"] = json!(sync::MATCH_CEILING);
            }
            Err(e) => return said(e),
        }
    }
    Json(out).into_response()
}

/// A team list as the dialog shows it: id, key and name.
fn teams_of(answer: &Value) -> Value {
    let teams = answer
        .get("teams")
        .or_else(|| answer.get("nodes"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Value::Array(
        teams
            .into_iter()
            .map(|t| {
                json!({
                    "id": t["id"],
                    "key": t["key"],
                    "name": sync::unfence(t["name"].as_str().unwrap_or_default()).0,
                })
            })
            .collect(),
    )
}

/// The states an issue in `target` can be in: Linear's team states with
/// their types; Jira's from the transitions of one of the project's
/// issues, by name.
pub async fn remote_states(
    remote: &dyn Remote,
    system: System,
    target: &str,
    target_id: Option<&str>,
) -> Result<Vec<RemoteState>, RemoteError> {
    match system {
        System::Linear => {
            let answer = sync::call(
                remote,
                system,
                "linear.states_list",
                json!({ "team_id": target_id.unwrap_or_default() }),
            )
            .await?;
            let list = answer
                .get("states")
                .or_else(|| answer.get("nodes"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            Ok(list
                .iter()
                .filter_map(|s| {
                    Some(RemoteState {
                        name: s["name"].as_str()?.to_string(),
                        kind: s["type"].as_str().map(str::to_string),
                    })
                })
                .collect())
        }
        System::Jira => {
            let page = sync::call(
                remote,
                system,
                "jira.search",
                json!({ "jql": format!("project = \"{target}\" ORDER BY updated DESC"), "max_results": 20, "fields": ["status"] }),
            )
            .await?;
            let mut names: Vec<String> = vec![];
            for i in page["issues"].as_array().into_iter().flatten() {
                if let Some(s) = i["status"].as_str() {
                    names.push(s.to_string());
                }
            }
            if let Some(key) = page["issues"][0]["key"].as_str() {
                let t = sync::call(
                    remote,
                    system,
                    "jira.list_transitions",
                    json!({ "issue_key": key }),
                )
                .await?;
                for tr in t["transitions"].as_array().into_iter().flatten() {
                    if let Some(s) = tr["to_status"].as_str() {
                        names.push(s.to_string());
                    }
                }
            }
            let mut seen = std::collections::HashSet::new();
            names.retain(|n| seen.insert(n.to_ascii_lowercase()));
            Ok(names
                .into_iter()
                .map(|name| RemoteState { name, kind: None })
                .collect())
        }
    }
}

async fn link_board(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(id): Path<Uuid>,
    Json(mut link): Json<LinkBoard>,
) -> Response {
    let person = person!(r, h, q);
    if link.via == Via::ThisHub {
        if let Some(name) = r.sync.hub_offering(link.system, &person).await {
            return refusal(
                StatusCode::CONFLICT,
                &format!(
                    "{name} has {}: link through it, so this computer keeps no {} keys of its own.",
                    link.system.title(),
                    link.system.title()
                ),
            );
        }
    }
    let hub = r.sync.hub_name(&link.via).await;
    let remote = match r.sync.offered(link.system, &link.via, &person, None).await {
        Ok(reached) => {
            link.agent = reached.agent;
            reached.remote
        }
        Err(p) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": paused_words(&hub, link.system, &p), "paused": p })),
            )
                .into_response();
        }
    };
    // Who the person is there, kept on the link for "assigned to me",
    // Mine and Take it; an id they typed is theirs to keep.
    match &link.me {
        _ if link.not_me && link.me.is_none() => {
            link.me_note = Some(format!(
                "You said {hub}'s answer of who you are on {} isn't you: check again once it's right.",
                link.system.title()
            ));
        }
        Some(me) => {
            if let Err(e) = sync::check_me(link.system, me) {
                return refusal(StatusCode::BAD_REQUEST, &e);
            }
        }
        None => match sync::identify(&*remote, link.system).await {
            Ok(sync::Who::Person(me)) => {
                link.me = Some(me.id);
                link.me_name = Some(me.name);
                link.me_source = Some(workforce_desk::board::MeSource::Vouched);
            }
            Ok(sync::Who::ByEmail(me)) => {
                link.me = Some(me.id);
                link.me_name = Some(me.name);
                link.me_source = Some(workforce_desk::board::MeSource::VouchedByEmail);
            }
            Ok(other) => link.me_note = other.why_not(&hub, link.system),
            Err(e) => return refusal(StatusCode::BAD_GATEWAY, &format!("{hub}: {e}")),
        },
    }
    // A Linear team is named by its key and its id: the two must be one team
    // on that hub, so a link never syncs a team other than the one named.
    if link.system == System::Linear {
        let teams = match sync::call(&*remote, link.system, "linear.teams_list", json!({})).await {
            Ok(t) => teams_of(&t),
            Err(e) => return refusal(StatusCode::BAD_GATEWAY, &format!("{hub}: {e}")),
        };
        let named = teams.as_array().into_iter().flatten().any(|t| {
            t["id"].as_str() == link.target_id.as_deref()
                && t["key"].as_str() == Some(link.target.as_str())
        });
        if !named {
            return refusal(
                StatusCode::BAD_REQUEST,
                &format!("{hub} has no Linear team {} with that id.", link.target),
            );
        }
    }
    match r.sync.desk.link_board(id, link, &person).await {
        Ok(b) => Json(b).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn unlink_board(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(id): Path<Uuid>,
) -> Response {
    let _person = person!(r, h, q);
    match r.sync.desk.unlink_board(id).await {
        Ok(b) => Json(b).into_response(),
        Err(e) => e.into_response(),
    }
}

#[derive(Deserialize)]
struct PauseBody {
    paused: bool,
}

async fn pause_board(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(id): Path<Uuid>,
    Json(body): Json<PauseBody>,
) -> Response {
    let _person = person!(r, h, q);
    match r.sync.desk.pause_board(id, body.paused).await {
        Ok(b) => Json(b).into_response(),
        Err(e) => e.into_response(),
    }
}

/// Syncs a board at once: Desk asks when the board is opened.
async fn sync_board(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(id): Path<Uuid>,
) -> Response {
    let _person = person!(r, h, q);
    match r.sync.desk.board_sync(id).await {
        Ok(Some(_)) => {
            r.sync.kick(id);
            StatusCode::ACCEPTED.into_response()
        }
        Ok(None) => refusal(StatusCode::CONFLICT, "That board isn't synced."),
        Err(e) => e.into_response(),
    }
}

/// Take it: the ticket's issue is assigned to the person there, as
/// themselves, through the route its board syncs by.
async fn take_ticket(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(id): Path<Uuid>,
) -> Response {
    let person = person!(r, h, q);
    let desk = &r.sync.desk;
    let board = match desk.ticket(id).await {
        Ok(t) => t.ticket.board,
        Err(e) => return e.into_response(),
    };
    let s = match desk.board_sync(board).await {
        Ok(Some((_, s))) => s,
        Ok(None) => return refusal(StatusCode::CONFLICT, "That ticket's board isn't synced."),
        Err(e) => return e.into_response(),
    };
    // A board syncs as the person who linked it; Take it assigns to them.
    if s.person != person {
        return refusal(
            StatusCode::FORBIDDEN,
            "Only the person who linked this board takes its issues.",
        );
    }
    let remote = match r
        .sync
        .remote(s.system, &s.via, &person, s.agent.as_deref())
        .await
    {
        Ok(remote) => remote,
        Err(p) => {
            let hub = r.sync.hub_name(&s.via).await;
            return refusal(StatusCode::CONFLICT, &paused_words(&hub, s.system, &p));
        }
    };
    let hub = r.sync.hub_name(&s.via).await;
    match desk
        .take_ticket(id, &*remote, &hub, &workforce_desk::crew::By::you())
        .await
    {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

/// Asks the hub again who the person is on a board's Jira or Linear (the
/// board's "that's not me" or "check again"), and keeps its answer.
async fn recheck_me(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(id): Path<Uuid>,
) -> Response {
    let person = person!(r, h, q);
    let desk = &r.sync.desk;
    let s = match desk.board_sync(id).await {
        Ok(Some((_, s))) => s,
        Ok(None) => return refusal(StatusCode::CONFLICT, "That board isn't synced."),
        Err(e) => return e.into_response(),
    };
    if s.person != person {
        return refusal(
            StatusCode::FORBIDDEN,
            "Only the person who linked this board checks who they are on it.",
        );
    }
    let hub = r.sync.hub_name(&s.via).await;
    let remote = match r
        .sync
        .remote(s.system, &s.via, &person, s.agent.as_deref())
        .await
    {
        Ok(remote) => remote,
        Err(p) => return refusal(StatusCode::CONFLICT, &paused_words(&hub, s.system, &p)),
    };
    if let Err(e) = desk.recheck_me(id, &*remote, &hub).await {
        return refusal(StatusCode::BAD_GATEWAY, &format!("{hub}: {e}"));
    }
    match desk.board_sync(id).await {
        Ok(Some((b, _))) => Json(b).into_response(),
        Ok(None) => refusal(StatusCode::CONFLICT, "That board isn't synced."),
        Err(e) => e.into_response(),
    }
}

/// The board sync for this `[desk]` hub, and its routes.
pub fn install(state: &Arc<GatewayState>, desk: Desk) -> (Arc<BoardSync>, Router) {
    let sync = Arc::new(BoardSync::new(Arc::clone(state), desk));
    let routes = router(Arc::clone(&sync));
    (sync, routes)
}

#[cfg(test)]
#[path = "desk_board_sync_tests.rs"]
mod tests;
