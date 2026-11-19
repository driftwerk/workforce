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
//! - **Away, typing, in order.** What would come right away waits while the
//!   person is away from the DM (its posts unread `away_after_minutes`, as
//!   a boss's are judged), while they type there, while the agent's reply
//!   there is being written, and behind what already waits for that DM; it
//!   goes once they are back.
//! - **Held is kept.** What waits is kept in Postgres (`monitor_held`)
//!   before the dispatch is done, so a restart loses none of it, as one row
//!   however often its dispatch runs. When its time comes, everything due
//!   for one DM goes as one message: one finding in its own words, several
//!   under a line saying why they waited (the first [`SHOWN`] in full, the
//!   rest counted).

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;
use workforce_common::channels::desk::{DeliveryKind, DeskPlace, DeskTarget, DESK_CHANNEL};
use workforce_common::oversight::{Oversight, Route};
use workforce_desk::crew::Crew as _;
use workforce_desk::monitor_held_store::{HeldFinding, HeldFor};
use workforce_desk::update_cadence as cadence;
use workforce_orchestrator::monitor::DeskDelivery;

use super::CrewHub;
use crate::desk_surface::{Delivered, Delivery};
use crate::monitor::desk::Finding;
use crate::serve::AgentService;

/// The most findings one held message shows in full; the rest are counted.
pub(crate) const SHOWN: usize = 20;

