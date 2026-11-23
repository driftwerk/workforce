//! Reading a goal's measured parts on GitHub, as the person: a workflow's
//! runs on a branch (their minutes, or whether they went green), and a pull
//! request's stage or checks. Only [`super::github::CALLS`]' reads are
//! made, through [`super::github::calls_as_person`]; what they answer is
//! folded into a [`Reading`] by the pure parsers here.

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use workforce_desk::goal::{self, Evidence, Measure, Per, PrPick, Reading, Want};

use super::github::{self, GithubAccess};

/// How long before a goal was agreed a run may have started and still
/// count toward it: the clocks of GitHub and this computer differ.
const SLACK: Duration = Duration::minutes(2);
/// The most runs a CI part reads at once.
const RUNS_READ: u64 = 10;

/// Why a part can't be read where it says: no such repository, workflow
/// or pull request for the person, or GitHub not connected. A goal that
/// says one is refused as it is saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Unreadable(pub String);

/// Reads part `n` of a goal agreed at `agreed_at`, as the person in scope,
/// at `at`. What GitHub couldn't answer for now (slow, rate limited) is a
/// failed reading; what isn't there for the person is [`Unreadable`].
pub(super) async fn read(
    access: &GithubAccess<'_>,
    part: &Measure,
    n: u16,
    agreed_at: DateTime<Utc>,
    at: DateTime<Utc>,
) -> Result<Reading, Unreadable> {
    let read = match part {
        Measure::Ci {
            repo,
            workflow,
            branch,
            want,
        } => read_ci(access, repo, workflow.as_deref(), branch, want, agreed_at).await,
        Measure::PrMerged { repo, pr } => read_pull(access, repo, pr)
            .await
            .map(|p| pr_stage(repo, pr, p.as_ref())),
        Measure::ChecksGreen { repo, pr } => read_pull(access, repo, pr)
            .await
            .map(|p| checks_green(repo, pr, p.as_ref())),
        Measure::Judged { .. } => {
            return Err(Unreadable("A judged part isn't read.".into()));
        }
    };
    match read {
        Ok(found) => Ok(found.at(n, at)),
        Err(e) if e == github::NOT_CONNECTED => Err(Unreadable(
            "GitHub isn't connected for the person here, so Desk can't read this goal: connect \
             GitHub (connections.setup), or ask the person to."
                .into(),
        )),
        Err(e) if e == github::NOT_FOUND => Err(Unreadable(format!(
            "GitHub has no {} that the person's GitHub can see: check the repository and what it names.",
            part.label()
        ))),
        Err(e) if e.starts_with(NOT_THERE) => Err(Unreadable(e)),
        Err(e) => Ok(Reading {
            at,
            part: n,
            value: None,
            met: false,
            words: String::new(),
            evidence: Vec::new(),
            failed: Some(workforce_desk::crew::one_line(&e, 300)),
        }),
    }
}

/// What a refusal that names something GitHub hasn't got starts with.
const NOT_THERE: &str = "GitHub has no";

/// What one read found, before it is said whose part and when.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Found {
    pub value: Option<f64>,
    pub met: bool,
    pub words: String,
    pub evidence: Vec<Evidence>,
    pub failed: Option<String>,
}

impl Found {
    fn at(self, part: u16, at: DateTime<Utc>) -> Reading {
        Reading {
            at,
            part,
            value: self.value,
            met: self.met,
            words: self.words,
            evidence: self.evidence,
            failed: self.failed,
        }
    }

    fn unread(why: String) -> Self {
        Self {
            value: None,
            met: false,
            words: String::new(),
            evidence: Vec::new(),
            failed: Some(why),
        }
    }
}

fn split(repo: &str) -> Result<(&str, &str), String> {
    goal::github_repo(repo).ok_or_else(|| format!("{NOT_THERE} repository named {repo}."))
}

