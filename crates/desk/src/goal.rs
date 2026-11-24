//! Goals: a plan's result made measurable, and what Desk keeps of it. The
//! person agrees a goal with the boss in their own words, and only a met
//! reading or the person's own checked words close it. The boss reports
//! facts toward it, and shows checkpoints toward the parts nothing can
//! measure.
//!
//! Everything here is pure: the hub reads the measures and the person's
//! messages, and this module decides what they come to.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use crate::crew::Party;
use crate::model::text_enum;
use crate::plan::Plan;

/// The most parts a result has.
pub const MAX_PARTS: usize = 4;
/// The most a result's words say, in characters.
pub const WORDS_MAX: usize = 300;
/// The most runs in a row a CI part asks for.
pub const MAX_RUNS: u8 = 10;
/// The most a fact or checkpoint says, as kept and as shown.
pub const FACT_MAX: usize = 300;
/// The most facts and checkpoints the goals block shows.
pub const FACTS_SHOWN: usize = 8;
pub use workforce_core::desk::GOALS_MARKER;

/// How done is measured: met when every part is met.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoalResult {
    /// Done when …, in the person's terms.
    pub words: String,
    pub parts: Vec<Measure>,
}

impl GoalResult {
    /// Whether `other` is this result read from elsewhere: the same words
    /// and parts, each measuring the same to the same bound.
    pub fn same_but_address(&self, other: &GoalResult) -> bool {
        self.words == other.words
            && self.parts.len() == other.parts.len()
            && self
                .parts
                .iter()
                .zip(&other.parts)
                .all(|(a, b)| a == b || a.same_but_address(b))
    }
}

/// One part of a result, and where it is read from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Measure {
    /// A GitHub Actions workflow's runs on a branch.
    Ci {
        repo: String,
        /// The workflow by its name; every workflow's runs when none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workflow: Option<String>,
        branch: String,
        want: Want,
    },
    /// A pull request merged.
    PrMerged { repo: String, pr: PrPick },
    /// A pull request's checks green on its current head.
    ChecksGreen { repo: String, pr: PrPick },
    /// What nothing can measure: the boss judges it by `evidence`.
    Judged { evidence: String },
}

/// What a CI part wants of its runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Want {
    /// The newest `runs` runs each took at most `at_most` minutes, a run
    /// whole or its longest job.
    Minutes { per: Per, at_most: f64, runs: u8 },
    /// The newest `runs` runs each succeeded.
    Green { runs: u8 },
}

impl Want {
    /// How many of the newest runs it reads.
    pub fn runs(&self) -> u8 {
        match self {
            Self::Minutes { runs, .. } | Self::Green { runs } => *runs,
        }
    }
}

text_enum!(Per {
    Run => "run",
    Job => "job",
});

/// A pull request, by its number or by its head branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PrPick {
    Number(u64),
    Head(String),
}

/// What a reading or a checkpoint was read from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Evidence {
    Run { repo: String, id: u64 },
    Pull { repo: String, number: u64 },
    Commit { repo: String, sha: String },
    Ticket { key: String },
}

impl Evidence {
    /// The repository it is of, when it is GitHub's.
    pub fn repo(&self) -> Option<&str> {
        match self {
            Self::Run { repo, .. } | Self::Pull { repo, .. } | Self::Commit { repo, .. } => {
                Some(repo)
            }
            Self::Ticket { .. } => None,
        }
    }
}

/// One reading of a measured part: its value, none when it couldn't be
/// read (then `failed` says why), and whether it met the part.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reading {
    pub at: DateTime<Utc>,
    pub part: u16,
    pub value: Option<f64>,
    pub met: bool,
    /// What it read, in words: "22 minutes, the longest job".
    pub words: String,
    pub evidence: Vec<Evidence>,
    pub failed: Option<String>,
}

impl Reading {
    /// Part `part` not read at `at`, for `why`.
    pub fn failed(part: u16, at: DateTime<Utc>, why: &str) -> Self {
        Self {
            at,
            part,
            value: None,
            met: false,
            words: String::new(),
            evidence: Vec::new(),
            failed: Some(crate::crew::one_line(why, 300)),
        }
    }
}

text_enum!(FactKind {
    // What the boss reported toward the goal.
    Fact => "fact",
    // What the boss showed toward a judged part.
    Checkpoint => "checkpoint",
});

/// A fact or a checkpoint, as kept with its goal.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Fact {
    pub id: Uuid,
    pub plan: Uuid,
    pub kind: FactKind,
    /// The judged part a checkpoint is toward.
    pub part: Option<u16>,
    pub at: DateTime<Utc>,
    pub text: String,
    pub evidence: Vec<Evidence>,
    /// Every piece of its evidence was found as it says.
    pub checked: bool,
    pub made_by: Party,
    /// The fact it corrects, which is no longer shown.
    pub replaces: Option<Uuid>,
}

text_enum!(MetBy {
    // A reading met every part.
    Result => "result",
    // The person said it is done, in their checked words.
    Person => "person",
});

