//! The boss manages the tickets on Desk's own boards: it reads one in full,
//! edits its words keeping the rest, comments, moves it, links its needs and
//! makes new ones, through the calls the person's board makes; a delete is
//! always the person's yes.

use super::*;
use workforce_desk::board::TicketState;
use workforce_desk::{NewBoard, NewTicket, TicketView};

const ACT: &str = "[oversight]\nenabled = true\nlevel = \"act\"\n";
const SUGGEST: &str = "[oversight]\nenabled = true\nlevel = \"suggest\"\n";

async fn board(d: &Desk, name: &str) -> uuid::Uuid {
    d.create_board(NewBoard {
        name: name.into(),
        group: None,
        prefix: None,
    })
    .await
    .unwrap()
    .id
}

async fn ticket(d: &Desk, board: uuid::Uuid, title: &str, body: &str) -> TicketView {
    d.create_ticket(
        NewTicket {
            board,
            title: title.into(),
            body: body.into(),
            state: Some(TicketState::Todo),
            needs: vec![],
        },
        &By::you(),
    )
    .await
    .unwrap()
}

async fn now(d: &Desk, key: &str) -> TicketView {
    d.ticket_by_key(key).await.unwrap()
}

/// At Act each change runs and is the boss's in the ticket's history, and
/// only what it says changes.
#[tokio::test(flavor = "multi_thread")]
async fn a_boss_reads_edits_comments_links_moves_and_makes_tickets() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        let b = board(&d, "Vellev").await;
        ticket(&d, b, "Design the recall cache", "Sketch the cache.").await;
        ticket(&d, b, "Build the recall cache", "").await;
        eventually("the boss's ticket tools", async || {
            overseer.tools.has("crew.edit_ticket").then_some(())
        })
        .await;
        let tool = |name: &str| overseer.tools.get(name).unwrap();
        let run = async |name: &str, input: Value| {
            let r = tool(name).execute(input).await.unwrap();
            assert!(r.success, "{name}: {r:?}");
            r.output
        };

        // A title alone: its brief, column and needs stay.
        let done = run(
            "crew.edit_ticket",
            json!({"ticket": "vel-1", "title": "Design the recall cache (v2)", "reason": "The person renamed it."}),
        )
        .await;
        assert_eq!(done["done"], "changed VEL-1", "{done}");
        let t = now(&d, "VEL-1").await;
        assert_eq!(t.ticket.title, "Design the recall cache (v2)");
        assert_eq!(t.ticket.body, "Sketch the cache.");
        assert_eq!(t.ticket.state, TicketState::Todo);

        // Added to its brief: what it said is kept.
        run(
            "crew.edit_ticket",
            json!({"ticket": "VEL-1", "append": "Design approved by the person.", "reason": "They approved it."}),
        )
        .await;
        let t = now(&d, "VEL-1").await;
        assert_eq!(
            t.ticket.body,
            "Sketch the cache.\n\nDesign approved by the person."
        );
        assert_eq!(t.ticket.title, "Design the recall cache (v2)");

        // A comment, a need, a move and a new ticket.
        let done = run(
            "crew.comment_ticket",
            json!({"ticket": "VEL-1", "comment": "Approved on 11/19.", "reason": "Recording it."}),
        )
        .await;
        assert_eq!(done["done"], "commented on VEL-1");
        let said = d.ticket_comments(t.ticket.id).await.unwrap();
        assert_eq!(said.len(), 1);
        assert_eq!(said[0].body, "Approved on 11/19.");
        assert_eq!(
            said[0].posted_by.as_ref().map(|p| p.kind),
            Some(PartyKind::Manager)
        );

        run(
            "crew.link_ticket",
            json!({"ticket": "VEL-2", "needs": "VEL-1", "reason": "It builds on it."}),
        )
        .await;
        let t2 = now(&d, "VEL-2").await;
        assert_eq!(t2.ticket.needs, vec![t.ticket.id]);
        assert!(!t2.ready, "it waits on VEL-1");

        run(
            "crew.move_ticket",
            json!({"ticket": "VEL-1", "state": "backlog", "reason": "Not yet."}),
        )
        .await;
        assert_eq!(now(&d, "VEL-1").await.ticket.state, TicketState::Backlog);

        let done = run(
            "crew.create_ticket",
            json!({"title": "Wire the cache in", "body": "After the build.", "needs": ["VEL-2"], "reason": "Next step."}),
        )
        .await;
        assert_eq!(done["done"], "made VEL-3 “Wire the cache in”", "{done}");
        let t3 = now(&d, "VEL-3").await;
        assert_eq!(
            (t3.ticket.body.as_str(), t3.ticket.state),
            ("After the build.", TicketState::Todo)
        );
        assert_eq!(t3.ticket.needs, vec![t2.ticket.id]);

        run(
            "crew.link_ticket",
            json!({"ticket": "VEL-2", "needs": "VEL-1", "unlink": true, "reason": "Not needed."}),
        )
        .await;
        assert!(now(&d, "VEL-2").await.ticket.needs.is_empty());

        // Read back in full, its words as reported.
        let read = run("crew.ticket", json!({"ticket": "VEL-1"})).await;
        assert_eq!(read["column"], "backlog");
        assert!(read["brief"]
            .as_str()
            .unwrap()
            .contains("Design approved by the person."));
        assert!(read["comments"][0]["says"]
            .as_str()
            .unwrap()
            .contains("Approved on 11/19."));
        let history: Vec<&str> = read["history"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["did"].as_str())
            .collect();
        assert!(
            history.contains(&"added to its description on VEL-1"),
            "{history:?}"
        );

        // Each is the boss's doing, with its reason.
        let page = d.activity(Filter::All, None, 100).await.unwrap();
        for kind in [
            Kind::TicketEdited,
            Kind::TicketCommented,
            Kind::TicketCreated,
            Kind::TicketMoved,
        ] {
            assert!(
                page.events
                    .iter()
                    .any(|e| e.kind == kind && e.actor.kind == PartyKind::Manager),
                "{kind:?}"
            );
        }

        // Where a ticket in no plan runs is said as it is handed out.
        let r = tool("crew.edit_ticket")
            .execute(json!({"ticket": "VEL-2", "machine": "devbox", "folder": "/src/vellev", "reason": "Move it."}))
            .await
            .unwrap();
        assert!(!r.success);
        assert!(
            r.error.as_deref().unwrap_or_default().contains("crew.assign_ticket"),
            "{r:?}"
        );
        // An edit that says nothing, or both a brief and words to add.
        for wrong in [
            json!({"ticket": "VEL-2", "reason": "x"}),
            json!({"ticket": "VEL-2", "body": "a", "append": "b", "reason": "x"}),
        ] {
            assert!(!tool("crew.edit_ticket").execute(wrong).await.unwrap().success);
        }
        let _ = crew;
    })
    .await;
}

