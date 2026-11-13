//! A boss's updates (overseer H7): what changed on its crew since it last
//! told the person, gathered for a few minutes after the first result and
//! sent by a ceiling at the latest, as one message in its DM.
//!
//! The pure parts live here: what changed in a session's account since it
//! was last told ([`changed`]), when a gathered update is due ([`due`],
//! with the quiet-evening backoff of [`interval`]), the plain update
//! written from the rows when no model writes it ([`plain_session`],
//! [`render`]), and the shape the chat's extras carry ([`MentionedUpdate`]).
//! Decisions and open items are rendered verbatim, never paraphrased.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::account::{Account, Done, Ref, RefKind};
use crate::decision::{BossDecision, DecisionLine};
use crate::open_item::OpenItem;

/// What a session's account said when the person was last told of it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Told {
    /// The newest done line they were told of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub next: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_on: Option<String>,
}

impl Told {
    /// What `a` says now, as told.
    pub fn of(a: &Account) -> Self {
        Self {
            done_at: a.done.iter().map(|d| d.at).max(),
            next: a.next.trim().to_string(),
            waiting_on: a
                .waiting_on
                .as_deref()
                .map(str::trim)
                .filter(|w| !w.is_empty())
                .map(str::to_string),
        }
    }
}

/// How much a change is news.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
    None,
    /// It goes out with the next update, or by the ceiling.
    Small,
    /// A visible result (a PR, merged, deployed, failed): it starts the
    /// gather at once.
    Result,
}

/// The words a done line says a result with.
const RESULT_WORDS: &[&str] = &[
    "merged", "deployed", "released", "shipped", "failed", "opened", "finished", "fixed",
];

/// Whether `d` is a visible result: it names a PR or a deploy, or says one.
pub fn is_result(d: &Done) -> bool {
    d.refs
        .iter()
        .any(|r| matches!(r.kind, RefKind::Pr | RefKind::Deploy))
        || d.text
            .split(|c: char| !c.is_alphanumeric())
            .any(|w| RESULT_WORDS.contains(&w.to_lowercase().as_str()))
}

/// What changed in `a` since `told` (none: never told, so only what was
/// done since `since` is news). What it is doing this minute alone is not
/// news: it moves on every refresh.
pub fn changed(told: Option<&Told>, a: &Account, since: DateTime<Utc>) -> Change {
    let after = told.and_then(|t| t.done_at).unwrap_or(since).max(since);
    let new: Vec<&Done> = a.done.iter().filter(|d| d.at > after).collect();
    if new.iter().any(|d| is_result(d)) {
        return Change::Result;
    }
    if !new.is_empty() {
        return Change::Small;
    }
    let now = Told::of(a);
    match told {
        Some(t) if t.next != now.next || t.waiting_on != now.waiting_on => Change::Small,
        Some(_) => Change::None,
        // A session first seen with nothing done is told of once it does
        // something.
        None => Change::None,
    }
}

/// The done lines of `a` since `told` (or `since`), oldest first.
pub fn done_since<'a>(told: Option<&Told>, a: &'a Account, since: DateTime<Utc>) -> Vec<&'a Done> {
    let after = told.and_then(|t| t.done_at).unwrap_or(since).max(since);
    let mut new: Vec<&Done> = a.done.iter().filter(|d| d.at > after).collect();
    new.sort_by_key(|d| d.at);
    new
}

/// The least time between two updates, after `unread` of them in a row
/// the person hasn't read: `every`, doubling from the third unread one
/// (3 → 6 → 12 minutes), at most three times `max_wait`.
pub fn interval(every: Duration, max_wait: Duration, unread: u32) -> Duration {
    let cap = (max_wait * 3).max(every);
    if unread < 3 {
        return every;
    }
    let doublings = (unread - 2).min(16);
    (every * 2i32.pow(doublings)).min(cap)
}

/// How many updates in a row are unread, of `kept` once the person read
/// their DM through `read` and the last update went out at `last`.
pub fn streak(kept: u32, last: Option<DateTime<Utc>>, read: Option<DateTime<Utc>>) -> u32 {
    match (last, read) {
        (Some(last), Some(read)) if read >= last => 0,
        (None, _) => 0,
        _ => kept,
    }
}

