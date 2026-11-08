//! Plans end to end on a hub: a boss proposes one with `crew.plan`, the
//! person approves it, and the runner starts its ready tickets' sessions up
//! to the boss's limit, the next as a slot frees; a start cut off by a stop
//! starts once after it; a session that ended while the hub was down wakes
//! the boss once; at `suggest` each start is a suggestion; at `watch` a boss
//! plans nothing; and `hand_out_work = false` doesn't hold back what the
//! person approved.

use super::*;
use workforce_desk::board::TicketState;
use workforce_desk::charge::How;
use workforce_desk::crew::Party;
use workforce_desk::plan::{Draft, DraftTicket, PlanState};
use workforce_desk::NewPlan;

use super::super::actions::{Action, Gate};

/// An overseer `desk` at `oversight`, on a hub whose Claude Code is
/// installed, with its DM open; the crew not started yet.
async fn hub_with(
    dir: &Path,
    pool: workforce_postgres::PostgresPool,
    oversight: &str,
) -> (
    Arc<AgentService>,
    Arc<GatewayState>,
    Desk,
    wiremock::MockServer,
) {
    let (overseer, llm) = agent(dir, "desk", "ok").await;
    write_config(&overseer, oversight);
    let state = hub(std::slice::from_ref(&overseer));
    let d = desk_running(
        pool,
        dir,
        Default::default(),
        installed_cli(dir),
        not_installed(dir),
    )
    .await;
    let dm = overseer
        .sessions
        .get_or_create(DESK_CHANNEL, PERSON, "desk")
        .unwrap();
    overseer.sessions.save(&dm).unwrap();
    eventually("Desk to read its engines", async || {
        let e = d.engines();
        e.iter()
            .any(|v| v.engine == Engine::Claude && v.installed == Some(true))
            .then_some(())
    })
    .await;
    (overseer, state, d, llm)
}

const ACTS: &str = "[oversight]\nenabled = true\nlevel = \"act\"\n";

fn tickets(dir: &Path, n: usize) -> Vec<Value> {
    (1..=n)
        .map(|i| json!({ "title": format!("Fix flaky test {i}"), "folder": dir.to_string_lossy() }))
        .collect()
}

/// A plan of `n` independent tickets proposed by `desk`, without its
/// tools: for a hub whose crew isn't running yet.
async fn proposed(d: &Desk, dir: &Path, n: usize) -> workforce_desk::PlanDetail {
    d.propose_plan(
        NewPlan {
            draft: Draft {
                goal: "Make the suite green".into(),
                tickets: (1..=n)
                    .map(|i| DraftTicket {
                        title: format!("Fix flaky test {i}"),
                        folder: dir.to_string_lossy().into_owned(),
                        ..Default::default()
                    })
                    .collect(),
            },
            board: None,
            parallel: None,
            engine: Engine::Claude,
            group: None,
        },
        &Party::manager("desk", "Desk"),
        &By::manager("desk", "Desk", "Sam asked for a green suite."),
    )
    .await
    .unwrap()
}

