//! `POST /mcp` — the hub as an MCP server for external clients.
//!
//! Every call runs as the token's identity: authenticated live, rate limited
//! per identity, then dispatched through the same policy → capability →
//! cascade → secret-marker → sentinel → access chain the agent runner applies
//! (`workforce_agent::tools::dispatch`), inside the caller's `mcp:<identity>`
//! session scope. Stateless POST with no session id: one JSON body per call,
//! except the calls that wait on an agent turn, which stream to a client that
//! accepts SSE (`super::stream`).

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use tokio::sync::mpsc;

use workforce_agent::tools::registry::ToolRegistry;
use workforce_agent::tools::Tool;
use workforce_common::session::SpillConfig;

use crate::gateway::{check_action_rate_limit, too_many_requests_err, CallerContext, GatewayState};
use crate::hosts::RoutedApproval;
use crate::mcp::{build_mcp_registry, matches_any, McpEndpoint, Surface};
use crate::mcp_gate::{ApprovalMode, CallAudit, CallOrigin, CallSite, GatedExecutor, PendingCalls};
use crate::serve::AgentService;

use super::access::{reachable, Unreachable};
use super::agent_tools::{agent_tools, AgentToolsContext, ChatTurns};
use super::approvals::approval_tools;
use super::auth::authenticate;
use super::help::help_tools;
use super::skill_tools::skill_tools;
use super::stream;

const BODY_LIMIT_BYTES: usize = 4 * 1024 * 1024;
/// Outputs above this are staged and replaced with a retrievable handle
/// (when the surface can retrieve it) rather than returned inline.
const RESULT_SPILL_BYTES: usize = 1024 * 1024;
const EXPOSE_ALL: &str = "*";

/// What every remote caller's session channel starts with.
pub const MCP_CHANNEL_PREFIX: &str = "mcp:";

/// The session channel a remote caller's tool calls and `agent.chat` share.
pub fn session_channel(identity_id: &str) -> String {
    format!("{MCP_CHANNEL_PREFIX}{identity_id}")
}

pub struct RemoteMcpState {
    gateway: Arc<GatewayState>,
    agent_tools: Vec<Arc<dyn Tool>>,
    bridge: Arc<[Arc<dyn Tool>]>,
    pending_calls: Arc<PendingCalls>,
    routed: mpsc::Sender<RoutedApproval>,
}

impl RemoteMcpState {
    pub fn new(gateway: Arc<GatewayState>) -> Arc<Self> {
        let pending_calls = Arc::new(PendingCalls::default());
        let routed = gateway.registry.routed_delivery();
        let ctx = Arc::new(AgentToolsContext {
            gateway: Arc::clone(&gateway),
            turns: Arc::new(ChatTurns::default()),
            pending_calls: Arc::clone(&pending_calls),
            routed: routed.clone(),
            config: gateway.remote_mcp.clone(),
            identity_store: gateway.identity_store.clone(),
            sentinel: gateway.sentinel.clone(),
        });
        let bridge: Arc<[Arc<dyn Tool>]> = approval_tools(Arc::clone(&ctx)).into();
        let mut tools = agent_tools(Arc::clone(&ctx));
        tools.extend(bridge.iter().cloned());
        tools.extend(skill_tools(ctx));
        tools.extend(help_tools());
        Arc::new(Self {
            gateway,
            agent_tools: tools,
            bridge,
            pending_calls,
            routed,
        })
    }

    /// The tools one agent exposes remotely: its registry plus the `agent.*`
    /// collaboration tools, filtered by `[remote_mcp] tools_allow`. Rebuilt
    /// per request so a skill enabled mid-session is visible at once.
    fn surface_for(&self, service: &AgentService) -> Arc<ToolRegistry> {
        Arc::new(curated_surface(
            &service.tools,
            &self.agent_tools,
            &self.gateway.remote_mcp.tools_allow,
        ))
    }
}

/// The hub tool kept off every remote surface, see [`curated_surface`].
const HIDDEN_AGENT_SHOW: &str = "agent.show";

/// `tools` and `extra` filtered by `patterns` as `build_mcp_registry` reads
/// them, with `["*"]` meaning everything.
pub fn curated_surface(
    tools: &ToolRegistry,
    extra: &[Arc<dyn Tool>],
    patterns: &[String],
) -> ToolRegistry {
    let everything = patterns.iter().any(|p| p == EXPOSE_ALL);
    let surface = build_mcp_registry(tools, if everything { &[] } else { patterns });
    // The hub's `agent.show` shows any agent, its prompt among it, whatever
    // the agent's access policy: remotely, `agent.list` names the agents a
    // caller may reach, and nothing names the others.
    surface.unregister(HIDDEN_AGENT_SHOW);
    extra
        .iter()
        .filter(|tool| everything || matches_any(tool.name(), patterns))
        .for_each(|tool| surface.register(Arc::clone(tool)));
    surface
}

pub fn router(remote: Arc<RemoteMcpState>) -> Router {
    Router::new()
        .route(
            "/mcp",
            post(serve_default)
                .get(method_not_allowed)
                .delete(session_closed),
        )
        .route(
            "/mcp/agents/{agent_id}",
            post(serve_agent)
                .get(method_not_allowed)
                .delete(session_closed),
        )
        .layer(DefaultBodyLimit::max(BODY_LIMIT_BYTES))
        .with_state(remote)
}

