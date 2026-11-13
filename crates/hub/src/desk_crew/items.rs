//! What the boss owes the person (its open items, `boss_open_items`): each
//! question it asked them and each decision it wants confirmed, kept until
//! it is answered or no longer applies, across restarts and DM rollovers.
//!
//! Every way one opens goes through the one insert keyed by its
//! `dedupe_key` ([`workforce_desk::open_item`]): `crew.ask_person`, a
//! session's question given to the person ([`CrewHub::post_question`]), a
//! request the boss's work turn left open, a DM reply that ends in a
//! question (unless that turn asked with `crew.ask_person`), and the
//! watcher's mark that it told the boss of something (an item never said
//! to the person, in place of `hub.crew.raised`).
//!
//! It closes by the person (its button, `POST /api/desk/items/{id}/answer`
//! and `/drop`, in that request, or their own words through
//! `crew.close_item`), by the session (its question answered, gone ahead
//! or withdrawn, its request answered in its terminal, the session ended
//! or gone, and with `close_from_accounts` its account no longer waiting),
//! or by the boss. Unanswered, it is said again ("Still waiting on you")
//! once it is 30 minutes old, then every `wake.remind_minutes`, at most 3
//! at once, and parked after 3 restatements. A nightly sweep at the
//! person's 03:00 closes what no longer applies among what hasn't changed
//! for a day. Needs-you posts go out at once: never through the check-in
//! budget or the watcher's held lines.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_common::channels::desk::{DeliveryKind, DeskPlace, DeskTarget};
use workforce_common::channels::DESK_CHANNEL;
use workforce_common::oversight::Oversight;
use workforce_desk::crew::By;
use workforce_desk::crew::{self, Extras, ItemRefused, MentionedApproval};
use workforce_desk::line::{Asked, LineKind};
use workforce_desk::model::{DeskEvent, SessionState};
use workforce_desk::open_item::{
    self, ClosedBy, NewItem, OpenItem, OpenKind, OpenState, FIRST_RESTATE, LAPSE_AFTER,
    RESTATED_AT_ONCE, SWEEP_AFTER,
};

use super::actions::{Action, Handout, Target, STATUS_QUESTION};
use super::decisions::Done;
use workforce_desk::open_item_store::Opened;
use workforce_desk::Speaker;

use super::tools::Bound;
use super::CrewHub;

pub const ASK: &str = "crew.ask_person";
pub const CLOSE: &str = "crew.close_item";
pub const OPEN: &str = "crew.open_items";
pub const PROMISE: &str = "crew.promise";
pub const ORDER: &str = "crew.order";

pub const ASK_DESCRIPTION: &str = "Ask the person something, or ask them to confirm a decision you took (kind: confirm): it goes to them at once as a needs-you message with its answers as buttons, and stays open until they answer, it no longer applies, or you close it (crew.close_item). Say it in full so it reads cold (text), what it holds up (holding_up), your recommendation, the answers to offer (options) and, when an answer would settle a kind of case from then on, that case (generalizes, e.g. \"When a session wants to merge with CI red\"). Asking the same thing again updates it: nothing is said twice. For a question a session asked you, use crew.escalate instead.";
pub const CLOSE_DESCRIPTION: &str = "Close one of your open items (item_id from crew.open_items or the open items Desk gives you): with answer when the person answered it in their own words (for a session's question, their answer goes on to the session), or with why when it no longer applies or you decided it yourself.";
pub const PROMISE_DESCRIPTION: &str = "Keep a promise you made the person about a session (session_id): what you promised (text, in full) and when it is kept (done_when: \"merged\", \"in production\", \"PR open\", or your own words). Desk tracks it until the session's account shows it done, then reports it once; it lapses if the session ends or nothing changes for 3 days. crew.ask_session keeps its own promise to report back.";
pub const ORDER_DESCRIPTION: &str = "Keep one of the person's orders you are carrying out (text, in their words) with the sessions working on it (session_ids), and done_when if it has a clear end (\"merged\", \"in production\"). An order you act on from their message (starting, telling or handing work to a session) is kept for you; use this for one that started nothing yet (\"leave the export alone\"). It is in every turn of yours until it is done or closed with crew.close_item.";
pub const OPEN_DESCRIPTION: &str = "What you owe the person now (\"what's open?\"): the questions you asked them, the decisions you want confirmed, your promises, their orders, and the parked ones (no longer restated; they can answer or drop them), each with its item_id, when it was opened and last said, and what it holds up.";

/// A promise to report back what a session says to a status question.
const REPLIED: &str = "replied";

/// The state key the nightly sweep keeps its last day under.
const SWEPT: &str = "hub.crew.items_swept";
/// The hour, in the person's time, the sweep runs at.
const SWEEP_HOUR: u32 = 3;
/// How long the watcher's mark of a check-in line is kept (as
/// `hub.crew.raised` kept it).
pub(super) const MARK_FOR: chrono::TimeDelta = chrono::TimeDelta::days(1);
/// The most of the open items a turn is given, in characters.
const CONTEXT_CHARS: usize = 2_400;

pub fn schema(name: &str) -> Value {
    match name {
        ASK => json!({ "type": "object", "properties": {
            "text": { "type": "string", "description": "What you ask, in full, readable cold" },
            "kind": { "type": "string", "enum": ["ask", "confirm"], "description": "ask (default), or confirm a decision you took" },
            "session_id": { "type": "string", "description": "The session it is about, if one (from crew.overview)" },
            "options": { "type": "array", "items": { "type": "string" }, "description": "The answers to offer, as buttons" },
            "recommendation": { "type": "string" },
            "holding_up": { "type": "string", "description": "What waits on the answer, e.g. \"Billing migration is paused on this\"" },
            "generalizes": { "type": "string", "description": "The kind of case an answer would settle from then on" }
        }, "required": ["text"] }),
        PROMISE => json!({ "type": "object", "properties": {
            "session_id": { "type": "string" },
            "text": { "type": "string", "description": "What you promised, in full" },
            "done_when": { "type": "string", "description": "merged, in production, PR open, or your own words" }
        }, "required": ["session_id", "text", "done_when"] }),
        ORDER => json!({ "type": "object", "properties": {
            "text": { "type": "string", "description": "The person's order, in their words" },
            "session_ids": { "type": "array", "items": { "type": "string" } },
            "done_when": { "type": "string" }
        }, "required": ["text"] }),
        CLOSE => json!({ "type": "object", "properties": {
            "item_id": { "type": "string" },
            "answer": { "type": "string", "description": "The person's answer, in their words" },
            "why": { "type": "string", "description": "Why it is settled without their answer" }
        }, "required": ["item_id"] }),
        _ => json!({ "type": "object", "properties": {} }),
    }
}

