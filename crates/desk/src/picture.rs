//! The boss's one live picture (designs/desk-boss-one-picture.md): what
//! happened that a boss's next instruction to a session must stand on,
//! written as it happens, in one ordered feed. Every row has a sequence
//! number; a session's version, for a boss, is the newest row that
//! touches it. A boss's turn reads the picture when it starts, and what it
//! sends a session is checked against what that session's version has
//! become since: one decided on a picture that has moved on is decided
//! again, never sent.
//!
//! What moves a session's version (and only this, so a session busy with
//! its own tools never holds its boss up):
//! - the person's words: typed into the session or written to it from
//!   Desk (that session), or to its boss in their DM (every session the
//!   boss oversees);
//! - an instruction any of the boss's turns sent it;
//! - the session writing to its boss on its line, or asking something;
//! - a shared fact naming it (or naming none: every session in scope);
//! - a standing order of the person's (every session in scope).
//!
//! This module decides; [`crate::service`]'s `picture` keeps the feed and
//! `picture_store` writes it to Postgres (`picture_events`).

use std::collections::VecDeque;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The most rows kept in memory: older ones are still in Postgres, and
/// no turn decides on a picture that old.
pub const KEPT: usize = 20_000;

/// How long rows are kept in memory: a turn running longer than this has
/// its picture read again.
pub const KEPT_FOR: chrono::TimeDelta = chrono::TimeDelta::hours(24);

/// How long a shared fact or standing order is read back after a restart.
pub const FACTS_FOR: chrono::TimeDelta = chrono::TimeDelta::days(14);

/// The longest text a row keeps.
pub const TEXT_MAX: usize = 1_500;

/// What happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum What {
    /// The person's own words: into a session (`session`), or to a boss
    /// in its DM (`boss`, no session).
    PersonSaid,
    /// A boss told a session something on its line.
    Instruction,
    /// A session wrote to its boss on its line.
    SessionReported,
    /// A session asked: a question or a request waiting on an answer.
    SessionAsked,
    /// A fact that holds across sessions ("Sam owns the fix, #648").
    Fact,
    /// A standing order of the person's, kept or changed.
    Order,
    /// The person handed a session back to its boss.
    HandedBack,
}

impl What {
    pub fn as_str(self) -> &'static str {
        match self {
            What::PersonSaid => "person_said",
            What::Instruction => "instruction",
            What::SessionReported => "session_reported",
            What::SessionAsked => "session_asked",
            What::Fact => "fact",
            What::Order => "order",
            What::HandedBack => "handed_back",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "person_said" => What::PersonSaid,
            "instruction" => What::Instruction,
            "session_reported" => What::SessionReported,
            "session_asked" => What::SessionAsked,
            "fact" => What::Fact,
            "order" => What::Order,
            "handed_back" => What::HandedBack,
            _ => return None,
        })
    }
}

/// One row of the picture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Happened {
    /// Its place in the feed: later rows have larger numbers.
    pub seq: u64,
    pub at: DateTime<Utc>,
    pub what: What,
    /// The boss it is about or by: none, every boss.
    pub boss: Option<String>,
    /// The session it is about: none, every session in the boss's scope
    /// (or in `sessions`).
    pub session: Option<Uuid>,
    /// For a fact or an order with no `session`: the sessions it names
    /// (none: every session in scope).
    pub sessions: Vec<Uuid>,
    pub text: String,
    /// The boss's turn that made it (an instruction): that turn's own
    /// sends never make its picture stale.
    pub turn: Option<String>,
}

impl Happened {
    /// Whether this row touches `session` for `boss`.
    pub fn touches(&self, boss: &str, session: Uuid) -> bool {
        if self.boss.as_deref().is_some_and(|b| b != boss) && self.session != Some(session) {
            // Another boss's own words (its DM, its facts) aren't this
            // boss's picture; anything about the session itself is.
            return false;
        }
        match self.session {
            Some(s) => s == session,
            None => self.sessions.is_empty() || self.sessions.contains(&session),
        }
    }

    /// The row in a line a boss reads: when, who, what.
    pub fn said(&self, tz: chrono_tz::Tz) -> String {
        let at = self.at.with_timezone(&tz).format("%H:%M:%S");
        // All of it, on one line: a later sentence ("…must not write its
        // own") is often the one that matters.
        let text = crate::crew::clip(
            &self.text.split_whitespace().collect::<Vec<_>>().join(" "),
            600,
        );
        match self.what {
            What::PersonSaid if self.session.is_some() => {
                format!("{at} the person, in the session: “{text}”")
            }
            What::PersonSaid => format!("{at} the person, to you: “{text}”"),
            What::Instruction => format!("{at} you told it: “{text}”"),
            What::SessionReported => format!("{at} it wrote to you: “{text}”"),
            What::SessionAsked => format!("{at} it asked: “{text}”"),
            What::Fact => format!("{at} fact: {text}"),
            What::Order => format!("{at} the person's standing order: {text}"),
            What::HandedBack => format!("{at} the person handed it back to you"),
        }
    }
}

