//! Every plan its boss runs moves, or someone is told why not (the vellev
//! plan, 19 Nov: VEL-1 in Doing with no session; VEL-5, VEL-6 and VEL-8
//! ending their turns on "I'll report when CI finishes" with nothing to
//! wake them, their tickets left open so VEL-7 never started; the person
//! told nothing all afternoon).
//!
//! Each minute, with `[oversight.work] watch` on, every running plan a
//! boss above Watch runs is looked at, ticket by ticket. Only the tickets
//! it runs: one whose session the person runs by hand (not in this boss's
//! charge) is theirs and left alone, and one on a machine is left to what
//! the machine says (one Desk can't see is unknown, never gone).
//!
//! - **In Doing, its session gone** past `orphan_grace_minutes`: its boss
//!   puts it back in To do, and the plan starts it again, its new session
//!   told to check what is already done first. Its session not running
//!   (its process gone, or its turn failed): resumed with that note, as
//!   the boss's own `crew.resume` ([`CrewHub::gate`]: below where it
//!   acts, a suggestion). Stopped or closed by someone: they may want it
//!   so, so the person is asked, once.
//! - **Its session's turn ended waiting outside** (its CI, its pull
//!   request's merge, a scheduled run): what GitHub says of its pull
//!   requests is read again every few minutes. Its checks finished on its
//!   head, the session is told the result, once. Every one merged with its
//!   ticket still open, its boss is told so in a line of its plan's
//!   turn, once: whether that was the work (Done, and the close sweep
//!   closes the session and the next tickets start) or what is left is
//!   the boss's judgement, never this code's; a wait on a scheduled run is
//!   the boss's promise to check, not a session left open.
//! - **At Act fully, its pull request green** at a head no one reviewed:
//!   Desk starts its review itself, as its boss, in a worktree detached at
//!   that head ([`super::review::step`]); once the review's turn ends its
//!   verdict is recorded and the review closed. Clean, its boss is told
//!   once a head to merge it; changes asked for, the ticket's session is
//!   given them.
//! - **Nothing moved** for `stall_minutes` while a ticket is ready or in
//!   Doing and none works: the person is told once, in plain words, what
//!   is stuck and why. A plan whose goal the person agreed is its goal's
//!   check's instead ([`super::goal_check`]), which tells its boss alone,
//!   paused or not.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use uuid::Uuid;
use workforce_common::oversight::{Level, Oversight};
use workforce_desk::board::TicketState;
use workforce_desk::charge::How;
use workforce_desk::crew::PartyKind;
use workforce_desk::model::{Origin, SessionState};
use workforce_desk::open_item::{NewItem, OpenKind};
use workforce_desk::open_item_store::Opened;
use workforce_desk::plan::PlanState;
use workforce_desk::{DeskError, PlanDetail, PlanTicketView};

use workforce_desk::plan::{PrRef, ReviewOf};

use super::actions::{Action, Gate, Target};
use super::github_seen::Pull;
use super::review::{self, short, Reviewing, Step, Unrecorded};
use super::workspace::{self, Workspace};
use super::{AgentService, CrewHub};

/// What a session started again is told first.
pub(crate) const CHECK_FIRST: &str = "Your session ended while your ticket was still in Doing, and Desk started it again. Check what is already done first (your folder, branch, commits and pull requests) and carry on from there; don't redo it.";

/// What an item asking the person about a stopped ticket's session is
/// keyed by: `plan-stopped:{ticket}:{session}`.
const STOPPED_KEY: &str = "plan-stopped:";

/// What the watchdog did, once each, by what it was about.
#[derive(Default)]
pub(crate) struct Watch {
    done: HashSet<String>,
}

/// Where one of a plan's tickets stands, as the watchdog reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Stands {
    /// Not the boss's to look after, or nothing to do.
    Fine,
    /// Its session is gone: put it back in To do.
    Gone(Uuid),
    /// Its session doesn't run: resume it.
    Down(Uuid),
    /// Its session was stopped or closed: ask the person.
    Stopped(Uuid, SessionState),
    /// Its session's turn ended with it in Doing or Review.
    Rests(Uuid),
    /// Its session works, or waits on a request.
    Busy,
}

/// The key that puts `ticket` back once each time its session went: its
/// time in Doing says which time, and it is let go of once the ticket is on
/// a session again.
fn back_key(ticket: &workforce_desk::board::Ticket) -> String {
    format!("back:{}:{}", ticket.id, ticket.state_at.timestamp_millis())
}

