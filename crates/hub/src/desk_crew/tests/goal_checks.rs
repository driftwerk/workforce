//! The hub checks goals on a schedule (DW-729): no boss turn and no watch
//! keeps them going (the night of 22 November is replayed in
//! [`super::plan_watch`]). A met reading closes the goal, its ticket and
//! its plan's watches without a word, and wakes the boss's closing turn
//! once, a paused plan's once it runs again; a plan the person paused is
//! read and wakes no one; a boss message held from a goal's session, and a
//! part read failing, each wake its boss once; a checkpoint on the goal's
//! work is taken; a session gone stalls as any other; and what was said is
//! said once across a restart.

use super::goals::{boss_on, boss_until, run, ACTS};
use super::plan_watch::{asked_with, turn_ends};
use super::*;
use workforce_desk::board::TicketState;
use workforce_desk::boss_watch::{NewWatch, WatchKind};
use workforce_desk::crew::Party;
use workforce_desk::goal::{GoalResult, Measure, Per, Want};
use workforce_desk::line::{LineKind, LineMessage};
use workforce_desk::model::SessionView;
use workforce_desk::plan::{Draft, DraftTicket, Paused, PlanBudget, PlanState};
use workforce_desk::{NewBoard, NewPlan, NewTicket};

/// Act, watching, with a stall window of `minutes` crew minutes (a tenth of
/// a second each).
pub(super) fn checking(minutes: u32) -> String {
    format!("{ACTS}[oversight.work]\nstall_minutes = {minutes}\n")
}

pub(super) const STALLED: &str = "No progress toward";

/// GitHub as the person's key reads it: acme/mantis's main has three
/// finished runs of CI since `from`, each job of each taking `minutes`,
/// and pull request #40 into main, last updated at `from`. Commit
/// [`COMMIT`] is `compared` to main: "ahead" (on it, and after where main
/// stood as the goal was agreed, [`MAIN_HEAD`]), "old" (on it, but there
/// already then) or another status (off it). Runs answer `status` (an error
/// from 400).
pub(super) async fn mantis(
    from: chrono::DateTime<chrono::Utc>,
    minutes: i64,
    compared: &str,
) -> wiremock::MockServer {
    let gh = wiremock::MockServer::start().await;
    mantis_reads(&gh, from, minutes, compared, 200).await;
    gh
}

/// `gh` answering as [`mantis`] says, whatever it answered before.
async fn mantis_reads(
    gh: &wiremock::MockServer,
    from: chrono::DateTime<chrono::Utc>,
    minutes: i64,
    compared: &str,
    status: u16,
) {
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, ResponseTemplate};
    gh.reset().await;
    let stamp = |m: i64| (from + chrono::Duration::minutes(m)).to_rfc3339();
    let conclusion = if minutes <= 5 { "success" } else { "failure" };
    // New runs when CI changes: a finished run's jobs never do.
    let runs: Vec<Value> = [9u64, 8, 7]
        .iter()
        .map(|id| id + 100 * minutes.unsigned_abs())
        .enumerate()
        .map(|(i, id)| {
            let start = 30 * (2 - i as i64);
            json!({ "id": id, "name": "CI", "head_branch": "main", "status": "completed",
                "conclusion": conclusion, "run_number": id, "run_attempt": 1,
                "html_url": format!("https://github.com/acme/mantis/actions/runs/{id}"),
                "created_at": stamp(start), "run_started_at": stamp(start),
                "updated_at": stamp(start + minutes) })
        })
        .collect();
    Mock::given(method("GET"))
        .and(path("/repos/acme/mantis/actions/runs"))
        .respond_with(
            ResponseTemplate::new(status)
                .set_body_json(json!({ "total_count": runs.len(), "workflow_runs": runs })),
        )
        .mount(gh)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/repos/acme/mantis/actions/runs/[0-9]+/jobs$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({ "total_count": 1, "jobs": [{
            "id": 1, "name": "test", "status": "completed", "conclusion": conclusion,
            "started_at": stamp(0), "completed_at": stamp(minutes), "steps": [] }] }),
        ))
        .mount(gh)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/repos/acme/mantis/actions/runs/[0-9]+$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 9, "name": "CI", "head_branch": "main", "status": "completed",
            "conclusion": conclusion, "created_at": stamp(0), "updated_at": stamp(minutes) })))
        .mount(gh)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/mantis/pulls/40"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 40, "state": "open", "merged": false, "updated_at": stamp(0),
            "head": { "ref": "fix/cache", "sha": "b".repeat(40) }, "base": { "ref": "main" } })))
        .mount(gh)
        .await;
    let (on, after) = match compared {
        "old" => ("ahead", "behind"),
        on => (on, "ahead"),
    };
    let compare = |base: &str, head: &str, status: &str, sha: &str| {
        Mock::given(method("GET"))
            .and(path(format!("/repos/acme/mantis/compare/{base}...{head}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": status, "base_commit": { "sha": sha }, "ahead_by": 1, "behind_by": 0,
                "total_commits": 1, "files": [] })))
    };
    let whole = format!("{COMMIT}{}", "0".repeat(40 - COMMIT.len()));
    for m in [
        compare("main", "main", "identical", MAIN_HEAD),
        compare(COMMIT, "main", on, &whole),
        compare(MAIN_HEAD, COMMIT, after, MAIN_HEAD),
    ] {
        m.mount(gh).await;
    }
}

