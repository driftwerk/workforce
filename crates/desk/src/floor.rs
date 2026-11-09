//! The floor: what the person's agents and sessions are doing, what they
//! finished, and what still needs the person, folded from a log of turns.
//! Types and pure functions only; the log itself is the hub's.

use std::collections::{BTreeMap, HashMap, HashSet};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::crew::{Counts, ItemQuestion, PartyKind};
use crate::gist::{self, Doing, Gist, Happening, Paragraph, Said};
use crate::model::{text_enum, Engine};

/// Whose work a turn is: an agent's (any of its sessions) or one of Desk's
/// Claude Code / Codex sessions on this hub.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Subject {
    Agent {
        agent_id: String,
        session_id: String,
    },
    Session {
        id: Uuid,
        engine: Engine,
    },
}

/// What a question is scoped to: an agent across all its sessions, or one
/// Desk session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SubjectKey(String);

impl SubjectKey {
    pub fn agent(agent_id: &str) -> Self {
        Self(format!("agent:{agent_id}"))
    }

    pub fn session(id: Uuid) -> Self {
        Self(format!("session:{id}"))
    }

    /// A key as the log keeps it.
    pub fn kept(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Subject {
    pub fn key(&self) -> SubjectKey {
        match self {
            Self::Agent { agent_id, .. } => SubjectKey::agent(agent_id),
            Self::Session { id, .. } => SubjectKey::session(*id),
        }
    }

    fn who(&self, name: &str) -> Who {
        let (kind, id, engine) = match self {
            Self::Agent { agent_id, .. } => (PartyKind::Agent, agent_id.clone(), None),
            Self::Session { id, engine } => (PartyKind::Session, id.to_string(), Some(*engine)),
        };
        Who {
            kind,
            id,
            name: name.to_string(),
            engine,
        }
    }
}

/// Where a turn, and anything it asks, can be found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Place {
    Dm,
    Room {
        room: Uuid,
        thread: Option<Uuid>,
    },
    Call,
    Session,
    Slack {
        team: Option<String>,
        channel: String,
        thread: Option<String>,
    },
    Elsewhere,
}

text_enum!(Outcome {
    Finished => "finished",
    Asked => "asked",
    Failed => "failed",
    Stopped => "stopped",
    Limit => "limit",
    Interrupted => "interrupted",
});

/// How a need can be answered in place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Answer {
    Decide {
        at: DecideRef,
    },
    /// Only the terminal can answer: a tool request in a session whose
    /// approvals are the terminal's.
    Terminal,
    Reply,
}

/// What a decision is made on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum DecideRef {
    /// A Desk session's tool request.
    Session { session_id: Uuid, approval_id: Uuid },
    /// An agent's prompt in its live turn.
    Prompt {
        agent_id: String,
        session_id: String,
        request_id: String,
    },
    /// A routed approval the person is an approver of.
    Routed { request_id: String },
    /// A change made to an agent's settings file outside the hub, by the
    /// hash of what the file held when it was shown: Allow uses it, Deny
    /// keeps the saved one.
    Config { agent_id: String, hash: String },
    /// A Codex session's request, answered with one of what Codex offers
    /// for it (an older Desk offers allow and deny, as for a session's).
    Codex {
        session_id: Uuid,
        approval_id: Uuid,
        choices: Vec<crate::model::Decision>,
    },
    /// A breaker holding a session, an agent or a pool's new starts
    /// (`session:<id>`, `agent:<id>`, `pool:desk`, `pool:group:<id>`):
    /// Allow is Continue, Deny is Keep paused. Only the person answers it.
    Breaker { subject: String },
    /// A boss's plan, or a change to it, as of `revision`: Allow is
    /// Approve and run, Deny keeps it as it is for the person to open.
    /// Only the person answers it.
    Plan { plan: Uuid, revision: i32 },
}

/// A stretch of a turn that waited on the person: one per ask.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Wait {
    pub key: String,
    pub from: DateTime<Utc>,
    pub to: Option<DateTime<Utc>>,
    pub ask: String,
    pub answer: Answer,
    /// The answers a question it asks offers, as its terminal lists them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
}

/// Work a turn started that runs on while the turn itself waits for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Background {
    pub count: u32,
    pub since: DateTime<Utc>,
}

/// A stretch the turn spent idle while its background work ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Idle {
    pub from: DateTime<Utc>,
    pub to: Option<DateTime<Utc>>,
    pub count: u32,
}

/// One turn of work, as the log keeps it.
#[derive(Debug, Clone, PartialEq)]
pub struct Turn {
    pub id: Uuid,
    /// `None`: a Desk session on a personal hub.
    pub owner: Option<String>,
    pub subject: Subject,
    /// Who, as shown when recorded (re-labelled at read).
    pub name: String,
    pub place: Place,
    pub task: Option<String>,
    pub doing: Option<String>,
    pub started_at: DateTime<Utc>,
    pub heard_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub outcome: Option<Outcome>,
    pub answer: Option<String>,
    pub question: Option<ItemQuestion>,
    /// When its question stopped needing the person.
    pub settled_at: Option<DateTime<Utc>>,
    pub waits: Vec<Wait>,
    /// Where it waited on its own background work.
    pub idle: Vec<Idle>,
    /// The session's turn token (agents), for liveness.
    pub token: Option<String>,
    /// The hub instance running it (agents), for liveness.
    pub instance: Option<String>,
}

impl Turn {
    /// Asked the person something they haven't answered.
    pub fn asking(&self) -> bool {
        self.outcome == Some(Outcome::Asked) && self.settled_at.is_none()
    }

    fn end(
        &mut self,
        at: DateTime<Utc>,
        outcome: Outcome,
        answer: Option<String>,
        question: Option<ItemQuestion>,
    ) {
        self.ended_at = Some(at);
        self.outcome = Some(outcome);
        self.answer = answer;
        self.question = question;
        self.waits
            .iter_mut()
            .filter(|w| w.to.is_none())
            .for_each(|w| w.to = Some(at));
        self.idled(0, at);
    }

    /// Its main turn is idle with `count` pieces of background work
    /// running, from `at`; none closes the stretch.
    fn idled(&mut self, count: u32, at: DateTime<Utc>) {
        match (self.idle.iter_mut().find(|i| i.to.is_none()), count) {
            (Some(open), 0) => open.to = Some(at),
            (Some(open), count) => open.count = count,
            (None, 0) => {}
            (None, count) => self.idle.push(Idle {
                from: at,
                to: None,
                count,
            }),
        }
    }

    /// The background work it is waiting on now.
    pub fn background(&self) -> Option<Background> {
        self.idle
            .iter()
            .find(|i| i.to.is_none())
            .map(|i| Background {
                count: i.count,
                since: i.from,
            })
    }
}

/// One write to the log, in the order a turn makes them.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnOp {
    /// `by_person`: the person started this turn themselves, which settles
    /// the subject's open questions.
    Begin {
        turn: Box<Turn>,
        by_person: bool,
    },
    Progress {
        turn: Uuid,
        doing: Option<String>,
        at: DateTime<Utc>,
    },
    Wait {
        turn: Uuid,
        wait: Wait,
    },
    Unwait {
        turn: Uuid,
        key: String,
        at: DateTime<Utc>,
    },
    /// The turn is idle with `count` pieces of its background work running
    /// (none: it is not).
    Background {
        turn: Uuid,
        count: u32,
        at: DateTime<Utc>,
    },
    End {
        turn: Uuid,
        at: DateTime<Utc>,
        outcome: Outcome,
        answer: Option<String>,
        question: Option<ItemQuestion>,
    },
    /// The subject's open questions stop needing the person (a session
    /// stopped).
    Settle {
        subject: SubjectKey,
        owner: Option<String>,
        at: DateTime<Utc>,
    },
    /// The subjects were never the person's (sessions the hub ran
    /// itself): every turn and gist of theirs goes.
    Forget {
        subjects: Vec<SubjectKey>,
    },
    /// The person had Desk forget what it remembers of the subjects: their
    /// gists go, and their turns stay without what they were asked, did or
    /// answered, so no gist is written from those again.
    ForgetGists {
        subjects: Vec<SubjectKey>,
    },
    /// `to` continues `from`'s work (a handoff): its gist starts with
    /// `from`'s purpose, so the floor doesn't learn the work again. One it
    /// has already is kept.
    CarryPurpose {
        from: SubjectKey,
        to: SubjectKey,
    },
    /// `from` is being forgotten: the purpose `to` were given from it goes,
    /// in those that haven't written their own since.
    ForgetCarried {
        from: SubjectKey,
        to: Vec<SubjectKey>,
    },
}

impl TurnOp {
    /// Whether applying it changes what the floor lists.
    pub fn lists(&self) -> bool {
        !matches!(self, Self::Progress { .. })
    }