/// An item's text as a needs-you post.
fn needs_you(i: &OpenItem) -> String {
    let mut s = format!("**Needs you.** {}", i.text);
    if let Some(h) = i.holding_up.as_deref().filter(|h| !h.trim().is_empty()) {
        s.push_str(&format!("\n\nHolding up: {}", h.trim()));
    }
    if let Some(r) = i.recommendation.as_deref().filter(|r| !r.trim().is_empty()) {
        s.push_str(&format!("\n\nI'd say: {}", r.trim()));
    }
    s
}

/// The session a watcher key (`{kind}:{session}:…`) is about.
fn session_of_key(key: &str) -> Option<Uuid> {
    key.split(':').nth(1)?.parse().ok()
}

/// The request an approval key (`approval:{session}:{approval}`) names.
fn approval_of_key(key: &str) -> Option<(Uuid, Uuid)> {
    let mut parts = key.splitn(3, ':');
    (parts.next()? == "approval").then_some(())?;
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// What a session question's state says of the item on it, once it no
/// longer waits on the person.
fn line_closed(asked: Option<Asked>, session: &str) -> Option<(ClosedBy, String)> {
    match asked? {
        Asked::Open | Asked::Escalated => None,
        Asked::Answered => Some((ClosedBy::Session, "It was answered.".into())),
        Asked::Lapsed => Some((
            ClosedBy::Session,
            format!("{session} went ahead without your answer."),
        )),
        Asked::Withdrawn => Some((ClosedBy::Session, "The session withdrew it.".into())),
        Asked::Dismissed => Some((ClosedBy::Person, "You set it aside unanswered.".into())),
    }
}

/// What a promise or order the boss kept says, once, at `tz`: when it was
/// asked, what it was and what showed it done. None for anything else.
pub(super) fn kept_line(closed: &OpenItem, tz: chrono_tz::Tz) -> Option<String> {
    let why = closed.closed_why.as_deref()?;
    let why = why.strip_prefix("Done: ").unwrap_or(why);
    if closed.closed_by != Some(ClosedBy::Boss) || closed.done_when.as_deref() == Some(REPLIED) {
        return None;
    }
    let at = closed.opened_at.with_timezone(&tz).format("%H:%M");
    let what = match closed.kind {
        OpenKind::Order => format!("The change you asked for at {at}"),
        OpenKind::Promise => format!("What I promised at {at}"),
        _ => return None,
    };
    Some(format!(
        "{what} (“{}”) is {}: {why}",
        crew::one_line(&closed.text, 200),
        closed.done_when.as_deref().unwrap_or("done"),
    ))
}

/// What is said of `due` (one boss's items due to be said again, oldest
/// first) at `now`: at most [`RESTATED_AT_ONCE`], the last counting the
/// rest; each with whether this restatement parks it.
fn restatements<'a>(due: &[&'a OpenItem], now: DateTime<Utc>) -> Vec<(&'a OpenItem, String, bool)> {
    let more = due.len().saturating_sub(RESTATED_AT_ONCE);
    let shown = &due[..due.len().min(RESTATED_AT_ONCE)];
    let last = shown.len().saturating_sub(1);
    shown
        .iter()
        .enumerate()
        .map(|(n, i)| {
            let park = i.parks_after_this();
            let mut text = i.still_waiting(now);
            if park {
                text.push_str(
                    "\n\nI won't bring this up again: it stays in what's open, where you can answer or drop it.",
                );
            }
            if n == last && more > 0 {
                text.push_str(&format!("\n\nAnd {more} more: say *what's open?*"));
            }
            (*i, text, park)
        })
        .collect()
}

impl CrewHub {
    fn item_err(e: eyre::Report) -> String {
        tracing::warn!(error = %format!("{e:#}"), "an open item couldn't be kept");
        "Desk couldn't keep that just now.".to_string()
    }

    /// The item tools, for the boss `b`.
    pub(super) async fn items_tool(
        &self,
        name: &str,
        b: &Bound,
        input: &Value,
    ) -> Result<Value, String> {
        let boss = b.service.agent_id.as_str();
        match name {
            ASK => self.ask_person(b, input).await,
            CLOSE => self.close_item_tool(boss, input).await,
            PROMISE | ORDER => self.keep_tool(name, b, input).await,
            OPEN => {
                let items = self
                    .desk
                    .store()
                    .items_not_closed(Some(boss))
                    .await
                    .map_err(Self::item_err)?;
                let listed = open_item::listed(&items, Utc::now(), self.desk.timezone());
                Ok(json!(match listed.is_empty() {
                    true => "Nothing is open: you owe the person nothing.".to_string(),
                    false => listed,
                }))
            }
            _ => Err("not an item tool".into()),
        }
    }

