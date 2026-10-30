//! Local Agent Registry: the nodes whose connection this hub instance holds,
//! each bound to the person whose paired device opened it.
//!
//! A node is a laptop running `workforce local-agent connect` over
//! `/ws/agent`, which runs `exec.run` for its person's sessions and offers
//! the tools of an MCP server it tunnels as `node.<node id>.<tool>`; or a
//! Desk over its hub link, which keeps its calls across a reconnect. Only
//! the bound person's turns reach a node: every route to one names the
//! caller, and a caller who is not its person is refused here, whatever the
//! session says.
//!
//! A turn may run on another instance than the one holding its person's
//! node, so what nodes there are is read from [`NodePresence`], and with a
//! NATS bus every call goes over the [`NodeRelay`], this instance's own
//! included.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, OnceLock, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use eyre::Result;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, RwLock};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use workforce_core::computer::{DeskNode, OnlineDesk};
use workforce_gateway_client::protocol::{
    LocalExecCancel, LocalExecRequest, LocalExecResult, NodeToolCall, NodeToolCancel,
    NodeToolCollect, NodeToolProgress, NodeToolResult, NodeToolSpec, NodeToolStatus, NodeTools,
};
use workforce_mcp_client::McpLimits;

use super::node_relay::{
    valid_request_id, AgentPresence, Desks, NodePresence, NodeRelay, PresenceKind, PresentAgent,
};

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

/// How long a Desk's calls wait for it to come back after its link drops.
pub const RECONNECT_GRACE: Duration = Duration::from_secs(60);

/// How often calls held past their grace are let go.
const SWEEP_EVERY: Duration = Duration::from_secs(5);

/// How often presence hears that this instance still holds its nodes.
pub const PRESENCE_EVERY: Duration = Duration::from_secs(30);

/// How many of a call's events may wait for its caller.
pub const CALL_EVENTS: usize = 32;

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

/// The key a Desk node goes by: its `:` keeps it apart from every agent id.
pub fn desk_key(device_id: &str) -> String {
    format!("desk:{device_id}")
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

/// What a node is.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeKind {
    /// `workforce local-agent connect` over `/ws/agent`: `exec.run` and
    /// tunnelled tools.
    Agent {
        capabilities: Vec<String>,
        tools: Vec<OfferedTool>,
    },
    /// A Desk over its hub link: computer verbs, resumable.
    Desk { device_id: String, node: DeskNode },
}

/// A node as it introduced itself.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeHello {
    pub key: String,
    pub name: String,
    pub kind: NodeKind,
}

impl NodeHello {
    /// A local agent, offering those of `specs` it may.
    pub fn agent(
        id: String,
        name: String,
        capabilities: Vec<String>,
        specs: Vec<NodeToolSpec>,
    ) -> Self {
        let name = bounded_name(&name);
        let tools = offered(&id, &name, specs);
        Self {
            key: id,
            name,
            kind: NodeKind::Agent {
                capabilities,
                tools,
            },
        }
    }

    /// A Desk, keyed by its paired device.
    pub fn desk(device_id: String, node: DeskNode) -> Self {
        Self {
            key: desk_key(&device_id),
            name: bounded_name(&node.name),
            kind: NodeKind::Desk { device_id, node },
        }
    }
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
    #[error("'{0}' is not a paired device's id")]
    InvalidDevice(String),
    #[error("another person's node is connected as '{0}'; connect with a different `--id`")]
    Taken(String),
}

/// A node's connection, once registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registered {
    pub connection: u64,
    /// The calls this instance waits on the node for: those it held across
    /// a reconnect, and those it took up from another instance.
    pub awaiting: Vec<String>,
}

/// What happens to a call a node holds, as its caller hears it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeEvent {
    Status(NodeToolStatus),
    Progress(NodeToolProgress),
    /// The call's outcome; nothing follows it. An `exec.run`'s carries its
    /// [`LocalExecResult`] as `result`.
    Result(NodeToolResult),
    /// The node went away with the call, which may have run in part.
    Gone,
}

impl NodeEvent {
    /// The call it is about; `None` for one that came of no message.
    fn request_id(&self) -> Option<&str> {
        match self {
            Self::Status(status) => Some(&status.request_id),
            Self::Progress(progress) => Some(&progress.request_id),
            Self::Result(result) => Some(&result.request_id),
            Self::Gone => None,
        }
    }

    /// Whether nothing follows it.
    pub fn is_last(&self) -> bool {
        match self {
            Self::Status(_) | Self::Progress(_) => false,
            Self::Result(_) | Self::Gone => true,
        }
    }
}

/// Where a call's events go.
pub type CallSink = mpsc::Sender<NodeEvent>;

/// What a call asks of a node.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeCall {
    /// One of its tools, by the node's own name for it, or a Desk's verb.
    Tool(NodeToolCall),
    /// A shell command, on an `Agent` node (`exec.run`).
    Exec(LocalExecRequest),
}

impl NodeCall {
    pub fn tool(request_id: String, name: String, arguments: serde_json::Value) -> Self {
        Self::Tool(NodeToolCall {
            msg_type: "node_tool_call".to_string(),
            request_id,
            name,
            arguments,
        })
    }

    pub fn request_id(&self) -> &str {
        match self {
            Self::Tool(call) => &call.request_id,
            Self::Exec(request) => &request.request_id,
        }
    }

    fn kind(&self) -> CallKind {
        match self {
            Self::Tool(_) => CallKind::Tool,
            Self::Exec(_) => CallKind::Exec,
        }
    }

    fn frame(&self) -> serde_json::Result<String> {
        match self {
            Self::Tool(call) => serde_json::to_string(call),
            Self::Exec(request) => serde_json::to_string(request),
        }
    }
}

/// Why a call reached no node.
#[derive(Debug, thiserror::Error)]
pub enum CallError {
    #[error("node '{0}' is not connected")]
    Offline(String),
    #[error("{0}")]
    Refused(String),
    #[error("the node relay failed: {0}")]
    Relay(eyre::Report),
}

/// The wait for the first of `events` that ends its call: the outcome, or
/// `None` once the node went away with it.
pub async fn outcome(events: &mut mpsc::Receiver<NodeEvent>) -> Option<NodeToolResult> {
    while let Some(event) = events.recv().await {
        match event {
            NodeEvent::Result(result) => return Some(result),
            NodeEvent::Gone => return None,
            NodeEvent::Status(_) | NodeEvent::Progress(_) => {}
        }
    }
    None
}

#[derive(Debug, Clone, Copy)]
enum CallKind {
    Tool,
    Exec,
}

impl CallKind {
    fn cancel(self, request_id: &str) -> serde_json::Result<String> {
        let request_id = request_id.to_string();
        match self {
            Self::Tool => serde_json::to_string(&NodeToolCancel {
                msg_type: "node_tool_cancel".to_string(),
                request_id,
            }),
            Self::Exec => serde_json::to_string(&LocalExecCancel {
                msg_type: "local_exec_cancel".to_string(),
                request_id,
            }),
        }
    }
}

/// A call a node holds, and who waits for it.
struct Pending {
    sink: CallSink,
    kind: CallKind,
}

/// The relay's listener for one connection, stopped with it.
struct Listener(JoinHandle<()>);

