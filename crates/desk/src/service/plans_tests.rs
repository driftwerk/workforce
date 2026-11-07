//! Plans end to end in the desk: proposed into Backlog, approved into To
//! do, changed by revision, started once however a start is cut off, and
//! dropped.

use std::path::Path;

use workforce_postgres::test_support::with_test_pool;

use super::super::tests::desk;
use super::*;
use crate::floor::{Answer, DecideRef};
use crate::store::build_pool;

fn sarah() -> Party {
    Party::manager("sarah", "Sarah")
}

fn by_sarah() -> By {
    By::manager("sarah", "Sarah", "The person asked for the checkout.")
}

fn draft(dir: &Path, titles: &[(&str, &[usize])]) -> Draft {
    Draft {
        goal: "Ship the new checkout".into(),
        tickets: titles
            .iter()
            .map(|(title, needs)| DraftTicket {
                id: None,
                title: (*title).into(),
                body: String::new(),
                engine: None,
                folder: dir.to_string_lossy().into_owned(),
                view: None,
                needs: needs.to_vec(),
            })
            .collect(),
    }
}

fn new_plan(dir: &Path, titles: &[(&str, &[usize])]) -> NewPlan {
    NewPlan {
        draft: draft(dir, titles),
        board: None,
        parallel: None,
        engine: Engine::Claude,
        group: None,
    }
}

/// The draft that keeps `p`'s tickets as they are, by id, in order.
fn kept(p: &PlanDetail) -> Vec<DraftTicket> {
    let ids: Vec<Uuid> = p.tickets.iter().map(|t| t.ticket.ticket.id).collect();
    p.tickets
        .iter()
        .map(|t| DraftTicket {
            id: Some(t.ticket.ticket.id),
            title: t.ticket.ticket.title.clone(),
            body: t.ticket.ticket.body.clone(),
            engine: t.engine.map(|e| e.as_str().to_string()),
            folder: t.folder.clone().unwrap_or_default(),
            view: None,
            needs: t
                .ticket
                .ticket
                .needs
                .iter()
                .filter_map(|n| ids.iter().position(|i| i == n))
                .collect(),
        })
        .collect()
}

fn states(p: &PlanDetail) -> Vec<TicketState> {
    p.tickets.iter().map(|t| t.ticket.ticket.state).collect()
}

#[tokio::test]
async fn a_proposed_plan_waits_in_backlog_for_the_persons_approval() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let p = d
            .propose_plan(
                new_plan(
                    dir.path(),
                    &[("Add the cart API", &[]), ("Wire the form", &[0])],
                ),
                &sarah(),
                &by_sarah(),
            )
            .await
            .unwrap();
        let plan = &p.summary.plan;
        assert_eq!(plan.state, PlanState::Proposed);
        assert_eq!((plan.revision, plan.approved_revision), (1, None));
        assert_eq!(states(&p), vec![TicketState::Backlog; 2]);
        assert_eq!(p.waves.len(), 2);
        assert_eq!(
            p.tickets[1].ticket.ticket.needs,
            vec![p.tickets[0].ticket.ticket.id]
        );
        assert_eq!(p.tickets[0].engine, Some(Engine::Claude));
        assert_eq!(p.revisions.len(), 1);
        assert_eq!(p.summary.progress.waiting, 2);
        // A board was made for it, and the person is asked.
        let b = d.boards().await.unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(p.tickets[0].ticket.key.as_deref(), Some("DES-1"));
        let needs = d.plan_needs();
        assert_eq!(needs.len(), 1);
        assert_eq!(needs[0].id, format!("plan:{}", plan.id));
        assert_eq!(
            needs[0].answer,
            Answer::Decide {
                at: DecideRef::Plan {
                    plan: plan.id,
                    revision: 1
                }
            }
        );
        assert!(
            needs[0].ask.starts_with("Approve Sarah's plan"),
            "{}",
            needs[0].ask
        );
        assert_eq!(needs[0].why.as_deref(), Some("2 tickets in 2 waves."));
        // Nothing of it can start.
        let start = Start {
            ticket: p.tickets[0].ticket.ticket.id,
            engine: Engine::Claude,
            folder: dir.path().to_string_lossy().into_owned(),
            view: None,
            worktree: false,
        };
        assert!(matches!(
            d.plan_start(plan, &start, &sarah(), &by_sarah()).await,
            Err(DeskError::Conflict(_))
        ));
    })
    .await;
}

