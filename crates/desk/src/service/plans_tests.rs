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
                machine: None,
                model: None,
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
        budget: None,
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
            machine: None,
            model: None,
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
            Err(StartRefused::Skipped(_))
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
            .begin_start(&mut t, id, 9_000.0, session, crew::now())
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
                    fresh_worktree: false,
                    engine: Engine::Claude,
                    view: None,
                    name: "again".into(),
                    cwd: dir.path().to_string_lossy().into_owned(),
                    goal: String::new(),
                    worktree: None,
                    prompt: None,
                    approvals: None,
                    approve_for_me: false,
                    fast: false,
                    model: None,
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
            .begin_start(&mut t, id, 9_000.0, session, crew::now())
            .await
            .unwrap();
        // Its session started; the runner stopped before linking it.
        let new = NewSession {
            fresh_worktree: false,
            engine: Engine::Claude,
            view: None,
            name: "DES-1".into(),
            cwd: dir.path().to_string_lossy().into_owned(),
            goal: String::new(),
            worktree: None,
            prompt: None,
            approvals: None,
            approve_for_me: false,
            fast: false,
            model: None,
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
            .begin_start(&mut t, id, 9_000.0, session, crew::now())
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

/// A ticket's session runs the model the person chose for it.
#[tokio::test]
async fn a_tickets_session_runs_the_tickets_model() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let mut plan = new_plan(dir.path(), &[("a", &[]), ("b", &[])]);
        plan.draft.tickets[0].model = Some("sonnet".into());
        let p = d.propose_plan(plan, &sarah(), &by_sarah()).await.unwrap();
        let p = d
            .approve_plan(p.summary.plan.id, 1, &By::you())
            .await
            .unwrap();
        let plan = p.summary.plan.clone();
        let start = |i: usize| Start {
            ticket: p.tickets[i].ticket.ticket.id,
            engine: Engine::Claude,
            folder: dir.path().to_string_lossy().into_owned(),
            view: None,
            worktree: false,
        };
        let a = d
            .plan_start(&plan, &start(0), &sarah(), &by_sarah())
            .await
            .unwrap();
        assert_eq!(d.view(a.id).unwrap().model.as_deref(), Some("sonnet"));
        let b = d
            .plan_start(&plan, &start(1), &sarah(), &by_sarah())
            .await
            .unwrap();
        assert_eq!(d.view(b.id).unwrap().model, None, "its engine's own");
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
            ("POST", format!("/api/desk/plans/{id}/pause")),
        ] {
            assert_eq!(
                call(&closed, method, &uri, Some(json!({"revision": 1})))
                    .await
                    .0,
                401,
                "{method} {uri}"
            );
        }
        // Approving and turning down are the Floor's decide route's, the
        // hub holder's alone: there are no routes for them here.
        let hub = crate::plan_api::api_routes(d.clone(), crate::testing::allow_all());
        for uri in [
            format!("/api/desk/plans/{id}/approve"),
            format!("/api/desk/plans/{id}/changes/1/reject"),
        ] {
            assert_eq!(
                call(&hub, "POST", &uri, Some(json!({ "revision": 1 })))
                    .await
                    .0,
                404,
                "{uri}"
            );
        }
        let app = crate::plan_api::api_routes(d.clone(), crate::testing::identified("sam"));
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
        let ran = d.approve_plan(id, 2, &By::you()).await.unwrap();
        assert_eq!(ran.summary.plan.state, PlanState::Running);
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

#[tokio::test]
async fn two_approvals_at_once_run_the_plan_once_and_an_approval_racing_a_change_keeps_every_revision(
) {
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
        // From the Floor and from the DM at once.
        let you = By::you();
        let (one, two) = tokio::join!(d.approve_plan(id, 1, &you), d.approve_plan(id, 1, &you));
        assert!(one.is_ok() && two.is_ok());
        let ran = d.plan(id).await.unwrap();
        assert_eq!(ran.summary.plan.state, PlanState::Running);
        assert_eq!(states(&ran), vec![TicketState::Todo; 2]);
        let todo: HashSet<i64> = ran
            .tickets
            .iter()
            .map(|t| t.ticket.ticket.rank as i64)
            .collect();
        assert_eq!(todo.len(), 2, "each moved once, to a place of its own");

        // The person approves as the boss adds a ticket: whichever lands
        // first, the plan's revision is its newest one kept.
        let q = d
            .propose_plan(new_plan(dir.path(), &[("c", &[])]), &sarah(), &by_sarah())
            .await
            .unwrap();
        let qid = q.summary.plan.id;
        let mut more = kept(&q);
        more.push(DraftTicket {
            title: "d".into(),
            folder: dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        });
        let edit = PlanEdit {
            revision: Some(1),
            tickets: Some(more),
            ..Default::default()
        };
        let boss = by_sarah();
        let (approved, changed) = tokio::join!(
            d.approve_plan(qid, 1, &you),
            d.change_plan(qid, edit, Engine::Claude, false, &boss)
        );
        let now = d.plan(qid).await.unwrap();
        let newest = now.revisions.iter().map(|r| r.revision).max().unwrap();
        assert_eq!(now.summary.plan.revision, newest);
        match (approved.is_ok(), changed.is_ok()) {
            // Approved first: the change waits for the person.
            (true, true) => {
                assert_eq!(now.summary.plan.approved_revision, Some(1));
                assert_eq!(now.summary.plan.revision, 2);
                assert_eq!(d.plan_needs().len(), 1);
            }
            // Changed first: the approval of what was read is refused.
            (false, true) => {
                assert_eq!(now.summary.plan.approved_revision, None);
                assert_eq!(now.summary.plan.revision, 2);
            }
            other => panic!("{other:?}: {approved:?} {changed:?}"),
        }
        // Either way the person can approve the newest.
        d.approve_plan(qid, newest, &you).await.unwrap();
        assert_eq!(d.plan(qid).await.unwrap().tickets.len(), 2);
    })
    .await;
}

