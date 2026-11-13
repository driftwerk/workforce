//! The boss's decision log (the boss understands sessions, H4): every
//! thing it did for a session, and every turn it chose to do nothing, is a
//! row on the hub, never read back out of its prose. Each says what was
//! decided, why (as said to the person), how (by itself, by a preference,
//! escalated, or the person first) and how it can be undone, honestly for
//! what the action was.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::model::text_enum;

text_enum!(Need {
    Approval => "approval",
    Question => "question",
    Choice => "choice",
    Stall => "stall",
    TurnEnd => "turn_end",
    Order => "order",
});

text_enum!(How {
    // The boss settled it by itself.
    Own => "own",
    // By one of the person's preferences.
    Preference => "preference",
    // It gave the matter to the person.
    Escalated => "escalated",
    // The person did it first (an Undo, say).
    PersonFirst => "person_first",
});

text_enum!(Undo {
    // A line message still queued or held: taken back before it goes in.
    Withdraw => "withdraw",
    // The reverse call (release for take on, resume for stop, ...).
    Reverse => "reverse",
    // Already delivered: a correction is sent.
    Correct => "correct",
    // What ran can't be taken back.
    None => "none",
});

/// One decision of a boss's.
#[derive(Debug, Clone, PartialEq)]
pub struct BossDecision {
    pub id: Uuid,
    /// The boss's agent id.
    pub boss: String,
    pub owner: String,
    pub at: DateTime<Utc>,
    pub session: Option<Uuid>,
    pub need: Need,
    /// The approval id, line message id or open item id it settled.
    pub need_ref: Option<String>,
    /// What was decided, as the person reads it ("told it to wait for green CI").
    pub what: String,
    /// Why, as said to the person.
    pub why: String,
    pub how: How,
    /// The preference it applied (H9).
    pub preference: Option<Uuid>,
    /// The crew action and what its undo needs, as [`UndoPlan`].
    pub action: Value,
    pub undo: Undo,
    pub undone_at: Option<DateTime<Utc>>,
    /// The Activity event it also wrote, when known.
    pub event: Option<String>,
}

impl BossDecision {
    /// A new decision of `boss`'s, now.
    pub fn new(boss: &str, need: Need, what: &str, why: &str, how: How) -> Self {
        Self {
            id: Uuid::new_v4(),
            boss: boss.to_string(),
            owner: crate::rooms::YOU.to_string(),
            at: crate::crew::now(),
            session: None,
            need,
            need_ref: None,
            what: what.trim().to_string(),
            why: why.trim().to_string(),
            how,
            preference: None,
            action: Value::Null,
            undo: Undo::None,
            undone_at: None,
            event: None,
        }
    }

    /// What it undoes with, as kept.
    pub fn plan(&self) -> Option<UndoPlan> {
        serde_json::from_value(self.action.clone()).ok()
    }

    /// The line the person reads in the chat: "I decided: … because …".
    pub fn said(&self) -> String {
        match self.why.trim() {
            "" => format!("I decided: {}.", self.what.trim_end_matches('.')),
            why => format!("I decided: {} — {}", self.what.trim_end_matches('.'), why),
        }
    }

    /// As the chat's extras carry it.
    pub fn line(&self) -> DecisionLine {
        DecisionLine {
            id: self.id.to_string(),
            text: self.what.clone(),
            why: self.why.clone(),
            undo: self.button().map(str::to_string),
            undone: self.undone_at.is_some(),
            preference: self.preference.map(|p| p.to_string()),
        }
    }

    /// The button the chat shows for undoing it, if any: `undo`, `correct`,
    /// `stop` (a session it started) or `resume` (one it interrupted).
    pub fn button(&self) -> Option<&'static str> {
        match (self.undo, self.plan()) {
            (Undo::None, _) | (_, None) => None,
            (Undo::Correct, _) => Some("correct"),
            (
                Undo::Withdraw,
                Some(UndoPlan::Line {
                    delivered: true, ..
                }),
            ) => Some("correct"),
            (Undo::Withdraw, _) => Some("undo"),
            (Undo::Reverse, Some(UndoPlan::StopStarted { .. })) => Some("stop"),
            (
                Undo::Reverse,
                Some(UndoPlan::Resume {
                    prompt: Some(_), ..
                }),
            ) => Some("resume"),
            (Undo::Reverse, _) => Some("undo"),
        }
    }
}

/// What undoing a decision does, kept with it when it is recorded: never
/// worked out from what the boss said.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UndoPlan {
    /// A line message to a session (a nudge, or an answer to its
    /// question): withdrawn while it is queued or held, corrected once it
    /// went in. `delivered` is what it was when recorded; the undo reads
    /// the row again.
    Line {
        session: Uuid,
        message: Uuid,
        /// What it said.
        text: String,
        /// The question it answered, when it was an answer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ask: Option<Uuid>,
        #[serde(default)]
        delivered: bool,
    },
    /// Tell a session it let wait to go ahead.
    GoAhead { session: Uuid },
    /// Back to the mode it ran in.
    SetMode { session: Uuid, mode: String },
    /// Let go a session it took on.
    Release { session: Uuid },
    /// Take on again a session it let go.
    TakeOn { session: Uuid },
    /// Move a ticket back to the column it was in.
    MoveTicket {
        ticket: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Uuid>,
        state: String,
    },
    /// Resume a session it stopped, or (with a prompt) one it interrupted.
    Resume {
        session: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    /// Stop a session it started.
    StopStarted { session: Uuid },
}

