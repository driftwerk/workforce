//! Who holds which job (designs/boss-orders-and-serial-turns.md §3): every
//! instruction a boss gives a session is an assignment
//! ([`workforce_desk::assignment`]), kept until it is done, cancelled,
//! superseded or let lapse.
//!
//! - **Reserved before it is sent.** [`CrewHub::perform_with`] checks a
//!   boss's message (or ticket, resume or restart prompt) against the
//!   boss's live assignments in its session's lane, after the picture's own
//!   check, under the boss's Postgres lock: a second session is never given
//!   a job another holds (R2, R3), and a session that has an irreversible
//!   job is never told to start it again (R1). Whichever of the boss's turns
//!   sends it (its DM, a line, a check-in, a focused call), it is the same
//!   reservation.
//! - **Moved with `replaces`.** The old holder is told to stand down (a
//!   line message that asks for an answer), and the new holder's order is
//!   held while the old one may have started it, until the old holder
//!   answers the stand-down or its session goes; then it goes through
//!   `perform`, on the picture as it was when it was held. Unanswered for
//!   [`HOLD_FOR`], it is the person's.
//! - **Approvals** of an irreversible request that another session's
//!   irreversible assignment acts on are the person's, never the boss's
//!   (R5, [`CrewHub::request_conflict`]).
//! - **Kept up to date** by [`assignments_loop`]: delivered and answered
//!   from the line, cancelled when its message is withdrawn, lapsed when
//!   reversible and old, closed from the session's account with
//!   `close_from_accounts`, and what a restart left being sent settled.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_desk::assignment::{
    self, short, Assignment, AssignmentState, Reading, Rule, Target, TargetKind,
};
use workforce_desk::assignment_store::Reserved;
use workforce_desk::crew::{self, By, Event, Kind, Party, PartyKind};
use workforce_desk::line::{Delivery, LineKind, LineMessage};
use workforce_desk::model::{DeskEvent, SessionState};
use workforce_desk::open_item::{NewItem, OpenKind};

use super::actions::{Action, Handout, Target as To, STATUS_QUESTION};
use super::picture::Deciding;
use super::CrewHub;

/// The boss's tool that closes one of its assignments.
pub(crate) const CLOSE: &str = "crew.assignment";
/// The boss's tool that lists them.
pub(crate) const LIST: &str = "crew.assignments";

pub(crate) const CLOSE_DESCRIPTION: &str = "Close one of your assignments (id, as a:… from crew.assignments): state done when its session did it, or cancelled when it no longer should; note says why, in a sentence the person reads. Closing it lets you give that job to another session.";
pub(crate) const LIST_DESCRIPTION: &str = "Your assignments: every job you gave a session (each message, ticket, resume or restart prompt), with who holds it, when it was sent, delivered and answered, and whether it can be undone. With all, the last day's closed ones too. A job one session holds isn't given to another: move it with crew.nudge's replaces.";

/// How long a held assignment waits for its old holder to answer its
/// stand-down, in the crew's minutes, before it is the person's.
pub(crate) const HOLD_FOR: u32 = 10;

/// What Desk adds to an order that takes a job over, or goes out after a
/// stand-down: the new holder checks first.
const CHECK_FIRST: &str = "Before you start it, check it hasn't already been started.";

/// What a boss's order says of the job it is, besides its words: the
/// fields of `crew.nudge` (and `urgent`, `replaces` of a ticket, resume or
/// restart). Every field is optional: an older hub's orders carry none,
/// and their job is read from what they name.
#[derive(Debug, Clone, Default)]
pub(crate) struct Order {
    pub job: Option<String>,
    /// The boss says it can't be taken back, whatever the words say.
    pub irreversible: bool,
    /// Kept for its delivery: an urgent order is steered in by a later
    /// change (§4); for now it is queued as any other.
    pub urgent: bool,
    /// The assignment it takes over from, as the boss names it (`a:…`).
    pub replaces: Option<String>,
    /// Kept at the home, not here: a call the home relayed.
    pub unrecorded: bool,
    /// A held assignment going out now: its row, already reserved.
    pub released: Option<Uuid>,
}

impl Order {
    /// What a crew tool's `input` says of its order.
    pub(crate) fn of(input: &Value) -> Self {
        let text = |k: &str| {
            input[k]
                .as_str()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
        };
        Self {
            job: text("job"),
            irreversible: input["irreversible"].as_bool().unwrap_or(false),
            urgent: input["urgent"].as_bool().unwrap_or(false),
            replaces: text("replaces"),
            ..Default::default()
        }
    }
}

/// The properties a crew tool that hands out work takes for its order.
pub(crate) fn order_properties(with_job: bool) -> Value {
    let mut p = json!({
        "urgent": { "type": "boolean", "description": "It can't wait for the session's turn to end (a correction, a stop)" },
        "replaces": { "type": "string", "description": "The assignment (a:… from crew.assignments) this takes over: its holder is told to stand down first, and this waits until it answers when it may have started" }
    });
    if with_job {
        p["job"] = json!({ "type": "string", "description": "The job this hands out, in a few words (\"release 4\", \"#708\"): one session holds a job at a time" });
        p["irreversible"] = json!({ "type": "boolean", "description": "true when it starts something that can't be taken back (a release, deploy, merge, delete), whatever the words say" });
    }
    p
}

/// A reservation made for an order about to be sent.
#[derive(Debug, Clone)]
pub(super) struct Reservation {
    pub id: Uuid,
    /// What Desk adds to the order: the guard to check first.
    pub guard: Option<String>,
}

