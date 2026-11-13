//! The crew at a glance: what happened (the activity log, always recorded),
//! where everyone is (the overview), and what the overseeing agent proposed
//! (suggestions). Sessions are recorded by the desk itself; agents' turns,
//! the overseer and its actions by the hub through [`Crew`].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::floor::{Need, Outcome};
use crate::model::{Engine, Origin, SessionState, SessionView};

/// Who or what an event is by or about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PartyKind {
    Agent,
    Session,
    Manager,
    You,
    /// Desk itself: its breaker.
    Desk,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Party {
    pub kind: PartyKind,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub engine: Option<Engine>,
    #[serde(default)]
    pub origin: Option<Origin>,
}

impl Party {
    pub fn you() -> Self {
        Self {
            kind: PartyKind::You,
            id: "you".into(),
            name: "You".into(),
            engine: None,
            origin: None,
        }
    }

    pub fn agent(id: &str, name: &str) -> Self {
        Self {
            kind: PartyKind::Agent,
            id: id.into(),
            name: name.into(),
            engine: None,
            origin: None,
        }
    }

    pub fn manager(id: &str, name: &str) -> Self {
        Self {
            kind: PartyKind::Manager,
            ..Self::agent(id, name)
        }
    }

    /// Desk's breaker, which steers and pauses what has gone too far.
    pub fn breaker() -> Self {
        Self {
            kind: PartyKind::Desk,
            id: "breaker".into(),
            name: "Desk's breaker".into(),
            engine: None,
            origin: None,
        }
    }

    /// Desk itself, where it moves what follows from something it saw
    /// (a ticket following its pull request).
    pub fn desk() -> Self {
        Self {
            kind: PartyKind::Desk,
            id: "desk".into(),
            name: "Desk".into(),
            engine: None,
            origin: None,
        }
    }

    pub fn session(v: &SessionView) -> Self {
        Self {
            kind: PartyKind::Session,
            id: v.id.to_string(),
            name: v.label().to_string(),
            engine: Some(v.engine),
            origin: Some(v.origin),
        }
    }
}

