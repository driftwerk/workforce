//! The boss closes the sessions it runs once their work is done
//! (`[oversight.work] close_finished`, on unless the person turns it off),
//! so they leave the grid and the sidebar instead of hanging around: each
//! is closed in Desk (`Desk::end`) and removed from it as the person's
//! Remove does (`Desk::remove_closed`), off the closed list too, its
//! conversation and folder kept: never deleted.
//!
//! - **Done** is the work finished: its pull request merged on GitHub (as
//!   the person reads it, [`super::github`]) with none still open, or every
//!   ticket it was given done, or its boss judged its last turn done at Act
//!   fully ([`super::idle`]), or it sat a day with nothing open.
//! - **Never the person's.** Only sessions in a boss's charge that it
//!   started, or (at Act fully) that the person handed it: never one it
//!   took on by itself, one the person typed into lately or holds the
//!   wheel of, one in a Desk terminal or another terminal, or one asking
//!   or waiting on anything.
//! - **Nothing lost.** Its folder is checked first: uncommitted files or
//!   commits on no remote (one whose merged pull request's head is its
//!   HEAD is pushed) keep it open, and the boss says so once.
//! - **Levels.** At Act fully it closes; at Act it closes the ones it
//!   started and suggests the rest; below, it suggests. Each close is a
//!   decision whose Undo brings it back and resumes it, and the boss's next
//!   update says them in one line ([`workforce_desk::update::closed_line`]).
//! - **Asked.** The person asking it to close what is finished, the boss
//!   closes and removes with `crew.close_session` ([`TOOL`]): at Act fully
//!   one it may end runs, anything else is a suggestion, and every close
//!   of a boss's, swept or asked, is held by [`CrewHub::close_held`].

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_common::oversight::{Level, Oversight};
use workforce_desk::account::RefKind;
use workforce_desk::board::TicketState;
use workforce_desk::charge::How;
use workforce_desk::crew::{By, MentionedSuggestion};
use workforce_desk::decision::{BossDecision, How as Decided, Need};
use workforce_desk::model::{Origin, SessionState, SessionView};
use workforce_desk::update::{ClosedKind, ClosedSession};

use super::actions::Action;
use super::github_seen::Pull;
use super::workspace::{self, folder, held, origin_of, pull_ref};
use super::{name_of, AgentService, CrewHub};

/// The action's tool name: a suggestion to close a session holds it.
pub(crate) const TOOL: &str = "crew.close_session";

/// How often, in the crew's minutes, the sessions are looked at.
const EVERY_MINUTES: u32 = 5;
/// How long a session rests before it is closed: what it did is seen first.
pub(crate) const SETTLE: chrono::Duration = chrono::Duration::minutes(10);
/// How long a session sits with nothing open before it is closed as done.
const IDLE_HOURS: i64 = 24;
/// How long the person's last input keeps a session theirs here.
const HANDS_OFF_HOURS: i64 = 2;
/// How long the person's last input keeps a session theirs from any close
/// of its boss's, asked for or not: they are in it.
const PERSON_NOW_MINUTES: i64 = 15;
/// The most pull requests one session's account names that are read.
const PULLS_MAX: usize = 5;

/// Why a session's work is done, as the person reads it after a dash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Finished {
    Merged(u64),
    Ticket,
    Said(String),
    Idle,
}

impl Finished {
    fn why(&self) -> String {
        match self {
            Self::Merged(n) => format!("Its PR #{n} merged."),
            Self::Ticket => "Its ticket is done.".into(),
            Self::Said(why) => {
                let why = workforce_desk::crew::one_line(why, 160);
                match why.trim() {
                    "" => "It finished its work.".into(),
                    w => format!("It finished its work: {}", w.trim_end_matches('.')) + ".",
                }
            }
            Self::Idle => "It sat a day with nothing left to do.".into(),
        }
    }
}

