//! A boss's updates (overseer H7): what changed on its crew since it last
//! told the person, gathered and sent as one message in its DM.
//!
//! - **What is news.** A session's account that did something (a done
//!   line) or changed what comes next or what it waits on; a decision the
//!   boss made; a promise or order it kept; an ask due to be said again.
//!   What a session is doing this minute alone is not.
//! - **When.** A visible result (a PR, merged, deployed, failed, a
//!   decision, a kept order) starts the gather and goes out
//!   `every_minutes` after; a small change by `max_wait_minutes`. Never two
//!   within `every_minutes`, doubling after three in a row the person
//!   hasn't read. Its state is in Postgres (`boss_update_state`), and the
//!   update is kept before it is sent, so a restart neither loses nor
//!   repeats it.
//! - **Who writes it.** The cheap model writes each session's paragraph
//!   in the boss's voice ([`UpdateWriter`]); decisions, what it kept and
//!   what still waits are the rows' own words. Off, failed or past the
//!   day's ceiling, the paragraphs are written plainly from the accounts.
//! - **Where.** At Act and Act fully the update is its own message
//!   (`DeliveryKind::Update`). At Watch and Suggest it is handed to a
//!   check-in, whose message (with its suggestions) is the one post; with
//!   the check-ins spent, it goes out plainly as its own.
//!
//! Needs-you and decisions never wait on any of this: they are posted and
//! recorded as they happen.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_common::channels::desk::{DeliveryKind, DeskPlace, DeskTarget};
use workforce_common::channels::DESK_CHANNEL;
use workforce_common::oversight::Oversight;
use workforce_desk::account::{Account, Done};
use workforce_desk::crew::Extras;
use workforce_desk::decision::{BossDecision, Need};
use workforce_desk::model::SessionState;
use workforce_desk::open_item::{OpenItem, OpenKind, OpenState};
use workforce_desk::update::{
    self, Change, MentionedUpdate, Pending, Told, UpdateSession, UpdateState, WaitingItem,
};

use super::watcher::Budget;
use super::{CrewHub, CHECK_IN};
use crate::serve::AgentService;

/// Who writes an update's session paragraphs: the cheap model, on the
/// boss's own plan for the person.
#[async_trait::async_trait]
pub(crate) trait UpdateWriter: Send + Sync {
    async fn write(
        &self,
        boss: &str,
        owner: &str,
        system: &str,
        user: &str,
    ) -> eyre::Result<String>;
}

/// The hub's writer: the floor's cheap model, booked as `desk:update`.
pub(crate) struct HubWriter {
    pub model: crate::floor::gists::HubModel,
}

#[async_trait::async_trait]
impl UpdateWriter for HubWriter {
    async fn write(
        &self,
        boss: &str,
        owner: &str,
        system: &str,
        user: &str,
    ) -> eyre::Result<String> {
        let subject = workforce_desk::floor::Subject::Agent {
            agent_id: boss.to_string(),
            session_id: String::new(),
        };
        self.model
            .answer_for(
                Some(&subject),
                Some(owner),
                system,
                user,
                crate::floor::gists::Call::UPDATE,
            )
            .await
            .map(|(text, _)| text)
    }
}

/// A session that changed since the person was last told of it.
struct Changed {
    id: Uuid,
    name: String,
    account: Account,
    new: Vec<Done>,
}

/// What one boss's next update would say now.
struct Batch {
    change: Change,
    sessions: Vec<Changed>,
    decisions: Vec<BossDecision>,
    kept: Vec<String>,
    waiting: Vec<OpenItem>,
    restated: Vec<(Uuid, bool)>,
    told: HashMap<Uuid, Told>,
}

impl CrewHub {
    /// Sets who writes the updates' paragraphs; with none, they are
    /// written plainly.
    pub(crate) fn set_update_writer(&self, writer: Arc<dyn UpdateWriter>) {
        *self.update_writer.lock() = Some(writer);
    }

    fn minutes(&self, n: u32) -> Duration {
        Duration::from_std(self.timing.minute).unwrap_or_default() * n as i32
    }

