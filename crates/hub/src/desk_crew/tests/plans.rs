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
use workforce_desk::model::SessionState;
use workforce_desk::plan::{Draft, DraftTicket, PlanState};
use workforce_desk::NewPlan;

use super::super::actions::{Action, Gate};

/// An overseer `desk` at `oversight`, on a hub whose Claude Code is
/// installed, with its DM open; the crew not started yet.
pub(super) async fn hub_with(
    dir: &Path,
    pool: workforce_postgres::PostgresPool,
    oversight: &str,
) -> (
    Arc<AgentService>,
    Arc<GatewayState>,
    Desk,
    wiremock::MockServer,
) {
    hub_running(dir, pool, oversight, installed_cli(dir)).await
}

/// [`hub_with`], its Claude Code `claude`.
async fn hub_running(
    dir: &Path,
    pool: workforce_postgres::PostgresPool,
    oversight: &str,
    claude: PathBuf,
) -> (
    Arc<AgentService>,
    Arc<GatewayState>,
    Desk,
    wiremock::MockServer,
) {
    let d = desk_running(pool, dir, Default::default(), claude, not_installed(dir)).await;
    hub_on(dir, oversight, d).await
}

/// [`hub_with`] over the desk `d`.
pub(super) async fn hub_on(
    dir: &Path,
    oversight: &str,
    d: Desk,
) -> (
    Arc<AgentService>,
    Arc<GatewayState>,
    Desk,
    wiremock::MockServer,
) {
    let (overseer, llm) = agent(dir, "desk", "ok").await;
    write_config(&overseer, oversight);
    let state = hub(std::slice::from_ref(&overseer));
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

/// Act, with `hand_out_work` off: what a plan the person approved starts
/// runs whatever it says.
pub(super) const ACTS: &str =
    "[oversight]\nenabled = true\nlevel = \"act\"\n[oversight.act]\nhand_out_work = false\n";

/// A result nothing measures: what a plan that is no goal's says.
pub(super) fn judged() -> Value {
    json!({ "words": "the suite is green", "parts": [{ "kind": "judged", "evidence": "the sessions' test runs" }] })
}

/// The person writes `text` in `boss`'s DM.
pub(super) fn person_says(boss: &AgentService, text: &str) {
    let mut dm = boss
        .sessions
        .get_or_create(DESK_CHANNEL, PERSON, "desk")
        .unwrap();
    dm.add_user_message(text, None);
    boss.sessions.save(&dm).unwrap();
    boss.sessions
        .append_to_archive(&dm.id, dm.messages.last().unwrap())
        .unwrap();
}

fn tickets(dir: &Path, n: usize) -> Vec<Value> {
    (1..=n)
        .map(|i| json!({ "title": format!("Fix flaky test {i}"), "folder": dir.to_string_lossy() }))
        .collect()
}

/// A plan of `n` independent tickets proposed by `desk`, without its
/// tools: for a hub whose crew isn't running yet.
pub(super) async fn proposed(d: &Desk, dir: &Path, n: usize) -> workforce_desk::PlanDetail {
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
            budget: None,
            result: None,
            goal_ticket: None,
        },
        &Party::manager("desk", "Desk"),
        &By::manager("desk", "Desk", "Sam asked for a green suite."),
    )
    .await
    .unwrap()
}

/// The sessions that are running for the plan's tickets.
pub(super) fn started(d: &Desk) -> Vec<workforce_desk::model::SessionView> {
    d.list()
        .into_iter()
        .filter(|v| v.name.starts_with("DES-"))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approved_plan_starts_four_sessions_and_the_fifth_when_one_finishes() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        // `hand_out_work` is off.
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
            .execute(json!({ "result": judged(), "goal": "Make the suite green", "tickets": tickets(dir.path(), 5) }))
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

        // One finishes its turn: it still holds its slot, so the fifth
        // waits; closed, it doesn't, and the fifth starts.
        let one = four[0].id;
        let url = hook_url(dir.path(), one).await;
        hook(
            &url,
            json!({"hook_event_name": "SessionStart", "session_id": one.to_string()}),
        )
        .await;
        eventually("its turn over", async || {
            (d.view(one).unwrap().state != SessionState::Starting).then_some(())
        })
        .await;
        assert_eq!(d.charges_busy("desk"), 3, "it is no longer working");
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(
            started(&d).len(),
            4,
            "a session that finished a turn holds its slot"
        );
        d.end(one, Some(By::you())).await.unwrap();
        eventually("the fifth session", async || {
            let open: Vec<_> = started(&d)
                .into_iter()
                .filter(|v| v.state != SessionState::Closed)
                .collect();
            (open.len() == 4 && open.iter().all(|v| v.id != one)).then_some(())
        })
        .await;
        assert!(d.plan(id).await.unwrap().summary.progress.next.is_empty());
    })
    .await;
}

/// How many sessions plan `id` holds now.
async fn held_now(d: &Desk, id: uuid::Uuid) -> usize {
    workforce_desk::plan::held(&d.plan_tickets(id).await.unwrap(), |s| d.session_busy(s))
}

/// A plan's session here keeps its slot until it is closed: a finished
/// turn or being let go keeps it, its ticket still in Doing.
#[tokio::test(flavor = "multi_thread")]
async fn a_plans_session_here_holds_its_slot_until_it_is_closed() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, state, d, _llm) = hub_with(dir.path(), pool, ACTS).await;
        let _crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        let p = proposed(&d, dir.path(), 2).await;
        let id = p.summary.plan.id;
        d.approve_plan(id, 1, &By::you()).await.unwrap();
        let two = eventually("two sessions", async || {
            let s = started(&d);
            (s.len() == 2).then_some(s)
        })
        .await;
        let (kept, let_go) = (two[0].id, two[1].id);
        for id in [kept, let_go] {
            let url = hook_url(dir.path(), id).await;
            hook(
                &url,
                json!({"hook_event_name": "SessionStart", "session_id": id.to_string()}),
            )
            .await;
        }
        eventually("their turns over", async || {
            [kept, let_go]
                .iter()
                .all(|id| d.view(*id).unwrap().state != SessionState::Starting)
                .then_some(())
        })
        .await;
        assert_eq!(held_now(&d, id).await, 2, "a finished turn holds it");
        d.release(let_go, &By::you()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(held_now(&d, id).await, 2, "let go, its ticket holds it");
        d.end(kept, Some(By::you())).await.unwrap();
        eventually("the closed one's slot free", async || {
            (held_now(&d, id).await == 1).then_some(())
        })
        .await;
        let line = workforce_desk::plan::full(&d.plan(id).await.unwrap().summary.progress, 1);
        assert!(line.is_some_and(|l| l.starts_with("holds 1 of 1 session: DES-")));
    })
    .await;
}

/// A plan's session on a machine keeps its slot while the machine says it
/// runs, was killed there or isn't listed; once the machine says it was
/// closed or removed there, as `desk.sessions` says of the ids it is asked
/// about, the slot is free and the next ticket starts.
#[tokio::test(flavor = "multi_thread")]
async fn a_plans_session_on_a_machine_holds_its_slot_until_the_machine_says_it_was_closed() {
    use super::super::relay::RelayAnswer;
    use super::super::served::ServedStatus;
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, _llm) = hub_with(dir.path(), pool, ACTS).await;
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("the plan tools", async || {
            overseer.tools.has("crew.plan").then_some(())
        })
        .await;
        // devbox: its open sessions, and those closed there, said only of
        // the ids it is asked about.
        let there: Arc<parking_lot::Mutex<Vec<Value>>> = Arc::default();
        let closed: Arc<parking_lot::Mutex<Vec<String>>> = Arc::default();
        let poller = {
            let (crew, there, closed) =
                (Arc::clone(&crew), Arc::clone(&there), Arc::clone(&closed));
            tokio::spawn(async move {
                loop {
                    for c in crew.relay_take(Duration::from_millis(100)).await {
                        let body = match c.tool.as_str() {
                            "desk.sessions" => {
                                let mut all = there.lock().clone();
                                let gone = closed.lock().clone();
                                all.extend(
                                    c.input["ids"]
                                        .as_array()
                                        .into_iter()
                                        .flatten()
                                        .filter_map(Value::as_str)
                                        .filter(|i| gone.iter().any(|g| g == i))
                                        .map(|i| json!({ "id": i, "state": "closed" })),
                                );
                                Value::Array(all)
                            }
                            "desk.session_start" => {
                                let s = json!({
                                    "id": uuid::Uuid::new_v4(), "name": c.input["name"],
                                    "cwd": c.input["cwd"], "state": "working", "lastReply": null,
                                });
                                there.lock().push(s.clone());
                                s
                            }
                            // The work loop's and the pump's takes, which this
                            // machine doesn't answer.
                            super::super::relayed_work::WORK_TAKE
                            | super::super::relayed_decide::TAKE => Value::Null,
                            other => panic!("{other} relayed"),
                        };
                        crew.relay_answer(
                            c.id,
                            RelayAnswer {
                                status: ServedStatus::Ok,
                                body,
                            },
                        );
                    }
                }
            })
        };
        let r = overseer
            .tools
            .get("crew.plan")
            .unwrap()
            .execute(json!({
                "result": judged(),
                "goal": "Speed up vellev's recall",
                "machine": "devbox",
                "parallel": 1,
                "tickets": [
                    {"title": "Profile recall", "folder": "/home/dev/vellev"},
                    {"title": "Cache embeddings", "folder": "/home/dev/vellev-cache"},
                ],
            }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        let id: uuid::Uuid = r.output["proposed"]["plan_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        d.approve_plan(id, 1, &By::you()).await.unwrap();
        eventually("its first session on devbox", async || {
            (there.lock().len() == 1).then_some(())
        })
        .await;
        let first = there.lock()[0]["id"].as_str().unwrap().to_string();
        for state in ["idle", "interrupted"] {
            there.lock()[0]["state"] = json!(state);
            tokio::time::sleep(Duration::from_millis(600)).await;
            assert_eq!(there.lock().len(), 1, "{state} there, it holds its slot");
        }
        // Gone from its list: not known closed, so it still holds it.
        there.lock().clear();
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(held_now(&d, id).await, 1, "unlisted, it holds its slot");
        // Said closed there: free, and the next starts.
        closed.lock().push(first.clone());
        eventually("the second session on devbox", async || {
            (there.lock().len() == 1).then_some(())
        })
        .await;
        assert_ne!(there.lock()[0]["id"], json!(first));
        let was = format!("devbox/{first}");
        let tickets = d.plan_tickets(id).await.unwrap();
        assert!(tickets
            .iter()
            .all(|t| t.session.as_deref() != Some(was.as_str())));
        assert_eq!(held_now(&d, id).await, 1);
        poller.abort();
    })
    .await;
}

