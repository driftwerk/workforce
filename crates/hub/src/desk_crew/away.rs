//! The person typing to a boss, and away from it (overseer H8).
//!
//! - **The typing hold.** Desk pings `POST /api/desk/dm/{agent}/typing`
//!   while the person types in a boss's DM. Its updates and needs-you wait
//!   for a pause, never longer than three minutes from when the typing
//!   began. Updates also wait for a reply the boss is writing in the DM,
//!   so they never land in the middle of it; needs-you don't, since the
//!   boss often asks inside that very turn.
//! - **Away.** Judged from how far the person read the DM: once the
//!   boss's posts there have gone unread `away_after_minutes`, they are
//!   away from it, however busy they are elsewhere in Desk. Desk's
//!   presence pings (`POST /api/desk/presence`) say when the DM is on
//!   screen; when they stop, the read point alone decides. `away_since` is
//!   kept in Postgres. A hub whose sessions keep no read points never
//!   counts the person away.
//! - **The catch-up.** On their return (the DM read or on screen again,
//!   or they wrote), one message covers the absence, in place of the
//!   updates it gathered: see [`workforce_desk::away`].

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;
use workforce_common::channels::DESK_CHANNEL;
use workforce_common::oversight::Oversight;
use workforce_common::session::MessageRole;
use workforce_desk::away::{self, MentionedAway, ParkedItem, Presence, Typing};
use workforce_desk::decision::{BossDecision, Handled};
use workforce_desk::model::SessionState;
use workforce_desk::open_item::{OpenItem, OpenKind, OpenState};
use workforce_desk::update::{Change, MentionedUpdate, Pending, UpdateState, WaitingItem};

use super::{CrewHub, CHECK_IN};
use crate::serve::AgentService;

/// What the updates' look found of a boss's DM.
pub(super) struct DmLook {
    /// The DM's session, while there is one.
    pub session: Option<String>,
    /// Since when the person is away from it, or none.
    pub away: Option<DateTime<Utc>>,
}

impl CrewHub {
    /// `minutes` of the crew's minute.
    fn of_minutes(&self, minutes: f64) -> Duration {
        Duration::from_std(self.timing.minute.mul_f64(minutes)).unwrap_or_default()
    }

    fn quiet(&self) -> Duration {
        self.of_minutes(away::QUIET)
    }

    fn cap(&self) -> Duration {
        self.of_minutes(away::CAP)
    }

    /// The person typed in `boss`'s DM; false when it is no boss here.
    pub(super) fn typing_in(&self, boss: &str) -> bool {
        if self.overseer_named(boss).is_none() {
            return false;
        }
        let quiet = self.quiet();
        let mut typing = self.dm_typing.lock();
        let was = typing.get(boss).copied();
        typing.insert(boss.to_string(), Typing::ping(was, Utc::now(), quiet));
        true
    }

    /// When the person's typing in `boss`'s DM lets its posts go, while it
    /// holds them. A stretch past the cap holds nothing, however long it
    /// goes on: it is only forgotten when they send.
    pub(super) fn typing_release(&self, boss: &str) -> Option<DateTime<Utc>> {
        let (quiet, cap) = (self.quiet(), self.cap());
        let t = *self.dm_typing.lock().get(boss)?;
        t.holds(Utc::now(), quiet, cap)
            .then(|| t.release_at(quiet, cap))
    }

    /// The person sent `boss` a message: they aren't typing it any more,
    /// and they are here.
    pub(super) fn dm_sent(&self, boss: &str) {
        self.dm_typing.lock().remove(boss);
        self.update_poke.notify_one();
    }

