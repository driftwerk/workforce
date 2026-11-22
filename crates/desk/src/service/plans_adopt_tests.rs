//! Plans that take on tickets already on a board, a synced one too: no copy
//! is made, their moves go out to their issues, a ticket someone holds is
//! refused, and dropping the plan (or the ticket from it) lets them go
//! where they are.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::{json, Value};
use workforce_postgres::test_support::with_test_pool;

use super::super::tests::desk;
use super::*;
use crate::board::{Filter, System, Via};
use crate::board_sync::{Remote, RemoteError};
use crate::plan_store::Adopt;
use crate::service::{LinkBoard, NewBoard};
use crate::store::build_pool;

/// A Jira whose search answers `n` issues, ACME-1 up, each in To Do.
struct Issues(usize);

#[async_trait]
impl Remote for Issues {
    async fn tools(&self) -> Result<Vec<String>, RemoteError> {
        Ok(crate::board_sync::JIRA_TOOLS
            .iter()
            .map(|t| t.to_string())
            .collect())
    }

    async fn call(&self, tool: &str, args: Value) -> Result<Value, RemoteError> {
        let issue = |i: usize| {
            json!({ "key": format!("ACME-{i}"), "summary": format!("Fix bug {i}"),
                "status": "To Do", "updated": format!("2026-11-09T10:00:{i:02}.000+0000"),
                "labels": ["bug"], "description_html": "<p>It breaks.</p>" })
        };
        match tool {
            "jira.search" => Ok(json!({
                "issues": (1..=self.0).map(issue).collect::<Vec<_>>(),
                "is_last": true,
            })),
            "jira.get_issue" => {
                let key = args["issue_key"].as_str().unwrap_or_default();
                let n: usize = key
                    .trim_start_matches("ACME-")
                    .parse()
                    .map_err(|_| RemoteError::Failed("404".into()))?;
                Ok(issue(n))
            }
            other => Err(RemoteError::NotAllowed(other.into())),
        }
    }
}

/// A board synced with Jira's ACME, holding `n` issues in To do.
async fn synced(d: &Desk, n: usize) -> Board {
    let b = d
        .create_board(NewBoard {
            name: "Acme".into(),
            group: None,
            prefix: None,
        })
        .await
        .unwrap();
    let link = LinkBoard {
        system: System::Jira,
        repo: None,
        target: "ACME".into(),
        target_id: None,
        via: Via::ThisHub,
        agent: None,
        workspace: None,
        filter: Filter::Label("bug".into()),
        me: None,
        me_name: None,
        me_source: None,
        me_note: None,
        not_me: false,
        keep_tickets: false,
        states: BTreeMap::from([
            (TicketState::Todo, "To Do".to_string()),
            (TicketState::Doing, "In Progress".to_string()),
        ]),
    };
    let b = d.link_board(b.id, link, "sam").await.unwrap();
    d.pull_board(b.id, &Issues(n)).await.unwrap();
    b
}

async fn by_key(d: &Desk, b: &Board, key: &str) -> Ticket {
    d.tickets(b.id, &Default::default())
        .await
        .unwrap()
        .into_iter()
        .find(|v| v.key.as_deref() == Some(key))
        .unwrap_or_else(|| panic!("{key} is on the board"))
        .ticket
}

fn sarah() -> Party {
    Party::manager("sarah", "Sarah")
}

fn by_sarah() -> By {
    By::manager("sarah", "Sarah", "The person asked to fix the recent bugs.")
}

fn adopt(id: Uuid, dir: &Path, needs: &[usize]) -> DraftTicket {
    DraftTicket {
        adopt: Some(id),
        folder: dir.to_string_lossy().into_owned(),
        needs: needs.to_vec(),
        ..Default::default()
    }
}

fn fresh(title: &str, dir: &Path, needs: &[usize]) -> DraftTicket {
    DraftTicket {
        title: title.into(),
        folder: dir.to_string_lossy().into_owned(),
        needs: needs.to_vec(),
        ..Default::default()
    }
}

fn plan_of(tickets: Vec<DraftTicket>) -> NewPlan {
    NewPlan {
        draft: Draft {
            goal: "Fix the recent bugs".into(),
            tickets,
        },
        board: None,
        parallel: None,
        engine: Engine::Claude,
        group: None,
        budget: None,
    }
}