/// The sessions that are running for the plan's tickets.
fn started(d: &Desk) -> Vec<workforce_desk::model::SessionView> {
    d.list()
        .into_iter()
        .filter(|v| v.name.starts_with("DES-"))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approved_plan_starts_four_sessions_and_the_fifth_when_one_finishes() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        // `hand_out_work` is off, as it is by default.
        let (overseer, state, d, _llm) = hub_with(dir.path(), pool, ACTS).await;
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("the plan tools", async || {
            overseer.tools.has("crew.plan").then_some(())
        })
        .await;
        let r = overseer
            .tools
            .get("crew.plan")
            .unwrap()
            .execute(json!({ "goal": "Make the suite green", "tickets": tickets(dir.path(), 5) }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        let id: uuid::Uuid = r.output["proposed"]["plan_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(r.output["proposed"]["state"], "proposed");
        // Nothing runs before the person approves it, and they are asked.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(started(&d).is_empty());
        assert_eq!(d.plan_needs().len(), 1);

        d.approve_plan(id, 1, &By::you()).await.unwrap();
        let four = eventually("four sessions", async || {
            let s = started(&d);
            (s.len() == 4).then_some(s)
        })
        .await;
        for v in &four {
            let c = d.charge_of(v.id).unwrap();
            assert_eq!((c.boss.as_str(), c.how), ("desk", How::Started));
        }
        assert_eq!(d.charges_busy("desk"), 4);
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(started(&d).len(), 4, "the limit holds");
        let p = d.plan(id).await.unwrap();
        assert_eq!(p.summary.progress.doing, 4);
        assert_eq!(p.summary.progress.next.len(), 1);
        for t in p
            .tickets
            .iter()
            .filter(|t| t.ticket.ticket.state == TicketState::Doing)
        {
            assert!(
                t.ticket.ticket.session().is_some(),
                "each is given its session"
            );
        }

        // The approval is the authority for its tickets: the boss's own
        // move of one runs, though it may not hand out other work.
        let (_, o) = crew.overseer_service().unwrap();
        assert!(!o.act.hand_out_work);
        let key = p.tickets[0].ticket.key.clone().unwrap();
        let mut mv = Action::parse(
            "crew.move_ticket",
            &json!({ "ticket": key, "state": "review", "reason": "done" }),
        )
        .unwrap();
        crew.resolve_ticket(&mut mv, None, Some("desk"))
            .await
            .unwrap();
        assert_eq!(crew.gate(&mv, &o), Gate::Run);

        // One finishes its turn: the fifth starts.
        let one = four[0].id;
        let url = hook_url(dir.path(), one).await;
        hook(
            &url,
            json!({"hook_event_name": "SessionStart", "session_id": one.to_string()}),
        )
        .await;
        eventually("the fifth session", async || {
            (started(&d).len() == 5).then_some(())
        })
        .await;
        assert!(d.plan(id).await.unwrap().summary.progress.next.is_empty());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_start_cut_off_between_its_link_and_its_session_starts_exactly_once_after_a_restart() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, state, d, _llm) = hub_with(dir.path(), pool, ACTS).await;
        let p = proposed(&d, dir.path(), 2).await;
        let id = p.summary.plan.id;
        d.approve_plan(id, 1, &By::you()).await.unwrap();
        // The runner wrote the first ticket's link and stopped there.
        let first = p.tickets[0].ticket.ticket.id;
        let mut t = d.store().ticket(first).await.unwrap().unwrap();
        let session = uuid::Uuid::new_v4();
        d.store()
            .begin_start(&mut t, id, 50_000.0, session, workforce_desk::crew::now())
            .await
            .unwrap();
        assert!(d.view(session).is_err());

        // The hub comes back.
        let _crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        let two = eventually("both tickets started", async || {
            let s = started(&d);
            (s.len() == 2).then_some(s)
        })
        .await;
        assert!(
            two.iter().any(|v| v.id == session),
            "the same session it named"
        );
        let ticket = d.ticket(first).await.unwrap();
        assert_eq!(ticket.ticket.session(), Some(session));
        eventually("its link finished", async || {
            d.store().starting().await.unwrap().is_empty().then_some(())
        })
        .await;
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(started(&d).len(), 2, "never started twice");
        let named = d
            .list()
            .into_iter()
            .filter(|v| Some(v.name.as_str()) == ticket.key.as_deref())
            .count();
        assert_eq!(named, 1);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_that_ended_while_the_hub_was_down_wakes_the_boss_once() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, state, d, llm) = hub_with(dir.path(), pool, ACTS).await;
        let p = proposed(&d, dir.path(), 1).await;
        let id = p.summary.plan.id;
        let p = d.approve_plan(id, 1, &By::you()).await.unwrap();
        let start = workforce_desk::plan::next_starts(
            &p.summary.plan,
            &d.plan_tickets(id).await.unwrap(),
            0,
            4,
            None,
        )
        .remove(0);
        let boss = Party::manager("desk", "Desk");
        let by = By::manager("desk", "Desk", "Approved.");
        let s = d
            .plan_start(&p.summary.plan, &start, &boss, &by)
            .await
            .unwrap();
        // It ended while no hub watched.
        d.stop(s.id, None).unwrap();
        eventually("it to stop", async || {
            d.view(s.id)
                .ok()
                .filter(|v| v.state != workforce_desk::model::SessionState::Starting)
                .map(drop)
        })
        .await;

        let _crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        let woke = || async {
            llm.received_requests()
                .await
                .unwrap_or_default()
                .iter()
                .filter(|r| {
                    let body = String::from_utf8_lossy(&r.body);
                    body.contains("Desk restarted at") && body.contains("DES-1")
                })
                .count()
        };
        eventually("the boss woken", async || (woke().await >= 1).then_some(())).await;
        tokio::time::sleep(Duration::from_millis(4_500)).await;
        assert_eq!(woke().await, 1, "once");
        assert_eq!(
            d.plan(id).await.unwrap().summary.plan.state,
            PlanState::Running
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn at_suggest_each_start_of_an_approved_plan_is_a_suggestion() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, state, d, _llm) = hub_with(
            dir.path(),
            pool,
            "[oversight]\nenabled = true\nlevel = \"suggest\"\n[oversight.work]\nparallel = 2\n",
        )
        .await;
        let p = proposed(&d, dir.path(), 3).await;
        let id = p.summary.plan.id;
        d.approve_plan(id, 1, &By::you()).await.unwrap();
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        let open = || async {
            d.store()
                .suggestions(50)
                .await
                .unwrap()
                .into_iter()
                .filter(|s| s.state == workforce_desk::crew::SuggestionState::Open)
                .collect::<Vec<_>>()
        };
        let two = eventually("two starts suggested", async || {
            let o = open().await;
            (o.len() == 2).then_some(o)
        })
        .await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(open().await.len(), 2, "each once, within the limit");
        assert!(started(&d).is_empty(), "nothing starts by itself");
        assert!(
            two[0].text.starts_with("Start a session in"),
            "{}",
            two[0].text
        );
        // The person says yes: it starts, the boss's to run.
        crew.accept(&two[0]).await.unwrap();
        let one = eventually("the accepted start", async || {
            let s = d.list();
            (s.len() == 1).then(|| s[0].clone())
        })
        .await;
        assert_eq!(d.charge_of(one.id).map(|c| c.boss), Some("desk".into()));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn at_watch_crew_plan_is_refused() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, _llm) = hub_with(dir.path(), pool, ACTS).await;
        let _crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("the plan tools", async || {
            overseer.tools.has("crew.plan").then_some(())
        })
        .await;
        let tool = overseer.tools.get("crew.plan").unwrap();
        write_config(
            &overseer,
            "[oversight]\nenabled = true\nlevel = \"watch\"\n",
        );
        super::super::config_saved(&overseer);
        let r = tool
            .execute(json!({ "goal": "Make the suite green", "tickets": tickets(dir.path(), 1) }))
            .await
            .unwrap();
        assert!(!r.success);
        assert_eq!(
            r.error.as_deref(),
            Some("Desk only watches; set them to Act to run work.")
        );
        assert!(d.plans(None, None).await.unwrap().is_empty());
        eventually("the acting tools gone", async || {
            (!overseer.tools.has("crew.plan")).then_some(())
        })
        .await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_boss_keeps_its_plan_current_and_a_change_of_scope_waits_for_the_person() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        // Paused right away, so nothing starts while it is changed.
        let (overseer, state, d, _llm) = hub_with(dir.path(), pool, ACTS).await;
        let _crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("the plan tools", async || overseer.tools.has("crew.plan").then_some(())).await;
        let made = overseer
            .tools
            .get("crew.plan")
            .unwrap()
            .execute(json!({ "goal": "Make the suite green", "tickets": tickets(dir.path(), 2), "parallel": 1 }))
            .await
            .unwrap();
        let plan = &made.output["proposed"];
        let id: uuid::Uuid = plan["plan_id"].as_str().unwrap().parse().unwrap();
        let keys: Vec<String> = plan["tickets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["key"].as_str().unwrap().to_string())
            .collect();
        d.approve_plan(id, 1, &By::you()).await.unwrap();
        d.pause_plan(id, workforce_desk::plan::Paused::Person, &By::you())
            .await
            .unwrap();
        let update = overseer.tools.get("crew.plan_update").unwrap();
        let folder = dir.path().to_string_lossy();
        // By key, reordered: it applies.
        let r = update
            .execute(json!({ "plan_id": id.to_string(), "why": "The second unblocks the first.",
                "tickets": [
                    { "key": keys[1], "title": "Fix flaky test 2", "folder": folder },
                    { "key": keys[0], "title": "Fix flaky test 1", "folder": folder, "needs": [0] },
                ] }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        assert_eq!(r.output["change"], "minor");
        assert_eq!(r.output["applied"], true);
        // A ticket added waits for the person.
        let r = update
            .execute(json!({ "plan_id": id.to_string(), "why": "The docs are stale too.",
                "tickets": [
                    { "key": keys[1], "title": "Fix flaky test 2", "folder": folder },
                    { "key": keys[0], "title": "Fix flaky test 1", "folder": folder, "needs": [0] },
                    { "title": "Update the docs", "folder": folder },
                ] }))
            .await
            .unwrap();
        assert_eq!(r.output["change"], "scope");
        assert_eq!(r.output["applied"], false);
        let needs = d.plan_needs();
        assert_eq!(needs.len(), 1);
        assert!(needs[0].ask.contains("Update the docs"), "{}", needs[0].ask);
        // A plan of another boss, or none, is no plan of its.
        let r = update
            .execute(json!({ "plan_id": uuid::Uuid::new_v4().to_string(), "why": "x", "goal": "y" }))
            .await
            .unwrap();
        assert!(!r.success);
        // Approved from the Floor's decision: the new ticket is on the board.
        d.approve_plan(id, 3, &By::you()).await.unwrap();
        assert_eq!(d.plan(id).await.unwrap().tickets.len(), 3);
        assert!(d.plan_needs().is_empty());
    })
    .await;
}

/// How many of the boss's model requests say `marker`.
async fn said(llm: &wiremock::MockServer, marker: &str) -> usize {
    llm.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| String::from_utf8_lossy(&r.body).contains(marker))
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_approval_counts_only_for_a_running_plan_of_the_bosss_own_and_where_it_says_to_run() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        // One at a time, so the second ticket waits in To do.
        let (_overseer, state, d, _llm) = hub_with(
            dir.path(),
            pool,
            "[oversight]\nenabled = true\nlevel = \"act\"\n[oversight.work]\nparallel = 1\n",
        )
        .await;
        let p = proposed(&d, dir.path(), 2).await;
        let id = p.summary.plan.id;
        d.approve_plan(id, 1, &By::you()).await.unwrap();
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        let doing = eventually("the first started", async || {
            d.plan(id).await.unwrap().tickets.into_iter().find(|t| t.ticket.ticket.state == TicketState::Doing)
        })
        .await;
        let waiting = d
            .plan(id)
            .await
            .unwrap()
            .tickets
            .into_iter()
            .find(|t| t.ticket.ticket.state == TicketState::Todo)
            .unwrap();
        let (_, o) = crew.overseer_service().unwrap();
        assert!(!o.act.hand_out_work);
        let key = |t: &workforce_desk::PlanTicketView| t.ticket.key.clone().unwrap();
        let gate_of = |input: Value, tool: &'static str, boss: Option<&'static str>| {
            let crew = Arc::clone(&crew);
            let o = o.clone();
            async move {
                let mut a = Action::parse(tool, &input)?;
                crew.resolve_ticket(&mut a, None, boss).await?;
                Ok::<Gate, String>(crew.gate(&a, &o))
            }
        };
        let review = json!({ "ticket": key(&doing), "state": "review", "reason": "done" });
        assert_eq!(gate_of(review.clone(), "crew.move_ticket", Some("desk")).await, Ok(Gate::Run));
        // Another boss's plan is no authority of this one's.
        assert!(matches!(
            gate_of(review.clone(), "crew.move_ticket", Some("sarah")).await,
            Ok(Gate::Suggest(_))
        ));
        // A ticket that has run goes back in the queue only by the person.
        let back = json!({ "ticket": key(&doing), "state": "todo", "reason": "redo" });
        assert!(matches!(
            gate_of(back, "crew.move_ticket", Some("desk")).await,
            Ok(Gate::Suggest(_))
        ));
        // A new session for it runs where the plan says, and nowhere else.
        let other = tempfile::tempdir().unwrap();
        let elsewhere = json!({ "ticket": key(&waiting), "to": { "newSession": { "folder": other.path().to_string_lossy() } }, "reason": "x" });
        let refused = gate_of(elsewhere, "crew.assign_ticket", Some("desk")).await.unwrap_err();
        assert!(refused.contains("as its plan says"), "{refused}");
        let there = json!({ "ticket": key(&waiting), "to": { "newSession": { "folder": dir.path().to_string_lossy() } }, "reason": "x" });
        assert_eq!(gate_of(there.clone(), "crew.assign_ticket", Some("desk")).await, Ok(Gate::Run));
        // To an agent or a session it stays under `hand_out_work`.
        let to_agent = json!({ "ticket": key(&waiting), "to": { "agent": "desk" }, "reason": "x" });
        assert!(matches!(
            gate_of(to_agent, "crew.assign_ticket", Some("desk")).await,
            Ok(Gate::Suggest(_))
        ));
        // Paused, the approval doesn't count.
        d.pause_plan(id, workforce_desk::plan::Paused::Person, &By::you())
            .await
            .unwrap();
        assert!(matches!(
            gate_of(there, "crew.assign_ticket", Some("desk")).await,
            Ok(Gate::Suggest(_))
        ));
        assert!(matches!(
            gate_of(review, "crew.move_ticket", Some("desk")).await,
            Ok(Gate::Suggest(_))
        ));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ticket_that_keeps_failing_to_start_tells_the_boss_once_and_pauses_its_plan_saying_why() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, state, d, llm) = hub_with(dir.path(), pool, ACTS).await;
        let folder = tempfile::tempdir().unwrap();
        let p = proposed(&d, folder.path(), 1).await;
        let id = p.summary.plan.id;
        d.approve_plan(id, 1, &By::you()).await.unwrap();
        // Its folder goes before it starts.
        drop(folder);
        let _crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        let paused = eventually("the plan paused", async || {
            let p = d.plan(id).await.unwrap();
            (p.summary.plan.paused == Some(workforce_desk::plan::Paused::StartFailing)).then_some(p)
        })
        .await;
        let why = paused.summary.paused_why.unwrap();
        assert!(
            why.starts_with("DES-1 couldn't start 5 times:") && why.contains("not a folder"),
            "{why}"
        );
        assert!(d
            .plan_needs()
            .iter()
            .any(|n| n.ask.contains("couldn't start 5 times")));
        eventually("the boss told", async || {
            (said(&llm, "DES-1 didn't start").await >= 1).then_some(())
        })
        .await;
        tokio::time::sleep(Duration::from_millis(3_500)).await;
        assert_eq!(
            said(&llm, "DES-1 didn't start").await,
            1,
            "told once for the one reason"
        );
        assert!(started(&d).is_empty());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_start_the_person_dismissed_isnt_suggested_again_until_the_plan_changes() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, state, d, _llm) = hub_with(
            dir.path(),
            pool,
            "[oversight]\nenabled = true\nlevel = \"suggest\"\n",
        )
        .await;
        let p = proposed(&d, dir.path(), 1).await;
        let id = p.summary.plan.id;
        d.approve_plan(id, 1, &By::you()).await.unwrap();
        let _crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        let mut s = eventually("the start suggested", async || {
            d.store()
                .suggestions_for("desk", "crew.assign_ticket")
                .await
                .unwrap()
                .pop()
        })
        .await;
        assert_eq!(s.action["input"]["plan_revision"], 1);
        s.state = workforce_desk::crew::SuggestionState::Dismissed;
        d.store().put_suggestion(&s).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        assert_eq!(
            d.store()
                .suggestions_for("desk", "crew.assign_ticket")
                .await
                .unwrap()
                .len(),
            1,
            "not again"
        );
        // The person changes the plan: it is suggested afresh.
        d.change_plan(
            id,
            workforce_desk::PlanEdit {
                revision: Some(1),
                goal: Some("Make the suite green today".into()),
                ..Default::default()
            },
            Engine::Claude,
            false,
            &By::you(),
        )
        .await
        .unwrap();
        eventually("suggested afresh", async || {
            (d.store()
                .suggestions_for("desk", "crew.assign_ticket")
                .await
                .unwrap()
                .len()
                == 2)
                .then_some(())
        })
        .await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_boss_is_told_in_plain_words_when_the_person_turns_its_change_down() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, state, d, llm) = hub_with(
            dir.path(),
            pool,
            "[oversight]\nenabled = true\nlevel = \"act\"\n[oversight.work]\nparallel = 1\n",
        )
        .await;
        let p = proposed(&d, dir.path(), 1).await;
        let id = p.summary.plan.id;
        let p = d.approve_plan(id, 1, &By::you()).await.unwrap();
        let _crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        let mut tickets: Vec<DraftTicket> = p
            .tickets
            .iter()
            .map(|t| DraftTicket {
                id: Some(t.ticket.ticket.id),
                title: t.ticket.ticket.title.clone(),
                folder: dir.path().to_string_lossy().into_owned(),
                ..Default::default()
            })
            .collect();
        tickets.push(DraftTicket {
            title: "Update the docs".into(),
            folder: dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        });
        d.change_plan(
            id,
            workforce_desk::PlanEdit {
                revision: Some(1),
                tickets: Some(tickets),
                ..Default::default()
            },
            Engine::Claude,
            false,
            &By::manager("desk", "Desk", "The docs are stale."),
        )
        .await
        .unwrap();
        d.reject_plan(id, 2, &By::you()).await.unwrap();
        eventually("the boss told", async || {
            (said(&llm, "The person turned down the change to the plan").await >= 1).then_some(())
        })
        .await;
        assert!(d.plan_needs().is_empty());
        assert_eq!(d.plan(id).await.unwrap().tickets.len(), 1);
    })
    .await;
}
