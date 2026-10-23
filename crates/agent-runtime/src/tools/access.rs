//! Per-user tool-visibility policy. Lets an external policy source
//! hide subsets of the registry from the active user without mutating
//! the process-wide [`super::registry::ToolRegistry`]. Used today by
//! the MCP shadow / built-in fallback decision in the hub crate; the
//! trait stays here so `AgentRunner` doesn't depend on hub.

use std::collections::HashMap;
use std::sync::Arc;

/// Frozen view of which tools are off-limits to the active user at a
/// single point in time. Computed once per LLM round so the tool list
/// sent to the LLM and the dispatch path that runs the chosen call
/// agree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolAccessSnapshot {
    blocked: HashMap<String, String>,
}

impl ToolAccessSnapshot {
    pub fn new(blocked: HashMap<String, String>) -> Self {
        Self { blocked }
    }

    /// Block-reason message if the tool is hidden for this user, else
    /// `None`. Returned text is surfaced verbatim to the LLM as the
    /// tool error so it can self-correct.
    pub fn block_reason(&self, internal_name: &str) -> Option<&str> {
        self.blocked.get(internal_name).map(String::as_str)
    }

    pub fn is_blocked(&self, internal_name: &str) -> bool {
        self.blocked.contains_key(internal_name)
    }

    pub fn len(&self) -> usize {
        self.blocked.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocked.is_empty()
    }

    pub fn blocked_names(&self) -> impl Iterator<Item = &str> {
        self.blocked.keys().map(String::as_str)
    }
}

/// Computes a fresh [`ToolAccessSnapshot`] for the active user.
/// Implementations should be cheap — called once per LLM round.
pub trait ToolAccess: Send + Sync {
    fn snapshot(&self) -> ToolAccessSnapshot;
}

/// Permissive default — nothing blocked, returned by the no-MCP path
/// and by every test that doesn't care about visibility.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopToolAccess;

impl ToolAccess for NoopToolAccess {
    fn snapshot(&self) -> ToolAccessSnapshot {
        ToolAccessSnapshot::default()
    }
}

pub fn noop() -> Arc<dyn ToolAccess> {
    Arc::new(NoopToolAccess)
}

/// Several independent visibility policies over one registry. Each contributes
/// its own blocked set; the first reason recorded for a tool wins, so a policy
/// added later can only widen the blocked set, never unblock.
pub struct CompositeToolAccess(Vec<Arc<dyn ToolAccess>>);

impl CompositeToolAccess {
    pub fn new(policies: Vec<Arc<dyn ToolAccess>>) -> Self {
        Self(policies)
    }
}

impl ToolAccess for CompositeToolAccess {
    fn snapshot(&self) -> ToolAccessSnapshot {
        ToolAccessSnapshot::new(self.0.iter().fold(HashMap::new(), |mut merged, policy| {
            for (name, reason) in policy.snapshot().blocked {
                merged.entry(name).or_insert(reason);
            }
            merged
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_snapshot_is_empty() {
        let snap = NoopToolAccess.snapshot();
        assert!(snap.is_empty());
        assert_eq!(snap.len(), 0);
        assert!(snap.block_reason("anything").is_none());
        assert!(!snap.is_blocked("anything"));
    }

    #[test]
    fn composite_merges_and_keeps_the_first_reason() {
        struct Fixed(&'static str, &'static str);
        impl ToolAccess for Fixed {
            fn snapshot(&self) -> ToolAccessSnapshot {
                ToolAccessSnapshot::new(HashMap::from([(self.0.to_string(), self.1.to_string())]))
            }
        }
        let composite = CompositeToolAccess::new(vec![
            Arc::new(Fixed("a", "first")),
            Arc::new(Fixed("a", "second")),
            Arc::new(Fixed("b", "other")),
        ]);
        let snap = composite.snapshot();
        assert_eq!(snap.block_reason("a"), Some("first"));
        assert_eq!(snap.block_reason("b"), Some("other"));
        assert_eq!(snap.len(), 2);
        assert!(CompositeToolAccess::new(vec![]).snapshot().is_empty());
    }

    #[test]
    fn snapshots_compare_by_blocked_set() {
        let blocked = |reason: &str| {
            ToolAccessSnapshot::new(HashMap::from([("a".to_string(), reason.to_string())]))
        };
        assert_eq!(blocked("shadowed"), blocked("shadowed"));
        assert_ne!(blocked("shadowed"), blocked("hidden"));
        assert_ne!(blocked("shadowed"), ToolAccessSnapshot::default());
    }

    #[test]
    fn snapshot_reports_block_reason() {
        let mut blocked = HashMap::new();
        blocked.insert(
            "linear_issue_create".to_string(),
            "shadowed by mcp.linear".to_string(),
        );
        let snap = ToolAccessSnapshot::new(blocked);
        assert_eq!(
            snap.block_reason("linear_issue_create"),
            Some("shadowed by mcp.linear")
        );
        assert!(snap.is_blocked("linear_issue_create"));
        assert!(snap.block_reason("filesystem_read").is_none());
        assert_eq!(snap.len(), 1);
    }
}