/// Whether an update gathered since `gather_since` (a result in it:
/// `result`) is due at `now`: a result once it has gathered `every`, a
/// small change by `max_wait`, and never sooner than `gap` after the last
/// one (`last`).
pub fn due(
    gather_since: Option<DateTime<Utc>>,
    result: bool,
    last: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    (every, max_wait, gap): (Duration, Duration, Duration),
) -> bool {
    let Some(since) = gather_since else {
        return false;
    };
    if last.is_some_and(|l| now - l < gap) {
        return false;
    }
    let gathered = now - since;
    (result && gathered >= every) || gathered >= max_wait
}

/// A boss's update state, as kept in Postgres (`boss_update_state`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateState {
    pub boss: String,
    pub since: DateTime<Utc>,
    pub gather_since: Option<DateTime<Utc>>,
    pub result: bool,
    pub last_update_at: Option<DateTime<Utc>>,
    pub unread: u32,
    pub told: HashMap<Uuid, Told>,
    pub pending: Option<Pending>,
    /// Since when the person is away (overseer H8): updates gather into
    /// one catch-up for their return.
    pub away_since: Option<DateTime<Utc>>,
}

/// An update written and about to go out: kept first, so a restart posts
/// it once (found by its id in the DM when it already went).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub id: Uuid,
    pub text: String,
    pub update: MentionedUpdate,
    /// What each session's account said, once this is told.
    pub told: HashMap<Uuid, Told>,
    /// The open items it restates, each with whether it parks after.
    #[serde(default)]
    pub restated: Vec<(Uuid, bool)>,
    /// What it was written from, as of: decisions and kept items after it
    /// are the next update's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cutoff: Option<DateTime<Utc>>,
    /// Handed to a check-in (Watch, Suggest), whose message is the post.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub handed: bool,
    /// The DM message it went out as, once it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent: Option<String>,
    /// A catch-up for the person's return, not an update: the absence it
    /// covers, and what it says beyond an update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub away: Option<crate::away::MentionedAway>,
}

/// The lines of `p` that must reach the person word for word (its
/// decisions and what it kept) that `said` leaves out. What still waits
/// may be said in the boss's words: each ask has its own card.
pub fn missing_lines(p: &Pending, decided: &[String], said: &str) -> Vec<String> {
    let flat = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
    let said = flat(said);
    decided
        .iter()
        .chain(&p.update.kept)
        .filter(|l| !said.contains(&flat(l)))
        .cloned()
        .collect()
}

/// A session as an update tells of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSession {
    pub session_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub purpose: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<Ref>,
}

/// What still waits on the person, as an update lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitingItem {
    pub item_id: String,
    pub text: String,
    /// How long it has waited, in words ("40 min").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub age: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holding_up: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}

impl WaitingItem {
    /// `i` as an update lists it at `now`.
    pub fn of(i: &OpenItem, now: DateTime<Utc>) -> Self {
        Self {
            item_id: i.id.to_string(),
            text: i.text.clone(),
            age: age(i.opened_at, now),
            holding_up: i.holding_up.clone(),
            recommendation: i.recommendation.clone(),
            options: i.options.clone(),
        }
    }
}

/// An answer the person can give, as a button: the recommended one filled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnswerOption {
    pub label: String,
    #[serde(default)]
    pub recommended: bool,
}

/// The most characters a needs-you question has.
pub const QUESTION_CHARS: usize = 160;

/// What waits on the person, laid out (`extras.update.needsYou`): one
/// question, its points, the boss's recommendation, the answers with the
/// recommended one marked, and what carries on meanwhile. Beside `waiting`,
/// which says the same item as before.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NeedsYou {
    pub item_id: String,
    /// One line.
    pub question: String,
    #[serde(default)]
    pub items: Vec<String>,
    #[serde(default)]
    pub recommendation: Option<String>,
    #[serde(default)]
    pub options: Vec<AnswerOption>,
    #[serde(default)]
    pub not_blocking: Option<String>,
    /// How long it has waited, in words ("1 h").
    #[serde(default)]
    pub age: String,
    /// The kind of case an answer would settle from then on, as
    /// `extras.item` says it: the chat offers "Always" with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generalizes: Option<String>,
}

