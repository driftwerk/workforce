//! What the boss owes the person (the boss understands sessions, H5): the
//! questions it asked and the decisions it wants confirmed, kept as rows
//! until they are answered or no longer apply, across restarts and DM
//! rollovers. Every way one opens goes through one insert keyed by
//! [`OpenItem::dedupe_key`], so a question raised twice is one item and one
//! card. Each closes saying who closed it and why; an ask restated three
//! times unanswered is parked, with a Drop.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::text_enum;

text_enum!(OpenKind {
    Ask => "ask",
    Confirm => "confirm",
    Promise => "promise",
    Order => "order",
});

text_enum!(OpenState {
    Open => "open",
    // Restated enough unanswered: no longer restated, kept with a Drop.
    Parked => "parked",
    Closed => "closed",
});

text_enum!(ClosedBy {
    Person => "person",
    Session => "session",
    Boss => "boss",
    Superseded => "superseded",
    Lapsed => "lapsed",
});

/// How old an ask is before it is first restated.
pub const FIRST_RESTATE: Duration = Duration::minutes(30);
/// How many restatements an ask gets before it is parked.
pub const RESTATES: i32 = 3;
/// The most items restated in one go; the rest are counted.
pub const RESTATED_AT_ONCE: usize = 3;
/// How long an item must be unchanged before the nightly sweep looks at it.
pub const SWEEP_AFTER: Duration = Duration::hours(24);

/// One thing the boss owes the person.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenItem {
    pub id: Uuid,
    pub boss: String,
    pub owner: String,
    pub kind: OpenKind,
    pub session: Option<Uuid>,
    pub line_message: Option<Uuid>,
    /// The item said in full, readable cold.
    pub text: String,
    pub holding_up: Option<String>,
    pub recommendation: Option<String>,
    pub options: Vec<String>,
    pub generalizes: Option<String>,
    pub done_when: Option<String>,
    pub dedupe_key: String,
    pub opened_at: DateTime<Utc>,
    pub last_said_at: Option<DateTime<Utc>>,
    pub said_count: i32,
    /// Last put to the boss in a check-in.
    pub raised_at: Option<DateTime<Utc>>,
    pub changed_at: DateTime<Utc>,
    pub state: OpenState,
    pub closed_at: Option<DateTime<Utc>>,
    pub closed_by: Option<ClosedBy>,
    pub closed_why: Option<String>,
}

/// An item to open (or, when one with its key is open, to update).
#[derive(Debug, Clone, PartialEq)]
pub struct NewItem {
    pub boss: String,
    pub kind: OpenKind,
    pub session: Option<Uuid>,
    pub line_message: Option<Uuid>,
    pub text: String,
    pub holding_up: Option<String>,
    pub recommendation: Option<String>,
    pub options: Vec<String>,
    pub generalizes: Option<String>,
    pub done_when: Option<String>,
    pub dedupe_key: String,
    /// It is said to the person as it opens (a card, a needs-you post, the
    /// reply it came from).
    pub said: bool,
}

impl NewItem {
    /// An ask of `boss`'s, keyed by what it is about.
    pub fn ask(boss: &str, kind: OpenKind, session: Option<Uuid>, text: &str) -> Self {
        Self {
            boss: boss.to_string(),
            kind,
            session,
            line_message: None,
            text: text.trim().to_string(),
            holding_up: None,
            recommendation: None,
            options: Vec::new(),
            generalizes: None,
            done_when: None,
            dedupe_key: ask_key(session, None, text),
            said: true,
        }
    }

    /// The watcher's mark that it put `key` (one of its own keys, or an
    /// item's) to the boss: an item never said to the person.
    pub fn raised(boss: &str, key: &str, session: Option<Uuid>, text: &str) -> Self {
        Self {
            boss: boss.to_string(),
            kind: OpenKind::Ask,
            session,
            line_message: None,
            text: text.trim().to_string(),
            holding_up: None,
            recommendation: None,
            options: Vec::new(),
            generalizes: None,
            done_when: None,
            dedupe_key: key.to_string(),
            said: false,
        }
    }

    /// Keyed by the kind of case it would settle, when it says one.
    pub fn generalizing(mut self, generalizes: Option<String>) -> Self {
        let g = generalizes
            .map(|g| g.trim().to_string())
            .filter(|g| !g.is_empty());
        self.dedupe_key = ask_key(self.session, g.as_deref(), &self.text);
        self.generalizes = g;
        self
    }
}

/// Text as a key: lower case, words only, at most 200 characters.
pub fn normalised(text: &str) -> String {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    words.join(" ").chars().take(200).collect()
}

