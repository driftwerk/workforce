//! Who an agent's background work runs as, and whose plan pays for it.

use workforce_common::session::TurnPrincipal;
use workforce_common::subscription::{agent_work_payer, background_payer, Payer};
use workforce_identity::types::{Identity, IdentityStatus};

use super::AgentService;

/// Whether `identity` is still a member of the install: not suspended.
pub fn is_member(identity: &Identity) -> bool {
    identity.status != IdentityStatus::Suspended
}

impl AgentService {
    /// The person whose plan pays for this agent's background work that runs
    /// as `runs_as`: the agent's owner, or the install's operator for the
    /// install's own work or an agent with no single owner. `None` when a
    /// person runs the work themselves or no one is set to pay. Budgets stay
    /// with `runs_as` ([`TurnPrincipal::billing`]).
    pub fn background_payer(&self, runs_as: Option<&str>) -> Option<uuid::Uuid> {
        background_payer(
            &Payer::classify(runs_as),
            &self.logins.owner(),
            self.config.subscription.operator,
        )
    }

    /// This agent's background work as `runs_as`, billed to the person it
    /// falls to when `runs_as` is not a person ([`Self::background_payer`]).
    pub fn background_principal(&self, runs_as: Option<&str>) -> TurnPrincipal {
        TurnPrincipal {
            run_as: runs_as.map(str::to_string),
            bill_to: self
                .background_payer(runs_as)
                .map(|person| person.to_string()),
        }
    }

    /// The person whose plan pays for this agent's own background work: its
    /// owner, else the install's operator.
    pub fn agent_work_payer(&self) -> Option<uuid::Uuid> {
        agent_work_payer(&self.logins.owner(), self.config.subscription.operator)
    }

    /// The suspended identity `principal` names, if it names one
    /// ([`crate::hub_core::standing::suspended`]).
    pub fn suspended(&self, principal: &str) -> Option<Identity> {
        crate::hub_core::standing::suspended(self.identity_store.read().as_ref(), principal)
    }

    /// Whether `person` has left the install or been suspended. Without an
    /// identity store there is no one to ask, and no one is taken to have
    /// left.
    pub fn has_left(&self, person: uuid::Uuid) -> bool {
        self.identity_store
            .read()
            .clone()
            .is_some_and(|store| match store.get_by_id(person) {
                Ok(identity) => !identity.as_ref().is_some_and(is_member),
                Err(e) => {
                    tracing::warn!(%person, error = %e, "Failed to look up whether a person has left");
                    false
                }
            })
    }
}
