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
use crate::board::Assignee;
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
        me_name: None,
        me_source: None,
        me_note: None,
        not_me: false,
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
    /// A tool the hub doesn't offer.
    missing: Option<&'static str>,
    calls: Mutex<Vec<(String, Value)>>,
}

impl WriteJira {
    fn new(creates: Creates) -> Self {
        Self {
            creates,
            types: vec!["Bug", "Task"],
            stray: None,
            missing: None,
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
        if self.missing == Some(tool) {
            return Err(RemoteError::Missing(tool.to_string()));
        }
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
        // Someone has it: a refused move leaves them on it.
        let c = d.0.store.pool().get().await.unwrap();
        let agent = serde_json::to_value(crate::crew::Party::agent("atlas", "Atlas")).unwrap();
        c.execute(
            "UPDATE tickets SET assignee = $2, state = 'doing' WHERE id = $1",
            &[&t, &agent],
        )
        .await
        .unwrap();
        drop(c);
        assert!(matches!(
            d.ticket_goes(t, synced_board.id, &By::you()).await,
            Err(DeskError::Conflict(_))
        ));
        let after = d.ticket(t).await.unwrap().ticket;
        assert_eq!(after.board, planned.id);
        assert_eq!(after.assignee.map(|a| a.id).as_deref(), Some("atlas"));
        assert_eq!(after.state, TicketState::Doing);
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

/// A tool the hub doesn't offer (a board linked before it was needed)
/// pauses the board as missing it, rather than backing off for ever.
#[tokio::test]
async fn a_tool_the_hub_lacks_pauses_the_board_as_missing() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        made_here(&d, &b, &By::you()).await;
        let mut remote = WriteJira::new(Creates::InProject);
        remote.missing = Some("jira.get_project");
        d.push_board(b.id, &remote).await.unwrap();
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        assert_eq!(
            s.paused,
            Some(Paused::Missing {
                tools: vec!["jira.get_project".into()]
            })
        );
        assert_eq!(remote.count("jira.create_issue"), 0);
    })
    .await;
}

/// A create held a day without approval stops, said on the ticket, and is
/// never sent again until the person changes the ticket.
#[tokio::test]
async fn a_create_held_a_day_stops_and_says_so() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        let t = made_here(&d, &b, &By::you()).await;
        let remote = WriteJira::new(Creates::Held);
        d.push_board(b.id, &remote).await.unwrap();
        let c = d.0.store.pool().get().await.unwrap();
        c.execute(
            "UPDATE board_outbox SET creating_at = now() - interval '25 hours', \
             due_at = now() - interval '1 second' WHERE ticket = $1",
            &[&t],
        )
        .await
        .unwrap();
        drop(c);
        d.push_board(b.id, &remote).await.unwrap();
        let v = d.ticket(t).await.unwrap();
        assert!(
            v.push_problem
                .as_deref()
                .is_some_and(|p| p.contains("wasn't approved")),
            "{:?}",
            v.push_problem
        );
        d.push_board(b.id, &remote).await.unwrap();
        assert_eq!(remote.count("jira.create_issue"), 1);
        // The person changes it: it may go again.
        d.edit_ticket(
            t,
            crate::service::TicketPatch {
                title: Some("Add a logout button, please".into()),
                ..Default::default()
            },
            &By::you(),
        )
        .await
        .unwrap();
        assert_eq!(d.ticket(t).await.unwrap().push_problem, None);
    })
    .await;
}

