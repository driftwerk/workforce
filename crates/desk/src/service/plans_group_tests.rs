//! A plan's group is the group its work belongs to: picked from its work,
//! changed by the person or its boss with its sessions, kept within its
//! boss's part of Desk, and ungrouped with its group.

use serde_json::json;
use workforce_postgres::test_support::with_test_pool;

use super::super::tests::desk;
use super::tests::{by_sarah, call, new_plan, sarah};
use super::*;
use crate::plan_group::{Picked, Why};
use crate::scope::Scope;
use crate::service::{session_item, Boss};
use crate::store::build_pool;

pub(super) fn boss(agent: &str, name: &str, group: Option<&str>) -> Boss {
    Boss {
        agent: agent.into(),
        name: name.into(),
        scope: Scope::of(group),
        level: "act".into(),
    }
}

async fn with_groups(d: &Desk) {
    d.change_layout(|l| {
        l.create("g-mantis", "Mantis", &[], None)?
            .create("g-atlas", "Atlas", &[], None)
    })
    .await
    .unwrap();
}

fn group_of(d: &Desk, session: Uuid) -> Option<String> {
    d.view(session).unwrap().group.map(|g| g.id)
}

fn start_of(p: &PlanDetail, i: usize) -> Start {
    let t = &p.tickets[i];
    Start {
        repo: None,
        ticket: t.ticket.ticket.id,
        engine: Engine::Claude,
        folder: t.folder.clone().unwrap_or_default(),
        view: None,
        worktree: false,
    }
}

/// The plans announced on `events` so far, and the Activity noted.
fn heard(
    events: &mut tokio::sync::broadcast::Receiver<DeskEvent>,
) -> (Vec<PlanSummary>, Vec<crew::Event>) {
    std::iter::from_fn(|| events.try_recv().ok()).fold(
        (Vec::new(), Vec::new()),
        |(mut plans, mut noted), e| {
            match e {
                DeskEvent::Plan { plan } => plans.push(*plan),
                DeskEvent::Activity { event } => noted.push(*event),
                _ => {}
            }
            (plans, noted)
        },
    )
}

#[tokio::test]
async fn a_plans_group_lifecycle_propose_regroup_start_and_a_deleted_group() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let mono = dir.path().join("mantis").join("monorepo");
        std::fs::create_dir_all(&mono).unwrap();
        let d = desk(pool, dir.path()).await;
        with_groups(&d).await;

        // Its folders name Mantis, so its work is Mantis's.
        let mut new = new_plan(&mono, &[("a", &[]), ("b", &[]), ("c", &[])]);
        let picked = d.work_group(&new.draft.tickets, |_| true);
        assert_eq!(
            picked,
            Some(Picked {
                group: "g-mantis".into(),
                why: Why::Named("mantis".into()),
            })
        );
        new.group = picked.map(|p| p.group);
        let p = d.propose_plan(new, &sarah(), &by_sarah()).await.unwrap();
        let id = p.summary.plan.id;
        assert_eq!(p.summary.plan.group.as_deref(), Some("g-mantis"));
        assert_eq!(p.summary.group_name.as_deref(), Some("Mantis"));
        let board = d.board_of(p.summary.plan.board).await.unwrap();
        assert_eq!(board.group.as_deref(), Some("g-mantis"));
        let keys = |p: &PlanDetail| -> Vec<Option<String>> {
            p.tickets.iter().map(|t| t.ticket.key.clone()).collect()
        };
        let was = keys(&p);

        // To Atlas: heard, noted, and its tickets stay on their board under
        // their keys.
        let (_, mut events) = d.subscribe();
        let moved = d
            .regroup_plan(id, Some("g-atlas".into()), &By::you())
            .await
            .unwrap();
        assert_eq!(moved.summary.plan.group.as_deref(), Some("g-atlas"));
        assert_eq!(moved.summary.plan.board, board.id);
        assert_eq!(keys(&moved), was);
        let (plans, noted) = heard(&mut events);
        assert!(
            plans
                .iter()
                .any(|s| s.plan.group.as_deref() == Some("g-atlas")),
            "{plans:?}"
        );
        assert!(
            noted
                .iter()
                .any(|e| e.kind == Kind::PlanChanged && e.text.contains("to Atlas")),
            "{noted:?}"
        );
        assert_eq!(d.plan_needs()[0].group.as_deref(), Some("g-atlas"));
        // The same group again changes nothing.
        let again = d
            .regroup_plan(id, Some("g-atlas".into()), &By::you())
            .await
            .unwrap();
        assert_eq!(again.summary.plan.updated_at, moved.summary.plan.updated_at);

        // Started, its sessions are in Atlas.
        let p = d.approve_plan(id, 1, &By::you()).await.unwrap();
        let read = p.summary.plan.clone();
        let a = d
            .plan_start(&read, &start_of(&p, 0), &sarah(), &by_sarah())
            .await
            .unwrap();
        let b = d
            .plan_start(&read, &start_of(&p, 1), &sarah(), &by_sarah())
            .await
            .unwrap();
        assert_eq!(group_of(&d, a.id).as_deref(), Some("g-atlas"));
        assert_eq!(group_of(&d, b.id).as_deref(), Some("g-atlas"));

        // The person takes b's session out of Atlas; back to Mantis, a's
        // session moves with the plan and b's stays where they put it.
        d.change_layout(|l| l.remove(&session_item(b.id)))
            .await
            .unwrap();
        d.regroup_plan(id, Some("g-mantis".into()), &By::you())
            .await
            .unwrap();
        assert_eq!(group_of(&d, a.id).as_deref(), Some("g-mantis"));
        assert_eq!(group_of(&d, b.id), None);

        // A start the runner read while the plan was in Atlas starts in
        // Mantis, where the plan is now.
        let c = d
            .plan_start(&read, &start_of(&p, 2), &sarah(), &by_sarah())
            .await
            .unwrap();
        assert_eq!(group_of(&d, c.id).as_deref(), Some("g-mantis"));

        // Mantis deleted: the plan is ungrouped with its board, heard so,
        // and on the Floor in Ungrouped.
        let (_, mut events) = d.subscribe();
        d.change_layout(|l| l.delete("g-mantis")).await.unwrap();
        let now = d.plan(id).await.unwrap();
        assert_eq!(
            (
                now.summary.plan.group.clone(),
                now.summary.group_name.clone()
            ),
            (None, None)
        );
        let (plans, _) = heard(&mut events);
        assert!(
            plans
                .iter()
                .any(|s| s.plan.id == id && s.plan.group.is_none()),
            "{plans:?}"
        );
        assert_eq!(d.floor_plans().await.unwrap()[0].group, None);
        assert_eq!(group_of(&d, a.id), None);
    })
    .await;
}