/// What [`CrewHub::assign_before`] decided.
pub(super) enum Before {
    /// Not an assignment (an answer, a status question, the person's).
    Not,
    /// Send it, reserved.
    Send(Reservation),
    /// Held for its old holder's stand-down; what the boss is told.
    Held(String),
}

/// The session `action`, a boss's, gives work to, what it says, and the
/// ticket it hands out.
fn assigned(action: &Action) -> Option<(Uuid, String, Option<String>)> {
    match action {
        Action::Nudge {
            to: To::Session(s),
            message,
            reply_to: None,
        } if !message.starts_with(STATUS_QUESTION) => Some((*s, message.clone(), None)),
        Action::AssignTicket {
            ticket,
            to: Handout::Session(s),
            ..
        } => Some((
            *s,
            format!("Ticket {ticket} is yours to work on."),
            Some(ticket.clone()),
        )),
        Action::Resume {
            session,
            prompt: Some(p),
        }
        | Action::Restart {
            session,
            prompt: Some(p),
        } => Some((*session, p.clone(), None)),
        _ => None,
    }
}

/// `action` with `guard` said after what it says.
pub(super) fn guarded(action: &Action, guard: &str) -> Option<Action> {
    let with = |t: &str| format!("{}\n\n{guard}", t.trim_end());
    Some(match action {
        Action::Nudge {
            to,
            message,
            reply_to,
        } => Action::Nudge {
            to: to.clone(),
            message: with(message),
            reply_to: *reply_to,
        },
        Action::Resume {
            session,
            prompt: Some(p),
        } => Action::Resume {
            session: *session,
            prompt: Some(with(p)),
        },
        Action::Restart {
            session,
            prompt: Some(p),
        } => Action::Restart {
            session: *session,
            prompt: Some(with(p)),
        },
        _ => return None,
    })
}

/// `action` as a held assignment keeps it to send later: a suggestion's
/// `{ "tool", "input" }`.
fn kept_form(action: &Action) -> Option<Value> {
    Some(match action {
        Action::Nudge {
            to: To::Session(s),
            message,
            reply_to: None,
        } => json!({ "tool": "crew.nudge", "input": { "session_id": s, "message": message } }),
        Action::AssignTicket {
            ticket,
            id,
            to: Handout::Session(s),
            ..
        } => json!({ "tool": "crew.assign_ticket", "input": {
            "ticket": ticket, "ticket_id": id.map(|i| i.to_string()), "to": { "session": s }
        }}),
        Action::Resume { session, prompt } => {
            json!({ "tool": "crew.resume", "input": { "session_id": session, "prompt": prompt } })
        }
        Action::Restart { session, prompt } => {
            json!({ "tool": "crew.restart", "input": { "session_id": session, "prompt": prompt } })
        }
        _ => return None,
    })
}

/// `text` on one line, its words kept up to `max` characters: what was
/// said, quoted (more than its first sentence, which may only greet).
fn quoted(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", flat[..at].trim_end()),
        None => flat,
    }
}

/// `r`, with an irreversible order that names no job ("Run the deploy")
/// keyed as the unnamed irreversible job in `place`: its session isn't
/// told to start it twice (R1), but it claims nothing from another
/// session, as nothing says it is the same job.
fn unnamed(mut r: Reading, place: &str) -> Reading {
    if r.irreversible && r.job_key.is_none() {
        r.job_key = Some(format!("{place}/unnamed"));
    }
    r
}

/// Whether `a` is keyed as an unnamed irreversible job ([`unnamed`]).
fn unnamed_key(a: &Assignment) -> bool {
    a.job_key
        .as_deref()
        .is_some_and(|k| k.ends_with("/unnamed"))
}

/// Whether `why`, a send's failure, is an assignment refusing it: never
/// done some other way instead (a new session started for it).
pub(super) fn refused(why: &str) -> bool {
    why.starts_with("Not sent: ")
}

/// The marks (`m:` and eight characters) `text` quotes.
fn quoted_marks(text: &str) -> Vec<String> {
    text.match_indices("m:")
        .filter_map(|(at, _)| {
            let hex: String = text[at + 2..]
                .chars()
                .take_while(char::is_ascii_hexdigit)
                .collect();
            (hex.len() >= 6).then(|| hex[..hex.len().min(8)].to_ascii_lowercase())
        })
        .collect()
}

/// Whether `m`, a session's message, answers `id`: a reply to it, or
/// quoting its mark ("Ack m:5068bfae"). One that only came after it
/// doesn't.
pub(super) fn answers(m: &LineMessage, id: Uuid) -> bool {
    m.in_reply_to == Some(id)
        || quoted_marks(&m.text)
            .iter()
            .any(|mark| workforce_desk::line::marks(mark, id))
}

impl CrewHub {
    /// The repository `session` works in, by its origin (`owner/name`), and
    /// the name its job keys are made in: that, else its folder's name.
    async fn place_of(&self, session: Uuid) -> (Option<String>, String) {
        let desk = self.desk.clone();
        let repo = tokio::task::spawn_blocking(move || desk.repo_of_session(session))
            .await
            .ok()
            .flatten();
        let folder = self
            .desk
            .view(session)
            .ok()
            .and_then(|v| {
                std::path::Path::new(v.cwd.trim_end_matches('/'))
                    .file_name()
                    .map(|n| n.to_string_lossy().to_lowercase())
            })
            .unwrap_or_else(|| "here".into());
        let place = repo.clone().unwrap_or(folder);
        (repo, place)
    }

    /// Whether `session` is gone: closed, removed, or not on Desk.
    fn holder_gone(&self, a: &Assignment) -> bool {
        if a.machine.is_some() {
            return false;
        }
        self.desk
            .view(a.session)
            .map_or(true, |v| v.state == SessionState::Closed)
    }