/// A session moving group never takes a client's synced ticket along, nor
/// keeps it: the ticket stays on its board, released.
#[tokio::test]
async fn a_synced_ticket_is_released_when_its_session_moves_group() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        d.pull_board(b.id, &jira(1, &[])).await.unwrap();
        let t = d
            .tickets(b.id, &Default::default())
            .await
            .unwrap()
            .remove(0)
            .ticket
            .id;
        let other = d.plan_board(None).await.unwrap();
        let c = d.0.store.pool().get().await.unwrap();
        let agent = serde_json::to_value(crate::crew::Party::agent("atlas", "Atlas")).unwrap();
        c.execute(
            "UPDATE tickets SET assignee = $2, state = 'doing' WHERE id = $1",
            &[&t, &agent],
        )
        .await
        .unwrap();
        drop(c);
        assert!(matches!(
            d.ticket_goes(t, other.id, &By::you()).await,
            Err(DeskError::Conflict(_))
        ));
        let after = d.ticket(t).await.unwrap().ticket;
        assert_eq!(after.board, b.id, "it stays on its board");
        assert_eq!(after.assignee, None, "and no one in another group has it");
    })
    .await;
}

// ── A whole Linear team ──────────────────────────────────────────────────

const TEAM: &str = "8a7b6c5d-0000-4000-8000-000000000001";
const ME: &str = "11111111-2222-4333-8444-555555555555";
const ANA: &str = "22222222-2222-4333-8444-555555555555";

/// A Linear team as a newer hub's tools answer it: newest change first,
/// fifty a page with a cursor, and the open ones only when asked.
#[derive(Default)]
struct FakeLinear {
    /// key → (state type, updated, assignee id)
    issues: Mutex<BTreeMap<u32, (String, String, Option<String>)>>,
    calls: Mutex<Vec<(String, Value)>>,
    /// What `linear.viewer` answers; none: the hub doesn't offer it.
    viewer: Mutex<Option<Value>>,
    /// Someone who takes an issue just before an assign lands.
    racer: Mutex<Option<String>>,
}

impl FakeLinear {
    fn node(n: u32, (kind, updated, who): &(String, String, Option<String>)) -> Value {
        json!({
            "id": format!("id-{n}"), "identifier": format!("ENG-{n}"),
            "title": format!("Issue {n}"), "description": "About it.",
            "url": format!("https://linear.example/ENG-{n}"),
            "state": { "name": kind, "type": kind },
            "updatedAt": updated,
            "assignee": who.as_ref().map(|id| match id.as_str() {
                // An assignee the answer names without an id.
                "noid" => json!({ "name": "Someone" }),
                id => json!({ "id": id, "name": if id == ME { "Sam" } else { "Ana" } }),
            }),
            "labels": { "nodes": [] },
        })
    }

    fn set(&self, n: u32, kind: &str, minute: u32, who: Option<&str>) {
        self.issues.lock().unwrap().insert(
            n,
            (
                kind.into(),
                format!(
                    "2026-11-09T{:02}:{:02}:00.000Z",
                    10 + minute / 60,
                    minute % 60
                ),
                who.map(str::to_string),
            ),
        );
    }

    fn calls_of(&self, tool: &str) -> Vec<Value> {
        let c = self.calls.lock().unwrap();
        c.iter()
            .filter(|(t, _)| t == tool)
            .map(|(_, a)| a.clone())
            .collect()
    }
}

#[async_trait]
impl Remote for FakeLinear {
    async fn tools(&self) -> Result<Vec<String>, RemoteError> {
        Ok(crate::board_sync::LINEAR_TOOLS
            .iter()
            .map(|t| t.to_string())
            .collect())
    }

