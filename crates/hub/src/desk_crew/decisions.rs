//! The boss's decision log on the hub (the boss understands sessions, H4):
//! each crew action a boss performs, each question it gives the person and
//! each line or work turn it ends doing nothing is recorded as it happens,
//! never read out of what it said. Recording is a store write, outside the
//! check-in budget and the held list, and never stops an action: a write
//! that fails is logged. Undo is honest per action ([`undo_of`]), done as
//! the person, and recorded too.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;
use workforce_common::channels::desk::DeliveryKind;
use workforce_desk::crew::{self, By, Extras, Kind, PartyKind, UndoRefused};
use workforce_desk::decision::{BossDecision, Handled, How, Need, Undo, UndoPlan};
use workforce_desk::line::{Delivery, LineMessage};

use super::actions::{Action, Target};
use super::watcher::reported;
use super::CrewHub;

/// The reading tool over the log.
pub const NAME: &str = "crew.decisions";

pub const DESCRIPTION: &str = "What you decided for the person, from Desk's log of it (since: a time, default the start of today): each thing you did for a session, each question you gave the person and each turn you let a session wait, with its time, why, and whether it was undone; grouped by session, with how many of the needs (approvals, questions, stalls) you handled yourself. Answer \"what did you decide?\" from this, never from memory.";

pub fn schema() -> Value {
    json!({ "type": "object", "properties": {
        "since": { "type": "string", "description": "An RFC 3339 time; the start of today when left out" },
        "limit": { "type": "integer", "minimum": 1, "maximum": 200 }
    }})
}

/// What a performed action left behind that its undo needs, handed back by
/// [`CrewHub::perform`]'s branches: never read from what it said.
#[derive(Debug, Default, Clone)]
pub(crate) struct Done {
    /// The line message it sent a session, and how it went.
    pub line: Option<(Uuid, Delivery)>,
    /// The session it started.
    pub started: Option<Uuid>,
}

/// What was true before the action that its undo goes back to.
#[derive(Debug, Default, Clone)]
pub(crate) struct Before {
    pub mode: Option<String>,
}

/// How `action`, done as `done` says, is undone: the kind, and what the
/// undo does. Pure, so the whole matrix is tested without a desk.
pub(crate) fn undo_of(action: &Action, done: &Done, before: &Before) -> (Undo, Option<UndoPlan>) {
    match action {
        Action::Nudge {
            to: Target::Session(s),
            message,
            reply_to,
        } => match done.line {
            Some((id, delivery)) => {
                let delivered = !matches!(delivery, Delivery::Queued | Delivery::Held);
                let plan = UndoPlan::Line {
                    session: *s,
                    message: id,
                    text: message.clone(),
                    ask: *reply_to,
                    delivered,
                };
                match delivery {
                    Delivery::Queued | Delivery::Held => (Undo::Withdraw, Some(plan)),
                    Delivery::Handed | Delivery::Delivered | Delivery::Noted => {
                        (Undo::Correct, Some(plan))
                    }
                    // It never goes in: nothing to take back.
                    Delivery::Refused | Delivery::Withdrawn => (Undo::None, None),
                }
            }
            None => (Undo::None, None),
        },
        Action::SetMode { session, .. } => match &before.mode {
            Some(mode) => (
                Undo::Reverse,
                Some(UndoPlan::SetMode {
                    session: *session,
                    mode: mode.clone(),
                }),
            ),
            None => (Undo::None, None),
        },
        Action::TakeOn { session } => {
            (Undo::Reverse, Some(UndoPlan::Release { session: *session }))
        }
        Action::Release { session } => {
            (Undo::Reverse, Some(UndoPlan::TakeOn { session: *session }))
        }
        Action::MoveTicket {
            ticket,
            id,
            was: Some(was),
            ..
        } => (
            Undo::Reverse,
            Some(UndoPlan::MoveTicket {
                ticket: ticket.clone(),
                id: *id,
                state: serde_json::to_value(was)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default(),
            }),
        ),
        Action::Stop { session } => (
            Undo::Reverse,
            Some(UndoPlan::Resume {
                session: *session,
                prompt: None,
            }),
        ),
        Action::Interrupt { session } => (
            Undo::Reverse,
            Some(UndoPlan::Resume {
                session: *session,
                prompt: Some("carry on".into()),
            }),
        ),
        Action::StartSession { .. } => match done.started {
            Some(session) => (Undo::Reverse, Some(UndoPlan::StopStarted { session })),
            None => (Undo::None, None),
        },
        // What ran (an approval, a deny, a restart, a hand-out, a message to
        // an agent) is fixed forward, by a new session or the person.
        _ => (Undo::None, None),
    }
}

