//! What a boss decided without the person that is routine (it allowed an
//! ordinary command or edit, sent a session an ordinary line, looked and
//! left it alone), said as runs: one line a session and kind of decision
//! ("Allowed Cufflink Migration 16 times, 09:05–09:15: test and fixture
//! edits, and local test runs."), the run's decisions behind it in full.
//!
//! A roundup lists only the important ones under "Decided without you"
//! ([`told`]), one entry a session ([`by_session`]): its count and when,
//! its decisions under it; the routine ones are its "N routine things ·
//! Show", as runs.
//! A message right away folds a run into one line of its "What I did"
//! ([`folded`]).

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use uuid::Uuid;
use workforce_common::oversight::Decided;

use crate::decision::{BossDecision, DecisionKind, DecisionLine, UndoPlan};
use crate::update::{is_close, BriefItem, BriefKind};
use crate::update_cadence::Important;
use crate::update_write::{capitalised, EventKind, Said, SessionLink};

/// Whether `d` is routine: what it did that is not important, or a look
/// that left the session alone. The sessions it closed are said in their
/// own line, never as routine.
pub fn is_routine(d: &BossDecision) -> bool {
    match d.kind {
        DecisionKind::Acted => d.important.is_none() && !is_close(d),
        DecisionKind::LeftAlone => true,
        DecisionKind::Escalated | DecisionKind::Undone => false,
    }
}

/// The kind of decision a run is of: what it did ("allowed", "told"), or
/// that it left the session alone.
fn verb(d: &BossDecision) -> String {
    match d.kind {
        DecisionKind::LeftAlone => "left".into(),
        _ => d
            .what
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_lowercase(),
    }
}

/// The session `d` is about, named as `what` names it ("allowed Cufflink
/// Migration", "told web “…”"): what follows its verb, before any quote.
fn named_in(d: &BossDecision) -> Option<String> {
    let rest = d.what.split_once(' ')?.1;
    let name = rest.split(['“', '"']).next()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// One session's decisions of one kind, in the order they were made.
#[derive(Debug, Clone, PartialEq)]
pub struct Run<'a> {
    pub session: Option<Uuid>,
    pub rows: Vec<&'a BossDecision>,
    /// Its line's opening, a count and when ("Allowed web 16 times,
    /// 09:05–09:15"), no full stop.
    pub head: String,
    /// What it allowed, by theme ("test and fixture edits, and local test
    /// runs"); none when its reasons say nothing that groups.
    pub themes: Option<String>,
}

impl Run<'_> {
    /// Its line, one sentence: its opening, and what it was about.
    pub fn line(&self) -> String {
        match &self.themes {
            Some(t) => format!("{}: {t}.", self.head),
            None => format!("{}.", self.head),
        }
    }

    /// The run as a line of a roundup: its sentence, its session's link,
    /// and each of its decisions behind it (from `lines`, laid out from its
    /// rows), the newest first.
    pub fn said(&self, lines: &[DecisionLine], session: Option<SessionLink>) -> Said {
        let mut did: Vec<BriefItem> = self
            .rows
            .iter()
            .filter_map(|d| Some(crate::update::brief_item(d, line_of(lines, d)?)))
            .collect();
        did.reverse();
        Said {
            kind: EventKind::Routine,
            headline: self.line(),
            body: String::new(),
            ids: vec![],
            not_checked: None,
            session,
            did,
            at: self.rows.first().map(|d| d.at),
            decisions: None,
        }
    }
}

fn line_of<'l>(lines: &'l [DecisionLine], d: &BossDecision) -> Option<&'l DecisionLine> {
    lines.iter().find(|l| l.id == d.id.to_string())
}

/// `rows` as runs: one for each session and kind of decision, in the order
/// each run began. `name` names a session as the person knows it (else as
/// its decisions did); times are said in `tz`.
pub fn runs<'a>(
    rows: impl IntoIterator<Item = &'a BossDecision>,
    name: &dyn Fn(Uuid) -> Option<String>,
    tz: &Tz,
) -> Vec<Run<'a>> {
    let mut rows: Vec<&BossDecision> = rows.into_iter().collect();
    rows.sort_by_key(|d| d.at);
    // What is about no session is grouped by what it names.
    type Key = (Option<Uuid>, String, Option<String>);
    let mut groups: Vec<(Key, Vec<&BossDecision>)> = Vec::new();
    for d in rows {
        let key = (
            d.session,
            verb(d),
            d.session.is_none().then(|| named_in(d)).flatten(),
        );
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, g)) => g.push(d),
            None => groups.push((key, vec![d])),
        }
    }
    groups
        .into_iter()
        .map(|((session, verb, _), rows)| {
            let named = session
                .and_then(name)
                .or_else(|| rows.iter().find_map(|d| named_in(d)))
                .unwrap_or_else(|| "the crew".into());
            let themes = match verb.as_str() {
                "allowed" => themes(rows.iter().map(|d| d.why.as_str())),
                _ => None,
            };
            Run {
                session,
                head: head(&verb, &named, &rows, tz),
                themes,
                rows,
            }
        })
        .collect()
}

/// "Allowed web 16 times, 09:05–09:15", "Left web alone 3 times at
/// 09:20", "Told web at 09:05".
fn head(verb: &str, name: &str, rows: &[&BossDecision], tz: &Tz) -> String {
    let clock = |at: DateTime<Utc>| at.with_timezone(tz).format("%H:%M").to_string();
    let who = match verb {
        "left" => format!("Left {name} alone"),
        v => format!("{} {name}", capitalised(v)),
    };
    let (Some(first), Some(last)) = (rows.first(), rows.last()) else {
        return who;
    };
    let (from, until) = (clock(first.at), clock(last.at));
    match (rows.len(), from == until) {
        (1, _) => format!("{who} at {from}"),
        (n, true) => format!("{who} {n} times at {from}"),
        (n, false) => format!("{who} {n} times, {from}–{until}"),
    }
}

