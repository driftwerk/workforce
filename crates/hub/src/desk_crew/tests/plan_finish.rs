//! A plan finishes once nothing is left of it to do (DW-768), whatever
//! told of it or didn't: its tickets settled before the crew ran, or while
//! Desk paused it, are seen on a runner pass; its closing summary is said
//! once; a plan the person paused is never finished under them; a boss
//! that only watches has it finished by Desk, in Desk's words; and the
//! boss ends or removes one only on the person's own words, a finished one
//! listed on its board as finished.

use super::*;
use workforce_desk::board::TicketState;
use workforce_desk::plan::{Paused, PlanState};

use super::decides_at_once::said;
use super::goals::run;
use super::plans::{hub_with, person_says, proposed, ACTS};

const WATCHES: &str = "[oversight]\nenabled = true\nlevel = \"watch\"\n";

const CLOSING: &str = "Write the person a short closing summary";

/// A running plan of `n` tickets whose every ticket is settled: the last
/// dropped, the rest done.
async fn settled(d: &Desk, dir: &Path, n: usize) -> uuid::Uuid {
    let p = proposed(d, dir, n).await;
    let id = p.summary.plan.id;
    let p = d.approve_plan(id, 1, &By::you()).await.unwrap();
    for (i, t) in p.tickets.iter().enumerate() {
        let state = match i + 1 == n {
            true => TicketState::Dropped,
            false => TicketState::Done,
        };
        let mv = workforce_desk::Move {
            state,
            before: None,
            after: None,
        };
        d.move_ticket(t.ticket.ticket.id, mv, &By::you())
            .await
            .unwrap();
    }
    id
}

/// How many of the boss's turns were asked for its closing summary.
async fn closings(llm: &wiremock::MockServer) -> usize {
    llm.received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| String::from_utf8_lossy(&r.body).contains(CLOSING))
        .count()
}

async fn done(d: &Desk, id: uuid::Uuid) {
    eventually("the plan finished", async || {
        (d.plan(id).await.ok()?.summary.plan.state == PlanState::Done).then_some(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plan_settled_before_the_crew_ran_finishes_on_a_pass_and_its_summary_is_asked_once() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, state, d, llm) = hub_with(dir.path(), pool, ACTS).await;
        let id = settled(&d, dir.path(), 3).await;
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        done(&d, id).await;
        for _ in 0..3 {
            crew.run_plans().await;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(closings(&llm).await, 1, "asked once");
        let p = d.plan(id).await.unwrap().summary;
        assert_eq!((p.progress.done, p.progress.total), (2, 2));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plan_the_person_paused_isnt_finished_under_them_but_one_desk_paused_is() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, state, d, llm) = hub_with(dir.path(), pool, ACTS).await;
        let theirs = proposed(&d, dir.path(), 1).await.summary.plan.id;
        let p = d.approve_plan(theirs, 1, &By::you()).await.unwrap();
        d.pause_plan(theirs, Paused::Person, &By::you())
            .await
            .unwrap();
        let mv = workforce_desk::Move {
            state: TicketState::Done,
            before: None,
            after: None,
        };
        d.move_ticket(p.tickets[0].ticket.ticket.id, mv, &By::you())
            .await
            .unwrap();
        let desks = settled(&d, dir.path(), 2).await;
        d.pause_plan(
            desks,
            Paused::Budget,
            &By::desk("Its budget is nearly spent."),
        )
        .await
        .unwrap();

        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        done(&d, desks).await;
        for _ in 0..3 {
            crew.run_plans().await;
        }
        assert_eq!(
            d.plan(theirs).await.unwrap().summary.plan.state,
            PlanState::Paused
        );
        assert_eq!(closings(&llm).await, 1, "only Desk's pause is finished");

        // Resumed by the person, it finishes.
        d.resume_plan(theirs, &[Paused::Person], &By::you())
            .await
            .unwrap();
        done(&d, theirs).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_watching_bosss_plan_is_finished_by_desk_in_its_own_words_once() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, llm) = hub_with(dir.path(), pool, WATCHES).await;
        let id = settled(&d, dir.path(), 2).await;
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        done(&d, id).await;
        crew.run_plans().await;
        let words = "“Make the suite green” is done: 1 of 1 tickets done.";
        eventually("the person told", async || {
            (said(&overseer, &[words]) == 1).then_some(())
        })
        .await;
        crew.run_plans().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(said(&overseer, &[words]), 1, "once");
        assert_eq!(
            closings(&llm).await,
            0,
            "a boss that only watches takes no turn"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_boss_ends_and_removes_a_plan_only_on_the_persons_own_words() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, _llm) = hub_with(dir.path(), pool, ACTS).await;
        let p = proposed(&d, dir.path(), 2).await;
        let id = p.summary.plan.id;
        d.approve_plan(id, 1, &By::you()).await.unwrap();
        let _crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("the plan tools", async || {
            overseer.tools.has("crew.plan_update").then_some(())
        })
        .await;
        let ask = |key: &str, said: Option<&str>| {
            let mut input = json!({ "plan_id": id.to_string(), "why": "They asked." });
            input[key] = json!({ "person_said": said });
            input
        };

        let r = run(&overseer, "crew.plan_update", ask("end", None)).await;
        assert!(!r.success);
        assert!(r.error.unwrap().contains("Quote the person's own words"));
        let r = run(
            &overseer,
            "crew.plan_update",
            ask("end", Some("end the plan")),
        )
        .await;
        assert!(!r.success, "words they never said");
        let mut both = ask("end", Some("x"));
        both["goal"] = json!("Something else");
        let r = run(&overseer, "crew.plan_update", both).await;
        assert!(r.error.unwrap().contains("a call of its own"));
        assert_eq!(
            d.plan(id).await.unwrap().summary.plan.state,
            PlanState::Running
        );

        person_says(&overseer, "Please end the suite plan, we're done with it");
        let r = run(
            &overseer,
            "crew.plan_update",
            ask("end", Some("Please end the suite plan, we're done with it")),
        )
        .await;
        assert!(r.success, "{r:?}");
        assert!(r.output["done"].as_str().unwrap().starts_with("Ended"));
        let ended = d.plan(id).await.unwrap();
        assert_eq!(ended.summary.plan.state, PlanState::Dropped);
        assert!(
            ended
                .tickets
                .iter()
                .all(|t| t.ticket.ticket.state != TicketState::Dropped),
            "its tickets stay as they are"
        );

        let board = run(&overseer, "crew.board", json!({})).await;
        assert!(board.success, "{board:?}");
        let finished = board.output["finished_plans"].as_array().unwrap();
        assert_eq!(finished.len(), 1, "{board:?}");
        assert_eq!(finished[0]["plan_id"], id.to_string());

        let r = run(
            &overseer,
            "crew.plan_update",
            ask("end", Some("Please end the suite plan, we're done with it")),
        )
        .await;
        assert!(r.error.unwrap().contains("remove takes it off the board"));
        person_says(&overseer, "Now remove the suite plan from the board");
        let r = run(
            &overseer,
            "crew.plan_update",
            ask("remove", Some("Now remove the suite plan from the board")),
        )
        .await;
        assert!(r.success, "{r:?}");
        assert!(d.plan(id).await.is_err(), "removed");
        let tickets: Vec<_> = p.tickets.iter().map(|t| t.ticket.ticket.id).collect();
        for t in tickets {
            assert!(d.ticket(t).await.is_ok(), "its tickets are kept");
        }
        assert!(d.plan_tickets(id).await.unwrap().is_empty(), "as no plan's");
    })
    .await;
}
