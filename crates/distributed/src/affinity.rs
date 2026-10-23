use crate::node::{Node, NodeCapability, NodeType};

/// Task affinity hints — tells the orchestrator where to run a task.
#[derive(Debug, Clone, Default)]
pub struct TaskAffinity {
    /// Required node capabilities.
    pub required_capabilities: Vec<NodeCapability>,
    /// Preferred node type.
    pub preferred_node_type: Option<NodeType>,
    /// Prefer a specific node by ID.
    pub preferred_node_id: Option<String>,
    /// Whether the task needs low-latency local access (IDE, filesystem).
    pub needs_local: bool,
    /// Whether the task is long-running (prefer remote nodes).
    pub long_running: bool,
    /// Maximum acceptable CPU load on target node.
    pub max_cpu_percent: Option<f32>,
}

/// Select the best node for a task based on affinity rules.
pub fn select_node<'a>(nodes: &'a [&'a Node], affinity: &TaskAffinity) -> Option<&'a Node> {
    if nodes.is_empty() {
        return None;
    }

    // Try preferred node first
    if let Some(ref preferred_id) = affinity.preferred_node_id {
        if let Some(node) = nodes
            .iter()
            .find(|n| n.id == *preferred_id && n.can_accept_work())
        {
            return Some(node);
        }
    }

    // Filter by required capabilities
    let mut candidates: Vec<&&Node> = nodes
        .iter()
        .filter(|n| n.can_accept_work())
        .filter(|n| {
            affinity
                .required_capabilities
                .iter()
                .all(|cap| n.has_capability(cap))
        })
        .collect();

    if candidates.is_empty() {
        return None;
    }

    // Filter by node type preference
    if let Some(ref preferred_type) = affinity.preferred_node_type {
        let typed: Vec<&&Node> = candidates
            .iter()
            .filter(|n| n.node_type == *preferred_type)
            .copied()
            .collect();
        if !typed.is_empty() {
            candidates = typed;
        }
    }

    // needs_local → prefer Local nodes
    if affinity.needs_local {
        let local: Vec<&&Node> = candidates
            .iter()
            .filter(|n| n.node_type == NodeType::Local)
            .copied()
            .collect();
        if !local.is_empty() {
            candidates = local;
        }
    }

    // long_running → prefer Remote nodes
    if affinity.long_running {
        let remote: Vec<&&Node> = candidates
            .iter()
            .filter(|n| n.node_type == NodeType::Remote || n.node_type == NodeType::Hub)
            .copied()
            .collect();
        if !remote.is_empty() {
            candidates = remote;
        }
    }

    // Filter by max CPU
    if let Some(max_cpu) = affinity.max_cpu_percent {
        let under_load: Vec<&&Node> = candidates
            .iter()
            .filter(|n| n.resources.cpu_percent <= max_cpu)
            .copied()
            .collect();
        if !under_load.is_empty() {
            candidates = under_load;
        }
    }

    // Score remaining candidates: prefer lowest load
    candidates.sort_by(|a, b| {
        let score_a = a.resources.cpu_percent + a.resources.active_tasks as f32 * 10.0;
        let score_b = b.resources.cpu_percent + b.resources.active_tasks as f32 * 10.0;
        score_a
            .partial_cmp(&score_b)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    candidates.first().copied().copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{NodeStatus, ResourceSnapshot};

    fn make_node(
        id: &str,
        node_type: NodeType,
        caps: Vec<NodeCapability>,
        cpu: f32,
        tasks: u32,
    ) -> Node {
        let mut node = Node::new(id, id, node_type, caps);
        node.resources = ResourceSnapshot {
            cpu_percent: cpu,
            active_tasks: tasks,
            ..Default::default()
        };
        node
    }

    #[test]
    fn select_preferred_node() {
        let hub = make_node(
            "hub",
            NodeType::Hub,
            vec![NodeCapability::AgentExecution],
            50.0,
            2,
        );
        let local = make_node(
            "local",
            NodeType::Local,
            vec![NodeCapability::AgentExecution],
            20.0,
            0,
        );
        let nodes = vec![&hub, &local];

        let affinity = TaskAffinity {
            preferred_node_id: Some("local".to_string()),
            ..Default::default()
        };

        let selected = select_node(&nodes, &affinity).unwrap();
        assert_eq!(selected.id, "local");
    }

    #[test]
    fn select_by_capability() {
        let hub = make_node(
            "hub",
            NodeType::Hub,
            vec![
                NodeCapability::AgentExecution,
                NodeCapability::DockerExecution,
            ],
            50.0,
            2,
        );
        let local = make_node(
            "local",
            NodeType::Local,
            vec![
                NodeCapability::AgentExecution,
                NodeCapability::LocalFilesystem,
            ],
            20.0,
            0,
        );
        let nodes = vec![&hub, &local];

        let affinity = TaskAffinity {
            required_capabilities: vec![NodeCapability::DockerExecution],
            ..Default::default()
        };

        let selected = select_node(&nodes, &affinity).unwrap();
        assert_eq!(selected.id, "hub");
    }

    #[test]
    fn prefer_local_for_interactive() {
        let hub = make_node(
            "hub",
            NodeType::Hub,
            vec![NodeCapability::AgentExecution],
            10.0,
            0,
        );
        let local = make_node(
            "local",
            NodeType::Local,
            vec![NodeCapability::AgentExecution],
            30.0,
            1,
        );
        let nodes = vec![&hub, &local];

        let affinity = TaskAffinity {
            needs_local: true,
            ..Default::default()
        };

        let selected = select_node(&nodes, &affinity).unwrap();
        assert_eq!(selected.id, "local");
    }

    #[test]
    fn prefer_remote_for_long_running() {
        let hub = make_node(
            "hub",
            NodeType::Hub,
            vec![NodeCapability::AgentExecution],
            30.0,
            1,
        );
        let local = make_node(
            "local",
            NodeType::Local,
            vec![NodeCapability::AgentExecution],
            10.0,
            0,
        );
        let remote = make_node(
            "remote",
            NodeType::Remote,
            vec![NodeCapability::AgentExecution],
            20.0,
            0,
        );
        let nodes = vec![&hub, &local, &remote];

        let affinity = TaskAffinity {
            long_running: true,
            ..Default::default()
        };

        let selected = select_node(&nodes, &affinity).unwrap();
        // Should pick remote (lower load than hub)
        assert_eq!(selected.id, "remote");
    }

    #[test]
    fn lowest_load_wins() {
        let n1 = make_node(
            "n1",
            NodeType::Remote,
            vec![NodeCapability::AgentExecution],
            80.0,
            3,
        );
        let n2 = make_node(
            "n2",
            NodeType::Remote,
            vec![NodeCapability::AgentExecution],
            20.0,
            0,
        );
        let n3 = make_node(
            "n3",
            NodeType::Remote,
            vec![NodeCapability::AgentExecution],
            50.0,
            1,
        );
        let nodes = vec![&n1, &n2, &n3];

        let selected = select_node(&nodes, &TaskAffinity::default()).unwrap();
        assert_eq!(selected.id, "n2");
    }

    #[test]
    fn skip_offline_nodes() {
        let mut node = make_node(
            "n1",
            NodeType::Remote,
            vec![NodeCapability::AgentExecution],
            10.0,
            0,
        );
        node.status = NodeStatus::Offline;
        let nodes = vec![&node];

        assert!(select_node(&nodes, &TaskAffinity::default()).is_none());
    }

    #[test]
    fn empty_nodes_returns_none() {
        assert!(select_node(&[], &TaskAffinity::default()).is_none());
    }
}