#[tokio::test]
async fn a_plan_that_cant_run_here_is_refused_in_plain_words() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let looped = new_plan(dir.path(), &[("a", &[1]), ("b", &[0])]);
        let e = d
            .propose_plan(looped, &sarah(), &by_sarah())
            .await
            .unwrap_err();
        assert!(e.to_string().contains("need each other in a loop"), "{e}");
        let mut nowhere = new_plan(dir.path(), &[("a", &[])]);
        nowhere.draft.tickets[0].folder = dir.path().join("gone").to_string_lossy().into_owned();
        let e = d
            .propose_plan(nowhere, &sarah(), &by_sarah())
            .await
            .unwrap_err();
        assert!(e.to_string().contains("isn't a folder"), "{e}");
        assert!(d.plans(None, None).await.unwrap().is_empty());
        assert!(d.boards().await.unwrap().is_empty(), "nothing was made");
    })
    .await;
}

#[tokio::test]
async fn approving_the_revision_read_runs_the_plan_and_a_stale_one_is_refused() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let p = d
            .propose_plan(
                new_plan(dir.path(), &[("a", &[]), ("b", &[])]),
                &sarah(),
                &by_sarah(),
            )
            .await
            .unwrap();
        let id = p.summary.plan.id;
        // Only the person approves.
        assert!(d.approve_plan(id, 1, &by_sarah()).await.is_err());
        // The boss changes it meanwhile: the person's read is old.
        let mut tickets = kept(&p);
        tickets.swap(0, 1);
        let changed = d
            .change_plan(
                id,
                PlanEdit {
                    revision: Some(1),
                    tickets: Some(tickets),
                    ..Default::default()
                },
                Engine::Claude,
                false,
                &by_sarah(),
            )
            .await
            .unwrap();
        assert!(changed.applied, "before approval a change is the proposal");
        assert_eq!(changed.plan.summary.plan.revision, 2);
        let refused = d.approve_plan(id, 1, &By::you()).await.unwrap_err();
        assert!(
            refused.to_string().contains("look at revision 2"),
            "{refused}"
        );
        assert!(matches!(refused, DeskError::Conflict(_)));
        assert_eq!(
            d.plan_needs()[0].answer,
            Answer::Decide {
                at: DecideRef::Plan {
                    plan: id,
                    revision: 2
                }
            }
        );
        let ran = d.approve_plan(id, 2, &By::you()).await.unwrap();
        assert_eq!(ran.summary.plan.state, PlanState::Running);
        assert_eq!(ran.summary.plan.approved_revision, Some(2));
        assert_eq!(states(&ran), vec![TicketState::Todo; 2]);
        // In the order approved: b before a.
        assert_eq!(ran.tickets[0].ticket.ticket.title, "b");
        assert!(ran.tickets[0].ticket.ticket.rank < ran.tickets[1].ticket.ticket.rank);
        assert_eq!(ran.summary.progress.next.len(), 2);
        assert!(d.plan_needs().is_empty());
        // Approving it again changes nothing.
        assert_eq!(
            d.approve_plan(id, 2, &By::you())
                .await
                .unwrap()
                .summary
                .plan
                .revision,
            2
        );
    })
    .await;
}

