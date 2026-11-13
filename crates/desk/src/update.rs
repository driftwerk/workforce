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
    /// What it says beyond its one line, behind "more".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl WaitingItem {
    /// `i` as an update lists it at `now`, at `full` (Act fully) or not:
    /// its one line, what it holds up in a few words, what it offers as
    /// [`OpenItem::offer`] says, and the rest behind "more".
    pub fn of(i: &OpenItem, now: DateTime<Utc>, full: bool) -> Self {
        let offer = i.offer(full);
        Self {
            item_id: i.id.to_string(),
            text: i.headline(),
            age: age(i.opened_at, now),
            holding_up: i
                .holding_up
                .as_deref()
                .map(|h| crate::crew::words(h, crate::open_item::POINT_WORDS))
                .filter(|h| !h.is_empty()),
            recommendation: offer
                .recommendation
                .map(|r| crate::crew::words(&r, crate::open_item::QUESTION_WORDS)),
            options: offer.options,
            detail: i.detail(),
        }
    }
}

/// How an item was settled in the session, apart from the update that
/// still lists it: the chat folds it to "✓ Settled in the session".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettledBy {
    /// The person answered or acted in the session themselves.
    You,
    /// The session settled it (its account shows it done).
    Session,
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
pub const QUESTION_CHARS: usize = crate::open_item::QUESTION_CHARS;

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
    /// What the person's "always" would cover (`all`, `group:<id>`,
    /// `repo:<owner/name>`), as `extras.item` says it: only with
    /// `generalizes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefer_scope: Option<String>,
    /// The session it is about, if one.
    #[serde(default)]
    pub session_id: Option<String>,
    /// What it says beyond its one line, behind "more".
    #[serde(default)]
    pub detail: Option<String>,
    /// Settled in the session since: folded, never answered here.
    #[serde(default)]
    pub settled_by: Option<SettledBy>,
}

impl NeedsYou {
    /// `i` laid out at `now`, at `full` (Act fully) or not: its one line
    /// ([`OpenItem::headline`]), at most two short points, what it offers
    /// as [`OpenItem::offer`] says (at Act fully no recommendation), what
    /// carries on in a few words, and the rest behind "more".
    pub fn of(i: &OpenItem, now: DateTime<Utc>, full: bool) -> Self {
        let l = &i.layout;
        let given = |t: &Option<String>| {
            t.as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
        };
        let offer = i.offer(full);
        let recommended = match full {
            true => None,
            false => given(&l.recommended).and_then(|r| {
                crate::open_item::matching_option(&offer.options, &r).map(str::to_string)
            }),
        };
        Self {
            item_id: i.id.to_string(),
            question: i.headline(),
            items: i.shown_points(),
            recommendation: offer
                .recommendation
                .map(|r| crate::crew::words(&r, crate::open_item::QUESTION_WORDS)),
            options: offer
                .options
                .iter()
                .map(|o| AnswerOption {
                    label: o.clone(),
                    recommended: recommended.as_deref() == Some(o.as_str()),
                })
                .collect(),
            not_blocking: given(&l.not_blocking)
                .map(|n| crate::crew::words(&n, crate::open_item::POINT_WORDS)),
            age: age(i.opened_at, now),
            generalizes: given(&i.generalizes),
            prefer_scope: given(&i.generalizes)
                .map(|_| i.prefer_scope.clone().unwrap_or_else(|| "all".into())),
            session_id: i.session.map(|s| s.to_string()),
            detail: i.detail(),
            settled_by: None,
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
    /// The update in one line (at most [`HEADLINE_WORDS`] words): what the
    /// chat shows first.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub headline: String,
    /// What it did and what only the person can do, by session: at most
    /// [`BRIEF_ITEMS`] lines in all, what it left alone as a count.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub brief: Vec<BriefGroup>,
    /// The lines past [`BRIEF_ITEMS`] the brief leaves out.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub more_items: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// The most lines an update's brief shows, across its groups.
pub const BRIEF_ITEMS: usize = 3;
/// The most words in an update's headline.
pub const HEADLINE_WORDS: usize = 12;

/// What a brief line is: something the boss did, or something only the
/// person can do (answered with "Done" alone).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BriefKind {
    Done,
    ForYou,
}

/// One line of an update's brief (`extras.update.brief[].items[]`): every
/// field always on the wire, null when it has none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BriefItem {
    /// The decision's id (done) or the item's (for you).
    pub id: String,
    pub kind: BriefKind,
    /// What, at most [`crate::decision::BRIEF_WORDS`] words.
    pub action: String,
    /// Why in a few words, no full stop.
    #[serde(default)]
    pub why: Option<String>,
    /// The rest, behind a disclosure.
    #[serde(default)]
    pub detail: Option<String>,
    /// A done line's undo button, as [`DecisionLine::undo`].
    #[serde(default)]
    pub undo: Option<String>,
    /// A for-you line settled since, filled when the page is read.
    #[serde(default)]
    pub settled_by: Option<SettledBy>,
    pub at: DateTime<Utc>,
}

/// One session's lines in an update's brief; `sessionId` null for what
/// isn't about one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BriefGroup {
    #[serde(default)]
    pub session_id: Option<String>,
    pub label: String,
    #[serde(default)]
    pub items: Vec<BriefItem>,
    /// How many times it looked and left it alone: a count, never lines.
    #[serde(default)]
    pub left_alone: u32,
}

