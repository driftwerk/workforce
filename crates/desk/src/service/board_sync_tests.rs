//! The pull against a fake Jira in the desk: a pass that runs out of calls
//! keeps what it took in, an issue that always fails is tried on its own
//! back-off while the cursor goes on, and one project key may be linked
//! once per hub.

use std::collections::{BTreeMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{json, Value};
use workforce_postgres::test_support::with_test_pool;

use super::super::tests::desk;
use super::*;
use crate::board_sync::{Remote, RemoteError};
use crate::service::NewBoard;
use crate::store::build_pool;

/// A Jira whose search answers every issue, oldest first, and whose
/// `get_issue` fails for the keys in `failing`.
struct FakeJira {
    issues: Vec<(String, String)>,
    failing: HashSet<String>,
    calls: Mutex<usize>,
}

fn wrapped(s: &str) -> Value {
    json!(s)
}

#[async_trait]
impl Remote for FakeJira {
    async fn tools(&self) -> Result<Vec<String>, RemoteError> {
        Ok(crate::board_sync::JIRA_TOOLS
            .iter()
            .map(|t| t.to_string())
            .collect())
    }

    async fn call(&self, tool: &str, args: Value) -> Result<Value, RemoteError> {
        *self.calls.lock().unwrap() += 1;
        let issue = |(key, updated): &(String, String), full: bool| {
            let mut v = json!({ "key": key, "summary": wrapped(&format!("Issue {key}")),
                "status": "To Do", "updated": updated, "labels": ["web"] });
            if full {
                v["description_html"] = json!("<p>About it.</p>");
            }
            v
        };
        match tool {
            "jira.search" => Ok(json!({
                "issues": self.issues.iter().map(|i| issue(i, false)).collect::<Vec<_>>(),
                "is_last": true,
            })),
            "jira.get_issue" => {
                let key = args["issue_key"].as_str().unwrap_or_default();
                if self.failing.contains(key) {
                    return Err(RemoteError::Failed(format!(
                        "Failed to get issue {key}: 500"
                    )));
                }
                self.issues
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|i| issue(i, true))
                    .ok_or_else(|| RemoteError::Failed("404".into()))
            }
            other => Err(RemoteError::NotAllowed(other.into())),
        }
    }
}

fn jira(n: usize, failing: &[&str]) -> FakeJira {
    FakeJira {
        issues: (1..=n)
            .map(|i| {
                (
                    format!("ACME-{i}"),
                    format!("2026-11-09T10:{:02}:{:02}.000+0000", i / 60, i % 60),
                )
            })
            .collect(),
        failing: failing.iter().map(|s| s.to_string()).collect(),
        calls: Mutex::new(0),
    }
}

fn link(hub: Option<&str>) -> LinkBoard {
    LinkBoard {
        system: System::Jira,
        target: "ACME".into(),
        target_id: None,
        via: match hub {
            Some(h) => Via::Hub { hub: h.into() },
            None => Via::ThisHub,
        },
        filter: Filter::Label("web".into()),
        me: None,
        states: BTreeMap::from([(TicketState::Todo, "To Do".to_string())]),
    }
}

async fn synced(d: &Desk, name: &str, hub: Option<&str>) -> Board {
    let b = d
        .create_board(NewBoard {
            name: name.into(),
            group: None,
            prefix: None,
        })
        .await
        .unwrap();
    d.link_board(b.id, link(hub), "sam").await.unwrap()
}

/// A pass that runs out of calls is no failure: what it took in stands,
/// the cursor moves to it, and nothing backs off.
#[tokio::test]
async fn a_pass_out_of_calls_keeps_what_it_took_in() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        let remote = jira(80, &[]);
        let pulled = d.pull_board(b.id, &remote).await.unwrap();
        assert!(pulled.imported > 0 && pulled.imported < 80, "{pulled:?}");
        assert_eq!(pulled.skipped, None);
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        assert_eq!((s.failures, s.backoff_until, s.paused), (0, None, None));
        assert!(s.cursor.is_some());
        // The next pass goes on from there.
        d.pull_board(b.id, &remote).await.unwrap();
        assert_eq!(
            d.tickets(b.id, &Default::default()).await.unwrap().len(),
            80
        );
    })
    .await;
}

