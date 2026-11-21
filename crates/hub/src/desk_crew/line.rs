//! The overseer on Desk's line: what a session asks or tells it becomes a
//! turn of the overseer's on that session's own channel
//! (`desk:line:<session>`), apart from its DM with the person. It answers
//! with `crew.nudge { reply_to }` or gives the question to the person with
//! `crew.escalate`, each through the gate; a turn that ends without either
//! is settled from its reply ([`settle`]). Watching only, or with
//! `wake.on_message` off, questions go straight to the person.
//!
//! What it gives the person shows three ways, from the one row: a Floor
//! need, a question card in its DM, and the session's thread. Its routine
//! answers are decisions in its log, each said in its DM as one line with
//! its Undo ([`super::decisions`]).
//!
//! A session it runs ([`super::work`]) wakes it here at each turn's end
//! too, gathered with the session's messages into one `[Desk work]` turn.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_common::channels::DESK_CHANNEL;
use workforce_common::oversight::{Level, Oversight};
use workforce_desk::crew::{self, By, Extras, MentionedQuestion, PartyKind};
use workforce_desk::line::{Asked, Delivery, Escalation, LineKind, LineMessage};
use workforce_desk::model::DeskEvent;
use workforce_desk::open_item::{self, NewItem, OpenKind};
use workforce_desk::open_item_store::Opened;

use super::actions::{Action, Gate, Target};
use super::watcher::{reported, STANDING_RULE};
use super::{name_of, CrewHub, MAX_HOPS};
use crate::serve::AgentService;

/// What a line turn's prompt starts with: the person never typed it.
pub const LINE: &str = "[Desk line]";

/// How long a session's messages are gathered into one turn.
pub(super) const GATHER: Duration = Duration::from_secs(3);

/// How long the reports of a line turn that didn't run wait for another.
const NOT_RUN_AGAIN: Duration = Duration::from_secs(60);

/// How many of a session's reports one line turn shows in full: the rest,
/// gathered while its boss's turns were spent, by id.
const REPORTS_SHOWN: usize = 20;

/// How often the sessions a boss runs are looked at for requests it owes
/// an answer to.
const ASKS_EVERY: Duration = Duration::from_millis(500);

/// The overseer's own conversation for `session`'s line.
pub fn line_channel(session: Uuid) -> String {
    format!("{DESK_CHANNEL}:line:{session}")
}

/// What becomes of a question a line turn left open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Settle {
    /// Its reply is the answer, through the gate.
    Answer(String),
    /// The person answers it.
    Escalate(&'static str),
}

/// What becomes of the questions a line turn left open (`still_open` of
/// them), given how it ended: a reply answers one question left open; an
/// empty one, or a turn that failed, leaves them to the person.
pub(crate) fn settle(reply: Result<&str, ()>, still_open: usize) -> Vec<Settle> {
    match reply {
        Err(()) => vec![Settle::Escalate("Desk couldn't answer just now."); still_open],
        Ok(r) if r.trim().is_empty() => vec![Settle::Escalate("Desk didn't answer."); still_open],
        Ok(r) if still_open == 1 => vec![Settle::Answer(r.trim().to_string())],
        Ok(_) => vec![
            Settle::Escalate("Desk didn't say which of the questions its reply answers.");
            still_open
        ],
    }
}

/// Why a question goes to the person without the overseer taking a turn,
/// if it does. An `unbound` boss takes its sessions' questions whatever
/// `wake.on_message` says: at Act fully only budget, audit and Stop hold it.
pub(crate) fn straight_to_person(o: &Oversight, hop: i16, unbound: bool) -> Option<&'static str> {
    if o.level == Level::Watch {
        return Some("Desk only watches, so this came to you.");
    }
    if !o.wake.on_message && !unbound {
        return Some("Desk is set to give you sessions' questions.");
    }
    if hop >= MAX_HOPS {
        return Some("This was handed on too many times for Desk to take it further.");
    }
    None
}

/// How a session's line turn went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineTurn {
    /// Its boss's turn ran.
    Ran,
    /// No turn was its boss's to take (it no longer runs the session, there
    /// is none, or nothing gathered is its now): what it asked went to the
    /// person.
    None,
    /// Its boss's turn failed, or its breaker holds it: its questions went
    /// to the person, and the reports that waited for it wait again.
    NotRun,
}

/// A message on a session's line as its boss's turn reads it: here, or as a
/// machine's take carries it ([`super::relayed_work`]).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Said {
    pub id: Uuid,
    pub kind: LineKind,
    pub in_reply_to: Option<Uuid>,
    pub text: String,
}

impl From<&LineMessage> for Said {
    fn from(m: &LineMessage) -> Self {
        Self {
            id: m.id,
            kind: m.kind,
            in_reply_to: m.in_reply_to,
            text: m.full_text(),
        }
    }
}

/// What a line turn says of the session it is about, from what is known of
/// it.
pub(crate) fn about_session(known: &serde_json::Value) -> String {
    reported(&known.to_string(), "the session")
}

/// What of `messages` a line turn shows in full, in order: every question
/// and its newest [`REPORTS_SHOWN`] reports; and the ids of the reports
/// before them, said by id alone.
fn shown_reports(messages: &[Said]) -> (Vec<&Said>, Vec<Uuid>) {
    let reports = messages.iter().filter(|m| m.kind != LineKind::Ask);
    let hidden: HashSet<Uuid> = reports
        .clone()
        .take(reports.count().saturating_sub(REPORTS_SHOWN))
        .map(|m| m.id)
        .collect();
    let (earlier, shown): (Vec<&Said>, Vec<&Said>) =
        messages.iter().partition(|m| hidden.contains(&m.id));
    (shown, earlier.into_iter().map(|m| m.id).collect())
}

/// The ids of `messages` that are `at` now (the store moves only the
/// reports among them).
fn ids_in(messages: &[LineMessage], at: Delivery) -> Vec<Uuid> {
    messages
        .iter()
        .filter(|m| m.delivery == at)
        .map(|m| m.id)
        .collect()
}

