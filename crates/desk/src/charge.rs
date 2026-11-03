//! Charges: which boss runs which session. A session in a boss's charge is
//! one the boss orchestrates over its line, is woken for at each turn's end,
//! and answers for. The person can always take the wheel: typing in its
//! terminal, prompting it or writing to it puts it in their hands, and the
//! boss's messages wait until it comes back, by click or after
//! [`QUIET`] with the session idle.
//!
//! Desk says "take on" and "release" here: picking up a session from
//! another terminal is [`crate::adopt`]'s, and is something else.
//!
//! This module decides; [`crate::service`]'s `charge` submodule stores and
//! announces.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::engine::{Deliver, Engine};
use crate::model::{text_enum, Origin, SessionState, SessionView};

text_enum!(How {
    // The boss started it.
    Started => "started",
    // The person handed it to the boss.
    Handed => "handed",
    // The boss took it on by itself: it takes on every session.
    Taken => "taken",
});

/// Who drives a charged session now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wheel {
    Boss,
    /// The person: back to the boss after [`QUIET`] with the session idle.
    Person,
    /// The person, until they hand it back.
    PersonPinned,
}

impl Wheel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Boss => "boss",
            Self::Person => "person",
            Self::PersonPinned => "person_pinned",
        }
    }

    pub fn parse(s: &str) -> eyre::Result<Self> {
        match s {
            "boss" => Ok(Self::Boss),
            "person" => Ok(Self::Person),
            "person_pinned" => Ok(Self::PersonPinned),
            other => Err(eyre::eyre!("unknown Wheel {other:?}")),
        }
    }

    /// Whether the person has it.
    pub fn person(self) -> bool {
        self != Self::Boss
    }
}

/// How long the person's wheel lasts with no input from them and the
/// session idle, before it goes back to the boss.
pub const QUIET: chrono::Duration = chrono::Duration::minutes(10);

/// One session's charge, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Charge {
    pub session: Uuid,
    /// The boss's sidebar group; none is the main boss.
    pub group: Option<String>,
    /// The boss's agent id.
    pub boss: String,
    pub how: How,
    pub wheel: Wheel,
    /// When the wheel last changed hands.
    pub wheel_since: Option<DateTime<Utc>>,
    pub since: DateTime<Utc>,
    pub released_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

impl Charge {
    /// A new charge of `session` to `boss`, now, the boss at the wheel.
    pub fn new(session: Uuid, boss: &str, how: How, group: Option<String>) -> Self {
        let now = crate::crew::now();
        Self {
            session,
            group,
            boss: boss.to_string(),
            how,
            wheel: Wheel::Boss,
            wheel_since: None,
            since: now,
            released_at: None,
            updated_at: now,
        }
    }

    /// Whether a boss runs it now: it hasn't been released.
    pub fn live(&self) -> bool {
        self.released_at.is_none()
    }
}

/// Why a session can't be taken on, in the words the person reads.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NotChargeable {
    #[error("It runs in another terminal. Continue it in Desk first.")]
    External,
    /// Its engine (or the version it runs) has no line: what to do about
    /// it, as Desk's notes say.
    #[error("{0}")]
    NoLine(String),
    #[error("A custom command has no line for a boss to talk on.")]
    Custom,
    /// It is in a group with a boss of its own.
    #[error("It's in {0}, which has a boss of its own.")]
    OtherScope(String),
    #[error("It has ended.")]
    Ended,
}

/// Whether `s` can be taken on by a boss that doesn't run the groups in
/// `bossed` (each run by a boss of its own): it needs a line from Desk.
pub fn chargeable(s: &SessionView, bossed: &HashSet<String>) -> Result<(), NotChargeable> {
    if s.origin == Origin::External {
        return Err(NotChargeable::External);
    }
    if s.state == SessionState::Closed {
        return Err(NotChargeable::Ended);
    }
    if s.engine == Engine::Custom {
        return Err(NotChargeable::Custom);
    }
    if s.caps.line == Deliver::None {
        let why = s.caps.notes.first().map_or_else(
            || format!("{} sessions have no line from Desk yet.", s.engine.title()),
            |n| n.to_string(),
        );
        return Err(NotChargeable::NoLine(why));
    }
    match &s.group {
        Some(g) if bossed.contains(&g.id) => Err(NotChargeable::OtherScope(g.name.clone())),
        _ => Ok(()),
    }
}