impl NeedsYou {
    /// `i` laid out at `now`: what the boss gave, else its text's first
    /// sentence as the question, with no points, no answer recommended and
    /// nothing said to carry on.
    pub fn of(i: &OpenItem, now: DateTime<Utc>) -> Self {
        let l = &i.layout;
        let given = |t: &Option<String>| {
            t.as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
        };
        let question = match given(&l.question) {
            Some(q) => crate::crew::one_line(&q, QUESTION_CHARS),
            None => crate::crew::one_line(&i.text, QUESTION_CHARS),
        };
        let recommended = given(&l.recommended)
            .and_then(|r| crate::open_item::matching_option(&i.options, &r).map(str::to_string));
        Self {
            item_id: i.id.to_string(),
            question,
            items: l
                .points
                .iter()
                .map(|p| p.trim())
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect(),
            recommendation: given(&i.recommendation),
            options: i
                .options
                .iter()
                .map(|o| AnswerOption {
                    label: o.clone(),
                    recommended: recommended.as_deref() == Some(o.as_str()),
                })
                .collect(),
            not_blocking: given(&l.not_blocking),
            age: age(i.opened_at, now),
            generalizes: given(&i.generalizes),
        }
    }
}

/// An update, as the chat's extras carry it (`extras.update`): every field
/// optional on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MentionedUpdate {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<UpdateSession>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<DecisionLine>,
    /// What it kept for the person since: their orders done, its promises
    /// kept, said as they were closed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kept: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waiting: Vec<WaitingItem>,
    /// What waits on the person, laid out: the same items as `waiting`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs_you: Vec<NeedsYou>,
    /// The day's cheap calls, near the ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub footer: Option<String>,
}

impl MentionedUpdate {
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
            && self.decisions.is_empty()
            && self.kept.is_empty()
            && self.waiting.is_empty()
    }
}

/// A session's paragraph written plainly from its account: what it did
/// since it was last told, where it is and what's next.
pub fn plain_session(new: &[&Done], a: &Account) -> String {
    let mut parts: Vec<String> = Vec::new();
    let done: Vec<&str> = new
        .iter()
        .map(|d| d.text.trim())
        .filter(|t| !t.is_empty())
        .collect();
    if !done.is_empty() {
        parts.push(format!("Done: {}.", done.join("; ").trim_end_matches('.')));
    }
    if !a.now.trim().is_empty() {
        parts.push(format!("Now: {}.", a.now.trim().trim_end_matches('.')));
    }
    if !a.next.trim().is_empty() {
        parts.push(format!("Next: {}.", a.next.trim().trim_end_matches('.')));
    }
    if let Some(w) = a
        .waiting_on
        .as_deref()
        .map(str::trim)
        .filter(|w| !w.is_empty())
    {
        parts.push(format!("Waiting on {}.", w.trim_end_matches('.')));
    }
    parts.join(" ")
}

/// The refs of `new`, once each.
pub fn refs_of(new: &[&Done]) -> Vec<Ref> {
    let mut out: Vec<Ref> = Vec::new();
    for r in new.iter().flat_map(|d| &d.refs) {
        if !out.iter().any(|o| o.kind == r.kind && o.id == r.id) {
            out.push(r.clone());
        }
    }
    out
}

/// An item's age at `now`, in words.
pub fn age(since: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let m = (now - since).num_minutes().max(0);
    match m {
        0 => "just now".into(),
        1..=59 => format!("{m} min"),
        _ if m < 48 * 60 => format!("{} h", m / 60),
        _ => format!("{} days", m / (24 * 60)),
    }
}