/// When and how a goal was met.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Met {
    pub at: DateTime<Utc>,
    pub by: MetBy,
    pub why: String,
}

/// How one part stands, folded from its readings or checkpoints.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PartProgress {
    pub met: bool,
    /// Its reading nearest the target.
    pub best: Option<Reading>,
    /// When it last came nearer its target, or a checkpoint toward it.
    pub improved_at: Option<DateTime<Utc>>,
    /// Its newest reading, read or failed.
    pub newest: Option<Reading>,
    /// Failed reads in a row, newest last.
    pub failing: u32,
    /// When the first of those failed reads was taken.
    pub failing_since: Option<DateTime<Utc>>,
}

/// How a goal stands, part by part, in its result's order.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Progress {
    pub parts: Vec<PartProgress>,
}

/// What a boss's question about a goal proposes: the person's answer must
/// be to this, and the change applied must be this one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<GoalResult>,
    /// That the goal is met, with what shows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub met: Option<String>,
}

impl Proposal {
    pub fn is_empty(&self) -> bool {
        self.goal.is_none() && self.result.is_none() && self.met.is_none()
    }
}

/// A question of the boss's about a goal, not yet closed: what it
/// proposed, and when.
#[derive(Debug, Clone, PartialEq)]
pub struct GoalAsk {
    pub item: Uuid,
    pub proposal: Proposal,
    pub at: DateTime<Utc>,
    /// The person's answer on the item's buttons, when they answered it
    /// there: when, and what.
    pub answered: Option<(DateTime<Utc>, String)>,
}

/// How an item the person answered on its buttons says their answer.
pub const YOU_ANSWERED: &str = "You answered: ";

/// Why a result doesn't read, in the boss's words.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    #[error("A result has 1 to {MAX_PARTS} parts.")]
    Parts,
    #[error("Say what done is in words (words), 1 to {WORDS_MAX} characters.")]
    Words,
    #[error("Part {0} names its repository as owner/name (driftwerk/workforce).")]
    Repo(usize),
    #[error("Part {0} says the branch its runs are read on.")]
    Branch(usize),
    #[error("Part {0} names its workflow by its name, or leaves it out.")]
    Workflow(usize),
    #[error("Part {0} is at most a positive number of minutes.")]
    Bound(usize),
    #[error("Part {0} asks for 1 to {MAX_RUNS} runs in a row.")]
    Runs(usize),
    #[error("Part {0} names its pull request by its number or its head branch.")]
    Pull(usize),
    #[error("Part {0} says what it is judged by, 1 to {WORDS_MAX} characters.")]
    Evidence(usize),
}

impl Measure {
    /// Whether a reader measures it, rather than the boss judging it.
    pub fn measured(&self) -> bool {
        !matches!(self, Self::Judged { .. })
    }

    /// The repository it is read from.
    pub fn repo(&self) -> Option<&str> {
        match self {
            Self::Ci { repo, .. }
            | Self::PrMerged { repo, .. }
            | Self::ChecksGreen { repo, .. } => Some(repo),
            Self::Judged { .. } => None,
        }
    }

    /// Whether `other` measures the same thing to the same bound, read
    /// from somewhere else perhaps: only where it reads may be repaired.
    pub fn same_but_address(&self, other: &Measure) -> bool {
        match (self, other) {
            (Self::Ci { want: a, .. }, Self::Ci { want: b, .. }) => a == b,
            (Self::PrMerged { .. }, Self::PrMerged { .. })
            | (Self::ChecksGreen { .. }, Self::ChecksGreen { .. }) => true,
            _ => false,
        }
    }

    /// Whether a lower value is nearer its target: minutes are; runs green,
    /// a pull request's stage and its checks are higher.
    pub fn lower_is_nearer(&self) -> bool {
        matches!(
            self,
            Self::Ci {
                want: Want::Minutes { .. },
                ..
            }
        )
    }

    fn nearer(&self, a: f64, b: f64) -> bool {
        match self.lower_is_nearer() {
            true => a < b,
            false => a > b,
        }
    }

    /// What it is, in words: "the CI workflow on main of driftwerk/workforce".
    pub fn label(&self) -> String {
        match self {
            Self::Ci {
                repo,
                workflow,
                branch,
                ..
            } => match workflow {
                Some(w) => format!("“{w}” on {branch} of {repo}"),
                None => format!("CI on {branch} of {repo}"),
            },
            Self::PrMerged { repo, pr } | Self::ChecksGreen { repo, pr } => {
                format!("{} of {repo}", pr_words(pr))
            }
            Self::Judged { evidence } => format!("“{evidence}”"),
        }
    }