/// A row to add: the feed numbers and stamps it.
#[derive(Debug, Clone, Default)]
pub struct Note {
    pub what: Option<What>,
    pub boss: Option<String>,
    pub session: Option<Uuid>,
    pub sessions: Vec<Uuid>,
    pub text: String,
    pub turn: Option<String>,
}

impl Note {
    pub fn new(what: What, text: impl Into<String>) -> Self {
        Self {
            what: Some(what),
            text: text.into(),
            ..Default::default()
        }
    }

    pub fn boss(mut self, boss: impl Into<String>) -> Self {
        self.boss = Some(boss.into());
        self
    }

    pub fn session(mut self, session: Uuid) -> Self {
        self.session = Some(session);
        self
    }

    pub fn sessions(mut self, sessions: Vec<Uuid>) -> Self {
        self.sessions = sessions;
        self
    }

    pub fn turn(mut self, turn: Option<String>) -> Self {
        self.turn = turn;
        self
    }
}

/// The feed, in memory: the rows of the last [`KEPT_FOR`] (at most
/// [`KEPT`]), and facts and orders since [`FACTS_FOR`].
#[derive(Debug, Default)]
pub struct Picture {
    rows: VecDeque<Happened>,
    /// The last number given.
    seq: u64,
    loaded: bool,
}

impl Picture {
    /// The newest row's number: what a turn starting now decides on.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Adds `note` as the next row, at `at`; its number.
    pub fn push(&mut self, note: Note, at: DateTime<Utc>) -> Option<Happened> {
        let what = note.what?;
        self.seq += 1;
        let row = Happened {
            seq: self.seq,
            at,
            what,
            boss: note.boss,
            session: note.session,
            sessions: note.sessions,
            text: crate::crew::clip(note.text.trim(), TEXT_MAX),
            turn: note.turn,
        };
        self.rows.push_back(row.clone());
        self.trim(at);
        Some(row)
    }

    /// Rows read back from Postgres, oldest first, and the largest number
    /// it ever gave (`top`). Rows added here before the read (`pending`)
    /// are numbered again after it, in their order, and handed back to be
    /// written; numbering goes on after them.
    pub fn load(&mut self, rows: Vec<Happened>, top: u64) -> Vec<Happened> {
        let mut pending: Vec<Happened> = self.rows.drain(..).collect();
        let base = top.max(rows.iter().map(|r| r.seq).max().unwrap_or(0));
        for (i, r) in pending.iter_mut().enumerate() {
            r.seq = base + 1 + i as u64;
        }
        self.seq = base + pending.len() as u64;
        self.rows = rows.into_iter().chain(pending.iter().cloned()).collect();
        self.rows.make_contiguous().sort_by_key(|r| r.seq);
        self.loaded = true;
        pending
    }

    /// Whether what Postgres kept has been read back: until then nothing
    /// is written, so no number is given twice.
    pub fn loaded(&self) -> bool {
        self.loaded
    }

    fn trim(&mut self, now: DateTime<Utc>) {
        let keeps = |r: &Happened| match r.what {
            What::Fact | What::Order => now - r.at < FACTS_FOR,
            _ => now - r.at < KEPT_FOR,
        };
        // What no longer counts goes; facts and orders stay longer.
        self.rows.retain(keeps);
        while self.rows.len() > KEPT {
            self.rows.pop_front();
        }
    }

    /// `session`'s version for `boss`: the newest row touching it (0: none).
    pub fn version(&self, boss: &str, session: Uuid) -> u64 {
        self.rows
            .iter()
            .rev()
            .find(|r| r.touches(boss, session))
            .map_or(0, |r| r.seq)
    }

    /// The rows touching `session` for `boss` after `basis`, oldest first,
    /// leaving out those `turn` made itself.
    pub fn since(
        &self,
        boss: &str,
        session: Uuid,
        basis: u64,
        turn: Option<&str>,
    ) -> Vec<Happened> {
        self.rows
            .iter()
            .filter(|r| r.seq > basis && r.touches(boss, session))
            .filter(|r| turn.is_none() || r.turn.as_deref() != turn)
            .cloned()
            .collect()
    }

    /// The newest `n` rows touching `session` for `boss`, oldest first.
    pub fn recent(&self, boss: &str, session: Uuid, n: usize) -> Vec<Happened> {
        let mut out: Vec<Happened> = self
            .rows
            .iter()
            .rev()
            .filter(|r| r.touches(boss, session))
            .take(n)
            .cloned()
            .collect();
        out.reverse();
        out
    }

    /// The newest `n` rows for `boss` that touch no one session (its DM,
    /// facts and orders across its scope), oldest first.
    pub fn recent_for(&self, boss: &str, n: usize) -> Vec<Happened> {
        let mut out: Vec<Happened> = self
            .rows
            .iter()
            .rev()
            .filter(|r| r.session.is_none() && r.boss.as_deref().is_none_or(|b| b == boss))
            .take(n)
            .cloned()
            .collect();
        out.reverse();
        out
    }