async fn queued(d: &Desk, ticket: Uuid) -> bool {
    let c = d.0.store.pool().get().await.unwrap();
    let n: i64 = c
        .query_one(
            "SELECT count(*) FROM board_outbox WHERE ticket = $1",
            &[&ticket],
        )
        .await
        .unwrap()
        .get(0);
    n > 0
}

/// "Fix the recent bugs": two of the board's Jira issues taken on as they
/// are, one needing the other, beside a new ticket of Desk's own that
/// needs the second. Nothing is copied, the plan shows on the synced
/// board, and starting one moves its issue there.
#[tokio::test]
async fn a_plan_takes_on_synced_issues_beside_new_tickets() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, 2).await;
        let one = by_key(&d, &b, "ACME-1").await;
        let two = by_key(&d, &b, "ACME-2").await;
        let p = d
            .propose_plan(
                plan_of(vec![
                    adopt(one.id, dir.path(), &[]),
                    adopt(two.id, dir.path(), &[0]),
                    fresh("Write the release note", dir.path(), &[1]),
                ]),
                &sarah(),
                &by_sarah(),
            )
            .await
            .unwrap();
        let plan = p.summary.plan.clone();
        assert_eq!(plan.board, b.id, "it shows on the board it took them from");
        let keys: Vec<Option<&str>> = p.tickets.iter().map(|t| t.ticket.key.as_deref()).collect();
        assert_eq!(keys, vec![Some("ACME-1"), Some("ACME-2"), Some("DES-1")]);
        let adopted: Vec<bool> = p.tickets.iter().map(|t| t.adopted).collect();
        assert_eq!(adopted, vec![true, true, false]);
        // The same tickets, not copies, as they were.
        assert_eq!(p.tickets[0].ticket.ticket.id, one.id);
        assert_eq!(p.tickets[0].ticket.ticket.title, one.title);
        assert_eq!(p.tickets[0].ticket.ticket.state, TicketState::Todo);
        assert_eq!(d.tickets(b.id, &Default::default()).await.unwrap().len(), 2);
        // The new one is on a board of Desk's own.
        let note = &p.tickets[2].ticket.ticket;
        assert_ne!(note.board, b.id);
        assert_eq!(note.state, TicketState::Backlog);
        assert_eq!(p.tickets[1].ticket.ticket.needs, vec![one.id]);
        assert_eq!(note.needs, vec![two.id]);
        assert_eq!(p.waves.len(), 3);
        let first = &p.revisions[0];
        assert!(first.snapshot.tickets[0].adopted);
        assert!(
            first.snapshot.tickets[0].body.is_empty(),
            "its words stay its board's"
        );

        let p = d.approve_plan(plan.id, 1, &By::you()).await.unwrap();
        assert_eq!(states(&p), vec![TicketState::Todo; 3]);
        let plan = p.summary.plan.clone();
        let ready = plan::next_starts(
            &plan,
            &d.plan_tickets(plan.id).await.unwrap(),
            plan::room(&plan, 0, 0, 4),
            None,
        );
        assert_eq!(
            ready.iter().map(|s| s.ticket).collect::<Vec<_>>(),
            vec![one.id]
        );
        assert!(
            !queued(&d, one.id).await,
            "nothing to go out before it starts"
        );
        let s = d
            .plan_start(&plan, &ready[0], &sarah(), &by_sarah())
            .await
            .unwrap();
        assert_eq!(s.name, "ACME-1");
        let now = d.ticket(one.id).await.unwrap().ticket;
        assert_eq!(now.state, TicketState::Doing);
        assert_eq!(now.session(), Some(s.id));
        // Its move goes out to its issue through the board's sync.
        assert!(queued(&d, one.id).await, "the move is queued to go out");
        let push = crate::board_sync::push_of(&now, &[]);
        assert_eq!(push.state, Some(TicketState::Doing));
        assert!(
            push.title.is_none() && push.body.is_none(),
            "never its words"
        );
    })
    .await;
}

fn states(p: &PlanDetail) -> Vec<TicketState> {
    p.tickets.iter().map(|t| t.ticket.ticket.state).collect()
}