macro_rules! kinds {
    ($($variant:ident => $text:literal),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum Kind { $($variant),+ }

        impl Kind {
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }

            pub fn parse(s: &str) -> Option<Self> {
                match s { $($text => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}

kinds! {
    Started => "started",
    Working => "working",
    NeedsYou => "needs_you",
    Question => "question",
    Answered => "answered",
    Finished => "finished",
    Failed => "failed",
    Limit => "limit",
    Quiet => "quiet",
    Stopped => "stopped",
    Interrupted => "interrupted",
    Resumed => "resumed",
    Restarted => "restarted",
    Nudged => "nudged",
    Approved => "approved",
    Denied => "denied",
    Suggested => "suggested",
    Dismissed => "dismissed",
    Summary => "summary",
    Assigned => "assigned",
    Replied => "replied",
    Delivered => "delivered",
    Steered => "steered",
    Paused => "paused",
    Continued => "continued",
    Budgeted => "budgeted",
    Asked => "asked",
    Told => "told",
    TakenOn => "taken_on",
    Released => "released",
    Driving => "driving",
    HandedBack => "handed_back",
    TicketCreated => "ticket_created",
    TicketMoved => "ticket_moved",
    TicketAssigned => "ticket_assigned",
    TicketEdited => "ticket_edited",
    TicketCommented => "ticket_commented",
    TicketDeleted => "ticket_deleted",
    Planned => "planned",
    PlanApproved => "plan_approved",
    PlanChanged => "plan_changed",
    PlanPaused => "plan_paused",
    PlanDropped => "plan_dropped",
    PlanDone => "plan_done",
    Moved => "moved",
    HandedOver => "handed_over",
    HandedOff => "handed_off",
}

/// One thing that happened on the crew.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub id: String,
    pub at: DateTime<Utc>,
    pub kind: Kind,
    pub actor: Party,
    #[serde(default)]
    pub subject: Option<Party>,
    pub text: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub suggestion_id: Option<String>,
    #[serde(default)]
    pub approval_id: Option<String>,
    /// Agent-to-agent hand-offs behind this event (the loop guard's count);
    /// the hub's own business, not on the wire.
    #[serde(skip)]
    pub hop: i16,
    /// What the overview reads back about the event: the task, the answer,
    /// the question and its choices. Not on the wire.
    #[serde(skip)]
    pub detail: Option<Value>,
    /// Where in Desk it happened, when in a room.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place: Option<crate::floor::Place>,
    /// The room message it is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<uuid::Uuid>,
    /// The board's ticket it is about: the ticket's history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<uuid::Uuid>,
    /// The sidebar group it was about when it was recorded (none:
    /// ungrouped): what an overseer's scope sees it by. Stamped as it is
    /// noted, so moving the item later never moves it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

/// Now, to the microsecond Postgres keeps.
pub fn now() -> DateTime<Utc> {
    let now = Utc::now();
    DateTime::from_timestamp_micros(now.timestamp_micros()).unwrap_or(now)
}

impl Event {
    /// A new event, now. `text` reads as what the actor did; for the
    /// overseer and you it names the subject.
    pub fn new(kind: Kind, actor: Party, subject: Option<Party>, text: impl Into<String>) -> Self {
        Self {
            id: new_id(),
            at: now(),
            kind,
            actor,
            subject,
            text: text.into(),
            reason: None,
            suggestion_id: None,
            approval_id: None,
            hop: 0,
            detail: None,
            place: None,
            message: None,
            ticket: None,
            group: None,
        }
    }

    /// The event, about the board's ticket `id` on a board of `group`: a
    /// ticket's events are its board's group's, whoever they name.
    pub fn about_ticket(mut self, id: uuid::Uuid, group: Option<&str>) -> Self {
        self.ticket = Some(id);
        self.group = group.map(str::to_string);
        self
    }

    /// The event, where in Desk it happened and the message it is about.
    pub fn at_place(
        mut self,
        place: Option<crate::floor::Place>,
        message: Option<uuid::Uuid>,
    ) -> Self {
        self.place = place;
        self.message = message;
        self
    }

    pub fn detail(mut self, detail: Value) -> Self {
        self.detail = Some(detail);
        self
    }

    pub fn reason(mut self, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        self.reason = (!reason.trim().is_empty()).then_some(reason);
        self
    }

    pub fn approval(mut self, id: impl ToString) -> Self {
        self.approval_id = Some(id.to_string());
        self
    }

    pub fn suggestion(mut self, id: Option<String>) -> Self {
        self.suggestion_id = id;
        self
    }

    pub fn hop(mut self, hop: i16) -> Self {
        self.hop = hop;
        self
    }

    /// Whether the "Needs you" filter shows it.
    pub fn needs_you(&self) -> bool {
        matches!(self.kind, Kind::NeedsYou | Kind::Question)
    }
}

/// Who is acting through the desk, and why: you through Desk's routes, the
/// overseer through its tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct By {
    pub party: Party,
    pub reason: Option<String>,
    pub suggestion_id: Option<String>,
    pub hop: i16,
    /// The person's preference a boss applied in doing it (H9), checked
    /// to be there and active: its decision cites it.
    pub preference: Option<uuid::Uuid>,
}

impl By {
    /// Desk itself, for `reason`: no boss or person did it.
    pub fn desk(reason: &str) -> Self {
        Self {
            party: Party::desk(),
            reason: Some(reason.to_string()).filter(|r| !r.trim().is_empty()),
            suggestion_id: None,
            hop: 0,
            preference: None,
        }
    }

    pub fn you() -> Self {
        Self {
            party: Party::you(),
            reason: None,
            suggestion_id: None,
            hop: 0,
            preference: None,
        }
    }

    pub fn manager(id: &str, name: &str, reason: &str) -> Self {
        Self {
            party: Party::manager(id, name),
            reason: Some(reason.to_string()).filter(|r| !r.trim().is_empty()),
            suggestion_id: None,
            hop: 0,
            preference: None,
        }
    }

    /// The event this actor's `kind` of action on `subject` makes.
    pub fn event(&self, kind: Kind, subject: Option<Party>, text: impl Into<String>) -> Event {
        let mut e = Event::new(kind, self.party.clone(), subject, text).hop(self.hop);
        e.reason = self.reason.clone();
        e.suggestion_id = self.suggestion_id.clone();
        e
    }
}

/// Which events a page of Activity holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Filter {
    #[default]
    All,
    NeedsYou,
    Manager,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ActivityPage {
    pub events: Vec<Event>,
    pub more: bool,
}

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A ULID: 26 characters that sort as the time they were made.
pub fn new_id() -> String {
    let ms = Utc::now().timestamp_millis().max(0) as u128;
    let random = u128::from_be_bytes(*uuid::Uuid::new_v4().as_bytes()) & ((1u128 << 80) - 1);
    let mut n = (ms << 80) | random;
    let mut out = [0u8; 26];
    for slot in out.iter_mut().rev() {
        *slot = CROCKFORD[(n & 31) as usize];
        n >>= 5;
    }
    String::from_utf8(out.to_vec()).expect("ascii")
}

// ── The overview ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemState {
    NeedsYou,
    Working,
    Quiet,
    Idle,
    Done,
    Failed,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    Session,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemApproval {
    pub id: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemQuestion {
    pub text: String,
    pub options: Option<Vec<String>>,
}

/// An agent or a session as the overview shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub kind: ItemKind,
    pub id: String,
    /// What the person calls it: a session's own name when they gave it
    /// one, else the hub's.
    pub name: String,
    /// The hub's name for a session the person renamed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hub_name: Option<String>,
    /// A session's folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The sidebar group it is in, by name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// The sidebar group it is in, by id: what a scope sees it by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    pub engine: Option<Engine>,
    pub origin: Option<Origin>,
    pub state: ItemState,
    pub task: Option<String>,
    pub detail: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub last_activity: Option<DateTime<Utc>>,
    pub approval: Option<ItemApproval>,
    pub question: Option<ItemQuestion>,
    /// Work it started that still runs after its turn: it is working.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<crate::floor::Background>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Who {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Brief {
    pub text: String,
    pub at: DateTime<Utc>,
    pub by: Who,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Counts {
    pub working: usize,
    pub needs_you: usize,
    pub done_today: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Overview {
    pub brief: Option<Brief>,
    pub counts: Counts,
    pub items: Vec<Item>,
    /// The sidebar's group names, in order.
    #[serde(default)]
    pub groups: Vec<String>,
}

/// The overseer, as `GET /api/desk/manager` says, and each sidebar group's
/// boss.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagerView {
    pub agent_id: Option<String>,
    pub level: Option<String>,
    /// Each sidebar group's boss, one per group; the main overseer is
    /// `agent_id` and never among them.
    pub bosses: Vec<GroupBoss>,
    /// Groups closed to everyone but the person: named for a boss that
    /// oversees nothing now, so neither it nor the main overseer sees them.
    pub sealed: Vec<String>,
}

/// A sidebar group's boss, as Desk shows it on the group and its row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupBoss {
    pub agent_id: String,
    pub name: String,
    pub group: String,
    /// `watch`, `suggest` or `act`.
    pub level: String,
}

/// The overseer as the desk needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overseer {
    pub id: String,
    pub name: String,
    /// `watch`, `suggest` or `act`.
    pub level: String,
    /// Working longer than this without a sign of life is quiet; 0 never.
    pub quiet_minutes: u32,
}

/// An agent's turn running now, as the hub sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveTurn {
    pub task: Option<String>,
    pub detail: Option<String>,
    pub since: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
}

/// One of the hub's agents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrewAgent {
    pub id: String,
    pub name: String,
    pub live: Option<LiveTurn>,
}