    /// What meets it, in words: "at most 5 minutes a job, 3 runs in a row".
    pub fn target(&self) -> String {
        match self {
            Self::Ci { want, .. } => match want {
                Want::Minutes { per, at_most, runs } => format!(
                    "at most {} a {}, {}",
                    minutes(*at_most),
                    per.as_str(),
                    in_a_row(*runs)
                ),
                Want::Green { runs } => format!("green, {}", in_a_row(*runs)),
            },
            Self::PrMerged { .. } => "merged".into(),
            Self::ChecksGreen { .. } => "its checks green".into(),
            Self::Judged { .. } => "judged from the work's evidence".into(),
        }
    }
}

/// A pull request, in words: "PR #12", "the PR from fix-ci".
pub fn pr_words(pr: &PrPick) -> String {
    match pr {
        PrPick::Number(n) => format!("PR #{n}"),
        PrPick::Head(head) => format!("the PR from {head}"),
    }
}

/// `n` minutes in words, to a tenth: "22 minutes", "4.5 minutes".
pub fn minutes(n: f64) -> String {
    let tenths = (n * 10.0).round() / 10.0;
    match tenths == 1.0 {
        true => "1 minute".into(),
        false => format!("{tenths} minutes"),
    }
}

fn in_a_row(runs: u8) -> String {
    match runs {
        1 => "its newest run".into(),
        n => format!("{n} runs in a row"),
    }
}

/// `repo` as GitHub's owner and name, when it is one: GitHub's own
/// characters alone, nothing that steers a request elsewhere (`?`, `#`,
/// `..`).
pub fn github_repo(repo: &str) -> Option<(&str, &str)> {
    let part = |p: &str| {
        !p.is_empty()
            && p != "."
            && p != ".."
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    repo.trim()
        .split_once('/')
        .filter(|(owner, name)| part(owner) && part(name))
}

/// Whether `r` reads as a result Desk can keep and check.
pub fn validate(r: &GoalResult) -> Result<(), Vec<Problem>> {
    let mut problems = Vec::new();
    if r.parts.is_empty() || r.parts.len() > MAX_PARTS {
        problems.push(Problem::Parts);
    }
    let words = r.words.trim().chars().count();
    if words == 0 || words > WORDS_MAX {
        problems.push(Problem::Words);
    }
    for (i, part) in r.parts.iter().enumerate() {
        problems.extend(part_problems(i + 1, part));
    }
    match problems.is_empty() {
        true => Ok(()),
        false => Err(problems),
    }
}

fn part_problems(n: usize, part: &Measure) -> Vec<Problem> {
    let mut problems = Vec::new();
    if let Some(repo) = part.repo() {
        if github_repo(repo).is_none() {
            problems.push(Problem::Repo(n));
        }
    }
    match part {
        Measure::Ci {
            workflow,
            branch,
            want,
            ..
        } => {
            if branch.trim().is_empty() {
                problems.push(Problem::Branch(n));
            }
            if workflow.as_deref().is_some_and(|w| w.trim().is_empty()) {
                problems.push(Problem::Workflow(n));
            }
            if let Want::Minutes { at_most, .. } = want {
                if !at_most.is_finite() || *at_most <= 0.0 {
                    problems.push(Problem::Bound(n));
                }
            }
            if !(1..=MAX_RUNS).contains(&want.runs()) {
                problems.push(Problem::Runs(n));
            }
        }
        Measure::PrMerged { pr, .. } | Measure::ChecksGreen { pr, .. } => {
            let unnamed = match pr {
                PrPick::Number(n) => *n == 0,
                PrPick::Head(head) => head.trim().is_empty(),
            };
            if unnamed {
                problems.push(Problem::Pull(n));
            }
        }
        Measure::Judged { evidence } => {
            let said = evidence.trim().chars().count();
            if said == 0 || said > WORDS_MAX {
                problems.push(Problem::Evidence(n));
            }
        }
    }
    problems
}

/// How each part of `r` stands from its `readings` and the checked
/// checkpoints among `facts`.
///
/// A measured part's best is its reading nearest the target; it came
/// nearer when a reading beat the best before it, or met it; and it is met
/// as its newest read says. A judged part comes nearer with each checked
/// checkpoint toward it, and is never met by a reading: only the person
/// meets it.
pub fn progress(r: &GoalResult, readings: &[Reading], facts: &[Fact]) -> Progress {
    Progress {
        parts: r
            .parts
            .iter()
            .enumerate()
            .map(|(i, part)| match part.measured() {
                true => measured_progress(part, i as u16, readings),
                false => PartProgress {
                    improved_at: facts
                        .iter()
                        .filter(|f| {
                            f.kind == FactKind::Checkpoint && f.checked && f.part == Some(i as u16)
                        })
                        .map(|f| f.at)
                        .max(),
                    ..PartProgress::default()
                },
            })
            .collect(),
    }
}

fn measured_progress(part: &Measure, n: u16, readings: &[Reading]) -> PartProgress {
    let mut own: Vec<&Reading> = readings.iter().filter(|r| r.part == n).collect();
    own.sort_by_key(|r| r.at);
    own.into_iter().fold(PartProgress::default(), |mut p, r| {
        p.newest = Some(r.clone());
        match r.value {
            None => {
                p.failing_since = p.failing_since.or(Some(r.at));
                p.failing += 1;
            }
            Some(v) => {
                p.failing = 0;
                p.failing_since = None;
                p.met = r.met;
                let beats = p
                    .best
                    .as_ref()
                    .and_then(|b| b.value)
                    .is_none_or(|b| part.nearer(v, b));
                if beats || r.met {
                    p.improved_at = Some(r.at);
                }
                if beats {
                    p.best = Some(r.clone());
                }
            }
        }
        p
    })
}

/// Whether every part is met.
pub fn met(p: &Progress) -> bool {
    !p.parts.is_empty() && p.parts.iter().all(|part| part.met)
}

/// A part with no progress toward its target for a spell of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stall {
    pub part: u16,
    /// When it last came nearer, or the goal was agreed.
    pub since: DateTime<Utc>,
    /// Which spell of it this is: 0 from one window, 1 from two, 2 from
    /// four, 3 from eight, then one more every eight.
    pub spell: u32,
}