    async fn ask_person(&self, b: &Bound, input: &Value) -> Result<Value, String> {
        let boss = b.service.agent_id.as_str();
        let text = input["text"]
            .as_str()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or("Say what you ask (text), in full.")?;
        let kind = match input["kind"].as_str() {
            None | Some("ask") => OpenKind::Ask,
            Some("confirm") => OpenKind::Confirm,
            Some(_) => return Err("kind is ask or confirm.".into()),
        };
        let session = match input["session_id"].as_str() {
            Some(s) => {
                let id: Uuid = s
                    .parse()
                    .map_err(|_| "That isn’t a session id from crew.overview.".to_string())?;
                self.session_seen(id, &b.seen)?;
                Some(id)
            }
            None => None,
        };
        let some = |k: &str| {
            input[k]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let mut new = NewItem::ask(boss, kind, session, text).generalizing(some("generalizes"));
        new.holding_up = some("holding_up");
        new.recommendation = some("recommendation");
        new.options = input["options"]
            .as_array()
            .map(|o| {
                o.iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|o| !o.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        // Its turn's reply is not also kept as a question of its own.
        self.asked_person
            .lock()
            .insert(boss.to_string(), crew::now());
        let opened = self
            .desk
            .store()
            .item_open(&new)
            .await
            .map_err(Self::item_err)?;
        let tz = self.desk.timezone();
        match opened {
            Opened::New(i) => {
                self.post_item(boss, &i, needs_you(&i), Vec::new()).await;
                Ok(json!(format!(
                    "Asked: it is with the person now, as item {}.",
                    i.id
                )))
            }
            Opened::Again(i) => Ok(json!(format!(
                "You already asked this at {} (item {}), so nothing new was said; it is updated.",
                i.opened_at.with_timezone(&tz).format("%H:%M"),
                i.id
            ))),
        }
    }

    /// `crew.promise` and `crew.order`.
    async fn keep_tool(&self, name: &str, b: &Bound, input: &Value) -> Result<Value, String> {
        let boss = b.service.agent_id.as_str();
        let some = |k: &str| {
            input[k]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let session = |s: &str| -> Result<Uuid, String> {
            let id: Uuid = s
                .parse()
                .map_err(|_| "That isn’t a session id from crew.overview.".to_string())?;
            self.session_seen(id, &b.seen)?;
            Ok(id)
        };
        let text = some("text").ok_or("Say it in full (text).")?;
        let new = match name {
            PROMISE => {
                let s = session(
                    input["session_id"]
                        .as_str()
                        .ok_or("Say which session it is about (session_id).")?,
                )?;
                let done_when = some("done_when").ok_or("Say when it is kept (done_when).")?;
                NewItem::promise(boss, s, &text, &done_when)
            }
            _ => {
                let mut sessions = Vec::new();
                for s in input["session_ids"].as_array().into_iter().flatten() {
                    sessions.push(session(s.as_str().unwrap_or_default())?);
                }
                let mut o = NewItem::order(boss, &text, sessions, None);
                o.done_when = some("done_when");
                o
            }
        };
        let opened = self
            .desk
            .store()
            .item_open(&new)
            .await
            .map_err(Self::item_err)?;
        Ok(json!(format!(
            "Kept as item {}: it is tracked until it is done, and in every turn of yours till then.",
            opened.item().id
        )))
    }

    /// After `action` was done by `by` (left in `done`): a status question
    /// to a session is a promise to report back its reply, and what acted on
    /// a session is kept for the turn's order.
    pub(crate) async fn after_action(&self, action: &Action, by: &By, done: &Done) {
        let Some(boss) = self.deciding_boss(by) else {
            return;
        };
        let now = crew::now();
        let acted: Vec<Uuid> = match action {
            Action::Nudge {
                to: Target::Session(s),
                message,
                reply_to: None,
            } if message.starts_with(STATUS_QUESTION) => {
                let question = message.trim_start_matches(STATUS_QUESTION).trim();
                let name = self.name_of_session(*s);
                let mut p = NewItem::promise(
                    &boss,
                    *s,
                    &format!(
                        "Report back what {name} says to “{}”",
                        crew::one_line(question, 200)
                    ),
                    REPLIED,
                );
                p.line_message = done.line.as_ref().map(|(id, _)| *id);
                if let Err(e) = self.desk.store().item_open(&p).await {
                    tracing::warn!(error = %format!("{e:#}"), "keeping the promise to report back failed");
                }
                Vec::new()
            }
            // A question isn't work done for the person: only what tells
            // a session to do something is ("Could you add …?" is that).
            Action::Nudge {
                to: Target::Session(s),
                reply_to: None,
                message,
            } if !asks(message) => vec![*s],
            Action::StartSession { .. } => done.started.into_iter().collect(),
            Action::AssignTicket { to, .. } => match to {
                Handout::Session(s) => vec![*s],
                Handout::NewSession { .. } => done.started.into_iter().collect(),
                Handout::Agent(_) => Vec::new(),
            },
            _ => Vec::new(),
        };
        // Only what a turn did is that turn's: outside one, nothing is.
        let turn = workforce_agent::runner::CURRENT_SESSION_ID
            .try_with(Clone::clone)
            .ok();
        let (Some(turn), false) = (turn, acted.is_empty()) else {
            return;
        };
        let mut list = self.turn_acts.lock();
        list.retain(|(_, at, _)| now - *at < chrono::Duration::hours(1));
        list.extend(acted.into_iter().map(|s| (turn.clone(), now, s)));
    }

    /// The person's message `text`, in `boss`'s DM turn that started at
    /// `started` in `sid`: an order, with the sessions the turn acted on,
    /// when it acted on any.
    pub(crate) async fn order_from_turn(
        &self,
        service: &crate::serve::AgentService,
        sid: &str,
        started: DateTime<Utc>,
        text: &str,
    ) {
        let boss = service.agent_id.as_str();
        // The DM's session now: the turn's own, or the one it rolled over
        // to, where its tools ran.
        let session = service.sessions.get(sid).ok().flatten().and_then(|s| {
            if s.is_sealed() {
                service.sessions.find(&s.channel, &s.owner).ok().flatten()
            } else {
                Some(s)
            }
        });
        let rolled = session
            .as_ref()
            .map(|s| s.id.clone())
            .filter(|id| id != sid);
        // This turn's acts, taken: its session's, since it started.
        let mut sessions: Vec<Uuid> = Vec::new();
        self.turn_acts.lock().retain(|(turn, at, s)| {
            let ours = (turn == sid || rolled.as_deref() == Some(turn.as_str())) && *at >= started;
            if ours && !sessions.contains(s) {
                sessions.push(*s);
            }
            !ours
        });
        if sessions.is_empty() || text.trim().is_empty() {
            return;
        }
        // Keyed by the person's message, found again after a rollover.
        let message = session.and_then(|s| {
            s.messages
                .iter()
                .rev()
                .find(|m| m.role == workforce_common::session::MessageRole::User)
                .map(|m| m.id.clone())
        });
        let new = NewItem::order(boss, text.trim(), sessions, message.as_deref());
        if let Err(e) = self.desk.store().item_open(&new).await {
            tracing::warn!(error = %format!("{e:#}"), "keeping the person's order failed");
        }
    }

    /// A session's message on its line to its boss keeps the boss's promise
    /// to report what it says: closed, and its reply said once.
    pub(super) async fn replied(&self, m: &workforce_desk::line::LineMessage) {
        let (Some(session), true) = (m.from_session(), m.to.kind == crew::PartyKind::Manager)
        else {
            return;
        };
        let key = open_item::promise_key(session, REPLIED);
        let items = match self.desk.store().items_not_closed(Some(&m.to.id)).await {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(error = %e, "reading the boss's promises failed");
                return;
            }
        };
        // The answer to the question it was asked, or, as a session's
        // reply to a plain message isn't threaded, its next word that
        // answers nothing else.
        let Some(p) = items.into_iter().find(|i| {
            i.dedupe_key == key
                && i.opened_at <= m.at
                && match m.in_reply_to {
                    Some(asked) => i.line_message == Some(asked),
                    None => true,
                }
        }) else {
            return;
        };
        let reply = crew::one_line(&m.text, 600);
        let why = format!("{} replied.", m.from.name);
        if let Ok(Some(closed)) = self
            .desk
            .store()
            .item_close(p.id, ClosedBy::Boss, &why)
            .await
        {
            let text = format!(
                "{} answered what I asked it at {}: {reply}",
                m.from.name,
                closed
                    .opened_at
                    .with_timezone(&self.desk.timezone())
                    .format("%H:%M")
            );
            self.say(&closed.boss, text).await;
        }
    }

    /// Says `text` in `boss`'s DM, as a notice.
    async fn say(&self, boss: &str, text: String) {
        let Some((service, _)) = self.overseer_named(boss) else {
            return;
        };
        let Some(owner) = self.person(Some(&service)) else {
            return;
        };
        let target =
            DeskTarget::of_turn(&DeskPlace::Dm { thread: None }, DESK_CHANNEL, None, false);
        if let Err(e) = crate::desk_surface::deliver(
            &service,
            &owner,
            &target,
            crate::desk_surface::Delivery::text(DeliveryKind::Notice, text),
        )
        .await
        {
            tracing::info!(error = %e, "a notice in the boss's DM failed");
        }
    }

    /// A promise or order kept by what its sessions' accounts say they did
    /// since it opened, if one: what did it.
    async fn kept(&self, i: &OpenItem) -> Option<String> {
        let done_when = i.done_when.as_deref().filter(|d| *d != REPLIED)?;
        let linked = i.linked();
        if linked.is_empty() {
            return None;
        }
        let accounts = self.desk.session_accounts_of(&linked).await.ok()?;
        accounts
            .iter()
            .find_map(|a| open_item::kept_by(done_when, &a.done, i.opened_at))
            .map(|d| d.text.clone())
    }

    async fn close_item_tool(&self, boss: &str, input: &Value) -> Result<Value, String> {
        let id: Uuid = input["item_id"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .ok_or("Say which item: an item_id from crew.open_items.")?;
        let item = self
            .desk
            .store()
            .open_item(id)
            .await
            .map_err(Self::item_err)?
            .filter(|i| i.boss == boss)
            .ok_or("That isn’t one of your open items (crew.open_items lists them).")?;
        if item.state == OpenState::Closed {
            return Ok(json!("That was already settled."));
        }
        let text = |k: &str| input[k].as_str().map(str::trim).filter(|s| !s.is_empty());
        match (text("answer"), text("why")) {
            // A session's question: the person's answer goes on to it, as
            // theirs, and that closes the item.
            (Some(answer), _) if item.line_message.is_some() => {
                let ask = item.line_message.unwrap_or_default();
                self.desk
                    .line_answer(ask, workforce_desk::LineSender::Person, answer.to_string())
                    .await
                    .map_err(|e| match e {
                        workforce_desk::LineError::Refused(r) => r.to_string(),
                        workforce_desk::LineError::Desk(e) => Self::item_err(eyre::eyre!("{e}")),
                    })?;
                Ok(json!(
                    "Their answer went to the session, and the item is closed."
                ))
            }
            (Some(answer), _) => {
                self.desk
                    .store()
                    .item_close(id, ClosedBy::Person, &format!("They answered: {answer}"))
                    .await
                    .map_err(Self::item_err)?;
                Ok(json!("Closed with their answer."))
            }
            (None, Some(why)) => {
                self.desk
                    .store()
                    .item_close(id, ClosedBy::Boss, why)
                    .await
                    .map_err(Self::item_err)?;
                Ok(json!("Closed."))
            }
            (None, None) => Err("Say the person's answer, or why it is settled without it.".into()),
        }
    }

    /// Posts `text` about item `i` in `boss`'s DM, with its buttons (or the
    /// request's, `approvals`), and keeps its extras: at once, never held
    /// for a check-in. While the person types to the boss it goes once they
    /// pause, at most three minutes on, without holding up the caller.
    /// Whether it went out, or will.
    pub(crate) async fn post_item(
        &self,
        boss: &str,
        i: &OpenItem,
        text: String,
        approvals: Vec<MentionedApproval>,
    ) -> bool {
        if self.typing_release(boss).is_none() {
            return self.post_item_now(boss, i, text, approvals).await;
        }
        if self.overseer_named(boss).is_none() {
            return false;
        }
        let crew = self.me();
        let (boss, i) = (boss.to_string(), i.clone());
        tokio::spawn(async move {
            crew.after_typing(&boss).await;
            crew.post_item_now(&boss, &i, text, approvals).await;
        });
        true
    }

    async fn post_item_now(
        &self,
        boss: &str,
        i: &OpenItem,
        text: String,
        approvals: Vec<MentionedApproval>,
    ) -> bool {
        let Some((service, _)) = self.overseer_named(boss) else {
            return false;
        };
        let Some(owner) = self.person(Some(&service)) else {
            return false;
        };
        let target =
            DeskTarget::of_turn(&DeskPlace::Dm { thread: None }, DESK_CHANNEL, None, false);
        let delivered = match crate::desk_surface::deliver(
            &service,
            &owner,
            &target,
            crate::desk_surface::Delivery::text(DeliveryKind::Question, text),
        )
        .await
        {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, "putting what the boss asks of the person in its DM failed");
                return false;
            }
        };
        let extras = Extras {
            item: Some(i.mention()),
            approvals,
            ..Default::default()
        };
        if let Err(e) = self
            .desk
            .store()
            .put_extras(&delivered.message_id, boss, &extras)
            .await
        {
            tracing::warn!(error = %e, "keeping an open item's buttons failed");
        }
        true
    }

    /// A DM reply of `boss`'s, in a turn that started at `started`, that
    /// ends on a question to the person is an ask of its own, unless that
    /// turn asked with `crew.ask_person`. Its reply is how it was said.
    pub(crate) async fn ask_from_reply(&self, boss: &str, started: DateTime<Utc>, reply: &str) {
        if self
            .asked_person
            .lock()
            .get(boss)
            .is_some_and(|at| *at >= started)
        {
            return;
        }
        let Some(q) = crew::question_in(reply) else {
            return;
        };
        if self.overseer_named(boss).is_none() {
            return;
        }
        let new = NewItem::ask(boss, OpenKind::Ask, None, &q);
        if let Err(e) = self.desk.store().item_open(&new).await {
            tracing::warn!(error = %format!("{e:#}"), "keeping the boss's question to the person failed");
        }
    }

    /// The request `approval` of `session` the boss's work turn left to the
    /// person: open, and said in its DM with the request's buttons.
    pub(crate) async fn approval_left_open(&self, session: Uuid, approval: Uuid, summary: &str) {
        let Some(boss) = self
            .overseer_of_session(session)
            .map(|(s, _)| s.agent_id.as_str().to_string())
        else {
            return;
        };
        let name = self
            .desk
            .view(session)
            .map(|v| v.label().to_string())
            .unwrap_or_else(|_| "A session".into());
        let key = open_item::approval_key(&session.to_string(), &approval.to_string());
        let mut new = NewItem::ask(
            &boss,
            OpenKind::Ask,
            Some(session),
            &format!("{name} waits on your answer to a request: {summary}"),
        );
        new.dedupe_key = key;
        match self.desk.store().item_open(&new).await {
            Ok(Opened::New(i)) => {
                let approvals = vec![MentionedApproval {
                    session_id: session.to_string(),
                    approval_id: approval.to_string(),
                    summary: summary.to_string(),
                }];
                self.post_item(&boss, &i, needs_you(&i), approvals).await;
            }
            Ok(Opened::Again(_)) => {}
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "keeping a request left to the person failed");
            }
        }
    }

