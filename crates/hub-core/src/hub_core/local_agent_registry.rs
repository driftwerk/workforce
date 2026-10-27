//! Local Agent Registry — the nodes connected over `/ws/agent` (a person's
//! laptop running `workforce local-agent connect`), each bound to the person
//! whose paired device it authenticated as.
//!
//! A node runs `exec.run` for its person's sessions and offers the tools of
//! an MCP server it tunnels, which their sessions see as
//! `node.<node id>.<tool>`. Only the bound person's turns reach a node:
//! every route to one names the caller, and a caller who is not its person
//! is refused here, whatever the session says.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use eyre::{bail, eyre, Result};
use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::{mpsc, oneshot, RwLock};
use tracing::{info, warn};

use workforce_gateway_client::protocol::{
    LocalExecCancel, LocalExecRequest, LocalExecResult, NodeToolCall, NodeToolCancel,
    NodeToolResult, NodeToolSpec,
};
use workforce_mcp_client::McpLimits;

/// What every node tool's name starts with.
pub const NODE_TOOL_PREFIX: &str = "node.";

/// The longest a node id may be, so its tools' names fit
/// [`MAX_TOOL_NAME_LEN`] with room for the tool's own name.
pub const MAX_NODE_ID_LEN: usize = 24;

/// The most of a node's own name its tools' descriptions carry.
const MAX_NODE_NAME_CHARS: usize = 64;

/// The longest tool name every model provider accepts (OpenAI and Bedrock
/// allow 64 characters). A node tool whose name would be longer is not
/// offered: one bad definition must not fail every turn of its person.
pub const MAX_TOOL_NAME_LEN: usize = 64;

/// The node tools that only read, as their nodes said, across every
/// registry in the process: [`crate::authz::tool_capability`] sees a call's
/// name alone and asks here.
static READ_ONLY_TOOLS: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

/// Whether `tool` is a node tool its node says only reads.
pub fn is_read_only_tool(tool: &str) -> bool {
    READ_ONLY_TOOLS.lock().contains(tool)
}

/// The name a node's tool goes by: `node.<node id>.<tool>`, the tool's own
/// dots made underscores so the prefix is the only dotted part.
pub fn tool_name(node_id: &str, tool: &str) -> String {
    format!("{NODE_TOOL_PREFIX}{node_id}.{}", tool.replace('.', "_"))
}

/// Whether `id` can name a node: 1–24 of `[a-z0-9-]`, so `node.<id>.` stays
/// unambiguous once a model's dots become underscores.
pub fn valid_node_id(id: &str) -> bool {
    (1..=MAX_NODE_ID_LEN).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Info about a connected local agent (for API/display).
#[derive(Debug, Clone, Serialize)]
pub struct LocalAgentInfo {
    pub id: String,
    pub name: String,
    pub capabilities: Vec<String>,
    pub connected_at: DateTime<Utc>,
    /// The tools it offers, by the names its person's sessions see.
    pub tools: Vec<String>,
}

/// A node as its hello introduced it.
pub struct NodeHello {
    pub id: String,
    pub name: String,
    pub capabilities: Vec<String>,
    pub tools: Vec<NodeToolSpec>,
}

/// One of a node's tools, as its person's sessions are offered it.
#[derive(Debug, Clone, PartialEq)]
pub struct OfferedTool {
    pub node_name: String,
    /// `node.<node id>.<tool>`.
    pub name: String,
    pub spec: NodeToolSpec,
}

/// Why a node was not registered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegisterRefusal {
    #[error(
        "the node id '{0}' is not usable: use 1-{MAX_NODE_ID_LEN} lowercase letters, digits \
         and dashes (`--id`)"
    )]
    InvalidId(String),
    #[error("another person's node is connected as '{0}'; connect with a different `--id`")]
    Taken(String),
}

/// A connected local agent with its communication channels.
struct ConnectedLocalAgent {
    name: String,
    capabilities: Vec<String>,
    connected_at: DateTime<Utc>,
    /// The person this node acts for.
    identity_id: String,
    /// Which connection holds the id, so a superseded one's teardown
    /// leaves its successor alone.
    connection: u64,
    tools: Vec<OfferedTool>,
    /// Send serialized JSON messages to this agent's WebSocket write loop.
    request_tx: mpsc::Sender<String>,
    pending_exec: Mutex<HashMap<String, oneshot::Sender<LocalExecResult>>>,
    pending_tools: Mutex<HashMap<String, oneshot::Sender<NodeToolResult>>>,
}