    /// The one turn it writes to, if it writes to one already logged.
    pub fn turn(&self) -> Option<Uuid> {
        match self {
            Self::Progress { turn, .. }
            | Self::Wait { turn, .. }
            | Self::Unwait { turn, .. }
            | Self::Background { turn, .. }
            | Self::End { turn, .. } => Some(*turn),
            Self::Begin { .. }
            | Self::Settle { .. }
            | Self::Forget { .. }
            | Self::ForgetGists { .. }
            | Self::CarryPurpose { .. }
            | Self::ForgetCarried { .. } => None,
        }
    }
}

/// Whose questions an op settles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Subject {
        subject: SubjectKey,
        owner: Option<String>,
    },
    /// The subject and owner of this turn, other than the turn itself.
    EarlierThan(Uuid),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settling {
    pub target: Target,
    pub at: DateTime<Utc>,
}

/// The one rule for what clears a question: the person starting a turn
/// with its subject, the subject asking something newer, or the subject
/// going away.
pub fn settles(op: &TurnOp) -> Option<Settling> {
    match op {
        TurnOp::Begin {
            turn,
            by_person: true,
        } => Some(Settling {
            target: Target::Subject {
                subject: turn.subject.key(),
                owner: turn.owner.clone(),
            },
            at: turn.started_at,
        }),
        TurnOp::End {
            turn,
            at,
            outcome: Outcome::Asked,
            ..
        } => Some(Settling {
            target: Target::EarlierThan(*turn),
            at: *at,
        }),
        TurnOp::Settle { subject, owner, at } => Some(Settling {
            target: Target::Subject {
                subject: subject.clone(),
                owner: owner.clone(),
            },
            at: *at,
        }),
        _ => None,
    }
}

/// `turns` with `op` applied: the log's semantics, which every store keeps.
/// Progress, waits and ends only touch a turn still open.
pub fn apply(mut turns: Vec<Turn>, op: &TurnOp) -> Vec<Turn> {
    if let Some(s) = settles(op) {
        let whose = match &s.target {
            Target::Subject { subject, owner } => Some((subject.clone(), owner.clone(), None)),
            Target::EarlierThan(id) => turns
                .iter()
                .find(|t| t.id == *id)
                .map(|t| (t.subject.key(), t.owner.clone(), Some(*id))),
        };
        if let Some((subject, owner, except)) = whose {
            turns
                .iter_mut()
                .filter(|t| {
                    t.asking()
                        && t.subject.key() == subject
                        && t.owner == owner
                        && Some(t.id) != except
                })
                .for_each(|t| t.settled_at = Some(s.at));
        }
    }
    match op {
        TurnOp::Begin { turn, .. } => {
            if turns.iter().all(|t| t.id != turn.id) {
                turns.push(turn.as_ref().clone());
            }
        }
        TurnOp::Progress { turn, doing, at } => {
            if let Some(t) = open_turn(&mut turns, *turn) {
                t.doing = doing.clone().or(t.doing.take());
                t.heard_at = *at;
            }
        }
        TurnOp::Wait { turn, wait } => {
            if let Some(t) = open_turn(&mut turns, *turn) {
                t.heard_at = t.heard_at.max(wait.from);
                match t.waits.iter_mut().find(|w| w.key == wait.key) {
                    Some(w) => {
                        w.ask = wait.ask.clone();
                        w.answer = wait.answer.clone();
                        w.options = wait.options.clone();
                    }
                    None => t.waits.push(wait.clone()),
                }
            }
        }
        TurnOp::Unwait { turn, key, at } => {
            if let Some(t) = open_turn(&mut turns, *turn) {
                t.heard_at = t.heard_at.max(*at);
                t.waits
                    .iter_mut()
                    .filter(|w| &w.key == key && w.to.is_none())
                    .for_each(|w| w.to = Some(*at));
            }
        }
        TurnOp::Background { turn, count, at } => {
            if let Some(t) = open_turn(&mut turns, *turn) {
                t.idled(*count, *at);
            }
        }
        TurnOp::End {
            turn,
            at,
            outcome,
            answer,
            question,
        } => {
            if let Some(t) = open_turn(&mut turns, *turn) {
                t.end(*at, *outcome, answer.clone(), question.clone());
            }
        }
        TurnOp::Settle { .. } | TurnOp::CarryPurpose { .. } | TurnOp::ForgetCarried { .. } => {}
        TurnOp::Forget { subjects } => turns.retain(|t| !subjects.contains(&t.subject.key())),
        TurnOp::ForgetGists { subjects } => {
            for t in turns
                .iter_mut()
                .filter(|t| subjects.contains(&t.subject.key()))
            {
                t.task = None;
                t.doing = None;
                t.answer = None;
                t.question = None;
            }
        }
    }
    turns
}

fn open_turn(turns: &mut [Turn], id: Uuid) -> Option<&mut Turn> {
    turns
        .iter_mut()
        .find(|t| t.id == id && t.ended_at.is_none())
}

/// Which turns the person sees: their own, and on their personal hub its
/// Desk sessions too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub owner: String,
    pub personal: bool,
}

impl Scope {
    pub fn admits(&self, owner: Option<&str>) -> bool {
        match owner {
            Some(o) => o == self.owner,
            None => self.personal,
        }
    }
}

/// Where the log is kept. Writes are fire-and-forget through one ordered
/// writer; reads are the floor's.
#[async_trait]
pub trait TurnLog: Send + Sync {
    fn send(&self, op: TurnOp);
    /// Turns of `scope` started or ended since `since`, every open one, and
    /// every question still unanswered, however old.
    async fn relevant(&self, scope: &Scope, since: DateTime<Utc>) -> eyre::Result<Vec<Turn>>;
}

// ── What the floor shows ─────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Who {
    pub kind: PartyKind,
    pub id: String,
    pub name: String,
    pub engine: Option<Engine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NeedKind {
    Approval,
    Question,
}

/// Something waiting on the person now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Need {
    pub id: String,
    pub kind: NeedKind,
    pub who: Who,
    pub ask: String,
    /// What the answers do, in plain words, when the ask needs it said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    pub options: Option<Vec<String>>,
    /// A session's question: what it said it goes ahead with if no answer
    /// comes in time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chose: Option<String>,
    pub since: DateTime<Utc>,
    pub place: Place,
    pub answer: Answer,
    /// The sidebar group it is in now, stamped from the layout when the
    /// view is read; none when ungrouped, or where the hub doesn't hold
    /// the person's sidebar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// That group's own boss (its agent id), when it has one: none for an
    /// ungrouped item and one of a group with no boss of its own, which
    /// are the main boss's, and one of a sealed group, which is no one's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boss: Option<String>,
}

/// A question to the person its session went ahead without, waiting
/// to be answered late or set aside.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WentAhead {
    pub id: String,
    pub who: Who,
    pub ask: String,
    /// What the session said it would go ahead with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chose: Option<String>,
    pub asked_at: DateTime<Utc>,
    pub went_ahead_at: DateTime<Utc>,
}

/// A turn running, or one that finished.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Work {
    pub who: Who,
    pub what: String,
    pub place: Place,
    pub since: DateTime<Utc>,
    pub heard_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub outcome: Option<Outcome>,
    /// What it is waiting on of its own work, while it is.
    pub background: Option<Background>,
    /// What its agent or session is for and doing, in plain words.
    pub gist: Option<Said>,
    /// The sidebar group it is in now, stamped from the layout when the
    /// view is read; none when ungrouped, or where the hub doesn't hold
    /// the person's sidebar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// That group's own boss (its agent id), when it has one: none for an
    /// ungrouped item and one of a group with no boss of its own, which
    /// are the main boss's, and one of a sealed group, which is no one's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boss: Option<String>,
    /// The session's work was handed to it from another: where from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continued: Option<Continued>,
}

/// Where the work a session's tile shows came from: "Continued from
/// Claude Code at 10:42".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Continued {
    /// The session it was handed from.
    pub from: Uuid,
    /// That session's engine, when Desk still has it.
    pub engine: Option<Engine>,
    pub at: DateTime<Utc>,
}