#[tokio::test]
async fn the_boss_keeps_a_running_plan_current_and_asks_before_changing_its_scope() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let p = d
            .propose_plan(
                new_plan(dir.path(), &[("a", &[]), ("b", &[])]),
                &sarah(),
                &by_sarah(),
            )
            .await
            .unwrap();
        let id = p.summary.plan.id;
        let p = d.approve_plan(id, 1, &By::you()).await.unwrap();
        // A minor change applies at once.
        let mut renamed = kept(&p);
        renamed[0].title = "a, with tests".into();
        renamed[1].needs = vec![0];
        let minor = d
            .change_plan(
                id,
                PlanEdit {
                    revision: Some(1),
                    tickets: Some(renamed),
                    why: Some("b uses a's API".into()),
                    ..Default::default()
                },
                Engine::Claude,
                false,
                &by_sarah(),
            )
            .await
            .unwrap();
        assert_eq!(minor.change, Some(Change::Minor));
        assert!(minor.applied);
        let plan = &minor.plan.summary.plan;
        assert_eq!((plan.revision, plan.approved_revision), (2, Some(2)));
        assert_eq!(minor.plan.tickets[0].ticket.ticket.title, "a, with tests");
        assert_eq!(minor.plan.tickets[1].ticket.ticket.needs.len(), 1);
        assert!(
            minor.summary.contains("(b uses a's API)"),
            "{}",
            minor.summary
        );
        assert!(d.plan_needs().is_empty());

        // Adding a ticket is the person's to approve; the plan runs on.
        let mut more = kept(&minor.plan);
        more.push(DraftTicket {
            title: "Refunds".into(),
            folder: dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        });
        let scope = d
            .change_plan(
                id,
                PlanEdit {
                    revision: Some(2),
                    tickets: Some(more),
                    ..Default::default()
                },
                Engine::Claude,
                false,
                &by_sarah(),
            )
            .await
            .unwrap();
        assert_eq!(scope.change, Some(Change::Scope));
        assert!(!scope.applied);
        let plan = &scope.plan.summary.plan;
        assert_eq!((plan.revision, plan.approved_revision), (3, Some(2)));
        assert_eq!(
            scope.plan.tickets.len(),
            2,
            "not on the board until approved"
        );
        let needs = d.plan_needs();
        assert_eq!(needs.len(), 1);
        assert!(
            needs[0].ask.starts_with("Plan change: Added “Refunds”"),
            "{}",
            needs[0].ask
        );

        let approved = d.approve_plan(id, 3, &By::you()).await.unwrap();
        assert_eq!(approved.tickets.len(), 3);
        let refunds = approved
            .tickets
            .iter()
            .find(|t| t.ticket.ticket.title == "Refunds")
            .unwrap();
        assert_eq!(refunds.ticket.ticket.state, TicketState::Todo);
        assert_eq!(refunds.engine, Some(Engine::Claude));
        assert!(d.plan_needs().is_empty());

        // At `suggest`, even a minor change waits.
        let mut again = kept(&approved);
        again.swap(1, 2);
        let held = d
            .change_plan(
                id,
                PlanEdit {
                    revision: Some(3),
                    tickets: Some(again),
                    ..Default::default()
                },
                Engine::Claude,
                true,
                &by_sarah(),
            )
            .await
            .unwrap();
        assert!(!held.applied);
        assert_eq!(held.change, Some(Change::Minor));
    })
    .await;
}

#[tokio::test]
async fn the_persons_edit_from_an_old_revision_is_refused_and_their_own_applies() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let p = d
            .propose_plan(new_plan(dir.path(), &[("a", &[])]), &sarah(), &by_sarah())
            .await
            .unwrap();
        let id = p.summary.plan.id;
        let p = d.approve_plan(id, 1, &By::you()).await.unwrap();
        let stale = d
            .change_plan(
                id,
                PlanEdit {
                    revision: Some(0),
                    goal: Some("x".into()),
                    ..Default::default()
                },
                Engine::Claude,
                false,
                &By::you(),
            )
            .await
            .unwrap_err();
        assert!(matches!(stale, DeskError::Conflict(_)));
        // The person's change of scope is theirs: it applies.
        let mut fewer = kept(&p);
        fewer.push(DraftTicket {
            title: "Docs".into(),
            folder: dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        });
        let mine = d
            .change_plan(
                id,
                PlanEdit {
                    revision: Some(1),
                    goal: Some("Ship checkout and its docs".into()),
                    parallel: Some(Some(2)),
                    tickets: Some(fewer),
                    ..Default::default()
                },
                Engine::Claude,
                false,
                &By::you(),
            )
            .await
            .unwrap();
        assert!(mine.applied);
        assert_eq!(mine.change, Some(Change::Scope));
        let plan = &mine.plan.summary.plan;
        assert_eq!(plan.goal, "Ship checkout and its docs");
        assert_eq!(plan.parallel, Some(2));
        assert_eq!(plan.approved_revision, Some(2));
        assert_eq!(mine.plan.tickets.len(), 2);
        assert!(d.plan_needs().is_empty());
    })
    .await;
}

