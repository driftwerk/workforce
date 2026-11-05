//! Desk's lines ([`crate::line`]) as the desk runs them: sends checked by
//! the line's rules and stored, then delivered once each recipient can
//! take it. Claude Code sessions are handed their messages by Desk's
//! plugin (`/line/…/next`), others are typed into at their prompt; the
//! person and the overseer read theirs where Desk shows them.
//!
//! One entry point serves every engine's tools: [`Desk::line_call`],
//! behind the plugin's `/line/…/call` and the MCP endpoint `/mcp/…`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use tokio::sync::broadcast;
use uuid::Uuid;

use super::{Desk, DeskError};
use crate::crew::{self, By, Event, Kind, Party, PartyKind};
use crate::engine::{Deliver, Steer};
use crate::line::{
    self, Asked, Call, Delivery, LineKind, LineMessage, LineRun, Reach, Refusal, Send, Steering,
    Target, Wait, When,
};
use crate::line_store::NotAnswered;
use crate::model::{DeskEvent, Origin, SessionState, SessionView};

/// What the desk keeps of its lines in memory: lost with the hub, which is
/// the person-spoke state for the loop guard.
#[derive(Default)]
pub(crate) struct Lines {
    /// Each session's current run of turns.
    runs: HashMap<Uuid, LineRun>,
    /// When the person last spoke to each session (a prompt, the composer,
    /// an answer to its question).
    spoke: HashMap<Uuid, DateTime<Utc>>,
    /// Questions a tool call is waiting on the answer of, until when: the
    /// answer goes back as the call's result, not as a message.
    waiting: HashMap<Uuid, Instant>,
    /// Sessions with messages waiting for them.
    pending: HashSet<Uuid>,
    /// Sessions whose Desk plugin was heard from: it takes their messages.
    plugin: HashSet<Uuid>,
    /// The questions waiting on the person: asked of them, or given to
    /// them by the overseer. The Floor's `line:` needs.
    for_you: HashMap<Uuid, LineMessage>,
    /// The questions to the person their sessions went ahead without,
    /// until they are answered or set aside: the Floor's "went ahead".
    went_ahead: HashMap<Uuid, LineMessage>,
    /// When each session's line last had a message for the person, and
    /// last an exchange of its own: what its thread is unread by.
    pub(crate) seen: HashMap<Uuid, Seen>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Seen {
    pub for_you: Option<DateTime<Utc>>,
    pub new: Option<DateTime<Utc>>,
}

impl Seen {
    fn heard(&mut self, m: &LineMessage) {
        let at = Some(m.at);
        if m.to.kind == PartyKind::You {
            self.for_you = self.for_you.max(at);
        } else if m.from.kind != PartyKind::You {
            self.new = self.new.max(at);
        }
    }
}

/// Who sends on a line.
#[derive(Debug, Clone)]
pub enum Sender {
    /// A session, through its line tools.
    Session(Uuid),
    /// The person, through Desk.
    Person,
    /// The overseer, through its crew tools.
    Overseer(By),
}

#[derive(Debug, thiserror::Error)]
pub enum LineError {
    /// The line's rules refused it; the words are the sender's to read.
    #[error(transparent)]
    Refused(#[from] Refusal),
    #[error(transparent)]
    Desk(#[from] DeskError),
}

impl From<eyre::Report> for LineError {
    fn from(e: eyre::Report) -> Self {
        Self::Desk(DeskError::Internal(e))
    }
}

/// Answers the MCP requests a session's engine sends on its line: the
/// hub's MCP endpoint over [`line::TOOLS`], installed once with
/// [`Desk::serve_lines`].
#[async_trait::async_trait]
pub trait SessionMcp: std::marker::Send + Sync {
    /// The JSON-RPC answer to `body` from `caller`'s engine, or none for a
    /// notification.
    async fn answer(&self, caller: Uuid, body: &str) -> Option<String>;
}

/// Whether `m` is a question waiting on the person: asked of them, or
/// given to them by the overseer.
fn needs_you(m: &LineMessage) -> bool {
    m.kind == LineKind::Ask
        && match m.asked {
            Some(Asked::Escalated) => true,
            Some(Asked::Open) => m.to.kind == PartyKind::You,
            Some(Asked::Answered | Asked::Withdrawn | Asked::Lapsed | Asked::Dismissed) | None => {
                false
            }
        }
}

/// What a draft types: `text` as one bracketed paste, cleaned of control
/// characters (so it can't end the paste early or press Enter), and no
/// Enter after it.
fn draft_bytes(text: &str) -> Vec<u8> {
    let mut input = b"\x1b[200~".to_vec();
    input.extend_from_slice(line::typable(text).as_bytes());
    input.extend_from_slice(b"\x1b[201~");
    input
}

/// How long a message handed to Desk's plugin may wait for it to say the
/// session has it before it is queued again (the plugin died between
/// taking and submitting it).
const HANDED_FOR: chrono::Duration = chrono::Duration::minutes(2);
/// How often stale hand-offs and waiting messages are looked at.
const PUMP_EVERY: Duration = Duration::from_secs(30);
/// How many thread rows a page holds at most.
const THREAD_MAX: i64 = 200;

impl Desk {
    /// Which sessions may message each other.
    pub fn line_reach(&self) -> Reach {
        self.0.opts.line_reach
    }

    /// Installs what answers sessions' MCP requests on their lines.
    pub fn serve_lines(&self, mcp: Arc<dyn SessionMcp>) {
        if self.0.line_mcp.set(mcp).is_err() {
            tracing::warn!("desk lines were served twice; the first stays");
        }
    }

    /// The answer to an MCP request on `caller`'s line, when lines are served.
    pub(crate) async fn line_mcp(&self, caller: Uuid, body: &str) -> Option<Option<String>> {
        let mcp = self.0.line_mcp.get()?;
        Some(mcp.answer(caller, body).await)
    }

    /// Desk's plugin in `id` was heard from: its messages are handed to
    /// it from now on.
    pub(crate) fn plugin_heard_from(&self, id: Uuid) {
        let new = self.0.state.lock().lines.plugin.insert(id);
        if new {
            self.line_pump_later(id);
        }
    }

    fn plugin_heard(&self, id: Uuid) -> bool {
        self.0.state.lock().lines.plugin.contains(&id)
    }

    /// The session a session's hook token speaks for.
    pub(crate) fn line_caller(&self, id: Uuid, token: &str) -> Option<Uuid> {
        let st = self.0.state.lock();
        st.live
            .get(&id)
            .filter(|l| crate::holder::same_token(&l.token, token))
            .map(|_| id)
    }

    /// Every session, ended ones too, as the line judges them.
    fn line_sessions(&self) -> Vec<SessionView> {
        let st = self.0.state.lock();
        st.records.values().map(|r| Self::view_of(&st, r)).collect()
    }

    /// The overseer, while it answers sessions on their lines.
    /// The hub's crew answers sessions on their lines from now on, when it
    /// has an overseer that does.
    pub fn answer_lines_with(&self, crew: &Arc<dyn crate::crew::Crew>) {
        *self.0.line_crew.lock() = Some(Arc::downgrade(crew));
    }

    fn line_overseer(&self) -> Option<Party> {
        let crew = self
            .0
            .line_crew
            .lock()
            .as_ref()
            .and_then(std::sync::Weak::upgrade)?;
        crew.answers_lines()
            .then(|| crew.overseer())
            .flatten()
            .map(|o| Party::manager(&o.id, &o.name))
    }

    fn line_run(&self, id: Uuid) -> LineRun {
        self.0
            .state
            .lock()
            .lines
            .runs
            .get(&id)
            .copied()
            .unwrap_or_default()
    }

    /// The person spoke to `id`: its loop guard starts again.
    pub(super) fn person_spoke(&self, id: Uuid) {
        let mut st = self.0.state.lock();
        st.lines.runs.remove(&id);
        st.lines.spoke.insert(id, Utc::now());
    }

    /// When the person last spoke to `id` while the hub ran, by prompt,
    /// message or answer.
    pub fn person_spoke_at(&self, id: Uuid) -> Option<DateTime<Utc>> {
        self.0.state.lock().lines.spoke.get(&id).copied()
    }

    /// Whether the person has typed in `id`'s terminal since they last
    /// gave it a prompt: what they type there waits to be sent, and Desk
    /// types nothing in over it until their next prompt.
    fn typed_since_idle(&self, id: Uuid) -> bool {
        let st = self.0.state.lock();
        let typed = st.live.get(&id).and_then(|l| l.pty.typed_at());
        let spoke = st.lines.spoke.get(&id).copied();
        typed.is_some_and(|t| spoke.is_none_or(|s| t > s))
    }

    /// Rows put back in the queue from a hand-off, told of. A boss's tell to
    /// a session it no longer runs (let go, or passed to another boss since
    /// it was handed) is withdrawn instead: it never goes in. A session no
    /// boss ever ran keeps the overseer's nudges, and so does one while
    /// Desk can't yet read who runs it (its line holds them meanwhile).
    pub(super) async fn line_requeued(&self, back: Vec<LineMessage>) {
        for m in back {
            let let_go = m.from.kind == PartyKind::Manager
                && m.kind == LineKind::Tell
                && self.charges_loaded()
                && m.to_session()
                    .and_then(|s| self.charge_of(s))
                    .is_some_and(|c| !c.live() || c.boss != m.from.id);
            if !let_go {
                self.line_announce(&m);
                continue;
            }
            match self.0.store.line_withdraw_queued(m.id).await {
                Ok(Some(withdrawn)) => self.line_announce(&withdrawn),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(id = %m.id, error = %e, "couldn't withdraw a boss's message to a session it let go");
                    self.line_announce(&m);
                }
            }
        }
    }

    /// Records `m` in Activity and tells every window, and what its
    /// sessions' threads are unread by.
    fn line_announce(&self, m: &LineMessage) {
        {
            let mut st = self.0.state.lock();
            for id in [m.from_session(), m.to_session()].into_iter().flatten() {
                st.lines.seen.entry(id).or_default().heard(m);
            }
            if let Some(to) = m.to_session() {
                if m.waits() {
                    st.lines.pending.insert(to);
                }
            }
            match needs_you(m) {
                true => st.lines.for_you.insert(m.id, m.clone()),
                false => st.lines.for_you.remove(&m.id),
            };
            match m.kind == LineKind::Ask && m.asked == Some(Asked::Lapsed) {
                true => st.lines.went_ahead.insert(m.id, m.clone()),
                false => st.lines.went_ahead.remove(&m.id),
            };
        }
        let _ = self.0.events.send(DeskEvent::Line {
            message: Box::new(m.clone()),
            removed: false,
        });
    }

    fn line_noted(&self, m: &LineMessage) {
        let what = crew::one_line(m.headline(), 120);
        let (kind, text) = match m.kind {
            LineKind::Ask => (Kind::Asked, format!("asked {} “{what}”", m.to.name)),
            LineKind::Tell => (Kind::Told, format!("told {} “{what}”", m.to.name)),
            LineKind::Answer => (Kind::Answered, format!("answered {} “{what}”", m.to.name)),
        };
        self.note(Event::new(kind, m.from.clone(), Some(m.to.clone()), text).hop(m.hop));
    }

    /// Sends on a line, as `from`, under the line's rules; delivered once
    /// its recipient can take it.
    pub async fn line_send(&self, from: Sender, send: Send) -> Result<LineMessage, LineError> {
        let hop_send = send.clone();
        let (to, text, kind, when) = match send {
            Send::Reply { to, text } => return self.line_answer(to, from, text).await,
            Send::Ask { to, text, .. } => (to, text, LineKind::Ask, When::Queue),
            Send::Tell { to, text, when } => (to, text, LineKind::Tell, when),
        };
        let text = line::typable(&text).trim().to_string();
        if text.is_empty() {
            return Err(Refusal::Missing("what to say").into());
        }
        let sessions = self.line_sessions();
        let overseer = self.line_overseer();
        let find = |id: Uuid| sessions.iter().find(|s| s.id == id);
        let (party, hop, group, recipient) = match &from {
            Sender::Session(id) => {
                let me = find(*id).ok_or(DeskError::NotFound)?;
                let run = self.line_run(*id);
                let hop = line::hop_of(&hop_send, run.cause, None);
                // Past its hops a chain can still ask the person: that is
                // what the refusal tells it to do.
                let checked = match to {
                    Target::Person => hop.min(crate::rooms::MAX_HOPS),
                    Target::Overseer | Target::Session(_) => hop,
                };
                line::guard(checked, run.sent)?;
                let recipient =
                    line::reach(me, &to, &sessions, overseer.as_ref(), self.line_reach())?;
                (
                    Party::session(me),
                    hop,
                    me.group.as_ref().map(|g| g.id.clone()),
                    recipient,
                )
            }
            Sender::Person | Sender::Overseer(_) => {
                let (party, hop) = match &from {
                    Sender::Overseer(by) => (by.party.clone(), by.hop.max(1)),
                    _ => (Party::you(), 0),
                };
                line::guard(hop, 0)?;
                let recipient = match to {
                    Target::Session(id) => {
                        let them = find(id).ok_or(DeskError::NotFound)?;
                        if them.state == SessionState::Closed {
                            return Err(Refusal::Gone(them.label().into()).into());
                        }
                        if them.origin == Origin::External || them.caps.line == Deliver::None {
                            return Err(Refusal::Unreachable(
                                them.label().into(),
                                "it has no line from Desk",
                            )
                            .into());
                        }
                        Party::session(them)
                    }
                    Target::Person if party.kind == PartyKind::You => {
                        return Err(Refusal::Unreachable("You".into(), "that is you").into())
                    }
                    Target::Person => Party::you(),
                    Target::Overseer => match overseer {
                        Some(o) if o.id != party.id => o,
                        _ => {
                            return Err(Refusal::Unreachable(
                                "Desk".into(),
                                "message a session or the person",
                            )
                            .into())
                        }
                    },
                };
                (party, hop, None, recipient)
            }
        };
        let mut m = LineMessage::new(party, recipient, kind, text, hop);
        m.group = group;
        if let Send::Ask { chose, parts, .. } = &hop_send {
            let clean = |v: &Option<String>| {
                v.as_deref()
                    .map(|c| line::typable(c).trim().to_string())
                    .filter(|c| !c.is_empty())
            };
            m.chose = clean(chose);
            m.about = clean(&parts.about);
            m.why = clean(&parts.why);
            m.details = clean(&parts.details);
            m.shape();
        }
        let recipient = m.to_session().and_then(find).cloned();
        let steering = match &recipient {
            Some(v) => line::steering(
                when,
                matches!(from, Sender::Person),
                v.caps.steer,
                v.state,
                v.label(),
            )?,
            None => Steering::Queue,
        };
        match &recipient {
            // Shown to the person, or heard by the overseer: there now.
            None => {
                m.delivery = Delivery::Delivered;
                m.delivered_at = Some(m.at);
            }
            Some(v) if !v.live => m.delivery = Delivery::Held,
            Some(_) => {}
        }
        self.0.store.put_line(&m).await?;
        // A session's question is waited on from the moment it is told of:
        // whoever takes it up (the overseer giving it to the person) finds
        // its call still waiting, not gone ahead.
        if let (Sender::Session(_), Send::Ask { wait, .. }) = (&from, &hop_send) {
            self.0
                .state
                .lock()
                .lines
                .waiting
                .insert(m.id, Instant::now() + *wait);
        }
        match &from {
            Sender::Session(id) => {
                self.0.state.lock().lines.runs.entry(*id).or_default().sent += 1;
            }
            Sender::Person => {
                if let Some(v) = &recipient {
                    self.person_spoke(v.id);
                }
            }
            Sender::Overseer(_) => {}
        }
        self.line_noted(&m);
        self.line_announce(&m);
        if let Some(v) = recipient {
            match steering {
                Steering::Queue => self.line_pump_later(v.id),
                Steering::PasteNow | Steering::TurnSteer => {
                    self.line_steer(&mut m, steering).await?
                }
            }
        }
        Ok(m)
    }

    /// Says the person's `m` into its recipient's running turn now.
    async fn line_steer(&self, m: &mut LineMessage, how: Steering) -> Result<(), LineError> {
        let Some(id) = m.to_session() else {
            return Ok(());
        };
        match how {
            Steering::PasteNow => {
                self.type_prompt(id, &m.text, Some(&By::you()))?;
            }
            Steering::TurnSteer => {
                // Said before it is sent: a refusal can come back at once,
                // and must find it said to put it back in the queue.
                let Some(taken) = self.0.store.line_take(m.id, Delivery::Delivered).await? else {
                    return Ok(());
                };
                *m = taken;
                self.line_announce(m);
                let sent = self
                    .0
                    .state
                    .lock()
                    .live
                    .get(&id)
                    .and_then(|l| l._codex.as_ref())
                    .is_some_and(|c| c.line_steers.send((m.id, m.text.clone())).is_ok());
                if !sent {
                    // No observer to steer through: it goes in at the
                    // turn's end.
                    self.line_steer_refused(id, m.id).await;
                }
                return Ok(());
            }
            Steering::Queue => return Ok(()),
        }
        if let Some(taken) = self.0.store.line_take(m.id, Delivery::Delivered).await? {
            *m = taken;
            self.line_announce(m);
        }
        Ok(())
    }

    /// Codex didn't take the line message `row` into its turn: it goes in
    /// at the next idle instead. Each refusal names its own message, so
    /// however many were said, each comes back and only it.
    pub(super) async fn line_steer_refused(&self, id: Uuid, row: Uuid) {
        let back =
            async {
                // Only one said to this session and not yet back: a message
                // that was taken some other way stays taken.
                let Some(mut m) =
                    self.0.store.line_message(row).await?.filter(|m| {
                        m.delivery == Delivery::Delivered && m.to_session() == Some(id)
                    })
                else {
                    return Ok(None);
                };
                m.delivery = Delivery::Queued;
                m.delivered_at = None;
                m.updated_at = Utc::now();
                self.0.store.put_line(&m).await?;
                Ok::<_, eyre::Report>(Some(m))
            };
        match back.await {
            Ok(None) => {}
            Ok(Some(m)) => {
                self.line_announce(&m);
                self.line_pump_later(id);
            }
            Err(e) => {
                tracing::warn!(session = %id, error = %e, "couldn't queue a refused steer again")
            }
        }
    }

    /// Answers the question `ask` as `by`: the first answer stands. The
    /// person may answer any question asked on Desk.
    pub async fn line_answer(
        &self,
        ask: Uuid,
        by: Sender,
        text: String,
    ) -> Result<LineMessage, LineError> {
        let text = line::typable(&text).trim().to_string();
        if text.is_empty() {
            return Err(Refusal::Missing("your answer").into());
        }
        let question = self
            .0
            .store
            .line_message(ask)
            .await?
            .filter(|q| q.kind == LineKind::Ask)
            .ok_or(Refusal::NoSuchAsk(ask))?;
        if question.asked == Some(Asked::Dismissed) {
            return Err(Refusal::SetAside.into());
        }
        let party = match &by {
            Sender::Session(id) if question.to_session() == Some(*id) => {
                Party::session(&self.view(*id)?)
            }
            Sender::Overseer(b)
                if question.to.kind == PartyKind::Manager && question.to.id == b.party.id =>
            {
                if let Some(asker) = question.from_session() {
                    self.boss_holds(asker, Some(b))?;
                }
                b.party.clone()
            }
            Sender::Person => Party::you(),
            Sender::Session(_) | Sender::Overseer(_) => return Err(Refusal::NoSuchAsk(ask).into()),
        };
        let mut a = LineMessage::new(
            party,
            question.from.clone(),
            LineKind::Answer,
            text,
            question.hop,
        );
        a.in_reply_to = Some(question.id);
        let asker = question.from_session().and_then(|id| self.view(id).ok());
        match &asker {
            Some(v) if !v.live => a.delivery = Delivery::Held,
            Some(_) => {}
            None => {
                a.delivery = Delivery::Delivered;
                a.delivered_at = Some(a.at);
            }
        }
        let asked = match self.0.store.line_answer(question.id, &a).await? {
            Ok(asked) => asked,
            Err(NotAnswered::Already) => return Err(Refusal::Answered.into()),
            Err(NotAnswered::NoSuchAsk) => return Err(Refusal::NoSuchAsk(ask).into()),
        };
        match (&by, &asker) {
            (Sender::Person, Some(v)) => self.person_spoke(v.id),
            (Sender::Session(id), _) => {
                self.0.state.lock().lines.runs.entry(*id).or_default().sent += 1;
            }
            // The overseer's answers count against the session that asked:
            // its budget holds the exchange, not the overseer's.
            (Sender::Overseer(_), Some(v)) => {
                self.0.state.lock().lines.runs.entry(v.id).or_default().sent += 1;
            }
            _ => {}
        }
        self.line_announce(&asked);
        self.line_noted(&a);
        self.line_announce(&a);
        if let Some(v) = asker {
            self.line_pump_later(v.id);
        }
        Ok(a)
    }

    /// Waits up to `wait` for the answer to `ask` a tool call waits on,
    /// and takes it as the call's result. The question stays the call's
    /// until `until`, so its answer isn't also sent as a message.
    async fn line_wait_answer(
        &self,
        ask: Uuid,
        wait: Duration,
        until: Instant,
    ) -> Result<Option<LineMessage>, LineError> {
        self.0.state.lock().lines.waiting.insert(ask, until);
        let mut rx = self.0.events.subscribe();
        // Set aside between the plugin's polls: its event has gone by.
        if self.set_aside(ask).await {
            return Ok(None);
        }
        let deadline = tokio::time::Instant::now() + wait;
        let found = loop {
            if let Some(a) = self.0.store.line_reply_to(ask).await? {
                break Some(a);
            }
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Err(_) => break None,
                Ok(Ok(DeskEvent::Line { message, .. }))
                    if message.in_reply_to == Some(ask) && message.kind == LineKind::Answer => {}
                // The person set it aside: no answer is coming.
                Ok(Ok(DeskEvent::Line { message, .. }))
                    if message.id == ask && message.asked == Some(Asked::Dismissed) =>
                {
                    break None
                }
                Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) => break None,
            }
        };
        let Some(answer) = found else {
            return Ok(None);
        };
        self.0.state.lock().lines.waiting.remove(&ask);
        if answer.waits() {
            if let Some(taken) = self
                .0
                .store
                .line_take(answer.id, Delivery::Delivered)
                .await?
            {
                if let Some(asker) = taken.to_session() {
                    self.0
                        .state
                        .lock()
                        .lines
                        .runs
                        .entry(asker)
                        .or_default()
                        .delivered(taken.hop);
                }
                self.line_announce(&taken);
                return Ok(Some(taken));
            }
        }
        Ok(Some(answer))
    }