/// Unbound, a boss's plan is still a draft for the person until their own
/// words ask for the work to start: asked only to plan, nothing starts and
/// the person is asked; told to start it, the boss approves it with their
/// words and it starts, four of five tickets up to its limit, the fifth
/// when one finishes, and its changes to the plan apply as they are made.
#[tokio::test(flavor = "multi_thread")]
async fn an_unbound_boss_starts_its_own_plan_only_when_the_person_said_to() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, _llm) =
            hub_with(dir.path(), pool, super::unbound::UNBOUND).await;
        let _crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("the plan tools", async || {
            overseer.tools.has("crew.plan").then_some(())
        })
        .await;
        let r = overseer
            .tools
            .get("crew.plan")
            .unwrap()
            .execute(json!({ "result": judged(), "goal": "Make the suite green", "tickets": tickets(dir.path(), 5) }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        let id: uuid::Uuid = r.output["proposed"]["plan_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        // Asked only for a plan: a draft, whatever its level.
        assert_eq!(r.output["proposed"]["state"], "proposed", "{r:?}");
        assert!(r.output["note"].as_str().unwrap().contains("ready for review"), "{r:?}");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(started(&d).is_empty(), "nothing starts unasked");
        assert_eq!(d.plan_needs().len(), 1, "the person is asked to approve it");
        // start_now with no words of the person's is refused.
        let update = overseer.tools.get("crew.plan_update").unwrap();
        let r = update
            .execute(json!({ "plan_id": id.to_string(), "start_now": {}, "why": "Go." }))
            .await
            .unwrap();
        assert!(!r.success, "{r:?}");
        assert!(started(&d).is_empty());
        // The person says to start it: the boss starts it, saying their words.
        person_says(&overseer, "Looks good, start it now");
        let r = update
            .execute(json!({
                "plan_id": id.to_string(),
                "start_now": { "person_said": "Looks good, start it now" },
                "why": "The person asked to start it.",
            }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        assert_ne!(r.output["plan"]["state"], "proposed", "{r:?}");
        assert!(d.plan_needs().is_empty(), "nothing waits on the person");
        let four = eventually("four sessions", async || {
            let s = started(&d);
            (s.len() == 4).then_some(s)
        })
        .await;
        let one = four[0].id;
        d.end(one, Some(By::you())).await.unwrap();
        eventually("the fifth session", async || {
            let open = started(&d);
            (open.len() == 4 && open.iter().all(|v| v.id != one)).then_some(())
        })
        .await;

        // Its goal is the person's, unbound too: changed only on their
        // answer.
        let update = overseer.tools.get("crew.plan_update").unwrap();
        let r = update
            .execute(json!({ "plan_id": id.to_string(), "goal": "Make the whole suite green", "why": "Wider." }))
            .await
            .unwrap();
        assert!(!r.success, "{r:?}");
        assert!(r.error.unwrap().starts_with("The goal and how it's measured are the person's"));
        // A change of scope applies at once, as the person's would.
        let before = d.plan(id).await.unwrap().summary.plan.revision;
        let mut more = tickets(dir.path(), 6);
        let kept: Vec<Value> = d
            .plan(id)
            .await
            .unwrap()
            .tickets
            .iter()
            .map(|t| json!({ "id": t.ticket.ticket.id.to_string(), "folder": t.folder }))
            .collect();
        more.splice(0..5, kept);
        let r = update
            .execute(json!({ "plan_id": id.to_string(), "tickets": more, "why": "One more flaky test." }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        assert_eq!(r.output["applied"], true, "{r:?}");
        let p = d.plan(id).await.unwrap();
        assert!(p.summary.plan.revision > before);
        assert_eq!(p.summary.plan.approved_revision, Some(p.summary.plan.revision));
        assert!(d.plan_needs().is_empty());
        // So does one its goal doesn't cover: a raised limit.
        let r = update
            .execute(json!({ "plan_id": id.to_string(), "parallel": 2, "why": "Fewer at once." }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        let r = update
            .execute(json!({ "plan_id": id.to_string(), "parallel": 3, "why": "One more at once." }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        assert_eq!(r.output["change"], "scope", "{r:?}");
        assert_eq!(r.output["applied"], true, "{r:?}");
        assert!(d.plan_needs().is_empty());
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
            workforce_desk::plan::room(&p.summary.plan, 0, 0, 4),
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
            .execute(json!({ "result": judged(), "goal": "Make the suite green", "tickets": tickets(dir.path(), 2), "parallel": 1 }))
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

/// The plan a boss proposes in `dir`: a ticket whose brief names this
/// computer's home and a volume mounted here, and one that names neither.
async fn plan_with_local_paths(
    overseer: &AgentService,
    d: &Desk,
    dir: &Path,
) -> (String, Vec<String>) {
    let folder = dir.to_string_lossy();
    let home = d
        .home_folder()
        .unwrap()
        .to_string_lossy()
        .trim_end_matches('/')
        .to_string();
    let made = overseer
        .tools
        .get("crew.plan")
        .unwrap()
        .execute(json!({ "result": judged(), "goal": "Faster reads", "tickets": [
            { "title": "Add the cache", "body": format!("Read {home}/Library/Desk/notes.md and /Volumes/T9/data/x.csv."), "folder": folder },
            { "title": "Use the cache", "body": "Read through the cache.", "folder": folder },
        ] }))
        .await
        .unwrap();
    let plan = &made.output["proposed"];
    let keys = plan["tickets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["key"].as_str().unwrap().to_string())
        .collect();
    (plan["plan_id"].as_str().unwrap().to_string(), keys)
}

/// Tickets moved to a machine whose briefs name paths only this computer
/// has: the boss is told which, and the move is made all the same; and a
/// plan the person said to start isn't started while one is said.
#[tokio::test(flavor = "multi_thread")]
async fn a_boss_moving_tickets_to_a_machine_is_told_of_paths_in_briefs_only_this_computer_has() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, _llm) = hub_with(dir.path(), pool, super::unbound::UNBOUND).await;
        let _crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("the plan tools", async || {
            overseer.tools.has("crew.plan").then_some(())
        })
        .await;
        let home = d
            .home_folder()
            .unwrap()
            .to_string_lossy()
            .trim_end_matches('/')
            .to_string();
        let update = overseer.tools.get("crew.plan_update").unwrap();
        let moved = |id: &str, keys: &[String], machine: &str| {
            json!({ "plan_id": id, "why": "The person wants it there.", "tickets_edit": [
                { "key": keys[0], "machine": machine, "folder": "/home/dev/vellev" },
                { "key": keys[1], "machine": machine, "folder": "/home/dev/vellev" },
            ] })
        };

        let (id, keys) = plan_with_local_paths(&overseer, &d, dir.path()).await;
        let r = update.execute(moved(&id, &keys, "devbox")).await.unwrap();
        assert!(r.success, "{r:?}");
        let note = r.output["note"].as_str().unwrap();
        assert!(
            note.contains(&format!("{}'s brief names ", keys[0])),
            "{note}"
        );
        assert!(
            note.contains("/Volumes/T9") && note.contains(&home),
            "{note}"
        );
        assert!(
            note.contains("on this computer, which devbox may not have"),
            "{note}"
        );
        assert!(!note.contains(&keys[1]), "{note}");
        let rows = d.plan(id.parse().unwrap()).await.unwrap().tickets;
        assert!(rows.iter().all(|t| t.machine.as_deref() == Some("devbox")));
        let r = update
            .execute(json!({ "plan_id": id, "why": "Its goal.", "goal": "Faster reads, soon" }))
            .await
            .unwrap();
        assert!(
            !r.output["note"]
                .as_str()
                .unwrap_or_default()
                .contains("may not have"),
            "{r:?}"
        );

        // Told to start it as it moves: it waits for the briefs.
        let (id, keys) = plan_with_local_paths(&overseer, &d, dir.path()).await;
        let mut start = moved(&id, &keys, "devbox");
        person_says(&overseer, "Move it to devbox and start it now");
        start["start_now"] = json!({ "person_said": "Move it to devbox and start it now" });
        let r = update.execute(start).await.unwrap();
        assert!(r.success, "{r:?}");
        assert!(
            r.output["note"]
                .as_str()
                .unwrap()
                .contains("isn't started yet"),
            "{r:?}"
        );
        assert_eq!(r.output["plan"]["state"], "proposed", "{r:?}");
    })
    .await;
}

/// Below unbound, a move of an approved plan's tickets that rewrites their
/// briefs is proposed: what the boss is told is of the briefs as proposed.
#[tokio::test(flavor = "multi_thread")]
async fn a_proposed_move_is_told_of_the_briefs_it_proposes() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, _llm) = hub_with(dir.path(), pool, ACTS).await;
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("the plan tools", async || overseer.tools.has("crew.plan").then_some(())).await;
        let update = overseer.tools.get("crew.plan_update").unwrap();
        let (id, keys) = plan_with_local_paths(&overseer, &d, dir.path()).await;
        // Nothing of the plan starts until the move is asked: a ticket that
        // started can't move. Every start waits its turn on this.
        let no_starts = crew.starting.lock().await;
        d.approve_plan(id.parse().unwrap(), 1, &By::you()).await.unwrap();
        let r = update
            .execute(json!({ "plan_id": id, "why": "The person wants it on devbox.", "tickets_edit": [
                { "key": keys[0], "machine": "devbox", "folder": "/home/dev/vellev", "body": "Read the notes in the repo." },
                { "key": keys[1], "machine": "devbox", "folder": "/home/dev/vellev", "body": "Read /Volumes/T9/data/y.csv." },
            ] }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        assert_eq!(r.output["applied"], false, "{r:?}");
        let note = r.output["note"].as_str().unwrap();
        assert!(note.contains(&format!("{}'s brief names /Volumes/T9 on", keys[1])), "{note}");
        assert!(!note.contains(&format!("{}'s brief", keys[0])), "{note}");
        drop(no_starts);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_boss_moves_tickets_by_key_keeping_their_briefs_and_reads_them_back() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, _llm) = hub_with(dir.path(), pool, ACTS).await;
        let _crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("the plan tools", async || overseer.tools.has("crew.plan").then_some(())).await;
        let folder = dir.path().to_string_lossy().into_owned();
        let made = overseer
            .tools
            .get("crew.plan")
            .unwrap()
            .execute(json!({ "result": judged(), "goal": "Faster reads", "tickets": [
                { "title": "Add the cache", "body": "Cache the index; keep it under 50 MB.", "folder": folder },
                { "title": "Use the cache", "body": "Read through the cache.", "folder": folder, "needs": [0] },
            ] }))
            .await
            .unwrap();
        let plan = &made.output["proposed"];
        let id = plan["plan_id"].as_str().unwrap().to_string();
        let keys: Vec<String> = plan["tickets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["key"].as_str().unwrap().to_string())
            .collect();
        let update = overseer.tools.get("crew.plan_update").unwrap();
        // Both to devbox, by key alone: titles and briefs stay.
        let r = update
            .execute(json!({ "plan_id": id, "why": "The person wants it on devbox.",
                "tickets_edit": [
                    { "key": keys[0], "machine": "devbox", "folder": "/home/dev/projects/driftwerk/vellev" },
                    { "key": keys[1].to_lowercase(), "machine": "devbox", "folder": "/home/dev/projects/driftwerk/vellev" },
                ] }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        let board = overseer.tools.get("crew.board").unwrap();
        let read = board
            .execute(json!({ "plan_id": id, "briefs": [keys[1]] }))
            .await
            .unwrap();
        assert!(read.success, "{read:?}");
        let rows = read.output["plan"]["tickets"].as_array().unwrap().clone();
        assert!(rows.iter().all(|t| t["machine"] == "devbox"), "{rows:?}");
        assert_eq!(rows[1]["title"], "Use the cache");
        assert!(rows[0].get("brief").is_none(), "only the brief asked for");
        assert!(
            rows[1]["brief"].as_str().unwrap().contains("Read through the cache."),
            "{rows:?}"
        );
        let all = board
            .execute(json!({ "plan_id": id, "briefs": true }))
            .await
            .unwrap();
        assert!(all.output["plan"]["tickets"][0]["brief"]
            .as_str()
            .unwrap()
            .contains("under 50 MB"));
        // A whole list that leaves out a title and a body keeps them.
        let r = update
            .execute(json!({ "plan_id": id, "why": "Order.", "tickets": [
                { "key": keys[0], "folder": "/home/dev/projects/driftwerk/vellev" },
                { "key": keys[1], "folder": "/home/dev/projects/driftwerk/vellev", "needs": [0] },
            ] }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        let p = d.plan(id.parse().unwrap()).await.unwrap();
        assert_eq!(p.tickets[0].ticket.ticket.title, "Add the cache");
        assert_eq!(p.tickets[0].ticket.ticket.body, "Cache the index; keep it under 50 MB.");
        assert_eq!(p.tickets[1].machine.as_deref(), Some("devbox"), "where it runs is kept");
        // Refused in the boss's words.
        let said = |r: workforce_agent::tools::ToolResult| format!("{r:?}");
        let unknown = update
            .execute(json!({ "plan_id": id, "why": "x", "tickets_edit": [{ "key": "ZZZ-9", "folder": "/x" }] }))
            .await
            .unwrap();
        assert!(said(unknown).contains("ZZZ-9 isn't one of the plan's tickets"));
        let away = update
            .execute(json!({ "plan_id": id, "why": "x", "tickets_edit": [{ "key": keys[0], "machine": "local" }] }))
            .await
            .unwrap();
        assert!(said(away).contains("moves to this computer: say its folder there too"));
        let both = update
            .execute(json!({ "plan_id": id, "why": "x",
                "tickets": tickets(dir.path(), 1),
                "tickets_edit": [{ "key": keys[0], "model": "opus" }] }))
            .await
            .unwrap();
        assert!(said(both).contains("not both"));
        // Another's plan is read as none.
        let none = board
            .execute(json!({ "plan_id": uuid::Uuid::new_v4().to_string() }))
            .await
            .unwrap();
        assert!(!none.success);
    })
    .await;
}

/// A ticket moves by its repository, its old folder left behind; the
/// machine tools say plainly what they can't do, and never reach a machine
/// Desk isn't carrying calls to.
#[tokio::test(flavor = "multi_thread")]
async fn a_boss_moves_a_ticket_by_its_repository_and_its_machine_tools_say_what_they_cant() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, _llm) = hub_with(dir.path(), pool, ACTS).await;
        let _crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("the plan tools", async || overseer.tools.has("crew.plan").then_some(())).await;
        let folder = dir.path().to_string_lossy().into_owned();
        let made = overseer
            .tools
            .get("crew.plan")
            .unwrap()
            .execute(json!({ "result": judged(), "goal": "Faster reads", "tickets": [
                { "title": "Add the cache", "body": "Cache it.", "folder": folder },
            ] }))
            .await
            .unwrap();
        let plan = &made.output["proposed"];
        let id = plan["plan_id"].as_str().unwrap().to_string();
        let key = plan["tickets"][0]["key"].as_str().unwrap().to_string();
        let r = overseer
            .tools
            .get("crew.plan_update")
            .unwrap()
            .execute(json!({ "plan_id": id, "why": "Run it on devbox.",
                "tickets_edit": [{ "key": key, "machine": "devbox", "repo": "driftwerk/vellev" }] }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        let p = d.plan(id.parse().unwrap()).await.unwrap();
        let t = &p.tickets[0];
        assert_eq!(t.machine.as_deref(), Some("devbox"));
        assert_eq!(t.repo.as_deref(), Some("github.com/driftwerk/vellev"));
        assert_eq!(t.folder.as_deref(), Some(""), "the Mac's folder isn't devbox's");
        // Its repository alone, changed: saved where it starts from.
        let r = overseer
            .tools
            .get("crew.plan_update")
            .unwrap()
            .execute(json!({ "plan_id": id, "why": "The other repository.",
                "tickets_edit": [{ "key": key, "repo": "driftwerk/workforce" }] }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        let p = d.plan(id.parse().unwrap()).await.unwrap();
        assert_eq!(
            p.tickets[0].repo.as_deref(),
            Some("github.com/driftwerk/workforce")
        );
        let runs = d.store().plan_runs(id.parse().unwrap()).await.unwrap();
        assert_eq!(
            runs[0].1.repo.as_deref(),
            Some("github.com/driftwerk/workforce"),
            "the row a start reads"
        );

        let said = |r: workforce_agent::tools::ToolResult| format!("{r:?}");
        let folders = overseer.tools.get("crew.machine_folders").unwrap();
        let bad = folders
            .execute(json!({ "machine": "devbox", "repo": "not a repo" }))
            .await
            .unwrap();
        assert!(said(bad).contains("host/owner/name"));
        let odd = folders.execute(json!({ "machine": "../x" })).await.unwrap();
        assert!(said(odd).contains("isn't one of the person's machines"));
        // Nothing carries calls in a test: said, never waited on.
        let away = folders.execute(json!({ "machine": "devbox" })).await.unwrap();
        let away = said(away);
        assert!(away.contains("can't be reached"), "{away}");
        let machines = overseer.tools.get("crew.machines").unwrap();
        assert!(!machines.execute(json!({})).await.unwrap().success);
        let make = overseer.tools.get("crew.machine_make_folder").unwrap();
        let why = make
            .execute(json!({ "machine": "devbox", "path": "~/projects/acme" }))
            .await
            .unwrap();
        assert!(said(why).contains("Give the reason"));
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
            "[oversight]\nenabled = true\nlevel = \"act\"\n[oversight.act]\nhand_out_work = false\n[oversight.work]\nparallel = 1\n",
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
        let noted = eventually("the pause noted", async || {
            d.activity(workforce_desk::crew::Filter::All, None, 50)
                .await
                .unwrap()
                .events
                .into_iter()
                .find(|e| e.kind == workforce_desk::crew::Kind::PlanPaused)
        })
        .await;
        assert_eq!(
            noted.reason.as_deref(),
            Some(why.as_str()),
            "kept with its pause"
        );
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

/// A start the person dismissed holds no slot: at one at a time, the next
/// ticket is suggested in its place.
#[tokio::test(flavor = "multi_thread")]
async fn a_dismissed_start_holds_no_slot_and_the_next_is_suggested() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, state, d, _llm) = hub_with(
            dir.path(),
            pool,
            "[oversight]\nenabled = true\nlevel = \"suggest\"\n[oversight.work]\nparallel = 1\n",
        )
        .await;
        let p = proposed(&d, dir.path(), 2).await;
        let id = p.summary.plan.id;
        d.approve_plan(id, 1, &By::you()).await.unwrap();
        let _crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        let suggested = async || {
            d.store()
                .suggestions_for("desk", "crew.assign_ticket")
                .await
                .unwrap()
        };
        let mut first = eventually("the first start suggested", async || {
            suggested().await.pop()
        })
        .await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(suggested().await.len(), 1, "one at a time");
        first.state = workforce_desk::crew::SuggestionState::Dismissed;
        d.store().put_suggestion(&first).await.unwrap();
        let next = eventually("the next start suggested", async || {
            suggested()
                .await
                .into_iter()
                .find(|s| s.action["input"]["ticket_id"] != first.action["input"]["ticket_id"])
        })
        .await;
        assert_eq!(next.state, workforce_desk::crew::SuggestionState::Open);
        assert_eq!(suggested().await.len(), 2, "the dismissed one not again");
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
            "[oversight]\nenabled = true\nlevel = \"act\"\n[oversight.act]\nhand_out_work = false\n[oversight.work]\nparallel = 1\n",
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

/// How many of the boss's turns were prompted with `marker`: the newest
/// message of each request, not the history it carries.
async fn newly_said(llm: &wiremock::MockServer, marker: &str) -> usize {
    llm.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| {
            let body: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
            let last = body["messages"]
                .as_array()
                .and_then(|m| m.last())
                .or_else(|| body["input"].as_array().and_then(|m| m.last()))
                .map(Value::to_string)
                .unwrap_or_else(|| String::from_utf8_lossy(&r.body).into_owned());
            last.contains(marker)
        })
        .count()
}

/// Waits until `marker` has been said `times` times, then for a plan turn's
/// whole gather: it is said no more.
async fn said_only(llm: &wiremock::MockServer, marker: &str, times: usize) {
    eventually(marker, async || {
        (newly_said(llm, marker).await >= times).then_some(())
    })
    .await;
    tokio::time::sleep(super::super::plan::GATHER + Duration::from_secs(1)).await;
    assert_eq!(
        newly_said(llm, marker).await,
        times,
        "one turn a hold, however often it is counted"
    );
}

/// `usd` spent by `session` now on a paid key, and the budgets counted again.
pub(super) async fn spent(d: &Desk, session: uuid::Uuid, usd: f64) {
    use workforce_desk::ledger::{Billing, Priced, Spend, Tokens};
    use workforce_desk::ledger_store::Kept;
    d.store()
        .ledger_write(
            session,
            Engine::Claude,
            None,
            &[Kept {
                spend: Spend {
                    key: uuid::Uuid::new_v4().to_string(),
                    at: chrono::Utc::now(),
                    model: Some("claude-opus-5-5".into()),
                    tokens: Tokens {
                        input: 1000,
                        ..Default::default()
                    },
                    usd: Some(usd),
                    priced: Priced::Reported,
                    billing: None,
                },
                // On a paid key: a dollar budget never holds plan use.
                billing: Billing::Key,
                account: None,
            }],
        )
        .await
        .unwrap();
    d.count_spend_now().await;
}

/// A plan of `n` independent tickets, one at a time, approved by the
/// person capped at `usd`.
async fn capped(d: &Desk, dir: &Path, n: usize, usd: f64) -> uuid::Uuid {
    let p = d
        .propose_plan(
            NewPlan {
                parallel: Some(1),
                ..proposal(dir, n)
            },
            &Party::manager("desk", "Desk"),
            &By::manager("desk", "Desk", "Sam asked for a green suite."),
        )
        .await
        .unwrap();
    let id = p.summary.plan.id;
    let budget = Some(Some(workforce_desk::plan::PlanBudget::usd(usd)));
    d.decide_plan(id, 1, true, budget, &By::you())
        .await
        .unwrap();
    id
}

fn proposal(dir: &Path, n: usize) -> NewPlan {
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
        budget: None,
        result: None,
        goal_ticket: None,
    }
}

/// A plan stops starting at its budget's warning and its boss is told once;
/// the person's Continue starts it again; over, it stops again. The boss
/// can't let it go on: raising the budget waits for the person.
#[tokio::test(flavor = "multi_thread")]
async fn a_plan_at_its_warning_stops_starting_until_the_person_says_continue_and_over_it_stops_again(
) {
    use workforce_desk::plan::Paused;
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, llm) = hub_with(dir.path(), pool, ACTS).await;
        d.count_budgets_in(chrono_tz::UTC);
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("the plan tools", async || {
            overseer.tools.has("crew.plan").then_some(())
        })
        .await;
        let id = capped(&d, dir.path(), 3, 10.0).await;
        let first = eventually("the first session", async || {
            let s = started(&d);
            (s.len() == 1).then(|| s[0].id)
        })
        .await;
        // Handed its ticket (what its spend is counted by), it spends.
        eventually("the first ticket handed to its session", async || {
            d.plan(id)
                .await
                .unwrap()
                .tickets
                .iter()
                .any(|t| t.ticket.ticket.session() == Some(first))
                .then_some(())
        })
        .await;
        spent(&d, first, 8.5).await;
        let paused = eventually("the plan paused at its warning", async || {
            let p = d.plan(id).await.unwrap().summary;
            (p.plan.paused == Some(Paused::Budget)).then_some(p)
        })
        .await;
        assert!(
            paused
                .held
                .as_deref()
                .unwrap()
                .ends_with("Desk has stopped starting sessions."),
            "{:?}",
            paused.held
        );
        let subject = workforce_desk::plan_subject(id);
        let needs: Vec<_> = d
            .breaker_needs()
            .into_iter()
            .filter(|n| n.id == format!("breaker:{subject}"))
            .collect();
        assert_eq!(needs.len(), 1, "one need");
        // Nor does a hand-out of one of its tickets to a new session.
        let waiting = d
            .plan(id)
            .await
            .unwrap()
            .tickets
            .into_iter()
            .find(|t| t.ticket.ticket.state == TicketState::Todo)
            .unwrap();
        let handout = Action::AssignTicket {
            ticket: waiting.ticket.key.clone().unwrap(),
            id: Some(waiting.ticket.ticket.id),
            text: String::new(),
            to: super::super::actions::Handout::NewSession {
                folder: dir.path().to_string_lossy().into_owned(),
                engine: Some(Engine::Claude),
                model: None,
            },
            planned: true,
        };
        let refused = crew
            .perform(&handout, By::manager("desk", "Desk", "Start the next."))
            .await
            .unwrap_err();
        assert!(refused.contains("breaker"), "{refused}");
        assert_eq!(started(&d).len(), 1);
        // The first session is closed, freeing its slot: nothing starts
        // while the plan is held.
        let key = d.view(first).unwrap().name;
        d.end(first, Some(By::you())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(started(&d).is_empty(), "held: no start");

        // Its boss can't let it go on: no tool of its reaches the breaker,
        // and a higher budget waits for the person.
        let names: Vec<String> = overseer.tools.names();
        assert!(
            names
                .iter()
                .filter(|n| n.starts_with("crew."))
                .all(|n| !n.contains("breaker")
                    && !n.contains("budget")
                    && !n.contains("continue")),
            "{names:?}"
        );
        let r = overseer
            .tools
            .get("crew.plan_update")
            .unwrap()
            .execute(
                json!({ "plan_id": id.to_string(), "budget": { "amount": 100 },
                "why": "The suite needs more." }),
            )
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        assert_eq!(r.output["applied"], false);
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(
            d.plan(id).await.unwrap().summary.plan.paused,
            Some(Paused::Budget)
        );
        assert!(started(&d).is_empty());
        // The boss was told of the hold once, in one plan turn.
        said_only(&llm, "Desk's breaker holds its new starts", 1).await;

        // The person says Continue: the next starts.
        d.breaker_continue(&subject, By::you()).unwrap();
        let second = eventually("the next session", async || {
            let s: Vec<_> = started(&d).into_iter().filter(|v| v.id != first).collect();
            (s.len() == 1).then(|| s[0].id)
        })
        .await;
        assert_ne!(
            d.view(second).unwrap().name,
            key,
            "a ticket of its own, not the one done"
        );
        assert_eq!(d.plan(id).await.unwrap().summary.plan.paused, None);

        // Over: it stops again, and the hold is said again only once.
        eventually("the next ticket handed to its session", async || {
            d.plan(id)
                .await
                .unwrap()
                .tickets
                .iter()
                .any(|t| t.ticket.ticket.session() == Some(second))
                .then_some(())
        })
        .await;
        spent(&d, second, 2.0).await;
        eventually("the plan paused over its budget", async || {
            let p = d.plan(id).await.unwrap().summary;
            (p.plan.paused == Some(Paused::Budget)).then_some(())
        })
        .await;
        for _ in 0..4 {
            d.count_spend_now().await;
        }
        said_only(&llm, "Desk's breaker holds its new starts", 2).await;
        assert_eq!(started(&d).len(), 1);
    })
    .await;
}

/// A boss held by Desk's breaker takes no turns: its plans pause until the
/// person lets it go on, and start again then.
#[tokio::test(flavor = "multi_thread")]
async fn a_held_bosss_plans_pause_until_the_person_lets_it_go_on() {
    use workforce_core::budget::{OnExceed, Period, Standing, Status, Unit};
    use workforce_desk::plan::Paused;
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, _llm) = hub_with(dir.path(), pool, ACTS).await;
        let _crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("the plan tools", async || {
            overseer.tools.has("crew.plan").then_some(())
        })
        .await;
        let now = chrono::Utc::now();
        let over = Standing {
            budget: uuid::Uuid::new_v4(),
            period: Period::Day,
            unit: Unit::Usd,
            used: 12.0,
            of: 11.0,
            status: Status::Over,
            on_exceed: OnExceed::Pause,
            from: now - chrono::Duration::hours(1),
            to: now + chrono::Duration::hours(10),
            shared: false,
        };
        d.agent_stand("desk", "Desk", vec![over], false);
        assert!(d.agent_held("desk").is_some());
        let p = proposed(&d, dir.path(), 2).await;
        let id = p.summary.plan.id;
        d.approve_plan(id, 1, &By::you()).await.unwrap();
        eventually("the plan paused for its boss", async || {
            let p = d.plan(id).await.unwrap().summary;
            (p.plan.paused == Some(Paused::BossHeld)).then_some(())
        })
        .await;
        assert!(started(&d).is_empty());
        d.breaker_continue(&workforce_desk::agent_subject("desk"), By::you())
            .unwrap();
        eventually("its sessions", async || {
            (started(&d).len() == 2).then_some(())
        })
        .await;
        assert_eq!(d.plan(id).await.unwrap().summary.plan.paused, None);
    })
    .await;
}

/// Bound at Act, start_now is the person's agreement, as `agreed` is: a
/// draft starts on their checked words. One that doesn't read changes
/// nothing else of the call either.
#[tokio::test(flavor = "multi_thread")]
async fn a_bound_boss_starts_its_own_plan_only_on_the_persons_words() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, _llm) = hub_with(dir.path(), pool, ACTS).await;
        let _crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("the plan tools", async || {
            overseer.tools.has("crew.plan").then_some(())
        })
        .await;
        let r = overseer
            .tools
            .get("crew.plan")
            .unwrap()
            .execute(json!({
                "result": judged(),
                "goal": "Make the suite green",
                "tickets": tickets(dir.path(), 2),
            }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        assert_eq!(r.output["proposed"]["state"], "proposed", "{r:?}");
        let id: uuid::Uuid = r.output["proposed"]["plan_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let update = overseer.tools.get("crew.plan_update").unwrap();
        // One that doesn't read: refused before the goal changes.
        let before = d.plan(id).await.unwrap().summary.plan.revision;
        let r = update
            .execute(json!({
                "plan_id": id.to_string(),
                "goal": "Make the whole suite green",
                "start_now": { "person_said": "  " },
                "why": "Wider.",
            }))
            .await
            .unwrap();
        assert!(!r.success, "{r:?}");
        let p = d.plan(id).await.unwrap();
        assert_eq!(p.summary.plan.revision, before, "nothing changed");
        assert_eq!(p.summary.plan.goal, "Make the suite green");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(started(&d).is_empty(), "nothing starts");
        assert_eq!(d.plan_needs().len(), 1, "the person is asked");
        // Their own words start it.
        person_says(&overseer, "Start it now, go ahead");
        let r = update
            .execute(json!({
                "plan_id": id.to_string(),
                "start_now": { "person_said": "go ahead" },
                "why": "The person asked.",
            }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        assert_ne!(r.output["plan"]["state"], "proposed", "{r:?}");
        assert!(d.plan_needs().is_empty());
    })
    .await;
}

/// Unbound and told to start the work, crew.plan approves its plan as it
/// proposes it, and Desk starts it.
#[tokio::test(flavor = "multi_thread")]
async fn an_unbound_boss_told_to_start_starts_its_plan_as_it_proposes_it() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, _llm) = hub_with(dir.path(), pool, super::unbound::UNBOUND).await;
        let _crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("the plan tools", async || {
            overseer.tools.has("crew.plan").then_some(())
        })
        .await;
        person_says(&overseer, "Plan it and run it");
        let r = overseer
            .tools
            .get("crew.plan")
            .unwrap()
            .execute(json!({
                "result": judged(),
                "goal": "Make the suite green",
                "tickets": tickets(dir.path(), 2),
                "start_now": { "person_said": "Plan it and run it" },
            }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        assert_ne!(r.output["proposed"]["state"], "proposed", "{r:?}");
        assert!(d.plan_needs().is_empty());
        eventually("its sessions", async || {
            (started(&d).len() == 2).then_some(())
        })
        .await;
    })
    .await;
}

/// The boss sets up its own board: `crew.plan` with `new_board` makes it
/// and files the plan there; a board it names that it doesn't have says
/// how to make one; `crew.board_create` makes one or gives back the one it
/// has; and `crew.plan_update` with `board` re-files every ticket there
/// under new keys, its needs kept.
#[tokio::test(flavor = "multi_thread")]
async fn a_boss_makes_its_own_board_for_a_plan_and_moves_a_plan_onto_another() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, _llm) = hub_with(dir.path(), pool, ACTS).await;
        let _crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("the plan tools", async || {
            overseer.tools.has("crew.board_create").then_some(())
        })
        .await;
        // A plan already on the boss's board, Desk's own.
        let p = proposed(&d, dir.path(), 2).await;
        let id = p.summary.plan.id;
        let old: Vec<uuid::Uuid> = p.tickets.iter().map(|t| t.ticket.ticket.id).collect();
        assert!(p.tickets[0].ticket.key.as_deref().unwrap().starts_with("DES-"));
        let plan = overseer.tools.get("crew.plan").unwrap();
        let mut two = tickets(dir.path(), 2);
        two[1]["needs"] = json!([0]);

        // A prefix it has no board for: the way to one is said.
        let r = plan
            .execute(json!({ "result": judged(), "goal": "Vellev work", "board": "VEL", "tickets": two }))
            .await
            .unwrap();
        assert!(!r.success);
        let said = r.error.unwrap();
        assert!(said.starts_with("That isn't one of yours.") && said.contains("new_board"), "{said}");

        // new_board makes it, and the plan goes on it.
        let r = plan
            .execute(json!({
                "result": judged(),
                "goal": "Vellev work",
                "new_board": { "name": "Vellev", "prefix": "vel" },
                "tickets": two,
            }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        let keys: Vec<&str> = r.output["proposed"]["tickets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["key"].as_str().unwrap())
            .collect();
        assert_eq!(keys, ["VEL-1", "VEL-2"]);
        let vel = d.store().board_by_prefix("VEL").await.unwrap().unwrap();
        assert_eq!(vel.name, "Vellev");
        assert_eq!(vel.group, None, "in the boss's own scope");

        // Made once: asked again, the board it has comes back.
        let create = overseer.tools.get("crew.board_create").unwrap();
        let r = create
            .execute(json!({ "name": "Vellev", "prefix": "VEL", "reason": "The person asked." }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        assert_eq!(r.output["made"], false);
        assert_eq!(r.output["board"]["id"], vel.id.to_string());
        assert_eq!(d.boards().await.unwrap().iter().filter(|b| b.prefix.as_deref() == Some("VEL")).count(), 1);
        let r = create
            .execute(json!({ "name": "Vellev ops", "prefix": "VOP", "reason": "The person asked." }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        assert_eq!(r.output["made"], true);
        // A prefix that can't be one is said plainly.
        let r = create
            .execute(json!({ "name": "Bad", "prefix": "v", "reason": "The person asked." }))
            .await
            .unwrap();
        assert!(!r.success);

        // The first plan, moved onto VOP: new keys, the same tickets,
        // still the plan's.
        let r = overseer
            .tools
            .get("crew.plan_update")
            .unwrap()
            .execute(json!({ "plan_id": id.to_string(), "board": "VOP", "why": "The person wants it on its own board." }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        assert_eq!(r.output["change"], "board");
        let moved = d.plan(id).await.unwrap();
        let vop = d.store().board_by_prefix("VOP").await.unwrap().unwrap();
        assert_eq!(moved.summary.plan.board, vop.id);
        let now: Vec<uuid::Uuid> = moved.tickets.iter().map(|t| t.ticket.ticket.id).collect();
        assert_eq!(now, old);
        let keys: Vec<String> = moved.tickets.iter().map(|t| t.ticket.key.clone().unwrap()).collect();
        assert_eq!(keys, ["VOP-1", "VOP-2"]);
        assert!(moved.tickets.iter().all(|t| t.ticket.ticket.board == vop.id));
        assert_eq!(d.plan_tickets(id).await.unwrap().len(), 2, "still the plan's");
        // Moved again with new_board and a goal change in one call: both apply.
        let r = overseer
            .tools
            .get("crew.plan_update")
            .unwrap()
            .execute(json!({
                "plan_id": id.to_string(),
                "new_board": { "name": "Suite", "prefix": "STE" },
                "goal": "Make the whole suite green",
                "why": "Its own board, and wider.",
            }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        let keys: Vec<String> = r.output["plan"]["tickets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["key"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(keys, ["STE-1", "STE-2"]);

        // A move with a stale change: neither applies, and the keys stand.
        let update = overseer.tools.get("crew.plan_update").unwrap();
        let r = update
            .execute(json!({
                "plan_id": id.to_string(),
                "board": "VEL",
                "revision": 999,
                "goal": "Something else",
                "why": "Back to Vellev.",
            }))
            .await
            .unwrap();
        assert!(!r.success, "{r:?}");
        let ste = d.store().board_by_prefix("STE").await.unwrap().unwrap();
        assert_eq!(d.plan(id).await.unwrap().summary.plan.board, ste.id);

        // A board of another group: said, with the way to one of its own.
        d.change_layout(|l| l.create("other", "Other", &[], None)).await.unwrap();
        d.create_board(workforce_desk::NewBoard {
            name: "Other work".into(),
            group: Some("other".into()),
            prefix: Some("OTH".into()),
        })
        .await
        .unwrap();
        let r = update
            .execute(json!({ "plan_id": id.to_string(), "board": "OTH", "why": "Elsewhere." }))
            .await
            .unwrap();
        assert!(!r.success, "{r:?}");
        assert!(r.error.unwrap().contains("new_board"));
        assert_eq!(d.plan(id).await.unwrap().summary.plan.board, ste.id);

        // Without a prefix, made once and then the same board.
        let r = create
            .execute(json!({ "name": "Notes", "reason": "The person asked." }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        assert_eq!(r.output["made"], true);
        let again = create
            .execute(json!({ "name": "Notes", "reason": "The person asked." }))
            .await
            .unwrap();
        assert_eq!(again.output["made"], false, "{again:?}");
        assert_eq!(again.output["board"]["id"], r.output["board"]["id"]);
    })
    .await;
}

/// A plan's tickets run where the person said: on devbox, its session is
/// started there through Desk's courier, once, with the ticket's model and
/// that machine's folder; the ticket is that session's, and its turns wake
/// the boss, who moves the ticket.
#[tokio::test(flavor = "multi_thread")]
async fn a_plans_ticket_on_a_machine_starts_there_once_and_its_turns_wake_the_boss() {
    use super::super::relay::RelayAnswer;
    use super::super::served::ServedStatus;
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, llm) = hub_with(dir.path(), pool, ACTS).await;
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("the plan tools", async || {
            overseer.tools.has("crew.plan").then_some(())
        })
        .await;

        // devbox, as Desk's native side carries to it: its sessions, and
        // each start it was sent.
        let there: Arc<parking_lot::Mutex<Vec<Value>>> = Arc::default();
        let starts: Arc<parking_lot::Mutex<Vec<Value>>> = Arc::default();
        let poller = {
            let (crew, there, starts) =
                (Arc::clone(&crew), Arc::clone(&there), Arc::clone(&starts));
            tokio::spawn(async move {
                loop {
                    for c in crew.relay_take(Duration::from_millis(200)).await {
                        assert_eq!(c.machine, "devbox");
                        let body = match c.tool.as_str() {
                            "desk.sessions" => Value::Array(there.lock().clone()),
                            "desk.session_start" => {
                                starts.lock().push(c.input.clone());
                                let s = json!({
                                    "id": uuid::Uuid::new_v4(), "name": c.input["name"],
                                    "cwd": c.input["cwd"], "state": "working", "lastReply": null,
                                });
                                there.lock().push(s.clone());
                                s
                            }
                            // The work loop's and the pump's takes, which this
                            // machine doesn't answer.
                            super::super::relayed_work::WORK_TAKE
                            | super::super::relayed_decide::TAKE => Value::Null,
                            other => panic!("{other} relayed"),
                        };
                        crew.relay_answer(
                            c.id,
                            RelayAnswer {
                                status: ServedStatus::Ok,
                                body,
                            },
                        );
                    }
                }
            })
        };

        let r = overseer
            .tools
            .get("crew.plan")
            .unwrap()
            .execute(json!({
                "result": judged(),
                "goal": "Speed up vellev's recall",
                "machine": "devbox",
                "model": "claude-opus-4-8",
                "tickets": [{"title": "Profile recall", "folder": "/home/dev/vellev"}],
            }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        let ticket = &r.output["proposed"]["tickets"][0];
        assert_eq!(ticket["machine"], "devbox", "{r:?}");
        assert_eq!(ticket["model"], "claude-opus-4-8");
        let id: uuid::Uuid = r.output["proposed"]["plan_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        d.approve_plan(id, 1, &By::you()).await.unwrap();

        let started = eventually("its session on devbox", async || {
            let p = d.plan(id).await.ok()?;
            let t = &p.tickets[0].ticket.ticket;
            let a = t.assignee.as_ref()?;
            (t.state == TicketState::Doing).then(|| a.id.clone())
        })
        .await;
        let sent = starts.lock().clone();
        assert_eq!(sent.len(), 1, "{sent:?}");
        let key = d.plan(id).await.unwrap().tickets[0]
            .ticket
            .key
            .clone()
            .unwrap();
        assert_eq!(sent[0]["name"], json!(key));
        assert_eq!(sent[0]["cwd"], "/home/dev/vellev");
        assert_eq!(sent[0]["model"], "claude-opus-4-8");
        assert_eq!(sent[0]["deps"], true, "its folder is made ready there");
        assert!(
            sent[0]["prompt"]
                .as_str()
                .unwrap()
                .contains("person's own computer"),
            "{sent:?}"
        );
        let made = there.lock()[0]["id"].as_str().unwrap().to_string();
        assert_eq!(started, format!("devbox/{made}"));
        // No local session for it, and none started twice.
        assert!(d.list().iter().all(|v| v.name != key));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(starts.lock().len(), 1);

        // Its turn ends there: the boss is told, with its reply.
        there.lock()[0]["state"] = json!("idle");
        there.lock()[0]["lastReply"] = json!("Recall is 3x faster now.");
        eventually("the boss told", async || {
            (said(&llm, "finished a turn").await > 0).then_some(())
        })
        .await;
        assert!(said(&llm, "Recall is 3x faster now.").await > 0);
        poller.abort();
    })
    .await;
}

/// A start on a machine whose answer never came isn't undone or made
/// again: Desk looks for it there and takes up the session it made, by the
/// id it gave it, though it waits on the person there by then; the ticket
/// is that session's. An older session of the same key is never taken up,
/// nor the person's own of that key there.
#[tokio::test(flavor = "multi_thread")]
async fn a_machine_start_never_answered_is_found_there_and_never_made_twice() {
    use super::super::relay::RelayAnswer;
    use super::super::served::ServedStatus;
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, _llm) = hub_with(dir.path(), pool, ACTS).await;
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        *crew.relay_wait.lock() = Duration::from_millis(400);
        eventually("the plan tools", async || {
            overseer.tools.has("crew.plan").then_some(())
        })
        .await;
        let r = overseer
            .tools
            .get("crew.plan")
            .unwrap()
            .execute(json!({
                "result": judged(),
                "goal": "Speed up vellev's recall",
                "machine": "devbox",
                "tickets": [{"title": "Profile recall", "folder": "/home/dev/vellev"}],
            }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        let id: uuid::Uuid = r.output["proposed"]["plan_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let key = r.output["proposed"]["tickets"][0]["key"]
            .as_str()
            .unwrap()
            .to_string();

        // devbox has an old session of that key, from long ago, and the
        // person's own of it, just made; it answers no start: it makes the
        // session, but the answer is lost.
        let old = json!({
            "id": uuid::Uuid::new_v4(), "name": key, "cwd": "/home/dev/vellev",
            "state": "idle", "lastReply": "old work", "createdAt": "2026-01-01T00:00:00Z",
        });
        let persons = json!({
            "id": uuid::Uuid::new_v4(), "name": key, "cwd": "/home/dev/vellev",
            "state": "idle", "lastReply": null, "createdAt": chrono::Utc::now().to_rfc3339(),
        });
        let there: Arc<parking_lot::Mutex<Vec<Value>>> =
            Arc::new(parking_lot::Mutex::new(vec![old, persons]));
        let starts: Arc<parking_lot::Mutex<usize>> = Arc::default();
        let poller = {
            let (crew, there, starts) =
                (Arc::clone(&crew), Arc::clone(&there), Arc::clone(&starts));
            tokio::spawn(async move {
                loop {
                    for c in crew.relay_take(Duration::from_millis(100)).await {
                        match c.tool.as_str() {
                            "desk.sessions" => {
                                let body = Value::Array(there.lock().clone());
                                crew.relay_answer(
                                    c.id,
                                    RelayAnswer {
                                        status: ServedStatus::Ok,
                                        body,
                                    },
                                );
                            }
                            "desk.session_start" => {
                                *starts.lock() += 1;
                                // Under the id it was given, run by its
                                // boss but waiting on the person by now.
                                there.lock().push(json!({
                                    "id": c.input["id"], "name": c.input["name"],
                                    "cwd": c.input["cwd"], "state": "waiting", "lastReply": null,
                                    "createdAt": chrono::Utc::now().to_rfc3339(),
                                }));
                                // No answer.
                            }
                            // The work loop's and the pump's takes, which this
                            // machine doesn't answer.
                            super::super::relayed_work::WORK_TAKE
                            | super::super::relayed_decide::TAKE => {}
                            other => panic!("{other} relayed"),
                        }
                    }
                }
            })
        };

        d.approve_plan(id, 1, &By::you()).await.unwrap();
        let given = eventually("the session it made, taken up", async || {
            let p = d.plan(id).await.ok()?;
            let t = &p.tickets[0].ticket.ticket;
            t.assignee.as_ref().map(|a| a.id.clone())
        })
        .await;
        let made = there.lock()[2]["id"].as_str().unwrap().to_string();
        assert_eq!(
            given,
            format!("devbox/{made}"),
            "the one it made, run by its boss"
        );
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(*starts.lock(), 1, "made once");
        assert_eq!(
            d.plan(id).await.unwrap().tickets[0].ticket.ticket.state,
            TicketState::Doing
        );
        poller.abort();
    })
    .await;
}

#[tokio::test]
async fn a_machine_start_whose_retry_cant_read_there_keeps_its_link() {
    use super::super::relay::RelayAnswer;
    use super::super::served::ServedStatus;
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, _llm) = hub_with(dir.path(), pool, ACTS).await;
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        *crew.relay_wait.lock() = Duration::from_millis(400);
        eventually("the plan tools", async || {
            overseer.tools.has("crew.plan").then_some(())
        })
        .await;
        let r = overseer
            .tools
            .get("crew.plan")
            .unwrap()
            .execute(json!({
                "result": judged(),
                "goal": "Speed up vellev's recall",
                "machine": "devbox",
                "tickets": [{"title": "Profile recall", "folder": "/home/dev/vellev"}],
            }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        let id: uuid::Uuid = r.output["proposed"]["plan_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();

        // devbox makes the session but its answer is lost, and then its
        // sessions can't be read for a while.
        let there: Arc<parking_lot::Mutex<Vec<Value>>> = Arc::default();
        let (starts, reads, down): (
            Arc<parking_lot::Mutex<usize>>,
            Arc<parking_lot::Mutex<usize>>,
            Arc<parking_lot::Mutex<bool>>,
        ) = (Arc::default(), Arc::default(), Arc::default());
        let poller = {
            let (crew, there, starts, reads, down) = (
                Arc::clone(&crew),
                Arc::clone(&there),
                Arc::clone(&starts),
                Arc::clone(&reads),
                Arc::clone(&down),
            );
            tokio::spawn(async move {
                loop {
                    for c in crew.relay_take(Duration::from_millis(100)).await {
                        match c.tool.as_str() {
                            "desk.sessions" => {
                                *reads.lock() += 1;
                                let answer = if *down.lock() {
                                    RelayAnswer {
                                        status: ServedStatus::Refused,
                                        body: json!("devbox is busy"),
                                    }
                                } else {
                                    RelayAnswer {
                                        status: ServedStatus::Ok,
                                        body: Value::Array(there.lock().clone()),
                                    }
                                };
                                crew.relay_answer(c.id, answer);
                            }
                            "desk.session_start" => {
                                *starts.lock() += 1;
                                *down.lock() = true;
                                there.lock().push(json!({
                                    "id": uuid::Uuid::new_v4(), "name": c.input["name"],
                                    "cwd": c.input["cwd"], "state": "working", "lastReply": null,
                                    "bossHas": "desk",
                                    "createdAt": chrono::Utc::now().to_rfc3339(),
                                }));
                            }
                            // The work loop's and the pump's takes, which this
                            // machine doesn't answer.
                            super::super::relayed_work::WORK_TAKE
                            | super::super::relayed_decide::TAKE => {}
                            other => panic!("{other} relayed"),
                        }
                    }
                }
            })
        };

        d.approve_plan(id, 1, &By::you()).await.unwrap();
        eventually("retries that can't read devbox", async || {
            (*reads.lock() >= 3).then_some(())
        })
        .await;
        let t = d.plan(id).await.unwrap().tickets[0].ticket.ticket.clone();
        assert_eq!(t.state, TicketState::Doing, "still under way");
        assert!(t.assignee.is_none());
        assert_eq!(
            d.store().starting().await.unwrap().len(),
            1,
            "its link kept"
        );
        assert_eq!(*starts.lock(), 1, "made once");

        *down.lock() = false;
        let given = eventually("the session it made, taken up", async || {
            let p = d.plan(id).await.ok()?;
            p.tickets[0]
                .ticket
                .ticket
                .assignee
                .as_ref()
                .map(|a| a.id.clone())
        })
        .await;
        let made = there.lock()[0]["id"].as_str().unwrap().to_string();
        assert_eq!(given, format!("devbox/{made}"));
        assert_eq!(*starts.lock(), 1, "made once");
        poller.abort();
    })
    .await;
}

/// "Plan fixing the open bugs": the boss finds the person's tickets on the
/// board with crew.find_tickets, proposes a plan that takes two on as they
/// are beside a new one, the person approves it, and the runner starts a
/// session on the very ticket it took on, under its own key.
#[tokio::test(flavor = "multi_thread")]
async fn a_boss_finds_tickets_and_plans_on_them_as_they_are() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, _llm) = hub_with(dir.path(), pool, ACTS).await;
        let b = d
            .create_board(workforce_desk::NewBoard {
                name: "Mantis".into(),
                group: None,
                prefix: Some("MAN".into()),
            })
            .await
            .unwrap();
        let mut keys = Vec::new();
        for title in ["The cart empties on reload", "Login loops on Safari", "Old banner"] {
            let t = d
                .create_ticket(
                    workforce_desk::NewTicket {
                        board: b.id,
                        title: title.into(),
                        body: "Seen on the 17th.".into(),
                        state: Some(TicketState::Todo),
                        needs: vec![],
                    },
                    &By::you(),
                )
                .await
                .unwrap();
            keys.push(t.key.unwrap());
        }
        let _crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("the plan tools", async || {
            overseer.tools.has("crew.find_tickets").then_some(())
        })
        .await;
        let found = overseer
            .tools
            .get("crew.find_tickets")
            .unwrap()
            .execute(json!({ "board": "mantis", "q": "o", "state": ["todo"] }))
            .await
            .unwrap();
        assert!(found.success, "{found:?}");
        assert_eq!(found.output["found"], 3, "{:?}", found.output);
        assert_eq!(found.output["free"], 3);
        let r = overseer
            .tools
            .get("crew.plan")
            .unwrap()
            .execute(json!({ "result": judged(), "goal": "Fix the open bugs", "tickets": [
                { "adopt": keys[0], "folder": dir.path().to_string_lossy() },
                { "adopt": keys[1].to_lowercase(), "folder": dir.path().to_string_lossy(), "needs": [0] },
                { "title": "Write the release note", "folder": dir.path().to_string_lossy(), "needs": [1] },
            ]}))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        let proposed = &r.output["proposed"];
        assert_eq!(proposed["tickets"][0]["key"], keys[0]);
        assert_eq!(proposed["tickets"][0]["adopted"], true);
        assert_eq!(proposed["tickets"][2]["adopted"], false);
        let id: uuid::Uuid = proposed["plan_id"].as_str().unwrap().parse().unwrap();
        // Taken: the same ticket in another plan is refused by its key.
        let again = overseer
            .tools
            .get("crew.plan")
            .unwrap()
            .execute(json!({ "result": judged(), "goal": "Again", "tickets": [
                { "adopt": keys[0], "folder": dir.path().to_string_lossy() },
            ]}))
            .await
            .unwrap();
        assert!(!again.success);
        let said = again.error.unwrap_or_default();
        assert!(said.contains(&format!("{} is in another plan already", keys[0])), "{said}");
        // A title beside adopt is refused: its words are the ticket's.
        let both = overseer
            .tools
            .get("crew.plan")
            .unwrap()
            .execute(json!({ "result": judged(), "goal": "Again", "tickets": [
                { "adopt": keys[2], "title": "Mine", "folder": dir.path().to_string_lossy() },
            ]}))
            .await
            .unwrap();
        assert!(!both.success);

        d.approve_plan(id, 1, &By::you()).await.unwrap();
        let first = eventually("its first ticket's session", async || {
            d.list().into_iter().find(|v| v.name == keys[0])
        })
        .await;
        // Its ticket is given it as the start ends: a moment after it shows.
        let t = eventually("its ticket given that session", async || {
            let t = d.ticket_by_key(&keys[0]).await.ok()?;
            (t.ticket.session() == Some(first.id)).then_some(t)
        })
        .await;
        assert_eq!(t.ticket.title, "The cart empties on reload", "as it was");
        // Nothing copied: the board has its three and the plan's new one.
        assert_eq!(d.tickets(b.id, &Default::default()).await.unwrap().len(), 4);
        assert_eq!(proposed["tickets"][2]["key"], "MAN-4");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_boss_runs_a_check_on_a_machine_and_what_no_boss_does_is_the_persons() {
    use super::super::relay::RelayAnswer;
    use super::super::served::ServedStatus;
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, _llm) = hub_with(dir.path(), pool, ACTS).await;
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("the run tool", async || overseer.tools.has("crew.machine_run").then_some(())).await;
        let run = overseer.tools.get("crew.machine_run").unwrap();
        let said = |r: workforce_agent::tools::ToolResult| format!("{r:?}");
        // What it must say, and what it may not.
        for (input, says) in [
            (json!({ "machine": "devbox", "command": "ls" }), "Give the reason"),
            (json!({ "machine": "devbox", "reason": "Look." }), "Say the command"),
            (json!({ "machine": "../x", "command": "ls", "reason": "Look." }), "isn't one of the person's machines"),
            (json!({ "machine": "devbox", "command": "ls", "timeout_secs": 60, "reason": "Look." }), "1 to 15 seconds"),
            (json!({ "machine": "devbox", "command": "x".repeat(9000), "reason": "Look." }), "at most"),
        ] {
            let r = run.execute(input).await.unwrap();
            assert!(!r.success && said(r.clone()).contains(says), "{says}: {r:?}");
        }
        // What no boss does is the person's: a suggestion, never sent.
        let r = run
            .execute(json!({ "machine": "devbox", "command": "rm -rf ~/old-checkouts", "reason": "Tidy up." }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        assert!(r.output["suggested"].as_str().unwrap().contains("Run on devbox"), "{r:?}");
        let s = d.store().suggestions(10).await.unwrap();
        let held = s.iter().find(|s| s.action["tool"] == "crew.machine_run").unwrap();
        assert_eq!(
            held.action["input"]["agent_id"],
            json!(overseer.agent_id.as_str()),
            "the boss whose check it is, kept with it"
        );
        // A plain check goes to the machine; with nothing carrying calls in a
        // test, it is said, never waited on, and noted in Activity.
        let r = run
            .execute(json!({ "machine": "devbox", "command": "git remote -v", "cwd": "~/projects/vellev", "reason": "Which remote." }))
            .await
            .unwrap();
        assert!(said(r).contains("can't be reached"));
        eventually("the check in Activity", async || {
            let page = d
                .activity(workforce_desk::crew::Filter::All, None, 50)
                .await
                .unwrap();
            serde_json::to_string(&page)
                .unwrap()
                .contains("couldn't send `git remote -v` to devbox")
                .then_some(())
        })
        .await;

        // Desk carrying calls: a machine that runs what's read, needs the
        // person for a project's tests, and loses the answer to a delete.
        *crew.relay_wait.lock() = Duration::from_secs(5);
        let sent: Arc<parking_lot::Mutex<Vec<Value>>> = Arc::default();
        let _poller = {
            let (crew, sent) = (Arc::clone(&crew), Arc::clone(&sent));
            tokio::spawn(async move {
                loop {
                    for c in crew.relay_take(Duration::from_millis(100)).await {
                        // Any other tool is said in what was sent, for the
                        // test to fail on.
                        sent.lock().push(match c.tool.as_str() {
                            "desk.run" => c.input.clone(),
                            other => json!({ "tool": other }),
                        });
                        let command = c.input["command"].as_str().unwrap_or_default().to_string();
                        let body = match (command.as_str(), c.input["by_person"].as_bool()) {
                            (c, _) if c.starts_with("rm ") => continue,
                            ("npm test", Some(false)) => {
                                json!({"outcome": "needs_person", "why": "runs a project's own code"})
                            }
                            _ => json!({"outcome": "ran", "cwd": "/home/dev", "exit": 0, "timed_out": false, "stdout": "clean\n", "stderr": "", "truncated": false}),
                        };
                        crew.relay_answer(c.id, RelayAnswer { status: ServedStatus::Ok, body });
                    }
                }
            })
        };
        eventually("Desk polling", async || crew.relay.polled_ever().then_some(())).await;
        // Read: run, judged there at Act.
        let r = run
            .execute(json!({ "machine": "devbox", "command": "git status", "reason": "Is it clean?" }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        assert!(r.output["ran"].as_str().unwrap().contains("clean"), "{r:?}");
        let first = sent.lock().last().cloned().unwrap();
        assert_eq!(first["judged"], true);
        assert_eq!(first["by_person"], false);
        // What the judge there won't allow: the person's, as a suggestion.
        let r = run
            .execute(json!({ "machine": "devbox", "command": "npm test", "cwd": "~/projects/vellev", "reason": "Do they pass?" }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        assert!(r.output["suggested"].as_str().unwrap().contains("`npm test`"), "{r:?}");
        let tests = d
            .store()
            .suggestions(10)
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.action["input"]["command"] == "npm test")
            .unwrap();
        // The person says yes: run on their word, unjudged.
        crew.accept(&tests).await.unwrap();
        let theirs = sent.lock().last().cloned().unwrap();
        assert_eq!(theirs["by_person"], true);
        assert_eq!(theirs["judged"], false);
        assert!(sent.lock().iter().all(|s| s.get("tool").is_none()), "{:?}", sent.lock());
        // Its answer lost: the boss is told it may have run; the person's
        // yes to the delete is done with, never left to be run again.
        *crew.relay_wait.lock() = Duration::from_millis(400);
        let r = run
            .execute(json!({ "machine": "devbox", "command": "rm -f ~/x", "reason": "Tidy." }))
            .await
            .unwrap();
        assert!(!r.success && said(r.clone()).contains("may have run"), "{r:?}");
        crew.accept(held).await.unwrap();
        eventually("the delete in Activity, maybe run", async || {
            let page = d
                .activity(workforce_desk::crew::Filter::All, None, 50)
                .await
                .unwrap();
            serde_json::to_string(&page)
                .unwrap()
                .contains("may have run `rm -rf ~/old-checkouts` on devbox")
                .then_some(())
        })
        .await;
    })
    .await;
}

/// crew.edit_ticket on a ticket of the boss's own open plan changes it
/// through the plan, as crew.plan_update's tickets_edit does: where it runs
/// moves, words added keep what its brief said.
#[tokio::test(flavor = "multi_thread")]
async fn editing_a_plans_ticket_goes_through_its_plan() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, _llm) = hub_with(dir.path(), pool, ACTS).await;
        let _crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("the ticket tools", async || {
            overseer.tools.has("crew.edit_ticket").then_some(())
        })
        .await;
        let folder = dir.path().to_string_lossy().into_owned();
        let made = overseer
            .tools
            .get("crew.plan")
            .unwrap()
            .execute(json!({ "result": judged(), "goal": "Faster reads", "tickets": [
                { "title": "Add the cache", "body": "Cache the index.", "folder": folder },
                { "title": "Use the cache", "folder": folder },
            ] }))
            .await
            .unwrap();
        let plan = &made.output["proposed"];
        let id: uuid::Uuid = plan["plan_id"].as_str().unwrap().parse().unwrap();
        let key = plan["tickets"][0]["key"].as_str().unwrap().to_string();
        let edit = overseer.tools.get("crew.edit_ticket").unwrap();
        let r = edit
            .execute(json!({ "ticket": key, "machine": "devbox",
                "folder": "/home/dev/projects/driftwerk/vellev", "reason": "The person wants it on devbox." }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        assert!(
            r.output["note"].as_str().unwrap_or_default().contains("went through the plan"),
            "{r:?}"
        );
        let p = d.plan(id).await.unwrap();
        assert_eq!(p.tickets[0].machine.as_deref(), Some("devbox"));
        assert_eq!(p.tickets[0].ticket.ticket.title, "Add the cache");
        let r = edit
            .execute(json!({ "ticket": key, "append": "Keep it under 50 MB.", "reason": "The person said." }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        // Read back as its plan says it now, applied or waiting for the person.
        let read = overseer
            .tools
            .get("crew.ticket")
            .unwrap()
            .execute(json!({ "ticket": key }))
            .await
            .unwrap();
        assert!(read.success, "{read:?}");
        assert_eq!(read.output["plan"]["machine"], "devbox", "{read:?}");
        assert_eq!(read.output["plan"]["yours"], true);
        // Its brief as the plan says it now: applied, or waiting, with what
        // was added after what it said.
        let p = d.plan(id).await.unwrap();
        let said = read.output["plan"]["brief_waiting"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| p.tickets[0].ticket.ticket.body.clone());
        assert!(said.contains("Cache the index.\n\nKeep it under 50 MB."), "{said}");
        // What a plan's tickets need, and taking one out, are the plan's.
        let other = plan["tickets"][1]["key"].as_str().unwrap().to_string();
        for (tool, input, says) in [
            ("crew.link_ticket", json!({ "ticket": other, "needs": key, "reason": "x" }), "crew.plan_update"),
            ("crew.delete_ticket", json!({ "ticket": key, "reason": "x" }), "through the plan"),
        ] {
            let r = overseer.tools.get(tool).unwrap().execute(input).await.unwrap();
            assert!(!r.success, "{tool}: {r:?}");
            assert!(r.error.as_deref().unwrap_or_default().contains(says), "{tool}: {r:?}");
        }
    })
    .await;
}

/// A Claude Code that says its version, and whose sessions say why they
/// can't start and end at once.
fn broken_cli(dir: &Path) -> PathBuf {
    cli_broken_while(dir, "true")
}

/// A Claude Code whose sessions run on, except while `broken` holds: then
/// they say why they can't start and end at once.
fn cli_broken_while(dir: &Path, broken: &str) -> PathBuf {
    let path = dir.join("broken-claude");
    workforce_common::executable::write(
        &path,
        format!(
            "#!/bin/sh\ncase \"$1\" in --version) echo '2.1.300 (Claude Code)'; exit 0;; auth) exit 1;; esac\n\
             if {broken}; then echo 'error: settings.json is not valid JSON'; exit 1; fi\nexec sleep 600\n"
        ),
    )
    .unwrap();
    path
}

/// A plan's ticket whose session here ends as it starts is back in To do,
/// held back: its boss is told once, with its exit and what it said, and
/// its plan starts it no more by itself. Moved on and back, it starts.
#[tokio::test(flavor = "multi_thread")]
async fn a_ticket_whose_session_here_ends_as_it_starts_waits_in_to_do_its_boss_told_once() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, state, d, llm) =
            hub_running(dir.path(), pool, ACTS, broken_cli(dir.path())).await;
        let p = proposed(&d, dir.path(), 1).await;
        let id = p.summary.plan.id;
        d.approve_plan(id, 1, &By::you()).await.unwrap();
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        let held = eventually("the ticket held back", async || {
            let t = d.plan(id).await.ok()?.tickets.remove(0);
            (t.ticket.ticket.state == TicketState::Todo).then_some(t.held_back?)
        })
        .await;
        assert_eq!(held, "its session ended as it started (exit 1)");
        let told = "DES-1's session on this computer ended as it started (exit 1). \
                    Its last output: “error: settings.json is not valid JSON” DES-1 is back in To do";
        eventually("the boss told", async || {
            (newly_said(&llm, told).await >= 1).then_some(())
        })
        .await;
        // Runner passes since the hold start nothing, so nothing more ends.
        for _ in 0..3 {
            crew.run_plans().await;
        }
        assert_eq!(d.list().iter().filter(|v| v.name == "DES-1").count(), 1, "never started again");
        assert_eq!(newly_said(&llm, "ended as it started").await, 1, "once");
        let p = d.plan(id).await.unwrap();
        assert_eq!(p.summary.progress.blocked, 1);
        assert!(p.summary.progress.next.is_empty());

        // Moved on and back by the person, its plan starts it again.
        let ticket = p.tickets[0].ticket.ticket.id;
        for state in [TicketState::Backlog, TicketState::Todo] {
            let mv = workforce_desk::Move {
                state,
                before: None,
                after: None,
            };
            d.move_ticket(ticket, mv, &By::you()).await.unwrap();
        }
        eventually("started again", async || {
            (d.list().iter().filter(|v| v.name == "DES-1").count() == 2).then_some(())
        })
        .await;
    })
    .await;
}

/// A session that ends as it starts before its plan gives it its ticket:
/// the ticket, given to no one yet, is held back all the same, its boss
/// told once.
#[tokio::test(flavor = "multi_thread")]
async fn a_ticket_whose_session_ends_before_it_is_given_it_waits_in_to_do_its_boss_told_once() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let broken = dir.path().join("broken");
        let cli = cli_broken_while(dir.path(), &format!("[ -f '{}' ]", broken.display()));
        let (_overseer, state, d, llm) = hub_running(dir.path(), pool, ACTS, cli).await;
        let ticket = |title: &str, needs: Vec<usize>| DraftTicket {
            title: title.into(),
            folder: dir.path().to_string_lossy().into_owned(),
            needs,
            ..Default::default()
        };
        let p = d
            .propose_plan(
                NewPlan {
                    draft: Draft {
                        goal: "Make the suite green".into(),
                        tickets: vec![ticket("Fix it", vec![]), ticket("Then this", vec![0])],
                    },
                    board: None,
                    parallel: None,
                    engine: Engine::Claude,
                    group: None,
                    budget: None,
                    result: None,
                    goal_ticket: None,
                },
                &Party::manager("desk", "Desk"),
                &By::manager("desk", "Desk", "Sam asked for a green suite."),
            )
            .await
            .unwrap();
        let id = p.summary.plan.id;
        let then = p.tickets[1].ticket.ticket.id;
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        d.approve_plan(id, 1, &By::you()).await.unwrap();
        eventually("its first ticket started", async || {
            d.list().iter().any(|v| v.name == "DES-1").then_some(())
        })
        .await;

        // DES-2's session, between its start and its ticket given it.
        let s = new_session(&d, dir.path(), "DES-2").await;
        d.stop(s.id, Some(By::you())).unwrap();
        eventually("it stopped", async || {
            (!d.view(s.id).ok()?.state.is_running()).then_some(())
        })
        .await;
        let mut t = d.ticket(then).await.unwrap().ticket;
        let rank = t.rank;
        d.store()
            .begin_start(&mut t, id, rank, s.id, chrono::Utc::now())
            .await
            .unwrap();
        std::fs::write(&broken, "").unwrap();
        d.resume(s.id, None, Some(By::you())).await.unwrap();

        let held = eventually("the ticket held back", async || {
            let t = d.plan(id).await.ok()?.tickets.remove(1);
            (t.ticket.ticket.state == TicketState::Todo).then_some(t.held_back?)
        })
        .await;
        assert_eq!(held, "its session ended as it started (exit 1)");
        assert_eq!(d.ticket(then).await.unwrap().ticket.assignee, None, "never given it");
        let told = "DES-2's session on this computer ended as it started (exit 1). \
                    Its last output: “error: settings.json is not valid JSON” DES-2 is back in To do";
        eventually("the boss told", async || {
            (newly_said(&llm, told).await >= 1).then_some(())
        })
        .await;
        for _ in 0..3 {
            crew.run_plans().await;
        }
        assert_eq!(newly_said(&llm, "ended as it started").await, 1, "once");
    })
    .await;
}

/// The same of one on a machine, as its Desk says it ended; a plain turn's
/// end there still reads as one.
#[tokio::test(flavor = "multi_thread")]
async fn a_ticket_whose_session_on_a_machine_ends_as_it_starts_waits_in_to_do_its_boss_told_once() {
    use super::super::relay::RelayAnswer;
    use super::super::served::ServedStatus;
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, llm) = hub_with(dir.path(), pool, ACTS).await;
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("the plan tools", async || {
            overseer.tools.has("crew.plan").then_some(())
        })
        .await;
        let there: Arc<parking_lot::Mutex<Vec<Value>>> = Arc::default();
        let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let poller = {
            let (crew, there, starts, reads) = (
                Arc::clone(&crew),
                Arc::clone(&there),
                Arc::clone(&starts),
                Arc::clone(&reads),
            );
            tokio::spawn(async move {
                loop {
                    for c in crew.relay_take(Duration::from_millis(200)).await {
                        let body = match c.tool.as_str() {
                            "desk.sessions" => {
                                reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                Value::Array(there.lock().clone())
                            }
                            "desk.session_start" => {
                                starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                let s = json!({
                                    "id": uuid::Uuid::new_v4(), "name": c.input["name"],
                                    "cwd": c.input["cwd"], "state": "starting", "lastReply": null,
                                });
                                there.lock().push(s.clone());
                                s
                            }
                            // The work loop's and the pump's takes, which this
                            // machine doesn't answer.
                            super::super::relayed_work::WORK_TAKE
                            | super::super::relayed_decide::TAKE => Value::Null,
                            other => panic!("{other} relayed"),
                        };
                        crew.relay_answer(
                            c.id,
                            RelayAnswer {
                                status: ServedStatus::Ok,
                                body,
                            },
                        );
                    }
                }
            })
        };
        let r = overseer
            .tools
            .get("crew.plan")
            .unwrap()
            .execute(json!({
                "result": judged(),
                "goal": "Speed up vellev's recall",
                "machine": "devbox",
                "tickets": [
                    {"title": "Profile recall", "folder": "/home/dev/vellev"},
                    {"title": "Cache recall", "folder": "/home/dev/vellev-cache"},
                ],
            }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        let id: uuid::Uuid = r.output["proposed"]["plan_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        d.approve_plan(id, 1, &By::you()).await.unwrap();
        eventually("both started on devbox", async || {
            (there.lock().len() == 2).then_some(())
        })
        .await;
        let key_of = |i: usize| there.lock()[i]["name"].as_str().unwrap().to_string();
        let (worked, died) = (key_of(0), key_of(1));

        // One ends its turn as ever; the other ends as it starts.
        {
            let mut on = there.lock();
            on[0]["state"] = json!("idle");
            on[0]["lastReply"] = json!("Profiled.");
            on[1]["state"] = json!("interrupted");
            on[1]["exited"] = json!({
                "code": 127, "lastOutput": "Starting\nclaude: command not found",
                "at": chrono::Utc::now(), "onStart": true,
            });
        }
        let told = format!(
            "{died}'s session on devbox ended as it started (exit 127). \
             Its last output: “Starting / claude: command not found” {died} is back in To do"
        );
        eventually("the boss told", async || {
            (newly_said(&llm, &told).await >= 1).then_some(())
        })
        .await;
        eventually("a plain turn told as ever", async || {
            (newly_said(
                &llm,
                &format!("{worked}'s session on devbox finished a turn"),
            )
            .await
                >= 1)
                .then_some(())
        })
        .await;
        let held = eventually("the ticket held back", async || {
            let p = d.plan(id).await.ok()?;
            let t = p
                .tickets
                .into_iter()
                .find(|t| t.ticket.key.as_deref() == Some(&died))?;
            (t.ticket.ticket.state == TicketState::Todo && t.ticket.ticket.assignee.is_none())
                .then_some(t.held_back?)
        })
        .await;
        assert_eq!(held, "its session ended as it started (exit 127)");
        // Devbox read again since, its session still ended as it started:
        // nothing more said, and nothing started.
        let after = reads.load(std::sync::atomic::Ordering::SeqCst);
        eventually("devbox read twice more", async || {
            (reads.load(std::sync::atomic::Ordering::SeqCst) >= after + 2).then_some(())
        })
        .await;
        assert_eq!(
            starts.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "never started again"
        );
        assert_eq!(newly_said(&llm, "ended as it started").await, 1, "once");
        poller.abort();
    })
    .await;
}

/// What a home's watcher asks of a machine: `desk.sessions` lists its open
/// sessions and says, of the ids it is asked about, those closed or removed
/// there, by id alone.
#[tokio::test(flavor = "multi_thread")]
async fn desk_sessions_says_which_of_the_ids_asked_about_were_closed_or_removed_there() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, _state, d, _llm) = hub_with(dir.path(), pool, ACTS).await;
        let open = new_session(&d, dir.path(), "open").await.id;
        let shut = new_session(&d, dir.path(), "shut").await.id;
        d.end(shut, Some(By::you())).await.unwrap();
        let removed = uuid::Uuid::new_v4();
        let tool = crate::desk_tools::DeskTool::all(&d, "desk")
            .into_iter()
            .find(|t| workforce_agent::tools::Tool::name(t) == "desk.sessions")
            .unwrap();
        let out = workforce_agent::runner::CURRENT_AGENT_ID
            .scope("desk".to_string(), async move {
                workforce_agent::tools::Tool::execute(
                    &tool,
                    json!({ "ids": [open, shut, removed] }),
                )
                .await
                .unwrap()
            })
            .await
            .output;
        let listed: Vec<workforce_desk::crew::MachineSession> =
            serde_json::from_value(out).unwrap();
        let closed: Vec<uuid::Uuid> = listed
            .iter()
            .filter(|s| s.state == "closed")
            .map(|s| s.id)
            .collect();
        assert_eq!(closed, [shut, removed]);
        assert!(listed.iter().all(|s| s.id != open || s.state != "closed"));
    })
    .await;
}

/// The plan keeps N working (DW-761): a ticket in Review whose session
/// rests holds no slot, so the next ticket starts; its session resumed
/// brings the plan to N+1 with nothing stopped and no new start; once its
/// ticket is done, the plan watch closes the session.
#[tokio::test(flavor = "multi_thread")]
async fn a_resting_review_frees_its_slot_a_resumed_one_is_n_plus_one_and_a_done_one_closes() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, state, d, _llm) = hub_with(dir.path(), pool, ACTS).await;
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        *crew.close_settle.lock() = chrono::Duration::zero();
        let p = proposed(&d, dir.path(), 6).await;
        let id = p.summary.plan.id;
        d.approve_plan(id, 1, &By::you()).await.unwrap();
        let four = eventually("four sessions", async || {
            let s = started(&d);
            (s.len() == 4).then_some(s)
        })
        .await;
        let one = four[0].id;
        let url = hook_url(dir.path(), one).await;
        hook(
            &url,
            json!({"hook_event_name": "SessionStart", "session_id": one.to_string()}),
        )
        .await;
        eventually("its turn over", async || {
            (!d.view(one).unwrap().state.busy()).then_some(())
        })
        .await;
        let ticket = d
            .plan_tickets(id)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.session.as_deref() == Some(&one.to_string()))
            .unwrap()
            .id;
        let mv = |state| workforce_desk::Move {
            state,
            before: None,
            after: None,
        };
        d.move_ticket(ticket, mv(TicketState::Review), &By::you())
            .await
            .unwrap();
        eventually("the fifth starts", async || {
            (started(&d).len() == 5).then_some(())
        })
        .await;
        assert_eq!(held_now(&d, id).await, 4, "the resting review holds none");

        hook(
            &url,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "address the review"}),
        )
        .await;
        eventually("the review at work again", async || {
            d.view(one).unwrap().state.busy().then_some(())
        })
        .await;
        eventually("N+1 held", async || {
            (held_now(&d, id).await == 5).then_some(())
        })
        .await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        let open: Vec<_> = started(&d)
            .into_iter()
            .filter(|v| v.state != SessionState::Closed)
            .collect();
        assert_eq!(open.len(), 5, "nothing stopped and nothing new started");

        // Another, its turn over and its ticket done, is closed by the
        // plan watch.
        let two = four[1].id;
        hook(
            &hook_url(dir.path(), two).await,
            json!({"hook_event_name": "SessionStart", "session_id": two.to_string()}),
        )
        .await;
        eventually("its turn over", async || {
            (!d.view(two).unwrap().state.busy()).then_some(())
        })
        .await;
        let done = d
            .plan_tickets(id)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.session.as_deref() == Some(&two.to_string()))
            .unwrap()
            .id;
        d.move_ticket(done, mv(TicketState::Done), &By::you())
            .await
            .unwrap();
        eventually("the plan watch closes it", async || {
            d.view(two)
                .map_or(true, |v| v.state == SessionState::Closed)
                .then_some(())
        })
        .await;
    })
    .await;
}