#[tokio::test]
async fn a_pause_that_comes_while_a_pass_runs_starts_nothing_and_a_pause_of_the_persons_stays() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let p = d
            .propose_plan(new_plan(dir.path(), &[("a", &[])]), &sarah(), &by_sarah())
            .await
            .unwrap();
        let id = p.summary.plan.id;
        let read = d.approve_plan(id, 1, &By::you()).await.unwrap();
        // The runner read it running; the person pauses it before it starts.
        let start = plan::next_starts(
            &read.summary.plan,
            &d.plan_tickets(id).await.unwrap(),
            0,
            4,
            None,
        )
        .remove(0);
        d.pause_plan(id, Paused::Person, &By::you()).await.unwrap();
        let refused = d
            .plan_start(&read.summary.plan, &start, &sarah(), &by_sarah())
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("paused or ended"), "{refused}");
        assert!(d.list().is_empty(), "nothing started");
        assert_eq!(
            d.ticket(start.ticket).await.unwrap().ticket.state,
            TicketState::Todo
        );
        assert!(d.0.store.starting().await.unwrap().is_empty());
        // Desk's own reasons never replace the person's pause.
        let kept = d
            .pause_plan(id, Paused::NoBoss, &By::desk("Its boss is gone."))
            .await
            .unwrap();
        assert_eq!(kept.summary.plan.paused, Some(Paused::Person));
        assert_eq!(kept.summary.paused_why.as_deref(), Some("You paused it"));
    })
    .await;
}

#[tokio::test]
async fn a_drop_racing_a_start_leaves_no_session_working_on_the_dropped_ticket() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let p = d
            .propose_plan(new_plan(dir.path(), &[("a", &[])]), &sarah(), &by_sarah())
            .await
            .unwrap();
        let id = p.summary.plan.id;
        let read = d.approve_plan(id, 1, &By::you()).await.unwrap();
        let start = plan::next_starts(
            &read.summary.plan,
            &d.plan_tickets(id).await.unwrap(),
            0,
            4,
            None,
        )
        .remove(0);
        let boss = sarah();
        let by = by_sarah();
        let you = By::you();
        let (started, dropped) = tokio::join!(
            d.plan_start(&read.summary.plan, &start, &boss, &by),
            d.drop_plan(id, OnDrop::Stop, &you)
        );
        dropped.unwrap();
        let t = d.ticket(start.ticket).await.unwrap();
        assert_eq!(t.ticket.state, TicketState::Dropped);
        assert!(d.0.store.starting().await.unwrap().is_empty());
        if let Ok(s) = started {
            // Started first: the drop stopped it.
            let v = d.view(s.id).unwrap();
            assert!(
                !matches!(v.state, SessionState::Starting | SessionState::Working),
                "{:?}",
                v.state
            );
        }
    })
    .await;
}

#[tokio::test]
async fn a_raised_limit_waits_for_the_person_a_lowered_one_applies_and_approval_keeps_it() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let mut new = new_plan(dir.path(), &[("a", &[]), ("b", &[])]);
        new.parallel = Some(2);
        let p = d.propose_plan(new, &sarah(), &by_sarah()).await.unwrap();
        let id = p.summary.plan.id;
        assert_eq!(p.revisions[0].snapshot.parallel, Some(2));
        d.approve_plan(id, 1, &By::you()).await.unwrap();
        let edit = |rev: i32, n: Option<i16>| PlanEdit {
            revision: Some(rev),
            parallel: Some(n),
            ..Default::default()
        };
        let lower = d
            .change_plan(id, edit(1, Some(1)), Engine::Claude, false, &by_sarah())
            .await
            .unwrap();
        assert_eq!((lower.change, lower.applied), (Some(Change::Minor), true));
        assert_eq!(lower.plan.summary.plan.parallel, Some(1));
        let raise = d
            .change_plan(id, edit(2, Some(4)), Engine::Claude, false, &by_sarah())
            .await
            .unwrap();
        assert_eq!((raise.change, raise.applied), (Some(Change::Scope), false));
        assert_eq!(
            raise.plan.summary.plan.parallel,
            Some(1),
            "it runs as approved"
        );
        let ran = d.approve_plan(id, 3, &By::you()).await.unwrap();
        assert_eq!(ran.summary.plan.parallel, Some(4));
    })
    .await;
}