/// The spell `stalled` is in, in windows of `window`: none under one.
pub fn spell_of(stalled: chrono::Duration, window: chrono::Duration) -> Option<u32> {
    let window = window.num_milliseconds();
    if window <= 0 {
        return None;
    }
    match stalled.num_milliseconds() / window {
        n if n < 1 => None,
        n if n < 2 => Some(0),
        n if n < 4 => Some(1),
        n if n < 8 => Some(2),
        n => Some(u32::try_from(n / 8 + 2).unwrap_or(u32::MAX)),
    }
}

/// Each part of `p`, agreed at `agreed_at`, that isn't met and hasn't come
/// nearer for at least `window` by `now`: on its own, so one part coming
/// nearer never hides another stuck.
pub fn stalls(
    p: &Progress,
    agreed_at: DateTime<Utc>,
    now: DateTime<Utc>,
    window: chrono::Duration,
) -> Vec<Stall> {
    p.parts
        .iter()
        .enumerate()
        .filter(|(_, part)| !part.met)
        .filter_map(|(i, part)| {
            let since = part.improved_at.map_or(agreed_at, |at| at.max(agreed_at));
            Some(Stall {
                part: u16::try_from(i).ok()?,
                since,
                spell: spell_of(now - since, window)?,
            })
        })
        .collect()
}

/// What a goal names on GitHub, by repository: the branches its work is
/// on, and its pull requests by number.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GoalRefs {
    pub branches: HashSet<(String, String)>,
    pub pulls: HashSet<(String, u64)>,
}

impl GoalRefs {
    /// What `r` names, with the pull requests its `readings` found by their
    /// head branch.
    pub fn of(r: &GoalResult, readings: &[Reading]) -> Self {
        let key = |repo: &str| repo.trim().to_ascii_lowercase();
        let mut refs = Self::default();
        for (i, part) in r.parts.iter().enumerate() {
            match part {
                Measure::Ci { repo, branch, .. } => {
                    refs.branches.insert((key(repo), branch.trim().to_string()));
                }
                Measure::PrMerged { repo, pr } | Measure::ChecksGreen { repo, pr } => {
                    match pr {
                        PrPick::Number(n) => {
                            refs.pulls.insert((key(repo), *n));
                        }
                        PrPick::Head(head) => {
                            refs.branches.insert((key(repo), head.trim().to_string()));
                        }
                    }
                    refs.pulls.extend(
                        readings
                            .iter()
                            .filter(|reading| usize::from(reading.part) == i)
                            .flat_map(|reading| &reading.evidence)
                            .filter_map(|e| match e {
                                Evidence::Pull { repo, number } => Some((key(repo), *number)),
                                _ => None,
                            }),
                    );
                }
                Measure::Judged { .. } => {}
            }
        }
        refs
    }

    /// Whether pull request `number` of `repo` is one of the goal's.
    pub fn names_pull(&self, repo: &str, number: u64) -> bool {
        self.pulls
            .contains(&(repo.trim().to_ascii_lowercase(), number))
    }

    /// Whether `branch` of `repo` is one the goal's work is on.
    pub fn names_branch(&self, repo: &str, branch: &str) -> bool {
        self.branches
            .contains(&(repo.trim().to_ascii_lowercase(), branch.trim().to_string()))
    }

