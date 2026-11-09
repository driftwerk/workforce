//! Tests of groups as boundaries on Desk's side: rows stamped with their
//! group as they are written and kept by it after a move, the ledger split
//! by group, and rooms that keep to their group.

use std::collections::HashSet;
use std::time::Duration;

use chrono::Utc;
use uuid::Uuid;
use workforce_postgres::test_support::with_test_pool;

use super::tests::desk;
use super::*;
use crate::crew::{Event, Kind, Party};
use crate::ledger::{Billing, Priced, Spend, SpendBy, Tokens};
use crate::ledger_store::{Filter, Kept};
use crate::rooms::{Member, NewRoom, RoomPatch};
use crate::scope::{Scope, Seen};
use crate::store::build_pool;

fn acme() -> Scope {
    Scope::Group("acme".into())
}

fn bosses() -> Vec<Boss> {
    vec![
        Boss {
            agent: "desk".into(),
            name: "Desk".into(),
            scope: Scope::Ungrouped,
            level: "act".into(),
        },
        Boss {
            agent: "sarah".into(),
            name: "Sarah".into(),
            scope: acme(),
            level: "act".into(),
        },
        Boss {
            agent: "mia".into(),
            name: "Mia".into(),
            scope: Scope::Group("atlas".into()),
            level: "act".into(),
        },
    ]
}

/// Acme and Atlas, each with a boss; Infra with none.
async fn groups(d: &Desk) {
    d.change_layout(|l| {
        l.create(
            "acme",
            "Acme",
            &["agent:local/scout".into(), "agent:acme-hub/zed".into()],
            None,
        )?
        .create("atlas", "Atlas", &["agent:atlas-hub/ada".into()], None)?
        .create("infra", "Infra", &["agent:local/infra-bot".into()], None)
    })
    .await
    .unwrap();
    d.set_bosses(bosses());
}

async fn seen_events(d: &Desk, scope: &Scope, n: usize) -> Vec<Event> {
    let seen = d.seen_by(scope);
    for _ in 0..200 {
        let events = d
            .store()
            .events_since(
                Utc::now() - chrono::Duration::hours(1),
                &[],
                50,
                Some(&seen),
            )
            .await
            .unwrap();
        if events.len() >= n {
            return events;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("fewer than {n} events were seen");
}

#[tokio::test]
async fn an_event_keeps_the_group_it_was_noted_in_and_each_scope_reads_its_own() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        groups(&d).await;
        let scout = Party::agent("scout", "Scout");
        d.note(Event::new(
            Kind::Started,
            scout.clone(),
            Some(scout.clone()),
            "started “the Acme launch”",
        ));
        // A boss's own doing is its scope's, wherever its agent sits.
        d.note(Event::new(
            Kind::Summary,
            Party::manager("sarah", "Sarah"),
            None,
            "sent your morning summary",
        ));
        let bot = Party::agent("infra-bot", "Infra bot");
        d.note(Event::new(Kind::Started, bot.clone(), Some(bot), "started"));
        let acme_rows = seen_events(&d, &acme(), 2).await;
        assert_eq!(
            acme_rows
                .iter()
                .map(|e| e.text.as_str())
                .collect::<Vec<_>>(),
            ["started “the Acme launch”", "sent your morning summary"]
        );
        assert!(acme_rows.iter().all(|e| e.group.as_deref() == Some("acme")));
        let main = seen_events(&d, &Scope::Ungrouped, 1).await;
        assert_eq!(main.len(), 1, "a group with no boss is the main overseer's");
        assert_eq!(main[0].group.as_deref(), Some("infra"));

        // Scout moves to Atlas: what was noted stays Acme's.
        d.change_layout(|l| l.put("agent:local/scout", "atlas", None))
            .await
            .unwrap();
        d.note(Event::new(
            Kind::Started,
            scout.clone(),
            Some(scout),
            "started “the Atlas fix”",
        ));
        let atlas = seen_events(&d, &Scope::Group("atlas".into()), 1).await;
        assert_eq!(atlas[0].text, "started “the Atlas fix”");
        let acme_rows = seen_events(&d, &acme(), 2).await;
        assert_eq!(acme_rows.len(), 2, "the move never moves Acme's history");
        assert!(!acme_rows.iter().any(|e| e.text.contains("Atlas")));
        assert!(!main.iter().any(|e| e.group.as_deref() == Some("acme")));
    })
    .await;
}