/// A ticket the person has, one another plan has, and one already under
/// way are each refused by name; a claim that loses a race to the person
/// is refused whole.
#[tokio::test]
async fn a_ticket_someone_holds_is_refused_plainly() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, 4).await;
        let t = |k: &'static str| {
            let d = &d;
            let b = &b;
            async move { by_key(d, b, k).await }
        };
        let (one, two, three, four) = (
            t("ACME-1").await,
            t("ACME-2").await,
            t("ACME-3").await,
            t("ACME-4").await,
        );
        let c = d.0.store.pool().get().await.unwrap();
        let you = serde_json::to_value(Party::you()).unwrap();
        c.execute(
            "UPDATE tickets SET assignee = $2 WHERE id = $1",
            &[&one.id, &you],
        )
        .await
        .unwrap();
        c.execute(
            "UPDATE tickets SET state = 'doing' WHERE id = $1",
            &[&three.id],
        )
        .await
        .unwrap();
        drop(c);
        let refused = |r: Result<PlanDetail, DeskError>, words: &str| match r {
            Err(DeskError::Conflict(why)) => assert!(why.contains(words), "{why}"),
            other => panic!("refused with {words:?}, not {other:?}"),
        };
        refused(
            d.propose_plan(
                plan_of(vec![adopt(one.id, dir.path(), &[])]),
                &sarah(),
                &by_sarah(),
            )
            .await,
            "ACME-1 is the person's already",
        );
        refused(
            d.propose_plan(
                plan_of(vec![adopt(three.id, dir.path(), &[])]),
                &sarah(),
                &by_sarah(),
            )
            .await,
            "ACME-3 is in Doing already",
        );
        d.propose_plan(
            plan_of(vec![adopt(two.id, dir.path(), &[])]),
            &sarah(),
            &by_sarah(),
        )
        .await
        .unwrap();
        refused(
            d.propose_plan(
                plan_of(vec![adopt(two.id, dir.path(), &[])]),
                &sarah(),
                &by_sarah(),
            )
            .await,
            "ACME-2 is in another plan already",
        );
        assert_eq!(
            d.plans(None, None).await.unwrap().len(),
            1,
            "nothing half made"
        );
        // Taken meanwhile: the claim itself refuses.
        let c = d.0.store.pool().get().await.unwrap();
        c.execute(
            "UPDATE tickets SET assignee = $2 WHERE id = $1",
            &[&four.id, &you],
        )
        .await
        .unwrap();
        drop(c);
        let runs = Runs {
            plan: Some(Uuid::new_v4()),
            ..Default::default()
        };
        assert!(matches!(
            d.0.store.adopt_ticket(four.id, &runs).await,
            Err(Adopt::Held(id)) if id == four.id
        ));
        // A ticket not on a board of the plan's group isn't found.
        let mut elsewhere = plan_of(vec![adopt(two.id, dir.path(), &[])]);
        elsewhere.group = Some("g-other".into());
        let e = d
            .propose_plan(elsewhere, &sarah(), &by_sarah())
            .await
            .unwrap_err();
        assert!(
            e.to_string()
                .contains("isn't on a board of the plan's group"),
            "{e}"
        );
    })
    .await;
}