/// devbox, as Desk's native side carries to it: no sessions, and every
/// start refused with `refusal`; when each start came.
fn refusing_devbox(
    crew: &Arc<CrewHub>,
    refusal: Value,
) -> (
    tokio::task::JoinHandle<()>,
    Arc<parking_lot::Mutex<Vec<std::time::Instant>>>,
) {
    use super::super::relay::RelayAnswer;
    use super::super::served::ServedStatus;
    let starts: Arc<parking_lot::Mutex<Vec<std::time::Instant>>> = Arc::default();
    let (crew, seen) = (Arc::clone(crew), Arc::clone(&starts));
    let poller = tokio::spawn(async move {
        loop {
            for c in crew.relay_take(Duration::from_millis(100)).await {
                let answer = match c.tool.as_str() {
                    "desk.sessions" => RelayAnswer {
                        status: ServedStatus::Ok,
                        body: json!([]),
                    },
                    "desk.session_start" => {
                        seen.lock().push(std::time::Instant::now());
                        RelayAnswer {
                            status: ServedStatus::Refused,
                            body: refusal.clone(),
                        }
                    }
                    super::super::relayed_work::WORK_TAKE | super::super::relayed_decide::TAKE => {
                        continue
                    }
                    other => panic!("{other} relayed"),
                };
                crew.relay_answer(c.id, answer);
            }
        }
    });
    (poller, starts)
}