// ── Suggestions ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SuggestionState {
    Open,
    Done,
    Dismissed,
}

impl SuggestionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Done => "done",
            Self::Dismissed => "dismissed",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "done" => Self::Done,
            "dismissed" => Self::Dismissed,
            _ => Self::Open,
        }
    }
}

/// The crew tool whose suggestion changes an overseer's own oversight
/// settings: only the person who holds the hub may accept it.
pub const OWN_OVERSIGHT_TOOL: &str = "crew.ask_oversight_change";

/// Whether accepting `suggestion` is for the hub's holder alone: one that
/// changes an overseer's own limits.
pub fn holder_only(suggestion: &Suggestion) -> bool {
    suggestion.action["tool"].as_str() == Some(OWN_OVERSIGHT_TOOL)
}

/// What the overseer proposed: `action` is the tool call it would have made
/// (`{"tool": "crew.restart", "input": {…}}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Suggestion {
    pub id: String,
    pub at: DateTime<Utc>,
    pub text: String,
    pub action: Value,
    pub state: SuggestionState,
    pub subject: Option<Party>,
    #[serde(skip)]
    pub agent_id: String,
}

/// What an overseer's DM message mentions to act on in place.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extras {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approvals: Vec<MentionedApproval>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<MentionedSuggestion>,
    /// Questions sessions asked on Desk's line that the overseer gave the
    /// person, answered in place.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<MentionedQuestion>,
    /// The plan a plan card shows, as of the revision it was posted for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<MentionedPlan>,
    /// What the boss decided, each with its Undo where it has one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<crate::decision::DecisionLine>,
    /// What the boss asks of the person (needs you, still waiting), with
    /// its answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<crate::open_item::MentionedItem>,
    /// A boss's update, as the chat renders it in place of its text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<crate::update::MentionedUpdate>,
    /// A catch-up on the person's return: the absence it covers, its
    /// headline, and what it says beyond its `update`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub away: Option<crate::away::MentionedAway>,
}