/// How the watchdog reads a ticket in Doing or Review whose session is
/// `session`: `view` is what Desk has of it (none, gone), `ours` whether
/// the boss runs it, `rested` whether it has been still for the grace.
pub(crate) fn stands(
    state: TicketState,
    session: Uuid,
    view: Option<&workforce_desk::model::SessionView>,
    ours: bool,
    rested: bool,
) -> Stands {
    if !ours || !matches!(state, TicketState::Doing | TicketState::Review) {
        return Stands::Fine;
    }
    let Some(v) = view else {
        return match (state, rested) {
            (TicketState::Doing, true) => Stands::Gone(session),
            _ => Stands::Fine,
        };
    };
    match v.state {
        SessionState::Working | SessionState::Starting | SessionState::Blocked => Stands::Busy,
        _ if v.pending.is_some() || v.asked.is_some() => Stands::Busy,
        SessionState::Stopped | SessionState::Closed if state == TicketState::Doing => match rested
        {
            true => Stands::Stopped(session, v.state),
            false => Stands::Fine,
        },
        SessionState::Stopped | SessionState::Closed => Stands::Fine,
        // Only one Desk runs, not one in another terminal: that is where
        // it goes on.
        _ if v.origin != Origin::Desk || v.terminal_id.is_some() => Stands::Fine,
        SessionState::Interrupted if state == TicketState::Doing && rested => Stands::Down(session),
        _ if !v.live && state == TicketState::Doing && rested => Stands::Down(session),
        SessionState::Done | SessionState::Idle if v.live => Stands::Rests(session),
        _ => Stands::Fine,
    }
}

/// What the watchdog says of a session resting on its pull requests: the
/// line to the session when its checks finished, or to its boss when
/// every one merged; none while any is pending or unread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Landed {
    /// Its checks on `head` finished: green, or `failed` failed.
    Checks { n: u64, head: String, failed: u64 },
    /// Every one merged, none open.
    Merged(Vec<(u64, Option<String>)>),
}

/// What landed of `pulls`, a resting session's pull requests as GitHub
/// last said: the first open one whose checks finished, else all merged.
pub(crate) fn landed(pulls: &[(u64, Option<Pull>)]) -> Option<Landed> {
    if pulls.is_empty() || pulls.iter().any(|(_, p)| p.is_none()) {
        return None;
    }
    let open: Vec<(u64, &Pull)> = pulls
        .iter()
        .filter_map(|(n, p)| p.as_ref().filter(|p| p.open).map(|p| (*n, p)))
        .collect();
    if !open.is_empty() {
        return open.into_iter().find_map(|(n, p)| {
            let head = p.head.clone()?;
            match p.checks.as_deref() {
                Some("success") => Some(Landed::Checks { n, head, failed: 0 }),
                Some("failure") => Some(Landed::Checks {
                    n,
                    head,
                    failed: p.failed.max(1),
                }),
                _ => None,
            }
        });
    }
    let merged: Vec<(u64, Option<String>)> = pulls
        .iter()
        .filter_map(|(n, p)| {
            p.as_ref()
                .filter(|p| p.merged)
                .map(|p| (*n, p.merged_at.clone()))
        })
        .collect();
    (!merged.is_empty()).then_some(Landed::Merged(merged))
}

/// When the plan last moved: its newest ticket change.
pub(crate) fn last_moved(tickets: &[PlanTicketView]) -> Option<DateTime<Utc>> {
    tickets.iter().map(|t| t.ticket.ticket.state_at).max()
}

impl CrewHub {
    /// One look at every plan a watching boss runs.
    pub(crate) async fn look_at_plans(&self) {
        if !self.desk.charges_loaded() {
            return;
        }
        let plans = match self.desk.plans(Some(PlanState::Running), None).await {
            Ok(p) => p,
            Err(e) => {
                tracing::info!(error = %e, "the running plans couldn't be read for the watchdog");
                return;
            }
        };
        for summary in plans {
            let p = summary.plan;
            if p.paused.is_some() {
                continue;
            }
            // Met while it was paused: closed now it runs again.
            if let Some(m) = &p.met {
                self.close_met(p.id, &m.why);
                continue;
            }
            let Some((service, o)) = self.overseer_named(&p.boss) else {
                continue;
            };
            if !o.work.watch || self.level_now(&p.boss, &o).0 == Level::Watch {
                continue;
            }
            if !self.desk.seen_by(&o.scope).sees(p.group.as_deref()) {
                continue;
            }
            let Ok(detail) = self.desk.plan(p.id).await else {
                continue;
            };
            self.watch_plan(&service, &o, &detail).await;
        }
        self.spawn_goal_checks();
    }

    /// Whether `session` is `boss`'s to look after: in its charge, started
    /// by it or handed to it. One gone has no charge: a ticket of a plan
    /// its boss runs is its own unless the person was given it.
    fn bosss_ticket(&self, boss: &str, session: Uuid, gone: bool) -> bool {
        match self.desk.charge_of(session) {
            // Gone, its charge ended with it: still the boss's, unless the
            // person took it back.
            Some(c) => {
                c.boss == boss
                    && matches!(c.how, How::Started | How::Handed)
                    && (c.live()
                        || gone
                            && c.released_by != Some(workforce_desk::charge::ReleasedBy::Person))
            }
            None => gone,
        }
    }