/// The label of a group not about one session.
pub const CREW_LABEL: &str = "Crew";

/// An update's brief from its decisions (`lines`, laid out from `rows`)
/// and the open items only the person can do (`for_you`), each group
/// named from `names`: what only the person can do first, then what the
/// boss did, newest first, [`BRIEF_ITEMS`] in all; what it left alone
/// counted; the lines left out counted apart.
pub fn brief(
    rows: &[BossDecision],
    lines: &[DecisionLine],
    for_you: &[OpenItem],
    names: &HashMap<Uuid, String>,
) -> (Vec<BriefGroup>, u32) {
    use crate::decision::DecisionKind;
    let mut groups: Vec<BriefGroup> = Vec::new();
    let group = |groups: &mut Vec<BriefGroup>, s: Option<Uuid>| -> usize {
        let id = s.map(|s| s.to_string());
        match groups.iter().position(|g| g.session_id == id) {
            Some(i) => i,
            None => {
                let label = s
                    .and_then(|s| names.get(&s).cloned())
                    .unwrap_or_else(|| CREW_LABEL.to_string());
                groups.push(BriefGroup {
                    session_id: id,
                    label,
                    items: Vec::new(),
                    left_alone: 0,
                });
                groups.len() - 1
            }
        }
    };
    let mut all: Vec<(Option<Uuid>, BriefItem)> = Vec::new();
    let mut todo: Vec<&OpenItem> = for_you.iter().collect();
    todo.sort_by_key(|i| std::cmp::Reverse(i.opened_at));
    for i in todo {
        all.push((
            i.session,
            BriefItem {
                id: i.id.to_string(),
                kind: BriefKind::ForYou,
                action: crate::crew::clip(
                    &crate::crew::words(&i.headline(), crate::decision::BRIEF_WORDS),
                    crate::decision::BRIEF_CHARS,
                ),
                why: i
                    .holding_up
                    .as_deref()
                    .and_then(crate::decision::clause)
                    .map(|h| crate::crew::words(&h, crate::decision::WHY_WORDS)),
                detail: i.detail(),
                undo: None,
                settled_by: None,
                at: i.opened_at,
            },
        ));
    }
    let mut done: Vec<(&BossDecision, &DecisionLine)> = Vec::new();
    let mut alone: Vec<Option<Uuid>> = Vec::new();
    for d in rows {
        match d.kind {
            DecisionKind::LeftAlone => alone.push(d.session),
            DecisionKind::Acted => {
                if let Some(l) = lines.iter().find(|l| l.id == d.id.to_string()) {
                    done.push((d, l));
                }
            }
            // Asked of the person (its own card) or taken back: no line.
            DecisionKind::Escalated | DecisionKind::Undone => {}
        }
    }
    done.sort_by_key(|(d, _)| std::cmp::Reverse(d.at));
    for (d, l) in done {
        let why = crate::decision::clause(&l.why);
        let full = d.why.trim();
        let detail = (!full.is_empty()
            && crate::open_item::normalised(full) != crate::open_item::normalised(l.why.trim()))
        .then(|| full.to_string());
        all.push((
            d.session,
            BriefItem {
                id: l.id.clone(),
                kind: BriefKind::Done,
                action: l.action.trim().trim_end_matches('.').to_string(),
                why,
                detail,
                undo: l.undo.clone(),
                settled_by: None,
                at: d.at,
            },
        ));
    }
    let more = all.len().saturating_sub(BRIEF_ITEMS) as u32;
    for (s, item) in all.into_iter().take(BRIEF_ITEMS) {
        let g = group(&mut groups, s);
        groups[g].items.push(item);
    }
    // Counted where its lines are, after them.
    for s in alone {
        let g = group(&mut groups, s);
        groups[g].left_alone += 1;
    }
    (groups, more)
}