async fn read_ci(
    access: &GithubAccess<'_>,
    repo: &str,
    workflow: Option<&str>,
    branch: &str,
    want: &Want,
    agreed_at: DateTime<Utc>,
) -> Result<Found, String> {
    let (owner, name) = split(repo)?;
    let (_, listed) = github::calls_as_person(
        access,
        vec![(
            "github.workflow_runs",
            json!({ "owner": owner, "repo": name, "branch": branch, "status": "completed", "per_page": RUNS_READ }),
        )],
    )
    .await?;
    let list = listed.into_iter().next().unwrap_or(Value::Null);
    let runs = finished_runs(&list, workflow);
    if let (Some(w), true, false) = (workflow, runs.is_empty(), runs_of(&list).is_empty()) {
        return Err(format!(
            "{NOT_THERE} workflow named “{w}” that has run on {branch} of {repo}."
        ));
    }
    let newest: Vec<Run> = runs.into_iter().take(usize::from(want.runs())).collect();
    let jobs = match want {
        Want::Minutes { per: Per::Job, .. } if !newest.is_empty() => {
            let calls = newest
                .iter()
                .map(|r| {
                    (
                        "github.workflow_run_logs",
                        json!({ "owner": owner, "repo": name, "run_id": r.id }),
                    )
                })
                .collect();
            github::calls_as_person(access, calls).await?.1
        }
        _ => Vec::new(),
    };
    Ok(ci_reading(repo, branch, want, &newest, &jobs, agreed_at))
}

/// A finished workflow run, as read from GitHub's list.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Run {
    pub id: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub success: bool,
}

fn time(v: &Value) -> Option<DateTime<Utc>> {
    v.as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
}