/// A running plan of one ticket on devbox, proposed by the boss and
/// approved: its id and its ticket's key.
async fn approved_on_devbox(overseer: &AgentService, d: &Desk) -> (uuid::Uuid, String) {
    eventually("the plan tools", async || {
        overseer.tools.has("crew.plan").then_some(())
    })
    .await;
    let r = overseer
        .tools
        .get("crew.plan")
        .unwrap()
        .execute(json!({
            "result": judged(),
            "goal": "Fix the CRM's bugs",
            "machine": "devbox",
            "tickets": [{"title": "Fix the login race", "folder": "/home/dev/crm"}],
        }))
        .await
        .unwrap();
    assert!(r.success, "{r:?}");
    let id: uuid::Uuid = r.output["proposed"]["plan_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let key = r.output["proposed"]["tickets"][0]["key"]
        .as_str()
        .unwrap()
        .to_string();
    d.approve_plan(id, 1, &By::you()).await.unwrap();
    (id, key)
}

async fn paused_failing(d: &Desk, id: uuid::Uuid) -> String {
    eventually("the plan paused", async || {
        let p = d.plan(id).await.unwrap();
        (p.summary.plan.paused == Some(workforce_desk::plan::Paused::StartFailing))
            .then_some(p.summary.paused_why)
            .flatten()
    })
    .await
}

/// A ticket whose worktree on its machine is held by something else isn't
/// tried again: its plan pauses at once saying why, once, for the person to
/// continue when it is put right, and its boss is told once.
#[tokio::test(flavor = "multi_thread")]
async fn a_machine_ticket_whose_worktree_there_is_taken_pauses_its_plan_once() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, llm) = hub_with(dir.path(), pool, ACTS).await;
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        let held = "/home/dev/crm/.worktrees/des-1 is there already on someone-else, not des-1";
        let (poller, starts) = refusing_devbox(
            &crew,
            json!({ "error": held, "code": workforce_desk::worktree::TAKEN }),
        );
        let (id, key) = approved_on_devbox(&overseer, &d).await;
        let why = paused_failing(&d, id).await;
        assert_eq!(why, format!("{key} can't start on devbox: {held}"));
        assert!(d.plan_needs().iter().any(|n| n.ask.contains(&why)));
        let told = format!("{key} didn't start on devbox: {held}");
        eventually("the boss told", async || {
            (said(&llm, &told).await >= 1).then_some(())
        })
        .await;
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert_eq!(starts.lock().len(), 1, "never tried again by itself");
        assert_eq!(said(&llm, &told).await, 1);
        assert_eq!(
            d.plan(id).await.unwrap().tickets[0].ticket.ticket.state,
            TicketState::Todo
        );
        // Continued with it still taken, it is tried once more and pauses
        // again: no loop.
        let revision = d.plan(id).await.unwrap().summary.plan.revision;
        d.decide_plan(id, revision, true, None, &By::you())
            .await
            .unwrap();
        eventually("tried once more", async || {
            (starts.lock().len() == 2).then_some(())
        })
        .await;
        assert_eq!(paused_failing(&d, id).await, why);
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert_eq!(starts.lock().len(), 2, "not tried again by itself");
        poller.abort();
    })
    .await;
}

