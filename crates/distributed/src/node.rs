use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A node in the distributed workforce system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    /// Unique node identifier.
    pub id: String,
    /// Human-readable name (e.g., "kings-macbook", "swarm-worker-1").
    pub name: String,
    /// Node type.
    pub node_type: NodeType,
    /// What this node can do.
    pub capabilities: Vec<NodeCapability>,
    /// Current health status.
    pub status: NodeStatus,
    /// When the node was registered.
    pub registered_at: DateTime<Utc>,
    /// Last heartbeat received.
    pub last_heartbeat: DateTime<Utc>,
    /// Resource usage snapshot.
    pub resources: ResourceSnapshot,
    /// Trust level for this node.
    #[serde(default)]
    pub trust_level: TrustLevel,
    /// Filesystem paths this node allows agents to access.
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    /// Arbitrary metadata.
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

/// Node types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeType {
    /// Central hub with NATS server, Weaviate, orchestrator.
    Hub,
    /// Remote VPS or Docker Swarm worker.
    Remote,
    /// Developer workstation connected via NATS leaf node.
    Local,
}

/// What a node can do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeCapability {
    /// Can run agent workloads.
    AgentExecution,
    /// Has access to local filesystem (IDE, repos).
    LocalFilesystem,
    /// Can run Docker containers.
    DockerExecution,
    /// Has GPU for ML workloads.
    GpuCompute,
    /// Can access the internet.
    InternetAccess,
    /// Custom capability.
    Custom(String),
}

/// Node trust level — determines what the node is allowed to do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    /// Hub-managed infrastructure (VPS, Swarm). Full tool access per policy.
    Trusted,
    /// Registered and admin-approved local nodes. Tool access per policy, scoped filesystem.
    Verified,
    /// New/unverified nodes. Read-only, no tool execution until promoted.
    #[default]
    Untrusted,
}

impl TrustLevel {
    /// Whether this trust level allows tool execution.
    pub fn can_execute_tools(&self) -> bool {
        matches!(self, TrustLevel::Trusted | TrustLevel::Verified)
    }

    /// Whether this trust level allows write operations.
    pub fn can_write(&self) -> bool {
        matches!(self, TrustLevel::Trusted | TrustLevel::Verified)
    }
}

/// Node health status.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    /// Node is online and healthy.
    #[default]
    Online,
    /// Node is online but degraded (high load, low disk, etc.).
    Degraded,
    /// Node hasn't sent a heartbeat recently.
    Stale,
    /// Node is offline.
    Offline,
    /// Node is draining (finishing work, not accepting new tasks).
    Draining,
}

/// Resource usage snapshot.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceSnapshot {
    /// CPU usage percentage (0-100).
    pub cpu_percent: f32,
    /// Memory usage percentage (0-100).
    pub memory_percent: f32,
    /// Disk usage percentage (0-100).
    pub disk_percent: f32,
    /// Number of active agent tasks on this node.
    pub active_tasks: u32,
}

/// Heartbeat message sent by nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Heartbeat {
    pub node_id: String,
    pub status: NodeStatus,
    pub resources: ResourceSnapshot,
    pub timestamp: DateTime<Utc>,
}

impl Node {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        node_type: NodeType,
        capabilities: Vec<NodeCapability>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: id.into(),
            name: name.into(),
            node_type,
            capabilities,
            status: NodeStatus::Online,
            registered_at: now,
            last_heartbeat: now,
            resources: ResourceSnapshot::default(),
            trust_level: TrustLevel::default(),
            allowed_paths: Vec::new(),
            metadata: HashMap::new(),
        }
    }

    /// Update from a heartbeat.
    pub fn apply_heartbeat(&mut self, heartbeat: &Heartbeat) {
        self.status = heartbeat.status.clone();
        self.resources = heartbeat.resources.clone();
        self.last_heartbeat = heartbeat.timestamp;
    }

    /// Check if the node has a specific capability.
    pub fn has_capability(&self, cap: &NodeCapability) -> bool {
        self.capabilities.contains(cap)
    }

    /// Check if the node can accept new work.
    pub fn can_accept_work(&self) -> bool {
        matches!(self.status, NodeStatus::Online | NodeStatus::Degraded)
    }
}

