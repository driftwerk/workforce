//! A boss's open orders to its sessions (designs/boss-orders-and-serial-turns.md
//! §3): every instruction it gives a session is an **assignment**, kept as a
//! row (`boss_assignments`) until it is done, cancelled, superseded or let
//! lapse, so Desk knows who holds which job. Called an assignment in the
//! boss's words because "standing orders" are the person's own.
//!
//! This module reads and decides, purely:
//! - what an instruction names ([`targets`]: a release, a PR, a commit, a
//!   run, a workflow file, an environment) and whether it starts something
//!   that can't be taken back ([`irreversible`]);
//! - the job it is, only ever as the boss declares it with the order
//!   ([`Job`]: kind, target repository, key), never from its words;
//! - whether the words name a job another session holds, so the boss is
//!   asked to confirm ([`looks_like`]: a backstop that never claims);
//! - whether it may go, against the boss's live assignments ([`rule`]):
//!   the same session told to start what it already has (R1), or a second
//!   session given a job another holds (R2, and R3 by keys in one form).
//!
//! [`crate::assignment_store`] keeps the rows and checks the rules under
//! the boss's lock; the hub's `desk_crew` sends, holds and releases.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::text_enum;

text_enum!(AssignmentState {
    // Reserved, its line message not sent yet.
    Sending => "sending",
    Open => "open",
    // Waiting for its old holder to answer a stand-down.
    Held => "held",
    Done => "done",
    Cancelled => "cancelled",
    // A newer order to the same session for the same job took its place.
    Superseded => "superseded",
    // Reversible, and nothing came of it for [`LAPSE`].
    Lapsed => "lapsed",
    // Its line message was refused, or never went.
    Failed => "failed",
});

impl AssignmentState {
    /// Whether it still holds its job: sending, open or held.
    pub fn live(self) -> bool {
        matches!(self, Self::Sending | Self::Open | Self::Held)
    }
}

/// How long a reversible assignment stays open with nothing closing it.
pub const LAPSE: chrono::TimeDelta = chrono::TimeDelta::hours(12);

/// From how old an irreversible assignment is said to be old (it never
/// lapses on its own).
pub const OLD: chrono::TimeDelta = chrono::TimeDelta::hours(6);

/// How an assignment's id is shown: `a:` and its first eight characters.
pub fn short(id: Uuid) -> String {
    format!("a:{}", &id.simple().to_string()[..8])
}

/// The assignment `given` names, among `ids`: its whole id, or its short
/// form (`a:3f2a1b2c`, or the eight characters bare).
pub fn named(given: &str, ids: &[Uuid]) -> Option<Uuid> {
    let t = given.trim();
    let t = t.strip_prefix("a:").unwrap_or(t).trim();
    if let Ok(id) = t.parse::<Uuid>() {
        return ids.contains(&id).then_some(id);
    }
    let hex: String = t.chars().filter(|c| *c != '-').collect::<String>();
    if hex.len() < 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let hex = hex.to_ascii_lowercase();
    let mut found = ids
        .iter()
        .filter(|id| id.simple().to_string().starts_with(&hex));
    match (found.next(), found.next()) {
        (Some(id), None) => Some(*id),
        _ => None,
    }
}

/// One assignment, as kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Assignment {
    pub id: Uuid,
    /// The boss's agent id, or `<hub>/<agent>` for a boss served here.
    pub boss: String,
    pub session: Uuid,
    /// The machine the session is on, when it isn't this one.
    pub machine: Option<String>,
    /// The line message that carries it.
    pub line_message: Option<Uuid>,
    pub text: String,
    /// The job as the boss named it.
    pub job: Option<String>,
    /// The job as one key ([`job_key`]); none when nothing names it.
    pub job_key: Option<String>,
    /// The repository of the session's folder, by its origin.
    pub repo: Option<String>,
    /// What it names ([`Target::key`]).
    pub targets: Vec<String>,
    pub irreversible: bool,
    pub urgent: bool,
    /// Whether it holds its job against every other session: irreversible,
    /// or a job the boss named.
    pub claims: bool,
    pub state: AssignmentState,
    pub sent_at: DateTime<Utc>,
    pub delivered_at: Option<DateTime<Utc>>,
    /// When the session answered it (a reply, or a message quoting its mark).
    pub acked_at: Option<DateTime<Utc>>,
    pub closed_at: Option<DateTime<Utc>>,
    pub closed_by: Option<String>,
    pub closed_why: Option<String>,
    /// The assignment it took over from.
    pub replaces: Option<Uuid>,
    /// The boss's turn that sent it.
    pub turn: Option<String>,
    /// The person's order it serves.
    pub item: Option<Uuid>,
    /// While held: the stand-down its old holder is to answer.
    pub stand_down: Option<Uuid>,
    /// The old holder's answer to that stand-down.
    pub answer: Option<String>,
    /// While held: the crew action to send when it is released, as
    /// `{ "tool", "input" }` (a suggestion's form).
    #[serde(skip)]
    pub action: Option<serde_json::Value>,
    /// While held: the picture's number it was decided on.
    #[serde(skip)]
    pub basis: Option<i64>,
}

impl Assignment {
    /// A new assignment, now, about to be sent.
    pub fn new(boss: &str, session: Uuid, text: &str) -> Self {
        Self {
            id: Uuid::new_v4(),
            boss: boss.to_string(),
            session,
            machine: None,
            line_message: None,
            text: text.trim().to_string(),
            job: None,
            job_key: None,
            repo: None,
            targets: Vec::new(),
            irreversible: false,
            urgent: false,
            claims: false,
            state: AssignmentState::Sending,
            sent_at: crate::crew::now(),
            delivered_at: None,
            acked_at: None,
            closed_at: None,
            closed_by: None,
            closed_why: None,
            replaces: None,
            turn: None,
            item: None,
            stand_down: None,
            answer: None,
            action: None,
            basis: None,
        }
    }

    /// What it is about in a few words: the job the boss named, else its
    /// strongest target, else its first words.
    pub fn about(&self) -> String {
        if let Some(j) = self.job.as_deref().filter(|j| !j.trim().is_empty()) {
            return j.trim().to_string();
        }
        if let Some(t) = strongest(&self.parsed_targets()) {
            return t.said();
        }
        crate::crew::one_line(&self.text, 60)
    }

    /// Its targets, read back.
    pub fn parsed_targets(&self) -> Vec<Target> {
        self.targets
            .iter()
            .filter_map(|t| Target::parse(t))
            .collect()
    }
}