impl Work {
    /// Where it stands, as its gist says it.
    pub fn doing(&self) -> Doing {
        match (self.ended_at, self.outcome, self.background) {
            (None, _, Some(b)) => Doing::Waiting(b.count),
            (None, _, None) => Doing::Working,
            (Some(_), Some(Outcome::Failed), _) => Doing::Failed,
            (Some(_), Some(Outcome::Limit), _) => Doing::Limit,
            (Some(_), _, _) => Doing::Finished,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanKind {
    Working,
    Waiting,
    /// Idle while its background work ran.
    Background,
}

/// A stretch of a lane; `to: None` runs to now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub kind: SpanKind,
    pub from: DateTime<Utc>,
    pub to: Option<DateTime<Utc>>,
}

/// One agent's or session's day.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lane {
    pub who: Who,
    pub spans: Vec<Span>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct More {
    pub done: usize,
    pub lanes: usize,
}

/// `GET /api/desk/floor`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FloorView {
    pub as_of: DateTime<Utc>,
    pub needs: Vec<Need>,
    /// Questions their sessions went ahead without: they no longer need
    /// the person, who may still look at them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub went_ahead: Vec<WentAhead>,
    pub working: Vec<Work>,
    pub done: Vec<Work>,
    pub lanes: Vec<Lane>,
    pub counts: Counts,
    pub more: More,
    /// The floor in one paragraph, when one has been written.
    pub summary: Option<Paragraph>,
    /// The overseers there are, each with its scope, for the person who
    /// holds the hub's Desk: none elsewhere.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bosses: Vec<BossView>,
    /// The groups closed to everyone but the person: named for a boss that
    /// isn't overseeing them now. Their items carry their group and no
    /// boss, and are not the main boss's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sealed: Vec<String>,
    /// The bosses' plans still open, each in its group, with where its
    /// tickets stand: for the person who holds the hub's Desk.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plans: Vec<PlanProgress>,
}

/// A plan as the Floor shows it in its group's section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanProgress {
    pub id: Uuid,
    pub goal: String,
    /// Its group, as it was made; none is ungrouped.
    pub group: Option<String>,
    /// Its boss's agent id, and what the boss is called.
    pub boss: String,
    pub boss_name: String,
    pub state: crate::plan::PlanState,
    /// Why it is paused, in the person's words, while it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_why: Option<String>,
    pub progress: crate::plan::Progress,
}

/// Whether a boss is in a turn now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BossState {
    Idle,
    Working,
}

/// An overseer, as the Floor shows it at the head of its section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BossView {
    pub scope: crate::scope::Scope,
    pub agent: String,
    pub name: String,
    /// `watch`, `suggest` or `act`.
    pub level: String,
    pub state: BossState,
    /// What holds its turns, in plain words, while something does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<String>,
    /// Its group's budget, the furthest along: none for the main boss, and
    /// for a group with no budget of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend: Option<workforce_core::budget::Standing>,
}

/// Each subject's gist, as kept.
pub type Gists = HashMap<SubjectKey, Gist>;

/// How many finished turns the floor lists.
pub const DONE_CAP: usize = 50;
/// How many lanes the floor lists.
pub const LANE_CAP: usize = 60;

/// `turns` with every open one not in `live` ended as interrupted when it
/// was last heard from, and its waits closed then.
pub fn settle(turns: Vec<Turn>, live: &HashSet<Uuid>) -> Vec<Turn> {
    turns
        .into_iter()
        .map(|mut t| {
            if t.ended_at.is_none() && !live.contains(&t.id) {
                let at = t.heard_at;
                let (answer, question) = (t.answer.take(), t.question.take());
                t.end(at, Outcome::Interrupted, answer, question);
            }
            t
        })
        .collect()
}

/// `turns` with every open wait of a session's open turn that nothing asks
/// now (`asks`: what each running session asks, by its waits' keys) closed
/// where its turn was last heard from: a session's wait stands only while
/// an ask of it stands. An agent's waits are its own hub's to keep.
pub fn backed(turns: Vec<Turn>, asks: &HashMap<Uuid, HashSet<String>>) -> Vec<Turn> {
    turns
        .into_iter()
        .map(|mut t| {
            if let (Subject::Session { id, .. }, None) = (&t.subject, t.ended_at) {
                let keys = asks.get(id);
                let at = t.heard_at;
                t.waits
                    .iter_mut()
                    .filter(|w| w.to.is_none() && keys.is_none_or(|k| !k.contains(&w.key)))
                    .for_each(|w| w.to = Some(at.max(w.from)));
            }
            t
        })
        .collect()
}

/// What still needs the person, longest waiting first: every open wait of
/// an open turn, and every question not yet settled.
pub fn open_needs(turns: &[Turn]) -> Vec<Need> {
    let waits = turns.iter().filter(|t| t.ended_at.is_none()).flat_map(|t| {
        t.waits
            .iter()
            .filter(|w| w.to.is_none())
            .map(move |w| Need {
                id: format!("wait:{}:{}", t.id, w.key),
                kind: match (&w.answer, &w.options) {
                    (Answer::Reply, _) | (_, Some(_)) => NeedKind::Question,
                    (Answer::Decide { .. } | Answer::Terminal, None) => NeedKind::Approval,
                },
                who: t.subject.who(&t.name),
                ask: w.ask.clone(),
                why: None,
                options: w.options.clone(),
                chose: None,
                since: w.from,
                place: t.place.clone(),
                answer: w.answer.clone(),
                group: None,
                boss: None,
            })
    });
    let questions = turns.iter().filter(|t| t.asking()).filter_map(|t| {
        let q = t.question.as_ref()?;
        Some(Need {
            id: format!("question:{}", t.id),
            kind: NeedKind::Question,
            who: t.subject.who(&t.name),
            ask: q.text.clone(),
            why: None,
            options: q.options.clone(),
            chose: None,
            since: t.ended_at.unwrap_or(t.started_at),
            place: t.place.clone(),
            answer: Answer::Reply,
            group: None,
            boss: None,
        })
    });
    longest_waiting(waits.chain(questions).collect())
}

/// `needs` in the order they are shown: longest waiting first.
pub fn longest_waiting(mut needs: Vec<Need>) -> Vec<Need> {
    needs.sort_by(|a, b| a.since.cmp(&b.since).then_with(|| a.id.cmp(&b.id)));
    needs
}

/// What each subject was first asked, of `turns`.
fn first_asks(turns: &[Turn]) -> HashMap<SubjectKey, &str> {
    let mut asked: Vec<&Turn> = turns.iter().filter(|t| t.task.is_some()).collect();
    asked.sort_by_key(|t| std::cmp::Reverse(t.started_at));
    asked
        .into_iter()
        .filter_map(|t| Some((t.subject.key(), t.task.as_deref()?)))
        .collect()
}

/// Where `t` stands.
fn doing_of(t: &Turn) -> Doing {
    match (t.ended_at, t.outcome, t.background()) {
        (None, _, Some(b)) => Doing::Waiting(b.count),
        (None, _, None) => Doing::Working,
        (Some(_), Some(Outcome::Failed), _) => Doing::Failed,
        (Some(_), Some(Outcome::Limit), _) => Doing::Limit,
        (Some(_), _, _) => Doing::Finished,
    }
}

/// `t`'s gist: its subject's as kept, unless that was written before `t`
/// began; else said plainly from `t`, keeping the purpose kept.
fn gist_of(t: &Turn, gists: &Gists, first: Option<&str>) -> Said {
    let kept = gists.get(&t.subject.key());
    if let Some(g) = kept.filter(|g| g.turn == t.id || g.now_at >= t.started_at) {
        return g.said();
    }
    let asks: Vec<String> = first.map(str::to_string).into_iter().collect();
    let (purpose, now) = gist::plainly(&gist::Input {
        asks: &asks,
        ask: t.task.as_deref(),
        reply: t.answer.as_deref(),
        doing: Some(doing_of(t)),
        ..Default::default()
    });
    Said {
        purpose: kept.and_then(|g| g.purpose.clone()).or(purpose),
        now,
        at: t.ended_at.unwrap_or(t.heard_at),
    }
}

fn work(t: &Turn, what: Option<&String>, gist: Said) -> Work {
    Work {
        who: t.subject.who(&t.name),
        what: what.or(t.task.as_ref()).cloned().unwrap_or_default(),
        place: t.place.clone(),
        since: t.started_at,
        heard_at: t.heard_at,
        ended_at: t.ended_at,
        outcome: t.outcome,
        background: t.background(),
        gist: Some(gist),
        group: None,
        boss: None,
        continued: None,
    }
}

/// The turns running now, newest first.
pub fn working(turns: &[Turn], gists: &Gists) -> Vec<Work> {
    let firsts = first_asks(turns);
    let mut out: Vec<Work> = turns
        .iter()
        .filter(|t| t.ended_at.is_none())
        .map(|t| {
            let gist = gist_of(t, gists, firsts.get(&t.subject.key()).copied());
            work(t, t.doing.as_ref(), gist)
        })
        .collect();
    out.sort_by(|a, b| b.since.cmp(&a.since));
    out
}

/// The turns that finished, failed or hit a limit since `from`, newest
/// first.
pub fn done(turns: &[Turn], from: DateTime<Utc>, gists: &Gists) -> Vec<Work> {
    let firsts = first_asks(turns);
    let mut out: Vec<Work> = turns
        .iter()
        .filter(|t| {
            matches!(
                t.outcome,
                Some(Outcome::Finished | Outcome::Failed | Outcome::Limit)
            ) && t.ended_at.is_some_and(|at| at >= from)
        })
        .map(|t| {
            let gist = gist_of(t, gists, firsts.get(&t.subject.key()).copied());
            work(t, t.answer.as_ref(), gist)
        })
        .collect();
    out.sort_by(|a, b| b.ended_at.cmp(&a.ended_at));
    out
}

