//! The one model call a monitor may make before its action: a yes/no put to
//! the install's decision backend about the event's text, as the owner.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::vault::Vault;
use async_trait::async_trait;
use workforce_common::session::TurnPrincipal;
use workforce_orchestrator::monitor::Decide;
use workforce_orchestrator::workflow::definition::{DecideKind, DecideSpec, NOUL_OPTIONS};
use workforce_orchestrator::workflow::engine::parse_outcome;

use crate::credentials::Caller;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Yes,
    /// Not to proceed, with why: the model said no, or could not settle.
    No(String),
    /// No answer; `busy` means asking again shortly may well get one.
    Undecided {
        reason: String,
        busy: bool,
    },
}

#[async_trait]
pub trait DecideGate: Send + Sync {
    /// Whether this install can answer at all, so a save never stores a gate
    /// that would silently pass.
    fn available(&self) -> bool;

    async fn decide(&self, owner: Option<&str>, decide: &Decide, state: &str) -> Verdict;
}

/// The production gate over [`crate::decide`], asked with the owner's keys.
pub struct DecisionGate {
    agent_vault: Option<Arc<Vault>>,
    global_vault: Option<Arc<Vault>>,
}

impl DecisionGate {
    pub fn new(agent_vault: Option<Arc<Vault>>, global_vault: Option<Arc<Vault>>) -> Self {
        Self {
            agent_vault,
            global_vault,
        }
    }
}

fn spec(decide: &Decide) -> DecideSpec {
    let options: BTreeMap<String, String> = [("yes", &decide.yes), ("no", &decide.no)]
        .into_iter()
        .filter_map(|(key, text)| text.clone().map(|text| (key.to_string(), text)))
        .collect();
    DecideSpec {
        question: decide.question.clone(),
        kind: DecideKind::Noul,
        options,
        levels: Vec::new(),
        min_probability: decide.min_probability,
    }
}

#[async_trait]
impl DecideGate for DecisionGate {
    fn available(&self) -> bool {
        crate::decide::decisions().is_some_and(|d| d.decision_steps_enabled())
    }

    async fn decide(&self, owner: Option<&str>, decide: &Decide, state: &str) -> Verdict {
        let principal = TurnPrincipal {
            run_as: owner.map(str::to_string),
            bill_to: None,
        };
        let branches: Vec<String> = NOUL_OPTIONS.iter().map(|o| o.to_string()).collect();
        let asked = crate::user_config::as_principal(&principal, async {
            let caller = Caller::in_scope(self.agent_vault.clone(), self.global_vault.clone());
            crate::decide::run_decision_step(&caller, &spec(decide), state, &branches, &[]).await
        })
        .await;
        match asked {
            Ok(decided) => match parse_outcome(&decided.response).as_deref() {
                Some("yes") => Verdict::Yes,
                Some("no") => Verdict::No(format!("{} answered no", decided.backend)),
                _ => Verdict::No(format!(
                    "{} could not settle on yes or no above the probability floor",
                    decided.backend
                )),
            },
            Err(undecided) => Verdict::Undecided {
                reason: undecided.reason,
                busy: undecided.busy,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decide_becomes_a_yes_no_spec_with_only_the_criteria_it_names() {
        let decide = Decide {
            question: "Is it an invoice?".into(),
            yes: Some("a bill with an amount due".into()),
            no: None,
            min_probability: Some(0.8),
        };
        let spec = spec(&decide);
        assert_eq!(spec.kind, DecideKind::Noul);
        assert_eq!(spec.question, "Is it an invoice?");
        assert_eq!(
            spec.options,
            BTreeMap::from([("yes".to_string(), "a bill with an amount due".to_string())])
        );
        assert_eq!(spec.min_probability, Some(0.8));
        assert!(spec.levels.is_empty());
    }

    #[test]
    fn the_production_gate_is_unavailable_until_the_install_decides() {
        assert!(!DecisionGate::new(None, None).available());
    }
}