/// A change the person approves that takes on a ticket someone took
/// meanwhile is refused whole: nothing of it is claimed, the plan runs on as
/// it was, and once the ticket is free the same approval goes through.
#[tokio::test]
async fn a_change_whose_ticket_was_taken_meanwhile_claims_nothing() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, 3).await;
        let one = by_key(&d, &b, "ACME-1").await;
        let two = by_key(&d, &b, "ACME-2").await;
        let three = by_key(&d, &b, "ACME-3").await;
        let p = d
            .propose_plan(
                plan_of(vec![adopt(one.id, dir.path(), &[])]),
                &sarah(),
                &by_sarah(),
            )
            .await
            .unwrap();
        let id = p.summary.plan.id;
        d.approve_plan(id, 1, &By::you()).await.unwrap();
        let mut tickets = vec![DraftTicket {
            id: Some(one.id),
            folder: dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        }];
        tickets.push(adopt(two.id, dir.path(), &[]));
        tickets.push(adopt(three.id, dir.path(), &[]));
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
        assert!(!changed.applied, "taking more on waits for the person");
        // The person takes ACME-3 before approving.
        let c = d.0.store.pool().get().await.unwrap();
        let you = serde_json::to_value(Party::you()).unwrap();
        c.execute(
            "UPDATE tickets SET assignee = $2 WHERE id = $1",
            &[&three.id, &you],
        )
        .await
        .unwrap();
        let e = d.approve_plan(id, 2, &By::you()).await.unwrap_err();
        assert!(
            e.to_string().contains("ACME-3 is the person's already"),
            "{e}"
        );
        assert_eq!(
            d.0.store.plan_of_ticket(two.id).await.unwrap(),
            None,
            "nothing of it claimed"
        );
        assert_eq!(
            d.plan(id).await.unwrap().summary.plan.approved_revision,
            Some(1)
        );
        // Let go, the same approval goes through: no claim left behind.
        c.execute(
            "UPDATE tickets SET assignee = NULL WHERE id = $1",
            &[&three.id],
        )
        .await
        .unwrap();
        drop(c);
        let p = d.approve_plan(id, 2, &By::you()).await.unwrap();
        assert_eq!(p.tickets.len(), 3);
        assert!(p.tickets.iter().all(|t| t.adopted));
        assert_eq!(d.0.store.plan_of_ticket(three.id).await.unwrap(), Some(id));
    })
    .await;
}

/// Letting go removes only the needs the plan added: one the person had
/// between two tickets it took on stays when the plan drops.
#[tokio::test]
async fn letting_go_keeps_the_needs_the_person_had() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, 3).await;
        let one = by_key(&d, &b, "ACME-1").await;
        let two = by_key(&d, &b, "ACME-2").await;
        let three = by_key(&d, &b, "ACME-3").await;
        // The person's own: ACME-2 needs ACME-1.
        assert!(d.0.store.put_need(two.id, one.id).await.unwrap());
        let p = d
            .propose_plan(
                plan_of(vec![
                    adopt(one.id, dir.path(), &[]),
                    adopt(two.id, dir.path(), &[0]),
                    adopt(three.id, dir.path(), &[0]),
                ]),
                &sarah(),
                &by_sarah(),
            )
            .await
            .unwrap();
        let id = p.summary.plan.id;
        d.drop_plan(id, OnDrop::Stop, &By::you()).await.unwrap();
        assert_eq!(
            d.ticket(two.id).await.unwrap().ticket.needs,
            vec![one.id],
            "the person's need stays"
        );
        assert!(
            d.ticket(three.id).await.unwrap().ticket.needs.is_empty(),
            "the plan's own goes"
        );
    })
    .await;
}