#[tokio::test]
async fn the_person_turns_down_a_change_or_must_before_editing_and_the_plan_stays_as_approved() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let p = d
            .propose_plan(new_plan(dir.path(), &[("a", &[])]), &sarah(), &by_sarah())
            .await
            .unwrap();
        let id = p.summary.plan.id;
        let p = d.approve_plan(id, 1, &By::you()).await.unwrap();
        let mut more = kept(&p);
        more.push(DraftTicket {
            title: "b".into(),
            folder: dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        });
        let waits = d
            .change_plan(
                id,
                PlanEdit {
                    revision: Some(1),
                    tickets: Some(more),
                    ..Default::default()
                },
                Engine::Claude,
                false,
                &by_sarah(),
            )
            .await
            .unwrap();
        assert!(!waits.applied);
        // The person's own edit doesn't slip the boss's change through.
        let blocked = d
            .change_plan(
                id,
                PlanEdit {
                    revision: Some(2),
                    goal: Some("Ship it all".into()),
                    ..Default::default()
                },
                Engine::Claude,
                false,
                &By::you(),
            )
            .await
            .unwrap_err();
        assert!(
            blocked
                .to_string()
                .contains("Approve or reject Sarah's change first"),
            "{blocked}"
        );
        // Deny on the Floor turns it down.
        let kept_plan = d.decide_plan(id, 2, false, None, &By::you()).await.unwrap();
        let plan = &kept_plan.summary.plan;
        assert_eq!((plan.revision, plan.approved_revision), (3, Some(3)));
        assert_eq!(kept_plan.tickets.len(), 1, "nothing was added");
        assert!(
            kept_plan.revisions[2]
                .summary
                .starts_with("Kept as approved"),
            "{}",
            kept_plan.revisions[2].summary
        );
        assert!(d.plan_needs().is_empty());
        // Nothing waits now: no change to turn down.
        assert!(d.reject_plan(id, 3, &By::you()).await.is_err());

        // Again, by revision.
        let mut again = kept(&kept_plan);
        again.push(DraftTicket {
            title: "c".into(),
            folder: dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        });
        d.change_plan(
            id,
            PlanEdit {
                revision: Some(3),
                tickets: Some(again),
                ..Default::default()
            },
            Engine::Claude,
            false,
            &by_sarah(),
        )
        .await
        .unwrap();
        // The Floor's Deny names the waiting revision: an older one is stale.
        assert!(matches!(
            d.decide_plan(id, 3, false, None, &By::you()).await,
            Err(DeskError::Conflict(_))
        ));
        let out = d.decide_plan(id, 4, false, None, &By::you()).await.unwrap();
        assert_eq!(out.summary.plan.approved_revision, Some(5));
        assert_eq!(out.tickets.len(), 1);
    })
    .await;
}

#[tokio::test]
async fn a_plan_paused_for_a_ticket_that_keeps_failing_says_why_and_the_floor_continues_it() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let p = d
            .propose_plan(new_plan(dir.path(), &[("a", &[])]), &sarah(), &by_sarah())
            .await
            .unwrap();
        let id = p.summary.plan.id;
        d.approve_plan(id, 1, &By::you()).await.unwrap();
        let why = "DES-1 couldn't start 5 times: /gone is not a folder";
        let paused = d
            .pause_plan_failing(id, why, &By::desk("It keeps failing."))
            .await
            .unwrap();
        assert_eq!(paused.summary.plan.paused, Some(Paused::StartFailing));
        assert_eq!(paused.summary.paused_why.as_deref(), Some(why));
        let v = serde_json::to_value(&paused.summary).unwrap();
        assert_eq!(v["pausedWhy"], why);
        let needs = d.plan_needs();
        assert_eq!(needs.len(), 1);
        assert!(needs[0].ask.contains(why), "{}", needs[0].ask);
        let back = d.decide_plan(id, 1, true, None, &By::you()).await.unwrap();
        assert!(back.summary.plan.runs());
        assert_eq!(back.summary.paused_why, None);
        assert!(d.plan_needs().is_empty());
    })
    .await;
}

#[tokio::test]
async fn the_breaker_holding_the_plans_group_holds_its_starts() {
    use crate::ledger::{Billing, Priced, Spend, Tokens};
    use crate::ledger_store::Kept;
    use workforce_core::budget::{BudgetScope, Period, Unit};
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        d.change_layout(|l| l.create("g-acme", "Acme", &[], None))
            .await
            .unwrap();
        let mut new = new_plan(dir.path(), &[("a", &[])]);
        new.group = Some("g-acme".into());
        let p = d.propose_plan(new, &sarah(), &by_sarah()).await.unwrap();
        let id = p.summary.plan.id;
        let read = d.approve_plan(id, 1, &By::you()).await.unwrap();
        // Acme's own budget is held; Sarah sits in no group.
        d.put_budget(
            super::super::NewBudget {
                scope: BudgetScope::Desk,
                group: Some("g-acme".into()),
                period: Period::Day,
                unit: Unit::Usd,
                amount: 11.0,
                warn_at: None,
                on_exceed: None,
            },
            By::you(),
        )
        .await
        .unwrap();
        let spender = d
            .create(
                NewSession {
                    fresh_worktree: false,
                    engine: Engine::Claude,
                    view: None,
                    name: "acme-web".into(),
                    cwd: dir.path().to_string_lossy().into_owned(),
                    goal: String::new(),
                    worktree: None,
                    prompt: None,
                    approvals: None,
                    approve_for_me: false,
                    fast: false,
                    model: None,
                    account: None,
                    env: Default::default(),
                    group: Some("g-acme".into()),
                },
                Some(By::you()),
            )
            .await
            .unwrap();
        d.0.store
            .ledger_write(
                spender.id,
                Engine::Claude,
                None,
                &[Kept {
                    spend: Spend {
                        key: Uuid::new_v4().to_string(),
                        at: chrono::Utc::now(),
                        model: Some("claude-opus-5-5".into()),
                        tokens: Tokens {
                            input: 1000,
                            ..Default::default()
                        },
                        usd: Some(10.5),
                        priced: Priced::Reported,
                        billing: None,
                    },
                    billing: Billing::Plan,
                    account: None,
                }],
            )
            .await
            .unwrap();
        d.breaker_spend(Some(vec![spender.id])).await;
        assert!(d.starts_held(Some("g-acme")).is_some());
        assert!(d.starts_held(None).is_none(), "not Desk's own total");
        let start = plan::next_starts(
            &read.summary.plan,
            &d.plan_tickets(id).await.unwrap(),
            0,
            4,
            None,
        )
        .remove(0);
        let refused = d
            .plan_start(&read.summary.plan, &start, &sarah(), &by_sarah())
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("breaker"), "{refused}");
        assert_eq!(d.list().len(), 1, "only the person's");
    })
    .await;
}