    /// Whether the person said something into `session` after `at`.
    pub fn person_since(&self, session: Uuid, at: DateTime<Utc>) -> bool {
        self.rows
            .iter()
            .rev()
            .take_while(|r| r.at > at)
            .any(|r| r.what == What::PersonSaid && r.session == Some(session))
    }

    /// The last instruction `boss` sent `session`, if any is still kept.
    pub fn last_instruction(&self, boss: &str, session: Uuid) -> Option<Happened> {
        self.rows
            .iter()
            .rev()
            .find(|r| {
                r.what == What::Instruction
                    && r.session == Some(session)
                    && r.boss.as_deref() == Some(boss)
            })
            .cloned()
    }
}

/// What a boss reads of the picture for `session`: the rows, one line
/// each, under a heading; none when there are none.
pub fn render(rows: &[Happened], tz: chrono_tz::Tz) -> Option<String> {
    if rows.is_empty() {
        return None;
    }
    Some(
        rows.iter()
            .map(|r| format!("- {}", r.said(tz)))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_763_300_000 + s, 0).unwrap()
    }

    #[test]
    fn a_sessions_version_moves_only_with_what_touches_it() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let mut p = Picture::default();
        assert_eq!(p.version("boss", a), 0);
        p.push(
            Note::new(What::Instruction, "Rebase.")
                .boss("boss")
                .session(a),
            at(0),
        );
        assert_eq!(p.version("boss", a), 1);
        assert_eq!(p.version("boss", b), 0, "another session's row");
        // The person's words to the boss touch every session it oversees.
        p.push(
            Note::new(What::PersonSaid, "Hold merges.").boss("boss"),
            at(1),
        );
        assert_eq!(p.version("boss", a), 2);
        assert_eq!(p.version("boss", b), 2);
        // ...but not another boss's.
        assert_eq!(p.version("other", b), 0);
        // A fact naming one session touches that one.
        p.push(
            Note::new(What::Fact, "Sam owns the fix (#648).")
                .boss("boss")
                .sessions(vec![a]),
            at(2),
        );
        assert_eq!(p.version("boss", a), 3);
        assert_eq!(p.version("boss", b), 2);
        // What the person typed into a session touches it for any boss.
        p.push(Note::new(What::PersonSaid, "stop").session(b), at(3));
        assert_eq!(p.version("other", b), 4);
    }

    #[test]
    fn a_turns_own_sends_never_make_it_stale() {
        let s = Uuid::new_v4();
        let mut p = Picture::default();
        let basis = p.seq();
        p.push(
            Note::new(What::Instruction, "Open the PR.")
                .boss("boss")
                .session(s)
                .turn(Some("line-1".into())),
            at(0),
        );
        assert!(p.since("boss", s, basis, Some("line-1")).is_empty());
        let other = p.since("boss", s, basis, Some("dm-1"));
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].what, What::Instruction);
    }

    #[test]
    fn old_rows_go_but_facts_and_orders_stay_longer() {
        let s = Uuid::new_v4();
        let mut p = Picture::default();
        p.push(Note::new(What::Fact, "Sam owns it.").boss("boss"), at(0));
        p.push(
            Note::new(What::Instruction, "Go.").boss("boss").session(s),
            at(1),
        );
        p.push(
            Note::new(What::PersonSaid, "hi").session(s),
            at(1) + KEPT_FOR + chrono::TimeDelta::seconds(5),
        );
        let kept: Vec<What> = p.recent("boss", s, 10).iter().map(|r| r.what).collect();
        assert_eq!(kept, vec![What::Fact, What::PersonSaid]);
        assert_eq!(p.seq(), 3, "numbering never goes back");
    }

    #[test]
    fn loaded_rows_keep_their_numbers_and_what_came_before_goes_after() {
        let s = Uuid::new_v4();
        let row = |seq| Happened {
            seq,
            at: Utc::now(),
            what: What::Fact,
            boss: Some("boss".into()),
            session: None,
            sessions: vec![],
            text: format!("fact {seq}"),
            turn: None,
        };
        let mut p = Picture::default();
        assert!(!p.loaded());
        // Said before what Postgres kept was read: numbered after it.
        p.push(Note::new(What::PersonSaid, "early").session(s), Utc::now());
        let pending = p.load(vec![row(3), row(7)], 9);
        assert!(p.loaded());
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].seq, 10);
        assert_eq!(p.seq(), 10);
        let r = p.push(Note::new(What::PersonSaid, "x").session(s), Utc::now());
        assert_eq!(r.unwrap().seq, 11);
        let all: Vec<u64> = p.recent("boss", s, 10).iter().map(|r| r.seq).collect();
        assert_eq!(all, vec![3, 7, 10, 11]);
    }

    #[test]
    fn what_round_trips() {
        for w in [
            What::PersonSaid,
            What::Instruction,
            What::SessionReported,
            What::SessionAsked,
            What::Fact,
            What::Order,
            What::HandedBack,
        ] {
            assert_eq!(What::parse(w.as_str()), Some(w));
        }
    }
}