/// The commit a boss's checkpoint cites.
pub(super) const COMMIT: &str = "d05e8a4c0ffee";
/// Where acme/mantis's main stood as the goal was agreed.
const MAIN_HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// Each CI job on acme/mantis's main at most 5 minutes, three runs in a
/// row, and, when `judged`, only the services a change touches tested.
pub(super) fn fast_ci(judged: bool) -> GoalResult {
    let mut parts = vec![Measure::Ci {
        repo: "acme/mantis".into(),
        workflow: None,
        branch: "main".into(),
        want: Want::Minutes {
            per: Per::Job,
            at_most: 5.0,
            runs: 3,
        },
    }];
    if judged {
        parts.push(Measure::Judged {
            evidence: "only the services a change touches are tested".into(),
        });
    }
    GoalResult {
        words: "Done when each CI job on main takes at most 5 minutes".into(),
        parts,
    }
}

/// An approved goal plan taking on one board ticket in `folder`, which
/// stands for the goal, measured by `r`, its session started by the
/// runner.
pub(super) async fn goal_running(
    d: &Desk,
    folder: &Path,
    r: GoalResult,
) -> (uuid::Uuid, SessionView) {
    goal_capped(d, folder, r, None).await
}

/// [`goal_running`], capped at `budget`.
async fn goal_capped(
    d: &Desk,
    folder: &Path,
    r: GoalResult,
    budget: Option<PlanBudget>,
) -> (uuid::Uuid, SessionView) {
    let board = d
        .create_board(NewBoard {
            name: "Mantis".into(),
            group: None,
            prefix: None,
        })
        .await
        .unwrap()
        .id;
    let ticket = d
        .create_ticket(
            NewTicket {
                board,
                title: "Fast CI".into(),
                body: String::new(),
                state: Some(TicketState::Todo),
                needs: vec![],
            },
            &By::you(),
        )
        .await
        .unwrap();
    let p = d
        .propose_plan(
            NewPlan {
                draft: Draft {
                    goal: "Mantis CI tests only what a change touches".into(),
                    tickets: vec![DraftTicket {
                        adopt: Some(ticket.ticket.id),
                        title: "Fast CI".into(),
                        folder: folder.to_string_lossy().into_owned(),
                        ..Default::default()
                    }],
                },
                board: None,
                parallel: None,
                engine: Engine::Claude,
                group: None,
                budget,
                result: Some(r),
                goal_ticket: None,
            },
            &Party::manager("desk", "Desk"),
            &By::manager("desk", "Desk", "Sam agreed it."),
        )
        .await
        .unwrap();
    let id = p.summary.plan.id;
    d.approve_agreed(id, 1, "keep Mantis CI fast", chrono::Utc::now(), &By::manager("desk", "Desk", "Sam agreed it."))
        .await
        .unwrap();
    let s = eventually("its ticket's session", async || {
        let held = d
            .plan(id)
            .await
            .ok()?
            .tickets
            .first()?
            .ticket
            .ticket
            .session()?;
        d.view(held).ok()
    })
    .await;
    (id, s)
}

pub(super) const CI: &str = "on CI on main of acme/mantis";