impl ConnectedLocalAgent {
    fn info(&self, id: &str) -> LocalAgentInfo {
        LocalAgentInfo {
            id: id.to_string(),
            name: self.name.clone(),
            capabilities: self.capabilities.clone(),
            connected_at: self.connected_at,
            tools: self.tools.iter().map(|t| t.name.clone()).collect(),
        }
    }

    fn bound_to(&self, identity_id: &str) -> bool {
        self.identity_id == identity_id
    }
}

/// Registry of connected local agents.
#[derive(Default)]
pub struct LocalAgentRegistry {
    agents: RwLock<HashMap<String, ConnectedLocalAgent>>,
    connections: AtomicU64,
}

impl LocalAgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `hello`'s node for `identity_id`, returning its connection.
    /// The same person connecting again replaces their earlier connection,
    /// whose waiting calls fail; another person's id is refused.
    pub async fn register(
        &self,
        hello: NodeHello,
        identity_id: String,
        request_tx: mpsc::Sender<String>,
    ) -> Result<u64, RegisterRefusal> {
        if !valid_node_id(&hello.id) {
            return Err(RegisterRefusal::InvalidId(hello.id));
        }
        let mut agents = self.agents.write().await;
        if agents
            .get(&hello.id)
            .is_some_and(|held| !held.bound_to(&identity_id))
        {
            return Err(RegisterRefusal::Taken(hello.id));
        }
        let connection = self.connections.fetch_add(1, Ordering::Relaxed);
        let name: String = hello.name.chars().take(MAX_NODE_NAME_CHARS).collect();
        info!(agent_id = %hello.id, name = %name, identity = %identity_id, "Registering local agent");
        let tools = offered(&hello.id, &name, hello.tools);
        mark_read_only(&hello.id, &tools);
        let agent = ConnectedLocalAgent {
            name,
            capabilities: hello.capabilities,
            connected_at: Utc::now(),
            identity_id,
            connection,
            tools,
            request_tx,
            pending_exec: Mutex::default(),
            pending_tools: Mutex::default(),
        };
        if let Some(replaced) = agents.insert(hello.id.clone(), agent) {
            drop_pending(&hello.id, &replaced);
        }
        Ok(connection)
    }

    /// Deregister a node's `connection` (on disconnect). Its waiting calls
    /// fail at once. A connection that was since replaced removes nothing.
    pub async fn deregister(&self, id: &str, connection: u64) {
        let mut agents = self.agents.write().await;
        if agents.get(id).is_some_and(|a| a.connection == connection) {
            let agent = agents.remove(id).expect("just checked");
            mark_read_only(id, &[]);
            info!(agent_id = %id, name = %agent.name, "Deregistered local agent");
            drop_pending(id, &agent);
        }
    }

    /// Replace the tools a node's `connection` offers.
    pub async fn set_tools(&self, id: &str, connection: u64, tools: Vec<NodeToolSpec>) {
        let mut agents = self.agents.write().await;
        if let Some(agent) = agents.get_mut(id).filter(|a| a.connection == connection) {
            agent.tools = offered(id, &agent.name, tools);
            mark_read_only(id, &agent.tools);
            info!(agent_id = %id, tools = agent.tools.len(), "Local agent updated its tools");
        }
    }

    /// Whether `id` is connected and acts for `identity_id`.
    pub async fn is_bound_to(&self, id: &str, identity_id: &str) -> bool {
        self.agents
            .read()
            .await
            .get(id)
            .is_some_and(|a| a.bound_to(identity_id))
    }

    /// The nodes connected for `identity_id`.
    pub async fn list_for(&self, identity_id: &str) -> Vec<LocalAgentInfo> {
        self.agents
            .read()
            .await
            .iter()
            .filter(|(_, a)| a.bound_to(identity_id))
            .map(|(id, a)| a.info(id))
            .collect()
    }

    /// Every connected node.
    pub async fn list(&self) -> Vec<LocalAgentInfo> {
        self.agents
            .read()
            .await
            .iter()
            .map(|(id, a)| a.info(id))
            .collect()
    }

    /// The tools `identity_id`'s nodes offer.
    pub async fn tools_for(&self, identity_id: &str) -> Vec<OfferedTool> {
        self.agents
            .read()
            .await
            .values()
            .filter(|a| a.bound_to(identity_id))
            .flat_map(|a| a.tools.iter().cloned())
            .collect()
    }

    /// Send `caller`'s exec request to node `id`, returning a receiver for
    /// its result. Refused unless the node acts for `caller`.
    pub async fn send_exec(
        &self,
        id: &str,
        caller: &str,
        request: LocalExecRequest,
    ) -> Result<oneshot::Receiver<LocalExecResult>> {
        let agents = self.agents.read().await;
        let agent = bound(&agents, id, caller)?;
        let (tx, rx) = oneshot::channel();
        agent
            .pending_exec
            .lock()
            .insert(request.request_id.clone(), tx);
        send(id, agent, &request).await?;
        info!(
            agent_id = %id,
            request_id = %request.request_id,
            command = %request.command,
            "Sent exec request to local agent"
        );
        Ok(rx)
    }

    /// Tell node `id` to stop `request_id`, an exec call nobody waits for
    /// any more.
    pub async fn cancel_exec(&self, id: &str, request_id: &str) {
        let agents = self.agents.read().await;
        let Some(agent) = agents.get(id) else { return };
        agent.pending_exec.lock().remove(request_id);
        let cancel = LocalExecCancel {
            msg_type: "local_exec_cancel".to_string(),
            request_id: request_id.to_string(),
        };
        if let Err(e) = send(id, agent, &cancel).await {
            warn!(agent_id = %id, request_id, error = %e, "Could not cancel a local exec");
        }
    }

    /// Fulfil a pending exec result from node `id`'s `connection`.
    pub async fn fulfill_result(&self, id: &str, connection: u64, result: LocalExecResult) {
        let agents = self.agents.read().await;
        let sender = agents
            .get(id)
            .filter(|a| a.connection == connection)
            .and_then(|a| a.pending_exec.lock().remove(&result.request_id));
        match sender {
            Some(sender) => {
                let _ = sender.send(result);
            }
            None => {
                warn!(agent_id = %id, request_id = %result.request_id, "No pending request for exec result")
            }
        }
    }

    /// Send `caller`'s call of the node tool `tool` (`node.<id>.<tool>`),
    /// returning a receiver for its result. Refused unless the node acts for
    /// `caller` and offers the tool.
    pub async fn call_tool(
        &self,
        tool: &str,
        caller: &str,
        request_id: &str,
        arguments: serde_json::Value,
    ) -> Result<oneshot::Receiver<NodeToolResult>> {
        let id = node_of(tool).ok_or_else(|| eyre!("'{tool}' is not a node tool"))?;
        let agents = self.agents.read().await;
        let agent = bound(&agents, id, caller)?;
        let offered = agent
            .tools
            .iter()
            .find(|t| t.name == tool)
            .ok_or_else(|| eyre!("node '{id}' no longer offers '{tool}'"))?;
        let (tx, rx) = oneshot::channel();
        agent
            .pending_tools
            .lock()
            .insert(request_id.to_string(), tx);
        let call = NodeToolCall {
            msg_type: "node_tool_call".to_string(),
            request_id: request_id.to_string(),
            name: offered.spec.name.clone(),
            arguments,
        };
        send(id, agent, &call).await?;
        Ok(rx)
    }

    /// Tell the node behind `tool` to stop `request_id`, a call nobody waits
    /// for any more.
    pub async fn cancel_tool_call(&self, tool: &str, request_id: &str) {
        let Some(id) = node_of(tool) else { return };
        let agents = self.agents.read().await;
        let Some(agent) = agents.get(id) else { return };
        agent.pending_tools.lock().remove(request_id);
        let cancel = NodeToolCancel {
            msg_type: "node_tool_cancel".to_string(),
            request_id: request_id.to_string(),
        };
        if let Err(e) = send(id, agent, &cancel).await {
            warn!(agent_id = %id, request_id, error = %e, "Could not cancel a node tool call");
        }
    }

    /// Fulfil a pending tool call result from node `id`'s `connection`.
    pub async fn fulfill_tool_result(&self, id: &str, connection: u64, result: NodeToolResult) {
        let agents = self.agents.read().await;
        let sender = agents
            .get(id)
            .filter(|a| a.connection == connection)
            .and_then(|a| a.pending_tools.lock().remove(&result.request_id));
        match sender {
            Some(sender) => {
                let _ = sender.send(result);
            }
            None => {
                warn!(agent_id = %id, request_id = %result.request_id, "No pending call for node tool result")
            }
        }
    }
}