    /// What an assignment's holder is called.
    fn holder_name(&self, a: &Assignment) -> String {
        match &a.machine {
            Some(m) => format!("{m}'s session {}", &a.session.simple().to_string()[..8]),
            None => self.session_name(a.session),
        }
    }

    /// What the holder of `a` wrote since it got it, first: the session's
    /// answer to it, else its first word on its line after.
    fn written_since(&self, a: &Assignment) -> Option<(DateTime<Utc>, String)> {
        let since = a.delivered_at.unwrap_or(a.sent_at);
        self.desk
            .picture_recent(&a.boss, a.session, 40)
            .into_iter()
            .find(|r| r.what == workforce_desk::picture::What::SessionReported && r.at > since)
            .map(|r| (r.at, r.text))
    }

    /// The refusal of an order by `rule`, for its holder `h`, in plain words
    /// with what to do next.
    fn refusal(&self, rule: Rule, h: &Assignment, new: &Assignment) -> String {
        let tz = self.desk.timezone();
        let at = |t: DateTime<Utc>| t.with_timezone(&tz).format("%H:%M").to_string();
        let name = self.holder_name(h);
        let id = short(h.id);
        let mut said = vec![
            format!("“{}”", quoted(&h.text, 160)),
            format!("sent {}", at(h.sent_at)),
        ];
        if let Some(d) = h.delivered_at {
            said.push(format!("delivered {}", at(d)));
        }
        if let Some((when, text)) = self.written_since(h) {
            said.push(format!("it answered {} “{}”", at(when), quoted(&text, 120)));
        }
        let undone = match h.irreversible || new.irreversible {
            true => "Two sessions on one job can run it twice, and what it starts can't be undone.",
            false => "Two sessions on one job get in each other's way.",
        };
        let unnamed = match unnamed_key(h) {
            true => " Neither order names its job: if they are different jobs, send again saying which with job: \"…\".",
            false => "",
        };
        let refused = match rule {
            Rule::SameSession => format!(
                "Not sent: {name} already has this job ({id}, delivered {}). Saying it again can start it twice. Ask it where it stands (crew.ask_session); to change what it does, send with replaces: \"{id}\", and it is told to check first.",
                h.delivered_at.map_or_else(|| "already".into(), at)
            ),
            Rule::Held => format!(
                "Not sent: {name} already waits to be given this job ({id}): it goes to it once its old holder answers its stand-down. To change what it is given, close {id} (crew.assignment) and send again."
            ),
            Rule::SameJob => format!(
                "Not sent: {name} holds this job ({id}, {}). {undone} To move it, send again with replaces: \"{id}\": Desk tells {name} to stand down first. Check that it hasn't already started.",
                said.join(", ")
            ),
            Rule::SharedTarget => {
                let theirs = h.parsed_targets();
                let shared: Vec<String> = new
                    .parsed_targets()
                    .into_iter()
                    .filter(|t| t.kind != TargetKind::Environment && theirs.contains(t))
                    .map(|t| t.said())
                    .collect();
                format!(
                    "Not sent: {name} holds an order that acts on {} too ({id}, {}). {undone} To move it, send again with replaces: \"{id}\": Desk tells {name} to stand down first. Check that it hasn't already started.",
                    shared.join(", "),
                    said.join(", ")
                )
            }
        };
        format!("{refused}{unnamed}")
    }

    /// Notes `kind` in Activity: the boss `by` did it, to `a`'s session.
    fn note_assignment(&self, kind: Kind, by: &Party, a: &Assignment, text: String) {
        let subject = self.desk.view(a.session).ok().map(|v| Party::session(&v));
        self.desk.note(Event::new(kind, by.clone(), subject, text));
    }