/// What the closer keeps between looks.
#[derive(Default)]
pub(crate) struct Closing {
    /// The sessions their Act fully boss judged done, at which prompt, and
    /// why.
    said_done: HashMap<Uuid, (u64, String)>,
    /// What was said of each session already, never said again: its
    /// suggestion, or why it was left open.
    told: HashSet<(Uuid, String)>,
    /// What each boss's next update says of the finished sessions it would
    /// close or left open, and when each was noted.
    notes: HashMap<String, Vec<(DateTime<Utc>, ClosedKind, ClosedSession)>>,
}

/// What the boss does with a finished session, by how it came to run it
/// and its level now; none, nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Do {
    Close,
    Suggest,
}

/// The table: one it started is closed at Act and Act fully, one handed
/// to it only at Act fully, and suggested where it isn't closed; one it
/// took on by itself is the person's own, never closed; at Watch it only
/// looks.
pub(crate) fn what_to_do(level: Level, how: How) -> Option<Do> {
    match (how, level) {
        (_, Level::Watch) | (How::Taken, _) => None,
        (How::Started, Level::Act | Level::Full) | (How::Handed, Level::Full) => Some(Do::Close),
        (How::Started | How::Handed, _) => Some(Do::Suggest),
    }
}

/// Whether a session in `v`'s state may be closed now: at rest, started
/// by Desk and not in a terminal, nothing asked or waiting, rested
/// `settle` ([`SETTLE`]).
pub(crate) fn at_rest(v: &SessionView, now: DateTime<Utc>, settle: chrono::Duration) -> bool {
    matches!(
        v.state,
        SessionState::Done | SessionState::Idle | SessionState::Stopped | SessionState::Interrupted
    ) && v.origin == Origin::Desk
        && v.terminal_id.is_none()
        && v.pending.is_none()
        && v.asked.is_none()
        && now - v.updated_at >= settle
}

/// Why a session's work is done, from what is known of it: any ticket
/// still open, or pull request still open, is work left; then a merged
/// pull request, its tickets all done, or (when every pull request was
/// read) its boss's word or a day idle.
pub(crate) fn finished(
    tickets: &[TicketState],
    pulls: &[(u64, Option<Pull>)],
    said: Option<&str>,
    idle: bool,
) -> Option<Finished> {
    let open_ticket = tickets
        .iter()
        .any(|t| !matches!(t, TicketState::Done | TicketState::Dropped));
    if open_ticket
        || pulls
            .iter()
            .any(|(_, p)| p.as_ref().is_some_and(|p| p.open))
    {
        return None;
    }
    if let Some((n, _)) = pulls
        .iter()
        .find(|(_, p)| p.as_ref().is_some_and(|p| p.merged))
    {
        return Some(Finished::Merged(*n));
    }
    if tickets.contains(&TicketState::Done) {
        return Some(Finished::Ticket);
    }
    // A pull request GitHub couldn't say may be open still: neither its
    // boss's word nor a quiet day closes it.
    if pulls.iter().any(|(_, p)| p.is_none()) {
        return None;
    }
    if let Some(why) = said {
        return Some(Finished::Said(why.to_string()));
    }
    idle.then_some(Finished::Idle)
}

/// Whether closing `v` and removing it loses nothing now: at rest, asking
/// nothing, and nothing uncommitted or unpushed in its folder. Read for a
/// close the person said yes to, which no boss's gate holds.
pub(crate) async fn nothing_to_lose(v: &SessionView) -> bool {
    v.pending.is_none()
        && v.asked.is_none()
        && !matches!(
            v.state,
            SessionState::Starting
                | SessionState::Working
                | SessionState::Blocked
                | SessionState::Asking
        )
        && held(&folder(&v.cwd).await, &[]).is_none()
}

/// Looks at the bosses' finished sessions every few minutes.
pub(super) async fn closing_loop(crew: Arc<CrewHub>, cancel: CancellationToken) {
    let mut every = tokio::time::interval(crew.timing.minute * EVERY_MINUTES);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = every.tick() => crew.look_at_finished_sessions().await,
        }
    }
}

impl CrewHub {
    /// Notes that `boss` judged `view`'s last turn done (at Act fully): the
    /// session said it is finished.
    pub(super) fn said_done(&self, view: &SessionView, why: &str) {
        self.closing
            .lock()
            .said_done
            .insert(view.id, (view.prompts, why.to_string()));
    }