/// A machine ticket whose starts keep being refused is tried again later
/// each time, and its plan pauses at the fifth.
#[tokio::test(flavor = "multi_thread")]
async fn a_machine_ticket_that_keeps_failing_to_start_backs_off_and_pauses() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, _llm) = hub_with(dir.path(), pool, ACTS).await;
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        let (poller, starts) = refusing_devbox(&crew, json!({ "error": "the disk is full" }));
        let (id, key) = approved_on_devbox(&overseer, &d).await;
        let why = paused_failing(&d, id).await;
        assert!(
            why.starts_with(&format!("{key} couldn't start 5 times:"))
                && why.contains("the disk is full"),
            "{why}"
        );
        let at = starts.lock().clone();
        assert_eq!(at.len(), 5, "{at:?}");
        for (n, pair) in at.windows(2).enumerate() {
            let waited = pair[1] - pair[0];
            assert!(
                waited >= FAST.minute * 2u32.pow(n as u32),
                "the wait before start {} was {waited:?}",
                n + 2
            );
        }
        poller.abort();
    })
    .await;
}

/// The boss's changes of its plan, as it hears of a ticket failing to
/// start, don't count its failures afresh: it still pauses at the fifth.
#[tokio::test(flavor = "multi_thread")]
async fn a_bosss_changes_between_failures_still_pause_at_the_fifth() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, _llm) = hub_with(dir.path(), pool, ACTS).await;
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        let (poller, starts) = refusing_devbox(&crew, json!({ "error": "the disk is full" }));
        let (id, key) = approved_on_devbox(&overseer, &d).await;
        for n in 1..=3 {
            eventually("a failed start", async || {
                (starts.lock().len() >= n).then_some(())
            })
            .await;
            d.change_plan(
                id,
                workforce_desk::PlanEdit {
                    goal: Some(format!("Fix the CRM's bugs, take {n}")),
                    ..Default::default()
                },
                Engine::Claude,
                false,
                &By::manager("desk", "Desk", "It didn't start."),
            )
            .await
            .unwrap();
        }
        let why = paused_failing(&d, id).await;
        assert!(
            why.starts_with(&format!("{key} couldn't start 5 times:")),
            "{why}"
        );
        assert_eq!(starts.lock().len(), 5);
        poller.abort();
    })
    .await;
}