/// The session a decision about `action` is about.
fn session_of(action: &Action, done: &Done) -> Option<Uuid> {
    match action {
        Action::TakeOn { session } | Action::Release { session } => Some(*session),
        Action::StartSession { .. } => done.started,
        a => CrewHub::session_of(a),
    }
}

/// The need it settled, and its ref.
fn need_of(action: &Action) -> (Need, Option<String>) {
    match action {
        Action::Approve { approval, .. } | Action::Deny { approval, .. } => {
            (Need::Approval, Some(approval.to_string()))
        }
        Action::Nudge {
            reply_to: Some(ask),
            ..
        } => (Need::Question, Some(ask.to_string())),
        _ => (Need::Choice, None),
    }
}

/// The whole of `why` once the level's tag (" (at Act fully)") is said.
fn why_of(by: &By) -> String {
    by.reason.clone().unwrap_or_default()
}

impl CrewHub {
    /// The boss a decision by `by` is recorded for: a boss overseeing
    /// here. The person (saying yes to a suggestion), Desk, and a remote
    /// boss acting on this machine's sessions through its grant record none
    /// here: their decisions are kept where they are made.
    pub(crate) fn deciding_boss(&self, by: &By) -> Option<String> {
        (by.party.kind == PartyKind::Manager && self.overseer_named(&by.party.id).is_some())
            .then(|| by.party.id.clone())
    }

    /// How many decisions `boss` has recorded about `session` (and about no
    /// session), for telling whether a turn did anything.
    pub(crate) fn acted_mark(&self, boss: &str, session: Uuid) -> (u64, u64) {
        let acted = self.acted.lock();
        (
            acted
                .get(&(boss.to_string(), Some(session)))
                .copied()
                .unwrap_or(0),
            acted.get(&(boss.to_string(), None)).copied().unwrap_or(0),
        )
    }

    /// `boss` did something about `session` (a decision, or a suggestion).
    pub(crate) fn acted_on(&self, boss: &str, session: Option<Uuid>) {
        *self
            .acted
            .lock()
            .entry((boss.to_string(), session))
            .or_default() += 1;
    }

    /// Keeps `d`. A write that fails is logged, never the action's failure.
    pub(crate) async fn record(&self, d: &BossDecision) {
        self.acted_on(&d.boss, d.session);
        if let Err(e) = self.desk.store().decision_put(d).await {
            tracing::warn!(boss = %d.boss, error = %e, "a boss's decision wasn't kept");
        }
    }

    /// What `before` an action knows that its undo goes back to.
    pub(crate) fn before(&self, action: &Action) -> Before {
        match action {
            Action::SetMode { session, .. } => Before {
                mode: self
                    .desk
                    .view(*session)
                    .ok()
                    .and_then(|v| v.mode)
                    .map(|m| m.as_str().to_string()),
            },
            _ => Before::default(),
        }
    }

    /// Records what `action`, done by `by` and said as `said`, decided, when
    /// it is a boss's here. Escalating is recorded where it happens
    /// ([`Self::record_escalated`]).
    pub(crate) async fn record_action(
        &self,
        action: &Action,
        by: &By,
        said: &str,
        done: &Done,
        before: &Before,
    ) -> Option<BossDecision> {
        if matches!(action, Action::Escalate { .. }) {
            return None;
        }
        let boss = self.deciding_boss(by)?;
        let (need, need_ref) = need_of(action);
        let what = match action {
            Action::Nudge {
                to: Target::Session(s),
                message,
                reply_to: None,
            } => format!(
                "told {} “{}”",
                self.name_of_session(*s),
                crew::clip(message, 200)
            ),
            Action::Nudge {
                to: Target::Session(s),
                message,
                reply_to: Some(ask),
            } => {
                let question = self.questions.lock().get(ask).cloned();
                match question {
                    Some(q) => format!(
                        "answered {}'s question “{}” with “{}”",
                        self.name_of_session(*s),
                        crew::one_line(&q, 160),
                        crew::clip(message, 200)
                    ),
                    None => format!(
                        "answered {} with “{}”",
                        self.name_of_session(*s),
                        crew::clip(message, 200)
                    ),
                }
            }
            _ => crew::clip(said, 240),
        };
        let mut d = BossDecision::new(&boss, need, &what, &why_of(by), How::Own);
        d.need_ref = need_ref;
        d.session = session_of(action, done);
        let (undo, plan) = undo_of(action, done, before);
        d.undo = undo;
        d.action = plan.map(|p| p.value()).unwrap_or(Value::Null);
        self.record(&d).await;
        Some(d)
    }