/// Whether a boss that takes on every session (`all`) takes `s` on now: it
/// can be, it isn't charged already, and the person never released it.
pub fn takes_automatically(
    all: bool,
    s: &SessionView,
    existing: Option<&Charge>,
    bossed: &HashSet<String>,
) -> bool {
    all && existing.is_none() && chargeable(s, bossed).is_ok()
}

/// How many of `charges` hold one of their boss's slots: starting or
/// working.
pub fn busy(charges: &[Charge], states: &HashMap<Uuid, SessionState>) -> usize {
    charges
        .iter()
        .filter(|c| c.live())
        .filter(|c| {
            matches!(
                states.get(&c.session),
                Some(SessionState::Starting | SessionState::Working)
            )
        })
        .count()
}

/// Whether a session in `state` is idle: the person's wheel comes back to
/// the boss only then.
fn idle(state: SessionState) -> bool {
    !matches!(
        state,
        SessionState::Starting | SessionState::Working | SessionState::Blocked
    )
}

/// Who has the wheel of `c` at `now`, given when the person last gave the
/// session input (typed in its terminal, prompted it, wrote to it) and its
/// state. Only input since the boss last had the wheel counts: since it
/// was charged, or handed back. The person keeps it while they give input,
/// and while the session works on it; it comes back after [`QUIET`] with
/// none and the session idle, unless they pinned it.
pub fn wheel(
    c: &Charge,
    last_person_input: Option<DateTime<Utc>>,
    state: SessionState,
    now: DateTime<Utc>,
) -> Wheel {
    let taken = match c.wheel {
        Wheel::PersonPinned => return Wheel::PersonPinned,
        Wheel::Person => Some(c.wheel_since.unwrap_or(c.since)),
        Wheel::Boss => None,
    };
    let boss_since = match c.wheel {
        Wheel::Boss => c.wheel_since.unwrap_or(c.since),
        Wheel::Person | Wheel::PersonPinned => c.since,
    };
    let input = last_person_input.filter(|t| *t > boss_since);
    match taken.max(input) {
        None => Wheel::Boss,
        Some(last) if idle(state) && now - last >= QUIET => Wheel::Boss,
        Some(_) => Wheel::Person,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Approvals, GroupRef, SessionRecord, View};

    fn session(engine: Engine) -> SessionView {
        let now = Utc::now();
        let r = SessionRecord {
            id: Uuid::new_v4(),
            engine,
            view: View::Terminal,
            engine_version: None,
            approve_for_me: false,
            account: None,
            name: "acme-web".into(),
            goal: String::new(),
            cwd: "/work/acme-web".into(),
            branch: None,
            origin: Origin::Desk,
            state: SessionState::Idle,
            approvals: Approvals::Desk,
            cli_session_id: None,
            transcript: None,
            last_prompt: None,
            last_reply: None,
            external_pid: None,
            restore_offer: false,
            terminal_id: None,
            display_name: None,
            asked: None,
            created_at: now,
            updated_at: now,
        };
        SessionView::new(&r, true, None, None)
    }

    #[test]
    fn only_a_session_with_a_line_from_desk_can_be_taken_on_and_why_not_is_said() {
        let none = HashSet::new();
        assert_eq!(chargeable(&session(Engine::Claude), &none), Ok(()));
        assert_eq!(chargeable(&session(Engine::Codex), &none), Ok(()));

        let mut external = session(Engine::Claude);
        external.origin = Origin::External;
        let mut ended = session(Engine::Codex);
        ended.state = SessionState::Closed;
        let mut old = session(Engine::Claude);
        old.caps =
            crate::engine::session_caps(Engine::Claude, View::Terminal, Some("2.1.0"), false);
        let custom = session(Engine::Custom);
        let mut atlas = session(Engine::Claude);
        atlas.group = Some(GroupRef {
            id: "g-atlas".into(),
            name: "Atlas".into(),
        });
        let bossed = HashSet::from(["g-atlas".to_string()]);

        let said = |s: &SessionView, bossed: &HashSet<String>| {
            chargeable(s, bossed).unwrap_err().to_string()
        };
        assert_eq!(
            said(&external, &none),
            "It runs in another terminal. Continue it in Desk first."
        );
        assert_eq!(said(&ended, &none), "It has ended.");
        assert_eq!(
            said(&custom, &none),
            "A custom command has no line for a boss to talk on."
        );
        assert_eq!(
            said(&old, &none),
            "Update Claude Code to 2.1.287 or later to message this session."
        );
        assert_eq!(
            said(&atlas, &bossed),
            "It's in Atlas, which has a boss of its own."
        );
        assert_eq!(
            chargeable(&atlas, &none),
            Ok(()),
            "a group with no boss is the main boss's"
        );
    }

    #[test]
    fn a_boss_that_takes_on_everything_never_takes_back_a_released_session() {
        let none = HashSet::new();
        let s = session(Engine::Claude);
        assert!(takes_automatically(true, &s, None, &none));
        assert!(!takes_automatically(false, &s, None, &none), "only handed");
        let mut released = Charge::new(s.id, "sarah", How::Taken, None);
        released.released_at = Some(Utc::now());
        assert!(!takes_automatically(true, &s, Some(&released), &none));
        let charged = Charge::new(s.id, "sarah", How::Handed, None);
        assert!(!takes_automatically(true, &s, Some(&charged), &none));
        let mut external = session(Engine::Claude);
        external.origin = Origin::External;
        assert!(!takes_automatically(true, &external, None, &none));
    }

    #[test]
    fn only_starting_and_working_sessions_hold_a_slot() {
        let states: Vec<SessionState> = vec![
            SessionState::Starting,
            SessionState::Working,
            SessionState::Blocked,
            SessionState::Asking,
            SessionState::Done,
            SessionState::Idle,
            SessionState::Stopped,
        ];
        let charges: Vec<Charge> = states
            .iter()
            .map(|_| Charge::new(Uuid::new_v4(), "sarah", How::Started, None))
            .collect();
        let by: HashMap<Uuid, SessionState> = charges
            .iter()
            .zip(&states)
            .map(|(c, s)| (c.session, *s))
            .collect();
        assert_eq!(busy(&charges, &by), 2);
        let mut released = charges.clone();
        released[0].released_at = Some(Utc::now());
        assert_eq!(busy(&released, &by), 1, "a released session holds none");
    }

    #[test]
    fn the_persons_input_takes_the_wheel_and_ten_quiet_minutes_give_it_back() {
        let now = Utc::now();
        let ago = |m: i64| now - chrono::Duration::minutes(m);
        let mut c = Charge::new(Uuid::new_v4(), "sarah", How::Handed, None);
        c.since = ago(60);
        let idle = SessionState::Idle;
        assert_eq!(wheel(&c, None, idle, now), Wheel::Boss);
        assert_eq!(
            wheel(&c, Some(ago(61)), idle, now),
            Wheel::Boss,
            "input before the charge doesn't count"
        );
        assert_eq!(wheel(&c, Some(ago(1)), idle, now), Wheel::Person);
        assert_eq!(wheel(&c, Some(ago(9)), idle, now), Wheel::Person);
        assert_eq!(wheel(&c, Some(ago(10)), idle, now), Wheel::Boss);
        assert_eq!(
            wheel(&c, Some(ago(30)), SessionState::Working, now),
            Wheel::Person,
            "it stays the person's while the session works on what they said"
        );
        assert_eq!(
            wheel(&c, Some(ago(30)), SessionState::Blocked, now),
            Wheel::Person
        );

        // Handed back: what they typed before doesn't take it again.
        c.wheel = Wheel::Boss;
        c.wheel_since = Some(ago(2));
        assert_eq!(wheel(&c, Some(ago(3)), idle, now), Wheel::Boss);
        assert_eq!(wheel(&c, Some(ago(1)), idle, now), Wheel::Person);

        // Taken by a click with no input since: it still comes back.
        c.wheel = Wheel::Person;
        c.wheel_since = Some(ago(11));
        assert_eq!(wheel(&c, None, idle, now), Wheel::Boss);
        c.wheel_since = Some(ago(5));
        assert_eq!(wheel(&c, None, idle, now), Wheel::Person);

        // Pinned, it stays.
        c.wheel = Wheel::PersonPinned;
        c.wheel_since = Some(ago(600));
        assert_eq!(wheel(&c, None, idle, now), Wheel::PersonPinned);
    }

    #[test]
    fn the_wheel_reads_and_writes_as_its_words() {
        for w in [Wheel::Boss, Wheel::Person, Wheel::PersonPinned] {
            assert_eq!(Wheel::parse(w.as_str()).unwrap(), w);
            assert_eq!(serde_json::to_value(w).unwrap(), w.as_str());
        }
        assert_eq!(How::parse("taken").unwrap(), How::Taken);
    }
}