/// The node id a node tool's name carries.
fn node_of(tool: &str) -> Option<&str> {
    tool.strip_prefix(NODE_TOOL_PREFIX)?
        .split_once('.')
        .map(|(id, _)| id)
}

/// Node `id`, when it acts for `caller`.
fn bound<'a>(
    agents: &'a HashMap<String, ConnectedLocalAgent>,
    id: &str,
    caller: &str,
) -> Result<&'a ConnectedLocalAgent> {
    match agents.get(id) {
        Some(agent) if agent.bound_to(caller) => Ok(agent),
        Some(_) => bail!("local agent '{id}' acts for another person"),
        None => bail!("local agent '{id}' is not connected"),
    }
}

async fn send(id: &str, agent: &ConnectedLocalAgent, message: &impl Serialize) -> Result<()> {
    let json = serde_json::to_string(message)?;
    agent
        .request_tx
        .send(json)
        .await
        .map_err(|_| eyre!("local agent '{id}' disconnected"))
}

/// Fail every call waiting on `agent`: dropping a sender ends its receiver.
fn drop_pending(id: &str, agent: &ConnectedLocalAgent) {
    let count =
        agent.pending_exec.lock().drain().count() + agent.pending_tools.lock().drain().count();
    if count > 0 {
        warn!(agent_id = %id, pending_count = count, "Dropped pending calls for a disconnected local agent");
    }
}