    /// Reads and reserves `action`, a boss's order, before it is sent: R1–R3
    /// against the boss's live assignments under its lock, the job taken
    /// over with `replaces`, and held when its old holder may have started
    /// it. `Err` says why it isn't sent.
    pub(super) async fn assign_before(
        &self,
        action: &Action,
        by: &By,
        order: &Order,
    ) -> Result<Before, String> {
        if by.party.kind != PartyKind::Manager || order.unrecorded {
            return Ok(Before::Not);
        }
        let Some((s, text, ticket)) = assigned(action) else {
            return Ok(Before::Not);
        };
        if let Some(id) = order.released {
            return Ok(Before::Send(Reservation { id, guard: None }));
        }
        let boss = by.party.id.as_str();
        let (repo, place) = self.place_of(s).await;
        let job = order.job.clone().or(ticket);
        let r = unnamed(
            assignment::read(&text, job.as_deref(), &place, order.irreversible),
            &place,
        );
        let mut a = Assignment::new(boss, s, &text);
        a.job = job.clone();
        a.job_key = r.job_key;
        a.repo = repo;
        a.targets = r.targets.iter().map(Target::key).collect();
        a.irreversible = r.irreversible;
        a.urgent = order.urgent;
        a.claims = (r.irreversible && !unnamed_key(&a)) || job.is_some();
        a.turn = self.current_turn();
        let internal = |e: eyre::Report| {
            tracing::warn!(error = %format!("{e:#}"), "a boss's assignment couldn't be kept");
            "Not sent: Desk couldn't record who holds this job just now, so it sent nothing. Try again in a moment.".to_string()
        };
        let old = match order.replaces.as_deref() {
            None => None,
            Some(named) => {
                let live = self
                    .desk
                    .store()
                    .assignments_of(boss, true)
                    .await
                    .map_err(internal)?;
                let ids: Vec<Uuid> = live.iter().map(|l| l.id).collect();
                let id = assignment::named(named, &ids).ok_or_else(|| {
                    format!("Not sent: {named} isn't one of your open assignments; crew.assignments lists them.")
                })?;
                live.into_iter().find(|l| l.id == id)
            }
        };
        let name = self.session_name(s);
        let mut guard = None;
        let mut stand_down_of = None;
        let mut why = String::new();
        if let Some(old) = &old {
            a.replaces = Some(old.id);
            guard = Some(CHECK_FIRST.to_string());
            if old.session == s && old.machine.is_none() {
                why = format!("replaced by {}", short(a.id));
            } else if self.holder_gone(old) {
                why = format!("moved to {name}; its holder had gone");
            } else {
                why = format!("moved to {name}");
                let working = self
                    .desk
                    .view(old.session)
                    .is_ok_and(|v| v.state == SessionState::Working);
                // It may have started it: what it says first decides.
                let may_have = (old.irreversible && old.delivered_at.is_some()) || working;
                if may_have {
                    a.state = AssignmentState::Held;
                    a.action = kept_form(action);
                    a.basis = Some(
                        self.deciding_now()
                            .map_or(self.desk.picture_seq(), |d| d.basis)
                            as i64,
                    );
                }
                stand_down_of = Some(old.clone());
            }
        }
        let reserved = self
            .desk
            .store()
            .assignment_reserve(&a, old.as_ref().map(|o| (o.id, why.as_str())))
            .await
            .map_err(internal)?;
        let superseded = match reserved {
            Reserved::Refused { rule, holder } => {
                let why = self.refusal(rule, &holder, &a);
                tracing::info!(session = %s, %boss, holder = %holder.id, ?rule, "a boss's order for a job another assignment holds wasn't sent");
                self.note_assignment(Kind::AssignmentRefused, &by.party, &a, why.clone());
                return Err(why);
            }
            Reserved::Kept { superseded, .. } => superseded,
        };
        // An irreversible order not yet in, taken over by this one: it
        // never goes in, or the session would be told twice.
        for was in superseded.iter().filter(|w| w.irreversible) {
            let Some(line) = was.line_message else {
                continue;
            };
            match self.desk.line_withdraw(line).await {
                Ok(Some(_)) => {}
                _ => {
                    // It went in meanwhile: it stands, and this doesn't go.
                    let store = self.desk.store();
                    let _ = store
                        .assignment_close(
                            a.id,
                            AssignmentState::Failed,
                            "desk",
                            "its earlier order went in meanwhile",
                        )
                        .await;
                    let _ = store.assignment_reopen(was.id).await;
                    let mut held = was.clone();
                    held.delivered_at = held.delivered_at.or(Some(Utc::now()));
                    return Err(self.refusal(Rule::SameSession, &held, &a));
                }
            }
        }
        if let Some(old) = &stand_down_of {
            let line = self.stand_down(by, old, &a, &name).await;
            if a.state == AssignmentState::Held {
                if let Some(line) = line {
                    if let Err(e) = self.desk.store().assignment_wait_on(a.id, line).await {
                        tracing::warn!(error = %format!("{e:#}"), "a held assignment's stand-down wasn't kept with it");
                    }
                }
                let old_name = self.holder_name(old);
                let said = format!(
                    "held {} for {name}: {old_name} may have started {} already, so Desk told it to stand down first. Your order goes to {name} once {old_name} answers (or its session goes), checked against what has changed by then; if it hasn't answered within {HOLD_FOR} minutes, it's the person's.",
                    short(a.id),
                    old.about()
                );
                tracing::info!(session = %s, %boss, old = %old.id, "a reassigned job is held until its old holder stands down");
                self.note_assignment(Kind::AssignmentHeld, &by.party, &a, said.clone());
                return Ok(Before::Held(said));
            }
        }
        Ok(Before::Send(Reservation { id: a.id, guard }))
    }

    /// Tells `old`'s holder to stand down from it: `new` is moved to
    /// `name`. The one place a stand-down is sent, so the urgent delivery
    /// to come (§4) takes it from here; for now it is a line message, queued
    /// as any other, that asks for an answer. While the person drives the
    /// old holder (or took it back), it goes to them instead, never into
    /// the session. The line message's id, when one was sent.
    async fn stand_down(
        &self,
        by: &By,
        old: &Assignment,
        new: &Assignment,
        name: &str,
    ) -> Option<Uuid> {
        let text = format!(
            "Stand down from {} ({}): it is moved to {name}. Don't start it. If you already started it, answer this message with the run id.",
            short(old.id),
            old.about()
        );
        let person = old.machine.is_none()
            && (self.desk.wheel_of(old.session).is_some_and(|w| w.person())
                || self.taken_back(old.session));
        if old.machine.is_some() || person {
            let what = format!(
                "{} should stand down from {} ({}): it is moved to {name}. You are driving it, so Desk hasn't told it; tell it, or check it hasn't started it.",
                self.holder_name(old),
                short(old.id),
                old.about()
            );
            let mut item = NewItem::ask(&old.boss, OpenKind::Todo, Some(old.session), &what);
            item.dedupe_key = format!("stand-down:{}", new.id);
            if let Err(e) = self.desk.store().item_open(&item).await {
                tracing::warn!(error = %format!("{e:#}"), "a stand-down for the person wasn't kept");
            }
            self.tell_dm(&old.boss, &what).await;
            return None;
        }
        let sent = self
            .desk
            .line_send(
                workforce_desk::LineSender::Overseer(by.clone()),
                workforce_desk::line::Send::Ask {
                    to: workforce_desk::line::Target::Session(old.session),
                    text: text.clone(),
                    wait: std::time::Duration::ZERO,
                    chose: None,
                    parts: Default::default(),
                },
            )
            .await;
        match sent {
            Ok(m) => {
                tracing::info!(session = %old.session, old = %old.id, "a session was told to stand down from a job moved on");
                self.picture_instruction(&old.boss, old.session, &text, self.current_turn());
                Some(m.id)
            }
            Err(e) => {
                tracing::warn!(session = %old.session, error = ?e, "a stand-down couldn't be sent");
                None
            }
        }
    }