    /// What an `ask` call answers: the answer; that the person set it
    /// aside; or, once it waited all it would, that the session goes on
    /// without one, which may still come later.
    fn ask_result(m: &LineMessage, answer: Option<&LineMessage>, more: bool) -> Value {
        match answer {
            Some(a) => json!({ "answer": a.text, "from": a.from.name }),
            None if m.asked == Some(Asked::Dismissed) => json!({
                "pending": "The person dismissed this question; carry on with your best judgement and say what you chose.",
                "id": m.id,
                "more": false,
            }),
            None if !more => {
                let routed = (m.to.kind == PartyKind::You).then_some(
                    "Desk has no overseer answering sessions, so your question went to the person.",
                );
                json!({
                    "pending": format!(
                        "No answer from {} in time. Go on with your own judgement; if they answer later, it reaches you as a message quoting your question.",
                        m.to.name
                    ),
                    "id": m.id,
                    "note": routed,
                    "more": false,
                })
            }
            None => {
                let routed = (m.to.kind == PartyKind::You).then_some(
                    "Desk has no overseer answering sessions, so your question went to the person.",
                );
                json!({
                    "pending": format!(
                        "No answer from {} yet; it reaches you as a message on Desk's line when it comes.",
                        m.to.name
                    ),
                    "id": m.id,
                    "note": routed,
                    "more": more,
                })
            }
        }
    }