    /// The person answers item `id` from its buttons: closed here, in this
    /// request; a session's question gets their answer, and anything else
    /// goes to its boss as their message.
    pub(crate) async fn answer_item(&self, id: Uuid, answer: &str) -> Result<String, ItemRefused> {
        let store = self.desk.store();
        let item = store
            .open_item(id)
            .await
            .map_err(|e| ItemRefused::Cant(Self::item_err(e)))?
            .ok_or(ItemRefused::NotFound)?;
        if item.state == OpenState::Closed {
            return Err(ItemRefused::Closed);
        }
        if let Some(ask) = item.line_message {
            self.desk
                .line_answer(ask, workforce_desk::LineSender::Person, answer.to_string())
                .await
                .map_err(|e| match e {
                    workforce_desk::LineError::Refused(r) => ItemRefused::Cant(r.to_string()),
                    workforce_desk::LineError::Desk(e) => {
                        ItemRefused::Cant(Self::item_err(eyre::eyre!("{e}")))
                    }
                })?;
            return Ok("Sent.".into());
        }
        let closed = store
            .item_close(id, ClosedBy::Person, &format!("You answered: {answer}"))
            .await
            .map_err(|e| ItemRefused::Cant(Self::item_err(e)))?
            .ok_or(ItemRefused::Closed)?;
        let Some((service, _)) = self.overseer_named(&closed.boss) else {
            return Ok("Answered.".into());
        };
        let Some(owner) = self.person(Some(&service)) else {
            return Ok("Answered.".into());
        };
        let message = format!("My answer to “{}”: {answer}", closed.text);
        let crew = self.me();
        tokio::spawn(async move {
            if let Err(e) = crew
                .run_dm_turn(&service, &owner, &message, 0, None, Some(Speaker::you()))
                .await
            {
                tracing::warn!(agent_id = %service.agent_id, error = %e, "the person's answer didn't reach the boss");
                crew.answer_lost(&service, &owner, &closed).await;
            }
        });
        Ok("Answered.".into())
    }