    /// The goal's branches on `repo`.
    pub fn branches_of<'a>(&'a self, repo: &str) -> impl Iterator<Item = &'a str> {
        let repo = repo.trim().to_ascii_lowercase();
        self.branches
            .iter()
            .filter(move |(r, _)| *r == repo)
            .map(|(_, b)| b.as_str())
    }

    /// Whether the goal names nothing on GitHub: any work shown there is
    /// taken as its own.
    pub fn is_empty(&self) -> bool {
        self.branches.is_empty() && self.pulls.is_empty()
    }

    /// What the goal names, in words, in a steady order: "main of
    /// acme/api, PR #12 of acme/api".
    pub fn words(&self) -> String {
        let mut branches: Vec<_> = self.branches.iter().collect();
        branches.sort();
        let mut pulls: Vec<_> = self.pulls.iter().collect();
        pulls.sort();
        branches
            .into_iter()
            .map(|(repo, b)| format!("{b} of {repo}"))
            .chain(
                pulls
                    .into_iter()
                    .map(|(repo, n)| format!("PR #{n} of {repo}")),
            )
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// What a goal's boss was told of it already, kept with its plan so that
/// nothing is said twice, a restart between or not.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalSaid {
    /// Each part's stall, by when it began, and the furthest spell of it
    /// said.
    #[serde(default)]
    pub spells: BTreeMap<u16, (DateTime<Utc>, u32)>,
    /// Each part's run of failed reads said, by when it began.
    #[serde(default)]
    pub failing: BTreeMap<u16, DateTime<Utc>>,
    /// The boss's messages it was told hadn't gone in, newest last.
    #[serde(default)]
    pub undelivered: Vec<Uuid>,
    /// The stall, by when it began, and its spell, the person's roundup was
    /// last given a note of.
    #[serde(default)]
    pub noted: Option<(DateTime<Utc>, u32)>,
}

/// The most of the boss's undelivered messages remembered as said.
pub const UNDELIVERED_KEPT: usize = 50;

impl GoalSaid {
    /// Whether `s`, or a later spell of the same stall, was said.
    pub fn told(&self, s: &Stall) -> bool {
        self.spells
            .get(&s.part)
            .is_some_and(|(since, spell)| *since == s.since && *spell >= s.spell)
    }

    /// Notes each of `stalls` as said.
    pub fn tell(&mut self, stalls: &[Stall]) {
        for s in stalls {
            if !self.told(s) {
                self.spells.insert(s.part, (s.since, s.spell));
            }
        }
    }

    /// Notes the boss's message `id` as said to have waited.
    pub fn undelivered_told(&mut self, id: Uuid) {
        self.undelivered.push(id);
        let over = self.undelivered.len().saturating_sub(UNDELIVERED_KEPT);
        self.undelivered.drain(..over);
    }
}

/// Where each of a goal's branches and pull requests stood as it was
/// agreed, by [`head_key`], and as each judged part's last checkpoint left
/// it, by [`part_head_key`]: what work shown toward that part must come
/// after. [`ABSENT`] for one that wasn't there then, [`UNREAD`] for one
/// still to be read.
pub type Heads = BTreeMap<String, String>;

/// What a head GitHub says isn't there is kept as: none, so everything on
/// it is new.
pub const ABSENT: &str = "";

/// What a head GitHub couldn't be asked for is kept as until it is read
/// again: no head yet.
pub const UNREAD: &str = "unread";

/// What a part's head is kept by before its own key.
const PART_HEAD: &str = "part ";

/// What a head of branch `branch` of `repo` is kept by, `branch` being
/// `#n` for pull request `n`'s.
pub fn head_key(repo: &str, branch: &str) -> String {
    format!("{} {branch}", repo.trim().to_ascii_lowercase())
}

/// What head `key` is kept by for judged part `part`, once a checkpoint
/// toward it moved it.
pub fn part_head_key(part: u16, key: &str) -> String {
    format!("{PART_HEAD}{part}: {key}")
}

/// Whether `heads` hold where the goal's work stood as it was agreed.
pub fn heads_agreed(heads: &Heads) -> bool {
    heads.keys().any(|k| !k.starts_with(PART_HEAD))
}

/// Whether head `key` is still to be read for where the goal's work stood
/// as it was agreed (`heads`): any, before that was read; one [`UNREAD`]
/// then; none that came later.
pub fn head_due(heads: &Heads, key: &str) -> bool {
    !heads_agreed(heads) || heads.get(key).is_some_and(|sha| sha == UNREAD)
}

/// Where head `key` stood for judged part `part` in `heads`: as its last
/// checkpoint left it, else as the goal was agreed.
pub fn head_for<'a>(heads: &'a Heads, part: u16, key: &str) -> Option<&'a str> {
    [part_head_key(part, key), key.to_string()]
        .iter()
        .filter_map(|k| heads.get(k))
        .map(String::as_str)
        .find(|sha| *sha != ABSENT && *sha != UNREAD)
}

/// The stall leading `stalls`: the furthest in its spells, the longest
/// stuck of those.
pub fn leading(stalls: &[Stall]) -> Option<&Stall> {
    stalls
        .iter()
        .max_by(|a, b| a.spell.cmp(&b.spell).then(b.since.cmp(&a.since)))
}

/// Whether a goal stalled as `stalls`, its boss told `said`, wakes it now:
/// once each spell of the leading stall, with every part stalled then, so
/// that parts stalled from different times never wake it apart.
pub fn wakes(stalls: &[Stall], said: &GoalSaid) -> bool {
    leading(stalls).is_some_and(|s| !said.told(s))
}

