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
use workforce_desk::crew::{self, Extras, ItemRefused, MentionedApproval};
use workforce_desk::line::{Asked, LineKind};
use workforce_desk::model::{DeskEvent, SessionState};
use workforce_desk::open_item::{
    self, ClosedBy, NewItem, OpenItem, OpenKind, OpenState, FIRST_RESTATE, RESTATED_AT_ONCE,
    SWEEP_AFTER,
};
use workforce_desk::open_item_store::Opened;
use workforce_desk::Speaker;

use super::tools::Bound;
use super::CrewHub;

pub const ASK: &str = "crew.ask_person";
pub const CLOSE: &str = "crew.close_item";
pub const OPEN: &str = "crew.open_items";

pub const ASK_DESCRIPTION: &str = "Ask the person something, or ask them to confirm a decision you took (kind: confirm): it goes to them at once as a needs-you message with its answers as buttons, and stays open until they answer, it no longer applies, or you close it (crew.close_item). Say it in full so it reads cold (text), what it holds up (holding_up), your recommendation, the answers to offer (options) and, when an answer would settle a kind of case from then on, that case (generalizes, e.g. \"When a session wants to merge with CI red\"). Asking the same thing again updates it: nothing is said twice. For a question a session asked you, use crew.escalate instead.";
pub const CLOSE_DESCRIPTION: &str = "Close one of your open items (item_id from crew.open_items or the open items Desk gives you): with answer when the person answered it in their own words (for a session's question, their answer goes on to the session), or with why when it no longer applies or you decided it yourself.";
pub const OPEN_DESCRIPTION: &str = "What you owe the person now (\"what's open?\"): the questions you asked them, the decisions you want confirmed, and the parked ones (no longer restated; they can answer or drop them), each with its item_id, when it was opened and last said, and what it holds up.";

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
    /// for a check-in. Whether it went out.
    pub(crate) async fn post_item(
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
                    // A session's question shows its own end where it is.
                    if closed.said() && closed.line_message.is_none() && by != ClosedBy::Person {
                        self.say_closed(&closed, &why).await;
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
    async fn resurface(&self, items: &[OpenItem]) {
        let now = Utc::now();
        let minute = chrono::Duration::from_std(self.timing.minute).unwrap_or_default();
        let first = minute * FIRST_RESTATE.num_minutes() as i32;
        let mut by_boss: HashMap<&str, Vec<&OpenItem>> = HashMap::new();
        for i in items {
            let Some((_, o)) = self.overseer_named(&i.boss) else {
                continue;
            };
            if o.wake.remind_minutes == 0 {
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