impl Extras {
    pub fn is_empty(&self) -> bool {
        self.approvals.is_empty()
            && self.suggestions.is_empty()
            && self.questions.is_empty()
            && self.plan.is_none()
            && self.decisions.is_empty()
            && self.item.is_none()
            && self.update.is_none()
            && self.away.is_none()
    }
}

/// A plan, as a card in its boss's DM names it: Desk reads the plan, and
/// says when a newer revision has come since.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MentionedPlan {
    pub id: uuid::Uuid,
    pub revision: i32,
}

/// A question on a session's line, as the overseer's DM shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MentionedQuestion {
    pub message_id: String,
    pub session_id: String,
    pub session: String,
    pub ask: String,
    #[serde(default)]
    pub why: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MentionedApproval {
    pub session_id: String,
    pub approval_id: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MentionedSuggestion {
    pub id: String,
    pub text: String,
    pub state: SuggestionState,
}

/// Why a decision wasn't undone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoRefused {
    NotFound,
    /// Undone already: once is all.
    Already,
    /// It can't be undone (what ran is fixed forward), or the undo failed.
    Cant(String),
}

/// Why an open item wasn't answered or dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemRefused {
    NotFound,
    /// Closed already.
    Closed,
    Cant(String),
}

/// The hub's side of the crew: its agents, the overseer, and doing what a
/// suggestion proposed.
#[async_trait::async_trait]
pub trait Crew: Send + Sync {
    /// The agent overseeing the crew, if one does.
    fn overseer(&self) -> Option<Overseer>;
    /// The hub's agents, with the turn each is running.
    fn agents(&self) -> Vec<CrewAgent>;
    /// What the agent `id` is called, as the hub's roster says now; `None`
    /// when it has no name of its own (it is known only by its id) or
    /// isn't on the roster.
    fn agent_name(&self, id: &str) -> Option<String> {
        self.agents()
            .into_iter()
            .find(|a| a.id == id)
            .map(|a| a.name)
            .filter(|n| !n.trim().is_empty() && n != id)
    }
    /// The engine the boss `agent` starts sessions on when nothing says
    /// another (`[oversight.work] engine`), while it oversees.
    fn default_engine(&self, _agent: &str) -> Option<String> {
        None
    }
    /// The start of today, in the person's time.
    fn midnight(&self) -> DateTime<Utc>;
    /// The person's timezone, which days and weeks of spend are counted in.
    fn timezone(&self) -> chrono_tz::Tz {
        chrono_tz::UTC
    }
    /// Does what `suggestion` proposed, as you; what went wrong in plain
    /// words.
    async fn accept(&self, suggestion: &Suggestion) -> Result<(), String>;
    /// Undoes the boss's decision `id`, as you: what was done, or why it
    /// can't be, in plain words. [`UndoRefused::Already`] when it is
    /// already undone.
    async fn undo_decision(&self, _id: uuid::Uuid) -> Result<String, UndoRefused> {
        Err(UndoRefused::Cant(
            "There is no boss here to undo it.".into(),
        ))
    }
    /// Answers the boss's open item `id` with `answer`, as you: the item
    /// is closed here, in the same request, and the answer goes on to
    /// whoever asked.
    async fn answer_item(&self, _id: uuid::Uuid, _answer: &str) -> Result<String, ItemRefused> {
        Err(ItemRefused::Cant("There is no boss here to answer.".into()))
    }
    /// Drops the boss's open item `id` unanswered, as you.
    async fn drop_item(&self, _id: uuid::Uuid) -> Result<String, ItemRefused> {
        Err(ItemRefused::Cant("There is no boss here.".into()))
    }
    /// The person is typing in the boss `agent`'s DM: what it would post
    /// waits for a pause. False when `agent` is no boss here.
    fn dm_typing(&self, _agent: &str) -> bool {
        false
    }
    /// What Desk says of the person: whether its window is attended, and
    /// which boss's DM (by its agent) is on screen.
    fn presence(&self, _attending: bool, _dm_on_screen: Option<&str>) {}
    /// What still needs `person` (the desk's own person when none is
    /// named), from the floor; `None` where the hub keeps none.
    async fn open_needs(&self, _person: Option<&str>) -> Option<Vec<Need>> {
        None
    }
    /// Whether a session's last reply asks the person to decide, answer,
    /// approve or do something, as a cheap model reads it: the model of the
    /// overseer whose scope the session's `group` is in, so no other scope's
    /// model reads its words. `None` where there is none, or it couldn't say.
    async fn asks(&self, _reply: &str, _group: Option<&str>) -> Option<bool> {
        None
    }
    /// Whether the overseer answers what sessions ask it on Desk's line;
    /// otherwise their questions to it go to the person.
    fn answers_lines(&self) -> bool {
        false
    }
    /// Gives the agent `agent` a task in its Desk conversation, as you,
    /// as `crew.assign` does: what was done, or what went wrong, in plain
    /// words.
    async fn assign(&self, _agent: &str, _task: &str) -> Result<String, String> {
        Err("There are no agents here to give it to.".into())
    }