/// The line a turn says of the reports `earlier` it doesn't show: how many,
/// the first and the last by id, however many there are.
fn earlier_said(earlier: &[Uuid]) -> Option<String> {
    let (first, last) = (earlier.first()?, earlier.last()?);
    Some(match earlier.len() {
        1 => format!("- and 1 earlier report, not shown: message {first}; read it whole with crew.history (message_id)"),
        n => format!(
            "- and {n} earlier reports, not shown, from message {first} to message {last}; read any whole with crew.history (message_id)"
        ),
    })
}

impl CrewHub {
    /// Keeps the words of the question `action` answers, for the gate.
    pub(crate) async fn know_question(&self, action: &Action) {
        let Action::Nudge {
            reply_to: Some(ask),
            ..
        } = action
        else {
            return;
        };
        if self.questions.lock().contains_key(ask) {
            return;
        }
        if let Ok(Some(q)) = self.desk.store().line_message(*ask).await {
            self.questions.lock().insert(*ask, q.full_text());
        }
    }

    /// An answer the gate made a suggestion goes to the person too, with
    /// it as Desk's draft.
    pub(crate) async fn answer_drafted(&self, action: &Action, by: &By, why: &str) {
        let Action::Nudge {
            reply_to: Some(ask),
            message,
            ..
        } = action
        else {
            return;
        };
        if let Err(e) = self
            .escalate(*ask, by, why, Vec::new(), Some(message.clone()))
            .await
        {
            tracing::info!(error = %e, "a drafted answer's question wasn't given to the person");
        }
    }

    /// Gives the question `ask` to the person, and puts it in the
    /// overseer's DM as a card they can answer from.
    pub(crate) async fn escalate(
        &self,
        ask: Uuid,
        by: &By,
        why: &str,
        options: Vec<String>,
        draft: Option<String>,
    ) -> Result<(), String> {
        let q = self
            .desk
            .line_escalate(
                ask,
                by,
                Escalation {
                    why: why.to_string(),
                    options,
                    draft,
                },
            )
            .await
            .map_err(|e| match e {
                workforce_desk::LineError::Refused(r) => r.to_string(),
                workforce_desk::LineError::Desk(e) => {
                    tracing::warn!(error = %e, "giving a question to the person failed");
                    "Something went wrong on this Mac.".to_string()
                }
            })?;
        self.record_escalated(&q, by, why).await;
        self.post_question(&q).await;
        Ok(())
    }

    /// The overseer `boss` and the person its DM is with, while there are
    /// both.
    fn dm(&self, boss: &str) -> Option<(Arc<AgentService>, String)> {
        let (service, _) = self.overseer_named(boss)?;
        let owner = self.person(Some(&service))?;
        Some((service, owner))
    }