#[tokio::test]
async fn a_plan_doesnt_move_across_a_boss_boundary_nor_to_a_group_there_isnt_nor_once_finished() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        with_groups(&d).await;
        d.change_layout(|l| {
            l.create("g-acme", "Acme", &[], None)?
                .create("g-sealed", "Sealed", &[], None)
        })
        .await
        .unwrap();
        d.set_bosses(vec![
            boss("sarah", "Sarah", None),
            boss("ada", "Ada", Some("g-acme")),
        ]);
        d.bound_groups(["g-sealed".to_string()]).await;
        let p = d
            .propose_plan(new_plan(dir.path(), &[("a", &[])]), &sarah(), &by_sarah())
            .await
            .unwrap();
        let id = p.summary.plan.id;
        let refused = |r: Result<PlanDetail, DeskError>| r.unwrap_err();

        let e = refused(d.regroup_plan(id, Some("g-acme".into()), &By::you()).await);
        assert!(
            matches!(&e, DeskError::Conflict(w) if w.starts_with("Acme is Ada's to run")),
            "{e}"
        );
        let e = refused(
            d.regroup_plan(id, Some("g-sealed".into()), &By::you())
                .await,
        );
        assert!(
            matches!(&e, DeskError::Conflict(w) if w.starts_with("No boss runs Sealed")),
            "{e}"
        );
        let e = refused(d.regroup_plan(id, Some("g-gone".into()), &By::you()).await);
        assert!(matches!(e, DeskError::Invalid(_)), "{e}");
        assert_eq!(d.plan(id).await.unwrap().summary.plan.group, None);

        // Within Sarah's part, it moves; a new plan of a group the sidebar
        // hasn't is refused before anything is made.
        d.regroup_plan(id, Some("g-mantis".into()), &By::you())
            .await
            .unwrap();
        let mut gone = new_plan(dir.path(), &[("b", &[])]);
        gone.group = Some("g-gone".into());
        assert!(matches!(
            d.propose_plan(gone, &sarah(), &by_sarah()).await,
            Err(DeskError::Invalid(_))
        ));
        assert_eq!(d.plans(None, None).await.unwrap().len(), 1);

        d.drop_plan(id, OnDrop::Leave, &By::you()).await.unwrap();
        let e = refused(d.regroup_plan(id, None, &By::you()).await);
        assert!(matches!(e, DeskError::Invalid(_)), "{e}");
    })
    .await;
}