    async fn call(&self, tool: &str, args: Value) -> Result<Value, RemoteError> {
        self.calls
            .lock()
            .unwrap()
            .push((tool.to_string(), args.clone()));
        let mut issues = self.issues.lock().unwrap();
        match tool {
            "linear.viewer" => self
                .viewer
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| RemoteError::Missing(tool.into())),
            "linear.issue_list" => {
                let open = args["open_only"].as_bool() == Some(true);
                let since = args["updated_since"].as_str().map(str::to_string);
                let mut all: Vec<(u32, (String, String, Option<String>))> = issues
                    .iter()
                    .filter(|(_, (k, u, _))| {
                        (!open || !matches!(k.as_str(), "completed" | "canceled"))
                            && since.as_ref().is_none_or(|s| {
                                chrono::DateTime::parse_from_rfc3339(u).unwrap()
                                    >= chrono::DateTime::parse_from_rfc3339(s).unwrap()
                            })
                    })
                    .map(|(n, i)| (*n, i.clone()))
                    .collect();
                all.sort_by(|a, b| b.1 .1.cmp(&a.1 .1));
                let from: usize = args["after"]
                    .as_str()
                    .map_or(0, |c| c.trim_start_matches("p").parse().unwrap());
                let page: Vec<Value> = all
                    .iter()
                    .skip(from)
                    .take(50)
                    .map(|(n, i)| Self::node(*n, i))
                    .collect();
                let more = from + 50 < all.len();
                Ok(json!({ "count": page.len(), "issues": page,
                    "page_info": { "has_next_page": more, "end_cursor": format!("p{}", from + 50) } }))
            }
            "linear.issue_get" => {
                let n: u32 = args["identifier"]
                    .as_str()
                    .and_then(|k| k.strip_prefix("ENG-"))
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                match issues.get(&n) {
                    Some(i) => Ok(json!({ "issue": Self::node(n, i) })),
                    None => Err(RemoteError::Failed("404 not found".into())),
                }
            }
            "linear.issue_update" => {
                let n: u32 = args["identifier"]
                    .as_str()
                    .and_then(|k| k.strip_prefix("ENG-"))
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                let i = issues.get_mut(&n).unwrap();
                i.2 = self
                    .racer
                    .lock()
                    .unwrap()
                    .clone()
                    .or_else(|| args["assignee_id"].as_str().map(str::to_string));
                i.1 = "2026-11-09T23:00:00.000Z".into();
                Ok(json!({ "issue": Self::node(n, i) }))
            }
            other => Err(RemoteError::NotAllowed(other.into())),
        }
    }
}

async fn whole_team(d: &Desk, me: Option<&str>) -> Board {
    let b = d
        .create_board(NewBoard {
            name: "Eng".into(),
            group: None,
            prefix: None,
        })
        .await
        .unwrap();
    d.link_board(
        b.id,
        LinkBoard {
            system: System::Linear,
            target: "ENG".into(),
            target_id: Some(TEAM.into()),
            via: Via::ThisHub,
            filter: Filter::All {
                include_done: false,
            },
            me: me.map(str::to_string),
            me_name: me.map(|_| "Sam".to_string()),
            me_source: None,
            me_note: None,
            not_me: false,
            states: BTreeMap::from([
                (TicketState::Todo, "unstarted".to_string()),
                (TicketState::Done, "completed".to_string()),
            ]),
        },
        "sam",
    )
    .await
    .unwrap()
}