    /// What Desk says of the person now.
    pub(super) fn present(&self, attending: bool, dm_on_screen: Option<&str>) {
        let now = Utc::now();
        let dm = dm_on_screen.filter(|_| attending).map(str::to_string);
        if let Some(dm) = &dm {
            let fresh = self.of_minutes(away::PRESENCE_FRESH);
            let was = self.presence.lock().clone();
            self.on_screen.lock().insert(dm.clone(), now);
            // Back on screen: a catch-up may be due at once.
            if !was.is_some_and(|p| p.shows(dm, now, fresh)) {
                self.update_poke.notify_one();
            }
        }
        *self.presence.lock() = Some(Presence {
            at: now,
            attending,
            dm_on_screen: dm,
        });
    }

    /// Waits while the person types in `boss`'s DM, at most the cap from
    /// now.
    pub(super) async fn after_typing(&self, boss: &str) {
        let until = Utc::now() + self.cap();
        loop {
            let Some(release) = self.typing_release(boss) else {
                return;
            };
            let at = release.min(until);
            let now = Utc::now();
            if at <= now {
                return;
            }
            tokio::time::sleep((at - now).to_std().unwrap_or_default()).await;
        }
    }

    /// Looks at the updates again once the person's typing in `boss`'s DM
    /// lets them go, rather than at the next minute's look.
    pub(super) fn poke_at_release(&self, boss: &str) {
        let Some(at) = self.typing_release(boss) else {
            return;
        };
        let wait = (at - Utc::now()).to_std().unwrap_or_default();
        let crew = self.me();
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            crew.update_poke.notify_one();
        });
    }

    /// Whether `boss`'s updates wait now: the person is typing to it, or
    /// its reply in the DM (`session`) is being written.
    pub(super) fn update_held(&self, boss: &str, session: Option<&str>) -> bool {
        self.typing_release(boss).is_some()
            || session.is_some_and(|sid| self.manager_turns.lock().contains_key(sid))
    }

    /// How far `owner` has read `service`'s DM: their read point, or when
    /// the points began if they never read it; none when the hub keeps no
    /// read points.
    fn read_point(&self, service: &AgentService, owner: &str) -> Option<DateTime<Utc>> {
        #[cfg(test)]
        if let Some(at) = self.test_reads.lock().get(service.agent_id.as_str()) {
            return Some(*at);
        }
        match service.sessions.desk_read(owner, DESK_CHANNEL) {
            Ok(Some(r)) => Some(r.at),
            Ok(None) => service.sessions.desk_read_floor(owner).ok().flatten(),
            Err(e) => {
                tracing::info!(error = %e, "the DM's read point couldn't be read");
                None
            }
        }
    }

    /// `service`'s DM with `owner` as the updates see it now: its session,
    /// and whether the person is away from it.
    pub(super) fn dm_look(&self, service: &AgentService, owner: &str, o: &Oversight) -> DmLook {
        let boss = service.agent_id.as_str();
        let now = Utc::now();
        let Some(dm) = service.sessions.find(DESK_CHANNEL, owner).ok().flatten() else {
            return DmLook {
                session: None,
                away: None,
            };
        };
        let session = Some(dm.id.to_string());
        let Some(read) = self.read_point(service, owner) else {
            return DmLook {
                session,
                away: None,
            };
        };
        // What they wrote there they saw; a check-in's prompt is not theirs.
        let wrote = dm
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::User && !m.content.starts_with(CHECK_IN))
            .map(|m| m.timestamp)
            .max();
        let screen = self.on_screen.lock().get(boss).copied();
        let seen = [Some(read), wrote, screen]
            .into_iter()
            .flatten()
            .max()
            .unwrap_or(read);
        let fresh = self.of_minutes(away::PRESENCE_FRESH);
        let on = self
            .presence
            .lock()
            .as_ref()
            .is_some_and(|p| p.shows(boss, now, fresh));
        let posts: Vec<DateTime<Utc>> = dm
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Assistant)
            .map(|m| m.timestamp)
            .collect();
        let after = self.of_minutes(f64::from(o.updates.away_after_minutes));
        DmLook {
            session,
            away: away::away_since(&posts, seen, on, now, after),
        }
    }

    /// The person is back after an absence since `since`: one catch-up,
    /// in place of the updates it gathered, then updates go on. Nothing to
    /// say is no message; the absence is over either way.
    pub(super) async fn catch_up(
        &self,
        service: &Arc<AgentService>,
        owner: &str,
        o: &Oversight,
        state: &UpdateState,
        since: DateTime<Utc>,
    ) -> eyre::Result<()> {
        let boss = service.agent_id.as_str();
        let now = Utc::now();
        let store = self.desk.store();
        let b = self.batch(service, o, state, now, Some(since)).await;
        let open = store.items_not_closed(Some(boss)).await.unwrap_or_default();
        let asks = |i: &&OpenItem| matches!(i.kind, OpenKind::Ask | OpenKind::Confirm);
        let waiting: Vec<&OpenItem> = open
            .iter()
            .filter(asks)
            .filter(|i| i.state == OpenState::Open)
            .collect();
        let parked: Vec<ParkedItem> = open
            .iter()
            .filter(asks)
            .filter(|i| i.state == OpenState::Parked)
            .map(|i| ParkedItem {
                item_id: i.id.to_string(),
                text: i.text.clone(),
            })
            .collect();
        if b.sessions.is_empty()
            && b.decisions.is_empty()
            && b.kept.is_empty()
            && waiting.is_empty()
            && parked.is_empty()
        {
            store.update_back(boss, since).await?;
            if b.change == Change::None && state.gather_since.is_some() {
                store.update_regather(boss, &b.told).await?;
            }
            return Ok(());
        }
        let sessions = self.paragraphs(boss, owner, o, &b).await;
        let finished: Vec<(String, String)> = b
            .sessions
            .iter()
            .filter(|s| {
                self.desk
                    .view(s.id)
                    .is_ok_and(|v| matches!(v.state, SessionState::Done))
            })
            .map(|s| (s.id.to_string(), s.name.clone()))
            .collect();
        let names: Vec<&str> = finished.iter().map(|(_, n)| n.as_str()).collect();
        let tz = self.desk.timezone();
        let decided: Vec<String> = b.decisions.iter().map(|d| timed(d, tz)).collect();
        let footer = match self.desk.cheap_today().await {
            Ok((spent, cap)) => workforce_desk::update::footer(spent, cap),
            Err(_) => None,
        };
        let u = MentionedUpdate {
            sessions,
            decisions: b.decisions.iter().map(BossDecision::line).collect(),
            kept: b.kept.clone(),
            waiting: waiting
                .iter()
                .map(|i| WaitingItem {
                    item_id: i.id.to_string(),
                    text: i.text.clone(),
                    age: workforce_desk::update::age(i.opened_at, now),
                    holding_up: i.holding_up.clone(),
                    recommendation: i.recommendation.clone(),
                    options: i.options.clone(),
                })
                .collect(),
            footer,
        };
        let a = MentionedAway {
            since,
            until: now,
            headline: away::headline(waiting.len(), &names),
            finished: finished.into_iter().map(|(id, _)| id).collect(),
            handled: away::handled_line(&Handled::of(&b.decisions)),
            parked,
        };
        let text = away::render(&a, &u, &decided);
        let p = Pending {
            id: Uuid::new_v4(),
            text,
            update: u,
            told: b.told.clone(),
            // Each open ask is said again; none parks for it.
            restated: waiting.iter().map(|i| (i.id, false)).collect(),
            cutoff: Some(now),
            handed: false,
            sent: None,
            away: Some(a),
        };
        store.update_pend(boss, &p).await?;
        self.send(service, owner, state, p).await
    }
}

/// A decision as the catch-up lists it: its time, then the row's own
/// words.
fn timed(d: &BossDecision, tz: chrono_tz::Tz) -> String {
    let at = d.at.with_timezone(&tz).format("%H:%M");
    match d.why.trim() {
        "" => format!("{at} · {}.", d.what.trim_end_matches('.')),
        why => format!("{at} · {} — {why}", d.what.trim_end_matches('.')),
    }
}