fn kept(at: chrono::DateTime<Utc>, usd: f64) -> Kept {
    Kept {
        spend: Spend {
            key: Uuid::new_v4().to_string(),
            at,
            model: Some("claude-opus-5-5".into()),
            tokens: Tokens {
                input: 100,
                ..Default::default()
            },
            usd: Some(usd),
            priced: Priced::Reported,
            billing: None,
        },
        billing: Billing::Plan,
        account: None,
    }
}

#[tokio::test]
async fn the_ledger_is_kept_by_group_and_spend_splits_by_group() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let (web, api, loose) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        d.change_layout(|l| {
            l.create("acme", "Acme", &[session_item(web)], None)?
                .create("atlas", "Atlas", &[session_item(api)], None)
        })
        .await
        .unwrap();
        d.set_bosses(bosses());
        // Made in Acme: after it was put there.
        tokio::time::sleep(Duration::from_millis(5)).await;
        let at = Utc::now();
        for (session, usd) in [(web, 2.0), (web, 1.0), (api, 4.0), (loose, 0.5)] {
            d.store()
                .ledger_write(
                    session,
                    crate::engine::Engine::Claude,
                    None,
                    &[kept(at, usd)],
                )
                .await
                .unwrap();
        }
        // The session moves: its calls kept so far stay Acme's.
        d.change_layout(|l| l.put(&session_item(web), "atlas", None))
            .await
            .unwrap();
        let q = |filter: Filter| SpendQuery {
            by: SpendBy::Group,
            from: at - chrono::Duration::minutes(1),
            to: Utc::now(),
            limit: 10,
            filter,
        };
        let v = d.spend(&q(Filter::default())).await.unwrap();
        let rows: Vec<(&str, f64)> = v
            .rows
            .iter()
            .map(|r| (r.label.as_str(), r.totals.usd))
            .collect();
        assert_eq!(rows, [("Atlas", 4.0), ("Acme", 3.0), ("Ungrouped", 0.5)]);
        assert_eq!(v.total.usd, 7.5);

        let acme_only = Filter {
            seen: Some(d.seen_by(&acme())),
            ..Default::default()
        };
        let v = d.spend(&q(acme_only.clone())).await.unwrap();
        assert_eq!(v.total.usd, 3.0, "Acme's boss reads Acme's alone");
        let page = d
            .ledger_page(
                at - chrono::Duration::minutes(1),
                Utc::now(),
                &acme_only,
                None,
            )
            .await
            .unwrap();
        assert!(page.rows.iter().all(|r| r.group.as_deref() == Some("acme")));
        assert_eq!(page.rows.len(), 2);
        let main = Filter {
            seen: Some(d.seen_by(&Scope::Ungrouped)),
            ..Default::default()
        };
        assert_eq!(d.spend(&q(main)).await.unwrap().total.usd, 0.5);
    })
    .await;
}

fn member(hub: &str, agent: &str) -> Member {
    Member {
        hub: hub.into(),
        agent: agent.into(),
    }
}