    /// The person's answer to `closed` didn't reach its boss: never lost,
    /// the item is open again (when it can be), and the person is told.
    pub(crate) async fn answer_lost(
        &self,
        service: &Arc<crate::serve::AgentService>,
        owner: &str,
        closed: &OpenItem,
    ) {
        let reopened = self
            .desk
            .store()
            .item_reopen(closed.id)
            .await
            .unwrap_or(false);
        let text = match reopened {
            true => format!(
                "Your answer to “{}” didn't reach {} just now, so it is still open: answer it again.",
                crew::one_line(&closed.text, 200),
                super::name_of(service)
            ),
            false => format!(
                "Your answer to “{}” didn't reach {} just now: say it again in this chat.",
                crew::one_line(&closed.text, 200),
                super::name_of(service)
            ),
        };
        let target =
            DeskTarget::of_turn(&DeskPlace::Dm { thread: None }, DESK_CHANNEL, None, false);
        if let Err(e) = crate::desk_surface::deliver(
            service,
            owner,
            &target,
            crate::desk_surface::Delivery::text(DeliveryKind::Notice, text),
        )
        .await
        {
            tracing::warn!(error = %e, "saying an answer didn't reach the boss failed");
        }
    }

    /// The person drops item `id` unanswered: a session's question is set
    /// aside with it.
    pub(crate) async fn drop_item(&self, id: Uuid) -> Result<String, ItemRefused> {
        let store = self.desk.store();
        let item = store
            .open_item(id)
            .await
            .map_err(|e| ItemRefused::Cant(Self::item_err(e)))?
            .ok_or(ItemRefused::NotFound)?;
        if item.state == OpenState::Closed {
            return Err(ItemRefused::Closed);
        }
        if let Some(ask) = item.line_message {
            // Set aside, or already settled: either way the item closes.
            if let Err(e) = self.desk.line_dismiss(ask).await {
                tracing::info!(error = ?e, "a dropped item's question wasn't set aside");
            }
        }
        store
            .item_close(id, ClosedBy::Person, "You dropped it.")
            .await
            .map_err(|e| ItemRefused::Cant(Self::item_err(e)))?;
        Ok("Dropped.".into())
    }