/// A theme of what a boss allowed, read off its reason: the first whose
/// words the reason holds.
const THEMES: &[(&str, &[&str], &[&str])] = &[
    (
        "local test runs",
        &["running", "runs ", "run "],
        &["test", "suite", "shellcheck", "lint"],
    ),
    (
        "test and fixture edits",
        &["edit", "adding", "adds ", "add ", "sed "],
        &["test", "fixture", "mock"],
    ),
    (
        "doc edits",
        &[
            "edit",
            "adding",
            "adds ",
            "documents",
            "wording",
            "correcting",
        ],
        &["doc", "design", ".md", "runbook", "description", "comment"],
    ),
    ("local commits", &["committing", "commits the"], &[""]),
    (
        "read-only checks",
        &[
            "read-only",
            "read only",
            "reads",
            "reading",
            "lists",
            "listing",
            "grep",
            "lookup",
            "check",
            "query",
            "diff",
        ],
        &[""],
    ),
    (
        "code edits",
        &["edit", "implement", "adding", "adds "],
        &[""],
    ),
];

/// How many themes a run's line names before the rest are "other steps".
const THEMES_SAID: usize = 2;

/// What `whys` (the reasons of what a boss allowed) were about, as a few
/// themes, the commonest first ("test and fixture edits, and local test
/// runs"), with "in its worktree" when every one was there. Never their
/// words, which hold commits and ids. None when none has a theme.
pub fn themes<'a>(whys: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let whys: Vec<String> = whys.into_iter().map(str::to_lowercase).collect();
    let theme_of = |w: &str| {
        THEMES.iter().position(|(_, any, and)| {
            any.iter().any(|a| w.contains(a)) && and.iter().any(|b| w.contains(b))
        })
    };
    let mut counts = vec![0usize; THEMES.len()];
    let mut other = 0usize;
    for w in &whys {
        match theme_of(w) {
            Some(i) => counts[i] += 1,
            None => other += 1,
        }
    }
    let mut named: Vec<(usize, usize)> = counts
        .iter()
        .enumerate()
        .filter(|(_, n)| **n > 0)
        .map(|(i, n)| (i, *n))
        .collect();
    // The commonest first; even, in the order the themes are listed.
    named.sort_by_key(|(i, n)| (std::cmp::Reverse(*n), *i));
    if named.is_empty() {
        return None;
    }
    let mut said: Vec<&str> = named
        .iter()
        .take(THEMES_SAID)
        .map(|(i, _)| THEMES[*i].0)
        .collect();
    if named.len() > THEMES_SAID || other > 0 {
        said.push("other steps");
    }
    let listed = match said.as_slice() {
        [one] => one.to_string(),
        [two, last] => format!("{two}, and {last}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
        [] => return None,
    };
    let everywhere = whys.iter().all(|w| w.contains("worktree"));
    Some(match (everywhere, whys.len()) {
        (false, _) => listed,
        (true, 1) => format!("{listed} in its worktree"),
        (true, _) => format!("{listed}, all in its worktree"),
    })
}

/// What a roundup tells of what a boss decided without the person, by
/// their decisions setting.
#[derive(Debug, Clone, PartialEq)]
pub struct Told<'a> {
    /// Listed under "Decided without you", in their session's entry
    /// ([`by_session`]): the important ones.
    pub listed: Vec<&'a BossDecision>,
    /// Listed in their session's entry too, as runs: the routine ones,
    /// when the person asked for every one.
    pub listed_runs: Vec<Run<'a>>,
    /// Behind "N routine things · Show", as runs.
    pub routine: Vec<Run<'a>>,
}

impl Told<'_> {
    /// How many things it left out of the roundup ("N routine things").
    pub fn routine_count(&self) -> u32 {
        self.routine.iter().map(|r| r.rows.len() as u32).sum()
    }
}

/// What a roundup tells of `rows` by `decided`: the important ones listed
/// (unless the person hears of them only when they ask), the routine ones
/// as runs, listed too when they asked for every one; looks that left a
/// session alone always routine. The sessions it closed, what it gave the
/// person and what they undid are said elsewhere.
pub fn told<'a>(
    rows: &'a [BossDecision],
    decided: Decided,
    name: &dyn Fn(Uuid) -> Option<String>,
    tz: &Tz,
) -> Told<'a> {
    let mut listed = Vec::new();
    let mut every = Vec::new();
    let mut routine = Vec::new();
    for d in rows.iter().filter(|d| !is_close(d)) {
        match (d.kind, d.important.is_some(), decided) {
            (DecisionKind::LeftAlone, _, _) => routine.push(d),
            (DecisionKind::Acted, true, Decided::Ask) => routine.push(d),
            (DecisionKind::Acted, true, _) => listed.push(d),
            (DecisionKind::Acted, false, Decided::All) => every.push(d),
            (DecisionKind::Acted, false, _) => routine.push(d),
            (DecisionKind::Escalated | DecisionKind::Undone, _, _) => {}
        }
    }
    Told {
        listed,
        listed_runs: runs(every, name, tz),
        routine: runs(routine, name, tz),
    }
}

/// Whether `d` sent a session words (a line, or an answer to its
/// question), rather than doing something itself.
fn is_message(d: &BossDecision) -> bool {
    matches!(d.plan(), Some(UndoPlan::Line { .. }))
}

/// Whether `d` did something that matters itself: merged, released,
/// deleted, spent, or overrode a session. A message about doing so (later,
/// or once CI is green) never is: the session does it, and says so.
pub fn is_notable(d: &BossDecision) -> bool {
    d.kind == DecisionKind::Acted
        && !is_message(d)
        && d.important.is_some_and(|i| i != Important::Later)
}

/// How much what an entry of "Decided without you" holds matters, for its
/// order: something done that matters itself, then what was important
/// (messages about merging or releasing later), then the routine.
fn weight(d: &BossDecision) -> u8 {
    match (is_notable(d), d.important.is_some()) {
        (true, _) => 2,
        (false, true) => 1,
        (false, false) => 0,
    }
}

/// What a roundup lists under "Decided without you" for one session (or
/// for what is about no session): one line, its decisions under it.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry<'a> {
    pub session: Option<Uuid>,
    /// The session as the person knows it; "Crew" for what is about none.
    pub name: String,
    /// Its decisions, in the order they were made.
    pub rows: Vec<&'a BossDecision>,
}

