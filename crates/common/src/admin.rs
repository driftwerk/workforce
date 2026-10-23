use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

/// System-wide dashboard view.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DashboardView {
    pub agents: AgentsSummary,
    pub teams: TeamsSummary,
    pub tasks: TasksSummary,
    pub nodes: NodesSummary,
    pub cost: CostSnapshot,
}

/// Agent summary for the dashboard.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentsSummary {
    pub total: usize,
    pub active: usize,
    pub idle: usize,
}

/// Team summary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TeamsSummary {
    pub total: usize,
    pub total_members: usize,
}

/// Task summary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TasksSummary {
    pub total: usize,
    pub in_progress: usize,
    pub completed: usize,
    pub failed: usize,
}

/// Node fleet summary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodesSummary {
    pub total: usize,
    pub online: usize,
    pub offline: usize,
}

/// Cost snapshot.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CostSnapshot {
    pub today_usd: f64,
    pub month_usd: f64,
    pub top_agent: Option<String>,
    pub top_model: Option<String>,
}

/// Audit log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub timestamp: String,
    pub actor: String,
    pub action: String,
    pub resource: String,
    pub detail: Option<String>,
    pub outcome: AuditOutcome,
}

/// Audit outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    Success,
    Denied,
    Error,
}

/// In-memory audit log.
pub struct AuditLog {
    entries: RwLock<Vec<AuditEntry>>,
    max_entries: usize,
}

impl AuditLog {
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: RwLock::new(Vec::new()),
            max_entries,
        }
    }

    /// Record an audit event.
    pub fn record(
        &self,
        actor: &str,
        action: &str,
        resource: &str,
        detail: Option<&str>,
        outcome: AuditOutcome,
    ) {
        let entry = AuditEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            actor: actor.to_string(),
            action: action.to_string(),
            resource: resource.to_string(),
            detail: detail.map(String::from),
            outcome,
        };

        let mut entries = self.entries.write();
        entries.push(entry);

        // Trim if over max
        if entries.len() > self.max_entries {
            let drain = entries.len() - self.max_entries;
            entries.drain(..drain);
        }
    }

    /// Query recent entries.
    pub fn recent(&self, limit: usize) -> Vec<AuditEntry> {
        let entries = self.entries.read();
        entries.iter().rev().take(limit).cloned().collect()
    }

    /// Search entries by actor or action.
    pub fn search(&self, query: &str, limit: usize) -> Vec<AuditEntry> {
        let query_lower = query.to_lowercase();
        let entries = self.entries.read();
        entries
            .iter()
            .rev()
            .filter(|e| {
                e.actor.to_lowercase().contains(&query_lower)
                    || e.action.to_lowercase().contains(&query_lower)
                    || e.resource.to_lowercase().contains(&query_lower)
            })
            .take(limit)
            .cloned()
            .collect()
    }

    /// Count total entries.
    pub fn count(&self) -> usize {
        self.entries.read().len()
    }
}

impl Default for AuditLog {
    fn default() -> Self {
        Self::new(10_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_serialization() {
        let dashboard = DashboardView {
            agents: AgentsSummary {
                total: 5,
                active: 3,
                idle: 2,
            },
            tasks: TasksSummary {
                total: 20,
                in_progress: 4,
                completed: 15,
                failed: 1,
            },
            ..Default::default()
        };

        let json = serde_json::to_string(&dashboard).unwrap();
        let parsed: DashboardView = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.agents.total, 5);
        assert_eq!(parsed.tasks.completed, 15);
    }

    #[test]
    fn audit_log_record_and_query() {
        let log = AuditLog::new(100);
        log.record(
            "dev-1",
            "tool.execute",
            "filesystem.write",
            Some("wrote /src/main.rs"),
            AuditOutcome::Success,
        );
        log.record(
            "dev-2",
            "tool.execute",
            "exec.run",
            Some("denied by policy"),
            AuditOutcome::Denied,
        );

        let recent = log.recent(10);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].actor, "dev-2"); // Most recent first
    }

    #[test]
    fn audit_log_search() {
        let log = AuditLog::new(100);
        log.record("alice", "login", "session", None, AuditOutcome::Success);
        log.record(
            "bob",
            "tool.execute",
            "filesystem.read",
            None,
            AuditOutcome::Success,
        );
        log.record(
            "alice",
            "tool.execute",
            "exec.run",
            None,
            AuditOutcome::Denied,
        );

        let results = log.search("alice", 10);
        assert_eq!(results.len(), 2);

        let results = log.search("exec", 10);
        assert_eq!(results.len(), 2); // Both tool.execute entries match
    }

    #[test]
    fn audit_log_max_entries() {
        let log = AuditLog::new(3);
        for i in 0..5 {
            log.record(
                &format!("agent-{}", i),
                "action",
                "resource",
                None,
                AuditOutcome::Success,
            );
        }
        assert_eq!(log.count(), 3);
        let recent = log.recent(10);
        assert_eq!(recent[0].actor, "agent-4"); // Most recent
    }
}