    /// What `boss` owes the person, as its turns are given it.
    pub(crate) async fn items_context(&self, boss: &str) -> Option<String> {
        let items = match self.desk.store().items_not_closed(Some(boss)).await {
            Ok(i) => i,
            Err(e) => {
                tracing::info!(error = %e, "a boss's open items couldn't be read for its turn");
                return None;
            }
        };
        let listed = open_item::listed(&items, Utc::now(), self.desk.timezone());
        if listed.is_empty() {
            return None;
        }
        let listed = match listed.chars().count() > CONTEXT_CHARS {
            true => format!(
                "{}…\n(crew.open_items lists them all)\n",
                listed.chars().take(CONTEXT_CHARS).collect::<String>()
            ),
            false => listed,
        };
        Some(format!(
            "What you owe the person now (your open items: when they answer one in their own \
             words, or it is settled, close it with crew.close_item):\n{listed}"
        ))
    }

    /// The watcher's marks of what it told each boss in check-ins, by key,
    /// since [`MARK_FOR`] ago: `hub.crew.raised`, moved to the items once.
    pub(super) async fn raised_marks(&self) -> HashMap<String, DateTime<Utc>> {
        self.migrate_raised().await;
        let since = Utc::now() - MARK_FOR;
        let mut out = HashMap::new();
        for (s, _) in self.overseers() {
            match self
                .desk
                .store()
                .items_raised(s.agent_id.as_str(), since)
                .await
            {
                Ok(m) => out.extend(m),
                Err(e) => {
                    tracing::warn!(error = %e, "reading what the boss was told of failed");
                }
            }
        }
        out
    }

    /// Keeps that the watcher put `key` (said as `line`) to `boss` at `at`.
    pub(super) async fn mark_raised(&self, boss: &str, key: &str, line: &str, at: DateTime<Utc>) {
        let mark = NewItem::raised(boss, key, session_of_key(key), line);
        if let Err(e) = self.desk.store().item_raise(&mark, at).await {
            tracing::warn!(error = %format!("{e:#}"), "keeping what the boss was told of failed");
        }
    }