    async fn watch_plan(&self, service: &Arc<AgentService>, o: &Oversight, p: &PlanDetail) {
        let boss = service.agent_id.as_str();
        let full = self.level_now(boss, o).0 == Level::Full;
        let now = Utc::now();
        let minute = chrono::Duration::from_std(self.timing.minute).unwrap_or_default();
        let grace = minute * o.work.orphan_grace_minutes as i32;
        let mut busy = false;
        let mut stuck: Vec<String> = Vec::new();
        for t in &p.tickets {
            let ticket = &t.ticket.ticket;
            let key = t.ticket.key.clone().unwrap_or_else(|| "a ticket".into());
            // One on a machine is the machine's to say: in Doing, it may be
            // at work there.
            if t.machine.is_some() || t.starting {
                let stalled = ticket
                    .assignee
                    .as_ref()
                    .filter(|a| ticket.state == TicketState::Doing && a.kind == PartyKind::Session)
                    .and_then(|a| self.stall_of(&a.id, o))
                    .is_some_and(|seen| self.stuck_on(p, ticket.id, &key, &seen, &mut stuck));
                busy |= !stalled && (t.starting || ticket.state == TicketState::Doing);
                continue;
            }
            if ticket.state == TicketState::Todo && t.ticket.ready {
                stuck.push(format!("{key} is ready but hasn't started"));
                continue;
            }
            // A machine's session (`machine/id`) is the machine's to say.
            let Some(session) = ticket.session() else {
                // In Doing with no one on it (its session ended and let it
                // go): back in To do, so the plan starts it again.
                if ticket.state == TicketState::Doing && ticket.assignee.is_none() {
                    stuck.push(format!("{key} is in Doing with no session"));
                    if now - ticket.state_at >= grace {
                        self.put_back(service, o, t, &key).await;
                    }
                }
                continue;
            };
            let view = match self.desk.view(session) {
                Ok(v) => Some(v),
                Err(DeskError::NotFound) => None,
                Err(_) => continue,
            };
            // On a session again (given another within Doing, whose time
            // in Doing doesn't change): when that one goes, it goes back
            // again.
            if view.is_some() {
                self.plan_watch.lock().done.remove(&back_key(ticket));
            }
            let since = view
                .as_ref()
                .map_or(ticket.state_at, |v| v.updated_at.max(ticket.state_at));
            let rested = now - since >= grace;
            // What Desk sees inside it says it is stuck, however busy it
            // looks.
            let stalled = view
                .as_ref()
                .filter(|_| ticket.state == TicketState::Doing)
                .and_then(|v| self.stall_of(&v.id.to_string(), o))
                .is_some_and(|seen| self.stuck_on(p, ticket.id, &key, &seen, &mut stuck));
            // At work, whoever runs it: the plan moves.
            if !stalled
                && view.as_ref().is_some_and(|v| {
                    matches!(
                        v.state,
                        SessionState::Working | SessionState::Starting | SessionState::Blocked
                    )
                })
            {
                busy = true;
            }
            let ours = self.bosss_ticket(boss, session, view.is_none());
            if let (true, true, Some(v)) = (full, ours, &view) {
                busy |= self.record_reviews(service, o, p, &key, v).await;
            }
            let runs = view
                .as_ref()
                .is_some_and(|v| v.live && v.state != SessionState::Closed);
            match stands(ticket.state, session, view.as_ref(), ours, rested) {
                Stands::Busy => busy |= !stalled,
                Stands::Fine if ticket.state == TicketState::Doing && !runs => {
                    stuck.push(format!("{key} is in Doing with no session"));
                }
                Stands::Fine => {}
                Stands::Gone(s) => {
                    stuck.push(format!("{key} is in Doing, its session gone"));
                    tracing::debug!(%key, session = %s, "a ticket's session is gone");
                    self.put_back(service, o, t, &key).await;
                }
                Stands::Down(_) => {
                    stuck.push(format!("{key} is in Doing, its session not running"));
                    if let Some(v) = &view {
                        self.start_again(service, o, &key, v).await;
                    }
                }
                Stands::Stopped(s, state) => {
                    stuck.push(format!("{key} is in Doing, its session {}", state.as_str()));
                    self.ask_stopped(boss, ticket.id, &key, s, state).await;
                }
                Stands::Rests(_) => {
                    if let Some(v) = &view {
                        let settle = *self.close_settle.lock();
                        if now - v.updated_at >= settle {
                            stuck.push(format!(
                                "{key}'s session finished its turn; the ticket is still in {}",
                                ticket.state.title()
                            ));
                            if full {
                                self.review_green(service, o, p, &key, v).await;
                            }
                            self.rests_on(service, o, p, t, &key, v, full).await;
                        }
                    }
                }
            }
        }
        // An agreed goal's stall is its check's to say, to its boss alone.
        if p.summary.plan.agreed_at.is_none() {
            self.say_stall(boss, o, p, busy, &stuck, now).await;
        }
    }