impl Entry<'_> {
    /// Its line: one sentence ("Billing Migration · 14 decisions,
    /// 17:05–17:55"), the session's link, and under it each decision
    /// (from `lines`, laid out from its rows), the newest first, without
    /// the session's name again ("Sent it a message"); a run of routine
    /// ones folded into one line, as "What I did" folds them. Every one has
    /// its Undo; a Desk shows the first few and the notable ones, the rest
    /// behind "N more". Its kind is routine so a Desk older than this lists
    /// it as a run, each of them behind "Show all".
    pub fn said(&self, lines: &[DecisionLine], session: Option<SessionLink>, tz: &Tz) -> Said {
        let mut items: Vec<BriefItem> = self
            .rows
            .iter()
            .filter_map(|d| Some(crate::update::brief_item(d, line_of(lines, d)?)))
            .collect();
        items.reverse();
        let rows: Vec<BossDecision> = self.rows.iter().map(|d| (*d).clone()).collect();
        let mut did = folded(items, &rows, tz);
        for i in &mut did {
            i.action = without_name(&i.action, &self.name);
        }
        let n: u32 = did.iter().map(|i| i.folded.unwrap_or(1)).sum();
        let clock = |at: DateTime<Utc>| at.with_timezone(tz).format("%H:%M").to_string();
        let (from, until) = match (self.rows.first(), self.rows.last()) {
            (Some(f), Some(l)) => (clock(f.at), clock(l.at)),
            _ => (String::new(), String::new()),
        };
        let count = match n {
            1 => "1 decision".to_string(),
            n => format!("{n} decisions"),
        };
        let when = match from == until {
            true => format!(" at {from}"),
            false => format!(", {from}–{until}"),
        };
        Said {
            kind: EventKind::Routine,
            headline: format!("{} · {count}{when}", self.name),
            body: String::new(),
            ids: vec![],
            not_checked: None,
            session,
            did,
            at: self.rows.last().map(|d| d.at),
            decisions: Some(n),
        }
    }
}

/// `action` said under its session's own line: "Sent Billing Migration a
/// message" as "Sent it a message", "Answered Billing Migration's
/// question" as "Answered its question".
fn without_name(action: &str, name: &str) -> String {
    let name = name.trim();
    if name.is_empty() || name == crate::update::CREW_LABEL {
        return action.to_string();
    }
    let possessive = format!("{name}'s");
    match (action.find(&possessive), action.find(name)) {
        (Some(_), _) => action.replacen(&possessive, "its", 1),
        (None, Some(_)) => action.replacen(name, "it", 1),
        (None, None) => action.to_string(),
    }
}

/// What a roundup lists under "Decided without you" (`rows`: the
/// important ones, and every one when the person asked for that), as one
/// entry a session and one for what is about no session: those that did
/// something that matters first, then those that only said something
/// important, the latest first within each. `name` names a session as the
/// person knows it (else as its decisions did).
pub fn by_session<'a>(
    rows: impl IntoIterator<Item = &'a BossDecision>,
    name: &dyn Fn(Uuid) -> Option<String>,
) -> Vec<Entry<'a>> {
    let mut rows: Vec<&BossDecision> = rows.into_iter().collect();
    rows.sort_by_key(|d| d.at);
    let mut entries: Vec<Entry> = Vec::new();
    for d in rows {
        match entries.iter_mut().find(|e| e.session == d.session) {
            Some(e) => e.rows.push(d),
            None => entries.push(Entry {
                session: d.session,
                name: String::new(),
                rows: vec![d],
            }),
        }
    }
    for e in &mut entries {
        e.name = match e.session {
            Some(s) => name(s)
                .or_else(|| e.rows.iter().find_map(|d| named_in(d)))
                .unwrap_or_else(|| "a session".into()),
            None => crate::update::CREW_LABEL.into(),
        };
    }
    entries.sort_by_key(|e| {
        let weight = e.rows.iter().map(|d| weight(d)).max().unwrap_or(0);
        let latest = e.rows.last().map(|d| d.at);
        (std::cmp::Reverse(weight), std::cmp::Reverse(latest))
    });
    entries
}

/// How many routine items in a row of one kind a "What I did" folds into
/// one line.
const FOLD_FROM: usize = 2;