/// What Desk found of one piece of a checkpoint's evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    /// It is there, and part of the goal's own work: on its branches or
    /// its pull requests.
    OnGoal,
    /// It is there, but other work.
    OffGoal,
    /// It isn't there as it says, or couldn't be looked up.
    Missing,
    /// It may be there, but Desk can't look up where the goal's work is to
    /// place it.
    Unplaced,
    /// It is the goal's work, but from before the goal was agreed or its
    /// part's last checkpoint.
    Old,
}

/// Why a checkpoint isn't accepted toward its judged part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotShown {
    /// It shows no evidence.
    Nothing,
    /// This piece of it isn't there as it says.
    Missing(Evidence),
    /// This piece of it is other work than the goal's.
    OffGoal(Evidence),
    /// This piece of it can't be placed on the goal's work.
    Unplaced(Evidence),
    /// This piece of it was there before the goal was agreed, or before
    /// its part's last checkpoint.
    Old(Evidence),
    /// Every piece was shown toward the part already.
    NothingNew,
}

/// Whether a checkpoint showing `evidence`, each piece as `found` says,
/// moves its judged part on from `earlier`, what its accepted checkpoints
/// showed: every piece there and the goal's own work, and one at least
/// new.
pub fn checkpoint_shows(
    evidence: &[Evidence],
    found: &[Found],
    earlier: &[&Evidence],
) -> Result<(), NotShown> {
    if evidence.is_empty() {
        return Err(NotShown::Nothing);
    }
    for (e, f) in evidence.iter().zip(found) {
        match f {
            Found::OnGoal => {}
            Found::OffGoal => return Err(NotShown::OffGoal(e.clone())),
            Found::Missing => return Err(NotShown::Missing(e.clone())),
            Found::Unplaced => return Err(NotShown::Unplaced(e.clone())),
            Found::Old => return Err(NotShown::Old(e.clone())),
        }
    }
    if found.len() < evidence.len() {
        return Err(NotShown::Missing(evidence[found.len()].clone()));
    }
    match evidence.iter().any(|e| !earlier.contains(&e)) {
        true => Ok(()),
        false => Err(NotShown::NothingNew),
    }
}

/// How each part stands, in a line: "CI on main of acme/api: 22 minutes,
/// for at most 5 minutes a job, 3 runs in a row".
pub fn progress_line(r: &GoalResult, p: &Progress) -> String {
    r.parts
        .iter()
        .zip(&p.parts)
        .map(|(part, now)| format!("{}: {}", part.label(), now_words(part, now)))
        .collect::<Vec<_>>()
        .join("; ")
}

fn now_words(part: &Measure, now: &PartProgress) -> String {
    if !part.measured() {
        return match now.improved_at {
            Some(at) => format!("judged, last shown {}", at.format("%Y-%m-%d %H:%M UTC")),
            None => "judged, nothing shown yet".into(),
        };
    }
    let read = match (&now.newest, &now.best) {
        (Some(newest), _) if newest.value.is_none() => format!(
            "not read: {}",
            newest.failed.as_deref().unwrap_or("it couldn't be read")
        ),
        (Some(newest), _) => newest.words.clone(),
        (None, _) => "not read yet".into(),
    };
    match now.met {
        true => format!("{read}, met"),
        false => format!("{read}, for {}", part.target()),
    }
}

/// What Desk says once a goal is saved, in one sentence carrying its first
/// reading, and what it can't measure: "I've saved this goal: … It's done
/// when …; now 22 minutes. I can't measure “…” automatically, so I'll judge
/// it from …".
pub fn read_back(goal: &str, r: &GoalResult, first: &Progress) -> String {
    let done = r.words.trim().trim_end_matches(['.', '!', ';']);
    let done = done
        .strip_prefix("Done when ")
        .or_else(|| done.strip_prefix("done when "))
        .unwrap_or(done);
    let measured: Vec<String> = r
        .parts
        .iter()
        .zip(&first.parts)
        .filter(|(part, _)| part.measured())
        .map(
            |(part, now)| match r.parts.iter().filter(|p| p.measured()).count() {
                1 => reading_words(now),
                _ => format!("{} {}", part.label(), reading_words(now)),
            },
        )
        .collect();
    let now = match (measured.is_empty(), met(first)) {
        (_, true) => "; it's met already".to_string(),
        (true, false) => String::new(),
        (false, false) => format!("; now {}", measured.join(", ")),
    };
    let judged: Vec<String> = r
        .parts
        .iter()
        .filter_map(|part| match part {
            Measure::Judged { evidence } => Some(evidence.trim().to_string()),
            _ => None,
        })
        .collect();
    let judged = match judged.is_empty() {
        true => String::new(),
        false => format!(
            " I can't measure {} automatically, so I'll judge it from the work's evidence.",
            judged
                .iter()
                .map(|e| format!("“{e}”"))
                .collect::<Vec<_>>()
                .join(" or ")
        ),
    };
    format!(
        "I've saved this goal: {}. It's done when {done}{now}.{judged}",
        goal.trim().trim_end_matches(['.', '!'])
    )
}

