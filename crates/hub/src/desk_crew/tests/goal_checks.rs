//! The hub checks goals on a schedule (DW-729): no boss turn and no watch
//! keeps them going (the night of 22 November is replayed in
//! [`super::plan_watch`]). A met reading closes the goal, its
//! ticket and its plan's watches without a word, and wakes the boss's
//! closing turn; a plan the person
//! paused is read and wakes no one; a boss message held from a goal's
//! session wakes its boss once.

use super::goals::{boss_on, ACTS};
use super::plan_watch::{asked_with, turn_ends};
use super::*;
use workforce_desk::boss_watch::{NewWatch, WatchKind};
use workforce_desk::crew::Party;
use workforce_desk::goal::{GoalResult, Measure, Per, Want};
use workforce_desk::line::{LineKind, LineMessage};
use workforce_desk::model::SessionView;
use workforce_desk::plan::{Draft, DraftTicket, Paused};
use workforce_desk::NewPlan;

/// Act, watching, with a stall window of `minutes` crew minutes (a tenth of
/// a second each).
pub(super) fn checking(minutes: u32) -> String {
    format!("{ACTS}[oversight.work]\nstall_minutes = {minutes}\n")
}

pub(super) const STALLED: &str = "No progress toward";

/// GitHub as the person's key reads it: acme/mantis's main has three
/// finished runs of CI since `from`, each job of each taking `minutes`;
/// a commit compared with a branch is `compared` to it.
pub(super) async fn mantis(
    from: chrono::DateTime<chrono::Utc>,
    minutes: i64,
    compared: &str,
) -> wiremock::MockServer {
    let gh = wiremock::MockServer::start().await;
    mantis_reads(&gh, from, minutes, compared).await;
    gh
}

/// `gh` answering as [`mantis`] says, whatever it answered before.
async fn mantis_reads(
    gh: &wiremock::MockServer,
    from: chrono::DateTime<chrono::Utc>,
    minutes: i64,
    compared: &str,
) {
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, ResponseTemplate};
    gh.reset().await;
    let stamp = |m: i64| (from + chrono::Duration::minutes(m)).to_rfc3339();
    let conclusion = if minutes <= 5 { "success" } else { "failure" };
    let runs: Vec<Value> = [9u64, 8, 7]
        .iter()
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
            ResponseTemplate::new(200)
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
        .and(path_regex(r"^/repos/acme/mantis/compare/.+$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": compared, "ahead_by": 0, "behind_by": 3, "total_commits": 0, "files": [] })))
        .mount(gh)
        .await;
}

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

/// An approved goal plan of one ticket in `folder`, measured by `r`, its
/// session started by the runner.
pub(super) async fn goal_running(
    d: &Desk,
    folder: &Path,
    r: GoalResult,
) -> (uuid::Uuid, SessionView) {
    let p = d
        .propose_plan(
            NewPlan {
                draft: Draft {
                    goal: "Mantis CI tests only what a change touches".into(),
                    tickets: vec![DraftTicket {
                        title: "Fast CI".into(),
                        folder: folder.to_string_lossy().into_owned(),
                        ..Default::default()
                    }],
                },
                board: None,
                parallel: None,
                engine: Engine::Claude,
                group: None,
                budget: None,
                result: Some(r),
                goal_ticket: None,
            },
            &Party::manager("desk", "Desk"),
            &By::manager("desk", "Desk", "Sam agreed it."),
        )
        .await
        .unwrap();
    let id = p.summary.plan.id;
    d.approve_plan(id, 1, &By::you()).await.unwrap();
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

/// How many of the boss's plan turns were woken with a stall of `part`:
/// each wake is one turn, and the boss here answers each in one go.
pub(super) async fn stalls_said(llm: &wiremock::MockServer, part: &str) -> usize {
    asked_with(llm, &[STALLED, part]).await
}

pub(super) const CI: &str = "on CI on main of acme/mantis";

#[tokio::test(flavor = "multi_thread")]
async fn a_met_reading_closes_the_goal_and_ends_its_plans_watches_without_a_word() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let from = chrono::Utc::now();
        let gh = mantis(from, 22, "ahead").await;
        let (_, _, d, llm) = boss_on(&base, pool, &checking(5), &gh).await;
        let (id, _) = goal_running(&d, &base, fast_ci(false)).await;
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
                due_at: Some(chrono::Utc::now() + chrono::Duration::hours(10)),
                expires_at: chrono::Utc::now() + chrono::Duration::hours(20),
            })
            .await
            .unwrap();
        let w = match w {
            workforce_desk::boss_watch::Opened::Started(w) => w,
            other => panic!("{other:?}"),
        };
        // CI gets fast: its next reading meets the goal.
        mantis_reads(&gh, from, 4, "ahead").await;
        let p = eventually("the goal met", async || {
            let p = d.plan(id).await.ok()?;
            p.summary.plan.met.is_some().then_some(p)
        })
        .await;
        let met = p.summary.plan.met.clone().expect("met");
        assert!(met.why.starts_with("it reads met:"), "{met:?}");
        eventually("its closing turn", async || {
            (asked_with(&llm, &["The goal is met: it reads met"]).await > 0).then_some(())
        })
        .await;
        assert!(d
            .store()
            .watches_open("desk", PERSON)
            .await
            .unwrap()
            .iter()
            .all(|o| o.id != w.id));
        assert!(
            d.store().watches_undelivered().await.unwrap().is_empty(),
            "ended silently: the closing turn says it"
        );
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
        let (id, s) = goal_running(&d, &base, fast_ci(false)).await;
        turn_ends(&d, &base, &s, "Done for now.").await;
        d.pause_plan(id, Paused::Person, &By::you()).await.unwrap();
        eventually("the paused goal read", async || {
            d.plan(id).await.ok()?.goal?.parts.first()?.newest.clone()
        })
        .await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(
            stalls_said(&llm, CI).await,
            0,
            "the person's pause wakes no one"
        );
        assert!(crew.goal_notes("desk", chrono::Utc::now()).is_empty());

        d.resume_plan(id, &[Paused::Person], &By::you())
            .await
            .unwrap();
        d.pause_plan(
            id,
            Paused::Budget,
            &By::desk("Its budget is at its warning."),
        )
        .await
        .unwrap();
        eventually("a budget pause woken", async || {
            (stalls_said(&llm, CI).await > 0).then_some(())
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
        eventually("its boss told once", async || {
            (asked_with(&llm, &["Your message to", "hasn't gone in"]).await > 0).then_some(())
        })
        .await;
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let said = llm
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .map(|r| {
                String::from_utf8_lossy(&r.body)
                    .matches("hasn't gone in")
                    .count()
            })
            .max()
            .unwrap_or_default();
        assert_eq!(said, 1);
        assert!(crew.on_goal(&s.id.to_string()));
    })
    .await;
}
