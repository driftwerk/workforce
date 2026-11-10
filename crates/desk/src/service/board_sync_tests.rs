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

// ── The push ─────────────────────────────────────────────────────────────

/// How the fake Jira answers a create.
#[derive(Clone, Copy, PartialEq)]
enum Creates {
    InProject,
    /// With a key of another project.
    Elsewhere,
    /// Held for the person's approval.
    Held,
    /// Refused for its issue type.
    BadType,
}

struct WriteJira {
    creates: Creates,
    types: Vec<&'static str>,
    /// An issue already carrying a ticket's ref, outside the project.
    stray: Option<String>,
    calls: Mutex<Vec<(String, Value)>>,
}

impl WriteJira {
    fn new(creates: Creates) -> Self {
        Self {
            creates,
            types: vec!["Bug", "Task"],
            stray: None,
            calls: Mutex::new(vec![]),
        }
    }

    fn count(&self, tool: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t == tool)
            .count()
    }

    fn sent(&self, tool: &str) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t == tool)
            .map(|(_, a)| a.clone())
            .collect()
    }
}

#[async_trait]
impl Remote for WriteJira {
    async fn tools(&self) -> Result<Vec<String>, RemoteError> {
        Ok(crate::board_sync::JIRA_TOOLS
            .iter()
            .map(|t| t.to_string())
            .collect())
    }

    async fn call(&self, tool: &str, args: Value) -> Result<Value, RemoteError> {
        self.calls
            .lock()
            .unwrap()
            .push((tool.to_string(), args.clone()));
        match tool {
            "jira.search" => Ok(match (&self.stray, args["jql"].as_str()) {
                (Some(stray), Some(jql)) if jql.contains("desk-") => json!({ "issues": [
                    { "key": stray, "summary": "x", "status": "To Do", "updated": "2026-11-09T10:00:00.000+0000" }
                ], "is_last": true }),
                _ => json!({ "issues": [], "is_last": true }),
            }),
            "jira.get_project" => Ok(json!({
                "issue_types": self.types.iter().map(|t| json!({ "id": "1", "name": t })).collect::<Vec<_>>(),
            })),
            "jira.create_issue" => match self.creates {
                Creates::InProject => Ok(json!({ "key": "ACME-101", "id": "10101" })),
                Creates::Elsewhere => Ok(json!({ "key": "GLOBEX-5", "id": "20005" })),
                Creates::Held => Ok(json!({ "status": "approval_pending", "request_id": "r-1" })),
                Creates::BadType => Err(RemoteError::Failed(
                    "Failed to create issue: Specify a valid issue type".into(),
                )),
            },
            "jira.list_transitions" => Ok(json!({ "transitions": [] })),
            _ => Ok(json!({})),
        }
    }
}

async fn made_here(d: &Desk, b: &Board, by: &By) -> Uuid {
    d.create_ticket(
        crate::service::NewTicket {
            board: b.id,
            title: "Add a logout button".into(),
            body: String::new(),
            state: Some(TicketState::Todo),
            needs: vec![],
        },
        by,
    )
    .await
    .unwrap()
    .ticket
    .id
}

/// Makes every queued push of `b` due now, as if its wait were over.
async fn due_now(d: &Desk, b: Uuid) {
    let c = d.0.store.pool().get().await.unwrap();
    c.execute(
        "UPDATE board_outbox SET due_at = now() - interval '1 second' WHERE board = $1",
        &[&b],
    )
    .await
    .unwrap();
}

/// A create that answered with another project's key made an issue: the
/// board pauses, saying so, and it is never sent again.
#[tokio::test]
async fn a_create_answered_outside_the_project_is_never_sent_again() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        made_here(&d, &b, &By::you()).await;
        let mut remote = WriteJira::new(Creates::Elsewhere);
        d.push_board(b.id, &remote).await.unwrap();
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        assert!(
            matches!(s.paused, Some(Paused::Refused { .. })),
            "{:?}",
            s.paused
        );
        // Resumed, with the stray issue found by its ref: still never again.
        remote.stray = Some("GLOBEX-5".into());
        d.pause_board(b.id, false).await.unwrap();
        due_now(&d, b.id).await;
        d.push_board(b.id, &remote).await.unwrap();
        assert_eq!(remote.count("jira.create_issue"), 1);
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        assert!(matches!(s.paused, Some(Paused::Refused { .. })));
    })
    .await;
}

/// Only what the person made becomes an issue: an agent's ticket stays
/// Desk's own.
#[tokio::test]
async fn only_the_person_s_tickets_are_created_there() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        let agent = By {
            party: crate::crew::Party::agent("atlas", "Atlas"),
            reason: None,
            suggestion_id: None,
            hop: 0,
        };
        made_here(&d, &b, &agent).await;
        let remote = WriteJira::new(Creates::InProject);
        d.push_board(b.id, &remote).await.unwrap();
        assert_eq!(remote.count("jira.create_issue"), 0);
        made_here(&d, &b, &By::you()).await;
        d.push_board(b.id, &remote).await.unwrap();
        assert_eq!(remote.count("jira.create_issue"), 1);
        // The type is the project's Task.
        assert_eq!(remote.sent("jira.create_issue")[0]["issue_type"], "Task");
    })
    .await;
}

/// A create held for the person's approval is looked for, never sent again.
#[tokio::test]
async fn a_held_create_is_never_sent_again() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        made_here(&d, &b, &By::you()).await;
        let remote = WriteJira::new(Creates::Held);
        assert_eq!(d.push_board(b.id, &remote).await.unwrap().waiting, 1);
        due_now(&d, b.id).await;
        assert_eq!(d.push_board(b.id, &remote).await.unwrap().waiting, 1);
        assert_eq!(remote.count("jira.create_issue"), 1);
    })
    .await;
}

/// A project without the type: the board pauses rather than trying again;
/// with no Task, the first plain type is taken.
#[tokio::test]
async fn a_type_the_project_refuses_pauses_the_board() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        made_here(&d, &b, &By::you()).await;
        let mut remote = WriteJira::new(Creates::BadType);
        remote.types = vec!["Sub-task", "Story"];
        d.push_board(b.id, &remote).await.unwrap();
        assert_eq!(remote.sent("jira.create_issue")[0]["issue_type"], "Story");
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        assert!(
            matches!(s.paused, Some(Paused::Refused { .. })),
            "{:?}",
            s.paused
        );
    })
    .await;
}

/// No plan's tickets and no moved ticket go onto a synced board.
#[tokio::test]
async fn plans_and_moves_keep_off_a_synced_board() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let synced_board = synced(&d, "Acme", None).await;
        let planned = d.plan_board(None).await.unwrap();
        assert_ne!(planned.id, synced_board.id);
        assert!(planned.sync.is_none());
        let t = made_here(&d, &planned, &By::you()).await;
        assert!(matches!(
            d.ticket_goes(t, synced_board.id, &By::you()).await,
            Err(DeskError::Conflict(_))
        ));
        assert_eq!(d.ticket(t).await.unwrap().ticket.board, planned.id);
    })
    .await;
}