/// The tools of `specs` node `id` may offer: each under its
/// [`tool_name`], within [`McpLimits`] and [`MAX_TOOL_NAME_LEN`], first of
/// any two names that collide.
fn offered(id: &str, node_name: &str, specs: Vec<NodeToolSpec>) -> Vec<OfferedTool> {
    let limits = McpLimits::default();
    let mut names = HashSet::new();
    specs
        .into_iter()
        .filter_map(|spec| {
            let name = tool_name(id, &spec.name);
            let admitted = limits
                .check_name(&spec.name)
                .and_then(|()| limits.check_description(&spec.name, &spec.description))
                .and_then(|()| limits.check_input_schema(&spec.name, &spec.input_schema))
                .map_err(|e| e.to_string())
                .and_then(|()| {
                    (name.len() <= MAX_TOOL_NAME_LEN)
                        .then_some(())
                        .ok_or_else(|| format!("'{name}' is longer than {MAX_TOOL_NAME_LEN}"))
                })
                .and_then(|()| {
                    names
                        .insert(name.clone())
                        .then_some(())
                        .ok_or_else(|| format!("'{name}' is offered twice"))
                });
            match admitted {
                Ok(()) => Some(OfferedTool {
                    node_name: node_name.to_string(),
                    name,
                    spec,
                }),
                Err(reason) => {
                    warn!(agent_id = %id, tool = %spec.name, %reason, "Local agent tool not offered");
                    None
                }
            }
        })
        .take(limits.max_tools_per_server)
        .collect()
}