/// How long what was held for an agent the hub no longer serves is kept,
/// in case it comes back (a reload, a config being fixed).
const GONE_AFTER: Duration = Duration::days(7);

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
        let store = self.desk.store();
        let (why, until) = match cadence::monitor_route(notice, &c, quiet) {
            Route::RightAway => {
                let dm = self.dm_look(service, owner, &o);
                if dm.away.is_some() {
                    (HeldFor::Away, now)
                } else if self.dm_held(agent, dm.session.as_deref())
                    || store
                        .monitor_held_waiting(agent, owner, notice, now)
                        .await?
                {
                    (HeldFor::Typing, now)
                } else {
                    match self
                        .tell_monitor(service, owner, notice, finding.text.clone())
                        .await
                    {
                        Ok(told) => {
                            tracing::info!(agent, monitor = %finding.monitor_id, "a monitor told the person in Desk");
                            return Ok(MonitorTold::Now(told));
                        }
                        // Kept to be told at the next look instead.
                        Err(e) => {
                            tracing::warn!(agent, monitor = %finding.monitor_id, error = %format!("{e:#}"), "telling the person what a monitor saw failed: kept for the next look");
                            (HeldFor::Typing, now)
                        }
                    }
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
        store
            .monitor_hold(&HeldFinding {
                id: HeldFinding::id_of(&format!("{agent}\n{}", finding.key)),
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
        if why == HeldFor::Typing {
            match self.typing_release(agent) {
                // A pause in their typing lets it go before the next
                // minute's look.
                Some(_) => self.poke_at_release(agent),
                // Behind a reply (its end pokes), older findings (told, they
                // poke) or a failed tell: looked at again now.
                None => self.update_poke.notify_one(),
            }
        }
        Ok(MonitorTold::Held { until, why })
    }

    /// Puts `text` in `owner`'s DM with `service`'s agent: a message for
    /// them, or a notice.
    pub(super) async fn tell_monitor(
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
                // Its agent isn't served now: kept a while, in case it
                // comes back, then forgotten with its DM.
                let gone: Vec<Uuid> = held
                    .iter()
                    .filter(|h| h.held_at < now - GONE_AFTER)
                    .map(|h| h.id)
                    .collect();
                if !gone.is_empty() {
                    tracing::warn!(%agent, n = gone.len(), "what monitors saw for an agent that is gone was forgotten");
                    if let Err(e) = store.monitor_held_done(&gone).await {
                        tracing::warn!(error = %format!("{e:#}"), "forgetting what monitors saw failed");
                    }
                }
                continue;
            };
            let o = self.monitor_oversight(&service);
            let dm = self.dm_look(&service, &owner, &o);
            if dm.away.is_some() || self.dm_held(&agent, dm.session.as_deref()) {
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
                .tell_monitor(&service, &owner, notice, held_text(&held, now, &tz))
                .await
            {
                Ok(_) => {
                    tracing::info!(%agent, n = held.len(), "what monitors saw was told once it could be");
                    if let Err(e) = store.monitor_held_done(&taken).await {
                        tracing::warn!(error = %format!("{e:#}"), "forgetting what monitors saw, told, failed: it may be told again");
                    }
                    // What waited behind these may go now, not at the next
                    // minute's look.
                    self.update_poke.notify_one();
                }
                Err(e) => {
                    tracing::warn!(%agent, error = %format!("{e:#}"), "telling what monitors saw failed: tried again at the next look");
                    match store.monitor_held_untake(&taken).await {
                        Ok(monitors) if monitors.is_empty() => {}
                        Ok(monitors) => {
                            tracing::warn!(%agent, %owner, dropped = monitors.len(), ?monitors, "what monitors saw couldn't be told after many tries and was dropped")
                        }
                        Err(e) => {
                            tracing::info!(error = %format!("{e:#}"), "letting what monitors saw go failed: taken again in a minute")
                        }
                    }
                }
            }
        }
    }
}

/// What one message says of `held`, oldest first: one finding in its own
/// words, noting what held it; several under a line saying why they
/// waited, each with its monitor and when it was seen (local time in `tz`,
/// with the weekday when it wasn't `now`'s day, the date past six days),
/// the first [`SHOWN`] in full and the rest counted by monitor.
pub(crate) fn held_text<Tz: chrono::TimeZone>(
    held: &[HeldFinding],
    now: DateTime<Utc>,
    tz: &Tz,
) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let today = now.with_timezone(tz).date_naive();
    let at = |h: &HeldFinding| {
        let local = h.held_at.with_timezone(tz);
        let days = (today - local.date_naive()).num_days();
        match days {
            // Today, or tomorrow on a hub whose clock runs ahead.
            -1..=0 => local.format("%H:%M").to_string(),
            1..=6 => local.format("%a %H:%M").to_string(),
            _ => local.format("%a %-d %b %H:%M").to_string(),
        }
    };
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
    let mut parts: Vec<String> = held
        .iter()
        .take(SHOWN)
        .map(|h| format!("**{}** · {}\n{}", h.monitor_name, at(h), h.text))
        .collect();
    let mut more: BTreeMap<&str, usize> = BTreeMap::new();
    for h in held.iter().skip(SHOWN) {
        *more.entry(h.monitor_name.as_str()).or_default() += 1;
    }
    if !more.is_empty() {
        let counted: Vec<String> = more
            .iter()
            .map(|(name, n)| format!("{n} more from {name}"))
            .collect();
        parts.push(format!("…and {}.", counted.join(", ")));
    }
    format!("{lead}\n\n{}", parts.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn held(name: &str, text: &str, reason: HeldFor, day: u32, hour: u32) -> HeldFinding {
        HeldFinding {
            id: Uuid::new_v4(),
            agent: "scout".into(),
            owner: "person-1".into(),
            monitor_id: "invoices".into(),
            monitor_name: name.into(),
            notice: false,
            text: text.into(),
            reason,
            held_at: Utc.with_ymd_and_hms(2026, 11, day, hour, 5, 0).unwrap(),
            release_at: Utc::now(),
        }
    }

    /// Thursday 19 November 2026, 09:00 UTC.
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 19, 9, 0, 0).unwrap()
    }

    #[test]
    fn one_held_finding_is_its_own_words_with_why_it_waited() {
        let tz = chrono_tz::UTC;
        let one = held("Invoices", "Acme sent invoice 42.", HeldFor::Away, 19, 7);
        assert_eq!(
            held_text(std::slice::from_ref(&one), now(), &tz),
            "Acme sent invoice 42.\n\n_Invoices saw this at 07:05, while you were away._"
        );
        let typing = HeldFinding {
            reason: HeldFor::Typing,
            ..one
        };
        assert_eq!(held_text(&[typing], now(), &tz), "Acme sent invoice 42.");
    }

    #[test]
    fn several_go_as_one_message_under_why_they_waited_with_the_day_when_not_today() {
        let tz = chrono_tz::UTC;
        let text = held_text(
            &[
                held("Invoices", "Acme sent invoice 42.", HeldFor::Quiet, 18, 23),
                held("Invoices", "Globex sent invoice 7.", HeldFor::Away, 19, 6),
            ],
            now(),
            &tz,
        );
        assert_eq!(
            text,
            "While you were away, your monitors saw:\n\n\
             **Invoices** · Wed 23:05\nAcme sent invoice 42.\n\n\
             **Invoices** · 06:05\nGlobex sent invoice 7."
        );
        let week = held_text(
            &[
                held("Invoices", "Old.", HeldFor::Away, 12, 8),
                held("Invoices", "New.", HeldFor::Away, 19, 8),
            ],
            now(),
            &tz,
        );
        assert!(
            week.contains("**Invoices** · Thu 12 Nov 08:05\nOld."),
            "{week}"
        );
        // A hub whose clock runs a little ahead: today's, not a date.
        let ahead = held("Invoices", "Ahead.", HeldFor::Away, 20, 0);
        let one = held_text(std::slice::from_ref(&ahead), now(), &tz);
        assert!(one.contains("saw this at 00:05,"), "{one}");
    }

    #[test]
    fn past_the_first_twenty_the_rest_are_counted_by_monitor() {
        let tz = chrono_tz::UTC;
        let mut many: Vec<HeldFinding> = (0..SHOWN)
            .map(|n| held("Invoices", &format!("invoice {n}"), HeldFor::Away, 19, 1))
            .collect();
        many.extend((0..3).map(|n| held("Invoices", &format!("late {n}"), HeldFor::Away, 19, 2)));
        many.push(held("Refunds", "refund", HeldFor::Away, 19, 3));
        let text = held_text(&many, now(), &tz);
        assert!(
            text.contains("invoice 19") && !text.contains("late 0"),
            "{text}"
        );
        assert!(
            text.ends_with("…and 3 more from Invoices, 1 more from Refunds."),
            "{text}"
        );
    }
}
