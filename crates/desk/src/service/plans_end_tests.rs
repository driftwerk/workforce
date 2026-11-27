//! A plan whose work is done finishes once, a running one is ended with its
//! tickets left as they are, and a finished or ended one is removed with
//! nothing of it left behind but its tickets.

use std::path::Path;

use workforce_postgres::test_support::with_test_pool;

use super::super::tests::desk;
use super::*;
use crate::boss_watch::{NewWatch, WatchKind};
use crate::goal::Measure;
use crate::open_item::{NewItem, OpenKind, OpenState};
use crate::open_item_store::Opened;
use crate::plan::PlanTicket;
use crate::service::NewBoard;
use crate::store::build_pool;

fn by_sarah() -> By {
    By::manager("sarah", "Sarah", "The person asked for the checkout.")
}

fn new_plan(dir: &Path, titles: &[(&str, &[usize])]) -> NewPlan {
    NewPlan {
        draft: Draft {
            goal: "Ship the new checkout".into(),
            tickets: titles
                .iter()
                .map(|(title, needs)| DraftTicket {
                    title: (*title).into(),
                    folder: dir.to_string_lossy().into_owned(),
                    needs: needs.to_vec(),
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

/// Plan `titles`, approved and running.
async fn running(d: &Desk, dir: &Path, titles: &[(&str, &[usize])]) -> PlanDetail {
    let p = d
        .propose_plan(
            new_plan(dir, titles),
            &Party::manager("sarah", "Sarah"),
            &by_sarah(),
        )
        .await
        .unwrap();
    d.approve_plan(p.summary.plan.id, 1, &By::you())
        .await
        .unwrap()
}

async fn moved(d: &Desk, ticket: Uuid, state: TicketState) {
    let mv = Move {
        state,
        before: None,
        after: None,
    };
    d.move_ticket(ticket, mv, &By::you()).await.unwrap();
}

fn ids(p: &PlanDetail) -> Vec<Uuid> {
    p.tickets.iter().map(|t| t.ticket.ticket.id).collect()
}

/// A session started for `p`'s ticket `ticket`, in `dir`: Doing, in its
/// boss's charge.
async fn started(d: &Desk, p: &PlanDetail, ticket: Uuid, dir: &Path) -> Uuid {
    let start = Start {
        repo: None,
        ticket,
        engine: Engine::Claude,
        folder: dir.to_string_lossy().into_owned(),
        view: None,
        worktree: false,
    };
    d.plan_start(
        &p.summary.plan,
        &start,
        &Party::manager("sarah", "Sarah"),
        &by_sarah(),
    )
    .await
    .unwrap()
    .id
}

async fn watching(d: &Desk, plan: Uuid) -> Uuid {
    let w = NewWatch {
        boss: "sarah".into(),
        person: "kim".into(),
        kind: WatchKind::Pr,
        repo: Some("acme/web".into()),
        number: Some(7),
        run_id: None,
        until_merged: true,
        note: "merge when green".into(),
        session: None,
        plan: Some(plan),
        due_at: None,
        expires_at: crew::now() + chrono::Duration::hours(4),
    };
    match d.store().watch_open(&w).await.unwrap() {
        crate::boss_watch::Opened::Started(w) => w.id,
        other => panic!("{other:?}"),
    }
}

async fn asking(d: &Desk, plan: Uuid) -> Uuid {
    let item = NewItem::ask("sarah", OpenKind::Ask, None, "Is the checkout shipped?");
    let item = match d.store().item_open(&item).await.unwrap() {
        Opened::New(i) | Opened::Again(i) => i,
    };
    d.store()
        .item_set_goal(item.id, plan, &Default::default(), crew::now())
        .await
        .unwrap();
    item.id
}

async fn count(d: &Desk, sql: &str, id: Uuid) -> i64 {
    let client = d.store().pool().get().await.unwrap();
    client.query_one(sql, &[&id]).await.unwrap().get(0)
}

fn ticket(id: u128, state: TicketState) -> PlanTicket {
    serde_json::from_value(serde_json::json!({
        "id": Uuid::from_u128(id),
        "state": state.as_str(),
        "rank": 0.0,
    }))
    .unwrap()
}

#[test]
fn a_plan_closes_once_its_work_is_done_and_after_its_bosss_turn_while_its_goal_isnt_met() {
    use crate::plan::Closing::{AfterTurn, Now};
    let plan = |state, paused, result: bool, met: bool| Plan {
        id: Uuid::nil(),
        board: Uuid::nil(),
        group: None,
        boss: "sarah".into(),
        goal: "Ship".into(),
        state,
        paused,
        paused_why: None,
        paused_until: None,
        paused_at: None,
        paused_for: None,
        parallel: None,
        revision: 1,
        approved_revision: Some(1),
        created_at: crew::now(),
        updated_at: crew::now(),
        result: result.then(|| GoalResult {
            words: "shipped".into(),
            parts: vec![Measure::Judged {
                evidence: "the work".into(),
            }],
        }),
        agreed: None,
        agreed_at: None,
        read_back_at: None,
        goal_ticket: None,
        met: met.then(|| Met {
            at: crew::now(),
            by: crate::goal::MetBy::Person,
            why: "they said so".into(),
        }),
    };
    let settled = [
        ticket(1, TicketState::Done),
        ticket(2, TicketState::Dropped),
    ];
    let open = [ticket(1, TicketState::Done), ticket(2, TicketState::Doing)];
    let closing = |state, paused, result, met, t: &[PlanTicket]| {
        plan::closing(&plan(state, paused, result, met), t)
    };
    let running = PlanState::Running;
    assert_eq!(closing(running, None, false, false, &settled), Some(Now));
    assert_eq!(
        closing(
            PlanState::Paused,
            Some(Paused::Budget),
            false,
            false,
            &settled
        ),
        Some(Now)
    );
    assert_eq!(
        closing(
            PlanState::Paused,
            Some(Paused::Person),
            false,
            false,
            &settled
        ),
        None
    );
    for state in [PlanState::Proposed, PlanState::Done, PlanState::Dropped] {
        assert_eq!(
            closing(state, None, false, false, &settled),
            None,
            "{state:?}"
        );
    }
    assert_eq!(closing(running, None, false, false, &open), None);
    assert_eq!(closing(running, None, false, false, &[]), None);
    assert_eq!(
        closing(running, None, true, false, &settled),
        Some(AfterTurn)
    );
    assert_eq!(closing(running, None, true, false, &open), None);
    assert_eq!(closing(running, None, true, true, &open), Some(Now));
    assert_eq!(closing(running, None, true, true, &settled), Some(Now));
}

#[tokio::test]
async fn a_plan_whose_work_is_done_finishes_once_and_is_removed_leaving_only_its_tickets() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let p = running(&d, dir.path(), &[("a", &[]), ("b", &[0])]).await;
        let id = p.summary.plan.id;
        let [a, b] = ids(&p)[..] else { panic!() };
        let watch = watching(&d, id).await;
        let ask = asking(&d, id).await;
        moved(&d, a, TicketState::Done).await;
        let refused = d.finish_plan(id, &by_sarah()).await.unwrap_err();
        assert!(refused.to_string().contains("aren't done yet"), "{refused}");
        assert!(matches!(
            d.remove_plan(id, &By::you()).await,
            Err(DeskError::Conflict(_))
        ));
        moved(&d, b, TicketState::Done).await;
        assert_eq!(
            d.plan_closing(&d.plan(id).await.unwrap().summary.plan)
                .await
                .unwrap(),
            Some(crate::plan::Closing::Now)
        );

        let done = d.finish_plan(id, &by_sarah()).await.unwrap().unwrap();
        assert_eq!(done.summary.plan.state, PlanState::Done);
        assert!(
            d.finish_plan(id, &by_sarah()).await.unwrap().is_none(),
            "once"
        );
        assert_eq!(d.plan_closing(&done.summary.plan).await.unwrap(), None);
        let ended = d.store().watches_undelivered().await.unwrap();
        assert!(ended.iter().all(|w| w.id != watch), "ended without a word");
        assert!(d.store().watches_open_all().await.unwrap().is_empty());
        let item = d.store().open_item(ask).await.unwrap().unwrap();
        assert_eq!(item.state, OpenState::Closed);

        let mut events = d.events();
        d.remove_plan(id, &By::you()).await.unwrap();
        let mut removed = false;
        while let Ok(e) = events.try_recv() {
            removed |= matches!(e, DeskEvent::PlanRemoved { id: gone } if gone == id);
        }
        assert!(removed, "announced");
        assert!(matches!(d.plan(id).await, Err(DeskError::NotFound)));
        assert!(d.plans(None, None).await.unwrap().is_empty());
        for t in [a, b] {
            let v = d.ticket(t).await.unwrap();
            assert_eq!(v.ticket.state, TicketState::Done, "kept on its board");
            assert_eq!(d.store().plan_of_ticket(t).await.unwrap(), None);
        }
        assert!(
            d.ticket(b).await.unwrap().ticket.needs.is_empty(),
            "its need went"
        );
        for sql in [
            "SELECT count(*) FROM plan_revisions WHERE plan = $1",
            "SELECT count(*) FROM boss_watches WHERE plan = $1",
            "SELECT count(*) FROM boss_open_items WHERE plan = $1",
            "SELECT count(*) FROM ticket_needs WHERE plan = $1",
            "SELECT count(*) FROM tickets WHERE plan = $1",
        ] {
            assert_eq!(count(&d, sql, id).await, 0, "{sql}");
        }
        assert!(matches!(
            d.remove_plan(id, &By::you()).await,
            Err(DeskError::NotFound)
        ));
    })
    .await;
}

#[tokio::test]
async fn ending_a_plan_leaves_its_tickets_as_they_are_and_its_sessions_to_the_person() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let p = running(&d, dir.path(), &[("a", &[]), ("b", &[]), ("c", &[])]).await;
        let id = p.summary.plan.id;
        let [a, b, c] = ids(&p)[..] else { panic!() };
        let s = started(&d, &p, a, dir.path()).await;
        moved(&d, c, TicketState::Done).await;
        let watch = watching(&d, id).await;

        let ended = d.end_plan(id, &By::you()).await.unwrap();
        assert_eq!(ended.summary.plan.state, PlanState::Dropped);
        assert_eq!(ended.summary.plan.paused, None);
        let state = |t: Uuid| {
            ended
                .tickets
                .iter()
                .find(|v| v.ticket.ticket.id == t)
                .map(|v| v.ticket.ticket.state)
        };
        assert_eq!(state(a), Some(TicketState::Doing));
        assert_eq!(state(b), Some(TicketState::Todo));
        assert_eq!(state(c), Some(TicketState::Done));
        assert!(
            d.charge_of(s).is_some_and(|c| !c.live()),
            "the person's now"
        );
        assert!(d.view(s).is_ok_and(|v| v.state != SessionState::Closed));
        assert!(d
            .store()
            .watches_open_all()
            .await
            .unwrap()
            .iter()
            .all(|w| w.id != watch));
        let again = d.end_plan(id, &By::you()).await.unwrap();
        assert_eq!(
            again.summary.plan.updated_at, ended.summary.plan.updated_at,
            "once"
        );
        assert!(matches!(
            d.finish_plan(id, &by_sarah()).await,
            Err(DeskError::Conflict(_))
        ));

        // What it left to do can be taken on by another plan.
        let mut next = new_plan(dir.path(), &[]);
        next.draft.tickets = vec![DraftTicket {
            adopt: Some(b),
            folder: dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        }];
        let taken = d
            .propose_plan(next, &Party::manager("sarah", "Sarah"), &by_sarah())
            .await
            .unwrap();
        assert_eq!(
            d.store().plan_of_ticket(b).await.unwrap(),
            Some(taken.summary.plan.id)
        );

        d.remove_plan(id, &By::you()).await.unwrap();
        assert_eq!(d.ticket(a).await.unwrap().ticket.state, TicketState::Doing);
        assert_eq!(
            d.store().plan_of_ticket(b).await.unwrap(),
            Some(taken.summary.plan.id),
            "another plan's ticket stays its"
        );
    })
    .await;
}