    /// Puts the question `q`, given to the person, in the DM of the
    /// overseer it was asked of (by its scope) as a question card. With no
    /// DM yet, the Floor and the session's thread still show it.
    pub(crate) async fn post_question(&self, q: &LineMessage) {
        let boss = match q.to.kind {
            PartyKind::Manager => Some(q.to.id.clone()),
            _ => self
                .overseer_of_group(q.group.as_deref())
                .map(|(s, _)| s.agent_id.as_str().to_string()),
        };
        let Some((service, _)) = boss.and_then(|b| self.dm(&b)) else {
            return;
        };
        // The person is in that session themselves, answering it or
        // typing there: theirs, and nothing is posted about it. At it only
        // a moment before it asked, it is held (in Postgres, so a restart
        // keeps it) until they have left it for its boss's answer gap
        // ([`CrewHub::post_held_questions`]).
        if let Some(s) = q.from_session() {
            // Theirs for good: input since it asked, or the wheel pinned or
            // taken by a click since. A wheel that is theirs only from
            // input a moment before holds it, as that input does.
            use workforce_desk::charge::Wheel;
            let taken = self
                .desk
                .charge_of(s)
                .filter(|c| c.live())
                .is_some_and(|c| match c.wheel {
                    Wheel::PersonPinned => true,
                    Wheel::Person => c.wheel_since.unwrap_or(c.since) > q.at,
                    Wheel::Boss => false,
                });
            let since_asked = taken || self.desk.person_input_at(s).is_some_and(|at| at > q.at);
            if since_asked {
                self.let_go(q.id).await;
                return;
            }
            if self.person_in(s, q.at) {
                match self.desk.store().held_question_put(q.id).await {
                    Ok(()) => return,
                    // Not kept, it would never be said: said now instead.
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), "holding a session's question failed; said now")
                    }
                }
            }
            self.let_go(q.id).await;
        }
        let e = q.escalation.clone().unwrap_or(Escalation {
            why: String::new(),
            options: Vec::new(),
            draft: None,
        });
        // The question in its words: never the marks it copied from what
        // it read ("Re: [Desk m:…]"), and nothing posted when that is all.
        let headline = workforce_desk::line::without_marks(q.headline());
        if headline.trim().is_empty() {
            tracing::info!("a session's question held only marks; nothing to give the person");
            self.let_go(q.id).await;
            return;
        }
        // One item, and one card, however often it is given to the person:
        // escalated again, it is updated and nothing new is posted.
        let mut item = NewItem::ask(
            service.agent_id.as_str(),
            OpenKind::Ask,
            q.from_session(),
            &format!(
                "{} asks: “{}”",
                q.from.name,
                crew::one_line(headline.as_str(), 400)
            ),
        );
        item.dedupe_key = open_item::line_key(q.id);
        item.line_message = Some(q.id);
        item.options = e.options.clone();
        item.recommendation = e.draft.clone();
        // Given to the person is what the boss couldn't decide.
        item.cant_decide = Some(match e.why.trim() {
            "" => "It gave the person the session's question.".to_string(),
            why => why.to_string(),
        });
        let full = self
            .overseer_named(service.agent_id.as_str())
            .is_some_and(|(_, o)| {
                self.level_now(service.agent_id.as_str(), &o).0
                    == workforce_common::oversight::Level::Full
            });
        let item_id = match self.desk.store().item_open(&item).await {
            Ok(Opened::New(i)) => Some(i.id),
            Ok(Opened::Again(_)) => return,
            // Kept or not, the person still gets the question.
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "keeping a session's question as an open item failed");
                None
            }
        };
        // One line: the question and why in a few words; the rest is
        // behind "more".
        let asked = crew::words(
            &crew::one_line(headline.as_str(), 400),
            workforce_desk::open_item::QUESTION_WORDS,
        );
        let why = workforce_desk::decision::clause(&crew::words(
            &crew::one_line(&e.why, 400),
            workforce_desk::open_item::POINT_WORDS,
        ));
        let text = match &why {
            Some(w) => format!("{} asks: “{asked}” — {w}", q.from.name),
            None => format!("{} asks: “{asked}”", q.from.name),
        };
        // What the line leaves out of either is behind "more".
        let flat = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut detail: Vec<String> = Vec::new();
        if flat(headline.as_str()) != flat(&asked) {
            detail.push(headline.as_str().trim().to_string());
        }
        if !e.why.trim().is_empty()
            && why.as_deref().map(flat) != workforce_desk::decision::clause(&flat(&e.why))
        {
            detail.push(e.why.trim().to_string());
        }
        let extras = Extras {
            questions: vec![MentionedQuestion {
                message_id: q.id.to_string(),
                session_id: q.from.id.clone(),
                session: q.from.name.clone(),
                ask: asked.clone(),
                why: why.clone().unwrap_or_default(),
                options: e.options,
                // At Act fully the boss recommends nothing.
                draft: e.draft.filter(|_| !full),
                detail: (!detail.is_empty()).then(|| detail.join("\n\n")),
            }],
            ..Default::default()
        };
        // While the person types to the boss, the card waits for a pause,
        // at most three minutes, without holding up the caller.
        self.card(service.agent_id.as_str(), item_id, text, extras)
            .await;
    }

    /// Lets the held session question `id` go, if it was held.
    async fn let_go(&self, id: Uuid) {
        if let Err(e) = self.desk.store().held_question_drop(id).await {
            tracing::info!(error = %format!("{e:#}"), "a held question wasn't let go");
        }
    }

    /// Whether the session question `id` is held now.
    #[cfg(test)]
    pub(crate) async fn is_held(&self, id: Uuid) -> bool {
        self.desk
            .store()
            .held_questions()
            .await
            .unwrap_or_default()
            .contains(&id)
    }

    /// Says each held session question ([`Self::post_question`]) still
    /// waiting on the person, once they have left its session for its
    /// boss's answer gap; one they answered there, or that stopped waiting,
    /// is let go.
    pub(crate) async fn post_held_questions(&self) {
        let held = match self.desk.store().held_questions().await {
            Ok(h) => h,
            Err(e) => {
                tracing::info!(error = %format!("{e:#}"), "the held questions couldn't be read");
                return;
            }
        };
        for id in held {
            let q = match self.desk.store().line_message(id).await {
                Ok(Some(q)) if q.asked == Some(workforce_desk::line::Asked::Escalated) => q,
                Ok(_) => {
                    self.let_go(id).await;
                    continue;
                }
                Err(e) => {
                    tracing::info!(error = %e, "a held question couldn't be read");
                    continue;
                }
            };
            self.post_question(&q).await;
        }
    }

    /// Gives every question in `asks` (of `session`) still open to the
    /// person, saying `why`, as the person's own: no boss is in it.
    async fn questions_to_person(&self, session: Uuid, asks: &[LineMessage], why: &str) {
        for ask in asks {
            match self.desk.store().line_message(ask.id).await {
                Ok(Some(now)) if now.asked == Some(Asked::Open) => {}
                _ => continue,
            }
            if let Err(e) = self
                .escalate(ask.id, &By::you(), why, Vec::new(), None)
                .await
            {
                tracing::info!(%session, error = %e, "a question wasn't given to the person");
            }
        }
    }

    /// Gives every question in `asks` (of `session`) to the person, saying
    /// `why`: as the overseer of its scope, when there is one.
    async fn all_to_person(&self, session: Uuid, asks: &[LineMessage], why: &str) {
        let by = match self.overseer_of_session(session) {
            Some((service, _)) => By::manager(service.agent_id.as_str(), &name_of(&service), why),
            None => By::you(),
        };
        for ask in asks {
            if let Err(e) = self.escalate(ask.id, &by, why, Vec::new(), None).await {
                tracing::info!(error = %e, "a question wasn't given to the person");
            }
        }
    }

    /// One turn of the overseer's on `session`'s line, for `messages`,
    /// and for what `ended` says of its turn's end when the overseer runs
    /// it (a work turn): how it went.
    pub(crate) async fn answer_line(
        &self,
        session: Uuid,
        messages: Vec<LineMessage>,
        ended: Vec<String>,
    ) -> LineTurn {
        // Its boss decides it with nothing else of its own deciding at
        // once, its prompt and picture read once it holds its mind
        // ([`super::mind`]).
        match self.line_boss(session) {
            Some(boss) => {
                self.with_mind(
                    &boss,
                    "a session's line",
                    self.answer_line_now(session, messages, ended),
                )
                .await
            }
            None => self.answer_line_now(session, messages, ended).await,
        }
    }

    /// The boss that answers `session`'s line now, if one does.
    fn line_boss(&self, session: Uuid) -> Option<String> {
        self.overseer_of_session(session)
            .map(|(s, _)| s.agent_id.as_str().to_string())
    }

    /// How the boss's turn went.
    async fn answer_line_now(
        &self,
        session: Uuid,
        messages: Vec<LineMessage>,
        ended: Vec<String>,
    ) -> LineTurn {
        // Taken back from its boss, it is the person's: what it tells Desk
        // is kept (its thread, the picture) and no boss's turn is spent on
        // it; what it asks goes to the person.
        if self.taken_back(session) {
            let asks: Vec<LineMessage> = messages
                .into_iter()
                .filter(|m| m.kind == LineKind::Ask)
                .collect();
            self.questions_to_person(
                session,
                &asks,
                "Desk no longer runs this session, so its questions come to you.",
            )
            .await;
            return LineTurn::None;
        }
        // Run by a boss on the Mac that reads its line: its home takes it
        // there ([`super::relayed_work`]).
        if let Some(boss) = self.reading_home(session) {
            self.hold_for_home(&boss, &messages).await;
            return LineTurn::None;
        }
        // The overseer of the session's scope answers it, and no other.
        let Some((service, o)) = self.overseer_of_session(session) else {
            let asks: Vec<LineMessage> = messages
                .into_iter()
                .filter(|m| m.kind == LineKind::Ask)
                .collect();
            self.all_to_person(session, &asks, "No overseer answers sessions now.")
                .await;
            return LineTurn::None;
        };
        // Held by Desk's breaker, it takes no turns: what its session asks
        // goes straight to the person, saying why, and its turn's end waits
        // for no one.
        if let Some(held) = self.desk.agent_held_why(service.agent_id.as_str()) {
            let asks: Vec<LineMessage> = messages
                .into_iter()
                .filter(|m| m.kind == LineKind::Ask)
                .collect();
            let why = format!("{} is {held}, so this came to you.", name_of(&service));
            self.all_to_person(session, &asks, &why).await;
            return LineTurn::NotRun;
        }
        // Each read again as it is now: one the session's move (or anyone)
        // took from this overseer since it was gathered isn't its to read,
        // and a question so taken is the person's.
        let seen = self.desk.seen_by(&o.scope);
        let mut kept: Vec<LineMessage> = Vec::new();
        let mut taken = Vec::new();
        for m in messages {
            // Gathered twice (taken up at start and heard as it came): once.
            if kept.iter().chain(taken.iter()).any(|k| k.id == m.id) {
                continue;
            }
            let now = match self.desk.store().line_message(m.id).await {
                Ok(Some(now)) => now,
                _ => continue,
            };
            let ours = now.to.id == service.agent_id.as_str()
                && seen.sees_row(now.group.as_deref(), now.at)
                && (now.kind != LineKind::Ask || now.asked == Some(Asked::Open));
            match (
                ours,
                now.kind == LineKind::Ask && now.asked == Some(Asked::Open),
            ) {
                (true, _) => kept.push(now),
                (false, true) => taken.push(now),
                (false, false) => {}
            }
        }
        for ask in &taken {
            let why = "The overseer it asked no longer oversees it, so this came to you.";
            if let Err(e) = self
                .escalate(ask.id, &By::you(), why, Vec::new(), None)
                .await
            {
                tracing::info!(error = %e, "a question wasn't given to the person");
            }
        }
        if kept.is_empty() && ended.is_empty() {
            return LineTurn::None;
        }
        let mut messages = kept;
        let mut asks: Vec<LineMessage> = messages
            .iter()
            .filter(|m| m.kind == LineKind::Ask)
            .cloned()
            .collect();
        let hop = messages.iter().map(|m| m.hop).max().unwrap_or(0);
        if let Some(why) = straight_to_person(&o, hop, self.unbound(&o)) {
            self.all_to_person(session, &asks, why).await;
            if ended.is_empty() {
                return LineTurn::None;
            }
            // Its turn's end is still the overseer's to follow up.
            messages.retain(|m| m.kind != LineKind::Ask);
            asks.clear();
        }
        let Some(owner) = self.person(Some(&service)) else {
            self.all_to_person(
                session,
                &asks,
                "Desk has no conversation with you to work from yet.",
            )
            .await;
            return LineTurn::None;
        };
        {
            let mut known = self.questions.lock();
            for a in &asks {
                known.insert(a.id, a.full_text());
            }
        }
        let prompt = self.line_prompt(session, &messages, &ended, &o);
        let boss = service.agent_id.as_str().to_string();
        let mark = self.acted_mark(&boss, session);
        let since = self.turns_begun();
        let ran = self
            .run_turn_with(
                &service,
                &owner,
                &line_channel(session),
                &prompt,
                hop,
                None,
                None,
                None,
            )
            .await;
        let did = ran.as_ref().is_ok_and(|r| r.acted);
        let ran = ran.map(|r| (r.session_id, r.answer));
        if let Err(e) = &ran {
            tracing::warn!(%session, error = %format!("{e:#}"), "the overseer's line turn failed");
        }
        let mut still_open = Vec::new();
        for a in &asks {
            if let Ok(Some(now)) = self.desk.store().line_message(a.id).await {
                if now.asked == Some(Asked::Open) && now.to.id == service.agent_id.as_str() {
                    still_open.push(now);
                }
            }
        }
        let reply = ran.as_ref().map(|(_, r)| r.as_str()).map_err(|_| ());
        // What its reply settles was decided in the turn, on its picture.
        let deciding = ran
            .as_ref()
            .ok()
            .and_then(|(sid, _)| self.turn_deciding(sid));
        for (ask, settled) in still_open.iter().zip(settle(reply, still_open.len())) {
            match settled {
                Settle::Escalate(why) => {
                    let by = By::manager(service.agent_id.as_str(), &name_of(&service), why);
                    let _ = self.escalate(ask.id, &by, why, Vec::new(), None).await;
                }
                Settle::Answer(text) => {
                    // The reply's own first sentence is why, as the person
                    // reads it.
                    let reason = match crew::one_line(&text, 200) {
                        r if r.is_empty() => {
                            "Its reply answered the session's question.".to_string()
                        }
                        r => r,
                    };
                    let by = By::manager(service.agent_id.as_str(), &name_of(&service), &reason);
                    let action = Action::Nudge {
                        to: Target::Session(session),
                        message: text,
                        reply_to: Some(ask.id),
                    };
                    match self.gate(&action, &o) {
                        Gate::Run => {
                            let by = self.by_at_level(by.clone(), &o);
                            match self.perform_in(deciding.clone(), &action, by.clone()).await {
                                // Decided before what moved: the question
                                // isn't left open with no one on it.
                                Err(e) if super::picture::stale(&e) => {
                                    let why = "Its answer was decided before something changed, so this came to you.";
                                    let _ = self.escalate(ask.id, &by, why, Vec::new(), None).await;
                                }
                                Err(e) => {
                                    tracing::info!(error = %e, "the overseer's reply didn't answer");
                                }
                                Ok(_) => {}
                            }
                        }
                        Gate::Suggest(why) => {
                            let input = serde_json::json!({
                                "session_id": session.to_string(),
                                "reply_to": ask.id.to_string(),
                                "message": match &action { Action::Nudge { message, .. } => message.clone(), _ => String::new() },
                            });
                            if let Err(e) = self.suggest("crew.nudge", &input, &action, &by).await {
                                tracing::info!(error = %e, "an answer's suggestion wasn't kept");
                            }
                            self.answer_drafted(&action, &by, &why).await;
                        }
                        Gate::No(_) => {
                            let _ = self
                                .escalate(
                                    ask.id,
                                    &By::you(),
                                    "Desk no longer runs this session, so its questions come to you.",
                                    Vec::new(),
                                    None,
                                )
                                .await;
                        }
                        Gate::Refuse => {
                            let _ = self
                                .escalate(
                                    ask.id,
                                    &by,
                                    "Desk only watches, so this came to you.",
                                    Vec::new(),
                                    None,
                                )
                                .await;
                        }
                    }
                }
            }
        }
        // A turn that decided nothing for the session, and suggested
        // nothing, is a decision too: recorded here, never read from what
        // it said. One that did nothing at all is kept out of Activity.
        let decided = self.acted_mark(&boss, session) != mark;
        if let (Ok((_, reply)), false) = (&ran, decided) {
            self.record_nothing(&boss, session, !ended.is_empty(), reply)
                .await;
        }
        self.turn_end_said(&boss, &line_channel(session), since, did || decided);
        match ran {
            Ok(_) => LineTurn::Ran,
            Err(_) => LineTurn::NotRun,
        }
    }

    /// What the overseer reads for a session's messages to it, and its
    /// turn's end (`ended`) when it runs it.
    pub(super) fn line_prompt(
        &self,
        session: Uuid,
        messages: &[LineMessage],
        ended: &[String],
        o: &Oversight,
    ) -> String {
        let about = match self.desk.view(session) {
            Ok(v) => about_session(&serde_json::json!({
                "name": v.label(),
                "engine": v.engine.title(),
                "folder": v.cwd,
                "goal": v.goal,
                "state": v.state,
                "last_reply": v.last_reply.as_deref().map(|r| crew::clip(r, 1500)),
            })),
            Err(_) => format!("session {session}"),
        };
        let said: Vec<Said> = messages.iter().map(Said::from).collect();
        self.line_prompt_of(
            &session.to_string(),
            &about,
            &said,
            ended,
            o,
            self.ships_in(session, o),
        )
    }

    /// [`Self::line_prompt`] for the session the overseer knows as
    /// `session`, told of as `about`: one here, or one of a machine's
    /// ([`super::relayed_work`]). `ships`: it acts fully in the session's
    /// repository.
    pub(super) fn line_prompt_of(
        &self,
        session: &str,
        about: &str,
        messages: &[Said],
        ended: &[String],
        o: &Oversight,
        ships: bool,
    ) -> String {
        let (shown, earlier) = shown_reports(messages);
        let lines: Vec<String> = shown
            .into_iter()
            .map(|m| {
                let kind = match (m.kind, m.in_reply_to) {
                    (LineKind::Ask, _) => "asks".to_string(),
                    (LineKind::Tell, Some(to)) => format!("replies to your message {to}"),
                    (LineKind::Tell | LineKind::Answer, _) => "tells you".to_string(),
                };
                format!("- message {} ({kind}): {}", m.id, m.text)
            })
            .chain(earlier_said(&earlier))
            .collect();
        // Acting fully in its repository, shipping is the overseer's too.
        let (level, outward) = match ships {
            true => (
                Level::Full,
                "deletes, spends, sends to people, changes scope or touches secrets, where the person's own \
                 orders and preferences don't cover it (merging, pushing to the default branch, tagging, \
                 promoting and deploying this repository are yours at Act fully)",
            ),
            false => (
                match o.level {
                    Level::Full => Level::Act,
                    l => l,
                },
                "pushes, deletes, deploys, spends, sends to people or changes scope",
            ),
        };
        // Unbound, it answers everything itself.
        let unbound = self.unbound(o);
        let (outward, level_line) = match unbound {
            true => ("you truly can't decide, and say why", Self::boss_hint(o)),
            false => (outward, Self::bound_level_line(level)),
        };
        if !ended.is_empty() {
            let said = match lines.is_empty() {
                true => String::new(),
                false => format!(
                    "\n\nIt also wrote to you on Desk's line:\n{}\nFor each question: answer it with crew.nudge \
                     (session_id, reply_to = the message id, message), or give it to the person with crew.escalate \
                     (message_id, reason, options, draft).",
                    reported(&lines.join("\n"), "the session")
                ),
            };
            return format!(
                "{}\n{about}{said}\n\nAlways give the person anything that {outward}. {}",
                self.work_intro(session, ended, o),
                level_line,
            );
        }
        format!(
            "{LINE} A session on Desk's line (session_id {session}) wrote to you, its overseer:\n{about}\n\n{}\n\n\
             For each question: answer it with crew.nudge (session_id, reply_to = the message id, message), \
             or give it to the person with crew.escalate (message_id, reason, options, draft) — always for \
             anything that {outward}, and whenever \
             you are unsure. A reply with no tool call is taken as your answer to a single question. \
             Messages that only tell you something need no answer. {}",
            reported(&lines.join("\n"), "the session"),
            level_line,
        )
    }

    /// What a line turn says of `level` when its boss isn't unbound: at
    /// Act fully (shipping in a repository the person named), the standing
    /// rule too.
    pub(super) fn bound_level_line(level: Level) -> String {
        match level {
            Level::Full => format!("{} {STANDING_RULE}", Self::level_line(level)),
            _ => Self::level_line(level).to_string(),
        }
    }

    fn level_line(level: Level) -> &'static str {
        match level {
            Level::Watch => "You only watch.",
            Level::Suggest => "You suggest: an answer you give becomes a draft the person sends.",
            Level::Act => "You may answer within what the person allowed; anything else becomes a draft for them.",
            Level::Full => "You act fully: within what the person allowed, and in the repositories they named also on merging a pull request, pushing the default branch, tagging, releasing, and running the workflows they listed to promote and deploy. Anything else becomes a draft for them. Never secrets, registries, force pushes, remote deletes or your own limits.",
        }
    }

    /// Whether `messages` from `session` would start a line turn of their
    /// own that its boss's hour of turns paces: reports alone, each to the
    /// boss that runs it at a level that gives it turns. The same test that
    /// keeps them out of the loop guard's counts
    /// ([`workforce_desk::Desk::its_acting_boss`]): what isn't counted
    /// there is paced here.
    /// Its boss is the one whose hour of turns is spent ([`Self::line_boss`]).
    fn reports_paced(&self, session: Uuid, messages: &[LineMessage]) -> bool {
        let boss = self.line_boss(session);
        !messages.is_empty()
            && messages.iter().all(|m| {
                m.kind == LineKind::Tell
                    && boss.as_deref() == Some(m.to.id.as_str())
                    && self.desk.its_acting_boss(session, &m.to)
            })
    }

    /// Keeps the reports `waiting` for their boss's next turn as waiting:
    /// a restart before it gathers them again.
    async fn wait_for_next_turn(&self, waiting: &mut [LineMessage]) {
        let heard = ids_in(waiting, Delivery::Delivered);
        match self.desk.line_hold_for_boss(&heard).await {
            Ok(moved) => waiting
                .iter_mut()
                .filter(|m| moved.contains(&m.id))
                .for_each(|m| m.delivery = Delivery::Queued),
            Err(e) => tracing::warn!(error = %e, "reports couldn't be kept waiting"),
        }
    }

    /// The reports in `messages` that waited for the turn that took them,
    /// taken.
    async fn reports_taken(&self, messages: &[LineMessage]) {
        let waited = ids_in(messages, Delivery::Queued);
        if let Err(e) = self.desk.line_reports_taken(&waited).await {
            tracing::warn!(error = %e, "reports taken weren't marked");
        }
    }

    /// What waits on the line for a boss's turn, by session, as the store
    /// keeps it: the questions open to a boss, and the reports waiting for
    /// its next turn. Read at start, and again when the line loop fell
    /// behind Desk's events.
    pub(super) async fn line_kept(&self) -> HashMap<Uuid, Vec<LineMessage>> {
        let mut kept = self.take_up_open_asks().await;
        for (session, reports) in self.take_up_waiting_reports().await {
            kept.entry(session).or_default().extend(reports);
        }
        kept
    }

    /// The reports that wait for a boss's next turn, by session.
    async fn take_up_waiting_reports(&self) -> HashMap<Uuid, Vec<LineMessage>> {
        let reports = match self.desk.store().line_waiting_for_bosses().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "reading reports waiting for a boss failed");
                Vec::new()
            }
        };
        reports.into_iter().fold(HashMap::new(), |mut by, m| {
            if let Some(session) = m.from_session() {
                by.entry(session).or_insert_with(Vec::new).push(m);
            }
            by
        })
    }

    /// Questions asked of an overseer that is gone, or never got their
    /// turn (the hub stopped first): taken up by the overseer of the
    /// session's scope now, or given to the person when there is none. One
    /// asked in another scope than the session's now is never re-pointed
    /// to this one's overseer: it goes to the person.
    async fn take_up_open_asks(&self) -> HashMap<Uuid, Vec<LineMessage>> {
        let mut by_session: HashMap<Uuid, Vec<LineMessage>> = HashMap::new();
        let asks = match self.desk.store().line_open_asks(None).await {
            Ok(asks) => asks,
            Err(e) => {
                tracing::warn!(error = %e, "reading open questions to the overseer failed");
                return by_session;
            }
        };
        for mut ask in asks {
            if ask.to.kind != PartyKind::Manager || ask.asked != Some(Asked::Open) {
                continue;
            }
            let Some(session) = ask.from_session() else {
                continue;
            };
            let asked_in = self.desk.scope_of_group(ask.group.as_deref());
            match self.overseer_of_session(session) {
                Some((service, o)) if o.scope == asked_in => {
                    if service.agent_id.as_str() != ask.to.id {
                        // Re-pointed to the overseer its scope has now.
                        ask.to =
                            crew::Party::manager(service.agent_id.as_str(), &name_of(&service));
                        ask.updated_at = crew::now();
                        if let Err(e) = self.desk.store().put_line(&ask).await {
                            tracing::warn!(error = %e, "re-pointing a question failed");
                            continue;
                        }
                    }
                }
                // Asked in another scope than the session is in now: never
                // this one's overseer's, so the person's.
                Some(_) => {
                    let why = "The overseer it asked no longer oversees it, so this came to you.";
                    if let Err(e) = self
                        .escalate(ask.id, &By::you(), why, Vec::new(), None)
                        .await
                    {
                        tracing::info!(error = %e, "a question wasn't given to the person");
                    }
                    continue;
                }
                // Its line turn gives it to the person.
                None => {}
            }
            by_session.entry(session).or_default().push(ask);
        }
        by_session
    }
}

