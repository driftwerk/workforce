//! Who holds which job (designs/boss-orders-and-serial-turns.md §3): every
//! instruction a boss gives a session is an assignment
//! ([`workforce_desk::assignment`]), kept until it is done, cancelled,
//! superseded or let lapse.
//!
//! - **The job is declared.** A boss's order (a message, ticket, resume or
//!   restart prompt, or a start's goal) says the job it hands out
//!   (`job: { kind, target, key }`); only a declared job is kept and
//!   compared, never one read from its words. An order that looks like it
//!   can't be taken back and declares none isn't sent; one whose words
//!   seem to name a job another session holds asks the boss to say which
//!   job it is ([`assignment::looks_like`]), and claims nothing.
//! - **Reserved before it is sent.** [`CrewHub::perform_with`] checks it
//!   against the boss's live assignments in its session's lane, after the
//!   picture's own check, under the boss's Postgres lock: a second session
//!   is never given a job another holds (R2), and a session that has an
//!   irreversible job is never told to start it again (R1). Whichever of
//!   the boss's turns sends it, it is the same reservation.
//! - **Moved with `replaces`.** The old holder is told to stand down (a
//!   line message that asks for an answer), and the new holder's order is
//!   held while the old one may have started it, until the old holder
//!   answers the stand-down or its session goes; then it goes through
//!   `perform`, on the picture as it was when it was held. The person (or
//!   the boss, once it knows) can let it go; unanswered for [`HOLD_FOR`],
//!   it is the person's. Its old order not yet in is withdrawn first, and
//!   a stand-down that can't be sent holds it for the person.
//! - **Approvals** of an irreversible request beside another session's
//!   irreversible job in the same repository are the person's, never the
//!   boss's (R5, [`CrewHub::request_conflict`]).
//! - **Kept up to date** by [`assignments_loop`]: delivered and answered
//!   from the line, cancelled when its message is withdrawn, lapsed when
//!   reversible and old, closed from the session's account with
//!   `close_from_accounts`, and what a restart left being sent settled.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_desk::assignment::{self, short, Assignment, AssignmentState, Job, Rule, Target};
use workforce_desk::assignment_store::{Reserved, Restored, NEVER_WENT_IN};
use workforce_desk::crew::{self, By, Event, Kind, Party, PartyKind};
use workforce_desk::line::{Delivery, LineKind, LineMessage};
use workforce_desk::model::{DeskEvent, SessionState};
use workforce_desk::open_item::{NewItem, OpenKind};

use super::actions::{Action, Handout, Target as To, STATUS_QUESTION};
use super::picture::Deciding;
use super::refusal::{Refusal, Refused};
use super::CrewHub;

/// The boss's tool that closes one of its assignments.
pub(crate) const CLOSE: &str = "crew.assignment";
/// The boss's tool that lists them.
pub(crate) const LIST: &str = "crew.assignments";

pub(crate) const CLOSE_DESCRIPTION: &str = "Close one of your assignments (id, as a:… from crew.assignments): state done when its session did it, or cancelled when it no longer should; note says why, in a sentence the person reads. Closing it lets you give that job to another session. state release lets a held one go to its new holder once you know its old holder didn't start it.";
pub(crate) const LIST_DESCRIPTION: &str = "Your assignments: every job you gave a session (each order that declared its job), with who holds it, when it was sent, delivered and answered, and whether it can be undone. With all, the last day's closed ones too. A job one session holds isn't given to another: move it with crew.nudge's replaces.";

/// How long a held assignment waits for its old holder to answer its
/// stand-down, in the crew's minutes, before it is the person's.
pub(crate) const HOLD_FOR: u32 = 10;

/// What Desk adds to an order that takes a job over, or goes out after a
/// stand-down: the new holder checks first.
const CHECK_FIRST: &str = "Before you start it, check it hasn't already been started.";

/// How a job is declared, as the boss's refusals show it.
const JOB_EXAMPLE: &str =
    r#"job: { "kind": "release", "target": "mantis-crm/monorepo", "key": "release-4" }"#;

/// The field a relayed call carries the home's irreversible jobs in, for
/// the machine to compare a request with (R5).
pub(super) const HELD_JOBS: &str = "held_jobs";

/// What a boss's order says of the job it hands out: the job it declares,
/// never one read from its words.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum Declared {
    /// Nothing said: it holds no job.
    #[default]
    Unsaid,
    /// `job: "none"`: the boss says it is no job.
    NoJob,
    /// The job; its target, left out, is the session's repository.
    Job {
        kind: String,
        target: Option<String>,
        key: String,
    },
    /// Said, but not as a job is: what the boss is told.
    Malformed(String),
}

impl Declared {
    fn of(v: &Value) -> Self {
        let field = |o: &serde_json::Map<String, Value>, k: &str| {
            o.get(k)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
        };
        match v {
            Value::Null => Self::Unsaid,
            Value::String(s) if s.trim().is_empty() => Self::Unsaid,
            Value::String(s) if s.trim().eq_ignore_ascii_case("none") => Self::NoJob,
            Value::Object(o) => match (field(o, "kind"), field(o, "key")) {
                (Some(kind), Some(key)) => Self::Job {
                    kind,
                    target: field(o, "target"),
                    key,
                },
                _ => Self::Malformed(format!(
                    "Not sent: a job says its kind and its key, as {JOB_EXAMPLE}."
                )),
            },
            _ => Self::Malformed(format!(
                "Not sent: say the job as {JOB_EXAMPLE}, or job: \"none\" when this isn't one."
            )),
        }
    }
}

/// What a boss's order says besides its words: the fields of `crew.nudge`
/// (and of a ticket, a resume, a restart or a start). Every field is
/// optional: an order that declares no job holds none.
#[derive(Debug, Clone, Default)]
pub(crate) struct Order {
    pub job: Declared,
    /// The boss says it can't be taken back, whatever the words say.
    pub irreversible: bool,
    /// It can't wait for the session's turn to end: steered into the
    /// running turn (§4, [`super::urgent`]).
    pub urgent: bool,
    /// An interrupt for a stand-down not read in time: of the turn it was
    /// sent in alone, and only while it is still unread (the stand-down's
    /// row, and that turn).
    pub only_turn: Option<(Uuid, workforce_desk::line::TurnMark)>,
    /// The assignment it takes over from, as the boss names it (`a:…`).
    pub replaces: Option<String>,
    /// Kept at the home, not here: a call the home relayed.
    pub unrecorded: bool,
    /// A held assignment going out now: its row, already reserved.
    pub released: Option<Uuid>,
    /// Relayed from the boss's home: the irreversible jobs it keeps for its
    /// other sessions, which a request here is compared with (R5).
    pub held_elsewhere: Option<Vec<Assignment>>,
    /// The turn end whose question this message answers (`answer`): while
    /// that is still what its session asks, it goes in as the answer.
    pub answers: Option<workforce_desk::line::TurnMark>,
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
            job: Declared::of(&input["job"]),
            irreversible: input["irreversible"].as_bool().unwrap_or(false),
            urgent: input["urgent"].as_bool().unwrap_or(false),
            replaces: text("replaces"),
            held_elsewhere: input
                .get(HELD_JOBS)
                .and_then(|v| serde_json::from_value(v.clone()).ok()),
            ..Default::default()
        }
    }
}

/// The properties a crew tool that hands out work takes for its order.
/// `urgent` only where it is steered in: a line message (crew.nudge).
pub(crate) fn order_properties(with_job: bool, urgent: bool) -> Value {
    let mut p = json!({
        "replaces": { "type": "string", "description": "The assignment (a:… from crew.assignments) this takes over, with its job: its holder is told to stand down first, and this waits until it answers when it may have started" }
    });
    if urgent {
        p["urgent"] = json!({ "type": "boolean", "description": super::urgent::description() });
    }
    if with_job {
        p["job"] = json!({
            "type": ["object", "string"],
            "description": "The job this hands out, so no second session is given it: { kind, target, key }. Declare one with any release, deploy, merge to a protected branch, migration, publish, dispatch or rollback, and with any work only one session should do. kind: release, deploy, merge, migration, publish, dispatch, rollback, or another word for it. target: the repository it is in, owner/name (left out: the session's own). key: the job's own id, the same whoever says it and however (release-4, pr-711, the migration's file name, the run id); never a session's name. \"none\" when this isn't a job. An order that looks like it can't be taken back isn't sent without one.",
            "properties": {
                "kind": { "type": "string" },
                "target": { "type": "string" },
                "key": { "type": "string" }
            }
        });
        p["irreversible"] = json!({ "type": "boolean", "description": "true when it starts something that can't be taken back (a release, deploy, merge, migration, publish, delete), whatever the words say; it then needs its job" });
    }
    p
}