#[tokio::test]
async fn a_plan_takes_on_tickets_and_boards_of_any_group_its_boss_runs() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        with_groups(&d).await;
        d.change_layout(|l| l.create("g-acme", "Acme", &[], None))
            .await
            .unwrap();
        d.set_bosses(vec![
            boss("sarah", "Sarah", None),
            boss("ada", "Ada", Some("g-acme")),
        ]);
        let board = |name: &str, group: Option<&str>| {
            d.create_board(NewBoard {
                name: name.into(),
                group: group.map(str::to_string),
                prefix: None,
            })
        };
        let loose = board("Loose", None).await.unwrap();
        let atlas = board("Atlas", Some("g-atlas")).await.unwrap();
        let acme = board("Acme", Some("g-acme")).await.unwrap();
        let taken = d
            .create_ticket(
                super::super::board::NewTicket {
                    board: atlas.id,
                    title: "Fix the Atlas build".into(),
                    body: String::new(),
                    state: Some(TicketState::Todo),
                    needs: vec![],
                },
                &By::you(),
            )
            .await
            .unwrap();
        let folder = dir.path().to_string_lossy().into_owned();
        let mut new = new_plan(dir.path(), &[("a", &[])]);
        new.group = Some("g-mantis".into());
        new.board = Some(loose.id);
        new.draft.tickets.push(DraftTicket {
            adopt: Some(taken.ticket.id),
            folder: folder.clone(),
            ..Default::default()
        });
        let p = d.propose_plan(new, &sarah(), &by_sarah()).await.unwrap();
        assert_eq!(
            (p.summary.plan.group.as_deref(), p.summary.plan.board),
            (Some("g-mantis"), loose.id)
        );
        assert!(p.tickets.iter().any(|t| t.adopted));

        let mut across = new_plan(dir.path(), &[("b", &[])]);
        across.board = Some(acme.id);
        let e = d
            .propose_plan(across, &sarah(), &by_sarah())
            .await
            .unwrap_err();
        assert!(
            e.to_string().contains("a group the plan's boss runs"),
            "{e}"
        );
        let id = p.summary.plan.id;
        let e = d.move_plan(id, acme.id, &By::you()).await.unwrap_err();
        assert!(
            e.to_string().contains("a group the plan's boss runs"),
            "{e}"
        );
        let moved = d.move_plan(id, atlas.id, &By::you()).await.unwrap();
        assert_eq!(
            (
                moved.summary.plan.board,
                moved.summary.plan.group.as_deref()
            ),
            (atlas.id, Some("g-mantis"))
        );
    })
    .await;
}

#[tokio::test]
async fn the_persons_route_regroups_a_plan_and_refuses_what_it_cant() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        with_groups(&d).await;
        d.change_layout(|l| l.create("g-acme", "Acme", &[], None))
            .await
            .unwrap();
        d.set_bosses(vec![
            boss("sarah", "Sarah", None),
            boss("ada", "Ada", Some("g-acme")),
        ]);
        let p = d
            .propose_plan(new_plan(dir.path(), &[("a", &[])]), &sarah(), &by_sarah())
            .await
            .unwrap();
        let uri = format!("/api/desk/plans/{}/group", p.summary.plan.id);
        let closed = crate::plan_api::api_routes(d.clone(), crate::testing::deny_all());
        assert_eq!(
            call(&closed, "POST", &uri, Some(json!({ "group": "g-atlas" })))
                .await
                .0,
            401
        );
        let hub = crate::plan_api::api_routes(d.clone(), crate::testing::allow_all());
        let (status, body) = call(&hub, "POST", &uri, Some(json!({ "group": "g-atlas" }))).await;
        assert_eq!((status, body["group"].as_str()), (200, Some("g-atlas")));
        assert_eq!(body["groupName"].as_str(), Some("Atlas"));
        let (status, body) = call(&hub, "POST", &uri, Some(json!({ "group": null }))).await;
        assert_eq!((status, &body["group"]), (200, &serde_json::Value::Null));
        assert_eq!(call(&hub, "POST", &uri, Some(json!({}))).await.0, 400);
        assert_eq!(call(&hub, "POST", &uri, None).await.0, 400);
        assert_eq!(
            call(&hub, "POST", &uri, Some(json!({ "group": "g-acme" })))
                .await
                .0,
            409
        );
        assert_eq!(
            d.plan(p.summary.plan.id).await.unwrap().summary.plan.group,
            None
        );
    })
    .await;
}

#[tokio::test]
async fn a_plans_machine_calls_carry_its_group_only_when_that_is_a_boundary() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        with_groups(&d).await;
        d.change_layout(|l| l.create("g-acme", "Acme", &[], None))
            .await
            .unwrap();
        d.set_bosses(vec![
            boss("sarah", "Sarah", None),
            boss("ada", "Ada", Some("g-acme")),
        ]);
        let p = d
            .propose_plan(new_plan(dir.path(), &[("a", &[])]), &sarah(), &by_sarah())
            .await
            .unwrap()
            .summary
            .plan;
        let plan_in = |group: Option<&str>| Plan {
            group: group.map(str::to_string),
            ..p.clone()
        };
        assert_eq!(d.relay_group(&plan_in(None)), None);
        assert_eq!(d.relay_group(&plan_in(Some("g-mantis"))), None);
        assert_eq!(d.relay_group(&plan_in(Some("g-acme"))), Some("g-acme"));
    })
    .await;
}