/// What kind of thing an instruction names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TargetKind {
    Release,
    Pr,
    Workflow,
    Commit,
    Run,
    Environment,
}

impl TargetKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::Pr => "pr",
            Self::Workflow => "workflow",
            Self::Commit => "commit",
            Self::Run => "run",
            Self::Environment => "env",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "release" => Self::Release,
            "pr" => Self::Pr,
            "workflow" => Self::Workflow,
            "commit" => Self::Commit,
            "run" => Self::Run,
            "env" => Self::Environment,
            _ => return None,
        })
    }
}

/// One thing an instruction names.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Target {
    pub kind: TargetKind,
    pub id: String,
}

impl Target {
    fn new(kind: TargetKind, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: id.into(),
        }
    }

    /// As kept: `release:4`, `pr:708`, `env:production`.
    pub fn key(&self) -> String {
        format!("{}:{}", self.kind.as_str(), self.id)
    }

    pub fn parse(key: &str) -> Option<Self> {
        let (kind, id) = key.split_once(':')?;
        Some(Self::new(TargetKind::parse(kind)?, id))
    }

    /// As the person reads it: "release 4", "#708", "promote.yml".
    pub fn said(&self) -> String {
        match self.kind {
            TargetKind::Release => format!("release {}", self.id),
            TargetKind::Pr => format!("#{}", self.id),
            TargetKind::Workflow => self.id.clone(),
            TargetKind::Commit => format!("commit {}", &self.id[..self.id.len().min(10)]),
            TargetKind::Run => format!("run {}", self.id),
            TargetKind::Environment => self.id.clone(),
        }
    }
}

/// The strongest of `targets` to key a job by: a release, then a PR, a
/// workflow, a commit, a run; never an environment.
pub fn strongest(targets: &[Target]) -> Option<&Target> {
    targets
        .iter()
        .filter(|t| t.kind != TargetKind::Environment)
        .min_by_key(|t| t.kind)
}

/// A word of an instruction, with the sentence and clause it is in.
#[derive(Debug, Clone)]
struct Word {
    text: String,
    sentence: usize,
    clause: usize,
}

/// `text` as lower-case words. A sentence ends at `.`, `!`, `?` or a line
/// break; a clause also at `,`, `;`, `:` and dashes. A `.` inside a word
/// (`promote.yml`) is kept; an apostrophe splits (`don't` is `don`, `t`).
fn words(text: &str) -> Vec<Word> {
    let lower = text.to_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    let mut out = Vec::new();
    let (mut sentence, mut clause) = (0usize, 0usize);
    let mut cur = String::new();
    let push = |cur: &mut String, out: &mut Vec<Word>, sentence: usize, clause: usize| {
        if !cur.is_empty() {
            out.push(Word {
                text: std::mem::take(cur),
                sentence,
                clause,
            });
        }
    };
    for (i, &c) in chars.iter().enumerate() {
        let next = chars.get(i + 1).copied();
        let ends_here = next.is_none_or(char::is_whitespace);
        match c {
            '.' | '!' | '?' if ends_here => {
                push(&mut cur, &mut out, sentence, clause);
                sentence += 1;
                clause += 1;
            }
            '\n' => {
                push(&mut cur, &mut out, sentence, clause);
                sentence += 1;
                clause += 1;
            }
            ',' | ';' | '—' | '–' => {
                push(&mut cur, &mut out, sentence, clause);
                clause += 1;
            }
            ':' if ends_here => {
                push(&mut cur, &mut out, sentence, clause);
                clause += 1;
            }
            '\'' | '’' | '‘' | '"' | '“' | '”' | '(' | ')' | '[' | ']' | '{' | '}' | '`' | '*' => {
                push(&mut cur, &mut out, sentence, clause)
            }
            c if c.is_whitespace() => push(&mut cur, &mut out, sentence, clause),
            c => cur.push(c),
        }
    }
    push(&mut cur, &mut out, sentence, clause);
    for w in &mut out {
        let t = w
            .text
            .trim_matches(|c: char| matches!(c, '.' | ',' | '!' | '?'));
        w.text = t.to_string();
    }
    out.retain(|w| !w.text.is_empty());
    out
}

/// Whether `w` is one of Desk's own marks, never a commit: a line
/// message's `m:…`, an assignment's `a:…`, `[Desk m:…]`, or a whole id.
fn mark(w: &str) -> bool {
    w.starts_with("m:") || w.starts_with("a:") || w.parse::<Uuid>().is_ok()
}

fn digits(w: &str) -> bool {
    !w.is_empty() && w.chars().all(|c| c.is_ascii_digit())
}

/// Whether a run of digits reads as a date or a time stamp (`20261118`,
/// `202611181230`), not a run id.
fn date_like(w: &str) -> bool {
    if w.len() < 8 || w.len() > 14 || !w.starts_with("20") {
        return false;
    }
    let month: u32 = w[4..6].parse().unwrap_or(0);
    let day: u32 = w[6..8].parse().unwrap_or(0);
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

/// A commit: 7 to 40 hex characters with at least one letter a–f and one
/// digit (an English word of hex letters, "defaced", is none).
fn commit_like(w: &str) -> bool {
    (7..=40).contains(&w.len())
        && w.chars().all(|c| c.is_ascii_hexdigit())
        && w.chars().any(|c| c.is_ascii_alphabetic())
        && w.chars().any(|c| c.is_ascii_digit())
}

fn environment(w: &str) -> Option<&'static str> {
    Some(match w {
        "prod" | "production" => "production",
        "staging" | "stage" => "staging",
        "demo" => "demo",
        "dev" | "development" => "dev",
        _ => return None,
    })
}

/// A version as released: `1.4.0`, `v4.2` (its `v` dropped).
fn version(w: &str) -> Option<&str> {
    let v = w.strip_prefix('v').unwrap_or(w);
    let parts: Vec<&str> = v.split('.').collect();
    ((2..=4).contains(&parts.len()) && parts.iter().all(|p| digits(p))).then_some(v)
}

/// A number or a version: what follows "release" when it names one.
fn numberish(w: &str) -> bool {
    digits(w) || version(w).is_some()
}

/// A release number as said after "release": `4`, `4,`, `#4`, `1.4.0`.
fn release_number(w: &str) -> Option<String> {
    let w = w.strip_prefix('#').unwrap_or(w);
    if let Some(v) = version(w) {
        return Some(v.to_string());
    }
    digits(w)
        .then(|| w.trim_start_matches('0').to_string())
        .map(|n| if n.is_empty() { "0".into() } else { n })
}