/// Node registry — tracks all nodes in the distributed system.
pub struct NodeRegistry {
    nodes: HashMap<String, Node>,
    heartbeat_timeout_secs: u64,
}

impl NodeRegistry {
    pub fn new(heartbeat_timeout_secs: u64) -> Self {
        Self {
            nodes: HashMap::new(),
            heartbeat_timeout_secs,
        }
    }

    /// Register a new node.
    pub fn register(&mut self, node: Node) {
        tracing::info!(node_id = %node.id, name = %node.name, node_type = ?node.node_type, "Node registered");
        self.nodes.insert(node.id.clone(), node);
    }

    /// Register a node with pairing check.
    ///
    /// When node pairing is enabled and the node is not yet approved,
    /// the node is registered with `TrustLevel::Untrusted` (read-only,
    /// no tool execution) and a pairing request is created.
    /// Returns `Ok(Some(code))` if pairing is required, `Ok(None)` if
    /// the node was registered directly.
    pub fn register_with_pairing(
        &mut self,
        mut node: Node,
        pairing: &workforce_pairing::NodePairingService,
    ) -> eyre::Result<Option<String>> {
        if !pairing.is_enabled() {
            // Pairing disabled — register directly
            self.register(node);
            return Ok(None);
        }

        if pairing.is_node_approved(&node.id)? {
            // Already approved — register as Verified
            node.trust_level = TrustLevel::Verified;
            self.register(node);
            return Ok(None);
        }

        // Not approved — register as Untrusted and create pairing request
        node.trust_level = TrustLevel::Untrusted;
        let caps: Vec<String> = node.capabilities.iter().map(|c| format!("{c:?}")).collect();
        let req =
            pairing.request_join(&node.id, &node.name, &format!("{:?}", node.node_type), caps)?;
        self.register(node);
        Ok(Some(req.code))
    }

    /// Deregister a node.
    pub fn deregister(&mut self, node_id: &str) {
        self.nodes.remove(node_id);
    }

    /// Process a heartbeat.
    pub fn heartbeat(&mut self, heartbeat: Heartbeat) {
        if let Some(node) = self.nodes.get_mut(&heartbeat.node_id) {
            node.apply_heartbeat(&heartbeat);
        }
    }

    /// Get a node by ID.
    pub fn get(&self, node_id: &str) -> Option<&Node> {
        self.nodes.get(node_id)
    }

    /// List all nodes.
    pub fn list(&self) -> Vec<&Node> {
        self.nodes.values().collect()
    }

    /// List healthy nodes that can accept work.
    pub fn available_nodes(&self) -> Vec<&Node> {
        self.nodes
            .values()
            .filter(|n| n.can_accept_work())
            .collect()
    }

    /// Find nodes with a specific capability.
    pub fn nodes_with_capability(&self, cap: &NodeCapability) -> Vec<&Node> {
        self.available_nodes()
            .into_iter()
            .filter(|n| n.has_capability(cap))
            .collect()
    }

    /// Check for stale nodes and update their status.
    pub fn check_health(&mut self) {
        let now = Utc::now();
        let timeout = chrono::Duration::seconds(self.heartbeat_timeout_secs as i64);

        for node in self.nodes.values_mut() {
            if node.status != NodeStatus::Offline
                && node.status != NodeStatus::Stale
                && now - node.last_heartbeat > timeout
            {
                tracing::warn!(node_id = %node.id, "Node marked stale (missed heartbeat)");
                node.status = NodeStatus::Stale;
            }
        }
    }

    /// Count nodes.
    pub fn count(&self) -> usize {
        self.nodes.len()
    }