#[tokio::test]
async fn a_start_that_cant_happen_now_is_skipped_and_one_whose_session_cant_start_failed() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let folder = tempfile::tempdir().unwrap();
        let p = d
            .propose_plan(
                new_plan(folder.path(), &[("a", &[])]),
                &sarah(),
                &by_sarah(),
            )
            .await
            .unwrap();
        let id = p.summary.plan.id;
        let read = d.approve_plan(id, 1, &By::you()).await.unwrap();
        let start = plan::next_starts(
            &read.summary.plan,
            &d.plan_tickets(id).await.unwrap(),
            0,
            4,
            None,
        )
        .remove(0);
        drop(folder);
        // Its folder is gone: that keeps failing until someone acts.
        let failed = d
            .plan_start(&read.summary.plan, &start, &sarah(), &by_sarah())
            .await
            .unwrap_err();
        assert!(matches!(failed, StartRefused::Failed(_)), "{failed}");
        assert_eq!(
            d.ticket(start.ticket).await.unwrap().ticket.state,
            TicketState::Todo,
            "back in To do"
        );
        assert!(d.0.store.starting().await.unwrap().is_empty());
        // Moved by the person: nothing wrong with it, it is only read again.
        d.move_ticket(
            start.ticket,
            Move {
                state: TicketState::Backlog,
                before: None,
                after: None,
            },
            &By::you(),
        )
        .await
        .unwrap();
        assert!(matches!(
            d.plan_start(&read.summary.plan, &start, &sarah(), &by_sarah())
                .await,
            Err(StartRefused::Skipped(_))
        ));
    })
    .await;
}

#[tokio::test]
async fn a_plans_session_is_written_down_before_its_process_starts() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let p = d
            .propose_plan(new_plan(dir.path(), &[("a", &[])]), &sarah(), &by_sarah())
            .await
            .unwrap();
        let id = p.summary.plan.id;
        let read = d.approve_plan(id, 1, &By::you()).await.unwrap();
        let start = plan::next_starts(
            &read.summary.plan,
            &d.plan_tickets(id).await.unwrap(),
            0,
            4,
            None,
        )
        .remove(0);
        let s = d
            .plan_start(&read.summary.plan, &start, &sarah(), &by_sarah())
            .await
            .unwrap();
        // Its row is there as the start returns, not only once written behind.
        assert!(d.0.store.get(s.id).await.unwrap().is_some());
        assert_eq!(
            d.charge_of(s.id).map(|c| c.how),
            Some(crate::charge::How::Started)
        );
    })
    .await;
}

#[tokio::test]
async fn only_a_bosss_own_starts_under_way_hold_its_slots() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let mia = Party::manager("mia", "Mia");
        let by_mia = By::manager("mia", "Mia", "Atlas asked.");
        let mut links = Vec::new();
        for (boss, by) in [(sarah(), by_sarah()), (mia, by_mia)] {
            let p = d
                .propose_plan(new_plan(dir.path(), &[("a", &[])]), &boss, &by)
                .await
                .unwrap();
            let id = p.summary.plan.id;
            d.approve_plan(id, 1, &By::you()).await.unwrap();
            let mut t = d.ticket_of(p.tickets[0].ticket.ticket.id).await.unwrap();
            d.0.store
                .begin_start(&mut t, id, 9_000.0, Uuid::new_v4(), crew::now())
                .await
                .unwrap();
            links.push(id);
        }
        assert_eq!(d.starts_under_way("sarah").await, 1);
        assert_eq!(d.starts_under_way("mia").await, 1);
        assert_eq!(d.starts_under_way("desk").await, 0);
    })
    .await;
}

