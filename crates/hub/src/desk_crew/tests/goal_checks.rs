//! The hub checks goals on a schedule (DW-729): no boss turn and no watch
//! keeps them going. The night of 22 November replayed: a goal of two
//! parts, CI per job at most 5 minutes and one judged; its session works
//! on another branch all night, the boss's checkpoints from those commits
//! are refused as off the goal, CI stays at 22 minutes, and the boss arms
//! no watch. It is woken at the stall window, at twice and at four times
//! it, the person's roundup is given one note at four times, and the hub
//! tells the person nothing directly. A met reading closes the goal, its
//! ticket and its plan's watches without a word, and wakes the boss's
//! closing turn; a plan the person
//! paused is read and wakes no one; a boss message held from a goal's
//! session wakes its boss once.

use super::goals::{boss_on, run, ACTS};
use super::plan_watch::{asked_with, turn_ends};
use super::*;
use workforce_desk::boss_watch::{NewWatch, WatchKind};
use workforce_desk::crew::Party;
use workforce_desk::goal::{GoalResult, Measure, Per, Want};
use workforce_desk::line::{LineKind, LineMessage};
use workforce_desk::model::SessionView;
use workforce_desk::plan::{Draft, DraftTicket, Paused};
use workforce_desk::NewPlan;

/// Act, watching, with a stall window of half a second.
fn checking() -> String {
    format!("{ACTS}[oversight.work]\nstall_minutes = 5\n")
}

const STALLED: &str = "No progress toward";

/// GitHub as the person's key reads it: acme/mantis's main has three
/// finished runs of CI since `from`, each job of each taking `minutes`;
/// a commit compared with a branch is `compared` to it.
async fn mantis(from: chrono::DateTime<chrono::Utc>, minutes: i64, compared: &str) -> wiremock::MockServer {
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let gh = MockServer::start().await;
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
        .mount(&gh)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/repos/acme/mantis/actions/runs/[0-9]+/jobs$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "total_count": 1, "jobs": [{
            "id": 1, "name": "test", "status": "completed", "conclusion": conclusion,
            "started_at": stamp(0), "completed_at": stamp(minutes), "steps": [] }] })))
        .mount(&gh)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/repos/acme/mantis/compare/.+$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": compared, "ahead_by": 0, "behind_by": 3, "total_commits": 0, "files": [] })))
        .mount(&gh)
        .await;
    gh
}

/// Each CI job on acme/mantis's main at most 5 minutes, three runs in a
/// row, and, when `judged`, only the services a change touches tested.
fn fast_ci(judged: bool) -> GoalResult {
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
async fn goal_running(d: &Desk, folder: &Path, r: GoalResult) -> (uuid::Uuid, SessionView) {
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
        let held = d.plan(id).await.ok()?.tickets.first()?.ticket.ticket.session()?;
        d.view(held).ok()
    })
    .await;
    (id, s)
}

/// The distinct stall lines the boss's plan turns were given so far that
/// name `part`: a turn's request carries the plan's earlier turns too.
async fn stalls_said(llm: &wiremock::MockServer, part: &str) -> usize {
    llm.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .flat_map(|r| {
            let body = String::from_utf8_lossy(&r.body).into_owned();
            body.split(STALLED)
                .skip(1)
                .filter_map(|l| l.split_once(". What Desk sees").map(|(l, _)| l.to_string()))
                .collect::<Vec<_>>()
        })
        .filter(|l| l.contains(part))
        .collect::<std::collections::HashSet<_>>()
        .len()
}

const CI: &str = "on CI on main of acme/mantis";

#[tokio::test(flavor = "multi_thread")]
async fn the_night_of_22_november() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = mantis(chrono::Utc::now(), 22, "diverged").await;
        let (overseer, crew, d, llm) = boss_on(&base, pool, &checking(), &gh).await;
        let (id, s) = goal_running(&d, &base, fast_ci(true)).await;
        // It finishes its turn on another branch: Done, all night.
        turn_ends(&d, &base, &s, "Pushed the cache work to spike-cache.").await;

        // Read with no boss turn and no watch.
        let read = eventually("the goal read on its own", async || {
            let p = d.plan(id).await.ok()?;
            let part = p.goal?.parts.first()?.newest.clone()?;
            (part.value == Some(22.0)).then_some(part)
        })
        .await;
        assert!(!read.met);
        assert!(d
            .store()
            .watches_open("desk", PERSON)
            .await
            .unwrap()
            .is_empty());

        // Its checkpoints from those commits are refused as off the goal.
        let r = run(
            &overseer,
            "crew.plan_update",
            json!({ "plan_id": id.to_string(), "why": "Showing the work",
                    "checkpoint": { "part": 2, "said": "Only api's tests ran",
                                    "evidence": [{ "kind": "commit", "repo": "acme/mantis", "sha": "d05e8a4c0ffee" }] } }),
        )
        .await;
        assert!(!r.success, "{r:?}");
        assert!(
            r.error.as_deref().unwrap_or_default().contains("isn't the goal's own work"),
            "{r:?}"
        );

        // Woken at the window, at twice it and at four times it, each part
        // on its own; the roundup is given a note at four times.
        let notes = eventually("a note for the roundup", async || {
            let n = crew.goal_notes("desk", chrono::Utc::now());
            (!n.is_empty()).then_some(n)
        })
        .await;
        assert_eq!(stalls_said(&llm, CI).await, 3, "once a spell, never again within one");
        assert_eq!(stalls_said(&llm, "on “only the services").await, 3);
        assert!(asked_with(&llm, &[CI, "22 minutes"]).await > 0);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].starts_with("No progress on “Mantis CI"), "{notes:?}");
        // No checkpoint reset the CI part, and the person was told nothing.
        let p = d.plan(id).await.unwrap();
        assert!(p.facts.iter().all(|f| !f.checked));
        assert_eq!(super::decides_at_once::said(&overseer, &[STALLED]), 0);
        assert_eq!(super::decides_at_once::said(&overseer, &["hasn't moved in"]), 0);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_met_reading_closes_the_goal_and_ends_its_plans_watches_without_a_word() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let gh = mantis(chrono::Utc::now(), 4, "ahead").await;
        let (_, _, d, llm) = boss_on(&base, pool, &checking(), &gh).await;
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
        let (_, crew, d, llm) = boss_on(&base, pool, &checking(), &gh).await;
        let (id, s) = goal_running(&d, &base, fast_ci(false)).await;
        turn_ends(&d, &base, &s, "Done for now.").await;
        d.pause_plan(id, Paused::Person, &By::you()).await.unwrap();
        eventually("the paused goal read", async || {
            d.plan(id).await.ok()?.goal?.parts.first()?.newest.clone()
        })
        .await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(stalls_said(&llm, CI).await, 0, "the person's pause wakes no one");
        assert!(crew.goal_notes("desk", chrono::Utc::now()).is_empty());

        d.resume_plan(id, &[Paused::Person], &By::you()).await.unwrap();
        d.pause_plan(id, Paused::Budget, &By::desk("Its budget is at its warning."))
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
        let (_, crew, d, llm) = boss_on(&base, pool, &checking(), &gh).await;
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
            .map(|r| String::from_utf8_lossy(&r.body).matches("hasn't gone in").count())
            .max()
            .unwrap_or_default();
        assert_eq!(said, 1);
        assert!(crew.on_goal(&s.id.to_string()));
    })
    .await;
}