/// An issue that always fails is set aside on its own back-off; the cursor
/// goes on past it and the rest come in.
#[tokio::test]
async fn an_issue_that_always_fails_doesnt_hold_up_the_rest() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        let remote = jira(3, &["ACME-2"]);
        d.pull_board(b.id, &remote).await.unwrap();
        let keys: Vec<String> = d
            .tickets(b.id, &Default::default())
            .await
            .unwrap()
            .into_iter()
            .filter_map(|v| v.key)
            .collect();
        assert_eq!(keys.len(), 2, "{keys:?}");
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        assert!(
            s.cursor.as_deref().unwrap().contains("10:00:03"),
            "{:?}",
            s.cursor
        );
        assert_eq!(s.retry["ACME-2"].tries, 1);
        assert_eq!(s.backoff_until, None);
    })
    .await;
}

/// One project key may be linked once per hub: Acme's ACME and Globex's
/// ACME are two boards, each with its own issues.
#[tokio::test]
async fn a_project_key_is_linked_once_per_hub() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        d.put_sync_hub("acme", "Acme", "https://acme.example", "sam")
            .await
            .unwrap();
        d.put_sync_hub("globex", "Globex", "https://globex.example", "sam")
            .await
            .unwrap();
        let a = synced(&d, "Acme", Some("acme")).await;
        let g = synced(&d, "Globex", Some("globex")).await;
        let again = d
            .create_board(NewBoard {
                name: "Again".into(),
                group: None,
                prefix: None,
            })
            .await
            .unwrap();
        assert!(matches!(
            d.link_board(again.id, link(Some("acme")), "sam").await,
            Err(DeskError::Conflict(_))
        ));
        let remote = jira(2, &[]);
        d.pull_board(a.id, &remote).await.unwrap();
        d.pull_board(g.id, &remote).await.unwrap();
        for b in [a.id, g.id] {
            assert_eq!(d.tickets(b, &Default::default()).await.unwrap().len(), 2);
        }
    })
    .await;
}

/// A synced ticket's title reaches a move's note only as its issue's words.
#[tokio::test]
async fn a_move_note_says_a_synced_title_as_the_issue_s() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        d.pull_board(b.id, &jira(1, &[])).await.unwrap();
        let t = d
            .tickets(b.id, &Default::default())
            .await
            .unwrap()
            .remove(0);
        let label = d.item_label(&crate::moves::Item::Ticket(t.ticket.id)).await;
        assert!(label.contains("Jira issue's own words"), "{label}");
    })
    .await;
}

/// Two links in two groups may each hold ACME-1: a key names the one the
/// caller reaches, and the palette shows both.
#[tokio::test]
async fn one_key_on_two_links_names_the_one_in_reach() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        d.put_sync_hub("acme", "Acme", "https://acme.example", "sam").await.unwrap();
        d.put_sync_hub("globex", "Globex", "https://globex.example", "sam").await.unwrap();
        let a = synced(&d, "Acme", Some("acme")).await;
        let g = synced(&d, "Globex", Some("globex")).await;
        // Each board's group, as the sidebar would have it.
        for (b, group) in [(a.id, "g-acme"), (g.id, "g-globex")] {
            let c = d.0.store.pool().get().await.unwrap();
            c.execute("UPDATE boards SET group_id = $2 WHERE id = $1", &[&b, &group])
                .await
                .unwrap();
        }
        let remote = jira(1, &[]);
        for b in [a.id, g.id] {
            let c = d.0.store.pool().get().await.unwrap();
            c.execute("UPDATE boards SET sync = jsonb_set(sync, '{group}', to_jsonb(group_id)) WHERE id = $1", &[&b])
                .await
                .unwrap();
            d.pull_board(b, &remote).await.unwrap();
        }
        for (group, board) in [("g-acme", a.id), ("g-globex", g.id)] {
            let t = d
                .ticket_by_key_in("ACME-1", |g| g == Some(group))
                .await
                .unwrap();
            assert_eq!(t.ticket.board, board, "{group}");
        }
        assert_eq!(d.search_tickets("ACME-1").await.unwrap().len(), 2);
    })
    .await;
}
