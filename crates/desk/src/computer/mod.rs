//! What each agent may touch on this computer: the pure verdict, the
//! always-asks list for commands, and the kill switch, grants and activity
//! kept in Postgres. Nothing here is wired into a hub yet.

pub(crate) mod api;
pub mod asks;
pub mod boss;
pub mod on_its_own;
pub mod policy;
mod store;

#[cfg(test)]
mod policy_tests;

use std::sync::Arc;

use eyre::Result;
use workforce_core::computer::Verdict;

use crate::store::DeskStore;

pub use on_its_own::OnItsOwn;
pub use policy::{
    browser_site, verdict, AgentPolicy, Browser, Browsing, CallFacts, Connectors, Grant, Namespace,
    RunOf, NAMESPACES,
};
pub use store::{
    Activity, ActivityFilter, ActivityRow, KeptGrant, NewGrant, OnItsOwnRow, ACTIVITY_KEPT_DAYS,
    PAGE,
};

/// The verdict with this computer's kill switch, the agent's live grants
/// and what it does on its own read from Postgres. A read that fails is an error the caller
/// refuses on: nothing runs on a verdict it could not reach.
///
/// What was read is kept until the switch or the grants change through
/// this store, and for [`FRESH`] at most, so a grant's expiry is seen.
#[derive(Clone)]
pub struct Computer {
    store: Arc<DeskStore>,
    read: Arc<parking_lot::Mutex<Option<Read>>>,
}

/// How long what was read stands without a change.
pub const FRESH: std::time::Duration = std::time::Duration::from_secs(30);

struct Read {
    version: u64,
    at: std::time::Instant,
    stopped: bool,
    agents: std::collections::HashMap<(String, String), Agent>,
}

/// What was read of one agent.
struct Agent {
    grants: Vec<Grant>,
    on_its_own: OnItsOwn,
}

/// `policy` with what the agent does on its own as it was read.
fn decided(policy: &AgentPolicy, stopped: bool, agent: &Agent, call: &CallFacts) -> Verdict {
    let policy = AgentPolicy {
        on_its_own: agent.on_its_own.clone(),
        ..policy.clone()
    };
    verdict(&policy, stopped, &agent.grants, call)
}

impl Computer {
    pub fn new(store: Arc<DeskStore>) -> Self {
        Self {
            store,
            read: Default::default(),
        }
    }

    pub fn store(&self) -> &DeskStore {
        &self.store
    }

    pub async fn verdict(&self, policy: &AgentPolicy, call: &CallFacts) -> Result<Verdict> {
        let key = (policy.hub.clone(), policy.agent.clone());
        let version = self.store.access_version();
        {
            let read = self.read.lock();
            if let Some(r) = read
                .as_ref()
                .filter(|r| r.version == version && r.at.elapsed() < FRESH)
            {
                if let Some(agent) = r.agents.get(&key) {
                    return Ok(decided(policy, r.stopped, agent, call));
                }
            }
        }
        let stopped = self.store.access_stopped().await?;
        let agent = Agent {
            grants: self.store.live_grants(&policy.hub, &policy.agent).await?,
            on_its_own: self.store.on_its_own(&policy.hub, &policy.agent).await?,
        };
        let v = decided(policy, stopped, &agent, call);
        let mut read = self.read.lock();
        let fresh = read
            .as_ref()
            .is_some_and(|r| r.version == version && r.at.elapsed() < FRESH);
        if !fresh {
            *read = Some(Read {
                version,
                at: std::time::Instant::now(),
                stopped,
                agents: Default::default(),
            });
        }
        if let Some(r) = read.as_mut() {
            r.agents.insert(key, agent);
        }
        Ok(v)
    }
}
