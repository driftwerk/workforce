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
    // The person undoing one of its decisions: not a need of its own.
    Undo => "undo",
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

/// What a decision was, as the chat lays it out: the times it left a
/// session alone fold into one line.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    /// It let a session wait, or sent it nothing.
    LeftAlone,
    /// It gave a session's question to the person, or left an action to
    /// them.
    Escalated,
    /// The person undid one of its decisions.
    Undone,
    /// It did something for a session; a kind this build doesn't know
    /// reads as this, so a read never fails on it.
    #[default]
    #[serde(other)]
    Acted,
}

impl DecisionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Acted => "acted",
            Self::LeftAlone => "left_alone",
            Self::Escalated => "escalated",
            Self::Undone => "undone",
        }
    }

    /// `s` as kept; one this build doesn't know is [`Self::Acted`].
    pub fn parse(s: &str) -> eyre::Result<Self> {
        Ok(match s {
            "acted" => Self::Acted,
            "left_alone" => Self::LeftAlone,
            "escalated" => Self::Escalated,
            "undone" => Self::Undone,
            _ => Self::Acted,
        })
    }
}

/// The most characters a decision's short line has.
pub const BRIEF_CHARS: usize = 70;
/// The most characters its one-sentence why has.
pub const WHY_CHARS: usize = 200;

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
    /// What was done, short ("Told web to wait for green CI."): never the
    /// message it sent. Empty on older rows: [`Self::brief`] says it then.
    pub brief: String,
    /// Why, in one sentence. Empty on older rows.
    pub brief_why: String,
    /// The full text sent to the session; none when nothing was sent.
    pub message: Option<String>,
    pub kind: DecisionKind,
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
            brief: String::new(),
            brief_why: String::new(),
            message: None,
            kind: DecisionKind::Acted,
        }
    }

    /// With its short line and one-sentence why, and the message it sent.
    pub fn briefly(mut self, brief: &str, message: Option<&str>) -> Self {
        self.message = message
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string);
        self.brief = brief_line(brief, self.message.as_deref()).unwrap_or_default();
        self.brief_why = why_line(&self.why, self.message.as_deref());
        self
    }

    /// Its short line: as kept, else from what was decided (an older row),
    /// never with the message it sent in it.
    pub fn brief(&self) -> String {
        brief_line(&self.brief, self.message.as_deref())
            .or_else(|| brief_line(&self.what, self.message.as_deref()))
            .unwrap_or_else(|| {
                let what = crate::crew::one_line(&self.what, 200);
                let cut = what.split(['“', '"']).next().unwrap_or_default().trim();
                sentence(&crate::crew::clip(
                    cut.trim_end_matches(':'),
                    BRIEF_CHARS - 1,
                ))
            })
    }

    /// Its why in one sentence: as kept, else from the full why.
    pub fn brief_why(&self) -> String {
        match self.brief_why.trim() {
            "" => why_line(&self.why, self.message.as_deref()),
            w => w.to_string(),
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
        self.line_now(None)
    }

    /// As the chat's extras carry it now: once undone, no button; a line
    /// message's button as its row now is (`delivery`: still waiting is
    /// Undo, gone in is Correct, never going in is none).
    pub fn line_now(&self, delivery: Option<crate::line::Delivery>) -> DecisionLine {
        use crate::line::Delivery;
        let button = match (self.undone_at, self.plan(), delivery) {
            (Some(_), _, _) => None,
            (None, Some(UndoPlan::Line { .. }), Some(d)) if self.undo != Undo::None => match d {
                Delivery::Queued | Delivery::Held => Some("undo"),
                Delivery::Handed | Delivery::Delivered | Delivery::Noted => Some("correct"),
                Delivery::Refused | Delivery::Withdrawn => None,
            },
            _ => self.button(),
        };
        DecisionLine {
            id: self.id.to_string(),
            text: self.what.clone(),
            why: self.brief_why(),
            undo: button.map(str::to_string),
            undone: self.undone_at.is_some(),
            preference: self.preference.map(|p| p.to_string()),
            at: Some(self.at),
            action: self.brief(),
            message: self.message.clone(),
            kind: self.kind,
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

/// `text` as a sentence: a capital first, a full stop last.
fn sentence(text: &str) -> String {
    let t = text.trim();
    let mut c = t.chars();
    let Some(first) = c.next() else {
        return String::new();
    };
    let mut out: String = first.to_uppercase().chain(c).collect();
    if !out.ends_with(['.', '!', '?', '…']) {
        out.push('.');
    }
    out
}

/// Whether `text` says `message`, or a good part of it: the short line
/// and why never quote what was sent. A short message is found by its
/// words together, a longer one by any run of five of them.
pub fn quotes(text: &str, message: Option<&str>) -> bool {
    let words = |s: &str| -> Vec<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_lowercase)
            .collect()
    };
    let m = words(message.unwrap_or_default());
    let t = format!(" {} ", words(text).join(" "));
    let run = |w: &[String]| t.contains(&format!(" {} ", w.join(" ")));
    match m.len() {
        0 => false,
        n if n < 5 => run(&m),
        _ => m.windows(5).any(run),
    }
}

/// `text` as a decision's short line: one line, at most [`BRIEF_CHARS`],
/// a sentence; none when empty or it quotes `message`.
pub fn brief_line(text: &str, message: Option<&str>) -> Option<String> {
    let cut = crate::crew::one_line(text, BRIEF_CHARS);
    // Its full stop only where it fits: a line of exactly 70 stays whole.
    let line = match sentence(&cut) {
        s if s.chars().count() > BRIEF_CHARS => s.chars().take(BRIEF_CHARS).collect(),
        s => s,
    };
    (!line.trim().is_empty() && !quotes(&line, message)).then_some(line)
}

/// `why` as one sentence, never quoting `message`: its first sentence
/// that doesn't, or empty when none is left.
pub fn why_line(why: &str, message: Option<&str>) -> String {
    sentences(why)
        .into_iter()
        .map(|s| sentence(&crate::crew::clip(s, WHY_CHARS - 1)))
        .find(|s| !s.is_empty() && !quotes(s, message))
        .unwrap_or_default()
}

/// `text`'s sentences in order: each line's, split after `.`, `!` or `?`
/// followed by a space or the line's end.
fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim().trim_start_matches(['#', '>', '-', '*']).trim();
        let mut start = 0;
        for (i, c) in line.char_indices() {
            let end = i + c.len_utf8();
            if matches!(c, '.' | '!' | '?')
                && line[end..].chars().next().is_none_or(char::is_whitespace)
            {
                out.push(line[start..end].trim());
                start = end;
            }
        }
        out.push(line[start..].trim());
    }
    out.retain(|s| !s.is_empty());
    out
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
    /// When it was decided: a catch-up says each one's time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<chrono::DateTime<chrono::Utc>>,
    /// What was done, short (at most [`BRIEF_CHARS`]): never the message.
    #[serde(default)]
    pub action: String,
    /// The full text sent to the session, behind the chat's Message; null
    /// when nothing was sent.
    #[serde(default)]
    pub message: Option<String>,
    /// `acted` | `left_alone` | `escalated` | `undone`.
    #[serde(default)]
    pub kind: DecisionKind,
}

