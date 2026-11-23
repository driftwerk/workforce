//! A checkpoint toward a judged part is kept only once it shows the goal's
//! own work moving it on (designs/boss-keeps-goals.md §7.3): every piece of
//! its evidence found on GitHub as the person, one at least new for that
//! part, and each on the goal's work. That is a commit reachable from one
//! of the goal's branches or the head of one of its pull requests, one of
//! those pull requests or one into or from those branches, or a run on
//! those branches. A goal that names nothing on GitHub takes any work found
//! there as its own. What it refuses names what it would take instead.

use serde_json::{json, Value};
use workforce_desk::goal::{
    self, Evidence, FactKind, Found, GoalRefs, GoalResult, NotShown, Reading,
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

/// What a checkpoint toward a judged part of a goal naming `refs` is to
/// show, as the boss is told to record one.
pub(super) fn shows_what(refs: &GoalRefs) -> String {
    match refs.is_empty() {
        true => "the pull request, commits or runs on GitHub that show it".into(),
        false => format!(
            "commits, runs or a pull request on the goal's work ({}); work on another branch counts once a pull request carries it there",
            refs.words()
        ),
    }
}

/// What the boss is told of a checkpoint that doesn't show its judged part
/// moving, `why`, toward a goal naming `refs`.
pub(super) fn not_shown_line(why: &NotShown, refs: &GoalRefs) -> String {
    let work = refs.words();
    match why {
        NotShown::Nothing => format!(
            "A checkpoint shows what moved the goal on: cite {} (evidence).",
            shows_what(refs)
        ),
        NotShown::Missing(e) => format!(
            "{} isn't on GitHub as it says, or couldn't be looked up: check it, and record the checkpoint again.",
            evidence_words(e)
        ),
        NotShown::OffGoal(e @ Evidence::Commit { .. }) => format!(
            "{} isn't on the goal's work ({work}): open a pull request from its branch into one of the goal's branches and cite that, or cite a pull request the goal names.",
            evidence_words(e)
        ),
        NotShown::OffGoal(e @ Evidence::Pull { .. }) => format!(
            "{} isn't the goal's: only a pull request it names, or one into or from its branches, counts ({work}).",
            evidence_words(e)
        ),
        NotShown::OffGoal(e @ Evidence::Run { .. }) => format!(
            "{} didn't run on the goal's work ({work}): cite a run on its branches, or the pull request that carries the work there.",
            evidence_words(e)
        ),
        NotShown::OffGoal(Evidence::Ticket { key }) => format!(
            "Ticket {key} shows no work on GitHub: cite {} instead.",
            shows_what(refs)
        ),
        NotShown::Unplaced(e) => format!(
            "Desk can't look up the goal's branch to place {}: cite the pull request that carries it instead.",
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
    /// as the person: refused in the boss's words when it doesn't.
    pub(super) async fn checkpoint_shown(
        &self,
        service: &AgentService,
        detail: &PlanDetail,
        r: &GoalResult,
        part: u16,
        evidence: &[Evidence],
    ) -> Result<(), String> {
        let refs = GoalRefs::of(r, &readings_of(detail));
        let earlier: Vec<&Evidence> = detail
            .facts
            .iter()
            .filter(|f| f.kind == FactKind::Checkpoint && f.checked && f.part == Some(part))
            .flat_map(|f| &f.evidence)
            .collect();
        let access = super::goal::access(service);
        let found = self
            .as_person(service, found_on_goal(&access, &refs, evidence))
            .await;
        goal::checkpoint_shows(evidence, &found, &earlier)
            .map_err(|why| not_shown_line(&why, &refs))
    }
}

/// A goal's pull request as GitHub said it: its repository, its head
/// branch, and its head's commit.
struct GoalPull {
    repo: String,
    head: Option<String>,
    head_sha: Option<String>,
}

impl GoalPull {
    fn of(repo: &str, got: &Value) -> Self {
        let text = |k: &str| got[k].as_str().map(str::to_string);
        Self {
            repo: repo.to_string(),
            head: text("head"),
            head_sha: text("head_sha").filter(|s| is_sha(s)),
        }
    }
}

/// What is found of each of `evidence` on GitHub, as the person in scope,
/// against what the goal names (`refs`): two rounds of reads, what the
/// goal's pull requests and the evidence's own say, then where each commit
/// is.
async fn found_on_goal(
    access: &GithubAccess<'_>,
    refs: &GoalRefs,
    evidence: &[Evidence],
) -> Vec<Found> {
    let pull = |repo: &str, n: u64| {
        goal::github_repo(repo).map(|(owner, name)| {
            (
                "github.get_pull",
                json!({ "owner": owner, "repo": name, "number": n }),
            )
        })
    };
    let mut goal_pulls: Vec<(String, u64)> = refs.pulls.iter().cloned().collect();
    goal_pulls.sort();
    let goal_calls: Vec<_> = goal_pulls
        .iter()
        .filter_map(|(repo, n)| Some((repo.clone(), pull(repo, *n)?)))
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
        .map(|(_, c)| c.clone())
        .chain(own_calls.iter().flatten().cloned())
        .collect();
    let Ok(answered) = each_as_person(access, calls).await else {
        return vec![Found::Missing; evidence.len()];
    };
    let mut answered = answered.into_iter().map(Result::ok);
    let pulls: Vec<GoalPull> = goal_calls
        .iter()
        .zip(answered.by_ref())
        .filter_map(|((repo, _), got)| Some(GoalPull::of(repo, &got?)))
        .collect();
    let own: Vec<Option<Value>> = own_calls
        .iter()
        .map(|c| c.as_ref().and_then(|_| answered.next().flatten()))
        .collect();
    let on_branch = |repo: &str, b: &str| {
        refs.names_branch(repo, b)
            || pulls
                .iter()
                .any(|p| p.repo.eq_ignore_ascii_case(repo) && p.head.as_deref() == Some(b))
    };
    let commits = commits_placed(access, refs, &pulls, evidence).await;
    evidence
        .iter()
        .zip(own)
        .zip(commits)
        .map(|((e, got), placed)| match (e, got) {
            (Evidence::Commit { .. }, _) => placed,
            (Evidence::Ticket { .. }, _) => Found::OffGoal,
            (_, None) => Found::Missing,
            (_, Some(_)) if refs.is_empty() => Found::OnGoal,
            (Evidence::Pull { repo, number }, Some(got)) => {
                let branch = |k: &str| got[k].as_str().is_some_and(|b| on_branch(repo, b));
                on(refs.names_pull(repo, *number) || branch("head") || branch("base"))
            }
            (Evidence::Run { repo, .. }, Some(got)) => on(got["run"]["head_branch"]
                .as_str()
                .is_some_and(|b| on_branch(repo, b))),
        })
        .collect()
}

/// [`Found::OnGoal`] when `yes`, else [`Found::OffGoal`].
fn on(yes: bool) -> Found {
    match yes {
        true => Found::OnGoal,
        false => Found::OffGoal,
    }
}

/// A commit as it is compared: its repository and id, what with, and
/// whether a branch of the goal's couldn't be looked up.
struct Compare {
    args: Value,
    heads: Vec<String>,
    unplain: bool,
}

/// Where each commit of `evidence` is, compared with the goal's branches
/// and the heads of its pull requests (`pulls`), in one round of reads:
/// none for evidence that is no commit. With nothing named, a commit is
/// the goal's once GitHub has it.
async fn commits_placed(
    access: &GithubAccess<'_>,
    refs: &GoalRefs,
    pulls: &[GoalPull],
    evidence: &[Evidence],
) -> Vec<Found> {
    // What each commit is compared with: its own id, with nothing named.
    let heads = |repo: &str, sha: &str| -> (Vec<String>, bool) {
        if refs.is_empty() {
            return (vec![sha.to_string()], false);
        }
        let branches: Vec<&str> = refs.branches_of(repo).collect();
        let unplain = branches.iter().any(|b| !plain_ref(b));
        let heads = branches
            .into_iter()
            .filter(|b| plain_ref(b))
            .map(str::to_string)
            .chain(
                pulls
                    .iter()
                    .filter(|p| p.repo.eq_ignore_ascii_case(repo))
                    .filter_map(|p| p.head_sha.clone()),
            )
            .collect();
        (heads, unplain)
    };
    let asks: Vec<Option<Compare>> = evidence
        .iter()
        .map(|e| match e {
            Evidence::Commit { repo, sha } if is_sha(sha) => {
                let (owner, name) = goal::github_repo(repo)?;
                let (heads, unplain) = heads(repo, sha);
                Some(Compare {
                    args: json!({ "owner": owner, "repo": name, "base": sha }),
                    heads,
                    unplain,
                })
            }
            _ => None,
        })
        .collect();
    let calls: Vec<(&str, Value)> = asks
        .iter()
        .flatten()
        .flat_map(|c| {
            c.heads.iter().map(|head| {
                let mut args = c.args.clone();
                args["head"] = json!(head);
                ("github.compare", args)
            })
        })
        .collect();
    let answered = match calls.is_empty() {
        true => Vec::new(),
        false => each_as_person(access, calls).await.unwrap_or_default(),
    };
    let mut answered = answered.into_iter().map(Result::ok);
    evidence
        .iter()
        .zip(&asks)
        .map(|(e, ask)| match (e, ask) {
            (Evidence::Commit { .. }, None) => Found::Missing,
            (_, None) => Found::OffGoal,
            (_, Some(c)) => {
                let got: Vec<Option<Value>> =
                    c.heads.iter().map(|_| answered.next().flatten()).collect();
                let reached = got
                    .iter()
                    .flatten()
                    .any(|g| matches!(g["status"].as_str(), Some("ahead" | "identical")));
                match (reached, got.iter().any(Option::is_some), c.heads.is_empty()) {
                    (true, ..) => Found::OnGoal,
                    (false, true, _) => Found::OffGoal,
                    (false, false, true) if c.unplain => Found::Unplaced,
                    (false, false, true) => Found::OffGoal,
                    (false, false, false) => Found::Missing,
                }
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

    #[test]
    fn a_checkpoint_refused_says_which_evidence_why_and_what_it_would_take() {
        let refs = refs();
        let commit = Evidence::Commit {
            repo: "acme/mantis".into(),
            sha: "d05e8a4c0ffee".into(),
        };
        assert_eq!(
            not_shown_line(&NotShown::OffGoal(commit.clone()), &refs),
            "Commit d05e8a4 of acme/mantis isn't on the goal's work (main of acme/mantis, PR #12 \
             of acme/mantis): open a pull request from its branch into one of the goal's branches \
             and cite that, or cite a pull request the goal names."
        );
        let pull = Evidence::Pull {
            repo: "acme/mantis".into(),
            number: 40,
        };
        assert!(not_shown_line(&NotShown::OffGoal(pull), &refs)
            .contains("one into or from its branches"));
        assert!(not_shown_line(&NotShown::Unplaced(commit.clone()), &refs)
            .starts_with("Desk can't look up the goal's branch to place Commit d05e8a4"));
        assert!(not_shown_line(&NotShown::Missing(commit), &refs)
            .contains("isn't on GitHub as it says"));
        assert!(
            not_shown_line(&NotShown::NothingNew, &refs).contains("shown toward this part already")
        );
        let none = GoalRefs::default();
        assert_eq!(
            not_shown_line(&NotShown::Nothing, &none),
            "A checkpoint shows what moved the goal on: cite the pull request, commits or runs on \
             GitHub that show it (evidence)."
        );
        assert!(shows_what(&refs).contains("counts once a pull request carries it there"));
    }
}