/// What waits to become a session's next line turn.
struct Gathered {
    since: Instant,
    /// Not tried again before then: past its boss's turns for the hour,
    /// looked at again every [`GATHER`]; its turn didn't run,
    /// [`NOT_RUN_AGAIN`].
    /// A question gathered with it goes at once all the same.
    retry: Option<Instant>,
    messages: Vec<LineMessage>,
    /// What its turn's end said, when the overseer runs it.
    ended: Vec<String>,
    /// The requests it waits on that its boss is given, each with its line
    /// for the work turn: read again as the turn starts, and only those
    /// still open go in.
    asks: Vec<(Uuid, String)>,
    /// Its turn's end is one the fast call handed over with its refused
    /// next step (`super::idle`).
    handed: bool,
}

impl Gathered {
    /// Adds `messages`, each once.
    fn take(&mut self, messages: Vec<LineMessage>) {
        for m in messages {
            self.add(m);
        }
    }

    /// Adds `m`, once.
    fn add(&mut self, m: LineMessage) {
        if !self.messages.iter().any(|k| k.id == m.id) {
            self.messages.push(m);
        }
    }

    fn new() -> Self {
        Self {
            since: Instant::now(),
            retry: None,
            messages: Vec::new(),
            ended: Vec::new(),
            asks: Vec::new(),
            handed: false,
        }
    }
}