    /// Records a session's question `ask` given to the person by `by`,
    /// saying `why`, when it is a boss's here.
    pub(crate) async fn record_escalated(&self, q: &LineMessage, by: &By, why: &str) {
        let Some(boss) = self.deciding_boss(by) else {
            return;
        };
        let what = format!(
            "gave you {}'s question “{}”",
            q.from.name,
            crew::one_line(q.headline(), 160)
        );
        let mut d = BossDecision::new(&boss, Need::Question, &what, why, How::Escalated);
        d.need_ref = Some(q.id.to_string());
        d.session = q.from_session();
        self.record(&d).await;
    }

    /// Records that `boss`'s line or work turn on `session` ended doing
    /// nothing, with what it said (`reply`) as why. A work turn's session
    /// waits for its next step: its undo tells it to go ahead.
    pub(crate) async fn record_nothing(&self, boss: &str, session: Uuid, work: bool, reply: &str) {
        let name = self.name_of_session(session);
        let why = match crew::one_line(reply, 300) {
            w if w.is_empty() => "It didn't say why.".to_string(),
            w => w,
        };
        let (need, what) = match work {
            true => (Need::TurnEnd, format!("let {name} wait")),
            false => (Need::Choice, format!("sent {name} nothing")),
        };
        let mut d = BossDecision::new(boss, need, &what, &why, How::Own);
        d.session = Some(session);
        if work {
            d.undo = Undo::Reverse;
            d.action = UndoPlan::GoAhead { session }.value();
        }
        self.record(&d).await;
    }

    pub(crate) fn name_of_session(&self, id: Uuid) -> String {
        self.desk
            .view(id)
            .map(|v| v.label().to_string())
            .unwrap_or_else(|_| "a session".into())
    }

    /// Puts decision `d` in its boss's DM, said verbatim from the row, with
    /// its Undo where it has one.
    pub(crate) async fn post_decision(&self, d: &BossDecision) {
        let Some((service, _)) = self.overseer_named(&d.boss) else {
            return;
        };
        let Some(owner) = self.person(Some(&service)) else {
            return;
        };
        let extras = Extras {
            decisions: vec![d.line()],
            ..Default::default()
        };
        let delivery = crate::desk_surface::Delivery::text(DeliveryKind::Notice, d.said());
        if let Err(e) = self
            .deliver_with_extras(&service, &owner, delivery, &extras)
            .await
        {
            tracing::warn!(error = %e, "putting a decision in the DM failed");
        }
    }

    /// Undoes decision `id` as the person, as its kind allows, and records
    /// that as a decision of its own. Once only.
    pub(crate) async fn undo(&self, id: Uuid) -> Result<String, UndoRefused> {
        let store = self.desk.store();
        let found = store.decision(id).await.map_err(|e| {
            tracing::warn!(error = %e, "a decision couldn't be read");
            UndoRefused::Cant("Something went wrong on this Mac.".into())
        })?;
        let d = found.ok_or(UndoRefused::NotFound)?;
        if d.undone_at.is_some() {
            return Err(UndoRefused::Already);
        }
        let plan = match (d.undo, d.plan()) {
            (Undo::None, _) | (_, None) => {
                return Err(UndoRefused::Cant(
                    "That can't be undone: what ran is fixed forward.".into(),
                ))
            }
            (_, Some(plan)) => plan,
        };
        let Some(claimed) = store.decision_claim_undo(id).await.map_err(|e| {
            tracing::warn!(error = %e, "a decision's undo couldn't be claimed");
            UndoRefused::Cant("Something went wrong on this Mac.".into())
        })?
        else {
            return Err(UndoRefused::Already);
        };
        let reason = format!("You undid my decision: {}.", d.what.trim_end_matches('.'));
        let by = By {
            reason: Some(reason.clone()),
            ..By::you()
        };
        match self.undo_plan(&plan, by).await {
            Ok(done) => {
                let mut mine = BossDecision::new(
                    &claimed.boss,
                    Need::Undo,
                    &format!("you undid: {}", claimed.what),
                    &done,
                    How::PersonFirst,
                );
                mine.session = claimed.session;
                mine.need_ref = Some(claimed.id.to_string());
                if let Err(e) = store.decision_put(&mine).await {
                    tracing::warn!(error = %e, "an undo wasn't kept as a decision");
                }
                Ok(done)
            }
            Err(why) => {
                if let Err(e) = store.decision_unclaim_undo(id).await {
                    tracing::warn!(error = %e, "a failed undo's claim wasn't let go");
                }
                Err(UndoRefused::Cant(why))
            }
        }
    }

