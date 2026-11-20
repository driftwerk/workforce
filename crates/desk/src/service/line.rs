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
    self, Asked, Call, Delivery, LineAction, LineKind, LineMessage, LineRun, Reach, Refusal, Send,
    Steering, Target, Wait, When,
};
use crate::line_store::NotAnswered;
use crate::model::{DeskEvent, Origin, SessionState, SessionView};
use crate::open_item::ClosedBy;

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
    /// The sessions the loop guard stopped, until the person speaks to
    /// them (or, for a stop its boss lifts, their boss does): each a need
    /// of the person's on the Floor, said once.
    stalled: HashMap<Uuid, Stalled>,
}

/// A session the loop guard stopped.
#[derive(Debug, Clone)]
struct Stalled {
    since: DateTime<Utc>,
    /// What the person is told, in full.
    text: String,
    /// Its boss speaking to it starts it again: it was stopped by the
    /// budget its boss's answers renew.
    boss_lifts: bool,
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

impl Sender {
    /// Who replies, as [`line::reply_target`] judges a reply.
    fn replier(&self) -> line::Replier<'_> {
        match self {
            Self::Session(id) => line::Replier::Session(*id),
            Self::Person => line::Replier::Person,
            Self::Overseer(b) => line::Replier::Overseer(&b.party.id),
        }
    }
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
/// What a session the person took back from its boss is told as it tells
/// Desk something: kept, but nobody at Desk answers it now.
pub(crate) const TAKEN_BACK_NOTE: &str =
    "Desk no longer runs this session: the person took it back \
    and runs it themselves. This is kept, but nobody at Desk answers it; send Desk no more \
    reports or questions, and talk to the person (to: \"you\").";

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

    /// The boss a session in `group` reaches as "desk": its scope's, so a
    /// session in a group with a boss of its own never reaches another.
    fn line_overseer(&self, group: Option<&str>) -> Option<Party> {
        let crew = self
            .0
            .line_crew
            .lock()
            .as_ref()
            .and_then(std::sync::Weak::upgrade)?;
        if !crew.answers_lines() {
            return None;
        }
        self.boss_of(&self.scope_of_group(group))
            .map(|b| Party::manager(&b.agent, &b.name))
    }

    /// The group a line message is its session's: its sender's when that
    /// is a session, else its recipient's.
    pub(super) fn line_group(&self, m: &LineMessage) -> Option<String> {
        m.from_session()
            .or_else(|| m.to_session())
            .and_then(|id| self.group_of_member(&super::session_item(id)))
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
        let lifted = {
            let mut st = self.0.state.lock();
            st.lines.runs.remove(&id);
            st.lines.spoke.insert(id, Utc::now());
            st.lines.stalled.remove(&id).is_some()
        };
        if lifted {
            self.stall_lifted(id, ClosedBy::Person, "You spoke to it.");
        }
    }

    /// `boss` spoke to `id`: when it is the session's own boss, its budget
    /// starts again (its ceiling and its budget for other sessions don't),
    /// and a stop that budget made is lifted. Whether it was.
    fn boss_spoke(&self, id: Uuid, boss: &str) -> bool {
        if !self.own_boss(id, boss) {
            return false;
        }
        let lifted = {
            let mut st = self.0.state.lock();
            st.lines.runs.entry(id).or_default().boss_spoke();
            match st.lines.stalled.get(&id) {
                Some(s) if s.boss_lifts => st.lines.stalled.remove(&id).is_some(),
                _ => false,
            }
        };
        if lifted {
            self.stall_lifted(id, ClosedBy::Boss, "Its boss answered it.");
        }
        true
    }

    /// The boss that has `id` now: its charge's while one runs it, else
    /// its scope's.
    fn boss_of_session(&self, id: Uuid) -> Option<String> {
        if let Some(c) = self.charge_of(id).filter(crate::charge::Charge::live) {
            return Some(c.boss);
        }
        let group = self.group_of_member(&super::session_item(id));
        self.boss_of(&self.scope_of_group(group.as_deref()))
            .map(|b| b.agent)
    }

    /// Whether `boss` is `id`'s own boss.
    fn own_boss(&self, id: Uuid, boss: &str) -> bool {
        self.boss_of_session(id).is_some_and(|b| b == boss)
    }