impl Drop for Listener {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A connected node with its communication channels.
struct Node {
    name: String,
    kind: NodeKind,
    connected_at: DateTime<Utc>,
    /// The person this node acts for.
    identity_id: String,
    /// Which connection holds the key, so a superseded one's teardown
    /// leaves its successor alone.
    connection: u64,
    /// Send serialized JSON messages to this node's socket write loop.
    request_tx: mpsc::Sender<String>,
    pending: Mutex<HashMap<String, Pending>>,
    _listener: Option<Listener>,
}

impl Node {
    fn bound_to(&self, identity_id: &str) -> bool {
        self.identity_id == identity_id
    }

    fn presence(&self) -> PresenceKind {
        match &self.kind {
            NodeKind::Agent {
                capabilities,
                tools,
            } => PresenceKind::Agent(AgentPresence {
                name: self.name.clone(),
                capabilities: capabilities.clone(),
                tools: tools.iter().map(|t| t.spec.clone()).collect(),
                connected_at: self.connected_at,
            }),
            NodeKind::Desk { device_id, node } => PresenceKind::Desk {
                device_id: device_id.clone(),
                node: node.clone(),
            },
        }
    }

    /// The frame `call` goes to this node as.
    fn frame(&self, key: &str, call: &NodeCall) -> Result<String, CallError> {
        match (&self.kind, call) {
            (NodeKind::Agent { tools, .. }, NodeCall::Tool(tool))
                if !tools.iter().any(|t| t.spec.name == tool.name) =>
            {
                Err(CallError::Refused(format!(
                    "node '{key}' no longer offers '{}'",
                    tool.name
                )))
            }
            (NodeKind::Desk { .. }, NodeCall::Exec(_)) => Err(CallError::Refused(format!(
                "node '{key}' runs no shell commands"
            ))),
            (NodeKind::Agent { .. } | NodeKind::Desk { .. }, _) => {
                call.frame().map_err(|e| CallError::Relay(e.into()))
            }
        }
    }