/// Record which of node `id`'s tools only read, in place of what it said
/// before.
fn mark_read_only(id: &str, tools: &[OfferedTool]) {
    let prefix = format!("{NODE_TOOL_PREFIX}{id}.");
    let mut read_only = READ_ONLY_TOOLS.lock();
    read_only.retain(|name| !name.starts_with(&prefix));
    read_only.extend(
        tools
            .iter()
            .filter(|t| t.spec.read_only)
            .map(|t| t.name.clone()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, read_only: bool) -> NodeToolSpec {
        NodeToolSpec {
            name: name.to_string(),
            description: format!("{name} on the laptop"),
            input_schema: serde_json::json!({"type": "object"}),
            read_only,
        }
    }

    fn hello(id: &str, tools: Vec<NodeToolSpec>) -> NodeHello {
        NodeHello {
            id: id.to_string(),
            name: format!("{id}'s laptop"),
            capabilities: vec!["shell".into()],
            tools,
        }
    }

    async fn connect(
        registry: &LocalAgentRegistry,
        id: &str,
        person: &str,
        tools: Vec<NodeToolSpec>,
    ) -> (Result<u64, RegisterRefusal>, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel(8);
        let registered = registry
            .register(hello(id, tools), person.to_string(), tx)
            .await;
        (registered, rx)
    }

    #[tokio::test]
    async fn a_node_id_another_person_holds_is_refused_and_theirs_is_kept() {
        let registry = LocalAgentRegistry::new();
        let (alices, _rx) = connect(&registry, "studio", "alice", vec![]).await;
        alices.unwrap();
        let (bobs, _bob_rx) = connect(&registry, "studio", "bob", vec![]).await;
        assert_eq!(bobs, Err(RegisterRefusal::Taken("studio".into())));
        assert!(registry.is_bound_to("studio", "alice").await);
        assert!(!registry.is_bound_to("studio", "bob").await);
        assert!(registry.list_for("bob").await.is_empty());
    }

    #[tokio::test]
    async fn an_id_that_would_blur_the_tool_namespace_is_refused() {
        let registry = LocalAgentRegistry::new();
        for id in ["Studio", "studio.local", "my_mac", "", &"x".repeat(25)] {
            let (refused, _rx) = connect(&registry, id, "alice", vec![]).await;
            assert_eq!(refused, Err(RegisterRefusal::InvalidId(id.into())), "{id}");
        }
    }

    /// A person's node reconnecting replaces its earlier connection, whose
    /// late teardown must not remove the new one.
    #[tokio::test]
    async fn a_superseded_connection_leaves_its_successor_registered() {
        let registry = LocalAgentRegistry::new();
        let (first, _rx) = connect(&registry, "studio", "alice", vec![]).await;
        let (second, _rx2) = connect(&registry, "studio", "alice", vec![]).await;
        let (first, second) = (first.unwrap(), second.unwrap());
        assert_ne!(first, second);
        registry.deregister("studio", first).await;
        assert!(registry.is_bound_to("studio", "alice").await);
        registry.deregister("studio", second).await;
        assert!(registry.list().await.is_empty());
    }

    #[tokio::test]
    async fn only_the_bound_person_can_reach_a_node() {
        let registry = LocalAgentRegistry::new();
        let (_, _rx) = connect(&registry, "studio", "alice", vec![spec("desk_read", false)]).await;
        let exec = || LocalExecRequest {
            msg_type: "local_exec".into(),
            request_id: uuid::Uuid::new_v4().to_string(),
            command: "id".into(),
            workdir: None,
            timeout_secs: None,
        };
        let refused = registry
            .send_exec("studio", "bob", exec())
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("another person"), "{refused}");
        assert!(registry.send_exec("studio", "alice", exec()).await.is_ok());

        let tool = tool_name("studio", "desk_read");
        assert!(registry
            .call_tool(&tool, "bob", "r1", serde_json::json!({}))
            .await
            .is_err());
        assert!(registry.tools_for("bob").await.is_empty());
        assert_eq!(
            registry.tools_for("alice").await[0].name,
            "node.studio.desk_read"
        );
    }

    #[tokio::test]
    async fn a_call_waiting_on_a_node_fails_at_once_when_it_disconnects() {
        let registry = LocalAgentRegistry::new();
        let (connection, mut rx) =
            connect(&registry, "studio", "alice", vec![spec("desk_read", false)]).await;
        let waiting = registry
            .call_tool(
                "node.studio.desk_read",
                "alice",
                "r1",
                serde_json::json!({"a": 1}),
            )
            .await
            .unwrap();
        let sent: NodeToolCall = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(
            (sent.name.as_str(), sent.request_id.as_str()),
            ("desk_read", "r1")
        );
        registry.deregister("studio", connection.unwrap()).await;
        assert!(waiting.await.is_err(), "the sender was dropped");
    }

    #[tokio::test]
    async fn advertised_tools_are_named_bounded_and_classed() {
        let registry = LocalAgentRegistry::new();
        let tools = vec![
            spec("fs.read", true),
            spec("fs_read", false),
            spec("exec run", false),
            spec(&"t".repeat(60), false),
            spec("desk_sessions", false),
        ];
        let (connection, _rx) = connect(&registry, "classes", "alice", tools).await;
        let offered: Vec<String> = registry
            .tools_for("alice")
            .await
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(
            offered,
            ["node.classes.fs_read", "node.classes.desk_sessions"]
        );
        assert!(is_read_only_tool("node.classes.fs_read"));
        assert!(!is_read_only_tool("node.classes.desk_sessions"));

        let connection = connection.unwrap();
        registry
            .set_tools("classes", connection, vec![spec("fs_read", false)])
            .await;
        assert!(!is_read_only_tool("node.classes.fs_read"), "updated");
        registry
            .set_tools("classes", connection, vec![spec("fs_read", true)])
            .await;
        registry.deregister("classes", connection).await;
        assert!(
            !is_read_only_tool("node.classes.fs_read"),
            "gone with its node"
        );
    }
}