/// A delete can't be undone: at Act, and at Act fully, it is a card the
/// person says yes to, and a boss's own delete is refused whatever path it
/// takes. Their yes deletes it.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_ticket_waits_for_the_persons_yes() {
    use super::super::actions::{Action, Gate};
    use workforce_common::oversight::Level;
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        let b = board(&d, "Vellev").await;
        ticket(&d, b, "Scratch idea", "").await;
        eventually("the boss's ticket tools", async || {
            overseer.tools.has("crew.delete_ticket").then_some(())
        })
        .await;
        let r = overseer
            .tools
            .get("crew.delete_ticket")
            .unwrap()
            .execute(json!({"ticket": "VEL-1", "reason": "Not needed."}))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        assert_eq!(
            r.output["suggested"], "Delete VEL-1 for good (it can't be undone)",
            "{r:?}"
        );
        assert!(r.output["note"]
            .as_str()
            .unwrap_or_default()
            .contains("can't be undone"));
        assert!(d.ticket_by_key("VEL-1").await.is_ok(), "still there");

        let del = Action::parse(
            "crew.delete_ticket",
            &json!({"ticket": "VEL-1", "reason": "x"}),
        )
        .unwrap();
        let (_, o) = crew.overseer_service().unwrap();
        for level in [Level::Act, Level::Full] {
            let at = workforce_common::oversight::Oversight { level, ..o.clone() };
            assert!(
                matches!(crew.gate(&del, &at), Gate::Suggest(_)),
                "{level:?}"
            );
        }
        // A boss's own delete, by any path, is refused.
        let boss = crew.by_overseer("cleaning up").unwrap();
        let refused = crew.perform(&del, boss).await.unwrap_err();
        assert!(
            refused.contains("Only the person deletes a ticket"),
            "{refused}"
        );
        assert!(d.ticket_by_key("VEL-1").await.is_ok());

        // The person's yes on the card deletes it.
        let kept = d.store().suggestions(10).await.unwrap().remove(0);
        crew.accept(&kept).await.unwrap();
        assert!(d.ticket_by_key("VEL-1").await.is_err(), "deleted");
        let page = d.activity(Filter::All, None, 50).await.unwrap();
        let gone = page
            .events
            .iter()
            .find(|e| e.kind == Kind::TicketDeleted)
            .unwrap();
        assert_eq!(gone.actor.kind, PartyKind::You);
    })
    .await;
}

/// At Suggest an edit is a card, and nothing changes until the person
/// says yes; then only what it said changes.
#[tokio::test(flavor = "multi_thread")]
async fn at_suggest_an_edit_waits_on_its_card() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", SUGGEST, &[]).await;
        let b = board(&d, "Vellev").await;
        ticket(&d, b, "Design the recall cache", "Sketch the cache.").await;
        eventually("the boss's ticket tools", async || {
            overseer.tools.has("crew.edit_ticket").then_some(())
        })
        .await;
        let r = overseer
            .tools
            .get("crew.edit_ticket")
            .unwrap()
            .execute(
                json!({"ticket": "VEL-1", "append": "Approved.", "reason": "They approved it."}),
            )
            .await
            .unwrap();
        assert_eq!(
            r.output["suggested"], "On VEL-1, add to its brief: “Approved.”",
            "{r:?}"
        );
        assert_eq!(now(&d, "VEL-1").await.ticket.body, "Sketch the cache.");
        let kept = d.store().suggestions(10).await.unwrap().remove(0);
        crew.accept(&kept).await.unwrap();
        let t = now(&d, "VEL-1").await;
        assert_eq!(t.ticket.body, "Sketch the cache.\n\nApproved.");
        assert_eq!(t.ticket.title, "Design the recall cache");
    })
    .await;
}