/// A turn's own spans: its run, each wait, and an unanswered question's
/// wait from its end until it was settled.
fn spans_of(t: &Turn) -> impl Iterator<Item = Span> + '_ {
    let run = Span {
        kind: SpanKind::Working,
        from: t.started_at,
        to: t.ended_at,
    };
    let waits = t.waits.iter().map(|w| Span {
        kind: SpanKind::Waiting,
        from: w.from,
        to: w.to,
    });
    let asked = (t.outcome == Some(Outcome::Asked))
        .then_some(t.ended_at)
        .flatten()
        .map(|from| Span {
            kind: SpanKind::Waiting,
            from,
            to: t.settled_at,
        });
    let idle = t.idle.iter().map(|i| Span {
        kind: SpanKind::Background,
        from: i.from,
        to: i.to,
    });
    std::iter::once(run).chain(waits).chain(asked).chain(idle)
}

/// `spans` clipped to start no earlier than `day`, merged per kind where
/// they touch, in order of start.
fn merged(spans: Vec<Span>, day: DateTime<Utc>, now: DateTime<Utc>) -> Vec<Span> {
    let end = |s: &Span| s.to.unwrap_or(now);
    let mut clipped: Vec<Span> = spans
        .into_iter()
        .filter(|s| end(s) >= day)
        .map(|s| Span {
            from: s.from.max(day),
            ..s
        })
        .collect();
    clipped.sort_by(|a, b| (a.kind as u8, a.from).cmp(&(b.kind as u8, b.from)));
    let mut out: Vec<Span> = clipped.into_iter().fold(Vec::new(), |mut acc, s| {
        match acc.last_mut() {
            Some(last) if last.kind == s.kind && s.from <= end(last) => {
                last.to = match (last.to, s.to) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    _ => None,
                };
            }
            _ => acc.push(s),
        }
        acc
    });
    out.sort_by(|a, b| {
        a.from
            .cmp(&b.from)
            .then((a.kind as u8).cmp(&(b.kind as u8)))
    });
    out
}

/// One lane per subject with anything on `day`, the most recently active
/// first.
pub fn lanes(turns: &[Turn], day: DateTime<Utc>, now: DateTime<Utc>) -> Vec<Lane> {
    let mut by_subject: BTreeMap<SubjectKey, Vec<&Turn>> = BTreeMap::new();
    turns
        .iter()
        .for_each(|t| by_subject.entry(t.subject.key()).or_default().push(t));
    let mut lanes: Vec<(DateTime<Utc>, Lane)> = by_subject
        .into_values()
        .filter_map(|ts| {
            let newest = ts.iter().max_by_key(|t| t.started_at)?;
            let spans = merged(ts.iter().flat_map(|t| spans_of(t)).collect(), day, now);
            let last = spans.iter().map(|s| s.to.unwrap_or(now)).max()?;
            Some((
                last,
                Lane {
                    who: newest.subject.who(&newest.name),
                    spans,
                },
            ))
        })
        .collect();
    lanes.sort_by(|(a, x), (b, y)| b.cmp(a).then_with(|| x.who.name.cmp(&y.who.name)));
    lanes.into_iter().map(|(_, lane)| lane).collect()
}

/// The floor's counts; `done` holds at least everything since `day`.
pub fn counts(needs: &[Need], working: &[Work], done: &[Work], day: DateTime<Utc>) -> Counts {
    Counts {
        working: working.len(),
        needs_you: needs.len(),
        done_today: done
            .iter()
            .filter(|w| w.ended_at.is_some_and(|at| at >= day))
            .count(),
    }
}

/// The floor as of `now` for the person's `day`, from settled `turns`, the
/// `needs` they and the hub's other sources make, and the subjects' `gists`.
pub fn view(
    turns: &[Turn],
    needs: Vec<Need>,
    gists: &Gists,
    day: DateTime<Utc>,
    now: DateTime<Utc>,
) -> FloorView {
    let working = working(turns, gists);
    let recent = now - chrono::Duration::hours(24);
    let finished = done(turns, day.min(recent), gists);
    let counts = counts(&needs, &working, &finished, day);
    let done: Vec<Work> = finished
        .into_iter()
        .filter(|w| w.ended_at.is_some_and(|at| at >= recent))
        .collect();
    let lanes = lanes(turns, day, now);
    let more = More {
        done: done.len().saturating_sub(DONE_CAP),
        lanes: lanes.len().saturating_sub(LANE_CAP),
    };
    FloorView {
        as_of: now,
        needs,
        went_ahead: Vec::new(),
        working,
        done: done.into_iter().take(DONE_CAP).collect(),
        lanes: lanes.into_iter().take(LANE_CAP).collect(),
        counts,
        more,
        summary: None,
        bosses: Vec::new(),
        sealed: Vec::new(),
        plans: Vec::new(),
    }
}

/// `view` with every need and every piece of work stamped with the group
/// its agent or session is in now (`group_of`), and that group's own boss
/// (`boss_of`, by group id), so the Floor shows where each is after a move.
pub fn stamp(
    view: &mut FloorView,
    group_of: impl Fn(&Who) -> Option<String>,
    boss_of: impl Fn(&str) -> Option<String>,
) {
    let place = |who: &Who| {
        let group = group_of(who);
        let boss = group.as_deref().and_then(&boss_of);
        (group, boss)
    };
    for n in &mut view.needs {
        let planned = match &n.answer {
            Answer::Decide {
                at: DecideRef::Plan { .. },
            } => true,
            Answer::Decide {
                at: DecideRef::Breaker { subject },
            } => subject.starts_with("plan:"),
            _ => false,
        };
        (n.group, n.boss) = match planned {
            // A plan is its own group's, as it was made, wherever its boss
            // is now: the group it carries, with that group's boss now. So
            // is what holds its new starts.
            true => {
                let group = n.group.take();
                let boss = group.as_deref().and_then(&boss_of);
                (group, boss)
            }
            false => place(&n.who),
        };
    }
    for w in view.working.iter_mut().chain(view.done.iter_mut()) {
        (w.group, w.boss) = place(&w.who);
    }
}

/// `view` with each piece of work on one tile: the sessions whose work
/// went on in another (`handed`) are folded under the one it continues in,
/// shown neither working nor done, and a session handed work says where
/// from (`from`, by session). `day` is the person's, which the counts are
/// of.
pub fn fold_work(
    view: &mut FloorView,
    handed: &HashSet<Uuid>,
    from: &HashMap<Uuid, Continued>,
    day: DateTime<Utc>,
) {
    let session = |w: &Work| match w.who.kind {
        PartyKind::Session => w.who.id.parse::<Uuid>().ok(),
        _ => None,
    };
    let folded = |w: &Work| session(w).is_some_and(|s| handed.contains(&s));
    let working = view.working.len();
    view.working.retain(|w| !folded(w));
    let today = view
        .done
        .iter()
        .filter(|w| folded(w) && w.ended_at.is_some_and(|at| at >= day))
        .count();
    view.done.retain(|w| !folded(w));
    view.counts.working = view
        .counts
        .working
        .saturating_sub(working - view.working.len());
    view.counts.done_today = view.counts.done_today.saturating_sub(today);
    for w in view.working.iter_mut().chain(view.done.iter_mut()) {
        if let Some(c) = session(w).and_then(|s| from.get(&s)) {
            w.continued = Some(c.clone());
        }
    }
}

/// Whether the boss `agent` is in a turn on `view` now.
pub fn boss_state(view: &FloorView, agent: &str) -> BossState {
    let working = view
        .working
        .iter()
        .any(|w| matches!(w.who.kind, PartyKind::Agent | PartyKind::Manager) && w.who.id == agent);
    match working {
        true => BossState::Working,
        false => BossState::Idle,
    }
}