    /// Takes the boss off the group `group`, before it is deleted: every
    /// agent whose config names it is switched off and names it no more.
    /// What went wrong, in plain words.
    async fn release_group(&self, _group: &str) -> Result<(), String> {
        Ok(())
    }

    /// Asks the boss an item leaves to draft handover note `h` for the
    /// boss it goes to, in one `[Desk handover]` turn of its own, before
    /// `h.asked_until`; `facts` is Desk's note from the facts, which stands
    /// if it doesn't. Nothing where there's no crew.
    async fn ask_handover(&self, _h: &crate::moves::Handover, _facts: &str) {}

    /// Delivers handover note `h`, as the person sent it, to its new boss:
    /// one `[Desk handover]` prompt, text only. What went wrong, in plain
    /// words.
    async fn deliver_handover(&self, _h: &crate::moves::Handover) -> Result<(), String> {
        Err("There is no boss here to hand it to.".into())
    }

    /// Moves the boss `agent` to oversee `to` (none: everything ungrouped):
    /// what it remembered is first kept to the scope it leaves, then its
    /// `[oversight] group` is written; `replace` lets it take over a scope
    /// that has a boss, which is switched off. What went wrong, in plain
    /// words.
    async fn move_boss(
        &self,
        _agent: &str,
        _to: Option<&str>,
        _replace: bool,
    ) -> Result<(), String> {
        Err("Desk can't move a boss here.".into())
    }

    /// `assign`, as `by`: the overseer handing out a ticket is the overseer
    /// that the agent reads, and is held to its loop guards.
    async fn assign_as(&self, agent: &str, task: &str, _by: &By) -> Result<String, String> {
        self.assign(agent, task).await
    }
}

// ── Words ────────────────────────────────────────────────────────────────

/// What a boss with no name of its own is called where its name is shown.
pub const UNNAMED_BOSS: &str = "Boss";
/// What any other agent with no name of its own is called.
pub const UNNAMED_AGENT: &str = "Agent";

/// What the agent `id` is shown as: `name` when it is one of its own, never
/// its id; else [`UNNAMED_BOSS`] for a boss and [`UNNAMED_AGENT`] for the
/// rest.
pub fn shown_name(id: &str, name: Option<&str>, boss: bool) -> String {
    match name.map(str::trim) {
        Some(n) if !n.is_empty() && n != id => n.to_string(),
        _ if boss => UNNAMED_BOSS.into(),
        _ => UNNAMED_AGENT.into(),
    }
}