#[tokio::test]
async fn a_room_keeps_to_its_group_and_a_bossed_groups_room_asks_once_before_its_talk_goes_to_a_new_hub(
) {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        groups(&d).await;
        let acme_room = d
            .create_room(NewRoom {
                name: "Acme launch".into(),
                members: vec![member("local", "scout")],
                group: Some("acme".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        // Atlas's agent can't join Acme's room, through any patch.
        match d
            .update_room(
                acme_room.id,
                RoomPatch {
                    members: Some(vec![member("local", "scout"), member("atlas-hub", "ada")]),
                    share_with: vec!["atlas-hub".into()],
                    ..Default::default()
                },
            )
            .await
        {
            Err(DeskError::Invalid(why)) => {
                assert!(why.contains("another client's group"), "{why}")
            }
            other => panic!("{other:?}"),
        }
        // Nor post there, as a delivery or otherwise.
        let post = crate::rooms::NewMessage {
            author: "atlas-hub/ada".into(),
            content: "Here is Atlas's plan".into(),
            ..Default::default()
        };
        assert!(matches!(
            d.post(acme_room.id, post.clone()).await,
            Err(DeskError::Invalid(_))
        ));
        // A bossed group's agent joins no ungrouped room either.
        let open = d
            .create_room(NewRoom {
                name: "Everyone".into(),
                members: vec![member("local", "infra-bot")],
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(matches!(
            d.update_room(
                open.id,
                RoomPatch {
                    members: Some(vec![member("local", "infra-bot"), member("local", "scout")]),
                    ..Default::default()
                },
            )
            .await,
            Err(DeskError::Invalid(_))
        ));
        assert!(matches!(
            d.post(
                open.id,
                crate::rooms::NewMessage {
                    author: "local/scout".into(),
                    content: "hi".into(),
                    ..Default::default()
                }
            )
            .await,
            Err(DeskError::Invalid(_))
        ));
        // An ungrouped room mixes hubs as before: nothing is asked.
        let mixed = vec![member("local", "infra-bot"), member("other-hub", "x")];
        assert!(d.room_asks(Some(open.id), None, &mixed, &[]).is_empty());
        d.update_room(
            open.id,
            RoomPatch {
                members: Some(mixed.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        d.create_room(NewRoom {
            name: "Mixed from the start".into(),
            members: mixed,
            ..Default::default()
        })
        .await
        .unwrap();
        // A bossed group's room: a new hub of its own agents is asked about once.
        let acme_mixed = vec![member("local", "scout"), member("acme-hub", "zed")];
        assert_eq!(
            d.room_asks(Some(acme_room.id), None, &acme_mixed, &[]),
            ["acme-hub"]
        );
        match d
            .update_room(
                acme_room.id,
                RoomPatch {
                    members: Some(acme_mixed.clone()),
                    ..Default::default()
                },
            )
            .await
        {
            Err(DeskError::Conflict(why)) => assert!(why.contains("acme-hub"), "{why}"),
            other => panic!("{other:?}"),
        }
        let shared = d
            .update_room(
                acme_room.id,
                RoomPatch {
                    members: Some(acme_mixed.clone()),
                    share_with: vec!["acme-hub".into()],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(shared.shared_with, ["acme-hub", "local"]);
        // Gone and back again: not asked again.
        for members in [vec![member("local", "scout")], acme_mixed] {
            d.update_room(
                acme_room.id,
                RoomPatch {
                    members: Some(members),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        let kept = d.store().room(acme_room.id).await.unwrap().unwrap();
        assert_eq!(kept.shared_with, ["acme-hub", "local"]);
        assert!(matches!(
            d.update_room(
                open.id,
                RoomPatch {
                    group: Some(Some("nowhere".into())),
                    ..Default::default()
                },
            )
            .await,
            Err(DeskError::Invalid(_))
        ));
    })
    .await;
}

#[tokio::test]
async fn a_group_deleted_takes_its_rows_out_of_every_overseers_sight() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        groups(&d).await;
        let bot = Party::agent("infra-bot", "Infra bot");
        d.note(Event::new(
            Kind::Started,
            bot.clone(),
            Some(bot.clone()),
            "started “the infra job”",
        ));
        seen_events(&d, &Scope::Ungrouped, 1).await;
        let bossed: HashSet<String> = ["acme".to_string(), "atlas".to_string()].into();
        assert_eq!(d.bossed(), bossed);
        assert_eq!(
            d.seen_by(&acme()),
            Seen::new(acme(), bossed.clone(), ["infra".to_string()].into())
        );
        // Infra goes: its bot is ungrouped from now on, and what was
        // written while it was Infra's is no overseer's.
        d.change_layout(|l| l.delete("infra")).await.unwrap();
        d.note(Event::new(
            Kind::Started,
            bot.clone(),
            Some(bot),
            "started “the next job”",
        ));
        let main = seen_events(&d, &Scope::Ungrouped, 1).await;
        assert_eq!(
            main.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            ["started “the next job”"]
        );
    })
    .await;
}

#[tokio::test]
async fn a_group_stays_a_boundary_until_its_boss_is_taken_off_and_then_opens_from_then_on() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        groups(&d).await;
        // No boss oversees Acme now (its config didn't load), but one is
        // named for it: Acme's rows are the person's alone.
        d.set_bosses(vec![]);
        d.bound_groups(["acme".to_string()]).await;
        let scout = Party::agent("scout", "Scout");
        d.note(Event::new(
            Kind::Started,
            scout.clone(),
            Some(scout.clone()),
            "started “before”",
        ));
        // A room agent of another hub is stamped by its own sidebar key.
        let ada = Party::agent("atlas-hub/ada", "Ada");
        d.note(Event::new(
            Kind::Started,
            ada.clone(),
            Some(ada),
            "started “ada's”",
        ));
        let bot = Party::agent("infra-bot", "Infra bot");
        d.note(Event::new(
            Kind::Started,
            bot.clone(),
            Some(bot.clone()),
            "started “marker”",
        ));
        let main = seen_events(&d, &Scope::Ungrouped, 1).await;
        assert!(
            main.iter().all(|e| e.text != "started “before”"),
            "{main:?}"
        );
        assert!(main
            .iter()
            .any(|e| e.text == "started “ada's”" && e.group.as_deref() == Some("atlas")));

        // Kept across a restart.
        d.0.scopes.lock().bounded.clear();
        d.scopes_load().await;
        assert!(d.bossed().contains("acme"));

        // The person takes the boss off: from now on, not its past.
        d.unbound_group("acme").await;
        d.note(Event::new(
            Kind::Started,
            scout.clone(),
            Some(scout),
            "started “after”",
        ));
        let main = seen_events(&d, &Scope::Ungrouped, 3).await;
        let texts: Vec<&str> = main.iter().map(|e| e.text.as_str()).collect();
        assert!(texts.contains(&"started “after”"), "{texts:?}");
        assert!(!texts.contains(&"started “before”"), "{texts:?}");
    })
    .await;
}

#[tokio::test]
async fn a_session_moved_across_bosses_leaves_its_ticket_questions_and_boss_behind() {
    use crate::board::TicketState;
    use crate::line::{Asked, Delivery, LineKind, LineMessage};
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        groups(&d).await;
        let s = super::tests::new_session(&d, dir.path()).await;
        d.change_layout(|l| l.put(&session_item(s.id), "acme", None))
            .await
            .unwrap();
        let board = d
            .create_board(NewBoard {
                name: "Acme".into(),
                group: Some("acme".into()),
                prefix: Some("ACM".into()),
            })
            .await
            .unwrap();
        let t = d
            .create_ticket(
                NewTicket {
                    board: board.id,
                    title: "Fix the checkout".into(),
                    body: String::new(),
                    state: Some(TicketState::Todo),
                    needs: vec![],
                },
                &By::you(),
            )
            .await
            .unwrap();
        d.assign_ticket(t.ticket.id, Some(AssignTo::Session(s.id)), &By::you())
            .await
            .unwrap();
        let mut ask = LineMessage::new(
            Party::session(&d.view(s.id).unwrap()),
            Party::manager("sarah", "Sarah"),
            LineKind::Ask,
            "Which port?".into(),
            1,
        );
        ask.asked = Some(Asked::Open);
        d.store().put_line(&ask).await.unwrap();
        let sarah = Party::manager("sarah", "Sarah");
        for _ in 0..200 {
            if d.charges_loaded() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        d.take_on(
            s.id,
            &sarah,
            crate::charge::How::Handed,
            &acme(),
            &By::you(),
        )
        .await
        .unwrap();

        // Within Acme nothing but the sidebar changes; to Atlas, it all goes.
        d.change_layout(|l| l.put(&session_item(s.id), "atlas", None))
            .await
            .unwrap();
        let after = d.ticket(t.ticket.id).await.unwrap().ticket;
        assert_eq!((after.state, after.assignee), (TicketState::Todo, None));
        let asked = d.store().line_message(ask.id).await.unwrap().unwrap();
        assert_eq!(asked.asked, Some(Asked::Withdrawn));
        assert!(
            !d.charge_of(s.id).unwrap().live(),
            "Sarah no longer runs it"
        );
        let thread = d.line_thread(s.id, None, None).await.unwrap();
        let told = thread
            .iter()
            .find(|m| m.text.starts_with("You were moved to Atlas"))
            .expect("the session is told");
        assert!(told.text.contains("back in To do"), "{}", told.text);
        assert_ne!(told.delivery, Delivery::Refused);
        // It can't be taken on by Acme's boss any more.
        assert!(d
            .take_on(
                s.id,
                &sarah,
                crate::charge::How::Handed,
                &acme(),
                &By::you()
            )
            .await
            .is_err());
    })
    .await;
}

#[tokio::test]
async fn a_group_closed_for_a_write_that_didnt_happen_is_forgotten_across_a_restart() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        d.bound_groups(["acme".to_string()]).await;
        let kept = || async {
            d.store()
                .state("desk.groups.bounded")
                .await
                .unwrap()
                .map(|e| e.value)
        };
        assert_eq!(kept().await, Some(serde_json::json!(["acme"])));
        d.forget_bound("acme").await;
        assert!(!d.bossed().contains("acme"));
        assert_eq!(kept().await, Some(serde_json::json!([])), "kept forgotten");
    })
    .await;
}

/// A Floor of what `whos` are doing: one need and one turn running each.
fn floor_of(whos: &[crate::floor::Who]) -> crate::floor::FloorView {
    use crate::floor::{Answer, FloorView, More, Need, NeedKind, Place, Work};
    let now = Utc::now();
    FloorView {
        as_of: now,
        needs: whos
            .iter()
            .map(|who| Need {
                id: format!("need:{}", who.id),
                kind: NeedKind::Question,
                who: who.clone(),
                ask: "Go on?".into(),
                why: None,
                options: None,
                chose: None,
                since: now,
                place: Place::Elsewhere,
                answer: Answer::Reply,
                group: None,
                boss: None,
            })
            .collect(),
        went_ahead: vec![],
        working: whos
            .iter()
            .map(|who| Work {
                who: who.clone(),
                what: "working".into(),
                place: Place::Elsewhere,
                since: now,
                heard_at: now,
                ended_at: None,
                outcome: None,
                background: None,
                gist: None,
                group: None,
                boss: None,
                continued: None,
            })
            .collect(),
        done: vec![],
        lanes: vec![],
        counts: Default::default(),
        more: More::default(),
        summary: None,
        bosses: vec![],
        sealed: vec![],
        plans: vec![],
    }
}

fn who(kind: crate::crew::PartyKind, id: &str) -> crate::floor::Who {
    crate::floor::Who {
        kind,
        id: id.into(),
        name: id.into(),
        engine: None,
    }
}

/// Each need's and each turn's group and boss, by who it is.
fn placed(v: &crate::floor::FloorView) -> Vec<(String, Option<String>, Option<String>)> {
    let needs = v
        .needs
        .iter()
        .map(|n| (n.who.id.clone(), n.group.clone(), n.boss.clone()));
    let work: Vec<_> = v
        .working
        .iter()
        .map(|w| (w.who.id.clone(), w.group.clone(), w.boss.clone()))
        .collect();
    let needs: Vec<_> = needs.collect();
    assert_eq!(needs, work, "a need and its work are placed alike");
    needs
}

#[tokio::test]
async fn the_floor_shows_each_item_in_its_group_now_with_that_groups_own_boss() {
    use crate::crew::PartyKind;
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        groups(&d).await;
        let s = super::tests::new_session(&d, dir.path()).await;
        d.change_layout(|l| l.put(&session_item(s.id), "acme", None))
            .await
            .unwrap();
        let whos = [
            who(PartyKind::Session, &s.id.to_string()),
            who(PartyKind::Agent, "scout"),
            // A boss's own work is its scope's, wherever its agent sits.
            who(PartyKind::Agent, "sarah"),
            who(PartyKind::Agent, "infra-bot"),
            who(PartyKind::Agent, "blog-bot"),
            // Acme's budget, held by the breaker.
            who(PartyKind::Desk, "pool:group:acme"),
            who(PartyKind::Desk, "pool:desk"),
        ];
        let mut v = floor_of(&whos);
        d.stamp_floor(&mut v);
        let in_acme = || (Some("acme".to_string()), Some("sarah".to_string()));
        let at = |(g, b): (Option<String>, Option<String>), id: &str| (id.to_string(), g, b);
        assert_eq!(
            placed(&v),
            [
                at(in_acme(), &s.id.to_string()),
                at(in_acme(), "scout"),
                at(in_acme(), "sarah"),
                // A group with no boss of its own: its group, no boss.
                at((Some("infra".into()), None), "infra-bot"),
                // Ungrouped: neither.
                at((None, None), "blog-bot"),
                at(in_acme(), "pool:group:acme"),
                at((None, None), "pool:desk"),
            ]
        );
        assert!(v.sealed.is_empty());

        // An id with a hub in it is no agent of this hub's: Desk places it.
        let mut v = floor_of(&[who(PartyKind::Agent, "acme-hub/zed")]);
        d.stamp_floor(&mut v);
        assert_eq!(placed(&v), [("acme-hub/zed".to_string(), None, None)]);

        // Moved to Atlas, the session shows in Atlas with Mia.
        d.change_layout(|l| l.put(&session_item(s.id), "atlas", None))
            .await
            .unwrap();
        let mut v = floor_of(&whos[..1]);
        d.stamp_floor(&mut v);
        assert_eq!(
            placed(&v),
            [at(
                (Some("atlas".into()), Some("mia".into())),
                &s.id.to_string()
            )]
        );

        // The bosses head their sections: Sarah in a turn, the others idle,
        // with no budget to stand against.
        let mut v = floor_of(&whos);
        d.stamp_floor(&mut v);
        let bosses = d.floor_bosses(&v).await;
        let heads: Vec<_> = bosses
            .iter()
            .map(|b| {
                (
                    b.agent.as_str(),
                    b.scope.clone(),
                    b.state,
                    b.spend.is_some(),
                )
            })
            .collect();
        use crate::floor::BossState::{Idle, Working};
        assert_eq!(
            heads,
            [
                ("desk", Scope::Ungrouped, Idle, false),
                ("sarah", acme(), Working, false),
                ("mia", Scope::Group("atlas".into()), Idle, false),
            ]
        );
        assert!(bosses.iter().all(|b| b.paused.is_none()));
    })
    .await;
}