    /// Runs one of the line's tools for the session `caller`: the result a
    /// tool returns, or the words of its error. `step` is how long one
    /// request may wait for an answer (Desk's plugin keeps asking, through
    /// [`Self::line_await`], while the result says `more`).
    pub async fn line_call(
        &self,
        caller: Uuid,
        name: &str,
        input: &Value,
        step: Option<Duration>,
    ) -> Result<Value, String> {
        let call = line::call_of(name, input)?;
        let sessions = self.line_sessions();
        let say = |e: LineError| match e {
            LineError::Refused(r) => r.to_string(),
            LineError::Desk(DeskError::NotFound) => "Desk doesn't know this session.".into(),
            LineError::Desk(e) => {
                tracing::warn!(session = %caller, error = %e, "a line tool failed");
                "Desk can't be reached right now; carry on without it.".into()
            }
        };
        match call {
            Call::Ask {
                to,
                text,
                wait,
                chose,
                parts,
            } => {
                let to = line::target_of(&to, &sessions).map_err(|r| r.to_string())?;
                let m = self
                    .line_send(
                        Sender::Session(caller),
                        Send::Ask {
                            to,
                            text,
                            wait,
                            chose,
                            parts,
                        },
                    )
                    .await
                    .map_err(say)?;
                let now = wait.min(step.unwrap_or(wait));
                let answer = self
                    .line_wait_answer(m.id, now, Instant::now() + wait)
                    .await
                    .map_err(say)?;
                let more = answer.is_none() && now < wait && !self.set_aside(m.id).await;
                let m = match answer.is_none() && !more {
                    true => self.line_waited_out(m).await,
                    false => m,
                };
                Ok(Self::ask_result(&m, answer.as_ref(), more))
            }
            Call::Tell { to, text } => {
                let to = line::target_of(&to, &sessions).map_err(|r| r.to_string())?;
                let m = self
                    .line_send(
                        Sender::Session(caller),
                        Send::Tell {
                            to,
                            text,
                            when: When::Queue,
                        },
                    )
                    .await
                    .map_err(say)?;
                let note = (m.delivery == Delivery::Held)
                    .then(|| format!("{} is stopped; it gets this when it runs again.", m.to.name));
                Ok(json!({ "id": m.id, "delivery": m.delivery, "note": note }))
            }
            Call::Reply { to, text } => {
                let a = self
                    .line_answer(to, Sender::Session(caller), text)
                    .await
                    .map_err(say)?;
                Ok(json!({ "id": a.id }))
            }
            Call::Ticket(call) => self.ticket_call(caller, call).await,
            Call::Inbox => {
                let taken = self.line_take_inbox(caller).await.map_err(say)?;
                Ok(json!({ "messages": taken }))
            }
            Call::Peers => {
                let me = sessions
                    .iter()
                    .find(|s| s.id == caller)
                    .ok_or_else(|| "Desk doesn't know this session.".to_string())?;
                let peers: Vec<Value> = line::peers(me, &sessions, self.line_reach())
                    .into_iter()
                    .map(|s| {
                        json!({
                            "id": s.id,
                            "name": s.label(),
                            "engine": s.engine.title(),
                            "folder": s.cwd,
                            "state": s.state,
                        })
                    })
                    .collect();
                let desk = match self.line_overseer() {
                    Some(o) => format!("{}, the person's overseer: to \"desk\"", o.name),
                    None => "No overseer answers here: \"desk\" reaches the person".into(),
                };
                Ok(json!({
                    "desk": desk,
                    "you": "The person: to \"you\"",
                    "sessions": peers,
                    "reach": self.line_reach(),
                }))
            }
        }
    }