/// Turns sessions' messages to the overseer, and the turn ends of the
/// sessions it runs, into its line turns: each session's gathered for
/// [`GATHER`], one line turn at a time per boss ([`super::mind`]): while
/// its mind is held, or the person's own turn runs in its DM, what its
/// sessions say keeps gathering, and the session that has waited longest
/// goes first. It takes sessions on as they appear, when it is set to.
pub(super) fn line_loop(
    crew: Arc<CrewHub>,
    cancel: CancellationToken,
) -> impl std::future::Future<Output = ()> {
    let mut events = crew.desk.events();
    let mut rework = crew.rework.subscribe();
    async move {
        let mut gathering: HashMap<Uuid, Gathered> = HashMap::new();
        for (session, kept) in crew.line_kept().await {
            gathering
                .entry(session)
                .or_insert_with(Gathered::new)
                .take(kept);
        }
        // The waiting reports of a turn that didn't run, given back.
        let (give_back, mut given_back) =
            tokio::sync::mpsc::unbounded_channel::<(Uuid, Vec<LineMessage>)>();
        // Each line turn running, by session, with the boss it is for.
        let mut running: HashMap<Uuid, (tokio::task::JoinHandle<()>, Option<String>)> =
            HashMap::new();
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut taking_on: Option<tokio::task::JoinHandle<()>> = None;
        let mut looked: Option<Instant> = None;
        // The requests already given to a boss, by id and boss, while open.
        let mut offered: HashSet<(Uuid, String)> = HashSet::new();
        let mut asked_at: Option<Instant> = None;
        loop {
            let mut look = false;
            let mut lagged = false;
            tokio::select! {
                () = cancel.cancelled() => return,
                received = events.recv() => match received {
                    Ok(DeskEvent::Line { message, removed: false }) => {
                        let fresh = message.to.kind == PartyKind::Manager
                            && matches!(message.kind, LineKind::Ask | LineKind::Tell)
                            && message.at == message.updated_at
                            && message.asked.is_none_or(|a| a == Asked::Open);
                        if let (true, Some(session)) = (fresh, message.from_session()) {
                            gathering
                                .entry(session)
                                .or_insert_with(Gathered::new)
                                .add(*message);
                        }
                    }
                    Ok(DeskEvent::Activity { event }) => {
                        // What the breaker did to a session the overseer runs
                        // outside a plan (a plan's go to its own turn).
                        if let Some((session, line, None)) = crew.breaker_note(&event).await {
                            gathering
                                .entry(session)
                                .or_insert_with(Gathered::new)
                                .ended
                                .push(line);
                        }
                        if let Some((session, mut line)) = crew.work_line(&event) {
                            if super::watcher::finished_session(&event).is_some() {
                                line = crew.with_doing(session, line).await;
                            }
                            gathering
                                .entry(session)
                                .or_insert_with(Gathered::new)
                                .ended
                                .push(line);
                        }
                    }
                    Ok(DeskEvent::Session { .. }) => {
                        look = looked.is_none_or(|at| at.elapsed() >= Duration::from_secs(1));
                    }
                    Err(RecvError::Lagged(missed)) => {
                        // Its questions are read again from the store: none
                        // missed is left unanswered.
                        tracing::warn!(missed, "the line loop fell behind Desk's events");
                        lagged = true;
                    }
                    Ok(_) => {}
                    Err(RecvError::Closed) => return,
                },
                // A turn's end the boss's fast call handed to its full turn.
                Ok(event) = rework.recv() => {
                    if let Some((session, line)) = crew.work_line(&event) {
                        let line = crew.with_doing(session, line).await;
                        let g = gathering.entry(session).or_insert_with(Gathered::new);
                        g.ended.push(line);
                        g.handed = true;
                    }
                }
                Some((session, reports)) = given_back.recv() => {
                    let g = gathering.entry(session).or_insert_with(Gathered::new);
                    g.retry = Some(Instant::now() + NOT_RUN_AGAIN);
                    g.take(reports);
                }
                _ = tick.tick() => {}
            }
            // A session's turn running now has what it was given: only the
            // others' are taken up again.
            if lagged {
                for (session, kept) in crew
                    .line_kept()
                    .await
                    .into_iter()
                    .filter(|(s, _)| !running.contains_key(s))
                {
                    gathering
                        .entry(session)
                        .or_insert_with(Gathered::new)
                        .take(kept);
                }
            }
            // What the sessions a boss runs ask Desk to allow, at most twice a
            // second: each open request it can answer and hasn't been given.
            if asked_at.is_none_or(|at| at.elapsed() >= ASKS_EVERY) {
                asked_at = Some(Instant::now());
                crew.sweep_unread_homes().await;
                for (session, approval, line) in crew.owed_asks(&mut offered) {
                    tracing::info!(%session, %approval, "a request goes to the boss that runs its session");
                    gathering
                        .entry(session)
                        .or_insert_with(Gathered::new)
                        .asks
                        .push((approval, line));
                }
            }
            // New sessions as they appear, and all of them now and then.
            look |= looked.is_none_or(|at| at.elapsed() >= crew.timing.tick);
            if look && taking_on.as_ref().is_none_or(|h| h.is_finished()) {
                looked = Some(Instant::now());
                let crew = Arc::clone(&crew);
                taking_on = Some(tokio::spawn(async move { crew.take_on_all().await }));
            }
            running.retain(|_, (h, _)| !h.is_finished());
            let mut due: Vec<(Uuid, Instant)> = gathering
                .iter()
                .filter(|(s, g)| {
                    g.since.elapsed() >= GATHER
                        && (g.retry.is_none_or(|at| at <= Instant::now())
                            || g.messages.iter().any(|m| m.kind == LineKind::Ask))
                        && !running.contains_key(s)
                })
                .map(|(s, g)| (*s, g.since))
                .collect();
            // The longest waiting first: each boss starts one at a time.
            due.sort_by_key(|(_, since)| *since);
            let mut busy: HashSet<String> =
                running.values().filter_map(|(_, b)| b.clone()).collect();
            for (session, since) in due {
                let boss = crew.line_boss(session);
                if let Some(b) = &boss {
                    // One line turn per boss at a time; while its mind is busy
                    // what its sessions say gathers, until it has waited long
                    // enough to queue for the mind in turn.
                    if busy.contains(b) || !crew.mind_may_start(b, since) {
                        continue;
                    }
                }
                let Some(Gathered {
                    mut messages,
                    mut ended,
                    mut asks,
                    mut handed,
                    ..
                }) = gathering.remove(&session)
                else {
                    continue;
                };
                // Answered while it was gathered (by the person, say): no turn
                // for it.
                let open = crew
                    .desk
                    .view(session)
                    .ok()
                    .and_then(|v| v.pending)
                    .map(|p| p.id);
                asks.retain(|(approval, _)| Some(*approval) == open);
                let work = !ended.is_empty() || !asks.is_empty();
                // Let go, or taken over, while it waited: no work turn, and its
                // requests are the boss's to be given again once it drives.
                if work && !crew.still_works(session) {
                    ended.clear();
                    handed = false;
                    let boss = crew.boss_of(session);
                    for (approval, _) in asks.drain(..) {
                        offered.retain(|(a, b)| *a != approval || Some(b) != boss.as_ref());
                    }
                    if messages.is_empty() {
                        continue;
                    }
                }
                // Out of its boss's turns for the hour: its turn's end, what it
                // asks Desk to allow and its reports wait, gathered, for the
                // next, in their place in line; its questions go on now.
                let work = !ended.is_empty() || !asks.is_empty();
                let reports = crew.reports_paced(session, &messages);
                if (work || reports) && !crew.spend_work_turn(session) {
                    let mut waiting = if reports {
                        std::mem::take(&mut messages)
                    } else {
                        Vec::new()
                    };
                    crew.wait_for_next_turn(&mut waiting).await;
                    gathering.insert(
                        session,
                        Gathered {
                            since,
                            // Looked at again soon, not only when the hour
                            // frees one: the person may raise the cap or
                            // turn on Act fully meanwhile.
                            retry: Some(Instant::now() + GATHER),
                            messages: waiting,
                            ended: std::mem::take(&mut ended),
                            asks: std::mem::take(&mut asks),
                            handed: std::mem::take(&mut handed),
                        },
                    );
                    if messages.is_empty() {
                        continue;
                    }
                }
                ended.extend(asks.iter().map(|(_, line)| line.clone()));
                if let Some(b) = &boss {
                    busy.insert(b.clone());
                }
                let crew = Arc::clone(&crew);
                let give_back = give_back.clone();
                let turn_boss = boss.clone().filter(|_| handed);
                running.insert(
                    session,
                    (
                        tokio::spawn(async move {
                            let waited: Vec<LineMessage> = messages
                                .iter()
                                .filter(|m| m.delivery == Delivery::Queued)
                                .cloned()
                                .collect();
                            let turn = crew.answer_line(session, messages, ended).await;
                            // Taken once its turn has read them; a turn that
                            // didn't run leaves them waiting, for another.
                            match turn {
                                LineTurn::NotRun if !waited.is_empty() => {
                                    let _ = give_back.send((session, waited));
                                }
                                _ => crew.reports_taken(&waited).await,
                            }
                            let ran = turn == LineTurn::Ran;
                            // A refused next step handed to it was its to take
                            // (`super::idle`), once its turn ran.
                            if let Some(b) = turn_boss.as_ref().filter(|_| ran) {
                                crew.full_turn_over(b, session);
                            }
                            // What its turn was given and left open is the
                            // person's: never given to it again by itself.
                            crew.left_open(session, &asks).await;
                        }),
                        boss,
                    ),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_left_open_is_settled_by_its_reply_or_given_to_the_person() {
        assert_eq!(settle(Ok("5433"), 1), vec![Settle::Answer("5433".into())]);
        assert_eq!(
            settle(Ok("  "), 1),
            vec![Settle::Escalate("Desk didn't answer.")]
        );
        assert_eq!(
            settle(Err(()), 2),
            vec![Settle::Escalate("Desk couldn't answer just now."); 2]
        );
        assert_eq!(settle(Ok("yes"), 2).len(), 2);
        assert!(settle(Ok("yes"), 2)
            .iter()
            .all(|s| matches!(s, Settle::Escalate(_))));
        assert!(settle(Ok("done"), 0).is_empty(), "the tools answered");
    }

    #[test]
    fn a_turn_shows_every_question_and_its_newest_reports_and_the_rest_by_id() {
        let from = crew::Party {
            kind: PartyKind::Session,
            id: Uuid::new_v4().to_string(),
            name: "Legacy removal".into(),
            engine: None,
            origin: None,
        };
        let to = crew::Party::manager("desk", "Desk");
        let said = |kind, text: String| LineMessage::new(from.clone(), to.clone(), kind, text, 1);
        let mut messages: Vec<LineMessage> = (0..REPORTS_SHOWN + 5)
            .map(|i| said(LineKind::Tell, format!("step {i} done")))
            .collect();
        messages.insert(2, said(LineKind::Ask, "Drop the queue table?".into()));
        let said: Vec<Said> = messages.iter().map(Said::from).collect();
        let (shown, earlier) = shown_reports(&said);
        assert_eq!(
            earlier,
            messages
                .iter()
                .filter(|m| m.kind == LineKind::Tell)
                .take(5)
                .map(|m| m.id)
                .collect::<Vec<_>>(),
            "the oldest reports, by id"
        );
        assert_eq!(shown.len(), REPORTS_SHOWN + 1);
        assert_eq!(
            shown[0].text, "Drop the queue table?",
            "every question, in its place"
        );
        assert_eq!(shown[1].text, "step 5 done");
        assert_eq!(
            shown[REPORTS_SHOWN].text,
            format!("step {} done", REPORTS_SHOWN + 4)
        );
        let few = &said[..4];
        assert_eq!(shown_reports(few).0.len(), 4);
        assert!(shown_reports(few).1.is_empty());
    }

    #[test]
    fn watching_switched_off_or_a_long_chain_goes_straight_to_the_person() {
        let mut o = Oversight {
            enabled: true,
            level: Level::Act,
            ..Oversight::default()
        };
        assert_eq!(straight_to_person(&o, 1, false), None);
        assert!(straight_to_person(&o, MAX_HOPS, false).is_some());
        o.wake.on_message = false;
        assert_eq!(
            straight_to_person(&o, 1, false),
            Some("Desk is set to give you sessions' questions.")
        );
        // Unbound, the boss takes them whatever the switch says; a chain
        // handed on too long still goes to the person.
        assert_eq!(straight_to_person(&o, 1, true), None);
        assert!(straight_to_person(&o, MAX_HOPS, true).is_some());
        o.wake.on_message = true;
        o.level = Level::Watch;
        assert_eq!(
            straight_to_person(&o, 1, false),
            Some("Desk only watches, so this came to you.")
        );
    }

    #[test]
    fn a_line_has_a_channel_of_its_own_that_is_no_desk_room() {
        let id = Uuid::new_v4();
        let channel = line_channel(id);
        assert_eq!(channel, format!("desk:line:{id}"));
        assert!(!workforce_common::channels::is_desk_conversation(&channel));
    }
}