/// The words after `#3` that make it a ranking, not a PR ("#1 priority").
const NOT_A_PR_AFTER: &[&str] = &[
    "priority", "thing", "goal", "choice", "pick", "rule", "spot", "place", "seed", "fan",
];

/// The `gh pr` subcommands that name a PR after them (`gh pr merge 711`).
const GH_PR: &[&str] = &[
    "merge", "view", "checkout", "close", "review", "edit", "ready", "diff", "checks",
];

/// The words of a sentence that say releases are what it is about: an
/// `R4` names a release only beside one ("cut R4"; Cloudflare's R2 isn't).
const RELEASE_WORDS: &[&str] = &[
    "release", "releases", "promote", "deploy", "ship", "cut", "rollout", "tag",
];

/// What `text` names, each once, in the order it says them, with the word
/// each starts at.
fn targets_at(ws: &[Word]) -> Vec<(usize, Target)> {
    let mut out: Vec<(usize, Target)> = Vec::new();
    let add = |at: usize, t: Target, out: &mut Vec<(usize, Target)>| {
        if !out.iter().any(|(_, o)| *o == t) {
            out.push((at, t));
        }
    };
    let mut i = 0;
    while i < ws.len() {
        let w = ws[i].text.as_str();
        let next = ws.get(i + 1).map(|n| n.text.as_str());
        if mark(w) || (w == "[desk" || w == "desk") && next.is_some_and(mark) {
            i += 1;
            continue;
        }
        // release 4, releases 4, 5 or 6, release-4, release=4, R4.
        if matches!(w, "release" | "releases") {
            let mut j = i + 1;
            let mut any = false;
            while let Some(n) = ws.get(j) {
                if let Some(num) = release_number(&n.text) {
                    add(i, Target::new(TargetKind::Release, num), &mut out);
                    any = true;
                    j += 1;
                } else if any && matches!(n.text.as_str(), "and" | "or" | "to" | "&") {
                    j += 1;
                } else {
                    break;
                }
                if w == "release"
                    && any
                    && ws.get(j).is_some_and(|n| n.text != "and" && n.text != "or")
                {
                    break;
                }
            }
        }
        for sep in ["release-", "release_", "release=", "release#", "release:"] {
            if let Some(num) = w.strip_prefix(sep).and_then(release_number) {
                add(i, Target::new(TargetKind::Release, num), &mut out);
            }
        }
        let sentence = ws[i].sentence;
        let about_releases = || {
            ws.iter()
                .filter(|o| o.sentence == sentence)
                .any(|o| RELEASE_WORDS.contains(&o.text.as_str()))
        };
        if let Some(num) = w
            .strip_prefix('r')
            .filter(|n| digits(n) && n.len() <= 3)
            .filter(|_| about_releases())
        {
            add(i, Target::new(TargetKind::Release, num), &mut out);
        }
        // v1.4.0, v4.2: a release by its version.
        if w.starts_with('v') {
            if let Some(v) = version(w) {
                add(i, Target::new(TargetKind::Release, v), &mut out);
            }
        }
        // #708, PR 708, PR #708, pr-708, pull/708, gh pr merge 708.
        if let Some(n) = w.strip_prefix('#').filter(|n| digits(n)) {
            if !next.is_some_and(|n| NOT_A_PR_AFTER.contains(&n)) {
                add(i, Target::new(TargetKind::Pr, n), &mut out);
            }
        }
        if w == "pr" {
            let after = match next {
                Some(sub) if GH_PR.contains(&sub) => ws.get(i + 2).map(|n| n.text.as_str()),
                n => n,
            };
            if let Some(n) = after
                .map(|n| n.strip_prefix('#').unwrap_or(n))
                .filter(|n| digits(n))
            {
                add(i, Target::new(TargetKind::Pr, n), &mut out);
            }
        }
        for sep in ["pr-", "pr#", "pull/"] {
            if let Some(n) = w.strip_prefix(sep).filter(|n| digits(n)) {
                add(i, Target::new(TargetKind::Pr, n), &mut out);
            }
        }
        // promote.yml, .github/workflows/promote.yaml.
        if w.ends_with(".yml") || w.ends_with(".yaml") {
            let file = w.rsplit('/').next().unwrap_or(w);
            add(i, Target::new(TargetKind::Workflow, file), &mut out);
        }
        if commit_like(w) {
            add(i, Target::new(TargetKind::Commit, w), &mut out);
        }
        // A run id: GitHub's are nine to eleven digits; a longer number
        // (an AWS account's twelve) is one only when said to be a run.
        let said_run = i
            .checked_sub(1)
            .is_some_and(|p| matches!(ws[p].text.as_str(), "run" | "runs" | "run_id" | "run-id"));
        if digits(w) && w.len() >= 9 && (w.len() <= 11 || said_run) && !date_like(w) {
            add(i, Target::new(TargetKind::Run, w), &mut out);
        }
        if let Some(env) = environment(w) {
            add(i, Target::new(TargetKind::Environment, env), &mut out);
        }
        i += 1;
    }
    out
}

/// What `text` names: releases, PRs, workflow files, commits, run ids and
/// environments, each once.
pub fn targets(text: &str) -> Vec<Target> {
    targets_at(&words(text))
        .into_iter()
        .map(|(_, t)| t)
        .collect()
}

/// The verbs that start, on their own, what can't be taken back.
const ACTS: &[&str] = &[
    "release", "deploy", "promote", "dispatch", "publish", "tag", "merge", "rollback", "delete",
    "drop", "migrate", "redeploy",
];

/// The verbs that start what can't be taken back when what follows is a
/// release, a deploy, a promote or a workflow ("start release 4").
const STARTS: &[&str] = &[
    "start", "kick", "trigger", "launch", "ship", "cut", "begin", "fire", "run", "roll", "proceed",
];

/// The verbs that act on what can't be taken back only when what they
/// act on is named right after them ("tag v1.4.0", "delete release 4");
/// "tag me", "delete the comment on #711" act on nothing that ships.
const ON_ITS_OBJECT: &[&str] = &["tag", "delete"];

/// What, after one of [`STARTS`], makes it irreversible.
const UNDONE: &[&str] = &[
    "release",
    "releases",
    "promote",
    "promotion",
    "deploy",
    "deployment",
    "rollout",
    "workflow",
    "dispatch",
    "migration",
    "migrations",
];

/// The words before a verb that make it a noun ("a merge", "the
/// promote", "no promote").
const DETERMINERS: &[&str] = &[
    "a", "an", "the", "this", "that", "its", "your", "my", "our", "their", "of", "for", "each",
    "every", "no", "any", "dry", "real", "last", "next", "first", "another",
];

