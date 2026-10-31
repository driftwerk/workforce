//! Which of this hub's agents a remote MCP caller may reach: the agents a
//! chat over REST or the WebSocket would let them talk to, named exactly.
//!
//! Every remote route and `agent.*` tool that acts through an agent comes
//! here first, so `agent.list` shows exactly the agents `agent.chat` accepts.

use std::sync::Arc;

use tracing::info;
use workforce_common::settings::AccessDecision;

use crate::access_gate::AccessRequest;
use crate::agent_registry::canonical_agent_id;
use crate::gateway::{AgentViewer, CallerContext, GatewayState};
use crate::serve::AgentService;

/// Why a caller cannot reach the agent they named. Both read the same to the
/// caller, so an agent the caller may not use cannot be told from one that
/// does not exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unreachable {
    /// No agent on this hub has that id.
    Unknown(String),
    /// The agent exists, but its access policy (or the roster) does not
    /// admit the caller.
    Denied(String),
}

impl Unreachable {
    /// The agent id the caller asked for, or the default agent's.
    pub fn agent(&self) -> &str {
        match self {
            Self::Unknown(id) | Self::Denied(id) => id,
        }
    }
}

/// The agent `agent_id` names for `identity_id`, if they may reach it.
///
/// - A given id names one agent exactly, or ignoring case when only one
///   agent matches that way; a blank one means none was given. It never
///   falls back to the default agent: a mistyped or removed id would
///   otherwise send the message to an agent the caller never chose.
/// - With no id given, the default agent.
/// - Either way the agent's access policy must admit the caller, as a chat
///   over REST or the WebSocket requires, and the roster must show it to
///   them.
pub fn reachable(
    gateway: &GatewayState,
    identity_id: &str,
    agent_id: Option<&str>,
) -> Result<Arc<AgentService>, Unreachable> {
    let registry = &gateway.registry;
    let id = match agent_id.map(str::trim).filter(|id| !id.is_empty()) {
        Some(wanted) => {
            let live: Vec<String> = registry.agent_ids().iter().map(|a| a.to_string()).collect();
            canonical_agent_id(wanted, &live)
                .map(str::to_string)
                .ok_or_else(|| Unreachable::Unknown(wanted.to_string()))?
        }
        None => registry.default_id().to_string(),
    };
    let service = registry
        .get(&id)
        .ok_or_else(|| Unreachable::Unknown(id.clone()))?;
    if admits(gateway, &viewer(gateway, identity_id), identity_id, &id) {
        Ok(service)
    } else {
        info!(agent = %id, identity = %identity_id, "remote MCP: agent's access policy does not admit the caller");
        Err(Unreachable::Denied(id))
    }
}

/// The ids of every agent `identity_id` may reach, sorted.
pub fn reachable_ids(gateway: &GatewayState, identity_id: &str) -> Vec<String> {
    let viewer = viewer(gateway, identity_id);
    let mut ids: Vec<String> = gateway
        .registry
        .agent_ids()
        .iter()
        .map(|a| a.to_string())
        .filter(|id| admits(gateway, &viewer, identity_id, id))
        .collect();
    ids.sort();
    ids
}

/// The caller as the roster judges them: a person, never the gateway's
/// admin, whatever their role.
fn viewer<'a>(gateway: &'a GatewayState, identity_id: &str) -> AgentViewer<'a> {
    AgentViewer::new(
        gateway,
        CallerContext {
            identity_id: Some(identity_id.to_string()),
            ..Default::default()
        },
    )
}

/// Whether the agent `agent_id`'s access policy admits `identity_id` and the
/// roster shows it to them.
fn admits(
    gateway: &GatewayState,
    viewer: &AgentViewer<'_>,
    identity_id: &str,
    agent_id: &str,
) -> bool {
    let policy = gateway.access_gate.check(AccessRequest {
        agent_id,
        identity_id: Some(identity_id),
        surface_ids: &[],
        is_external: false,
    });
    matches!(policy, AccessDecision::Allow) && viewer.can_see(agent_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_mcp::testing::{fixture, with_agents};
    use workforce_common::settings::{AccessMode, AgentAccessConfig};

    fn only(identity: &str) -> AgentAccessConfig {
        AgentAccessConfig {
            mode: AccessMode::Allowlist,
            allow: vec![identity.to_string()],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn an_agent_whose_policy_excludes_the_caller_is_unreachable() {
        let tmp = tempfile::tempdir().unwrap();
        let fx = fixture(true);
        with_agents(&fx, tmp.path(), &["default", "private"]).await;
        fx.state.access_gate.upsert("private".into(), only("alice"));

        assert!(reachable(&fx.state, "alice", Some("private")).is_ok());
        assert_eq!(
            reachable(&fx.state, "bob", Some("private")).err(),
            Some(Unreachable::Denied("private".into()))
        );
        assert_eq!(reachable_ids(&fx.state, "alice"), ["default", "private"]);
        assert_eq!(reachable_ids(&fx.state, "bob"), ["default"]);
    }

    #[tokio::test]
    async fn the_default_agent_is_held_to_its_policy_too() {
        let tmp = tempfile::tempdir().unwrap();
        let fx = fixture(true);
        with_agents(&fx, tmp.path(), &["default"]).await;
        fx.state.access_gate.upsert("default".into(), only("alice"));

        assert!(reachable(&fx.state, "alice", None).is_ok());
        assert_eq!(
            reachable(&fx.state, "bob", None).err(),
            Some(Unreachable::Denied("default".into()))
        );
    }

    #[tokio::test]
    async fn an_unknown_id_never_reaches_the_default_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let fx = fixture(true);
        with_agents(&fx, tmp.path(), &["default", "helper"]).await;

        for typo in ["helpr", "removed"] {
            assert_eq!(
                reachable(&fx.state, "alice", Some(typo)).err(),
                Some(Unreachable::Unknown(typo.into()))
            );
        }
        assert!(reachable(&fx.state, "alice", Some(" helper ")).is_ok());
        assert!(
            reachable(&fx.state, "alice", Some("HELPER")).is_ok(),
            "ids match ignoring case when only one agent does, as hand-offs do"
        );
        assert!(
            reachable(&fx.state, "alice", Some("")).is_ok(),
            "blank is none given"
        );
    }
}
