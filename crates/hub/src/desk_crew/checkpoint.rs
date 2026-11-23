//! A checkpoint toward a judged part is kept only once it shows the goal's
//! own work moving it on since it was agreed (designs/boss-keeps-goals.md
//! §7.3): every piece of its evidence found on GitHub as the person, one at
//! least new for that part, each on the goal's work and new since the goal
//! was agreed or its part's last checkpoint. On the goal's work is a commit
//! reachable from one of the goal's branches or the head of one of its pull
//! requests, one of those pull requests or one into or from those branches,
//! or a run on those branches; a goal that names nothing on GitHub takes a
//! pull request or run in its tickets' repositories. New is a commit not
//! already in where its branch or pull request stood then ([`Heads`]): the
//! commit itself, a run's, or a pull request's head (or the pull request
//! merged since). Without such a head kept, a commit is new, and a pull
//! request or run is new when it was opened, merged or made since. Heads
//! are kept as the goal is checked, and for its part whenever a checkpoint
//! moves it. What it refuses names what it would take instead.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;
use workforce_desk::goal::{
    self, head_for, head_key, part_head_key, Evidence, FactKind, Found, GoalRefs, GoalResult,
    Heads, NotShown, Reading,
};
use workforce_desk::PlanDetail;

use super::github::{each_as_person, GithubAccess};
use super::{AgentService, CrewHub};