/// The key of a session's question on its line.
pub fn line_key(message: Uuid) -> String {
    format!("line:{message}")
}

/// The key of a session's request.
pub fn approval_key(session: &str, approval: &str) -> String {
    format!("approval:{session}:{approval}")
}

/// The key of the boss's own ask: by the case it would settle, or by what
/// it says.
pub fn ask_key(session: Option<Uuid>, generalizes: Option<&str>, text: &str) -> String {
    let about = session.map_or_else(|| "none".to_string(), |s| s.to_string());
    let what = match generalizes.map(normalised).filter(|g| !g.is_empty()) {
        Some(g) => g,
        None => normalised(text),
    };
    format!("ask:{about}:{what}")
}

impl OpenItem {
    /// How many times it was put to the person again after it was first
    /// said.
    pub fn restated(&self) -> i32 {
        (self.said_count - 1).max(0)
    }

    /// Whether the person was ever told of it: an item never said is only
    /// the watcher's mark, and is neither listed nor restated.
    pub fn said(&self) -> bool {
        self.said_count > 0
    }

    /// Whether it is restated now, at `now`: an open ask or confirm said to
    /// the person, first once it is `first` old ([`FIRST_RESTATE`]), then
    /// every `remind`.
    pub fn due(&self, now: DateTime<Utc>, first: Duration, remind: Duration) -> bool {
        if self.state != OpenState::Open
            || !matches!(self.kind, OpenKind::Ask | OpenKind::Confirm)
            || !self.said()
        {
            return false;
        }
        if now - self.opened_at < first {
            return false;
        }
        let gap = match self.restated() {
            0 => first,
            _ => remind,
        };
        self.last_said_at.is_none_or(|at| now - at >= gap)
    }

    /// Whether, restated once more, it is parked.
    pub fn parks_after_this(&self) -> bool {
        self.restated() + 1 >= RESTATES
    }

    /// Its age as said ("2 h 10 min", "25 min").
    pub fn age(&self, now: DateTime<Utc>) -> String {
        let m = (now - self.opened_at).num_minutes().max(1);
        match (m / 60, m % 60) {
            (0, m) => format!("{m} min"),
            (h, 0) => format!("{h} h"),
            (h, m) => format!("{h} h {m} min"),
        }
    }

    /// "Still waiting on you: {text} · {age} · {holding up}".
    pub fn still_waiting(&self, now: DateTime<Utc>) -> String {
        let mut s = format!("Still waiting on you: {} · {}", self.text, self.age(now));
        if let Some(h) = self.holding_up.as_deref().filter(|h| !h.trim().is_empty()) {
            s.push_str(&format!(" · {}", h.trim()));
        }
        if let Some(r) = self
            .recommendation
            .as_deref()
            .filter(|r| !r.trim().is_empty())
        {
            s.push_str(&format!("\n\nI'd say: {}", r.trim()));
        }
        s
    }

    /// As the chat's extras carry it.
    pub fn mention(&self) -> MentionedItem {
        MentionedItem {
            item_id: self.id.to_string(),
            recommendation: self.recommendation.clone(),
            options: self.options.clone(),
            generalizes: self.generalizes.clone(),
            parked: self.state == OpenState::Parked,
        }
    }
}

/// A needs-you item, as the chat's extras carry it (`extras.item`): every
/// field but its id is optional on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MentionedItem {
    pub item_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generalizes: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parked: bool,
}

