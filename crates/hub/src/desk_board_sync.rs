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
//!   by asking the hub which of its agents the person may reach (on an older
//!   hub, its default agent's `tools/list`), then keeps it (write-only).
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

use crate::surface::routing::{get, post, put};
use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_common::settings::VaultActor;
use workforce_desk::board::{Filter, Paused, System, Via};
use workforce_desk::board_sync::{self as sync, Remote, RemoteError, RemoteState};
use workforce_desk::github_sync as gh;
use workforce_desk::{Desk, DeskError, LinkBoard};
use workforce_mcp_client::{McpClient, McpError, McpLimits, StaticBearer};

use crate::gateway::{resolve_caller_context, AuthFailure, CallerContext, GatewayState};
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
    /// It reaches the hub's default agent (`/mcp`), not a named one.
    plain: bool,
}

impl HubRemote {
    pub fn new(
        url: &str,
        token: &Secret,
        http: Arc<reqwest::Client>,
        agent: Option<&str>,
    ) -> Result<Self, RemoteError> {
        let base = url.trim_end_matches('/');
        // Marked as the board sync, a program: the hub offers it the file
        // readers it keeps off a model's client.
        let mark = workforce_core::desk::BOARD_SYNC_CALLER;
        let path = match agent {
            Some(a) if agent_id_ok(a) => format!("{base}/mcp/agents/{a}?{mark}"),
            Some(_) => return Err(RemoteError::Refused("that isn't an agent's id".into())),
            None => format!("{base}/mcp?{mark}"),
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
            plain: agent.is_none(),
        })
    }

    async fn ready(&self) -> Result<(), McpError> {
        self.ready
            .get_or_try_init(|| async { self.client.initialize().await.map(drop) })
            .await
            .map(|_| ())
    }

    /// Whether `e` says the agent this reaches doesn't let the person in:
    /// the default agent's 403, or a named agent's 404 (one that is gone or
    /// no longer admits them reads as not there). The token is fine; that
    /// agent just offers them nothing.
    fn not_admitted(&self, e: &McpError) -> bool {
        match e {
            McpError::Forbidden { said, .. } => {
                self.plain && said.contains(crate::remote_mcp::server::DEFAULT_NOT_ADMITTED)
            }
            McpError::Protocol(m) => !self.plain && m.contains("404"),
            _ => false,
        }
    }
}

/// Why no agent on a hub serves the person, as a pause keeps it.
pub const NO_AGENT_LETS_YOU_IN: &str =
    "no agent there lets you in; ask whoever runs it to let you use one";

/// The same, said of a hub by name.
fn no_agent_words(hub: &str) -> String {
    format!("{hub} has no agent that lets you in; ask whoever runs it to let you use one.")
}

/// A hub's 403, in words for the person: the hub's own words are matched
/// (both ends are this code); anything else is passed on, clipped. Never
/// "make a new token": the token was good, the person or its use wasn't.
fn forbidden(said: &str) -> RemoteError {
    use crate::remote_mcp::{auth, server};
    let reason = if said.contains(auth::NOT_ACTIVE) {
        "your account there isn't active; ask whoever runs it to turn it back on".to_string()
    } else if said.contains(auth::OUT_OF_SCOPE) {
        "that token isn't one for Desk; make a new one there".to_string()
    } else if said.contains(server::DEFAULT_NOT_ADMITTED) {
        NO_AGENT_LETS_YOU_IN.to_string()
    } else if said.trim().is_empty() {
        "it turned the token away".to_string()
    } else {
        said.trim().chars().take(300).collect()
    };
    RemoteError::Refused(reason)
}

/// What a hub's answer means for the sync. Its words are already redacted
/// by the client; they are clipped here too.
fn mcp_error(e: McpError) -> RemoteError {
    let said = |s: &str| s.chars().take(300).collect::<String>();
    match e {
        McpError::Unreachable(m) => RemoteError::Offline(said(&m)),
        // 401: a token the hub doesn't know (revoked, expired).
        McpError::AuthExpired(_) => RemoteError::NoToken,
        // 403: a token it knows, refused for why it says.
        McpError::Forbidden { said, .. } => forbidden(&said),
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
        let listed = match self.ready().await {
            Ok(()) => tokio::time::timeout(CALL_TIMEOUT, self.client.list_tool_names())
                .await
                .map_err(|_| RemoteError::Offline("the hub didn't answer in time".into()))?,
            Err(e) => Err(e),
        };
        match listed {
            Ok(survey) => Ok(survey.names.into_iter().map(|t| t.name).collect()),
            // An agent that doesn't let the person in offers them nothing:
            // the others they may reach are asked.
            Err(e) if self.not_admitted(&e) => Ok(vec![]),
            Err(e) => Err(mcp_error(e)),
        }
    }

    async fn call(&self, tool: &str, args: Value) -> Result<Value, RemoteError> {
        let wire = tool.replace('.', "_");
        let result = match self.ready().await {
            Ok(()) => tokio::time::timeout(
                CALL_TIMEOUT,
                self.client
                    .tools_call(&wire, args, CancellationToken::new()),
            )
            .await
            .map_err(|_| RemoteError::Offline("the hub didn't answer in time".into()))?,
            Err(e) => Err(e),
        };
        let result = result.map_err(|e| match e {
            // An unknown tool (JSON-RPC "method not found"): not offered.
            McpError::ServerError { code: -32601, .. } => RemoteError::Missing(tool.to_string()),
            // An agent that stopped letting the person in: its tools read as
            // gone, so the board's next pass asks the agents again.
            e if self.not_admitted(&e) => RemoteError::Missing(tool.to_string()),
            e => mcp_error(e),
        })?;
        sync::mcp_answer(&result)
    }
}

/// The agents `token`'s person may reach on the hub at `url`
/// (`GET /mcp/agents`), default first; `None` from an older hub without the
/// route. An empty list: the token is good, but no agent there lets its
/// person in.
pub async fn hub_agents(
    url: &str,
    token: &Secret,
    http: &reqwest::Client,
) -> Result<Option<Vec<String>>, RemoteError> {
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
        // Not allowed, or asked too often: said as such, never as "no
        // agent has it", and never as a token to make again.
        403 => {
            let said = read_bounded(resp).await.unwrap_or_default();
            return Err(forbidden(&String::from_utf8_lossy(&said)));
        }
        429 => {
            return Err(RemoteError::RateLimited {
                retry_after_secs: resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse().ok()),
            })
        }
        // An older hub without the route: only its default agent is asked.
        404 | 405 => return Ok(None),
        s if s >= 500 => return Err(RemoteError::Offline(format!("the hub answered {s}"))),
        s if s >= 400 => return Err(RemoteError::Failed(format!("the hub answered {s}"))),
        _ => {}
    }
    let body = read_bounded(resp).await?;
    let body: Value = serde_json::from_slice(&body).unwrap_or_default();
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
    agents.dedup();
    Ok(Some(
        agents
            .into_iter()
            .take(MAX_AGENTS)
            .map(|(_, id)| id)
            .collect(),
    ))
}