#[tokio::test]
async fn only_a_running_or_paused_plan_ends_and_only_desks_pause_finishes_by_its_tickets() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let proposed = d
            .propose_plan(
                new_plan(dir.path(), &[("a", &[])]),
                &Party::manager("sarah", "Sarah"),
                &by_sarah(),
            )
            .await
            .unwrap();
        assert!(matches!(
            d.end_plan(proposed.summary.plan.id, &By::you()).await,
            Err(DeskError::Conflict(_))
        ));

        let p = running(&d, dir.path(), &[("a", &[])]).await;
        let id = p.summary.plan.id;
        d.pause_plan(id, Paused::Person, &By::you()).await.unwrap();
        moved(&d, ids(&p)[0], TicketState::Done).await;
        let refused = d.finish_plan(id, &by_sarah()).await.unwrap_err();
        assert!(
            refused.to_string().contains("person paused it"),
            "{refused}"
        );
        d.resume_plan(id, &[Paused::Person], &By::you())
            .await
            .unwrap();
        d.pause_plan(id, Paused::Budget, &By::desk("Its budget is nearly spent."))
            .await
            .unwrap();
        let done = d.finish_plan(id, &by_sarah()).await.unwrap().unwrap();
        assert_eq!(done.summary.plan.state, PlanState::Done);
        assert_eq!(done.summary.plan.paused, None);
        assert!(matches!(
            d.end_plan(id, &By::you()).await,
            Err(DeskError::Conflict(_))
        ));

        let paused = running(&d, dir.path(), &[("b", &[])]).await;
        d.pause_plan(paused.summary.plan.id, Paused::Person, &By::you())
            .await
            .unwrap();
        let ended = d
            .end_plan(paused.summary.plan.id, &By::you())
            .await
            .unwrap();
        assert_eq!(ended.summary.plan.state, PlanState::Dropped);
        assert_eq!(ended.tickets[0].ticket.ticket.state, TicketState::Todo);
        assert!(matches!(
            d.remove_plan(Uuid::new_v4(), &By::you()).await,
            Err(DeskError::NotFound)
        ));
    })
    .await;
}