    /// Keeps waiting, for Desk's plugin, on the answer to `ask` that
    /// `caller` asked.
    pub async fn line_await(
        &self,
        caller: Uuid,
        ask: Uuid,
        step: Duration,
    ) -> Result<Value, String> {
        let m = self
            .0
            .store
            .line_message(ask)
            .await
            .ok()
            .flatten()
            .filter(|m| m.from_session() == Some(caller) && m.kind == LineKind::Ask)
            .ok_or_else(|| Refusal::NoSuchAsk(ask).to_string())?;
        let until = self
            .0
            .state
            .lock()
            .lines
            .waiting
            .get(&ask)
            .copied()
            .unwrap_or_else(Instant::now);
        let left = until.saturating_duration_since(Instant::now());
        let now = left.min(step);
        let answer = self
            .line_wait_answer(ask, now, until)
            .await
            .map_err(|e| e.to_string())?;
        let more = answer.is_none() && now < left && !self.set_aside(ask).await;
        let m = match answer.is_none() && !more {
            true => self.line_waited_out(m).await,
            false => m,
        };
        Ok(Self::ask_result(&m, answer.as_ref(), more))
    }

    /// No tool call waits on `ask` any more: the session goes on without
    /// an answer, and one given later comes as a message. A question that
    /// waited on the person no longer does. The ask as it now is.
    async fn line_waited_out(&self, ask: LineMessage) -> LineMessage {
        self.0.state.lock().lines.waiting.remove(&ask.id);
        self.line_went_ahead(ask.id).await.unwrap_or(ask)
    }