/// A whole team comes in page by page, whoever has each issue and none,
/// with no done issue; who has each is kept, and a card whose issue is
/// done there moves to Done.
#[tokio::test]
async fn a_whole_team_comes_in_page_by_page_with_who_has_each() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = whole_team(&d, Some(ME)).await;
        let linear = FakeLinear::default();
        for n in 1..=130 {
            let who = match n % 3 {
                0 => None,
                1 => Some(ME),
                _ => Some(ANA),
            };
            linear.set(n, "unstarted", n, who);
        }
        for n in 131..=140 {
            linear.set(n, "completed", n, Some(ANA));
        }
        let pulled = d.pull_board(b.id, &linear).await.unwrap();
        assert_eq!(
            (pulled.imported, pulled.skipped.clone()),
            (130, None),
            "{pulled:?}"
        );
        // Three pages, the open ones only, and no member id asked for.
        let lists = linear.calls_of("linear.issue_list");
        assert_eq!(lists.len(), 3, "{lists:?}");
        assert!(lists
            .iter()
            .all(|a| a["open_only"] == json!(true) && a.get("assignee_id").is_none()));
        let tickets = d.tickets(b.id, &Default::default()).await.unwrap();
        let unassigned = tickets
            .iter()
            .filter(|t| t.ticket.external.as_ref().unwrap().assignee == Some(None))
            .count();
        assert_eq!(unassigned, 43);
        let ana = tickets
            .iter()
            .find(|t| t.key.as_deref() == Some("ENG-2"))
            .unwrap();
        assert_eq!(
            ana.ticket.external.as_ref().unwrap().assignee,
            Some(Some(Assignee {
                id: ANA.into(),
                name: "Ana".into()
            }))
        );
        // Read through: the cursor is a little before the latest change it
        // saw, and never past when the listing began.
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        assert!(
            s.cursor.as_deref().unwrap().starts_with("2026-11-09T12:08"),
            "{:?}",
            s.cursor
        );
        assert_eq!(s.listing, None);

        // ENG-3 is done there, ENG-4 changes hands: the next pull follows.
        linear.set(3, "completed", 200, None);
        linear.set(4, "unstarted", 201, Some(ANA));
        linear.calls.lock().unwrap().clear();
        let pulled = d.pull_board(b.id, &linear).await.unwrap();
        assert_eq!(pulled.updated, 2, "{pulled:?}");
        let since = linear.calls_of("linear.issue_list");
        assert!(since[0]["updated_since"].is_string() && since[0].get("open_only").is_none());
        let t = |key: &str| {
            let d = d.clone();
            let key = key.to_string();
            async move {
                d.tickets(b.id, &Default::default())
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|t| t.key.as_deref() == Some(key.as_str()))
                    .unwrap()
            }
        };
        assert_eq!(t("ENG-3").await.ticket.state, TicketState::Done);
        assert_eq!(
            t("ENG-4")
                .await
                .ticket
                .external
                .unwrap()
                .assignee
                .unwrap()
                .unwrap()
                .id,
            ANA
        );
        // No done issue that wasn't on the board came on.
        assert_eq!(
            d.tickets(b.id, &Default::default()).await.unwrap().len(),
            130
        );
    })
    .await;
}

fn viewer(personal: bool) -> Value {
    json!({ "id": ME, "name": "sam", "display_name": "Sam", "personal": personal })
}

async fn taker(d: &Desk, linear: &FakeLinear) -> Board {
    let b = whole_team(d, None).await;
    d.pull_board(b.id, linear).await.unwrap();
    b
}

async fn ticket_of(d: &Desk, b: &Board, key: &str) -> crate::service::TicketView {
    d.tickets(b.id, &Default::default())
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.key.as_deref() == Some(key))
        .unwrap()
}

fn refused(r: Result<crate::service::TicketView, DeskError>, words: &str) {
    assert!(
        matches!(&r, Err(DeskError::Conflict(w)) if w.contains(words)),
        "{words:?}: {r:?}"
    );
}