    async fn send(&self, key: &str, frame: String) -> Result<(), CallError> {
        self.request_tx
            .send(frame)
            .await
            .map_err(|_| CallError::Offline(key.to_string()))
    }
}

/// A Desk's calls, kept while it may come back.
struct Held {
    identity_id: String,
    until: Instant,
    calls: HashMap<String, Pending>,
}

/// What this instance reaches other instances and presence through.
struct Links {
    relay: Option<NodeRelay>,
    presence: Option<Arc<dyn NodePresence>>,
}

/// Registry of the nodes connected to this instance.
pub struct LocalAgentRegistry {
    me: Weak<Self>,
    nodes: RwLock<HashMap<String, Node>>,
    held: Mutex<HashMap<String, Held>>,
    connections: AtomicU64,
    links: OnceLock<Links>,
}

impl LocalAgentRegistry {
    /// A registry for this instance alone, until [`attach`](Self::attach)ed.
    pub fn new() -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            nodes: RwLock::default(),
            held: Mutex::default(),
            connections: AtomicU64::default(),
            links: OnceLock::new(),
        })
    }

    /// Reach other instances over `nats` and keep presence in `presence`;
    /// without them this instance's own connections are all there is. Also
    /// starts letting go of calls held past their grace, and telling
    /// presence this instance still holds its nodes, until the registry is
    /// dropped.
    pub fn attach(
        &self,
        nats: Option<async_nats::Client>,
        presence: Option<Arc<dyn NodePresence>>,
    ) -> Option<JoinHandle<()>> {
        let links = Links {
            relay: nats.map(NodeRelay::new),
            presence,
        };
        if self.links.set(links).is_err() {
            warn!("The node registry was attached twice; keeping the first");
            return None;
        }
        Some(tokio::spawn(upkeep(self.me.clone())))
    }

    fn relay(&self) -> Option<&NodeRelay> {
        self.links.get().and_then(|links| links.relay.as_ref())
    }

    fn presence(&self) -> &dyn NodePresence {
        match self.links.get().and_then(|links| links.presence.as_deref()) {
            Some(presence) => presence,
            None => self,
        }
    }

    /// Register `hello`'s node for `identity_id`. The same person connecting
    /// again replaces their earlier connection: a local agent's waiting
    /// calls fail, a Desk's carry over to the new one, as do those it held
    /// since its link dropped. A Desk's `open` calls that no instance waits
    /// for here are taken up, so their events reach whichever instance does.
    /// Another person's key is refused.
    pub async fn register(
        &self,
        hello: NodeHello,
        identity_id: String,
        request_tx: mpsc::Sender<String>,
        open: Vec<String>,
    ) -> Result<Registered, RegisterRefusal> {
        check_key(&hello)?;
        let key = hello.key;
        let mut nodes = self.nodes.write().await;
        if nodes
            .get(&key)
            .is_some_and(|held| !held.bound_to(&identity_id))
        {
            return Err(RegisterRefusal::Taken(key));
        }
        let connection = self.connections.fetch_add(1, Ordering::Relaxed);
        let listener = self.listen(&key, connection).await;
        info!(node = %key, name = %hello.name, identity = %identity_id, "Registering node");
        let replaced = nodes.remove(&key);
        let calls = match &hello.kind {
            NodeKind::Agent { tools, .. } => {
                mark_read_only(&key, tools);
                if let Some(replaced) = replaced {
                    drop_pending(&key, replaced.pending.into_inner());
                }
                HashMap::new()
            }
            NodeKind::Desk { .. } => {
                let mut calls = self.reclaim(&key, &identity_id);
                calls.extend(replaced.into_iter().flat_map(|n| n.pending.into_inner()));
                self.adopt(open, &mut calls);
                calls
            }
        };
        let mut awaiting: Vec<String> = calls.keys().cloned().collect();
        awaiting.sort();
        let node = Node {
            name: hello.name,
            kind: hello.kind,
            connected_at: Utc::now(),
            identity_id,
            connection,
            request_tx,
            pending: Mutex::new(calls),
            _listener: listener,
        };
        let present = (node.identity_id.clone(), node.presence());
        nodes.insert(key.clone(), node);
        drop(nodes);
        self.put_presence(&key, &present.0, &present.1).await;
        Ok(Registered {
            connection,
            awaiting,
        })
    }

    /// The relay's listener for `key`'s `connection`, with NATS.
    async fn listen(&self, key: &str, connection: u64) -> Option<Listener> {
        let relay = self.relay()?;
        match relay.listen(self.me.clone(), key, connection).await {
            Ok(task) => Some(Listener(task)),
            Err(e) => {
                warn!(node = %key, error = %e, "Listening for calls to a node failed");
                None
            }
        }
    }

    /// The calls held for `key` since its link dropped, when it is the same
    /// person's.
    fn reclaim(&self, key: &str, identity_id: &str) -> HashMap<String, Pending> {
        let mut held = self.held.lock();
        match held.get(key) {
            Some(h) if h.identity_id == identity_id => {
                held.remove(key).map(|h| h.calls).unwrap_or_default()
            }
            _ => HashMap::new(),
        }
    }

    /// Take up the `open` calls nobody here waits for: their events are
    /// published for the instance that does. Only a relay can reach it.
    fn adopt(&self, open: Vec<String>, calls: &mut HashMap<String, Pending>) {
        let Some(relay) = self.relay() else { return };
        for request_id in open {
            if calls.contains_key(&request_id) {
                continue;
            }
            match relay.sink(&request_id) {
                Ok(sink) => {
                    calls.insert(
                        request_id,
                        Pending {
                            sink,
                            kind: CallKind::Tool,
                        },
                    );
                }
                Err(e) => warn!(request_id, error = %e, "A node's open call was not taken up"),
            }
        }
    }

    /// Deregister a node's `connection` (on disconnect). A local agent's
    /// waiting calls fail at once; a Desk's are held for
    /// [`RECONNECT_GRACE`]. A connection that was since replaced removes
    /// nothing.
    pub async fn deregister(&self, key: &str, connection: u64) {
        let removed = {
            let mut nodes = self.nodes.write().await;
            match nodes.get(key) {
                Some(node) if node.connection == connection => nodes.remove(key),
                _ => None,
            }
        };
        let Some(node) = removed else { return };
        info!(node = %key, name = %node.name, "Deregistered node");
        let calls = node.pending.into_inner();
        match node.kind {
            NodeKind::Agent { .. } => {
                mark_read_only(key, &[]);
                drop_pending(key, calls);
            }
            NodeKind::Desk { .. } if calls.is_empty() => {}
            NodeKind::Desk { .. } => {
                self.held
                    .lock()
                    .entry(key.to_string())
                    .or_insert_with(|| Held {
                        identity_id: node.identity_id.clone(),
                        until: Instant::now() + RECONNECT_GRACE,
                        calls: HashMap::new(),
                    })
                    .calls
                    .extend(calls);
            }
        }
        if let Err(e) = self.presence().gone(key).await {
            warn!(node = %key, error = %e, "Recording a node offline failed");
        }
    }

    /// Let go of the calls held past their grace: silently when their Desk
    /// is online on another instance, which took them up; as gone otherwise.
    pub async fn sweep(&self, now: Instant) {
        let expired: Vec<(String, Held)> = {
            let mut held = self.held.lock();
            let keys: Vec<String> = held
                .iter()
                .filter(|(_, h)| h.until <= now)
                .map(|(key, _)| key.clone())
                .collect();
            keys.into_iter()
                .filter_map(|key| held.remove_entry(&key))
                .collect()
        };
        for (key, held) in expired {
            let elsewhere = match self.presence().desks_of(&held.identity_id).await {
                Ok(desks) => desks.online.iter().any(|desk| desk.key == key),
                Err(e) => {
                    warn!(node = %key, error = %e, "Presence unreadable; held calls end");
                    false
                }
            };
            match elsewhere {
                true => {
                    info!(node = %key, calls = held.calls.len(), "Held calls continue elsewhere")
                }
                false => drop_pending(&key, held.calls),
            }
        }
    }

    /// Tell presence again that this instance holds its nodes.
    async fn refresh(&self) {
        let present: Vec<(String, String, PresenceKind)> = self
            .nodes
            .read()
            .await
            .iter()
            .map(|(key, node)| (key.clone(), node.identity_id.clone(), node.presence()))
            .collect();
        for (key, identity, kind) in present {
            self.put_presence(&key, &identity, &kind).await;
        }
    }

    async fn put_presence(&self, key: &str, identity: &str, kind: &PresenceKind) {
        if let Err(e) = self.presence().put(key, identity, kind).await {
            warn!(node = %key, error = %e, "Recording a node online failed");
        }
    }

    /// Replace the tools a local agent's `connection` offers.
    pub async fn set_tools(&self, key: &str, connection: u64, specs: Vec<NodeToolSpec>) {
        self.update(key, connection, |name, kind| match kind {
            NodeKind::Agent { tools, .. } => {
                *tools = offered(key, name, specs);
                mark_read_only(key, tools);
                info!(node = %key, tools = tools.len(), "Local agent updated its tools");
            }
            NodeKind::Desk { .. } => warn!(node = %key, "A Desk sent tools; ignored"),
        })
        .await;
    }

    /// Replace what a Desk's `connection` said of itself.
    pub async fn set_desk(&self, key: &str, connection: u64, update: DeskNode) {
        self.update(key, connection, |name, kind| match kind {
            NodeKind::Desk { node, .. } => {
                *name = bounded_name(&update.name);
                *node = update;
            }
            NodeKind::Agent { .. } => {
                warn!(node = %key, "A local agent sent a Desk's update; ignored")
            }
        })
        .await;
    }

    async fn update(
        &self,
        key: &str,
        connection: u64,
        change: impl FnOnce(&mut String, &mut NodeKind),
    ) {
        let present = {
            let mut nodes = self.nodes.write().await;
            let Some(node) = nodes.get_mut(key).filter(|n| n.connection == connection) else {
                return;
            };
            change(&mut node.name, &mut node.kind);
            (node.identity_id.clone(), node.presence())
        };
        self.put_presence(key, &present.0, &present.1).await;
    }

    /// Whether this instance holds `key`'s `connection`.
    pub(super) async fn holds(&self, key: &str, connection: u64) -> bool {
        self.nodes
            .read()
            .await
            .get(key)
            .is_some_and(|node| node.connection == connection)
    }

    /// Send `caller`'s `call` to node `key`, wherever it is connected,
    /// returning its events. Refused unless the node acts for `caller`.
    pub async fn call(
        &self,
        key: &str,
        caller: &str,
        call: NodeCall,
    ) -> Result<mpsc::Receiver<NodeEvent>, CallError> {
        if let Some(relay) = self.relay() {
            return relay.call(key, caller, call).await;
        }
        let (sink, events) = mpsc::channel(CALL_EVENTS);
        self.accept(key, caller, call, sink).await?;
        Ok(events)
    }

    /// Wait anew for `caller`'s call `request_id` to Desk `key`, wherever it
    /// is: the Desk sends everything about it again. A local agent's calls
    /// end with its connection, so they can't be waited for anew.
    pub async fn collect(
        &self,
        key: &str,
        request_id: &str,
        caller: &str,
    ) -> Result<mpsc::Receiver<NodeEvent>, CallError> {
        if let Some(relay) = self.relay() {
            return relay.collect(key, request_id, caller).await;
        }
        let (sink, events) = mpsc::channel(CALL_EVENTS);
        self.reattach(key, request_id, caller, sink).await?;
        Ok(events)
    }

    /// Tell node `key` to stop `request_id`, a call nobody waits for any
    /// more.
    pub async fn cancel(&self, key: &str, request_id: &str) {
        match self.relay() {
            Some(relay) => relay.cancel(key, request_id).await,
            None => self.withdraw(key, request_id).await,
        }
    }

    /// Take `caller`'s `call` to node `key`, connected here, its events
    /// going to `sink`.
    pub(super) async fn accept(
        &self,
        key: &str,
        caller: &str,
        call: NodeCall,
        sink: CallSink,
    ) -> Result<(), CallError> {
        let nodes = self.nodes.read().await;
        let node = bound(&nodes, key, caller)?;
        let frame = node.frame(key, &call)?;
        let request_id = call.request_id().to_string();
        node.pending.lock().insert(
            request_id.clone(),
            Pending {
                sink,
                kind: call.kind(),
            },
        );
        if let Err(e) = node.send(key, frame).await {
            node.pending.lock().remove(&request_id);
            return Err(e);
        }
        info!(node = %key, request_id, "Sent a call to a node");
        Ok(())
    }

    /// Stop waiting for `request_id` on node `key`, connected or held here,
    /// and tell the node to stop it.
    pub(super) async fn withdraw(&self, key: &str, request_id: &str) {
        if let Some(held) = self.held.lock().get_mut(key) {
            held.calls.remove(request_id);
        }
        let nodes = self.nodes.read().await;
        let Some(node) = nodes.get(key) else { return };
        let pending = node.pending.lock().remove(request_id);
        let kind = match (pending, &node.kind) {
            (Some(pending), _) => pending.kind,
            // A Desk keeps a call nobody waits for here; it stops it too.
            (None, NodeKind::Desk { .. }) => CallKind::Tool,
            (None, NodeKind::Agent { .. }) => return,
        };
        let sent = match kind.cancel(request_id) {
            Ok(frame) => node.send(key, frame).await,
            Err(e) => Err(CallError::Relay(e.into())),
        };
        if let Err(e) = sent {
            warn!(node = %key, request_id, error = %e, "Could not cancel a node call");
        }
    }

    /// Send `request_id`'s events to `sink` from now on, and ask Desk `key`
    /// to send them again from the start.
    pub(super) async fn reattach(
        &self,
        key: &str,
        request_id: &str,
        caller: &str,
        sink: CallSink,
    ) -> Result<(), CallError> {
        let pending = Pending {
            sink,
            kind: CallKind::Tool,
        };
        let nodes = self.nodes.read().await;
        if !nodes.contains_key(key) {
            drop(nodes);
            return match self.held.lock().get_mut(key) {
                Some(held) if held.identity_id == caller => {
                    held.calls.insert(request_id.to_string(), pending);
                    Ok(())
                }
                _ => Err(CallError::Offline(key.to_string())),
            };
        }
        let node = bound(&nodes, key, caller)?;
        if let NodeKind::Agent { .. } = node.kind {
            return Err(CallError::Refused(format!(
                "node '{key}' ends its calls with its connection; this one can't be waited for again"
            )));
        }
        node.pending.lock().insert(request_id.to_string(), pending);
        let collect = NodeToolCollect {
            msg_type: "node_tool_collect".to_string(),
            request_id: request_id.to_string(),
        };
        let frame = serde_json::to_string(&collect).map_err(|e| CallError::Relay(e.into()))?;
        node.send(key, frame).await
    }

    /// Act on one message from node `key`'s `connection`.
    pub async fn receive_from_node(&self, key: &str, connection: u64, message: serde_json::Value) {
        fn parsed<T: serde::de::DeserializeOwned>(
            key: &str,
            kind: &str,
            message: serde_json::Value,
        ) -> Option<T> {
            serde_json::from_value(message)
                .inspect_err(|e| warn!(node = key, kind, "Failed to parse a node message: {e}"))
                .ok()
        }
        let kind = message
            .get("type")
            .and_then(|t| t.as_str())
            .map(str::to_owned);
        let event = match kind.as_deref() {
            Some(kind @ "local_exec_result") => {
                parsed::<LocalExecResult>(key, kind, message).map(|result| {
                    NodeEvent::Result(NodeToolResult {
                        msg_type: "node_tool_result".to_string(),
                        request_id: result.request_id.clone(),
                        result: serde_json::to_value(result).ok(),
                        error: None,
                    })
                })
            }
            Some(kind @ "node_tool_result") => parsed(key, kind, message).map(NodeEvent::Result),
            Some(kind @ "node_tool_status") => parsed(key, kind, message).map(NodeEvent::Status),
            Some(kind @ "node_tool_progress") => {
                parsed(key, kind, message).map(NodeEvent::Progress)
            }
            Some(kind @ "node_tools") => {
                if let Some(update) = parsed::<NodeTools>(key, kind, message) {
                    self.set_tools(key, connection, update.tools).await;
                }
                None
            }
            // Exec output is informational, and a pong is a keepalive.
            Some("local_exec_progress" | "pong") => None,
            Some(other) => {
                tracing::debug!(node = key, msg_type = %other, "Unknown message type from node");
                None
            }
            None => {
                warn!(node = key, "Node message missing 'type' field");
                None
            }
        };
        if let Some(event) = event {
            self.deliver(key, connection, event).await;
        }
    }

    /// Pass `event` on to whoever waits for its call on node `key`'s
    /// `connection`; an event that ends the call ends the wait.
    async fn deliver(&self, key: &str, connection: u64, event: NodeEvent) {
        let Some(request_id) = event.request_id().map(str::to_owned) else {
            return;
        };
        let sink = {
            let nodes = self.nodes.read().await;
            let node = nodes.get(key).filter(|n| n.connection == connection);
            node.and_then(|node| {
                let mut pending = node.pending.lock();
                match event.is_last() {
                    true => pending.remove(&request_id).map(|p| p.sink),
                    false => pending.get(&request_id).map(|p| p.sink.clone()),
                }
            })
        };
        match sink {
            Some(sink) => {
                let _ = sink.send(event).await;
            }
            None => warn!(node = %key, request_id, "No pending call for a node's event"),
        }
    }

    /// The local agents online for `identity_id`, wherever connected.
    pub async fn agents_of(&self, identity_id: &str) -> Vec<PresentAgent> {
        self.present_agents(Some(identity_id)).await
    }

    async fn present_agents(&self, identity_id: Option<&str>) -> Vec<PresentAgent> {
        let agents = match self.presence().agents(identity_id).await {
            Ok(agents) => agents,
            Err(e) => {
                warn!(error = %e, "Presence unreadable; listing this instance's nodes alone");
                self.local_agents(identity_id).await
            }
        };
        agents
            .iter()
            .for_each(|agent| mark_read_only(&agent.key, &agent.offered()));
        agents
    }

    /// Whether local agent `key` is online and acts for `identity_id`.
    pub async fn is_bound_to(&self, key: &str, identity_id: &str) -> bool {
        self.agents_of(identity_id)
            .await
            .iter()
            .any(|agent| agent.key == key)
    }

    /// The local agents online for `identity_id`.
    pub async fn list_for(&self, identity_id: &str) -> Vec<LocalAgentInfo> {
        self.agents_of(identity_id)
            .await
            .iter()
            .map(PresentAgent::info)
            .collect()
    }

    /// Every local agent online.
    pub async fn list(&self) -> Vec<LocalAgentInfo> {
        self.present_agents(None)
            .await
            .iter()
            .map(PresentAgent::info)
            .collect()
    }

    /// The tools `identity_id`'s local agents offer.
    pub async fn tools_for(&self, identity_id: &str) -> Vec<OfferedTool> {
        self.agents_of(identity_id)
            .await
            .iter()
            .flat_map(PresentAgent::offered)
            .collect()
    }

    /// `identity_id`'s Desks online, wherever connected.
    pub async fn desks_for(&self, identity_id: &str) -> Vec<OnlineDesk> {
        match self.presence().desks_of(identity_id).await {
            Ok(desks) => desks.online,
            Err(e) => {
                warn!(error = %e, "Presence unreadable; listing this instance's Desks alone");
                self.local_desks(identity_id).await.online
            }
        }
    }

    /// Forget a Desk whose device was unpaired.
    pub async fn forget_desk(&self, device_id: &str) {
        let key = desk_key(device_id);
        if let Err(e) = self.presence().forget(&key).await {
            warn!(node = %key, error = %e, "Forgetting an unpaired Desk failed");
        }
    }

    async fn local_agents(&self, identity_id: Option<&str>) -> Vec<PresentAgent> {
        self.nodes
            .read()
            .await
            .iter()
            .filter(|(_, node)| identity_id.is_none_or(|id| node.bound_to(id)))
            .filter_map(|(key, node)| match node.presence() {
                PresenceKind::Agent(agent) => Some(PresentAgent {
                    key: key.clone(),
                    identity_id: node.identity_id.clone(),
                    agent,
                }),
                PresenceKind::Desk { .. } => None,
            })
            .collect()
    }

    async fn local_desks(&self, identity_id: &str) -> Desks {
        let online: Vec<OnlineDesk> = self
            .nodes
            .read()
            .await
            .iter()
            .filter(|(_, node)| node.bound_to(identity_id))
            .filter_map(|(key, node)| match &node.kind {
                NodeKind::Desk { device_id, node } => Some(OnlineDesk {
                    key: key.clone(),
                    device_id: device_id.clone(),
                    node: node.clone(),
                }),
                NodeKind::Agent { .. } => None,
            })
            .collect();
        Desks {
            known: online.iter().map(|desk| desk.device_id.clone()).collect(),
            online,
            last_seen: None,
        }
    }
}