    /// Get summary stats.
    pub fn stats(&self) -> NodeStats {
        let mut stats = NodeStats::default();
        for node in self.nodes.values() {
            match node.status {
                NodeStatus::Online => stats.online += 1,
                NodeStatus::Degraded => stats.degraded += 1,
                NodeStatus::Stale => stats.stale += 1,
                NodeStatus::Offline => stats.offline += 1,
                NodeStatus::Draining => stats.draining += 1,
            }
        }
        stats.total = self.nodes.len();
        stats
    }
}

impl Default for NodeRegistry {
    fn default() -> Self {
        Self::new(60)
    }
}

/// Summary stats for the node fleet.
#[derive(Debug, Default)]
pub struct NodeStats {
    pub total: usize,
    pub online: usize,
    pub degraded: usize,
    pub stale: usize,
    pub offline: usize,
    pub draining: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub_node() -> Node {
        Node::new(
            "hub-1",
            "Main Hub",
            NodeType::Hub,
            vec![
                NodeCapability::AgentExecution,
                NodeCapability::DockerExecution,
                NodeCapability::InternetAccess,
            ],
        )
    }

    fn local_node() -> Node {
        Node::new(
            "local-1",
            "Kings Macbook",
            NodeType::Local,
            vec![
                NodeCapability::AgentExecution,
                NodeCapability::LocalFilesystem,
            ],
        )
    }

    #[test]
    fn register_and_list() {
        let mut reg = NodeRegistry::default();
        reg.register(hub_node());
        reg.register(local_node());

        assert_eq!(reg.count(), 2);
        assert_eq!(reg.available_nodes().len(), 2);
    }

    #[test]
    fn capability_query() {
        let mut reg = NodeRegistry::default();
        reg.register(hub_node());
        reg.register(local_node());

        let docker_nodes = reg.nodes_with_capability(&NodeCapability::DockerExecution);
        assert_eq!(docker_nodes.len(), 1);
        assert_eq!(docker_nodes[0].id, "hub-1");

        let local_nodes = reg.nodes_with_capability(&NodeCapability::LocalFilesystem);
        assert_eq!(local_nodes.len(), 1);
        assert_eq!(local_nodes[0].id, "local-1");
    }

    #[test]
    fn heartbeat_updates_node() {
        let mut reg = NodeRegistry::default();
        reg.register(hub_node());

        reg.heartbeat(Heartbeat {
            node_id: "hub-1".to_string(),
            status: NodeStatus::Degraded,
            resources: ResourceSnapshot {
                cpu_percent: 85.0,
                memory_percent: 70.0,
                disk_percent: 40.0,
                active_tasks: 3,
            },
            timestamp: Utc::now(),
        });

        let node = reg.get("hub-1").unwrap();
        assert_eq!(node.status, NodeStatus::Degraded);
        assert_eq!(node.resources.cpu_percent, 85.0);
        assert_eq!(node.resources.active_tasks, 3);
    }

    #[test]
    fn stale_detection() {
        let mut reg = NodeRegistry::new(60);
        let mut node = hub_node();
        node.last_heartbeat = Utc::now() - chrono::Duration::seconds(120);
        reg.register(node);

        reg.check_health();
        assert_eq!(reg.get("hub-1").unwrap().status, NodeStatus::Stale);
        assert_eq!(reg.available_nodes().len(), 0);
    }

    #[test]
    fn offline_node_not_available() {
        let mut reg = NodeRegistry::default();
        let mut node = hub_node();
        node.status = NodeStatus::Offline;
        reg.register(node);

        assert_eq!(reg.available_nodes().len(), 0);
    }

    #[test]
    fn stats() {
        let mut reg = NodeRegistry::default();
        reg.register(hub_node());

        let mut local = local_node();
        local.status = NodeStatus::Offline;
        reg.register(local);

        let stats = reg.stats();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.online, 1);
        assert_eq!(stats.offline, 1);
    }

    #[test]
    fn node_serialization() {
        let node = hub_node();
        let json = serde_json::to_string(&node).unwrap();
        let parsed: Node = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "hub-1");
        assert_eq!(parsed.capabilities.len(), 3);
    }
}