/// The words after a verb that make it a noun ("release notes", "merge
/// conflict", "deploy key").
const NOUN_AFTER: &[&str] = &[
    "notes",
    "note",
    "conflict",
    "conflicts",
    "request",
    "requests",
    "queue",
    "train",
    "candidate",
    "process",
    "plan",
    "pipeline",
    "step",
    "steps",
    "job",
    "window",
    "status",
    "script",
    "file",
    "prep",
    "branch",
    "log",
    "logs",
    "id",
    "ids",
    "url",
    "link",
    "number",
    "key",
    "keys",
    "is",
    "was",
    "has",
    "had",
    "can",
    "will",
    "went",
    "ran",
    "of",
    "succeeded",
    "failed",
    "finished",
    "already",
    "exists",
    "existed",
    "still",
    "looks",
];

/// Whether the words just before `at`, in its clause, turn it down
/// ("don't dispatch", "do NOT start releases 4, 5 or 6").
fn negated(ws: &[Word], at: usize) -> bool {
    let clause = ws[at].clause;
    ws[at.saturating_sub(4)..at]
        .iter()
        .rev()
        .filter(|w| w.clause == clause)
        // "Stop waiting and merge #711": a no before an and is its own.
        .take_while(|w| !matches!(w.text.as_str(), "and" | "then" | "but"))
        .any(|w| {
            matches!(
                w.text.as_str(),
                "not"
                    | "never"
                    | "don"
                    | "dont"
                    | "t"
                    | "without"
                    | "nor"
                    | "no"
                    | "avoid"
                    | "neither"
                    | "stop"
                    | "instead"
            )
        })
}

/// The package managers whose `publish` can't be taken back.
const PUBLISHERS: &[&str] = &["npm", "cargo", "yarn", "pnpm", "twine", "gem", "poetry"];

/// The objects that stand for what was named before ("merge it").
const PRONOUNS: &[&str] = &["it", "this", "that", "them"];

/// The verbs whose "it" is something that ships ("dispatch it"); "delete
/// it" of a scratch file isn't one.
const SHIPS_IT: &[&str] = &[
    "release", "deploy", "promote", "publish", "tag", "dispatch", "rollback", "redeploy", "merge",
];

/// Whether "roll back it" / "roll it back" speaks of something named.
fn next_but_one_pronoun(ws: &[Word], i: usize) -> bool {
    ws.get(i + 2)
        .is_some_and(|n| n.clause == ws[i].clause && PRONOUNS.contains(&n.text.as_str()))
}

/// Whether what is asked from clause `clause` of sentence `sentence` on
/// is a dry run (`--dry-run`, "a dry run of it"), which takes nothing
/// irreversible; "dry run off", or "as dry run 4438…" (one done before)
/// isn't, and neither is a dry run said before it ("The dry run passed,
/// so dispatch release 4").
fn dry(ws: &[Word], sentence: usize, clause: usize) -> bool {
    let words: Vec<&Word> = ws
        .iter()
        .filter(|w| w.sentence == sentence && w.clause >= clause)
        .collect();
    words.iter().enumerate().any(|(k, w)| {
        let t = w.text.as_str();
        if t.contains("dry-run=false") || t.contains("dry_run=false") {
            return false;
        }
        let flag = matches!(
            t,
            "--dry-run" | "-dry-run" | "dry-run" | "dry_run" | "--dry"
        ) || t.starts_with("--dry-run=")
            || t.starts_with("dry_run=");
        let said = t == "dry"
            && words
                .get(k + 1)
                .is_some_and(|n| matches!(n.text.as_str(), "run" | "runs"));
        let after = words
            .get(k + if said { 2 } else { 1 })
            .map(|n| n.text.as_str());
        let before = k.checked_sub(1).map(|p| words[p].text.as_str());
        (flag || said)
            && !matches!(after, Some("off" | "false" | "disabled"))
            && !matches!(before, Some("as" | "like" | "same" | "from" | "after"))
    })
}

/// The irreversible acts `ws` asks for: the word each verb is at.
fn acts(ws: &[Word], has_target: bool) -> Vec<usize> {
    // A merge acts on a PR or a commit, never only on an environment
    // ("merge main into dev").
    let targets = targets_at(ws);
    let names_more_than_a_place = targets
        .iter()
        .any(|(_, t)| t.kind != TargetKind::Environment);
    let mut out = Vec::new();
    for (i, w) in ws.iter().enumerate() {
        let t = w.text.as_str();
        let prev = i
            .checked_sub(1)
            .map(|p| &ws[p])
            .filter(|p| p.clause == w.clause)
            .map(|p| p.text.as_str())
            .filter(|p| *p != "please");
        let next = ws
            .get(i + 1)
            .filter(|n| n.clause == w.clause)
            .map(|n| n.text.as_str());
        let noun = noun_at(ws, i);
        let force = t == "force-push"
            || t == "--force"
            || (t == "force" && next.is_some_and(|n| n.starts_with("push")));
        let roll_back = t == "roll" && next == Some("back");
        // What a command line does that can't be taken back.
        let command = (t == "publish" && prev.is_some_and(|p| PUBLISHERS.contains(&p)))
            || (t == "apply" && prev == Some("terraform"))
            || (t == "release" && prev == Some("gh") && next == Some("create"))
            || t.ends_with("/dispatches")
            // A tag pushed: `git push origin v1.4.0`, `git push --tags`.
            || (t == "push"
                && prev == Some("git")
                && ws[i + 1..]
                    .iter()
                    .take(5)
                    .take_while(|n| n.sentence == w.sentence)
                    .any(|n| n.text == "--tags" || version_tag(&n.text)));
        // What it acts on, right after it ("tag v1.4.0", "delete the
        // release 4 tag"): a target, at most a word on.
        let object_named = targets
            .iter()
            .any(|(at, t)| t.kind != TargetKind::Environment && (*at == i + 1 || *at == i + 2))
            || ws.get(i + 1).is_some_and(|n| version_tag(&n.text));
        let undone_follows = || {
            ws[i + 1..]
                .iter()
                .take(6)
                .take_while(|n| n.sentence == w.sentence)
                .any(|n| {
                    UNDONE.contains(&n.text.as_str())
                        || n.text.ends_with(".yml")
                        || n.text.ends_with(".yaml")
                        || n.text.starts_with("release-")
                        || n.text.starts_with("release=")
                })
        };
        let pronoun = SHIPS_IT.contains(&t) && next.is_some_and(|n| PRONOUNS.contains(&n));
        let fires = force
            || command
            || (roll_back && (has_target || next_but_one_pronoun(ws, i)))
            || (ACTS.contains(&t)
                && !noun
                && (!ON_ITS_OBJECT.contains(&t) || object_named || pronoun)
                && ((has_target && (t != "merge" || names_more_than_a_place))
                    || pronoun
                    || undone_follows()))
            || (STARTS.contains(&t) && undone_follows());
        if fires && !negated(ws, i) && !dry(ws, w.sentence, w.clause) {
            out.push(i);
        }
    }
    out
}