#[tokio::test]
async fn a_plan_is_on_the_floor_in_its_own_group_whoever_its_boss_is_now() {
    use crate::scope::Scope;
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        d.change_layout(|l| {
            l.create("acme", "Acme", &[], None)?
                .create("atlas", "Atlas", &[], None)
        })
        .await
        .unwrap();
        let boss = |agent: &str, name: &str, group: Option<&str>| crate::service::Boss {
            agent: agent.into(),
            name: name.into(),
            scope: Scope::of(group),
            level: "act".into(),
        };
        d.set_bosses(vec![
            boss("desk", "Desk", None),
            boss("sarah", "Sarah", Some("acme")),
        ]);
        let p = d
            .propose_plan(
                NewPlan {
                    group: Some("acme".into()),
                    budget: None,
                    ..new_plan(
                        dir.path(),
                        &[("Add the cart API", &[]), ("Wire the form", &[0])],
                    )
                },
                &sarah(),
                &by_sarah(),
            )
            .await
            .unwrap();

        // The Floor lists it in Acme, with where its tickets stand.
        let plans = d.floor_plans().await.unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].id, p.summary.plan.id);
        assert_eq!(
            (plans[0].group.as_deref(), plans[0].boss.as_str()),
            (Some("acme"), "sarah")
        );
        assert_eq!(plans[0].state, PlanState::Proposed);
        assert_eq!(plans[0].progress.waiting, 2);

        let need_of = |d: &Desk| {
            let mut v = crate::floor::view(
                &[],
                d.plan_needs(),
                &Default::default(),
                chrono::Utc::now(),
                chrono::Utc::now(),
            );
            d.stamp_floor(&mut v);
            let n = v.needs.into_iter().next().unwrap();
            (n.group, n.boss)
        };
        assert_eq!(need_of(&d), (Some("acme".into()), Some("sarah".into())));

        // Sarah moves to oversee Atlas and Mia takes Acme: the plan stays
        // Acme's, now Mia's to lead.
        d.set_bosses(vec![
            boss("desk", "Desk", None),
            boss("sarah", "Sarah", Some("atlas")),
            boss("mia", "Mia", Some("acme")),
        ]);
        assert_eq!(need_of(&d), (Some("acme".into()), Some("mia".into())));

        // Acme is sealed: still Acme's, and no one's to lead.
        d.set_bosses(vec![
            boss("desk", "Desk", None),
            boss("sarah", "Sarah", Some("atlas")),
        ]);
        d.bound_groups(["acme".to_string()]).await;
        assert_eq!(need_of(&d), (Some("acme".into()), None));

        // A finished plan is off the Floor.
        d.drop_plan(p.summary.plan.id, OnDrop::Leave, &By::you())
            .await
            .unwrap();
        assert!(d.floor_plans().await.unwrap().is_empty());
    })
    .await;
}

/// `usd` spent by `session` now.
async fn spent(d: &Desk, session: Uuid, usd: f64) {
    spent_at(d, session, usd, chrono::Utc::now()).await;
}

/// `usd` spent by `session` at `at`.
async fn spent_at(d: &Desk, session: Uuid, usd: f64, at: chrono::DateTime<chrono::Utc>) {
    use crate::ledger::{Billing, Priced, Spend, Tokens};
    use crate::ledger_store::Kept;
    d.0.store
        .ledger_write(
            session,
            Engine::Claude,
            None,
            &[Kept {
                spend: Spend {
                    key: Uuid::new_v4().to_string(),
                    at,
                    model: Some("claude-opus-5-5".into()),
                    tokens: Tokens {
                        input: 1000,
                        ..Default::default()
                    },
                    usd: Some(usd),
                    priced: Priced::Reported,
                    billing: None,
                },
                billing: Billing::Plan,
                account: None,
            }],
        )
        .await
        .unwrap();
}

fn capped(usd: f64) -> Option<Option<PlanBudget>> {
    Some(Some(PlanBudget::usd(usd)))
}

/// A plan in Acme of `titles`, approved by the person capped at `usd`.
async fn capped_plan(d: &Desk, dir: &Path, titles: &[(&str, &[usize])], usd: f64) -> PlanDetail {
    d.change_layout(|l| l.create("g-acme", "Acme", &[], None))
        .await
        .unwrap();
    let mut new = new_plan(dir, titles);
    new.group = Some("g-acme".into());
    let p = d.propose_plan(new, &sarah(), &by_sarah()).await.unwrap();
    d.decide_plan(p.summary.plan.id, 1, true, capped(usd), &By::you())
        .await
        .unwrap()
}

/// What may start now in plan `id`, as the runner reads it, with `held`
/// passed as its standing.
async fn next(d: &Desk, id: Uuid) -> Vec<Start> {
    let p = d.plan(id).await.unwrap().summary.plan;
    let held = d.plan_starts_held(p.group.as_deref(), id).map(|(s, _)| s);
    plan::next_starts(&p, &d.plan_tickets(id).await.unwrap(), 0, 4, held)
}