/// How many of the boss's turns were asked with all of `words`, once the
/// checks have had `quiet` to ask more.
async fn asked_after(llm: &wiremock::MockServer, words: &[&str], quiet: u64) -> usize {
    tokio::time::sleep(Duration::from_millis(quiet)).await;
    asked_with(llm, words).await
}

/// A watch of plan `id`, due long after the test.
async fn watching(d: &Desk, id: uuid::Uuid) -> uuid::Uuid {
    let now = chrono::Utc::now();
    let w = d
        .store()
        .watch_open(&NewWatch {
            boss: "desk".into(),
            person: PERSON.into(),
            kind: WatchKind::Time,
            repo: None,
            number: None,
            run_id: None,
            until_merged: false,
            note: "check CI tomorrow".into(),
            session: None,
            plan: Some(id),
            due_at: Some(now + chrono::Duration::hours(10)),
            expires_at: now + chrono::Duration::hours(20),
        })
        .await
        .unwrap();
    match w {
        workforce_desk::boss_watch::Opened::Started(w) => w.id,
        other => panic!("{other:?}"),
    }
}

/// How many of the boss's turns came once its goal was met, of any kind:
/// each carries that it is met. Counted once nothing more comes.
async fn turns_after_met(llm: &wiremock::MockServer) -> usize {
    let mut seen = asked_with(llm, &["The goal is met"]).await;
    loop {
        tokio::time::sleep(Duration::from_millis(800)).await;
        let now = asked_with(llm, &["The goal is met"]).await;
        if now == seen {
            return now;
        }
        seen = now;
    }
}

/// The boss's closing turns of a goal read met.
const CLOSING: &[&str] = &["The goal is met: it reads met", "closing summary"];