    /// What `service`'s next update would say, from `state`, at `now`.
    async fn batch(
        &self,
        service: &AgentService,
        o: &Oversight,
        state: &UpdateState,
        now: DateTime<Utc>,
    ) -> Batch {
        let boss = service.agent_id.as_str();
        let seen = self.desk.seen_by(&o.scope);
        let views: Vec<_> = self
            .desk
            .list()
            .into_iter()
            .filter(|v| v.state != SessionState::Closed)
            .filter(|v| seen.sees(v.group.as_ref().map(|g| g.id.as_str())))
            .filter(|v| !workforce_desk::ledger::agent_turn(&v.cwd, &v.name))
            .filter(|v| !self.desk.hub_internal(v.id))
            .collect();
        let ids: Vec<Uuid> = views.iter().map(|v| v.id).collect();
        let accounts = self
            .desk
            .session_accounts_of(&ids)
            .await
            .unwrap_or_else(|e| {
                tracing::info!(error = %e, "the sessions' accounts couldn't be read for an update");
                Vec::new()
            });
        let mut change = Change::None;
        let mut sessions = Vec::new();
        let mut told = state.told.clone();
        for v in &views {
            let Some(a) = accounts.iter().find(|a| a.session == v.id) else {
                continue;
            };
            let was = state.told.get(&v.id);
            let c = update::changed(was, a, state.since);
            told.insert(v.id, Told::of(a));
            if c == Change::None {
                continue;
            }
            change = change.max(c);
            sessions.push(Changed {
                id: v.id,
                name: v.label().to_string(),
                new: update::done_since(was, a, state.since)
                    .into_iter()
                    .cloned()
                    .collect(),
                account: a.clone(),
            });
        }
        let last = state.last_update_at.unwrap_or(state.since);
        let store = self.desk.store();
        let composed = o.updates.enabled;
        let decisions: Vec<BossDecision> = store
            .decisions_since(boss, last, 500)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|d| d.at > last && d.at <= now && d.undone_at.is_none())
            // Without the composer, an answer to a session was said at once.
            .filter(|d| composed || d.need != Need::Question)
            .collect();
        let tz = self.desk.timezone();
        let kept: Vec<String> = match composed {
            true => store
                .items_closed_since(boss, last, 200)
                .await
                .unwrap_or_default()
                .iter()
                .filter(|i| i.closed_at.is_some_and(|at| at > last && at <= now))
                .filter_map(|i| super::items::kept_line(i, tz))
                .collect(),
            false => Vec::new(),
        };
        let open = store.items_not_closed(Some(boss)).await.unwrap_or_default();
        let restated: Vec<(Uuid, bool)> = match composed {
            true => self
                .due_again(&open, o, now)
                .into_iter()
                .map(|(i, _, park)| (i.id, park))
                .collect(),
            false => Vec::new(),
        };
        let waiting: Vec<OpenItem> = open
            .into_iter()
            .filter(|i| matches!(i.kind, OpenKind::Ask | OpenKind::Confirm))
            .filter(|i| i.said() && i.state == OpenState::Open)
            .collect();
        let quiet = self
            .quiet_made
            .lock()
            .get(boss)
            .is_some_and(|m| !m.is_empty());
        if !decisions.is_empty() || !kept.is_empty() || quiet {
            change = Change::Result;
        }
        if !restated.is_empty() {
            change = change.max(Change::Small);
        }
        Batch {
            change,
            sessions,
            decisions,
            kept,
            waiting,
            restated,
            told,
        }
    }

    /// The session paragraphs of `b`: the cheap model's in the boss's
    /// voice when it is on and may be asked, else plainly from the rows;
    /// any it leaves out plainly too.
    async fn paragraphs(
        &self,
        boss: &str,
        owner: &str,
        o: &Oversight,
        b: &Batch,
    ) -> Vec<UpdateSession> {
        let mut written: HashMap<Uuid, String> = HashMap::new();
        let writer = self.update_writer.lock().clone();
        if let (true, Some(writer), false) = (o.updates.enabled, writer, b.sessions.is_empty()) {
            if self.desk.update_take().await.unwrap_or(false) {
                let input: Vec<(Uuid, String, &Account, Vec<&Done>)> = b
                    .sessions
                    .iter()
                    .map(|s| (s.id, s.name.clone(), &s.account, s.new.iter().collect()))
                    .collect();
                let user = update::composer_input(&input);
                match writer
                    .write(boss, owner, update::COMPOSER_SYSTEM, &user)
                    .await
                {
                    Ok(answer) => match update::parse_composed(&answer) {
                        Some(got) => written = got,
                        None => tracing::info!(
                            "the update's writer answered out of form; written plainly"
                        ),
                    },
                    Err(e) => {
                        tracing::info!(error = %format!("{e:#}"), "the update's writer failed; written plainly")
                    }
                }
            }
        }
        b.sessions
            .iter()
            .map(|s| {
                let new: Vec<&Done> = s.new.iter().collect();
                UpdateSession {
                    session_id: s.id.to_string(),
                    name: s.name.clone(),
                    purpose: s.account.purpose.trim().to_string(),
                    text: written
                        .remove(&s.id)
                        .unwrap_or_else(|| update::plain_session(&new, &s.account)),
                    refs: update::refs_of(&new),
                }
            })
            .collect()
    }

    /// `b` written as an update for `owner`: its extras and its text.
    async fn written(
        &self,
        boss: &str,
        owner: &str,
        o: &Oversight,
        b: &Batch,
        now: DateTime<Utc>,
    ) -> (MentionedUpdate, String) {
        let sessions = self.paragraphs(boss, owner, o, b).await;
        let footer = match self.desk.cheap_today().await {
            Ok((spent, cap)) => update::footer(spent, cap),
            Err(_) => None,
        };
        let u = MentionedUpdate {
            sessions,
            decisions: b.decisions.iter().map(BossDecision::line).collect(),
            kept: b.kept.clone(),
            waiting: b
                .waiting
                .iter()
                .map(|i| WaitingItem {
                    item_id: i.id.to_string(),
                    text: i.text.clone(),
                    age: update::age(i.opened_at, now),
                    holding_up: i.holding_up.clone(),
                    recommendation: i.recommendation.clone(),
                    options: i.options.clone(),
                })
                .collect(),
            footer,
        };
        let decided: Vec<String> = b.decisions.iter().map(BossDecision::said).collect();
        let text = update::render(&u, &decided);
        (u, text)
    }

    /// Looks at every boss's update once: sends one written but not yet
    /// gone, starts gathering on news, and sends what is due.
    pub(crate) async fn look_at_updates(&self) {
        for (service, o) in self.overseers() {
            let boss = service.agent_id.to_string();
            // One look at a boss's update at a time: a wake-up's check-in
            // may be writing it.
            let Some(_one) = Updating::begin(&self.updating, &boss) else {
                continue;
            };
            if let Err(e) = self.update_of(&service, &o).await {
                tracing::warn!(agent = %service.agent_id, error = %format!("{e:#}"), "a boss's update failed");
            }
        }
    }

    async fn update_of(&self, service: &Arc<AgentService>, o: &Oversight) -> eyre::Result<()> {
        let boss = service.agent_id.as_str();
        let Some(owner) = self.person(Some(service)) else {
            return Ok(());
        };
        let now = Utc::now();
        let store = self.desk.store();
        let state = store.update_state(boss, now).await?;
        if let Some(p) = state.pending.clone() {
            return self.resume(service, &owner, o, &state, p).await;
        }
        let b = self.batch(service, o, &state, now).await;
        let gather_since = match (b.change, state.gather_since) {
            (Change::None, None) => return Ok(()),
            (Change::None, Some(_)) => {
                // What was gathered no longer says anything (undone, or
                // told another way): nothing to send.
                store.update_regather(boss, &b.told).await?;
                return Ok(());
            }
            (c, since) => {
                let result = c == Change::Result;
                if since.is_none() || (result && !state.result) {
                    store.update_gather(boss, now, result).await?;
                }
                since.unwrap_or(now)
            }
        };
        let read = service
            .sessions
            .desk_read(&owner, DESK_CHANNEL)
            .ok()
            .flatten()
            .map(|r| r.at);
        let unread = update::streak(state.unread, state.last_update_at, read);
        let every = self.minutes(o.updates.every_minutes);
        let max_wait = self.minutes(o.updates.max_wait_minutes);
        let gap = update::interval(every, max_wait, unread);
        let result = state.result || b.change == Change::Result;
        if !update::due(
            Some(gather_since),
            result,
            state.last_update_at,
            now,
            (every, max_wait, gap),
        ) {
            return Ok(());
        }
        let (level, _) = self.level_now(boss, o);
        let p = self.pend(boss, &owner, o, &b, now, !level.acts()).await?;
        self.resume(service, &owner, o, &state, p).await
    }

    /// `b` written and kept as `boss`'s pending update, before anything is
    /// sent: `handed` to a check-in at Watch and Suggest.
    async fn pend(
        &self,
        boss: &str,
        owner: &str,
        o: &Oversight,
        b: &Batch,
        now: DateTime<Utc>,
        handed: bool,
    ) -> eyre::Result<Pending> {
        let (u, text) = self.written(boss, owner, o, b, now).await;
        let p = Pending {
            id: Uuid::new_v4(),
            text,
            update: u,
            told: b.told.clone(),
            restated: b.restated.clone(),
            cutoff: Some(now),
            handed,
            sent: None,
        };
        self.desk.store().update_pend(boss, &p).await?;
        Ok(p)
    }

    /// Sends the pending `p`, as it was written: handed to a check-in while
    /// one may run (it ran already when its check-in answered), else as an
    /// update of its own.
    async fn resume(
        &self,
        service: &Arc<AgentService>,
        owner: &str,
        o: &Oversight,
        state: &UpdateState,
        p: Pending,
    ) -> eyre::Result<()> {
        let boss = service.agent_id.as_str();
        // Its check-in answered it already: only what follows is left.
        if p.handed && p.sent.is_some() {
            return self.told(service, owner, state, &p).await;
        }
        if p.handed {
            // A check-in that answered it before a restart was its post.
            let answered = self
                .desk
                .store()
                .latest_check_in(boss)
                .await
                .ok()
                .flatten()
                .filter(|c| c.prompt.contains(&marker(&p)))
                .and_then(|c| c.reply);
            if let Some(reply) = answered {
                return self.answered(service, owner, state, &p, &reply).await;
            }
            if matches!(self.budget(boss, o).await, Ok(Budget::Open)) {
                return self.in_check_in(service, owner, o, state, p).await;
            }
            // The check-ins are spent: the update goes out on its own.
        }
        self.send(service, owner, state, p).await
    }

    /// The prompt's part that hands `p` to a check-in, found again by its
    /// id after a restart.
    fn handing(p: &Pending) -> String {
        format!(
            "Your update to the person is due ({}). Here is what changed since you last told them, each session's \
             paragraph already written:\n{}\n\n\
             Give the person this update in your message: keep each session's paragraph and every line that \
             starts \"I decided\", \"Still waiting on you\" or says what you kept, word for word, and add what you suggest.",
            marker(p),
            super::watcher::reported(&p.text, "your update"),
        )
    }

    /// What a wake-up's check-in at Watch or Suggest also carries: the
    /// boss's update, written now from what changed, as its one post. None
    /// when nothing changed, or another look is writing it.
    pub(super) async fn update_for_wake(
        &self,
        service: &AgentService,
        o: &Oversight,
    ) -> Option<(Pending, String, Updating<'_>)> {
        let boss = service.agent_id.as_str();
        let one = Updating::begin(&self.updating, boss)?;
        let owner = self.person(Some(service))?;
        let now = Utc::now();
        let state = self.desk.store().update_state(boss, now).await.ok()?;
        let p = match state.pending.clone() {
            Some(p) if p.handed && p.sent.is_none() => p,
            Some(_) => return None,
            None => {
                let b = self.batch(service, o, &state, now).await;
                if b.change == Change::None {
                    return None;
                }
                self.pend(boss, &owner, o, &b, now, true).await.ok()?
            }
        };
        let handing = Self::handing(&p);
        Some((p, handing, one))
    }

    /// A wake-up's check-in carried `p` and answered `reply`: told.
    pub(super) async fn update_answered(
        &self,
        service: &Arc<AgentService>,
        p: &Pending,
        reply: &str,
    ) {
        let boss = service.agent_id.as_str();
        let Some(owner) = self.person(Some(service)) else {
            return;
        };
        let state = match self.desk.store().update_state(boss, Utc::now()).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "a boss's update state couldn't be read");
                return;
            }
        };
        if let Err(e) = self.answered(service, &owner, &state, p, reply).await {
            tracing::warn!(error = %format!("{e:#}"), "marking a check-in's update told failed");
        }
    }

    /// Hands `p` to a check-in of its own at Watch or Suggest: its message,
    /// with its suggestions, is the update.
    async fn in_check_in(
        &self,
        service: &Arc<AgentService>,
        owner: &str,
        o: &Oversight,
        state: &UpdateState,
        p: Pending,
    ) -> eyre::Result<()> {
        let (level, _) = self.level_now(service.agent_id.as_str(), o);
        let prompt = format!(
            "{CHECK_IN} {} (You oversee the crew at level {}.) {}",
            Self::handing(&p),
            level.as_str(),
            match self.unbound_given(service, o) {
                true => Self::boss_hint(o),
                false => Self::level_hint(level, o),
            },
        );
        // A check-in like any other: it counts against the hour's.
        if let Some(reply) = self
            .check_in_on(service, owner, "wake", &prompt, 0, None, false)
            .await
        {
            self.answered(service, owner, state, &p, &reply).await?;
        }
        Ok(())
    }

    /// A check-in answered `reply` for `p`: what of its decisions, what it
    /// kept and what still waits the reply left out goes out as the rows
    /// say it, so none is lost to its wording; then told.
    async fn answered(
        &self,
        service: &Arc<AgentService>,
        owner: &str,
        state: &UpdateState,
        p: &Pending,
        reply: &str,
    ) -> eyre::Result<()> {
        let decided: Vec<String> = self
            .desk
            .store()
            .decisions_of(
                &p.update
                    .decisions
                    .iter()
                    .filter_map(|d| d.id.parse().ok())
                    .collect::<Vec<_>>(),
            )
            .await
            .unwrap_or_default()
            .iter()
            .map(BossDecision::said)
            .collect();
        // Answered: kept so first, so a restart from here on neither hands
        // it to another check-in nor posts what it left out twice.
        let mut p = p.clone();
        p.sent = Some(format!("check-in {}", marker(&p)));
        self.desk
            .store()
            .update_pend(service.agent_id.as_str(), &p)
            .await?;
        let missing = update::missing_lines(&p, &decided, reply);
        if !missing.is_empty() {
            let target =
                DeskTarget::of_turn(&DeskPlace::Dm { thread: None }, DESK_CHANNEL, None, false);
            let delivery =
                crate::desk_surface::Delivery::text(DeliveryKind::Update, missing.join("\n\n"));
            if let Err(e) = crate::desk_surface::deliver(service, owner, &target, delivery).await {
                tracing::warn!(error = %e, "posting what a check-in's update left out failed");
            }
        }
        self.told(service, owner, state, &p).await
    }

    /// Sends `p` as `service`'s update, once: when a restart finds it
    /// already in the DM, it is only marked told; one that couldn't go is
    /// kept, and sent as it is on a later look.
    async fn send(
        &self,
        service: &Arc<AgentService>,
        owner: &str,
        state: &UpdateState,
        mut p: Pending,
    ) -> eyre::Result<()> {
        let boss = service.agent_id.as_str();
        let id = p.id.to_string();
        let found = p.sent.clone().or_else(|| {
            service
                .sessions
                .find(DESK_CHANNEL, owner)
                .ok()
                .flatten()
                .and_then(|s| {
                    s.messages
                        .iter()
                        .rev()
                        .find(|m| {
                            m.desk.as_ref().and_then(|d| d.request_id.as_deref()) == Some(&id)
                        })
                        .map(|m| m.id.clone())
                })
        });
        let extras = Extras {
            update: Some(p.update.clone()),
            suggestions: self.quiet_suggestions(boss, &p),
            ..Default::default()
        };
        match found {
            Some(m) => self.keep_extras_of(&m, boss, &extras).await,
            None => {
                let mut delivery =
                    crate::desk_surface::Delivery::text(DeliveryKind::Update, p.text.clone());
                delivery.request_id = Some(id);
                // Its extras kept first: the update is read whole the
                // moment it is in the DM.
                let message = self
                    .deliver_with_extras(service, owner, delivery, &extras)
                    .await?
                    .message_id;
                // Sent: kept so, before anything else, so a restart from
                // here on finds it whatever the DM did since.
                p.sent = Some(message);
                self.desk.store().update_pend(boss, &p).await?;
            }
        }
        self.told(service, owner, state, &p).await
    }

    /// The suggestions quiet check-ins made since `p`'s last update, shown
    /// with it: no message of their own carries them.
    fn quiet_suggestions(
        &self,
        boss: &str,
        p: &Pending,
    ) -> Vec<workforce_desk::crew::MentionedSuggestion> {
        let cutoff = p.cutoff.unwrap_or_else(Utc::now);
        self.quiet_made
            .lock()
            .get(boss)
            .map(|made| {
                made.iter()
                    .filter(|(at, _)| *at <= cutoff)
                    .map(|(_, s)| s.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Keeps the suggestions `boss`'s quiet check-in made since `since`
    /// for its next update.
    pub(super) async fn keep_quiet_suggestions(&self, boss: &str, since: DateTime<Utc>) {
        let made = match self.desk.store().suggestions_since(boss, since).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "reading a quiet check-in's suggestions failed");
                return;
            }
        };
        if made.is_empty() {
            return;
        }
        let now = Utc::now();
        let mut kept = self.quiet_made.lock();
        let list = kept.entry(boss.to_string()).or_default();
        for s in made {
            if !list.iter().any(|(_, k)| k.id == s.id) {
                list.push((
                    now,
                    workforce_desk::crew::MentionedSuggestion {
                        id: s.id,
                        text: s.text,
                        state: s.state,
                    },
                ));
            }
        }
    }

    /// The person was told `p`: its restated items counted, the
    /// suggestions it carried let go, and the state moved on as of what it
    /// was written from.
    async fn told(
        &self,
        service: &AgentService,
        owner: &str,
        state: &UpdateState,
        p: &Pending,
    ) -> eyre::Result<()> {
        let store = self.desk.store();
        let now = Utc::now();
        let cutoff = p.cutoff.unwrap_or(now);
        for (item, park) in &p.restated {
            if let Err(e) = store.item_said(*item, now, *park).await {
                tracing::warn!(error = %e, "counting a restated item failed");
            }
        }
        if let Some(made) = self.quiet_made.lock().get_mut(service.agent_id.as_str()) {
            made.retain(|(at, _)| *at > cutoff);
        }
        let read = service
            .sessions
            .desk_read(owner, DESK_CHANNEL)
            .ok()
            .flatten()
            .map(|r| r.at);
        // The last one unread as this one goes: one more in a row.
        let unread = update::streak(state.unread, state.last_update_at, read) + 1;
        store
            .update_sent(service.agent_id.as_str(), cutoff, &p.told, unread)
            .await
    }
}

/// What a check-in's prompt names a handed update by.
fn marker(p: &Pending) -> String {
    format!("update {}", p.id)
}

/// A boss's update being looked at, by one look at a time; let go when
/// dropped.
pub(super) struct Updating<'a>(
    &'a parking_lot::Mutex<std::collections::HashSet<String>>,
    String,
);

impl<'a> Updating<'a> {
    fn begin(
        set: &'a parking_lot::Mutex<std::collections::HashSet<String>>,
        boss: &str,
    ) -> Option<Self> {
        set.lock()
            .insert(boss.to_string())
            .then(|| Self(set, boss.to_string()))
    }
}

impl Drop for Updating<'_> {
    fn drop(&mut self) {
        self.0.lock().remove(&self.1);
    }
}

/// Looks at the bosses' updates every minute: an update gathers for
/// minutes, so a finer look buys nothing.
pub(super) async fn updates_loop(crew: Arc<CrewHub>, cancel: CancellationToken) {
    let mut every = tokio::time::interval(crew.timing.minute);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = every.tick() => crew.look_at_updates().await,
        }
    }
}