/// The update's headline written plainly: what it did, what is the
/// person's (`for_you` in all) and what it left alone, in counts.
pub fn plain_headline(u: &MentionedUpdate, for_you: usize) -> String {
    let count = |k: BriefKind| {
        u.brief
            .iter()
            .flat_map(|g| &g.items)
            .filter(|i| i.kind == k)
            .count()
    };
    // Lines left out are what only the person can do past those shown,
    // then what it did.
    let shown = count(BriefKind::ForYou);
    let done = count(BriefKind::Done)
        + (u.more_items as usize).saturating_sub(for_you - shown.min(for_you));
    let waiting = u.needs_you.len();
    let alone: u32 = u.brief.iter().map(|g| g.left_alone).sum();
    let mut parts: Vec<String> = Vec::new();
    if done > 0 {
        parts.push(format!("{done} done"));
    }
    if for_you > 0 {
        parts.push(format!("{for_you} for you"));
    }
    if waiting > 0 {
        parts.push(format!("{waiting} waiting on you"));
    }
    if alone > 0 {
        parts.push(format!("{alone} left alone"));
    }
    if parts.is_empty() {
        return match u.sessions.len() {
            0 => "Nothing new.".into(),
            1 => format!("{} moved on.", u.sessions[0].name),
            n => format!("{n} sessions moved on."),
        };
    }
    let mut out = parts.join(", ");
    if let Some(c) = out.get(..1) {
        out = c.to_uppercase() + &out[1..];
    }
    format!("{out}.")
}