    /// `ask` went ahead without the person's answer, when it waited on
    /// them; told of. The row as it now is, when it moved.
    async fn line_went_ahead(&self, ask: Uuid) -> Option<LineMessage> {
        match self.0.store.line_went_ahead(ask, true).await {
            Ok(Some(m)) => {
                self.line_announce(&m);
                Some(m)
            }
            Ok(None) => self.0.store.line_message(ask).await.ok().flatten(),
            Err(e) => {
                tracing::warn!(%ask, error = %e, "couldn't say a question went ahead without its answer");
                None
            }
        }
    }

    /// Questions no `ask` call waits on any more stop waiting on the
    /// person: a wait that ran out with no one asking again (the turn was
    /// stopped, the engine quit), and, after a restart, any older than the
    /// longest wait.
    pub(super) async fn line_sweep_waits(&self) {
        let now = Instant::now();
        let expired: Vec<Uuid> = {
            let mut st = self.0.state.lock();
            let gone: Vec<Uuid> = st
                .lines
                .waiting
                .iter()
                .filter(|(_, until)| **until <= now)
                .map(|(id, _)| *id)
                .collect();
            for id in &gone {
                st.lines.waiting.remove(id);
            }
            gone
        };
        for ask in expired {
            self.line_went_ahead(ask).await;
        }
        let before = Utc::now()
            - chrono::Duration::from_std(line::ASK_WAIT_MAX + PUMP_EVERY).unwrap_or_default();
        let stale = match self.0.store.line_stale_asks(before).await {
            Ok(stale) => stale,
            Err(e) => {
                tracing::warn!(error = %e, "couldn't read the questions no one waits on");
                return;
            }
        };
        for ask in stale {
            if self.0.state.lock().lines.waiting.contains_key(&ask.id) {
                continue;
            }
            self.line_went_ahead(ask.id).await;
        }
    }

    /// The person sets the question `ask` aside unanswered: it leaves what
    /// needs them, and nothing goes to the session (an `ask` call still
    /// waiting on it is told to go on).
    pub async fn line_dismiss(&self, ask: Uuid) -> Result<LineMessage, LineError> {
        match self.0.store.line_dismiss(ask).await? {
            Some(m) => {
                self.line_announce(&m);
                Ok(m)
            }
            None => match self.0.store.line_message(ask).await? {
                Some(q) if q.kind == LineKind::Ask => Err(match q.asked {
                    Some(Asked::Answered) => Refusal::Answered,
                    Some(Asked::Dismissed) => Refusal::SetAside,
                    _ => Refusal::NotWaitingOnYou,
                }
                .into()),
                _ => Err(Refusal::NoSuchAsk(ask).into()),
            },
        }
    }

    /// Whether the person set the question `ask` aside: no answer comes.
    async fn set_aside(&self, ask: Uuid) -> bool {
        self.0
            .store
            .line_message(ask)
            .await
            .ok()
            .flatten()
            .is_some_and(|q| q.asked == Some(Asked::Dismissed))
    }

    /// Whether a tool call still waits on the question `ask`.
    fn waited_on_ask(&self, ask: Uuid) -> bool {
        self.0
            .state
            .lock()
            .lines
            .waiting
            .get(&ask)
            .is_some_and(|until| *until > Instant::now())
    }

    /// Whether the answer `m` is one a tool call still waits on.
    fn waited_on(&self, m: &LineMessage) -> bool {
        let st = self.0.state.lock();
        m.in_reply_to
            .and_then(|a| st.lines.waiting.get(&a))
            .is_some_and(|until| *until > Instant::now())
    }