/// The update's text, as history, Slack and the model read it: each
/// session's paragraph (`**name** (purpose) — text`), then the decisions'
/// lines, what it kept, and what still waits, each verbatim.
pub fn render(u: &MentionedUpdate, decided: &[String]) -> String {
    let mut out: Vec<String> = Vec::new();
    for s in &u.sessions {
        let purpose = match s.purpose.trim() {
            "" => String::new(),
            p => format!(" ({})", p.trim_end_matches('.')),
        };
        out.push(format!("**{}**{purpose} — {}", s.name, s.text.trim()));
    }
    out.extend(decided.iter().cloned());
    out.extend(u.kept.iter().cloned());
    if !u.waiting.is_empty() {
        let lines: Vec<String> = u
            .waiting
            .iter()
            .map(|w| match w.age.as_str() {
                "" => format!("- {}", w.text),
                age => format!("- {} ({age})", w.text),
            })
            .collect();
        out.push(format!("Still waiting on you:\n{}", lines.join("\n")));
    }
    if let Some(f) = &u.footer {
        out.push(format!("_{f}_"));
    }
    out.join("\n\n")
}

/// The footer near the day's ceiling (80% of it spent), or none.
pub fn footer(spent: i64, cap: u32) -> Option<String> {
    (cap > 0 && spent * 5 >= i64::from(cap) * 4)
        .then(|| format!("Notes today: {spent} of {cap} cheap calls."))
}

/// The composer's instructions: per session, in the boss's voice, where it
/// is, what's next and roughly when; per decision, what it did in a short
/// line and why in one sentence, never quoting the message it sent (shown
/// apart). Asks and what it kept are added verbatim.
pub const COMPOSER_SYSTEM: &str = "You write parts of an overseer's short update to the person it works for, in its voice (\"I\", plain words, no lists, no headings).\n\
For each session given, write one or two sentences: where it is, what's next and roughly when, from what it did since the person was last told. Don't mention decisions, questions or anything waiting on the person in these: those are added separately.\n\
For each decision given, write its action: what you did, in at most 70 characters, as one short sentence (\"Lifted my hold on tonight's release.\", \"Let web wait for CI.\"); and its why: one sentence, the reason. Never quote or repeat the message you sent: the person reads it separately.\n\
Leave out anything not given; never invent PR numbers, names or times. Answer with JSON only: {\"sessions\": [{\"id\": \"<session id>\", \"text\": \"<paragraph>\"}], \"decisions\": [{\"id\": \"<decision id>\", \"action\": \"<short line>\", \"why\": \"<one sentence>\"}]}.";

/// What the composer is given of each session: its id, name, purpose,
/// what it did since it was last told, now, next and what it waits on.
pub fn composer_input(sessions: &[(Uuid, String, &Account, Vec<&Done>)]) -> String {
    let mut out = String::new();
    for (id, name, a, new) in sessions {
        out.push_str(&format!("Session {id} “{name}”"));
        if !a.purpose.trim().is_empty() {
            out.push_str(&format!(" — for: {}", a.purpose.trim()));
        }
        out.push('\n');
        for d in new {
            out.push_str(&format!("  did: {}\n", d.text.trim()));
        }
        if !a.now.trim().is_empty() {
            out.push_str(&format!("  now: {}\n", a.now.trim()));
        }
        if !a.next.trim().is_empty() {
            out.push_str(&format!("  next: {}\n", a.next.trim()));
        }
        if let Some(w) = a.waiting_on.as_deref().filter(|w| !w.trim().is_empty()) {
            out.push_str(&format!("  waiting on: {}\n", w.trim()));
        }
    }
    out
}

/// The most decisions the composer is given: the newest it acted on.
pub const COMPOSED_DECISIONS: usize = 30;
/// The most of a decision's message the composer is given, in characters.
pub const COMPOSED_MESSAGE_CHARS: usize = 300;

/// What the composer is given of each decision: its id, kind, what was
/// decided and why, and the message it sent (to say what it did, never to
/// quote).
pub fn composer_decisions(decisions: &[BossDecision]) -> String {
    let mut out = String::new();
    for d in decisions {
        out.push_str(&format!("Decision {} ({})\n", d.id, d.kind.as_str()));
        out.push_str(&format!("  decided: {}\n", d.what.trim()));
        if !d.why.trim().is_empty() {
            out.push_str(&format!("  why: {}\n", d.why.trim()));
        }
        if let Some(m) = &d.message {
            out.push_str(&format!(
                "  message sent (don't quote it): {}\n",
                crate::crew::clip(m, COMPOSED_MESSAGE_CHARS)
            ));
        }
    }
    out
}