/// Open items as the boss reads them ("what's open?"): grouped by kind,
/// parked ones last, at `tz`.
pub fn listed(items: &[OpenItem], now: DateTime<Utc>, tz: chrono_tz::Tz) -> String {
    let mut out = String::new();
    let line = |i: &OpenItem| {
        let mut l = format!(
            "- [{}] {} (item {}, opened {}, said {} time{}",
            i.kind.as_str(),
            i.text,
            i.id,
            i.opened_at.with_timezone(&tz).format("%H:%M"),
            i.said_count,
            if i.said_count == 1 { "" } else { "s" }
        );
        if let Some(at) = i.last_said_at {
            l.push_str(&format!(
                ", last at {}",
                at.with_timezone(&tz).format("%H:%M")
            ));
        }
        l.push(')');
        if let Some(h) = &i.holding_up {
            l.push_str(&format!(" · holding up: {h}"));
        }
        if let Some(d) = &i.done_when {
            l.push_str(&format!(" · done when: {d}"));
        }
        if !i.options.is_empty() {
            l.push_str(&format!(" · answers: {}", i.options.join(" / ")));
        }
        l.push_str(&format!(" · {} old\n", i.age(now)));
        l
    };
    for kind in [
        OpenKind::Ask,
        OpenKind::Confirm,
        OpenKind::Promise,
        OpenKind::Order,
    ] {
        let of: Vec<&OpenItem> = items
            .iter()
            .filter(|i| i.kind == kind && i.state == OpenState::Open && i.said())
            .collect();
        if of.is_empty() {
            continue;
        }
        out.push_str(&format!(
            "{}:\n",
            match kind {
                OpenKind::Ask => "Questions you asked",
                OpenKind::Confirm => "Decisions you want confirmed",
                OpenKind::Promise => "Promises you made",
                OpenKind::Order => "The person's orders",
            }
        ));
        of.into_iter().for_each(|i| out.push_str(&line(i)));
    }
    let parked: Vec<&OpenItem> = items
        .iter()
        .filter(|i| i.state == OpenState::Parked && i.said())
        .collect();
    if !parked.is_empty() {
        out.push_str("Parked (no longer restated; the person can answer or drop them):\n");
        parked.into_iter().for_each(|i| out.push_str(&line(i)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(age: i64, said: i32, last: Option<i64>) -> OpenItem {
        let now = Utc::now();
        OpenItem {
            id: Uuid::new_v4(),
            boss: "desk".into(),
            owner: "you".into(),
            kind: OpenKind::Ask,
            session: None,
            line_message: None,
            text: "Merge with CI red?".into(),
            holding_up: Some("Billing migration is paused on this".into()),
            recommendation: None,
            options: vec![],
            generalizes: None,
            done_when: None,
            dedupe_key: "k".into(),
            opened_at: now - Duration::minutes(age),
            last_said_at: last.map(|l| now - Duration::minutes(l)),
            said_count: said,
            raised_at: None,
            changed_at: now,
            state: OpenState::Open,
            closed_at: None,
            closed_by: None,
            closed_why: None,
        }
    }

    #[test]
    fn an_ask_is_restated_at_30_minutes_then_every_reminder_and_parked_after_three() {
        let now = Utc::now();
        let (first, remind) = (FIRST_RESTATE, Duration::minutes(120));
        assert!(!item(20, 1, Some(20)).due(now, first, remind), "too young");
        assert!(
            item(31, 1, Some(31)).due(now, first, remind),
            "first at 30 min"
        );
        assert!(
            !item(60, 2, Some(29)).due(now, first, remind),
            "then not before the reminder"
        );
        assert!(item(200, 2, Some(121)).due(now, first, remind));
        assert!(
            !item(200, 0, None).due(now, first, remind),
            "the watcher's mark is never said to the person"
        );
        let mut parked = item(500, 4, Some(200));
        parked.state = OpenState::Parked;
        assert!(!parked.due(now, first, remind), "parked is never restated");
        assert!(!item(31, 1, Some(31)).parks_after_this());
        assert!(!item(31, 2, Some(31)).parks_after_this());
        assert!(
            item(31, 3, Some(31)).parks_after_this(),
            "the third restatement parks it"
        );
        let mut promise = item(500, 1, Some(500));
        promise.kind = OpenKind::Promise;
        assert!(!promise.due(now, first, remind), "promises aren't nagged");
    }

    #[test]
    fn keys_are_one_per_matter() {
        let s = Uuid::new_v4();
        assert_eq!(
            ask_key(Some(s), None, "Merge with CI red?"),
            ask_key(Some(s), None, "  merge with CI   red ")
        );
        assert_eq!(
            ask_key(
                Some(s),
                Some("When a session wants to merge with CI red"),
                "Merge #12 now?"
            ),
            ask_key(
                Some(s),
                Some("when a session wants to merge with CI red."),
                "Merge #14?"
            )
        );
        assert_ne!(ask_key(Some(s), None, "x"), ask_key(None, None, "x"));
        assert!(ask_key(None, None, "x").starts_with("ask:none:"));
        assert_eq!(approval_key("s1", "a1"), "approval:s1:a1");
    }

    #[test]
    fn still_waiting_says_it_in_full_with_its_age_and_what_it_holds_up() {
        let i = item(130, 2, Some(10));
        let said = i.still_waiting(Utc::now());
        assert!(
            said.starts_with("Still waiting on you: Merge with CI red? · 2 h 10 min · Billing"),
            "{said}"
        );
        let listed = listed(std::slice::from_ref(&i), Utc::now(), chrono_tz::UTC);
        assert!(
            listed.starts_with("Questions you asked:\n- [ask] Merge with CI red?"),
            "{listed}"
        );
        let v = serde_json::to_value(i.mention()).unwrap();
        assert!(v.get("parked").is_none() && v.get("options").is_none());
    }
}