    /// Takes everything waiting for `caller`, as its `inbox` tool reads it.
    async fn line_take_inbox(&self, caller: Uuid) -> Result<Vec<Value>, LineError> {
        let view = self.view(caller)?;
        let mut out = Vec::new();
        for m in self.0.store.line_inbox(&caller.to_string()).await? {
            // What waits for the person (a breaker's hold, their answer,
            // their wheel) waits here too.
            if m.delivery == Delivery::Handed
                || self.waited_on(&m)
                || self.line_gate(&view, &m).is_some()
            {
                continue;
            }
            if let Some(taken) = self.0.store.line_take(m.id, Delivery::Delivered).await? {
                self.line_delivered_into(caller, &taken);
                out.push(json!({
                    "id": taken.id,
                    "from": taken.from.name,
                    "kind": taken.kind,
                    "text": self.rendered(&taken).await,
                }));
            }
        }
        Ok(out)
    }

    fn line_delivered_into(&self, id: Uuid, m: &LineMessage) {
        self.0
            .state
            .lock()
            .lines
            .runs
            .entry(id)
            .or_default()
            .delivered(m.hop);
        self.line_announce(m);
    }

    /// What `m` reads as to its session.
    async fn rendered(&self, m: &LineMessage) -> String {
        let question = match m.in_reply_to {
            Some(ask) => self.0.store.line_message(ask).await.ok().flatten(),
            None => None,
        };
        line::render(m, question.as_ref().map(|q| q.headline()))
    }