/// Whether the verb-like word at `i` is a noun there: "own release 4",
/// "Release 4 can start" (a number right after names the thing; first in
/// its clause and going on, "Release 4 now.", "Deploy 4 to prod", it is
/// what to do), "a merge", "release notes".
fn noun_at(ws: &[Word], i: usize) -> bool {
    let w = &ws[i];
    let t = w.text.as_str();
    let prev = i
        .checked_sub(1)
        .map(|p| &ws[p])
        .filter(|p| p.clause == w.clause)
        .map(|p| p.text.as_str())
        .filter(|p| *p != "please");
    let next = ws
        .get(i + 1)
        .filter(|n| n.clause == w.clause)
        .map(|n| n.text.as_str());
    let after_next = ws
        .get(i + 2)
        .filter(|n| n.clause == w.clause)
        .map(|n| n.text.as_str());
    let numbered = matches!(t, "release" | "promote" | "deploy")
        && next.is_some_and(numberish)
        && (prev.is_some() || after_next.is_some_and(|n| NOUN_AFTER.contains(&n)));
    numbered
        || prev.is_some_and(|p| DETERMINERS.contains(&p))
        || next.is_some_and(|n| NOUN_AFTER.contains(&n))
}

/// Whether `w` is a version tag: `v1.4.0`, `v2.0`.
fn version_tag(w: &str) -> bool {
    w.strip_prefix('v')
        .is_some_and(|rest| rest.contains('.') && version(rest).is_some())
}

/// Whether `text` asks for anything to be done at all with a verb that
/// starts work that can't be taken back (release, deploy, dispatch,
/// start, run, proceed…), said or not, named or not: what a word on a
/// job a session already holds must not, to be only a word on it.
pub fn asks_an_act(text: &str) -> bool {
    let ws = words(text);
    (0..ws.len()).any(|i| {
        let t = ws[i].text.as_str();
        (ACTS.contains(&t) || STARTS.contains(&t) || GO_ON.contains(&t))
            && !noun_at(&ws, i)
            && !negated(&ws, i)
    })
}

/// The words that tell a session to carry on with what it holds ("Go
/// ahead now.", "Continue.", "Retry it."): on a delivered irreversible
/// job they ask for the act again, never a follow-up.
const GO_ON: &[&str] = &["go", "continue", "retry", "redo", "again", "resume"];

/// Whether `text` asks for something that can't be taken back: one of
/// the verbs that do (release, deploy, promote, dispatch, run a workflow,
/// publish, tag, merge, roll back, delete, force, drop, migrate) said to
/// be done, on something it names. Never when it is said not to be done
/// ("don't dispatch"), and not "release notes", "a merge conflict" or
/// "write a migration".
pub fn irreversible(text: &str) -> bool {
    let ws = words(text);
    let has = !targets_at(&ws).is_empty();
    !acts(&ws, has).is_empty()
}

/// Whether `ws` asks for an irreversible act only to say not to do it.
fn is_negated_order(ws: &[Word]) -> bool {
    ws.iter().enumerate().any(|(i, w)| {
        (ACTS.contains(&w.text.as_str()) || STARTS.contains(&w.text.as_str())) && negated(ws, i)
    })
}

/// The job an order hands out, as the boss declares it with the order
/// (`job: { kind, target, key }`): what kind of work, in which repository,
/// and the job's own id. Two orders are the same job when their kind,
/// target and key are, however each was written ([`Job::new`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub kind: String,
    pub target: String,
    pub key: String,
}

impl Job {
    /// The job of `kind` in `target`, keyed `key`, each in one form:
    /// `Mantis-CRM/monorepo.git` and `github.com/mantis-crm/monorepo` are
    /// one target; `release-4`, `Release 4`, `r4` and `4` one release;
    /// `pr-711`, `#711` and `711` one merge. None when one of them says
    /// nothing.
    pub fn new(kind: &str, target: &str, key: &str) -> Option<Self> {
        let kind = normal_kind(kind)?;
        let target = normal_target(target)?;
        let key = normal_key(&kind, key)?;
        Some(Self { kind, target, key })
    }

    /// As kept: `<target>/<kind>/<key>`.
    pub fn as_key(&self) -> String {
        format!("{}/{}/{}", self.target, self.kind, self.key)
    }

    /// Read back from its key.
    pub fn of_key(key: &str) -> Option<Self> {
        let mut parts = key.rsplitn(3, '/');
        let (k, kind, target) = (parts.next()?, parts.next()?, parts.next()?);
        Some(Self {
            kind: kind.into(),
            target: target.into(),
            key: k.into(),
        })
    }

    /// As the person reads it: "release 4 of mantis-crm/monorepo".
    pub fn said(&self) -> String {
        let what = match self.kind.as_str() {
            "merge" => format!("the merge of #{}", self.key),
            k => format!("{k} {}", self.key),
        };
        format!("{what} of {}", self.target)
    }
}

/// A kind in one form: lower case, one word, singular.
fn normal_kind(kind: &str) -> Option<String> {
    let k: String = kind
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let k = k.trim_matches('-').to_string();
    let k = match k.as_str() {
        "pr" | "pull-request" | "merges" => "merge".to_string(),
        "releases" => "release".into(),
        "deploys" | "deployment" | "deployments" => "deploy".into(),
        "migrations" => "migration".into(),
        "roll-back" | "rollbacks" => "rollback".into(),
        "promotion" | "promotions" => "promote".into(),
        _ => k,
    };
    (!k.is_empty() && !k.contains('/')).then_some(k)
}

/// A repository in one form: `owner/name`, lower case, without its host,
/// scheme or `.git`.
fn normal_target(target: &str) -> Option<String> {
    let mut t = target.trim().to_lowercase();
    for p in ["https://", "http://", "ssh://", "git@"] {
        if let Some(rest) = t.strip_prefix(p) {
            t = rest.to_string();
        }
    }
    for host in [
        "github.com/",
        "github.com:",
        "www.github.com/",
        "gitlab.com/",
        "gitlab.com:",
    ] {
        if let Some(rest) = t.strip_prefix(host) {
            t = rest.to_string();
        }
    }
    let t = t.trim_end_matches('/');
    let t = t.strip_suffix(".git").unwrap_or(t).trim_matches('/');
    (!t.is_empty() && !t.contains(char::is_whitespace)).then(|| t.to_string())
}