    /// Does `plan` as `by` (the person): what was done.
    async fn undo_plan(&self, plan: &UndoPlan, by: By) -> Result<String, String> {
        let person_tells = |session: Uuid, text: String| async move {
            self.desk
                .line_send(
                    workforce_desk::LineSender::Person,
                    workforce_desk::line::Send::Tell {
                        to: workforce_desk::line::Target::Session(session),
                        text,
                        when: workforce_desk::line::When::Queue,
                    },
                )
                .await
                .map_err(|e| match e {
                    workforce_desk::LineError::Refused(r) => r.to_string(),
                    workforce_desk::LineError::Desk(e) => {
                        tracing::warn!(error = %e, "an undo's message wasn't sent");
                        "Something went wrong on this Mac.".to_string()
                    }
                })
        };
        let action = match plan {
            UndoPlan::Line {
                session,
                message,
                text,
                ask,
                ..
            } => {
                let name = self.name_of_session(*session);
                // Read again now: one still waiting is taken back; one gone
                // in is corrected.
                match self.desk.line_withdraw(*message).await {
                    Ok(Some((_, reopened))) => {
                        // An answer taken back: its question is the
                        // person's again, carded in the DM.
                        if let Some(q) = reopened {
                            self.post_question(&q).await;
                        }
                        self.desk.note(
                            by.event(
                                Kind::Dismissed,
                                self.desk
                                    .view(*session)
                                    .ok()
                                    .map(|v| crew::Party::session(&v)),
                                format!("took back Desk's message to {name} before it went in"),
                            ),
                        );
                        return Ok(match ask {
                            Some(_) => format!(
                                "took back Desk's answer to {name} before it went in; its question is yours"
                            ),
                            None => format!("took back Desk's message to {name} before it went in"),
                        });
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "a boss's message couldn't be taken back");
                        return Err("Something went wrong on this Mac.".into());
                    }
                }
                // No longer waiting: read again. Only one that went in is
                // corrected; one that never will (withdrawn, refused, gone)
                // has nothing to take back.
                let now = self
                    .desk
                    .store()
                    .line_message(*message)
                    .await
                    .map_err(|e| {
                        tracing::warn!(error = %e, "a boss's message couldn't be read");
                        "Something went wrong on this Mac.".to_string()
                    })?;
                if !now.as_ref().is_some_and(|m| {
                    matches!(
                        m.delivery,
                        Delivery::Handed | Delivery::Delivered | Delivery::Noted
                    )
                }) {
                    return Ok(format!(
                        "Desk's message to {name} never went in, so there was nothing to take back"
                    ));
                }
                let said = match ask {
                    Some(_) => format!(
                        "Please disregard Desk's earlier answer to your question (“{}”); I'll tell you what to do instead.",
                        crew::clip(text, 200)
                    ),
                    None => format!(
                        "Please disregard Desk's earlier message (“{}”); I'll tell you what to do instead.",
                        crew::clip(text, 200)
                    ),
                };
                person_tells(*session, said).await?;
                return Ok(format!(
                    "{name} already had Desk's message, so it was told to disregard it"
                ));
            }
            UndoPlan::GoAhead { session } => {
                person_tells(*session, "Go ahead with your next step.".into()).await?;
                return Ok(format!(
                    "told {} to go ahead",
                    self.name_of_session(*session)
                ));
            }
            UndoPlan::SetMode { session, mode } => Action::parse(
                "crew.set_mode",
                &json!({ "session_id": session.to_string(), "mode": mode }),
            )?,
            UndoPlan::Release { session } => Action::Release { session: *session },
            UndoPlan::TakeOn { session } => Action::TakeOn { session: *session },
            UndoPlan::MoveTicket { ticket, id, state } => {
                let mut a = Action::parse(
                    "crew.move_ticket",
                    &json!({ "ticket": ticket, "state": state }),
                )?;
                if let Action::MoveTicket { id: slot, .. } = &mut a {
                    *slot = *id;
                }
                a
            }
            UndoPlan::Resume { session, prompt } => Action::Resume {
                session: *session,
                prompt: prompt.clone(),
            },
            UndoPlan::StopStarted { session } => Action::Stop { session: *session },
        };
        self.perform(&action, by).await
    }

    /// `crew.decisions` for `boss`: its decisions since `input.since`.
    pub(crate) async fn decisions_tool(&self, boss: &str, input: &Value) -> Result<Value, String> {
        let since = match input["since"].as_str().filter(|s| !s.trim().is_empty()) {
            Some(s) => s.trim().parse::<DateTime<Utc>>().map_err(|_| {
                "since must be an RFC 3339 time, like 2026-11-12T18:00:00Z.".to_string()
            })?,
            None => workforce_desk::crew::Crew::midnight(self),
        };
        let limit = input["limit"].as_i64().unwrap_or(100).clamp(1, 200);
        let list = self
            .desk
            .store()
            .decisions_since(boss, since, limit)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "a boss's decisions couldn't be read");
                "Your decisions could not be read just now.".to_string()
            })?;
        let tz = workforce_desk::crew::Crew::timezone(self);
        let h = self
            .desk
            .store()
            .decisions_handled(boss, since)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "a boss's handled counts couldn't be read");
                Handled::of(&list)
            });
        Ok(json!({
            "since": since.to_rfc3339(),
            "handled": {
                "needs": h.needs(),
                "yourself": h.itself(),
                "by_preference": h.preference,
                "escalated": h.escalated,
                "person_first": h.person_first,
            },
            "decisions": reported(&grouped(&list, tz, |s| self.name_of_session(s)), "your decision log"),
        }))
    }
}