    /// The next message for Desk's plugin to submit into `caller`, handed
    /// to it, waiting up to `wait` for one. One at a time, in order.
    pub async fn line_next(&self, caller: Uuid, wait: Duration) -> Option<Value> {
        self.plugin_heard_from(caller);
        let mut rx = self.0.events.subscribe();
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            match self.line_next_now(caller).await {
                Ok(Some(next)) => return Some(next),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(session = %caller, error = %e, "couldn't read a session's line");
                    return None;
                }
            }
            loop {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Err(_) | Ok(Err(broadcast::error::RecvError::Closed)) => return None,
                    Ok(Ok(DeskEvent::Line { message, .. }))
                        if message.to_session() == Some(caller) =>
                    {
                        break
                    }
                    Ok(Err(broadcast::error::RecvError::Lagged(_))) => break,
                    Ok(Ok(_)) => {}
                }
            }
        }
    }

    /// Why `m` mustn't go into `view` now, whatever its engine could take:
    /// a session Desk's breaker paused, or one asking the person something,
    /// takes only the person's own messages; one the person drives, none
    /// of its boss's.
    fn line_gate(&self, view: &SessionView, m: &LineMessage) -> Option<Wait> {
        if m.from.kind == PartyKind::You {
            return None;
        }
        if self.breaker_refuses_prompt(view.id, None).is_err() {
            return Some(Wait::Paused);
        }
        if m.from.kind == PartyKind::Manager
            && m.kind == LineKind::Tell
            && self.person_driving(view.id)
        {
            return Some(Wait::PersonDriving);
        }
        (view.state == SessionState::Asking).then_some(Wait::Asking)
    }

    /// Who a message is typed in for: the person's own, as theirs.
    fn typed_by(m: &LineMessage) -> Option<By> {
        (m.from.kind == PartyKind::You).then(By::you)
    }

    async fn line_next_now(&self, caller: Uuid) -> Result<Option<Value>, LineError> {
        let view = self.view(caller)?;
        if !view.live {
            return Ok(None);
        }
        let inbox = self.0.store.line_inbox(&caller.to_string()).await?;
        // One on its way at a time: the order they were sent in holds.
        if inbox.iter().any(|m| m.delivery == Delivery::Handed) {
            return Ok(None);
        }
        for m in inbox {
            if self.waited_on(&m) || self.line_gate(&view, &m).is_some() {
                continue;
            }
            if let Some(taken) = self.0.store.line_take(m.id, Delivery::Handed).await? {
                self.line_announce(&taken);
                return Ok(Some(
                    json!({ "id": taken.id, "text": self.rendered(&taken).await }),
                ));
            }
        }
        Ok(None)
    }

    /// Desk's plugin says what became of a message it was handed: in the
    /// session (`ok`), or dropped (the person interrupted), to go again.
    pub async fn line_ack(&self, caller: Uuid, id: Uuid, ok: bool) -> Result<(), LineError> {
        let m = self
            .0
            .store
            .line_message(id)
            .await?
            .filter(|m| m.to_session() == Some(caller))
            .ok_or(DeskError::NotFound)?;
        if ok {
            if let Some(m) = self.0.store.line_ack(m.id).await? {
                self.line_delivered_into(caller, &m);
            }
        } else {
            let back = self.0.store.line_requeue(Some(m.id), Utc::now()).await?;
            self.line_requeued(back).await;
        }
        Ok(())
    }

    /// A line message's marker came back in a prompt `id` started: it is
    /// in, at its hop.
    pub(super) async fn line_heard(&self, id: Uuid, mark: &str) {
        match self.0.store.line_marked(&id.to_string(), mark).await {
            Ok(Some(m)) => {
                self.0
                    .state
                    .lock()
                    .lines
                    .runs
                    .entry(id)
                    .or_default()
                    .delivered(m.hop);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(session = %id, error = %e, "couldn't read a line message back")
            }
        }
    }

    /// Delivers what waits for `id`, soon.
    fn line_pump_later(&self, id: Uuid) {
        let desk = self.clone();
        tokio::spawn(async move { desk.line_pump(id).await });
    }

    /// Delivers what waits for `id` that it can take now, in order: one
    /// typed in per idle, the rest held or queued with why.
    pub(super) async fn line_pump(&self, id: Uuid) {
        let _one = self.0.line_pump.lock().await;
        if let Err(e) = self.line_pump_now(id).await {
            tracing::warn!(session = %id, error = %e, "couldn't deliver a session's line messages");
        }
    }

    async fn line_pump_now(&self, id: Uuid) -> Result<(), LineError> {
        let Ok(view) = self.view(id) else {
            self.0.state.lock().lines.pending.remove(&id);
            return Ok(());
        };
        let inbox = self.0.store.line_inbox(&id.to_string()).await?;
        if inbox.is_empty() {
            self.0.state.lock().lines.pending.remove(&id);
            return Ok(());
        }
        let typed = self.typed_since_idle(id);
        let plugin = self.plugin_heard(id);
        let mut typed_in = false;
        for mut m in inbox {
            if m.delivery == Delivery::Handed || self.waited_on(&m) {
                continue;
            }
            let now = line::deliverable(&view, typed, plugin);
            let held = matches!(now, Err(Wait::Stopped));
            let wanted = if held {
                Delivery::Held
            } else {
                Delivery::Queued
            };
            if m.delivery != wanted {
                m.delivery = wanted;
                m.updated_at = Utc::now();
                self.0.store.put_line(&m).await?;
                self.line_announce(&m);
            }
            if now != Ok(Deliver::Paste) || typed_in || self.line_gate(&view, &m).is_some() {
                continue;
            }
            let Some(taken) = self.0.store.line_take(m.id, Delivery::Delivered).await? else {
                continue;
            };
            let text = self.rendered(&taken).await;
            if let Err(e) = self.type_prompt(id, &text, Self::typed_by(&taken).as_ref()) {
                // Not typed in: it waits on.
                let mut back = taken;
                back.delivery = Delivery::Queued;
                back.delivered_at = None;
                self.0.store.put_line(&back).await?;
                self.line_announce(&back);
                tracing::debug!(session = %id, error = %e, "a line message couldn't be typed in");
                continue;
            }
            self.line_delivered_into(id, &taken);
            typed_in = true;
        }
        Ok(())
    }

    /// The pump's own loop: a session coming to its prompt (or running
    /// again) gets what waits for it, and a hand-off the plugin never
    /// finished goes again.
    pub(super) async fn line_loop(self) {
        let mut rx = self.0.events.subscribe();
        let mut tick = tokio::time::interval(PUMP_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        match self.0.store.line_waiting().await {
            Ok(rows) => {
                let mut st = self.0.state.lock();
                st.lines
                    .pending
                    .extend(rows.iter().filter_map(LineMessage::to_session));
            }
            Err(e) => tracing::warn!(error = %e, "couldn't read what waits on Desk's lines"),
        }
        // After a restart no call waits on what was asked before it.
        self.line_sweep_waits().await;
        match self.0.store.line_open_asks(None).await {
            Ok(asks) => {
                let mut st = self.0.state.lock();
                for ask in asks {
                    if needs_you(&ask) {
                        st.lines.for_you.insert(ask.id, ask);
                    } else if ask.asked == Some(Asked::Lapsed) {
                        st.lines.went_ahead.insert(ask.id, ask);
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "couldn't read the questions waiting on you"),
        }
        match self.0.store.line_latest().await {
            Ok(latest) => {
                let mut st = self.0.state.lock();
                for (id, for_you, new) in latest {
                    st.lines.seen.insert(id, Seen { for_you, new });
                }
            }
            Err(e) => tracing::warn!(error = %e, "couldn't read when Desk's lines were last used"),
        }
        loop {
            tokio::select! {
                _ = self.0.cancel.cancelled() => return,
                _ = tick.tick() => {
                    match self.0.store.line_requeue(None, Utc::now() - HANDED_FOR).await {
                        Ok(back) => self.line_requeued(back).await,
                        Err(e) => tracing::warn!(error = %e, "couldn't requeue stale line hand-offs"),
                    }
                    self.line_pump_pending().await;
                    self.line_sweep_waits().await;
                }
                ev = rx.recv() => match ev {
                    Ok(DeskEvent::Session { session }) => {
                        let waits = self.0.state.lock().lines.pending.contains(&session.id);
                        if waits && matches!(
                            session.state,
                            SessionState::Idle | SessionState::Done | SessionState::Asking
                                | SessionState::Starting | SessionState::Interrupted
                                | SessionState::Stopped
                        ) {
                            self.line_pump(session.id).await;
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => self.line_pump_pending().await,
                    Err(broadcast::error::RecvError::Closed) => return,
                },
            }
        }
    }

    async fn line_pump_pending(&self) {
        let pending: Vec<Uuid> = self.0.state.lock().lines.pending.iter().copied().collect();
        for id in pending {
            self.line_pump(id).await;
        }
    }

    /// `id` is being forgotten: questions to and from it are withdrawn,
    /// and whoever asked it is told.
    pub(super) async fn line_forgotten(&self, gone: &SessionView) {
        let id = gone.id;
        let asks = match self.0.store.line_open_asks(None).await {
            Ok(asks) => asks,
            Err(e) => {
                tracing::warn!(session = %id, error = %e, "couldn't withdraw a forgotten session's questions");
                return;
            }
        };
        for mut ask in asks {
            let to_it = ask.to_session() == Some(id);
            if !to_it && ask.from_session() != Some(id) {
                continue;
            }
            ask.asked = Some(Asked::Withdrawn);
            ask.updated_at = Utc::now();
            if let Err(e) = self.0.store.put_line(&ask).await {
                tracing::warn!(error = %e, "couldn't withdraw a question");
                continue;
            }
            self.line_announce(&ask);
            let Some(asker) = ask.from_session().filter(|_| to_it) else {
                continue;
            };
            let mut note = LineMessage::new(
                Party::session(gone),
                ask.from.clone(),
                LineKind::Tell,
                format!(
                    "{} has ended, so your question “{}” to it is withdrawn.",
                    gone.label(),
                    crew::one_line(ask.headline(), 120)
                ),
                ask.hop,
            );
            if !self.view(asker).is_ok_and(|v| v.live) {
                note.delivery = Delivery::Held;
            }
            if self.0.store.put_line(&note).await.is_ok() {
                self.line_announce(&note);
                self.line_pump_later(asker);
            }
        }
        let mut st = self.0.state.lock();
        st.lines.runs.remove(&id);
        st.lines.spoke.remove(&id);
        st.lines.pending.remove(&id);
        st.lines.plugin.remove(&id);
        st.lines
            .for_you
            .retain(|_, m| m.from_session() != Some(id) && m.to_session() != Some(id));
        st.lines
            .went_ahead
            .retain(|_, m| m.from_session() != Some(id) && m.to_session() != Some(id));
        st.lines.seen.remove(&id);
    }

    /// A session's thread, oldest first, the page before `before`; each
    /// waiting message says why it waits.
    pub async fn line_thread(
        &self,
        id: Uuid,
        before: Option<DateTime<Utc>>,
        limit: Option<i64>,
    ) -> Result<Vec<LineMessage>, DeskError> {
        let view = self.view(id)?;
        let typed = self.typed_since_idle(id);
        let plugin = self.plugin_heard(id);
        let mut rows = self
            .0
            .store
            .line_thread(id, before, limit.unwrap_or(THREAD_MAX).clamp(1, THREAD_MAX))
            .await?;
        rows.reverse();
        for m in rows.iter_mut().filter(|m| m.to_session() == Some(id)) {
            m.waiting = match m.delivery {
                Delivery::Held => Some(Wait::Stopped),
                Delivery::Queued => match line::deliverable(&view, typed, plugin) {
                    Err(why) => Some(why),
                    Ok(_) if self.line_gate(&view, m).is_some() => self.line_gate(&view, m),
                    Ok(Deliver::Submit)
                        if matches!(view.state, SessionState::Working | SessionState::Blocked) =>
                    {
                        Some(Wait::Working)
                    }
                    Ok(_) => None,
                },
                Delivery::Handed
                | Delivery::Delivered
                | Delivery::Refused
                | Delivery::Withdrawn
                | Delivery::Noted => None,
            };
        }
        Ok(rows)
    }

    /// The person writes to a session from Desk: queued for its next idle,
    /// or `When::Now` into its running turn where its engine can take it.
    pub async fn line_post(
        &self,
        id: Uuid,
        text: String,
        when: When,
    ) -> Result<LineMessage, LineError> {
        // Theirs before it is queued: a boss's message waiting for the same
        // idle can't go in ahead of it.
        self.person_drove(id);
        self.line_send(
            Sender::Person,
            Send::Tell {
                to: Target::Session(id),
                text,
                when,
            },
        )
        .await
    }

    /// The person changes (`Some`) or takes back (`None`) a message of
    /// theirs that hasn't gone out.
    pub async fn line_edit(&self, id: Uuid, text: Option<String>) -> Result<(), DeskError> {
        let text = text.map(|t| line::typable(&t).trim().to_string());
        if text.as_deref().is_some_and(str::is_empty) {
            return Err(DeskError::Invalid("the message is empty".into()));
        }
        match self.0.store.line_edit(id, text.as_deref()).await? {
            Some(m) => {
                let _ = self.0.events.send(DeskEvent::Line {
                    message: Box::new(m),
                    removed: text.is_none(),
                });
                Ok(())
            }
            None => match self.0.store.line_message(id).await? {
                Some(_) => Err(DeskError::Conflict("It's already on its way.".into())),
                None => Err(DeskError::NotFound),
            },
        }
    }

    /// The overseer gives the question `ask` to the person, saying why and
    /// offering `escalation`'s answers and draft. It waits on them (and
    /// still on the overseer, who may answer first).
    pub async fn line_escalate(
        &self,
        ask: Uuid,
        by: &By,
        escalation: line::Escalation,
    ) -> Result<LineMessage, LineError> {
        let mut q = self
            .0
            .store
            .line_message(ask)
            .await?
            .filter(|q| q.kind == LineKind::Ask)
            .ok_or(Refusal::NoSuchAsk(ask))?;
        match q.asked {
            Some(Asked::Open | Asked::Escalated) => {}
            // The session went on without an answer: nothing to give the
            // person now.
            Some(Asked::Lapsed) => return Err(Refusal::WentAhead.into()),
            Some(Asked::Dismissed) => return Err(Refusal::SetAside.into()),
            Some(Asked::Answered | Asked::Withdrawn) | None => return Err(Refusal::Answered.into()),
        }
        // Given to the person after its call stopped waiting: it went
        // ahead without them.
        if q.from_session().is_some() && !self.waited_on_ask(q.id) {
            return match self.0.store.line_went_ahead(q.id, false).await? {
                Some(m) => {
                    self.line_announce(&m);
                    Err(Refusal::WentAhead.into())
                }
                None => Err(Refusal::WentAhead.into()),
            };
        }
        q.asked = Some(Asked::Escalated);
        q.escalation = Some(escalation);
        q.updated_at = Utc::now();
        self.0.store.put_line(&q).await?;
        let why = q.escalation.as_ref().map_or("", |e| e.why.as_str());
        self.note(
            by.event(
                Kind::NeedsYou,
                Some(q.from.clone()),
                format!(
                    "gave you {}'s question “{}”",
                    q.from.name,
                    crew::one_line(q.headline(), 120)
                ),
            )
            .reason(why)
            .hop(q.hop),
        );
        self.line_announce(&q);
        Ok(q)
    }

    /// The questions on Desk's lines waiting on the person, as the Floor
    /// counts its needs: answered by a reply.
    pub fn line_needs(&self) -> Vec<crate::floor::Need> {
        let st = self.0.state.lock();
        st.lines
            .for_you
            .values()
            .map(|m| crate::floor::Need {
                id: format!("line:{}", m.id),
                kind: crate::floor::NeedKind::Question,
                who: crate::floor::Who {
                    kind: m.from.kind,
                    id: m.from.id.clone(),
                    name: m.from.name.clone(),
                    engine: m.from.engine,
                },
                ask: m.headline().to_string(),
                chose: m.chose.clone(),
                why: m
                    .escalation
                    .as_ref()
                    .map(|e| e.why.clone())
                    .filter(|w| !w.is_empty()),
                options: m
                    .escalation
                    .as_ref()
                    .map(|e| e.options.clone())
                    .filter(|o| !o.is_empty()),
                since: m.at,
                place: crate::floor::Place::Session,
                answer: crate::floor::Answer::Reply,
            })
            .collect()
    }

    /// The questions to the person their sessions went ahead without,
    /// oldest first, until answered or set aside: what the Floor lists
    /// under "went ahead without you".
    pub fn line_went_ahead_list(&self) -> Vec<crate::floor::WentAhead> {
        let st = self.0.state.lock();
        let mut out: Vec<_> = st
            .lines
            .went_ahead
            .values()
            .map(|m| crate::floor::WentAhead {
                id: format!("line:{}", m.id),
                who: crate::floor::Who {
                    kind: m.from.kind,
                    id: m.from.id.clone(),
                    name: m.from.name.clone(),
                    engine: m.from.engine,
                },
                ask: m.headline().to_string(),
                chose: m.chose.clone(),
                asked_at: m.at,
                // No later than its longest wait: one found after a restart
                // went ahead long before it was marked.
                went_ahead_at: m
                    .updated_at
                    .min(m.at + chrono::Duration::from_std(line::ASK_WAIT_MAX).unwrap_or_default()),
            })
            .collect();
        out.sort_by_key(|w| w.asked_at);
        out
    }

    /// The person's draft for `id`, put in its prompt without sending it:
    /// typed in as a paste, with no Enter, for them to add to and send
    /// themselves. Refused while a dialog (a permission) has the terminal,
    /// where it would answer the dialog instead.
    pub fn line_draft(&self, id: Uuid, text: &str) -> Result<SessionView, DeskError> {
        let text = line::typable(text);
        if text.trim().is_empty() {
            return Err(DeskError::Invalid("there is nothing to put in".into()));
        }
        let view = self.view(id)?;
        if view.state == SessionState::Blocked {
            return Err(DeskError::Conflict(
                "It's showing a dialog in its terminal; answer that first.".into(),
            ));
        }
        let pty = self.attach(id)?;
        pty.write(&draft_bytes(&text))?;
        // Theirs, waiting in the prompt: nothing is typed in over it.
        pty.person_typed();
        Ok(view)
    }

    /// Whether `steer` lets the person's message into a working turn: what
    /// the composer offers.
    pub fn line_steers(&self, id: Uuid) -> Result<Steer, DeskError> {
        Ok(self.view(id)?.caps.steer)
    }
}

#[cfg(test)]
#[path = "line_tests.rs"]
mod tests;