/// A job's own id in one form: what names it once its kind is said, so
/// `release-4`, `Release 4`, `r4` and `4` are all `4`.
fn normal_key(kind: &str, key: &str) -> Option<String> {
    let mut k = key.trim().to_lowercase();
    let seps = |c: char| matches!(c, '-' | '_' | ' ' | '#' | ':' | '=' | '/' | '.');
    let words: &[&str] = match kind {
        "merge" => &["pull-request", "pull", "pr", "merge"],
        "release" | "deploy" | "promote" | "rollback" | "dispatch" | "publish" | "tag" => &[
            "release", "deploy", "promote", "rollback", "dispatch", "publish", "tag",
        ],
        _ => &[],
    };
    loop {
        let before = k.clone();
        for w in words.iter().copied().chain(std::iter::once(kind)) {
            if let Some(rest) = k.strip_prefix(w) {
                if rest.is_empty()
                    || rest.starts_with(seps)
                    || rest.starts_with(|c: char| c.is_ascii_digit())
                {
                    k = rest.trim_start_matches(seps).to_string();
                }
            }
        }
        k = k.trim_start_matches(seps).to_string();
        if k == before {
            break;
        }
    }
    // r4, v1.4.0: the letter only marks a number.
    if let Some(rest) = k.strip_prefix(['r', 'v']) {
        if numberish(rest) {
            k = rest.to_string();
        }
    }
    let k = k.trim_end_matches(seps).to_string();
    let k = if digits(&k) {
        let n = k.trim_start_matches('0');
        if n.is_empty() {
            "0".to_string()
        } else {
            n.to_string()
        }
    } else {
        k.split_whitespace().collect::<Vec<_>>().join("-")
    };
    (!k.is_empty()).then_some(k)
}

/// Whether `text` names `job` as something to do: a release, PR, run,
/// commit or workflow that is the job's own key, said in an order (not
/// only in one not to do it). Never a claim: what a boss is asked to
/// confirm before it sends ([`Reading`]).
pub fn looks_like(text: &str, job: &Job) -> bool {
    let ws = words(text);
    let found = targets_at(&ws);
    if acts(&ws, !found.is_empty()).is_empty() && is_negated_order(&ws) {
        return false;
    }
    found.iter().any(|(_, t)| {
        let fits = match t.kind {
            TargetKind::Pr => job.kind == "merge",
            TargetKind::Release => job.kind != "merge",
            TargetKind::Environment => false,
            TargetKind::Workflow | TargetKind::Commit | TargetKind::Run => true,
        };
        fits && normal_key(&job.kind, &t.id).as_deref() == Some(job.key.as_str())
    })
}

/// What an instruction's words say, read: what it names, and whether it
/// starts something that can't be taken back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    pub targets: Vec<Target>,
    pub irreversible: bool,
}

/// Reads `text`, with `said_irreversible` as the boss said it (true is
/// honoured whatever the text says). The words never make a job: a job
/// is only ever the one the boss declared ([`Job`]).
pub fn read(text: &str, said_irreversible: bool) -> Reading {
    Reading {
        targets: targets(text),
        irreversible: said_irreversible || irreversible(text),
    }
}

/// A live assignment as the rules read it: whether its line message has
/// reached its session.
#[derive(Debug, Clone)]
pub struct Live {
    pub a: Assignment,
    pub delivered: bool,
}

/// Which rule refused an order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// R1: the same session already has this irreversible job, delivered.
    SameSession,
    /// The same session waits, held, to be given this job.
    Held,
    /// R2: another session holds this job.
    SameJob,
}

/// What [`rule`] decides of a new order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ruling {
    /// It may go. `supersede`: the same session's earlier orders for the
    /// job it takes the place of. `follow_up`: the session already holds
    /// this job irreversibly; this is a word on it, holding nothing itself.
    Send {
        supersede: Vec<Uuid>,
        follow_up: bool,
    },
    Refused {
        rule: Rule,
        holder: Uuid,
    },
}

/// Whether two jobs' keys are one job.
fn same_job(a: Option<&str>, b: Option<&str>) -> bool {
    matches!((a, b), (Some(a), Some(b)) if a == b)
}