#[tokio::test]
async fn a_plans_budget_is_set_on_approval_kept_in_its_revision_and_only_changed_there() {
    use workforce_core::budget::{BudgetScope, Period, Unit};
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let p = capped_plan(&d, dir.path(), &[("a", &[])], 40.0).await;
        let id = p.summary.plan.id;
        // Capped as the person approved it: a revision of theirs first.
        assert_eq!(
            (p.summary.plan.revision, p.summary.plan.approved_revision),
            (2, Some(2))
        );
        let r = &p.revisions[1];
        assert_eq!(r.by.kind, PartyKind::You);
        assert_eq!(r.snapshot.budget, Some(PlanBudget::usd(40.0)));
        assert_eq!(r.summary, "Capped it at a total budget of $40");
        assert_eq!(p.summary.budget, Some(PlanBudget::usd(40.0)));
        let kept: Vec<_> = d
            .budget_list()
            .into_iter()
            .filter(|b| b.scope == BudgetScope::Plan(id))
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(
            (kept[0].group.as_deref(), kept[0].period, kept[0].amount),
            (Some("g-acme"), Period::Total, 40.0)
        );
        // Not among the person's own budgets, and never set as one.
        assert!(d.budgets_view().await.unwrap().budgets.is_empty());
        let own = super::super::NewBudget {
            scope: BudgetScope::Plan(id),
            group: None,
            period: Period::Total,
            unit: Unit::Usd,
            amount: 400.0,
            warn_at: None,
            on_exceed: None,
        };
        assert!(matches!(
            d.put_budget(own, By::you()).await,
            Err(DeskError::Invalid(_))
        ));
        let patch = super::super::BudgetPatch {
            amount: Some(400.0),
            ..Default::default()
        };
        assert!(d.patch_budget(kept[0].id, patch, By::you()).await.is_err());
        assert!(d.delete_budget(kept[0].id, By::you()).await.is_err());

        // The boss raising it waits for the person; lowering it applies.
        let raise = PlanEdit {
            budget: capped(60.0),
            why: Some("The tests are slow.".into()),
            ..Default::default()
        };
        let waits = d
            .change_plan(id, raise, Engine::Claude, false, &by_sarah())
            .await
            .unwrap();
        assert_eq!((waits.change, waits.applied), (Some(Change::Scope), false));
        assert_eq!(
            d.plan(id).await.unwrap().summary.budget,
            Some(PlanBudget::usd(40.0))
        );
        let r = waits.plan.summary.plan.revision;
        d.decide_plan(id, r, false, None, &By::you()).await.unwrap();
        let lower = PlanEdit {
            budget: capped(25.0),
            why: Some("Less is enough.".into()),
            ..Default::default()
        };
        let applies = d
            .change_plan(id, lower, Engine::Claude, false, &by_sarah())
            .await
            .unwrap();
        assert_eq!(
            (applies.change, applies.applied),
            (Some(Change::Minor), true)
        );
        let amounts: Vec<f64> = d
            .budget_list()
            .iter()
            .filter(|b| b.scope == BudgetScope::Plan(id))
            .map(|b| b.amount)
            .collect();
        assert_eq!(amounts, [25.0]);
        // A budget only goes with an approval through the Floor's answer.
        let now = d.plan(id).await.unwrap().summary.plan.revision;
        assert!(d
            .decide_plan(id, now, true, capped(90.0), &By::you())
            .await
            .is_err());
        // Dropped, its budget goes.
        d.drop_plan(id, OnDrop::Stop, &By::you()).await.unwrap();
        assert!(!d
            .budget_list()
            .iter()
            .any(|b| b.scope == BudgetScope::Plan(id)));
    })
    .await;
}

