//! Act fully ships (DW-761): a pull request's review is recorded from its
//! review session, as Desk reads it, and a green pull request with a clean
//! review at its head is merged by the boss itself.
//!
//! - **`crew.start_session` with `review_of`** starts a review session of
//!   a pull request at its head now, read on GitHub as the person: Desk
//!   asks it for its verdict line ([`plan::REVIEW_VERDICT`]) and links it
//!   to the pull request.
//! - **`crew.record_review(session_id)`** takes nothing else: the session
//!   is one the boss runs, linked to its pull request; its folder's HEAD is
//!   the pull request's head; the verdict is the last `REVIEW:` line of its
//!   final reply, read from its transcript, naming that head.
//! - **`crew.github_merge(repo, number)`**, at Act fully only: its head is
//!   the branch of a session the boss runs, in that session's own
//!   repository; it is open, not a draft, mergeable, every check passed,
//!   and a clean review is recorded at its head. Squash where the
//!   repository allows it, else a merge commit, never a rebase; GitHub is
//!   asked to merge that reviewed head alone (`sha`). Every refusal says
//!   why, and none is retried silently.

use std::time::Duration;

use serde_json::{json, Value};
use uuid::Uuid;
use workforce_common::oversight::Level;
use workforce_desk::crew::{self, Kind, Party};
use workforce_desk::decision::{BossDecision, How, Need};
use workforce_desk::plan::{self, PrRef, PrReview};
use workforce_desk::TranscriptAsk;

use super::actions::Action;
use super::github::{self, GithubAccess, GithubError};
use super::tools::Bound;
use super::workspace::{self, Workspace};
use super::CrewHub;

pub(super) const RECORD: &str = "crew.record_review";
pub(super) const MERGE: &str = "crew.github_merge";

pub(super) const RECORD_DESCRIPTION: &str = "Record the verdict of a review session you started with crew.start_session and review_of (session_id), once its turn has ended: Desk reads its final reply's REVIEW line, and checks its folder is at the pull request's head. A clean review at the head is what crew.github_merge needs.";
pub(super) const MERGE_DESCRIPTION: &str = "Merge a pull request (repo as owner/name, number), at Act fully: its head is the branch of a session you run, in that session's own repository; it is open, not a draft, mergeable, every check passed, and crew.record_review recorded a clean review at its head. Desk squashes where the repository allows it, else merges; you're told why when it won't.";

/// A review session's final reply with no verdict line.
pub(crate) const NO_VERDICT: &str = "Its final reply has no REVIEW line: nudge it to end with `REVIEW: clean|changes <sha>`, then record again.";

/// What GitHub not having worked out mergeability yet is, after its tries.
pub(crate) const UNDECIDED: &str = "GitHub hasn't decided mergeability yet; try again";

/// How often mergeability is read again while GitHub works it out, and
/// how many more times.
#[cfg(not(test))]
const UNDECIDED_EVERY: Duration = Duration::from_secs(5);
#[cfg(test)]
const UNDECIDED_EVERY: Duration = Duration::from_millis(10);
const UNDECIDED_TRIES: usize = 3;

pub(super) fn record_schema() -> Value {
    json!({ "type": "object", "properties": {
        "session_id": { "type": "string", "description": "The review session's id" }
    }, "required": ["session_id"] })
}

pub(super) fn merge_schema() -> Value {
    json!({ "type": "object", "properties": {
        "repo": { "type": "string", "description": "The repository as owner/name" },
        "number": { "type": "integer", "description": "The pull request's number" },
        "reason": { "type": "string", "description": "Why, in a sentence the person reads in Activity" }
    }, "required": ["repo", "number"] })
}

/// `repo` (`owner/name`) and `number` as a pull request.
fn pr_ref(repo: &str, number: Option<i64>) -> Result<PrRef, String> {
    let (owner, name) = workforce_desk::goal::github_repo(repo)
        .ok_or("Say the repository as owner/name (driftwerk/workforce).")?;
    let number = number
        .filter(|n| *n > 0)
        .ok_or("Say which pull request: its number.")?;
    Ok(PrRef {
        owner: owner.to_string(),
        repo: name.to_string(),
        number,
    })
}