/// What's happening on the floor: one entry per subject working, waiting
/// on its background work, or finished within [`gist::DONE_FOR`]; working
/// first, then waiting, then finished, each most recently said first.
pub fn happening(view: &FloorView, now: DateTime<Utc>) -> Vec<Happening> {
    let recent = now - gist::DONE_FOR;
    let lately = view
        .done
        .iter()
        .filter(|w| w.ended_at.is_some_and(|at| at >= recent));
    let mut by: HashMap<(PartyKind, &str), (Happening, DateTime<Utc>)> = HashMap::new();
    let theirs =
        |w: &&Work| w.who.kind == PartyKind::Session && gist::agent_turn_folder(&w.who.name);
    for w in view.working.iter().chain(lately).filter(|w| !theirs(w)) {
        let entry = Happening {
            who: w.who.clone(),
            doing: w.doing(),
            gist: w.gist.clone().unwrap_or_else(|| Said {
                purpose: None,
                now: w.what.clone(),
                at: w.ended_at.unwrap_or(w.heard_at),
            }),
        };
        let at = w.ended_at.unwrap_or(w.since);
        let rank = |e: &Happening, at: DateTime<Utc>| (e.doing.rank(), std::cmp::Reverse(at));
        match by.get(&(w.who.kind, w.who.id.as_str())) {
            Some((have, have_at)) if rank(have, *have_at) <= rank(&entry, at) => {}
            _ => {
                by.insert((w.who.kind, w.who.id.as_str()), (entry, at));
            }
        }
    }
    let mut out: Vec<Happening> = by.into_values().map(|(e, _)| e).collect();
    out.sort_by(|a, b| {
        (a.doing.rank(), std::cmp::Reverse(a.gist.at), &a.who.name).cmp(&(
            b.doing.rank(),
            std::cmp::Reverse(b.gist.at),
            &b.who.name,
        ))
    });
    out
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use serde_json::json;

    use super::*;

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339).unwrap().to_utc()
    }

    fn agent(id: &str, session: &str) -> Subject {
        Subject::Agent {
            agent_id: id.into(),
            session_id: session.into(),
        }
    }

    fn turn(subject: Subject, owner: Option<&str>, from: &str) -> Turn {
        Turn {
            id: Uuid::new_v4(),
            owner: owner.map(str::to_string),
            subject,
            name: "Scout".into(),
            place: Place::Dm,
            task: Some("Compare spend.".into()),
            doing: None,
            started_at: at(from),
            heard_at: at(from),
            ended_at: None,
            outcome: None,
            answer: None,
            question: None,
            settled_at: None,
            waits: vec![],
            idle: vec![],
            token: None,
            instance: None,
        }
    }

    fn ended(mut t: Turn, to: &str, outcome: Outcome) -> Turn {
        t.ended_at = Some(at(to));
        t.outcome = Some(outcome);
        t.answer = Some("Spend is flat.".into());
        if outcome == Outcome::Asked {
            t.question = Some(ItemQuestion {
                text: "Ship it?".into(),
                options: None,
            });
        }
        t
    }

    fn wait(key: &str, from: &str, to: Option<&str>, answer: Answer) -> Wait {
        Wait {
            key: key.into(),
            from: at(from),
            to: to.map(at),
            ask: "Run cargo test?".into(),
            answer,
            options: None,
        }
    }

    fn decide() -> Answer {
        Answer::Decide {
            at: DecideRef::Session {
                session_id: Uuid::nil(),
                approval_id: Uuid::nil(),
            },
        }
    }

    #[test]
    fn a_work_handed_on_is_one_tile_saying_where_it_came_from() {
        let (claude, codex, other) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let session = |id, engine| Subject::Session { id, engine };
        let turns = vec![
            ended(
                turn(
                    session(claude, Engine::Claude),
                    None,
                    "2026-10-29T09:00:00Z",
                ),
                "2026-10-29T10:00:00Z",
                Outcome::Finished,
            ),
            turn(session(codex, Engine::Codex), None, "2026-10-29T10:42:00Z"),
            turn(session(other, Engine::Claude), None, "2026-10-29T10:50:00Z"),
        ];
        let (day, now) = (at("2026-10-29T00:00:00Z"), at("2026-10-29T11:00:00Z"));
        let mut v = view(&turns, vec![], &Gists::new(), day, now);
        assert_eq!((v.counts.working, v.counts.done_today), (2, 1));
        let continued = Continued {
            from: claude,
            engine: Some(Engine::Claude),
            at: at("2026-10-29T10:42:00Z"),
        };
        fold_work(
            &mut v,
            &[claude].into(),
            &[(codex, continued.clone())].into(),
            day,
        );
        assert!(v.done.is_empty(), "the handed-off session isn't shown done");
        assert_eq!((v.counts.working, v.counts.done_today), (2, 0));
        let tile = v
            .working
            .iter()
            .find(|w| w.who.id == codex.to_string())
            .unwrap();
        assert_eq!(tile.continued.as_ref(), Some(&continued));
        let plain = v
            .working
            .iter()
            .find(|w| w.who.id == other.to_string())
            .unwrap();
        assert_eq!(plain.continued, None);
        assert_eq!(
            serde_json::to_value(tile).unwrap()["continued"]["engine"],
            json!("claude")
        );
    }

    #[test]
    fn a_turn_that_waits_on_the_person_was_heard_from_then() {
        let t = turn(agent("scout", "s1"), Some("u1"), "2026-10-29T09:00:00Z");
        let waited = apply(
            vec![t.clone()],
            &TurnOp::Wait {
                turn: t.id,
                wait: wait("r1", "2026-10-29T09:10:00Z", None, decide()),
            },
        );
        assert_eq!(waited[0].heard_at, at("2026-10-29T09:10:00Z"));
        let answered = apply(
            waited,
            &TurnOp::Unwait {
                turn: t.id,
                key: "r1".into(),
                at: at("2026-10-29T09:12:00Z"),
            },
        );
        assert_eq!(answered[0].heard_at, at("2026-10-29T09:12:00Z"));
        let lost = settle(answered, &HashSet::new());
        assert!(
            lost[0].waits.iter().all(|w| w.to >= Some(w.from)),
            "a turn lost after it waited never shows a wait ending before it began"
        );
    }

    #[test]
    fn open_turns_that_are_not_live_were_interrupted_when_last_heard() {
        let mut lost = turn(agent("scout", "s1"), Some("u1"), "2026-10-29T09:00:00Z");
        lost.heard_at = at("2026-10-29T09:20:00Z");
        lost.waits = vec![wait("r1", "2026-10-29T09:10:00Z", None, decide())];
        let live = turn(agent("scout", "s2"), Some("u1"), "2026-10-29T10:00:00Z");
        let finished = ended(
            turn(agent("scout", "s3"), Some("u1"), "2026-10-29T08:00:00Z"),
            "2026-10-29T08:30:00Z",
            Outcome::Finished,
        );
        let settled = settle(
            vec![lost.clone(), live.clone(), finished.clone()],
            &HashSet::from([live.id]),
        );
        assert_eq!(settled[0].outcome, Some(Outcome::Interrupted));
        assert_eq!(settled[0].ended_at, Some(lost.heard_at));
        assert_eq!(settled[0].waits[0].to, Some(lost.heard_at));
        assert_eq!(settled[1], live);
        assert_eq!(settled[2], finished);
    }

    #[test]
    fn needs_are_open_waits_and_unanswered_questions() {
        let mut asking = turn(agent("scout", "s1"), Some("u1"), "2026-10-29T09:00:00Z");
        asking.waits = vec![
            wait("r1", "2026-10-29T09:05:00Z", None, decide()),
            wait(
                "r0",
                "2026-10-29T09:01:00Z",
                Some("2026-10-29T09:02:00Z"),
                decide(),
            ),
            wait("in", "2026-10-29T09:06:00Z", None, Answer::Reply),
        ];
        let asked = ended(
            turn(agent("lens", "s2"), Some("u1"), "2026-10-29T08:00:00Z"),
            "2026-10-29T08:30:00Z",
            Outcome::Asked,
        );
        let mut answered = asked.clone();
        answered.id = Uuid::new_v4();
        answered.settled_at = Some(at("2026-10-29T08:40:00Z"));
        let mut gone = ended(
            turn(agent("scout", "s3"), Some("u1"), "2026-10-29T07:00:00Z"),
            "2026-10-29T07:30:00Z",
            Outcome::Interrupted,
        );
        gone.waits = vec![wait("r9", "2026-10-29T07:10:00Z", None, decide())];
        let turns = [asking.clone(), asked.clone(), answered, gone];
        let needs = open_needs(&turns);
        assert_eq!(
            needs
                .iter()
                .map(|n| (n.id.clone(), n.kind))
                .collect::<Vec<_>>(),
            [
                (format!("question:{}", asked.id), NeedKind::Question),
                (format!("wait:{}:r1", asking.id), NeedKind::Approval),
                (format!("wait:{}:in", asking.id), NeedKind::Question),
            ]
        );
        assert_eq!(needs[0].ask, "Ship it?");
        assert_eq!(needs[0].since, at("2026-10-29T08:30:00Z"));
        assert_eq!(needs, open_needs(&turns), "ids are stable");
    }

    #[test]
    fn the_floor_never_lists_a_sessions_wait_that_nothing_asks() {
        let session = Uuid::new_v4();
        let mut asking = turn(
            Subject::Session {
                id: session,
                engine: Engine::Claude,
            },
            None,
            "2026-10-29T09:00:00Z",
        );
        asking.heard_at = at("2026-10-29T09:20:00Z");
        asking.waits = vec![
            wait("toolu_live", "2026-10-29T09:10:00Z", None, Answer::Terminal),
            // Raised before a restart that took its ask with it.
            wait("toolu_gone", "2026-10-29T09:09:00Z", None, Answer::Terminal),
        ];
        let mut agents = turn(agent("scout", "s1"), None, "2026-10-29T09:00:00Z");
        agents.waits = vec![wait("r1", "2026-10-29T09:05:00Z", None, decide())];
        let asks = HashMap::from([(session, HashSet::from(["toolu_live".to_string()]))]);
        let shown = backed(vec![asking.clone(), agents.clone()], &asks);
        let ids: Vec<String> = open_needs(&shown).into_iter().map(|n| n.id).collect();
        assert_eq!(
            ids,
            [
                format!("wait:{}:r1", agents.id),
                format!("wait:{}:toolu_live", asking.id),
            ],
            "an agent's wait is its own hub's; a session's stands only while it is asked"
        );
        assert_eq!(shown[0].waits[1].to, Some(at("2026-10-29T09:20:00Z")));
        // A session that asks nothing lists nothing.
        let none = backed(vec![asking], &HashMap::new());
        assert!(open_needs(&none).is_empty());
    }

    #[test]
    fn working_is_what_runs_and_done_what_finished_since() {
        let mut running = turn(agent("scout", "s1"), Some("u1"), "2026-10-29T09:00:00Z");
        running.doing = Some("● Bash cargo test".into());
        let quiet = turn(agent("lens", "s2"), Some("u1"), "2026-10-29T10:00:00Z");
        let finished = ended(
            turn(agent("scout", "s3"), Some("u1"), "2026-10-29T08:00:00Z"),
            "2026-10-29T08:30:00Z",
            Outcome::Finished,
        );
        let failed = ended(
            turn(agent("scout", "s4"), Some("u1"), "2026-10-29T08:40:00Z"),
            "2026-10-29T08:50:00Z",
            Outcome::Failed,
        );
        let stopped = ended(
            turn(agent("scout", "s5"), Some("u1"), "2026-10-29T08:00:00Z"),
            "2026-10-29T08:55:00Z",
            Outcome::Stopped,
        );
        let old = ended(
            turn(agent("scout", "s6"), Some("u1"), "2026-10-28T08:00:00Z"),
            "2026-10-28T08:30:00Z",
            Outcome::Finished,
        );
        let turns = [running, quiet, finished, failed, stopped, old];
        let w = working(&turns, &Gists::new());
        assert_eq!(
            w.iter().map(|w| w.what.as_str()).collect::<Vec<_>>(),
            ["Compare spend.", "● Bash cargo test"]
        );
        let d = done(&turns, at("2026-10-29T00:00:00Z"), &Gists::new());
        assert_eq!(
            d.iter().map(|w| w.outcome).collect::<Vec<_>>(),
            [Some(Outcome::Failed), Some(Outcome::Finished)]
        );
        assert_eq!(d[0].what, "Spend is flat.");
    }

    #[test]
    fn a_lane_per_subject_clipped_to_the_day_and_merged_where_spans_touch() {
        let day = at("2026-10-29T00:00:00Z");
        let now = at("2026-10-29T12:00:00Z");
        let overnight = ended(
            turn(agent("scout", "dm"), Some("u1"), "2026-10-28T23:00:00Z"),
            "2026-10-29T01:00:00Z",
            Outcome::Finished,
        );
        let mut room = ended(
            turn(agent("scout", "room"), Some("u1"), "2026-10-29T01:00:00Z"),
            "2026-10-29T02:00:00Z",
            Outcome::Asked,
        );
        room.place = Place::Room {
            room: Uuid::nil(),
            thread: None,
        };
        room.waits = vec![wait(
            "r1",
            "2026-10-29T01:30:00Z",
            Some("2026-10-29T01:40:00Z"),
            decide(),
        )];
        let mut answered = ended(
            turn(agent("lens", "dm"), Some("u1"), "2026-10-29T03:00:00Z"),
            "2026-10-29T04:00:00Z",
            Outcome::Asked,
        );
        answered.settled_at = Some(at("2026-10-29T05:00:00Z"));
        let lanes = lanes(&[overnight, room, answered], day, now);
        assert_eq!(lanes.len(), 2, "the DM and the room are one agent's lane");
        let span = |kind, from: &str, to: Option<&str>| Span {
            kind,
            from: at(from),
            to: to.map(at),
        };
        assert_eq!(lanes[0].who.id, "scout", "waiting until now is the latest");
        assert_eq!(
            lanes[0].spans,
            [
                span(
                    SpanKind::Working,
                    "2026-10-29T00:00:00Z",
                    Some("2026-10-29T02:00:00Z")
                ),
                span(
                    SpanKind::Waiting,
                    "2026-10-29T01:30:00Z",
                    Some("2026-10-29T01:40:00Z")
                ),
                span(SpanKind::Waiting, "2026-10-29T02:00:00Z", None),
            ]
        );
        assert_eq!(
            lanes[1].spans,
            [
                span(
                    SpanKind::Working,
                    "2026-10-29T03:00:00Z",
                    Some("2026-10-29T04:00:00Z")
                ),
                span(
                    SpanKind::Waiting,
                    "2026-10-29T04:00:00Z",
                    Some("2026-10-29T05:00:00Z")
                ),
            ]
        );
    }

    #[test]
    fn done_today_counts_from_midnight_and_the_column_the_last_day() {
        let now = at("2026-10-29T09:00:00Z");
        let day = at("2026-10-29T00:00:00Z");
        let last_night = ended(
            turn(agent("scout", "s1"), Some("u1"), "2026-10-28T20:00:00Z"),
            "2026-10-28T21:00:00Z",
            Outcome::Finished,
        );
        let this_morning = ended(
            turn(agent("scout", "s2"), Some("u1"), "2026-10-29T07:00:00Z"),
            "2026-10-29T08:00:00Z",
            Outcome::Finished,
        );
        let v = view(&[last_night, this_morning], vec![], &Gists::new(), day, now);
        assert_eq!(v.done.len(), 2);
        assert_eq!(v.counts.done_today, 1);
        assert_eq!(v.more, More::default());
    }

    #[test]
    fn the_caps_say_how_many_were_cut() {
        let now = at("2026-10-29T12:00:00Z");
        let turns: Vec<Turn> = (0..LANE_CAP + 3)
            .map(|i| {
                let mut t = ended(
                    turn(
                        agent(&format!("a{i}"), "s"),
                        Some("u1"),
                        "2026-10-29T09:00:00Z",
                    ),
                    "2026-10-29T10:00:00Z",
                    Outcome::Finished,
                );
                t.ended_at = Some(t.ended_at.unwrap() + Duration::seconds(i as i64));
                t
            })
            .collect();
        let v = view(
            &turns,
            vec![],
            &Gists::new(),
            at("2026-10-29T00:00:00Z"),
            now,
        );
        assert_eq!(
            (v.done.len(), v.more.done),
            (DONE_CAP, LANE_CAP + 3 - DONE_CAP)
        );
        assert_eq!((v.lanes.len(), v.more.lanes), (LANE_CAP, 3));
        assert_eq!(v.counts.done_today, LANE_CAP + 3);
    }

    #[test]
    fn the_person_speaking_settles_only_their_questions_with_that_subject() {
        let asked = ended(
            turn(agent("scout", "dm"), Some("u1"), "2026-10-29T08:00:00Z"),
            "2026-10-29T08:30:00Z",
            Outcome::Asked,
        );
        let theirs = Turn {
            id: Uuid::new_v4(),
            owner: Some("u2".into()),
            ..asked.clone()
        };
        let other_agent = Turn {
            id: Uuid::new_v4(),
            subject: agent("lens", "dm"),
            ..asked.clone()
        };
        let log = vec![asked.clone(), theirs.clone(), other_agent.clone()];
        let job = turn(agent("scout", "job"), Some("u1"), "2026-10-29T09:00:00Z");
        let after_job = apply(
            log.clone(),
            &TurnOp::Begin {
                turn: Box::new(job.clone()),
                by_person: false,
            },
        );
        assert!(after_job[0].asking(), "a job settles nothing");
        let reply = turn(agent("scout", "slack"), Some("u1"), "2026-10-29T09:30:00Z");
        let after = apply(
            log,
            &TurnOp::Begin {
                turn: Box::new(reply.clone()),
                by_person: true,
            },
        );
        assert_eq!(
            after[0].settled_at,
            Some(reply.started_at),
            "wherever they reply"
        );
        assert!(after[1].asking(), "another person's question stays");
        assert!(after[2].asking(), "another agent's question stays");
        assert_eq!(after[3], reply);
    }

    #[test]
    fn a_newer_question_settles_the_earlier_and_a_settle_clears_the_rest() {
        let first = ended(
            turn(agent("scout", "dm"), Some("u1"), "2026-10-29T08:00:00Z"),
            "2026-10-29T08:30:00Z",
            Outcome::Asked,
        );
        let second = turn(agent("scout", "dm"), Some("u1"), "2026-10-29T09:00:00Z");
        let end = TurnOp::End {
            turn: second.id,
            at: at("2026-10-29T09:10:00Z"),
            outcome: Outcome::Asked,
            answer: None,
            question: Some(ItemQuestion {
                text: "And now?".into(),
                options: None,
            }),
        };
        let log = apply(vec![first, second], &end);
        assert_eq!(log[0].settled_at, Some(at("2026-10-29T09:10:00Z")));
        assert!(log[1].asking());
        assert_eq!(open_needs(&log).len(), 1);
        let cleared = apply(
            log,
            &TurnOp::Settle {
                subject: SubjectKey::agent("scout"),
                owner: Some("u1".into()),
                at: at("2026-10-29T10:00:00Z"),
            },
        );
        assert!(open_needs(&cleared).is_empty());
    }

    #[test]
    fn a_forgotten_subjects_turns_go_and_the_rest_stay() {
        let hubs = Uuid::new_v4();
        let session = |id| Subject::Session {
            id,
            engine: Engine::Claude,
        };
        let log = vec![
            turn(session(hubs), None, "2026-10-29T08:00:00Z"),
            turn(session(Uuid::new_v4()), None, "2026-10-29T08:01:00Z"),
            turn(agent("scout", "dm"), Some("u1"), "2026-10-29T08:02:00Z"),
            turn(session(hubs), None, "2026-10-29T08:03:00Z"),
        ];
        let kept = apply(
            log.clone(),
            &TurnOp::Forget {
                subjects: vec![SubjectKey::session(hubs)],
            },
        );
        assert_eq!(kept, log[1..3]);
    }

    #[test]
    fn waits_open_once_per_key_close_and_never_reopen_an_ended_turn() {
        let t = turn(agent("scout", "dm"), Some("u1"), "2026-10-29T08:00:00Z");
        let id = t.id;
        let w = wait("r1", "2026-10-29T08:05:00Z", None, decide());
        let log = [
            TurnOp::Wait {
                turn: id,
                wait: w.clone(),
            },
            TurnOp::Wait {
                turn: id,
                wait: Wait {
                    answer: Answer::Terminal,
                    ..w.clone()
                },
            },
        ]
        .iter()
        .fold(vec![t], apply);
        assert_eq!(log[0].waits.len(), 1);
        assert_eq!(log[0].waits[0].answer, Answer::Terminal);
        let log = [
            TurnOp::Unwait {
                turn: id,
                key: "r1".into(),
                at: at("2026-10-29T08:06:00Z"),
            },
            TurnOp::End {
                turn: id,
                at: at("2026-10-29T08:10:00Z"),
                outcome: Outcome::Finished,
                answer: None,
                question: None,
            },
            TurnOp::Wait {
                turn: id,
                wait: Wait {
                    key: "late".into(),
                    ..w
                },
            },
        ]
        .iter()
        .fold(log, apply);
        assert_eq!(log[0].waits.len(), 1);
        assert_eq!(log[0].waits[0].to, Some(at("2026-10-29T08:06:00Z")));
        assert_eq!(log[0].outcome, Some(Outcome::Finished));
    }

    #[test]
    fn needs_have_the_wire_shape_desk_reads() {
        let need = Need {
            id: "wait:t:a".into(),
            kind: NeedKind::Approval,
            who: Subject::Session {
                id: Uuid::nil(),
                engine: Engine::Claude,
            }
            .who("auth-fix"),
            ask: "Run npm test?".into(),
            why: None,
            options: None,
            chose: None,
            since: at("2026-10-29T16:30:00Z"),
            place: Place::Session,
            answer: decide(),
            group: None,
            boss: None,
        };
        let nil = Uuid::nil().to_string();
        assert_eq!(
            serde_json::to_value(&need).unwrap(),
            json!({
                "id": "wait:t:a", "kind": "approval",
                "who": {"kind": "session", "id": nil, "name": "auth-fix", "engine": "claude"},
                "ask": "Run npm test?", "options": null, "since": "2026-10-29T16:30:00Z",
                "place": {"kind": "session"},
                "answer": {"kind": "decide", "at": {"kind": "session", "sessionId": nil, "approvalId": nil}}
            })
        );
        assert_eq!(
            serde_json::to_value(Place::Slack {
                team: None,
                channel: "C1".into(),
                thread: Some("1.2".into())
            })
            .unwrap(),
            json!({"kind": "slack", "team": null, "channel": "C1", "thread": "1.2"})
        );
        let span = Span {
            kind: SpanKind::Waiting,
            from: at("2026-10-29T16:30:00Z"),
            to: None,
        };
        assert_eq!(
            serde_json::to_value(span).unwrap(),
            json!({"kind": "waiting", "from": "2026-10-29T16:30:00Z", "to": null})
        );
        let work = Work {
            who: agent("scout", "dm").who("Scout"),
            what: "● Bash cargo test".into(),
            place: Place::Dm,
            since: at("2026-10-29T16:00:00Z"),
            heard_at: at("2026-10-29T16:01:00Z"),
            ended_at: None,
            outcome: None,
            background: Some(Background {
                count: 2,
                since: at("2026-10-29T16:30:00Z"),
            }),
            gist: Some(Said {
                purpose: Some("Speeding up the test run.".into()),
                now: "Running the tests again.".into(),
                at: at("2026-10-29T16:02:00Z"),
            }),
            group: None,
            boss: None,
            continued: None,
        };
        assert_eq!(
            serde_json::to_value(work).unwrap(),
            json!({
                "who": {"kind": "agent", "id": "scout", "name": "Scout", "engine": null},
                "what": "● Bash cargo test", "place": {"kind": "dm"},
                "since": "2026-10-29T16:00:00Z", "heardAt": "2026-10-29T16:01:00Z",
                "endedAt": null, "outcome": null,
                "background": {"count": 2, "since": "2026-10-29T16:30:00Z"},
                "gist": {
                    "purpose": "Speeding up the test run.", "now": "Running the tests again.",
                    "at": "2026-10-29T16:02:00Z"
                }
            })
        );
    }

    /// A floor with one need and one piece of work each for Scout (an
    /// agent) and a session.
    fn floor_of_two() -> FloorView {
        let scout = turn(agent("scout", "dm"), Some("u1"), "2026-10-29T08:00:00Z");
        let mut session = turn(
            Subject::Session {
                id: Uuid::nil(),
                engine: Engine::Claude,
            },
            Some("u1"),
            "2026-10-29T08:01:00Z",
        );
        session
            .waits
            .push(wait("a", "2026-10-29T08:02:00Z", None, decide()));
        let finished = ended(
            turn(agent("scout", "dm"), Some("u1"), "2026-10-29T07:00:00Z"),
            "2026-10-29T07:30:00Z",
            Outcome::Finished,
        );
        let turns = [scout, session, finished];
        let now = at("2026-10-29T09:00:00Z");
        view(
            &turns,
            open_needs(&turns),
            &Gists::new(),
            at("2026-10-29T00:00:00Z"),
            now,
        )
    }

    #[test]
    fn each_item_is_stamped_with_its_group_now_and_that_groups_own_boss() {
        let mut v = floor_of_two();
        let nil = Uuid::nil().to_string();
        let group_of = |who: &Who| match (who.kind, who.id.as_str()) {
            (PartyKind::Agent, "scout") => Some("acme".to_string()),
            (PartyKind::Session, id) if id == nil => Some("infra".to_string()),
            _ => None,
        };
        let boss_of = |g: &str| (g == "acme").then(|| "sarah".to_string());
        stamp(&mut v, group_of, boss_of);
        let work = |kind| v.working.iter().find(|w| w.who.kind == kind).unwrap();
        let scout = work(PartyKind::Agent);
        assert_eq!(
            (scout.group.as_deref(), scout.boss.as_deref()),
            (Some("acme"), Some("sarah"))
        );
        // A group with no boss of its own carries its group and no boss.
        let session = work(PartyKind::Session);
        assert_eq!(
            (session.group.as_deref(), session.boss.as_deref()),
            (Some("infra"), None)
        );
        assert_eq!(
            (v.needs[0].group.as_deref(), v.needs[0].boss.as_deref()),
            (Some("infra"), None)
        );
        // What finished is stamped as what runs.
        assert_eq!(v.done.len(), 1);
        assert_eq!(
            (v.done[0].group.as_deref(), v.done[0].boss.as_deref()),
            (Some("acme"), Some("sarah"))
        );

        // Stamped again after a move, it says where each is now; ungrouped
        // carries neither.
        stamp(&mut v, |_| None, boss_of);
        assert!(v
            .working
            .iter()
            .chain(&v.done)
            .all(|w| w.group.is_none() && w.boss.is_none()));
        assert!(v
            .needs
            .iter()
            .all(|n| n.group.is_none() && n.boss.is_none()));
        assert_eq!(boss_state(&v, "scout"), BossState::Working);
        assert_eq!(boss_state(&v, "sarah"), BossState::Idle);
    }

    #[test]
    fn a_floor_by_group_is_the_old_shape_with_more_and_an_older_desk_reads_it() {
        let plain = floor_of_two();
        let before = serde_json::to_value(&plain).unwrap();
        for key in ["bosses", "sealed", "plans"] {
            assert!(before.get(key).is_none(), "no groups: today's shape");
        }
        assert!(before["needs"][0].get("group").is_none());

        let mut v = plain.clone();
        stamp(&mut v, |_| Some("acme".into()), |_| Some("sarah".into()));
        v.sealed = vec!["globex".into()];
        v.bosses = vec![BossView {
            scope: crate::scope::Scope::Group("acme".into()),
            agent: "sarah".into(),
            name: "Sarah".into(),
            level: "act".into(),
            state: BossState::Idle,
            paused: None,
            spend: None,
        }];
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["needs"][0]["group"], "acme");
        assert_eq!(json["working"][0]["boss"], "sarah");
        assert_eq!(
            json["bosses"][0],
            json!({
                "scope": {"kind": "group", "group": "acme"}, "agent": "sarah",
                "name": "Sarah", "level": "act", "state": "idle"
            })
        );
        assert_eq!(json["sealed"], json!(["globex"]));

        // An older Desk's types, with none of the new fields, read it.
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        #[allow(dead_code)]
        struct OldNeed {
            id: String,
            kind: NeedKind,
            who: Who,
            ask: String,
            since: DateTime<Utc>,
            place: Place,
            answer: Answer,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        #[allow(dead_code)]
        struct OldWork {
            who: Who,
            what: String,
            place: Place,
            since: DateTime<Utc>,
            heard_at: DateTime<Utc>,
            ended_at: Option<DateTime<Utc>>,
            outcome: Option<Outcome>,
            background: Option<Background>,
            gist: Option<Said>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        #[allow(dead_code)]
        struct OldView {
            as_of: DateTime<Utc>,
            needs: Vec<OldNeed>,
            working: Vec<serde_json::Value>,
            done: Vec<serde_json::Value>,
            lanes: Vec<Lane>,
            counts: Counts,
            more: More,
            summary: Option<Paragraph>,
        }
        let old: OldView = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(old.needs.len(), v.needs.len());
        // The new fields are the only ones an older Work didn't know.
        let mut work = json["working"][0].clone();
        assert!(serde_json::from_value::<OldWork>(work.clone()).is_err());
        let w = work.as_object_mut().unwrap();
        w.remove("group");
        w.remove("boss");
        serde_json::from_value::<OldWork>(work).unwrap();

        // And this Desk reads what an older hub sends.
        let back: FloorView = serde_json::from_value(before).unwrap();
        assert_eq!(back, plain);
    }

    #[test]
    fn a_turn_waiting_on_its_background_work_is_working_until_it_ends() {
        let t = turn(agent("scout", "dm"), Some("u1"), "2026-10-29T08:00:00Z");
        let id = t.id;
        let idle = |count, when: &str| TurnOp::Background {
            turn: id,
            count,
            at: at(when),
        };
        let log = [
            idle(2, "2026-10-29T08:10:00Z"),
            idle(1, "2026-10-29T08:20:00Z"),
            idle(0, "2026-10-29T08:25:00Z"),
            idle(1, "2026-10-29T08:30:00Z"),
        ]
        .iter()
        .fold(vec![t], apply);
        let running = working(&log, &Gists::new());
        assert_eq!(
            running[0].background,
            Some(Background {
                count: 1,
                since: at("2026-10-29T08:30:00Z")
            })
        );
        assert!(
            done(&log, at("2026-10-29T00:00:00Z"), &Gists::new()).is_empty(),
            "not done"
        );
        let now = at("2026-10-29T09:00:00Z");
        assert_eq!(
            lanes(&log, at("2026-10-29T00:00:00Z"), now)[0]
                .spans
                .iter()
                .filter(|s| s.kind == SpanKind::Background)
                .map(|s| (s.from, s.to))
                .collect::<Vec<_>>(),
            [
                (at("2026-10-29T08:10:00Z"), Some(at("2026-10-29T08:25:00Z"))),
                (at("2026-10-29T08:30:00Z"), None)
            ]
        );
        let ended = apply(
            log,
            &TurnOp::End {
                turn: id,
                at: at("2026-10-29T08:40:00Z"),
                outcome: Outcome::Finished,
                answer: None,
                question: None,
            },
        );
        assert_eq!(ended[0].background(), None);
        assert_eq!(ended[0].idle[1].to, Some(at("2026-10-29T08:40:00Z")));
        assert_eq!(
            view(
                &ended,
                vec![],
                &Gists::new(),
                at("2026-10-29T00:00:00Z"),
                now
            )
            .counts
            .done_today,
            1
        );
    }

    fn kept(turn: Uuid, now: &str, written: &str) -> Gist {
        Gist {
            purpose: Some("Getting the release out to the team.".into()),
            purpose_at: Some(at("2026-10-29T08:00:00Z")),
            now: now.into(),
            now_at: at(written),
            turn,
            by_model: true,
        }
    }

    #[test]
    fn the_floor_carries_each_subjects_gist_or_says_it_plainly() {
        let now = at("2026-10-29T12:00:00Z");
        let day = at("2026-10-29T00:00:00Z");
        let mut running = turn(agent("scout", "dm"), Some("u1"), "2026-10-29T11:00:00Z");
        running.task = Some("Look at #12 again".into());
        let finished = ended(
            turn(agent("lens", "dm"), Some("u1"), "2026-10-29T09:00:00Z"),
            "2026-10-29T09:30:00Z",
            Outcome::Finished,
        );
        let gists = Gists::from([
            (
                SubjectKey::agent("scout"),
                kept(Uuid::nil(), "Was tidying up.", "2026-10-29T10:00:00Z"),
            ),
            (
                SubjectKey::agent("lens"),
                kept(
                    finished.id,
                    "Compared the spend; it is flat.",
                    "2026-10-29T09:31:00Z",
                ),
            ),
        ]);
        let v = view(&[running, finished], vec![], &gists, day, now);
        assert_eq!(
            v.done[0].gist,
            Some(Said {
                purpose: Some("Getting the release out to the team.".into()),
                now: "Compared the spend; it is flat.".into(),
                at: at("2026-10-29T09:31:00Z"),
            })
        );
        assert_eq!(
            v.working[0].gist,
            Some(Said {
                purpose: Some("Getting the release out to the team.".into()),
                now: "Working on it now.".into(),
                at: at("2026-10-29T11:00:00Z"),
            }),
            "a gist written before this turn began is not this turn's: it is said plainly"
        );
        let plain = view(
            &[turn(
                agent("scout", "dm"),
                Some("u1"),
                "2026-10-29T11:00:00Z",
            )],
            vec![],
            &Gists::new(),
            day,
            now,
        );
        assert_eq!(
            plain.working[0].gist.as_ref().map(|g| g.purpose.as_deref()),
            Some(Some("Compare spend.")),
            "without a gist the purpose is its first ask"
        );
        assert_eq!(v.summary, None);
    }

    #[test]
    fn what_is_happening_is_one_entry_per_subject_working_then_waiting_then_done() {
        let now = at("2026-10-29T12:00:00Z");
        let day = at("2026-10-29T00:00:00Z");
        let older = ended(
            turn(agent("scout", "dm"), Some("u1"), "2026-10-29T10:00:00Z"),
            "2026-10-29T10:30:00Z",
            Outcome::Finished,
        );
        let newer = ended(
            turn(agent("scout", "slack"), Some("u1"), "2026-10-29T11:00:00Z"),
            "2026-10-29T11:30:00Z",
            Outcome::Failed,
        );
        let gone_by = ended(
            turn(agent("old", "dm"), Some("u1"), "2026-10-29T08:00:00Z"),
            "2026-10-29T09:30:00Z",
            Outcome::Finished,
        );
        let mut waiting = turn(agent("lens", "dm"), Some("u1"), "2026-10-29T11:40:00Z");
        waiting.idle = vec![Idle {
            from: at("2026-10-29T11:50:00Z"),
            to: None,
            count: 2,
        }];
        let lens_done = ended(
            turn(agent("lens", "job"), Some("u1"), "2026-10-29T11:00:00Z"),
            "2026-10-29T11:55:00Z",
            Outcome::Finished,
        );
        let working = turn(agent("atlas", "dm"), Some("u1"), "2026-10-29T11:00:00Z");
        let agents_own = Turn {
            name: "at-dm-d4774cbb6c89-2e".into(),
            ..turn(
                Subject::Session {
                    id: Uuid::new_v4(),
                    engine: Engine::Claude,
                },
                None,
                "2026-10-29T11:50:00Z",
            )
        };
        let v = view(
            &[
                older, newer, gone_by, waiting, lens_done, working, agents_own,
            ],
            vec![],
            &Gists::new(),
            day,
            now,
        );
        let entries = happening(&v, now);
        assert_eq!(
            entries
                .iter()
                .map(|e| (e.who.id.as_str(), e.doing))
                .collect::<Vec<_>>(),
            [
                ("atlas", Doing::Working),
                ("lens", Doing::Waiting(2)),
                ("scout", Doing::Failed),
            ],
            "one each, the latest of what ended; two hours ago is gone by; an agent's \
             turn's own session is the agent's"
        );
    }
}