#[tokio::test]
async fn a_plan_at_its_warning_stops_starting_until_continue_and_stops_again_when_over() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let p = capped_plan(&d, dir.path(), &[("a", &[]), ("b", &[]), ("c", &[])], 10.0).await;
        let id = p.summary.plan.id;
        let plan = p.summary.plan.clone();
        let first = next(&d, id).await.remove(0);
        let s = d
            .plan_start(&plan, &first, &sarah(), &by_sarah())
            .await
            .unwrap();
        // Its first calls, made before it was handed its ticket, count too;
        // what it did an hour before isn't the plan's.
        spent_at(
            &d,
            s.id,
            0.5,
            chrono::Utc::now() - chrono::Duration::minutes(1),
        )
        .await;
        spent_at(
            &d,
            s.id,
            30.0,
            chrono::Utc::now() - chrono::Duration::hours(1),
        )
        .await;
        spent(&d, s.id, 8.0).await;
        d.breaker_spend(Some(vec![s.id])).await;

        // At its warning: the standing the runner passes starts nothing.
        let (status, ask) = d.plan_starts_held(Some("g-acme"), id).unwrap();
        assert_eq!(status, workforce_core::budget::Status::Warn);
        assert_eq!(
            ask,
            "The plan “Ship the new checkout” is at 85% of its total budget ($8.50 of $10). \
             Sarah has stopped starting sessions."
        );
        assert!(next(&d, id).await.is_empty());
        let second =
            plan::next_starts(&plan, &d.plan_tickets(id).await.unwrap(), 1, 4, None).remove(0);
        let refused = d
            .plan_start(&plan, &second, &sarah(), &by_sarah())
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("breaker"), "{refused}");
        // One Floor need, in the plan's group, that only the person answers.
        let subject = super::super::plan_subject(id);
        let needs: Vec<Need> = d
            .breaker_needs()
            .into_iter()
            .filter(|n| n.id == format!("breaker:{subject}"))
            .collect();
        assert_eq!(needs.len(), 1);
        assert_eq!(needs[0].group.as_deref(), Some("g-acme"));
        assert_eq!(
            needs[0].answer,
            Answer::Decide {
                at: DecideRef::Breaker {
                    subject: subject.clone()
                }
            }
        );
        assert!(needs[0]
            .why
            .as_deref()
            .unwrap()
            .contains("until it is over"));
        let detail = d.plan(id).await.unwrap();
        assert_eq!(detail.summary.held.as_deref(), Some(ask.as_str()));
        assert_eq!(detail.spend.as_ref().map(|s| s.used), Some(8.5));
        assert_eq!(d.session_held(s.id), None, "its session was only told");
        // Spent again at its warning, it is held once.
        d.breaker_spend(None).await;
        assert_eq!(d.breaker_needs().len(), 1);

        // Continue: it starts again.
        d.breaker_continue(&subject, By::you()).unwrap();
        assert_eq!(d.plan_starts_held(Some("g-acme"), id), None);
        assert_eq!(next(&d, id).await.len(), 2);
        let b = d
            .plan_start(&plan, &second, &sarah(), &by_sarah())
            .await
            .unwrap();

        // Over: its starts stop again, and its sessions' breakers hold them
        // from their next turns. Their tickets wait on the person.
        spent(&d, b.id, 2.0).await;
        d.breaker_spend(Some(vec![b.id])).await;
        let (status, _) = d.plan_starts_held(Some("g-acme"), id).unwrap();
        assert_eq!(status, workforce_core::budget::Status::Over);
        assert!(next(&d, id).await.is_empty());
        assert!(d.session_held(s.id).is_some(), "a sibling's spend counts");
        assert!(d.session_held(b.id).is_some());
        let held: Vec<bool> = d
            .plan(id)
            .await
            .unwrap()
            .tickets
            .iter()
            .map(|t| t.held.is_some())
            .collect();
        assert_eq!(held, [true, true, false]);

        // Its ticket done, what its session does next isn't the plan's.
        d.move_ticket(
            first.ticket,
            Move {
                state: TicketState::Done,
                before: None,
                after: None,
            },
            &By::you(),
        )
        .await
        .unwrap();
        spent(&d, s.id, 25.0).await;
        d.breaker_spend(Some(vec![s.id])).await;
        assert_eq!(d.plan(id).await.unwrap().spend.unwrap().used, 10.5);

        // The person raising the cap lets go of what it held.
        let raise = PlanEdit {
            revision: Some(d.plan(id).await.unwrap().summary.plan.revision),
            budget: capped(50.0),
            ..Default::default()
        };
        d.change_plan(id, raise, Engine::Claude, false, &By::you())
            .await
            .unwrap();
        assert_eq!(d.plan_starts_held(Some("g-acme"), id), None);
        assert_eq!(next(&d, id).await.len(), 1);
    })
    .await;
}

/// A session's spend with no ticket stamped goes to the plan whose ticket
/// it was handed then, one window a hand-out: back five minutes only after
/// a gap, never into what it worked on before, and to one plan at most.
#[tokio::test]
async fn ticketless_spend_goes_to_one_plan_by_each_hand_out() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let a = d
            .propose_plan(
                new_plan(dir.path(), &[("a1", &[]), ("a2", &[])]),
                &sarah(),
                &by_sarah(),
            )
            .await
            .unwrap();
        let b = d
            .propose_plan(new_plan(dir.path(), &[("b", &[])]), &sarah(), &by_sarah())
            .await
            .unwrap();
        let (pa, pb) = (a.summary.plan.id, b.summary.plan.id);
        let (a1, a2) = (a.tickets[0].ticket.ticket.id, a.tickets[1].ticket.ticket.id);
        let tb = b.tickets[0].ticket.ticket.id;
        let s = Uuid::new_v4();
        let t0 = chrono::Utc::now() - chrono::Duration::hours(3);
        let m = |n: i64| t0 + chrono::Duration::minutes(n);
        let c = d.0.store.pool().get().await.unwrap();
        // A, then B straight after, then A again after a gap.
        for (ticket, from, until) in [
            (a1, m(0), Some(m(30))),
            (tb, m(30), Some(m(60))),
            (a2, m(90), None),
        ] {
            c.execute(
                "INSERT INTO ticket_assignments (ticket, session, from_at, until_at) \
                 VALUES ($1, $2, $3, $4)",
                &[&ticket, &s, &from, &until],
            )
            .await
            .unwrap();
        }
        for (at, usd) in [
            (m(-30), 32.0), // long before: no plan's
            (m(-2), 1.0),   // its first calls: A's
            (m(29), 2.0),   // in A's first hand-out
            (m(35), 4.0),   // in B's
            (m(65), 8.0),   // in the gap: no plan's
            (m(87), 16.0),  // before A's second, after the gap: A's
        ] {
            c.execute(
                "INSERT INTO ledger (engine, key, session, at, input, usd, priced, billing) \
                 VALUES ('claude', $1, $2, $3, 10, $4, 'reported', 'plan')",
                &[&Uuid::new_v4().to_string(), &s, &at, &usd],
            )
            .await
            .unwrap();
        }
        let used =
            d.0.store
                .ledger_used_by_plan(chrono::DateTime::UNIX_EPOCH, &[pa, pb])
                .await
                .unwrap();
        assert_eq!(used.get(&pa).map(|u| u.usd), Some(19.0));
        assert_eq!(used.get(&pb).map(|u| u.usd), Some(4.0), "no grace into A's");
        // B's hand-out came straight after A's: what was just before it is
        // A's alone, not both.
        let total: f64 = used.values().map(|u| u.usd).sum();
        assert_eq!(total, 23.0);
    })
    .await;
}