/// A bounded read: an answer this long isn't a list of agents.
async fn read_bounded(mut resp: reqwest::Response) -> Result<Vec<u8>, RemoteError> {
    let mut body = Vec::new();
    while let Some(chunk) = tokio::time::timeout(PROBE_TIMEOUT, resp.chunk())
        .await
        .map_err(|_| RemoteError::Offline("the hub didn't answer in time".into()))?
        .map_err(|e| RemoteError::Offline(e.without_url().to_string()))?
    {
        body.extend_from_slice(&chunk);
        if body.len() > AGENTS_BODY_MAX {
            return Err(RemoteError::Failed(
                "the hub's list of agents is too long".into(),
            ));
        }
    }
    Ok(body)
}

/// The most agents on a hub a board's probe asks.
pub const MAX_AGENTS: usize = 16;
/// The longest list of agents a hub may answer with.
const AGENTS_BODY_MAX: usize = 64 * 1024;

/// Where a board's tools were found: what reaches them, and the agent
/// there that offers them (none: the default, or this hub).
pub struct Reached {
    pub remote: Arc<dyn Remote>,
    pub agent: Option<String>,
}

/// What a link asks of a person's connections there, for
/// [`BoardSync::github_connection`].
struct Wanted<'a> {
    system: System,
    names: &'a [String],
    default: Option<&'a str>,
    asked: Option<&'a str>,
    target: Option<&'a str>,
    owner: Option<&'a str>,
}

/// Why a link can't go through a GitHub connection yet: the ones to pick
/// from, and why, as the dialog says it.
struct PickConnection {
    names: Vec<String>,
    why: String,
}

// ── This hub's own tools ─────────────────────────────────────────────────

/// One of this hub's agents' own `jira.*` / `linear.*` / `github.*` tools
/// (`agent`, else the default's), with the person's keys from its vault,
/// run through the gate chain remote MCP runs, as the person. An agent that
/// is gone or doesn't let the person in offers them nothing, as a linked
/// hub's does.
pub struct LocalRemote {
    state: Arc<GatewayState>,
    person: String,
    system: System,
    agent: Option<String>,
    adapters: Arc<crate::remote_mcp::server::AdapterChecks>,
}

impl LocalRemote {
    pub(crate) fn new(
        state: Arc<GatewayState>,
        person: &str,
        system: System,
        agent: Option<&str>,
        adapters: Arc<crate::remote_mcp::server::AdapterChecks>,
    ) -> Self {
        Self {
            state,
            person: person.to_string(),
            system,
            agent: agent.map(str::to_string),
            adapters,
        }
    }

    fn service(&self) -> Option<Arc<crate::serve::AgentService>> {
        crate::remote_mcp::access::reachable(&self.state, &self.person, self.agent.as_deref()).ok()
    }

    /// The registry's tools the sync may call for this system, its adapter
    /// loaded first with the person's keys, as a turn of theirs loads it: a
    /// key they keep for themselves otherwise reaches the registry only once
    /// some turn of theirs has used it. An adapter that isn't loaded is
    /// looked at once in [`crate::remote_mcp::server::ADAPTERS_RECHECK`] per
    /// agent, person and system, as remote MCP looks at a person's.
    fn surface(
        &self,
        service: &crate::serve::AgentService,
    ) -> workforce_agent::tools::registry::ToolRegistry {
        let skill = self.system.as_str();
        let due = || {
            self.adapters.due(
                (
                    service.agent_id.to_string(),
                    format!("{}/{skill}", self.person),
                ),
                (),
            )
        };
        if !crate::skill_loader::skill_tools_registered(skill, &service.tools) && due() {
            workforce_agent::runner::CURRENT_USER_ID.sync_scope(self.person.clone(), || {
                crate::skill_loader::ensure_adapters_loaded(service, |s| s == skill)
            });
        }
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
        Ok(self
            .service()
            .map(|service| self.surface(&service).names())
            .unwrap_or_default())
    }

    async fn call(&self, tool: &str, args: Value) -> Result<Value, RemoteError> {
        self.run(tool, args, false).await
    }

    async fn call_quiet(&self, tool: &str, args: Value) -> Result<Value, RemoteError> {
        self.run(tool, args, true).await
    }
}

impl LocalRemote {
    /// Runs `tool`; `quiet`: with no approver to route to and nowhere to
    /// hand a routed approval, so a call that needs one asks no one.
    async fn run(&self, tool: &str, args: Value, quiet: bool) -> Result<Value, RemoteError> {
        use crate::mcp::ToolCallExecutor as _;
        use crate::mcp_gate::{ApprovalMode, CallAudit, CallOrigin, CallSite, GatedExecutor};
        let service = self
            .service()
            .ok_or_else(|| RemoteError::Missing(tool.to_string()))?;
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
                program: true,
            },
            ctx,
            router: if quiet {
                None
            } else {
                service.approval_router.read().clone()
            },
            mode: ApprovalMode::Park {
                pending: Arc::new(crate::mcp_gate::PendingCalls::default()),
                routed: if quiet {
                    tokio::sync::mpsc::channel(1).0
                } else {
                    self.state.registry.routed_delivery()
                },
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
    /// When this computer's adapters were last looked at ([`LocalRemote`]).
    adapters: Arc<crate::remote_mcp::server::AdapterChecks>,
    /// Whether a GitHub connection reaches what a link names, as last
    /// asked ([`Self::reaches`]).
    reached: crate::recent::Recent<String, bool>,
}

/// The most agents, or GitHub connections, asked at once.
const PROBES_AT_ONCE: usize = 4;
/// How long whether a connection reaches a link's target is kept.
const REACH_RECHECK: Duration = Duration::from_secs(60);

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
            adapters: Arc::new(crate::remote_mcp::server::adapter_checks()),
            reached: crate::recent::Recent::new(REACH_RECHECK),
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
        // An agent that stopped offering the tools, or no agent there
        // letting the person in: the agents there are asked again, and the
        // board goes on through one that offers them. Any other refusal (a
        // policy's no) waits out its back-off as before.
        let reprobe = match &s.paused {
            Some(Paused::Missing { .. }) => true,
            Some(Paused::Refused { reason }) => reason == NO_AGENT_LETS_YOU_IN,
            _ => false,
        };
        let remote = if reprobe {
            match self
                .offered(
                    s.system,
                    &s.via,
                    &s.person,
                    s.agent.as_deref(),
                    s.workspace.as_deref(),
                )
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
                .remote(
                    s.system,
                    &s.via,
                    &s.person,
                    s.agent.as_deref(),
                    s.workspace.as_deref(),
                )
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
        workspace: Option<&str>,
    ) -> Result<Arc<dyn Remote>, Paused> {
        self.reach(system, via, person, agent)
            .await
            .map(|r| sync::in_workspace(r, system, workspace))
    }