#[tokio::test]
async fn a_sealed_groups_items_carry_their_group_and_no_boss_and_the_floor_says_it_is_sealed() {
    use crate::crew::PartyKind;
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        groups(&d).await;
        // Sarah's config doesn't load: no boss oversees Acme now, but one is
        // named for it.
        d.set_bosses(vec![bosses().remove(0)]);
        d.bound_groups(["acme".to_string()]).await;
        let mut v = floor_of(&[
            who(PartyKind::Agent, "scout"),
            who(PartyKind::Agent, "infra-bot"),
        ]);
        d.stamp_floor(&mut v);
        assert_eq!(
            placed(&v),
            [
                ("scout".to_string(), Some("acme".to_string()), None),
                ("infra-bot".to_string(), Some("infra".to_string()), None),
            ]
        );
        assert_eq!(v.sealed, ["acme"], "Acme is no one's, not the main boss's");
        let bosses = d.floor_bosses(&v).await;
        assert_eq!(
            bosses.iter().map(|b| b.agent.as_str()).collect::<Vec<_>>(),
            ["desk"]
        );
    })
    .await;
}

/// What each local agent spent, as the hub's telemetry says it: after
/// `wait`, or failing.
struct Spent {
    wait: Duration,
    spent: Option<Vec<(&'static str, f64)>>,
}

impl AgentSpend for Spent {
    fn spent(
        &self,
        _: chrono::DateTime<Utc>,
        _: chrono::DateTime<Utc>,
    ) -> eyre::Result<Vec<(String, crate::budget_store::Used)>> {
        std::thread::sleep(self.wait);
        let spent = self
            .spent
            .clone()
            .ok_or_else(|| eyre::eyre!("telemetry is down"))?;
        Ok(spent
            .into_iter()
            .map(|(a, usd)| (a.to_string(), crate::budget_store::Used { tokens: 0, usd }))
            .collect())
    }
}

/// A day budget of $40 for Acme, set by the person.
async fn acme_budget(d: &Desk) {
    use workforce_core::budget::{BudgetScope, Period, Unit};
    d.put_budget(
        NewBudget {
            scope: BudgetScope::Desk,
            group: Some("acme".into()),
            period: Period::Day,
            unit: Unit::Usd,
            amount: 40.0,
            warn_at: None,
            on_exceed: None,
        },
        crate::crew::By::you(),
    )
    .await
    .unwrap();
}

/// Each boss's spend standing on the Floor: what it used of what.
async fn spends(d: &Desk) -> Vec<(String, Option<(f64, f64)>)> {
    d.floor_bosses(&floor_of(&[]))
        .await
        .into_iter()
        .map(|b| (b.agent, b.spend.map(|s| (s.used, s.of))))
        .collect()
}

fn spend_of(all: &[(String, Option<(f64, f64)>)], agent: &str) -> Option<(f64, f64)> {
    all.iter().find(|(a, _)| a == agent).unwrap().1
}

#[tokio::test]
async fn a_groups_boss_has_its_groups_budget_standing_on_the_floor_its_own_spend_counted() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        groups(&d).await;
        d.count_budgets_in(chrono_tz::UTC);
        // Sarah sits in no group, but her own turns are Acme's, as the Floor
        // shows them; Scout is in Acme; Mia's are Atlas's.
        d.spend_agents_with(std::sync::Arc::new(Spent {
            wait: Duration::ZERO,
            spent: Some(vec![("sarah", 3.0), ("scout", 2.0), ("mia", 7.0)]),
        }));
        acme_budget(&d).await;
        let all = spends(&d).await;
        assert_eq!(spend_of(&all, "sarah"), Some((5.0, 40.0)));
        assert_eq!(
            spend_of(&all, "mia"),
            None,
            "Atlas has no budget of its own"
        );
        assert_eq!(
            spend_of(&all, "desk"),
            None,
            "the main boss's section has none"
        );
    })
    .await;
}