/// Without a presence store, the nodes this instance holds are all there
/// are.
#[async_trait]
impl NodePresence for LocalAgentRegistry {
    async fn put(&self, _key: &str, _identity: &str, _kind: &PresenceKind) -> Result<()> {
        Ok(())
    }

    async fn gone(&self, _key: &str) -> Result<()> {
        Ok(())
    }

    async fn forget(&self, _key: &str) -> Result<()> {
        Ok(())
    }

    async fn desks_of(&self, identity: &str) -> Result<Desks> {
        Ok(self.local_desks(identity).await)
    }

    async fn agents(&self, identity: Option<&str>) -> Result<Vec<PresentAgent>> {
        Ok(self.local_agents(identity).await)
    }
}

impl PresentAgent {
    /// Its tools, by the names its person's sessions see.
    pub fn offered(&self) -> Vec<OfferedTool> {
        offered(&self.key, &self.agent.name, self.agent.tools.clone())
    }

    fn info(&self) -> LocalAgentInfo {
        LocalAgentInfo {
            id: self.key.clone(),
            name: self.agent.name.clone(),
            capabilities: self.agent.capabilities.clone(),
            connected_at: self.agent.connected_at,
            tools: self.offered().into_iter().map(|t| t.name).collect(),
        }
    }
}

