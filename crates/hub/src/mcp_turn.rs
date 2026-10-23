//! One MCP binder for every CLI round the hub spawns.
//!
//! [`RoundTools::bind`] captures, once and in the scope of the turn the round
//! belongs to, everything its tool calls need: the principal they run as, the
//! session they belong to, the turn's progress sink and approval ledger, and a
//! snapshot of the agent's gates. The served endpoint then answers every call
//! through the one [`GatedExecutor`], wherever the transport's task runs — a
//! loopback listener or the runner tunnel inherit no task-locals, so nothing
//! may be read from ambient scope once the round is running.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use eyre::{Result, WrapErr};
use tokio_util::sync::{CancellationToken, DropGuard};

use workforce_agent::runner::{
    current_progress_tx, current_turn_approvals, ApprovalRouter, SessionContext, ToolInvocation,
    SELF_SERVE_APPROVAL_TIMEOUT,
};
use workforce_agent::tools::registry::ToolRegistry;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_agent::turn_journal::TurnJournal;

use crate::mcp::{matches_any, serving_instructions, McpEndpoint, Surface, MCP_SERVER_KEY};
use crate::mcp_gate::{
    ApprovalMode, CallAudit, CallOrigin, CallSite, GatedExecutor, McpTurn, RouteResume,
};
use crate::mcp_session::McpSession;
use crate::serve::AgentService;
use crate::subscription::pools::TurnSlot;
use crate::turn_loop::tools::TurnTools;

/// Tools that act on the runner's own conversation and mean nothing to a CLI
/// that keeps its own.
pub const RUNNER_ONLY_TOOLS: &[&str] = &["context.status", "context.suggest", "context.drop"];

/// Tools that start a coding round of their own. A coding round holds a slot
/// of the pool they wait on, so it is never offered them.
pub const NESTED_ROUND_TOOLS: &[&str] = &["coding.session", "coding.fanout"];

/// How long Claude Code waits for one MCP call when `MCP_TOOL_TIMEOUT` is
/// unset: its default of 100,000,000 ms, about 28 hours.
const CLAUDE_MCP_TOOL_TIMEOUT: Duration = Duration::from_millis(100_000_000);

/// Which of the agent's tools a round is served.
pub enum McpExposure {
    /// An agent CLI turn: the tools the turn itself is offered.
    Turn(TurnTools),
    /// A coding or background round: the agent's tools these patterns name.
    Patterns(Arc<[String]>),
}

/// How long an MCP client waits for one call, and how much of that the tool's
/// own run may take; what is left over is how long a person has to answer
/// the call's approval prompt before the client gives up on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientTimeout {
    pub tool: Duration,
    pub client: Duration,
}

impl ClientTimeout {
    /// Claude Code with its default `MCP_TOOL_TIMEOUT`, which bounds the whole
    /// call and reserves nothing for the tool.
    pub const CLAUDE_CODE: Self = Self {
        tool: Duration::ZERO,
        client: CLAUDE_MCP_TOOL_TIMEOUT,
    };

    /// A loop in this process, which waits on a call for as long as it takes.
    pub const IN_PROCESS: Self = Self {
        tool: Duration::ZERO,
        client: Duration::MAX,
    };

    /// A client configured with [`min_client_timeout`] for tools that run up
    /// to `tool`.
    pub fn for_tool(tool: Duration) -> Self {
        Self {
            tool,
            client: min_client_timeout(tool),
        }
    }

    /// How long a held approval may wait: never so long that the client
    /// gives up on the call first and its model calls again.
    pub fn patience(self) -> Duration {
        SELF_SERVE_APPROVAL_TIMEOUT.min(self.client.saturating_sub(self.tool))
    }
}

/// The least a client may wait for one call to a tool that runs up to
/// `tool`: the tool and a full approval prompt.
fn min_client_timeout(tool: Duration) -> Duration {
    tool + SELF_SERVE_APPROVAL_TIMEOUT
}

/// What a round's endpoints are built from.
struct Frame {
    site: CallSite,
    router: Option<Arc<dyn ApprovalRouter>>,
    server_name: String,
    instructions: String,
    timeout: ClientTimeout,
}

/// The MCP tools of one round, bound to the principal and turn it runs for.
#[derive(Clone)]
pub struct RoundTools {
    frame: Arc<Frame>,
    turn: Arc<McpTurn>,
    endpoint: McpEndpoint,
}

impl RoundTools {
    /// Bind `service`'s tools for a round run as `principal`, in the scope of
    /// the turn it belongs to; `None` when the exposure admits nothing.
    /// Held calls end with `cancel`, when there is one. A call a rule routes
    /// to designated approvers is refused unless the caller says, with
    /// [`RoundTools::resumed`], that its session resumes it.
    pub fn bind(
        service: &AgentService,
        principal: &str,
        exposure: McpExposure,
        cancel: Option<&CancellationToken>,
        timeout: ClientTimeout,
    ) -> Option<Self> {
        let (surface, patterns) = match exposure {
            McpExposure::Turn(turn) => (turn_surface(turn), Vec::new()),
            McpExposure::Patterns(patterns) => (
                patterned(service, principal, Arc::clone(&patterns)),
                patterns.to_vec(),
            ),
        };
        let now = surface.now();
        if now.is_empty() {
            return None;
        }
        let agent_id = service.agent_id.to_string();
        let ctx = SessionContext {
            user_id: Some(principal.to_string()),
            agent_id: Some(agent_id.clone()),
            ..SessionContext::capture()
        };
        let site = CallSite {
            gates: service.tool_gates(),
            policy: Arc::clone(&service.policy),
            surface,
            access: service.tool_access_snapshot(),
            spill: service.current_spill_config(),
            audit: CallAudit {
                origin: CallOrigin::Round,
                identity_id: principal.to_string(),
                agent_id: agent_id.clone(),
            },
        };
        let turn = McpTurn {
            ctx,
            progress: current_progress_tx(),
            approvals: current_turn_approvals(),
            patience: timeout.patience(),
            writer: service.permission_writer.read().clone(),
            routed: service
                .agent_registry
                .get()
                .and_then(std::sync::Weak::upgrade)
                .map(|registry| registry.routed_delivery()),
            resume: RouteResume::Nowhere,
            media: crate::mcp_gate::media_sinks(service),
            model: String::new(),
            journal: None,
            park: None,
            reports: false,
            cancel: cancel.map_or_else(CancellationToken::new, CancellationToken::child_token),
            invocations: Default::default(),
        };
        let frame = Frame {
            site,
            router: service.approval_router.read().clone(),
            instructions: serving_instructions(&now, &patterns),
            server_name: format!("workforce-{agent_id}"),
            timeout,
        };
        Some(Self::assemble(Arc::new(frame), turn))
    }

    fn assemble(frame: Arc<Frame>, turn: McpTurn) -> Self {
        let turn = Arc::new(turn);
        let endpoint = endpoint_of(&frame, &turn, turn.cancel.clone());
        Self {
            frame,
            turn,
            endpoint,
        }
    }

    /// The same round with `adjust` applied to its turn and nothing recorded
    /// yet.
    fn adjusted(self, adjust: impl FnOnce(&mut McpTurn)) -> Self {
        let mut turn = (*self.turn).clone();
        turn.invocations = Default::default();
        adjust(&mut turn);
        Self::assemble(self.frame, turn)
    }

    /// Refuse every call that needs a person's approval: nobody is watching
    /// this round.
    pub fn unattended(self) -> Self {
        self.adjusted(|turn| turn.approvals = None)
    }

    /// Where a routed approval's decision resumes the call it holds.
    pub fn resumed(self, resume: RouteResume) -> Self {
        self.adjusted(|turn| turn.resume = resume)
    }

    /// Record every call's outcome in `journal` under the call's own id, so
    /// collectable work a call starts survives a restart of the hub.
    pub fn with_journal(self, journal: Arc<dyn TurnJournal>) -> Self {
        self.adjusted(|turn| turn.journal = Some(journal))
    }