#[tokio::test]
async fn a_start_cut_off_before_its_session_starts_that_session_and_no_other() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let p = d
            .propose_plan(
                new_plan(dir.path(), &[("a", &[]), ("b", &[])]),
                &sarah(),
                &by_sarah(),
            )
            .await
            .unwrap();
        let id = p.summary.plan.id;
        let p = d.approve_plan(id, 1, &By::you()).await.unwrap();
        let plan = p.summary.plan.clone();
        let a = p.tickets[0].ticket.ticket.id;

        // The runner wrote its link and stopped before the session started.
        let mut t = d.ticket_of(a).await.unwrap();
        let session = Uuid::new_v4();
        d.0.store
            .begin_start(&mut t, 9_000.0, session, crew::now())
            .await
            .unwrap();
        assert!(d.view(session).is_err());
        let rows = d.plan_tickets(id).await.unwrap();
        assert!(rows[0].starting);
        assert_eq!(plan::progress(&rows).doing, 1);

        let link = d.0.store.starting().await.unwrap().remove(0);
        let started = d
            .plan_restart_start(link, Some(&sarah()), &by_sarah())
            .await
            .unwrap()
            .expect("it starts now");
        assert_eq!(started.id, session, "the same session it named");
        assert_eq!(d.charge_of(session).map(|c| c.boss), Some("sarah".into()));
        let now = d.ticket(a).await.unwrap();
        assert_eq!(now.ticket.session(), Some(session));
        assert_eq!(now.ticket.state, TicketState::Doing);
        assert!(d.0.store.starting().await.unwrap().is_empty());
        // Run again, it starts nothing more.
        assert!(matches!(
            d.plan_restart_start(link, Some(&sarah()), &by_sarah()).await,
            Ok(Some(v)) if v.id == session
        ));
        let start = Start {
            ticket: a,
            engine: Engine::Claude,
            folder: dir.path().to_string_lossy().into_owned(),
            view: None,
            worktree: false,
        };
        assert!(d
            .plan_start(&plan, &start, &sarah(), &by_sarah())
            .await
            .is_err());
        let named: Vec<_> = d.list().into_iter().filter(|v| v.name == "DES-1").collect();
        assert_eq!(named.len(), 1, "one session for the ticket");
        // Asked to start an id it has, Desk refuses.
        let again = d
            .create_for_as(
                session,
                NewSession {
                    engine: Engine::Claude,
                    view: None,
                    name: "again".into(),
                    cwd: dir.path().to_string_lossy().into_owned(),
                    goal: String::new(),
                    worktree: None,
                    prompt: None,
                    approvals: None,
                    approve_for_me: false,
                    account: None,
                    env: Default::default(),
                    group: None,
                },
                &sarah(),
                by_sarah(),
            )
            .await;
        assert!(matches!(again, Err(DeskError::Conflict(_))));
    })
    .await;
}