fn reading_words(now: &PartProgress) -> String {
    match &now.newest {
        Some(r) if r.value.is_some() => r.words.clone(),
        Some(r) => format!(
            "unread ({})",
            r.failed.as_deref().unwrap_or("it couldn't be read")
        ),
        None => "unread".into(),
    }
}

/// How the person's quoted words are found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Said {
    /// Agreeing a goal or starting its work, with no question of the
    /// boss's to answer: the whole of one of their messages, or at least 2
    /// words and 6 characters of it.
    Agreeing,
    /// Changing an agreed goal, or saying it is met, unprompted: at least 4
    /// words and 15 characters of one message.
    Changing,
    /// Answering the boss's question that proposed the change: its
    /// proposal says what the answer means, so their whole reply counts,
    /// however short ("yes"), as does at least 2 words and 6 characters of
    /// a longer one.
    Answering,
}

impl Said {
    /// The fewest words and characters a quote of part of a message has.
    pub fn floor(self) -> (usize, usize) {
        match self {
            Self::Agreeing | Self::Answering => (2, 6),
            Self::Changing => (4, 15),
        }
    }

    /// Whether the whole of a message counts, however short.
    fn whole_counts(self) -> bool {
        matches!(self, Self::Agreeing | Self::Answering)
    }
}

/// Why the person's quoted words weren't found as they must be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotSaid {
    /// Part of a message, under the floor of what it is for: the fewest
    /// words and characters.
    TooShort { words: usize, chars: usize },
    /// Found, but only in a message older than it may be, the newest of
    /// them written at this time.
    TooOld(DateTime<Utc>),
    /// Only part of a reply, for an answer: a part can say other than the
    /// whole does ("No — yes, I see why, but not now").
    NotWhole,
    /// In none of their messages, word for word.
    NotFound,
}

/// Text as it is compared: compatibility forms folded (NFKC), curly
/// quotes straightened, lower case, whitespace collapsed, and trailing
/// punctuation trimmed.
pub fn normalise(text: &str) -> String {
    let folded: String = text
        .nfkc()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{2032}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{2033}' => '"',
            c => c,
        })
        .flat_map(char::to_lowercase)
        .collect();
    folded
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(|c: char| c.is_ascii_punctuation() || c == '…')
        .to_string()
}

/// `text`'s words as compared: [`normalise`]d, each without the
/// punctuation around it.
fn compared_words(text: &str) -> Vec<String> {
    normalise(text)
        .split(' ')
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
        .filter(|w| !w.is_empty())
        .collect()
}

/// When the person said `quote`, word for word in one of their
/// `messages` newer than `since`, as `said` asks: the newest such
/// message's time, or why it isn't one.
pub fn said_by(
    messages: &[(DateTime<Utc>, String)],
    quote: &str,
    since: DateTime<Utc>,
    said: Said,
) -> Result<DateTime<Utc>, NotSaid> {
    let quoted = compared_words(quote);
    if quoted.is_empty() {
        return Err(NotSaid::NotFound);
    }
    let (least_words, least_chars) = said.floor();
    let short = quoted.len() < least_words || quoted.join(" ").chars().count() < least_chars;
    let mut found = None;
    let (mut too_short, mut not_whole) = (false, false);
    for (at, text) in messages {
        let words = compared_words(text);
        let whole = words == quoted;
        let part = words.windows(quoted.len()).any(|w| w == quoted.as_slice());
        // An answer is the whole reply, however short: never a part of it.
        let counts = match said {
            Said::Answering => whole,
            _ => (whole && said.whole_counts()) || (part && !short),
        };
        let newer = part && !counts && *at > since;
        not_whole |= newer && said == Said::Answering;
        too_short |= newer && said != Said::Answering;
        if counts {
            found = found.max(Some(*at));
        }
    }
    match found {
        Some(at) if at > since => Ok(at),
        _ if not_whole => Err(NotSaid::NotWhole),
        _ if too_short => Err(NotSaid::TooShort {
            words: least_words,
            chars: least_chars,
        }),
        Some(at) => Err(NotSaid::TooOld(at)),
        None => Err(NotSaid::NotFound),
    }
}

/// Whether the person's message `text` says enough to stand on its own,
/// as an unprompted change must ([`Said::Changing`]'s floor): a shorter one
/// ("ok", "yes go ahead") only answers what was said just before it.
pub fn stands_alone(text: &str) -> bool {
    let words = compared_words(text);
    let (least_words, least_chars) = Said::Changing.floor();
    words.len() >= least_words && words.join(" ").chars().count() >= least_chars
}

/// Whether the person's message `text` holds `goal` in their own words:
/// the goal they change it to, said by them.
pub fn says_goal(text: &str, goal: &str) -> bool {
    let words = compared_words(goal);
    !words.is_empty()
        && compared_words(text)
            .windows(words.len())
            .any(|w| w == words.as_slice())
}

