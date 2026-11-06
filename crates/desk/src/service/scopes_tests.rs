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
        let at = Utc::now() - chrono::Duration::minutes(5);
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