    /// What Desk sees inside ticket `key`'s session (`seen`: its glance and
    /// stall), its boss's plan turn told once until the evidence changes:
    /// whether it is stuck, then said with the plan's stall. One only to
    /// look at isn't.
    fn stuck_on(
        &self,
        p: &PlanDetail,
        ticket: Uuid,
        key: &str,
        (glance, stall): &(workforce_desk::sight::Glance, workforce_desk::sight::Stall),
        stuck: &mut Vec<String>,
    ) -> bool {
        let who = format!("{key}'s session");
        let once = super::inside::evidence_key(&ticket.to_string(), glance, stall);
        if self.plan_watch.lock().done.insert(once) {
            self.wake_plan(
                p.summary.plan.id,
                super::inside::evidence_line(&who, glance, stall),
            );
        }
        let stuck_by = !super::inside::only_looks(glance, stall);
        if stuck_by {
            stuck.push(format!("{who} {}", stall.words));
        }
        stuck_by
    }

    /// Ticket `key` is in Doing with its session gone: its boss puts it
    /// back in To do, so the plan starts it again, as its own
    /// `crew.move_ticket` ([`CrewHub::gate`]: a planned ticket that ran goes
    /// back in the queue by the person, so below Act fully it is suggested
    /// to them). Once each time its session went, however a look sees it
    /// gone: a session removed is gone from Desk before the ticket's let go
    /// of it is written, so one look may see it still on the ticket and the
    /// next the ticket with no one on it.
    async fn put_back(
        &self,
        service: &Arc<AgentService>,
        o: &Oversight,
        t: &PlanTicketView,
        key: &str,
    ) {
        let boss = service.agent_id.as_str();
        let ticket = &t.ticket.ticket;
        let once = back_key(ticket);
        // Claimed before anything is awaited: two looks never both act.
        if !self.plan_watch.lock().done.insert(once.clone()) {
            return;
        }
        let input = serde_json::json!({
            "ticket": key,
            "ticket_id": t.ticket.ticket.id.to_string(),
            "state": "todo",
            "reason": format!("{key} is in Doing with no session: back in To do, its plan starts it again."),
        });
        let mut action = match Action::parse("crew.move_ticket", &input) {
            Ok(a) => a,
            Err(e) => {
                tracing::info!(%key, error = %e, "a ticket with no session couldn't be put back");
                self.plan_watch.lock().done.remove(&once);
                return;
            }
        };
        if let Err(e) = self.resolve_ticket(&mut action, None, Some(boss)).await {
            tracing::info!(%key, error = %e, "a ticket with no session couldn't be found to put back");
            self.plan_watch.lock().done.remove(&once);
            return;
        }
        let why = format!("{key}'s session is gone while it was in Doing: it starts again.");
        let by = self.by_boss(service, &why);
        let went = match self.gate(&action, o) {
            Gate::Run => {
                let by = self.by_at_level(by, o);
                self.perform(&action, by)
                    .await
                    .map(|done| tracing::info!(%key, %done, "a ticket whose session was gone went back to To do"))
            }
            Gate::Suggest(_) => self
                .suggest("crew.move_ticket", &input, &action, &by)
                .await
                .map(|_| tracing::info!(%key, "putting back a ticket whose session was gone was suggested")),
            Gate::No(why) => Err(why.into_owned()),
            Gate::Refuse => return,
        };
        if let Err(e) = went {
            tracing::info!(%key, error = %e, "a ticket whose session was gone couldn't be put back");
            self.plan_watch.lock().done.remove(&once);
        }
    }

    /// Resumes `v`, the session of ticket `key` that doesn't run, as its
    /// boss would: told to check what is already done first.
    async fn start_again(
        &self,
        service: &Arc<AgentService>,
        o: &Oversight,
        key: &str,
        v: &workforce_desk::model::SessionView,
    ) {
        // Once each time it went down: a session not running counts no
        // prompts, so when it last changed says which time.
        let once = format!("resume:{}:{}", v.id, v.updated_at.timestamp_millis());
        if !self.plan_watch.lock().done.insert(once) {
            return;
        }
        let action = Action::Resume {
            session: v.id,
            prompt: Some(CHECK_FIRST.to_string()),
        };
        let why = format!("{key} is in Doing and its session isn't running: it goes on.");
        let by = self.by_boss(service, &why);
        match self.gate(&action, o) {
            Gate::Run => {
                let by = self.by_at_level(by, o);
                match self.perform(&action, by).await {
                    Ok(done) => {
                        tracing::info!(session = %v.id, %done, "a plan's ticket's session was started again")
                    }
                    Err(e) => {
                        tracing::info!(session = %v.id, error = %e, "a plan's ticket's session couldn't be started again")
                    }
                }
            }
            Gate::Suggest(_) => {
                let input =
                    serde_json::json!({ "session_id": v.id.to_string(), "prompt": CHECK_FIRST });
                if let Err(e) = self.suggest("crew.resume", &input, &action, &by).await {
                    tracing::info!(session = %v.id, error = %e, "suggesting a plan's ticket's session go on failed");
                }
            }
            Gate::No(why) => {
                tracing::info!(session = %v.id, %why, "a plan's ticket's session isn't the boss's to start again")
            }
            Gate::Refuse => {}
        }
    }