/// Whether `r` is a branch as GitHub's compare takes it in its path: git's
/// own characters alone, nothing that steers the request elsewhere.
pub(super) fn plain_ref(r: &str) -> bool {
    !r.is_empty()
        && !r.starts_with(['/', '-', '.'])
        && !r.ends_with(['/', '.'])
        && !r.contains("..")
        && r.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

/// Whether `sha` is a commit's id, whole or shortened.
fn is_sha(sha: &str) -> bool {
    (7..=40).contains(&sha.len()) && sha.chars().all(|c| c.is_ascii_hexdigit())
}

/// `e` in words, as a refusal names it.
fn evidence_words(e: &Evidence) -> String {
    match e {
        Evidence::Run { repo, id } => format!("Run {id} of {repo}"),
        Evidence::Pull { repo, number } => format!("PR #{number} of {repo}"),
        Evidence::Commit { repo, sha } => format!("Commit {} of {repo}", &sha[..sha.len().min(7)]),
        Evidence::Ticket { key } => format!("Ticket {key}"),
    }
}

/// What a checkpoint toward a goal may show: work on what it names
/// (`refs`), or in its tickets' repositories (`repos`) when it names
/// nothing, new since `since` for judged part `part`, by where its work
/// stood then (`heads`).
pub(super) struct Taken {
    pub refs: GoalRefs,
    pub repos: BTreeSet<String>,
    pub since: DateTime<Utc>,
    pub heads: Heads,
    pub part: u16,
}

impl Taken {
    /// Where head `key` stood as this part last moved, when it is kept.
    fn then(&self, key: &str) -> Option<&str> {
        head_for(&self.heads, self.part, key)
    }
}

/// `repos` in words.
fn repos_words(repos: &BTreeSet<String>) -> String {
    match repos.is_empty() {
        true => {
            "which Desk can't tell: none of its tickets names one, and none has a session".into()
        }
        false => repos.iter().cloned().collect::<Vec<_>>().join(", "),
    }
}

/// What a checkpoint toward a judged part of a goal naming `refs`, its
/// tickets in `repos`, is to show, as the boss is told to record one.
pub(super) fn shows_what(refs: &GoalRefs, repos: &BTreeSet<String>) -> String {
    match refs.is_empty() {
        true => format!(
            "a pull request or run since the goal was agreed, in the plan's repositories ({})",
            repos_words(repos)
        ),
        false => format!(
            "commits, runs or a pull request on the goal's work ({}) since the goal was agreed or its last checkpoint; work on another branch counts once a pull request carries it there",
            refs.words()
        ),
    }
}

/// What the boss is told of a checkpoint that doesn't show its judged part
/// moving, `why`, toward a goal taking `t`.
pub(super) fn not_shown_line(why: &NotShown, t: &Taken) -> String {
    let work = t.refs.words();
    let what = shows_what(&t.refs, &t.repos);
    match why {
        NotShown::Nothing => {
            format!("A checkpoint shows what moved the goal on: cite {what} (evidence).")
        }
        NotShown::Missing(e) => format!(
            "{} isn't on GitHub as it says, or couldn't be looked up: check it, and record the checkpoint again.",
            evidence_words(e)
        ),
        NotShown::OffGoal(Evidence::Ticket { key }) => {
            format!("Ticket {key} shows no work on GitHub: cite {what} instead.")
        }
        NotShown::OffGoal(e) if t.refs.is_empty() => format!(
            "{} isn't in the plan's repositories ({}): cite {what}.",
            evidence_words(e),
            repos_words(&t.repos)
        ),
        NotShown::OffGoal(e @ Evidence::Commit { .. }) => format!(
            "{} isn't on the goal's work ({work}): open a pull request from its branch into one of the goal's branches and cite that, or cite a pull request the goal names.",
            evidence_words(e)
        ),
        NotShown::OffGoal(e @ Evidence::Pull { .. }) => format!(
            "{} isn't the goal's: only a pull request it names, or one into or from its branches, counts ({work}).",
            evidence_words(e)
        ),
        NotShown::OffGoal(e) => format!(
            "{} didn't run on the goal's work ({work}): cite a run on its branches, or the pull request that carries the work there.",
            evidence_words(e)
        ),
        NotShown::Unplaced(e) if t.refs.is_empty() => format!(
            "Desk can't tell when {} was made, as the goal names no branch: cite the pull request that carries it instead.",
            evidence_words(e)
        ),
        NotShown::Unplaced(e) => format!(
            "Desk can't look up the goal's branch to place {}: cite the pull request that carries it instead.",
            evidence_words(e)
        ),
        NotShown::Old(e) => format!(
            "{} was there before the goal was agreed or its part's last checkpoint: cite work done since.",
            evidence_words(e)
        ),
        NotShown::NothingNew => "All it shows was shown toward this part already: record a checkpoint once something new moves it on.".into(),
    }
}

/// The readings `detail`'s goal was last read from, each part's newest and
/// best.
pub(super) fn readings_of(detail: &PlanDetail) -> Vec<Reading> {
    detail
        .goal
        .iter()
        .flat_map(|p| &p.parts)
        .flat_map(|p| p.newest.iter().chain(&p.best))
        .cloned()
        .collect()
}

impl CrewHub {
    /// Whether a checkpoint showing `evidence` toward judged part `part` of
    /// goal plan `detail`, its result `r`, moves it on, looked up on GitHub
    /// as the person: its evidence as GitHub names it (each commit by its
    /// whole id), or refused in the boss's words. Taken, where the goal's
    /// work stands is kept for that part, as what its next must be new
    /// against.
    pub(super) async fn checkpoint_shown(
        &self,
        service: &AgentService,
        detail: &PlanDetail,
        r: &GoalResult,
        part: u16,
        evidence: &[Evidence],
    ) -> Result<Vec<Evidence>, String> {
        let plan = &detail.summary.plan;
        let checkpoints: Vec<&goal::Fact> = detail
            .facts
            .iter()
            .filter(|f| f.kind == FactKind::Checkpoint && f.checked && f.part == Some(part))
            .collect();
        let earlier: Vec<&Evidence> = checkpoints.iter().flat_map(|f| &f.evidence).collect();
        let taken = Taken {
            refs: GoalRefs::of(r, &readings_of(detail)),
            repos: self.work_repos(detail),
            since: checkpoints
                .iter()
                .map(|f| f.at)
                .max()
                .or(plan.agreed_at)
                .unwrap_or(plan.created_at),
            heads: self
                .desk
                .store()
                .goal_heads(plan.id)
                .await
                .unwrap_or_default(),
            part,
        };
        let access = super::goal::access(service);
        let Shown {
            found,
            evidence: shown,
            heads,
        } = self
            .as_person(service, found_on_goal(&access, &taken, evidence))
            .await;
        goal::checkpoint_shows(&shown, &found, &earlier)
            .map_err(|why| not_shown_line(&why, &taken))?;
        let now = self
            .as_person(service, heads_now(&access, &taken.refs, &Heads::new()))
            .await;
        let moved: Heads = heads
            .into_iter()
            .chain(now)
            .map(|(key, sha)| (part_head_key(part, &key), sha))
            .collect();
        self.keep_heads(plan.id, &moved, true).await;
        Ok(shown)
    }

    /// Where goal plan `plan`'s work, naming `refs`, stood, kept for each
    /// branch and pull request not kept yet, as read now as the person:
    /// what was there as it was agreed.
    pub(super) async fn heads_kept(&self, service: &AgentService, plan: Uuid, refs: &GoalRefs) {
        let Ok(kept) = self.desk.store().goal_heads(plan).await else {
            return;
        };
        let access = super::goal::access(service);
        let now = self
            .as_person(service, heads_now(&access, refs, &kept))
            .await;
        self.keep_heads(plan, &now, false).await;
    }

    /// Keeps `heads` with goal plan `plan`'s, over those it names when
    /// `over`, else only where none is kept.
    async fn keep_heads(&self, plan: Uuid, heads: &Heads, over: bool) {
        if heads.is_empty() {
            return;
        }
        if let Err(e) = self.desk.store().keep_goal_heads(plan, heads, over).await {
            tracing::info!(plan = %plan, error = %e, "where a goal's work stands wasn't kept");
        }
    }

    /// The repositories goal plan `detail`'s tickets work in: what a goal
    /// that names nothing on GitHub takes work from. Each by the one it
    /// names, else its session's here, else its folder's here.
    pub(super) fn work_repos(&self, detail: &PlanDetail) -> BTreeSet<String> {
        detail
            .tickets
            .iter()
            .filter_map(|t| {
                t.repo
                    .as_deref()
                    .and_then(workforce_desk::charge::repo_name)
                    .map(|r| r.path)
                    .or_else(|| self.desk.repo_of_session(t.ticket.ticket.session()?))
                    .or_else(|| match t.machine {
                        Some(_) => None,
                        None => workforce_desk::Desk::repo_of_folder(t.folder.as_deref()?),
                    })
            })
            .collect()
    }
}

/// Where each of `refs`' branches and pull requests not in `kept` stands
/// now, as the person in scope reads it, by [`head_key`]: none it couldn't
/// read.
async fn heads_now(access: &GithubAccess<'_>, refs: &GoalRefs, kept: &Heads) -> Heads {
    let mut asks: Vec<(String, (&str, Value))> = refs
        .branches
        .iter()
        .filter(|(_, b)| plain_ref(b))
        .filter_map(|(repo, b)| {
            let (owner, name) = goal::github_repo(repo)?;
            let args = json!({ "owner": owner, "repo": name, "base": b, "head": b });
            Some((head_key(repo, b), ("github.compare", args)))
        })
        .chain(refs.pulls.iter().filter_map(|(repo, n)| {
            let (owner, name) = goal::github_repo(repo)?;
            let args = json!({ "owner": owner, "repo": name, "number": n });
            Some((head_key(repo, &format!("#{n}")), ("github.get_pull", args)))
        }))
        .filter(|(key, _)| !kept.contains_key(key))
        .collect();
    asks.sort_by(|a, b| a.0.cmp(&b.0));
    if asks.is_empty() {
        return Heads::new();
    }
    let calls = asks.iter().map(|(_, c)| c.clone()).collect();
    let Ok(answered) = each_as_person(access, calls).await else {
        return Heads::new();
    };
    asks.into_iter()
        .zip(answered)
        .filter_map(|((key, _), got)| {
            let got = got.ok()?;
            let sha = got["base_sha"].as_str().or(got["head_sha"].as_str())?;
            is_sha(sha).then(|| (key, sha.to_string()))
        })
        .collect()
}

/// A goal's pull request as GitHub said it: its repository, number, head
/// branch, and its head's commit.
struct GoalPull {
    repo: String,
    number: u64,
    head: Option<String>,
    head_sha: Option<String>,
}

impl GoalPull {
    fn of(repo: &str, number: u64, got: &Value) -> Self {
        let text = |k: &str| got[k].as_str().map(str::to_string);
        Self {
            repo: repo.to_string(),
            number,
            head: text("head"),
            head_sha: text("head_sha").filter(|s| is_sha(s)),
        }
    }
}

/// Whether GitHub's time `at` is at or after `since`.
fn fresh(at: &Value, since: DateTime<Utc>) -> bool {
    at.as_str()
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .is_some_and(|t| t >= since)
}

/// What a commit is to where its branch or pull request stood, as GitHub's
/// compare from there to it says: new when not already in it (`ahead`, or
/// `diverged` after a rebase or a force-push), else there already.
fn newer(got: Option<&Value>) -> Found {
    match got.and_then(|g| g["status"].as_str()) {
        Some("ahead" | "diverged") => Found::OnGoal,
        Some(_) => Found::Old,
        None => Found::Missing,
    }
}

/// What is found of a checkpoint's evidence: each piece as found, as
/// GitHub names it, and the heads of its pull requests by [`head_key`].
struct Shown {
    found: Vec<Found>,
    evidence: Vec<Evidence>,
    heads: Heads,
}

/// How a piece of evidence on the goal's work is judged new, once its own
/// reading is in.
enum Judging {
    /// Judged already.
    Found(Found),
    /// By one compare, from where its work stood to its commit.
    Compare(Value),
    /// A commit, by where it is placed.
    Commit(Placing),
}

/// What is found of each of `evidence` on GitHub, as the person in scope,
/// against what goal `t` takes: two rounds of reads, what the goal's pull
/// requests and the evidence's own say, then where each commit is and
/// whether each is new.
async fn found_on_goal(access: &GithubAccess<'_>, t: &Taken, evidence: &[Evidence]) -> Shown {
    let pull = |repo: &str, n: u64| {
        goal::github_repo(repo).map(|(owner, name)| {
            (
                "github.get_pull",
                json!({ "owner": owner, "repo": name, "number": n }),
            )
        })
    };
    let mut goal_pulls: Vec<(String, u64)> = t.refs.pulls.iter().cloned().collect();
    goal_pulls.sort();
    let goal_calls: Vec<_> = goal_pulls
        .iter()
        .filter_map(|(repo, n)| Some((repo.clone(), *n, pull(repo, *n)?)))
        .collect();
    let own_calls: Vec<Option<(&str, Value)>> = evidence
        .iter()
        .map(|e| match e {
            Evidence::Pull { repo, number } => pull(repo, *number),
            Evidence::Run { repo, id } => goal::github_repo(repo).map(|(owner, name)| {
                (
                    "github.workflow_run_logs",
                    json!({ "owner": owner, "repo": name, "run_id": id }),
                )
            }),
            Evidence::Commit { .. } | Evidence::Ticket { .. } => None,
        })
        .collect();
    let calls = goal_calls
        .iter()
        .map(|(.., c)| c.clone())
        .chain(own_calls.iter().flatten().cloned())
        .collect();
    let Ok(answered) = each_as_person(access, calls).await else {
        return Shown {
            found: vec![Found::Missing; evidence.len()],
            evidence: evidence.to_vec(),
            heads: Heads::new(),
        };
    };
    let mut answered = answered.into_iter().map(Result::ok);
    let pulls: Vec<GoalPull> = goal_calls
        .iter()
        .zip(answered.by_ref())
        .filter_map(|((repo, n, _), got)| Some(GoalPull::of(repo, *n, &got?)))
        .collect();
    let own: Vec<Option<Value>> = own_calls
        .iter()
        .map(|c| c.as_ref().and_then(|_| answered.next().flatten()))
        .collect();
    let on_branch = |repo: &str, b: &str| {
        t.refs.names_branch(repo, b)
            || pulls
                .iter()
                .any(|p| p.repo.eq_ignore_ascii_case(repo) && p.head.as_deref() == Some(b))
    };
    let in_repos = |repo: &str| t.repos.contains(&repo.trim().to_ascii_lowercase());
    // Where branch `b` of `repo` stood: its own, else that of the goal's
    // pull request from it.
    let then_of = |repo: &str, b: &str| {
        t.then(&head_key(repo, b)).or_else(|| {
            pulls
                .iter()
                .find(|p| p.repo.eq_ignore_ascii_case(repo) && p.head.as_deref() == Some(b))
                .and_then(|p| t.then(&head_key(repo, &format!("#{}", p.number))))
        })
    };
    let compare = |repo: &str, then: &str, sha: &str| {
        goal::github_repo(repo)
            .map(|(owner, name)| json!({ "owner": owner, "repo": name, "base": then, "head": sha }))
    };
    // New when its head is past where it stood, else, with none kept, when
    // `at` is since.
    let new_by = |repo: &str, then: Option<&str>, sha: Option<&str>, at: &Value| match (
        then,
        sha.filter(|s| is_sha(s)),
    ) {
        (Some(then), Some(sha)) => {
            compare(repo, then, sha).map_or(Judging::Found(Found::Missing), Judging::Compare)
        }
        (Some(_), None) => Judging::Found(Found::Missing),
        (None, _) => Judging::Found(match fresh(at, t.since) {
            true => Found::OnGoal,
            false => Found::Old,
        }),
    };
    let judging: Vec<Judging> = evidence
        .iter()
        .zip(&own)
        .map(|(e, got)| match (e, got) {
            (Evidence::Commit { repo, sha }, _) => commit_placing(t, &pulls, repo, sha),
            (Evidence::Ticket { .. }, _) => Judging::Found(Found::OffGoal),
            (_, None) => Judging::Found(Found::Missing),
            (Evidence::Pull { repo, number }, Some(got)) => {
                let branch = |k: &str| got[k].as_str().is_some_and(|b| on_branch(repo, b));
                let on = match t.refs.is_empty() {
                    true => in_repos(repo),
                    false => t.refs.names_pull(repo, *number) || branch("head") || branch("base"),
                };
                let then = t
                    .then(&head_key(repo, &format!("#{number}")))
                    .or_else(|| then_of(repo, got["head"].as_str()?));
                match (on, fresh(&got["merged_at"], t.since)) {
                    (false, _) => Judging::Found(Found::OffGoal),
                    (true, true) => Judging::Found(Found::OnGoal),
                    (true, false) => {
                        new_by(repo, then, got["head_sha"].as_str(), &got["created_at"])
                    }
                }
            }
            (Evidence::Run { repo, .. }, Some(got)) => {
                let run = &got["run"];
                let branch = run["head_branch"].as_str();
                let on = match t.refs.is_empty() {
                    true => in_repos(repo),
                    false => branch.is_some_and(|b| on_branch(repo, b)),
                };
                match on {
                    false => Judging::Found(Found::OffGoal),
                    true => new_by(
                        repo,
                        branch.and_then(|b| then_of(repo, b)),
                        run["head_sha"].as_str(),
                        &run["created_at"],
                    ),
                }
            }
        })
        .collect();
    let calls: Vec<(&str, Value)> = judging
        .iter()
        .flat_map(|j| match j {
            Judging::Found(_) => Vec::new(),
            Judging::Compare(args) => vec![args.clone()],
            Judging::Commit(c) => c.compares(),
        })
        .map(|args| ("github.compare", args))
        .collect();
    let answered = match calls.is_empty() {
        true => Vec::new(),
        false => each_as_person(access, calls).await.unwrap_or_default(),
    };
    let mut answered = answered.into_iter().map(Result::ok);
    let judged: Vec<(Found, Option<String>)> = judging
        .iter()
        .map(|j| match j {
            Judging::Found(f) => (*f, None),
            Judging::Compare(_) => (newer(answered.next().flatten().as_ref()), None),
            Judging::Commit(c) => c.placed(&mut answered),
        })
        .collect();
    let heads = evidence
        .iter()
        .zip(&own)
        .filter_map(|(e, got)| match (e, got) {
            (Evidence::Pull { repo, number }, Some(got)) => {
                let sha = got["head_sha"].as_str().filter(|s| is_sha(s))?;
                Some((head_key(repo, &format!("#{number}")), sha.to_string()))
            }
            _ => None,
        })
        .collect();
    let shown = evidence
        .iter()
        .zip(&judged)
        .map(|(e, (_, whole))| match (e, whole) {
            (Evidence::Commit { repo, .. }, Some(sha)) => Evidence::Commit {
                repo: repo.clone(),
                sha: sha.clone(),
            },
            (e, _) => e.clone(),
        })
        .collect();
    Shown {
        found: judged.into_iter().map(|(f, _)| f).collect(),
        evidence: shown,
        heads,
    }
}

/// One branch or pull request head a commit is placed by: where it stands
/// now, and where it stood as the part last moved, when that is kept.
struct Target {
    now: String,
    then: Option<String>,
}

/// A commit as it is placed: its repository and id, what by, and whether a
/// branch of the goal's couldn't be looked up.
struct Placing {
    args: Value,
    sha: String,
    targets: Vec<Target>,
    unplain: bool,
}

/// Where commit `sha` of `repo` is placed by, on goal `t`'s branches and
/// the heads of its pull requests (`pulls`). A goal that names nothing
/// places no commit: it is Unplaced in its repositories, else off them.
fn commit_placing(t: &Taken, pulls: &[GoalPull], repo: &str, sha: &str) -> Judging {
    if !is_sha(sha) {
        return Judging::Found(Found::Missing);
    }
    if t.refs.is_empty() {
        return Judging::Found(match t.repos.contains(&repo.trim().to_ascii_lowercase()) {
            true => Found::Unplaced,
            false => Found::OffGoal,
        });
    }
    let Some((owner, name)) = goal::github_repo(repo) else {
        return Judging::Found(Found::Missing);
    };
    let branches: Vec<&str> = t.refs.branches_of(repo).collect();
    let then = |key: String| t.then(&key).map(str::to_string);
    let targets = branches
        .iter()
        .filter(|b| plain_ref(b))
        .map(|b| Target {
            now: b.to_string(),
            then: then(head_key(repo, b)),
        })
        .chain(
            pulls
                .iter()
                .filter(|p| p.repo.eq_ignore_ascii_case(repo))
                .filter_map(|p| {
                    Some(Target {
                        now: p.head_sha.clone()?,
                        then: then(head_key(repo, &format!("#{}", p.number))),
                    })
                }),
        )
        .collect();
    Judging::Commit(Placing {
        args: json!({ "owner": owner, "repo": name }),
        sha: sha.to_string(),
        targets,
        unplain: branches.iter().any(|b| !plain_ref(b)),
    })
}

/// How a commit stands to one of its targets.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// Unanswered.
    Unread,
    /// Not reachable from it.
    Off,
    /// Reachable from it, and new against where it stood (or none kept).
    New,
    /// Reachable from it, but there already as the part last moved.
    Old,
    /// Reachable from it; whether it is new couldn't be read.
    Unknown,
}

