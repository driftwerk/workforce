//! Act fully's directives on the hub ([`workforce_desk::directive`]): when a
//! boss here is at Act fully and wasn't before, it posts one card in its DM
//! asking the person's directives (Desk draws it from their live state);
//! an update going out at a quiet moment, at most once a day, carries one
//! line on what's still open; and `crew.directives` lets the person answer
//! them, add their own, or say to stop asking, in the chat. Their answers
//! are their preferences, so every turn is given them (as orders) by
//! [`CrewHub::preferences_context`]. Nothing waits on them.

use std::sync::Arc;

use chrono::Utc;
use serde_json::{json, Value};
use workforce_common::channels::desk::DeliveryKind;
use workforce_common::oversight::{Level, Oversight};
use workforce_desk::crew::Extras;
use workforce_desk::directive::{self, MentionedDirectives, MentionedNudge, DIRECTIVES};
use workforce_desk::preference::{self, PrefState};
use workforce_desk::rooms::YOU;
use workforce_desk::update::Pending;

use super::tools::Bound;
use super::CrewHub;
use crate::serve::AgentService;

pub const NAME: &str = "crew.directives";

pub const DESCRIPTION: &str = "The person's directives for you: their standing answers to a few questions (can you merge PRs, deploy to production, change data, delete things, spend, message people, add dependencies, when to interrupt them), plus any of their own. They are orders, and win over your own judgment. list (default) shows each with its answers; answer sets one (key, answer: its key or label; no answer clears it) when the person tells you in chat (\"you can merge once CI is green\"); add keeps one in their own words (text); forget drops one of their own (preference_id); stop_asking when they say to stop asking about unanswered ones, ask_again to start again. Confirm what you did in one line.";

pub fn schema() -> Value {
    json!({ "type": "object", "properties": {
        "action": { "type": "string", "enum": ["list", "answer", "add", "forget", "stop_asking", "ask_again"], "description": "list (default)" },
        "key": { "type": "string", "description": format!("answer: the directive, one of {}", DIRECTIVES.iter().map(|d| d.key).collect::<Vec<_>>().join(", ")) },
        "answer": { "type": "string", "description": "answer: the answer's key or label (list shows them); empty clears it" },
        "text": { "type": "string", "description": "add: the directive, in the person's words" },
        "preference_id": { "type": "string", "description": "forget: the id of one of their own" }
    }})
}