    /// Asks the person, once, about ticket `key` in Doing whose session
    /// `session` someone stopped or closed.
    async fn ask_stopped(
        &self,
        boss: &str,
        ticket: Uuid,
        key: &str,
        session: Uuid,
        state: SessionState,
    ) {
        let store = self.desk.store();
        let dedupe = format!("{STOPPED_KEY}{ticket}:{session}");
        match store.item_ever_keyed(boss, &dedupe).await {
            Ok(false) => {}
            Ok(true) => return,
            Err(e) => {
                tracing::info!(error = %e, "whether a stopped ticket was raised couldn't be read");
                return;
            }
        }
        let text = format!(
            "{key} is in Doing but its session was {}, so its plan can't go on with it. Resume the session, or move {key} back to To do and its plan starts it again.",
            state.as_str()
        );
        let mut new = NewItem::ask(boss, OpenKind::Todo, Some(session), &text);
        new.dedupe_key = dedupe;
        new.holding_up = Some(format!("{key}, and what waits on it"));
        match store.item_open(&new).await {
            Ok(Opened::New(i)) => {
                self.post_item(boss, &i, super::items::needs_you(&i), Vec::new())
                    .await;
            }
            Ok(Opened::Again(_)) => {}
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "raising a stopped ticket to the person failed")
            }
        }
    }

    /// `v`, ticket `t`'s session, finished its turn with the ticket open:
    /// what its pull requests did since is said, once each; at Act fully
    /// (`full`), a green one with what follows.
    #[allow(clippy::too_many_arguments)]
    async fn rests_on(
        &self,
        service: &Arc<AgentService>,
        o: &Oversight,
        p: &PlanDetail,
        t: &PlanTicketView,
        key: &str,
        v: &workforce_desk::model::SessionView,
        full: bool,
    ) {
        let pulls = self.pulls_of(service, v).await;
        match landed(&pulls) {
            None => {}
            Some(Landed::Checks { n, head, failed }) => {
                // In Review it waits on its review, not CI; with work of its
                // own running, or a wait given to the person, it isn't told.
                if t.ticket.ticket.state != TicketState::Doing || v.background.is_some() {
                    return;
                }
                let once = format!("ci:{}:{head}:{}", v.id, failed > 0);
                if self.plan_watch.lock().done.contains(&once) {
                    return;
                }
                // What it waits on the person for (a wait or a question
                // given to them) is theirs to settle, not CI's line.
                match self
                    .desk
                    .store()
                    .items_not_closed(Some(service.agent_id.as_str()))
                    .await
                {
                    Ok(items)
                        if items.iter().any(|i| {
                            i.session == Some(v.id)
                                && (super::waiting::is_waiting(&i.dedupe_key)
                                    || matches!(i.kind, OpenKind::Ask | OpenKind::Confirm))
                        }) =>
                    {
                        return
                    }
                    Ok(_) => {}
                    Err(_) => return,
                }
                if !self.plan_watch.lock().done.insert(once.clone()) {
                    return;
                }
                let message = match failed {
                    0 => format!(
                        "Desk checked GitHub: CI on #{n}'s head {} is green. That is CI's result only; carry on with your ticket as it stands.{}",
                        short(&head),
                        match full {
                            true => " Desk has it reviewed at that head, and your boss merges it once the review is clean: don't merge it yourself.",
                            false => "",
                        }
                    ),
                    f => format!(
                        "Desk checked GitHub: CI on #{n}'s head {} is red ({f} failed {}). Look at the failed jobs and fix them.",
                        short(&head),
                        if f == 1 { "check" } else { "checks" }
                    ),
                };
                let action = Action::Nudge {
                    to: Target::Session(v.id),
                    message,
                    reply_to: None,
                };
                let why = format!("{key}'s session waits on CI, which finished.");
                if self.gate(&action, o) != Gate::Run {
                    // The boss is told instead, as for the merged.
                    {
                        self.wake_plan(
                            p.summary.plan.id,
                            format!(
                                "{key}'s session finished its turn waiting on CI; CI on #{n}'s head {} is now {}. Tell it with crew.nudge.",
                                short(&head),
                                if failed == 0 { "green" } else { "red" }
                            ),
                        );
                    }
                    return;
                }
                let by = self.by_at_level(self.by_boss(service, &why), o);
                match self.perform(&action, by).await {
                    Ok(_) => {
                        tracing::info!(session = %v.id, n, "a session waiting on CI was told it finished");
                    }
                    Err(e) => {
                        tracing::info!(session = %v.id, error = %e, "a session waiting on CI couldn't be told");
                        self.plan_watch.lock().done.remove(&once);
                    }
                }
            }
            Some(Landed::Merged(merged)) => {
                let once = format!(
                    "merged:{}:{}:{}",
                    v.id,
                    v.prompts,
                    t.ticket.ticket.state.as_str()
                );
                if !self.plan_watch.lock().done.insert(once) {
                    return;
                }
                let tz = self.desk.timezone();
                let said: Vec<String> = merged
                    .iter()
                    .map(|(n, at)| {
                        let at = at
                            .as_deref()
                            .and_then(|a| a.parse::<DateTime<Utc>>().ok())
                            .map(|a| format!(" at {}", a.with_timezone(&tz).format("%H:%M")))
                            .unwrap_or_default();
                        format!("#{n} merged{at}")
                    })
                    .collect();
                let line = format!(
                    "{key}: {} (checked on GitHub), and its session finished its turn; {key} is still in {}. \
                     If that was its work, move {key} to Done (crew.move_ticket): its session is closed and what \
                     waits on it starts. If something is left, tell the session what (crew.nudge). If it waits on \
                     a scheduled run (a nightly), move it on and keep the check as your own promise \
                     (crew.promise) rather than leaving the session open.",
                    said.join(", "),
                    t.ticket.ticket.state.title()
                );
                self.wake_plan(p.summary.plan.id, line);
            }
        }
    }

    /// Ticket `key`'s session `v` rests with its own pull request (its
    /// folder's branch, in its origin) open and green: reviewed, merged by
    /// its boss, or left as its review says ([`review::step`]).
    async fn review_green(
        &self,
        service: &Arc<AgentService>,
        o: &Oversight,
        p: &PlanDetail,
        key: &str,
        v: &workforce_desk::model::SessionView,
    ) {
        let boss = service.agent_id.as_str();
        let Workspace::Repo(r) = workspace::workspace_of(&v.cwd).await else {
            return;
        };
        let (Some(origin), Some(branch)) = (r.origin, r.branch) else {
            return;
        };
        let Some((owner, name)) = workforce_desk::goal::github_repo(&origin) else {
            return;
        };
        let Ok(seen) = self.branch_pull(service, &origin, &branch).await else {
            return;
        };
        let Some(pull) = seen
            .value
            .filter(|p| p.open && p.checks.as_deref() == Some("success"))
        else {
            return;
        };
        let Some(head) = pull
            .head
            .clone()
            .filter(|h| workforce_desk::plan::is_sha(h))
        else {
            return;
        };
        let Ok(number) = i64::try_from(pull.number) else {
            return;
        };
        let pr = PrRef::new(owner, name, number);
        let store = self.desk.store();
        let (Ok(newest), Ok(started)) = (
            store.pr_review_newest(boss, &pr).await,
            store.pr_reviews_at(boss, &pr, &head).await,
        ) else {
            return;
        };
        let at_head = Reviewing::of(
            started
                .iter()
                .map(|s| self.desk.view(*s).ok().map(|r| r.state)),
        );
        let id = p.summary.plan.id;
        match review::step(&head, newest.as_ref(), at_head) {
            Step::Merge => self.ready_once(id, key, &pr, &head, pull.draft),
            Step::Settled | Step::Underway => {}
            Step::Lost => self.unreviewed_once(id, key, &pr, &head, "its review ended with no verdict"),
            Step::Start => self.start_review(service, o, id, key, v, pr, head).await,
        }
    }

    /// Tells plan `id`'s turn, once a head, that ticket `key`'s `pr` is
    /// green with a clean review at `head`, for its boss to merge.
    fn ready_once(&self, id: Uuid, key: &str, pr: &PrRef, head: &str, draft: bool) {
        if self
            .plan_watch
            .lock()
            .done
            .insert(format!("merge:{}:{head}", review::named(pr)))
        {
            self.wake_plan(id, review::ready_line(key, pr, head, draft));
        }
    }

    /// Tells plan `id`'s turn, once a head, that ticket `key`'s `pr` is
    /// green at `head` with no review, and why.
    fn unreviewed_once(&self, id: Uuid, key: &str, pr: &PrRef, head: &str, why: &str) {
        if self
            .plan_watch
            .lock()
            .done
            .insert(format!("unreviewed:{}:{head}", review::named(pr)))
        {
            self.wake_plan(id, review::unreviewed_line(key, pr, head, why));
        }
    }

    /// Starts the review of ticket `key`'s `pr` at `head` as its boss, in
    /// `v`'s repository: once a head, again next look while no slot is
    /// free or it didn't start, its boss told once why it didn't.
    #[allow(clippy::too_many_arguments)]
    async fn start_review(
        &self,
        service: &Arc<AgentService>,
        o: &Oversight,
        id: Uuid,
        key: &str,
        v: &workforce_desk::model::SessionView,
        pr: PrRef,
        head: String,
    ) {
        let boss = service.agent_id.as_str();
        if self.desk.charges_busy(boss) >= o.work.parallel as usize {
            return;
        }
        let once = format!("review:{}:{head}", review::named(&pr));
        if !self.plan_watch.lock().done.insert(once.clone()) {
            return;
        }
        let action = Action::StartSession {
            folder: v.cwd.clone(),
            goal: review::goal(&pr, &head, key),
            name: Some(format!("Review {key}")),
            engine: None,
            model: None,
            view: None,
            worktree: Some(review::worktree(&pr, &head)),
            fresh: false,
            review_of: Some(ReviewOf {
                pr: pr.clone(),
                head_sha: head.clone(),
                for_session: Some(v.id),
            }),
        };
        let why = format!("{key}'s pull request is green at a head no one has reviewed.");
        let started = match self.gate(&action, o) {
            Gate::Run => {
                let by = self.by_at_level(self.by_boss(service, &why), o);
                self.perform(&action, by).await
            }
            _ => Err("you may not start sessions yourself".into()),
        };
        match started {
            Ok(done) => tracing::info!(%key, pr = %review::named(&pr), %done, "a pull request's review started"),
            Err(e) => {
                tracing::info!(%key, pr = %review::named(&pr), error = %e, "a pull request's review didn't start");
                self.plan_watch.lock().done.remove(&once);
                self.unreviewed_once(id, key, &pr, &head, &format!("Desk couldn't start one: {e}"));
            }
        }
    }

    /// Records each review of ticket `key`'s session `v` whose turn ended,
    /// once a turn, and closes it once its review is done: its verdict
    /// said, or the head it read moved on. Whether one of them works: its
    /// ticket moves.
    async fn record_reviews(
        &self,
        service: &Arc<AgentService>,
        o: &Oversight,
        p: &PlanDetail,
        key: &str,
        v: &workforce_desk::model::SessionView,
    ) -> bool {
        let boss = service.agent_id.as_str();
        let Ok(links) = self.desk.store().pr_reviews_for(v.id).await else {
            return false;
        };
        let settle = *self.close_settle.lock();
        let mut works = false;
        for l in links.into_iter().filter(|l| l.boss == boss) {
            let (Ok(r), Some(head)) = (self.desk.view(l.session), l.head_sha.as_deref()) else {
                continue;
            };
            works |= r.state.busy();
            if !super::closing::at_rest(&r, Utc::now(), settle) {
                continue;
            }
            let once = format!("recorded:{}:{}", r.id, r.prompts);
            if !self.plan_watch.lock().done.insert(once.clone()) {
                continue;
            }
            let id = p.summary.plan.id;
            match self.record_of(service, boss, r.id).await {
                Ok((done, said)) => {
                    match done.clean {
                        true => {
                            let draft = self
                                .pull(service, &format!("{}/{}", done.pr.owner, done.pr.repo), done.pr.number.unsigned_abs())
                                .await
                                .is_some_and(|p| p.draft);
                            self.ready_once(id, key, &done.pr, &done.head_sha, draft);
                        }
                        false => {
                            self.changes_asked(service, o, id, key, v, &done.pr, &done.head_sha, &said)
                                .await
                        }
                    }
                    self.close_review(service, o, &r, &l.pr, head).await;
                }
                Err(Unrecorded::NoVerdict) => {
                    let first = self
                        .plan_watch
                        .lock()
                        .done
                        .insert(format!("verdict:{}", r.id));
                    match first {
                        true => {
                            let action = Action::Nudge {
                                to: Target::Session(r.id),
                                message: workforce_desk::plan::review_instruction(head),
                                reply_to: None,
                            };
                            let by = self.by_at_level(
                                self.by_boss(service, "Its review ended with no verdict line."),
                                o,
                            );
                            if let Err(e) = self.perform(&action, by).await {
                                tracing::info!(session = %r.id, error = %e, "a review with no verdict wasn't asked for one");
                            }
                        }
                        false => self.wake_plan(
                            id,
                            format!("{key}: its review {} ended again with no verdict line. {}", r.label(), review::NO_VERDICT),
                        ),
                    }
                }
                Err(Unrecorded::Moved(why)) => {
                    tracing::info!(session = %r.id, %why, "a review of a head that moved on is closed");
                    self.close_review(service, o, &r, &l.pr, head).await;
                }
                Err(Unrecorded::No(why)) => {
                    tracing::info!(session = %r.id, %why, "a review wasn't recorded; looked at again next minute");
                    self.plan_watch.lock().done.remove(&once);
                }
            }
        }
        works
    }

    /// Gives ticket `key`'s session `v` the changes its review of `pr` at
    /// `head` asked for (`said`, its reviewer's final reply), as its boss,
    /// and tells plan `id`'s turn.
    #[allow(clippy::too_many_arguments)]
    async fn changes_asked(
        &self,
        service: &Arc<AgentService>,
        o: &Oversight,
        id: Uuid,
        key: &str,
        v: &workforce_desk::model::SessionView,
        pr: &PrRef,
        head: &str,
        said: &str,
    ) {
        let action = Action::Nudge {
            to: Target::Session(v.id),
            message: review::changes_message(pr, head, said),
            reply_to: None,
        };
        let by = self.by_at_level(
            self.by_boss(service, "Its review asked for changes."),
            o,
        );
        match self.perform(&action, by).await {
            Ok(_) => self.wake_plan(id, review::changes_line(key, pr, head)),
            Err(e) => {
                tracing::info!(session = %v.id, error = %e, "a review's changes weren't given to its session");
                self.wake_plan(
                    id,
                    format!(
                        "{key}: the review of {} at {} asked for changes, and Desk couldn't tell its session ({e}): tell it with crew.nudge.",
                        review::named(pr),
                        short(head)
                    ),
                );
            }
        }
    }

    /// Closes review session `r` of `pr` at `head`, its review done, as
    /// its boss, and removes the worktree Desk made it read in: kept while
    /// it holds work.
    async fn close_review(
        &self,
        service: &Arc<AgentService>,
        o: &Oversight,
        r: &workforce_desk::model::SessionView,
        pr: &PrRef,
        head: &str,
    ) {
        let by = self.by_at_level(self.by_boss(service, "Its review is done."), o);
        match self.perform(&Action::Close { session: r.id }, by).await {
            Ok(_) if review::made_for(&r.cwd, pr, head) => {
                if let Err(e) = workforce_desk::worktree::remove(std::path::Path::new(&r.cwd)).await {
                    tracing::info!(session = %r.id, error = %e, "a closed review's worktree stays");
                }
            }
            Ok(_) => {}
            Err(why) => tracing::info!(session = %r.id, %why, "a review session stays open"),
        }
    }

    /// Wakes plan `id`'s boss with `line`, on its own task: its turn
    /// never holds up the minute's other looks.
    fn wake_plan(&self, id: Uuid, line: String) {
        self.wake_plan_with(id, vec![line]);
    }

    /// Wakes plan `id`'s boss with `lines` in one turn, on its own task.
    pub(crate) fn wake_plan_with(&self, id: Uuid, lines: Vec<String>) {
        let Some(crew) = self.me.upgrade() else {
            return;
        };
        tokio::spawn(async move { crew.plan_turn(id, lines).await });
    }

    /// Tells the person, once a stall, that plan `p` hasn't moved in
    /// `stall_minutes` while it has work ready or in Doing and none of it
    /// works: what is stuck, in plain words.
    async fn say_stall(
        &self,
        boss: &str,
        o: &Oversight,
        p: &PlanDetail,
        busy: bool,
        stuck: &[String],
        now: DateTime<Utc>,
    ) {
        if busy || stuck.is_empty() {
            return;
        }
        let Some(moved) = last_moved(&p.tickets) else {
            return;
        };
        let minute = chrono::Duration::from_std(self.timing.minute).unwrap_or_default();
        if now - moved < minute * o.work.stall_minutes as i32 {
            return;
        }
        let once = format!("stall:{}:{}", p.summary.plan.id, moved.timestamp());
        if !self.plan_watch.lock().done.insert(once) {
            return;
        }
        let mins = (now - moved).num_milliseconds() / minute.num_milliseconds().max(1);
        let text = format!(
            "The plan “{}” hasn't moved in {} min: {}.",
            workforce_desk::crew::one_line(&p.summary.plan.goal, 120),
            mins.max(o.work.stall_minutes as i64),
            stuck.join("; ")
        );
        tracing::info!(plan = %p.summary.plan.id, "a plan stalled: the person was told");
        self.tell_dm(boss, &text).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(checks: Option<&str>, failed: u64) -> Option<Pull> {
        Some(Pull {
            open: true,
            head: Some("d05e8a4c0ffee".into()),
            checks: checks.map(str::to_string),
            failed,
            ..Default::default()
        })
    }

    fn merged(at: Option<&str>) -> Option<Pull> {
        Some(Pull {
            merged: true,
            merged_at: at.map(str::to_string),
            ..Default::default()
        })
    }

    #[test]
    fn what_landed_is_finished_checks_on_an_open_head_else_everything_merged() {
        // VEL-5: pending says nothing; green and red each say so.
        assert_eq!(landed(&[(191, open(Some("pending"), 0))]), None);
        assert_eq!(landed(&[(191, open(None, 0))]), None, "unread checks");
        assert_eq!(
            landed(&[(191, open(Some("success"), 0))]),
            Some(Landed::Checks {
                n: 191,
                head: "d05e8a4c0ffee".into(),
                failed: 0
            })
        );
        assert_eq!(
            landed(&[(191, open(Some("failure"), 0))]),
            Some(Landed::Checks {
                n: 191,
                head: "d05e8a4c0ffee".into(),
                failed: 1
            }),
            "red is red however many GitHub counted"
        );
        // VEL-6 and VEL-8: every one merged.
        assert_eq!(
            landed(&[(175, merged(Some("2026-11-19T14:45:32Z")))]),
            Some(Landed::Merged(vec![(
                175,
                Some("2026-11-19T14:45:32Z".into())
            )]))
        );
        assert_eq!(
            landed(&[(184, merged(None)), (189, merged(None))]),
            Some(Landed::Merged(vec![(184, None), (189, None)]))
        );
        // One open still pending keeps the merged one's word back.
        assert_eq!(
            landed(&[(184, merged(None)), (191, open(Some("pending"), 0))]),
            None
        );
        // One GitHub couldn't say, or none at all: nothing.
        assert_eq!(landed(&[(184, merged(None)), (9, None)]), None);
        assert_eq!(landed(&[]), None);
        // Closed unmerged: nothing landed.
        assert_eq!(landed(&[(7, Some(Pull::default()))]), None);
    }
}