impl Placing {
    /// The compares that place it, in order: whether each target reaches
    /// it, then, where that target's head is kept, whether it is new.
    fn compares(&self) -> Vec<Value> {
        let compare = |base: &str, head: &str| {
            let mut args = self.args.clone();
            args["base"] = json!(base);
            args["head"] = json!(head);
            args
        };
        self.targets
            .iter()
            .flat_map(|g| {
                std::iter::once(compare(&self.sha, &g.now))
                    .chain(g.then.as_deref().map(|then| compare(then, &self.sha)))
            })
            .collect()
    }

    /// Where it is, from the answers to [`Self::compares`] (taken from
    /// `answered` in order), and its whole id.
    fn placed(
        &self,
        answered: &mut impl Iterator<Item = Option<Value>>,
    ) -> (Found, Option<String>) {
        let read: Vec<(Reach, Option<String>)> = self
            .targets
            .iter()
            .map(|g| {
                let on = answered.next().flatten();
                let after = g.then.as_ref().map(|_| answered.next().flatten());
                let Some(on) = on else {
                    return (Reach::Unread, None);
                };
                let whole = on["base_sha"]
                    .as_str()
                    .filter(|s| is_sha(s))
                    .map(str::to_string);
                let reach = match (on["status"].as_str(), after) {
                    (Some("ahead" | "identical"), None) => Reach::New,
                    (Some("ahead" | "identical"), Some(after)) => match newer(after.as_ref()) {
                        Found::OnGoal => Reach::New,
                        Found::Old => Reach::Old,
                        _ => Reach::Unknown,
                    },
                    _ => Reach::Off,
                };
                (reach, whole)
            })
            .collect();
        let whole = read.iter().find_map(|(_, w)| w.clone());
        let reach: Vec<Reach> = read.into_iter().map(|(r, _)| r).collect();
        let found = [
            (Reach::New, Found::OnGoal),
            (Reach::Old, Found::Old),
            (Reach::Unknown, Found::Missing),
            (Reach::Off, Found::OffGoal),
        ]
        .into_iter()
        .find(|(r, _)| reach.contains(r))
        .map_or(
            match (reach.is_empty(), self.unplain) {
                (true, true) => Found::Unplaced,
                (true, false) => Found::OffGoal,
                (false, _) => Found::Missing,
            },
            |(_, f)| f,
        );
        (found, whole)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_desk::goal::{Measure, Per, PrPick, Want};

    fn refs() -> GoalRefs {
        GoalRefs::of(
            &GoalResult {
                words: "CI fast".into(),
                parts: vec![
                    Measure::Ci {
                        repo: "acme/mantis".into(),
                        workflow: None,
                        branch: "main".into(),
                        want: Want::Minutes {
                            per: Per::Job,
                            at_most: 5.0,
                            runs: 3,
                        },
                    },
                    Measure::PrMerged {
                        repo: "acme/mantis".into(),
                        pr: PrPick::Number(12),
                    },
                ],
            },
            &[],
        )
    }

    #[test]
    fn only_a_plain_branch_or_commit_reaches_a_compare() {
        for ok in ["main", "fix/ci-cache", "release-1.2", "d05e8a4", "MAN_768"] {
            assert!(plain_ref(ok), "{ok}");
        }
        for not in [
            "",
            "../main",
            "main?per_page=1",
            "a#b",
            "-x",
            "/main",
            "main/",
            "a..b",
            "a b",
            "main.",
        ] {
            assert!(!plain_ref(not), "{not}");
        }
        assert!(is_sha("d05e8a4") && is_sha(&"a".repeat(40)));
        assert!(!is_sha("d05e8a") && !is_sha("main123") && !is_sha(&"a".repeat(41)));
    }

    fn taken(refs: GoalRefs) -> Taken {
        Taken {
            refs,
            repos: BTreeSet::from(["acme/mantis".to_string()]),
            since: Utc::now(),
            heads: Heads::new(),
            part: 1,
        }
    }

    #[test]
    fn a_checkpoint_refused_says_which_evidence_why_and_what_it_would_take() {
        let t = taken(refs());
        let commit = Evidence::Commit {
            repo: "acme/mantis".into(),
            sha: "d05e8a4c0ffee".into(),
        };
        assert_eq!(
            not_shown_line(&NotShown::OffGoal(commit.clone()), &t),
            "Commit d05e8a4 of acme/mantis isn't on the goal's work (main of acme/mantis, PR #12 \
             of acme/mantis): open a pull request from its branch into one of the goal's branches \
             and cite that, or cite a pull request the goal names."
        );
        let pull = Evidence::Pull {
            repo: "acme/mantis".into(),
            number: 40,
        };
        assert!(not_shown_line(&NotShown::OffGoal(pull.clone()), &t)
            .contains("one into or from its branches"));
        assert!(not_shown_line(&NotShown::Unplaced(commit.clone()), &t)
            .starts_with("Desk can't look up the goal's branch to place Commit d05e8a4"));
        assert!(not_shown_line(&NotShown::Missing(commit.clone()), &t)
            .contains("isn't on GitHub as it says"));
        assert_eq!(
            not_shown_line(&NotShown::Old(pull.clone()), &t),
            "PR #40 of acme/mantis was there before the goal was agreed or its part's last \
             checkpoint: cite work done since."
        );
        assert!(
            not_shown_line(&NotShown::NothingNew, &t).contains("shown toward this part already")
        );
        assert!(
            shows_what(&t.refs, &t.repos).contains("counts once a pull request carries it there")
        );

        let none = taken(GoalRefs::default());
        assert_eq!(
            not_shown_line(&NotShown::Nothing, &none),
            "A checkpoint shows what moved the goal on: cite a pull request or run since the goal \
             was agreed, in the plan's repositories (acme/mantis) (evidence)."
        );
        assert!(not_shown_line(&NotShown::OffGoal(pull), &none)
            .starts_with("PR #40 of acme/mantis isn't in the plan's repositories (acme/mantis)"));
        assert!(not_shown_line(&NotShown::Unplaced(commit), &none)
            .contains("as the goal names no branch: cite the pull request"));
        let unknown = Taken {
            repos: BTreeSet::new(),
            ..taken(GoalRefs::default())
        };
        let line = not_shown_line(&NotShown::Nothing, &unknown);
        assert!(
            line.contains("(which Desk can't tell: none of its tickets names one"),
            "{line}"
        );
        assert!(!line.contains("()"), "{line}");
    }

    #[test]
    fn work_counts_from_when_the_goal_last_moved() {
        let since = Utc::now();
        let at = |m: i64| json!((since + chrono::Duration::minutes(m)).to_rfc3339());
        assert!(fresh(&at(1), since) && fresh(&at(0), since));
        assert!(!fresh(&at(-1), since));
        assert!(!fresh(&Value::Null, since), "no time is no proof");
    }

    #[test]
    fn a_commit_is_new_unless_it_was_there_already() {
        let status = |s: &str| Some(json!({ "status": s }));
        for (s, found) in [
            ("ahead", Found::OnGoal),
            ("diverged", Found::OnGoal),
            ("behind", Found::Old),
            ("identical", Found::Old),
        ] {
            assert_eq!(newer(status(s).as_ref()), found, "{s}");
        }
        assert_eq!(newer(None), Found::Missing);

        let placing = |then: Option<&str>| Placing {
            args: json!({ "owner": "acme", "repo": "mantis" }),
            sha: "d05e8a4".into(),
            targets: vec![Target {
                now: "main".into(),
                then: then.map(str::to_string),
            }],
            unplain: false,
        };
        let whole = format!("d05e8a4{}", "0".repeat(33));
        let on = |s: &str| Some(json!({ "status": s, "base_sha": whole }));
        let placed = |p: &Placing, answers: Vec<Option<Value>>| p.placed(&mut answers.into_iter());
        let kept = placing(Some(&"a".repeat(40)));
        assert_eq!(kept.compares().len(), 2, "reached, then new");
        for (after, found) in [
            ("ahead", Found::OnGoal),
            ("diverged", Found::OnGoal),
            ("behind", Found::Old),
            ("identical", Found::Old),
        ] {
            assert_eq!(
                placed(&kept, vec![on("ahead"), status(after)]),
                (found, Some(whole.clone())),
                "{after}"
            );
        }
        assert_eq!(placed(&kept, vec![on("ahead"), None]).0, Found::Missing);
        assert_eq!(
            placed(&kept, vec![on("diverged"), status("ahead")]).0,
            Found::OffGoal
        );
        assert_eq!(placed(&kept, vec![None, None]).0, Found::Missing);
        let none_kept = placing(None);
        assert_eq!(none_kept.compares().len(), 1);
        assert_eq!(
            placed(&none_kept, vec![on("identical")]).0,
            Found::OnGoal,
            "with no head kept, it is new"
        );
    }
}