/// A reservation made for an order about to be sent.
#[derive(Debug, Clone)]
pub(super) struct Reservation {
    pub id: Uuid,
    /// What Desk adds to the order: the guard to check first.
    pub guard: Option<String>,
    /// Kept for a session the order starts: its id once started.
    pub starts: bool,
}

/// A relayed order kept here before it goes to its machine: its row, and
/// what to add to what it says.
pub(super) struct Relayed {
    pub id: Uuid,
    pub guard: Option<&'static str>,
    /// Held for its old holder's stand-down, not relayed now: what the boss
    /// is told.
    pub held: Option<String>,
}

/// What [`CrewHub::assign_before`] decided.
pub(super) enum Before {
    /// No job: an answer, a status question, the person's, or plain words.
    Not,
    /// Send it, reserved.
    Send(Reservation),
    /// Held for its old holder's stand-down; what the boss is told.
    Held(String),
}

/// Who an order goes to: a session, or one started for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Holder {
    Session(Uuid),
    New,
}

/// Who `action`, a boss's, gives work to, what it says, and, for a ticket,
/// its own title and body: what is read for whether it can be undone.
fn assigned(action: &Action) -> Option<(Holder, String, Option<String>)> {
    match action {
        Action::Nudge {
            to: To::Session(s),
            message,
            reply_to: None,
        } if !message.starts_with(STATUS_QUESTION) => {
            Some((Holder::Session(*s), message.clone(), None))
        }
        Action::AssignTicket {
            ticket, to, text, ..
        } => {
            let holder = match to {
                Handout::Session(s) => Holder::Session(*s),
                Handout::NewSession { .. } => Holder::New,
                Handout::Agent(_) => return None,
            };
            Some((
                holder,
                format!("Ticket {ticket} is yours to work on."),
                Some(format!("{ticket}: {text}")),
            ))
        }
        Action::Resume {
            session,
            prompt: Some(p),
        }
        | Action::Restart {
            session,
            prompt: Some(p),
        } => Some((Holder::Session(*session), p.clone(), None)),
        Action::StartSession { goal, .. } if !goal.trim().is_empty() => {
            Some((Holder::New, goal.clone(), None))
        }
        _ => None,
    }
}

/// The job `said` declares, `repo` being the session's: one taken over
/// with `replaces` is the old one's, unless another is said. A ticket
/// holds no job of its own unless one is declared with it: the board
/// already gives a ticket to one holder.
fn job_of(
    said: &Declared,
    repo: Option<&str>,
    old: Option<&Assignment>,
) -> Result<Option<Job>, Refused> {
    let unclear = |why: String| Refused::guard(Refusal::JobUnclear, why);
    match said {
        Declared::Malformed(why) => Err(unclear(why.clone())),
        Declared::Job { kind, target, key } => {
            let Some(target) = target.as_deref().or(repo) else {
                return Err(unclear(format!(
                    "Not sent: say which repository the job is in, as {JOB_EXAMPLE}: Desk doesn't know this session's."
                )));
            };
            Job::new(kind, target, key).map(Some).ok_or_else(|| {
                unclear(format!(
                    "Not sent: a job says its kind, target and key, as {JOB_EXAMPLE}."
                ))
            })
        }
        Declared::NoJob => Ok(None),
        Declared::Unsaid => Ok(old.and_then(|o| o.job_key.as_deref()).and_then(Job::of_key)),
    }
}

/// The refusal of an order that looks like it can't be taken back but
/// declares no job.
fn no_job() -> Refused {
    Refused::guard(
        Refusal::NoJob,
        format!(
            "Not sent: this looks like it starts something that can't be taken back, and it says no job, so Desk can't keep a second session from being given it. Send it again with the job it is, as {JOB_EXAMPLE} (the key is the release's, PR's or migration's own id)."
        ),
    )
}

/// How an old holder was told to stand down.
enum StoodDown {
    /// A line message to it, which its answer replies to.
    Asked(Uuid),
    /// The person was asked to tell it: they drive it, or it is on a
    /// machine.
    Person,
    /// It couldn't be told.
    Unsent,
}

/// `input`, a crew call for a machine, without the fields of its job
/// (kept at the home, never relayed; its urgency goes with it), with
/// `guard` said after what it says.
pub(super) fn relayed_input(input: &Value, guard: Option<&str>) -> Value {
    let mut sent = input.clone();
    if let Some(o) = sent.as_object_mut() {
        // Its urgency goes with it: the machine steers it in.
        for f in ["job", "irreversible", "replaces"] {
            o.remove(f);
        }
        if let Some(guard) = guard {
            for f in ["message", "prompt"] {
                if let Some(t) = o.get(f).and_then(Value::as_str) {
                    let t = format!("{}\n\n{guard}", t.trim_end());
                    o.insert(f.into(), json!(t));
                }
            }
        }
    }
    sent
}