    /// Moves what `hub.crew.raised` kept to the items, once: each key that
    /// is still recent becomes its boss's mark, and the old key goes.
    async fn migrate_raised(&self) {
        let store = self.desk.store();
        let kept = match store.state(super::watcher::RAISED).await {
            Ok(Some(e)) => e.value,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(error = %e, "reading the questions the overseer raised failed");
                return;
            }
        };
        let kept: HashMap<String, DateTime<Utc>> = serde_json::from_value(kept).unwrap_or_default();
        let overseers = self.overseers();
        let now = Utc::now();
        for (key, at) in kept.into_iter().filter(|(_, at)| now - *at < MARK_FOR) {
            let boss = session_of_key(&key)
                .and_then(|s| self.overseer_of_session(s))
                .map(|(s, _)| s)
                .or_else(|| (overseers.len() == 1).then(|| overseers[0].0.clone()));
            if let Some(boss) = boss {
                self.mark_raised(boss.agent_id.as_str(), &key, &key, at)
                    .await;
            }
        }
        if let Err(e) = store.delete_state(super::watcher::RAISED).await {
            tracing::warn!(error = %e, "retiring hub.crew.raised failed");
        }
    }

    /// Why item `i` no longer applies, if it doesn't: the watcher's mark
    /// over a day old; for what was said to the person, its session ended or
    /// gone, its question no longer waiting on the person, its request
    /// answered, or (with `close_from_accounts`)
    /// its session's account no longer waiting.
    async fn past(&self, i: &OpenItem, o: Option<&Oversight>) -> Option<(ClosedBy, String)> {
        let now = Utc::now();
        // The watcher's mark is kept a day, as hub.crew.raised kept it,
        // whatever became of what it marks.
        if !i.said() {
            let old =
                i.raised_at.is_none_or(|at| now - at >= MARK_FOR) && now - i.changed_at >= MARK_FOR;
            return old.then(|| (ClosedBy::Boss, "Told of over a day ago.".into()));
        }
        // Parked, it waits for the person to answer or drop it: never
        // closed under them.
        if i.state == OpenState::Parked {
            return None;
        }
        // A promise or order ends when it is kept, or lapses.
        if matches!(i.kind, OpenKind::Promise | OpenKind::Order) {
            if let Some(what) = self.kept(i).await {
                return Some((
                    ClosedBy::Boss,
                    format!("Done: {}", crew::one_line(&what, 200)),
                ));
            }
            let minute = chrono::Duration::from_std(self.timing.minute).unwrap_or_default();
            let after = minute * LAPSE_AFTER.num_minutes() as i32;
            // Gone or closed ended it; a view that failed says nothing.
            let ended = |s: Uuid| match self.desk.view(s) {
                Ok(v) => v.state == SessionState::Closed,
                Err(workforce_desk::DeskError::NotFound) => true,
                Err(_) => false,
            };
            return open_item::lapsed(i, now, after, ended).map(|why| (ClosedBy::Lapsed, why));
        }
        let view = match i.session {
            Some(s) => match self.desk.view(s) {
                Ok(v) if v.state == SessionState::Closed => {
                    return Some((ClosedBy::Session, format!("{} ended.", v.label())));
                }
                Ok(v) => Some(v),
                Err(_) => return Some((ClosedBy::Session, "Its session is gone.".into())),
            },
            None => None,
        };
        if let Some(ask) = i.line_message {
            let name = view.as_ref().map_or("The session", |v| v.label());
            return match self.desk.store().line_message(ask).await {
                Ok(Some(q)) if q.kind == LineKind::Ask => line_closed(q.asked, name),
                Ok(_) => Some((ClosedBy::Session, "Its question is gone.".into())),
                Err(e) => {
                    tracing::info!(error = %e, "an open item's question couldn't be read");
                    None
                }
            };
        }
        if let Some((_, approval)) = approval_of_key(&i.dedupe_key) {
            let open = view
                .as_ref()
                .and_then(|v| v.pending.as_ref())
                .is_some_and(|p| p.id == approval);
            return (!open).then(|| (ClosedBy::Session, "Its request was answered.".into()));
        }
        let accounts = o.is_some_and(|o| o.updates.close_from_accounts);
        if accounts && i.said() && matches!(i.kind, OpenKind::Ask | OpenKind::Confirm) {
            if let Some(s) = i.session {
                let account = self
                    .desk
                    .session_accounts_of(&[s])
                    .await
                    .ok()
                    .and_then(|a| a.into_iter().next());
                // Past only when the session itself chose since it was
                // asked, and waits on nothing now: it went ahead.
                if let Some(chose) = account.as_ref().and_then(|a| {
                    (a.by_model && a.waiting_on.is_none())
                        .then(|| a.decisions.iter().rfind(|d| d.at > i.opened_at))
                        .flatten()
                }) {
                    return Some((
                        ClosedBy::Session,
                        format!(
                            "Its account says it went ahead: {}",
                            crew::one_line(&chose.what, 200)
                        ),
                    ));
                }
            }
        }
        None
    }

    /// Closes what no longer applies among `items`, saying each said one
    /// that was the person's to answer in its boss's DM.
    async fn close_past(&self, items: Vec<OpenItem>) {
        for i in items {
            let o = self.overseer_named(&i.boss).map(|(_, o)| o);
            let Some((by, why)) = self.past(&i, o.as_ref()).await else {
                continue;
            };
            match self.desk.store().item_close(i.id, by, &why).await {
                Ok(Some(closed)) => {
                    tracing::info!(item = %closed.id, by = by.as_str(), %why, "an open item closed");
                    let tz = self.desk.timezone();
                    match (closed.kind, by) {
                        // Kept: reported once, with when it was asked.
                        // With the composer on, it goes in the next update.
                        (OpenKind::Promise | OpenKind::Order, ClosedBy::Boss)
                            if !o.as_ref().is_some_and(|o| o.updates.enabled) =>
                        {
                            if let Some(text) = kept_line(&closed, tz) {
                                self.say(&closed.boss, text).await;
                            }
                        }
                        (OpenKind::Promise | OpenKind::Order, ClosedBy::Boss) => {}
                        (OpenKind::Promise | OpenKind::Order, ClosedBy::Lapsed) => {
                            self.say(
                                &closed.boss,
                                format!(
                                    "I'm no longer tracking “{}”: {why}.",
                                    crew::one_line(&closed.text, 200)
                                ),
                            )
                            .await;
                        }
                        // A session's question shows its own end where it is.
                        _ if closed.said()
                            && closed.line_message.is_none()
                            && by != ClosedBy::Person =>
                        {
                            self.say_closed(&closed, &why).await;
                        }
                        _ => {}
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, "closing an open item failed"),
            }
        }
    }

    /// Says in `i`'s boss's DM that it no longer waits on the person.
    async fn say_closed(&self, i: &OpenItem, why: &str) {
        let Some((service, _)) = self.overseer_named(&i.boss) else {
            return;
        };
        let Some(owner) = self.person(Some(&service)) else {
            return;
        };
        let target =
            DeskTarget::of_turn(&DeskPlace::Dm { thread: None }, DESK_CHANNEL, None, false);
        let text = format!(
            "No longer waiting on you: {} ({})",
            crew::one_line(&i.text, 300),
            why.trim_end_matches('.')
        );
        if let Err(e) = crate::desk_surface::deliver(
            &service,
            &owner,
            &target,
            crate::desk_surface::Delivery::text(DeliveryKind::Notice, text),
        )
        .await
        {
            tracing::info!(error = %e, "saying an open item closed failed");
        }
    }

    /// Says again what has waited on the person past its time, for each
    /// boss: at most [`RESTATED_AT_ONCE`], the rest counted; the third
    /// restatement parks it.
    /// Of `items` (one boss's, under `o`), what is due to be said again at
    /// `now`, as [`restatements`] says it.
    pub(super) fn due_again<'a>(
        &self,
        items: &'a [OpenItem],
        o: &Oversight,
        now: DateTime<Utc>,
    ) -> Vec<(&'a OpenItem, String, bool)> {
        if o.wake.remind_minutes == 0 {
            return Vec::new();
        }
        let minute = chrono::Duration::from_std(self.timing.minute).unwrap_or_default();
        let first = minute * FIRST_RESTATE.num_minutes() as i32;
        let remind = minute * o.wake.remind_minutes as i32;
        let due: Vec<&OpenItem> = items.iter().filter(|i| i.due(now, first, remind)).collect();
        restatements(&due, now)
    }

    async fn resurface(&self, items: &[OpenItem]) {
        let now = Utc::now();
        let minute = chrono::Duration::from_std(self.timing.minute).unwrap_or_default();
        let first = minute * FIRST_RESTATE.num_minutes() as i32;
        let mut by_boss: HashMap<&str, Vec<&OpenItem>> = HashMap::new();
        for i in items {
            let Some((_, o)) = self.overseer_named(&i.boss) else {
                continue;
            };
            // With the composer on, what is due goes in the boss's update.
            if o.wake.remind_minutes == 0 || o.updates.enabled {
                continue;
            }
            let remind = minute * o.wake.remind_minutes as i32;
            if i.due(now, first, remind) {
                by_boss.entry(i.boss.as_str()).or_default().push(i);
            }
        }
        for (boss, due) in by_boss {
            for (i, text, park) in restatements(&due, now) {
                let approvals = approval_of_key(&i.dedupe_key)
                    .map(|(s, a)| {
                        vec![MentionedApproval {
                            session_id: s.to_string(),
                            approval_id: a.to_string(),
                            summary: i.text.clone(),
                        }]
                    })
                    .unwrap_or_default();
                let mut shown = i.clone();
                if park {
                    shown.state = OpenState::Parked;
                }
                if self.post_item(boss, &shown, text, approvals).await {
                    if let Err(e) = self.desk.store().item_said(i.id, now, park).await {
                        tracing::warn!(error = %e, "counting a restated item failed");
                    }
                }
            }
        }
    }

    /// The nightly sweep: of every item open or parked and unchanged for
    /// `after`, closes what no longer applies (the minute look does too; the
    /// sweep is the backstop for what it missed). A parked one is never
    /// closed by it: it waits for the person to answer or drop it.
    pub(crate) async fn sweep_items(&self, after: chrono::Duration) {
        let items = match self.desk.store().items_not_closed(None).await {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(error = %e, "the open items' sweep couldn't read them");
                return;
            }
        };
        let now = Utc::now();
        let stale: Vec<OpenItem> = items
            .into_iter()
            .filter(|i| now - i.changed_at >= after)
            .collect();
        tracing::info!(items = stale.len(), "sweeping the open items");
        self.close_past(stale).await;
    }

    /// Whether the nightly sweep is due now (past the person's 03:00, and
    /// not yet run today), marking it run when it is.
    async fn sweep_due(&self) -> bool {
        let tz = self.desk.timezone();
        let local = Utc::now().with_timezone(&tz);
        if chrono::Timelike::hour(&local) < SWEEP_HOUR {
            return false;
        }
        let today = local.date_naive().to_string();
        let store = self.desk.store();
        let last = store
            .state(SWEPT)
            .await
            .ok()
            .flatten()
            .and_then(|e| e.value.as_str().map(str::to_string));
        if last.as_deref() == Some(today.as_str()) {
            return false;
        }
        if let Err(e) = store.put_state(SWEPT, &json!(today)).await {
            tracing::warn!(error = %e, "keeping the open items' sweep day failed");
            return false;
        }
        true
    }

    /// One look over every open item: what is past closes, and what is
    /// due is said again.
    pub(crate) async fn look_at_items(&self) {
        let items = match self.desk.store().items_not_closed(None).await {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(error = %e, "the open items couldn't be read");
                return;
            }
        };
        self.close_past(items).await;
        let items = match self.desk.store().items_not_closed(None).await {
            Ok(i) => i,
            Err(_) => return,
        };
        self.resurface(&items).await;
    }
}