/// `list` grouped by session, oldest first in each, at `tz`.
fn grouped(list: &[BossDecision], tz: chrono_tz::Tz, name: impl Fn(Uuid) -> String) -> String {
    let mut order: Vec<Option<Uuid>> = Vec::new();
    for d in list {
        if !order.contains(&d.session) {
            order.push(d.session);
        }
    }
    let mut out = String::new();
    for s in order {
        out.push_str(&match s {
            Some(id) => format!("{} (session_id {id}):\n", name(id)),
            None => "Not about one session:\n".to_string(),
        });
        for d in list.iter().filter(|d| d.session == s) {
            let undo = match (d.undone_at, d.button()) {
                (Some(_), _) => " · undone".to_string(),
                (None, Some(b)) => format!(" · can {b}"),
                (None, None) => String::new(),
            };
            out.push_str(&format!(
                "- {} {} — {} [{}]{undo}\n",
                d.at.with_timezone(&tz).format("%H:%M"),
                d.what,
                match d.why.as_str() {
                    "" => "no reason given",
                    w => w,
                },
                d.how.as_str(),
            ));
        }
    }
    if out.is_empty() {
        out.push_str("Nothing decided in that time.\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_desk::board::TicketState;

    #[test]
    fn each_action_is_undone_as_honestly_as_it_can_be() {
        let s = Uuid::new_v4();
        let line = |d| Done {
            line: Some((Uuid::new_v4(), d)),
            started: None,
        };
        let nudge = Action::Nudge {
            to: Target::Session(s),
            message: "Wait for green CI.".into(),
            reply_to: None,
        };
        let none = Before::default();
        // Still queued or held: withdrawn. In already: corrected.
        assert_eq!(
            undo_of(&nudge, &line(Delivery::Queued), &none).0,
            Undo::Withdraw
        );
        assert_eq!(
            undo_of(&nudge, &line(Delivery::Held), &none).0,
            Undo::Withdraw
        );
        assert_eq!(
            undo_of(&nudge, &line(Delivery::Delivered), &none).0,
            Undo::Correct
        );
        assert_eq!(
            undo_of(&nudge, &line(Delivery::Handed), &none).0,
            Undo::Correct
        );
        assert_eq!(
            undo_of(&nudge, &line(Delivery::Withdrawn), &none).0,
            Undo::None
        );
        let answer = Action::Nudge {
            to: Target::Session(s),
            message: "5433".into(),
            reply_to: Some(Uuid::new_v4()),
        };
        let (undo, plan) = undo_of(&answer, &line(Delivery::Queued), &none);
        assert_eq!(undo, Undo::Withdraw);
        assert!(matches!(plan, Some(UndoPlan::Line { ask: Some(_), .. })));
        // The reverse call.
        let mode = Before {
            mode: Some("default".into()),
        };
        let set = Action::SetMode {
            session: s,
            mode: workforce_desk::mode::Mode::Auto,
        };
        assert_eq!(
            undo_of(&set, &Done::default(), &mode).1,
            Some(UndoPlan::SetMode {
                session: s,
                mode: "default".into()
            })
        );
        assert_eq!(undo_of(&set, &Done::default(), &none).0, Undo::None);
        assert_eq!(
            undo_of(&Action::TakeOn { session: s }, &Done::default(), &none).1,
            Some(UndoPlan::Release { session: s })
        );
        assert_eq!(
            undo_of(&Action::Release { session: s }, &Done::default(), &none).1,
            Some(UndoPlan::TakeOn { session: s })
        );
        let mv = Action::MoveTicket {
            ticket: "ACM-12".into(),
            id: None,
            state: TicketState::Done,
            was: Some(TicketState::Doing),
            off_external: false,
            planned: false,
        };
        assert!(matches!(
            undo_of(&mv, &Done::default(), &none).1,
            Some(UndoPlan::MoveTicket { ref state, .. }) if state == "doing"
        ));
        assert_eq!(
            undo_of(&Action::Stop { session: s }, &Done::default(), &none).1,
            Some(UndoPlan::Resume {
                session: s,
                prompt: None
            })
        );
        assert_eq!(
            undo_of(&Action::Interrupt { session: s }, &Done::default(), &none).1,
            Some(UndoPlan::Resume {
                session: s,
                prompt: Some("carry on".into())
            })
        );
        let start = Action::StartSession {
            folder: "/tmp".into(),
            goal: "g".into(),
            name: None,
            engine: None,
            view: None,
            worktree: None,
            fresh: false,
        };
        let started = Done {
            line: None,
            started: Some(s),
        };
        assert_eq!(
            undo_of(&start, &started, &none).1,
            Some(UndoPlan::StopStarted { session: s })
        );
        // What ran is fixed forward.
        for a in [
            Action::Approve {
                session: s,
                approval: Uuid::new_v4(),
            },
            Action::Deny {
                session: s,
                approval: Uuid::new_v4(),
            },
            Action::Restart {
                session: s,
                prompt: None,
            },
        ] {
            assert_eq!(undo_of(&a, &Done::default(), &none), (Undo::None, None));
        }
    }

    #[test]
    fn the_log_reads_grouped_by_session_with_its_undo() {
        let s = Uuid::new_v4();
        let mut a = BossDecision::new(
            "desk",
            Need::Approval,
            "allowed web",
            "Safe read.",
            How::Own,
        );
        a.session = Some(s);
        let mut b = BossDecision::new("desk", Need::TurnEnd, "let web wait", "CI runs.", How::Own);
        b.session = Some(s);
        b.undo = Undo::Reverse;
        b.action = UndoPlan::GoAhead { session: s }.value();
        let c = BossDecision::new("desk", Need::Choice, "assigned scout", "", How::Own);
        let text = grouped(&[a, b, c], chrono_tz::UTC, |_| "web".into());
        assert!(text.starts_with("web (session_id"), "{text}");
        assert!(
            text.contains("let web wait — CI runs. [own] · can undo"),
            "{text}"
        );
        assert!(text.contains("Not about one session:\n- "), "{text}");
        assert!(text.contains("no reason given"), "{text}");
    }
}
