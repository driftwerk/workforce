//! The person's budgets on Desk, as the hub's cost governor holds agents to
//! them: the desk says which budgets apply to an agent and keeps its
//! breaker; the governor counts the agent's use and refuses its next turn
//! while it is held. Installed by `[desk]` alone.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use workforce_common::telemetry::TelemetryStore;
use workforce_core::budget::{Budget, Standing};
use workforce_desk::budget_store::Used;
use workforce_desk::{AgentSpend, AgentVerdict, Desk};

use crate::agent_registry::AgentRegistry;
use crate::cost_governor::{Ceilings, Verdict};

/// The desk's budgets for the hub's agents.
pub struct DeskCeilings {
    pub desk: Desk,
    pub registry: Arc<AgentRegistry>,
}

impl Ceilings for DeskCeilings {
    fn budgets(&self, agent_id: &str) -> Vec<Budget> {
        self.desk.agent_budgets(agent_id)
    }

    fn held(&self, agent_id: &str) -> Option<String> {
        self.desk.agent_held(agent_id)
    }

    fn stand(&self, agent_id: &str, standings: Vec<Standing>) -> Verdict {
        let name = self
            .registry
            .get(agent_id)
            .map_or_else(|| agent_id.to_string(), |s| crate::desk_crew::name_of(&s));
        match self.desk.agent_stand(agent_id, &name, standings) {
            AgentVerdict::Proceed => Verdict::Proceed,
            AgentVerdict::Warn(w) => Verdict::Warn(w),
            AgentVerdict::Held(why) => Verdict::Held(why),
        }
    }
}

/// The hub's agents' use, every billing, for the desk's totals.
pub struct HubAgentSpend(pub Arc<dyn TelemetryStore>);

impl AgentSpend for HubAgentSpend {
    fn spent(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> eyre::Result<Vec<(String, Used)>> {
        Ok(self
            .0
            .agent_use(from, to)?
            .into_iter()
            .map(|u| {
                (
                    u.agent_id,
                    Used {
                        tokens: u.tokens,
                        usd: u.cost_usd,
                    },
                )
            })
            .collect())
    }
}
