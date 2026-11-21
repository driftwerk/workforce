//! The links a session's Linear and Jira calls may go through: those of
//! the boards it reaches, as the ticket tool reaches them.

use std::collections::BTreeMap;
use std::path::Path;

use tokio_util::sync::CancellationToken;
use workforce_postgres::test_support::with_test_pool;

use super::super::tests::{desk, new_session, options};
use super::*;
use crate::board::{Filter, System, TicketState, Via};
use crate::line::Reach;
use crate::service::{LinkBoard, NewBoard, Options};
use crate::store::{build_pool, DeskStore};

/// A board named `target` in `group`, linked to Jira project `target`.
async fn linked(d: &Desk, target: &str, group: Option<&str>) {
    let b = d
        .create_board(NewBoard {
            name: target.into(),
            group: group.map(str::to_string),
            prefix: None,
        })
        .await
        .unwrap();
    d.link_board(
        b.id,
        LinkBoard {
            system: System::Jira,
            repo: None,
            target: target.into(),
            target_id: None,
            via: Via::ThisHub,
            agent: None,
            workspace: None,
            filter: Filter::All {
                include_done: false,
            },
            me: None,
            me_name: None,
            me_source: None,
            me_note: None,
            not_me: false,
            keep_tickets: false,
            states: BTreeMap::from([(TicketState::Todo, "To Do".to_string())]),
        },
        "sam",
    )
    .await
    .unwrap();
}

/// The targets of the links `session` reaches, sorted.
async fn targets(d: &Desk, session: &str) -> Vec<String> {
    let mut t: Vec<String> = d
        .issue_links(session)
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.target)
        .collect();
    t.sort();
    t
}

/// Groups Acme (with `acme_items` in it) and Open, a board linked in each
/// and one in no group.
async fn boards(d: &Desk, acme_items: Vec<String>) {
    d.change_layout(|l| {
        l.create("g-acme", "Acme", &acme_items, None)?
            .create("g-open", "Open", &[], None)
    })
    .await
    .unwrap();
    linked(d, "ACME", Some("g-acme")).await;
    linked(d, "OPEN", Some("g-open")).await;
    linked(d, "LOOSE", None).await;
}

#[tokio::test]
async fn a_grouped_session_reaches_only_its_groups_links() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let (inside, outside) = (
            new_session(&d, dir.path()).await,
            new_session(&d, dir.path()).await,
        );
        let devbox = Uuid::new_v4();
        boards(
            &d,
            vec![
                format!("session:{}", inside.id),
                format!("session:devbox/{devbox}"),
            ],
        )
        .await;
        assert_eq!(targets(&d, &inside.id.to_string()).await, ["ACME"]);
        assert_eq!(targets(&d, &format!("devbox/{devbox}")).await, ["ACME"]);
        assert_eq!(targets(&d, &outside.id.to_string()).await, ["LOOSE"]);
        // A session Desk places in no group reaches no group's link.
        let unknown = format!("devbox/{}", Uuid::new_v4());
        assert_eq!(targets(&d, &unknown).await, ["LOOSE"]);
    })
    .await;
}

async fn reaching_all(pool: workforce_postgres::PostgresPool, dir: &Path) -> Desk {
    DeskStore::migrate(&pool).await.unwrap();
    Desk::start(
        DeskStore::from_pool(pool),
        Options {
            line_reach: Reach::All,
            ..options(dir)
        },
        Default::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap()
}

/// Reaching every group, a session still never reaches past a group with
/// its own boss, nor one in it past its group.
#[tokio::test]
async fn a_bossed_group_is_a_boundary_even_when_sessions_reach_all() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = reaching_all(pool, dir.path()).await;
        let (inside, outside) = (
            new_session(&d, dir.path()).await,
            new_session(&d, dir.path()).await,
        );
        boards(&d, vec![format!("session:{}", inside.id)]).await;
        assert_eq!(
            targets(&d, &outside.id.to_string()).await,
            ["ACME", "LOOSE", "OPEN"]
        );
        d.set_bosses(vec![crate::service::Boss {
            agent: "ann".into(),
            name: "Ann".into(),
            scope: crate::scope::Scope::Group("g-acme".into()),
            level: "act".into(),
        }]);
        assert_eq!(
            targets(&d, &outside.id.to_string()).await,
            ["LOOSE", "OPEN"]
        );
        assert_eq!(targets(&d, &inside.id.to_string()).await, ["ACME"]);
    })
    .await;
}

fn boss_of(group: &str) -> crate::service::Boss {
    crate::service::Boss {
        agent: format!("boss-{group}"),
        name: group.into(),
        scope: crate::scope::Scope::Group(group.into()),
        level: "act".into(),
    }
}

async fn refusal(d: &Desk, session: &str) -> String {
    match d.issue_links(session).await {
        Err(DeskError::Invalid(why)) => why,
        other => panic!("{session} wasn't refused: {other:?}"),
    }
}

/// A machine is one OS account: on one running a client's sessions, only
/// that client's are given its links, none of its others, ungrouped ones
/// too; on one running more than one client's, none is given any.
#[tokio::test]
async fn a_machine_running_a_clients_sessions_gives_the_others_nothing() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let (client, loose) = (Uuid::new_v4(), Uuid::new_v4());
        boards(&d, vec![format!("session:devbox/{client}")]).await;
        d.set_bosses(vec![boss_of("g-acme")]);
        assert_eq!(targets(&d, &format!("devbox/{client}")).await, ["ACME"]);
        let why = refusal(&d, &format!("devbox/{loose}")).await;
        assert!(why.contains("runs another client's sessions"), "{why}");
        // Another machine, and this computer's own sessions, as they were.
        assert_eq!(targets(&d, &format!("laptop/{loose}")).await, ["LOOSE"]);
        let here = new_session(&d, dir.path()).await;
        assert_eq!(targets(&d, &here.id.to_string()).await, ["LOOSE"]);

        let other = Uuid::new_v4();
        d.change_layout(|l| {
            l.create(
                "g-globex",
                "Globex",
                &[format!("session:devbox/{other}")],
                None,
            )
        })
        .await
        .unwrap();
        d.set_bosses(vec![boss_of("g-acme"), boss_of("g-globex")]);
        for s in [client, other, loose] {
            let why = refusal(&d, &format!("devbox/{s}")).await;
            assert!(why.contains("sessions of more than one client"), "{why}");
        }
    })
    .await;
}
