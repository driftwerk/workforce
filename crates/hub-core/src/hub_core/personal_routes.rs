//! People's own routes on this hub: the hub's live `[llm.personal_routes]`
//! switch, and where each person's two settings are kept (every agent, and
//! one agent), with every change appended to an audit table. Kept in
//! Postgres; without it nobody has a setting and everyone stays on the
//! hub's route.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use eyre::Result;
use serde::Serialize;
use uuid::Uuid;
use workforce_common::personal_route::{
    chosen_route, AgentPersonalRoutes, AgentRoute, PersonalRoute, PersonalRoutesConfig,
};

#[cfg(feature = "postgres")]
pub mod postgres;

#[cfg(test)]
mod tests;

/// A person's two settings as they bear on one agent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RouteSettings {
    /// Their setting for every agent.
    pub every_agent: Option<PersonalRoute>,
    /// Their setting for this agent, which wins over the every-agent one.
    pub this_agent: Option<AgentRoute>,
}

impl RouteSettings {
    /// The route the person chose for this agent; `None` is the hub's.
    pub fn chosen(&self) -> Option<PersonalRoute> {
        chosen_route(self.every_agent, self.this_agent)
    }
}

/// One change of a person's setting, as the audit table keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteChange {
    /// `None` for the every-agent setting.
    pub agent_id: Option<String>,
    pub before: Option<AgentRoute>,
    pub after: Option<AgentRoute>,
    pub actor: String,
    pub at: DateTime<Utc>,
}

/// Where people's route settings are kept.
#[async_trait]
pub trait PersonRouteStore: Send + Sync {
    /// `person`'s settings, for `agent_id` when one is named.
    async fn settings(&self, person: Uuid, agent_id: Option<&str>) -> Result<RouteSettings>;
    /// Sets (or with `None`, clears) `person`'s every-agent setting, noting
    /// the change as made by `actor`; the setting it replaced.
    async fn set_every_agent(
        &self,
        person: Uuid,
        route: Option<PersonalRoute>,
        actor: &str,
    ) -> Result<Option<PersonalRoute>>;
    /// Sets (or with `None`, clears) `person`'s setting for `agent_id`,
    /// noting the change as made by `actor`; the setting it replaced.
    async fn set_for_agent(
        &self,
        person: Uuid,
        agent_id: &str,
        route: Option<AgentRoute>,
        actor: &str,
    ) -> Result<Option<AgentRoute>>;
    /// `person`'s latest changes, newest first, at most `limit`.
    async fn changes(&self, person: Uuid, limit: usize) -> Result<Vec<RouteChange>>;
}

/// The hub's people's-routes state every agent shares: the switch as the
/// config now says it, and the store once boot has opened it.
#[derive(Clone, Default)]
pub struct PersonalRoutes {
    switch: Arc<parking_lot::RwLock<PersonalRoutesConfig>>,
    store: Arc<parking_lot::RwLock<Option<Arc<dyn PersonRouteStore>>>>,
}

impl std::fmt::Debug for PersonalRoutes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersonalRoutes")
            .field("switch", &*self.switch.read())
            .field("store", &self.store.read().is_some())
            .finish()
    }
}

impl PersonalRoutes {
    pub fn new(switch: PersonalRoutesConfig) -> Self {
        Self {
            switch: Arc::new(parking_lot::RwLock::new(switch)),
            store: Default::default(),
        }
    }

    /// The hub's switch now.
    pub fn switch(&self) -> PersonalRoutesConfig {
        self.switch.read().clone()
    }

    /// The switch as it stands for an agent with `agent`'s own section.
    pub fn for_agent(&self, agent: Option<&AgentPersonalRoutes>) -> PersonalRoutesConfig {
        self.switch.read().for_agent(agent)
    }

    /// Replaces the switch, for every holder of this handle at once.
    pub fn set_switch(&self, switch: PersonalRoutesConfig) {
        let mut now = self.switch.write();
        if *now != switch {
            tracing::info!(
                enabled = switch.enabled,
                allow = ?switch.allow,
                "[llm.personal_routes] changed"
            );
            *now = switch;
        }
    }

    /// Keeps people's settings in `store` from now on.
    pub fn attach_store(&self, store: Arc<dyn PersonRouteStore>) {
        *self.store.write() = Some(store);
    }

    /// Where people's settings are kept; `None` without Postgres.
    pub fn store(&self) -> Option<Arc<dyn PersonRouteStore>> {
        self.store.read().clone()
    }
}

/// The install's route store; `None` without Postgres, or when it can't be
/// opened (said in the log).
pub async fn open(
    config: &workforce_common::settings::WorkforceConfig,
) -> Option<Arc<dyn PersonRouteStore>> {
    #[cfg(feature = "postgres")]
    if let Some(pg) = config.postgres.as_ref().filter(|p| p.enabled) {
        return match postgres::PostgresRouteStore::new(pg).await {
            Ok(store) => Some(Arc::new(store) as Arc<dyn PersonRouteStore>),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "people's route settings unavailable; everyone runs on the hub's route");
                None
            }
        };
    }
    let _ = config;
    None
}

/// People's settings in memory, for tests.
#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub struct MemoryRouteStore {
    inner: parking_lot::Mutex<MemoryRoutes>,
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
struct MemoryRoutes {
    every_agent: std::collections::HashMap<Uuid, PersonalRoute>,
    per_agent: std::collections::HashMap<(Uuid, String), AgentRoute>,
    changes: Vec<(Uuid, RouteChange)>,
}

#[cfg(any(test, feature = "test-support"))]
#[async_trait]
impl PersonRouteStore for MemoryRouteStore {
    async fn settings(&self, person: Uuid, agent_id: Option<&str>) -> Result<RouteSettings> {
        let inner = self.inner.lock();
        Ok(RouteSettings {
            every_agent: inner.every_agent.get(&person).copied(),
            this_agent: agent_id
                .and_then(|agent| inner.per_agent.get(&(person, agent.to_string())).copied()),
        })
    }

    async fn set_every_agent(
        &self,
        person: Uuid,
        route: Option<PersonalRoute>,
        actor: &str,
    ) -> Result<Option<PersonalRoute>> {
        let mut inner = self.inner.lock();
        let before = match route {
            Some(route) => inner.every_agent.insert(person, route),
            None => inner.every_agent.remove(&person),
        };
        inner.changes.push((
            person,
            RouteChange {
                agent_id: None,
                before: before.map(AgentRoute::Personal),
                after: route.map(AgentRoute::Personal),
                actor: actor.to_string(),
                at: Utc::now(),
            },
        ));
        Ok(before)
    }

    async fn set_for_agent(
        &self,
        person: Uuid,
        agent_id: &str,
        route: Option<AgentRoute>,
        actor: &str,
    ) -> Result<Option<AgentRoute>> {
        let mut inner = self.inner.lock();
        let key = (person, agent_id.to_string());
        let before = match route {
            Some(route) => inner.per_agent.insert(key, route),
            None => inner.per_agent.remove(&key),
        };
        inner.changes.push((
            person,
            RouteChange {
                agent_id: Some(agent_id.to_string()),
                before,
                after: route,
                actor: actor.to_string(),
                at: Utc::now(),
            },
        ));
        Ok(before)
    }

    async fn changes(&self, person: Uuid, limit: usize) -> Result<Vec<RouteChange>> {
        Ok(self
            .inner
            .lock()
            .changes
            .iter()
            .rev()
            .filter(|(who, _)| *who == person)
            .take(limit)
            .map(|(_, change)| change.clone())
            .collect())
    }
}