/// Whether `new` (a declared job) may go, against the boss's `live`
/// assignments, leaving out `replacing` (the one it takes over, with
/// `replaces`). Only declared jobs are compared: an order's words never
/// refuse another's.
pub fn rule(new: &Assignment, live: &[Live], replacing: Option<Uuid>) -> Ruling {
    let mut supersede = Vec::new();
    let mut follow_up = false;
    let mut refused: Option<(Rule, Uuid)> = None;
    let refuse = |r: Rule, id: Uuid, refused: &mut Option<(Rule, Uuid)>| {
        let rank = |r: Rule| match r {
            Rule::SameSession => 0,
            Rule::Held => 1,
            Rule::SameJob => 2,
        };
        if refused.is_none_or(|(was, _)| rank(r) < rank(was)) {
            *refused = Some((r, id));
        }
    };
    for l in live
        .iter()
        .filter(|l| l.a.state.live() && Some(l.a.id) != replacing && l.a.id != new.id)
        .filter(|l| same_job(l.a.job_key.as_deref(), new.job_key.as_deref()))
    {
        let a = &l.a;
        if a.session == new.session && a.machine == new.machine {
            match (a.state, a.irreversible && l.delivered, new.irreversible) {
                (AssignmentState::Held, ..) => refuse(Rule::Held, a.id, &mut refused),
                (_, true, true) => refuse(Rule::SameSession, a.id, &mut refused),
                // A word on it, asking nothing to be done: a follow-up.
                // Anything that asks for an act may start it again,
                // however the words read.
                (_, true, false) if !asks_an_act(&new.text) => follow_up = true,
                (_, true, false) => refuse(Rule::SameSession, a.id, &mut refused),
                _ => supersede.push(a.id),
            }
            continue;
        }
        if a.claims || new.claims {
            refuse(Rule::SameJob, a.id, &mut refused);
        }
    }
    match refused {
        Some((rule, holder)) => Ruling::Refused { rule, holder },
        None => Ruling::Send {
            supersede,
            follow_up,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(text: &str) -> Vec<String> {
        targets(text).iter().map(Target::key).collect()
    }

    // The orders of 18 Nov, anonymised: names changed; numbers, words and
    // their order kept.
    const OWN_4: &str = "Desk, 20:14. Thanks, and I've checked it: 44383453606 and converge 44388058690 are green on GitHub. You now own release 4, and only you dispatch it. GitHub shows no promote of any kind since release 3";
    const URGENT_4: &str = "URGENT, one job: start Harbor production release 4 (leads step 2) and watch it through. Sam, the owner, ordered every production release out tonight";
    const DISPATCH_3: &str = "Dispatch the real release 3 promote now at exactly f40816536a2fa8d947706f7f7a2c3f6d951a0123, with the same workflow and inputs as dry run 44380837640 and dry run off, and send Desk the run id";
    const URGENT_3: &str = "URGENT, one job, from Desk for Sam (owner): start Harbor production release 3 now and watch it through.";

    #[test]
    fn the_nights_orders_name_their_release_and_cant_be_taken_back() {
        for text in [OWN_4, URGENT_4, DISPATCH_3, URGENT_3] {
            assert!(read(text, false).irreversible, "{text}");
        }
        // The night's words, read: the backstop sees release 4 and 3 in them.
        let four = Job::new("release", "harbor/monorepo", "release-4").unwrap();
        let three = Job::new("release", "harbor/monorepo", "3").unwrap();
        assert!(looks_like(OWN_4, &four) && looks_like(URGENT_4, &four));
        assert!(looks_like(DISPATCH_3, &three) && looks_like(URGENT_3, &three));
        assert!(!looks_like(URGENT_3, &four));
        assert_eq!(
            keys(OWN_4),
            vec![
                "run:44383453606",
                "run:44388058690",
                "release:4",
                "release:3"
            ]
        );
        let d3 = keys(DISPATCH_3);
        assert!(d3.contains(&"commit:f40816536a2fa8d947706f7f7a2c3f6d951a0123".into()));
        assert!(d3.contains(&"run:44380837640".into()));
        assert!(keys(URGENT_4).contains(&"env:production".into()));
    }

    #[test]
    fn what_a_session_says_of_a_release_starts_nothing() {
        for said in [
            "Release 3 is green. Release 4 can start.",
            "Release 4 can start",
            "Confirm #639 merged and staging is healthy, then stop.",
            "Write the release notes for release 4.",
            "There is a merge conflict on #711; resolve it on the branch.",
            "Write a migration for the ledger table.",
            "Promote 44389184927 already exists (20:15:56)",
            "GitHub shows no promote of any kind since release 3",
        ] {
            assert!(!irreversible(said), "{said}");
        }
        for asked in [
            "Merge #711.",
            "deploy to production",
            "Dispatch the promote for release 4.",
            "Run promote.yml for release 4",
            "gh workflow run promote.yml -f release=4",
            "Force-push the branch.",
            "Roll back release 4 in production.",
            "Release 4 now.",
            "Deploy 4 to prod",
            "Stop waiting and merge #711",
            "Dispatch it.",
            "gh pr merge 711 --squash",
            "gh release create v1.4.0",
            "npm publish",
            "cargo publish -p workforce-desk",
            "terraform apply -auto-approve",
            "gh api repos/acme/harbor/actions/workflows/promote.yml/dispatches",
        ] {
            assert!(irreversible(asked), "{asked}");
        }
        for not in [
            "gh workflow run promote.yml -f release=4 --dry-run",
            "Do a dry run of release 4 first.",
            "Delete it.",
            "Merge main into dev.",
            "Release 4 can start",
        ] {
            assert!(!irreversible(not), "{not}");
        }
    }

    #[test]
    fn an_order_not_to_is_no_order_and_names_no_job() {
        for text in [
            "Do NOT start releases 4, 5 or 6 until I say.",
            "Don't dispatch release 4; Second batch prep has it.",
            "Never merge #711 yourself.",
        ] {
            assert!(!read(text, false).irreversible, "{text}");
            let four = Job::new("release", "harbor/monorepo", "4").unwrap();
            assert!(
                !looks_like(text, &four),
                "a no names nothing to confirm: {text}"
            );
        }
        assert_eq!(
            keys("Do NOT start releases 4, 5 or 6"),
            vec!["release:4", "release:5", "release:6"]
        );
        // A no in its own clause doesn't reach the next one.
        assert!(irreversible("Don't wait, dispatch release 4 now."));
        // The boss saying so is honoured.
        assert!(read("Carry on.", true).irreversible);
    }

    #[test]
    fn targets_tell_commits_runs_and_dates_apart() {
        // A commit has letters and digits; a word of hex letters isn't one.
        assert_eq!(keys("at f408165 now"), vec!["commit:f408165"]);
        assert!(keys("the facade decade").is_empty());
        assert!(
            keys("abc1234").len() == 1 && keys("abc12").is_empty(),
            "seven at least"
        );
        // A run id is nine digits or more; a date isn't one.
        assert_eq!(keys("rerun 44383453606"), vec!["run:44383453606"]);
        assert!(keys("on 20261118, and 202611181230").is_empty());
        assert!(keys("issue 12345678").is_empty());
        // Desk's own marks are never commits.
        assert!(keys("Ack m:5068bfae, re a:3f2a1b2c and [Desk m:0a1b2c3d]").is_empty());
        assert!(keys("id 0c9d3f6e-1a2b-4c5d-8e9f-0123456789ab").is_empty());
        // Releases, PRs and workflows however they are written.
        assert_eq!(keys("Cut R4 now"), vec!["release:4"]);
        assert!(
            keys("Upload it to the R2 bucket").is_empty(),
            "Cloudflare's R2"
        );
        assert!(keys("This is the #1 priority").is_empty());
        assert!(keys("account 123456789012").is_empty(), "an AWS account");
        assert_eq!(keys("run 123456789012"), vec!["run:123456789012"]);
        assert_eq!(keys("release 1.4.0"), vec!["release:1.4.0"]);
        assert_eq!(
            keys("Deploy v4.2 to prod"),
            vec!["release:4.2", "env:production"]
        );
        assert_eq!(keys("gh pr merge 711"), vec!["pr:711"]);
        assert_eq!(keys("release-4"), vec!["release:4"]);
        assert_eq!(
            keys("PR 708 and pr #709 and #710"),
            vec!["pr:708", "pr:709", "pr:710"]
        );
        assert_eq!(
            keys("run .github/workflows/promote.yml in prod"),
            vec!["workflow:promote.yml", "env:production"]
        );
    }

    #[test]
    fn what_the_second_review_found_reads_as_it_should() {
        for text in [
            "The dry run passed, so dispatch release 4.",
            "Proceed with release 4.",
            "git push origin v1.4.0",
            "Tag v1.4.0 and push it.",
            "Delete release 4 from GitHub.",
        ] {
            assert!(irreversible(text), "irreversible: {text}");
        }
        for text in [
            "Review #711 and tag me when done.",
            "Rerun the failed CI job on #711.",
            "Delete the comment on #711.",
            "Dispatch the promote for release 4 with --dry-run.",
        ] {
            assert!(!irreversible(text), "not irreversible: {text}");
        }
        assert!(asks_an_act("Go ahead and proceed."));
        for text in ["Go ahead now.", "Continue.", "Retry it.", "Do it again."] {
            assert!(asks_an_act(text), "asks for the act again: {text}");
        }
        assert!(!asks_an_act(
            "When release 3 finishes, send Desk its run id."
        ));
        assert!(!asks_an_act(
            "Send Desk the release 3 run id when you have it."
        ));
    }

    #[test]
    fn a_declared_job_is_one_key_however_it_is_written() {
        let key =
            |kind: &str, target: &str, key: &str| Job::new(kind, target, key).unwrap().as_key();
        let four = "mantis-crm/monorepo/release/4";
        for (kind, target, k) in [
            ("release", "mantis-crm/monorepo", "release-4"),
            ("Release", "Mantis-CRM/monorepo", "Release 4"),
            (
                "releases",
                "https://github.com/mantis-crm/monorepo.git",
                "r4",
            ),
            ("release", "git@github.com:mantis-crm/monorepo.git", "4"),
            ("release", "github.com/mantis-crm/monorepo/", "#4"),
        ] {
            assert_eq!(key(kind, target, k), four, "{kind} {target} {k}");
        }
        for k in ["pr-711", "#711", "711", "PR 711", "pull/711"] {
            assert_eq!(key("pr", "acme/harbor", k), "acme/harbor/merge/711", "{k}");
        }
        assert_eq!(
            key("release", "acme/harbor", "v1.4.0"),
            "acme/harbor/release/1.4.0"
        );
        assert_eq!(
            key("migration", "acme/harbor", "Ledger table v2"),
            "acme/harbor/migration/ledger-table-v2"
        );
        assert!(Job::new("release", "", "4").is_none());
        assert!(Job::new("release", "acme/harbor", " ").is_none());
        let job = Job::of_key(four).unwrap();
        assert_eq!(job.said(), "release 4 of mantis-crm/monorepo");
    }

    #[test]
    fn the_backstop_sees_a_held_job_said_only_when_it_is_asked_for() {
        let four = Job::new("release", "harbor/monorepo", "4").unwrap();
        let pr = Job::new("merge", "harbor/monorepo", "711").unwrap();
        assert!(looks_like("Pick up release 4 and watch it through.", &four));
        assert!(
            looks_like("Review #711.", &pr),
            "a mention asks to confirm, never claims"
        );
        assert!(!looks_like("Review #711.", &four));
        assert!(!looks_like("Release 5 is next.", &four));
        assert!(!looks_like("Don't start release 4.", &four));
        assert!(!looks_like("Carry on with the auth reset.", &four));
    }

    fn live(session: Uuid, text: &str, job: Option<&str>, delivered: bool) -> Live {
        let r = read(text, false);
        let mut a = Assignment::new("desk", session, text);
        a.job_key = job.map(|j| Job::new("release", "acme/harbor", j).unwrap().as_key());
        a.targets = r.targets.iter().map(Target::key).collect();
        a.irreversible = r.irreversible;
        a.claims = a.job_key.is_some();
        a.repo = Some("acme/harbor".into());
        a.state = AssignmentState::Open;
        Live { a, delivered }
    }

    #[test]
    fn the_rules_refuse_a_second_holder_and_a_second_start() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let held = [live(a, OWN_4, Some("release-4"), true)];
        // R2: release 4 to another session, however it is keyed.
        let new = live(b, URGENT_4, Some("Release 4"), false).a;
        assert_eq!(
            rule(&new, &held, None),
            Ruling::Refused {
                rule: Rule::SameJob,
                holder: held[0].a.id
            }
        );
        // ...unless it replaces the holder.
        assert!(matches!(
            rule(&new, &held, Some(held[0].a.id)),
            Ruling::Send { .. }
        ));
        // Another release is another job.
        let five = live(b, "Start release 5.", Some("5"), false).a;
        assert!(matches!(rule(&five, &held, None), Ruling::Send { .. }));
        // R1: release 3 again to the same session, once the first went in.
        let first = [live(a, DISPATCH_3, Some("3"), true)];
        let again = live(a, URGENT_3, Some("3"), false).a;
        assert!(matches!(
            rule(&again, &first, None),
            Ruling::Refused {
                rule: Rule::SameSession,
                ..
            }
        ));
        // Not yet delivered: the new one takes its place.
        let waiting = [live(a, DISPATCH_3, Some("3"), false)];
        assert_eq!(
            rule(&again, &waiting, None),
            Ruling::Send {
                supersede: vec![waiting[0].a.id],
                follow_up: false
            }
        );
        // A word on a job it holds is sent and holds nothing.
        let word = live(
            a,
            "When release 3 finishes, send Desk its run id.",
            Some("3"),
            false,
        )
        .a;
        assert_eq!(
            rule(&word, &first, None),
            Ruling::Send {
                supersede: vec![],
                follow_up: true
            }
        );
        // Anything asking for an act again, however its words read, is
        // refused: it may start it twice.
        let proceed = live(
            a,
            "Go on with it now, as agreed: proceed.",
            Some("3"),
            false,
        )
        .a;
        assert!(matches!(
            rule(&proceed, &first, None),
            Ruling::Refused {
                rule: Rule::SameSession,
                ..
            }
        ));
        // An order that declares no job is never compared.
        let mention = live(b, "Write the release notes for release 4.", None, false).a;
        assert!(matches!(rule(&mention, &held, None), Ruling::Send { .. }));
    }

    #[test]
    fn an_assignment_is_named_by_its_short_id() {
        let ids = [Uuid::new_v4(), Uuid::new_v4()];
        assert_eq!(named(&short(ids[0]), &ids), Some(ids[0]));
        assert_eq!(named(&ids[1].to_string(), &ids), Some(ids[1]));
        assert_eq!(named("a:zz", &ids), None);
        assert_eq!(named(&short(Uuid::new_v4()), &ids), None);
    }
}