/// `headline` within [`HEADLINE_WORDS`] words, one line; none when empty.
pub fn headline_line(headline: &str) -> Option<String> {
    let line = crate::crew::words(&crate::crew::one_line(headline, usize::MAX), HEADLINE_WORDS);
    (!line.trim().is_empty()).then_some(line)
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
    if !u.headline.trim().is_empty() {
        return render_brief(u);
    }
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

/// A briefed update's text: its headline, each line ("✓ did — why",
/// "● For you: … — why"), what it left alone as a count, what it left
/// out, what waits on the person in one line each, and what it kept.
pub fn render_brief(u: &MentionedUpdate) -> String {
    let mut out: Vec<String> = vec![u.headline.trim().to_string()];
    let mut lines: Vec<String> = Vec::new();
    for i in u.brief.iter().flat_map(|g| &g.items) {
        let mark = match i.kind {
            BriefKind::Done => "✓",
            BriefKind::ForYou => "● For you:",
        };
        lines.push(match i.why.as_deref().filter(|w| !w.trim().is_empty()) {
            Some(why) => format!("{mark} {} — {why}", i.action),
            None => format!("{mark} {}", i.action),
        });
    }
    // Where the brief leaves room, what the sessions did, one sentence
    // each; the rest counted with what it left out.
    let room = BRIEF_ITEMS.saturating_sub(lines.len());
    for s in u.sessions.iter().take(room) {
        lines.push(format!(
            "**{}** — {}",
            s.name,
            crate::crew::words(s.text.trim(), SESSION_WORDS)
        ));
    }
    let more = u.more_items as usize + u.sessions.len().saturating_sub(room);
    if more > 0 {
        lines.push(format!("+{more} more"));
    }
    let alone: u32 = u.brief.iter().map(|g| g.left_alone).sum();
    if alone > 0 {
        lines.push(format!(
            "Left alone: {alone} {}.",
            if alone == 1 { "time" } else { "times" }
        ));
    }
    for n in &u.needs_you {
        lines.push(format!("● Needs you: {}", n.question));
    }
    if !lines.is_empty() {
        out.push(lines.join("\n"));
    }
    out.extend(u.kept.iter().cloned());
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
pub const COMPOSER_SYSTEM: &str = "You write parts of an overseer's short update to the person it works for, in its voice (\"I\", plain words, no lists, no headings). Critical facts only: never retell a plan, never quote a message.\n\
headline: the whole update in at most 12 words (\"Unblocked web's release; one thing needs you.\").\n\
For each session given, write one sentence of at most 25 words: where it is and what's next, from what it did since the person was last told. Don't mention decisions, questions or anything waiting on the person in these: those are added separately.\n\
For each decision given, write its action: what you did, in at most 12 words (\"Lifted my hold on tonight's release.\", \"Let web wait for CI.\"); and its why: the reason in at most 8 words (\"CI is green again\"). Never quote or repeat the message you sent: the person reads it separately.\n\
Leave out anything not given; never invent PR numbers, names or times. Answer with JSON only: {\"headline\": \"<line>\", \"sessions\": [{\"id\": \"<session id>\", \"text\": \"<sentence>\"}], \"decisions\": [{\"id\": \"<decision id>\", \"action\": \"<short line>\", \"why\": \"<few words>\"}]}.";

/// The most words of a session's sentence in an update.
pub const SESSION_WORDS: usize = 25;

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
    pub headline: Option<String>,
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
    let mut out = Composed {
        headline: v["headline"]
            .as_str()
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(str::to_string),
        ..Default::default()
    };
    let sessions = v["sessions"].as_array();
    let decisions = v["decisions"].as_array();
    if sessions.is_none() && decisions.is_none() && out.headline.is_none() {
        return None;
    }
    for s in sessions.into_iter().flatten() {
        if let (Some(id), Some(text)) = (id_of(s), text_of(s, "text")) {
            out.sessions.insert(id, crate::crew::one_line(&text, 800));
        }
    }
    for d in decisions.into_iter().flatten() {
        let (action, why) = (text_of(d, "action"), text_of(d, "why"));
        if let (Some(id), true) = (id_of(d), action.is_some() || why.is_some()) {
            out.decisions
                .insert(id, (action.unwrap_or_default(), why.unwrap_or_default()));
        }
    }
    Some(out)
}

/// What of `c` is over its bounds, said for the composer to write again:
/// the headline past [`HEADLINE_WORDS`], a session's sentence past
/// [`SESSION_WORDS`], a decision's action past
/// [`crate::decision::BRIEF_WORDS`] words or [`crate::decision::BRIEF_CHARS`]
/// characters, its why past [`crate::decision::WHY_WORDS`] words.
pub fn too_long(c: &Composed) -> Vec<String> {
    use crate::crew::word_count;
    use crate::decision::{BRIEF_CHARS, BRIEF_WORDS, WHY_WORDS};
    let mut out = Vec::new();
    if c.headline
        .as_deref()
        .is_some_and(|h| word_count(h) > HEADLINE_WORDS)
    {
        out.push(format!("headline (at most {HEADLINE_WORDS} words)"));
    }
    let mut sessions: Vec<_> = c.sessions.iter().collect();
    sessions.sort_by_key(|(id, _)| **id);
    for (id, text) in sessions {
        if word_count(text) > SESSION_WORDS {
            out.push(format!("session {id} text (at most {SESSION_WORDS} words)"));
        }
    }
    let mut decisions: Vec<_> = c.decisions.iter().collect();
    decisions.sort_by_key(|(id, _)| **id);
    for (id, (action, why)) in decisions {
        if word_count(action) > BRIEF_WORDS || action.chars().count() > BRIEF_CHARS {
            out.push(format!(
                "decision {id} action (at most {BRIEF_WORDS} words, {BRIEF_CHARS} characters)"
            ));
        }
        if word_count(why) > WHY_WORDS {
            out.push(format!("decision {id} why (at most {WHY_WORDS} words)"));
        }
    }
    out
}

/// What the composer is asked the second time, when `fields` of its first
/// answer were too long: those alone, shorter, in the same JSON.
pub fn reask(fields: &[String]) -> String {
    format!(
        "\n\nYour last answer was too long in: {}. Write only those again, within the limits, \
         in the same JSON shape (leave out every other field).",
        fields.join("; ")
    )
}

/// `again`'s fields over `first`'s: the second answer's shorter words win.
pub fn merge_composed(mut first: Composed, again: Composed) -> Composed {
    if again.headline.is_some() {
        first.headline = again.headline;
    }
    first.sessions.extend(again.sessions);
    for (id, (action, why)) in again.decisions {
        let e = first.decisions.entry(id).or_default();
        if !action.is_empty() {
            e.0 = action;
        }
        if !why.is_empty() {
            e.1 = why;
        }
    }
    first
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
                detail: None,
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
                    detail: None,
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
        assert!(COMPOSER_SYSTEM.contains("at most 12 words"));
        assert!(composer_decisions(&[d]).contains("message sent (don't quote it)"));
    }

    fn item_of(new: &crate::open_item::NewItem) -> OpenItem {
        let now = Utc::now();
        OpenItem {
            id: Uuid::new_v4(),
            boss: new.boss.clone(),
            owner: "you".into(),
            kind: new.kind,
            session: new.session,
            line_message: None,
            text: new.text.clone(),
            holding_up: new.holding_up.clone(),
            recommendation: new.recommendation.clone(),
            options: new.options.clone(),
            generalizes: None,
            done_when: None,
            dedupe_key: new.dedupe_key.clone(),
            opened_at: now,
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
            prefer_scope: None,
            cant_decide: None,
        }
    }

    #[test]
    fn the_brief_is_three_lines_at_most_what_was_left_alone_a_count() {
        let web = Uuid::new_v4();
        let at = |d: BossDecision, mins: i64| BossDecision {
            session: Some(web),
            at: Utc::now() - Duration::minutes(mins),
            ..d
        };
        let acted: Vec<BossDecision> = (0..4)
            .map(|n| {
                at(
                    nudge(&format!("Message number {n} for web, sent in full.")),
                    n,
                )
            })
            .collect();
        let alone = |n| {
            at(
                BossDecision::new(
                    "desk",
                    crate::decision::Need::TurnEnd,
                    "nothing",
                    "Still running its tests.",
                    crate::decision::How::Own,
                ),
                n,
            )
        };
        let mut left = alone(9);
        left.kind = crate::decision::DecisionKind::LeftAlone;
        let mut left2 = alone(10);
        left2.kind = crate::decision::DecisionKind::LeftAlone;
        let mut rows = acted.clone();
        rows.push(left);
        rows.push(left2);
        let lines = decision_lines(&rows, &HashMap::new());
        let mut todo = crate::open_item::NewItem::ask(
            "desk",
            crate::open_item::OpenKind::Todo,
            None,
            "Rotate the staging deploy key: it is in your own vault account, which I can't reach.",
        );
        todo.holding_up = Some("The staging deploy waits on it.".into());
        let item = item_of(&todo);
        let names = HashMap::from([(web, "acme-web".to_string())]);
        let (groups, more) = brief(&rows, &lines, std::slice::from_ref(&item), &names);
        // What only the person can do first, then the newest it did.
        assert_eq!(groups[0].session_id, None);
        assert_eq!(groups[0].label, CREW_LABEL);
        assert_eq!(groups[0].items[0].kind, BriefKind::ForYou);
        assert_eq!(
            groups[0].items[0].action,
            "Rotate the staging deploy key: it is in your own vault account…"
        );
        assert_eq!(
            groups[0].items[0].why.as_deref(),
            Some("the staging deploy waits on it")
        );
        let web_group = &groups[1];
        assert_eq!(web_group.label, "acme-web");
        assert_eq!(web_group.items.len(), 2);
        assert_eq!(web_group.items[0].id, acted[0].id.to_string());
        assert_eq!(web_group.items[0].action, "Sent web a message");
        assert_eq!(
            web_group.items[0].why.as_deref(),
            Some("you confirmed it yourself at 15:33")
        );
        assert_eq!(web_group.left_alone, 2);
        assert_eq!(more, 2);
        let mut u = MentionedUpdate {
            brief: groups,
            more_items: more,
            ..Default::default()
        };
        u.headline = plain_headline(&u, 1);
        assert_eq!(u.headline, "4 done, 1 for you, 2 left alone.");
        let text = render(&u, &[]);
        assert_eq!(
            text,
            "4 done, 1 for you, 2 left alone.\n\n\
             ● For you: Rotate the staging deploy key: it is in your own vault account… — the staging deploy waits on it\n\
             ✓ Sent web a message — you confirmed it yourself at 15:33\n\
             ✓ Sent web a message — you confirmed it yourself at 15:33\n\
             +2 more\n\
             Left alone: 2 times."
        );
        assert!(!text.contains("Message number"));
        // The wire: every brief field there, null when none.
        let v = serde_json::to_value(&u).unwrap();
        let first = &v["brief"][1]["items"][0];
        for k in [
            "id",
            "kind",
            "action",
            "why",
            "detail",
            "undo",
            "settledBy",
            "at",
        ] {
            assert!(first.get(k).is_some(), "{k} in {first}");
        }
        assert_eq!(v["brief"][0]["items"][0]["kind"], "forYou");
        assert_eq!(v["moreItems"], 2);
        assert_eq!(v["brief"][1]["leftAlone"], 2);
    }

    #[test]
    fn what_is_past_its_bounds_is_named_for_the_reask() {
        let id = Uuid::new_v4();
        let c = Composed {
            headline: Some(
                "one two three four five six seven eight nine ten eleven twelve thirteen".into(),
            ),
            sessions: HashMap::new(),
            decisions: HashMap::from([(
                id,
                (
                    "Held it.".to_string(),
                    "one two three four five six seven eight nine".to_string(),
                ),
            )]),
        };
        let long = too_long(&c);
        assert_eq!(
            long,
            vec![
                "headline (at most 12 words)".to_string(),
                format!("decision {id} why (at most 8 words)"),
            ]
        );
        assert!(reask(&long).contains("too long in: headline (at most 12 words);"));
        let again = Composed {
            headline: Some("Held the release.".into()),
            sessions: HashMap::new(),
            decisions: HashMap::from([(id, (String::new(), "CI failed".to_string()))]),
        };
        let merged = merge_composed(c, again);
        assert_eq!(merged.headline.as_deref(), Some("Held the release."));
        assert_eq!(
            merged.decisions[&id],
            ("Held it.".to_string(), "CI failed".to_string())
        );
        assert!(too_long(&merged).is_empty());
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
            prefer_scope: None,
            cant_decide: None,
        };
        // Plainly: its first sentence, no points, nothing recommended.
        let plain = NeedsYou::of(&i, now, false);
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
        let laid = NeedsYou::of(&i, now, false);
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
        assert_eq!(v["preferScope"], "all");
        assert!(serde_json::to_value(&plain)
            .unwrap()
            .get("preferScope")
            .is_none());
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