/// `items` (one session's "What I did", the newest first, each laid out
/// from its row in `rows`) with each run of routine decisions that has no
/// Undo folded into one line: what the run was, and each of them behind
/// its "more". The rest stay as they are.
pub fn folded(items: Vec<BriefItem>, rows: &[BossDecision], tz: &Tz) -> Vec<BriefItem> {
    let row = |i: &BriefItem| rows.iter().find(|d| d.id.to_string() == i.id);
    let foldable = |i: &BriefItem| i.undo.is_none() && row(i).is_some_and(is_routine);
    let run_rows: Vec<&BossDecision> = items
        .iter()
        .filter(|i| foldable(i))
        .filter_map(row)
        .collect();
    let runs: Vec<Run> = runs(run_rows, &|_| None, tz)
        .into_iter()
        .filter(|r| r.rows.len() >= FOLD_FROM)
        .collect();
    let run_of = |i: &BriefItem| {
        runs.iter()
            .position(|r| r.rows.iter().any(|d| d.id.to_string() == i.id))
    };
    let clock = |at: DateTime<Utc>| at.with_timezone(tz).format("%H:%M").to_string();
    let mut out: Vec<BriefItem> = Vec::new();
    let mut said: Vec<usize> = Vec::new();
    for i in &items {
        let Some(r) = run_of(i).filter(|_| foldable(i)) else {
            out.push(i.clone());
            continue;
        };
        if said.contains(&r) {
            continue;
        }
        said.push(r);
        let each: Vec<String> = items
            .iter()
            .filter(|j| run_of(j) == Some(r))
            .map(|j| match &j.why {
                Some(why) => format!("{} {} — {why}", clock(j.at), j.action),
                None => format!("{} {}", clock(j.at), j.action),
            })
            .collect();
        out.push(BriefItem {
            id: i.id.clone(),
            kind: BriefKind::Done,
            action: runs[r].head.clone(),
            why: runs[r].themes.clone(),
            detail: Some(each.join("\n")),
            undo: None,
            settled_by: None,
            at: i.at,
            folded: Some(each.len() as u32),
            notable: false,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision::{How, Need};
    use crate::update_cadence::{important, ActionKind, Important};
    use chrono::TimeZone;

    const FIX: &str = "f157c5c0-dfff-48e5-8767-5413fdfbd665";

    fn at(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 17, h, m, 0).unwrap()
    }

    /// One decision of the boss's about the migration session at `h:m`,
    /// its importance judged as the hub judges it, from what it allowed.
    fn allowed(h: u32, m: u32, request: &str, why: &str) -> BossDecision {
        let mut d = BossDecision::new(
            "desk",
            Need::Approval,
            "allowed Billing Migration",
            why,
            How::Own,
        );
        d.at = at(h, m);
        d.session = Some(FIX.parse().unwrap());
        d.important = important(ActionKind::Approve, request);
        d
    }

    /// An hour like the 10:00 roundup of 17 Nov: sixteen ordinary steps in
    /// the migration's fix worktree, allowed one by one, each a file or
    /// a script whose name says "deploy", and one merge.
    fn the_hour() -> Vec<BossDecision> {
        let wt = "/home/p/repo/.worktrees/fix-d15";
        let steps: Vec<(u32, String, &str)> = vec![
            (5, format!("Read: {wt}/tests/swarm-drift-tests.sh"), "A read-only look at the swarm-drift test fixture and test script, which the deploy-step fix needs, in the fix agent's worktree."),
            (6, format!("Bash: git -C {wt} log --oneline main..HEAD"), "This is a read-only git check of the fix agent's worktree (current branch, commits ahead of main)."),
            (7, format!("Edit: {wt}/tests/swarm-drift-tests.sh"), "Adding placement-only test cases to swarm-drift-tests.sh in the fix agent's own worktree is local and reversible."),
            (7, format!("Bash: cd {wt} && ./tests/swarm-drift-tests.sh"), "Running the repo's swarm-drift test suite in the fix agent's worktree only runs local tests."),
            (8, format!("Bash: grep -rn deploy {wt}/scripts"), "A read-only grep in the fix agent's worktree serves the implementation of the convergence fix."),
            (9, format!("Edit: {wt}/tests/fixtures/stack-deploy-swarm.json"), "This test-fixture edit in the fix agent's worktree adds slots to mocked tasks, part of the D15 fix."),
            (9, format!("Edit: {wt}/tests/stack-deploy-tests.sh"), "Editing the docker mock in the stack-deploy test script, inside the fix agent's worktree, is part of the D15 fix."),
            (9, format!("Write: {wt}/tests/fixtures/nodes.json"), "Adding a nodes.json mock to the stack-deploy test fixture in the fix agent's worktree is part of the D15 fix."),
            (9, format!("Edit: {wt}/tests/stack-deploy-tests.sh"), "This edits only a comment in a test script inside the fix agent's worktree, as part of the D15 fix."),
            (10, format!("Edit: {wt}/tests/stack-deploy-tests.sh"), "This test-script edit inside the fix agent's worktree is part of the D15 implementation."),
            (10, format!("Bash: cd {wt} && ./tests/stack-deploy-tests.sh"), "Running the repo's own stack-deploy test suite in the fix agent's worktree is a local check."),
            (13, format!("Bash: sed -i 's/deploy_unit/unit/' {wt}/tests/deploy-preview-tests.sh"), "A local sed edit to a test script in the fix agent's worktree, part of the D15 fix."),
            (13, format!("Edit: {wt}/tests/deploy-preview-tests.sh"), "Adding the placement-only test cases to deploy-preview-tests.sh in the fix agent's worktree is part of the D15 fix."),
            (14, format!("Bash: {wt}/tests/deploy-preview-tests.sh"), "Running the deploy-preview test suite locally in the fix agent's worktree is a read-only check."),
            (15, format!("Edit: {wt}/docs/DEPLOYS.md"), "The edit documents the D15 placement-only deploy-unit fix in DEPLOYS.md, inside the fix agent's worktree."),
            (15, format!("Bash: cd {wt} && bash -n scripts/stack-deploy.sh && ./tests/run-all.sh"), "Running the session's own test-suite script in the fix agent's worktree is a local, read-only check."),
        ];
        let mut rows: Vec<BossDecision> = steps
            .iter()
            .map(|(m, request, why)| allowed(9, *m, request, why))
            .collect();
        rows.push(allowed(
            9,
            16,
            "Bash: gh pr merge 680 --squash",
            "The person said to get it all done, and #680 passed its review and CI.",
        ));
        rows
    }

    fn name(_: Uuid) -> Option<String> {
        Some("Billing Migration".into())
    }

    /// What runs is named by its words: a script, a workflow or a target
    /// that deploys releases, unless its name says it tests or checks.
    #[test]
    fn a_step_in_what_runs_is_read_by_its_words() {
        use ActionKind::Approve;
        for (request, step) in [
            ("Bash: ./deploy-prod.sh", Important::Released),
            ("Bash: ./scripts/deploy_prod.sh eu", Important::Released),
            ("Bash: bash release-web.sh", Important::Released),
            ("Bash: make deploy-prod", Important::Released),
            ("Bash: make -C infra deploy-prod", Important::Released),
            ("Bash: npm run deploy:prod", Important::Released),
            ("Bash: pnpm run release:web", Important::Released),
            ("Bash: just release", Important::Released),
            ("Bash: just deploy-prod", Important::Released),
            (
                "Bash: gh workflow run \"Deploy signal\"",
                Important::Released,
            ),
            (
                "Bash: gh workflow run 'Release web' -f ref=main",
                Important::Released,
            ),
            ("Bash: gh workflow run deploy-prod.yml", Important::Released),
            ("Bash: pnpm deploy", Important::Released),
            ("Bash: cargo release", Important::Released),
            ("Bash: npx release-it", Important::Released),
            ("Bash: pnpm --filter web deploy", Important::Released),
            ("Bash: make -f deploy.mk deploy-prod", Important::Released),
            ("Bash: make drop-db", Important::Deleted),
            ("Bash: gh release delete v1.2.0 --yes", Important::Deleted),
            ("Bash: glab release delete v1.2.0", Important::Deleted),
        ] {
            assert_eq!(important(Approve, request), Some(step), "{request}");
        }
        // A name that says it tests or checks is not a release.
        for request in [
            "Edit: /repo/tests/stack-deploy-tests.sh",
            "Bash: bash deploy-preview-tests.sh",
            "Bash: ./tests/stack-deploy-tests.sh",
            "Bash: ./scripts/deploy-preview.sh prod",
            "Bash: ./deploy-check.sh",
            "Bash: make deploy-lint",
            "Bash: npm run release:dry-run",
            "Bash: npm run test:deploy",
            "Bash: just deploy-spec",
            "Bash: gh workflow run \"Deploy preview\"",
            "Bash: gh workflow run release-plan.yml",
            // Words given to other programs are read whole.
            "Bash: docker run my-release-image",
            "Bash: git push -u origin release-1.2",
            // A package installed or a workspace named is not a target.
            "Bash: npm install release-it",
            "Bash: pnpm add semantic-release",
            "Bash: npm uninstall release-it",
            "Bash: pnpm --filter deploy-web test",
            "Bash: pnpm --filter deploy-web build",
            "Bash: pnpm -F release-notes build",
            "Bash: make release-notes",
            "Bash: ./deploy-status.sh",
        ] {
            assert_eq!(important(Approve, request), None, "{request}");
        }
    }

    /// Steps named "deploy" only in their files are ordinary; what a
    /// command runs is what counts.
    #[test]
    fn an_approval_is_judged_by_what_it_runs_never_by_a_files_name() {
        use ActionKind::Approve;
        for request in [
            "Edit: /repo/tests/stack-deploy-tests.sh",
            "Write: /repo/docs/DEPLOYS.md",
            "Read: /repo/.github/workflows/promote.yml",
            "Bash: ./tests/stack-deploy-tests.sh",
            "Bash: ./scripts/deploy-preview.sh prod",
            "Bash: grep -rn deploy scripts/",
            "Bash: git fetch origin && cat .github/workflows/promote.yml",
            "Bash: git commit -m \"Merge the deploy fixture fix\"",
            "Bash: shellcheck scripts/deploy.sh",
            "Bash: docker run --rm alpine true",
            "Bash: git commit -m \"$(cat <<'EOF'\nMerge the deploy fixture fix\n\nDrop the old tag.\nEOF\n)\"",
            "Bash: gh pr create --title \"Fix it\" --body \"$(cat <<'EOF'\nRelease notes: merge after deploy.\nEOF\n)\"",
            "Bash: git commit -m \"Fix $X deploy\"",
            "Bash: git commit -am \"Merge the deploy fix\"",
            "Bash: git diff -- scripts/deploy.sh",
            "Bash: git show HEAD:scripts/deploy.sh",
            "Bash: git grep deploy",
            "Bash: git -C /repo/.worktrees/release log --oneline",
            "Bash: cd /repo/.worktrees/release && ls",
            "Bash: npx vitest run src/merge.test.ts",
            "Bash: cargo test -- merge",
            "Bash: bash -n scripts/deploy.sh",
            "Bash: git commit -F - <<'EOF'\nMerge the deploy fixture fix\nRemove the old tag.\nEOF",
            "Bash: cd \"$REPO\" && git commit -am 'Fix `deploy` and `rm` handling'",
            "Bash: cd \"$REPO\" && git commit -am \"Fix \\`deploy\\` step\"",
            "Bash: cd $R && git commit -m 'note $(release) text'",
            "Bash: # build then deploy later\nmake build $X",
            "Bash: cargo test -p desk deploy",
            "Bash: cd $HOME/x && cargo test -p desk deploy 2>&1 | tail",
            "Bash: pytest -k deploy tests/",
            "Bash: python -m pytest -k deploy",
            "Bash: uv run pytest -k merge",
            "Bash: pnpm vitest deploy",
            "Bash: gh release list",
            "Bash: gh release view v1.2.0",
            "Bash: gh pr list --state merged",
            "Bash: gh run list -w deploy",
            "Bash: gh workflow view deploy",
            "Bash: kubectl get deploy",
            "Bash: kubectl describe deploy web",
            "Bash: docker compose logs deploy",
            "Bash: git tag -l",
            "Bash: git tag --list 'v*'",
            "Bash: git checkout release",
            "Bash: npx playwright test release",
            "Bash: ./scripts/run-tests.sh --filter release",
            "Bash: find . -type d -name release",
            "Bash: gh release download v1.2.0",
            "Bash: git push -u origin release",
            "Bash: git commit -n -m 'Release the deploy fix'",
            "Bash: ./tests/stack-deploy-tests.sh $(date +%s)",
            "Bash: git commit -m \"Release the deploy fix\" && echo $(git rev-parse HEAD)",
        ] {
            assert_eq!(important(Approve, request), None, "{request}");
        }
        for (request, step) in [
            ("Bash: gh pr merge 680 --squash", Important::Merged),
            ("Bash: ./scripts/deploy.sh prod", Important::Released),
            (
                "Bash: gh workflow run promote.yml -f ref=main",
                Important::Released,
            ),
            (
                "Bash: docker stack deploy -c stack.yml web",
                Important::Released,
            ),
            ("Bash: rm -rf target/", Important::Deleted),
            ("Bash: bash scripts/deploy.sh prod", Important::Released),
            ("Bash: git -C /repo merge origin/main", Important::Merged),
            (
                "Bash: git commit -m \"$(cat <<'EOF'\nFix the fixture\nEOF\n)\" && gh pr merge 12 --squash",
                Important::Merged,
            ),
            ("Bash: cd $WT && ./scripts/deploy.sh prod", Important::Released),
            ("Bash: ssh prod <<'EOF'\ndocker stack deploy -c s.yml web\nEOF", Important::Released),
            ("Bash: ssh prod \"docker stack deploy -c s.yml $STACK\"", Important::Released),
            ("Bash: ssh host 'cd /srv && ./deploy.sh'", Important::Released),
            ("Bash: psql -c \"DROP TABLE users\" $(echo db)", Important::Deleted),
            ("Bash: env FOO=1 ./deploy.sh", Important::Released),
            ("Bash: gh release create v1.2.0", Important::Released),
            ("Bash: git tag v1.2.0", Important::Released),
            ("Bash: kubectl -n web delete deploy web", Important::Deleted),
            ("Bash: git push origin --delete old", Important::Deleted),
            ("Bash: xargs -I {} rm {}", Important::Deleted),
            ("Bash: npm run deploy", Important::Released),
            ("Bash: rm -rf logs", Important::Deleted),
            ("Bash: rm -rf images", Important::Deleted),
            ("Bash: kubectl --context prod -n x delete pod y", Important::Deleted),
            ("Bash: kubectl -n x --context prod delete deploy y", Important::Deleted),
            ("Bash: docker --context prod stack deploy -c s.yml web", Important::Released),
            ("Bash: psql -U postgres -d app -c 'DROP TABLE users'", Important::Deleted),
            ("Bash: gh workflow run Deploy", Important::Released),
            ("Bash: gh workflow run -R a/b deploy.yml", Important::Released),
            ("Bash: ssh -i k.pem prod ./deploy.sh", Important::Released),
            ("Bash: git push -d origin old", Important::Deleted),
            ("Bash: git push origin :old", Important::Deleted),
            ("Bash: sudo -u svc ./deploy.sh", Important::Released),
            ("Bash: timeout 600 ./deploy.sh", Important::Released),
            (
                "Bash: gh pr comment 5 --body \"$(echo \"Don't merge\")\" && gh pr merge 6",
                Important::Merged,
            ),
            ("Bash: cd \"$REPO\"\n# it's green now\ngh pr merge 9 --squash", Important::Merged),
            ("Bash: out=\"$(gh pr merge 12 --squash)\"", Important::Merged),
            ("Bash: echo \"done: $(gh pr merge 12)\"", Important::Merged),
            (
                "Bash: git commit -m \"say \\\"it's $X\\\"\" && gh pr merge 12",
                Important::Merged,
            ),
            ("Bash: echo \\\"$X && gh pr merge 12", Important::Merged),
            (
                "Bash: python3 -c \"print(1<<4)\" && echo $HOME\ngh pr merge 12",
                Important::Merged,
            ),
            (
                "Bash: cat <<'END-MSG' > f\nhi\nEND-MSG\ngh pr merge 12",
                Important::Merged,
            ),
            (
                "Bash: x=\"$(cat <<'EOF'\nhi\nEOF)\" && gh pr merge 12",
                Important::Merged,
            ),
            ("Bash: PR=$(gh pr list --head x -q .[0].number) && gh pr merge $PR", Important::Merged),
            ("Bash: cd \"$(git rev-parse --show-toplevel)\" && rm -rf build", Important::Deleted),
            (
                "Bash: find /tmp/x -name '*.log' -delete",
                Important::Deleted,
            ),
            ("Bash: psql -c 'DROP TABLE runs'", Important::Deleted),
            ("github · merge_pull_request", Important::Merged),
        ] {
            assert_eq!(important(Approve, request), Some(step), "{request}");
        }
        // However deep its expansions nest, a command is read without
        // running out of stack.
        let deep = format!("Bash: echo \"{}x{}\"", "$(".repeat(5000), ")".repeat(5000));
        assert_eq!(important(Approve, &deep), None);
        let hops = format!("Bash: {}true", "ssh h ".repeat(5000));
        assert_eq!(important(Approve, &hops), None);
        // A message naming the merge queue or a dry run sets nothing to
        // happen; one that queues the release does.
        use ActionKind::Message;
        assert_eq!(
            important(
                Message,
                "Check the merge-queue state now. No real release until I reply."
            ),
            None
        );
        assert_eq!(
            important(
                Message,
                "Watch it through the merge queue, then run the promote dry run."
            ),
            None
        );
        assert_eq!(
            important(Message, "Once CI is green, run the real release."),
            Some(Important::Later)
        );
        assert_eq!(important(Message, "No, merge it."), Some(Important::Merged));
        assert_eq!(
            important(Message, "Looks good, no blockers. Merge it."),
            Some(Important::Merged)
        );
    }

    /// The 10:00 roundup: the merge listed on its own under "Decided
    /// without you", the sixteen ordinary steps one line behind "16
    /// routine things · Show", each of them behind that line.
    #[test]
    fn an_hour_of_ordinary_approvals_is_one_routine_line_and_the_merge_is_listed() {
        let rows = the_hour();
        let tz = chrono_tz::Europe::London;
        let t = told(&rows, Decided::Important, &name, &tz);
        assert_eq!(t.listed.len(), 1, "{:?}", t.listed);
        assert_eq!(t.listed[0].important, Some(Important::Merged));
        assert!(t.listed_runs.is_empty());
        assert_eq!(t.routine.len(), 1, "one session, one kind: one line");
        assert_eq!(t.routine_count(), 16);
        let run = &t.routine[0];
        assert_eq!(
            run.line(),
            "Allowed Billing Migration 16 times, 09:05–09:15: test and fixture edits, \
             local test runs, and other steps, all in its worktree."
        );
        let lines: Vec<DecisionLine> = rows.iter().map(BossDecision::line).collect();
        let link = SessionLink {
            session_id: FIX.into(),
            name: "Billing Migration".into(),
            closed: false,
        };
        let said = run.said(&lines, Some(link.clone()));
        assert_eq!(said.kind, EventKind::Routine);
        assert_eq!(said.session, Some(link));
        assert_eq!(said.did.len(), 16, "the whole list, behind Show all");
        assert!(said.did[0].at > said.did[15].at, "the newest first");

        // Every one, as the person asked: the run is listed too, still one
        // line; with decisions only on asking, the merge joins the routine.
        let every = told(&rows, Decided::All, &name, &tz);
        assert_eq!((every.listed.len(), every.listed_runs.len()), (1, 1));
        assert_eq!(every.routine_count(), 0);
        let asked = told(&rows, Decided::Ask, &name, &tz);
        assert!(asked.listed.is_empty());
        assert_eq!(asked.routine_count(), 17);
    }

    /// An hour like the 17:00 roundup of 17 Nov, its names changed: five
    /// sessions, most of what the boss decided messages about merging
    /// later, a few stops and a restart in manual mode, and the routine.
    fn the_busy_hour() -> (Vec<BossDecision>, Vec<(Uuid, &'static str)>) {
        use crate::decision::Undo;
        let billing: Uuid = FIX.parse().unwrap();
        let leads = Uuid::from_u128(2);
        let ledger = Uuid::from_u128(3);
        let lockdown = Uuid::from_u128(4);
        let headers = Uuid::from_u128(5);
        let review = Uuid::from_u128(6);
        let names = vec![
            (billing, "Billing Migration"),
            (leads, "Lead Fix"),
            (ledger, "Ledger Tidy"),
            (lockdown, "Access Lockdown"),
            (headers, "Access Headers"),
            (review, "Review PRs"),
        ];
        let name_of = |s: Uuid| names.iter().find(|(id, _)| *id == s).unwrap().1;
        let told = |s: Uuid, m: u32, text: &str, tag: Option<Important>| {
            let mut d = BossDecision::new(
                "desk",
                Need::Approval,
                &format!("told {} “{text}”", name_of(s)),
                "It keeps the plan the person set.",
                How::Own,
            );
            d.at = at(17, m);
            d.session = Some(s);
            d.message = Some(text.into());
            d.brief = format!("Sent {} a message.", name_of(s));
            d.action = serde_json::json!({
                "kind": "line", "session": s, "message": Uuid::new_v4(), "text": text,
            });
            d.undo = Undo::Withdraw;
            d.important = tag;
            d
        };
        let did = |s: Uuid, m: u32, what: &str, tag: Option<Important>| {
            let mut d = BossDecision::new(
                "desk",
                Need::Approval,
                &format!("{what} {}", name_of(s)),
                "The person asked for it.",
                How::Own,
            );
            d.at = at(17, m);
            d.session = Some(s);
            d.important = tag;
            if what == "stopped" {
                d.action = serde_json::json!({ "kind": "resume", "session": s });
                d.undo = Undo::Reverse;
            }
            d
        };
        let alone = |s: Uuid, m: u32| {
            let mut d = BossDecision::new(
                "desk",
                Need::Approval,
                &format!("sent {} nothing", name_of(s)),
                "",
                How::Own,
            );
            d.kind = DecisionKind::LeftAlone;
            d.at = at(17, m);
            d.session = Some(s);
            d
        };
        use Important::{Later, Merged, Overrode};
        let mut rows = vec![
            told(
                billing,
                5,
                "Wait for the #1 re-review, then merge it.",
                Some(Merged),
            ),
            did(ledger, 17, "allowed", None),
            did(ledger, 18, "allowed", None),
            told(
                billing,
                19,
                "Wait for the re-review, then merge.",
                Some(Merged),
            ),
            did(ledger, 20, "allowed", None),
            did(ledger, 21, "allowed", None),
            told(ledger, 25, "#2 merged; carry on.", Some(Later)),
            told(billing, 25, "Yes, flip it once CI is green.", Some(Later)),
            told(billing, 26, "The person said yes.", Some(Later)),
            alone(billing, 29),
            did(ledger, 29, "allowed", None),
            told(billing, 31, "Watch the checks to green.", Some(Later)),
            told(
                billing,
                31,
                "Pause the watcher before it queues.",
                Some(Later),
            ),
            told(leads, 32, "Fix the leads issue first.", Some(Later)),
            told(billing, 32, "No, don't merge yet.", Some(Merged)),
            did(lockdown, 33, "stopped", Some(Overrode)),
            did(ledger, 34, "stopped", Some(Overrode)),
            told(leads, 35, "Fix it now, in this order.", Some(Later)),
            told(
                billing,
                35,
                "While it waits, merge the other.",
                Some(Merged),
            ),
            did(leads, 37, "restarted", None),
            told(billing, 38, "Yes, queue it now.", Some(Later)),
            told(leads, 38, "Good report, carry on.", None),
            did(review, 39, "started", None),
            told(billing, 42, "It merged; next.", Some(Later)),
            told(leads, 46, "A reminder about order.", None),
            told(billing, 47, "All four are green.", Some(Later)),
            told(review, 48, "Wait for both reviewers.", None),
            told(billing, 48, "Keep the design's order.", Some(Later)),
            told(leads, 49, "Yes, clear more space.", Some(Later)),
            alone(leads, 50),
            told(leads, 51, "It is mergeable once CI is green.", Some(Later)),
            told(headers, 51, "Merge it once its CI is green.", Some(Later)),
            told(leads, 52, "Yes, dispatch one run.", Some(Later)),
            told(leads, 52, "The person said yes.", Some(Later)),
            did(billing, 56, "started again in manual mode", Some(Overrode)),
            told(billing, 56, "No need to run the clean-up.", Some(Later)),
            did(billing, 56, "allowed", None),
            told(leads, 56, "Still waiting on CI.", Some(Later)),
            told(headers, 59, "Yes, fix it, then merge.", Some(Later)),
            alone(review, 59),
        ];
        // The restart in manual mode overrode it without a plan to undo.
        rows.iter_mut()
            .filter(|d| d.what.starts_with("started again"))
            .for_each(|d| d.what = "started Billing Migration again in manual mode".into());
        (rows, names)
    }

    /// "Decided without you" is one entry a session: what did something
    /// that matters first, then the sessions only told what to do later,
    /// each a line of its count and when, its decisions under it without
    /// its name again, the notable ones always shown; the headline counts
    /// every decision.
    #[test]
    fn what_was_decided_without_you_is_one_entry_a_session() {
        use crate::update_cadence::{render, shown, Roundup, SectionKey};
        let (rows, names) = the_busy_hour();
        let tz = chrono_tz::UTC;
        let name = |s: Uuid| {
            names
                .iter()
                .find(|(id, _)| *id == s)
                .map(|(_, n)| n.to_string())
        };
        let t = told(&rows, Decided::Important, &name, &tz);
        assert_eq!(t.listed.len(), 26, "every important one is listed");
        let entries = by_session(t.listed.iter().copied(), &name);
        let order: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            order,
            [
                "Billing Migration",
                "Ledger Tidy",
                "Access Lockdown",
                "Access Headers",
                "Lead Fix",
            ],
            "what did something that matters first, the latest first"
        );
        let lines: Vec<DecisionLine> = rows.iter().map(BossDecision::line).collect();
        let said: Vec<Said> = entries.iter().map(|e| e.said(&lines, None, &tz)).collect();
        let billing = &said[0];
        assert_eq!(
            billing.headline,
            "Billing Migration · 14 decisions, 17:05–17:56"
        );
        assert_eq!(billing.decisions, Some(14));
        assert_eq!(
            billing.kind,
            EventKind::Routine,
            "an older Desk lists it as a run"
        );
        assert_eq!(billing.did.len(), 14);
        assert!(billing.did[0].at >= billing.did[13].at, "the newest first");
        assert!(
            billing
                .did
                .iter()
                .all(|i| !i.action.contains("Billing Migration")),
            "{:?}",
            billing.did
        );
        let actions: Vec<&str> = billing.did.iter().map(|i| i.action.as_str()).collect();
        assert!(actions.contains(&"Sent it a message"), "{actions:?}");
        // The restart in manual mode is notable; the messages about
        // merging, however tagged, are not.
        let notable: Vec<&str> = billing
            .did
            .iter()
            .filter(|i| i.notable)
            .map(|i| i.action.as_str())
            .collect();
        assert_eq!(notable, ["Started it again in manual mode"]);
        let (seen, more) = shown(&billing.did);
        assert_eq!((seen.len(), more), (4, 10));
        assert!(seen.iter().any(|i| i.notable));
        // Every stop has its Undo.
        let ledger = &said[1];
        assert_eq!(ledger.headline, "Ledger Tidy · 2 decisions, 17:25–17:34");
        assert!(
            ledger.did.iter().all(|i| i.undo.is_some()),
            "{:?}",
            ledger.did
        );
        assert_eq!(said[2].headline, "Access Lockdown · 1 decision at 17:33");
        assert_eq!(said[4].headline, "Lead Fix · 7 decisions, 17:32–17:56");

        // The headline counts each decision, not each entry.
        let r = Roundup::of(
            at(17, 0),
            said.iter()
                .map(|s| (SectionKey::Decided, s.clone()))
                .collect(),
            t.routine_count(),
            vec![],
        );
        assert_eq!(r.sections[0].lines.len(), 5);
        assert_eq!(r.plain_headline(), "26 decided without you.");
        let text = render("26 decided without you.", &r);
        assert!(
            text.contains("- **Billing Migration · 14 decisions"),
            "{text}"
        );
        assert!(text.contains("  - 10 more"), "{text}");
        assert!(
            text.contains("  - Started it again in manual mode"),
            "{text}"
        );

        // Every one, as the person asked: the routine ones join their
        // session's entry, folded; still one entry a session.
        let every = told(&rows, Decided::All, &name, &tz);
        let listed = every.listed.iter().copied().chain(
            every
                .listed_runs
                .iter()
                .flat_map(|r| r.rows.iter().copied()),
        );
        let entries = by_session(listed, &name);
        assert_eq!(entries.len(), 6, "Review PRs too, last");
        assert_eq!(entries[5].name, "Review PRs");
        let ledger = entries[1].said(&lines, None, &tz);
        assert_eq!(ledger.decisions, Some(7));
        assert!(
            ledger.did.iter().any(|i| i.folded == Some(5)),
            "{:?}",
            ledger.did
        );
    }

    /// The reasons of what was allowed are said as themes, never in their
    /// own words; with no theme, the line is a count.
    #[test]
    fn what_was_allowed_is_said_by_theme_or_counted() {
        assert_eq!(
            themes([
                "Editing a test fixture in its worktree.",
                "Adding a mock to the test script in its worktree.",
                "Running the test suite in its worktree.",
            ]),
            Some("test and fixture edits, and local test runs, all in its worktree".into())
        );
        assert_eq!(themes(["Kingsley said so."]), None);
        assert_eq!(
            themes(["A read-only look at the logs."]),
            Some("read-only checks".into())
        );
        let mut d = allowed(9, 5, "Bash: ls", "Kingsley said so.");
        d.session = None;
        let r = runs([&d], &|_| None, &chrono_tz::UTC);
        assert_eq!(r[0].line(), "Allowed Billing Migration at 09:05.");
    }

    /// A message right away folds a run of routine approvals into one line
    /// of "What I did", each behind its "more"; what can be undone and
    /// what is important stay their own.
    #[test]
    fn a_run_of_routine_approvals_is_one_line_of_what_i_did() {
        let rows = the_hour();
        let lines: Vec<DecisionLine> = rows.iter().map(BossDecision::line).collect();
        let mut items: Vec<BriefItem> = rows
            .iter()
            .zip(&lines)
            .map(|(d, l)| crate::update::brief_item(d, l))
            .collect();
        items.reverse();
        let out = folded(items, &rows, &chrono_tz::UTC);
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[0].action, rows[16].brief().trim_end_matches('.'));
        assert_eq!(
            out[1].action,
            "Allowed Billing Migration 16 times, 09:05–09:15"
        );
        assert_eq!(
            out[1].detail.as_deref().map(|d| d.lines().count()),
            Some(16)
        );
        // "What I did" counts each decision a folded line stands for.
        let counted: u32 = out.iter().map(|i| i.folded.unwrap_or(1)).sum();
        assert_eq!(
            (out[0].folded, out[1].folded, counted),
            (None, Some(16), 17)
        );
    }
}