/// A change of a plan's cap never reads as let go before it is counted
/// again; a tightening keeps the person's Continue, and only a loosening
/// the person approves forgets it.
#[tokio::test]
async fn a_change_of_cap_keeps_the_hold_until_counted_and_only_a_loosening_forgets_continue() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let p = capped_plan(&d, dir.path(), &[("a", &[]), ("b", &[])], 10.0).await;
        let id = p.summary.plan.id;
        let first = next(&d, id).await.remove(0);
        let s = d
            .plan_start(&p.summary.plan, &first, &sarah(), &by_sarah())
            .await
            .unwrap();
        spent(&d, s.id, 8.5).await;
        d.breaker_spend(Some(vec![s.id])).await;
        let subject = super::super::plan_subject(id);
        d.breaker_continue(&subject, By::you()).unwrap();
        assert_eq!(d.plan_starts_held(Some("g-acme"), id), None);

        // The boss lowers it, still at its warning: the Continue stands.
        let lower = PlanEdit {
            budget: capped(9.6),
            why: Some("Less is enough.".into()),
            ..Default::default()
        };
        let r = d
            .change_plan(id, lower, Engine::Claude, false, &by_sarah())
            .await
            .unwrap();
        assert!(r.applied);
        assert_eq!(d.plan_starts_held(Some("g-acme"), id), None, "still let go");

        // Over now; the person raises it, still over: held all along.
        spent(&d, s.id, 2.0).await;
        d.breaker_spend(Some(vec![s.id])).await;
        assert!(d.plan_starts_held(Some("g-acme"), id).is_some());
        let (_, mut events) = d.subscribe();
        let raise = |amount: f64| PlanEdit {
            budget: capped(amount),
            ..Default::default()
        };
        d.change_plan(id, raise(10.2), Engine::Claude, false, &By::you())
            .await
            .unwrap();
        assert!(d.plan_starts_held(Some("g-acme"), id).is_some());
        while let Ok(e) = events.try_recv() {
            if let DeskEvent::Breaker { breaker } = e {
                assert!(
                    breaker.subject != subject || breaker.level != crate::breaker::Level::Clear,
                    "never let go while counted again"
                );
            }
        }
        // Raised to where it is at its warning again: the person's raise
        // forgot the Continue, so it holds at the warning.
        d.change_plan(id, raise(12.5), Engine::Claude, false, &By::you())
            .await
            .unwrap();
        let (status, _) = d.plan_starts_held(Some("g-acme"), id).unwrap();
        assert_eq!(status, workforce_core::budget::Status::Warn);
    })
    .await;
}

/// A plan stays on its board while a ticket of it is starting, or while one
/// is filed in another tracker: nothing of it moves.
#[tokio::test]
async fn a_plan_isnt_moved_while_a_ticket_starts_or_one_is_filed_elsewhere() {
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
        let from = p.summary.plan.board;
        let (to, made) = d.board_in_group(None, "Vellev", Some("VEL")).await.unwrap();
        assert!(made);
        let keys = |p: &PlanDetail| -> Vec<String> {
            p.tickets.iter().map(|t| t.ticket.key.clone().unwrap()).collect()
        };
        let before = keys(&d.plan(id).await.unwrap());

        // Starting.
        let p = d.approve_plan(id, 1, &By::you()).await.unwrap();
        let a = p.tickets[0].ticket.ticket.id;
        let mut t = d.ticket_of(a).await.unwrap();
        let session = Uuid::new_v4();
        d.0.store
            .begin_start(&mut t, id, 9_000.0, session, crew::now())
            .await
            .unwrap();
        let e = d.move_plan(id, to.id, &by_sarah()).await.unwrap_err();
        assert!(e.to_string().contains("try the move again"), "{e}");
        let still = d.plan(id).await.unwrap();
        assert_eq!(still.summary.plan.board, from);
        assert_eq!(keys(&still), before);
        d.0.store.end_start(a, session).await.unwrap();

        // Filed in another tracker.
        let client = d.0.store.pool().get().await.unwrap();
        let ext = serde_json::json!({"system": "linear", "key": "ENG-9", "url": "https://linear.app/x/ENG-9",
                                     "remoteUpdatedAt": "2026-11-14T00:00:00Z", "synced": true,
                                     "link": Uuid::new_v4()});
        client
            .execute("UPDATE tickets SET external = $2 WHERE id = $1", &[&a, &ext])
            .await
            .unwrap();
        let e = d.move_plan(id, to.id, &by_sarah()).await.unwrap_err();
        assert!(e.to_string().contains("another tracker"), "{e}");
        let still = d.plan(id).await.unwrap();
        assert_eq!(still.summary.plan.board, from);
        assert!(still.tickets.iter().all(|t| t.ticket.ticket.board == from));

        // Neither: it moves, renumbered there.
        client
            .execute("UPDATE tickets SET external = NULL WHERE id = $1", &[&a])
            .await
            .unwrap();
        let moved = d.move_plan(id, to.id, &by_sarah()).await.unwrap();
        assert_eq!(keys(&moved), ["VEL-1", "VEL-2"]);
    })
    .await;
}