/// A restart of the hub keeps how often a ticket failed to start: its plan
/// still pauses at the fifth, not five more.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_keeps_how_often_a_ticket_failed_to_start() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, _llm) = hub_with(dir.path(), pool, ACTS).await;
        let first_cancel = CancellationToken::new();
        let first = CrewHub::start(state.clone(), d.clone(), FAST, first_cancel.clone());
        let refusal = json!({ "error": "the disk is full" });
        let (poller, before) = refusing_devbox(&first, refusal.clone());
        let (id, key) = approved_on_devbox(&overseer, &d).await;
        eventually("two failed starts", async || {
            (before.lock().len() >= 2).then_some(())
        })
        .await;
        first_cancel.cancel();
        tokio::time::sleep(Duration::from_millis(500)).await;
        poller.abort();
        // What was kept: a start cut off by the restart, its answer never
        // read, isn't counted.
        let kept = d
            .store()
            .state("hub.crew.plan_failed")
            .await
            .unwrap()
            .expect("kept")
            .value;
        let counted = kept
            .as_object()
            .unwrap()
            .values()
            .map(|f| f["count"].as_u64().unwrap())
            .sum::<u64>() as usize;
        assert!(counted >= 2, "{kept}");
        // A clock that jumped (a next try a century off) waits no longer
        // than its wait; a plan gone since is forgotten.
        let mut kept = kept;
        for f in kept.as_object_mut().unwrap().values_mut() {
            f["next"] = json!("2126-01-01T00:00:00Z");
        }
        let mut gone = kept.as_object().unwrap().values().next().unwrap().clone();
        gone["plan"] = json!(uuid::Uuid::new_v4());
        let gone_ticket = uuid::Uuid::new_v4().to_string();
        kept[&gone_ticket] = gone;
        d.store()
            .put_state("hub.crew.plan_failed", &kept)
            .await
            .unwrap();
        let second = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        let (poller, after) = refusing_devbox(&second, refusal);
        let why = paused_failing(&d, id).await;
        assert!(
            why.starts_with(&format!("{key} couldn't start 5 times:")),
            "{why}"
        );
        assert_eq!(
            after.lock().len(),
            5 - counted,
            "{counted} kept over the restart"
        );
        let now = d
            .store()
            .state("hub.crew.plan_failed")
            .await
            .unwrap()
            .expect("kept")
            .value;
        assert!(now.get(&gone_ticket).is_none(), "{now}");
        poller.abort();
    })
    .await;
}
