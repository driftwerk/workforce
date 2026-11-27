//! A plan finishes once nothing is left of it to do (DW-768), whatever
//! told of it or didn't: its tickets settled before the crew ran, or while
//! Desk paused it, are seen on a runner pass; its closing summary is said
//! once; a plan the person paused is never finished under them; a boss
//! that only watches has it finished by Desk, in Desk's words; and the
//! boss ends or removes one only on the person's own words asking for it,
//! or their plain reply to its question, a finished one listed on its board
//! as finished; and one whose finish was refused (a ticket reopened during
//! its closing turn) is finished on a later pass once settled again; and
//! no boss turn is spent on a plan paused at a budget.

use super::*;
use workforce_desk::board::TicketState;
use workforce_desk::plan::{Paused, PlanState};

use super::decides_at_once::said;
use super::goals::{boss_says, run};
use super::plans::{hub_with, judged, person_says, proposal, proposed, ACTS};

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

/// The boss's request for `key` (end or remove) of plan `id` on `said`.
fn ending(id: uuid::Uuid, key: &str, said: &str) -> Value {
    let mut input = json!({ "plan_id": id.to_string(), "why": "They asked." });
    input[key] = json!({ "person_said": said });
    input
}

/// Lets the next DM message land a moment later than the last.
async fn then(boss: &AgentService, says: fn(&AgentService, &str), text: &str) {
    tokio::time::sleep(Duration::from_millis(20)).await;
    says(boss, text);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_short_quote_ends_a_plan_only_as_the_persons_reply_to_the_bosss_question_about_it() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, _llm) = hub_with(dir.path(), pool, ACTS).await;
        let first = proposed(&d, dir.path(), 1).await.summary.plan.id;
        d.approve_plan(first, 1, &By::you()).await.unwrap();
        let second = proposed(&d, dir.path(), 1).await.summary.plan.id;
        d.approve_plan(second, 1, &By::you()).await.unwrap();
        let _crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("the plan tools", async || {
            overseer.tools.has("crew.plan_update").then_some(())
        })
        .await;
        let running = async |id| d.plan(id).await.unwrap().summary.plan.state == PlanState::Running;

        then(&overseer, person_says, "ok").await;
        let r = run(&overseer, "crew.plan_update", ending(first, "end", "ok")).await;
        assert!(!r.success, "a bare ok asks for nothing");
        assert!(r.error.unwrap().contains("doesn't ask for the plan to end"));
        then(&overseer, person_says, "the tests are green on main").await;
        let r = run(
            &overseer,
            "crew.plan_update",
            ending(first, "end", "the tests"),
        )
        .await;
        assert!(!r.success, "words that don't ask for it");
        assert!(running(first).await);

        then(
            &overseer,
            boss_says,
            "Every ticket is done. Shall I end the suite plan?",
        )
        .await;
        then(&overseer, person_says, "yes").await;
        let r = run(&overseer, "crew.plan_update", ending(first, "end", "yes")).await;
        assert!(r.success, "{r:?}");
        assert_eq!(
            d.plan(first).await.unwrap().summary.plan.state,
            PlanState::Dropped
        );
        let r = run(
            &overseer,
            "crew.plan_update",
            ending(second, "end", "end the"),
        )
        .await;
        assert!(!r.success, "too little of their words");
        assert!(running(second).await);

        then(&overseer, person_says, "end the Mantis bugs plan").await;
        let r = run(
            &overseer,
            "crew.plan_update",
            ending(second, "end", "end the Mantis bugs plan"),
        )
        .await;
        assert!(r.success, "{r:?}");
        assert_eq!(
            d.plan(second).await.unwrap().summary.plan.state,
            PlanState::Dropped
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plan_whose_ticket_reopened_during_its_closing_turn_finishes_once_settled_again() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, state, d, _llm) = hub_with(dir.path(), pool, ACTS).await;
        let id = settled(&d, dir.path(), 2).await;
        let ticket = d.plan(id).await.unwrap().tickets[0].ticket.ticket.id;
        // Moved behind the crew's back, as a lagged event stream misses it.
        let set = async |state| {
            let mut t = d.store().ticket(ticket).await.unwrap().unwrap();
            t.state = state;
            d.store().put_ticket(&mut t).await.unwrap();
        };
        set(TicketState::Doing).await;
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        // Its closing turn ran while it was settled, and ends now it isn't.
        assert!(crew.closing_started(id));
        let p = d.plan(id).await.unwrap().summary.plan;
        crew.close_plan(&p, Some("All done."), &By::desk("Every ticket is settled."))
            .await;
        assert_eq!(
            p.state,
            d.plan(id).await.unwrap().summary.plan.state,
            "refused"
        );
        assert!(
            !crew.goal_checks.lock().closing.contains(&id),
            "its closing is over"
        );

        set(TicketState::Done).await;
        crew.run_plans().await;
        done(&d, id).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plan_paused_at_a_budget_gets_no_bosss_turn_desk_finishes_it_or_it_waits_for_the_lift() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, llm) = hub_with(dir.path(), pool, ACTS).await;
        let no_goal = settled(&d, dir.path(), 2).await;
        let mut new = proposal(dir.path(), 2);
        new.result = Some(serde_json::from_value(judged()).unwrap());
        let party = workforce_desk::crew::Party::manager("desk", "Desk");
        let unmet = d
            .propose_plan(new, &party, &By::you())
            .await
            .unwrap()
            .summary
            .plan
            .id;
        let p = d.approve_plan(unmet, 1, &By::you()).await.unwrap();
        for t in &p.tickets {
            let mv = workforce_desk::Move {
                state: TicketState::Done,
                before: None,
                after: None,
            };
            d.move_ticket(t.ticket.ticket.id, mv, &By::you())
                .await
                .unwrap();
        }
        for id in [no_goal, unmet] {
            d.pause_plan(id, Paused::Budget, &By::desk("Its budget is spent."))
                .await
                .unwrap();
        }
        // No runner: nothing lifts the pauses under the test.
        let stopped = CancellationToken::new();
        stopped.cancel();
        let crew = CrewHub::start(state, d.clone(), FAST, stopped);

        crew.plan_turn_now(no_goal, vec!["Every ticket is done or dropped.".into()])
            .await;
        assert_eq!(
            d.plan(no_goal).await.unwrap().summary.plan.state,
            PlanState::Done
        );
        let words = "“Make the suite green” is done: 1 of 1 tickets done.";
        assert_eq!(said(&overseer, &[words]), 1, "in Desk's words");

        crew.plan_turn_now(unmet, vec!["Every ticket is done or dropped.".into()])
            .await;
        let p = d.plan(unmet).await.unwrap().summary.plan;
        assert_eq!(p.state, PlanState::Paused, "waits for its pause to lift");
        assert!(!crew.goal_checks.lock().closing.contains(&unmet));
        let unmet_turns = llm
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| String::from_utf8_lossy(&r.body).contains("Not read met:"))
            .count();
        assert_eq!(
            (closings(&llm).await, unmet_turns),
            (0, 0),
            "no boss turn past a budget"
        );
    })
    .await;
}