    /// What reaches `system` `via` a hub (or this one), as `person`, in
    /// whichever workspace a call names.
    async fn reach(
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
                agent,
                Arc::clone(&self.adapters),
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
    /// offers them: `prefer` (the board's agent) first, then the agents the
    /// person may reach there ([`Self::candidates`]), its default first,
    /// since each agent offers its own tools. None of them: `Missing`; no
    /// agent that lets the person in: `Refused`.
    pub async fn offered(
        &self,
        system: System,
        via: &Via,
        person: &str,
        prefer: Option<&str>,
        workspace: Option<&str>,
    ) -> Result<Reached, Paused> {
        let probe = |agent: Option<String>| async move {
            let remote = self
                .remote(system, via, person, agent.as_deref(), workspace)
                .await?;
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
        // A token refused or a hub away: no agent there will do.
        let fatal = |p: &Paused| {
            matches!(
                p,
                Paused::NoToken | Paused::HubOffline { .. } | Paused::HubUnpaired
            )
        };
        // The board's own agent first. What the agents lack outranks why
        // one of them refused.
        let mut missing = None;
        let mut other = None;
        if let Some(agent) = prefer {
            match probe(Some(agent.to_string())).await {
                Ok(r) => return Ok(r),
                Err(p) if fatal(&p) => return Err(p),
                Err(p @ Paused::Missing { .. }) => missing = Some(p),
                Err(p) => other = Some(p),
            }
        }
        let rest: Vec<Option<String>> = self
            .candidates(via, person)
            .await?
            .into_iter()
            .filter(|a| a.is_none() || a.as_deref() != prefer)
            .collect();
        // The first (the default, where it lets the person in) alone, as
        // it most often has them; then the others a few at once, in order,
        // until one has them.
        let mut rest = rest.into_iter();
        let first = futures::stream::iter(rest.next()).then(probe);
        let others = futures::stream::iter(rest.map(probe)).buffered(PROBES_AT_ONCE);
        let mut found = std::pin::pin!(first.chain(others));
        while let Some(found) = found.next().await {
            match found {
                Ok(r) => return Ok(r),
                Err(p @ Paused::Missing { .. }) => missing = missing.or(Some(p)),
                Err(p) => other = other.or(Some(p)),
            }
        }
        Err(missing.or(other).unwrap_or(Paused::Missing {
            tools: sync::tools_of(system)
                .iter()
                .map(|t| t.to_string())
                .collect(),
        }))
    }

    /// The agents `person` may reach `via` a hub, in the order they are
    /// asked, its default first: this hub's roster as remote MCP serves it,
    /// or a linked hub's `GET /mcp/agents`. `[None]` (its default agent
    /// only) for an older hub without that list; none at all: `Refused`.
    async fn candidates(&self, via: &Via, person: &str) -> Result<Vec<Option<String>>, Paused> {
        let agents = match via {
            Via::ThisHub => {
                let default = self.state.registry.default_id().to_string();
                let mut ids = crate::remote_mcp::access::reachable_ids(&self.state, person);
                ids.sort_by_key(|id| *id != default);
                Some(ids)
            }
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
                hub_agents(&h.url, &token, &self.http)
                    .await
                    .map_err(|e| paused_by(&e))?
            }
        };
        match agents {
            None => Ok(vec![None]),
            Some(agents) if agents.is_empty() => Err(Paused::Refused {
                reason: NO_AGENT_LETS_YOU_IN.into(),
            }),
            Some(agents) => Ok(agents.into_iter().map(Some).collect()),
        }
    }

    /// Which of the person's several GitHub connections there a link goes
    /// through: the one they picked, unless GitHub says it can't reach what
    /// the link names; else the default, unless GitHub says the same of it;
    /// else the only other that does, the others asked a few at once.
    /// `None`: nothing to choose (Jira or Linear, one connection, nothing
    /// named, a pick that isn't theirs, or none reaching it), so the link
    /// goes on as asked.
    async fn github_connection(
        &self,
        via: &Via,
        person: &str,
        reached: &Reached,
        wanted: Wanted<'_>,
    ) -> Result<Option<String>, PickConnection> {
        let Wanted {
            system,
            names,
            default,
            asked,
            target,
            owner,
        } = wanted;
        if system != System::Github || names.len() < 2 {
            return Ok(None);
        }
        let Some((tool, args)) = gh::reach_probe(target, owner) else {
            return Ok(None);
        };
        let named = target.or(owner).unwrap_or_default();
        let key = format!("{person}|{via:?}|{:?}|{tool}|{args}", reached.agent);
        let (key, args, remote) = (&key, &args, &reached.remote);
        if let Some(asked) = asked.filter(|a| names.iter().any(|n| n == a)) {
            return match self.reaches(remote, key, asked, tool, args).await {
                gh::Reach::No => Err(PickConnection {
                    names: names.to_vec(),
                    why: format!(
                        "Your GitHub connection {asked} can't reach {named}: pick another."
                    ),
                }),
                gh::Reach::Yes | gh::Reach::Unknown => Ok(Some(asked.to_string())),
            };
        }
        if asked.is_some() {
            return Ok(None);
        }
        let default = default.filter(|d| names.iter().any(|n| n == d));
        if let Some(d) = default {
            match self.reaches(remote, key, d, tool, args).await {
                gh::Reach::Yes | gh::Reach::Unknown => return Ok(Some(d.to_string())),
                gh::Reach::No => {}
            }
        }
        let others: Vec<_> = names
            .iter()
            .filter(|n| Some(n.as_str()) != default)
            .map(|name| self.reaching(remote, key, name, tool, args))
            .collect();
        let reaching: Vec<String> = futures::stream::iter(others)
            .buffered(PROBES_AT_ONCE)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .flatten()
            .collect();
        match gh::among(reaching) {
            Some(gh::Connection::Use(one)) => Ok(Some(one)),
            Some(gh::Connection::Ask(several)) => Err(PickConnection {
                why: sync::workspace_words(&workspace_kind(system), None, &several),
                names: several,
            }),
            None => Ok(None),
        }
    }

    /// `name`, if it reaches what `tool` reads ([`Self::reaches`]).
    async fn reaching(
        &self,
        remote: &Arc<dyn Remote>,
        key: &str,
        name: &str,
        tool: &'static str,
        args: &Value,
    ) -> Option<String> {
        (self.reaches(remote, key, name, tool, args).await == gh::Reach::Yes)
            .then(|| name.to_string())
    }

    /// Whether GitHub connection `name` reaches what `tool` reads, as last
    /// asked under `key` within [`REACH_RECHECK`]; asked now otherwise. An
    /// answer GitHub couldn't give isn't kept.
    async fn reaches(
        &self,
        remote: &Arc<dyn Remote>,
        key: &str,
        name: &str,
        tool: &'static str,
        args: &Value,
    ) -> gh::Reach {
        let key = format!("{key}|{name}");
        if let Some(yes) = self.reached.get(&key) {
            return if yes { gh::Reach::Yes } else { gh::Reach::No };
        }
        let through = sync::in_workspace(Arc::clone(remote), System::Github, Some(name));
        let answer = sync::call_quiet(&*through, System::Github, tool, args.clone()).await;
        let reach = gh::reach_of(tool, &answer);
        if reach != gh::Reach::Unknown {
            self.reached.put(key, reach == gh::Reach::Yes);
        }
        reach
    }

    /// The linked hub that offers `system`'s tools to `person`, if any: a
    /// board then syncs through it rather than with this computer's own
    /// keys. A hub that can't be reached now is passed over.
    pub async fn hub_offering(&self, system: System, person: &str) -> Option<String> {
        let hubs = self.desk.sync_hubs().await.ok()?;
        let probes = hubs.into_iter().map(|h| async move {
            let via = Via::Hub { hub: h.id.clone() };
            let offered = tokio::time::timeout(
                PROBE_TIMEOUT * 4,
                self.offered(system, &via, person, None, None),
            )
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
            Via::ThisHub => workforce_desk::board::THIS_COMPUTER.to_string(),
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
        "No agent on {} you can reach has {} switched on.",
        workforce_desk::board::in_sentence(hub),
        system.title()
    )
}

pub(crate) fn paused_words(hub: &str, system: System, p: &Paused) -> String {
    match p {
        Paused::Missing { tools } => missing_words(hub, system, tools),
        Paused::Refused { reason } if reason == NO_AGENT_LETS_YOU_IN => no_agent_words(hub),
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

pub(crate) fn refusal(status: StatusCode, error: &str) -> Response {
    (status, Json(json!({ "error": error }))).into_response()
}

/// The person asking: signed in as themselves, holding this hub.
async fn person(r: &Routes, headers: &HeaderMap, token: Option<&str>) -> Result<String, Response> {
    person_of(&r.sync, headers, token).await
}

/// The person asking `sync`'s hub: signed in as themselves, holding it.
pub(crate) async fn person_of(
    sync: &BoardSync,
    headers: &HeaderMap,
    token: Option<&str>,
) -> Result<String, Response> {
    let caller = resolve_caller_context(&sync.state, headers, token).await;
    holder_of(sync, &caller)
}

/// The person themselves at their own Desk, for what goes out in their own
/// words, unprefixed: [`person_of`], asking with their own sign-in or with
/// the Desk app on a computer paired to them
/// ([`crate::device_link::is_desktop_device`]), never with a phone's, a
/// voice box's or any other device's token. A token made for an agent, a
/// session or a tool (a remote MCP or boss link token) is no sign-in here
/// at all.
pub(crate) async fn person_at_desk(
    sync: &BoardSync,
    headers: &HeaderMap,
    token: Option<&str>,
) -> Result<String, Response> {
    let caller = resolve_caller_context(&sync.state, headers, token).await;
    let person = holder_of(sync, &caller)?;
    if let Some(device) = caller.device_id.as_deref() {
        let store = sync.state.identity_store.as_ref();
        let at_desk = store.is_some_and(|store| {
            crate::device_link::is_desktop_device(store.as_ref(), device)
                && crate::device_link::identity_for_device(store.as_ref(), device).as_deref()
                    == Some(person.as_str())
        });
        if !at_desk {
            return Err(refusal(
                StatusCode::FORBIDDEN,
                "Only you, at Desk on your computer, post in your own words: not a paired phone or other device",
            ));
        }
    }
    Ok(person)
}

/// The person `caller` is, holding `sync`'s hub. The refusal is the
/// answer, as [`person_of`]'s is.
#[allow(clippy::result_large_err)]
fn holder_of(sync: &BoardSync, caller: &CallerContext) -> Result<String, Response> {
    let state = &sync.state;
    match caller.auth_failure {
        Some(AuthFailure::InvalidCredentials | AuthFailure::SurfaceRefused) => {
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
    let Some(person) = caller.identity_id.clone() else {
        return Err(refusal(
            StatusCode::FORBIDDEN,
            "A board's sync is a person's: ask as yourself, not as the hub",
        ));
    };
    if !crate::desk_floor::holds_the_hub(state.identity_store.as_ref(), true, caller.holder()) {
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
pub fn router(sync: crate::surface::Bound<Arc<BoardSync>>) -> crate::surface::Routes {
    crate::surface::Routes::new()
        .admin("/api/desk/sync-hubs", get(list_hubs))
        .admin("/api/desk/sync-hubs/{hub}", put(put_hub).delete(delete_hub))
        .admin("/api/desk/boards/link/check", post(check_link))
        .admin("/api/desk/boards/link/members", post(members_named))
        .admin("/api/desk/boards/link/me", post(choose_me))
        .admin(
            "/api/desk/boards/{id}/link",
            put(link_board).delete(unlink_board),
        )
        .admin("/api/desk/boards/{id}/pause", post(pause_board))
        .admin("/api/desk/boards/{id}/sync", post(sync_board))
        .admin("/api/desk/tickets/{id}/take", post(take_ticket))
        .admin("/api/desk/boards/{id}/me", post(recheck_me))
        .bind(sync.map(|sync| Arc::new(Routes { sync })))
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
    // kept. It is tried on the list of agents its person may reach there,
    // which answers whichever agent is the default; an older hub without
    // that list, on its default agent.
    let name = body.name.trim();
    let turned_away = |e: RemoteError| match e {
        RemoteError::NoToken => refusal(
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("{name} refused that token; make a new one there."),
        ),
        RemoteError::Refused(why) if why == NO_AGENT_LETS_YOU_IN => {
            refusal(StatusCode::UNPROCESSABLE_ENTITY, &no_agent_words(name))
        }
        RemoteError::Refused(why) => refusal(
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("{name} turned that token away: {why}."),
        ),
        e => refusal(
            StatusCode::BAD_GATEWAY,
            &format!("{name} couldn't be reached with it: {e}"),
        ),
    };
    let tried = match &token {
        Some(t) => match hub_agents(url.as_str(), t, &r.sync.http).await {
            Ok(Some(agents)) if agents.is_empty() => {
                return refusal(StatusCode::UNPROCESSABLE_ENTITY, &no_agent_words(name))
            }
            Ok(Some(_)) => true,
            Ok(None) => {
                let remote = match HubRemote::new(url.as_str(), t, Arc::clone(&r.sync.http), None) {
                    Ok(remote) => remote,
                    Err(e) => return refusal(StatusCode::BAD_REQUEST, &e.to_string()),
                };
                match remote.tools().await {
                    Ok(_) => true,
                    Err(e) => return turned_away(e),
                }
            }
            Err(e) => return turned_away(e),
        },
        None => false,
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
    let offers = match tried {
        true => {
            let via = Via::Hub { hub: hub.clone() };
            let (jira, linear, github) = futures::join!(
                r.sync.offered(System::Jira, &via, &person, None, None),
                r.sync.offered(System::Linear, &via, &person, None, None),
                r.sync.offered(System::Github, &via, &person, None, None)
            );
            Some(
                json!({ "jira": jira.is_ok(), "linear": linear.is_ok(), "github": github.is_ok() }),
            )
        }
        false => None,
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
    /// The Jira site or Linear workspace picked, where the person has
    /// several there.
    #[serde(default)]
    workspace: Option<String>,
    /// The GitHub account or organisation whose projects and repositories
    /// to list: none, the person's own.
    #[serde(default)]
    owner: Option<String>,
}

/// Who the person is on a shared key, chosen: the members there whose name
/// contains `name`, or their choice kept (`id`, `name`) or cleared.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MeBody {
    system: System,
    via: Via,
    /// The board's agent there, tried first.
    #[serde(default)]
    agent: Option<String>,
    /// The Jira site or Linear workspace, where the person has several.
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    clear: bool,
}

/// The hub's agent for `body`, and the hub's name; or the refusal.
async fn reach_for_me(
    r: &Routes,
    body: &MeBody,
    person: &str,
) -> Result<(Arc<dyn Remote>, String), Response> {
    if let Some(w) = body.workspace.as_deref() {
        if let Err(e) = sync::check_workspace(w) {
            return Err(refusal(StatusCode::BAD_REQUEST, &e));
        }
    }
    let hub = r.sync.hub_name(&body.via).await;
    match r
        .sync
        .offered(
            body.system,
            &body.via,
            person,
            body.agent.as_deref(),
            body.workspace.as_deref(),
        )
        .await
    {
        Ok(reached) => Ok((reached.remote, hub)),
        Err(p) => Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": paused_words(&hub, body.system, &p), "paused": p })),
        )
            .into_response()),
    }
}

/// The members on the board's service whose name contains what the person
/// typed: id, name and avatar, as the hub's agent lists them (never emails).
async fn members_named(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Json(body): Json<MeBody>,
) -> Response {
    let person = person!(r, h, q);
    let (remote, hub) = match reach_for_me(&r, &body, &person).await {
        Ok(reached) => reached,
        Err(refused) => return refused,
    };
    let name = body.name.as_deref().unwrap_or_default();
    match sync::call(
        &*remote,
        body.system,
        sync::members_tool(body.system),
        json!({ "name": name }),
    )
    .await
    {
        // Only what a member is shown by: whatever else came, it stays here.
        Ok(answer) => Json(
            json!({ "members": answer["members"].as_array().into_iter().flatten().map(|m| json!({
            "id": m["id"], "name": m["name"], "avatar": m["avatar"],
        })).collect::<Vec<_>>() }),
        )
        .into_response(),
        Err(e) => refusal(StatusCode::BAD_GATEWAY, &format!("{hub}: {e}")),
    }
}

/// Keeps or clears whom the person chose to be on the board's service:
/// always their own, on the hub that holds the key.
async fn choose_me(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Json(body): Json<MeBody>,
) -> Response {
    let person = person!(r, h, q);
    let args = if body.clear {
        json!({ "clear": true })
    } else {
        let Some(id) = body.id.as_deref().map(str::trim).filter(|i| !i.is_empty()) else {
            return refusal(
                StatusCode::BAD_REQUEST,
                "Choose someone, or clear your choice.",
            );
        };
        if let Err(e) = sync::check_me(body.system, id) {
            return refusal(StatusCode::BAD_REQUEST, &e);
        }
        json!({ "id": id, "name": body.name })
    };
    let (remote, hub) = match reach_for_me(&r, &body, &person).await {
        Ok(reached) => reached,
        Err(refused) => return refused,
    };
    match sync::call(&*remote, body.system, sync::choose_tool(body.system), args).await {
        Ok(answer) => Json(answer).into_response(),
        Err(e) => refusal(StatusCode::BAD_GATEWAY, &format!("{hub}: {e}")),
    }
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
    shared_me: Option<&str>,
) -> Result<(usize, bool), RemoteError> {
    let mut n = 0usize;
    let pages = sync::MATCH_CEILING.div_ceil(sync::PAGE);
    let mut page: Option<String> = None;
    for _ in 0..pages {
        let (found, next) = match system {
            System::Github => {
                let (n, more) =
                    github_matches(remote, target, target_id, filter, me, page.as_deref()).await?;
                (n, more)
            }
            System::Jira => {
                let mut args = json!({ "jql": sync::jql(target, filter, None, shared_me), "max_results": sync::PAGE, "fields": ["summary"] });
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
                // Counting needs only each issue's key, state and labels:
                // plain pages of 100, not the sync's rich ones.
                let mut args = sync::linear_args(team, filter, me, None, page.as_deref());
                args["rich"] = json!(false);
                args["limit"] = json!(sync::PAGE);
                let a = sync::call(remote, system, "linear.issue_list", args).await?;
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
    Json(mut body): Json<CheckBody>,
) -> Response {
    let person = person!(r, h, q);
    if let Some(target) = body.target.as_deref() {
        if let Err(e) = sync::check_target(body.system, target) {
            return refusal(StatusCode::BAD_REQUEST, &e);
        }
    }
    if let Some(owner) = body.owner.as_deref() {
        if !workforce_desk::github_sync::login_ok(owner) {
            return refusal(
                StatusCode::BAD_REQUEST,
                &format!("{owner:?} isn't a GitHub account."),
            );
        }
    }
    // A filter is checked before any call; Linear's "assigned to me" once
    // the hub has said who the person is there.
    if let Some(filter) = &body.filter {
        let asks_me = matches!(body.system, System::Linear | System::Github)
            && *filter == Filter::AssignedToMe
            && body.me.is_none();
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
        let checked = match body.system {
            System::Github => workforce_desk::github_sync::check_node_id(id),
            _ => sync::check_linear_id(id),
        };
        if let Err(e) = checked {
            return refusal(StatusCode::BAD_REQUEST, &e);
        }
    }
    if let Some(w) = body.workspace.as_deref() {
        if let Err(e) = sync::check_workspace(w) {
            return refusal(StatusCode::BAD_REQUEST, &e);
        }
    }
    let hub = r.sync.hub_name(&body.via).await;
    let reached = match r
        .sync
        .offered(body.system, &body.via, &person, None, None)
        .await
    {
        Ok(reached) => reached,
        Err(p) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": paused_words(&hub, body.system, &p), "paused": p })),
            )
                .into_response()
        }
    };
    // Which agent there offers the tools: "Jira through Lattice".
    let mut out = json!({
        "offered": true,
        "agent": reached.agent.as_deref().map(|a| json!({ "id": a, "name": workforce_desk::board::agent_title(a) })),
    });
    // The person's workspaces there, where they have several: one is
    // picked before any team or project is listed, and every call after
    // names it. Of several GitHub connections, the one that reaches the
    // owner or target named.
    let listed = sync::workspaces(&*reached.remote, body.system).await;
    if let Some((names, default)) = &listed {
        let wanted = Wanted {
            system: body.system,
            names,
            default: default.as_deref(),
            asked: body.workspace.as_deref(),
            target: body.target.as_deref(),
            owner: body.owner.as_deref(),
        };
        match r
            .sync
            .github_connection(&body.via, &person, &reached, wanted)
            .await
        {
            Ok(chosen) => body.workspace = chosen.or(body.workspace),
            Err(pick) => {
                out["workspaces"] = json!(pick.names);
                out["workspaceWhy"] = json!(pick.why);
                return Json(out).into_response();
            }
        }
    }
    let remote = sync::in_workspace(reached.remote, body.system, body.workspace.as_deref());
    if let Some((names, default)) = &listed {
        if names.len() > 1 {
            out["workspaces"] = json!(names);
            out["workspace"] = json!(body.workspace.as_ref().or(default.as_ref()));
        }
        let unknown = body
            .workspace
            .as_ref()
            .filter(|w| !names.contains(w))
            .map(|w| Some(w.clone()));
        let unpicked =
            (body.workspace.is_none() && names.len() > 1 && default.is_none()).then_some(None);
        if let Some(asked) = unknown.or(unpicked) {
            out["workspaces"] = json!(names);
            out["workspaceWhy"] = json!(sync::workspace_words(
                &workspace_kind(body.system),
                asked.as_deref(),
                names
            ));
            return Json(out).into_response();
        }
    }
    // An older hub, without the list, names them when a call goes to none
    // of the person's: the dialog then asks which, as above.
    // None of theirs at all: nothing to pick, said as the error it is.
    let pick_one = |mut out: Value, e: &RemoteError| match e {
        RemoteError::Workspace { names, .. } if !names.is_empty() => {
            out["workspaces"] = json!(names);
            out["workspaceWhy"] = json!(e.to_string());
            Some(Json(out).into_response())
        }
        _ => None,
    };
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
            Err(e) => return pick_one(out, &e).unwrap_or_else(|| said(e)),
        }
    }
    // Jira's projects, for a picker: asked once, with the first check (no
    // target yet), not again as the dialog is edited; only when the hub can
    // list them (a token minted before the tool was offered can't, and the
    // key is typed). Never asks anyone to approve it. Jira Cloud pages at
    // 50, so up to two pages are read for the 100 shown.
    // GitHub's projects and repositories of the owner asked for (else the
    // person's own), for the pickers: asked with the first check, quietly.
    if body.system == System::Github && body.target.is_none() {
        let owner = match body.owner.clone() {
            Some(o) => Some(o),
            None => sync::call_quiet(&*remote, body.system, "github.viewer", json!({}))
                .await
                .ok()
                .and_then(|v| v["login"].as_str().map(str::to_string))
                .filter(|l| workforce_desk::github_sync::login_ok(l)),
        };
        if let Some(owner) = owner {
            out["owner"] = json!(owner);
            match sync::call_quiet(
                &*remote,
                body.system,
                "github.projects_list",
                json!({ "owner": owner }),
            )
            .await
            {
                Ok(list) => out["projects"] = github_projects_of(&owner, &list),
                // A key that can't read projects: said, and repositories
                // are still offered.
                Err(e) => out["projectsWhy"] = json!(github_scope_words(&e)),
            }
            if let Ok(list) = sync::call_quiet(
                &*remote,
                body.system,
                "github.list_repos",
                json!({ "owner": owner, "sort": "updated", "per_page": 100 }),
            )
            .await
            {
                out["repos"] = github_repos_of(&list);
            }
        }
    }
    if body.system == System::Jira && body.target.is_none() {
        let mut all: Vec<Value> = Vec::new();
        while all.len() < MAX_PROJECTS {
            let got = sync::call_quiet(
                &*remote,
                body.system,
                "jira.list_projects",
                json!({ "max_results": 50, "start_at": all.len() }),
            )
            .await;
            let page = match got {
                Ok(page) => page,
                Err(e @ RemoteError::Workspace { .. }) => {
                    return pick_one(out, &e).unwrap_or_else(|| said(e))
                }
                Err(_) => break,
            };
            let list = projects_of(&page);
            let n = list.as_array().map_or(0, Vec::len);
            all.extend(list.as_array().cloned().unwrap_or_default());
            if n < 50 {
                break;
            }
        }
        all.truncate(MAX_PROJECTS);
        if !all.is_empty() {
            out["projects"] = Value::Array(all);
        }
    }
    let Some(target) = body.target.as_deref() else {
        return Json(out).into_response();
    };
    // A repository's milestones, for the milestone filter.
    if body.system == System::Github
        && workforce_desk::github_sync::target_of(target).is_some_and(|t| !t.is_project())
    {
        let (owner, repo) = target.split_once('/').unwrap_or_default();
        if let Ok(v) = sync::call_quiet(
            &*remote,
            body.system,
            "github.repo_labels",
            json!({ "owner": owner, "repo": repo }),
        )
        .await
        {
            out["milestones"] = json!(v["milestones"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| m["title"].as_str())
                .map(|t| sync::unfence(t).0)
                .collect::<Vec<_>>());
        }
    }
    // A project typed by owner and number is found by them: its id is said
    // back, for the link to carry.
    if body.system == System::Github && body.target_id.is_none() {
        match github_project_id(&*remote, target).await {
            Ok(Some(id)) => {
                out["targetId"] = json!(id);
                body.target_id = Some(id);
            }
            Ok(None) => {}
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": format!("{hub}: {}", github_scope_words(&e)) })),
                )
                    .into_response()
            }
        }
    }
    let states = match remote_states(&*remote, body.system, target, body.target_id.as_deref()).await
    {
        Ok(s) => s,
        Err(e) if body.system == System::Github => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("{hub}: {}", github_scope_words(&e)) })),
            )
                .into_response()
        }
        Err(e @ RemoteError::Workspace { .. }) => {
            return pick_one(out, &e).unwrap_or_else(|| said(e))
        }
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
            Ok(sync::Who::ByEmail(me, source)) => {
                out["meBy"] = json!(match source {
                    workforce_desk::board::MeSource::VouchedBySlack => "slack",
                    workforce_desk::board::MeSource::Chosen => "chosen",
                    _ => "email",
                });
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
            me.as_ref()
                .map(|m| m.id.as_str())
                .filter(|_| out["meBy"].is_string()),
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

/// What `system` calls one of a person's instances, as the dialog says it.
fn workspace_kind(system: System) -> String {
    match system {
        System::Jira => "Jira site".into(),
        System::Linear => "Linear workspace".into(),
        System::Github => "GitHub connection".into(),
    }
}

/// The most projects the link dialog lists.
const MAX_PROJECTS: usize = 100;

/// A project list as the dialog shows it: key and name.
fn projects_of(answer: &Value) -> Value {
    let list = answer
        .get("projects")
        .or_else(|| answer.get("values"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Value::Array(
        list.into_iter()
            .filter_map(|p| {
                let key = p["key"].as_str()?;
                Some(json!({
                    "key": key,
                    "name": sync::unfence(p["name"].as_str().unwrap_or(key)).0,
                }))
            })
            .collect(),
    )
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
        System::Github => github_states(remote, target, target_id).await,
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
    if let Some(w) = link.workspace.as_deref() {
        if let Err(e) = sync::check_workspace(w) {
            return refusal(StatusCode::BAD_REQUEST, &e);
        }
    }
    let hub = r.sync.hub_name(&link.via).await;
    let reached = match r
        .sync
        .offered(link.system, &link.via, &person, None, None)
        .await
    {
        Ok(reached) => reached,
        Err(p) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": paused_words(&hub, link.system, &p), "paused": p })),
            )
                .into_response();
        }
    };
    // The workspace it syncs, kept on the link and named on every call:
    // the one picked, which must be the person's there; else the one a call
    // naming none goes to, so a workspace they add later changes nothing.
    // With several and none of them the default, one must be picked. Of
    // several GitHub connections, the one that reaches the target.
    if let Some((names, default)) = sync::workspaces(&*reached.remote, link.system).await {
        let wanted = Wanted {
            system: link.system,
            names: &names,
            default: default.as_deref(),
            asked: link.workspace.as_deref(),
            target: Some(&link.target),
            owner: None,
        };
        match r
            .sync
            .github_connection(&link.via, &person, &reached, wanted)
            .await
        {
            Ok(chosen) => link.workspace = chosen.or(link.workspace.take()),
            Err(pick) => return refusal(StatusCode::BAD_REQUEST, &pick.why),
        }
        match link.workspace.as_ref() {
            Some(w) if !names.contains(w) => {
                return refusal(
                    StatusCode::BAD_REQUEST,
                    &sync::workspace_words(&workspace_kind(link.system), Some(w), &names),
                )
            }
            Some(_) => {}
            // "default" is the name of a key kept without one: it stops
            // naming anything once a named one is added, so isn't kept.
            None if default.as_deref().is_some_and(|d| d != "default") => link.workspace = default,
            None if default.is_some() => {}
            None if names.len() > 1 => {
                return refusal(
                    StatusCode::BAD_REQUEST,
                    &sync::workspace_words(&workspace_kind(link.system), None, &names),
                )
            }
            None => {}
        }
    }
    link.agent = reached.agent;
    let remote = sync::in_workspace(reached.remote, link.system, link.workspace.as_deref());
    // Who the person is there, kept on the link for "assigned to me",
    // Mine and Take it; an id they typed is theirs to keep.
    match &link.me {
        _ if link.not_me && link.me.is_none() => {
            link.me_note = Some(format!(
                "You said {}'s answer of who you are on {} isn't you: check again once it's right.",
                workforce_desk::board::in_sentence(&hub),
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
            Ok(sync::Who::ByEmail(me, source)) => {
                link.me = Some(me.id);
                link.me_name = Some(me.name);
                link.me_source = Some(source);
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
            Err(e @ RemoteError::Workspace { .. }) => {
                return refusal(StatusCode::BAD_REQUEST, &e.to_string())
            }
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
    // A GitHub project is named by owner and number and by its id: the
    // two must be one project there.
    if link.system == System::Github {
        if link.target_id.is_none() {
            match github_project_id(&*remote, &link.target).await {
                Ok(id) => link.target_id = id,
                Err(e) => {
                    return refusal(
                        StatusCode::BAD_REQUEST,
                        &format!("{hub}: {}", github_scope_words(&e)),
                    )
                }
            }
        }
        if let Err(e) = github_states(&*remote, &link.target, link.target_id.as_deref()).await {
            return refusal(
                StatusCode::BAD_REQUEST,
                &format!("{hub}: {}", github_scope_words(&e)),
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
        .remote(
            s.system,
            &s.via,
            &person,
            s.agent.as_deref(),
            s.workspace.as_deref(),
        )
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
        .remote(
            s.system,
            &s.via,
            &person,
            s.agent.as_deref(),
            s.workspace.as_deref(),
        )
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

/// The board sync for this `[desk]` hub.
pub fn install(state: &Arc<GatewayState>, desk: Desk) -> Arc<BoardSync> {
    Arc::new(BoardSync::new(Arc::clone(state), desk))
}

#[cfg(test)]
#[path = "desk_board_sync_tests.rs"]
pub(crate) mod tests;

#[cfg(test)]
#[path = "desk_board_workspace_tests.rs"]
mod workspace_tests;

// ── GitHub ───────────────────────────────────────────────────────────────

/// A GitHub key's refusal in the person's words: one that can't read
/// projects says what to do (sign in again with Projects allowed, or link
/// the repository's issues), never a command to run.
pub(crate) fn github_scope_words(e: &RemoteError) -> String {
    let said = e.to_string();
    let lower = said.to_ascii_lowercase();
    if lower.contains("insufficient_scopes")
        || lower.contains("read:project")
        || lower.contains("resource not accessible")
        || lower.contains("forbidden")
    {
        return "This GitHub key can't read projects. Connect GitHub again with Projects allowed, or link the repository's issues instead. (A fine-grained token never reaches a project owned by a person's account.)".into();
    }
    said
}

/// A projects list as the dialog shows it: its target (`owner/projects/7`),
/// name and id; closed ones left out.
fn github_projects_of(owner: &str, answer: &Value) -> Value {
    Value::Array(
        answer["projects"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| p["closed"].as_bool() != Some(true))
            .filter_map(|p| {
                let n = p["number"].as_u64()?;
                let id = p["id"].as_str()?;
                Some(json!({
                    "key": format!("{owner}/projects/{n}"),
                    "name": sync::unfence(p["title"].as_str().unwrap_or_default()).0,
                    "id": id,
                }))
            })
            .collect(),
    )
}

/// A repositories list as the dialog shows it: `owner/repo`.
fn github_repos_of(answer: &Value) -> Value {
    Value::Array(
        answer["repos"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r["full_name"].as_str())
            .filter(|n| workforce_desk::github_sync::check_repo(n).is_ok())
            .map(|n| json!({ "key": n, "name": n }))
            .collect(),
    )
}

/// The states a GitHub board's columns can be: a project's Status options
/// (the project checked to be the one `target_id` names), or a
/// repository's open, closed and labels.
async fn github_states(
    remote: &dyn Remote,
    target: &str,
    target_id: Option<&str>,
) -> Result<Vec<RemoteState>, RemoteError> {
    use workforce_desk::github_sync::Target;
    match gh::target_of(target) {
        Some(Target::Project { owner, number }) => {
            let p = sync::call(
                remote,
                System::Github,
                "github.project_get",
                json!({ "owner": owner, "number": number }),
            )
            .await?;
            if target_id.is_some_and(|id| p["id"].as_str() != Some(id)) {
                return Err(RemoteError::Failed(format!(
                    "{target} isn't the project with that id"
                )));
            }
            if p["status"].is_null() {
                return Err(RemoteError::Failed(format!(
                    "{target} has no Status field to make columns of"
                )));
            }
            Ok(gh::project_states(&p))
        }
        Some(Target::Repo { owner, repo }) => {
            let v = sync::call(
                remote,
                System::Github,
                "github.repo_labels",
                json!({ "owner": owner, "repo": repo }),
            )
            .await?;
            Ok(gh::repo_states(&v))
        }
        None => Err(RemoteError::Failed(format!(
            "{target} isn't a GitHub project or repository"
        ))),
    }
}

/// The id of the project `target` names by owner and number, or `None`
/// when `target` is a repository.
async fn github_project_id(
    remote: &dyn Remote,
    target: &str,
) -> Result<Option<String>, RemoteError> {
    use workforce_desk::github_sync::Target;
    let Some(Target::Project { owner, number }) = gh::target_of(target) else {
        return Ok(None);
    };
    let p = sync::call(
        remote,
        System::Github,
        "github.project_get",
        json!({ "owner": owner, "number": number }),
    )
    .await?;
    match p["id"].as_str() {
        Some(id) if gh::check_node_id(id).is_ok() => Ok(Some(id.to_string())),
        _ => Err(RemoteError::Failed(format!(
            "{target} isn't a project there"
        ))),
    }
}

/// One page of what a GitHub board's filter matches, counted: the issues on
/// it, and the page after.
async fn github_matches(
    remote: &dyn Remote,
    target: &str,
    target_id: Option<&str>,
    filter: &Filter,
    me: Option<&str>,
    page: Option<&str>,
) -> Result<(usize, Option<String>), RemoteError> {
    use workforce_desk::github_sync::Target;
    match gh::target_of(target) {
        Some(Target::Project { .. }) => {
            let mut args = json!({ "project_id": target_id.unwrap_or_default() });
            if let Some(q) = gh::project_query(filter, me) {
                args["query"] = json!(q);
            }
            if let Some(p) = page {
                args["after"] = json!(p);
            }
            let a = sync::call(remote, System::Github, "github.project_items", args).await?;
            let (items, next, _) = gh::lean_items(&a);
            Ok((items.len(), next))
        }
        Some(Target::Repo { owner, repo }) => {
            let n: u32 = page.and_then(|p| p.parse().ok()).unwrap_or(1);
            let args = gh::repo_args(&owner, &repo, filter, me, None, n);
            let a = sync::call(remote, System::Github, "github.issues_changed", args).await?;
            let found = a["issues"].as_array().map_or(0, Vec::len);
            let next = a["next_page"].as_u64().map(|n| n.to_string());
            Ok((found, next))
        }
        None => Ok((0, None)),
    }
}