#[tokio::test]
async fn the_floor_has_no_spend_it_cant_count_and_waits_for_none() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let store_pool = pool.clone();
        let d = desk(pool, dir.path()).await;
        groups(&d).await;
        acme_budget(&d).await;

        // Desk doesn't know the timezone yet: no day to count.
        *d.0.budget_tz.lock() = None;
        assert_eq!(spend_of(&spends(&d).await, "sarah"), None);

        // The telemetry fails: the sessions' ledger still counts.
        d.count_budgets_in(chrono_tz::UTC);
        d.group_spend_changed();
        d.spend_agents_with(std::sync::Arc::new(Spent {
            wait: Duration::ZERO,
            spent: None,
        }));
        assert_eq!(spend_of(&spends(&d).await, "sarah"), Some((0.0, 40.0)));

        // Counting is slow: the Floor doesn't wait for it, and says none.
        d.group_spend_changed();
        d.spend_agents_with(std::sync::Arc::new(Spent {
            wait: Duration::from_secs(3),
            spent: Some(vec![]),
        }));
        let begun = std::time::Instant::now();
        assert_eq!(spend_of(&spends(&d).await, "sarah"), None);
        assert!(
            begun.elapsed() < super::breaker::SPEND_WAIT * 3,
            "{:?}",
            begun.elapsed()
        );
        // A read while that count runs doesn't wait for it, nor start one.
        let counts = d.group_counts();
        let begun = std::time::Instant::now();
        assert_eq!(spend_of(&spends(&d).await, "sarah"), None);
        assert!(begun.elapsed() < super::breaker::SPEND_WAIT);
        assert_eq!(d.group_counts(), counts);
        tokio::time::sleep(Duration::from_secs(3)).await;

        // The ledger can't be read: none.
        d.group_spend_changed();
        d.spend_agents_with(std::sync::Arc::new(Spent {
            wait: Duration::ZERO,
            spent: Some(vec![]),
        }));
        store_pool
            .get()
            .await
            .unwrap()
            .batch_execute("ALTER TABLE ledger RENAME TO ledger_gone")
            .await
            .unwrap();
        assert_eq!(spend_of(&spends(&d).await, "sarah"), None);
    })
    .await;
}