fn number_of(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| {
        v.as_str()
            .and_then(|s| s.trim().trim_start_matches('#').parse().ok())
    })
}

fn full_name(pr: &PrRef) -> String {
    format!("{}/{}", pr.owner, pr.repo)
}

/// A review's verdict, read from its final reply (`reply`), checked
/// against its folder's HEAD (`folder`) and the pull request's (`head`):
/// clean or not.
pub(crate) fn verdict_at(reply: &str, folder: &str, head: &str) -> Result<bool, String> {
    if folder != head {
        return Err(format!(
            "Its folder is at {folder}, the PR is at {head}: it didn't review the PR's head."
        ));
    }
    let (clean, sha) = plan::review_verdict(reply).ok_or(NO_VERDICT)?;
    match sha == head {
        true => Ok(clean),
        false => Err(format!("the review was of {sha}, the PR is at {head}")),
    }
}

/// How a pull request merges, once it may.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ready {
    pub sha: String,
    pub method: &'static str,
}

/// Why a pull request doesn't merge now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unready {
    /// GitHub is still working out whether it can.
    Undecided,
    No(String),
}

/// Whether `pull` (GitHub's `get_pull`, of `pr`) merges now in `repo`
/// (its `get_repo`), with `review` the newest recorded of it and
/// `branches` the `(origin, branch)` of each session the boss runs.
pub(crate) fn ready(
    pr: &PrRef,
    pull: &Value,
    repo: &Value,
    review: Option<&PrReview>,
    branches: &[(String, String)],
) -> Result<Ready, Unready> {
    let no = |why: String| Err(Unready::No(why));
    let named = full_name(pr);
    if pull["merged"].as_bool() == Some(true) {
        return no("It has merged already.".into());
    }
    if pull["state"].as_str() != Some("open") {
        return no(format!(
            "It isn't open (it is {}).",
            pull["state"].as_str().unwrap_or("unknown")
        ));
    }
    if pull["draft"].as_bool() != Some(false) {
        return no("It's a draft: it merges once it is marked ready.".into());
    }
    let head_repo = pull["head_repo"].as_str().unwrap_or_default();
    if !head_repo.eq_ignore_ascii_case(&named) {
        return no(format!(
            "Its head is in {}, not {named}: a pull request from a fork is the person's to merge.",
            if head_repo.is_empty() {
                "a repository GitHub doesn't name"
            } else {
                head_repo
            }
        ));
    }
    let branch = pull["head"].as_str().unwrap_or_default();
    if !branches
        .iter()
        .any(|(origin, b)| origin.eq_ignore_ascii_case(&named) && b == branch)
    {
        return no(format!(
            "Its head branch {branch} isn't the branch of a session you run in {named}: only a session's own work is merged."
        ));
    }
    let head = pull["head_sha"].as_str().unwrap_or_default();
    if !plan::is_sha(head) {
        return no("GitHub didn't say its head commit.".into());
    }
    let checks = &pull["checks"];
    let count = |k: &str| checks[k].as_u64().unwrap_or(0);
    if checks.is_null() {
        return no("Its checks couldn't be read, so it isn't known to be green.".into());
    }
    if count("total") == 0 {
        return no("No checks have reported on its head yet.".into());
    }
    if count("failure") > 0 {
        return no(format!(
            "Its checks failed ({}): have its session fix them and push.",
            checks["failed"]
                .as_array()
                .map(|f| f
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default()
        ));
    }
    if count("pending") > 0 || checks["overall"].as_str() != Some("success") {
        return no(
            "Its checks haven't all passed yet: watch them and merge once they have.".into(),
        );
    }
    match review {
        None => {
            return no(format!(
                "No review is recorded at its head {head}: start one with crew.start_session and review_of, then crew.record_review."
            ))
        }
        Some(r) if r.head_sha != head => {
            return no(format!(
                "The review was of {}, the PR is at {head}: review its head again.",
                r.head_sha
            ))
        }
        Some(r) if !r.clean => {
            return no(format!(
                "Its review at {head} asked for changes: have the session make them and push."
            ))
        }
        Some(_) => {}
    }
    match pull["mergeable"].as_bool() {
        None => return Err(Unready::Undecided),
        Some(false) => {
            return no(format!(
                "GitHub says it can't merge ({}): have its session bring it up to date.",
                pull["mergeable_state"].as_str().unwrap_or("conflicting")
            ))
        }
        Some(true) => {}
    }
    let method = match (
        repo["allow_squash_merge"].as_bool(),
        repo["allow_merge_commit"].as_bool(),
    ) {
        (Some(true), _) => "squash",
        (_, Some(true)) => "merge",
        _ => {
            return no(format!(
                "{named} allows neither squash nor merge commits, and Desk never rebases: it's the person's to merge."
            ))
        }
    };
    Ok(Ready {
        sha: head.to_string(),
        method,
    })
}

/// Merges `pr` as the person in `access` once it is [`ready`], reading it
/// again while GitHub works out whether it can: what GitHub said of the
/// merge, or why it didn't.
pub(crate) async fn merge(
    access: &GithubAccess<'_>,
    pr: &PrRef,
    review: Option<&PrReview>,
    branches: &[(String, String)],
) -> Result<(Ready, Value), String> {
    let args = json!({ "owner": pr.owner, "repo": pr.repo, "number": pr.number });
    let mut tries = 0;
    let ready = loop {
        let (_, got) = github::calls_as_person(
            access,
            vec![
                ("github.get_pull", args.clone()),
                ("github.get_repo", args.clone()),
            ],
        )
        .await
        .map_err(|e| e.to_string())?;
        match ready(pr, &got[0], &got[1], review, branches) {
            Ok(r) => break r,
            Err(Unready::No(why)) => return Err(why),
            Err(Unready::Undecided) if tries == UNDECIDED_TRIES => return Err(UNDECIDED.into()),
            Err(Unready::Undecided) => {
                tries += 1;
                tokio::time::sleep(UNDECIDED_EVERY).await;
            }
        }
    };
    let merged = github::merge_as_person(
        access,
        json!({
            "owner": pr.owner,
            "repo": pr.repo,
            "number": pr.number,
            "merge_method": ready.method,
            "sha": ready.sha,
        }),
    )
    .await
    .map_err(|e| match e {
        GithubError::Other(said) => format!("GitHub didn't merge it: {said}"),
        e => e.to_string(),
    })?;
    match merged["merged"].as_bool() {
        Some(true) => Ok((ready, merged)),
        _ => Err(format!(
            "GitHub didn't merge it: {}",
            merged["message"].as_str().unwrap_or("it gave no reason")
        )),
    }
}

impl CrewHub {
    fn access<'a>(b: &'a Bound) -> GithubAccess<'a> {
        GithubAccess {
            vault: b.service.vault.clone(),
            global: b.service.global_vault.clone(),
            config: &b.service.config,
            registry: &b.service.tools,
        }
    }

    /// A pull request's head now, read on GitHub as the person.
    async fn pr_head(b: &Bound, pr: &PrRef) -> Result<String, String> {
        let (_, got) = github::calls_as_person(
            &Self::access(b),
            vec![(
                "github.get_pull",
                json!({ "owner": pr.owner, "repo": pr.repo, "number": pr.number }),
            )],
        )
        .await
        .map_err(|e| e.to_string())?;
        got[0]["head_sha"]
            .as_str()
            .filter(|h| plan::is_sha(h))
            .map(str::to_string)
            .ok_or_else(|| "GitHub didn't say the pull request's head commit.".into())
    }

    /// A start of a review session, `review_of` as the boss said it
    /// (`{repo, number}`), at the pull request's head now: the head is
    /// GitHub's, never the boss's, in the action and in `kept`, the input
    /// a suggestion of it keeps.
    pub(super) async fn review_head(
        &self,
        b: &Bound,
        action: &mut Action,
        kept: &mut Value,
    ) -> Result<(), String> {
        let Action::StartSession {
            review_of: Some(r), ..
        } = action
        else {
            return Ok(());
        };
        r.head_sha = Self::pr_head(b, &r.pr).await?;
        kept["review_of"]["head_sha"] = json!(r.head_sha);
        Ok(())
    }

    /// The `(origin, branch)` of each session `boss` runs, as its folder is.
    async fn branches_of(&self, boss: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for c in self.desk.charges().into_iter().filter(|c| c.boss == boss) {
            let Ok(v) = self.desk.view(c.session) else {
                continue;
            };
            if let Workspace::Repo(r) = workspace::workspace_of(&v.cwd).await {
                if let (Some(origin), Some(branch)) = (r.origin, r.branch) {
                    out.push((origin, branch));
                }
            }
        }
        out
    }

    /// `crew.record_review`, for the boss `b`.
    pub(super) async fn record_review_tool(
        &self,
        b: &Bound,
        input: &Value,
    ) -> Result<Value, String> {
        let boss = b.service.agent_id.as_str();
        let id: Uuid = input["session_id"]
            .as_str()
            .and_then(|s| s.trim().parse().ok())
            .ok_or("Say which review session (session_id).")?;
        self.session_seen(id, &b.seen)?;
        let v = self
            .desk
            .view(id)
            .map_err(|_| self.not_ours(&b.seen.scope))?;
        if !self
            .desk
            .charge_of(id)
            .is_some_and(|c| c.live() && c.boss == boss)
        {
            return Err(
                "It isn't a session you run: only your own review sessions are recorded.".into(),
            );
        }
        let pr = self
            .desk
            .store()
            .pr_review_of(id)
            .await
            .map_err(|e| format!("Desk couldn't read what it reviews: {e}"))?
            .ok_or("It wasn't started to review a pull request: start a review with crew.start_session and review_of.")?;
        if v.state.busy() {
            return Err("It's still reviewing: record its verdict once its turn has ended.".into());
        }
        let head = Self::pr_head(b, &pr).await?;
        let folder = match workspace::workspace_of(&v.cwd).await {
            Workspace::Repo(r) => r.head,
            _ => None,
        }
        .ok_or("Its folder's HEAD couldn't be read.")?;
        let page = self
            .desk
            .transcript_page(id, TranscriptAsk::default())
            .await?;
        let reply = page
            .exchanges
            .first()
            .and_then(|x| x["answer"].as_str())
            .unwrap_or_default();
        let clean = verdict_at(reply, &folder, &head)?;
        let review = PrReview {
            pr: pr.clone(),
            head_sha: head.clone(),
            clean,
            by_session: id,
            at: chrono::Utc::now(),
        };
        self.desk
            .store()
            .pr_review_put(&review)
            .await
            .map_err(|e| format!("Desk couldn't keep the review: {e}"))?;
        let verdict = match clean {
            true => "clean",
            false => "changes",
        };
        Ok(json!({
            "recorded": verdict,
            "pull_request": format!("{}#{}", full_name(&pr), pr.number),
            "head_sha": head,
        }))
    }

    /// `crew.github_merge`, for the boss `b`: merged and recorded as its
    /// decision, or refused with why, which Activity notes too.
    pub(super) async fn github_merge_tool(
        &self,
        b: &Bound,
        input: &Value,
    ) -> Result<Value, String> {
        let boss = b.service.agent_id.as_str();
        if workforce_desk::grant::is_served(boss)
            || self.level_now(boss, &b.oversight).0 != Level::Full
        {
            return Err("Merging a pull request yourself is for Act fully only: tell the person it's ready to merge.".into());
        }
        let pr = pr_ref(
            input["repo"].as_str().unwrap_or_default(),
            number_of(&input["number"]),
        )?;
        let why = input["reason"]
            .as_str()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .unwrap_or("It is green, with a clean review at its head.");
        let review = self
            .desk
            .store()
            .pr_review_newest(&pr)
            .await
            .map_err(|e| format!("Desk couldn't read its review: {e}"))?;
        let branches = self.branches_of(boss).await;
        let named = format!("{}#{}", full_name(&pr), pr.number);
        let actor = Party::manager(boss, &super::name_of(&b.service));
        match merge(&Self::access(b), &pr, review.as_ref(), &branches).await {
            Ok((ready, merged)) => {
                let what = format!("merged {named}");
                let mut d = BossDecision::new(boss, Need::Choice, &what, why, How::Own)
                    .briefly(&format!("Merged {named}."), None);
                d.session = review.as_ref().map(|r| r.by_session);
                d.action = json!({ "tool": MERGE, "input": { "repo": full_name(&pr), "number": pr.number, "sha": ready.sha, "merge_method": ready.method } });
                self.record(&mut d).await;
                let mut e = crew::Event::new(Kind::Approved, actor, None, what);
                e.reason = Some(why.to_string());
                self.desk.note(e);
                Ok(json!({
                    "merged": named,
                    "sha": merged["sha"],
                    "merge_method": ready.method,
                    "next": "Move its ticket to done and start the next ticket.",
                }))
            }
            Err(refused) => {
                let mut e = crew::Event::new(
                    Kind::Told,
                    actor,
                    None,
                    format!("didn't merge {named}: {refused}"),
                );
                e.reason = Some(why.to_string());
                self.desk.note(e);
                Err(refused)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use workforce_agent::runner::CURRENT_USER_ID;

    const HEAD: &str = "1111111111111111111111111111111111111111";
    const OLD: &str = "2222222222222222222222222222222222222222";

    fn pr() -> PrRef {
        PrRef {
            owner: "acme".into(),
            repo: "web".into(),
            number: 7,
        }
    }

    fn review(at: &str, clean: bool) -> PrReview {
        PrReview {
            pr: pr(),
            head_sha: at.into(),
            clean,
            by_session: Uuid::new_v4(),
            at: chrono::Utc::now(),
        }
    }

    /// `get_pull` of a green, mergeable PR from the session's branch.
    fn pull() -> Value {
        json!({
            "number": 7, "state": "open", "merged": false, "draft": false,
            "mergeable": true, "mergeable_state": "clean",
            "head": "dw-761", "head_repo": "acme/web", "head_sha": HEAD,
            "checks": {"overall": "success", "total": 3, "success": 3, "failure": 0, "pending": 0},
        })
    }

    fn squash() -> Value {
        json!({ "allow_squash_merge": true, "allow_merge_commit": true })
    }

    fn mine() -> Vec<(String, String)> {
        vec![("acme/web".into(), "dw-761".into())]
    }

    fn refused(pull: &Value, repo: &Value, r: Option<&PrReview>) -> String {
        match ready(&pr(), pull, repo, r, &mine()) {
            Err(Unready::No(why)) => why,
            other => panic!("not refused: {other:?}"),
        }
    }

    #[test]
    fn a_green_pull_request_with_a_clean_review_at_its_head_squashes_that_head() {
        let clean = review(HEAD, true);
        assert_eq!(
            ready(&pr(), &pull(), &squash(), Some(&clean), &mine()),
            Ok(Ready {
                sha: HEAD.into(),
                method: "squash"
            })
        );
        let merge_only = json!({ "allow_squash_merge": false, "allow_merge_commit": true });
        assert_eq!(
            ready(&pr(), &pull(), &merge_only, Some(&clean), &mine()).map(|r| r.method),
            Ok("merge")
        );
        let rebase_only = json!({ "allow_squash_merge": false, "allow_merge_commit": false });
        assert!(refused(&pull(), &rebase_only, Some(&clean)).contains("never rebases"));
    }

    #[test]
    fn a_merge_is_refused_with_why_for_each_unmet_condition() {
        let clean = review(HEAD, true);
        let with = |k: &str, v: Value| {
            let mut p = pull();
            p[k] = v;
            p
        };
        let stale = refused(&pull(), &squash(), Some(&review(OLD, true)));
        assert!(
            stale.contains(&format!("The review was of {OLD}, the PR is at {HEAD}")),
            "{stale}"
        );
        assert!(refused(&pull(), &squash(), None).contains("No review is recorded"));
        assert!(
            refused(&pull(), &squash(), Some(&review(HEAD, false))).contains("asked for changes")
        );
        let theirs = refused(&with("head", json!("feature")), &squash(), Some(&clean));
        assert!(
            theirs.contains("isn't the branch of a session you run"),
            "{theirs}"
        );
        let fork = refused(
            &with("head_repo", json!("evil/web")),
            &squash(),
            Some(&clean),
        );
        assert!(fork.contains("from a fork"), "{fork}");
        assert!(refused(&with("draft", json!(true)), &squash(), Some(&clean)).contains("draft"));
        assert!(
            refused(&with("state", json!("closed")), &squash(), Some(&clean))
                .contains("isn't open")
        );
        let pending = with(
            "checks",
            json!({"overall": "pending", "total": 3, "success": 2, "failure": 0, "pending": 1}),
        );
        assert!(refused(&pending, &squash(), Some(&clean)).contains("haven't all passed"));
        let red = with(
            "checks",
            json!({"overall": "failure", "total": 3, "success": 2, "failure": 1, "pending": 0, "failed": ["Test"]}),
        );
        assert!(refused(&red, &squash(), Some(&clean)).contains("failed (Test)"));
        assert_eq!(
            ready(
                &pr(),
                &with("mergeable", Value::Null),
                &squash(),
                Some(&clean),
                &mine()
            ),
            Err(Unready::Undecided)
        );
        assert!(
            refused(&with("mergeable", json!(false)), &squash(), Some(&clean))
                .contains("can't merge")
        );
    }

    #[test]
    fn a_verdict_is_recorded_only_at_the_folders_and_the_prs_head() {
        let said = |v: &str| format!("Looked at everything.\n\nREVIEW: {v}");
        assert_eq!(
            verdict_at(&said(&format!("clean {HEAD}")), HEAD, HEAD),
            Ok(true)
        );
        assert_eq!(
            verdict_at(&said(&format!("changes {HEAD}")), HEAD, HEAD),
            Ok(false)
        );
        assert_eq!(
            verdict_at(&said(&format!("clean {OLD}")), HEAD, HEAD),
            Err(format!("the review was of {OLD}, the PR is at {HEAD}"))
        );
        assert_eq!(
            verdict_at("All good, ship it.", HEAD, HEAD),
            Err(NO_VERDICT.to_string())
        );
        assert!(verdict_at(&said(&format!("clean {HEAD}")), OLD, HEAD)
            .unwrap_err()
            .contains(&format!("Its folder is at {OLD}, the PR is at {HEAD}")));
    }

    /// GitHub as `get_pull`, `get_repo` and `pr_merge` read `acme/web#7`,
    /// mergeable as `mergeable` says.
    async fn github(server: &MockServer, mergeable: Value) {
        Mock::given(method("GET"))
            .and(path("/repos/acme/web/pulls/7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "number": 7, "state": "open", "merged": false, "draft": false,
                "mergeable": mergeable, "mergeable_state": "clean",
                "head": {"ref": "dw-761", "sha": HEAD, "repo": {"full_name": "acme/web"}},
                "base": {"ref": "main", "repo": {"full_name": "acme/web"}},
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/acme/web/commits/{HEAD}/check-runs")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"check_runs": [
                    {"name": "Test", "status": "completed", "conclusion": "success"},
                ]})),
            )
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/web"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "full_name": "acme/web", "allow_squash_merge": true, "allow_merge_commit": true,
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn the_merge_squashes_the_reviewed_head_and_never_while_github_is_undecided() {
        let server = MockServer::start().await;
        github(&server, json!(true)).await;
        Mock::given(method("PUT"))
            .and(path("/repos/acme/web/pulls/7/merge"))
            .and(body_partial_json(json!({"sha": HEAD, "merge_method": "squash"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "merged": true, "sha": "3333333333333333333333333333333333333333", "message": "Merged",
            })))
            .expect(1)
            .mount(&server)
            .await;
        let h = super::super::github::tests::hub(&server);
        let clean = review(HEAD, true);
        let (ready, merged) = CURRENT_USER_ID
            .scope(
                "ann".into(),
                merge(&h.access(), &pr(), Some(&clean), &mine()),
            )
            .await
            .unwrap();
        assert_eq!(ready.method, "squash");
        assert_eq!(merged["merged"], true);

        let undecided = MockServer::start().await;
        super::super::github::tests::no_writes(&undecided).await;
        github(&undecided, Value::Null).await;
        let h = super::super::github::tests::hub(&undecided);
        let said = CURRENT_USER_ID
            .scope(
                "ann".into(),
                merge(&h.access(), &pr(), Some(&clean), &mine()),
            )
            .await
            .unwrap_err();
        assert_eq!(said, UNDECIDED);
        let reads = undecided
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/repos/acme/web/pulls/7")
            .count();
        assert_eq!(reads, 1 + UNDECIDED_TRIES, "read again three times");
    }
}