#[tokio::test]
async fn a_start_cut_off_after_its_session_started_only_links_it() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let p = d
            .propose_plan(new_plan(dir.path(), &[("a", &[])]), &sarah(), &by_sarah())
            .await
            .unwrap();
        let id = p.summary.plan.id;
        let p = d.approve_plan(id, 1, &By::you()).await.unwrap();
        let a = p.tickets[0].ticket.ticket.id;
        let mut t = d.ticket_of(a).await.unwrap();
        let session = Uuid::new_v4();
        d.0.store
            .begin_start(&mut t, 9_000.0, session, crew::now())
            .await
            .unwrap();
        // Its session started; the runner stopped before linking it.
        let new = NewSession {
            engine: Engine::Claude,
            view: None,
            name: "DES-1".into(),
            cwd: dir.path().to_string_lossy().into_owned(),
            goal: String::new(),
            worktree: None,
            prompt: None,
            approvals: None,
            approve_for_me: false,
            account: None,
            env: Default::default(),
            group: None,
        };
        d.create_for_as(session, new, &sarah(), by_sarah())
            .await
            .unwrap();
        let before = d.list().len();
        let link = d.0.store.starting().await.unwrap().remove(0);
        let v = d
            .plan_restart_start(link, Some(&sarah()), &by_sarah())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v.id, session);
        assert_eq!(d.list().len(), before, "no second session");
        assert_eq!(d.ticket(a).await.unwrap().ticket.session(), Some(session));
        assert!(d.0.store.starting().await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn a_ticket_the_person_moved_before_a_cut_off_start_is_left_where_they_put_it() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let p = d
            .propose_plan(new_plan(dir.path(), &[("a", &[])]), &sarah(), &by_sarah())
            .await
            .unwrap();
        let id = p.summary.plan.id;
        let p = d.approve_plan(id, 1, &By::you()).await.unwrap();
        let a = p.tickets[0].ticket.ticket.id;
        let mut t = d.ticket_of(a).await.unwrap();
        let session = Uuid::new_v4();
        d.0.store
            .begin_start(&mut t, 9_000.0, session, crew::now())
            .await
            .unwrap();
        d.move_ticket(
            a,
            Move {
                state: TicketState::Backlog,
                before: None,
                after: None,
            },
            &By::you(),
        )
        .await
        .unwrap();
        let link = d.0.store.starting().await.unwrap().remove(0);
        assert!(d
            .plan_restart_start(link, Some(&sarah()), &by_sarah())
            .await
            .unwrap()
            .is_none());
        assert!(d.view(session).is_err(), "nothing started");
        assert_eq!(
            d.ticket(a).await.unwrap().ticket.state,
            TicketState::Backlog
        );
        assert!(d.0.store.starting().await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn a_started_ticket_keeps_where_it_runs_and_dropping_the_plan_stops_or_leaves_its_sessions() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let p = d
            .propose_plan(
                new_plan(dir.path(), &[("a", &[]), ("b", &[])]),
                &sarah(),
                &by_sarah(),
            )
            .await
            .unwrap();
        let id = p.summary.plan.id;
        let p = d.approve_plan(id, 1, &By::you()).await.unwrap();
        let plan = p.summary.plan.clone();
        let start = Start {
            ticket: p.tickets[0].ticket.ticket.id,
            engine: Engine::Claude,
            folder: dir.path().to_string_lossy().into_owned(),
            view: None,
            worktree: false,
        };
        let s = d
            .plan_start(&plan, &start, &sarah(), &by_sarah())
            .await
            .unwrap();
        assert_eq!(
            d.charge_of(s.id).map(|c| c.how),
            Some(crate::charge::How::Started)
        );
        let other = tempfile::tempdir().unwrap();
        let mut moved = kept(&d.plan(id).await.unwrap());
        moved[0].folder = other.path().to_string_lossy().into_owned();
        let refused = d
            .change_plan(
                id,
                PlanEdit {
                    revision: Some(1),
                    tickets: Some(moved),
                    ..Default::default()
                },
                Engine::Claude,
                false,
                &by_sarah(),
            )
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("has started"), "{refused}");

        // Dropped, leaving its session to the person.
        let dropped = d.drop_plan(id, OnDrop::Leave, &By::you()).await.unwrap();
        assert_eq!(dropped.summary.plan.state, PlanState::Dropped);
        let by_title: HashMap<String, TicketState> = dropped
            .tickets
            .iter()
            .map(|t| (t.ticket.ticket.title.clone(), t.ticket.ticket.state))
            .collect();
        assert_eq!(by_title["a"], TicketState::Doing, "left with its session");
        assert_eq!(by_title["b"], TicketState::Dropped);
        assert!(
            d.charge_of(s.id).is_some_and(|c| !c.live()),
            "the person runs it"
        );
        assert!(d.view(s.id).is_ok_and(|v| v.state != SessionState::Closed));
        assert!(d
            .change_plan(id, PlanEdit::default(), Engine::Claude, false, &By::you())
            .await
            .is_err());
    })
    .await;
}