/// Take it assigns an unassigned issue to the person there, as themselves,
/// with the tool a move uses, once their own key vouches for who they are,
/// asked afresh; it reads the issue again after. One someone has stays
/// theirs.
#[tokio::test]
async fn take_it_assigns_an_unassigned_issue_to_the_person() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let linear = FakeLinear::default();
        *linear.viewer.lock().unwrap() = Some(viewer(true));
        linear.set(1, "unstarted", 1, None);
        linear.set(2, "unstarted", 2, None);
        let b = taker(&d, &linear).await;
        let you = crate::crew::By::you();
        let one = ticket_of(&d, &b, "ENG-1").await;
        let taken = d
            .take_ticket(one.ticket.id, &linear, "Atlas", &you)
            .await
            .unwrap();
        assert_eq!(
            taken.ticket.external.unwrap().assignee,
            Some(Some(Assignee {
                id: ME.into(),
                name: "Sam".into()
            }))
        );
        assert_eq!(
            linear.calls_of("linear.issue_update"),
            vec![json!({ "identifier": "ENG-1", "assignee_id": ME })]
        );
        // Read before and after the write, and who the person is asked each time.
        assert_eq!(linear.calls_of("linear.issue_get").len(), 2);
        assert_eq!(linear.calls_of("linear.viewer").len(), 1);
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        assert_eq!(
            (s.me.as_deref(), s.me_source),
            (Some(ME), Some(crate::board::MeSource::Vouched))
        );
        // Ana took ENG-2 meanwhile: it stays hers, and the card says so.
        linear.set(2, "unstarted", 3, Some(ANA));
        let two = ticket_of(&d, &b, "ENG-2").await;
        refused(
            d.take_ticket(two.ticket.id, &linear, "Atlas", &you).await,
            "Ana has ENG-2",
        );
        assert_eq!(linear.calls_of("linear.issue_update").len(), 1);
        assert_eq!(
            ticket_of(&d, &b, "ENG-2")
                .await
                .ticket
                .external
                .unwrap()
                .assignee,
            Some(Some(Assignee {
                id: ANA.into(),
                name: "Ana".into()
            }))
        );
    })
    .await;
}

/// A shared key, an older hub, an id the person typed, or a hub that now
/// says someone else: nothing is assigned, and the person is told why.
#[tokio::test]
async fn take_it_goes_only_to_whom_the_person_s_own_key_says() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let linear = FakeLinear::default();
        linear.set(1, "unstarted", 1, None);
        let b = taker(&d, &linear).await;
        let t = ticket_of(&d, &b, "ENG-1").await.ticket.id;
        let you = crate::crew::By::you();
        // The hub doesn't offer linear.viewer.
        refused(
            d.take_ticket(t, &linear, "Atlas", &you).await,
            "linear.viewer",
        );
        // An agent's shared key answers as its owner.
        *linear.viewer.lock().unwrap() = Some(viewer(false));
        refused(
            d.take_ticket(t, &linear, "Atlas", &you).await,
            "Atlas uses a shared Linear key, so it can't say who you are.",
        );
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        assert_eq!(s.me, None);
        assert!(s.me_note.unwrap().contains("shared Linear key"));
        // An older hub that doesn't say whose key it used.
        *linear.viewer.lock().unwrap() = Some(json!({ "id": ME, "name": "sam" }));
        refused(
            d.take_ticket(t, &linear, "Atlas", &you).await,
            "can't say who you are",
        );
        // Vouched once, then the hub answers as someone else.
        *linear.viewer.lock().unwrap() = Some(viewer(true));
        let other = "33333333-2222-4333-8444-555555555555";
        d.change_sync(b.id, |s| {
            s.me = Some(other.into());
            s.me_source = Some(crate::board::MeSource::Vouched);
        })
        .await
        .unwrap();
        refused(
            d.take_ticket(t, &linear, "Atlas", &you).await,
            "now says you are Sam",
        );
        // An id the person typed is never assigned to.
        d.change_sync(b.id, |s| {
            s.me = Some(ME.into());
            s.me_source = Some(crate::board::MeSource::Typed);
        })
        .await
        .unwrap();
        refused(
            d.take_ticket(t, &linear, "Atlas", &you).await,
            "You typed your Linear id",
        );
        assert!(linear.calls_of("linear.issue_update").is_empty());
    })
    .await;
}