/// A day budget of `amount` for Acme, set by the person.
async fn acme_budget_of(d: &Desk, amount: f64) {
    use workforce_core::budget::{BudgetScope, Period, Unit};
    d.put_budget(
        NewBudget {
            scope: BudgetScope::Desk,
            group: Some("acme".into()),
            period: Period::Day,
            unit: Unit::Usd,
            amount,
            warn_at: None,
            on_exceed: None,
        },
        crate::crew::By::you(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn reads_at_once_start_one_count_and_a_count_begun_before_the_budgets_changed_isnt_kept() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        groups(&d).await;
        acme_budget_of(&d, 40.0).await;
        d.spend_agents_with(std::sync::Arc::new(Spent {
            wait: Duration::from_secs(1),
            spent: Some(vec![]),
        }));
        d.group_spend_changed();
        let before = d.group_counts();

        // Eight Floor reads at once under slow telemetry: one count.
        let reads = futures::future::join_all((0..8).map(|_| spends(&d))).await;
        assert!(reads.iter().all(|r| spend_of(r, "sarah").is_none()));
        assert_eq!(d.group_counts() - before, 1);
        // Once it ends, what it counted is kept.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(spend_of(&spends(&d).await, "sarah"), Some((0.0, 40.0)));
        assert_eq!(d.group_counts() - before, 1);

        // A count runs; the budget changes meanwhile; what it counted is
        // not kept, and the next read counts the new budget.
        d.group_spend_changed();
        let _ = spends(&d).await;
        assert_eq!(d.group_counts() - before, 2);
        acme_budget_of(&d, 50.0).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            d.0.group_spend.lock().kept().is_none(),
            "a count begun before the change isn't kept"
        );
        let _ = spends(&d).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(spend_of(&spends(&d).await, "sarah"), Some((0.0, 50.0)));
        assert_eq!(d.group_counts() - before, 3);
    })
    .await;
}