    /// The loop guard refused `me` with `r`: the person is told, once a
    /// stall, on the Floor, in Activity, and in its boss's updates.
    async fn line_stalled(&self, me: &SessionView, r: &Refusal, boss_lifts: bool) {
        let Some(why) = line::stalled_why(r) else {
            return;
        };
        let text = format!("{} {why}", me.label());
        {
            let mut st = self.0.state.lock();
            if st.lines.stalled.contains_key(&me.id) {
                return;
            }
            st.lines.stalled.insert(
                me.id,
                Stalled {
                    since: Utc::now(),
                    text: text.clone(),
                    boss_lifts,
                },
            );
        }
        tracing::info!(session = %me.id, refusal = %r, "the line's loop guard stopped a session; the person is told");
        let it = Party::session(me);
        self.note(Event::new(Kind::NeedsYou, it.clone(), Some(it), why));
        if let Some(boss) = self.boss_of_session(me.id) {
            let mut item = crate::open_item::NewItem::ask(
                &boss,
                crate::open_item::OpenKind::Todo,
                Some(me.id),
                &text,
            );
            item.dedupe_key = crate::open_item::stalled_key(me.id);
            if let Err(e) = self.0.store.item_open(&item).await {
                tracing::warn!(session = %me.id, error = %e, "couldn't keep a stalled session for its boss's updates");
            }
            // Lifted while it opened: its close may have run first.
            if !self.0.state.lock().lines.stalled.contains_key(&me.id) {
                let key = crate::open_item::stalled_key(me.id);
                if let Err(e) = self
                    .0
                    .store
                    .item_close_of_key(&key, ClosedBy::Superseded, "It carried on.")
                    .await
                {
                    tracing::warn!(session = %me.id, error = %e, "couldn't close a stalled session's item");
                }
            }
        }
    }

    /// `id`'s stall is over: said in Activity (so the Floor reads it
    /// gone), and its boss's item closes.
    fn stall_lifted(&self, id: Uuid, by: ClosedBy, why: &'static str) {
        if let Ok(v) = self.view(id) {
            let it = Party::session(&v);
            self.note(Event::new(
                Kind::Resumed,
                it.clone(),
                Some(it),
                format!("can carry on: {}", why.to_lowercase().trim_end_matches('.')),
            ));
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(session = %id, "no runtime to close a stalled session's item; a restart closes it");
            return;
        };
        let store = self.0.store.clone();
        rt.spawn(async move {
            let key = crate::open_item::stalled_key(id);
            if let Err(e) = store.item_close_of_key(&key, by, why).await {
                tracing::warn!(session = %id, error = %e, "couldn't close a stalled session's item");
            }
        });
    }