impl DecisionLine {
    /// Its layout as `d`, its row, says it: the kind and message always,
    /// the short line and why only where it has none (the composer's
    /// words stay).
    pub fn laid_out_from(&mut self, d: &BossDecision) {
        self.kind = d.kind;
        self.message = d.message.clone();
        // A line kept before the layout (no short line) takes the row's
        // short line and one-sentence why; a composed one keeps its words.
        if self.action.trim().is_empty() {
            self.action = d.brief();
            self.why = d.brief_why();
        } else if self.why.trim().is_empty() {
            self.why = d.brief_why();
        }
    }
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
            h.add(d.need, d.how, 1);
        }
        h
    }

    /// Counts `n` decisions of `need` made `how`: only needs count.
    pub fn add(&mut self, need: Need, how: How, n: usize) {
        if !matches!(need, Need::Approval | Need::Question | Need::Stall) {
            return;
        }
        match how {
            How::Own => self.own += n,
            How::Preference => self.preference += n,
            How::Escalated => self.escalated += n,
            How::PersonFirst => self.person_first += n,
        }
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

    #[test]
    fn the_short_line_is_bounded_and_never_quotes_the_message() {
        let message =
            "Please hold the release tonight until the migration check passes on staging.";
        let d = BossDecision::new(
            "desk",
            Need::Choice,
            &format!("told web “{message}”"),
            "The migration check failed twice. It might be flaky.",
            How::Own,
        )
        .briefly("Sent web a message", Some(message));
        assert_eq!(d.brief, "Sent web a message.");
        assert_eq!(d.brief_why, "The migration check failed twice.");
        let l = d.line();
        assert_eq!(l.action, "Sent web a message.");
        assert_eq!(l.why, "The migration check failed twice.");
        assert_eq!(l.message.as_deref(), Some(message));
        assert_eq!(l.kind, DecisionKind::Acted);
        // The old text is still there, as it was.
        assert!(l.text.contains(message));
        // A line that says the message is not a short line; nor a why.
        assert_eq!(
            brief_line(&format!("Told web: {message}"), Some(message)),
            None
        );
        assert_eq!(why_line(message, Some(message)), "");
        // A reason whose first sentence quotes it says the next one.
        assert_eq!(
            why_line(
                &format!("I told it: {message} The check failed twice.\nIt may be flaky."),
                Some(message)
            ),
            "The check failed twice."
        );
        assert!(quotes("Told web “5433”.", Some("5433")));
        assert!(!quotes("Answered web's question.", Some("5433")));
        // At most 70 characters, counted as characters, ellipsis and all.
        let long =
            "Überprüfte die Läufe für jede einzelne Sitzung, die heute noch offen war, und mehr";
        let b = brief_line(long, None).unwrap();
        assert!(b.chars().count() <= BRIEF_CHARS, "{b}");
        assert!(b.ends_with('…'), "{b}");
        let back = brief_line(&b, None).unwrap();
        assert_eq!(back, b, "a bounded line stays as it is");
    }

    #[test]
    fn an_older_row_reads_short_from_what_it_decided() {
        let message = "Wait for green CI before merging, then deploy to staging only.";
        let mut d = BossDecision::new(
            "desk",
            Need::Choice,
            &format!("told web “{message}”"),
            "CI is red on main.",
            How::Own,
        );
        // As V060 left it: no short line, its message from its undo plan.
        d.message = Some(message.into());
        let l = d.line();
        assert_eq!(l.action, "Told web.");
        assert!(!l.action.contains("green CI"));
        assert_eq!(l.why, "CI is red on main.");
        let plain = BossDecision::new("desk", Need::Approval, "allowed web", "", How::Own);
        assert_eq!(plain.line().action, "Allowed web.");
        assert_eq!(plain.line().message, None);
    }

    #[test]
    fn the_kinds_and_message_are_on_the_wire() {
        let mut d = BossDecision::new("desk", Need::TurnEnd, "let web wait", "CI runs.", How::Own)
            .briefly("let web wait", None);
        d.kind = DecisionKind::LeftAlone;
        let v = serde_json::to_value(d.line()).unwrap();
        assert_eq!(v["kind"], "left_alone");
        assert_eq!(v["action"], "Let web wait.");
        assert!(v.get("message").is_some_and(Value::is_null), "{v}");
        for k in [
            DecisionKind::Acted,
            DecisionKind::LeftAlone,
            DecisionKind::Escalated,
            DecisionKind::Undone,
        ] {
            assert_eq!(DecisionKind::parse(k.as_str()).unwrap(), k);
            assert_eq!(serde_json::to_value(k).unwrap(), k.as_str());
        }
        // A line kept before these fields reads with their defaults.
        let old: DecisionLine = serde_json::from_value(serde_json::json!({
            "id": "x", "text": "told web “go”", "why": "CI is green.", "undo": "undo"
        }))
        .unwrap();
        assert_eq!((old.action.as_str(), old.message), ("", None));
        assert_eq!(old.kind, DecisionKind::Acted);
    }

    #[test]
    fn a_line_of_exactly_70_stays_whole_and_an_unknown_kind_reads_as_acted() {
        let seventy = "a".repeat(BRIEF_CHARS);
        let b = brief_line(&seventy, None).unwrap();
        assert_eq!(b.chars().count(), BRIEF_CHARS);
        assert!(b.starts_with('A') && !b.ends_with('…'), "{b}");
        assert_eq!(brief_line("Held web.", None).as_deref(), Some("Held web."));
        assert_eq!(DecisionKind::parse("later").unwrap(), DecisionKind::Acted);
        let k: DecisionKind = serde_json::from_value(serde_json::json!("later")).unwrap();
        assert_eq!(k, DecisionKind::Acted);
    }

    #[test]
    fn an_update_line_takes_its_rows_layout_and_keeps_the_composers_words() {
        let mut d = BossDecision::new("desk", Need::TurnEnd, "let web wait", "CI runs.", How::Own)
            .briefly("let web wait", None);
        d.kind = DecisionKind::LeftAlone;
        // Kept before the layout: no short line, the full why.
        let mut old: DecisionLine = serde_json::from_value(serde_json::json!({
            "id": d.id.to_string(), "text": "let web wait", "why": ""
        }))
        .unwrap();
        old.laid_out_from(&d);
        assert_eq!(old.kind, DecisionKind::LeftAlone);
        assert_eq!(
            (old.action.as_str(), old.why.as_str()),
            ("Let web wait.", "CI runs.")
        );
        // Kept before the layout with the full why: one sentence of it.
        let mut long: DecisionLine = serde_json::from_value(serde_json::json!({
            "id": d.id.to_string(), "text": "let web wait", "why": "CI runs. It takes an hour."
        }))
        .unwrap();
        long.laid_out_from(&d);
        assert_eq!(long.why, "CI runs.");
        // Composed: its words stay.
        let mut composed = d.line();
        composed.action = "Left web to its tests.".into();
        composed.laid_out_from(&d);
        assert_eq!(composed.action, "Left web to its tests.");
    }
}