/// A draft that drops a ticket it took on leaves it on its board; dropping
/// a running plan lets its taken tickets go where they are, its started
/// one taken off its stopped session, never moved to Dropped; and an edit
/// never renames one.
#[tokio::test]
async fn dropping_lets_taken_tickets_go_where_they_are() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, 3).await;
        let one = by_key(&d, &b, "ACME-1").await;
        let two = by_key(&d, &b, "ACME-2").await;
        let three = by_key(&d, &b, "ACME-3").await;
        let p = d
            .propose_plan(
                plan_of(vec![
                    adopt(one.id, dir.path(), &[]),
                    adopt(two.id, dir.path(), &[0]),
                    adopt(three.id, dir.path(), &[]),
                ]),
                &sarah(),
                &by_sarah(),
            )
            .await
            .unwrap();
        let id = p.summary.plan.id;
        // The draft lets ACME-3 go and says another title for ACME-1.
        let mut keep: Vec<DraftTicket> = p.tickets[..2]
            .iter()
            .map(|t| DraftTicket {
                id: Some(t.ticket.ticket.id),
                folder: dir.path().to_string_lossy().into_owned(),
                ..Default::default()
            })
            .collect();
        keep[0].title = "Renamed by the boss".into();
        keep[1].needs = vec![0];
        let changed = d
            .change_plan(
                id,
                PlanEdit {
                    revision: Some(1),
                    tickets: Some(keep),
                    ..Default::default()
                },
                Engine::Claude,
                false,
                &by_sarah(),
            )
            .await
            .unwrap();
        assert!(changed.summary.contains("Let go of"), "{}", changed.summary);
        assert!(!changed.summary.contains("renamed"), "{}", changed.summary);
        let three_now = d.ticket(three.id).await.unwrap().ticket;
        assert_eq!(
            three_now.state,
            TicketState::Todo,
            "where it was, not deleted"
        );
        assert_eq!(d.0.store.plan_of_ticket(three.id).await.unwrap(), None);
        assert_eq!(d.ticket(one.id).await.unwrap().ticket.title, one.title);
        // Re-filing the plan leaves what it took on where it is.
        let own = d.plan_board(None).await.unwrap();
        d.move_plan(id, own.id, &by_sarah()).await.unwrap();
        assert_eq!(d.ticket(one.id).await.unwrap().ticket.board, b.id);

        let p = d.approve_plan(id, 2, &By::you()).await.unwrap();
        let plan = p.summary.plan.clone();
        let ready = plan::next_starts(
            &plan,
            &d.plan_tickets(id).await.unwrap(),
            plan::room(&plan, 0, 0, 4),
            None,
        );
        d.plan_start(&plan, &ready[0], &sarah(), &by_sarah())
            .await
            .unwrap();
        let dropped = d.drop_plan(id, OnDrop::Stop, &By::you()).await.unwrap();
        assert_eq!(dropped.summary.plan.state, PlanState::Dropped);
        assert!(dropped.tickets.is_empty(), "both let go");
        let one_now = d.ticket(one.id).await.unwrap().ticket;
        assert_eq!(one_now.state, TicketState::Doing, "left where it is");
        assert_eq!(
            one_now.assignee, None,
            "its stopped session has it no longer"
        );
        let two_now = d.ticket(two.id).await.unwrap().ticket;
        assert_eq!(two_now.state, TicketState::Todo);
        assert!(two_now.needs.is_empty(), "the plan's own need goes with it");
        assert_eq!(d.0.store.plan_of_ticket(two.id).await.unwrap(), None);
        // Free again: another plan may take them on.
        d.propose_plan(
            plan_of(vec![adopt(two.id, dir.path(), &[])]),
            &sarah(),
            &by_sarah(),
        )
        .await
        .unwrap();
    })
    .await;
}

/// A boss finds the tickets to take on by board and label, and sees which
/// a plan holds already; a board it doesn't have is refused by name.
#[tokio::test]
async fn the_board_s_tickets_are_found_by_what_their_issues_say() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, 3).await;
        let two = by_key(&d, &b, "ACME-2").await;
        d.propose_plan(
            plan_of(vec![adopt(two.id, dir.path(), &[])]),
            &sarah(),
            &by_sarah(),
        )
        .await
        .unwrap();
        let find = crate::find::TicketFind {
            board: Some("acme".into()),
            label: Some("Bug".into()),
            ..Default::default()
        };
        let (found, total) = d.find_tickets(&find, |_| true, 2).await.unwrap();
        assert_eq!(total, 3);
        assert_eq!(found.len(), 2, "as many as asked");
        let (all, _) = d.find_tickets(&find, |_| true, 10).await.unwrap();
        let held: Vec<(Option<&str>, bool)> = all
            .iter()
            .map(|f| (f.view.key.as_deref(), f.plan.is_some()))
            .collect();
        assert!(held.contains(&(Some("ACME-2"), true)), "{held:?}");
        assert!(held.contains(&(Some("ACME-1"), false)), "{held:?}");
        let none = crate::find::TicketFind {
            label: Some("design".into()),
            ..Default::default()
        };
        assert_eq!(d.find_tickets(&none, |_| true, 10).await.unwrap().1, 0);
        let elsewhere = crate::find::TicketFind {
            board: Some("Globex".into()),
            ..Default::default()
        };
        let e = d.find_tickets(&elsewhere, |_| true, 10).await.unwrap_err();
        assert!(e.to_string().contains("yours are Acme"), "{e}");
        // Out of sight, it isn't there.
        let e = d
            .find_tickets(&find, |g| g == Some("g-other"), 10)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("you have none"), "{e}");
    })
    .await;
}