    /// Serve an agent's turn: each call gives the turn's pool permit back
    /// while it waits, and is reported as the turn's own tool activity.
    pub fn for_agent_turn(self, slot: TurnSlot) -> Self {
        self.adjusted(|turn| {
            turn.park = Some(slot);
            turn.reports = true;
        })
    }

    /// Render viewed files for `model`, the model the round runs on.
    pub fn with_model(self, model: impl Into<String>) -> Self {
        let model = model.into();
        self.adjusted(|turn| turn.model = model)
    }

    /// How long the round's client was bound to wait for one call.
    pub fn client_timeout(&self) -> ClientTimeout {
        self.frame.timeout
    }

    /// The `--allowedTools` names the round's tools are known by.
    pub fn allowed_tools(&self) -> Vec<String> {
        self.endpoint.allowed_tools()
    }

    /// Every call the round's endpoints answered, in the order they
    /// finished.
    pub fn invocations(&self) -> Vec<ToolInvocation> {
        self.turn.invocations.lock().clone()
    }

    /// The endpoint a remote round's tunnelled calls are answered from.
    pub fn endpoint(&self) -> &McpEndpoint {
        &self.endpoint
    }

    /// The round's tools as the tools of a loop in this process: each call
    /// runs through the endpoint, gated and scoped exactly as a served call
    /// is, wherever the loop's task runs.
    pub fn in_process(&self) -> Vec<Arc<dyn Tool>> {
        let tools = self.frame.site.surface.now();
        tools
            .names()
            .into_iter()
            .filter_map(|name| tools.get(&name))
            .map(|tool| {
                Arc::new(Served {
                    tool,
                    endpoint: self.endpoint.clone(),
                }) as Arc<dyn Tool>
            })
            .collect()
    }

    /// Serve the round on a loopback port, with a config naming it written
    /// into a directory of its own for the CLI, never the checkout the round
    /// works in. The listener, the config and every call still held on it
    /// end when the returned value is dropped.
    pub async fn serve_local(&self) -> Result<LocalMcp> {
        let ends = self.turn.cancel.child_token();
        let session = McpSession::start(endpoint_of(&self.frame, &self.turn, ends.clone())).await?;
        let mut dir = tempfile::Builder::new();
        dir.prefix("workforce-mcp-");
        #[cfg(unix)]
        dir.permissions(std::os::unix::fs::PermissionsExt::from_mode(0o750));
        let dir = dir
            .tempdir()
            .wrap_err("creating the MCP config directory")?;
        let path = dir.path().join("mcp.json");
        write_http_config(&path, &session.url(), session.token())
            .wrap_err_with(|| format!("writing the MCP config {}", path.display()))?;
        Ok(LocalMcp {
            path,
            _dir: dir,
            session,
            _ends: ends.drop_guard(),
        })
    }
}

/// One of a round's tools as an in-process loop calls it: through the
/// round's endpoint, never the tool itself.
struct Served {
    tool: Arc<dyn Tool>,
    endpoint: McpEndpoint,
}

#[async_trait::async_trait]
impl Tool for Served {
    fn name(&self) -> &str {
        self.tool.name()
    }

    fn description(&self) -> &str {
        self.tool.description()
    }

    fn input_schema(&self) -> serde_json::Value {
        self.tool.input_schema()
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        Ok(self
            .endpoint
            .call(self.tool.name(), input)
            .await
            .into_result())
    }
}

fn endpoint_of(frame: &Frame, turn: &Arc<McpTurn>, ends: CancellationToken) -> McpEndpoint {
    let executor = GatedExecutor {
        site: frame.site.clone(),
        ctx: turn.ctx.clone(),
        router: frame.router.clone(),
        mode: ApprovalMode::Hold {
            turn: Arc::clone(turn),
            ends,
        },
    };
    McpEndpoint::new(
        frame.site.surface.clone(),
        frame.server_name.clone(),
        Some(frame.instructions.clone()),
        Arc::new(executor),
    )
}

/// A turn's tools as its CLI is served them: an exclusive turn's frozen
/// surface as it stands, any other turn's live, and neither with the
/// runner's own conversation tools.
/// The tools `patterns` name to a round run as `principal`, the adapters
/// they come from loaded first.
fn patterned(service: &AgentService, principal: &str, patterns: Arc<[String]>) -> Surface {
    let admits = pattern_filter(patterns);
    workforce_agent::runner::CURRENT_USER_ID.sync_scope(principal.to_string(), || {
        crate::skill_loader::ensure_adapters_loaded(service, |skill| {
            crate::skill_loader::static_skill_tool_names(skill)
                .iter()
                .any(|tool| admits(tool))
        })
    });
    Surface::Live {
        source: Arc::clone(&service.tools),
        admits: Arc::new(admits),
    }
}

/// The tools themselves that `patterns` expose to a round run as
/// `principal`, ungated: for a loop that binds and gates its tools itself.
pub fn exposed_tools(
    service: &AgentService,
    principal: &str,
    patterns: Arc<[String]>,
) -> Vec<Arc<dyn Tool>> {
    let tools = patterned(service, principal, patterns).now();
    tools
        .names()
        .into_iter()
        .filter_map(|name| tools.get(&name))
        .collect()
}

fn turn_surface(turn: TurnTools) -> Surface {
    let admits = |name: &str| !RUNNER_ONLY_TOOLS.contains(&name);
    match turn.exclusive {
        true => {
            let fixed = ToolRegistry::new();
            turn.registry
                .names()
                .into_iter()
                .filter(|name| admits(name))
                .filter_map(|name| turn.registry.get(&name))
                .for_each(|tool| fixed.register(tool));
            Surface::Fixed(Arc::new(fixed))
        }
        false => Surface::Live {
            source: turn.registry,
            admits: Arc::new(admits),
        },
    }
}

/// Whether a coding or background round's `patterns` admit `name`: never a
/// runner-only tool, nor one that would start a round of its own.
fn pattern_filter(patterns: Arc<[String]>) -> impl Fn(&str) -> bool + Send + Sync + 'static {
    move |name: &str| {
        matches_any(name, &patterns)
            && !RUNNER_ONLY_TOOLS.contains(&name)
            && !NESTED_ROUND_TOOLS.contains(&name)
    }
}

/// A round served on a loopback port for a CLI on this host.
pub struct LocalMcp {
    path: PathBuf,
    /// Holds the config, and removes it with the round.
    _dir: tempfile::TempDir,
    session: McpSession,
    _ends: DropGuard,
}

impl LocalMcp {
    /// The config file the CLI is pointed at with `--mcp-config`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn url(&self) -> String {
        self.session.url()
    }

    pub fn token(&self) -> &str {
        self.session.token()
    }
}

/// Write the config that points a CLI at a hub-owned listener. The bearer
/// token authorises the round's whole tool surface, so the file is created
/// readable by its group only — the unprivileged user a round runs as reads
/// it, and enters its directory, through the shared group; nothing wider —
/// and never exists with looser permissions.
fn write_http_config(path: &Path, url: &str, token: &str) -> std::io::Result<()> {
    use std::io::Write;
    let config = serde_json::json!({
        "mcpServers": {
            MCP_SERVER_KEY: {
                "type": "http",
                "url": url,
                "headers": { "Authorization": format!("Bearer {token}") },
            }
        }
    });
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o640);
    options
        .open(path)?
        .write_all(serde_json::to_string_pretty(&config)?.as_bytes())
}

#[cfg(test)]
pub(crate) mod testing {
    use crate::serve::Compose as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use workforce_agent::runner::{
        CURRENT_BILLING_IDENTITY, CURRENT_CHANNEL_ID, CURRENT_SESSION_ID, CURRENT_SESSION_KEY,
        CURRENT_USER_ID,
    };
    use workforce_agent::tools::{Tool, ToolResult};
    use workforce_core::policy::PermissionChoice;