/// What the composer wrote, by session and by decision.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Composed {
    pub sessions: HashMap<Uuid, String>,
    /// Each decision's short line and why, as written.
    pub decisions: HashMap<Uuid, (String, String)>,
}

/// The composer's paragraphs by session and lines by decision, from its
/// answer; none when it isn't the JSON asked for. What it didn't write is
/// written plainly by the caller.
pub fn parse_composed(answer: &str) -> Option<Composed> {
    let start = answer.find('{')?;
    let end = answer.rfind('}')?;
    let v: serde_json::Value = serde_json::from_str(answer.get(start..=end)?).ok()?;
    let id_of =
        |e: &serde_json::Value| e["id"].as_str().and_then(|i| i.trim().parse::<Uuid>().ok());
    let text_of = |e: &serde_json::Value, k: &str| {
        e[k].as_str()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    };
    let mut out = Composed::default();
    let sessions = v["sessions"].as_array();
    let decisions = v["decisions"].as_array();
    if sessions.is_none() && decisions.is_none() {
        return None;
    }
    for s in sessions.into_iter().flatten() {
        if let (Some(id), Some(text)) = (id_of(s), text_of(s, "text")) {
            out.sessions.insert(id, crate::crew::one_line(&text, 800));
        }
    }
    for d in decisions.into_iter().flatten() {
        if let (Some(id), Some(action), why) = (id_of(d), text_of(d, "action"), text_of(d, "why")) {
            out.decisions.insert(id, (action, why.unwrap_or_default()));
        }
    }
    Some(out)
}

