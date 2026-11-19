//! What the person's monitors saw, told in Desk by their cadence
//! (`surface = "desk"`, [`crate::monitor::desk`]).
//!
//! - **Whose cadence.** The agent's own `[oversight.updates]` when it is a
//!   boss; else that of the boss over the agent's sidebar group (the main
//!   boss for an ungrouped agent): how the person said Desk should tell
//!   them things. With no boss at all, the Balanced preset.
//! - **When.** One for the DM is the person's own "tell me when", so it
//!   comes right away; a notice goes as progress does (the roundup, in
//!   Balanced). In the quiet hours either waits for the first roundup
//!   after them ([`cadence::monitor_route`]).
//! - **Away and typing.** What would come right away waits while the person
//!   is away from the DM (its posts unread `away_after_minutes`, as a
//!   boss's are judged), while they type there, or while the agent's reply
//!   there is being written; it goes once they are back.
//! - **Held is kept.** What waits is kept in Postgres (`monitor_held`)
//!   before the dispatch is done, so a restart loses none of it. When its
//!   time comes, everything due for one DM goes as one message: one finding
//!   in its own words, several under a line saying why they waited.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use uuid::Uuid;
use workforce_common::channels::desk::{DeliveryKind, DeskPlace, DeskTarget, DESK_CHANNEL};
use workforce_common::oversight::{Oversight, Route};
use workforce_desk::monitor_held_store::{HeldFinding, HeldFor};
use workforce_desk::update_cadence as cadence;
use workforce_orchestrator::monitor::DeskDelivery;

use super::CrewHub;
use crate::desk_surface::{Delivered, Delivery};
use crate::monitor::desk::Finding;
use crate::serve::AgentService;
use workforce_desk::crew::Crew as _;

/// How a monitor's finding was told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MonitorTold {
    /// In the DM now.
    Now(Delivered),
    /// Kept, to be told no sooner than `until`, once the person is back.
    Held { until: DateTime<Utc>, why: HeldFor },
}

impl CrewHub {
    /// The `[oversight]` whose cadence says when `service`'s monitors tell
    /// the person: its own as a boss, else its group's boss's, else none
    /// (Balanced).
    fn monitor_oversight(&self, service: &AgentService) -> Oversight {
        let agent = service.agent_id.as_str();
        if let Some((_, o)) = self.overseer_named(agent) {
            return o;
        }
        let group = self
            .desk
            .group_of_member(&workforce_desk::local_agent_item(agent));
        self.overseer_of_group(group.as_deref())
            .map(|(_, o)| o)
            .unwrap_or_default()
    }

    /// Tell `owner` what one of their monitors saw, in their DM with
    /// `service`'s agent: now, or held by their cadence (see the module).
    pub(crate) async fn monitor_told(
        &self,
        service: &Arc<AgentService>,
        owner: &str,
        finding: Finding,
    ) -> eyre::Result<MonitorTold> {
        let agent = service.agent_id.as_str();
        let o = self.monitor_oversight(service);
        let c = o.updates.cadence;
        let tz = self.timezone();
        let now = Utc::now();
        let notice = finding.place == DeskDelivery::Notice;
        let quiet = cadence::quiet_now(&c, now, &tz);
        let (why, until) = match cadence::monitor_route(notice, &c, quiet) {
            Route::RightAway => {
                let dm = self.dm_look(service, owner, &o);
                if dm.away.is_some() {
                    (HeldFor::Away, now)
                } else if self.update_held(agent, dm.session.as_deref()) {
                    (HeldFor::Typing, now)
                } else {
                    let told = self
                        .tell_monitor(service, owner, notice, finding.text)
                        .await?;
                    tracing::info!(agent, monitor = %finding.monitor_id, "a monitor told the person in Desk");
                    return Ok(MonitorTold::Now(told));
                }
            }
            Route::Roundup | Route::Ask => (
                if quiet {
                    HeldFor::Quiet
                } else {
                    HeldFor::Roundup
                },
                cadence::next_roundup(&c, now, &tz).unwrap_or(now),
            ),
        };
        self.desk
            .store()
            .monitor_hold(&HeldFinding {
                id: Uuid::new_v4(),
                agent: agent.to_string(),
                owner: owner.to_string(),
                monitor_id: finding.monitor_id.clone(),
                monitor_name: finding.monitor_name,
                notice,
                text: finding.text,
                reason: why,
                held_at: now,
                release_at: until,
            })
            .await?;
        tracing::info!(agent, monitor = %finding.monitor_id, why = why.as_str(), %until, "a monitor's finding is held for the person");
        // Typing or a reply being written ends within minutes: looked at
        // again as the updates are.
        if why == HeldFor::Typing {
            self.poke_at_release(agent);
        }
        Ok(MonitorTold::Held { until, why })
    }

    /// Puts `text` in `owner`'s DM with `service`'s agent: a message for
    /// them, or a notice.
    async fn tell_monitor(
        &self,
        service: &AgentService,
        owner: &str,
        notice: bool,
        text: String,
    ) -> eyre::Result<Delivered> {
        let kind = match notice {
            true => DeliveryKind::Notice,
            false => DeliveryKind::Monitor,
        };
        let dm = DeskTarget::of_turn(&DeskPlace::Dm { thread: None }, DESK_CHANNEL, None, false);
        crate::desk_surface::deliver(service, owner, &dm, Delivery::text(kind, text)).await
    }