#[tokio::test]
async fn a_goal_not_read_met_closes_after_its_bosss_turn_once_every_ticket_is_settled() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let mut new = new_plan(dir.path(), &[("a", &[])]);
        new.result = Some(GoalResult {
            words: "the bugs are fixed".into(),
            parts: vec![Measure::PrMerged {
                repo: "acme/web".into(),
                pr: crate::goal::PrPick::Number(7),
            }],
        });
        let p = d
            .propose_plan(new, &Party::manager("sarah", "Sarah"), &by_sarah())
            .await
            .unwrap();
        let id = p.summary.plan.id;
        let p = d.approve_plan(id, 1, &By::you()).await.unwrap();
        assert_eq!(d.plan_closing(&p.summary.plan).await.unwrap(), None);
        moved(&d, ids(&p)[0], TicketState::Done).await;
        let now = d.plan(id).await.unwrap().summary.plan;
        assert_eq!(
            d.plan_closing(&now).await.unwrap(),
            Some(crate::plan::Closing::AfterTurn)
        );
        let done = d.finish_plan(id, &by_sarah()).await.unwrap().unwrap();
        assert_eq!(done.summary.plan.state, PlanState::Done);
        assert_eq!(done.summary.plan.met, None, "never said met");
    })
    .await;
}

#[tokio::test]
async fn the_persons_routes_end_and_remove_a_plan() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        d.create_board(NewBoard {
            name: "Acme".into(),
            group: None,
            prefix: Some("ACM".into()),
        })
        .await
        .unwrap();
        let p = running(&d, dir.path(), &[("a", &[])]).await;
        let id = p.summary.plan.id;
        let closed = crate::plan_api::api_routes(d.clone(), crate::testing::deny_all());
        let open = crate::plan_api::api_routes(d.clone(), crate::testing::allow_all());
        for rest in ["end", "remove"] {
            let uri = format!("/api/desk/plans/{id}/{rest}");
            assert_eq!(super::tests::call(&closed, "POST", &uri, None).await.0, 401);
        }
        let remove = format!("/api/desk/plans/{id}/remove");
        assert_eq!(
            super::tests::call(&open, "POST", &remove, None).await.0,
            409
        );
        let (status, ended) =
            super::tests::call(&open, "POST", &format!("/api/desk/plans/{id}/end"), None).await;
        assert_eq!(status, 200);
        assert_eq!(ended["state"], "dropped");
        let (status, removed) = super::tests::call(&open, "POST", &remove, None).await;
        assert_eq!(status, 200);
        assert_eq!(removed["removed"], id.to_string());
        assert_eq!(
            super::tests::call(&open, "POST", &remove, None).await.0,
            404
        );
        let gone = format!("/api/desk/plans/{}/end", Uuid::new_v4());
        assert_eq!(super::tests::call(&open, "POST", &gone, None).await.0, 404);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plan_ending_closes_its_plan_limit_asks_parked_ones_too() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let p = running(&d, dir.path(), &[("a", &[])]).await;
        let id = p.summary.plan.id;
        let limit_ask = async |key: String| {
            let mut new = NewItem::ask("sarah", OpenKind::Ask, None, "Raise the limit?");
            new.dedupe_key = key;
            match d.store().item_open(&new).await.unwrap() {
                Opened::New(i) | Opened::Again(i) => i.id,
            }
        };
        let open = limit_ask(format!("{}open", crate::plan::limit_prefix(id))).await;
        let parked = limit_ask(format!("{}parked", crate::plan::limit_prefix(id))).await;
        d.store()
            .item_said(parked, crew::now(), true)
            .await
            .unwrap();
        let state = async |item| d.store().open_item(item).await.unwrap().unwrap().state;
        assert_eq!(state(parked).await, OpenState::Parked);

        d.end_plan(id, &By::you()).await.unwrap();
        assert_eq!(state(open).await, OpenState::Closed);
        assert_eq!(state(parked).await, OpenState::Closed);
    })
    .await;
}