/// The first sentence (or line) of `text`, flattened and clipped to `max`
/// characters.
pub fn one_line(text: &str, max: usize) -> String {
    let line = text
        .lines()
        .map(|l| l.trim().trim_start_matches(['#', '>', '-', '*']).trim())
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    let sentence = line
        .char_indices()
        .find(|&(i, c)| {
            matches!(c, '.' | '!' | '?')
                && line[i + c.len_utf8()..]
                    .chars()
                    .next()
                    .is_none_or(char::is_whitespace)
        })
        .map(|(i, c)| &line[..i + c.len_utf8()])
        .unwrap_or(line);
    clip(sentence, max)
}

/// `text` cut to `max` characters, with an ellipsis when cut.
pub fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

/// The question a reply ends on, to the person: its last sentence when that
/// asks something. Code blocks and trailing sign-offs are not questions.
pub fn question_in(reply: &str) -> Option<String> {
    let mut prose = String::new();
    let mut fenced = false;
    for line in reply.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if !fenced {
            prose.push_str(line);
            prose.push('\n');
        }
    }
    let last = prose.lines().map(str::trim).rfind(|l| !l.is_empty())?;
    if !last.trim_end_matches(['*', '_', ')']).ends_with('?') {
        return None;
    }
    // The sentence the line ends on.
    let body = last.trim_end_matches(['*', '_']);
    let start = body[..body.len() - 1]
        .char_indices()
        .filter(|&(i, c)| {
            matches!(c, '.' | '!' | '?')
                && body[i + c.len_utf8()..]
                    .chars()
                    .next()
                    .is_some_and(char::is_whitespace)
        })
        .map(|(i, c)| i + c.len_utf8())
        .next_back()
        .unwrap_or(0);
    let q = body[start..]
        .trim()
        .trim_start_matches(['-', '*', '>'])
        .trim();
    (!q.is_empty()).then(|| clip(q, 240))
}

/// The choices a question offers, when they are clear: code or bold spans
/// it names, the short list just before it, or "A or B" between two single
/// words.
pub fn options_in(reply: &str, question: &str) -> Option<Vec<String>> {
    let spans = spans(question);
    if spans.len() >= 2 && spans.len() <= 6 {
        return Some(spans);
    }
    // A short list right before the question.
    let lines: Vec<&str> = reply.lines().map(str::trim).collect();
    let at = lines
        .iter()
        .rposition(|l| l.contains(question.trim_end_matches('?')))?;
    let mut listed = Vec::new();
    for l in lines[..at].iter().rev() {
        if l.is_empty() && listed.is_empty() {
            continue;
        }
        let item = l
            .strip_prefix("- ")
            .or_else(|| l.strip_prefix("* "))
            .or_else(|| {
                let (n, rest) = l.split_once(". ").or_else(|| l.split_once(") "))?;
                n.chars().all(|c| c.is_ascii_digit()).then_some(rest)
            });
        match item {
            Some(item) if item.chars().count() <= 80 => listed.push(clean(item)),
            _ => break,
        }
    }
    if (2..=6).contains(&listed.len()) {
        listed.reverse();
        return Some(listed);
    }
    either_or(question)
}

fn clean(s: &str) -> String {
    s.trim()
        .trim_matches(|c| matches!(c, '`' | '*' | '_' | '"' | '“' | '”'))
        .trim()
        .to_string()
}

/// `code` and **bold** spans, in order, each once.
fn spans(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for marker in ["`", "**"] {
        let parts: Vec<&str> = text.split(marker).collect();
        if parts.len() >= 5 {
            for span in parts.iter().skip(1).step_by(2) {
                let span = span.trim();
                if !span.is_empty() && span.chars().count() <= 60 && !out.iter().any(|o| o == span)
                {
                    out.push(span.to_string());
                }
            }
            if out.len() >= 2 {
                return out;
            }
            out.clear();
        }
    }
    out
}

const NOT_CHOICES: &[&str] = &[
    "it",
    "this",
    "that",
    "them",
    "not",
    "now",
    "later",
    "so",
    "too",
    "else",
    "either",
    "both",
    "something",
    "anything",
    "you",
    "me",
    "i",
    "we",
];