fn text(input: &Value, key: &str) -> Option<String> {
    input[key]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl CrewHub {
    fn directives_err(e: eyre::Report) -> String {
        tracing::warn!(error = %format!("{e:#}"), "the person's directives couldn't be read or kept");
        "Desk couldn't read or keep the person's directives just now.".to_string()
    }

    /// `crew.directives`, for the boss `b`.
    pub(super) async fn directives_tool(&self, b: &Bound, input: &Value) -> Result<Value, String> {
        let scope = preference::boss_scope_of(b.oversight.scope.group());
        let store = self.desk.store();
        let action = input["action"].as_str().map(str::trim).unwrap_or("list");
        let said = match action {
            "" | "list" => {
                let (v, _) = store
                    .directive_view(YOU, &scope)
                    .await
                    .map_err(Self::directives_err)?;
                let mut lines: Vec<String> = v
                    .questions
                    .iter()
                    .map(|q| {
                        let answers: Vec<String> = q
                            .answers
                            .iter()
                            .map(|a| format!("{} ({})", a.label, a.key))
                            .collect();
                        let now = match (&q.answer, q.inherited) {
                            (None, _) => "not answered".to_string(),
                            (Some(a), false) => format!("answered: {a}"),
                            (Some(a), true) => format!("answered for the main boss: {a}"),
                        };
                        format!(
                            "- {} [{}] {}: {}",
                            q.question,
                            q.key,
                            now,
                            answers.join(" / ")
                        )
                    })
                    .collect();
                for o in &v.own {
                    lines.push(format!("- Their own: {} (preference {})", o.text, o.id));
                }
                let asking = match v.stop_asking {
                    true => "They said to stop asking about unanswered ones.",
                    false => "Desk asks about unanswered ones at most once a day.",
                };
                return Ok(json!(super::watcher::reported(
                    &format!("{}\n{asking}", lines.join("\n")),
                    "the person's directives"
                )));
            }
            "answer" => {
                let key = text(input, "key").ok_or("Say which directive (key).")?;
                let answer = text(input, "answer");
                let kept = store
                    .directive_answer(YOU, &scope, &key, answer.as_deref())
                    .await
                    .map_err(Self::directives_err)??;
                match kept {
                    Some(p) => format!(
                        "Kept their directive: when {}, {}.",
                        p.when_text, p.then_text
                    ),
                    None => format!("Cleared their answer to {key}."),
                }
            }
            "add" => {
                let t = text(input, "text").ok_or("Say the directive (text).")?;
                let p = store
                    .directive_own(YOU, &scope, &t)
                    .await
                    .map_err(Self::directives_err)??;
                format!("Kept their own directive: {}.", p.then_text)
            }
            "forget" => {
                let id = text(input, "preference_id")
                    .and_then(|i| i.parse().ok())
                    .ok_or("Say which (preference_id: one of their own, from list).")?;
                let p = store
                    .directive_of(YOU, id)
                    .await
                    .map_err(Self::directives_err)?
                    .filter(|p| p.boss_scope == scope)
                    .ok_or("That isn't one of their directives for you.")?;
                store
                    .preference_set_state(p.id, PrefState::Retired)
                    .await
                    .map_err(Self::directives_err)?;
                format!("Forgot their directive: {}.", p.then_text)
            }
            "stop_asking" | "ask_again" => {
                let stop = action == "stop_asking";
                store
                    .directive_asking_change(YOU, &scope, None, Some(stop))
                    .await
                    .map_err(Self::directives_err)?;
                match stop {
                    true => "Desk won't ask about the unanswered ones again.".to_string(),
                    false => "Desk will ask about the unanswered ones again, at most once a day."
                        .to_string(),
                }
            }
            _ => {
                return Err("action is list, answer, add, forget, stop_asking or ask_again.".into())
            }
        };
        if action != "stop_asking" && action != "ask_again" {
            // Their directives are their standing orders.
            self.picture_order(&said, Self::current_turn());
        }
        Ok(json!(format!(
            "{said} Confirm it to the person in one line."
        )))
    }

    /// Looks at every boss's directives once: one that is at Act fully and
    /// wasn't before posts its card, when something is unanswered and the
    /// person hasn't said to stop asking; one that isn't is kept so, so Act
    /// fully turned on again asks again. A failure is logged and looked at
    /// again next minute.
    pub(crate) async fn look_at_directives(&self) {
        // The directives are the desk's person's, as their preferences are;
        // the DM a card goes to is the boss's with whoever it talks to.
        let mut off: Vec<String> = Vec::new();
        for (service, o) in self.overseers() {
            // Act fully as the person chose it, not as it is in force this
            // minute: a budget spent for the day, or not read yet after a
            // restart, holds it at Act for a while, and that is no turning
            // off and on again to ask afresh.
            let scope = preference::boss_scope_of(o.scope.group());
            if o.level != Level::Full {
                off.push(scope);
                continue;
            }
            let Some(owner) = self.person(Some(&service)) else {
                continue;
            };
            if let Err(e) = self.directives_of(&service, &owner, &scope).await {
                tracing::warn!(agent = %service.agent_id, error = %format!("{e:#}"), "a boss's directives couldn't be looked at");
            }
        }
        if let Err(e) = self.desk.store().directive_full_off(YOU, &off).await {
            tracing::warn!(error = %format!("{e:#}"), "the bosses off Act fully couldn't be kept so");
        }
    }

    async fn directives_of(
        &self,
        service: &Arc<AgentService>,
        owner: &str,
        scope: &str,
    ) -> eyre::Result<()> {
        let store = self.desk.store();
        let (v, _) = store.directive_view(YOU, scope).await?;
        let asking = store.directive_asking(YOU, scope).await?;
        if !directive::card_due(true, &asking, v.open().len()) {
            if asking.full_since.is_none() {
                store.directive_full_seen(YOU, scope).await?;
            }
            return Ok(());
        }
        let extras = Extras {
            directives: Some(MentionedDirectives {
                boss_scope: scope.to_string(),
            }),
            ..Default::default()
        };
        // One look posts it, however many look at once.
        if !store.directive_card_claim(YOU, scope).await? {
            return Ok(());
        }
        let delivery =
            crate::desk_surface::Delivery::text(DeliveryKind::Notice, directive::card_text(&v));
        let delivered = match self
            .deliver_with_extras(service, owner, delivery, &extras)
            .await
        {
            Ok(d) => d,
            Err(e) => {
                // Not out: the next look asks again.
                store.directive_full_off(YOU, &[scope.to_string()]).await?;
                return Err(e);
            }
        };
        store
            .directive_card_posted(YOU, scope, &delivered.message_id)
            .await?;
        tracing::info!(agent = %service.agent_id, scope, "a boss at Act fully asked the person's directives");
        Ok(())
    }

    /// The one line `p`, an update `boss` is about to send, may
    /// carry on the directives still open: at Act fully, a quiet update
    /// (nothing in it waits on the person, and no catch-up), and a day
    /// since the card or the last line. Kept as said by the caller, once
    /// the update is in the DM.
    pub(super) async fn directives_line(
        &self,
        boss: &str,
        o: &Oversight,
        p: &Pending,
    ) -> Option<MentionedNudge> {
        if self.level_now(boss, o).0 != Level::Full {
            return None;
        }
        let quiet =
            p.away.is_none() && p.update.needs_you.is_empty() && p.update.waiting.is_empty();
        let scope = preference::boss_scope_of(o.scope.group());
        let store = self.desk.store();
        let read = async {
            let (v, _) = store.directive_view(YOU, &scope).await?;
            let asking = store.directive_asking(YOU, &scope).await?;
            eyre::Ok((v, asking))
        };
        let (v, asking) = match read.await {
            Ok(r) => r,
            Err(e) => {
                tracing::info!(error = %format!("{e:#}"), "the directives still open couldn't be read for an update");
                return None;
            }
        };
        let open = v.open();
        if !directive::nudge_due(true, &asking, open.len(), quiet, Utc::now()) {
            return None;
        }
        Some(MentionedNudge {
            boss_scope: scope,
            open: open.len(),
            topics: open.iter().map(|q| q.topic.to_string()).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tool_names_every_directive() {
        let s = schema().to_string();
        for d in DIRECTIVES {
            assert!(s.contains(d.key), "{}", d.key);
        }
    }
}
