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
use crate::decision::DecisionLine;

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
/// is, what's next and roughly when; nothing of decisions or asks (those
/// are added verbatim).
pub const COMPOSER_SYSTEM: &str = "You write the session paragraphs of an overseer's short update to the person it works for, in its voice (\"I\", plain words, no lists, no headings). For each session given, write one or two sentences: where it is, what's next and roughly when, from what it did since the person was last told. Leave out anything not given; never invent PR numbers, names or times. Don't mention decisions, questions or anything waiting on the person: those are added separately. Answer with JSON only: {\"sessions\": [{\"id\": \"<session id>\", \"text\": \"<paragraph>\"}]}.";

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

/// The composer's paragraphs by session, from its answer; none when it
/// isn't the JSON asked for. A paragraph it didn't write is written
/// plainly by the caller.
pub fn parse_composed(answer: &str) -> Option<HashMap<Uuid, String>> {
    let start = answer.find('{')?;
    let end = answer.rfind('}')?;
    let v: serde_json::Value = serde_json::from_str(answer.get(start..=end)?).ok()?;
    let mut out = HashMap::new();
    for s in v["sessions"].as_array()? {
        let (Some(id), Some(text)) = (
            s["id"].as_str().and_then(|i| i.trim().parse::<Uuid>().ok()),
            s["text"].as_str().map(str::trim).filter(|t| !t.is_empty()),
        ) else {
            continue;
        };
        out.insert(id, crate::crew::one_line(text, 800));
    }
    Some(out)
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
        assert_eq!(got.len(), 1);
        assert!(got[&id].starts_with("PR #622 is up"));
        assert!(parse_composed("I couldn't.").is_none());
    }
}