impl UndoPlan {
    /// The session it acts on, when one.
    pub fn session(&self) -> Option<Uuid> {
        match self {
            Self::Line { session, .. }
            | Self::GoAhead { session }
            | Self::SetMode { session, .. }
            | Self::Release { session }
            | Self::TakeOn { session }
            | Self::Resume { session, .. }
            | Self::StopStarted { session } => Some(*session),
            Self::MoveTicket { .. } => None,
        }
    }

    pub fn value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// A decision, as the chat's extras carry it (`extras.decisions`): every
/// field but `id`, `text` and `why` is optional on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DecisionLine {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub why: String,
    /// `undo` | `correct` | `stop` | `resume`; none when it can't be undone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub undone: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preference: Option<String>,
}

/// How many needs a boss settled itself, gave the person, or the person
/// got to first, of `decisions`: what "handled 23 of 25 myself" is
/// counted from. Choices and the turns it let wait are not needs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Handled {
    pub own: usize,
    pub preference: usize,
    pub escalated: usize,
    pub person_first: usize,
}

impl Handled {
    pub fn of(decisions: &[BossDecision]) -> Self {
        let mut h = Self::default();
        for d in decisions {
            if !matches!(d.need, Need::Approval | Need::Question | Need::Stall) {
                continue;
            }
            match d.how {
                How::Own => h.own += 1,
                How::Preference => h.preference += 1,
                How::Escalated => h.escalated += 1,
                How::PersonFirst => h.person_first += 1,
            }
        }
        h
    }

    pub fn needs(&self) -> usize {
        self.own + self.preference + self.escalated + self.person_first
    }

    pub fn itself(&self) -> usize {
        self.own + self.preference
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(undo: Undo, plan: UndoPlan) -> BossDecision {
        let mut d = BossDecision::new("desk", Need::Choice, "did it", "why", How::Own);
        d.undo = undo;
        d.action = plan.value();
        d
    }

    #[test]
    fn each_kind_of_undo_shows_its_own_button() {
        let s = Uuid::new_v4();
        let line = |delivered| UndoPlan::Line {
            session: s,
            message: Uuid::new_v4(),
            text: "go".into(),
            ask: None,
            delivered,
        };
        assert_eq!(with(Undo::Withdraw, line(false)).button(), Some("undo"));
        assert_eq!(with(Undo::Withdraw, line(true)).button(), Some("correct"));
        assert_eq!(with(Undo::Correct, line(true)).button(), Some("correct"));
        assert_eq!(
            with(Undo::Reverse, UndoPlan::StopStarted { session: s }).button(),
            Some("stop")
        );
        assert_eq!(
            with(
                Undo::Reverse,
                UndoPlan::Resume {
                    session: s,
                    prompt: Some("carry on".into())
                }
            )
            .button(),
            Some("resume")
        );
        assert_eq!(
            with(
                Undo::Reverse,
                UndoPlan::Resume {
                    session: s,
                    prompt: None
                }
            )
            .button(),
            Some("undo")
        );
        assert_eq!(
            with(Undo::Reverse, UndoPlan::Release { session: s }).button(),
            Some("undo")
        );
        assert_eq!(
            with(Undo::None, UndoPlan::Release { session: s }).button(),
            None
        );
        let mut plain = BossDecision::new("desk", Need::Approval, "allowed it", "", How::Own);
        plain.undo = Undo::None;
        assert_eq!(plain.button(), None);
        assert_eq!(plain.said(), "I decided: allowed it.");
    }

    #[test]
    fn the_extras_line_is_optional_past_its_text() {
        let d = with(
            Undo::None,
            UndoPlan::GoAhead {
                session: Uuid::new_v4(),
            },
        );
        let v = serde_json::to_value(d.line()).unwrap();
        assert!(v.get("undo").is_none() && v.get("undone").is_none());
        assert!(v.get("preference").is_none());
        let back: DecisionLine =
            serde_json::from_value(serde_json::json!({"id": "x", "text": "t"})).unwrap();
        assert_eq!(back.undo, None);
    }

    #[test]
    fn handled_counts_needs_only() {
        let mk = |need, how| BossDecision::new("desk", need, "x", "", how);
        let all = vec![
            mk(Need::Approval, How::Own),
            mk(Need::Question, How::Escalated),
            mk(Need::Question, How::Preference),
            mk(Need::Stall, How::PersonFirst),
            mk(Need::TurnEnd, How::Own),
            mk(Need::Choice, How::Own),
        ];
        let h = Handled::of(&all);
        assert_eq!((h.itself(), h.needs()), (2, 4));
        assert_eq!(h.escalated, 1);
        assert_eq!(h.person_first, 1);
    }
}