    /// When the person last took part in `id` in the session itself: typed
    /// in its terminal or Desk's composer, or prompted it. Never an answer
    /// given from Desk's buttons (that is one ask's, not the session's), nor
    /// Desk's own deliveries for them. Kept in memory: what it settles is
    /// kept by closing the item.
    pub fn person_input_at(&self, id: Uuid) -> Option<DateTime<Utc>> {
        let st = self.0.state.lock();
        Desk::last_input(&st, id)
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

    /// `id` moved from group `from` to `to`: across a boss boundary, what
    /// [`crate::scope::move_effects`] says is done, as Desk. Its rows stay
    /// where they were written.
    pub(super) async fn moved_across(&self, id: Uuid, from: Option<&str>, to: Option<&str>) {
        let bossed = self.bossed();
        if crate::scope::scope_of(from, &bossed) == crate::scope::scope_of(to, &bossed) || {
            let s = self.0.scopes.lock();
            s.placing.contains(&id) || s.moving.contains(&id)
        } {
            return;
        }
        let was = self.seen_by(&crate::scope::scope_of(from, &bossed));
        let mut tickets = Vec::new();
        match self.0.store.session_tickets(id).await {
            Ok(mine) => {
                for t in mine {
                    let group = self.group_of_board(t.board).await.ok().flatten();
                    let open = !matches!(
                        t.state,
                        crate::board::TicketState::Done | crate::board::TicketState::Dropped
                    );
                    if open && was.sees(group.as_deref()) {
                        tickets.push((t.id, t.state));
                    }
                }
            }
            Err(e) => tracing::warn!(%id, error = %e, "couldn't read a moved session's tickets"),
        }
        // A charge of the new scope's boss stays.
        let new_scope = crate::scope::scope_of(to, &bossed);
        let charged = self.charge_of(id).is_some_and(|c| {
            c.live() && crate::scope::scope_of(c.group.as_deref(), &bossed) != new_scope
        });
        let ids: Vec<Uuid> = tickets.iter().map(|(t, _)| *t).collect();
        let by = By {
            party: Party::desk(),
            reason: Some("It moved to another group.".into()),
            suggestion_id: None,
            hop: 0,
            preference: None,
        };
        let mut said = Vec::new();
        for effect in crate::scope::move_effects(from, to, &ids, charged, &bossed) {
            match effect {
                crate::scope::Effect::Unassign { ticket } => {
                    let doing = tickets
                        .iter()
                        .any(|(t, s)| *t == ticket && !crate::board::unassigns(*s));
                    let done = match doing {
                        true => {
                            let mv = crate::service::Move {
                                state: crate::board::TicketState::Todo,
                                before: None,
                                after: None,
                            };
                            self.move_ticket(ticket, mv, &by).await.map(drop)
                        }
                        false => self.assign_ticket(ticket, None, &by).await.map(drop),
                    };
                    match done {
                        Ok(()) => said
                            .push("Your ticket from there is back in To do, and no longer yours."),
                        Err(e) => {
                            tracing::warn!(%id, error = %e, "a moved session's ticket wasn't unassigned")
                        }
                    }
                }
                crate::scope::Effect::WithdrawAsks => {
                    let asks = match self.0.store.line_open_asks(None).await {
                        Ok(asks) => asks,
                        Err(e) => {
                            tracing::warn!(%id, error = %e, "couldn't read a moved session's questions");
                            continue;
                        }
                    };
                    let mut withdrawn = false;
                    for ask in asks.into_iter().filter(|a| {
                        a.from_session() == Some(id)
                            && a.to.kind == PartyKind::Manager
                            && a.asked == Some(Asked::Open)
                    }) {
                        // One statement: an answer that came first stands.
                        match self.0.store.line_withdraw_ask(ask.id, &[Asked::Open]).await {
                            Ok(Some(ask)) => {
                                self.line_announce(&ask);
                                withdrawn = true;
                            }
                            Ok(None) => {}
                            Err(e) => {
                                tracing::warn!(error = %e, "couldn't withdraw a moved session's question")
                            }
                        }
                    }
                    if withdrawn {
                        said.push("Your questions to the boss you had are withdrawn.");
                    }
                }
                crate::scope::Effect::Release => {
                    if let Err(e) = self.release(id, &by).await {
                        tracing::warn!(%id, error = %e, "a moved session's boss didn't let it go");
                    }
                }
                crate::scope::Effect::Tell => {
                    let Ok(view) = self.view(id) else {
                        continue;
                    };
                    let place = to
                        .and_then(|g| self.group_name(g))
                        .unwrap_or_else(|| "no group".into());
                    let text = std::iter::once(format!(
                        "You were moved to {place}, which another boss oversees."
                    ))
                    .chain(said.iter().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
                    .join(" ");
                    let mut note = LineMessage::new(
                        Party::desk(),
                        Party::session(&view),
                        LineKind::Tell,
                        text,
                        0,
                    );
                    note.group = to.map(str::to_string);
                    if !view.live {
                        note.delivery = Delivery::Held;
                    }
                    match self.0.store.put_line(&note).await {
                        Ok(()) => {
                            self.line_announce(&note);
                            self.line_pump_later(id);
                        }
                        Err(e) => tracing::warn!(%id, error = %e, "a moved session wasn't told"),
                    }
                }
            }
        }
    }

    /// Records `m` in Activity and tells every window, and what its
    /// sessions' threads are unread by.
    pub(super) fn line_announce(&self, m: &LineMessage) {
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
        Ok(self.line_send_with(from, send, None).await?.0)
    }

    /// [`Self::line_send`], with how urgently: a boss's tell or ask with
    /// `urgency` to a session that is working goes into its running turn
    /// (design note §4), and what became of that is said. Urgency is a
    /// boss's to give (the person's Send now is [`When::Now`]); from anyone
    /// else, and for a reply, it is refused or unused.
    pub async fn line_send_with(
        &self,
        from: Sender,
        send: Send,
        urgency: Option<line::Urgency>,
    ) -> Result<(LineMessage, Option<line::Rushed>), LineError> {
        self.line_send_threaded(from, send, urgency, None).await
    }

    /// [`Self::line_send_with`], as a reply to the line message
    /// `in_reply_to` when one is given: it says what it replies to.
    async fn line_send_threaded(
        &self,
        from: Sender,
        send: Send,
        urgency: Option<line::Urgency>,
        in_reply_to: Option<Uuid>,
    ) -> Result<(LineMessage, Option<line::Rushed>), LineError> {
        if urgency.is_some() && !matches!(from, Sender::Overseer(_)) {
            return Err(Refusal::NotYours.into());
        }
        let hop_send = send.clone();
        let (to, text, kind, when) = match send {
            Send::Reply { to, text } => return Ok((self.line_answer(to, from, text).await?, None)),
            Send::Ask { to, text, .. } => (to, text, LineKind::Ask, When::Queue),
            Send::Tell { to, text, when } => (to, text, LineKind::Tell, when),
        };
        let text = line::typable(&text).trim().to_string();
        if text.is_empty() {
            return Err(Refusal::Missing("what to say").into());
        }
        let sessions = self.line_sessions();
        let bossed = self.bossed();
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
                let to_session = matches!(to, Target::Session(_));
                if let Err(r) = line::guard(checked, &run, to_session) {
                    // Only the budget its boss's answers renew is lifted
                    // by them.
                    self.line_stalled(me, &r, r == Refusal::Budget).await;
                    return Err(r.into());
                }
                let overseer = self.line_overseer(me.group.as_ref().map(|g| g.id.as_str()));
                let recipient = line::reach(
                    me,
                    &to,
                    &sessions,
                    overseer.as_ref(),
                    self.line_reach(),
                    &bossed,
                )?;
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
                line::guard(hop, &LineRun::default(), false)?;
                let recipient = match to {
                    Target::Session(id) => {
                        let them = find(id).ok_or(DeskError::NotFound)?;
                        // A boss writes only to its own scope's sessions:
                        // another's is refused as one that isn't there, before
                        // anything is said of it.
                        if let Sender::Overseer(by) = &from {
                            let group = them.group.as_ref().map(|g| g.id.as_str());
                            if !self.boss_reaches(&by.party.id, group) {
                                return Err(Refusal::NoSuchSession(id.to_string()).into());
                            }
                            // Taken back by the person: no boss writes to
                            // it until they hand it to one again.
                            if self.taken_back(id) {
                                return Err(Refusal::TakenBack(crate::charge::left_alone(
                                    them.label(),
                                ))
                                .into());
                            }
                        }
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
                    // An overseer never messages another.
                    Target::Overseer if matches!(from, Sender::Overseer(_)) => {
                        return Err(Refusal::Unreachable(
                            "Desk".into(),
                            "message a session or the person",
                        )
                        .into())
                    }
                    Target::Overseer => match self.line_overseer(None) {
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
        m.in_reply_to = in_reply_to;
        // The person's and the overseer's to a session are its group's.
        m.group = group.or_else(|| self.line_group(&m));
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
        // How a boss's urgent words reach it, decided and counted before
        // they are kept (the count's lock held until they are).
        let plan = match (urgency, &recipient) {
            (Some(u), Some(v)) => Some(self.rush_plan(v, &mut m, u).await?),
            _ => None,
        };
        self.0.store.put_line(&m).await?;
        let (rushed, mark) = match plan {
            Some(p) => {
                // Kept: the next urgent send to it counts this one.
                drop(p.counted);
                (Some(p.rushed), p.mark)
            }
            None => (None, None),
        };
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
                self.0
                    .state
                    .lock()
                    .lines
                    .runs
                    .entry(*id)
                    .or_default()
                    .sent_one(m.to_session().is_some());
                let what = match kind {
                    LineKind::Ask => crate::picture::What::SessionAsked,
                    _ => crate::picture::What::SessionReported,
                };
                self.picture_session_said(*id, what, &m.text, m.id);
            }
            Sender::Person => {
                if let Some(v) = &recipient {
                    self.person_spoke(v.id);
                }
            }
            // Its own boss speaking to it is the designed conversation:
            // its budget starts again.
            Sender::Overseer(by) => {
                if let Some(v) = &recipient {
                    self.boss_spoke(v.id, &by.party.id);
                }
            }
        }
        self.line_noted(&m);
        self.line_announce(&m);
        if let Some(v) = recipient {
            match (rushed, &from) {
                (Some(r), Sender::Overseer(by)) => {
                    self.rush_deliver(&mut m, v.id, r, mark, by).await?
                }
                _ => match steering {
                    Steering::Queue => self.line_pump_later(v.id),
                    Steering::PasteNow | Steering::TurnSteer => {
                        self.line_steer(&mut m, steering).await?
                    }
                },
            }
        }
        Ok((m, rushed))
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
        let back = async {
            // Only one said to this session and not yet back: a message
            // that was taken some other way stays taken.
            let Some(m) = self.0.store.line_untake(row, &id.to_string()).await? else {
                return Ok(None);
            };
            Ok::<_, eyre::Report>(Some(m))
        };
        match back.await {
            Ok(None) => {}
            Ok(Some(m)) => {
                self.line_announce(&m);
                self.rush_refused(id, &m);
                self.line_pump_later(id);
            }
            Err(e) => {
                tracing::warn!(session = %id, error = %e, "couldn't queue a refused steer again")
            }
        }
    }

    /// Answers the question `ask` as `by`: the first answer stands. The
    /// person may answer any question asked on Desk.
    ///
    /// A reply is never refused into the void: one to a line message that
    /// is no question waiting on `by` (a plain message, or a question
    /// answered, set aside or gone ahead since) goes to whoever wrote it,
    /// as a message saying what it replies to ([`Self::line_reply_told`]).
    /// The person's own answers keep their question's rules.
    pub async fn line_answer(
        &self,
        ask: Uuid,
        by: Sender,
        text: String,
    ) -> Result<LineMessage, LineError> {
        match self.line_answer_ask(ask, by.clone(), text.clone()).await {
            Err(LineError::Refused(r)) if line::replies_as_message(&r, by.replier()) => {
                self.line_reply_told(ask, by, text, r).await
            }
            answered => answered,
        }
    }

    /// `by`'s reply to the line message `to`, that answered no question
    /// waiting on it (`refused`, said again when it can't go either): a
    /// message to the one who wrote `to`, in reply to it, under the line's
    /// rules for any other message between them.
    async fn line_reply_told(
        &self,
        to: Uuid,
        by: Sender,
        text: String,
        refused: Refusal,
    ) -> Result<LineMessage, LineError> {
        // A question keeps its rules: only a plain message is replied to so.
        let Some(original) = self
            .0
            .store
            .line_message(to)
            .await?
            .filter(|m| m.kind != LineKind::Ask)
        else {
            return Err(refused.into());
        };
        let Some(target) = line::reply_target(&original, by.replier()) else {
            return Err(refused.into());
        };
        tracing::info!(message = %to, kind = original.kind.as_str(), "a reply to what no longer waits on its sender goes as a message in reply to it");
        let send = Send::Tell {
            to: target,
            text,
            when: When::Queue,
        };
        // Boxed: a send of a reply comes back here.
        Ok(
            Box::pin(self.line_send_threaded(by, send, None, Some(original.id)))
                .await?
                .0,
        )
    }

    async fn line_answer_ask(
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
                if question.to.kind == PartyKind::Manager
                    && question.to.id == b.party.id
                    && self.boss_reaches(&b.party.id, question.group.as_deref()) =>
            {
                if let Some(asker) = question.from_session() {
                    self.boss_holds(asker, Some(b))?;
                    self.mode_lets(asker, Some(b))?;
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
        a.group = question.group.clone().or_else(|| self.line_group(&a));
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
        // What the boss owed the person on it is settled in this request.
        let (closer, why) = match &by {
            Sender::Person => (ClosedBy::Person, "You answered it.".to_string()),
            Sender::Overseer(b) => (ClosedBy::Boss, format!("{} answered it.", b.party.name)),
            Sender::Session(_) => (ClosedBy::Session, "The session answered it.".to_string()),
        };
        self.close_line_item(question.id, closer, &why).await;
        match (&by, &asker) {
            (Sender::Person, Some(v)) => {
                // Their answer, not them taking the wheel: what a boss has
                // waiting for the session isn't overtaken by it.
                self.picture_person_answered(v.id, &a.text);
                self.person_spoke(v.id)
            }
            (Sender::Session(id), _) => {
                self.0
                    .state
                    .lock()
                    .lines
                    .runs
                    .entry(*id)
                    .or_default()
                    .sent_one(question.from_session().is_some());
            }
            // Its own boss's answer starts the asker's budget again; any
            // other overseer's counts against it: its budget holds the
            // exchange, not the overseer's.
            (Sender::Overseer(b), Some(v)) => {
                if !self.boss_spoke(v.id, &b.party.id) {
                    self.0.state.lock().lines.runs.entry(v.id).or_default().sent += 1;
                }
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
        let call = line::call_of(name, input).inspect_err(|e| {
            // A recall asked of what it can't name is refused before anything
            // is searched: kept in the log, as a refused read is.
            if name.starts_with("recall") {
                tracing::info!(session = %caller, tool = name, refused = %e, "a session's recall was refused as asked");
            }
        })?;
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
                // Taken back from its boss, it is the person's: what it
                // tells Desk is kept, and nobody answers it.
                let note = match m.delivery {
                    Delivery::Held => Some(format!(
                        "{} is stopped; it gets this when it runs again.",
                        m.to.name
                    )),
                    _ if m.to.kind == PartyKind::Manager && self.taken_back(caller) => {
                        Some(TAKEN_BACK_NOTE.to_string())
                    }
                    _ => None,
                };
                Ok(json!({ "id": m.id, "delivery": m.delivery, "note": note }))
            }
            Call::Reply { to, text } => {
                // A mark is the question's to this session that it opens.
                let to = match to {
                    line::ReplyTo::Id(id) => id,
                    line::ReplyTo::Mark(mark) => self
                        .0
                        .store
                        .line_marked(&caller.to_string(), &mark)
                        .await
                        .map_err(|e| say(e.into()))?
                        .map(|m| m.id)
                        .ok_or_else(|| {
                            format!("No question to you on Desk's line has the mark {mark}.")
                        })?,
                };
                let a = self
                    .line_answer(to, Sender::Session(caller), text)
                    .await
                    .map_err(say)?;
                Ok(json!({ "id": a.id }))
            }
            Call::Ticket(call) => self.ticket_call(caller, call).await,
            Call::Recall {
                query,
                reach,
                kinds,
                limit,
            } => self.recall_call(caller, query, reach, kinds, limit).await,
            Call::RecallRead { reference } => self.recall_read_call(caller, reference).await,
            Call::Brief => self.brief_call(caller).await,
            Call::Inbox => {
                let taken = self.line_take_inbox(caller).await.map_err(say)?;
                Ok(json!({ "messages": taken }))
            }
            Call::Peers => {
                let me = sessions
                    .iter()
                    .find(|s| s.id == caller)
                    .ok_or_else(|| "Desk doesn't know this session.".to_string())?;
                // A work is listed once: the session it went on in, not
                // those it left.
                let lineage = self.lineage();
                let peers: Vec<Value> =
                    line::peers(me, &sessions, self.line_reach(), &self.bossed())
                        .into_iter()
                        .filter(|s| !lineage.handed_off(s.id))
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
                let desk = match self.line_overseer(me.group.as_ref().map(|g| g.id.as_str())) {
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
                self.close_line_item(ask, ClosedBy::Person, "You set it aside unanswered.")
                    .await;
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

    /// Closes the boss's open item on the question `ask`, if one: a failure
    /// is logged and never fails the answer (the hub's closers catch it).
    pub async fn close_line_item(&self, ask: Uuid, by: ClosedBy, why: &str) {
        if let Err(e) = self.0.store.item_close_of_line(ask, by, why).await {
            tracing::warn!(%ask, error = %e, "closing the open item of an answered question failed");
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
    pub(super) fn waited_on(&self, m: &LineMessage) -> bool {
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

    pub(super) fn line_delivered_into(&self, id: Uuid, m: &LineMessage) {
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
    pub(super) async fn rendered(&self, m: &LineMessage) -> String {
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
                    // Its turn ended: what Desk kept through it is handed
                    // now, not at the plugin's next look.
                    Ok(Ok(DeskEvent::Session { session }))
                        if session.id == caller && !line::in_turn(session.state) =>
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
    pub(super) fn line_gate(&self, view: &SessionView, m: &LineMessage) -> Option<Wait> {
        if m.from.kind == PartyKind::You {
            return None;
        }
        if self.breaker_refuses_prompt(view.id, None).is_err() {
            return Some(Wait::Paused);
        }
        // Still in a mode that doesn't ask, from a boss no longer unbound
        // over it: nothing but the person's goes in until Desk has started
        // it again in its own mode.
        if self.mode_ending(view.id) {
            return Some(Wait::ModeEnding);
        }
        if m.from.kind == PartyKind::Manager
            && m.kind == LineKind::Tell
            && self.person_driving(view.id)
        {
            return Some(Wait::PersonDriving);
        }
        (view.state == SessionState::Asking).then_some(Wait::Asking)
    }

    /// Desk's plugin in `id`, waiting on its line, looks again now: a turn
    /// Desk kept its messages through ended without a state of its own to
    /// say so (Esc).
    pub(super) fn line_wake(&self, id: Uuid) {
        let desk = self.clone();
        tokio::spawn(async move {
            if let Ok(inbox) = desk.0.store.line_inbox(&id.to_string()).await {
                if let Some(m) = inbox.iter().find(|m| m.delivery == Delivery::Queued) {
                    desk.line_announce(m);
                }
            }
        });
    }

    /// Whether `view` is inside a turn Desk keeps its line's messages
    /// through, Desk's plugin handed none until it ends: running, working
    /// or on a dialog, and not a turn Esc ended (which Desk may still read
    /// as working, no Stop following it).
    pub(super) fn in_a_turn(&self, view: &SessionView) -> bool {
        view.live && line::in_turn(view.state) && !self.esc_ended(view.id)
    }

    /// Who a message is typed in for: the person's own, as theirs.
    pub(super) fn typed_by(m: &LineMessage) -> Option<By> {
        (m.from.kind == PartyKind::You).then(By::you)
    }

    async fn line_next_now(&self, caller: Uuid) -> Result<Option<Value>, LineError> {
        let view = self.view(caller)?;
        // Inside a turn Desk keeps what waits (see [`line::deliverable`]).
        if !view.live || self.in_a_turn(&view) {
            return Ok(None);
        }
        let inbox = self.0.store.line_inbox(&caller.to_string()).await?;
        // One on its way at a time: the order they were sent in holds.
        if inbox.iter().any(|m| m.delivery == Delivery::Handed) {
            return Ok(None);
        }
        for m in inbox {
            if self.waited_on(&m)
                || self.line_gate(&view, &m).is_some()
                || self.steer_reserved(caller, &m)
            {
                continue;
            }
            // Decided before the person's own words there: never handed in.
            if self.line_overtaken(&m).await {
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

    /// Hand-offs Desk's plugin never said went in, handed before `before`
    /// ([`HANDED_FOR`] ago), back in the queue (the plugin died between
    /// taking and submitting one). Not one to a session still in a turn:
    /// Claude Code holds it behind that turn and submits it as the turn
    /// ends, so handed again it would go in twice.
    pub(super) async fn line_requeue_stale(&self, before: DateTime<Utc>) {
        let stale = match self.0.store.line_handed_before(before).await {
            Ok(stale) => stale,
            Err(e) => {
                tracing::warn!(error = %e, "couldn't read stale line hand-offs");
                return;
            }
        };
        for m in stale {
            let held = m
                .to_session()
                .and_then(|s| self.view(s).ok())
                .is_some_and(|v| self.in_a_turn(&v));
            if held {
                continue;
            }
            // Still handed from then: one dropped and handed again since
            // is the new hand-off's.
            match self.0.store.line_requeue(Some(m.id), Some(before)).await {
                Ok(back) => self.line_requeued(back).await,
                Err(e) => {
                    tracing::warn!(id = %m.id, error = %e, "couldn't requeue a stale line hand-off")
                }
            }
        }
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
            let back = self.0.store.line_requeue(Some(m.id), None).await?;
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

    /// `id` runs now: what its line held for it is delivered, soon.
    pub(super) fn line_went_live(&self, id: Uuid) {
        if self.0.state.lock().lines.pending.contains(&id) {
            self.line_pump_later(id);
        }
    }

    /// Delivers what waits for `id`, soon.
    pub(super) fn line_pump_later(&self, id: Uuid) {
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
            // One its running turn's next tool call takes is left to it.
            if m.delivery == Delivery::Handed || self.waited_on(&m) || self.steer_reserved(id, &m) {
                continue;
            }
            // Decided before the person's own words there: never typed in.
            if self.line_overtaken(&m).await {
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
                // One statement: a row taken, withdrawn or edited since
                // the inbox was read is left as it is.
                match self.0.store.line_set_waiting(m.id, wanted).await? {
                    Some(set) => {
                        self.line_announce(&set);
                        m = set;
                    }
                    None => continue,
                }
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
                if let Some(back) = self.0.store.line_untake(taken.id, &id.to_string()).await? {
                    self.line_announce(&back);
                }
                tracing::debug!(session = %id, error = %e, "a line message couldn't be typed in");
                continue;
            }
            self.line_delivered_into(id, &taken);
            typed_in = true;
        }
        Ok(())
    }

    /// What the lines hold at the start, read before `start` returns: a
    /// snapshot read by a task running beside the first calls could be
    /// taken before a question is dismissed and laid down after it, putting
    /// the question back among what waits on the person.
    pub(super) async fn line_load(&self) {
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
        // Nor is any session stopped: its run starts again, so what the
        // bosses were told of stalls before it no longer holds.
        if let Err(e) = self
            .0
            .store
            .item_close_of_key_prefix(
                crate::open_item::STALLED_KEY,
                ClosedBy::Lapsed,
                "Desk restarted, so the session can carry on.",
            )
            .await
        {
            tracing::warn!(error = %e, "couldn't close the stalled sessions' items from before a restart");
        }
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
    }

    /// The pump's own loop: a session coming to its prompt (or running
    /// again) gets what waits for it, and a hand-off the plugin never
    /// finished goes again.
    pub(super) async fn line_loop(self) {
        let mut rx = self.0.events.subscribe();
        let mut tick = tokio::time::interval(PUMP_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = self.0.cancel.cancelled() => return,
                _ = tick.tick() => {
                    self.line_requeue_stale(Utc::now() - HANDED_FOR).await;
                    self.line_pump_pending().await;
                    self.line_sweep_waits().await;
                }
                ev = rx.recv() => match ev {
                    Ok(DeskEvent::Session { session }) => {
                        // Its turn ended in a mode that is ending: it goes
                        // back to its own first, and what waits for it after.
                        if self.mode_ending(session.id) {
                            self.modes_changed();
                            continue;
                        }
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
        self.rush_forget(id);
        let asks = match self.0.store.line_open_asks(None).await {
            Ok(asks) => asks,
            Err(e) => {
                tracing::warn!(session = %id, error = %e, "couldn't withdraw a forgotten session's questions");
                return;
            }
        };
        for ask in asks {
            let to_it = ask.to_session() == Some(id);
            if !to_it && ask.from_session() != Some(id) {
                continue;
            }
            // One statement: an answer or a dismissal that came first
            // stands, and is not withdrawn over.
            let ask = match self
                .0
                .store
                .line_withdraw_ask(ask.id, &[Asked::Open, Asked::Escalated, Asked::Lapsed])
                .await
            {
                Ok(Some(ask)) => ask,
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!(error = %e, "couldn't withdraw a question");
                    continue;
                }
            };
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
            note.group = ask.group.clone().or_else(|| self.line_group(&note));
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
        st.lines.stalled.remove(&id);
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

    /// Why `m`, still to go into its session, waits now; none when nothing
    /// holds it there.
    pub fn line_waits(&self, m: &LineMessage) -> Option<Wait> {
        let id = m.to_session()?;
        let view = self.view(id).ok()?;
        self.waits_why(&view, self.typed_since_idle(id), self.plugin_heard(id), m)
    }

    fn waits_why(
        &self,
        view: &SessionView,
        typed: bool,
        plugin: bool,
        m: &LineMessage,
    ) -> Option<Wait> {
        match m.delivery {
            Delivery::Held => Some(Wait::Stopped),
            Delivery::Queued if self.steered(view.id, m.id) => Some(Wait::NextCall),
            Delivery::Queued => match line::deliverable(view, typed, plugin) {
                // A turn Esc ended: the plugin takes it now.
                Err(Wait::Working)
                    if plugin && view.caps.line == Deliver::Submit && !self.in_a_turn(view) =>
                {
                    self.line_gate(view, m)
                }
                Err(why) => Some(why),
                Ok(_) => self.line_gate(view, m),
            },
            Delivery::Handed
            | Delivery::Delivered
            | Delivery::Refused
            | Delivery::Withdrawn
            | Delivery::Noted => None,
        }
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
            m.waiting = self.waits_why(&view, typed, plugin, m);
            m.actions = self.line_actions(&view, m);
        }
        Ok(rows)
    }

    /// What the person can do now with `m`, to `view`, while it waits.
    fn line_actions(&self, view: &SessionView, m: &LineMessage) -> Vec<LineAction> {
        if !matches!(m.delivery, Delivery::Queued | Delivery::Held) {
            return Vec::new();
        }
        let mut can = Vec::new();
        if self.send_now_how(view, m).is_ok() {
            can.push(LineAction::SendNow);
        }
        let theirs = m.from.kind == PartyKind::You;
        if theirs {
            can.push(LineAction::Edit);
        }
        if theirs || Self::boss_word(m) {
            can.push(LineAction::Withdraw);
        }
        can
    }

    /// Whether `m` is a boss's tell or answer, which the person may take
    /// back unsent ([`Self::line_take_back`]).
    fn boss_word(m: &LineMessage) -> bool {
        m.from.kind == PartyKind::Manager && matches!(m.kind, LineKind::Tell | LineKind::Answer)
    }

    /// The person takes back `id`, waiting to go into a session: their own
    /// is gone, as if never sent; a boss's tell or answer is withdrawn
    /// unsent, kept on the line, and its boss's picture says the person
    /// took it back (an answer's question comes back to them).
    pub async fn line_take_back(&self, id: Uuid) -> Result<(), DeskError> {
        let m = self
            .0
            .store
            .line_message(id)
            .await?
            .ok_or(DeskError::NotFound)?;
        if m.from.kind == PartyKind::You {
            return self.line_edit(id, None).await;
        }
        let Some(session) = m.to_session().filter(|_| Self::boss_word(&m)) else {
            return Err(DeskError::Conflict(
                "Only your own messages and a boss's can be taken back.".into(),
            ));
        };
        match self.line_withdraw(id).await? {
            Some((withdrawn, _)) => {
                self.picture_note(
                    crate::picture::Note::new(
                        crate::picture::What::TakenBack,
                        withdrawn.text.clone(),
                    )
                    .boss(&withdrawn.from.id)
                    .session(session),
                );
                Ok(())
            }
            None => Err(DeskError::Conflict("It's already on its way.".into())),
        }
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
        // idle can't go in ahead of it, and in the bosses' picture before
        // anything else is done of it. (What Desk writes on the person's
        // side, a ticket's notice or an undo, goes through `line_send`
        // and isn't their words.)
        self.person_drove(id);
        let m = self
            .line_send(
                Sender::Person,
                Send::Tell {
                    to: Target::Session(id),
                    text,
                    when,
                },
            )
            .await?;
        // Taken (a refused one isn't their word to it).
        self.picture_person_said(id, &m.text);
        Ok(m)
    }

    /// Takes back the one message `id` a boss sent a session, while it is
    /// queued or held (an Undo of its decision): it never goes in. An
    /// answer taken back puts its question back with the person. The
    /// message as it now is, and the question put back; `None` when it is
    /// no longer waiting.
    pub async fn line_withdraw(
        &self,
        id: Uuid,
    ) -> Result<Option<(LineMessage, Option<LineMessage>)>, DeskError> {
        let back = line::Escalation {
            why: "You took back Desk's answer, so this is yours to answer.".into(),
            options: Vec::new(),
            draft: None,
        };
        let Some((withdrawn, reopened)) = self.0.store.line_withdraw_one(id, &back).await? else {
            return Ok(None);
        };
        self.line_announce(&withdrawn);
        if let Some(q) = &reopened {
            self.line_announce(q);
        }
        Ok(Some((withdrawn, reopened)))
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
        // One statement: an answer, a dismissal or a lapse that came first
        // stands.
        let Some(given) = self.0.store.line_escalate_ask(q.id, &escalation).await? else {
            return Err(
                match self.0.store.line_message(ask).await?.and_then(|q| q.asked) {
                    Some(Asked::Lapsed) => Refusal::WentAhead,
                    Some(Asked::Dismissed) => Refusal::SetAside,
                    _ => Refusal::Answered,
                }
                .into(),
            );
        };
        q = given;
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
        let stalled = st.lines.stalled.iter().map(|(id, s)| {
            let view = st.records.get(id).map(|r| Self::view_of(&st, r));
            crate::floor::Need {
                id: format!("stalled:{id}"),
                kind: crate::floor::NeedKind::Question,
                who: crate::floor::Who {
                    kind: PartyKind::Session,
                    id: id.to_string(),
                    name: view
                        .as_ref()
                        .map_or_else(|| "A session".into(), |v| v.label().to_string()),
                    engine: view.as_ref().map(|v| v.engine),
                },
                ask: s.text.clone(),
                chose: None,
                why: None,
                options: None,
                since: s.since,
                place: crate::floor::Place::Session,
                answer: crate::floor::Answer::Reply,
                group: None,
                boss: None,
            }
        });
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
                group: None,
                boss: None,
            })
            .chain(stalled)
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
