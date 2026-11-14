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
//!   in the boss's voice ([`UpdateWriter`]), and the short line and why of
//!   the newest decisions it acted on (never quoting the message it sent;
//!   one that doesn't fit is the row's). The text's decision lines, what
//!   it kept and what still waits are the rows' own words. Off, failed or
//!   past the day's ceiling, all of it is written plainly from the rows.
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
use workforce_desk::decision::{BossDecision, DecisionKind, DecisionLine, Need};
use workforce_desk::model::SessionState;
use workforce_desk::open_item::{OpenItem, OpenKind, OpenState};
use workforce_desk::update::{
    self, Change, MentionedUpdate, NeedsYou, Pending, Told, UpdateSession, UpdateState, WaitingItem,
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
pub(super) struct Changed {
    pub id: Uuid,
    pub name: String,
    account: Account,
    new: Vec<Done>,
}

/// What one boss's next update would say now.
pub(super) struct Batch {
    pub change: Change,
    pub sessions: Vec<Changed>,
    pub decisions: Vec<BossDecision>,
    pub kept: Vec<String>,
    waiting: Vec<OpenItem>,
    /// What only the person can do, still open: the brief's "for you".
    pub for_you: Vec<OpenItem>,
    restated: Vec<(Uuid, bool)>,
    pub told: HashMap<Uuid, Told>,
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
    pub(super) async fn batch(
        &self,
        service: &AgentService,
        o: &Oversight,
        state: &UpdateState,
        now: DateTime<Utc>,
        away: Option<DateTime<Utc>>,
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
            let mut c = update::changed(was, a, state.since);
            let mut new: Vec<Done> = update::done_since(was, a, state.since)
                .into_iter()
                .cloned()
                .collect();
            // A catch-up says all that was done while the person was away,
            // told in an update they didn't read or not.
            if let Some(from) = away {
                let all = update::done_since(None, a, from.max(state.since));
                if !all.is_empty() {
                    c = c.max(match all.iter().any(|d| update::is_result(d)) {
                        true => Change::Result,
                        false => Change::Small,
                    });
                    new = all.into_iter().cloned().collect();
                }
            }
            told.insert(v.id, Told::of(a));
            if c == Change::None {
                continue;
            }
            change = change.max(c);
            sessions.push(Changed {
                id: v.id,
                name: v.label().to_string(),
                new,
                account: a.clone(),
            });
        }
        let mut last = state.last_update_at.unwrap_or(state.since);
        if let Some(from) = away {
            last = last.min(from);
        }
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
        let kept: Vec<String> = match composed {
            true => store
                .items_closed_since(boss, last, 200)
                .await
                .unwrap_or_default()
                .iter()
                .filter(|i| i.closed_at.is_some_and(|at| at > last && at <= now))
                .filter_map(super::items::kept_line)
                .collect(),
            false => Vec::new(),
        };
        let gap = self.answer_gap(o);
        let open: Vec<OpenItem> = store
            .items_not_closed(Some(boss))
            .await
            .unwrap_or_default()
            .into_iter()
            // The person in that session now: theirs, nothing said of it,
            // nor counted as said again.
            .filter(|i| {
                !matches!(i.kind, OpenKind::Ask | OpenKind::Confirm)
                    || i.session.is_none_or(|s| !self.person_in_for(s, i.opened_at, gap))
            })
            .collect();
        let restated: Vec<(Uuid, bool)> = match composed {
            true => self
                .due_again(&open, o, now)
                .into_iter()
                .map(|(i, _, park)| (i.id, park))
                .collect(),
            false => Vec::new(),
        };
        let for_you: Vec<OpenItem> = open
            .iter()
            .filter(|i| i.kind == OpenKind::Todo && i.state == OpenState::Open && i.said())
            .cloned()
            .collect();
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
            for_you,
            restated,
            told,
        }
    }

    /// The session paragraphs and decision lines of `b`: the cheap
    /// model's in the boss's voice when it is on and may be asked, else
    /// plainly from the rows; any it leaves out (or writes out of bounds)
    /// plainly too.
    pub(super) async fn composed(
        &self,
        boss: &str,
        owner: &str,
        o: &Oversight,
        b: &Batch,
    ) -> (Vec<UpdateSession>, Vec<DecisionLine>, Option<String>) {
        let mut written = update::Composed::default();
        let writer = self.update_writer.lock().clone();
        // Only what it did is worth the model's words, the newest first: the
        // rest read as their rows.
        let mut acted: Vec<BossDecision> = b
            .decisions
            .iter()
            .rev()
            .filter(|d| d.kind == DecisionKind::Acted)
            .take(update::COMPOSED_DECISIONS)
            .cloned()
            .collect();
        acted.reverse();
        let something = !b.sessions.is_empty() || !acted.is_empty();
        if let (true, Some(writer), true) = (o.updates.enabled, writer, something) {
            if self.desk.update_take().await.unwrap_or(false) {
                let input: Vec<(Uuid, String, &Account, Vec<&Done>)> = b
                    .sessions
                    .iter()
                    .map(|s| (s.id, s.name.clone(), &s.account, s.new.iter().collect()))
                    .collect();
                let mut user = update::composer_input(&input);
                user.push_str(&update::composer_decisions(&acted));
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
                // Over its bounds: asked once more for those fields alone,
                // then whatever is still long is clipped.
                let long = update::too_long(&written);
                if !long.is_empty() && self.desk.update_take().await.unwrap_or(false) {
                    user.push_str(&update::reask(&long));
                    match writer
                        .write(boss, owner, update::COMPOSER_SYSTEM, &user)
                        .await
                        .map(|a| update::parse_composed(&a))
                    {
                        Ok(Some(again)) => {
                            written = update::merge_composed(std::mem::take(&mut written), again)
                        }
                        Ok(None) => tracing::info!(
                            "the update's writer answered a re-ask out of form; clipped"
                        ),
                        Err(e) => {
                            tracing::info!(error = %format!("{e:#}"), "the update's writer failed a re-ask; clipped")
                        }
                    }
                }
            }
        }
        let sessions = b
            .sessions
            .iter()
            .map(|s| {
                let new: Vec<&Done> = s.new.iter().collect();
                UpdateSession {
                    session_id: s.id.to_string(),
                    name: s.name.clone(),
                    purpose: s.account.purpose.trim().to_string(),
                    text: written
                        .sessions
                        .remove(&s.id)
                        .map(|t| workforce_desk::crew::words(&t, update::SESSION_WORDS))
                        .unwrap_or_else(|| update::plain_session(&new, &s.account)),
                    refs: update::refs_of(&new),
                }
            })
            .collect();
        let decisions = update::decision_lines(&b.decisions, &written.decisions);
        let headline = written.headline.as_deref().and_then(update::headline_line);
        (sessions, decisions, headline)
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
        let (sessions, decisions, headline) = self.composed(boss, owner, o, b).await;
        let full = self.level_now(boss, o).0 == workforce_common::oversight::Level::Full;
        let footer = match self.desk.cheap_today().await {
            Ok((spent, cap)) => update::footer(spent, cap),
            Err(_) => None,
        };
        let names: HashMap<Uuid, String> = self
            .desk
            .list()
            .into_iter()
            .map(|v| (v.id, v.label().to_string()))
            .collect();
        let (brief, more_items) = update::brief(&b.decisions, &decisions, &b.for_you, &names);
        let mut u = MentionedUpdate {
            sessions,
            decisions,
            kept: b.kept.clone(),
            waiting: b
                .waiting
                .iter()
                .map(|i| WaitingItem::of(i, now, full))
                .collect(),
            needs_you: b
                .waiting
                .iter()
                .map(|i| NeedsYou::of(i, now, full))
                .collect(),
            footer,
            headline: String::new(),
            brief,
            more_items,
        };
        u.headline = headline.unwrap_or_else(|| update::plain_headline(&u, b.for_you.len()));
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
            // What the person settled in a session themselves is no longer
            // theirs to answer here.
            self.settle_in_sessions(&boss).await;
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
        let dm = self.dm_look(service, &owner, o);
        // While the person types to it, or its reply is being written,
        // nothing of this goes out: the next look sends it.
        let held = self.update_held(boss, dm.session.as_deref());
        let mut state = state;
        if let Some(p) = state.pending.clone() {
            // An update that didn't go before the person went away (a
            // failed send, a restart): the catch-up covers it, rather than
            // a second message for one return.
            let away = state.away_since.is_some() || dm.away.is_some();
            if away && p.away.is_none() && p.sent.is_none() && !p.handed {
                store.update_unpend(boss).await?;
                state.pending = None;
            } else if held && p.sent.is_none() {
                self.poke_at_release(boss);
                return Ok(());
            } else {
                return self.resume(service, &owner, o, &state, p).await;
            }
        }
        // Away: what changes gathers for their return. Back: one catch-up.
        let away = match (state.away_since, dm.away) {
            (None, Some(since)) => {
                store.update_away(boss, since).await?;
                true
            }
            (Some(_), Some(_)) => true,
            (Some(since), None) if !held => {
                return self.catch_up(service, &owner, o, &state, since).await;
            }
            (Some(_), None) => {
                self.poke_at_release(boss);
                return Ok(());
            }
            (None, None) => false,
        };
        let b = self.batch(service, o, &state, now, None).await;
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
        if away || held {
            self.poke_at_release(boss);
            return Ok(());
        }
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
            away: None,
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
        // Away, it gathers for their catch-up instead.
        if state.away_since.is_some() {
            return None;
        }
        let p = match state.pending.clone() {
            Some(p) if p.handed && p.sent.is_none() => p,
            Some(_) => return None,
            None => {
                let b = self.batch(service, o, &state, now, None).await;
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
            match self.unbound_given(service.agent_id.as_str(), o) {
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
        // The check-in's turn is the crew's housekeeping, never on the
        // Floor: what it said is, briefly.
        if let Some(dm) = service.sessions.find(DESK_CHANNEL, owner).ok().flatten() {
            self.on_floor(service, owner, &dm.id, &p);
        }
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
    pub(super) async fn send(
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
            away: p.away.clone(),
            suggestions: self.quiet_suggestions(boss, &p),
            ..Default::default()
        };
        match found {
            Some(m) => self.keep_extras_of(&m, boss, &extras).await,
            None => {
                let kind = match p.away {
                    Some(_) => DeliveryKind::Away,
                    None => DeliveryKind::Update,
                };
                let mut delivery = crate::desk_surface::Delivery::text(kind, p.text.clone());
                delivery.request_id = Some(id);
                // Its extras kept first: the update is read whole the
                // moment it is in the DM.
                let delivered = self
                    .deliver_with_extras(service, owner, delivery, &extras)
                    .await?;
                self.on_floor(service, owner, &delivered.session_id, &p);
                let message = delivered.message_id;
                // Sent: kept so, before anything else, so a restart from
                // here on finds it whatever the DM did since.
                p.sent = Some(message);
                self.desk.store().update_pend(boss, &p).await?;
            }
        }
        self.told(service, owner, state, &p).await
    }

    /// The Floor told of `p` as it went to `owner`'s DM `session_id`: the
    /// boss's purpose and where things stand, from its brief, in plain
    /// words and no ids; what was settled since and a session the person
    /// is in now left out. Never the update's prose.
    fn on_floor(&self, service: &AgentService, owner: &str, session_id: &str, p: &Pending) {
        #[cfg(feature = "postgres")]
        if let Some(log) = crate::floor::turns() {
            let now = Utc::now();
            let hidden = |s: &str| s.parse::<Uuid>().is_ok_and(|s| self.person_in(s, now));
            use workforce_desk::update_brief as brief;
            let shown = brief::shown(&p.update, &hidden);
            // All of it about sessions the person is in: nothing is said.
            let hid = brief::hides(&p.update, &shown);
            if !brief::says_anything(&shown) && (p.away.is_none() || hid) {
                return;
            }
            // A catch-up's own headline, unless it was written over what
            // is no longer said.
            let away = p.away.as_ref().map(|a| match hid {
                true => "",
                false => a.headline.as_str(),
            });
            let (purpose, state) = brief::floor_line(&shown, away);
            let state = workforce_common::redact::redact_secrets(&state);
            crate::floor::agents::briefed(
                log.as_ref(),
                service.agent_id.as_str(),
                owner,
                session_id,
                &purpose,
                &state,
            );
        }
        #[cfg(not(feature = "postgres"))]
        let _ = (service, owner, session_id, p);
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
        // A catch-up ends the absence it covered, once.
        if let Some(a) = &p.away {
            store
                .update_back(service.agent_id.as_str(), a.since)
                .await?;
        }
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
            _ = every.tick() => {
                // Held needs-you a restart left: theirs by now.
                crew.post_due_cards().await;
                crew.look_at_updates().await;
            }
            // The person paused typing, came back, or wrote: look now.
            () = crew.update_poke.notified() => crew.look_at_updates().await,
        }
    }
}
