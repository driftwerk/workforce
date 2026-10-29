//! The floor: what the person's agents and sessions are doing, what they
//! finished, and what still needs the person, folded from a log of turns.
//! Types and pure functions only; the log itself is the hub's.

use std::collections::{BTreeMap, HashSet};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::crew::{Counts, ItemQuestion, PartyKind};
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
    Session { session_id: Uuid, approval_id: Uuid },
}

/// A stretch of a turn that waited on the person: one per ask.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Wait {
    pub key: String,
    pub from: DateTime<Utc>,
    pub to: Option<DateTime<Utc>>,
    pub ask: String,
    pub answer: Answer,
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
            | Self::End { turn, .. } => Some(*turn),
            Self::Begin { .. } | Self::Settle { .. } => None,
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
                match t.waits.iter_mut().find(|w| w.key == wait.key) {
                    Some(w) => {
                        w.ask = wait.ask.clone();
                        w.answer = wait.answer.clone();
                    }
                    None => t.waits.push(wait.clone()),
                }
            }
        }
        TurnOp::Unwait { turn, key, at } => {
            if let Some(t) = open_turn(&mut turns, *turn) {
                t.waits
                    .iter_mut()
                    .filter(|w| &w.key == key && w.to.is_none())
                    .for_each(|w| w.to = Some(*at));
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
        TurnOp::Settle { .. } => {}
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
    /// Turns of `scope` started since `since`, every open one, and every
    /// question still unanswered, however old.
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
    pub options: Option<Vec<String>>,
    pub since: DateTime<Utc>,
    pub place: Place,
    pub answer: Answer,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanKind {
    Working,
    Waiting,
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
    pub working: Vec<Work>,
    pub done: Vec<Work>,
    pub lanes: Vec<Lane>,
    pub counts: Counts,
    pub more: More,
}

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

/// What still needs the person, longest waiting first: every open wait of
/// an open turn, and every question not yet settled.
pub fn open_needs(turns: &[Turn]) -> Vec<Need> {
    let waits = turns.iter().filter(|t| t.ended_at.is_none()).flat_map(|t| {
        t.waits
            .iter()
            .filter(|w| w.to.is_none())
            .map(move |w| Need {
                id: format!("wait:{}:{}", t.id, w.key),
                kind: match w.answer {
                    Answer::Reply => NeedKind::Question,
                    Answer::Decide { .. } | Answer::Terminal => NeedKind::Approval,
                },
                who: t.subject.who(&t.name),
                ask: w.ask.clone(),
                options: None,
                since: w.from,
                place: t.place.clone(),
                answer: w.answer.clone(),
            })
    });
    let questions = turns.iter().filter(|t| t.asking()).filter_map(|t| {
        let q = t.question.as_ref()?;
        Some(Need {
            id: format!("question:{}", t.id),
            kind: NeedKind::Question,
            who: t.subject.who(&t.name),
            ask: q.text.clone(),
            options: q.options.clone(),
            since: t.ended_at.unwrap_or(t.started_at),
            place: t.place.clone(),
            answer: Answer::Reply,
        })
    });
    let mut needs: Vec<Need> = waits.chain(questions).collect();
    needs.sort_by(|a, b| a.since.cmp(&b.since).then_with(|| a.id.cmp(&b.id)));
    needs
}

fn work(t: &Turn, what: Option<&String>) -> Work {
    Work {
        who: t.subject.who(&t.name),
        what: what.or(t.task.as_ref()).cloned().unwrap_or_default(),
        place: t.place.clone(),
        since: t.started_at,
        heard_at: t.heard_at,
        ended_at: t.ended_at,
        outcome: t.outcome,
    }
}

/// The turns running now, newest first.
pub fn working(turns: &[Turn]) -> Vec<Work> {
    let mut out: Vec<Work> = turns
        .iter()
        .filter(|t| t.ended_at.is_none())
        .map(|t| work(t, t.doing.as_ref()))
        .collect();
    out.sort_by(|a, b| b.since.cmp(&a.since));
    out
}

/// The turns that finished, failed or hit a limit since `from`, newest
/// first.
pub fn done(turns: &[Turn], from: DateTime<Utc>) -> Vec<Work> {
    let mut out: Vec<Work> = turns
        .iter()
        .filter(|t| {
            matches!(
                t.outcome,
                Some(Outcome::Finished | Outcome::Failed | Outcome::Limit)
            ) && t.ended_at.is_some_and(|at| at >= from)
        })
        .map(|t| work(t, t.answer.as_ref()))
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
    std::iter::once(run).chain(waits).chain(asked)
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

/// The floor as of `now` for the person's `day`, from settled `turns` and
/// the `needs` they and the hub's other sources make.
pub fn view(turns: &[Turn], needs: Vec<Need>, day: DateTime<Utc>, now: DateTime<Utc>) -> FloorView {
    let working = working(turns);
    let recent = now - chrono::Duration::hours(24);
    let finished = done(turns, day.min(recent));
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
        working,
        done: done.into_iter().take(DONE_CAP).collect(),
        lanes: lanes.into_iter().take(LANE_CAP).collect(),
        counts,
        more,
    }
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
        let w = working(&turns);
        assert_eq!(
            w.iter().map(|w| w.what.as_str()).collect::<Vec<_>>(),
            ["Compare spend.", "● Bash cargo test"]
        );
        let d = done(&turns, at("2026-10-29T00:00:00Z"));
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
        let v = view(&[last_night, this_morning], vec![], day, now);
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
        let v = view(&turns, vec![], at("2026-10-29T00:00:00Z"), now);
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
            options: None,
            since: at("2026-10-29T16:30:00Z"),
            place: Place::Session,
            answer: decide(),
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
        };
        assert_eq!(
            serde_json::to_value(work).unwrap(),
            json!({
                "who": {"kind": "agent", "id": "scout", "name": "Scout", "engine": null},
                "what": "● Bash cargo test", "place": {"kind": "dm"},
                "since": "2026-10-29T16:00:00Z", "heardAt": "2026-10-29T16:01:00Z",
                "endedAt": null, "outcome": null
            })
        );
    }
}