/// Every finished run of a list, newest first.
fn runs_of(list: &Value) -> Vec<(Run, &str)> {
    let mut runs: Vec<(Run, &str)> = list["runs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["status"].as_str() == Some("completed"))
        .filter_map(|r| {
            Some((
                Run {
                    id: r["id"].as_u64()?,
                    created_at: time(&r["created_at"])?,
                    updated_at: time(&r["updated_at"])?,
                    success: r["conclusion"].as_str() == Some("success"),
                },
                r["name"].as_str().unwrap_or_default(),
            ))
        })
        .collect();
    runs.sort_by_key(|(r, _)| std::cmp::Reverse(r.created_at));
    runs
}

/// The finished runs of a list of `workflow` (every one when none), newest
/// first.
pub(super) fn finished_runs(list: &Value, workflow: Option<&str>) -> Vec<Run> {
    runs_of(list)
        .into_iter()
        .filter(|(_, name)| workflow.is_none_or(|w| name.trim().eq_ignore_ascii_case(w.trim())))
        .map(|(r, _)| r)
        .collect()
}

/// The minutes a run took, start to end.
fn run_minutes(r: &Run) -> f64 {
    (r.updated_at - r.created_at).num_seconds().max(0) as f64 / 60.0
}

/// The minutes the longest job of a run took (its `workflow_run_logs`),
/// skipped jobs left out; none when no job ran.
pub(super) fn longest_job(logs: &Value) -> Option<f64> {
    logs["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|j| j["conclusion"].as_str() != Some("skipped"))
        .filter_map(|j| Some((time(&j["completed_at"])? - time(&j["started_at"])?).num_seconds()))
        .map(|s| s.max(0) as f64 / 60.0)
        .reduce(f64::max)
}

/// What a CI part reads from the `newest` finished runs of its branch (and,
/// per job, each run's `jobs`, in the same order): for minutes the most any
/// of them took, for green how many of them in a row succeeded. Met only by
/// as many runs as it wants, each since the goal was agreed.
pub(super) fn ci_reading(
    repo: &str,
    branch: &str,
    want: &Want,
    newest: &[Run],
    jobs: &[Value],
    agreed_at: DateTime<Utc>,
) -> Found {
    if newest.is_empty() {
        return Found::unread(format!("no run on {branch} of {repo} has finished yet"));
    }
    let runs = usize::from(want.runs());
    let enough = newest.len() >= runs && newest.iter().all(|r| r.created_at + SLACK >= agreed_at);
    let evidence = newest
        .iter()
        .map(|r| Evidence::Run {
            repo: repo.to_string(),
            id: r.id,
        })
        .collect();
    let of = match newest.len() {
        1 => "the newest run".to_string(),
        n => format!("the newest {n} runs"),
    };
    match want {
        Want::Minutes { per, at_most, .. } => {
            let minutes: Vec<f64> = match per {
                Per::Run => newest.iter().map(run_minutes).collect(),
                Per::Job => jobs.iter().filter_map(longest_job).collect(),
            };
            let Some(most) = minutes.iter().copied().reduce(f64::max) else {
                return Found::unread(format!("no job of {of} on {branch} ran"));
            };
            let words = match per {
                Per::Run => format!("{} at most a run, over {of}", goal::minutes(most)),
                Per::Job => format!("{} the longest job, over {of}", goal::minutes(most)),
            };
            Found {
                value: Some(most),
                met: enough && minutes.len() == newest.len() && most <= *at_most,
                words,
                evidence,
                failed: None,
            }
        }
        Want::Green { .. } => {
            let green = newest.iter().take_while(|r| r.success).count();
            Found {
                value: Some(green as f64),
                met: enough && green >= runs,
                words: format!("{green} of {of} green in a row"),
                evidence,
                failed: None,
            }
        }
    }
}

/// The pull request `pr` of `repo`, as `github.get_pull` answers: none
/// when a head branch has none yet.
async fn read_pull(
    access: &GithubAccess<'_>,
    repo: &str,
    pr: &PrPick,
) -> Result<Option<Value>, String> {
    let (owner, name) = split(repo)?;
    let number = match pr {
        PrPick::Number(n) => *n,
        PrPick::Head(head) => {
            let (_, found) = github::calls_as_person(
                access,
                vec![(
                    "github.list_pulls",
                    json!({ "owner": owner, "repo": name, "head": head, "state": "all", "per_page": 10 }),
                )],
            )
            .await?;
            let newest = found.first().and_then(|f| newest_pull(f, head));
            match newest {
                Some(n) => n,
                None => return Ok(None),
            }
        }
    };
    let (_, got) = github::calls_as_person(
        access,
        vec![(
            "github.get_pull",
            json!({ "owner": owner, "repo": name, "number": number }),
        )],
    )
    .await?;
    Ok(got.into_iter().next())
}

/// The newest pull request of a `list_pulls` answer whose head is `head`.
pub(super) fn newest_pull(list: &Value, head: &str) -> Option<u64> {
    list["pulls"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["head"].as_str() == Some(head))
        .filter_map(|p| Some((time(&p["created_at"])?, p["number"].as_u64()?)))
        .max()
        .map(|(_, n)| n)
}

fn pull_evidence(repo: &str, pull: &Value) -> Vec<Evidence> {
    pull["number"]
        .as_u64()
        .map(|number| Evidence::Pull {
            repo: repo.to_string(),
            number,
        })
        .into_iter()
        .collect()
}

/// How far a pull request is along: 0 none (or closed unmerged), 1 open,
/// 2 its checks green, 3 merged; met merged.
pub(super) fn pr_stage(repo: &str, pr: &PrPick, pull: Option<&Value>) -> Found {
    let named = goal::pr_words(pr);
    let Some(p) = pull else {
        return Found {
            value: Some(0.0),
            met: false,
            words: format!("{named} isn't opened yet"),
            evidence: Vec::new(),
            failed: None,
        };
    };
    let (stage, words) = match (
        p["merged"].as_bool() == Some(true),
        p["state"].as_str(),
        p["checks"]["overall"].as_str(),
    ) {
        (true, _, _) => (3.0, format!("{named} merged")),
        (false, Some("open"), Some("success")) => (2.0, format!("{named} open, its checks green")),
        (false, Some("open"), _) => (1.0, format!("{named} open")),
        _ => (0.0, format!("{named} closed without merging")),
    };
    Found {
        value: Some(stage),
        met: stage >= 3.0,
        words,
        evidence: pull_evidence(repo, p),
        failed: None,
    }
}

/// Whether a pull request's checks are green on its current head: 1 when
/// they are, met then.
pub(super) fn checks_green(repo: &str, pr: &PrPick, pull: Option<&Value>) -> Found {
    let named = goal::pr_words(pr);
    let Some(p) = pull else {
        return Found {
            value: Some(0.0),
            met: false,
            words: format!("{named} isn't opened yet"),
            evidence: Vec::new(),
            failed: None,
        };
    };
    let overall = p["checks"]["overall"].as_str().unwrap_or("not reported");
    let green = overall == "success";
    Found {
        value: Some(f64::from(u8::from(green))),
        met: green,
        words: match green {
            true => format!("{named}'s checks green"),
            false => format!("{named}'s checks {overall}"),
        },
        evidence: pull_evidence(repo, p),
        failed: None,
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn at(minute: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 20, 20, 0, 0).unwrap() + Duration::minutes(minute)
    }

    fn stamp(minute: i64) -> String {
        at(minute).to_rfc3339()
    }

    fn listed(runs: &[(u64, &str, i64, i64, &str)]) -> Value {
        json!({ "runs": runs.iter().map(|(id, name, from, to, conclusion)| json!({
            "id": id, "name": name, "status": "completed", "conclusion": conclusion,
            "created_at": stamp(*from), "updated_at": stamp(*to),
        })).collect::<Vec<_>>() })
    }

    fn logs(jobs: &[(i64, i64, &str)]) -> Value {
        json!({ "jobs": jobs.iter().map(|(from, to, conclusion)| json!({
            "conclusion": conclusion, "started_at": stamp(*from), "completed_at": stamp(*to),
        })).collect::<Vec<_>>() })
    }

    fn minutes(per: Per, at_most: f64, runs: u8) -> Want {
        Want::Minutes { per, at_most, runs }
    }

    #[test]
    fn finished_runs_are_newest_first_and_of_their_workflow() {
        let mut list = listed(&[
            (1, "CI", 0, 10, "success"),
            (2, "Deploy", 20, 25, "success"),
            (3, "ci", 30, 42, "failure"),
        ]);
        list["runs"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "id": 4, "name": "CI", "status": "in_progress", "created_at": stamp(40), "updated_at": stamp(41) }));
        let ids = |w| {
            finished_runs(&list, w)
                .iter()
                .map(|r| r.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(Some("CI")), vec![3, 1]);
        assert_eq!(ids(None), vec![3, 2, 1]);
        assert!(ids(Some("Lint")).is_empty());
    }

    #[test]
    fn ci_minutes_per_run_is_the_most_a_run_took() {
        let runs = finished_runs(
            &listed(&[(1, "CI", 10, 14, "success"), (2, "CI", 20, 26, "success")]),
            None,
        );
        let r = ci_reading(
            "acme/api",
            "main",
            &minutes(Per::Run, 5.0, 2),
            &runs,
            &[],
            at(0),
        );
        assert_eq!(r.value, Some(6.0));
        assert!(!r.met);
        assert_eq!(r.words, "6 minutes at most a run, over the newest 2 runs");
        assert_eq!(
            r.evidence,
            vec![
                Evidence::Run {
                    repo: "acme/api".into(),
                    id: 2
                },
                Evidence::Run {
                    repo: "acme/api".into(),
                    id: 1
                }
            ]
        );
        let r = ci_reading(
            "acme/api",
            "main",
            &minutes(Per::Run, 6.0, 2),
            &runs,
            &[],
            at(0),
        );
        assert!(r.met);
    }

    #[test]
    fn ci_minutes_per_job_leaves_skipped_jobs_out() {
        let runs = finished_runs(&listed(&[(1, "CI", 10, 40, "success")]), None);
        let jobs = [logs(&[
            (10, 13, "success"),
            (10, 30, "skipped"),
            (11, 15, "success"),
        ])];
        let r = ci_reading(
            "acme/api",
            "main",
            &minutes(Per::Job, 5.0, 1),
            &runs,
            &jobs,
            at(0),
        );
        assert_eq!(r.value, Some(4.0));
        assert!(r.met);
        assert_eq!(r.words, "4 minutes the longest job, over the newest run");
        assert_eq!(longest_job(&logs(&[(0, 9, "skipped")])), None);
    }

    #[test]
    fn runs_before_the_goal_was_agreed_never_meet_it() {
        let runs = finished_runs(
            &listed(&[(1, "CI", 0, 3, "success"), (2, "CI", 10, 12, "success")]),
            None,
        );
        let want = minutes(Per::Run, 5.0, 2);
        let r = ci_reading("acme/api", "main", &want, &runs, &[], at(9));
        assert_eq!(r.value, Some(3.0), "read, but not met");
        assert!(!r.met);
        assert!(
            ci_reading("acme/api", "main", &want, &runs, &[], at(1)).met,
            "2 minutes' slack"
        );
    }

    #[test]
    fn fewer_runs_than_it_wants_dont_meet_it() {
        let runs = finished_runs(&listed(&[(1, "CI", 10, 12, "success")]), None);
        let r = ci_reading(
            "acme/api",
            "main",
            &minutes(Per::Run, 5.0, 3),
            &runs,
            &[],
            at(0),
        );
        assert!(!r.met);
        let none = ci_reading(
            "acme/api",
            "main",
            &minutes(Per::Run, 5.0, 3),
            &[],
            &[],
            at(0),
        );
        assert_eq!(none.value, None);
        assert_eq!(
            none.failed.as_deref(),
            Some("no run on main of acme/api has finished yet")
        );
    }

    #[test]
    fn ci_green_counts_the_newest_successes_in_a_row() {
        let runs = finished_runs(
            &listed(&[
                (1, "Deploy", 10, 12, "failure"),
                (2, "Deploy", 20, 22, "success"),
                (3, "Deploy", 30, 32, "success"),
            ]),
            None,
        );
        let r = ci_reading(
            "acme/api",
            "main",
            &Want::Green { runs: 3 },
            &runs,
            &[],
            at(0),
        );
        assert_eq!(r.value, Some(2.0));
        assert!(!r.met);
        assert_eq!(r.words, "2 of the newest 3 runs green in a row");
        let two = ci_reading(
            "acme/api",
            "main",
            &Want::Green { runs: 2 },
            &runs[..2],
            &[],
            at(0),
        );
        assert!(two.met);
    }

    #[test]
    fn pr_stage_goes_from_none_to_merged() {
        let pr = PrPick::Head("fix-ci".into());
        let pull = |state: &str, merged: bool, overall: &str| json!({ "number": 12, "state": state, "merged": merged, "checks": { "overall": overall } });
        let stage = |p: Option<&Value>| {
            let f = pr_stage("acme/api", &pr, p);
            (f.value, f.met, f.words)
        };
        assert_eq!(
            stage(None),
            (
                Some(0.0),
                false,
                "the PR from fix-ci isn't opened yet".into()
            )
        );
        assert_eq!(
            stage(Some(&pull("open", false, "pending"))),
            (Some(1.0), false, "the PR from fix-ci open".into())
        );
        assert_eq!(
            stage(Some(&pull("open", false, "success"))),
            (
                Some(2.0),
                false,
                "the PR from fix-ci open, its checks green".into()
            )
        );
        assert_eq!(
            stage(Some(&pull("closed", true, "success"))),
            (Some(3.0), true, "the PR from fix-ci merged".into())
        );
        assert_eq!(stage(Some(&pull("closed", false, "failure"))).0, Some(0.0));
        let merged = pr_stage("acme/api", &pr, Some(&pull("closed", true, "success")));
        assert_eq!(
            merged.evidence,
            vec![Evidence::Pull {
                repo: "acme/api".into(),
                number: 12
            }]
        );
    }

    #[test]
    fn checks_green_is_met_on_success_alone() {
        let pr = PrPick::Number(12);
        let green = checks_green(
            "acme/api",
            &pr,
            Some(&json!({ "number": 12, "checks": { "overall": "success" } })),
        );
        assert_eq!((green.value, green.met), (Some(1.0), true));
        let red = checks_green(
            "acme/api",
            &pr,
            Some(&json!({ "number": 12, "checks": { "overall": "failure" } })),
        );
        assert_eq!((red.value, red.met), (Some(0.0), false));
        assert_eq!(red.words, "PR #12's checks failure");
    }

    #[test]
    fn the_newest_pull_of_a_head_is_found() {
        let list = json!({ "pulls": [
            { "number": 3, "head": "fix-ci", "created_at": stamp(0) },
            { "number": 9, "head": "fix-ci", "created_at": stamp(5) },
            { "number": 11, "head": "other", "created_at": stamp(9) },
        ] });
        assert_eq!(newest_pull(&list, "fix-ci"), Some(9));
        assert_eq!(newest_pull(&list, "gone"), None);
    }
}