#[tokio::test(flavor = "multi_thread")]
async fn a_met_reading_closes_the_goal_and_ends_its_plans_watches_without_a_word() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let from = chrono::Utc::now();
        let gh = mantis(from, 22, "ahead").await;
        let (_, _, d, llm) = boss_on(&base, pool, &checking(5), &gh).await;
        let (id, _) = goal_running(&d, &base, fast_ci(false)).await;
        let w = watching(&d, id).await;
        // CI gets fast: its next reading meets the goal.
        mantis_reads(&gh, from, 4, "ahead", 200).await;
        let p = eventually("the goal met and closed", async || {
            let p = d.plan(id).await.ok()?;
            (p.summary.plan.state == PlanState::Done).then_some(p)
        })
        .await;
        let met = p.summary.plan.met.clone().expect("met");
        assert!(met.why.starts_with("it reads met:"), "{met:?}");
        let ticket = p.summary.plan.goal_ticket.expect("its ticket");
        assert_eq!(
            d.ticket(ticket).await.unwrap().ticket.state,
            TicketState::Done
        );
        assert_eq!(turns_after_met(&llm).await, 1, "one turn once it is met: its closing");
        assert_eq!(asked_with(&llm, CLOSING).await, 1);
        assert!(d
            .store()
            .watches_open("desk", PERSON)
            .await
            .unwrap()
            .iter()
            .all(|o| o.id != w));
        assert!(
            d.store().watches_undelivered().await.unwrap().is_empty(),
            "ended silently: the closing turn says it"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_goal_met_while_the_person_paused_it_is_closed_once_they_resume_it() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let from = chrono::Utc::now();
        let gh = mantis(from, 22, "ahead").await;
        let (_, _, d, llm) = boss_on(&base, pool, &checking(5), &gh).await;
        let (id, _) = goal_running(&d, &base, fast_ci(false)).await;
        d.pause_plan(id, Paused::Person, &By::you()).await.unwrap();
        mantis_reads(&gh, from, 4, "ahead", 200).await;
        eventually("the paused goal read met", async || {
            d.plan(id).await.ok()?.summary.plan.met.map(|_| ())
        })
        .await;
        assert_eq!(
            asked_after(&llm, CLOSING, 800).await,
            0,
            "no turn while paused"
        );
        assert_eq!(
            d.plan(id).await.unwrap().summary.plan.state,
            PlanState::Paused
        );

        d.resume_plan(id, &[Paused::Person], &By::you())
            .await
            .unwrap();
        eventually("closed once it runs", async || {
            (d.plan(id).await.ok()?.summary.plan.state == PlanState::Done).then_some(())
        })
        .await;
        assert_eq!(turns_after_met(&llm).await, 1, "one turn once it is met: its closing");
        assert_eq!(asked_with(&llm, CLOSING).await, 1);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_goal_the_person_paused_is_read_and_wakes_no_one_one_paused_for_budget_wakes() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = mantis(chrono::Utc::now(), 22, "diverged").await;
        let (_, crew, d, llm) = boss_on(&base, pool, &checking(5), &gh).await;
        d.count_budgets_in(chrono_tz::UTC);
        let budget = PlanBudget::usd(10.0);
        let (id, s) = goal_capped(&d, &base, fast_ci(false), Some(budget)).await;
        turn_ends(&d, &base, &s, "Done for now.").await;
        d.pause_plan(id, Paused::Person, &By::you()).await.unwrap();
        eventually("the paused goal read", async || {
            d.plan(id).await.ok()?.goal?.parts.first()?.newest.clone()
        })
        .await;
        assert_eq!(
            asked_after(&llm, &[STALLED, CI], 1500).await,
            0,
            "the person's pause wakes no one"
        );
        assert!(crew.goal_notes("desk", chrono::Utc::now()).is_empty());

        d.resume_plan(id, &[Paused::Person], &By::you())
            .await
            .unwrap();
        super::plans::spent(&d, s.id, 8.5).await;
        eventually("paused at its budget's warning", async || {
            let p = d.plan(id).await.ok()?.summary.plan;
            (p.paused == Some(Paused::Budget)).then_some(())
        })
        .await;
        eventually("a budget pause woken, said as one", async || {
            (asked_with(&llm, &[STALLED, CI, "paused at its budget"]).await > 0).then_some(())
        })
        .await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_boss_message_held_from_a_goals_session_wakes_its_boss_once() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = mantis(chrono::Utc::now(), 4, "ahead").await;
        let (_, crew, d, llm) = boss_on(&base, pool, &checking(5), &gh).await;
        let (_, s) = goal_running(&d, &base, fast_ci(true)).await;
        // Its turn never ends: the boss's message waits behind it.
        let mut held = LineMessage::new(
            Party::manager("desk", "Desk"),
            Party::session(&s),
            LineKind::Tell,
            "Switch the cache work back onto main.".into(),
            1,
        );
        held.at = chrono::Utc::now() - chrono::Duration::hours(1);
        d.store().put_line(&held).await.unwrap();
        let undelivered = ["Your message to", "hasn't gone in"];
        eventually("its boss told", async || {
            (asked_with(&llm, &undelivered).await > 0).then_some(())
        })
        .await;
        assert_eq!(asked_after(&llm, &undelivered, 1000).await, 1, "told once");
        assert!(crew
            .goal_checked_sessions()
            .await
            .contains(&s.id.to_string()));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_part_read_failing_six_times_in_a_row_wakes_its_boss_once() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = wiremock::MockServer::start().await;
        mantis_reads(&gh, chrono::Utc::now(), 22, "ahead", 503).await;
        let (_, _, d, llm) = boss_on(&base, pool, &checking(1440), &gh).await;
        let (id, _) = goal_running(&d, &base, fast_ci(false)).await;
        let failing = [
            "Desk can't read the goal's CI on main of acme/mantis",
            "reads in a row",
        ];
        eventually("six reads failed", async || {
            let p = d.plan(id).await.ok()?.goal?;
            (p.parts.first()?.failing >= 6).then_some(())
        })
        .await;
        eventually("its boss told why", async || {
            (asked_with(&llm, &failing).await > 0).then_some(())
        })
        .await;
        eventually("a seventh read failed", async || {
            let p = d.plan(id).await.ok()?.goal?;
            (p.parts.first()?.failing >= 7).then_some(())
        })
        .await;
        assert_eq!(
            asked_after(&llm, &failing, 300).await,
            1,
            "once a run of failures"
        );
        assert_eq!(asked_with(&llm, &[STALLED]).await, 0);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_checkpoint_takes_only_the_goals_work_since_it_was_agreed() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        // The runs and the pull request are from before the goal; the
        // commit was on main already as it was agreed.
        let from = chrono::Utc::now() - chrono::Duration::hours(3);
        let gh = mantis(from, 22, "old").await;
        let (overseer, _, d, _) = boss_on(&base, pool, &checking(1440), &gh).await;
        let (id, _) = goal_running(&d, &base, fast_ci(true)).await;
        let main = workforce_desk::goal::head_key("acme/mantis", "main");
        eventually("where main stood kept", async || {
            let heads = d.store().goal_heads(id).await.ok()?;
            (heads.get(&main).map(String::as_str) == Some(MAIN_HEAD)).then_some(())
        })
        .await;
        let checkpoint = |evidence: Value| {
            run(
                &overseer,
                "crew.plan_update",
                json!({ "plan_id": id.to_string(), "why": "Showing the work",
                        "checkpoint": { "part": 2, "said": "Only api's tests ran",
                                        "evidence": [evidence] } }),
            )
        };
        let commit = json!({ "kind": "commit", "repo": "acme/mantis", "sha": COMMIT });
        for (evidence, refused) in [
            (commit.clone(), "Commit d05e8a4 of acme/mantis was there before"),
            (json!({ "kind": "run", "repo": "acme/mantis", "id": 9 }), "Run 9 of acme/mantis was there before"),
            (json!({ "kind": "pull", "repo": "acme/mantis", "number": 40 }), "PR #40 of acme/mantis was there before"),
        ] {
            let r = checkpoint(evidence).await;
            assert!(!r.success, "{r:?}");
            let error = r.error.unwrap_or_default();
            assert!(error.starts_with(refused), "{error}");
        }

        // A commit on main since: taken, by its whole id.
        mantis_reads(&gh, from, 22, "ahead", 200).await;
        let before = chrono::Utc::now();
        let r = checkpoint(commit).await;
        assert!(r.success, "{r:?}");
        let p = d.plan(id).await.unwrap();
        let shown = p
            .facts
            .iter()
            .find(|f| f.checked && f.part == Some(1))
            .unwrap_or_else(|| panic!("{:?}", p.facts));
        assert_eq!(
            shown.evidence,
            [workforce_desk::goal::Evidence::Commit {
                repo: "acme/mantis".into(),
                sha: format!("{COMMIT}{}", "0".repeat(40 - COMMIT.len())),
            }]
        );
        let goal = p.goal.expect("its progress");
        assert!(goal.parts[1].improved_at.is_some_and(|at| at >= before), "its stall starts again");
        assert!(!goal.parts[0].met, "CI is still stuck");
        assert!(p.summary.plan.met.is_none());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_goal_whose_session_is_gone_still_stalls() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = mantis(chrono::Utc::now(), 22, "diverged").await;
        let (_, _, d, llm) = boss_on(&base, pool, &checking(5), &gh).await;
        let (_, s) = goal_running(&d, &base, fast_ci(false)).await;
        d.remove(s.id, workforce_desk::Removal::default())
            .await
            .unwrap();
        eventually("its boss woken: the session gone", async || {
            (asked_with(&llm, &[STALLED, CI]).await > 0).then_some(())
        })
        .await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn what_a_goals_boss_was_told_is_told_once_across_a_restart() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = mantis(chrono::Utc::now(), 22, "diverged").await;
        let stop = CancellationToken::new();
        let (overseer, _, d, llm) = boss_until(&base, pool, &checking(20), &gh, stop.clone()).await;
        let (id, _) = goal_running(&d, &base, fast_ci(false)).await;
        eventually("its first spell said", async || {
            (asked_with(&llm, &[STALLED, CI]).await > 0).then_some(())
        })
        .await;
        let read = d.plan(id).await.unwrap().goal.unwrap().parts[0]
            .newest
            .clone()
            .unwrap();
        stop.cancel();
        let _crew = CrewHub::start(
            hub(std::slice::from_ref(&overseer)),
            d.clone(),
            FAST,
            CancellationToken::new(),
        );
        eventually("read again after the restart", async || {
            let p = d.plan(id).await.ok()?.goal?;
            (p.parts.first()?.newest.as_ref()?.at > read.at).then_some(())
        })
        .await;
        assert_eq!(
            asked_after(&llm, &[STALLED, CI], 300).await,
            1,
            "its spell said once, a restart or not"
        );
    })
    .await;
}