/// Let go of held calls past their grace, and keep presence current,
/// until the registry is gone.
async fn upkeep(registry: Weak<LocalAgentRegistry>) {
    let mut sweep = tokio::time::interval(SWEEP_EVERY);
    let mut refresh = tokio::time::interval(PRESENCE_EVERY);
    loop {
        tokio::select! {
            _ = sweep.tick() => match registry.upgrade() {
                Some(registry) => registry.sweep(Instant::now()).await,
                None => return,
            },
            _ = refresh.tick() => match registry.upgrade() {
                Some(registry) => registry.refresh().await,
                None => return,
            },
        }
    }
}

/// The node id a node tool's name carries.
pub fn node_of(tool: &str) -> Option<&str> {
    tool.strip_prefix(NODE_TOOL_PREFIX)?
        .split_once('.')
        .map(|(id, _)| id)
}

fn bounded_name(name: &str) -> String {
    name.chars().take(MAX_NODE_NAME_CHARS).collect()
}

fn check_key(hello: &NodeHello) -> Result<(), RegisterRefusal> {
    match &hello.kind {
        NodeKind::Agent { .. } if valid_node_id(&hello.key) => Ok(()),
        NodeKind::Agent { .. } => Err(RegisterRefusal::InvalidId(hello.key.clone())),
        NodeKind::Desk { device_id, .. }
            if valid_request_id(device_id) && hello.key == desk_key(device_id) =>
        {
            Ok(())
        }
        NodeKind::Desk { device_id, .. } => Err(RegisterRefusal::InvalidDevice(device_id.clone())),
    }
}

/// Node `key`, when it acts for `caller`.
fn bound<'a>(
    nodes: &'a HashMap<String, Node>,
    key: &str,
    caller: &str,
) -> Result<&'a Node, CallError> {
    match nodes.get(key) {
        Some(node) if node.bound_to(caller) => Ok(node),
        Some(_) => Err(CallError::Refused(format!(
            "node '{key}' acts for another person"
        ))),
        None => Err(CallError::Offline(key.to_string())),
    }
}