/// "deploy main or release/4.2 to staging?" → main, release/4.2.
fn either_or(question: &str) -> Option<Vec<String>> {
    let body = question.trim_end_matches('?');
    let mut halves = body.split(" or ");
    let (left, right) = (halves.next()?, halves.next()?);
    if halves.next().is_some() {
        return None;
    }
    let a = clean(left.split_whitespace().last()?.trim_end_matches(','));
    let b = clean(right.split_whitespace().next()?.trim_end_matches(','));
    let fine = |w: &str| {
        !w.is_empty()
            && w.chars().count() <= 40
            && !NOT_CHOICES.contains(&w.to_lowercase().as_str())
            && !w.ends_with("ing")
            && !w.ends_with("ed")
    };
    (fine(&a) && fine(&b) && a != b).then(|| vec![a, b])
}

/// Whether a reply says the work stopped on a usage or rate limit. The
/// last resort: a Claude Code session's limit is read from its
/// `StopFailure` and transcript ([`crate::limit`]) first.
pub fn hit_limit(reply: &str) -> bool {
    let r = reply.to_lowercase();
    crate::limit::says_plan_limit(reply)
        || [
            "usage limit",
            "limit reached",
            "limit will reset",
            "out of extra usage",
        ]
        .iter()
        .any(|p| r.contains(p))
}

/// How long an answer's line is kept.
const ANSWER_CHARS: usize = 120;

/// What a turn's answer (or, as `Err`, its failure) means: how it ended,
/// its first line, and the question it leaves the person.
pub fn outcome_of(answer: Result<&str, &str>) -> (Outcome, Option<String>, Option<ItemQuestion>) {
    let (Ok(text) | Err(text)) = answer;
    let line = Some(one_line(text, ANSWER_CHARS)).filter(|l| !l.is_empty());
    match answer {
        _ if hit_limit(text) => (Outcome::Limit, line, None),
        Ok(reply) => match question_in(reply) {
            Some(q) => {
                let options = options_in(reply, &q);
                (
                    Outcome::Asked,
                    line,
                    Some(ItemQuestion { text: q, options }),
                )
            }
            None => (Outcome::Finished, line, None),
        },
        Err(_) => (Outcome::Failed, line, None),
    }
}

/// The activity a turn's end is recorded as.
pub fn ended_kind(outcome: Outcome) -> Kind {
    match outcome {
        Outcome::Finished => Kind::Finished,
        Outcome::Asked => Kind::Question,
        Outcome::Limit => Kind::Limit,
        Outcome::Stopped => Kind::Stopped,
        Outcome::Failed | Outcome::Interrupted => Kind::Failed,
    }
}

/// What a session's pending request asks, as a question: "Run npm install …?"
pub fn approval_summary(tool: &str, detail: &str) -> String {
    let name = tool.split('(').next().unwrap_or(tool).trim();
    let detail = detail.trim();
    match name {
        "Input" | "Question" => detail.to_string(),
        "Plan" if detail.is_empty() => "Approve the plan?".to_string(),
        "Plan" => format!("Approve the plan: {detail}?"),
        "Bash" if !detail.is_empty() => format!("Run {detail}?"),
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" if !detail.is_empty() => {
            format!("Edit {detail}?")
        }
        "Read" if !detail.is_empty() => format!("Read {detail}?"),
        "WebFetch" if !detail.is_empty() => format!("Fetch {detail}?"),
        _ if detail.is_empty() => format!("Use {name}?"),
        _ => format!("Use {name} on {detail}?"),
    }
}

