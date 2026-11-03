//! What each agent may touch on this computer: the pure verdict, the
//! always-asks list for commands, and the kill switch, grants and activity
//! kept in Postgres. Nothing here is wired into a hub yet.

pub(crate) mod api;
pub mod asks;
pub mod policy;
mod store;

#[cfg(test)]
mod policy_tests;

use std::sync::Arc;

use eyre::Result;
use workforce_core::computer::Verdict;

use crate::store::DeskStore;

pub use policy::{
    verdict, AgentPolicy, CallFacts, Connectors, Grant, Namespace, RunOf, NAMESPACES,
};
pub use store::{
    Activity, ActivityFilter, ActivityRow, KeptGrant, NewGrant, ACTIVITY_KEPT_DAYS, PAGE,
};

/// The verdict with this computer's kill switch and the agent's live
/// grants read from Postgres. A read that fails is an error the caller
/// refuses on: nothing runs on a verdict it could not reach.
#[derive(Clone)]
pub struct Computer {
    store: Arc<DeskStore>,
}

impl Computer {
    pub fn new(store: Arc<DeskStore>) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &DeskStore {
        &self.store
    }

    pub async fn verdict(&self, policy: &AgentPolicy, call: &CallFacts) -> Result<Verdict> {
        let stopped = self.store.access_stopped().await?;
        let grants = self.store.live_grants(&policy.hub, &policy.agent).await?;
        Ok(verdict(policy, stopped, &grants, call))
    }
}