    /// Tells what was held whose time has come, for each DM whose person
    /// is back: one message a DM and kind.
    pub(super) async fn release_monitor_holds(&self) {
        let now = Utc::now();
        let store = self.desk.store();
        let due = match store.monitor_held_due(now).await {
            Ok(due) => due,
            Err(e) => {
                tracing::info!(error = %format!("{e:#}"), "what monitors saw couldn't be read");
                return;
            }
        };
        let mut by_dm: BTreeMap<(String, String, bool), Vec<HeldFinding>> = BTreeMap::new();
        for held in due {
            by_dm
                .entry((held.agent.clone(), held.owner.clone(), held.notice))
                .or_default()
                .push(held);
        }
        let tz = self.timezone();
        for ((agent, owner, notice), held) in by_dm {
            let Some(service) = self.state.registry.get(&agent) else {
                // Its agent is gone, and its DM with it.
                tracing::warn!(%agent, n = held.len(), "what monitors saw for an agent that is gone was forgotten");
                let ids: Vec<Uuid> = held.iter().map(|h| h.id).collect();
                if let Err(e) = store.monitor_held_done(&ids).await {
                    tracing::warn!(error = %format!("{e:#}"), "forgetting what monitors saw failed");
                }
                continue;
            };
            let o = self.monitor_oversight(&service);
            let dm = self.dm_look(&service, &owner, &o);
            if dm.away.is_some() || self.update_held(&agent, dm.session.as_deref()) {
                continue;
            }
            let ids: Vec<Uuid> = held.iter().map(|h| h.id).collect();
            let taken = match store.monitor_held_take(&ids).await {
                Ok(taken) => taken,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "taking what monitors saw failed");
                    continue;
                }
            };
            let held: Vec<HeldFinding> =
                held.into_iter().filter(|h| taken.contains(&h.id)).collect();
            if held.is_empty() {
                continue;
            }
            match self
                .tell_monitor(&service, &owner, notice, held_text(&held, &tz))
                .await
            {
                Ok(_) => {
                    tracing::info!(%agent, n = held.len(), "what monitors saw was told once it could be");
                    if let Err(e) = store.monitor_held_done(&taken).await {
                        tracing::warn!(error = %format!("{e:#}"), "forgetting what monitors saw, told, failed: it may be told again");
                    }
                }
                Err(e) => {
                    tracing::warn!(%agent, error = %format!("{e:#}"), "telling what monitors saw failed: tried again at the next look");
                    if let Err(e) = store.monitor_held_untake(&taken).await {
                        tracing::info!(error = %format!("{e:#}"), "letting what monitors saw go failed: taken again in a minute");
                    }
                }
            }
        }
    }
}

/// What one message says of `held`, oldest first: one finding in its own
/// words, noting what held it; several under a line saying why they
/// waited, each with its monitor and when it was seen (local time in `tz`).
pub(crate) fn held_text<Tz: chrono::TimeZone>(held: &[HeldFinding], tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let at = |h: &HeldFinding| h.held_at.with_timezone(tz).format("%H:%M").to_string();
    let why = held
        .iter()
        .map(|h| h.reason)
        .max_by_key(|r| match r {
            HeldFor::Away => 3,
            HeldFor::Quiet => 2,
            HeldFor::Roundup => 1,
            HeldFor::Typing => 0,
        })
        .unwrap_or(HeldFor::Typing);
    if let [one] = held {
        let note = match one.reason {
            HeldFor::Typing => return one.text.clone(),
            HeldFor::Away => "while you were away",
            HeldFor::Quiet => "held for the quiet hours",
            HeldFor::Roundup => "held for the roundup",
        };
        return format!(
            "{}\n\n_{} saw this at {}, {note}._",
            one.text,
            one.monitor_name,
            at(one)
        );
    }
    let lead = match why {
        HeldFor::Away => "While you were away, your monitors saw:",
        HeldFor::Quiet => "Over the quiet hours, your monitors saw:",
        HeldFor::Roundup => "Since the last roundup, your monitors saw:",
        HeldFor::Typing => "Your monitors saw:",
    };
    let items: Vec<String> = held
        .iter()
        .map(|h| format!("**{}** · {}\n{}", h.monitor_name, at(h), h.text))
        .collect();
    format!("{lead}\n\n{}", items.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(name: &str, text: &str, reason: HeldFor, hour: u32) -> HeldFinding {
        use chrono::TimeZone;
        HeldFinding {
            id: Uuid::new_v4(),
            agent: "scout".into(),
            owner: "person-1".into(),
            monitor_id: "invoices".into(),
            monitor_name: name.into(),
            notice: false,
            text: text.into(),
            reason,
            held_at: Utc.with_ymd_and_hms(2026, 11, 19, hour, 5, 0).unwrap(),
            release_at: Utc::now(),
        }
    }

    #[test]
    fn one_held_finding_is_its_own_words_with_why_it_waited() {
        let tz = chrono_tz::UTC;
        let one = held("Invoices", "Acme sent invoice 42.", HeldFor::Quiet, 23);
        assert_eq!(
            held_text(std::slice::from_ref(&one), &tz),
            "Acme sent invoice 42.\n\n_Invoices saw this at 23:05, held for the quiet hours._"
        );
        let typing = HeldFinding {
            reason: HeldFor::Typing,
            ..one
        };
        assert_eq!(held_text(&[typing], &tz), "Acme sent invoice 42.");
    }

    #[test]
    fn several_go_as_one_message_under_why_they_waited() {
        let tz = chrono_tz::UTC;
        let text = held_text(
            &[
                held("Invoices", "Acme sent invoice 42.", HeldFor::Quiet, 23),
                held("Invoices", "Globex sent invoice 7.", HeldFor::Away, 6),
            ],
            &tz,
        );
        assert_eq!(
            text,
            "While you were away, your monitors saw:\n\n\
             **Invoices** · 23:05\nAcme sent invoice 42.\n\n\
             **Invoices** · 06:05\nGlobex sent invoice 7."
        );
    }
}