async fn method_not_allowed() -> StatusCode {
    StatusCode::METHOD_NOT_ALLOWED
}

async fn session_closed() -> StatusCode {
    StatusCode::OK
}

async fn serve_default(
    State(remote): State<Arc<RemoteMcpState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    serve(&remote, None, &headers, &body).await
}

async fn serve_agent(
    State(remote): State<Arc<RemoteMcpState>>,
    Path(agent_id): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    serve(&remote, Some(&agent_id), &headers, &body).await
}

async fn serve(
    remote: &RemoteMcpState,
    agent_id: Option<&str>,
    headers: &HeaderMap,
    body: &str,
) -> Response {
    let gateway = &remote.gateway;
    if !gateway.remote_mcp.enabled {
        return (
            StatusCode::NOT_FOUND,
            "remote MCP is not enabled on this install",
        )
            .into_response();
    }
    let caller = match authenticate(gateway, headers) {
        Ok(caller) => caller,
        Err(refusal) => return refusal.into_response(),
    };
    let identity_id = caller.identity.id.to_string();
    let limited = CallerContext {
        identity_id: Some(identity_id.clone()),
        ..Default::default()
    };
    if let Err(retry_after) = check_action_rate_limit(gateway.action_limiter.as_ref(), &limited) {
        return too_many_requests_err(retry_after).into_response();
    }
    let service = match reachable(gateway, &identity_id, agent_id) {
        Ok(service) => service,
        // The plain route serves the default agent: one that does not admit
        // the caller leaves them nothing to call through it.
        Err(Unreachable::Denied(_)) if agent_id.is_none() => {
            return (
                StatusCode::FORBIDDEN,
                "this hub's default agent does not admit you; connect to /mcp/agents/{agent_id} for an agent you can use",
            )
                .into_response()
        }
        // A named agent the caller may not use reads as one that is not
        // there, so ids cannot be probed.
        Err(_) => return (StatusCode::NOT_FOUND, "unknown agent").into_response(),
    };
    let ctx = match crate::remote_mcp::session_context::session_context(&service, &identity_id)
        .await
    {
        Ok(ctx) => ctx,
        Err(e) => {
            tracing::warn!(error = %e, identity = %identity_id, "remote MCP session unavailable");
            return (StatusCode::INTERNAL_SERVER_ERROR, "session unavailable").into_response();
        }
    };
    let surface = remote.surface_for(&service);
    let executor = GatedExecutor {
        site: CallSite {
            gates: service.tool_gates(crate::desk_surface::allows()),
            policy: Arc::clone(&service.policy),
            surface: Surface::Fixed(Arc::clone(&surface)),
            access: service.tool_access_snapshot(),
            spill: remote_spill_config(service.current_spill_config()),
            audit: CallAudit {
                origin: CallOrigin::RemoteToken(caller.token.id),
                identity_id,
                agent_id: service.agent_id.to_string(),
            },
        },
        ctx,
        router: service.approval_router.read().clone(),
        mode: ApprovalMode::Park {
            pending: Arc::clone(&remote.pending_calls),
            routed: remote.routed.clone(),
            bridge: Arc::clone(&remote.bridge),
        },
    };
    let instructions = instructions(&surface, &service.agent_id.to_string());
    // The executor scopes each call's full session context, so the endpoint
    // carries no identity of its own.
    let endpoint = McpEndpoint::new(
        Surface::Fixed(surface),
        format!("workforce-{}", service.agent_id),
        Some(instructions),
        Arc::new(executor),
    );
    if stream::streamed(headers, body) {
        let body = body.to_string();
        return stream::respond(
            async move { endpoint.handle(&body).await },
            stream::KEEPALIVE,
        );
    }
    match endpoint.handle(body).await {
        Some(response) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            response,
        )
            .into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

fn remote_spill_config(agent: SpillConfig) -> SpillConfig {
    SpillConfig {
        threshold_bytes: agent.threshold_bytes.max(RESULT_SPILL_BYTES),
        ..agent
    }
}

fn instructions(surface: &ToolRegistry, agent_id: &str) -> String {
    let mut families: Vec<String> = surface
        .names()
        .iter()
        .map(|n| crate::failed_tools::tool_family(n).to_string())
        .collect();
    families.sort();
    families.dedup();
    format!(
        "Workforce agent `{agent_id}` over remote MCP ({} tools; families: {}). Every call runs as you, \
         under your workforce permissions — a denied call names the capability you lack. Use agent.chat \
         to ask this hub's agents questions or delegate work (agent.poll for turns that outlive the wait), \
         agent.history to reread a session, and agent.list to see who is here. A call that needs your \
         approval comes back pending with a request_id: ask the user, then answer with approval.decide. \
         help serves this install's documentation.",
        surface.len(),
        families.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_mcp::testing::{fixture, issue, TokenShape};
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tower::ServiceExt;
    use workforce_agent::runner::{CapabilityGate, CascadeState, PermissionGate, CURRENT_USER_ID};
    use workforce_agent::tools::access::ToolAccessSnapshot;
    use workforce_agent::tools::approval::testing::RecordingTool;
    use workforce_agent::tools::dispatch::ToolGates;
    use workforce_common::sentinel_config::SentinelConfig;
    use workforce_common::settings::RemoteMcpConfig;
    use workforce_policy::engine::PolicyEngine;

    use crate::mcp_turn::testing::text_of;
    use async_trait::async_trait;
    use uuid::Uuid;
    use workforce_agent::runner::{
        ApprovalDecision, ApprovalRoute, ApprovalRouteCtx, ApprovalRouter, PendingApprovalCore,
        SessionContext, UNREACHABLE_APPROVER,
    };
    use workforce_agent::tools::ToolResult;
    use workforce_core::types::AgentId;
    use workforce_identity::store::IdentityStore;
    use workforce_identity::types::{Identity, IdentityRole};

    struct Named(&'static str);

    #[async_trait]
    impl Tool for Named {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "fixture"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
            Ok(ToolResult::success(serde_json::json!({ "ran": self.0 }), 0))
        }
    }

    /// Reports the identity the call ran under and counts executions.
    struct WhoAmI {
        runs: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for WhoAmI {
        fn name(&self) -> &str {
            "test.whoami"
        }
        fn description(&self) -> &str {
            "who"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            let who = CURRENT_USER_ID
                .try_with(|u| u.clone())
                .unwrap_or_else(|_| "<unscoped>".into());
            Ok(ToolResult::success(
                serde_json::json!({ "identity": who }),
                0,
            ))
        }
    }

    fn registry(names: &[&'static str]) -> ToolRegistry {
        let registry = ToolRegistry::new();
        names
            .iter()
            .for_each(|n| registry.register(Arc::new(Named(n))));
        registry
    }

    #[test]
    fn a_star_exposes_the_whole_registry() {
        let source = registry(&["exec.run", "vault.set", "workflow"]);
        let extra: Vec<Arc<dyn Tool>> = vec![Arc::new(Named("agent.chat"))];
        let all = curated_surface(&source, &extra, &["*".to_string()]);
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn the_agent_tools_are_subject_to_the_allowlist_too() {
        let source = registry(&["workflow"]);
        let extra: Vec<Arc<dyn Tool>> =
            vec![Arc::new(Named("agent.chat")), Arc::new(Named("agent.list"))];
        let without = curated_surface(&source, &extra, &["workflow".to_string()]);
        assert_eq!(without.names(), ["workflow"]);
        let with = curated_surface(
            &source,
            &extra,
            &["workflow".to_string(), "agent.*".to_string()],
        );
        assert_eq!(with.len(), 3);
    }

    #[test]
    fn the_consent_tools_ride_the_default_allowlist_like_the_agent_tools() {
        let source = registry(&["workflow"]);
        let extra: Vec<Arc<dyn Tool>> = vec![
            Arc::new(Named("skill.grant")),
            Arc::new(Named("skill.visibility")),
            Arc::new(Named("skill.transfer")),
            Arc::new(Named("help")),
        ];
        let mut names =
            curated_surface(&source, &extra, &RemoteMcpConfig::default().tools_allow).names();
        names.sort();
        assert_eq!(
            names,
            [
                "help",
                "skill.grant",
                "skill.transfer",
                "skill.visibility",
                "workflow"
            ]
        );
        let without = curated_surface(&source, &extra, &["workflow".to_string()]);
        assert_eq!(without.names(), ["workflow"]);
    }

    /// `agent.show` would name an agent the caller may not reach, with its
    /// prompt: it is on no remote surface, even one that exposes everything.
    #[test]
    fn the_hub_s_agent_show_is_never_on_the_remote_surface() {
        let source = registry(&["agent.show", "agent.create", "workflow"]);
        let extra: Vec<Arc<dyn Tool>> = vec![Arc::new(Named("agent.list"))];
        for patterns in [
            RemoteMcpConfig::default().tools_allow,
            vec!["*".to_string()],
            vec!["agent.show".to_string()],
        ] {
            let names = curated_surface(&source, &extra, &patterns).names();
            assert!(
                !names.iter().any(|n| n == "agent.show"),
                "{patterns:?}: {names:?}"
            );
        }
    }

    #[test]
    fn the_external_skill_tools_ride_the_default_allowlist_but_the_catalog_install_does_not() {
        let source = registry(&[
            "system.install_external_skill",
            "system.uninstall_external_skill",
            "system.list_external_skills",
            "system.publish_external_skill",
            "system.install_catalog_skill",
            "system.disable_skill",
        ]);
        let mut names =
            curated_surface(&source, &[], &RemoteMcpConfig::default().tools_allow).names();
        names.sort();
        assert_eq!(
            names,
            [
                "system.install_external_skill",
                "system.list_external_skills",
                "system.publish_external_skill",
                "system.uninstall_external_skill"
            ]
        );
    }

    #[test]
    fn the_default_allowlist_keeps_shell_and_code_tools_off_the_surface() {
        let source = registry(&[
            "exec.run",
            "code.git",
            "coding.session",
            "jobs",
            "vault.set",
            "vault.get",
            "workflow",
            "playbook",
            "scheduler",
            "wasm_skill.build",
            "slack.post",
        ]);
        let surface = curated_surface(&source, &[], &RemoteMcpConfig::default().tools_allow);
        let mut names = surface.names();
        names.sort();
        assert_eq!(
            names,
            [
                "playbook",
                "scheduler",
                "vault.get",
                "vault.set",
                "wasm_skill.build",
                "workflow"
            ]
        );
    }

    fn request(method: &str, uri: &str, token: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        builder
            .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#))
            .unwrap()
    }

    #[tokio::test]
    async fn the_routes_are_absent_until_enabled() {
        let fx = fixture(false);
        let app = router(RemoteMcpState::new(Arc::clone(&fx.state)));
        for uri in ["/mcp", "/mcp/agents/anyone"] {
            let resp = app
                .clone()
                .oneshot(request("POST", uri, None))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri}");
        }
    }

    #[tokio::test]
    async fn only_post_carries_the_protocol() {
        let fx = fixture(true);
        let app = router(RemoteMcpState::new(Arc::clone(&fx.state)));
        let get = app
            .clone()
            .oneshot(request("GET", "/mcp", None))
            .await
            .unwrap();
        assert_eq!(get.status(), StatusCode::METHOD_NOT_ALLOWED);
        let delete = app.oneshot(request("DELETE", "/mcp", None)).await.unwrap();
        assert_eq!(delete.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn an_unauthenticated_post_is_refused_before_anything_else() {
        let fx = fixture(true);
        let app = router(RemoteMcpState::new(Arc::clone(&fx.state)));
        let resp = app.oneshot(request("POST", "/mcp", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(resp.headers().get("www-authenticate").unwrap(), "Bearer");
    }

    #[tokio::test]
    async fn an_authenticated_caller_reaches_agent_resolution() {
        let fx = fixture(true);
        let who = Identity::new("Member", IdentityRole::Member);
        fx.identities.create(&who).unwrap();
        let token = issue(&fx, &who, TokenShape::default());
        let app = router(RemoteMcpState::new(Arc::clone(&fx.state)));
        let resp = app
            .oneshot(request("POST", "/mcp/agents/nobody", Some(&token)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"unknown agent");
    }

    /// A hub serving the default agent and `private`, where `private` (or,
    /// with `default_too`, the default agent as well) admits only `member`.
    async fn hub_with_a_private_agent(
        data_dir: &std::path::Path,
        default_too: bool,
    ) -> (crate::remote_mcp::testing::Fixture, Identity, Identity) {
        use workforce_common::settings::{AccessMode, AgentAccessConfig};
        let fx = fixture(true);
        crate::remote_mcp::testing::with_agents(&fx, data_dir, &["default", "private"]).await;
        let member = Identity::new("Member", IdentityRole::Member);
        let other = Identity::new("Other", IdentityRole::Member);
        fx.identities.create(&member).unwrap();
        fx.identities.create(&other).unwrap();
        let only_member = AgentAccessConfig {
            mode: AccessMode::Allowlist,
            allow: vec![member.id.to_string()],
            ..Default::default()
        };
        fx.state
            .access_gate
            .upsert("private".into(), only_member.clone());
        if default_too {
            fx.state.access_gate.upsert("default".into(), only_member);
        }
        (fx, member, other)
    }

    async fn status_and_body(app: &Router, uri: &str, token: &str) -> (StatusCode, String) {
        let resp = app
            .clone()
            .oneshot(request("POST", uri, Some(token)))
            .await
            .unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn an_agent_whose_policy_excludes_the_caller_reads_as_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let (fx, member, other) = hub_with_a_private_agent(tmp.path(), false).await;
        let app = router(RemoteMcpState::new(Arc::clone(&fx.state)));
        let admitted = issue(&fx, &member, TokenShape::default());
        let excluded = issue(&fx, &other, TokenShape::default());

        let (status, _) = status_and_body(&app, "/mcp/agents/private", &admitted).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            status_and_body(&app, "/mcp/agents/private", &excluded).await,
            (StatusCode::NOT_FOUND, "unknown agent".to_string())
        );
        let (status, _) = status_and_body(&app, "/mcp", &excluded).await;
        assert_eq!(status, StatusCode::OK, "the default agent admits everyone");
    }

    #[tokio::test]
    async fn a_mistyped_agent_never_reaches_the_default_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let (fx, member, _) = hub_with_a_private_agent(tmp.path(), false).await;
        let app = router(RemoteMcpState::new(Arc::clone(&fx.state)));
        let token = issue(&fx, &member, TokenShape::default());
        assert_eq!(
            status_and_body(&app, "/mcp/agents/defualt", &token).await,
            (StatusCode::NOT_FOUND, "unknown agent".to_string())
        );
    }

    #[tokio::test]
    async fn the_plain_route_refuses_a_caller_the_default_agent_excludes() {
        let tmp = tempfile::tempdir().unwrap();
        let (fx, member, other) = hub_with_a_private_agent(tmp.path(), true).await;
        let app = router(RemoteMcpState::new(Arc::clone(&fx.state)));
        let (status, _) =
            status_and_body(&app, "/mcp", &issue(&fx, &member, TokenShape::default())).await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) =
            status_and_body(&app, "/mcp", &issue(&fx, &other, TokenShape::default())).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.contains("/mcp/agents/"), "{body}");
    }

    struct FixedCapability(Option<&'static str>);

    impl CapabilityGate for FixedCapability {
        fn deny_reason(
            &self,
            _u: &str,
            _a: &str,
            _t: &str,
            _i: &serde_json::Value,
        ) -> Option<String> {
            self.0.map(str::to_string)
        }
    }

    struct FixedCascade(CascadeState);

    impl PermissionGate for FixedCascade {
        fn evaluate(&self, _u: &str, _a: &str, _t: &str) -> CascadeState {
            self.0
        }
    }

    /// Everything an executor shares across the calls of one test: parked
    /// calls, the routing rule, and where routed approvals are handed off.
    struct Bridge {
        pending_calls: Arc<PendingCalls>,
        router: Option<Arc<dyn ApprovalRouter>>,
        routed: mpsc::Sender<RoutedApproval>,
        revoked: Arc<AtomicBool>,
        tools: std::sync::OnceLock<Arc<[Arc<dyn Tool>]>>,
    }

    impl Default for Bridge {
        fn default() -> Self {
            Self {
                pending_calls: Arc::new(PendingCalls::default()),
                router: None,
                routed: mpsc::channel(4).0,
                revoked: Arc::new(AtomicBool::new(false)),
                tools: Default::default(),
            }
        }
    }

    impl Bridge {
        /// The `approval.*` tools, wired to the same parked calls as the
        /// executor under test and built once, so the surface and the
        /// executor hold the same instances.
        fn tools(&self) -> Arc<[Arc<dyn Tool>]> {
            Arc::clone(self.tools.get_or_init(|| {
                approval_tools(Arc::new(AgentToolsContext {
                    gateway: Arc::clone(&fixture(true).state),
                    turns: Arc::new(ChatTurns::default()),
                    pending_calls: Arc::clone(&self.pending_calls),
                    routed: self.routed.clone(),
                    config: RemoteMcpConfig::default(),
                    identity_store: None,
                    sentinel: SentinelConfig::default(),
                }))
                .into()
            }))
        }
    }

    /// A capability gate that starts open and can be shut after a call was
    /// parked, like an admin revoking the capability meanwhile.
    struct RevocableCapability(Arc<AtomicBool>);

    impl CapabilityGate for RevocableCapability {
        fn deny_reason(
            &self,
            _u: &str,
            _a: &str,
            _t: &str,
            _i: &serde_json::Value,
        ) -> Option<String> {
            self.0
                .load(Ordering::SeqCst)
                .then(|| "capability revoked".to_string())
        }
    }

    fn executor(
        surface: Arc<ToolRegistry>,
        deny: Option<&'static str>,
        cascade: CascadeState,
    ) -> GatedExecutor {
        executor_as("alice", surface, deny, cascade, &Bridge::default())
    }

    fn executor_as(
        identity: &str,
        surface: Arc<ToolRegistry>,
        deny: Option<&'static str>,
        cascade: CascadeState,
        bridge: &Bridge,
    ) -> GatedExecutor {
        executor_on("agent", identity, surface, deny, cascade, bridge)
    }

    fn executor_on(
        agent: &str,
        identity: &str,
        surface: Arc<ToolRegistry>,
        deny: Option<&'static str>,
        cascade: CascadeState,
        bridge: &Bridge,
    ) -> GatedExecutor {
        GatedExecutor {
            site: CallSite {
                gates: ToolGates {
                    agent_id: AgentId::new(agent),
                    team_id: None,
                    role: None,
                    capability_gate: Some(match deny {
                        Some(reason) => Arc::new(FixedCapability(Some(reason))),
                        None => Arc::new(RevocableCapability(Arc::clone(&bridge.revoked))),
                    }),
                    permission_gate: Some(Arc::new(FixedCascade(cascade))),
                    surface_allows: None,
                    arguments: workforce_agent::tools::arguments::ArgumentValidation::default(),
                },
                policy: Arc::new(
                    PolicyEngine::from_yaml(
                        "rules:\n  - name: allow-all\n    scope: global\n    tool: \"*\"\n    action: allow\n",
                    )
                    .unwrap(),
                ),
                surface: Surface::Fixed(surface),
                access: ToolAccessSnapshot::default(),
                spill: SpillConfig::default(),
                audit: CallAudit {
                    origin: CallOrigin::RemoteToken(Uuid::nil()),
                    identity_id: identity.into(),
                    agent_id: agent.into(),
                },
            },
            ctx: SessionContext {
                key: Some(session_channel(identity)),
                id: Some("session-1".into()),
                user_id: Some(identity.into()),
                agent_id: Some(agent.into()),
                ..Default::default()
            },
            router: bridge.router.clone(),
            mode: ApprovalMode::Park {
                pending: Arc::clone(&bridge.pending_calls),
                routed: bridge.routed.clone(),
                bridge: bridge.tools(),
            },
        }
    }

    /// A surface with the caller-reporting tool and the `approval.*` bridge.
    fn bridged_surface(bridge: &Bridge) -> (Arc<ToolRegistry>, Arc<AtomicUsize>) {
        let (surface, runs) = whoami_surface();
        bridge
            .tools()
            .iter()
            .for_each(|tool| surface.register(Arc::clone(tool)));
        (surface, runs)
    }

    async fn park_whoami(surface: &Arc<ToolRegistry>, bridge: &Bridge) -> serde_json::Value {
        let exec = executor_as(
            "alice",
            Arc::clone(surface),
            None,
            CascadeState::Ask,
            bridge,
        );
        let (is_error, text) = text_of(&call(surface, exec, "test_whoami").await);
        assert!(
            !is_error,
            "a parked call is a pending outcome, not an error"
        );
        serde_json::from_str(&text).unwrap()
    }

    async fn decide(
        surface: &Arc<ToolRegistry>,
        bridge: &Bridge,
        identity: &str,
        request_id: &str,
        choice: &str,
    ) -> (bool, String) {
        let exec = executor_as(
            identity,
            Arc::clone(surface),
            None,
            CascadeState::Ask,
            bridge,
        );
        let args = serde_json::json!({ "request_id": request_id, "choice": choice });
        text_of(&call_with(surface, exec, "approval_decide", args).await)
    }

    struct StubRouter {
        decision: fn() -> ApprovalDecision,
        store: Arc<dyn crate::hub_core::approvals::PendingApprovalStore>,
    }

    impl ApprovalRouter for StubRouter {
        fn route(&self, _ctx: &ApprovalRouteCtx<'_>) -> ApprovalDecision {
            (self.decision)()
        }

        fn defer(&self, record: &PendingApprovalCore) -> eyre::Result<()> {
            let expires_at = chrono::Utc::now()
                + chrono::Duration::from_std(record.ttl).unwrap_or(chrono::Duration::hours(1));
            self.store.insert_core(record, None, None, expires_at)
        }
    }

    fn endpoint(surface: &Arc<ToolRegistry>, exec: GatedExecutor) -> McpEndpoint {
        McpEndpoint::new(
            Surface::Fixed(Arc::clone(surface)),
            "t".into(),
            None,
            Arc::new(exec),
        )
    }

    async fn call(
        surface: &Arc<ToolRegistry>,
        exec: GatedExecutor,
        tool: &str,
    ) -> serde_json::Value {
        call_with(surface, exec, tool, serde_json::json!({})).await
    }

    async fn call_with(
        surface: &Arc<ToolRegistry>,
        exec: GatedExecutor,
        tool: &str,
        arguments: serde_json::Value,
    ) -> serde_json::Value {
        crate::mcp_turn::testing::call(&endpoint(surface, exec), tool, arguments).await
    }

    fn whoami_surface() -> (Arc<ToolRegistry>, Arc<AtomicUsize>) {
        let runs = Arc::new(AtomicUsize::new(0));
        let surface = ToolRegistry::new();
        surface.register(Arc::new(WhoAmI {
            runs: Arc::clone(&runs),
        }));
        (Arc::new(surface), runs)
    }

    #[tokio::test]
    async fn an_allowed_call_runs_as_the_caller() {
        let (surface, runs) = whoami_surface();
        let exec = executor(Arc::clone(&surface), None, CascadeState::Allow);
        let (is_error, text) = text_of(&call(&surface, exec, "test_whoami").await);
        assert!(!is_error);
        let output: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(output["identity"], "alice");
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn remote_mcp_result_is_tokenised() {
        const PAT: &str = concat!("ghp_AB", "CDEFGHIJKLMNOPQRSTUVWXYZabcdefghij1234");
        let surface = Arc::new(ToolRegistry::new());
        surface.register(crate::mcp_turn::testing::Probe::answering(
            "probe.leak",
            serde_json::json!({ "token": PAT }),
        ));
        let exec = executor(Arc::clone(&surface), None, CascadeState::Allow);
        let installed = crate::secret_cache::testing::GlobalCache::install(Arc::new(
            crate::secret_cache::InMemorySecretCache::with_defaults(),
        ));
        let response = call(&surface, exec, "probe_leak").await;
        drop(installed);
        let (is_error, text) = text_of(&response);
        assert!(!is_error);
        assert!(!text.contains(PAT), "{text}");
        assert!(
            text.contains("[[secret:"),
            "a marker under the caller's own session: {text}"
        );
    }

    #[tokio::test]
    async fn a_capability_denial_is_an_error_result_and_never_executes() {
        let (surface, runs) = whoami_surface();
        let exec = executor(
            Arc::clone(&surface),
            Some("needs ShellExec"),
            CascadeState::Allow,
        );
        let (is_error, text) = text_of(&call(&surface, exec, "test_whoami").await);
        assert!(is_error);
        assert_eq!(text, "Policy denied: needs ShellExec");
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    /// A skill tool that holds itself behind a human decision. Policy here
    /// allows everything and the cascade says Allow, so the only thing that
    /// can park the call is the tool's own requirement.
    struct GatedSkillTool(&'static str, Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl Tool for GatedSkillTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "apply a change"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn approval_requirement(&self) -> Option<String> {
            Some("Required by the skill's own approval gate".to_string())
        }
        async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
            self.1.fetch_add(1, Ordering::SeqCst);
            Ok(ToolResult::success(serde_json::json!({"applied": true}), 0))
        }
    }

    /// Park a gated tool and report how many times it actually ran.
    async fn park_gated(name: &'static str) -> (serde_json::Value, usize) {
        let bridge = Bridge::default();
        let (surface, _) = bridged_surface(&bridge);
        let ran = Arc::new(AtomicUsize::new(0));
        surface.register(Arc::new(GatedSkillTool(name, Arc::clone(&ran))));
        let exec = executor_as(
            "alice",
            Arc::clone(&surface),
            None,
            CascadeState::Allow,
            &bridge,
        );
        let (is_error, text) = text_of(&call(&surface, exec, name).await);
        assert!(!is_error, "a parked call is pending, not an error: {text}");
        (
            serde_json::from_str(&text).unwrap(),
            ran.load(Ordering::SeqCst),
        )
    }

    /// The gate's exemption is the approval bridge's own tools, not their
    /// names: a skill tool named like one, or merely under `approval.`, is
    /// gated like any other.
    #[tokio::test]
    async fn a_look_alike_skill_tool_does_not_inherit_the_bridge_exemption() {
        for name in ["approval.apply_change", "approval.decide"] {
            let (payload, ran) = park_gated(name).await;
            assert_eq!(payload["status"], "approval_pending", "{name}");
            assert_eq!(ran, 0, "{name}: nothing ran");
        }
    }

    #[tokio::test]
    async fn a_skill_gated_tool_parks_even_when_policy_and_cascade_allow() {
        let (payload, ran) = park_gated("record-keeper.apply_change").await;
        assert_eq!(payload["status"], "approval_pending");
        assert!(
            !payload["options"]
                .as_array()
                .unwrap()
                .iter()
                .any(|o| o.as_str() == Some("always")),
            "a skill gate never offers Always-Allow: {payload}"
        );
        assert_eq!(ran, 0, "nothing ran");
    }

    #[tokio::test]
    async fn an_ask_decision_parks_the_call_for_the_caller() {
        let bridge = Bridge::default();
        let (surface, runs) = bridged_surface(&bridge);
        let payload = park_whoami(&surface, &bridge).await;
        assert_eq!(payload["status"], "approval_pending");
        assert_eq!(payload["route"], "self");
        assert_eq!(payload["tool"], "test.whoami");
        assert_eq!(
            payload["options"],
            serde_json::json!(["allow_once", "deny_once", "allow_always", "deny_always"])
        );
        assert!(payload["reason"].as_str().is_some_and(|r| !r.is_empty()));
        let request_id = payload["request_id"].as_str().unwrap();
        assert!(bridge.pending_calls.get(request_id, "alice").is_some());
        assert!(bridge.pending_calls.get(request_id, "bob").is_none());
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_parked_call_runs_as_the_caller_once_allowed() {
        let bridge = Bridge::default();
        let (surface, runs) = bridged_surface(&bridge);
        let parked = park_whoami(&surface, &bridge).await;
        let request_id = parked["request_id"].as_str().unwrap();

        let (is_error, text) = decide(&surface, &bridge, "alice", request_id, "allow_once").await;
        assert!(!is_error, "{text}");
        let output: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(output["identity"], "alice");
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(bridge.pending_calls.get(request_id, "alice").is_none());

        let (is_error, text) = decide(&surface, &bridge, "alice", request_id, "allow_once").await;
        assert!(is_error);
        assert_eq!(text, "unknown request");
        assert_eq!(runs.load(Ordering::SeqCst), 1, "a decision is consumed");
    }

    #[tokio::test]
    async fn concurrent_decisions_run_a_parked_call_once() {
        let bridge = Bridge::default();
        let (surface, runs) = bridged_surface(&bridge);
        let parked = park_whoami(&surface, &bridge).await;
        let request_id = parked["request_id"].as_str().unwrap();

        let (first, second) = tokio::join!(
            decide(&surface, &bridge, "alice", request_id, "allow_once"),
            decide(&surface, &bridge, "alice", request_id, "allow_once"),
        );
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_ne!(
            first.0, second.0,
            "exactly one decision wins: {first:?} {second:?}"
        );
    }

    #[tokio::test]
    async fn a_parked_call_is_decided_through_its_own_agent() {
        let bridge = Bridge::default();
        let (surface, runs) = bridged_surface(&bridge);
        let parked = park_whoami(&surface, &bridge).await;
        let request_id = parked["request_id"].as_str().unwrap();

        let exec = executor_on(
            "other",
            "alice",
            Arc::clone(&surface),
            None,
            CascadeState::Allow,
            &bridge,
        );
        let args = serde_json::json!({ "request_id": request_id, "choice": "allow_once" });
        let (is_error, text) = text_of(&call_with(&surface, exec, "approval_decide", args).await);
        assert!(is_error);
        assert!(text.contains("belongs to agent agent"), "{text}");
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert!(
            bridge.pending_calls.get(request_id, "alice").is_some(),
            "still parked"
        );
    }

    #[tokio::test]
    async fn an_approved_call_is_still_gated_at_execution() {
        let bridge = Bridge::default();
        let (surface, runs) = bridged_surface(&bridge);
        let parked = park_whoami(&surface, &bridge).await;
        let request_id = parked["request_id"].as_str().unwrap();

        bridge.revoked.store(true, Ordering::SeqCst);
        let (is_error, text) = decide(&surface, &bridge, "alice", request_id, "allow_once").await;
        assert!(is_error);
        assert_eq!(text, "Policy denied: capability revoked");
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert!(
            bridge.pending_calls.get(request_id, "alice").is_none(),
            "a refused decision still consumes the request"
        );
    }

    #[tokio::test]
    async fn a_direct_call_missing_a_required_field_is_refused_not_parked() {
        let bridge = Bridge::default();
        let (surface, runs) = bridged_surface(&bridge);
        let catalogue = Arc::new(RecordingTool::default());
        surface.register(Arc::clone(&catalogue) as Arc<dyn Tool>);
        let exec = executor_as(
            "alice",
            Arc::clone(&surface),
            None,
            CascadeState::Ask,
            &bridge,
        );
        let (is_error, text) = text_of(&call(&surface, exec, "catalogue_remove_entry").await);
        assert!(is_error, "{text}");
        assert_eq!(
            text,
            "'catalogue.remove_entry' was called without required input `name`, so it did not \
             run and no approval was requested. Call it again with every required field."
        );
        assert_eq!(bridge.pending_calls.len(), 0, "nothing is parked");
        assert!(catalogue.runs().is_empty());
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_choice_the_prompt_withholds_is_refused() {
        let bridge = Bridge::default();
        let (surface, runs) = bridged_surface(&bridge);
        let exec = executor_as(
            "alice",
            Arc::clone(&surface),
            None,
            CascadeState::AlwaysAsk,
            &bridge,
        );
        let (_, text) = text_of(&call(&surface, exec, "test_whoami").await);
        let parked: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            parked["options"],
            serde_json::json!(["allow_once", "deny_once", "deny_always"])
        );
        let request_id = parked["request_id"].as_str().unwrap();

        let (is_error, text) = decide(&surface, &bridge, "alice", request_id, "allow_always").await;
        assert!(is_error);
        assert_eq!(text, "allow_always is not offered for this prompt");
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert!(bridge.pending_calls.get(request_id, "alice").is_some());
    }

    #[tokio::test]
    async fn a_parked_call_denied_never_runs() {
        let bridge = Bridge::default();
        let (surface, runs) = bridged_surface(&bridge);
        let parked = park_whoami(&surface, &bridge).await;
        let request_id = parked["request_id"].as_str().unwrap();

        let (is_error, text) = decide(&surface, &bridge, "alice", request_id, "deny_once").await;
        assert!(!is_error);
        let output: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(output["status"], "denied");
        assert_eq!(output["tool"], "test.whoami");
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert!(bridge.pending_calls.get(request_id, "alice").is_none());
    }

    #[tokio::test]
    async fn a_parked_call_is_only_its_callers_to_decide() {
        let bridge = Bridge::default();
        let (surface, runs) = bridged_surface(&bridge);
        let parked = park_whoami(&surface, &bridge).await;
        let request_id = parked["request_id"].as_str().unwrap();

        let (is_error, text) = decide(&surface, &bridge, "bob", request_id, "allow_once").await;
        assert!(is_error);
        assert_eq!(text, "unknown request");
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert!(bridge.pending_calls.get(request_id, "alice").is_some());
    }

    #[tokio::test]
    async fn a_routed_ask_persists_the_row_and_hands_delivery_off() {
        let store: Arc<dyn crate::hub_core::approvals::PendingApprovalStore> =
            Arc::new(crate::approvals::SqlitePendingApprovalStore::in_memory().unwrap());
        let (routed_tx, mut routed_rx) = mpsc::channel(4);
        let bridge = Bridge {
            router: Some(Arc::new(StubRouter {
                decision: || {
                    ApprovalDecision::Routed(ApprovalRoute {
                        approver_user_ids: vec!["bob".into()],
                        allow_self_approval: false,
                        ttl: std::time::Duration::from_secs(3600),
                    })
                },
                store: Arc::clone(&store),
            })),
            routed: routed_tx,
            ..Default::default()
        };
        let (surface, runs) = bridged_surface(&bridge);
        let payload = park_whoami(&surface, &bridge).await;
        assert_eq!(payload["status"], "approval_pending");
        assert_eq!(payload["route"], "routed");
        assert_eq!(payload["approvers"], serde_json::json!(["bob"]));
        assert_eq!(payload["expires_in_secs"], 3600);
        let request_id = payload["request_id"].as_str().unwrap();

        let row = store.get(request_id).unwrap().expect("the durable row");
        assert_eq!(row.requester_id, "alice");
        assert_eq!(row.tool_name, "test.whoami");
        assert_eq!(row.session_key, session_channel("alice"));
        assert!(!row.allow_self_approval);
        let handed = routed_rx.recv().await.unwrap();
        assert_eq!(handed.request_id, request_id);
        assert_eq!(handed.agent_id, "agent");
        assert_eq!(handed.approver_user_ids, ["bob"]);
        assert!(bridge.pending_calls.get(request_id, "alice").is_none());
        assert_eq!(runs.load(Ordering::SeqCst), 0);

        let (is_error, text) = decide(&surface, &bridge, "alice", request_id, "allow_once").await;
        assert!(is_error);
        assert_eq!(
            text, "unknown request",
            "no agent holds the store in this harness"
        );
    }

    #[tokio::test]
    async fn an_unreachable_route_denies_without_parking() {
        let store: Arc<dyn crate::hub_core::approvals::PendingApprovalStore> =
            Arc::new(crate::approvals::SqlitePendingApprovalStore::in_memory().unwrap());
        let bridge = Bridge {
            router: Some(Arc::new(StubRouter {
                decision: || ApprovalDecision::DenyUnreachable,
                store,
            })),
            ..Default::default()
        };
        let (surface, runs) = bridged_surface(&bridge);
        let exec = executor_as(
            "alice",
            Arc::clone(&surface),
            None,
            CascadeState::Ask,
            &bridge,
        );
        let (is_error, text) = text_of(&call(&surface, exec, "test_whoami").await);
        assert!(is_error);
        assert_eq!(text, UNREACHABLE_APPROVER);
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn the_curated_surface_is_what_tools_list_shows() {
        let source = registry(&["exec.run", "vault.set", "workflow"]);
        let surface = Arc::new(curated_surface(
            &source,
            &[],
            &RemoteMcpConfig::default().tools_allow,
        ));
        let exec = executor(Arc::clone(&surface), None, CascadeState::Allow);
        assert_eq!(
            crate::mcp_turn::testing::listed(&endpoint(&surface, exec)).await,
            ["vault_set", "workflow"]
        );
    }
}