/// Each of `decisions` as the update carries it: the composer's short line
/// and why where it wrote ones that fit (at most
/// [`crate::decision::BRIEF_CHARS`], one sentence, never quoting the
/// message), else the row's own.
pub fn decision_lines(
    decisions: &[BossDecision],
    composed: &HashMap<Uuid, (String, String)>,
) -> Vec<DecisionLine> {
    use crate::decision::{brief_line, why_line, BRIEF_CHARS};
    decisions
        .iter()
        .map(|d| {
            let mut line = d.line();
            if let Some((action, why)) = composed.get(&d.id) {
                let message = d.message.as_deref();
                let fits = action.chars().count() <= BRIEF_CHARS && !action.contains('\n');
                if let (true, Some(action)) = (fits, brief_line(action, message)) {
                    line.action = action;
                }
                let why = why_line(why, message);
                if !why.is_empty() {
                    line.why = why;
                }
            }
            line
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(done: &[(&str, Option<RefKind>, i64)], next: &str) -> Account {
        let now = Utc::now();
        let mut a = Account::new(Uuid::new_v4(), "person-1", now);
        a.purpose = "The lead picker".into();
        a.next = next.into();
        a.done = done
            .iter()
            .map(|(t, k, mins)| Done {
                text: (*t).into(),
                refs: k
                    .map(|k| Ref {
                        kind: k,
                        id: "#622".into(),
                        url: None,
                    })
                    .into_iter()
                    .collect(),
                at: now - Duration::minutes(*mins),
            })
            .collect();
        a
    }

    #[test]
    fn only_what_was_done_or_what_comes_next_is_news() {
        let since = Utc::now() - Duration::hours(1);
        let mut a = account(&[("Wrote the endpoint", None, 30)], "Tests");
        let told = Told::of(&a);
        assert_eq!(changed(Some(&told), &a, since), Change::None);
        // What it is doing this minute moves on every refresh: not news.
        a.now = "Running the tests".into();
        assert_eq!(changed(Some(&told), &a, since), Change::None);
        a.next = "Open the PR".into();
        assert_eq!(changed(Some(&told), &a, since), Change::Small);
        a.next = "Tests".into();
        a.done.push(Done {
            text: "Added a test".into(),
            refs: vec![],
            at: Utc::now(),
        });
        assert_eq!(changed(Some(&told), &a, since), Change::Small);
        a.done.push(Done {
            text: "Opened PR #622".into(),
            refs: vec![Ref {
                kind: RefKind::Pr,
                id: "#622".into(),
                url: None,
            }],
            at: Utc::now(),
        });
        assert_eq!(changed(Some(&told), &a, since), Change::Result);
        assert_eq!(done_since(Some(&told), &a, since).len(), 2);
        // Never told: only what it did since the boss started telling.
        let old = account(&[("Merged #600", Some(RefKind::Pr), 120)], "");
        assert_eq!(changed(None, &old, since), Change::None);
        let fresh = account(&[("Merged #622", Some(RefKind::Pr), 5)], "");
        assert_eq!(changed(None, &fresh, since), Change::Result);
    }

    #[test]
    fn an_update_waits_for_a_result_or_the_ceiling_and_backs_off_unread() {
        let m = Duration::minutes(1);
        let (every, max_wait) = (m * 3, m * 10);
        let now = Utc::now();
        let w = (every, max_wait, every);
        assert!(!due(None, true, None, now, w), "nothing gathered");
        assert!(!due(Some(now - m * 2), true, None, now, w));
        assert!(
            due(Some(now - m * 3), true, None, now, w),
            "a result within 3 minutes"
        );
        assert!(!due(Some(now - m * 9), false, None, now, w));
        assert!(
            due(Some(now - m * 10), false, None, now, w),
            "a small change by 10"
        );
        assert!(
            !due(Some(now - m * 10), true, Some(now - m), now, w),
            "never two within the interval"
        );
        assert_eq!(interval(every, max_wait, 0), every);
        assert_eq!(interval(every, max_wait, 2), every);
        assert_eq!(interval(every, max_wait, 3), m * 6);
        assert_eq!(interval(every, max_wait, 4), m * 12);
        assert_eq!(interval(every, max_wait, 9), m * 30, "at most max_wait × 3");
        let last = now - m;
        assert_eq!(streak(3, Some(last), Some(now)), 0, "read since");
        assert_eq!(streak(3, Some(last), Some(last - m)), 3);
        assert_eq!(streak(3, None, None), 0);
    }

    #[test]
    fn the_plain_update_is_written_from_the_rows() {
        let a = account(
            &[("Opened PR #622", Some(RefKind::Pr), 5)],
            "Merge once CI is green",
        );
        let since = Utc::now() - Duration::hours(1);
        let new = done_since(None, &a, since);
        let text = plain_session(&new, &a);
        assert_eq!(text, "Done: Opened PR #622. Next: Merge once CI is green.");
        let u = MentionedUpdate {
            sessions: vec![UpdateSession {
                session_id: a.session.to_string(),
                name: "acme-web".into(),
                purpose: a.purpose.clone(),
                text,
                refs: refs_of(&new),
            }],
            waiting: vec![WaitingItem {
                item_id: "i".into(),
                text: "Which key should staging use?".into(),
                age: "40 min".into(),
                holding_up: None,
                recommendation: None,
                options: vec![],
            }],
            footer: footer(410, 500),
            ..Default::default()
        };
        let decided =
            vec!["I decided: told acme-api to wait for green CI — it was red.".to_string()];
        let out = render(&u, &decided);
        assert!(
            out.starts_with("**acme-web** (The lead picker) — Done: Opened PR #622."),
            "{out}"
        );
        assert!(out.contains("\n\nI decided: told acme-api to wait for green CI — it was red.\n\n"));
        assert!(out.contains("Still waiting on you:\n- Which key should staging use? (40 min)"));
        assert!(out.ends_with("_Notes today: 410 of 500 cheap calls._"));
        assert_eq!(footer(399, 500), None);
        assert_eq!(u.sessions[0].refs.len(), 1);
    }

    #[test]
    fn a_check_ins_reply_that_drops_a_row_is_caught() {
        let p = Pending {
            id: Uuid::new_v4(),
            text: String::new(),
            update: MentionedUpdate {
                kept: vec!["What I promised at 10:00 (“Tell you”) is merged: #622".into()],
                waiting: vec![WaitingItem {
                    item_id: "i".into(),
                    text: "Which key should staging use?".into(),
                    age: String::new(),
                    holding_up: None,
                    recommendation: None,
                    options: vec![],
                }],
                ..Default::default()
            },
            told: HashMap::new(),
            restated: vec![],
            cutoff: None,
            handed: true,
            sent: None,
            away: None,
        };
        let decided = vec!["I decided: told acme-api to wait — CI is red.".to_string()];
        let reply = "Here is where things are.\n\nI decided: told acme-api to wait —   CI is red.\n\nStill waiting on you:\n- Which key should staging use?";
        assert_eq!(
            missing_lines(&p, &decided, reply),
            vec!["What I promised at 10:00 (“Tell you”) is merged: #622".to_string()]
        );
        // An ask said in its own words is no line missing.
        let reworded = "I decided: told acme-api to wait — CI is red.\nWhat I promised at 10:00 (“Tell you”) is merged: #622\nI still need the staging key from you.";
        assert!(missing_lines(&p, &decided, reworded).is_empty());
    }

    #[test]
    fn the_composers_answer_is_read_by_session_and_nothing_else() {
        let id = Uuid::new_v4();
        let got = parse_composed(&format!(
            "Here: {{\"sessions\": [{{\"id\": \"{id}\", \"text\": \"PR #622 is up; I'll merge it once CI is green.\"}}, {{\"id\": \"nope\", \"text\": \"x\"}}]}}"
        ))
        .unwrap();
        assert_eq!(got.sessions.len(), 1);
        assert!(got.sessions[&id].starts_with("PR #622 is up"));
        assert!(got.decisions.is_empty());
        assert!(parse_composed("I couldn't.").is_none());
    }

    fn nudge(message: &str) -> BossDecision {
        BossDecision::new(
            "desk",
            crate::decision::Need::Choice,
            &format!("told web “{message}”"),
            "You confirmed it yourself at 15:33. So it can go.",
            crate::decision::How::Own,
        )
        .briefly("Sent web a message", Some(message))
    }

    #[test]
    fn the_composers_decision_lines_are_taken_only_when_they_fit() {
        let message = "You can release tonight: the person confirmed the change themselves.";
        let d = nudge(message);
        let answer = format!(
            "{{\"sessions\": [], \"decisions\": [{{\"id\": \"{}\", \"action\": \"Lifted my hold on tonight's release.\", \"why\": \"You confirmed it yourself at 15:33.\"}}]}}",
            d.id
        );
        let got = parse_composed(&answer).unwrap();
        let lines = decision_lines(std::slice::from_ref(&d), &got.decisions);
        assert_eq!(lines[0].action, "Lifted my hold on tonight's release.");
        assert_eq!(lines[0].why, "You confirmed it yourself at 15:33.");
        assert_eq!(lines[0].message.as_deref(), Some(message));
        // The old text is kept as it was.
        assert_eq!(lines[0].text, d.what);
        let with = |action: &str, why: &str| {
            let c = HashMap::from([(d.id, (action.to_string(), why.to_string()))]);
            decision_lines(std::slice::from_ref(&d), &c).remove(0)
        };
        // Past 70 characters: the plain line.
        let long = with(
            "Lifted my hold on tonight's release after reading every check again carefully",
            "",
        );
        assert_eq!(long.action, "Sent web a message.");
        assert_eq!(long.why, "You confirmed it yourself at 15:33.");
        // Quoting the message: the plain line and why.
        let quoting = with(
            "Told web: you can release tonight, the person confirmed.",
            "Because the person confirmed the change themselves, it can go.",
        );
        assert_eq!(quoting.action, "Sent web a message.");
        assert_eq!(quoting.why, "You confirmed it yourself at 15:33.");
        // Not written: the plain line.
        let plain = decision_lines(std::slice::from_ref(&d), &HashMap::new()).remove(0);
        assert_eq!(plain.action, "Sent web a message.");
        assert!(plain.action.chars().count() <= crate::decision::BRIEF_CHARS);
        assert!(COMPOSER_SYSTEM.contains("at most 70 characters"));
        assert!(composer_decisions(&[d]).contains("message sent (don't quote it)"));
    }

    #[test]
    fn what_needs_the_person_is_laid_out_from_what_the_boss_gave() {
        let now = Utc::now();
        let mut new = crate::open_item::NewItem::ask(
            "desk",
            crate::open_item::OpenKind::Ask,
            None,
            "Two staging records need a decision before PR 3 can go. One has no organisation; one has roles but no user.",
        );
        new.recommendation = Some("Deactivate the roles; leave the user empty for now.".into());
        new.options = vec![
            "Deactivate · leave it empty".into(),
            "Hold, I'll look".into(),
        ];
        let mut i = OpenItem {
            id: Uuid::new_v4(),
            boss: new.boss.clone(),
            owner: "you".into(),
            kind: new.kind,
            session: None,
            line_message: None,
            text: new.text.clone(),
            holding_up: None,
            recommendation: new.recommendation.clone(),
            options: new.options.clone(),
            generalizes: None,
            done_when: None,
            dedupe_key: new.dedupe_key.clone(),
            opened_at: now - Duration::minutes(61),
            last_said_at: None,
            said_count: 1,
            raised_at: None,
            sessions: vec![],
            changed_at: now,
            state: crate::open_item::OpenState::Open,
            closed_at: None,
            closed_by: None,
            closed_why: None,
            layout: Default::default(),
        };
        // Plainly: its first sentence, no points, nothing recommended.
        let plain = NeedsYou::of(&i, now);
        assert_eq!(
            plain.question,
            "Two staging records need a decision before PR 3 can go."
        );
        assert!(plain.items.is_empty() && plain.not_blocking.is_none());
        assert!(plain.generalizes.is_none());
        i.generalizes = Some("When a staging record has no owner".into());
        assert!(plain.options.iter().all(|o| !o.recommended));
        assert_eq!(plain.age, "1 h");
        i.layout = crate::open_item::AskLayout {
            question: Some("Two records need a decision.".into()),
            points: vec!["A user with no organisation.".into(), " ".into()],
            recommended: Some("deactivate · leave it empty".into()),
            not_blocking: Some("The docs PR doesn't depend on it.".into()),
        };
        let laid = NeedsYou::of(&i, now);
        assert_eq!(laid.question, "Two records need a decision.");
        assert_eq!(laid.items, vec!["A user with no organisation.".to_string()]);
        assert_eq!(
            laid.options
                .iter()
                .map(|o| (o.label.as_str(), o.recommended))
                .collect::<Vec<_>>(),
            vec![
                ("Deactivate · leave it empty", true),
                ("Hold, I'll look", false)
            ]
        );
        let v = serde_json::to_value(&laid).unwrap();
        for k in [
            "itemId",
            "question",
            "items",
            "recommendation",
            "options",
            "notBlocking",
        ] {
            assert!(v.get(k).is_some(), "{k} in {v}");
        }
        assert_eq!(v["options"][0]["recommended"], true);
        assert_eq!(v["generalizes"], "When a staging record has no owner");
        let u = MentionedUpdate {
            needs_you: vec![laid],
            ..Default::default()
        };
        let v = serde_json::to_value(&u).unwrap();
        assert!(v.get("needsYou").is_some(), "{v}");
    }

    #[test]
    fn an_update_kept_before_the_layout_still_reads() {
        // A pending update and its extras, as kept before these fields.
        let p: Pending = serde_json::from_value(serde_json::json!({
            "id": Uuid::new_v4(),
            "text": "x",
            "update": {
                "decisions": [{"id": "d", "text": "told web “go”", "why": "CI is green."}],
                "waiting": [{"itemId": "i", "text": "Which key?", "options": ["A", "B"]}]
            },
            "told": {}
        }))
        .unwrap();
        assert_eq!(p.update.waiting[0].options, vec!["A", "B"]);
        assert!(p.update.needs_you.is_empty());
        assert_eq!(p.update.decisions[0].message, None);
        let e: crate::crew::Extras = serde_json::from_value(serde_json::json!({
            "update": {"decisions": [{"id": "d", "text": "t"}]}
        }))
        .unwrap();
        assert_eq!(e.update.unwrap().decisions[0].action, "");
    }
}