/// Keeps the open items: each session question that stops waiting on the
/// person closes its item as it happens; every minute what is past closes
/// and what is due is said again; and the nightly sweep runs.
pub(super) async fn items_loop(crew: Arc<CrewHub>, cancel: CancellationToken) {
    let (_, mut events) = crew.desk.subscribe();
    let mut every = tokio::time::interval(crew.timing.minute);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let sweep_after = chrono::Duration::from_std(crew.timing.minute)
        .map(|m| m * SWEEP_AFTER.num_minutes() as i32)
        .unwrap_or(SWEEP_AFTER);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            received = events.recv() => match received {
                Ok(DeskEvent::Line { message, removed: false })
                    if message.kind != LineKind::Ask && message.from_session().is_some() =>
                {
                    crew.replied(&message).await;
                }
                Ok(DeskEvent::Line { message, .. }) if message.kind == LineKind::Ask => {
                    let name = message.from.name.clone();
                    if let Some((by, why)) = line_closed(message.asked, &name) {
                        if let Err(e) = crew.desk.store().item_close_of_line(message.id, by, &why).await {
                            tracing::warn!(error = %e, "closing a question's open item failed");
                        }
                    }
                }
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return,
            },
            _ = every.tick() => {
                crew.look_at_items().await;
                if crew.sweep_due().await {
                    crew.sweep_items(sweep_after).await;
                }
            }
        }
    }
}

/// Whether `message` to a session only asks it something: it opens with a
/// question word and asks.
fn asks(message: &str) -> bool {
    let first = message
        .split(|c: char| !c.is_alphanumeric())
        .find(|w| !w.is_empty())
        .unwrap_or_default()
        .to_lowercase();
    message.contains('?')
        && matches!(
            first.as_str(),
            "where"
                | "what"
                | "when"
                | "which"
                | "who"
                | "why"
                | "how"
                | "is"
                | "are"
                | "was"
                | "were"
                | "did"
                | "does"
                | "has"
                | "have"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_name_their_session_and_request() {
        let (s, a) = (Uuid::new_v4(), Uuid::new_v4());
        assert_eq!(
            approval_of_key(&open_item::approval_key(&s.to_string(), &a.to_string())),
            Some((s, a))
        );
        assert_eq!(session_of_key(&format!("asked:{s}:Which port?")), Some(s));
        assert_eq!(approval_of_key(&format!("asked:{s}:x")), None);
        assert_eq!(session_of_key("ask:none:merge"), None);
    }

    #[test]
    fn at_most_three_are_said_again_at_once_and_the_third_restatement_parks() {
        let now = Utc::now();
        let items: Vec<OpenItem> = (0..5)
            .map(|n| {
                let mut new = NewItem::ask("desk", OpenKind::Ask, None, &format!("Q{n}?"));
                new.holding_up = None;
                OpenItem {
                    id: Uuid::new_v4(),
                    boss: new.boss,
                    owner: "you".into(),
                    kind: new.kind,
                    session: None,
                    line_message: None,
                    text: new.text,
                    holding_up: None,
                    recommendation: None,
                    options: vec![],
                    generalizes: None,
                    done_when: None,
                    dedupe_key: new.dedupe_key,
                    opened_at: now - chrono::Duration::hours(3),
                    last_said_at: None,
                    // The first said twice already: this is its third.
                    said_count: if n == 0 { 3 } else { 1 },
                    raised_at: None,
                    sessions: vec![],
                    changed_at: now,
                    state: OpenState::Open,
                    closed_at: None,
                    closed_by: None,
                    closed_why: None,
                }
            })
            .collect();
        let due: Vec<&OpenItem> = items.iter().collect();
        let said = restatements(&due, now);
        assert_eq!(said.len(), 3);
        assert!(said[0].2, "the third restatement parks it");
        assert!(said[0].1.contains("I won't bring this up again"));
        assert!(!said[1].2 && !said[2].2);
        assert!(
            said[2].1.ends_with("And 2 more: say *what's open?*"),
            "{}",
            said[2].1
        );
        assert!(!said[1].1.contains("more"));
        assert!(said[1].1.starts_with("Still waiting on you: Q1? · 3 h"));
        assert_eq!(restatements(&due[..2], now).len(), 2);
    }

    #[test]
    fn a_question_to_a_session_is_not_work_done_for_the_person() {
        assert!(asks("Where is it? Reply when you can."));
        assert!(asks("Which key did you use?"));
        assert!(!asks("Could you add the picker endpoint?"));
        assert!(!asks("Add the picker endpoint."));
    }

    #[test]
    fn a_question_closes_its_item_once_it_no_longer_waits_on_the_person() {
        assert_eq!(line_closed(Some(Asked::Escalated), "web"), None);
        assert_eq!(line_closed(Some(Asked::Open), "web"), None);
        assert_eq!(
            line_closed(Some(Asked::Lapsed), "web").map(|c| c.0),
            Some(ClosedBy::Session)
        );
        assert_eq!(
            line_closed(Some(Asked::Dismissed), "web").map(|c| c.0),
            Some(ClosedBy::Person)
        );
    }
}