/// The overview's word for a session's state.
pub fn session_state(state: SessionState) -> ItemState {
    match state {
        SessionState::Blocked | SessionState::Asking => ItemState::NeedsYou,
        SessionState::Starting | SessionState::Working => ItemState::Working,
        SessionState::Done => ItemState::Done,
        SessionState::Idle => ItemState::Idle,
        SessionState::Interrupted | SessionState::Stopped | SessionState::Closed => {
            ItemState::Stopped
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agent_is_shown_by_its_own_name_and_never_by_its_id() {
        assert_eq!(shown_name("sarah", Some("Sarah"), true), "Sarah");
        assert_eq!(shown_name("scout", Some("Scout"), false), "Scout");
        for name in [None, Some(""), Some("  "), Some("sarah")] {
            assert_eq!(shown_name("sarah", name, true), UNNAMED_BOSS, "{name:?}");
            assert_eq!(shown_name("sarah", name, false), UNNAMED_AGENT, "{name:?}");
        }
    }

    #[test]
    fn ids_sort_as_they_were_made() {
        let a = new_id();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = new_id();
        assert_eq!(a.len(), 26);
        assert!(a < b, "{a} {b}");
    }

    #[test]
    fn a_reply_ending_on_a_question_asks_it() {
        let reply = "I've built release/4.2.\n\nShould I deploy main or release/4.2 to staging?";
        let q = question_in(reply).unwrap();
        assert_eq!(q, "Should I deploy main or release/4.2 to staging?");
        assert_eq!(
            options_in(reply, &q),
            Some(vec!["main".to_string(), "release/4.2".to_string()])
        );
        assert_eq!(question_in("All green. Pushed."), None);
        assert_eq!(
            question_in("Done.\n```sh\nwhy?\n```\nAll good."),
            None,
            "code is not a question"
        );
        assert_eq!(
            question_in("Tests pass. Want me to open the PR?").as_deref(),
            Some("Want me to open the PR?")
        );
    }

    #[test]
    fn clear_choices_are_offered_and_vague_ones_are_not() {
        let q = "Which one: `blue` or `green`?";
        assert_eq!(
            options_in(q, q),
            Some(vec!["blue".to_string(), "green".to_string()])
        );
        let listed = "I can:\n- Roll back\n- Patch forward\n\nWhich would you like?";
        assert_eq!(
            options_in(listed, &question_in(listed).unwrap()),
            Some(vec!["Roll back".to_string(), "Patch forward".to_string()])
        );
        let vague = "Should I keep going or stop?";
        assert_eq!(options_in(vague, vague), None);
        let open = "What should the release notes say?";
        assert_eq!(options_in(open, open), None);
    }

    #[test]
    fn one_line_is_the_first_sentence_clipped() {
        assert_eq!(one_line("## Plan\nCompare spend. Then report.", 80), "Plan");
        assert_eq!(
            one_line("Compare spend. Then report.", 80),
            "Compare spend."
        );
        assert_eq!(one_line("abcdefghij", 5), "abcd…");
    }

    #[test]
    fn an_answer_means_finished_asked_limited_or_failed() {
        assert_eq!(
            outcome_of(Ok("Fixed it. 214 tests pass.")),
            (Outcome::Finished, Some("Fixed it.".into()), None)
        );
        let reply = "I can:\n- Roll back\n- Patch forward\n\nWhich would you like?";
        assert_eq!(
            outcome_of(Ok(reply)),
            (
                Outcome::Asked,
                Some("I can:".into()),
                Some(ItemQuestion {
                    text: "Which would you like?".into(),
                    options: Some(vec!["Roll back".into(), "Patch forward".into()])
                })
            )
        );
        assert_eq!(
            outcome_of(Ok("You've hit your usage limit.")).0,
            Outcome::Limit
        );
        assert_eq!(
            outcome_of(Err("rate limit reached, try later")).0,
            Outcome::Limit
        );
        // A 429 that isn't the plan's says "rate limit" too: not a limit.
        assert_eq!(
            outcome_of(Err("API Error: Request rejected (429) · This request would exceed your account's rate limit.")).0,
            Outcome::Failed
        );
        // As Claude Code 2.1.31x says its plan's own limits.
        for said in [
            "You've hit your weekly limit · resets Fri 9am",
            "You've hit your session limit · resets 9am (Europe/London)",
            "You've hit your 5-hour limit · resets 2:20pm",
        ] {
            assert_eq!(outcome_of(Ok(said)).0, Outcome::Limit, "{said}");
        }
        assert_eq!(
            outcome_of(Err("connection refused")),
            (Outcome::Failed, Some("connection refused".into()), None)
        );
        assert_eq!(outcome_of(Ok("  ")), (Outcome::Finished, None, None));
    }

    #[test]
    fn requests_read_as_questions() {
        assert_eq!(
            approval_summary(
                "Bash(npm install @acme/ds@4.2.0)",
                "npm install @acme/ds@4.2.0"
            ),
            "Run npm install @acme/ds@4.2.0?"
        );
        assert_eq!(approval_summary("Edit(a.rs)", "a.rs"), "Edit a.rs?");
        assert_eq!(
            approval_summary("Input", "Claude needs your permission"),
            "Claude needs your permission"
        );
    }
}