/// An answer that doesn't say who has the issue is never read as no one;
/// a race lost after the write is said.
#[tokio::test]
async fn take_it_needs_a_clear_answer_and_says_a_race_lost() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let linear = FakeLinear::default();
        *linear.viewer.lock().unwrap() = Some(viewer(true));
        linear.set(1, "unstarted", 1, None);
        linear.set(2, "unstarted", 2, None);
        let b = taker(&d, &linear).await;
        let you = crate::crew::By::you();
        // Someone has ENG-1, but the answer gives no id.
        linear.set(1, "unstarted", 3, Some("noid"));
        let one = ticket_of(&d, &b, "ENG-1").await.ticket.id;
        refused(
            d.take_ticket(one, &linear, "Atlas", &you).await,
            "can't tell who has ENG-1",
        );
        assert!(linear.calls_of("linear.issue_update").is_empty());
        // Ana's assign lands just before ours.
        *linear.racer.lock().unwrap() = Some(ANA.into());
        let two = ticket_of(&d, &b, "ENG-2").await.ticket.id;
        refused(
            d.take_ticket(two, &linear, "Atlas", &you).await,
            "Ana took ENG-2 on Linear just before you",
        );
        assert_eq!(
            ticket_of(&d, &b, "ENG-2")
                .await
                .ticket
                .external
                .unwrap()
                .assignee
                .unwrap()
                .unwrap()
                .id,
            ANA
        );
    })
    .await;
}

/// A Linear listing whose next page is the one it just read ends as an
/// error, and the listing left part read is dropped.
#[tokio::test]
async fn a_linear_page_that_repeats_ends_the_listing() {
    struct Loop;
    #[async_trait]
    impl Remote for Loop {
        async fn tools(&self) -> Result<Vec<String>, RemoteError> {
            Ok(vec![])
        }
        async fn call(&self, _tool: &str, args: Value) -> Result<Value, RemoteError> {
            let page = args["after"].as_str().unwrap_or("p1").to_string();
            Ok(json!({ "issues": [], "page_info": { "has_next_page": true, "end_cursor": page } }))
        }
    }
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = whole_team(&d, None).await;
        let pulled = d.pull_board(b.id, &Loop).await.unwrap();
        assert!(
            pulled
                .skipped
                .as_deref()
                .unwrap()
                .contains("same page twice"),
            "{pulled:?}"
        );
        d.change_sync(b.id, |s| {
            s.listing = Some(crate::board::Listing {
                after: "p1".into(),
                since: None,
                top: None,
                started: None,
            });
            s.backoff_until = None;
        })
        .await
        .unwrap();
        d.pull_board(b.id, &Loop).await.unwrap();
        assert_eq!(d.board_sync(b.id).await.unwrap().unwrap().1.listing, None);
    })
    .await;
}

/// A board linked before who has each issue was kept learns it for every
/// ticket on its first pass, even with nothing changed there.
#[tokio::test]
async fn an_older_board_learns_who_has_each_issue_on_its_first_pass() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = whole_team(&d, None).await;
        let linear = FakeLinear::default();
        linear.set(1, "unstarted", 1, Some(ANA));
        linear.set(2, "unstarted", 2, None);
        d.pull_board(b.id, &linear).await.unwrap();
        // As a board from before: no assignee known, swept just now.
        for t in d.tickets(b.id, &Default::default()).await.unwrap() {
            let mut t = t.ticket;
            t.external.as_mut().unwrap().assignee = None;
            d.save_ticket(&mut t).await.unwrap();
        }
        d.change_sync(b.id, |s| {
            s.assignees_swept = false;
            s.swept_at = Some(crate::crew::now());
            s.pulled_at = None;
        })
        .await
        .unwrap();
        d.pull_board(b.id, &linear).await.unwrap();
        let who = |key: &str| {
            let d = d.clone();
            let key = key.to_string();
            async move {
                d.tickets(b.id, &Default::default())
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|t| t.key.as_deref() == Some(key.as_str()))
                    .unwrap()
                    .ticket
                    .external
                    .unwrap()
                    .assignee
            }
        };
        assert_eq!(who("ENG-1").await.flatten().unwrap().id, ANA);
        assert_eq!(who("ENG-2").await, Some(None));
        assert!(d.board_sync(b.id).await.unwrap().unwrap().1.assignees_swept);
    })
    .await;
}