#[tokio::test]
async fn the_sidebars_routes_never_take_a_session_to_another_boss() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        groups(&d).await;
        let s = super::tests::new_session(&d, dir.path()).await;
        let item = session_item(s.id);
        d.change_layout(|l| l.put(&item, "acme", None))
            .await
            .unwrap();
        let crosses = |r: Result<Layout, DeskError>| matches!(r, Err(DeskError::CrossesBoss));
        // Into Atlas (Mia's), into Infra and out to Ungrouped (both the main
        // boss's): each crosses from Sarah, so each is refused.
        for to in ["atlas", "infra", UNGROUPED] {
            let r = d.change_layout_by_person(|l| l.put(&item, to, None)).await;
            assert!(crosses(r), "put into {to}");
        }
        let r = d.change_layout_by_person(|l| l.remove(&item)).await;
        assert!(crosses(r), "taken out of Acme");
        let r = d
            .change_layout_by_person(|l| l.create("new", "New", std::slice::from_ref(&item), None))
            .await;
        assert!(crosses(r), "a new group made with it");
        assert_eq!(
            d.layout().group_of(&item).map(|g| g.id.clone()),
            Some("acme".into()),
            "nothing moved"
        );

        // Within one boss's part, as before: Infra to Ungrouped and back.
        d.change_layout(|l| l.put(&item, "infra", None))
            .await
            .unwrap();
        d.change_layout_by_person(|l| l.put(&item, UNGROUPED, None))
            .await
            .unwrap();
        d.change_layout_by_person(|l| l.put(&item, "infra", None))
            .await
            .unwrap();
        // And everything else the person does still goes: an agent moves
        // as the sidebar has it.
        d.change_layout_by_person(|l| l.put("agent:local/scout", "atlas", None))
            .await
            .unwrap();
        assert_eq!(
            d.layout().group_of(&item).map(|g| g.id.clone()),
            Some("infra".into())
        );
    })
    .await;
}

