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
    // What only the person can do (rotate their own credentials): one
    // line, no answers, Done when it's done.
    Todo => "todo",
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

/// Why an ask closed when the person settled it in its session
/// themselves: what tells it apart from one answered with Desk's buttons.
pub const SETTLED_WHY: &str = "You settled it in the session.";
/// How `i` was settled in its session, if it was: the person there
/// ([`SETTLED_WHY`]), or the session itself.
pub fn settled_by(i: &OpenItem) -> Option<crate::update::SettledBy> {
    use crate::update::SettledBy;
    match (i.state, i.closed_by, i.closed_why.as_deref()) {
        (OpenState::Closed, _, Some(SETTLED_WHY)) => Some(SettledBy::You),
        (OpenState::Closed, Some(ClosedBy::Session), _) => Some(SettledBy::Session),
        _ => None,
    }
}

/// Why a "for you" item closed when the person said they did it.
pub const DONE_WHY: &str = "You did it.";

/// How old an ask is before it is first restated.
pub const FIRST_RESTATE: Duration = Duration::minutes(30);
/// How many restatements an ask gets before it is parked.
pub const RESTATES: i32 = 3;
/// The most items restated in one go; the rest are counted.
pub const RESTATED_AT_ONCE: usize = 3;
/// How long an item must be unchanged before the nightly sweep looks at it.
pub const SWEEP_AFTER: Duration = Duration::hours(24);

/// How an ask is laid out for the person, beside its full text: a
/// one-line question, its points, the option it recommends and what carries
/// on meanwhile. Each is what the boss gave; none is made up.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskLayout {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub points: Vec<String>,
    /// One of its options.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_blocking: Option<String>,
}

/// The most points an ask's layout keeps, and the most characters each.
pub const POINTS: usize = 6;
pub const POINT_CHARS: usize = 120;
/// The most points the person is shown, and the most words in each.
pub const SHOWN_POINTS: usize = 2;
pub const POINT_WORDS: usize = 12;
/// The most words an ask's one line has, as the person reads it.
pub const QUESTION_WORDS: usize = 15;
/// The most characters it has.
pub const QUESTION_CHARS: usize = 160;

/// What an ask offers the person to answer with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Offer {
    pub recommendation: Option<String>,
    pub options: Vec<String>,
}

