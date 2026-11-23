//! A checkpoint toward a judged part is kept only once it shows the goal's
//! own work moving it on since it was agreed (designs/boss-keeps-goals.md
//! §7.3): every piece of its evidence found on GitHub as the person, one at
//! least new for that part, each on the goal's work and done since the goal
//! was agreed or its part's last checkpoint. On the goal's work is a commit
//! reachable from one of the goal's branches or the head of one of its pull
//! requests, one of those pull requests or one into or from those branches,
//! or a run on those branches; a goal that names nothing on GitHub takes a
//! pull request or run in its tickets' repositories. Since then is a run
//! made, or a pull request updated, after it, and a commit that comes after
//! where that branch or pull request stood ([`Heads`], kept when the goal is
//! first checked and whenever a checkpoint moves it). What it refuses names
//! what it would take instead.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;
use workforce_desk::goal::{
    self, head_key, Evidence, FactKind, Found, GoalRefs, GoalResult, Heads, NotShown, Reading,
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
/// nothing, done since `since`, a commit after where its work stood then
/// (`heads`).
pub(super) struct Taken {
    pub refs: GoalRefs,
    pub repos: BTreeSet<String>,
    pub since: DateTime<Utc>,
    pub heads: Heads,
}

/// `repos` in words.
fn repos_words(repos: &BTreeSet<String>) -> String {
    match repos.is_empty() {
        true => "none Desk can tell".into(),
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
    /// work stands is kept as what the next must come after.
    pub(super) async fn checkpoint_shown(
        &self,
        service: &AgentService,
        detail: &PlanDetail,
        r: &GoalResult,
        part: u16,
        evidence: &[Evidence],
    ) -> Result<Vec<Evidence>, String> {
        let plan = &detail.summary.plan;
        let refs = GoalRefs::of(r, &readings_of(detail));
        let checkpoints: Vec<&goal::Fact> = detail
            .facts
            .iter()
            .filter(|f| f.kind == FactKind::Checkpoint && f.checked && f.part == Some(part))
            .collect();
        let earlier: Vec<&Evidence> = checkpoints.iter().flat_map(|f| &f.evidence).collect();
        let since = checkpoints
            .iter()
            .map(|f| f.at)
            .max()
            .or(plan.agreed_at)
            .unwrap_or(plan.created_at);
        let heads = match self.desk.store().goal_heads(plan.id).await {
            Ok(h) if !h.is_empty() || refs.is_empty() => h,
            _ => self.keep_heads(service, plan.id, &refs).await,
        };
        let taken = Taken {
            repos: self.work_repos(detail),
            refs,
            since,
            heads,
        };
        let access = super::goal::access(service);
        let (found, shown) = self
            .as_person(service, found_on_goal(&access, &taken, evidence))
            .await;
        goal::checkpoint_shows(&shown, &found, &earlier)
            .map_err(|why| not_shown_line(&why, &taken))?;
        if !taken.refs.is_empty() {
            self.keep_heads(service, plan.id, &taken.refs).await;
        }
        Ok(shown)
    }

    /// Where the work of goal plan `plan`, naming `refs`, stands now on
    /// GitHub, read as the person and kept with it: none it couldn't read.
    pub(super) async fn keep_heads(
        &self,
        service: &AgentService,
        plan: Uuid,
        refs: &GoalRefs,
    ) -> Heads {
        let access = super::goal::access(service);
        let heads = self.as_person(service, heads_now(&access, refs)).await;
        if !heads.is_empty() {
            if let Err(e) = self.desk.store().keep_goal_heads(plan, &heads).await {
                tracing::info!(plan = %plan, error = %e, "where a goal's work stands wasn't kept");
            }
        }
        heads
    }

    /// The repositories goal plan `detail`'s tickets' sessions here work
    /// in: what a goal that names nothing on GitHub takes work from.
    pub(super) fn work_repos(&self, detail: &PlanDetail) -> BTreeSet<String> {
        detail
            .tickets
            .iter()
            .filter_map(|t| t.ticket.ticket.session())
            .filter_map(|s| self.desk.repo_of_session(s))
            .collect()
    }
}

/// Where each of `refs`' branches and pull requests stands now, as the
/// person in scope reads it, by [`head_key`].
async fn heads_now(access: &GithubAccess<'_>, refs: &GoalRefs) -> Heads {
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

/// What is found of each of `evidence` on GitHub, as the person in scope,
/// against what goal `t` takes, and the evidence as GitHub names it: two
/// rounds of reads, what the goal's pull requests and the evidence's own
/// say, then where each commit is.
async fn found_on_goal(
    access: &GithubAccess<'_>,
    t: &Taken,
    evidence: &[Evidence],
) -> (Vec<Found>, Vec<Evidence>) {
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
        return (vec![Found::Missing; evidence.len()], evidence.to_vec());
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
    let since = |yes: bool, at: &Value| match (yes, fresh(at, t.since)) {
        (false, _) => Found::OffGoal,
        (true, true) => Found::OnGoal,
        (true, false) => Found::Old,
    };
    let commits = commits_placed(access, t, &pulls, evidence).await;
    let found = evidence
        .iter()
        .zip(own)
        .zip(&commits)
        .map(|((e, got), (placed, _))| match (e, got) {
            (Evidence::Commit { .. }, _) => *placed,
            (Evidence::Ticket { .. }, _) => Found::OffGoal,
            (_, None) => Found::Missing,
            (Evidence::Pull { repo, number }, Some(got)) => {
                let branch = |k: &str| got[k].as_str().is_some_and(|b| on_branch(repo, b));
                let on = match t.refs.is_empty() {
                    true => in_repos(repo),
                    false => t.refs.names_pull(repo, *number) || branch("head") || branch("base"),
                };
                since(on, &got["updated_at"])
            }
            (Evidence::Run { repo, .. }, Some(got)) => {
                let on = match t.refs.is_empty() {
                    true => in_repos(repo),
                    false => got["run"]["head_branch"]
                        .as_str()
                        .is_some_and(|b| on_branch(repo, b)),
                };
                since(on, &got["run"]["created_at"])
            }
        })
        .collect();
    let shown = evidence
        .iter()
        .zip(commits)
        .map(|(e, (_, whole))| match (e, whole) {
            (Evidence::Commit { repo, .. }, Some(sha)) => Evidence::Commit {
                repo: repo.clone(),
                sha,
            },
            (e, _) => e.clone(),
        })
        .collect();
    (found, shown)
}

/// One branch or pull request head a commit is placed by: where it stands
/// now, and where it stood when the goal last moved.
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

/// Where each commit of `evidence` is, by the goal's branches and the
/// heads of its pull requests (`pulls`), in one round of reads, with its
/// whole id: reachable from one of them now, and after where it stood when
/// the goal last moved. A goal that names nothing places no commit.
async fn commits_placed(
    access: &GithubAccess<'_>,
    t: &Taken,
    pulls: &[GoalPull],
    evidence: &[Evidence],
) -> Vec<(Found, Option<String>)> {
    let targets = |repo: &str| -> (Vec<Target>, bool) {
        let branches: Vec<&str> = t.refs.branches_of(repo).collect();
        let unplain = branches.iter().any(|b| !plain_ref(b));
        let then = |key: String| t.heads.get(&key).cloned();
        let targets = branches
            .into_iter()
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
        (targets, unplain)
    };
    let placing: Vec<Option<Placing>> = evidence
        .iter()
        .map(|e| match e {
            Evidence::Commit { repo, sha } if is_sha(sha) && !t.refs.is_empty() => {
                let (owner, name) = goal::github_repo(repo)?;
                let (targets, unplain) = targets(repo);
                Some(Placing {
                    args: json!({ "owner": owner, "repo": name }),
                    sha: sha.clone(),
                    targets,
                    unplain,
                })
            }
            _ => None,
        })
        .collect();
    let compare = |c: &Placing, base: &str, head: &str| {
        let mut args = c.args.clone();
        args["base"] = json!(base);
        args["head"] = json!(head);
        ("github.compare", args)
    };
    let calls: Vec<(&str, Value)> = placing
        .iter()
        .flatten()
        .flat_map(|c| {
            c.targets.iter().flat_map(move |g| {
                std::iter::once(compare(c, &c.sha, &g.now))
                    .chain(g.then.as_deref().map(|then| compare(c, then, &c.sha)))
            })
        })
        .collect();
    let answered = match calls.is_empty() {
        true => Vec::new(),
        false => each_as_person(access, calls).await.unwrap_or_default(),
    };
    let mut answered = answered.into_iter().map(Result::ok);
    let status = |g: &Option<Value>, of: &[&str]| {
        g.as_ref()
            .and_then(|g| g["status"].as_str())
            .is_some_and(|s| of.contains(&s))
    };
    evidence
        .iter()
        .zip(&placing)
        .map(|(e, placing)| match (e, placing) {
            (Evidence::Commit { repo, sha }, None) if is_sha(sha) && t.refs.is_empty() => {
                match t.repos.contains(&repo.trim().to_ascii_lowercase()) {
                    true => (Found::Unplaced, None),
                    false => (Found::OffGoal, None),
                }
            }
            (Evidence::Commit { .. }, None) => (Found::Missing, None),
            (_, None) => (Found::OffGoal, None),
            (_, Some(c)) => {
                let read: Vec<(Option<Value>, Option<Option<Value>>)> = c
                    .targets
                    .iter()
                    .map(|g| {
                        let on = answered.next().flatten();
                        (on, g.then.as_ref().map(|_| answered.next().flatten()))
                    })
                    .collect();
                let whole = read
                    .iter()
                    .filter_map(|(on, _)| on.as_ref()?["base_sha"].as_str())
                    .find(|s| is_sha(s))
                    .map(str::to_string);
                let reached = |(on, _): &&(Option<Value>, Option<Option<Value>>)| {
                    status(on, &["ahead", "identical"])
                };
                let since = |(_, after): &&(Option<Value>, Option<Option<Value>>)| {
                    after.as_ref().is_some_and(|a| status(a, &["ahead"]))
                };
                let found = match (
                    read.iter().filter(reached).any(|r| since(&r)),
                    read.iter().any(|r| reached(&r)),
                    read.iter().any(|(on, _)| on.is_some()),
                    c.targets.is_empty(),
                ) {
                    (true, ..) => Found::OnGoal,
                    (false, true, ..) => Found::Old,
                    (false, false, true, _) => Found::OffGoal,
                    (false, false, false, true) if c.unplain => Found::Unplaced,
                    (false, false, false, true) => Found::OffGoal,
                    (false, false, false, false) => Found::Missing,
                };
                (found, whole)
            }
        })
        .collect()
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
        assert!(shows_what(&t.refs, &t.repos).contains("counts once a pull request carries it there"));

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
    }

    #[test]
    fn work_counts_from_when_the_goal_last_moved() {
        let since = Utc::now();
        let at = |m: i64| json!((since + chrono::Duration::minutes(m)).to_rfc3339());
        assert!(fresh(&at(1), since) && fresh(&at(0), since));
        assert!(!fresh(&at(-1), since));
        assert!(!fresh(&Value::Null, since), "no time is no proof");
    }
}