/// The prefix the person's answer on an item's buttons is posted with in
/// their DM: "My answer to “…”: ".
pub const ANSWER_LEAD: &str = "My answer to “";

/// What the person answered in `text`, a message of theirs: after
/// [`ANSWER_LEAD`]'s question when it is an answer posted from an item,
/// so the question's own words are never read as theirs; else all of it.
pub fn answered_text(text: &str) -> &str {
    match text.strip_prefix(ANSWER_LEAD) {
        Some(rest) => rest.rsplit_once("”: ").map_or(text, |(_, answer)| answer),
        None => text,
    }
}

/// Whether `applying` is the change `asked` proposed: the same goal in
/// other spacing or case, the same result, the same claim that it is met.
pub fn same_proposal(asked: &Proposal, applying: &Proposal) -> bool {
    let goal = |p: &Proposal| p.goal.as_deref().map(normalise);
    goal(asked) == goal(applying)
        && asked.result == applying.result
        && asked.met.is_some() == applying.met.is_some()
}

/// Whether a plan whose goal is `goal`, measured by `result`, holds all
/// that `asked` proposed: the person approving it on the plan's card
/// settles that question.
pub fn approved_covers(asked: &Proposal, goal: &str, result: Option<&GoalResult>) -> bool {
    !asked.is_empty()
        && asked.met.is_none()
        && asked
            .goal
            .as_deref()
            .is_none_or(|g| normalise(g) == normalise(goal))
        && asked.result.as_ref().is_none_or(|r| Some(r) == result)
}

/// How soon after the person's turn the boss's message to a session is
/// work they asked for.
pub const SENT_ON_ASK: chrono::Duration = chrono::Duration::minutes(30);

/// The sessions the boss sent work to and holds no agreed goal for: each
/// of `runs` (live, started or handed to the boss) that the boss sent a
/// line message (`sends`) within [`SENT_ON_ASK`] of one of the person's
/// own turns (`turns`), so as work they asked for, and that no agreed goal
/// holds (`on_goals`). In `runs`' order.
pub fn unsaved_goals(
    runs: &[Uuid],
    sends: &[(Uuid, DateTime<Utc>)],
    turns: &[DateTime<Utc>],
    on_goals: &HashSet<Uuid>,
) -> Vec<Uuid> {
    let asked = |sent: &DateTime<Utc>| turns.iter().any(|t| t < sent && *sent <= *t + SENT_ON_ASK);
    runs.iter()
        .filter(|s| !on_goals.contains(s))
        .filter(|s| sends.iter().any(|(to, at)| to == *s && asked(at)))
        .copied()
        .collect()
}

/// The boss's open goals and the newest facts and checkpoints on them, as
/// its context says them: a line per agreed goal, the drafts apart, then
/// the facts; none with no open goal. `goals` is each open goal with its
/// progress, `facts` theirs newest first.
pub fn goals_block(goals: &[(Plan, Progress)], facts: &[Fact]) -> Option<String> {
    let line = |(p, progress): &(Plan, Progress)| {
        let r = p.result.as_ref()?;
        Some(format!(
            "- “{}” (plan_id {}) · {} · now: {} · {}",
            crate::crew::one_line(&p.goal, 300),
            p.id,
            crate::crew::one_line(&r.words, WORDS_MAX),
            progress_line(r, progress),
            match met(progress) {
                true => "met",
                false => "unmet",
            }
        ))
    };
    let agreed: Vec<String> = goals
        .iter()
        .filter(|(p, _)| p.approved_revision.is_some())
        .filter_map(line)
        .collect();
    let drafts: Vec<String> = goals
        .iter()
        .filter(|(p, _)| p.approved_revision.is_none())
        .filter_map(line)
        .collect();
    if agreed.is_empty() && drafts.is_empty() {
        return None;
    }
    let mut out = vec![format!(
        "{GOALS_MARKER} The goals you agreed with the person. Only a met reading or the person's \
         own words close one."
    )];
    out.extend(agreed);
    if !drafts.is_empty() {
        out.push("Proposed, not yet agreed (the person approves them on the board, or agrees them in your DM):".into());
        out.extend(drafts);
    }
    let shown: Vec<String> = facts
        .iter()
        .take(FACTS_SHOWN)
        .map(|f| {
            format!(
                "- {} {}{}: {}{}",
                f.at.format("%Y-%m-%d %H:%M UTC"),
                f.kind.as_str(),
                f.part
                    .map(|n| format!(" (part {})", n + 1))
                    .unwrap_or_default(),
                crate::crew::clip(
                    &f.text.split_whitespace().collect::<Vec<_>>().join(" "),
                    FACT_MAX
                ),
                match f.checked {
                    true => "",
                    false => " (unchecked)",
                }
            )
        })
        .collect();
    if !shown.is_empty() {
        out.push("Desk's goal facts, newest first:".into());
        out.extend(shown);
    }
    // One passage, so a transcript never holds part of it back.
    Some(out.join("\n"))
}

#[cfg(test)]
#[path = "goal_tests.rs"]
mod tests;