/// The one of `options` that `answer` names, ignoring case and spaces at
/// either end.
pub fn matching_option<'a>(options: &'a [String], answer: &str) -> Option<&'a str> {
    let want = answer.trim().to_lowercase();
    options
        .iter()
        .find(|o| o.trim().to_lowercase() == want)
        .map(String::as_str)
}

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
    /// The sessions a promise or order is linked to (with `session`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<Uuid>,
    /// What an "always" to it would cover (`all`, `group:<id>`,
    /// `repo:<owner/name>`), as the boss proposed; all when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefer_scope: Option<String>,
    pub changed_at: DateTime<Utc>,
    pub state: OpenState,
    pub closed_at: Option<DateTime<Utc>>,
    pub closed_by: Option<ClosedBy>,
    pub closed_why: Option<String>,
    #[serde(default)]
    pub layout: AskLayout,
    /// Why the boss couldn't decide it: at Act fully, the one thing that
    /// lets an ask carry answers.
    #[serde(default)]
    pub cant_decide: Option<String>,
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
    /// The sessions a promise or order is linked to.
    pub sessions: Vec<Uuid>,
    pub layout: AskLayout,
    pub cant_decide: Option<String>,
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
            sessions: Vec::new(),
            layout: AskLayout::default(),
            cant_decide: None,
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
            sessions: Vec::new(),
            layout: AskLayout::default(),
            cant_decide: None,
        }
    }

    /// A promise of `boss`'s to the person about `session`, kept when
    /// `done_when` ("merged", "in production", "replied").
    pub fn promise(boss: &str, session: Uuid, text: &str, done_when: &str) -> Self {
        Self {
            done_when: Some(done_when.trim().to_string()),
            dedupe_key: promise_key(session, done_when),
            sessions: vec![session],
            ..Self::ask(boss, OpenKind::Promise, Some(session), text)
        }
    }

    /// The person's order, in their words, with the sessions working on it;
    /// keyed by their message (`message`) when it came from one, else by its
    /// words.
    pub fn order(boss: &str, text: &str, sessions: Vec<Uuid>, message: Option<&str>) -> Self {
        Self {
            dedupe_key: match message {
                Some(m) => format!("order:{m}"),
                None => format!("order:text:{}", normalised(text)),
            },
            sessions,
            ..Self::ask(boss, OpenKind::Order, None, text)
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

/// The key of a promise: its session and when it is kept.
pub fn promise_key(session: Uuid, done_when: &str) -> String {
    format!("promise:{session}:{}", normalised(done_when))
}

/// What `done` (a session's account's done list) says kept `done_when`
/// since `since`, if anything: "merged" by a PR it merged, "in production"
/// or "deployed" by a deploy, "PR open" by a PR it opened, and anything
/// else by its words, whole. Words said as not done ("not merged yet",
/// "couldn't deploy") and other merges ("merged main into the branch")
/// keep nothing.
pub fn kept_by<'a>(
    done_when: &str,
    done: &'a [crate::account::Done],
    since: DateTime<Utc>,
) -> Option<&'a crate::account::Done> {
    use crate::account::RefKind;
    let want = normalised(done_when);
    let has = |d: &crate::account::Done, k: RefKind| d.refs.iter().any(|r| r.kind == k);
    done.iter().filter(|d| d.at >= since).find(|d| {
        let words: Vec<&str> = d
            .text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        let words: Vec<String> = words.iter().map(|w| w.to_lowercase()).collect();
        let says = |phrases: &[&str]| phrases.iter().any(|p| said(&words, p));
        match want.as_str() {
            "merged" | "merge" | "the pr merged" | "pr merged" => {
                // A merge of it, not of main into it or of a conflict.
                has(d, RefKind::Pr)
                    && (0..words.len()).any(|i| {
                        matches!(words[i].as_str(), "merged" | "merge")
                            && !negated(&words, i)
                            && !matches!(
                                words.get(i + 1).map(String::as_str),
                                Some(
                                    "main"
                                        | "master"
                                        | "origin"
                                        | "upstream"
                                        | "conflict"
                                        | "conflicts"
                                )
                            )
                    })
            }
            "in production" | "live" | "in prod" => {
                (has(d, RefKind::Deploy) && !negated_anywhere(&words) && !says(&["staging"]))
                    || says(&[
                        "in production",
                        "to production",
                        "to prod",
                        "in prod",
                        "went live",
                        "is live",
                    ])
            }
            "deployed" | "deploy" | "released" | "shipped" => {
                (has(d, RefKind::Deploy) && !negated_anywhere(&words))
                    || says(&["deployed", "released", "shipped"])
            }
            "pr open" | "pr opened" | "the pr open" | "a pr open" => {
                has(d, RefKind::Pr) && says(&["opened", "open", "raised", "created"])
            }
            "" => false,
            w => said(&words, w),
        }
    })
}

/// Whether `words` say `phrase` (normalised words) as done: whole words,
/// and not just after a "not" or "couldn't".
fn said(words: &[String], phrase: &str) -> bool {
    let want: Vec<&str> = phrase.split(' ').filter(|w| !w.is_empty()).collect();
    if want.is_empty() || want.len() > words.len() {
        return false;
    }
    (0..=words.len() - want.len()).any(|at| {
        words[at..at + want.len()]
            .iter()
            .zip(&want)
            .all(|(a, b)| a == b)
            && !negated(words, at)
    })
}

/// Whether the words just before `at` turn it down.
fn negated(words: &[String], at: usize) -> bool {
    words[at.saturating_sub(2)..at].iter().any(|w| negation(w))
}

/// Whether `w` turns what follows it down: "not", "never", "couldn't"
/// (split as "couldn" "t").
fn negation(w: &str) -> bool {
    matches!(w, "not" | "never" | "cannot" | "t" | "failed" | "unable")
}

/// Whether `words` say no anywhere, so a deploy ref isn't enough.
fn negated_anywhere(words: &[String]) -> bool {
    words.iter().any(|w| negation(w))
}

/// How long a promise or order is kept with nothing changing.
pub const LAPSE_AFTER: Duration = Duration::days(3);