/// What a list of assignments' lines read besides themselves, read once:
/// their sessions' accounts, and the old holders the held ones wait on.
struct Looked {
    accounts: Vec<workforce_desk::account::Account>,
    olds: Vec<Assignment>,
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

/// How `action` is sent, as its assignment keeps it: the tool and what it
/// says (a held one is sent from it later), so that what a restart left
/// being sent is settled by how it went ([`workforce_desk::DeskStore::assignments_reconcile`]).
fn sent_as(action: &Action) -> Value {
    kept_form(action).unwrap_or_else(|| {
        let tool = match action {
            // A reply is a line message too.
            Action::Nudge { .. } => "crew.nudge",
            Action::StartSession { .. } => "crew.start_session",
            Action::AssignTicket { .. } => "crew.assign_ticket",
            Action::Resume { .. } => "crew.resume",
            Action::Restart { .. } => "crew.restart",
            _ => "crew.act",
        };
        json!({ "tool": tool })
    })
}

/// What a `replaces` can't go with: none, when it may.
fn replaces_refused(order: &Order, starts: bool) -> Option<String> {
    order.replaces.as_ref()?;
    if order.job == Declared::NoJob {
        return Some("Not sent: replaces moves a job, and job: \"none\" says this isn't one. Leave job out to move the old one's, or say the job it is.".into());
    }
    starts.then(|| "Not sent: replaces moves a job to a session that is there already. Start the session first, then move the job to it with replaces.".into())
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

    /// What the holder of `a` last wrote since it got it, and when.
    fn written_since(&self, a: &Assignment) -> Option<(DateTime<Utc>, String)> {
        let since = a.delivered_at.unwrap_or(a.sent_at);
        self.desk
            .picture_recent(&a.boss, a.session, 40)
            .into_iter()
            .filter(|r| r.what == workforce_desk::picture::What::SessionReported && r.at > since)
            .max_by_key(|r| r.at)
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
        match rule {
            Rule::SameSession => format!(
                "Not sent: {name} already has this job ({id}, delivered {}). Saying it again can start it twice. See where it stands (crew.inside; crew.ask_session only if that can't tell); to change what it does, send with replaces: \"{id}\", and it is told to check first.",
                h.delivered_at.map_or_else(|| "already".into(), at)
            ),
            Rule::Held => format!(
                "Not sent: {name} already waits to be given this job ({id}): it goes to it once its old holder answers its stand-down. To change what it is given, close {id} (crew.assignment) and send again."
            ),
            Rule::SameJob if self.holder_gone(h) => format!(
                "Not sent: {name} held this job ({id}, {}), and its session has gone. Send again with replaces: \"{id}\" to move it at once; check first that it hadn't started it.",
                said.join(", ")
            ),
            Rule::SameJob => format!(
                "Not sent: {name} holds this job ({id}, {}). {undone} To move it, send again with replaces: \"{id}\": Desk tells {name} to stand down first. Check that it hasn't already started.",
                said.join(", ")
            ),
        }
    }
    /// Notes `kind` in Activity: the boss `by` did it, to `a`'s session.
    fn note_assignment(&self, kind: Kind, by: &Party, a: &Assignment, text: String) {
        let subject = self.desk.view(a.session).ok().map(|v| Party::session(&v));
        self.desk.note(Event::new(kind, by.clone(), subject, text));
    }

    /// Whether `text`, an order to `to` that declares no job, names a job
    /// one of `boss`'s other sessions holds: then the boss is asked to say
    /// which job it is. A backstop only: it never claims, and never stops
    /// an order when the jobs can't be read.
    async fn looks_held(
        &self,
        boss: &str,
        to: Option<(Uuid, Option<&str>)>,
        text: &str,
    ) -> Option<String> {
        let live = self.desk.store().assignments_live_of(boss).await.ok()?;
        let (h, job) = live.iter().find_map(|a| {
            let theirs = to.is_some_and(|(s, m)| a.session == s && a.machine.as_deref() == m);
            let job = a.job_key.as_deref().and_then(Job::of_key)?;
            (a.claims && !theirs && assignment::looks_like(text, &job)).then_some((a, job))
        })?;
        Some(format!(
            "Not sent: this looks like {}, which {} holds ({}). Send it again with job to say which job this is, or job: \"none\" if it isn't one.",
            job.said(),
            self.holder_name(h),
            short(h.id)
        ))
    }

    /// Reads and reserves `action`, a boss's order, before it is sent: the
    /// job it declares checked against the boss's live assignments under
    /// its lock (R1, R2), taken over with `replaces`, and held when its old
    /// holder may have started it. An order that looks irreversible
    /// declares its job, or isn't sent. `Err` says why it isn't sent.
    pub(super) async fn assign_before(
        &self,
        action: &Action,
        by: &By,
        order: &Order,
    ) -> Result<Before, Refused> {
        if by.party.kind != PartyKind::Manager || order.unrecorded {
            return Ok(Before::Not);
        }
        let Some((to, text, read_as)) = assigned(action) else {
            return Ok(Before::Not);
        };
        if let Some(id) = order.released {
            return Ok(Before::Send(Reservation {
                id,
                guard: None,
                starts: false,
            }));
        }
        let boss = by.party.id.as_str();
        let s = match to {
            Holder::Session(s) => Some(s),
            Holder::New => None,
        };
        if let Some(why) = replaces_refused(order, s.is_none()) {
            return Err(Refused::guard(Refusal::JobUnclear, why));
        }
        let repo = match s {
            Some(s) => self.place_of(s).await.0,
            None => None,
        };
        let internal = |e: eyre::Report| {
            tracing::warn!(error = %format!("{e:#}"), "a boss's assignment couldn't be kept");
            Refused::new(
                Refusal::Unrecorded,
                "Not sent: Desk couldn't record who holds this job just now, so it sent nothing. Try again in a moment.",
            )
        };
        let old = match order.replaces.as_deref() {
            None => None,
            Some(named) => Some(
                self.desk
                    .store()
                    .assignment_named(boss, named)
                    .await
                    .map_err(internal)?
                    .ok_or_else(|| {
                        Refused::guard(Refusal::JobUnclear, format!("Not sent: {named} isn't one of your open assignments; crew.assignments lists them."))
                    })?,
            ),
        };
        let job = job_of(&order.job, repo.as_deref(), old.as_ref())?;
        let r = assignment::read(read_as.as_deref().unwrap_or(&text), order.irreversible);
        let Some(job) = job else {
            if r.irreversible {
                return Err(no_job());
            }
            if order.job == Declared::Unsaid {
                if let Some(why) = self.looks_held(boss, s.map(|s| (s, None)), &text).await {
                    return Err(Refused::guard(Refusal::JobHeld, why));
                }
            }
            return Ok(Before::Not);
        };
        let session = s.unwrap_or_else(Uuid::new_v4);
        let name = match s {
            Some(s) => self.session_name(s),
            None => "the session it starts".into(),
        };
        let mut a = Assignment::new(boss, session, &text);
        a.job = Some(job.said());
        a.job_key = Some(job.as_key());
        a.repo = repo;
        a.targets = r.targets.iter().map(Target::key).collect();
        a.irreversible = r.irreversible;
        a.urgent = order.urgent;
        a.claims = true;
        a.turn = self.current_turn();
        a.action = Some(sent_as(action));
        let mut guard = None;
        let mut moving = None;
        let mut why = String::new();
        if let Some(old) = &old {
            a.replaces = Some(old.id);
            guard = Some(CHECK_FIRST.to_string());
            if Some(old.session) == s && old.machine.is_none() {
                why = format!("replaced by {}", short(a.id));
            } else if self.holder_gone(old) {
                why = format!("moved to {name}; its holder had gone");
            } else {
                why = format!("moved to {name}");
                moving = Some(old.clone());
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
                tracing::info!(%session, %boss, holder = %holder.id, ?rule, "a boss's order for a job another assignment holds wasn't sent");
                self.note_assignment(Kind::AssignmentRefused, &by.party, &a, why.clone());
                return Err(Refused::guard(Refusal::JobHeld, why));
            }
            Reserved::Kept { superseded, .. } => superseded,
        };
        // An irreversible order not yet in, taken over by this one: it
        // never goes in, or the session would be told twice.
        for was in superseded.iter().filter(|w| w.irreversible) {
            let Some(line) = was.line_message else {
                continue;
            };
            if !matches!(self.desk.line_withdraw(line).await, Ok(Some(_))) {
                // It went in meanwhile: it stands, and this doesn't go.
                if let Err(e) = self
                    .desk
                    .store()
                    .assignment_unsupersede(boss, a.id, was.id)
                    .await
                {
                    tracing::warn!(error = %format!("{e:#}"), "an order that went in meanwhile wasn't kept open again");
                }
                let mut held = was.clone();
                held.delivered_at = held.delivered_at.or(Some(Utc::now()));
                return Err(Refused::new(
                    Refusal::JobHeld,
                    self.refusal(Rule::SameSession, &held, &a),
                ));
            }
        }
        let Some(old) = moving else {
            return Ok(Before::Send(Reservation {
                id: a.id,
                guard,
                starts: s.is_none(),
            }));
        };
        let held = self
            .move_from(by, &old, &a, &name, kept_form(action))
            .await
            .map_err(|why| Refused::new(Refusal::JobHeld, why))?;
        if let Some(said) = held {
            self.note_assignment(Kind::AssignmentHeld, &by.party, &a, said.clone());
            return Ok(Before::Held(said));
        }
        Ok(Before::Send(Reservation {
            id: a.id,
            guard,
            starts: s.is_none(),
        }))
    }

    /// The job `new` (reserved, being sent) takes over from `old`, another
    /// session's: `old`'s order not yet in is withdrawn, and its holder is
    /// told to stand down when it got it. `new` is held while `old`'s
    /// holder may have started it, or when the stand-down couldn't be sent
    /// (then it is the person's): what the boss is told, when held. `kept`
    /// is `new`'s order, as kept to send later.
    async fn move_from(
        &self,
        by: &By,
        old: &Assignment,
        new: &Assignment,
        name: &str,
        kept: Option<Value>,
    ) -> Result<Option<String>, String> {
        let went_in = match (old.line_message, old.delivered_at) {
            (_, Some(_)) => true,
            (Some(line), None) => !matches!(self.desk.line_withdraw(line).await, Ok(Some(_))),
            (None, None) => false,
        };
        let working = old.machine.is_none()
            && self
                .desk
                .view(old.session)
                .is_ok_and(|v| v.state == SessionState::Working);
        if !went_in && !working {
            tracing::info!(old = %old.id, "a job moved on before its order went in: its order was withdrawn");
            return Ok(None);
        }
        let may_have = (old.irreversible && went_in) || working;
        let told = self.stand_down(by, old, new, name).await;
        let unsent = matches!(told, StoodDown::Unsent);
        if !may_have && !unsent {
            return Ok(None);
        }
        let store = self.desk.store();
        let basis = self
            .deciding_now()
            .map_or(self.desk.picture_seq(), |d| d.basis) as i64;
        let kept_held = match &kept {
            Some(k) => store.assignment_hold(new.id, Some(k), Some(basis)).await,
            None => Ok(false),
        };
        if !matches!(kept_held, Ok(true)) {
            let _ = store
                .assignment_close(
                    new.id,
                    AssignmentState::Failed,
                    "desk",
                    "it couldn't be held for its old holder's stand-down",
                )
                .await;
            return Err(format!(
                "Not sent: {} may have started {} already, and Desk couldn't hold your order until it stands down. See where it stands (crew.inside; crew.ask_session only if that can't tell), then send again.",
                self.holder_name(old),
                old.about()
            ));
        }
        let old_name = self.holder_name(old);
        let said = match told {
            StoodDown::Asked(line) => {
                if let Err(e) = store.assignment_wait_on(new.id, line).await {
                    tracing::warn!(error = %format!("{e:#}"), "a held assignment's stand-down wasn't kept with it");
                }
                format!(
                    "held {} for {name}: {old_name} may have started {} already, so Desk told it to stand down first. Your order goes to {name} once {old_name} answers (or its session goes), checked against what has changed by then; if it hasn't answered within {HOLD_FOR} minutes, it's the person's.",
                    short(new.id),
                    old.about()
                )
            }
            StoodDown::Person => format!(
                "held {} for {name}: {old_name} may have started {} already, and the person is driving it, so Desk asked them to tell it to stand down. Your order goes to {name} once they have dealt with it, or let it go (crew.assignment, state release, once you know it hasn't started).",
                short(new.id),
                old.about()
            ),
            StoodDown::Unsent => {
                let what = format!(
                    "Desk couldn't tell {old_name} to stand down from {} ({}), so {} for {name} is held. Check it hasn't started it, then let {} go (or close it).",
                    short(old.id),
                    old.about(),
                    short(new.id),
                    short(new.id)
                );
                self.for_the_person(&old.boss, Some(old.session), &what, format!("stand-down:{}", new.id))
                    .await;
                format!(
                    "held {} for {name}: Desk couldn't tell {old_name} to stand down, so it's the person's to check it hasn't started {} first. It goes once they let it go.",
                    short(new.id),
                    old.about()
                )
            }
        };
        tracing::info!(old = %old.id, new = %new.id, "a moved job is held until its old holder stands down");
        Ok(Some(said))
    }

    /// Opens an item for the person (`key` keeps it once), and says it in
    /// the boss's DM.
    async fn for_the_person(&self, boss: &str, session: Option<Uuid>, what: &str, key: String) {
        let mut item = NewItem::ask(boss, OpenKind::Todo, session, what);
        item.dedupe_key = key;
        if let Err(e) = self.desk.store().item_open(&item).await {
            tracing::warn!(error = %format!("{e:#}"), "an assignment's item for the person wasn't kept");
        }
        self.tell_dm(boss, what).await;
    }

    /// Tells `old`'s holder to stand down from it: `new` is moved to
    /// `name`. The one place a stand-down is sent: a line message that asks
    /// for an answer, steered into the old holder's running turn (§4), and,
    /// for a job that can't be undone, its turn interrupted when it isn't
    /// read in time. While the person drives the
    /// old holder (or took it back), or it is on a machine, it goes to the
    /// person instead, never into the session.
    async fn stand_down(
        &self,
        by: &By,
        old: &Assignment,
        new: &Assignment,
        name: &str,
    ) -> StoodDown {
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
                "{} should stand down from {} ({}): it is moved to {name}, and {} waits for it. Desk hasn't told it{}; tell it, or check it hasn't started it, then close this.",
                self.holder_name(old),
                short(old.id),
                old.about(),
                short(new.id),
                if person { " as you are driving it" } else { "" }
            );
            self.for_the_person(
                &old.boss,
                Some(old.session),
                &what,
                format!("stand-down:{}", new.id),
            )
            .await;
            return StoodDown::Person;
        }
        // Urgent (§4), never counted against the urgent limit; one of a
        // job that can't be undone interrupts a turn that doesn't read it
        // in time.
        let urgency = match old.irreversible {
            true => workforce_desk::line::Urgency::Escalate,
            false => workforce_desk::line::Urgency::StandDown,
        };
        let sent = self
            .desk
            .line_send_with(
                workforce_desk::LineSender::Overseer(by.clone()),
                workforce_desk::line::Send::Ask {
                    to: workforce_desk::line::Target::Session(old.session),
                    text: text.clone(),
                    wait: std::time::Duration::ZERO,
                    chose: None,
                    parts: Default::default(),
                },
                Some(urgency),
            )
            .await;
        match sent {
            Ok((m, _)) => {
                tracing::info!(session = %old.session, old = %old.id, "a session was told to stand down from a job moved on");
                self.picture_instruction(&old.boss, old.session, &text, self.current_turn());
                StoodDown::Asked(m.id)
            }
            Err(e) => {
                tracing::warn!(session = %old.session, error = ?e, "a stand-down couldn't be sent");
                StoodDown::Unsent
            }
        }
    }

    /// The reserved order `r` was sent (`line`, its line message; `started`,
    /// the session it started) or wasn't. Kept again once if it couldn't
    /// be; otherwise it stays being sent, and the crew minute's look
    /// settles it from the line.
    pub(super) async fn assign_after(
        &self,
        r: &Reservation,
        by: &By,
        line: Option<Uuid>,
        started: Option<Uuid>,
        ok: bool,
    ) {
        let store = self.desk.store();
        if ok && r.starts {
            let kept = match started {
                Some(s) => store.assignment_set_session(r.id, s).await,
                None => Err(eyre::eyre!("no session was said to start")),
            };
            if let Err(e) = kept {
                // No holder known: it holds nothing, rather than a job no
                // session can be asked about.
                tracing::warn!(assignment = %r.id, error = %format!("{e:#}"), "the session a start's assignment went to wasn't kept");
                let _ = store
                    .assignment_close(
                        r.id,
                        AssignmentState::Cancelled,
                        "desk",
                        "the session it started isn't known",
                    )
                    .await;
                return;
            }
        }
        let mut kept = store.assignment_sent(r.id, line, ok).await;
        if kept.is_err() {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            kept = store.assignment_sent(r.id, line, ok).await;
        }
        match kept {
            Ok(Some(a)) if ok && a.state != AssignmentState::Open => self.went_unheld(&a).await,
            Ok(Some(a)) if ok => {
                // A ticket, resume, restart or start reached it as it went.
                if line.is_none() {
                    let _ = store.assignment_delivered_by_id(a.id, Utc::now()).await;
                }
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
                tracing::warn!(assignment = %r.id, error = %format!("{e:#}"), "an assignment's sending wasn't kept: the next look settles it")
            }
        }
    }

    /// Reserves `input`, the home's own boss `boss`'s call for `machine`'s
    /// session `id`, before it is relayed: the home is where a local boss's
    /// assignments are kept, so its sessions here and on machines are
    /// compared. None when it holds no job.
    pub(super) async fn assign_relayed(
        &self,
        boss: &str,
        machine: &str,
        id: Uuid,
        tool: &str,
        input: &Value,
    ) -> Result<Option<Relayed>, String> {
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
        if text.starts_with(STATUS_QUESTION) {
            return Ok(None);
        }
        let order = Order::of(input);
        if let Some(why) = replaces_refused(&order, false) {
            return Err(why);
        }
        let internal = |e: eyre::Report| {
            tracing::warn!(error = %format!("{e:#}"), "a relayed assignment couldn't be kept");
            "Not sent: Desk couldn't record who holds this job just now, so it sent nothing. Try again in a moment.".to_string()
        };
        let old = match order.replaces.as_deref() {
            None => None,
            Some(named) => Some(
                self.desk
                    .store()
                    .assignment_named(boss, named)
                    .await
                    .map_err(internal)?
                    .ok_or_else(|| {
                        format!("Not sent: {named} isn't one of your open assignments; crew.assignments lists them.")
                    })?,
            ),
        };
        // A machine's session's repository isn't known here: the job says it.
        let job = job_of(&order.job, None, old.as_ref())?;
        let r = assignment::read(text, order.irreversible);
        let Some(job) = job else {
            if r.irreversible {
                return Err(no_job().into());
            }
            if order.job == Declared::Unsaid {
                if let Some(why) = self.looks_held(boss, Some((id, Some(machine))), text).await {
                    return Err(why);
                }
            }
            return Ok(None);
        };
        let mut a = Assignment::new(boss, id, text);
        a.machine = Some(machine.to_string());
        a.job = Some(job.said());
        a.job_key = Some(job.as_key());
        a.targets = r.targets.iter().map(Target::key).collect();
        a.irreversible = r.irreversible;
        a.urgent = order.urgent;
        a.claims = true;
        a.turn = self.current_turn();
        // Relayed, so it carries no line message here: never settled as
        // unsent for that ([`workforce_desk::DeskStore::assignments_reconcile`]).
        a.action =
            Some(json!({ "tool": tool, "input": relayed_input(input, None), "machine": machine }));
        if let Some(o) = &old {
            a.replaces = Some(o.id);
        }
        let name = self.holder_name(&a);
        let moving = old.as_ref().filter(|o| {
            let gone = o.machine.is_none() && self.holder_gone(o);
            let same = o.session == a.session && o.machine == a.machine;
            !gone && !same
        });
        let why = match (&old, moving) {
            (Some(_), Some(_)) => format!("moved to {name}"),
            (Some(o), None) if o.session == a.session => format!("replaced by {}", short(a.id)),
            _ => format!("moved to {name}; its holder had gone"),
        };
        match self
            .desk
            .store()
            .assignment_reserve(&a, old.as_ref().map(|o| (o.id, why.as_str())))
            .await
            .map_err(internal)?
        {
            Reserved::Kept { .. } => {}
            Reserved::Refused { rule, holder } => return Err(self.refusal(rule, &holder, &a)),
        }
        let mut held = None;
        if let Some(o) = moving {
            if let Some((service, _)) = self.overseer_named(boss) {
                let by = self.by_boss(&service, &why);
                let kept = json!({ "tool": tool, "input": relayed_input(input, None), "machine": machine });
                held = self.move_from(&by, o, &a, &name, Some(kept)).await?;
            }
        }
        Ok(Some(Relayed {
            id: a.id,
            guard: old.is_some().then_some(CHECK_FIRST),
            held,
        }))
    }

    /// A relayed assignment was done on its machine (and so reached its
    /// session), or wasn't.
    pub(super) async fn assign_relayed_done(&self, id: Uuid, ok: bool) {
        let store = self.desk.store();
        match store.assignment_sent(id, None, ok).await {
            Ok(Some(a)) if ok && a.state != AssignmentState::Open => {
                return self.went_unheld(&a).await
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(assignment = %id, error = %format!("{e:#}"), "a relayed assignment's outcome wasn't kept")
            }
        }
        if ok {
            let _ = store.assignment_delivered_by_id(id, Utc::now()).await;
        }
    }

    /// `a` went to its session after it was settled as unsent, and another
    /// order holds its job now: two sessions may have it. Its boss and the
    /// person are told, in its DM, with an item for the person.
    async fn went_unheld(&self, a: &Assignment) {
        let what = format!(
            "{} ({}) reached {} after Desk had settled it as unsent, and another order holds that job now, so two sessions may have it. Check which one should, and stand the other down.",
            short(a.id),
            a.about(),
            self.holder_name(a)
        );
        tracing::warn!(assignment = %a.id, "an order went after it was settled, and another holds its job");
        self.note_assignment(
            Kind::AssignmentRefused,
            &Party::manager(&a.boss, &a.boss),
            a,
            what.clone(),
        );
        self.for_the_person(&a.boss, Some(a.session), &what, format!("unheld:{}", a.id))
            .await;
    }

    /// The irreversible jobs `boss` keeps, for a call relayed to a machine
    /// to carry ([`HELD_JOBS`]): the machine compares a request there with
    /// them (R5).
    /// `Err` when they couldn't be read: then nothing is relayed that the
    /// machine would allow on an empty list.
    pub(super) async fn held_irreversible(&self, boss: &str) -> Result<Vec<Assignment>, String> {
        self.desk
            .store()
            .assignments_live_of(boss)
            .await
            .map(|l| l.into_iter().filter(|a| a.irreversible).collect())
            .map_err(|e| {
                tracing::warn!(error = %format!("{e:#}"), "a boss's irreversible jobs couldn't be read to relay");
                "Desk couldn't read which jobs your sessions hold just now".to_string()
            })
    }

    /// Why `boss` may not allow the request `pending` of `session` itself,
    /// if it may not: it looks irreversible, and another session holds an
    /// irreversible job in the same repository (R5), or which repository
    /// either is in isn't known, or the jobs couldn't be read. Then it is
    /// the person's. Every boss's jobs here are compared, not only
    /// `boss`'s: two bosses never release one repository at once.
    /// `elsewhere`: the home's, for a boss relayed from it; None when they
    /// should have come with it and didn't, which is the person's too.
    pub(super) async fn request_conflict(
        &self,
        boss: &str,
        session: Uuid,
        pending: &workforce_desk::model::PendingApproval,
        elsewhere: Option<&[Assignment]>,
    ) -> Option<String> {
        let asked = format!("{} {}", pending.tool, pending.detail);
        if !assignment::irreversible(&asked) {
            return None;
        }
        let Some(elsewhere) = elsewhere else {
            tracing::info!(%session, %boss, "a relayed request came without its home's jobs: it is the person's");
            return Some(
                "its home didn't say which jobs its sessions hold, and this looks like it can't be taken back"
                    .into(),
            );
        };
        let live = match self.desk.store().assignments_live().await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "which jobs a boss's sessions hold couldn't be read for a request");
                return Some(
                    "Desk couldn't read which jobs your sessions hold just now, and this looks like it can't be taken back"
                        .into(),
                );
            }
        };
        let repo = self
            .place_of(session)
            .await
            .0
            .and_then(|r| Job::new("release", &r, "0"))
            .map(|j| j.target);
        let theirs = live.iter().chain(elsewhere.iter()).find(|a| {
            a.irreversible
                && a.state.live()
                && a.session != session
                && match (a.job_key.as_deref().and_then(Job::of_key), &repo) {
                    (Some(j), Some(r)) => j.target == *r,
                    // Where one of them acts isn't known: the person's.
                    _ => true,
                }
        })?;
        tracing::info!(%session, %boss, holder = %theirs.id, "a request beside a job another session holds goes to the person");
        Some(format!(
            "{} holds {} ({}), which can't be undone, so allowing this here could run it twice",
            self.holder_name(theirs),
            theirs.about(),
            short(theirs.id)
        ))
    }

    /// Sends the held assignment `id` in the background: see
    /// [`Self::release_held`].
    fn spawn_release(self: &Arc<Self>, id: Uuid, answer: Option<String>) {
        let crew = Arc::clone(self);
        tokio::spawn(async move { Box::pin(crew.release_held(id, answer)).await });
    }

    /// Sends the held assignment `id` now: its old holder answered its
    /// stand-down (`answer`), the person or its boss let it go, or the old
    /// holder is gone. Through `perform`, as any send, checked against the
    /// picture as it was when it was held; if the picture moved, the boss
    /// decides again. One relayed to a machine is relayed now.
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
        let guard = match &answer {
            Some(said) => format!(
                "{CHECK_FIRST} Desk was told: “{}”",
                crew::one_line(said, 300)
            ),
            None => format!("{CHECK_FIRST} Its last holder's session has gone."),
        };
        let Some((service, o)) = self.overseer_named(&a.boss) else {
            return fail("you no longer oversee its session".into()).await;
        };
        if let Some(held) = self.desk.agent_held_why(&a.boss) {
            return fail(format!("you are {held}")).await;
        }
        if let Some(machine) = a.machine.clone() {
            let Some(kept) = a.action.clone() else {
                return fail("Desk kept no order to send".into()).await;
            };
            let tool = kept["tool"].as_str().unwrap_or("crew.nudge").to_string();
            let input = relayed_input(&kept["input"], Some(&guard));
            let item = format!("session:{machine}/{}", a.session);
            let Some(g) = self.desk.group_of_member(&item) else {
                return fail("its session is no longer in your group".into()).await;
            };
            let ceiling = self.ceiling_now(&a.boss, &o);
            let ok = self
                .relay(&machine, &g, &tool, input, &a.boss, &ceiling)
                .await
                .is_ok_and(|r| r.status == super::served::ServedStatus::Ok);
            if !ok {
                return fail(format!("{machine} didn't take it")).await;
            }
            self.assign_relayed_done(id, true).await;
            tracing::info!(assignment = %id, %machine, "a held assignment was relayed");
            return;
        }
        let Some(action) = a
            .action
            .as_ref()
            .and_then(|v| Action::from_suggestion(v).ok())
        else {
            return fail("Desk kept no order to send".into()).await;
        };
        let action = guarded(&action, &guard).unwrap_or(action);
        if let super::actions::Gate::Run = self.gate(&action, &o) {
        } else {
            return fail("you may no longer send it yourself".into()).await;
        }
        let by = self.by_boss(&service, "Its old holder stood down.");
        let d = Deciding {
            turn: a.turn.clone().unwrap_or_else(|| format!("held:{id}")),
            basis: a.basis.map_or(self.desk.picture_seq(), |b| b.max(0) as u64),
        };
        let order = Order {
            released: Some(id),
            urgent: a.urgent,
            ..Default::default()
        };
        match self.perform_in_with(Some(d), &action, by, &order).await {
            Ok(said) => tracing::info!(assignment = %id, %said, "a held assignment went out"),
            Err(why) => {
                // `perform` marked it failed, on each way it can refuse
                // (left alone, a gate, a moved picture): the boss decides
                // again.
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
    /// Wakes `boss` with `line` about `session`'s assignment.
    fn wake_for(self: &Arc<Self>, boss: &str, session: Uuid, line: String) {
        self.wake_about(boss, session, format!("assignment:{session}"), line);
    }

    /// Wakes `boss` with `line` about `session`, raised as `about`.
    fn wake_about(self: &Arc<Self>, boss: &str, session: Uuid, about: String, line: String) {
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
            about: Some(about),
            group,
            handed: None,
            routine: false,
        };
        let crew = Arc::clone(self);
        tokio::spawn(async move {
            crew.wake(service, o, vec![t]).await;
        });
    }

    /// `m` came on a line: what it says of the assignments it carries or
    /// answers. Anything missed here (a lagged event) the minute's look
    /// reads from the line.
    async fn assignment_heard(self: &Arc<Self>, m: &LineMessage, told: &mut Unreceived) {
        let store = self.desk.store();
        // Its own message: delivered, or never.
        if m.from.kind == PartyKind::Manager {
            match m.delivery {
                Delivery::Delivered => {
                    if m.late {
                        self.received_late(m, told).await;
                    }
                    let _ = store
                        .assignment_delivered(m.id, m.delivered_at.unwrap_or(m.updated_at))
                        .await;
                }
                Delivery::Withdrawn | Delivery::Refused => {
                    if let Ok(Some(a)) = store.assignment_of_line(m.id).await {
                        self.never_went_in(&a).await;
                    }
                }
                Delivery::Unreceived => {
                    if let Ok(Some(a)) = store.assignment_of_line(m.id).await {
                        self.never_went_in(&a).await;
                    }
                    self.unreceived(m, told);
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
            if a.session == from
                && a.state == AssignmentState::Open
                && a.line_message.is_some_and(|l| answers(m, l))
            {
                let _ = store.assignment_acked(a.id, m.at).await;
            }
            // An answer to a stand-down another assignment waits on, from
            // the session that was told to stand down.
            if a.state != AssignmentState::Held || !a.stand_down.is_some_and(|sd| answers(m, sd)) {
                continue;
            }
            let old = match a.replaces {
                Some(o) => store.assignment(o).await.ok().flatten(),
                None => None,
            };
            if old.is_some_and(|o| o.session == from && o.machine.is_none()) {
                let said = workforce_desk::line::without_marks(&m.text);
                tracing::info!(assignment = %a.id, session = %from, "a stand-down was answered: its job goes to its new holder");
                self.spawn_release(a.id, Some(said));
            }
        }
    }

    /// The boss's `m` was never shown to have gone into its session: the
    /// boss is told, once (`told` keeps who was), as it isn't put in again.
    fn unreceived(self: &Arc<Self>, m: &LineMessage, told: &mut Unreceived) {
        if told.said.insert(m.id, false).is_some() {
            return;
        }
        self.said_of_message(
            m,
            "may never have received",
            "Desk couldn't show it reached the session, so it won't put it in again. Check its \
             last reply (crew.history) before resending, and send it again only if it still \
             matters.",
        );
    }

    /// The boss's `m`, given up on, went in after all: the order it carries,
    /// cancelled then, stands again unless another holds its job now. The
    /// boss told it never arrived is told it did, once.
    async fn received_late(self: &Arc<Self>, m: &LineMessage, told: &mut Unreceived) {
        let restored = match self.desk.store().assignment_restore(m.id).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(message = %m.id, error = %format!("{e:#}"), "an order that arrived after all couldn't be restored");
                None
            }
        };
        let then = match &restored {
            Some(Restored::Back(a)) => {
                self.note_assignment(
                    Kind::Assigned,
                    &Party::manager(&a.boss, &a.boss),
                    a,
                    format!("restored {}: its message went in after all", short(a.id)),
                );
                "it arrived after all, after Desk had said it couldn't show it reached the \
                 session, so its order stands again: don't send it again."
                    .to_string()
            }
            Some(Restored::Taken { holder, .. }) => format!(
                "it arrived after all, after Desk had said it couldn't show it reached the \
                 session, but {} holds its job now, so the order isn't restored: re-assign it if \
                 it should still go to this session.",
                self.holder_name(holder)
            ),
            None => "it arrived after all, after Desk had said it couldn't show it reached the \
                     session, so don't send it again."
                .to_string(),
        };
        if told.said.get(&m.id) == Some(&false) {
            told.said.insert(m.id, true);
            self.said_of_message(m, "received", &then);
        }
    }

    /// The boss's messages given up on since `told` was kept, and those
    /// given up on that went in since: each said once, whatever events
    /// were missed.
    async fn unreceived_look(self: &Arc<Self>, told: &mut Unreceived) {
        let store = self.desk.store();
        match store.line_unreceived_since(told.since).await {
            Ok(gone) => {
                for m in gone.iter().filter(|m| m.from.kind == PartyKind::Manager) {
                    self.unreceived(m, told);
                }
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "couldn't read messages given up on")
            }
        }
        let waiting: Vec<Uuid> = told
            .said
            .iter()
            .filter(|(_, late)| !**late)
            .map(|(id, _)| *id)
            .collect();
        for id in waiting {
            if let Ok(Some(mut m)) = store.line_message(id).await {
                if m.delivery == Delivery::Delivered {
                    m.late = true;
                    self.assignment_heard(&m, told).await;
                }
            }
        }
    }

    /// Wakes the boss that sent `m` with what its session `did` with it,
    /// and `then`: one word per message, a later one taking an earlier's
    /// place while it waits.
    fn said_of_message(self: &Arc<Self>, m: &LineMessage, did: &str, then: &str) {
        let Some(session) = m.to_session() else {
            return;
        };
        let at = m.at.with_timezone(&self.desk.timezone()).format("%H:%M");
        self.wake_about(
            &m.from.id,
            session,
            format!("unreceived:{}", m.id),
            format!(
                "{} {did} your message of {at} (“{}”, message_id {}): {then}",
                self.session_name(session),
                crew::words(&m.text, 20),
                m.id,
            ),
        );
    }

    /// `a`'s line message was withdrawn, refused, or never shown to have
    /// gone in: it holds nothing.
    async fn never_went_in(&self, a: &Assignment) {
        let why = NEVER_WENT_IN;
        if let Ok(Some(a)) = self
            .desk
            .store()
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

    /// When `a` was last heard of: sent, delivered, answered, or its
    /// session's last word since.
    fn last_heard(&self, a: &Assignment) -> DateTime<Utc> {
        [
            Some(a.sent_at),
            a.delivered_at,
            a.acked_at,
            self.written_since(a).map(|(t, _)| t),
        ]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(a.sent_at)
    }

    /// Looks at every live assignment: what was left being sent is settled
    /// from the line, and one whose message never went in is cancelled;
    /// held ones go out once their old holder answered (read from the
    /// line), the person dealt with it, or the holder has gone, and those
    /// waiting past [`HOLD_FOR`] are the person's; reversible ones nothing
    /// was heard of past their lapse lapse; done ones close when the boss
    /// lets accounts close them.
    async fn assignments_look(self: &Arc<Self>, raised: &mut HashSet<Uuid>) {
        let store = self.desk.store();
        let now = Utc::now();
        let minute = chrono::TimeDelta::from_std(self.timing.minute).unwrap_or_default();
        if let Err(e) = store.assignments_reconcile(now - minute * 2).await {
            tracing::warn!(error = %format!("{e:#}"), "assignments left being sent couldn't be settled");
        }
        if let Ok(gone) = store.assignments_never_went_in().await {
            for a in &gone {
                self.never_went_in(a).await;
            }
        }
        let Ok(live) = store.assignments_live().await else {
            return;
        };
        let lapse = minute * (assignment::LAPSE.num_minutes() as i32);
        raised.retain(|id| {
            live.iter()
                .any(|a| a.id == *id && a.state == AssignmentState::Held)
        });
        for a in &live {
            match a.state {
                AssignmentState::Held => self.look_held(a, raised, now, minute).await,
                AssignmentState::Open if !a.irreversible && now - self.last_heard(a) >= lapse => {
                    let _ = store
                        .assignment_close(
                            a.id,
                            AssignmentState::Lapsed,
                            "desk",
                            "nothing was heard of it for 12 hours",
                        )
                        .await;
                }
                AssignmentState::Open if a.machine.is_none() => {
                    let closes = self
                        .overseer_named(&a.boss)
                        .is_some_and(|(_, o)| o.updates.close_from_accounts);
                    if !closes {
                        continue;
                    }
                    if let Some(done) = self.looks_done(a, None).await {
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
                _ => {}
            }
        }
    }

    /// The held assignment `a`, looked at.
    async fn look_held(
        self: &Arc<Self>,
        a: &Assignment,
        raised: &mut HashSet<Uuid>,
        now: DateTime<Utc>,
        minute: chrono::TimeDelta,
    ) {
        let store = self.desk.store();
        // Let go by the person or its boss.
        if let Some(said) = &a.answer {
            self.spawn_release(a.id, Some(said.clone()));
            return;
        }
        // Released for a gone holder only when it is known to be gone:
        // never because its row couldn't be read just now.
        let old = match a.replaces {
            Some(o) => match store.assignment(o).await {
                Ok(old) => old,
                Err(e) => {
                    tracing::warn!(assignment = %a.id, error = %format!("{e:#}"), "a held assignment's old holder couldn't be read");
                    return;
                }
            },
            None => None,
        };
        let Some(old) = old.filter(|o| !self.holder_gone(o)) else {
            self.spawn_release(a.id, None);
            return;
        };
        // Its stand-down's answer, from the line.
        if let (Some(sd), None) = (a.stand_down, &old.machine) {
            if let Ok(Some(said)) = store.stand_down_answer(sd, old.session, a.sent_at).await {
                let said = workforce_desk::line::without_marks(&said);
                self.spawn_release(a.id, Some(said));
                return;
            }
        }
        // The person was asked to tell its old holder, or to check it, and
        // closed that: it goes.
        for key in [format!("stand-down:{}", a.id), format!("held:{}", a.id)] {
            if let Ok(Some((state, by))) = store.item_state_of_key(&a.boss, &key).await {
                if state == "closed" && by.as_deref() == Some("person") {
                    self.spawn_release(a.id, Some("The person checked its old holder.".into()));
                    return;
                }
            }
        }
        if now - a.sent_at < minute * HOLD_FOR as i32 || !raised.insert(a.id) {
            return;
        }
        let line = format!(
            "{} hasn't answered its stand-down; {} is held for {} ({}).",
            self.holder_name(&old),
            a.about(),
            self.holder_name(a),
            short(a.id)
        );
        self.for_the_person(
            &a.boss,
            Some(a.session),
            &format!(
                "{line} Check {} hasn't started it, then close this: Desk sends {} once it's closed.",
                self.holder_name(&old),
                short(a.id)
            ),
            format!("held:{}", a.id),
        )
        .await;
        tracing::info!(assignment = %a.id, "a held assignment's stand-down went unanswered: it is the person's");
        self.wake_for(&a.boss, a.session, line);
    }

    /// What `a`'s session's account (`account`, else read) says it did of
    /// `a`'s own job since it got it, if it did it: its release or deploy
    /// deployed, its PR merged. Never another job's.
    async fn looks_done(
        &self,
        a: &Assignment,
        account: Option<&workforce_desk::account::Account>,
    ) -> Option<String> {
        let job = a.job_key.as_deref().and_then(Job::of_key)?;
        let done_when = match job.kind.as_str() {
            "merge" => "merged",
            "release" | "deploy" | "promote" | "rollback" | "dispatch" => "deployed",
            _ => return None,
        };
        let read;
        let account = match account {
            Some(a) => a,
            None => {
                read = self
                    .desk
                    .session_accounts_of(&[a.session])
                    .await
                    .ok()?
                    .into_iter()
                    .next()?;
                &read
            }
        };
        let names_it = |d: &workforce_desk::account::Done| {
            d.refs
                .iter()
                .any(|r| Job::new(&job.kind, &job.target, &r.id).is_some_and(|j| j.key == job.key))
                || {
                    let words: Vec<String> = d
                        .text
                        .split(|c: char| !c.is_alphanumeric() && c != '.')
                        .map(|w| w.trim_matches('.').to_lowercase())
                        .collect();
                    words.iter().enumerate().any(|(i, w)| {
                        *w == job.key
                            && (job.key.len() >= 3
                                || i.checked_sub(1)
                                    .is_some_and(|p| words[p] == job.kind || words[p] == "pr"))
                    })
                }
        };
        account
            .done
            .iter()
            .filter(|d| names_it(d))
            .find_map(|d| {
                workforce_desk::open_item::kept_by(done_when, std::slice::from_ref(d), a.sent_at)
            })
            .map(|d| crew::one_line(&d.text, 120))
    }

    /// After a restart: what was still being sent two minutes before
    /// `now` is settled ([`workforce_desk::DeskStore::assignments_reconcile`]),
    /// and closed assignments a month old are let go.
    pub(super) async fn assignments_after_restart(&self, now: DateTime<Utc>) {
        let store = self.desk.store();
        match store
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
        // Kept before jobs were declared: their words made their job, and
        // no longer do.
        if let Ok(n) = store.assignments_close_undeclared().await {
            if n > 0 {
                tracing::info!(
                    closed = n,
                    "assignments kept before jobs were declared were closed"
                );
            }
        }
        let _ = store
            .assignments_prune(now - chrono::TimeDelta::days(30))
            .await;
    }

    /// What a boss's turn reads of its assignments: the open and held ones,
    /// newest first, at most 20, read with one query for their sessions'
    /// accounts and one for the holders the held ones wait on.
    pub(super) async fn assignments_block(&self, boss: &str) -> Option<String> {
        let store = self.desk.store();
        let live = store.assignments_of(boss, true).await.ok()?;
        let live: Vec<Assignment> = live.into_iter().take(20).collect();
        if live.is_empty() {
            return None;
        }
        let looked = self.looked_up(&live).await;
        let mut lines = Vec::new();
        for a in &live {
            lines.push(format!("- {}", self.assignment_line(a, &looked).await));
        }
        Some(format!(
            "Your open assignments, newest first (one session holds a job at a time; move one with replaces):\n{}",
            lines.join("\n")
        ))
    }

    /// What `list`'s lines read besides themselves, read at once.
    async fn looked_up(&self, list: &[Assignment]) -> Looked {
        let mut sessions: Vec<Uuid> = list
            .iter()
            .filter(|a| a.state == AssignmentState::Open && a.machine.is_none())
            .map(|a| a.session)
            .collect();
        sessions.sort();
        sessions.dedup();
        let accounts = match sessions.is_empty() {
            true => Vec::new(),
            false => self
                .desk
                .session_accounts_of(&sessions)
                .await
                .unwrap_or_default(),
        };
        let mut olds = Vec::new();
        for o in list
            .iter()
            .filter(|a| a.state == AssignmentState::Held)
            .filter_map(|a| a.replaces)
        {
            if let Ok(Some(old)) = self.desk.store().assignment(o).await {
                olds.push(old);
            }
        }
        Looked { accounts, olds }
    }

    /// One assignment in a line: `a:id → session · job · times · state`.
    async fn assignment_line(&self, a: &Assignment, looked: &Looked) -> String {
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
                let from = a
                    .replaces
                    .and_then(|o| looked.olds.iter().find(|x| x.id == o));
                format!(
                    "held: waits for {} to stand down (let it go with crew.assignment, state release, once you know it hasn't started)",
                    from.map_or_else(|| "its old holder".into(), |o| self.holder_name(o))
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
            let account = looked.accounts.iter().find(|x| x.session == a.session);
            if let Some(account) = account {
                if let Some(done) = self.looks_done(a, Some(account)).await {
                    state.push_str(&format!(
                        " · looks done (“{done}”): close it with crew.assignment"
                    ));
                }
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
        let looked = self.looked_up(&list).await;
        let mut rows = Vec::new();
        for a in &list {
            let mut row = self.assignment_line(a, &looked).await;
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

    /// `crew.assignment`: the boss closes one of its own, or lets a held
    /// one go once it knows its old holder didn't start it, through the
    /// same gate as a message to its session.
    pub(super) async fn assignment_tool(
        &self,
        b: &super::tools::Bound,
        input: &Value,
    ) -> Result<Value, String> {
        let boss = b.service.agent_id.as_str();
        let state = match input["state"].as_str().map(str::trim) {
            Some("done") => Some(AssignmentState::Done),
            Some("cancelled" | "canceled") => Some(AssignmentState::Cancelled),
            Some("release") => None,
            _ => return Err("Say state: done, cancelled, or release (a held one).".into()),
        };
        let note = input["note"]
            .as_str()
            .or(input["reason"].as_str())
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .ok_or("Say why in note: the person reads it.")?;
        let named = input["id"].as_str().unwrap_or_default();
        let a = self
            .desk
            .store()
            .assignment_named(boss, named)
            .await
            .map_err(|_| "Your assignments couldn't be read just now.".to_string())?
            .ok_or_else(|| {
                format!("{named} isn't one of your open assignments; crew.assignments lists them.")
            })?;
        let id = a.id;
        let as_message = Action::Nudge {
            to: To::Session(a.session),
            message: note.to_string(),
            reply_to: None,
        };
        if a.machine.is_none() {
            match self.gate(&as_message, &b.oversight) {
                super::actions::Gate::Run => {}
                super::actions::Gate::Suggest(why) | super::actions::Gate::No(why) => {
                    return Err(format!("Not done: {why}. Tell the person instead."))
                }
                super::actions::Gate::Refuse => {
                    return Err("You only watch the crew: tell the person instead.".into())
                }
            }
        }
        let by = self.by_boss(&b.service, note);
        let Some(state) = state else {
            if a.state != AssignmentState::Held {
                return Err(format!("{} isn't held: it went already.", short(id)));
            }
            let let_go = self
                .desk
                .store()
                .assignment_let_go(id, note)
                .await
                .map_err(|_| "It couldn't be let go just now.".to_string())?;
            if !let_go {
                return Err(format!("{} isn't held any more.", short(id)));
            }
            self.note_assignment(
                Kind::AssignmentClosed,
                &by.party,
                &a,
                format!("let {} go: {note}", short(id)),
            );
            return Ok(
                json!({ "done": format!("let {} go: Desk sends it within a minute, checked as any order", short(id)) }),
            );
        };
        let closed = self
            .desk
            .store()
            .assignment_close(id, state, "boss", note)
            .await
            .map_err(|_| "It couldn't be closed just now.".to_string())?
            .ok_or("That assignment is closed already.")?;
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

/// The boss's messages given up on since the loop started, each with
/// whether its boss was told it went in after all: what is said of them,
/// once.
pub(super) struct Unreceived {
    since: DateTime<Utc>,
    said: HashMap<Uuid, bool>,
}

/// Keeps the assignments up to date with what happens on the lines, and
/// looks at them every crew minute. First settles what a restart left
/// being sent. Events missed (lagged) are read from the line at once.
pub(super) fn assignments_loop(
    crew: Arc<CrewHub>,
    cancel: CancellationToken,
) -> impl std::future::Future<Output = ()> {
    let mut events = crew.desk.events();
    async move {
        crew.assignments_after_restart(Utc::now()).await;
        let mut every = tokio::time::interval(crew.timing.minute);
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut raised: HashSet<Uuid> = HashSet::new();
        let mut told = Unreceived {
            since: Utc::now(),
            said: HashMap::new(),
        };
        let mut pruned = Utc::now();
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                received = events.recv() => match received {
                    Ok(DeskEvent::Line { message, removed: false }) => {
                        crew.assignment_heard(&message, &mut told).await;
                    }
                    Err(RecvError::Lagged(_)) => {
                        crew.assignments_look(&mut raised).await;
                        crew.unreceived_look(&mut told).await;
                    }
                    Ok(_) => {}
                    Err(RecvError::Closed) => return,
                },
                _ = every.tick() => {
                    crew.assignments_look(&mut raised).await;
                    crew.unreceived_look(&mut told).await;
                    // Once a day, closed rows a month old are let go.
                    if Utc::now() - pruned >= chrono::TimeDelta::days(1) {
                        pruned = Utc::now();
                        let _ = crew
                            .desk
                            .store()
                            .assignments_prune(pruned - chrono::TimeDelta::days(30))
                            .await;
                    }
                }
            }
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
    fn a_relayed_order_keeps_its_urgency_and_leaves_its_job_at_home() {
        let sent = relayed_input(
            &json!({ "session_id": "s", "message": "Stop.", "urgent": true,
                "job": { "kind": "release", "key": "4" }, "irreversible": true,
                "replaces": "a:3f2a1b2c" }),
            None,
        );
        assert_eq!(sent["urgent"], json!(true));
        assert!(sent.get("job").is_none() && sent.get("irreversible").is_none());
        assert!(sent.get("replaces").is_none());
        assert!(Order::of(&sent).urgent, "the machine steers it in");
    }

    #[test]
    fn an_order_reads_its_declared_job_and_none_is_needed() {
        let o = Order::of(&json!({
            "job": { "kind": "release", "target": "mantis-crm/monorepo", "key": " release-4 " },
            "irreversible": true,
            "replaces": "a:3f2a1b2c"
        }));
        assert_eq!(
            o.job,
            Declared::Job {
                kind: "release".into(),
                target: Some("mantis-crm/monorepo".into()),
                key: "release-4".into()
            }
        );
        assert!(o.irreversible && !o.urgent);
        assert_eq!(o.replaces.as_deref(), Some("a:3f2a1b2c"));
        let none = Order::of(&json!({ "message": "Carry on." }));
        assert!(none.job == Declared::Unsaid && none.replaces.is_none() && !none.irreversible);
        assert_eq!(Order::of(&json!({ "job": "None" })).job, Declared::NoJob);
        assert!(matches!(
            Order::of(&json!({ "job": "release 4" })).job,
            Declared::Malformed(why) if why.contains("\"kind\": \"release\"")
        ));
        assert!(matches!(
            Order::of(&json!({ "job": { "kind": "release" } })).job,
            Declared::Malformed(_)
        ));
    }

    #[test]
    fn a_job_is_its_target_its_kind_and_its_key_or_the_old_ones() {
        let said = Declared::Job {
            kind: "release".into(),
            target: None,
            key: "R4".into(),
        };
        let job = job_of(&said, Some("Mantis-CRM/monorepo"), None)
            .unwrap()
            .unwrap();
        assert_eq!(job.as_key(), "mantis-crm/monorepo/release/4");
        let unknown = job_of(&said, None, None).unwrap_err();
        assert_eq!(unknown.kind, Refusal::JobUnclear);
        assert!(unknown.why.contains("which repository"), "{unknown}");
        let malformed = job_of(
            &Declared::Malformed("Not sent: a job says…".into()),
            None,
            None,
        );
        let malformed = malformed.unwrap_err();
        assert_eq!(malformed.kind, Refusal::JobUnclear);
        // A guard's judgement of the order alone: what Act fully lifts.
        assert!(malformed.guard);
        assert_eq!(no_job().kind, Refusal::NoJob);
        assert!(no_job().guard);
        // What Desk failed to keep never is.
        assert!(!Refused::new(Refusal::Unrecorded, "Not sent: …").guard);
        assert_eq!(job_of(&Declared::NoJob, None, None), Ok(None));
        assert_eq!(job_of(&Declared::Unsaid, None, None), Ok(None));
        let mut old = Assignment::new("desk", Uuid::new_v4(), "Start release 4.");
        old.job_key = Some(job.as_key());
        assert_eq!(
            job_of(&Declared::Unsaid, None, Some(&old)).unwrap(),
            Some(job)
        );
    }

    #[test]
    fn replaces_goes_with_a_job_and_a_session_that_is_there() {
        let moving = Order {
            replaces: Some("a:1e569371".into()),
            ..Default::default()
        };
        assert_eq!(replaces_refused(&moving, false), None);
        let to_start = replaces_refused(&moving, true).unwrap();
        assert!(to_start.contains("Start the session first"), "{to_start}");
        let none = Order {
            job: Declared::NoJob,
            ..moving
        };
        let why = replaces_refused(&none, false).unwrap();
        assert!(why.contains("job: \"none\""), "{why}");
        assert_eq!(replaces_refused(&Order::default(), true), None);
    }
}