#[tokio::test]
async fn a_forced_import_never_takes_a_session_to_another_boss() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        groups(&d).await;
        let s = super::tests::new_session(&d, dir.path()).await;
        let item = session_item(s.id);
        d.change_layout(|l| l.put(&item, "acme", None))
            .await
            .unwrap();
        let forced = |layout: Layout| Import {
            layout,
            session_names: Default::default(),
            force: true,
        };
        // Desk's own copy, with the session in Atlas: refused, nothing moves.
        let elsewhere = d.layout().put(&item, "atlas", None).unwrap();
        let r = d.import_layout(forced(elsewhere)).await;
        assert!(matches!(r, Err(DeskError::CrossesBoss)), "{r:?}");
        assert_eq!(
            d.layout().group_of(&item).map(|g| g.id.clone()),
            Some("acme".into())
        );
        // Within Acme it is taken as before: a group renamed.
        let renamed = d.layout().edit("acme", Some("Acme Ltd"), None).unwrap();
        let done = d.import_layout(forced(renamed)).await.unwrap();
        assert!(done.imported);
        assert_eq!(
            d.layout()
                .groups
                .iter()
                .find(|g| g.id == "acme")
                .map(|g| g.name.clone()),
            Some("Acme Ltd".into())
        );
    })
    .await;
}