    /// A tool that answers with `output` and reports the scope each run saw.
    pub(crate) struct Probe {
        pub(crate) name: String,
        pub(crate) runs: AtomicUsize,
        pub(crate) output: serde_json::Value,
    }

    impl Probe {
        pub(crate) fn named(name: &str) -> Arc<Self> {
            Self::answering(name, serde_json::json!({}))
        }

        pub(crate) fn answering(name: &str, output: serde_json::Value) -> Arc<Self> {
            Arc::new(Self {
                name: name.to_string(),
                runs: AtomicUsize::new(0),
                output,
            })
        }

        pub(crate) fn runs(&self) -> usize {
            self.runs.load(Ordering::SeqCst)
        }
    }

    fn scoped(key: &'static tokio::task::LocalKey<String>) -> serde_json::Value {
        key.try_with(|v| v.clone())
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null)
    }

    #[async_trait::async_trait]
    impl Tool for Probe {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "reports the scope it ran in"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            let mut output = serde_json::json!({
                "identity": scoped(&CURRENT_USER_ID),
                "session_key": scoped(&CURRENT_SESSION_KEY),
                "session_id": scoped(&CURRENT_SESSION_ID),
                "billing": scoped(&CURRENT_BILLING_IDENTITY),
                "channel": scoped(&CURRENT_CHANNEL_ID),
                "call": workforce_agent::turn_journal::ToolCall::current()
                    .map(|call| call.id().to_string()),
                "ledger": current_turn_approvals().is_some(),
            });
            if let (Some(out), Some(extra)) = (output.as_object_mut(), self.output.as_object()) {
                out.extend(extra.clone());
            }
            Ok(ToolResult::success(output, 0))
        }
    }

    /// Records every Always-* choice it is asked to persist.
    #[derive(Default)]
    pub(crate) struct Writer(
        pub(crate) parking_lot::Mutex<Vec<(String, String, PermissionChoice)>>,
    );

    #[async_trait::async_trait]
    impl workforce_agent::runner::PermissionWriter for Writer {
        async fn persist_user_choice(
            &self,
            user: &str,
            tool: &str,
            choice: PermissionChoice,
        ) -> eyre::Result<()> {
            self.0.lock().push((user.into(), tool.into(), choice));
            Ok(())
        }
    }

    /// Routes every approval to `bob` and keeps each row it was asked to
    /// write.
    #[derive(Default)]
    pub(crate) struct Routes(
        pub(crate) parking_lot::Mutex<Vec<workforce_agent::runner::PendingApprovalCore>>,
    );

    impl workforce_agent::runner::ApprovalRouter for Routes {
        fn route(
            &self,
            _: &workforce_agent::runner::ApprovalRouteCtx<'_>,
        ) -> workforce_agent::runner::ApprovalDecision {
            workforce_agent::runner::ApprovalDecision::Routed(
                workforce_agent::runner::ApprovalRoute {
                    approver_user_ids: vec!["bob".into()],
                    allow_self_approval: false,
                    ttl: Duration::from_secs(3600),
                },
            )
        }
        fn defer(&self, record: &workforce_agent::runner::PendingApprovalCore) -> eyre::Result<()> {
            self.0.lock().push(record.clone());
            Ok(())
        }
    }

    pub(crate) async fn service() -> Arc<AgentService> {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = workforce_common::settings::WorkforceConfig::default();
        cfg.hub.data_dir = tmp.path().to_string_lossy().to_string();
        cfg.memory.db_path = std::path::Path::new(&cfg.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        cfg.llm.api_key = Some("test-key".into());
        std::mem::forget(tmp);
        AgentService::from_config(cfg, None)
            .await
            .expect("test service")
    }

    /// The response to a `tools/call` of `tool` with `arguments`.
    pub(crate) async fn call(
        endpoint: &McpEndpoint,
        tool: &str,
        arguments: serde_json::Value,
    ) -> serde_json::Value {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": tool, "arguments": arguments }
        })
        .to_string();
        serde_json::from_str(&endpoint.handle(&body).await.expect("answered")).unwrap()
    }

    /// Whether a tool result is an error, and its first text.
    pub(crate) fn text_of(response: &serde_json::Value) -> (bool, String) {
        (
            response["result"]["isError"]
                .as_bool()
                .expect("a tool result"),
            response["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_string(),
        )
    }

    /// The names `tools/list` declares, sorted.
    pub(crate) async fn listed(endpoint: &McpEndpoint) -> Vec<String> {
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let listed: serde_json::Value =
            serde_json::from_str(&endpoint.handle(body).await.unwrap()).unwrap();
        let mut names: Vec<String> = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        names.sort();
        names
    }

    /// A service serving a probe under each of `patterns`, and its tools bound
    /// for a round run as `alice` whose client waits `timeout` on a call.
    pub(crate) async fn round_tools(
        patterns: &[&str],
        timeout: ClientTimeout,
    ) -> (Arc<AgentService>, RoundTools) {
        let service = service().await;
        for pattern in patterns {
            service.tools.register(Probe::named(&format!(
                "{}probe",
                pattern.trim_end_matches('*')
            )));
        }
        let tools = RoundTools::bind(
            &service,
            "alice",
            McpExposure::Patterns(patterns.iter().map(|p| p.to_string()).collect()),
            None,
            timeout,
        )
        .expect("the patterns admit the probes");
        (service, tools)
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{call, listed, service, text_of, Probe, Routes, Writer};
    use super::*;
    use tokio::sync::mpsc;
    use workforce_agent::artifacts::types::ArtifactId;
    use workforce_agent::llm::types::{ContentBlock, ImageSource};
    use workforce_agent::runner::{
        scope_turn, CapabilityGate, ProgressEvent, ROUTED_APPROVAL_TOOL_RESULT,
    };
    use workforce_agent::tools::approval::testing::{AskEveryCall, RecordingTool, TOOL};
    use workforce_agent::tools::approval::{Refusal, TurnApprovals};
    use workforce_agent::tools::image_gen::ImageSink;
    use workforce_agent::tools::image_view::{ImageViewSink, ViewedImageRef};
    use workforce_agent::turn_journal::ToolCall;
    use workforce_core::policy::PermissionChoice;

    const KEY: &str = "slack:T1:C1:100.1";

    /// The session a round's turn runs in, as a surface scopes it.
    fn session() -> SessionContext {
        SessionContext {
            key: Some(KEY.into()),
            id: Some("session-1".into()),
            user_id: Some("alice".into()),
            billing_identity: Some("system:billing".into()),
            channel_id: Some("C1".into()),
            thread_id: Some("100.1".into()),
            ..SessionContext::default()
        }
    }

    fn patterns(patterns: &[&str]) -> McpExposure {
        McpExposure::Patterns(patterns.iter().map(|p| p.to_string()).collect())
    }

    /// Bind in the turn's session with its progress sink and ledger, as an
    /// engine does inside the turn it runs for.
    async fn bind_in_turn(
        service: &AgentService,
        principal: &str,
        exposure: McpExposure,
        progress: Option<mpsc::Sender<ProgressEvent>>,
        ledger: Option<Arc<TurnApprovals>>,
    ) -> Option<RoundTools> {
        session()
            .scope(scope_turn(progress, ledger, async {
                RoundTools::bind(
                    service,
                    principal,
                    exposure,
                    None,
                    ClientTimeout::CLAUDE_CODE,
                )
            }))
            .await
    }

    fn output_of(response: &serde_json::Value) -> serde_json::Value {
        let (is_error, text) = text_of(response);
        assert!(!is_error, "{text}");
        serde_json::from_str(&text).unwrap()
    }

    struct DenyFor(&'static str);

    impl CapabilityGate for DenyFor {
        fn deny_reason(
            &self,
            user: &str,
            _: &str,
            tool: &str,
            _: &serde_json::Value,
        ) -> Option<String> {
            (user == "alice" && tool == self.0).then(|| "needs ShellExec".to_string())
        }
    }

    #[tokio::test]
    async fn cli_round_mcp_call_is_gated_and_session_scoped() {
        let service = service().await;
        service.tools.register(Probe::named("probe.whoami"));
        let denied = Probe::named("probe.shell");
        service.tools.register(denied.clone());
        service.set_capability_gate(Arc::new(DenyFor("probe.shell")));
        let tools = bind_in_turn(&service, "alice", patterns(&["probe.*"]), None, None)
            .await
            .unwrap();

        let seen = output_of(&call(tools.endpoint(), "probe_whoami", serde_json::json!({})).await);
        assert_eq!(seen["identity"], "alice");
        assert_eq!(seen["session_key"], KEY);
        assert_eq!(seen["session_id"], "session-1");
        assert_eq!(seen["billing"], "system:billing");

        let (is_error, text) =
            text_of(&call(tools.endpoint(), "probe_shell", serde_json::json!({})).await);
        assert!(is_error);
        assert_eq!(text, "Policy denied: needs ShellExec");
        assert_eq!(denied.runs(), 0, "a denied call never runs");
    }

    #[tokio::test]
    async fn round_principal_is_never_unscoped() {
        let service = service().await;
        service.tools.register(Probe::named("probe.whoami"));
        let sandbox = crate::coding_sandbox::adopt_workdir(
            std::env::temp_dir(),
            vec!["probe.*".into()],
            None,
        );
        for (identity, stand_in, expected) in [
            (None, workforce_common::subscription::SCHEDULER, "scheduler"),
            (
                None,
                workforce_common::subscription::WORKFLOW_ENGINE,
                "workflow-engine",
            ),
            (
                Some("bob"),
                workforce_common::subscription::SCHEDULER,
                "bob",
            ),
        ] {
            let ctx = crate::coding_engine::RoundContext {
                service: &service,
                identity,
                stand_in,
                ticket_id: None,
                description: "coding.session",
                resume: crate::mcp_gate::RouteResume::Nowhere,
            };
            assert_eq!(ctx.principal(), expected);
            let tools = ctx
                .round_tools(sandbox.as_ref(), None, ClientTimeout::CLAUDE_CODE)
                .unwrap();
            let seen =
                output_of(&call(tools.endpoint(), "probe_whoami", serde_json::json!({})).await);
            assert_eq!(
                seen["identity"], expected,
                "no ambient identity is in scope"
            );
        }
    }

    const PAT: &str = concat!("ghp_AB", "CDEFGHIJKLMNOPQRSTUVWXYZabcdefghij1234");

    #[tokio::test]
    async fn cli_round_mcp_result_is_tokenised() {
        let service = service().await;
        service.tools.register(Probe::answering(
            "probe.leak",
            serde_json::json!({ "token": PAT }),
        ));
        let tools = bind_in_turn(&service, "alice", patterns(&["probe.*"]), None, None)
            .await
            .unwrap();
        let installed = crate::secret_cache::testing::GlobalCache::install(Arc::new(
            crate::secret_cache::InMemorySecretCache::with_defaults(),
        ));
        let response = call(tools.endpoint(), "probe_leak", serde_json::json!({})).await;
        drop(installed);
        let (is_error, text) = text_of(&response);
        assert!(!is_error);
        assert!(!text.contains(PAT), "{text}");
        assert!(
            text.contains("[[secret:"),
            "minted as a marker under the round's session, which a sink can resolve: {text}"
        );
    }

    #[tokio::test]
    async fn mcp_registry_is_per_agent() {
        let one = service().await;
        one.tools.register(Probe::named("one.probe"));
        let two = service().await;
        two.tools.register(Probe::named("two.probe"));
        let bind = |service: Arc<AgentService>| async move {
            bind_in_turn(&service, "alice", patterns(&["one.*", "two.*"]), None, None)
                .await
                .unwrap()
        };
        let (first, second) = (bind(Arc::clone(&one)).await, bind(Arc::clone(&two)).await);
        assert_eq!(listed(first.endpoint()).await, ["one_probe", "tool_call"]);
        assert_eq!(listed(second.endpoint()).await, ["tool_call", "two_probe"]);
        output_of(&call(first.endpoint(), "one_probe", serde_json::json!({})).await);
        let (is_error, _) = text_of_error_or_result(
            &call(first.endpoint(), "two_probe", serde_json::json!({})).await,
        );
        assert!(is_error, "an agent never serves another agent's tool");
    }

    /// A JSON-RPC error counts as an error result.
    fn text_of_error_or_result(response: &serde_json::Value) -> (bool, String) {
        match response.get("error") {
            Some(error) => (
                true,
                error["message"].as_str().unwrap_or_default().to_string(),
            ),
            None => text_of(response),
        }
    }

    #[tokio::test]
    async fn respawned_agent_serves_new_registry_next_round() {
        let registry = Arc::new(crate::agent_registry::AgentRegistry::from_parts(
            Default::default(),
            "a".into(),
        ));
        let before = service().await;
        before.tools.register(Probe::named("old.probe"));
        registry.insert("a", Arc::clone(&before));
        let round = |registry: Arc<crate::agent_registry::AgentRegistry>| async move {
            let service = registry.get("a").unwrap();
            bind_in_turn(&service, "alice", patterns(&["old.*", "new.*"]), None, None)
                .await
                .unwrap()
        };
        let first = round(Arc::clone(&registry)).await;

        let after = service().await;
        after.tools.register(Probe::named("new.probe"));
        registry.insert("a", after);
        let next = round(Arc::clone(&registry)).await;

        assert_eq!(listed(next.endpoint()).await, ["new_probe", "tool_call"]);
        assert_eq!(
            listed(first.endpoint()).await,
            ["old_probe", "tool_call"],
            "a round keeps the service it was bound to"
        );
    }

    #[tokio::test]
    async fn local_round_mcp_config_is_http_0640_and_removed_on_drop() {
        let (_service, tools) =
            testing::round_tools(&["probe.*"], ClientTimeout::CLAUDE_CODE).await;
        let local = tools.serve_local().await.unwrap();
        let path = local.path().to_path_buf();
        let dir = path.parent().unwrap().to_path_buf();
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let server = &config["mcpServers"][MCP_SERVER_KEY];
        assert_eq!(server["type"], "http");
        assert_eq!(server["url"], local.url());
        assert!(
            local.url().starts_with("http://127.0.0.1:"),
            "loopback only"
        );
        assert_eq!(
            server["headers"]["Authorization"],
            format!("Bearer {}", local.token())
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o640
            );
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o750,
                "a round's user enters it through the shared group"
            );
        }
        let reached = reqwest::Client::new()
            .post(local.url())
            .header("Authorization", format!("Bearer {}", local.token()))
            .body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
            .send()
            .await
            .unwrap();
        assert!(reached.text().await.unwrap().contains("probe_probe"));

        let url = local.url();
        drop(local);
        assert!(
            !dir.exists(),
            "the config and its directory go with the round"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            reqwest::Client::new()
                .post(url)
                .body("{}")
                .send()
                .await
                .is_err(),
            "and so does the listener"
        );
    }

    #[tokio::test]
    async fn workflow_empty_exposed_tools_means_everything_not_nothing() {
        let config = workforce_common::settings::McpConfig {
            enabled: true,
            ..Default::default()
        };
        let step = config.step_patterns().expect("MCP is on");
        assert_eq!(step, ["*"]);
        assert_eq!(
            workforce_common::settings::McpConfig::default().step_patterns(),
            None,
            "off serves nothing"
        );
        let service = service().await;
        service.tools.register(Probe::named("jira.get_issue"));
        let tools = bind_in_turn(
            &service,
            "alice",
            McpExposure::Patterns(step.into()),
            None,
            None,
        )
        .await
        .expect("everything is served");
        assert!(listed(tools.endpoint())
            .await
            .contains(&"jira_get_issue".to_string()));
        assert!(
            RoundTools::bind(
                &service,
                "alice",
                McpExposure::Patterns(Arc::from(Vec::<String>::new())),
                None,
                ClientTimeout::CLAUDE_CODE,
            )
            .is_none(),
            "an empty list reaching the binder admits nothing"
        );
    }

    fn register(service: &AgentService, names: &[&str]) {
        names
            .iter()
            .for_each(|name| service.tools.register(Probe::named(name)));
    }

    #[tokio::test]
    async fn coding_round_surface_excludes_nested_round_tools() {
        let service = service().await;
        register(
            &service,
            &[
                "coding.session",
                "coding.fanout",
                "context.status",
                "jira.get_issue",
            ],
        );
        let tools = bind_in_turn(&service, "alice", patterns(&["*"]), None, None)
            .await
            .unwrap();
        let names = listed(tools.endpoint()).await;
        assert!(names.contains(&"jira_get_issue".to_string()));
        let excluded = ["coding_session", "coding_fanout", "context_status"];
        assert!(
            !names.iter().any(|n| excluded.contains(&n.as_str())),
            "{names:?}"
        );
        assert!(!tools
            .allowed_tools()
            .iter()
            .any(|t| excluded.iter().any(|e| t.ends_with(e))));
        let (is_error, _) = text_of_error_or_result(
            &call(tools.endpoint(), "coding_session", serde_json::json!({})).await,
        );
        assert!(is_error, "not reachable by name either");
    }

    #[tokio::test]
    async fn agent_turn_surface_excludes_runner_only_tools() {
        let service = service().await;
        register(
            &service,
            &["context.status", "context.drop", "coding.session", "notes"],
        );
        let tools = bind_in_turn(
            &service,
            "alice",
            McpExposure::Turn(TurnTools {
                registry: Arc::clone(&service.tools),
                exclusive: false,
            }),
            None,
            None,
        )
        .await
        .unwrap();
        let names = listed(tools.endpoint()).await;
        assert!(names.contains(&"notes".to_string()));
        assert!(
            names.contains(&"coding_session".to_string()),
            "an agent turn holds no coding slot, so it keeps coding.session"
        );
        assert!(
            !names
                .iter()
                .any(|n| n == "context_status" || n == "context_drop"),
            "{names:?}"
        );
    }

    #[tokio::test]
    async fn exclusive_turn_mcp_surface_is_snapshot() {
        let service = service().await;
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Probe::named("memory.recall"));
        registry.register(Probe::named("context.status"));
        let turn = |exclusive| TurnTools {
            registry: Arc::clone(&registry),
            exclusive,
        };
        let frozen = bind_in_turn(&service, "alice", McpExposure::Turn(turn(true)), None, None)
            .await
            .unwrap();
        let live = bind_in_turn(
            &service,
            "alice",
            McpExposure::Turn(turn(false)),
            None,
            None,
        )
        .await
        .unwrap();
        registry.register(Probe::named("jira.get_issue"));
        assert_eq!(listed(frozen.endpoint()).await, ["memory_recall"]);
        assert_eq!(
            listed(live.endpoint()).await,
            ["jira_get_issue", "memory_recall", "tool_call"]
        );
    }

    /// Enables a skill mid-round: registers its tool and loads it.
    struct EnableSkill(Arc<ToolRegistry>);

    #[async_trait::async_trait]
    impl Tool for EnableSkill {
        fn name(&self) -> &str {
            "system.enable_skill"
        }
        fn description(&self) -> &str {
            "enable a skill"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
            self.0.register(Probe::named("linear.search"));
            Ok(ToolResult::success(serde_json::json!("linear enabled"), 0)
                .loading(vec!["linear.search".into()]))
        }
    }

    #[tokio::test]
    async fn tool_call_meta_reaches_tool_loaded_mid_round() {
        let service = service().await;
        service
            .tools
            .register(Arc::new(EnableSkill(Arc::clone(&service.tools))));
        let tools = bind_in_turn(&service, "alice", patterns(&["*"]), None, None)
            .await
            .unwrap();
        assert!(!listed(tools.endpoint())
            .await
            .contains(&"linear_search".to_string()));

        let (is_error, text) = text_of(
            &call(
                tools.endpoint(),
                "system_enable_skill",
                serde_json::json!({}),
            )
            .await,
        );
        assert!(!is_error);
        assert!(text.starts_with("linear enabled"), "{text}");
        assert!(
            text.contains("\"name\":\"linear_search\""),
            "the loaded tool's definition comes back with the result: {text}"
        );

        let seen = output_of(
            &call(
                tools.endpoint(),
                "tool_call",
                serde_json::json!({ "name": "linear_search", "arguments": {} }),
            )
            .await,
        );
        assert_eq!(
            seen["identity"], "alice",
            "gated and run like any other call"
        );
    }

    #[tokio::test]
    async fn mcp_call_runs_under_its_own_tool_call() {
        #[derive(Default)]
        struct Journal(parking_lot::Mutex<Vec<(String, String)>>);

        #[async_trait::async_trait]
        impl TurnJournal for Journal {
            async fn pending(
                &self,
                _: &str,
                _: &str,
                _: &serde_json::Value,
                _: serde_json::Value,
            ) -> eyre::Result<()> {
                Ok(())
            }
            async fn checkpoint(&self, _: &str, _: &str) -> eyre::Result<()> {
                Ok(())
            }
            async fn finished(&self, call: &str, invocation: &ToolInvocation) {
                self.0
                    .lock()
                    .push((call.to_string(), invocation.tool_name.clone()));
            }
        }

        let service = service().await;
        service.tools.register(Probe::named("probe.whoami"));
        let journal = Arc::new(Journal::default());
        let parent = ToolCall::new(None, serde_json::Value::Null);
        let tools = parent
            .clone()
            .scope(bind_in_turn(
                &service,
                "alice",
                patterns(&["probe.*"]),
                None,
                None,
            ))
            .await
            .unwrap()
            .with_journal(journal.clone());

        let first = output_of(&call(tools.endpoint(), "probe_whoami", serde_json::json!({})).await);
        let second =
            output_of(&call(tools.endpoint(), "probe_whoami", serde_json::json!({})).await);
        let ids = [
            first["call"].as_str().unwrap(),
            second["call"].as_str().unwrap(),
        ];
        assert_ne!(ids[0], ids[1], "each call is its own");
        assert!(
            ids.iter().all(|id| *id != parent.id()),
            "never the binding turn's call"
        );
        assert_eq!(
            *journal.0.lock(),
            ids.iter()
                .map(|id| (id.to_string(), "probe.whoami".to_string()))
                .collect::<Vec<_>>()
        );
    }

    /// A service whose cascade asks before every call to the recording tool.
    async fn asking() -> (Arc<AgentService>, Arc<RecordingTool>, Arc<Writer>) {
        let service = service().await;
        let tool = Arc::new(RecordingTool::default());
        service.tools.register(Arc::clone(&tool) as Arc<dyn Tool>);
        service.set_permission_gate(Arc::new(AskEveryCall));
        let writer = Arc::new(Writer::default());
        service.set_permission_writer(writer.clone());
        (service, tool, writer)
    }

    struct Turn {
        tools: RoundTools,
        progress: mpsc::Receiver<ProgressEvent>,
        ledger: Arc<TurnApprovals>,
    }

    async fn attended(service: &AgentService) -> Turn {
        let (tx, progress) = mpsc::channel(16);
        let ledger = Arc::new(TurnApprovals::default());
        let tools = bind_in_turn(
            service,
            "alice",
            patterns(&["catalogue.*"]),
            Some(tx),
            Some(Arc::clone(&ledger)),
        )
        .await
        .unwrap();
        Turn {
            tools,
            progress,
            ledger,
        }
    }

    impl Turn {
        fn remove(&self, name: &str) -> tokio::task::JoinHandle<serde_json::Value> {
            let endpoint = self.tools.endpoint().clone();
            let name = name.to_string();
            tokio::spawn(async move {
                call(
                    &endpoint,
                    "catalogue_remove_entry",
                    serde_json::json!({ "name": name }),
                )
                .await
            })
        }

        async fn prompted(&mut self) -> String {
            loop {
                match self.progress.recv().await.expect("a prompt") {
                    ProgressEvent::ApprovalRequired {
                        request_id,
                        routing: None,
                        ..
                    } => return request_id,
                    ProgressEvent::ApprovalRequired { routing, .. } => {
                        panic!("a held prompt is self-serve: {routing:?}")
                    }
                    _ => continue,
                }
            }
        }
    }

    #[tokio::test]
    async fn held_approval_runs_after_allow_and_persists_always() {
        let (service, tool, writer) = asking().await;
        let mut turn = attended(&service).await;
        let held = turn.remove("figma");
        let request_id = turn.prompted().await;
        assert!(tool.runs().is_empty(), "held, not run");
        assert!(turn
            .ledger
            .answer(&request_id, PermissionChoice::AllowAlways));
        let (is_error, text) = text_of(&held.await.unwrap());
        assert!(!is_error, "{text}");
        assert_eq!(tool.runs(), [serde_json::json!({ "name": "figma" })]);
        assert_eq!(
            *writer.0.lock(),
            [(
                "alice".to_string(),
                TOOL.to_string(),
                PermissionChoice::AllowAlways
            )]
        );
    }

    #[tokio::test]
    async fn held_approval_denied_is_not_reasked_in_turn() {
        let (service, tool, _) = asking().await;
        let mut turn = attended(&service).await;
        let held = turn.remove("figma");
        let request_id = turn.prompted().await;
        assert!(turn.ledger.answer(&request_id, PermissionChoice::DenyOnce));
        let (is_error, text) = text_of(&held.await.unwrap());
        assert!(is_error);
        assert_eq!(text, Refusal::Denied.tool_result());

        let (is_error, text) = text_of(&turn.remove("figma").await.unwrap());
        assert!(is_error);
        assert_eq!(
            text,
            Refusal::Denied.tool_result(),
            "refused without asking"
        );
        assert!(tool.runs().is_empty());
        assert!(
            !std::iter::from_fn(|| turn.progress.try_recv().ok())
                .any(|e| matches!(e, ProgressEvent::ApprovalRequired { .. })),
            "the person is not asked again"
        );
    }

    #[tokio::test]
    async fn held_approval_never_runs_after_round_ends() {
        let (service, tool, _) = asking().await;
        let (tx, mut progress) = mpsc::channel(16);
        let ledger = Arc::new(TurnApprovals::default());
        let tools = bind_in_turn(
            &service,
            "alice",
            patterns(&["catalogue.*"]),
            Some(tx),
            Some(Arc::clone(&ledger)),
        )
        .await
        .unwrap();
        let local = tools.serve_local().await.unwrap();
        let held = tokio::spawn(
            reqwest::Client::new()
                .post(local.url())
                .header("Authorization", format!("Bearer {}", local.token()))
                .body(
                    serde_json::json!({
                        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                        "params": { "name": "catalogue_remove_entry", "arguments": { "name": "figma" } }
                    })
                    .to_string(),
                )
                .send(),
        );
        let request_id = loop {
            if let Some(ProgressEvent::ApprovalRequired { request_id, .. }) = progress.recv().await
            {
                break request_id;
            }
        };
        drop(local);
        let ended: serde_json::Value = held.await.unwrap().unwrap().json().await.unwrap();
        assert_eq!(
            text_of(&ended),
            (true, crate::mcp_gate::ROUND_ENDED.to_string())
        );
        assert!(!ledger.answer(&request_id, PermissionChoice::AllowOnce));
        tokio::task::yield_now().await;
        assert!(tool.runs().is_empty(), "a late click finds nothing to run");
    }

    #[tokio::test]
    async fn held_approval_never_runs_after_round_ends_remote() {
        let (service, tool, _) = asking().await;
        let (tx, mut progress) = mpsc::channel(16);
        let ledger = Arc::new(TurnApprovals::default());
        let round = CancellationToken::new();
        let tools = session()
            .scope(scope_turn(Some(tx), Some(Arc::clone(&ledger)), async {
                RoundTools::bind(
                    &service,
                    "alice",
                    patterns(&["catalogue.*"]),
                    Some(&round),
                    ClientTimeout::CLAUDE_CODE,
                )
            }))
            .await
            .unwrap();
        let endpoint = tools.endpoint().clone();
        let held = tokio::spawn(async move {
            call(
                &endpoint,
                "catalogue_remove_entry",
                serde_json::json!({ "name": "figma" }),
            )
            .await
        });
        let request_id = loop {
            if let Some(ProgressEvent::ApprovalRequired { request_id, .. }) = progress.recv().await
            {
                break request_id;
            }
        };
        round.cancel();
        assert_eq!(
            text_of(&held.await.unwrap()),
            (true, crate::mcp_gate::ROUND_ENDED.to_string())
        );
        assert!(!ledger.answer(&request_id, PermissionChoice::AllowOnce));
        assert!(tool.runs().is_empty());
    }

    #[tokio::test]
    async fn routed_mcp_approval_is_delivered_once() {
        let (service, tool, _) = asking().await;
        let routes = Arc::new(Routes::default());
        service.set_approval_router(routes.clone());
        let registry = Arc::new(crate::agent_registry::AgentRegistry::single(Arc::clone(
            &service,
        )));
        let weak = Arc::downgrade(&registry);
        service.agent_registry.set(weak).expect("no registry yet");
        let mut turn = attended(&service).await;
        assert!(
            turn.tools
                .turn
                .routed
                .as_ref()
                .is_some_and(|routed| routed.same_channel(&registry.routed_delivery())),
            "a bound round hands routed approvals to its registry's one queue"
        );
        let (routed_tx, mut routed_rx) = mpsc::channel(4);
        let tools = turn.tools.clone().adjusted(|t| {
            t.routed = Some(routed_tx);
            t.resume = RouteResume::InSession;
        });

        let endpoint = tools.endpoint().clone();
        let (is_error, text) = text_of(
            &call(
                &endpoint,
                "catalogue_remove_entry",
                serde_json::json!({ "name": "figma" }),
            )
            .await,
        );
        assert!(!is_error);
        assert_eq!(text, ROUTED_APPROVAL_TOOL_RESULT);
        assert!(tool.runs().is_empty());

        let handed = routed_rx.try_recv().expect("handed to delivery");
        assert_eq!(handed.approver_user_ids, ["bob"]);
        assert_eq!(handed.channel.as_deref(), Some("C1"));
        assert_eq!(handed.thread.as_deref(), Some("100.1"));
        assert!(routed_rx.try_recv().is_err(), "exactly once");
        assert_eq!(routes.0.lock().len(), 1);
        assert!(
            !std::iter::from_fn(|| turn.progress.try_recv().ok())
                .any(|e| matches!(e, ProgressEvent::ApprovalRequired { .. })),
            "no event a surface would DM from a second time"
        );
        let recorded = tools.invocations();
        assert_eq!(recorded.len(), 1);
        assert!(
            recorded[0].output.get("approval_request_id").is_none(),
            "nothing post-run delivery would DM from again"
        );
    }

    #[tokio::test]
    async fn runner_and_mcp_share_route_approval() {
        let (service, _, _) = asking().await;
        let routes = Arc::new(Routes::default());
        service.set_approval_router(routes.clone());
        let figma = serde_json::json!({ "name": "figma" });

        let turn = attended(&service).await;
        call(
            turn.tools
                .clone()
                .resumed(RouteResume::InSession)
                .endpoint(),
            "catalogue_remove_entry",
            figma.clone(),
        )
        .await;

        let remote = McpEndpoint::new(
            Surface::Fixed(Arc::clone(&service.tools)),
            "t".into(),
            None,
            Arc::new(GatedExecutor {
                site: CallSite {
                    gates: service.tool_gates(),
                    policy: Arc::clone(&service.policy),
                    surface: Surface::Fixed(Arc::clone(&service.tools)),
                    access: service.tool_access_snapshot(),
                    spill: service.current_spill_config(),
                    audit: CallAudit {
                        origin: CallOrigin::RemoteToken(uuid::Uuid::nil()),
                        identity_id: "alice".into(),
                        agent_id: service.agent_id.to_string(),
                    },
                },
                ctx: SessionContext {
                    agent_id: Some(service.agent_id.to_string()),
                    ..session()
                },
                router: Some(routes.clone()),
                mode: ApprovalMode::Park {
                    pending: Arc::new(crate::mcp_gate::PendingCalls::default()),
                    routed: mpsc::channel(4).0,
                    bridge: Vec::new().into(),
                },
            }),
        );
        call(&remote, "catalogue_remove_entry", figma.clone()).await;

        session()
            .scope(async {
                workforce_agent::runner::route_approval(
                    Some(routes.as_ref()),
                    service.agent_id.as_str(),
                    TOOL,
                    "Approval required",
                    &figma,
                )
            })
            .await;

        let routed: Vec<_> = routes
            .0
            .lock()
            .iter()
            .map(|core| {
                (
                    core.agent_id.clone(),
                    core.session_key.clone(),
                    core.channel_id.clone(),
                    core.requester_user_id.clone(),
                    core.tool_name.clone(),
                    core.tool_input.clone(),
                    core.approver_user_ids.clone(),
                )
            })
            .collect();
        assert_eq!(routed.len(), 3);
        assert!(
            routed.iter().all(|core| *core == routed[2]),
            "a CLI round, remote MCP and the runner route one way: {routed:?}"
        );
        assert_eq!(routed[2].2, "C1");
    }

    /// Only the remote bridge's own tools skip the gate, and a round never
    /// has them: a skill tool that shares a bridge tool's name is gated.
    #[tokio::test]
    async fn skill_named_like_bridge_tool_is_gated_in_hold() {
        let service = service().await;
        let lookalike = Probe::named("approval.decide");
        service.tools.register(lookalike.clone());
        service.set_permission_gate(Arc::new(AskEveryCall));
        let tools = bind_in_turn(&service, "alice", patterns(&["approval.*"]), None, None)
            .await
            .unwrap();
        let (is_error, text) =
            text_of(&call(tools.endpoint(), "approval_decide", serde_json::json!({})).await);
        assert!(is_error);
        assert_eq!(text, Refusal::Unattended.tool_result());
        assert_eq!(lookalike.runs(), 0);
    }

    /// A held call cut off by its round's end withdraws its card, as one
    /// answered or timed out does.
    #[tokio::test]
    async fn held_card_is_withdrawn_when_round_ends() {
        let (service, tool, _) = asking().await;
        let (tx, mut progress) = mpsc::channel(16);
        let round = CancellationToken::new();
        let tools = session()
            .scope(scope_turn(
                Some(tx),
                Some(Arc::new(TurnApprovals::default())),
                async {
                    RoundTools::bind(
                        &service,
                        "alice",
                        patterns(&["catalogue.*"]),
                        Some(&round),
                        ClientTimeout::CLAUDE_CODE,
                    )
                },
            ))
            .await
            .unwrap();
        let endpoint = tools.endpoint().clone();
        let held = tokio::spawn(async move {
            call(
                &endpoint,
                "catalogue_remove_entry",
                serde_json::json!({ "name": "figma" }),
            )
            .await
        });
        let asked = loop {
            if let Some(ProgressEvent::ApprovalRequired { request_id, .. }) = progress.recv().await
            {
                break request_id;
            }
        };
        round.cancel();
        held.await.unwrap();
        let withdrawn = std::iter::from_fn(|| progress.try_recv().ok()).any(
            |e| matches!(e, ProgressEvent::ApprovalResolved { request_id } if request_id == asked),
        );
        assert!(withdrawn, "the card is withdrawn");
        assert!(tool.runs().is_empty());
    }

    /// The audit line of a call that waits on, or was refused, an approval
    /// says so, as its record does; it never says the call was allowed.
    #[tokio::test]
    async fn held_calls_are_audited_as_awaiting_approval() {
        #[derive(Clone, Default)]
        struct Lines(Arc<parking_lot::Mutex<Vec<u8>>>);

        impl std::io::Write for Lines {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let lines = Lines::default();
        let writer = lines.clone();
        let audit = tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .with_writer(move || writer.clone())
                .with_ansi(false)
                .finish(),
        );
        // With a single dispatcher registered, a callsite another test's
        // thread meets first takes that thread's interest, which is none; a
        // second one makes every callsite ask each registered dispatcher.
        let _others = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        let _audited = tracing::dispatcher::set_default(&audit);
        let (service, tool, _) = asking().await;
        let unattended = attended(&service).await.tools.unattended();
        service.set_approval_router(Arc::new(Routes::default()));
        let routes = attended(&service).await.tools;
        let figma = serde_json::json!({ "name": "figma" });
        for tools in [
            unattended,
            routes.clone(),
            routes.resumed(RouteResume::InSession),
        ] {
            call(tools.endpoint(), "catalogue_remove_entry", figma.clone()).await;
        }

        let text = String::from_utf8(lines.0.lock().clone()).unwrap();
        let audited: Vec<&str> = text
            .lines()
            .filter(|l| l.contains("round MCP tool call"))
            .collect();
        assert_eq!(audited.len(), 3, "{text}");
        assert!(
            audited.iter().all(|l| l.contains("require_approval")),
            "{audited:?}"
        );
        assert!(tool.runs().is_empty());
    }

    #[tokio::test]
    async fn unattended_round_refuses_approval_required_call() {
        let (service, tool, _) = asking().await;
        let mut turn = attended(&service).await;
        let tools = turn.tools.clone().unattended();
        let (is_error, text) = text_of(
            &call(
                tools.endpoint(),
                "catalogue_remove_entry",
                serde_json::json!({ "name": "figma" }),
            )
            .await,
        );
        assert!(is_error);
        assert_eq!(text, Refusal::Unattended.tool_result());
        assert!(tool.runs().is_empty());
        assert!(turn.progress.try_recv().is_err(), "nobody was asked");
    }

    #[tokio::test]
    async fn nested_coding_session_prompt_uses_parent_ledger() {
        let (service, tool, _) = asking().await;
        let (tx, mut progress) = mpsc::channel(16);
        let parent = Arc::new(TurnApprovals::default());
        // A coding round started by a tool of the parent turn binds in that
        // tool's task, where the runner scoped the turn's sink and ledger.
        let sandbox = crate::coding_sandbox::adopt_workdir(
            std::env::temp_dir(),
            vec!["catalogue.*".into()],
            None,
        );
        let tools = session()
            .scope(scope_turn(Some(tx), Some(Arc::clone(&parent)), async {
                crate::coding_engine::RoundContext {
                    service: &service,
                    identity: Some("alice"),
                    stand_in: workforce_common::subscription::SCHEDULER,
                    ticket_id: None,
                    description: "coding.session",
                    resume: crate::mcp_gate::RouteResume::Nowhere,
                }
                .round_tools(sandbox.as_ref(), None, ClientTimeout::CLAUDE_CODE)
            }))
            .await
            .unwrap();
        let endpoint = tools.endpoint().clone();
        let held = tokio::spawn(async move {
            call(
                &endpoint,
                "catalogue_remove_entry",
                serde_json::json!({ "name": "figma" }),
            )
            .await
        });
        let request_id = loop {
            if let Some(ProgressEvent::ApprovalRequired { request_id, .. }) = progress.recv().await
            {
                break request_id;
            }
        };
        assert!(parent.answer(&request_id, PermissionChoice::AllowOnce));
        assert!(!text_of(&held.await.unwrap()).0);
        assert_eq!(tool.runs().len(), 1);
    }

    /// Renders every viewed file as one PNG.
    struct Viewer;

    #[async_trait::async_trait]
    impl ImageViewSink for Viewer {
        async fn render(
            &self,
            _: &SessionContext,
            viewed: &ViewedImageRef,
            model: &str,
        ) -> eyre::Result<Vec<ContentBlock>> {
            assert_eq!(model, "claude-sonnet-5");
            Ok(vec![ContentBlock::Image {
                source: ImageSource {
                    source_type: "base64".into(),
                    media_type: viewed.media_type.clone(),
                    data: "iVBORw0KGgo=".into(),
                },
            }])
        }
    }

    /// Keeps where each generated image was delivered.
    #[derive(Default)]
    struct Deliveries(parking_lot::Mutex<Vec<(String, Option<String>)>>);

    #[async_trait::async_trait]
    impl ImageSink for Deliveries {
        async fn deliver(
            &self,
            ctx: &SessionContext,
            artifact_id: &ArtifactId,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
        ) -> eyre::Result<()> {
            self.0
                .lock()
                .push((artifact_id.as_str().to_string(), ctx.channel_id.clone()));
            Ok(())
        }
    }

    #[tokio::test]
    async fn viewed_image_returned_as_mcp_image_content() {
        let service = service().await;
        let id = ArtifactId::fresh();
        service.tools.register(Probe::answering(
            "files.view",
            serde_json::json!({
                workforce_agent::tools::image_view::VIEWED_IMAGES_KEY: [
                    { "artifact_id": id.as_str(), "media_type": "image/png", "filename": "a.png" }
                ]
            }),
        ));
        let tools = bind_in_turn(&service, "alice", patterns(&["files.*"]), None, None)
            .await
            .unwrap()
            .with_model("claude-sonnet-5")
            .adjusted(|t| t.media.view = Arc::new(Viewer));
        let response = call(tools.endpoint(), "files_view", serde_json::json!({})).await;
        let content = response["result"]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(
            content[1],
            serde_json::json!({ "type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png" })
        );
    }

    #[tokio::test]
    async fn generated_image_delivered_from_mcp_call() {
        let service = service().await;
        let id = ArtifactId::fresh();
        service.tools.register(Probe::answering(
            "media.generate",
            serde_json::json!({
                workforce_agent::tools::image_gen::GENERATED_IMAGES_KEY: [{ "artifact_id": id.as_str() }]
            }),
        ));
        let deliveries = Arc::new(Deliveries::default());
        let tools = bind_in_turn(&service, "alice", patterns(&["media.*"]), None, None)
            .await
            .unwrap()
            .adjusted(|t| t.media.deliver = deliveries.clone());
        output_of(&call(tools.endpoint(), "media_generate", serde_json::json!({})).await);
        assert_eq!(
            *deliveries.0.lock(),
            [(id.as_str().to_string(), Some("C1".to_string()))],
            "delivered to the thread the round runs for"
        );
    }

    #[tokio::test]
    async fn mcp_invocations_recorded_for_run_result() {
        let service = service().await;
        service.tools.register(Probe::named("probe.whoami"));
        service.tools.register(Probe::named("probe.shell"));
        service.set_capability_gate(Arc::new(DenyFor("probe.shell")));
        let tools = bind_in_turn(&service, "alice", patterns(&["probe.*"]), None, None)
            .await
            .unwrap();
        call(
            tools.endpoint(),
            "probe_whoami",
            serde_json::json!({ "n": 1 }),
        )
        .await;
        call(tools.endpoint(), "probe_shell", serde_json::json!({})).await;
        let recorded: Vec<(String, bool, String, serde_json::Value)> = tools
            .invocations()
            .into_iter()
            .map(|inv| (inv.tool_name, inv.success, inv.policy_decision, inv.input))
            .collect();
        assert_eq!(
            recorded,
            [
                (
                    "probe.whoami".to_string(),
                    true,
                    "allow".to_string(),
                    serde_json::json!({ "n": 1 })
                ),
                (
                    "probe.shell".to_string(),
                    false,
                    "deny".to_string(),
                    serde_json::json!({})
                ),
            ]
        );
    }

    #[test]
    fn the_client_timeout_leaves_a_whole_prompt_after_the_tool() {
        let tool = Duration::from_secs(120);
        assert_eq!(min_client_timeout(tool), tool + SELF_SERVE_APPROVAL_TIMEOUT);
        assert_eq!(
            ClientTimeout::for_tool(tool).patience(),
            SELF_SERVE_APPROVAL_TIMEOUT
        );
        assert_eq!(
            ClientTimeout {
                tool,
                client: tool + Duration::from_secs(60)
            }
            .patience(),
            Duration::from_secs(60),
            "a client that would give up first cuts the prompt short"
        );
        assert_eq!(
            ClientTimeout::CLAUDE_CODE.patience(),
            SELF_SERVE_APPROVAL_TIMEOUT
        );
    }

    /// A tool that runs a nested CLI turn: it takes a permit of `pool`
    /// itself, as a `subagent` call reaching another CLI turn would.
    struct NestedTurn {
        pool: crate::subscription::Pool,
        cancel: CancellationToken,
    }

    #[async_trait::async_trait]
    impl Tool for NestedTurn {
        fn name(&self) -> &str {
            "nested.turn"
        }
        fn description(&self) -> &str {
            "runs a nested CLI turn"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
            TurnSlot::acquire(&self.pool, &self.cancel, async {})
                .await
                .map_err(|e| eyre::eyre!("{e}"))?;
            Ok(ToolResult::success(
                serde_json::json!({ "nested": "done" }),
                0,
            ))
        }
    }

    #[tokio::test]
    async fn nested_cli_run_at_pool_cap_one_completes() {
        let service = service().await;
        let pool = crate::subscription::Pool::new(1);
        let cancel = CancellationToken::new();
        service.tools.register(Arc::new(NestedTurn {
            pool: pool.clone(),
            cancel: cancel.clone(),
        }));
        let held = TurnSlot::acquire(&pool, &cancel, async {}).await.unwrap();
        let slot = held.slot();
        let tools = bind_in_turn(&service, "alice", patterns(&["nested.*"]), None, None)
            .await
            .unwrap()
            .for_agent_turn(slot.clone());

        let response = tokio::time::timeout(
            Duration::from_secs(5),
            call(tools.endpoint(), "nested_turn", serde_json::json!({})),
        )
        .await
        .expect("a turn at the pool's cap never waits on the permit it holds");

        assert_eq!(output_of(&response)["nested"], "done");
        assert!(
            slot.holds(),
            "the permit is taken back before the result returns"
        );
    }

    #[tokio::test]
    async fn approval_wait_parks_turn_permit() {
        let (service, tool, _) = asking().await;
        let pool = crate::subscription::Pool::new(1);
        let cancel = CancellationToken::new();
        let held = TurnSlot::acquire(&pool, &cancel, async {}).await.unwrap();
        let slot = held.slot();
        let mut turn = attended(&service).await;
        turn.tools = turn.tools.for_agent_turn(slot.clone());

        let held = turn.remove("figma");
        let request_id = turn.prompted().await;

        assert!(!slot.holds(), "given back while the person decides");
        let other = tokio::time::timeout(
            Duration::from_secs(1),
            TurnSlot::acquire(&pool, &cancel, async {}),
        )
        .await
        .expect("another turn takes the permit meanwhile")
        .unwrap();
        drop(other);
        assert!(turn.ledger.answer(&request_id, PermissionChoice::AllowOnce));
        let (is_error, text) = text_of(&held.await.unwrap());
        assert!(!is_error, "{text}");
        assert_eq!(tool.runs(), [serde_json::json!({ "name": "figma" })]);
        assert!(slot.holds(), "taken back before the call returned");
    }
}