#[tokio::test]
async fn a_paused_plan_starts_nothing_until_the_person_resumes_it() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let p = d
            .propose_plan(new_plan(dir.path(), &[("a", &[])]), &sarah(), &by_sarah())
            .await
            .unwrap();
        let id = p.summary.plan.id;
        assert!(
            d.pause_plan(id, Paused::Person, &By::you()).await.is_err(),
            "not running yet"
        );
        d.approve_plan(id, 1, &By::you()).await.unwrap();
        let paused = d.pause_plan(id, Paused::Person, &By::you()).await.unwrap();
        assert_eq!(paused.summary.plan.paused, Some(Paused::Person));
        assert!(!paused.summary.plan.runs());
        let refused = d
            .resume_plan(id, &[Paused::NoBoss], &By::you())
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("you paused it"), "{refused}");
        let back = d
            .resume_plan(id, &[Paused::Person], &By::you())
            .await
            .unwrap();
        assert!(back.summary.plan.runs());
        assert_eq!(
            d.plans(Some(PlanState::Running), None).await.unwrap().len(),
            1
        );
        assert_eq!(d.plans(None, Some("g-other")).await.unwrap().len(), 0);
        assert_eq!(d.plans(None, Some("")).await.unwrap().len(), 1);
    })
    .await;
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (u16, serde_json::Value) {
    use tower::ServiceExt;
    let req = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(body.map_or_else(axum::body::Body::empty, |b| {
            axum::body::Body::from(b.to_string())
        }))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status().as_u16();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn the_persons_plan_routes_read_edit_approve_and_drop_and_refuse_a_stale_revision() {
    use serde_json::json;
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let p = d
            .propose_plan(
                new_plan(dir.path(), &[("a", &[]), ("b", &[0])]),
                &sarah(),
                &by_sarah(),
            )
            .await
            .unwrap();
        let id = p.summary.plan.id;
        let closed = crate::plan_api::api_routes(d.clone(), crate::testing::deny_all());
        for (method, uri) in [
            ("GET", "/api/desk/plans".to_string()),
            ("GET", format!("/api/desk/plans/{id}")),
            ("POST", format!("/api/desk/plans/{id}/approve")),
        ] {
            assert_eq!(
                call(&closed, method, &uri, Some(json!({"revision": 1})))
                    .await
                    .0,
                401,
                "{method} {uri}"
            );
        }
        let app = crate::plan_api::api_routes(d.clone(), crate::testing::allow_all());
        let (status, list) = call(&app, "GET", "/api/desk/plans?state=proposed", None).await;
        assert_eq!(status, 200);
        assert_eq!(list[0]["id"], id.to_string());
        assert_eq!(list[0]["bossName"], "Sarah");
        assert_eq!(list[0]["progress"]["waiting"], 2);
        assert_eq!(
            call(&app, "GET", "/api/desk/plans?state=tired", None)
                .await
                .0,
            400
        );
        let (status, one) = call(&app, "GET", &format!("/api/desk/plans/{id}"), None).await;
        assert_eq!(status, 200);
        assert_eq!(one["waves"].as_array().unwrap().len(), 2);
        assert_eq!(one["tickets"][0]["engine"], "claude");
        assert_eq!(one["revisions"][0]["change"], "scope");

        // The person's edit needs the revision it was made from, and a newer
        // one refuses it.
        let edit = |rev: Option<i32>| json!({ "revision": rev, "goal": "Ship the checkout today" });
        assert_eq!(
            call(
                &app,
                "PATCH",
                &format!("/api/desk/plans/{id}"),
                Some(edit(None))
            )
            .await
            .0,
            400
        );
        let (status, edited) = call(
            &app,
            "PATCH",
            &format!("/api/desk/plans/{id}"),
            Some(edit(Some(1))),
        )
        .await;
        assert_eq!(status, 200, "{edited}");
        assert_eq!(edited["plan"]["revision"], 2);
        let (status, stale) = call(
            &app,
            "PATCH",
            &format!("/api/desk/plans/{id}"),
            Some(edit(Some(1))),
        )
        .await;
        assert_eq!(status, 409);
        assert!(
            stale["error"]
                .as_str()
                .unwrap()
                .contains("look at revision 2"),
            "{stale}"
        );
        let approve = |rev: i32| json!({ "revision": rev });
        assert_eq!(
            call(
                &app,
                "POST",
                &format!("/api/desk/plans/{id}/approve"),
                Some(approve(1))
            )
            .await
            .0,
            409
        );
        assert_eq!(
            call(
                &app,
                "POST",
                &format!("/api/desk/plans/{id}/approve"),
                Some(json!({ "revision": 2, "budget": { "usd": 40 } }))
            )
            .await
            .0,
            400,
            "no plan budget yet"
        );
        let (status, ran) = call(
            &app,
            "POST",
            &format!("/api/desk/plans/{id}/approve"),
            Some(approve(2)),
        )
        .await;
        assert_eq!(status, 200, "{ran}");
        assert_eq!(ran["state"], "running");
        let (status, paused) =
            call(&app, "POST", &format!("/api/desk/plans/{id}/pause"), None).await;
        assert_eq!((status, paused["paused"].as_str()), (200, Some("person")));
        let (status, resumed) =
            call(&app, "POST", &format!("/api/desk/plans/{id}/resume"), None).await;
        assert_eq!((status, resumed["paused"].is_null()), (200, true));
        assert_eq!(
            call(&app, "POST", &format!("/api/desk/plans/{id}/drop"), None)
                .await
                .0,
            400
        );
        let (status, dropped) = call(
            &app,
            "POST",
            &format!("/api/desk/plans/{id}/drop"),
            Some(json!({ "sessions": "stop" })),
        )
        .await;
        assert_eq!((status, dropped["state"].as_str()), (200, Some("dropped")));
    })
    .await;
}