/// Why a promise or order of `i`'s lapses at `now`, if it does: every
/// session it is linked to has ended (`ended` says of one), or nothing
/// changed for `after`.
pub fn lapsed(
    i: &OpenItem,
    now: DateTime<Utc>,
    after: Duration,
    ended: impl Fn(Uuid) -> bool,
) -> Option<String> {
    if !matches!(i.kind, OpenKind::Promise | OpenKind::Order) {
        return None;
    }
    let linked = i.linked();
    if !linked.is_empty() && linked.iter().all(|s| ended(*s)) {
        return Some(match linked.len() {
            1 => "its session ended".into(),
            _ => "its sessions ended".into(),
        });
    }
    (now - i.changed_at >= after).then(|| "nothing changed for 3 days".into())
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

    /// The sessions it is about or linked to, each once.
    pub fn linked(&self) -> Vec<Uuid> {
        let mut out: Vec<Uuid> = Vec::new();
        for s in self.session.iter().chain(self.sessions.iter()) {
            if !out.contains(s) {
                out.push(*s);
            }
        }
        out
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

    /// "Still waiting on you: {its one line} · {age} · {holding up}", short:
    /// the rest is behind "more", and any recommendation in its buttons.
    pub fn still_waiting(&self, now: DateTime<Utc>) -> String {
        let mut s = format!("Still waiting on you: {} · {}", self.headline(), self.age(now));
        if let Some(h) = self.holding_up.as_deref().filter(|h| !h.trim().is_empty()) {
            s.push_str(&format!(" · {}", crate::crew::words(h, POINT_WORDS)));
        }
        s
    }

    /// Its one line, as the person reads it: the question given, else
    /// its text's first sentence, at most [`QUESTION_WORDS`].
    pub fn headline(&self) -> String {
        let q = self
            .layout
            .question
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .unwrap_or(&self.text);
        let first = crate::crew::one_line(q, usize::MAX);
        crate::crew::clip(&crate::crew::words(&first, QUESTION_WORDS), QUESTION_CHARS)
    }

    /// What it says beyond its one line, behind "more": its full text
    /// when longer, and what it holds up. None when there is nothing more.
    pub fn detail(&self) -> Option<String> {
        let head = self.headline();
        let mut parts: Vec<String> = Vec::new();
        let text = self.text.trim();
        if crate::open_item::normalised(text) != crate::open_item::normalised(&head) {
            parts.push(text.to_string());
        }
        if let Some(h) = self.holding_up.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
            parts.push(format!("Holding up: {h}"));
        }
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    /// Its points as the person is shown them: at most
    /// [`SHOWN_POINTS`], each at most [`POINT_WORDS`].
    pub fn shown_points(&self) -> Vec<String> {
        self.layout
            .points
            .iter()
            .map(|p| crate::crew::words(p, POINT_WORDS))
            .filter(|p| !p.is_empty())
            .take(SHOWN_POINTS)
            .collect()
    }

    /// What it offers to answer with, at `full` (Act fully) or not: at Act
    /// fully the boss recommends nothing, and an ask carries answers only
    /// when it said why it couldn't decide.
    pub fn offer(&self, full: bool) -> Offer {
        if self.kind == OpenKind::Todo {
            return Offer::default();
        }
        match full {
            false => Offer {
                recommendation: self.recommendation.clone(),
                options: self.options.clone(),
            },
            true => Offer {
                recommendation: None,
                options: match self.cant_decide.as_deref().map(str::trim) {
                    Some(w) if !w.is_empty() => self.options.clone(),
                    _ => Vec::new(),
                },
            },
        }
    }

    /// As the chat's extras carry it.
    pub fn mention(&self) -> MentionedItem {
        self.mention_at(false)
    }

    /// As the chat's extras carry it at `full` (Act fully) or not: what it
    /// offers to answer with as [`Self::offer`] says.
    pub fn mention_at(&self, full: bool) -> MentionedItem {
        let offer = self.offer(full);
        MentionedItem {
            item_id: self.id.to_string(),
            recommendation: offer.recommendation,
            options: offer.options,
            detail: self.detail(),
            for_you: self.kind == OpenKind::Todo,
            generalizes: self.generalizes.clone(),
            prefer_scope: self
                .generalizes
                .as_ref()
                .map(|_| self.prefer_scope.clone().unwrap_or_else(|| "all".into())),
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
    /// What it says beyond its one line, behind "more".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Only the person can do it: Done, never answers.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub for_you: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// The kind of case an "always" to it keeps a preference for: its
    /// `when`; the person's answer (or the recommendation) is its `then`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generalizes: Option<String>,
    /// What that preference would cover: `all`, `group:<id>` or
    /// `repo:<owner/name>`; set whenever `generalizes` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefer_scope: Option<String>,
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
        OpenKind::Todo,
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
                OpenKind::Todo => "What only the person can do",
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
            sessions: vec![],
            prefer_scope: None,
            changed_at: now,
            state: OpenState::Open,
            closed_at: None,
            closed_by: None,
            closed_why: None,
            layout: AskLayout::default(),
            cant_decide: None,
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
    fn a_promise_is_kept_by_what_its_session_did_and_lapses_when_it_cant_be() {
        use crate::account::{Done, Ref, RefKind};
        let now = Utc::now();
        let done = |text: &str, kind: Option<RefKind>, mins: i64| Done {
            text: text.into(),
            refs: kind
                .map(|k| Ref {
                    kind: k,
                    id: "#622".into(),
                    url: None,
                })
                .into_iter()
                .collect(),
            at: now - Duration::minutes(mins),
        };
        let list = vec![
            done("Opened PR #622 for the lead picker", Some(RefKind::Pr), 30),
            done("Merged #622 after CI went green", Some(RefKind::Pr), 5),
        ];
        let since = now - Duration::minutes(20);
        assert_eq!(kept_by("merged", &list, since).unwrap().text, list[1].text);
        assert!(kept_by("in production", &list, since).is_none());
        assert!(
            kept_by("PR open", &list, since).is_none(),
            "the PR was opened before the promise"
        );
        assert!(kept_by("merged", &list, now).is_none());
        let deployed = vec![done("Promoted to prod", Some(RefKind::Deploy), 1)];
        assert!(kept_by("in production", &deployed, since).is_some());
        // What only reads like it keeps the promise doesn't.
        for (text, kind, want) in [
            ("Merged main into the branch", Some(RefKind::Pr), "merged"),
            ("PR #622 is not merged yet", Some(RefKind::Pr), "merged"),
            (
                "Resolved a merge conflict in #622",
                Some(RefKind::Pr),
                "merged",
            ),
            ("Merged the two config files", None, "merged"),
            ("Wrote a reproduction of the bug", None, "in production"),
            (
                "Couldn't deploy: the pipeline is red",
                Some(RefKind::Deploy),
                "deployed",
            ),
            ("Ran the query against production", None, "in production"),
            ("Reviewed the pricing page", Some(RefKind::Pr), "PR open"),
            ("The tests didn't pass", None, "tests pass"),
        ] {
            let list = vec![done(text, kind, 1)];
            assert!(kept_by(want, &list, since).is_none(), "{text} keeps {want}");
        }
        for (text, kind, want) in [
            (
                "Resolved merge conflicts and merged #622",
                Some(RefKind::Pr),
                "merged",
            ),
            ("No conflicts, merged #622", Some(RefKind::Pr), "merged"),
            ("Deployed to production", None, "in production"),
        ] {
            let list = vec![done(text, kind, 1)];
            assert!(kept_by(want, &list, since).is_some(), "{text} keeps {want}");
        }
        let list = vec![done("All the tests pass now", None, 1)];
        assert!(kept_by("tests pass", &list, since).is_some());
        let list = vec![done("Deployed #622 to staging", None, 1)];
        assert!(kept_by("deployed", &list, since).is_some());
        assert!(kept_by("in production", &list, since).is_none());

        let mut p = item(10, 1, None);
        p.kind = OpenKind::Promise;
        p.session = Some(Uuid::new_v4());
        p.sessions = vec![p.session.unwrap()];
        assert_eq!(
            lapsed(&p, now, LAPSE_AFTER, |_| true).as_deref(),
            Some("its session ended")
        );
        assert_eq!(lapsed(&p, now, LAPSE_AFTER, |_| false), None);
        p.sessions.push(Uuid::new_v4());
        let one = p.session.unwrap();
        assert_eq!(
            lapsed(&p, now, LAPSE_AFTER, |s| s == one),
            None,
            "one still runs"
        );
        p.changed_at = now - Duration::days(4);
        assert_eq!(
            lapsed(&p, now, LAPSE_AFTER, |_| false).as_deref(),
            Some("nothing changed for 3 days")
        );
        let ask = item(10, 1, None);
        assert_eq!(
            lapsed(&ask, now, Duration::zero(), |_| true),
            None,
            "an ask never lapses"
        );
        assert!(NewItem::order("desk", "Ship it", vec![], Some("m1")).dedupe_key == "order:m1");
        assert_eq!(
            NewItem::promise("desk", one, "x", " Merged ").dedupe_key,
            format!("promise:{one}:merged")
        );
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