    /// The notes on finished sessions `boss`'s next update says, noted by
    /// `cutoff`, by kind.
    pub(super) fn closing_notes(
        &self,
        boss: &str,
        cutoff: DateTime<Utc>,
    ) -> Vec<(ClosedKind, ClosedSession)> {
        self.closing
            .lock()
            .notes
            .get(boss)
            .map(|n| {
                n.iter()
                    .filter(|(at, _, _)| *at <= cutoff)
                    .map(|(_, k, s)| (*k, s.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `boss`'s update said what was noted by `cutoff`: let go.
    pub(super) fn closing_told(&self, boss: &str, cutoff: DateTime<Utc>) {
        if let Some(n) = self.closing.lock().notes.get_mut(boss) {
            n.retain(|(at, _, _)| *at > cutoff);
        }
    }

    /// Whether `boss` has notes on finished sessions for its next update.
    pub(super) fn has_closing_notes(&self, boss: &str) -> bool {
        self.closing
            .lock()
            .notes
            .get(boss)
            .is_some_and(|n| !n.is_empty())
    }

    /// Looks once at every boss's finished sessions: closes, suggests or
    /// says what keeps each open, as its level and settings say.
    pub(crate) async fn look_at_finished_sessions(&self) {
        if !self.desk.charges_loaded() {
            return;
        }
        // One look at a time: two would close a session twice.
        let Ok(_one) = self.closing_now.try_lock() else {
            return;
        };
        let views = self.desk.list();
        let now = Utc::now();
        let settle = *self.close_settle.lock();
        {
            // Only what is still there and still at that turn is kept.
            let open: HashMap<Uuid, u64> = views
                .iter()
                .filter(|v| v.state != SessionState::Closed)
                .map(|v| (v.id, v.prompts))
                .collect();
            let mut c = self.closing.lock();
            c.said_done
                .retain(|id, (prompts, _)| open.get(id) == Some(prompts));
            c.told.retain(|(id, _)| open.contains_key(id));
        }
        for (service, o) in self.overseers() {
            if !o.work.close_finished {
                continue;
            }
            let boss = service.agent_id.as_str().to_string();
            let level = self.level_now(&boss, &o).0;
            let mine: Vec<(&SessionView, Do, How)> = views
                .iter()
                .filter_map(|v| {
                    let c = self.desk.charge_of(v.id)?;
                    (c.live() && c.boss == boss && at_rest(v, now, settle))
                        .then(|| what_to_do(level, c.how))
                        .flatten()
                        .map(|d| (v, d, c.how))
                })
                .collect();
            if mine.is_empty() {
                continue;
            }
            let asked: HashSet<Uuid> = match self.desk.store().items_not_closed(Some(&boss)).await {
                Ok(items) => items.iter().filter_map(|i| i.session).collect(),
                // What waits on the person isn't known: nothing is closed.
                Err(e) => {
                    tracing::info!(%boss, error = %format!("{e:#}"), "the open items couldn't be read; no session is closed this time");
                    continue;
                }
            };
            for (v, what, how) in mine {
                if asked.contains(&v.id) {
                    continue;
                }
                self.look_at_done(&service, &o, v, what, how).await;
            }
        }
    }

    /// Looks at `v`, one `service` runs, and does `what` when its work is
    /// done and nothing would be lost.
    async fn look_at_done(
        &self,
        service: &Arc<AgentService>,
        o: &Oversight,
        v: &SessionView,
        what: Do,
        how: How,
    ) {
        let now = Utc::now();
        let boss = service.agent_id.as_str();
        // The person in it lately, or at its wheel: theirs.
        let hands_off = chrono::Duration::hours(HANDS_OFF_HOURS);
        if self.person_in_for(v.id, now - hands_off, self.answer_gap(o)) {
            return;
        }
        // Stopped, or Desk started again since: what the person typed in it
        // is no longer known, so it rests the whole while first.
        if !v.live && now - v.updated_at < hands_off {
            return;
        }
        // The person reopened it after a close: theirs to close.
        match self.desk.store().close_undone(v.id).await {
            Ok(false) => {}
            Ok(true) => return,
            Err(e) => {
                tracing::info!(session = %v.id, error = %format!("{e:#}"), "whether a close of a session was undone couldn't be read; it isn't closed");
                return;
            }
        }
        let tickets: Vec<TicketState> = match self.desk.store().session_tickets(v.id).await {
            Ok(t) => t.into_iter().map(|t| t.state).collect(),
            Err(e) => {
                tracing::info!(session = %v.id, error = %format!("{e:#}"), "a session's tickets couldn't be read; it isn't closed");
                return;
            }
        };
        let said = {
            let c = self.closing.lock();
            c.said_done
                .get(&v.id)
                .filter(|(p, _)| *p == v.prompts)
                .map(|(_, w)| w.clone())
        };
        let a_day = chrono::Duration::hours(IDLE_HOURS);
        // Only one it started itself is closed for being quiet.
        let idle = how == How::Started
            && now - v.updated_at >= a_day
            && self
                .desk
                .person_input_at(v.id)
                .is_none_or(|at| now - at >= a_day);
        let pulls = self.pulls_of(service, v).await;
        let Some(why) = finished(&tickets, &pulls, said.as_deref(), idle) else {
            return;
        };
        let merged: Vec<String> = pulls
            .iter()
            .filter_map(|(_, p)| p.as_ref().filter(|p| p.merged))
            .filter_map(|p| p.head.clone())
            .collect();
        let name = v.label().to_string();
        let session = ClosedSession {
            session_id: v.id.to_string(),
            name: name.clone(),
            why: workforce_desk::decision::clause(&why.why()),
            decision_id: None,
            undo: None,
            suggestion_id: None,
        };
        if let Some(kept) = held(&folder(&v.cwd).await, &merged) {
            self.note_once(
                boss,
                v.id,
                "open",
                ClosedKind::KeptOpen,
                ClosedSession {
                    why: Some(kept),
                    ..session
                },
            );
            return;
        }
        // Looked at again just now: still where it was.
        let still = self.desk.view(v.id).is_ok_and(|now_v| {
            now_v.prompts == v.prompts && now_v.state == v.state && now_v.terminal_id.is_none()
        }) && !self.person_in_for(v.id, Utc::now() - hands_off, self.answer_gap(o));
        if !still {
            return;
        }
        let action = Action::Close { session: v.id };
        let by = By::manager(boss, &name_of(service), &why.why());
        match what {
            Do::Close => match self.perform(&action, by).await {
                Ok(done) => {
                    tracing::info!(session = %v.id, %boss, %done, "a boss closed a finished session");
                    self.closing.lock().said_done.remove(&v.id);
                }
                Err(refused) => {
                    tracing::info!(session = %v.id, %boss, %refused, "a finished session couldn't be closed");
                    // Still stopping, it closes once it has: not left open.
                    if self
                        .desk
                        .view(v.id)
                        .map_or(true, |now_v| now_v.state != v.state)
                    {
                        return;
                    }
                    self.note_once(
                        boss,
                        v.id,
                        "refused",
                        ClosedKind::KeptOpen,
                        ClosedSession {
                            why: Some(workforce_desk::crew::words(
                                &refused,
                                workforce_desk::decision::WHY_WORDS,
                            )),
                            ..session
                        },
                    );
                }
            },
            Do::Suggest => {
                if self.closing.lock().told.contains(&(v.id, "suggest".into()))
                    || self.close_suggested(v.id).await
                {
                    return;
                }
                let input = json!({ "session_id": v.id.to_string() });
                match self.suggest(TOOL, &input, &action, &by).await {
                    Ok(s) => {
                        self.quiet_made
                            .lock()
                            .entry(boss.to_string())
                            .or_default()
                            .push((
                                Utc::now(),
                                MentionedSuggestion {
                                    id: s.id.clone(),
                                    text: s.text.clone(),
                                    state: s.state,
                                },
                            ));
                        self.note_once(
                            boss,
                            v.id,
                            "suggest",
                            ClosedKind::Suggested,
                            ClosedSession {
                                suggestion_id: Some(s.id),
                                ..session
                            },
                        );
                    }
                    Err(e) => {
                        tracing::info!(session = %v.id, %boss, error = %e, "suggesting a finished session be closed failed")
                    }
                }
            }
        }
    }

    /// Why `boss` may not close `v` now, whichever way its close came (its
    /// sweep, or its own call when the person asked it to close what is
    /// finished): still at work, asking, waiting on the person, the
    /// person's now, or with work in its folder a close would leave behind.
    /// None: it may. Whether it is the boss's to close at all (it started
    /// it or was handed it) is Desk's to say, as it ends it.
    pub(super) async fn close_held(&self, v: &SessionView, boss: &str) -> Option<String> {
        if v.origin != Origin::Desk {
            return Some("It runs in another terminal, so it's the person's to close.".into());
        }
        let Some((service, o)) = self.overseer_named(boss) else {
            return Some("No boss runs it now.".into());
        };
        // At Act fully a request the boss decides itself is cancelled as it
        // closes ([`Self::cancel_to_close`]); Desk refuses any other.
        let cancels = v.pending.is_some() && self.level_now(boss, &o).0 == Level::Full;
        if v.asked.is_some() || v.state == SessionState::Asking {
            return Some(
                "It's asking something of its own: that's the person's to answer before it is closed."
                    .into(),
            );
        }
        if v.pending.is_some() && !cancels {
            return Some(
                "It's asking something: answer it, or ask the person, before it is closed.".into(),
            );
        }
        if matches!(v.state, SessionState::Starting | SessionState::Working)
            || (v.state == SessionState::Blocked && !cancels)
        {
            return Some("It's still working: it is closed only once its turn has ended.".into());
        }
        match self.hold_on(v.id).await {
            Ok(None) => {}
            Ok(Some(_)) => {
                return Some(
                    "It's held on the person's directive: it stays open until its hold ends."
                        .into(),
                )
            }
            Err(why) => return Some(why),
        }
        // Rested first, as the closer's own: what it did is seen.
        if !cancels && !at_rest(v, Utc::now(), *self.close_settle.lock()) {
            return Some(format!(
                "Its turn ended under {} minutes ago: it is closed once it has rested.",
                self.close_settle.lock().num_minutes().max(1)
            ));
        }
        let now = chrono::Duration::minutes(PERSON_NOW_MINUTES);
        if self.person_in_for(v.id, Utc::now() - now, self.answer_gap(&o)) {
            return Some("The person is in it now, so it's theirs to close.".into());
        }
        match self.desk.store().items_not_closed(Some(boss)).await {
            Ok(items) if items.iter().any(|i| i.session == Some(v.id)) => {
                return Some(
                    "It waits on the person for something: it stays open until that's answered."
                        .into(),
                )
            }
            Ok(_) => {}
            Err(e) => {
                tracing::info!(session = %v.id, error = %format!("{e:#}"), "the open items couldn't be read; the session isn't closed");
                return Some(
                    "What waits on the person couldn't be read, so it stays open for now.".into(),
                );
            }
        }
        // Work it was given still open is work left.
        match self.desk.store().session_tickets(v.id).await {
            Ok(t)
                if t.iter()
                    .any(|t| !matches!(t.state, TicketState::Done | TicketState::Dropped)) =>
            {
                return Some("Its ticket is still open: it stays open until that's done.".into())
            }
            Ok(_) => {}
            Err(e) => {
                tracing::info!(session = %v.id, error = %format!("{e:#}"), "a session's tickets couldn't be read; it isn't closed");
                return Some("Its tickets couldn't be read, so it stays open for now.".into());
            }
        }
        let (open, kept) = self.left_behind(&service, v).await;
        // A newer session carries its ticket, and its pull requests with it.
        let open = match self.superseded(v.id).await {
            true => Vec::new(),
            false => open,
        };
        if let Some(n) = open.first() {
            return Some(format!(
                "Its PR #{n} is still open: it stays open until that's merged or closed."
            ));
        }
        kept.map(|kept| format!("It stays open ({kept}): nothing is closed that would lose work."))
    }

    /// Whether a newer session now holds a ticket `session` was on: never
    /// one started to review its pull request, which carries no work.
    pub(crate) async fn superseded(&self, session: Uuid) -> bool {
        let tickets = self
            .desk
            .store()
            .linked_tickets(&[session])
            .await
            .unwrap_or_default();
        let newer = tickets.iter().filter_map(|t| {
            let a = t.assignee.as_ref()?;
            (!matches!(t.state, TicketState::Done | TicketState::Dropped)
                && a.kind == workforce_desk::crew::PartyKind::Session
                && a.id != session.to_string())
            .then_some(a.id.as_str())
        });
        for other in newer {
            let reviews = match other.parse::<Uuid>() {
                Ok(id) => !matches!(self.desk.store().pr_review_of(id).await, Ok(None)),
                Err(_) => false,
            };
            if !reviews {
                return true;
            }
        }
        false
    }

    /// The plan watch's bookkeeping for `boss`'s plan `tickets`, before it
    /// fills its slots: each session it runs whose ticket is done or
    /// dropped and that rests with nothing open (no request, no line
    /// message on its way) is closed, and so is each a newer session took
    /// its ticket from. One of those with work in its folder is named in
    /// Activity, once, and never closed.
    pub(super) async fn close_plan_finished(
        &self,
        tickets: &[workforce_desk::plan::PlanTicket],
        service: &Arc<AgentService>,
    ) {
        let boss = service.agent_id.as_str();
        let current: HashSet<String> = tickets.iter().filter_map(|t| t.session.clone()).collect();
        let finished = tickets
            .iter()
            .filter(|t| matches!(t.state, TicketState::Done | TicketState::Dropped))
            .filter_map(|t| t.session.as_deref()?.parse::<Uuid>().ok());
        let plan_ids: HashSet<Uuid> = tickets.iter().map(|t| t.id).collect();
        let mut superseded = Vec::new();
        for c in self.desk.charges().into_iter().filter(|c| c.boss == boss) {
            if current.contains(&c.session.to_string()) {
                continue;
            }
            let on_plan = self
                .desk
                .store()
                .linked_tickets(&[c.session])
                .await
                .is_ok_and(|ts| ts.iter().any(|t| plan_ids.contains(&t.id)));
            if on_plan && self.superseded(c.session).await {
                superseded.push(c.session);
            }
        }
        for id in finished.chain(superseded.iter().copied()) {
            let Ok(v) = self.desk.view(id) else {
                continue;
            };
            if !self
                .desk
                .charge_of(id)
                .is_some_and(|c| c.live() && c.boss == boss)
                || !matches!(v.state, SessionState::Idle | SessionState::Done)
                || v.pending.is_some()
                || v.asked.is_some()
                || self.line_on_its_way(id).await
            {
                continue;
            }
            let name = v.label().to_string();
            if superseded.contains(&id) {
                if let workspace::Workspace::Repo(r) = workspace::workspace_of(&v.cwd).await {
                    if !r.uncommitted.is_empty() {
                        if self.closing.lock().told.insert((id, "superseded".into())) {
                            let e = workforce_desk::crew::Event::new(
                                workforce_desk::crew::Kind::Told,
                                workforce_desk::crew::Party::manager(boss, &name_of(service)),
                                Some(workforce_desk::crew::Party::session(&v)),
                                format!(
                                    "kept {name} open: a newer session has its ticket, and its folder has {} uncommitted file(s) for that session to check",
                                    r.uncommitted.len()
                                ),
                            );
                            self.desk.note(e);
                        }
                        continue;
                    }
                }
            }
            let why = match superseded.contains(&id) {
                true => "A newer session has its ticket.",
                false => "Its ticket is done.",
            };
            let by = By::manager(boss, &name_of(service), why);
            match self.perform(&Action::Close { session: id }, by).await {
                Ok(done) => {
                    tracing::info!(session = %id, %boss, %done, "the plan watch closed a session")
                }
                Err(why) => {
                    tracing::debug!(session = %id, %boss, %why, "the plan watch left a session open")
                }
            }
        }
    }

    /// Whether a message to `session` on its line is still on its way.
    async fn line_on_its_way(&self, session: Uuid) -> bool {
        use workforce_desk::line::Delivery;
        self.desk
            .line_thread(session, None, None)
            .await
            .map_or(true, |ms| {
                ms.iter().any(|m| {
                    m.to_session() == Some(session)
                        && matches!(
                            m.delivery,
                            Delivery::Queued | Delivery::Handed | Delivery::Held
                        )
                })
            })
    }

    /// Cancels what `v` asks that `by`, its boss acting fully, decides
    /// itself, so it closes: each cancel kept as its decision. Desk's
    /// refusal, in its words, for anything else.
    pub(super) async fn cancel_to_close(&self, v: &SessionView, by: &By) -> Result<(), String> {
        if v.pending.is_none() {
            return Ok(());
        }
        let cancelled = self
            .desk
            .cancel_boss_pending(v.id, by)
            .await
            .map_err(|e| e.to_string())?;
        let name = v.label().to_string();
        for approval in cancelled {
            let what = format!("cancelled {name}'s request to close it");
            let why = by.reason.as_deref().unwrap_or("Its work is done.");
            let mut d = BossDecision::new(&by.party.id, Need::Approval, &what, why, Decided::Own)
                .briefly(&what, None);
            d.session = Some(v.id);
            d.need_ref = Some(approval.to_string());
            self.record(&mut d).await;
        }
        Ok(())
    }

    /// What closing `v` leaves behind, read as `service`: its pull requests
    /// still open, and why its folder keeps it open ([`held`]), its merged
    /// ones' heads counting as pushed.
    pub(super) async fn left_behind(
        &self,
        service: &Arc<AgentService>,
        v: &SessionView,
    ) -> (Vec<u64>, Option<String>) {
        let pulls = self.pulls_of(service, v).await;
        let open = pulls
            .iter()
            .filter(|(_, p)| p.as_ref().is_some_and(|p| p.open))
            .map(|(n, _)| *n)
            .collect();
        let merged: Vec<String> = pulls
            .iter()
            .filter_map(|(_, p)| p.as_ref().filter(|p| p.merged))
            .filter_map(|p| p.head.clone())
            .collect();
        (open, held(&folder(&v.cwd).await, &merged))
    }

    /// What closing `v` would leave, as the person reads it before they
    /// close it: `known`, with its open pull requests (read as the overseer
    /// of its scope, when it has one) and its folder's work.
    pub(super) async fn what_closing_leaves(
        &self,
        v: &SessionView,
        known: workforce_desk::Closing,
    ) -> workforce_desk::Closing {
        let (pulls, changes) = match self.overseer_of_session(v.id) {
            Some((service, _)) => self.left_behind(&service, v).await,
            None => (Vec::new(), held(&folder(&v.cwd).await, &[])),
        };
        workforce_desk::Closing {
            pulls,
            changes,
            ..known
        }
    }

    /// Notes `s` for `boss`'s next update, once for `session` and `what`.
    fn note_once(&self, boss: &str, session: Uuid, what: &str, kind: ClosedKind, s: ClosedSession) {
        let mut c = self.closing.lock();
        if !c.told.insert((session, what.to_string())) {
            return;
        }
        c.notes
            .entry(boss.to_string())
            .or_default()
            .push((Utc::now(), kind, s));
    }

    /// Whether closing `session` was suggested already: open still, or
    /// answered (a dismissed one is the person's word to leave it).
    async fn close_suggested(&self, session: Uuid) -> bool {
        let id = session.to_string();
        self.desk
            .store()
            .suggestions(200)
            .await
            .unwrap_or_default()
            .iter()
            .any(|s| {
                s.action["tool"] == TOOL
                    && s.action["input"]["session_id"] == Value::String(id.clone())
            })
    }

    /// The pull requests `v`'s account names, each as GitHub says now
    /// (none where it couldn't be read).
    pub(super) async fn pulls_of(
        &self,
        service: &Arc<AgentService>,
        v: &SessionView,
    ) -> Vec<(u64, Option<Pull>)> {
        let account = match self.desk.session_account(v.id).await {
            Ok(Some(a)) => a,
            _ => return Vec::new(),
        };
        let refs: Vec<(String, Option<String>)> = account
            .done
            .iter()
            .flat_map(|d| &d.refs)
            .filter(|r| r.kind == RefKind::Pr)
            .map(|r| (r.id.clone(), r.url.clone()))
            .collect();
        if refs.is_empty() {
            return Vec::new();
        }
        let origin = origin_of(&v.cwd).await;
        let mut seen: Vec<(String, u64)> = Vec::new();
        for (id, url) in refs {
            if let Some(p) = pull_ref(&id, url.as_deref(), origin.as_deref()) {
                if !seen.contains(&p) {
                    seen.push(p);
                }
            }
        }
        let mut out = Vec::new();
        for (repo, n) in seen.into_iter().take(PULLS_MAX) {
            out.push((n, self.pull(service, &repo, n).await));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_closes_what_it_started_at_act_and_what_it_was_handed_only_at_act_fully() {
        use How::*;
        assert_eq!(what_to_do(Level::Full, Started), Some(Do::Close));
        assert_eq!(what_to_do(Level::Full, Handed), Some(Do::Close));
        assert_eq!(what_to_do(Level::Act, Started), Some(Do::Close));
        assert_eq!(what_to_do(Level::Act, Handed), Some(Do::Suggest));
        assert_eq!(what_to_do(Level::Suggest, Started), Some(Do::Suggest));
        assert_eq!(what_to_do(Level::Suggest, Handed), Some(Do::Suggest));
        // The person's own sessions, taken on by itself, never; and at
        // Watch it only looks.
        for level in [Level::Watch, Level::Suggest, Level::Act, Level::Full] {
            assert_eq!(what_to_do(level, Taken), None, "{level:?}");
        }
        assert_eq!(what_to_do(Level::Watch, Started), None);
        assert_eq!(what_to_do(Level::Watch, Handed), None);
    }

    fn pull(open: bool, merged: bool) -> Option<Pull> {
        Some(Pull {
            open,
            merged,
            head: Some("abc".into()),
            ..Default::default()
        })
    }

    #[test]
    fn done_is_its_pr_merged_its_tickets_done_its_word_or_a_day_with_nothing_open() {
        use TicketState::*;
        assert_eq!(
            finished(&[], &[(12, pull(false, true))], None, false),
            Some(Finished::Merged(12))
        );
        assert_eq!(
            finished(&[Done, Dropped], &[], None, false),
            Some(Finished::Ticket)
        );
        assert_eq!(
            finished(&[], &[], Some("the fix shipped"), false),
            Some(Finished::Said("the fix shipped".into()))
        );
        assert_eq!(finished(&[], &[], None, true), Some(Finished::Idle));
        assert_eq!(
            finished(&[], &[], None, false),
            None,
            "nothing says it's done"
        );
        // Anything open is work left, whatever else is done.
        assert_eq!(finished(&[Done, Review], &[], Some("done"), true), None);
        assert_eq!(
            finished(
                &[Done],
                &[(3, pull(false, true)), (4, pull(true, false))],
                None,
                true
            ),
            None
        );
        // A pull request closed unmerged isn't its work merged; one GitHub
        // couldn't say leaves the rest to decide.
        assert_eq!(finished(&[], &[(5, pull(false, false))], None, false), None);
        assert_eq!(
            finished(&[Done], &[(6, None)], None, false),
            Some(Finished::Ticket)
        );
        assert_eq!(finished(&[], &[(6, None)], None, false), None);
        assert_eq!(finished(&[], &[(6, None)], Some("done"), true), None);
        assert_eq!(
            finished(&[], &[(6, None), (7, pull(false, true))], None, true),
            Some(Finished::Merged(7))
        );
    }

    #[test]
    fn its_why_reads_after_a_dash() {
        assert_eq!(Finished::Merged(12).why(), "Its PR #12 merged.");
        assert_eq!(
            Finished::Said("The fix shipped.".into()).why(),
            "It finished its work: The fix shipped."
        );
        assert_eq!(Finished::Said(" ".into()).why(), "It finished its work.");
        assert_eq!(
            workforce_desk::decision::clause(&Finished::Idle.why()).as_deref(),
            Some("it sat a day with nothing left to do")
        );
    }
}