    /// The reserved order `r` was sent (`done_line`, its line message) or
    /// wasn't.
    pub(super) async fn assign_after(
        &self,
        r: &Reservation,
        by: &By,
        line: Option<Uuid>,
        ok: bool,
    ) {
        match self.desk.store().assignment_sent(r.id, line, ok).await {
            Ok(Some(a)) if ok => {
                let text = format!(
                    "gave {} {} ({}){}",
                    self.holder_name(&a),
                    short(a.id),
                    a.about(),
                    if a.irreversible {
                        ", which can't be undone"
                    } else {
                        ""
                    }
                );
                self.note_assignment(Kind::Assigned, &by.party, &a, text);
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(assignment = %r.id, error = %format!("{e:#}"), "an assignment's sending wasn't kept")
            }
        }
    }

    /// Reserves `input`, the home's own boss `boss`'s call for `machine`'s
    /// session `id`, before it is relayed: the home is where a local boss's
    /// assignments are kept, so its sessions here and on machines are
    /// compared. None when it is no assignment.
    pub(super) async fn assign_relayed(
        &self,
        boss: &str,
        machine: &str,
        id: Uuid,
        tool: &str,
        input: &Value,
    ) -> Result<Option<Uuid>, String> {
        let text = match tool {
            "crew.nudge"
                if input["reply_to"]
                    .as_str()
                    .is_none_or(|r| r.trim().is_empty()) =>
            {
                input["message"].as_str()
            }
            "crew.resume" | "crew.restart" => input["prompt"].as_str(),
            _ => None,
        };
        let Some(text) = text.map(str::trim).filter(|t| !t.is_empty()) else {
            return Ok(None);
        };
        let order = Order::of(input);
        let r = unnamed(
            assignment::read(text, order.job.as_deref(), machine, order.irreversible),
            machine,
        );
        let mut a = Assignment::new(boss, id, text);
        a.machine = Some(machine.to_string());
        a.job = order.job.clone();
        a.job_key = r.job_key;
        a.targets = r.targets.iter().map(Target::key).collect();
        a.irreversible = r.irreversible;
        a.urgent = order.urgent;
        a.claims = (r.irreversible && !unnamed_key(&a)) || order.job.is_some();
        a.turn = self.current_turn();
        match self.desk.store().assignment_reserve(&a, None).await {
            Ok(Reserved::Kept { .. }) => Ok(Some(a.id)),
            Ok(Reserved::Refused { rule, holder }) => Err(self.refusal(rule, &holder, &a)),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "a relayed assignment couldn't be kept");
                Err("Not sent: Desk couldn't record who holds this job just now, so it sent nothing. Try again in a moment.".into())
            }
        }
    }

    /// A relayed assignment was done on its machine, or wasn't.
    pub(super) async fn assign_relayed_done(&self, id: Uuid, ok: bool) {
        if let Err(e) = self.desk.store().assignment_sent(id, None, ok).await {
            tracing::warn!(assignment = %id, error = %format!("{e:#}"), "a relayed assignment's outcome wasn't kept");
        }
    }

    /// Why `boss` may not allow the request `pending` of `session` itself,
    /// if it may not: it is irreversible, and another session's irreversible
    /// assignment is on the same job or acts on the same thing (R5). Then
    /// it is the person's.
    pub(super) async fn request_conflict(
        &self,
        boss: &str,
        session: Uuid,
        pending: &workforce_desk::model::PendingApproval,
    ) -> Option<String> {
        let asked = format!("{} {}", pending.tool, pending.detail);
        if !assignment::irreversible(&asked) {
            return None;
        }
        let live = self.desk.store().assignments_of(boss, true).await.ok()?;
        if !live.iter().any(|a| a.irreversible && a.session != session) {
            return None;
        }
        let (repo, place) = self.place_of(session).await;
        let r = assignment::read(&asked, None, &place, true);
        let theirs = live.into_iter().find(|a| {
            a.irreversible
                && (a.session != session || a.machine.is_some())
                && (a.repo.is_none() || repo.is_none() || a.repo == repo)
                && ((a.job_key.is_some() && a.job_key == r.job_key)
                    || assignment::share_target(&a.parsed_targets(), &r.targets))
        })?;
        tracing::info!(%session, %boss, holder = %theirs.id, "a request for a job another session holds goes to the person");
        Some(format!(
            "{} holds {} ({}), so allowing this here could run it twice",
            self.holder_name(&theirs),
            theirs.about(),
            short(theirs.id)
        ))
    }

    /// Sends the held assignment `id` now: its old holder answered its
    /// stand-down (`answer`), or is gone. Through `perform`, as any send,
    /// checked against the picture as it was when it was held; if the
    /// picture moved, the boss decides again.
    pub(super) async fn release_held(self: &Arc<Self>, id: Uuid, answer: Option<String>) {
        let store = self.desk.store();
        let Ok(Some(a)) = store.assignment_release(id, answer.as_deref()).await else {
            return;
        };
        let (boss, session, name) = (a.boss.clone(), a.session, self.holder_name(&a));
        let fail = |why: String| async move {
            let _ = self
                .desk
                .store()
                .assignment_close(id, AssignmentState::Failed, "desk", &why)
                .await;
            self.wake_for(
                &boss,
                session,
                format!(
                    "{} for {name} wasn't sent after its old holder stood down: {why}",
                    short(id)
                ),
            );
        };
        let Some(action) = a
            .action
            .as_ref()
            .and_then(|v| Action::from_suggestion(v).ok())
        else {
            return fail("Desk kept no order to send".into()).await;
        };
        let guard = match &answer {
            Some(said) => format!(
                "{CHECK_FIRST} Its last holder answered Desk: “{}”",
                crew::one_line(said, 300)
            ),
            None => format!("{CHECK_FIRST} Its last holder's session has gone."),
        };
        let action = guarded(&action, &guard).unwrap_or(action);
        let Some((service, o)) = self.overseer_named(&a.boss) else {
            return fail("you no longer oversee its session".into()).await;
        };
        if let super::actions::Gate::Run = self.gate(&action, &o) {
        } else {
            return fail("you may no longer send it yourself".into()).await;
        }
        if let Some(held) = self.desk.agent_held_why(&a.boss) {
            return fail(format!("you are {held}")).await;
        }
        let by = self.by_boss(&service, "Its old holder stood down.");
        let d = Deciding {
            turn: a.turn.clone().unwrap_or_else(|| format!("held:{id}")),
            basis: a.basis.map_or(self.desk.picture_seq(), |b| b.max(0) as u64),
        };
        let order = Order {
            released: Some(id),
            ..Default::default()
        };
        match self.perform_in_with(Some(d), &action, by, &order).await {
            Ok(said) => tracing::info!(assignment = %id, %said, "a held assignment went out"),
            Err(why) => {
                // `perform` marked it failed: the boss decides again.
                self.wake_for(
                    &a.boss,
                    a.session,
                    format!(
                        "{} for {} was held until its old holder stood down, and wasn't sent then: {why}",
                        short(id),
                        self.holder_name(&a)
                    ),
                );
            }
        }
    }

    /// Wakes `boss` with `line` about `session`.
    fn wake_for(self: &Arc<Self>, boss: &str, session: Uuid, line: String) {
        let Some((service, o)) = self.overseer_named(boss) else {
            tracing::info!(%boss, line, "an assignment's news has no boss here to wake");
            return;
        };
        let group = self
            .desk
            .view(session)
            .ok()
            .and_then(|v| v.group.map(|g| g.id));
        let t = super::watcher::Trigger {
            line,
            approvals: Vec::new(),
            hop: 0,
            about: Some(format!("assignment:{session}")),
            group,
        };
        let crew = Arc::clone(self);
        tokio::spawn(async move {
            crew.wake(service, o, vec![t]).await;
        });
    }

    /// `m` came on a line: what it says of the assignments it carries or
    /// answers.
    async fn assignment_heard(self: &Arc<Self>, m: &LineMessage) {
        let store = self.desk.store();
        // Its own message: delivered, or never.
        if m.from.kind == PartyKind::Manager {
            match m.delivery {
                Delivery::Delivered | Delivery::Handed => {
                    let _ = store
                        .assignment_delivered(m.id, m.delivered_at.unwrap_or(m.updated_at))
                        .await;
                }
                Delivery::Withdrawn | Delivery::Refused => {
                    if let Ok(Some(a)) = store.assignment_of_line(m.id).await {
                        let why = "its message never went in";
                        if let Ok(Some(a)) = store
                            .assignment_close(a.id, AssignmentState::Cancelled, "desk", why)
                            .await
                        {
                            self.note_assignment(
                                Kind::AssignmentClosed,
                                &Party::manager(&a.boss, &a.boss),
                                &a,
                                format!("cancelled {}: {why}", short(a.id)),
                            );
                        }
                    }
                }
                _ => {}
            }
            return;
        }
        let Some(from) = m.from_session() else {
            return;
        };
        if m.at != m.updated_at && m.kind != LineKind::Answer {
            return;
        }
        let Ok(live) = store.assignments_live().await else {
            return;
        };
        for a in &live {
            // An answer to one of its own assignments.
            if a.session == from && a.state == AssignmentState::Open {
                if let Some(line) = a.line_message.filter(|l| answers(m, *l)) {
                    let _ = line;
                    let _ = store.assignment_acked(a.id, m.at).await;
                }
            }
            // An answer to a stand-down another assignment waits on.
            if a.state == AssignmentState::Held {
                if let Some(sd) = a.stand_down.filter(|sd| answers(m, *sd)) {
                    let _ = sd;
                    let said = workforce_desk::line::without_marks(&m.text);
                    tracing::info!(assignment = %a.id, session = %from, "a stand-down was answered: its job goes to its new holder");
                    self.release_held(a.id, Some(said)).await;
                }
            }
        }
    }

    /// Looks at every live assignment: held ones whose old holder has gone
    /// go out, and those waiting past [`HOLD_FOR`] are the person's;
    /// reversible ones past their lapse lapse; done ones close when the
    /// boss lets accounts close them.
    async fn assignments_look(self: &Arc<Self>, raised: &mut HashSet<Uuid>) {
        let store = self.desk.store();
        let Ok(live) = store.assignments_live().await else {
            return;
        };
        let now = Utc::now();
        let minute = chrono::TimeDelta::from_std(self.timing.minute).unwrap_or_default();
        let lapse = minute * (assignment::LAPSE.num_minutes() as i32);
        for a in &live {
            match a.state {
                AssignmentState::Held => {
                    let old = match a.replaces {
                        Some(o) => store.assignment(o).await.ok().flatten(),
                        None => None,
                    };
                    if old.as_ref().is_none_or(|o| self.holder_gone(o)) {
                        self.release_held(a.id, None).await;
                        continue;
                    }
                    if now - a.sent_at >= minute * HOLD_FOR as i32 && raised.insert(a.id) {
                        let old = old.unwrap_or_else(|| a.clone());
                        let line = format!(
                            "{} hasn't answered its stand-down; {} is held for {} ({}).",
                            self.holder_name(&old),
                            a.about(),
                            self.holder_name(a),
                            short(a.id)
                        );
                        let mut item = NewItem::ask(&a.boss, OpenKind::Todo, Some(a.session), &format!(
                            "{line} Check it hasn't started it, then close {} or tell Desk to send it.",
                            short(old.id)
                        ));
                        item.dedupe_key = format!("held:{}", a.id);
                        if let Err(e) = store.item_open(&item).await {
                            tracing::warn!(error = %format!("{e:#}"), "a held assignment wasn't given to the person");
                        }
                        tracing::info!(assignment = %a.id, "a held assignment's stand-down went unanswered: it is the person's");
                        self.wake_for(&a.boss, a.session, line);
                    }
                }
                AssignmentState::Open if !a.irreversible && now - a.sent_at >= lapse => {
                    let _ = store
                        .assignment_close(
                            a.id,
                            AssignmentState::Lapsed,
                            "desk",
                            "nothing closed it for 12 hours",
                        )
                        .await;
                }
                AssignmentState::Open if a.machine.is_none() => {
                    if let Some(done) = self.looks_done(a).await {
                        let closes = self
                            .overseer_named(&a.boss)
                            .is_some_and(|(_, o)| o.updates.close_from_accounts);
                        if closes {
                            let why = format!("its account says {done}");
                            if let Ok(Some(c)) = store
                                .assignment_close(a.id, AssignmentState::Done, "session", &why)
                                .await
                            {
                                self.note_assignment(
                                    Kind::AssignmentClosed,
                                    &Party::manager(&a.boss, &a.boss),
                                    &c,
                                    format!("{} is done: {why}", short(c.id)),
                                );
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// What `a`'s session's account says it did of `a`'s job since it got
    /// it, if it did it: a release or deploy deployed, a PR merged.
    async fn looks_done(&self, a: &Assignment) -> Option<String> {
        let t = assignment::strongest(&a.parsed_targets())?.clone();
        let done_when = match t.kind {
            TargetKind::Pr => "merged",
            TargetKind::Release | TargetKind::Workflow | TargetKind::Run | TargetKind::Commit => {
                "deployed"
            }
            TargetKind::Environment => return None,
        };
        let account = self
            .desk
            .session_accounts_of(&[a.session])
            .await
            .ok()?
            .into_iter()
            .next()?;
        let d = workforce_desk::open_item::kept_by(done_when, &account.done, a.sent_at)?;
        Some(crew::one_line(&d.text, 120))
    }

    /// After a restart: what was still being sent two minutes before
    /// `now` is settled ([`workforce_desk::DeskStore::assignments_reconcile`]).
    pub(super) async fn assignments_after_restart(&self, now: DateTime<Utc>) {
        match self
            .desk
            .store()
            .assignments_reconcile(now - chrono::TimeDelta::minutes(2))
            .await
        {
            Ok(settled) if !settled.is_empty() => {
                tracing::info!(
                    settled = settled.len(),
                    "assignments a restart left being sent were settled"
                )
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "assignments a restart left being sent couldn't be settled")
            }
        }
    }

    /// What a boss's turn reads of its assignments: the open and held ones,
    /// newest first, at most 20.
    pub(super) async fn assignments_block(&self, boss: &str) -> Option<String> {
        let live = self.desk.store().assignments_of(boss, true).await.ok()?;
        if live.is_empty() {
            return None;
        }
        let mut lines = Vec::new();
        for a in live.iter().take(20) {
            lines.push(format!("- {}", self.assignment_line(a).await));
        }
        Some(format!(
            "Your open assignments, newest first (one session holds a job at a time; move one with replaces):\n{}",
            lines.join("\n")
        ))
    }

    /// One assignment in a line: `a:id → session · job · times · state`.
    async fn assignment_line(&self, a: &Assignment) -> String {
        let tz = self.desk.timezone();
        let at = |t: DateTime<Utc>| t.with_timezone(&tz).format("%H:%M").to_string();
        let mut times = vec![format!("sent {}", at(a.sent_at))];
        if let Some(d) = a.delivered_at {
            times.push(format!("delivered {}", at(d)));
        }
        match (a.acked_at, self.written_since(a)) {
            (Some(t), _) => times.push(format!("answered {}", at(t))),
            (None, Some((t, _))) if a.delivered_at.is_some() => {
                times.push(format!("it has written since {}", at(t)))
            }
            _ => {}
        }
        let mut state = match a.state {
            AssignmentState::Held => {
                let from = match a.replaces {
                    Some(o) => self.desk.store().assignment(o).await.ok().flatten(),
                    None => None,
                };
                format!(
                    "held: waits for {} to answer its stand-down",
                    from.as_ref()
                        .map_or_else(|| "its old holder".into(), |o| self.holder_name(o))
                )
            }
            s => s.as_str().to_string(),
        };
        if a.irreversible {
            state.push_str(" · can't be undone");
            if Utc::now() - a.sent_at >= assignment::OLD {
                state.push_str(" · old: check where it stands");
            }
        }
        if self.holder_gone(a) {
            state.push_str(" · holder gone: replaces moves it at once");
        } else if a.state == AssignmentState::Open && a.machine.is_none() {
            if let Some(done) = self.looks_done(a).await {
                state.push_str(&format!(
                    " · looks done (“{done}”): close it with crew.assignment"
                ));
            }
        }
        format!(
            "{} → {} · {} · {} · {state}",
            short(a.id),
            self.holder_name(a),
            a.about(),
            times.join(", ")
        )
    }

    /// `crew.assignments`: the boss's assignments in full.
    pub(super) async fn assignments_tool(
        &self,
        boss: &str,
        input: &Value,
    ) -> Result<Value, String> {
        let all = input["all"].as_bool().unwrap_or(false);
        let list = self
            .desk
            .store()
            .assignments_of(boss, !all)
            .await
            .map_err(|e| {
                tracing::warn!(error = %format!("{e:#}"), "the assignments couldn't be read");
                "Your assignments couldn't be read just now.".to_string()
            })?;
        let mut rows = Vec::new();
        for a in &list {
            let mut row = self.assignment_line(a).await;
            if !a.state.live() {
                if let Some(why) = &a.closed_why {
                    row.push_str(&format!(" ({why})"));
                }
            }
            rows.push(json!({ "line": row, "text": crew::clip(&a.text, 600) }));
        }
        Ok(
            json!({ "assignments": super::watcher::reported(&json!(rows).to_string(), "your assignments") }),
        )
    }

    /// `crew.assignment`: the boss closes one of its own, through the same
    /// gate as a message to its session.
    pub(super) async fn assignment_tool(
        &self,
        b: &super::tools::Bound,
        input: &Value,
    ) -> Result<Value, String> {
        let boss = b.service.agent_id.as_str();
        let state = match input["state"].as_str().map(str::trim) {
            Some("done") => AssignmentState::Done,
            Some("cancelled" | "canceled") => AssignmentState::Cancelled,
            _ => return Err("Say state: done or cancelled.".into()),
        };
        let note = input["note"]
            .as_str()
            .or(input["reason"].as_str())
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .ok_or("Say why in note: the person reads it.")?;
        let live = self
            .desk
            .store()
            .assignments_of(boss, true)
            .await
            .map_err(|_| "Your assignments couldn't be read just now.".to_string())?;
        let ids: Vec<Uuid> = live.iter().map(|a| a.id).collect();
        let named = input["id"].as_str().unwrap_or_default();
        let id = assignment::named(named, &ids).ok_or_else(|| {
            format!("{named} isn't one of your open assignments; crew.assignments lists them.")
        })?;
        let a = live
            .iter()
            .find(|a| a.id == id)
            .cloned()
            .ok_or("That assignment is closed already.")?;
        let as_message = Action::Nudge {
            to: To::Session(a.session),
            message: note.to_string(),
            reply_to: None,
        };
        if a.machine.is_none() {
            match self.gate(&as_message, &b.oversight) {
                super::actions::Gate::Run => {}
                super::actions::Gate::Suggest(why) | super::actions::Gate::No(why) => {
                    return Err(format!("Not closed: {why}. Tell the person instead."))
                }
                super::actions::Gate::Refuse => {
                    return Err("You only watch the crew: tell the person instead.".into())
                }
            }
        }
        let closed = self
            .desk
            .store()
            .assignment_close(id, state, "boss", note)
            .await
            .map_err(|_| "It couldn't be closed just now.".to_string())?
            .ok_or("That assignment is closed already.")?;
        let by = self.by_boss(&b.service, note);
        self.note_assignment(
            Kind::AssignmentClosed,
            &by.party,
            &closed,
            format!(
                "closed {} ({}) as {}",
                short(id),
                closed.about(),
                state.as_str()
            ),
        );
        Ok(json!({ "done": format!("closed {} as {}", short(id), state.as_str()) }))
    }
}

/// Keeps the assignments up to date with what happens on the lines, and
/// looks at them every crew minute. First settles what a restart left
/// being sent.
pub(super) async fn assignments_loop(crew: Arc<CrewHub>, cancel: CancellationToken) {
    let (_, mut events) = crew.desk.subscribe();
    crew.assignments_after_restart(Utc::now()).await;
    let mut every = tokio::time::interval(crew.timing.minute);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut raised: HashSet<Uuid> = HashSet::new();
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            received = events.recv() => match received {
                Ok(DeskEvent::Line { message, removed: false }) => {
                    crew.assignment_heard(&message).await;
                }
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return,
            },
            _ = every.tick() => crew.assignments_look(&mut raised).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stand_down_is_answered_by_a_reply_or_its_quoted_mark_only() {
        let sd = Uuid::new_v4();
        let mark = &sd.simple().to_string()[..8];
        let msg = |text: &str, reply: Option<Uuid>| {
            let mut m = LineMessage::new(
                Party::you(),
                Party::manager("desk", "Desk"),
                LineKind::Tell,
                text.into(),
                1,
            );
            m.in_reply_to = reply;
            m
        };
        assert!(answers(&msg("Not started.", Some(sd)), sd));
        assert!(answers(
            &msg(&format!("Ack m:{mark}, nothing started"), None),
            sd
        ));
        assert!(
            !answers(&msg("I will dispatch now", None), sd),
            "a message after it isn't an answer"
        );
        assert!(!answers(&msg("Ack m:00000000", None), sd));
    }

    #[test]
    fn an_order_reads_its_fields_and_none_is_needed() {
        let o = Order::of(
            &json!({ "job": " release 4 ", "irreversible": true, "replaces": "a:3f2a1b2c" }),
        );
        assert_eq!(o.job.as_deref(), Some("release 4"));
        assert!(o.irreversible && !o.urgent);
        assert_eq!(o.replaces.as_deref(), Some("a:3f2a1b2c"));
        let none = Order::of(&json!({ "message": "Carry on." }));
        assert!(none.job.is_none() && none.replaces.is_none() && !none.irreversible);
    }
}
