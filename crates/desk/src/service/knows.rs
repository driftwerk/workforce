//! What each agent knows of the work ([`crate::knows`]): the work as a rule
//! reads it now, and the person's changes to a rule, which say what the
//! agent holds that the new rule no longer covers.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use super::{Desk, DeskError};
use crate::knows::{self, BoardFacts, HeldCount, Rule, World};
use crate::knows_store::KeptRule;

fn internal(e: eyre::Report) -> DeskError {
    DeskError::Internal(e)
}

/// A rule as the person's page reads it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleView {
    pub agent: String,
    pub places: Vec<String>,
    pub transcripts: bool,
    pub digest: &'static str,
    pub revision: i64,
    pub changed_at: Option<DateTime<Utc>>,
}

impl RuleView {
    fn of(agent: &str, kept: Option<KeptRule>) -> Self {
        match kept {
            Some(k) => Self {
                agent: k.agent,
                places: k.rule.texts(),
                transcripts: k.rule.transcripts,
                digest: k.rule.digest.as_str(),
                revision: k.revision,
                changed_at: Some(k.changed_at),
            },
            // Nothing set: its own work alone.
            None => Self {
                agent: agent.to_string(),
                places: Vec::new(),
                transcripts: false,
                digest: knows::Digest::Off.as_str(),
                revision: 0,
                changed_at: None,
            },
        }
    }
}

/// What a change of rule did: the rule now, and what the agent holds that
/// it no longer covers, for the person to delete or keep. Nothing more of
/// that is shared meanwhile: the rule now leaves it out.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleChanged {
    pub rule: RuleView,
    pub held_outside: HeldCount,
}

impl Desk {
    /// The work as a rule reads it now: the sidebar, each board and the
    /// sessions its tickets are assigned to, each grid's panes, and who
    /// runs which session.
    pub async fn knows_world(&self) -> Result<World, DeskError> {
        let store = self.store();
        let assigned = store.knows_assigned().await.map_err(internal)?;
        let mut by_board: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
        for (board, session) in assigned {
            by_board.entry(board).or_default().push(session);
        }
        let boards = store
            .boards()
            .await
            .map_err(internal)?
            .into_iter()
            .map(|b| BoardFacts {
                sessions: by_board.remove(&b.id).unwrap_or_default(),
                id: b.id,
                group: b.group,
            })
            .collect();
        let grids = store
            .list_state("grid.", true)
            .await
            .map_err(internal)?
            .into_iter()
            .filter(|l| l.key != "grid.index")
            .filter_map(|l| l.value.as_ref().and_then(knows::grid_facts))
            .collect();
        let runs = store
            .charges()
            .await
            .map_err(internal)?
            .into_iter()
            .filter(|c| c.released_at.is_none())
            .map(|c| (c.boss, c.session))
            .collect();
        Ok(World {
            layout: self.layout(),
            boards,
            grids,
            runs,
        })
    }

    /// Every agent's rule that has been set.
    pub async fn knows_rules(&self) -> Result<Vec<RuleView>, DeskError> {
        Ok(self
            .store()
            .knows_rules()
            .await
            .map_err(internal)?
            .into_iter()
            .map(|k| RuleView::of(&k.agent.clone(), Some(k)))
            .collect())
    }

    /// The agent `agent`'s rule: its own work alone until one is set.
    pub async fn knows_rule(&self, agent: &str) -> Result<RuleView, DeskError> {
        let kept = self.store().knows_rule(agent).await.map_err(internal)?;
        Ok(RuleView::of(agent, kept))
    }

    /// Keeps `rule` for `agent`, and says what the agent holds that it no
    /// longer covers.
    pub async fn knows_rule_set(&self, agent: &str, rule: &Rule) -> Result<RuleChanged, DeskError> {
        if !knows::agent_key_ok(agent) {
            return Err(DeskError::Invalid(format!(
                "{agent:?} isn't an agent: say <hub>/<agent>."
            )));
        }
        let kept = self
            .store()
            .knows_rule_put(agent, rule, Utc::now())
            .await
            .map_err(internal)?;
        let held_outside = self.knows_outside(agent, rule).await?.1;
        Ok(RuleChanged {
            rule: RuleView::of(agent, Some(kept)),
            held_outside,
        })
    }

    /// What `agent` holds that `rule` doesn't cover now, and its count.
    pub async fn knows_outside(
        &self,
        agent: &str,
        rule: &Rule,
    ) -> Result<(Vec<knows::Held>, HeldCount), DeskError> {
        let held = self.store().knows_held(agent).await.map_err(internal)?;
        let world = self.knows_world().await?;
        let covered = knows::resolve(agent, rule, &world);
        let outside: Vec<knows::Held> = knows::held_outside(&held, rule, &covered)
            .into_iter()
            .cloned()
            .collect();
        let count = knows::count(&outside.iter().collect::<Vec<_>>());
        Ok((outside, count))
    }

    /// The person's choice for what `agent` holds outside its rule now:
    /// delete it (done once the agent's hub has), or keep it. Answers what
    /// the choice covered.
    pub async fn knows_choose(&self, agent: &str, delete: bool) -> Result<HeldCount, DeskError> {
        let rule = self
            .store()
            .knows_rule(agent)
            .await
            .map_err(internal)?
            .map(|k| k.rule)
            .unwrap_or_default();
        let (outside, count) = self.knows_outside(agent, &rule).await?;
        let items: Vec<[&str; 2]> = outside
            .iter()
            .map(|h| [h.kind.as_str(), h.reference.as_str()])
            .collect();
        let detail = serde_json::json!({
            "items": count.items,
            "transcripts": count.transcripts,
            "sessions": count.sessions,
            "refs": if delete { serde_json::json!(items) } else { serde_json::Value::Null },
        });
        if count.items > 0 {
            self.store()
                .knows_choice(agent, delete, &detail, Utc::now())
                .await
                .map_err(internal)?;
        }
        Ok(count)
    }
}