/// End every call in `calls` as gone with its node.
fn drop_pending(key: &str, calls: impl IntoIterator<Item = (String, Pending)>) {
    let count = calls
        .into_iter()
        .map(|(_, pending)| {
            let _ = pending.sink.try_send(NodeEvent::Gone);
        })
        .count();
    if count > 0 {
        warn!(node = %key, pending_count = count, "Dropped pending calls for a disconnected node");
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
    use serde_json::{json, Value};
    use workforce_core::computer::{Platform, Runtime};

    const WAIT: Duration = Duration::from_secs(10);

    fn spec(name: &str, read_only: bool) -> NodeToolSpec {
        NodeToolSpec {
            name: name.to_string(),
            description: format!("{name} on the laptop"),
            input_schema: json!({"type": "object"}),
            read_only,
        }
    }

    fn hello(id: &str, tools: Vec<NodeToolSpec>) -> NodeHello {
        NodeHello::agent(
            id.to_string(),
            format!("{id}'s laptop"),
            vec!["shell".into()],
            tools,
        )
    }

    async fn connect(
        registry: &LocalAgentRegistry,
        id: &str,
        person: &str,
        tools: Vec<NodeToolSpec>,
    ) -> (Result<Registered, RegisterRefusal>, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel(8);
        let registered = registry
            .register(hello(id, tools), person.to_string(), tx, vec![])
            .await;
        (registered, rx)
    }

    fn desk_node(name: &str) -> DeskNode {
        DeskNode {
            name: name.into(),
            platform: Platform::Macos,
            runtime: Runtime::Native,
            enabled: true,
            folders: vec!["~/dev/acme-web".into()],
            active_at: Utc::now(),
        }
    }

    fn new_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    async fn connect_desk(
        registry: &LocalAgentRegistry,
        device: &str,
        person: &str,
        open: Vec<String>,
    ) -> (Registered, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel(8);
        let registered = registry
            .register(
                NodeHello::desk(device.into(), desk_node("Sarah's MacBook Pro")),
                person.into(),
                tx,
                open,
            )
            .await
            .unwrap();
        (registered, rx)
    }

    /// The next frame the node is sent.
    async fn sent(rx: &mut mpsc::Receiver<String>) -> Value {
        let frame = tokio::time::timeout(WAIT, rx.recv())
            .await
            .expect("a frame in time")
            .expect("the node's channel is open");
        serde_json::from_str(&frame).unwrap()
    }

    async fn next_event(events: &mut mpsc::Receiver<NodeEvent>) -> Option<NodeEvent> {
        tokio::time::timeout(WAIT, events.recv())
            .await
            .expect("an event in time")
    }

    fn run(request_id: &str) -> NodeCall {
        NodeCall::tool(
            request_id.into(),
            "run".into(),
            json!({"command": "npm test"}),
        )
    }

    fn result(request_id: &str, text: &str) -> Value {
        json!({"type": "node_tool_result", "request_id": request_id, "result": {"text": text}})
    }

    fn result_text(event: Option<NodeEvent>) -> String {
        match event {
            Some(NodeEvent::Result(result)) => {
                result.result.unwrap()["text"].as_str().unwrap().to_string()
            }
            other => panic!("expected a result, got {other:?}"),
        }
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
        let (tx, _rx) = mpsc::channel(1);
        for device in ["d1", "a.b", "*"] {
            let refused = registry
                .register(
                    NodeHello::desk(device.into(), desk_node("Atlas")),
                    "alice".into(),
                    tx.clone(),
                    vec![],
                )
                .await;
            assert_eq!(
                refused,
                Err(RegisterRefusal::InvalidDevice(device.into())),
                "{device}"
            );
        }
    }

    /// A person's node reconnecting replaces its earlier connection, whose
    /// late teardown must not remove the new one.
    #[tokio::test]
    async fn a_superseded_connection_leaves_its_successor_registered() {
        let registry = LocalAgentRegistry::new();
        let (first, _rx) = connect(&registry, "studio", "alice", vec![]).await;
        let (second, _rx2) = connect(&registry, "studio", "alice", vec![]).await;
        let (first, second) = (first.unwrap().connection, second.unwrap().connection);
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
        let exec = || {
            NodeCall::Exec(LocalExecRequest {
                msg_type: "local_exec".into(),
                request_id: new_id(),
                command: "id".into(),
                workdir: None,
                timeout_secs: None,
            })
        };
        let refused = registry.call("studio", "bob", exec()).await.unwrap_err();
        assert!(refused.to_string().contains("another person"), "{refused}");
        assert!(registry.call("studio", "alice", exec()).await.is_ok());

        let tool = || NodeCall::tool(new_id(), "desk_read".into(), json!({}));
        assert!(registry.call("studio", "bob", tool()).await.is_err());
        let gone = NodeCall::tool(new_id(), "desk_write".into(), json!({}));
        let refused = registry.call("studio", "alice", gone).await.unwrap_err();
        assert!(
            refused.to_string().contains("no longer offers"),
            "{refused}"
        );
        assert!(registry.tools_for("bob").await.is_empty());
        assert_eq!(
            registry.tools_for("alice").await[0].name,
            "node.studio.desk_read"
        );
    }

    #[tokio::test]
    async fn a_call_waiting_on_a_node_fails_at_once_when_it_disconnects() {
        let registry = LocalAgentRegistry::new();
        let (registered, mut rx) =
            connect(&registry, "studio", "alice", vec![spec("desk_read", false)]).await;
        let mut waiting = registry
            .call(
                "studio",
                "alice",
                NodeCall::tool("r1".into(), "desk_read".into(), json!({"a": 1})),
            )
            .await
            .unwrap();
        let sent = sent(&mut rx).await;
        assert_eq!(
            (
                sent["type"].as_str(),
                sent["name"].as_str(),
                sent["request_id"].as_str()
            ),
            (Some("node_tool_call"), Some("desk_read"), Some("r1"))
        );
        registry
            .deregister("studio", registered.unwrap().connection)
            .await;
        assert!(matches!(
            next_event(&mut waiting).await,
            Some(NodeEvent::Gone)
        ));
        assert!(outcome(&mut waiting).await.is_none());
    }

    /// `exec.run` goes to a local agent as the frame it has always read, and
    /// its result comes back as the call's.
    #[tokio::test]
    async fn an_exec_runs_as_a_local_exec_and_its_result_comes_back() {
        let registry = LocalAgentRegistry::new();
        let (registered, mut rx) = connect(&registry, "studio", "alice", vec![]).await;
        let connection = registered.unwrap().connection;
        let request_id = new_id();
        let call = NodeCall::Exec(LocalExecRequest {
            msg_type: "local_exec".into(),
            request_id: request_id.clone(),
            command: "npm test".into(),
            workdir: Some("~/dev/acme-web".into()),
            timeout_secs: None,
        });
        let mut events = registry.call("studio", "alice", call).await.unwrap();
        let asked = sent(&mut rx).await;
        assert_eq!(
            (asked["type"].as_str(), asked["command"].as_str()),
            (Some("local_exec"), Some("npm test"))
        );
        registry
            .receive_from_node(
                "studio",
                connection,
                json!({"type": "local_exec_result", "request_id": request_id, "ok": true,
                       "stdout": "passed", "exit_code": 0}),
            )
            .await;
        let reply = outcome(&mut events).await.expect("a result");
        let ran: LocalExecResult = serde_json::from_value(reply.result.unwrap()).unwrap();
        assert!(ran.ok);
        assert_eq!(ran.stdout.as_deref(), Some("passed"));

        let request_id = new_id();
        let call = NodeCall::Exec(LocalExecRequest {
            msg_type: "local_exec".into(),
            request_id: request_id.clone(),
            command: "sleep 60".into(),
            workdir: None,
            timeout_secs: None,
        });
        let _events = registry.call("studio", "alice", call).await.unwrap();
        sent(&mut rx).await;
        registry.cancel("studio", &request_id).await;
        let cancel = sent(&mut rx).await;
        assert_eq!(
            (cancel["type"].as_str(), cancel["request_id"].as_str()),
            (Some("local_exec_cancel"), Some(request_id.as_str()))
        );
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
        let (registered, _rx) = connect(&registry, "classes", "alice", tools).await;
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

        let connection = registered.unwrap().connection;
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

    #[tokio::test]
    async fn a_desk_registers_updates_and_is_its_persons_alone() {
        let registry = LocalAgentRegistry::new();
        let device = new_id();
        let (first, _rx) = connect_desk(&registry, &device, "sarah", vec![]).await;
        assert!(first.awaiting.is_empty());
        let desks = registry.desks_for("sarah").await;
        assert_eq!(desks.len(), 1);
        assert_eq!(
            (desks[0].key.clone(), desks[0].device_id.clone()),
            (desk_key(&device), device.clone())
        );
        assert!(registry.desks_for("bob").await.is_empty());
        assert!(
            registry.list_for("sarah").await.is_empty(),
            "not a local agent"
        );

        let mut renamed = desk_node("Atlas");
        renamed.enabled = false;
        registry
            .set_desk(&desk_key(&device), first.connection, renamed.clone())
            .await;
        assert_eq!(registry.desks_for("sarah").await[0].node, renamed);

        let (second, _rx2) = connect_desk(&registry, &device, "sarah", vec![]).await;
        registry
            .deregister(&desk_key(&device), first.connection)
            .await;
        assert_eq!(registry.desks_for("sarah").await.len(), 1, "superseded");
        registry
            .deregister(&desk_key(&device), second.connection)
            .await;
        assert!(registry.desks_for("sarah").await.is_empty());
    }

    #[tokio::test]
    async fn a_desks_events_reach_its_caller_in_order_and_only_its_person_calls_it() {
        let registry = LocalAgentRegistry::new();
        let device = new_id();
        let key = desk_key(&device);
        let (registered, mut rx) = connect_desk(&registry, &device, "sarah", vec![]).await;
        let refused = registry
            .call(&key, "bob", run(&new_id()))
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("another person"), "{refused}");
        let exec = NodeCall::Exec(LocalExecRequest {
            msg_type: "local_exec".into(),
            request_id: new_id(),
            command: "id".into(),
            workdir: None,
            timeout_secs: None,
        });
        assert!(registry.call(&key, "sarah", exec).await.is_err());

        let request_id = new_id();
        let mut events = registry
            .call(&key, "sarah", run(&request_id))
            .await
            .unwrap();
        assert_eq!(sent(&mut rx).await["name"], "run");
        let connection = registered.connection;
        for frame in [
            json!({"type": "node_tool_status", "request_id": request_id, "state": "asking",
                   "ask": "Run npm test in acme-web?"}),
            json!({"type": "node_tool_status", "request_id": request_id, "state": "running"}),
            json!({"type": "node_tool_progress", "request_id": request_id, "line": "ok 1",
                   "lines": 1}),
            result(&request_id, "done"),
        ] {
            registry.receive_from_node(&key, connection, frame).await;
        }
        assert!(matches!(
            next_event(&mut events).await,
            Some(NodeEvent::Status(NodeToolStatus {
                state: workforce_gateway_client::protocol::NodeToolState::Asking,
                ask: Some(_),
                ..
            }))
        ));
        assert!(matches!(
            next_event(&mut events).await,
            Some(NodeEvent::Status(NodeToolStatus {
                state: workforce_gateway_client::protocol::NodeToolState::Running,
                ..
            }))
        ));
        assert!(matches!(
            next_event(&mut events).await,
            Some(NodeEvent::Progress(NodeToolProgress { lines: 1, .. }))
        ));
        assert_eq!(result_text(next_event(&mut events).await), "done");
        assert!(next_event(&mut events).await.is_none(), "nothing after it");
    }

    #[tokio::test]
    async fn a_desks_calls_wait_out_a_reconnect_inside_the_grace() {
        let registry = LocalAgentRegistry::new();
        let device = new_id();
        let key = desk_key(&device);
        let (first, mut rx) = connect_desk(&registry, &device, "sarah", vec![]).await;
        let request_id = new_id();
        let mut events = registry
            .call(&key, "sarah", run(&request_id))
            .await
            .unwrap();
        sent(&mut rx).await;
        registry.deregister(&key, first.connection).await;
        registry.sweep(Instant::now()).await;

        let (second, _rx) = connect_desk(&registry, &device, "sarah", vec![]).await;
        assert_eq!(second.awaiting, [request_id.as_str()]);
        registry
            .receive_from_node(&key, first.connection, result(&request_id, "stale"))
            .await;
        registry
            .receive_from_node(&key, second.connection, result(&request_id, "passed"))
            .await;
        assert_eq!(result_text(next_event(&mut events).await), "passed");
    }

    #[tokio::test]
    async fn past_the_grace_a_desks_calls_are_gone() {
        let registry = LocalAgentRegistry::new();
        let device = new_id();
        let key = desk_key(&device);
        let (registered, mut rx) = connect_desk(&registry, &device, "sarah", vec![]).await;
        let mut events = registry.call(&key, "sarah", run(&new_id())).await.unwrap();
        sent(&mut rx).await;
        registry.deregister(&key, registered.connection).await;
        registry
            .sweep(Instant::now() + RECONNECT_GRACE - Duration::from_secs(1))
            .await;
        registry.sweep(Instant::now() + RECONNECT_GRACE).await;
        assert!(matches!(
            next_event(&mut events).await,
            Some(NodeEvent::Gone)
        ));
        let (back, _rx) = connect_desk(&registry, &device, "sarah", vec![]).await;
        assert!(back.awaiting.is_empty());
    }

    #[tokio::test]
    async fn a_desks_call_is_waited_for_anew_and_cancelled_on_the_desk() {
        let registry = LocalAgentRegistry::new();
        let device = new_id();
        let key = desk_key(&device);
        let (registered, mut rx) = connect_desk(&registry, &device, "sarah", vec![]).await;
        let request_id = new_id();
        let _first = registry
            .call(&key, "sarah", run(&request_id))
            .await
            .unwrap();
        sent(&mut rx).await;

        assert!(registry.collect(&key, &request_id, "bob").await.is_err());
        let mut events = registry.collect(&key, &request_id, "sarah").await.unwrap();
        let collect = sent(&mut rx).await;
        assert_eq!(
            (collect["type"].as_str(), collect["request_id"].as_str()),
            (Some("node_tool_collect"), Some(request_id.as_str()))
        );
        registry
            .receive_from_node(&key, registered.connection, result(&request_id, "again"))
            .await;
        assert_eq!(result_text(next_event(&mut events).await), "again");

        // A call nobody here waits for is still the Desk's to stop.
        registry.cancel(&key, &new_id()).await;
        assert_eq!(sent(&mut rx).await["type"], "node_tool_cancel");

        let (agent, _rx) = connect(&registry, "studio", "sarah", vec![]).await;
        agent.unwrap();
        let refused = registry
            .collect("studio", &new_id(), "sarah")
            .await
            .unwrap_err();
        assert!(
            refused.to_string().contains("can't be waited for"),
            "{refused}"
        );
    }

    #[tokio::test]
    async fn a_held_call_is_waited_for_anew_while_its_desk_is_away() {
        let registry = LocalAgentRegistry::new();
        let device = new_id();
        let key = desk_key(&device);
        let (first, mut rx) = connect_desk(&registry, &device, "sarah", vec![]).await;
        let request_id = new_id();
        let _first = registry
            .call(&key, "sarah", run(&request_id))
            .await
            .unwrap();
        sent(&mut rx).await;
        registry.deregister(&key, first.connection).await;
        assert!(registry.collect(&key, &request_id, "bob").await.is_err());
        let mut events = registry.collect(&key, &request_id, "sarah").await.unwrap();
        let (second, _rx) = connect_desk(&registry, &device, "sarah", vec![]).await;
        assert_eq!(second.awaiting, [request_id.as_str()]);
        registry
            .receive_from_node(&key, second.connection, result(&request_id, "back"))
            .await;
        assert_eq!(result_text(next_event(&mut events).await), "back");
    }

    /// One node's row in [`SharedPresence`].
    struct Row {
        identity: String,
        kind: PresenceKind,
        online: bool,
        instance: &'static str,
    }

    /// Presence shared by instances, as Postgres keeps it: each marks
    /// offline only what it put.
    #[derive(Clone)]
    struct SharedPresence {
        rows: Arc<Mutex<HashMap<String, Row>>>,
        instance: &'static str,
    }

    impl SharedPresence {
        fn on(&self, instance: &'static str) -> Arc<dyn NodePresence> {
            Arc::new(Self {
                rows: Arc::clone(&self.rows),
                instance,
            })
        }

        fn online<T>(
            &self,
            identity: Option<&str>,
            of: impl Fn(&str, &Row) -> Option<T>,
        ) -> Vec<T> {
            self.rows
                .lock()
                .iter()
                .filter(|(_, row)| row.online && identity.is_none_or(|id| row.identity == id))
                .filter_map(|(key, row)| of(key, row))
                .collect()
        }
    }

    #[async_trait]
    impl NodePresence for SharedPresence {
        async fn put(&self, key: &str, identity: &str, kind: &PresenceKind) -> Result<()> {
            let row = Row {
                identity: identity.into(),
                kind: kind.clone(),
                online: true,
                instance: self.instance,
            };
            self.rows.lock().insert(key.into(), row);
            Ok(())
        }

        async fn gone(&self, key: &str) -> Result<()> {
            if let Some(row) = self.rows.lock().get_mut(key) {
                row.online &= row.instance != self.instance;
            }
            Ok(())
        }

        async fn forget(&self, key: &str) -> Result<()> {
            self.rows.lock().remove(key);
            Ok(())
        }

        async fn desks_of(&self, identity: &str) -> Result<Desks> {
            let online = self.online(Some(identity), |key, row| match &row.kind {
                PresenceKind::Desk { device_id, node } => Some(OnlineDesk {
                    key: key.into(),
                    device_id: device_id.clone(),
                    node: node.clone(),
                }),
                PresenceKind::Agent(_) => None,
            });
            Ok(Desks {
                online,
                ..Desks::default()
            })
        }

        async fn agents(&self, identity: Option<&str>) -> Result<Vec<PresentAgent>> {
            Ok(self.online(identity, |key, row| match &row.kind {
                PresenceKind::Agent(agent) => Some(PresentAgent {
                    key: key.into(),
                    identity_id: row.identity.clone(),
                    agent: agent.clone(),
                }),
                PresenceKind::Desk { .. } => None,
            }))
        }
    }

    /// Instances of one hub on a real NATS server, sharing presence:
    /// `WORKFORCE_TEST_NATS_URL=nats://localhost:4222`.
    async fn hub_on_nats(count: usize) -> Option<Vec<Arc<LocalAgentRegistry>>> {
        let url = std::env::var("WORKFORCE_TEST_NATS_URL").ok()?;
        let nats = async_nats::connect(url).await.expect("connect");
        workforce_nats::streams::StreamManager::new(async_nats::jetstream::new(nats.clone()), 1)
            .ensure_all_streams()
            .await
            .expect("streams");
        let presence = SharedPresence {
            rows: Arc::default(),
            instance: "",
        };
        let instances = ["hub-a", "hub-b", "hub-c"];
        Some(
            instances[..count]
                .iter()
                .map(|instance| {
                    let registry = LocalAgentRegistry::new();
                    registry.attach(Some(nats.clone()), Some(presence.on(instance)));
                    registry
                })
                .collect(),
        )
    }

    /// A node held by one instance is reached from another: the call is
    /// taken and its events stream back in order, a cancel and a collect
    /// reach the node, and a node nobody holds is offline.
    #[tokio::test]
    async fn a_node_held_by_another_instance_is_reached_over_nats() {
        let Some(hub) = hub_on_nats(2).await else {
            return;
        };
        let (holder, turn) = (&hub[0], &hub[1]);
        let device = new_id();
        let key = desk_key(&device);
        let (registered, mut rx) = connect_desk(holder, &device, "sarah", vec![]).await;
        assert_eq!(turn.desks_for("sarah").await.len(), 1, "presence is shared");

        let offline = turn
            .call(&desk_key(&new_id()), "sarah", run(&new_id()))
            .await
            .unwrap_err();
        assert!(matches!(offline, CallError::Offline(_)), "{offline}");
        let refused = turn.call(&key, "bob", run(&new_id())).await.unwrap_err();
        assert!(refused.to_string().contains("another person"), "{refused}");
        let bad_id = turn.call(&key, "sarah", run("r1")).await.unwrap_err();
        assert!(matches!(bad_id, CallError::Refused(_)), "{bad_id}");

        let request_id = new_id();
        let mut events = turn.call(&key, "sarah", run(&request_id)).await.unwrap();
        assert_eq!(sent(&mut rx).await["request_id"], request_id.as_str());
        for frame in [
            json!({"type": "node_tool_status", "request_id": request_id, "state": "running"}),
            json!({"type": "node_tool_progress", "request_id": request_id, "line": "ok 1",
                   "lines": 1}),
            result(&request_id, "passed"),
        ] {
            holder
                .receive_from_node(&key, registered.connection, frame)
                .await;
        }
        assert!(matches!(
            next_event(&mut events).await,
            Some(NodeEvent::Status(_))
        ));
        assert!(matches!(
            next_event(&mut events).await,
            Some(NodeEvent::Progress(_))
        ));
        assert_eq!(result_text(next_event(&mut events).await), "passed");

        let mut again = turn.collect(&key, &request_id, "sarah").await.unwrap();
        assert_eq!(sent(&mut rx).await["type"], "node_tool_collect");
        holder
            .receive_from_node(&key, registered.connection, result(&request_id, "again"))
            .await;
        assert_eq!(result_text(next_event(&mut again).await), "again");

        turn.cancel(&key, &request_id).await;
        let cancel = sent(&mut rx).await;
        assert_eq!(
            (cancel["type"].as_str(), cancel["request_id"].as_str()),
            (Some("node_tool_cancel"), Some(request_id.as_str()))
        );
    }

    /// A Desk whose link drops mid-call and comes back on a third instance
    /// keeps reporting to the turn waiting on another; the instance it left
    /// lets the call go without ending it.
    #[tokio::test]
    async fn a_desk_reconnecting_to_another_instance_keeps_reporting_to_the_turn() {
        let Some(hub) = hub_on_nats(3).await else {
            return;
        };
        let (first, turn, then) = (&hub[0], &hub[1], &hub[2]);
        let device = new_id();
        let key = desk_key(&device);
        let (left, mut rx) = connect_desk(first, &device, "sarah", vec![]).await;
        let request_id = new_id();
        let mut events = turn.call(&key, "sarah", run(&request_id)).await.unwrap();
        sent(&mut rx).await;
        first.deregister(&key, left.connection).await;

        let (back, _rx) = connect_desk(then, &device, "sarah", vec![request_id.clone()]).await;
        assert_eq!(back.awaiting, [request_id.as_str()]);
        then.receive_from_node(
            &key,
            back.connection,
            json!({"type": "node_tool_status", "request_id": request_id, "state": "running"}),
        )
        .await;
        assert!(matches!(
            next_event(&mut events).await,
            Some(NodeEvent::Status(_))
        ));
        first.sweep(Instant::now() + RECONNECT_GRACE).await;
        then.receive_from_node(&key, back.connection, result(&request_id, "passed"))
            .await;
        assert_eq!(
            result_text(next_event(&mut events).await),
            "passed",
            "no gone came first"
        );
    }

    /// Exec and tools of a local agent held by another instance.
    #[tokio::test]
    async fn a_local_agent_held_by_another_instance_runs_exec_and_offers_its_tools() {
        let Some(hub) = hub_on_nats(2).await else {
            return;
        };
        let (holder, turn) = (&hub[0], &hub[1]);
        let id = format!("n{}", &new_id()[..8]);
        let (registered, mut rx) =
            connect(holder, &id, "sarah", vec![spec("files_read", true)]).await;
        let connection = registered.unwrap().connection;
        let tools = turn.tools_for("sarah").await;
        let tool = tools
            .iter()
            .find(|t| t.name == tool_name(&id, "files_read"));
        assert!(tool.is_some(), "{tools:?}");
        assert!(is_read_only_tool(&tool_name(&id, "files_read")));
        assert!(turn.is_bound_to(&id, "sarah").await);

        let request_id = new_id();
        let call = NodeCall::Exec(LocalExecRequest {
            msg_type: "local_exec".into(),
            request_id: request_id.clone(),
            command: "id".into(),
            workdir: None,
            timeout_secs: None,
        });
        let mut events = turn.call(&id, "sarah", call).await.unwrap();
        assert_eq!(sent(&mut rx).await["type"], "local_exec");
        holder
            .receive_from_node(
                &id,
                connection,
                json!({"type": "local_exec_result", "request_id": request_id, "ok": true,
                       "stdout": "uid=501"}),
            )
            .await;
        let reply = outcome(&mut events).await.expect("a result");
        assert_eq!(reply.result.unwrap()["stdout"], "uid=501");

        let mut waiting = turn
            .call(
                &id,
                "sarah",
                NodeCall::tool(new_id(), "files_read".into(), json!({})),
            )
            .await
            .unwrap();
        sent(&mut rx).await;
        holder.deregister(&id, connection).await;
        assert!(outcome(&mut waiting).await.is_none(), "gone with its node");
        assert!(turn.tools_for("sarah").await.is_empty());
    }
}
