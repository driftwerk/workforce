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
use workforce_desk::line::{Asked, Escalation, LineKind, LineMessage};
use workforce_desk::model::DeskEvent;
use workforce_desk::open_item::{self, NewItem, OpenKind};
use workforce_desk::open_item_store::Opened;

use super::actions::{Action, Gate, Target};
use super::watcher::reported;
use super::{name_of, CrewHub, MAX_HOPS};
use crate::serve::AgentService;

/// What a line turn's prompt starts with: the person never typed it.
pub const LINE: &str = "[Desk line]";

/// How long a session's messages are gathered into one turn.
const GATHER: Duration = Duration::from_secs(3);

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
/// if it does.
pub(crate) fn straight_to_person(o: &Oversight, hop: i16) -> Option<&'static str> {
    if o.level == Level::Watch {
        return Some("Desk only watches, so this came to you.");
    }
    if !o.wake.on_message {
        return Some("Desk is set to give you sessions' questions.");
    }
    if hop >= MAX_HOPS {
        return Some("This was handed on too many times for Desk to take it further.");
    }
    None
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
                if let Err(e) = self.desk.store().held_question_put(q.id).await {
                    tracing::warn!(error = %format!("{e:#}"), "holding a session's question failed");
                }
                return;
            }
        }
        self.let_go(q.id).await;
        let e = q.escalation.clone().unwrap_or(Escalation {
            why: String::new(),
            options: Vec::new(),
            draft: None,
        });
        // One item, and one card, however often it is given to the person:
        // escalated again, it is updated and nothing new is posted.
        let mut item = NewItem::ask(
            service.agent_id.as_str(),
            OpenKind::Ask,
            q.from_session(),
            &format!(
                "{} asks: “{}”",
                q.from.name,
                crew::one_line(q.headline(), 400)
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
            &crew::one_line(q.headline(), 400),
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
        if flat(q.headline()) != flat(&asked) {
            detail.push(q.headline().trim().to_string());
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
    /// it (a work turn).
    pub(crate) async fn answer_line(
        &self,
        session: Uuid,
        messages: Vec<LineMessage>,
        ended: Vec<String>,
    ) {
        // The overseer of the session's scope answers it, and no other.
        let Some((service, o)) = self.overseer_of_session(session) else {
            let asks: Vec<LineMessage> = messages
                .into_iter()
                .filter(|m| m.kind == LineKind::Ask)
                .collect();
            self.all_to_person(session, &asks, "No overseer answers sessions now.")
                .await;
            return;
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
            return;
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
            return;
        }
        let mut messages = kept;
        let mut asks: Vec<LineMessage> = messages
            .iter()
            .filter(|m| m.kind == LineKind::Ask)
            .cloned()
            .collect();
        let hop = messages.iter().map(|m| m.hop).max().unwrap_or(0);
        if let Some(why) = straight_to_person(&o, hop) {
            self.all_to_person(session, &asks, why).await;
            if ended.is_empty() {
                return;
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
            return;
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
        let ran = self
            .run_turn(
                &service,
                &owner,
                &line_channel(session),
                &prompt,
                hop,
                None,
                None,
            )
            .await;
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
                            if let Err(e) = self.perform(&action, by).await {
                                tracing::info!(error = %e, "the overseer's reply didn't answer");
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
        // A turn that did nothing, and suggested nothing, is a decision
        // too: recorded here, never read from what it said.
        if let Ok((_, reply)) = &ran {
            if self.acted_mark(&boss, session) == mark {
                self.record_nothing(&boss, session, !ended.is_empty(), reply)
                    .await;
            }
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
            Ok(v) => {
                let said = serde_json::json!({
                    "name": v.label(),
                    "engine": v.engine.title(),
                    "folder": v.cwd,
                    "goal": v.goal,
                    "state": v.state,
                    "last_reply": v.last_reply.as_deref().map(|r| crew::clip(r, 1500)),
                });
                reported(&said.to_string(), "the session")
            }
            Err(_) => format!("session {session}"),
        };
        let lines: Vec<String> = messages
            .iter()
            .map(|m| {
                let kind = match m.kind {
                    LineKind::Ask => "asks",
                    LineKind::Tell | LineKind::Answer => "tells you",
                };
                format!("- message {} ({kind}): {}", m.id, m.full_text())
            })
            .collect();
        // Acting fully in its repository, shipping is the overseer's too.
        let (level, outward) = match self.ships_in(session, o) {
            true => (
                Level::Full,
                "deletes, spends, sends to people, changes scope or touches secrets (merging, pushing to the \
                 default branch, tagging, promoting and deploying this repository are yours at Act fully)",
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
            false => (outward, Self::level_line(level).to_string()),
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

    fn level_line(level: Level) -> &'static str {
        match level {
            Level::Watch => "You only watch.",
            Level::Suggest => "You suggest: an answer you give becomes a draft the person sends.",
            Level::Act => "You may answer within what the person allowed; anything else becomes a draft for them.",
            Level::Full => "You act fully: within what the person allowed, and in the repositories they named also on merging a pull request, pushing the default branch, tagging, releasing, and running the workflows they listed to promote and deploy. Anything else becomes a draft for them. Never secrets, registries, force pushes, remote deletes or your own limits.",
        }
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
    messages: Vec<LineMessage>,
    /// What its turn's end said, when the overseer runs it.
    ended: Vec<String>,
    /// The requests it waits on that its boss is given, each with its line
    /// for the work turn: read again as the turn starts, and only those
    /// still open go in.
    asks: Vec<(Uuid, String)>,
}

impl Gathered {
    fn new() -> Self {
        Self {
            since: Instant::now(),
            messages: Vec::new(),
            ended: Vec::new(),
            asks: Vec::new(),
        }
    }
}

/// Turns sessions' messages to the overseer, and the turn ends of the
/// sessions it runs, into its line turns: each session's gathered for
/// [`GATHER`], one turn at a time per session. It takes sessions on as
/// they appear, when it is set to.
pub(super) async fn line_loop(crew: Arc<CrewHub>, cancel: CancellationToken) {
    let (_, mut events) = crew.desk.subscribe();
    let mut gathering: HashMap<Uuid, Gathered> = HashMap::new();
    for (session, asks) in crew.take_up_open_asks().await {
        gathering.insert(
            session,
            Gathered {
                messages: asks,
                ..Gathered::new()
            },
        );
    }
    let mut running: HashMap<Uuid, tokio::task::JoinHandle<()>> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut taking_on: Option<tokio::task::JoinHandle<()>> = None;
    let mut looked: Option<Instant> = None;
    // The requests already given to a boss, by id and boss, while open.
    let mut offered: HashSet<(Uuid, String)> = HashSet::new();
    let mut asked_at: Option<Instant> = None;
    loop {
        let mut look = false;
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
                            .messages
                            .push(*message);
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
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return,
            },
            _ = tick.tick() => {}
        }
        // What the sessions a boss runs ask Desk to allow, at most twice a
        // second: each open request it can answer and hasn't been given.
        if asked_at.is_none_or(|at| at.elapsed() >= ASKS_EVERY) {
            asked_at = Some(Instant::now());
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
        running.retain(|_, h| !h.is_finished());
        let due: Vec<Uuid> = gathering
            .iter()
            .filter(|(s, g)| g.since.elapsed() >= GATHER && !running.contains_key(s))
            .map(|(s, _)| *s)
            .collect();
        for session in due {
            let Some(Gathered {
                messages,
                mut ended,
                mut asks,
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
                let boss = crew.boss_of(session);
                for (approval, _) in asks.drain(..) {
                    offered.retain(|(a, b)| *a != approval || Some(b) != boss.as_ref());
                }
                if messages.is_empty() {
                    continue;
                }
            }
            // Out of work turns for the hour: its turn's end waits for the
            // next, and what it asked goes on.
            if work && !crew.spend_work_turn(session) {
                gathering.insert(
                    session,
                    Gathered {
                        ended: std::mem::take(&mut ended),
                        asks: std::mem::take(&mut asks),
                        ..Gathered::new()
                    },
                );
                if messages.is_empty() {
                    continue;
                }
            }
            ended.extend(asks.iter().map(|(_, line)| line.clone()));
            let crew = Arc::clone(&crew);
            running.insert(
                session,
                tokio::spawn(async move {
                    crew.answer_line(session, messages, ended).await;
                    // What its turn was given and left open is the
                    // person's: never given to it again by itself.
                    crew.left_open(session, &asks).await;
                }),
            );
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
    fn watching_switched_off_or_a_long_chain_goes_straight_to_the_person() {
        let mut o = Oversight {
            enabled: true,
            level: Level::Act,
            ..Oversight::default()
        };
        assert_eq!(straight_to_person(&o, 1), None);
        assert!(straight_to_person(&o, MAX_HOPS).is_some());
        o.wake.on_message = false;
        assert_eq!(
            straight_to_person(&o, 1),
            Some("Desk is set to give you sessions' questions.")
        );
        o.wake.on_message = true;
        o.level = Level::Watch;
        assert_eq!(
            straight_to_person(&o, 1),
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
