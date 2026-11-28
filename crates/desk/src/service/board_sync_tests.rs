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
        repo: None,
        target: "ACME".into(),
        target_id: None,
        via: match hub {
            Some(h) => Via::Hub { hub: h.into() },
            None => Via::ThisHub,
        },
        agent: None,
        workspace: None,
        filter: Filter::Label("web".into()),
        me: None,
        me_name: None,
        me_source: None,
        me_note: None,
        not_me: false,
        keep_tickets: false,
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
            preference: None,
            outside: None,
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
    /// What the hub's `identity.whoami` says of the person's verified email.
    verified_email: Mutex<Option<String>>,
    /// The workspace's members: (id, name, email).
    members: Mutex<Vec<(String, String, String)>>,
    /// Whom the person chose to be there: (id, name).
    chosen: Mutex<Option<(String, String)>>,
    /// Issues whose description is this, at the workspace `acme`'s address.
    described: Mutex<BTreeMap<u32, String>>,
    /// Members the hub finds by name, with their profile: (slug, full name).
    profiles: Mutex<Vec<(String, String)>>,
    /// Issues' sortOrder, for those that answer one.
    sorts: Mutex<BTreeMap<u32, f64>>,
    /// The hub holds a sortOrder change for the person's approval.
    hold_sort: Mutex<bool>,
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
        let mut answer = self.answer(tool, args).await?;
        // A described issue: its description, at a Linear address.
        let described = self.described.lock().unwrap();
        let sorts = self.sorts.lock().unwrap();
        let nodes = match answer.get_mut("issues") {
            Some(Value::Array(a)) => a.iter_mut().collect::<Vec<_>>(),
            _ => answer.get_mut("issue").into_iter().collect(),
        };
        for i in nodes {
            let n: u32 = i["identifier"]
                .as_str()
                .and_then(|k| k.strip_prefix("ENG-"))
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            if let Some(body) = described.get(&n) {
                i["description"] = json!(body);
                i["url"] = json!(format!("https://linear.app/acme/issue/ENG-{n}"));
            }
            if let Some(at) = sorts.get(&n) {
                i["sortOrder"] = json!(at);
            }
        }
        Ok(answer)
    }
}

impl FakeLinear {
    async fn answer(&self, tool: &str, args: Value) -> Result<Value, RemoteError> {
        self.calls
            .lock()
            .unwrap()
            .push((tool.to_string(), args.clone()));
        let mut issues = self.issues.lock().unwrap();
        match tool {
            // The hub's own name search, with each member's profile.
            "identity.linear_members_named" => {
                let name = args["name"].as_str().unwrap_or_default().to_lowercase();
                let members: Vec<Value> = self
                    .profiles
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(slug, _)| slug.contains(&name))
                    .map(|(slug, full)| {
                        json!({ "id": slug, "name": slug, "full_name": full,
                        "url": format!("https://linear.app/acme/profiles/{slug}") })
                    })
                    .collect();
                Ok(json!({ "members": members }))
            }
            "linear.viewer" => self
                .viewer
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| RemoteError::Missing(tool.into())),
            // The hub's own match: its filters are the hub's to test; here,
            // what it answers for the person's verified email.
            "identity.linear_member_by_my_email" => {
                if let Some((id, name)) = self.chosen.lock().unwrap().clone() {
                    return Ok(
                        json!({ "user": { "id": id, "name": name }, "why": null, "source": "chosen" }),
                    );
                }
                let Some(want) = self.verified_email.lock().unwrap().clone() else {
                    return Ok(json!({ "user": null, "why": "no_verified_email" }));
                };
                let found: Vec<(String, String)> = self
                    .members
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(_, _, e)| e.eq_ignore_ascii_case(&want))
                    .map(|(id, name, _)| (id.clone(), name.clone()))
                    .collect();
                Ok(match found.as_slice() {
                    [(id, name)] => json!({ "user": { "id": id, "name": name }, "why": null }),
                    [] => json!({ "user": null, "why": "none" }),
                    _ => json!({ "user": null, "why": "several" }),
                })
            }
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
                if args.get("sort_order").is_some() && *self.hold_sort.lock().unwrap() {
                    return Ok(json!({ "status": "approval_pending", "request_id": "r-2" }));
                }
                let i = issues.get_mut(&n).unwrap();
                if let Some(at) = args["sort_order"].as_f64() {
                    self.sorts.lock().unwrap().insert(n, at);
                }
                let racer = self.racer.lock().unwrap().clone();
                if racer.is_some() || args.get("assignee_id").is_some() {
                    i.2 = racer.or_else(|| args["assignee_id"].as_str().map(str::to_string));
                }
                i.1 = "2026-11-09T23:00:00.000Z".into();
                Ok(json!({ "issue": Self::node(n, i) }))
            }
            "linear.issue_link" => Ok(json!({ "success": true })),
            other => Err(RemoteError::NotAllowed(other.into())),
        }
    }
}

/// A description's profile of its own workspace comes in as a mention of
/// the member the hub finds it is; one the hub doesn't find, or another
/// workspace's, as it is; and an edit here sends each back as it came.
#[tokio::test]
async fn a_linear_profile_comes_in_as_a_mention_and_goes_back() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = whole_team(&d, Some(ME)).await;
        let linear = FakeLinear::default();
        linear.set(1, "unstarted", 1, None);
        let said = "Ask [@ann](https://linear.app/acme/profiles/ann), https://linear.app/acme/profiles/zed and https://linear.app/other/profiles/ann.";
        linear.described.lock().unwrap().insert(1, said.into());
        linear
            .profiles
            .lock()
            .unwrap()
            .extend([("ann".to_string(), "Ann Lee".to_string()), ("annie".to_string(), "Annie".to_string())]);
        d.pull_board(b.id, &linear).await.unwrap();
        let tickets = d.tickets(b.id, &Default::default()).await.unwrap();
        let t = &tickets[0].ticket;
        assert_eq!(
            t.body,
            "Ask :mention[Ann Lee]{url=\"https://linear.app/acme/profiles/ann\" text=\"@ann\"}, https://linear.app/acme/profiles/zed and https://linear.app/other/profiles/ann."
        );
        // Each of its own workspace's looked up once, by its slug.
        let asked = linear.calls_of("identity.linear_members_named");
        assert_eq!(asked, vec![json!({ "name": "ann" }), json!({ "name": "zed" })]);
        // Edited here: it goes back with the profile as Linear had it.
        d.edit_ticket(
            t.id,
            crate::service::TicketPatch {
                body: Some(format!("{} More.", t.body)),
                ..Default::default()
            },
            &By::you(),
        )
        .await
        .unwrap();
        due_now(&d, b.id).await;
        d.push_board(b.id, &linear).await.unwrap();
        let sent = linear.calls_of("linear.issue_update");
        assert_eq!(sent.last().unwrap()["description"], json!(format!("{said} More.")));
    })
    .await;
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
            repo: None,
            target: "ENG".into(),
            target_id: Some(TEAM.into()),
            via: Via::ThisHub,
            agent: None,
            workspace: None,
            filter: Filter::All {
                include_done: false,
            },
            me: me.map(str::to_string),
            me_name: me.map(|_| "Sam".to_string()),
            me_source: None,
            me_note: None,
            not_me: false,
            keep_tickets: false,
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

/// A whole team pulled, and who the person is asked of the hub (as the
/// link does), when it can say.
async fn taker(d: &Desk, linear: &FakeLinear) -> Board {
    let b = whole_team(d, None).await;
    d.pull_board(b.id, linear).await.unwrap();
    d.recheck_me(b.id, linear, "Atlas").await.unwrap();
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
        // Read before and after the write; who the person is asked when the
        // board checked, and again by Take it.
        assert_eq!(linear.calls_of("linear.issue_get").len(), 2);
        assert_eq!(linear.calls_of("linear.viewer").len(), 2);
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
        // The hub doesn't offer linear.viewer: no one is known, so check
        // who you are first.
        refused(
            d.take_ticket(t, &linear, "Atlas", &you).await,
            "Use Check who you are on the board, then take it.",
        );
        // An agent's shared key answers as its owner: checking says so, and
        // Take it still won't.
        *linear.viewer.lock().unwrap() = Some(viewer(false));
        d.recheck_me(b.id, &linear, "Atlas").await.unwrap();
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        assert_eq!(s.me, None);
        assert!(s.me_note.unwrap().contains("shared Linear key"));
        refused(
            d.take_ticket(t, &linear, "Atlas", &you).await,
            "Use Check who you are on the board, then take it.",
        );
        // Vouched, then the key becomes a shared one: nothing is assigned,
        // and the board's answer isn't changed by Take it.
        d.change_sync(b.id, |s| {
            s.me = Some(ME.into());
            s.me_source = Some(crate::board::MeSource::Vouched);
        })
        .await
        .unwrap();
        refused(
            d.take_ticket(t, &linear, "Atlas", &you).await,
            "Atlas uses a shared Linear key, so it can't say who you are.",
        );
        // An older hub that doesn't say whose key it used.
        *linear.viewer.lock().unwrap() = Some(json!({ "id": ME, "name": "sam" }));
        refused(
            d.take_ticket(t, &linear, "Atlas", &you).await,
            "can't say who you are",
        );
        assert_eq!(
            d.board_sync(b.id).await.unwrap().unwrap().1.me_source,
            Some(crate::board::MeSource::Vouched)
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
            // p1 names itself; p2 and p3 name each other.
            let next = match args["after"].as_str() {
                None => "p1",
                Some("p1") => "p1",
                Some("p2") => "p3",
                Some(_) => "p2",
            };
            Ok(json!({ "issues": [], "page_info": { "has_next_page": true, "end_cursor": next } }))
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
                seen: vec![],
            });
            s.backoff_until = None;
        })
        .await
        .unwrap();
        d.pull_board(b.id, &Loop).await.unwrap();
        assert_eq!(d.board_sync(b.id).await.unwrap().unwrap().1.listing, None);
        // Two pages that name each other, one pass apart: the second pass
        // knows the first's pages.
        d.change_sync(b.id, |s| {
            s.listing = Some(crate::board::Listing {
                after: "p2".into(),
                since: None,
                top: None,
                started: None,
                seen: vec!["p3".into()],
            });
            s.backoff_until = None;
        })
        .await
        .unwrap();
        let pulled = d.pull_board(b.id, &Loop).await.unwrap();
        assert!(
            pulled
                .skipped
                .as_deref()
                .unwrap()
                .contains("same page twice"),
            "{pulled:?}"
        );
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

/// A link the person said isn't them: Take it refuses without asking the
/// hub, and leaves their choice and its note as they were.
#[tokio::test]
async fn take_it_leaves_a_not_me_link_as_it_was() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let linear = FakeLinear::default();
        *linear.viewer.lock().unwrap() = Some(viewer(true));
        linear.set(1, "unstarted", 1, None);
        let b = whole_team(&d, None).await;
        d.pull_board(b.id, &linear).await.unwrap();
        let note = "You said Atlas's answer of who you are on Linear isn't you: check again once it's right.";
        d.change_sync(b.id, |s| s.me_note = Some(note.into()))
            .await
            .unwrap();
        let t = ticket_of(&d, &b, "ENG-1").await.ticket.id;
        let r = d
            .take_ticket(t, &linear, "Atlas", &crate::crew::By::you())
            .await;
        refused(
            r,
            "Desk doesn't know who you are on Linear yet. Use Check who you are on the board, then take it.",
        );
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        assert_eq!((s.me, s.me_source, s.me_note.as_deref()), (None, None, Some(note)));
        assert!(linear.calls_of("linear.viewer").is_empty());
        assert!(linear.calls_of("linear.issue_update").is_empty());
    })
    .await;
}

// ── A shared key, and the person's verified email ────────────────────────

/// A hub whose Linear key is shared names the person by the one member with
/// the email their sign-in provider verified: Take it assigns that member,
/// with the shared key. No member, several, or no verified email: no one.
#[tokio::test]
async fn a_shared_key_names_the_one_member_with_the_verified_email() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let linear = FakeLinear::default();
        *linear.viewer.lock().unwrap() = Some(viewer(false));
        *linear.verified_email.lock().unwrap() = Some("sam@example.com".into());
        linear.members.lock().unwrap().extend([
            (
                ME.to_string(),
                "Sam".to_string(),
                "Sam@Example.com".to_string(),
            ),
            (
                ANA.to_string(),
                "Ana".to_string(),
                "ana@example.com".to_string(),
            ),
        ]);
        linear.set(1, "unstarted", 1, None);
        let b = taker(&d, &linear).await;
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        assert_eq!(
            (s.me.as_deref(), s.me_source, s.me_note),
            (Some(ME), Some(crate::board::MeSource::VouchedByEmail), None)
        );
        // Asked of the hub with nothing: it knows whose email to match.
        assert_eq!(
            linear.calls_of("identity.linear_member_by_my_email"),
            vec![json!({})]
        );
        let t = ticket_of(&d, &b, "ENG-1").await.ticket.id;
        d.take_ticket(t, &linear, "Atlas", &crate::crew::By::you())
            .await
            .unwrap();
        assert_eq!(
            linear.calls_of("linear.issue_update"),
            vec![json!({ "identifier": "ENG-1", "assignee_id": ME })]
        );
    })
    .await;
}

/// Whom the person chose to be on a shared key is who the board goes by:
/// Take it assigns them, "assigned to me" asks for them; a choice cleared
/// falls back to the verified email.
#[tokio::test]
async fn a_chosen_member_is_who_the_board_goes_by() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let linear = FakeLinear::default();
        *linear.viewer.lock().unwrap() = Some(viewer(false));
        // Their Slack email isn't their Linear one: no match, so they choose.
        *linear.verified_email.lock().unwrap() = Some("sam@slack.example".into());
        linear.members.lock().unwrap().extend([
            (
                ME.to_string(),
                "Sam".to_string(),
                "sam@linear.example".to_string(),
            ),
            (
                ANA.to_string(),
                "Ana".to_string(),
                "ana@example.com".to_string(),
            ),
        ]);
        *linear.chosen.lock().unwrap() = Some((ME.to_string(), "Sam".to_string()));
        linear.set(1, "unstarted", 1, None);
        let b = taker(&d, &linear).await;
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        assert_eq!(
            (s.me.as_deref(), s.me_source),
            (Some(ME), Some(crate::board::MeSource::Chosen))
        );
        assert_eq!(
            crate::board_sync::linear_args(
                "team",
                &crate::board::Filter::AssignedToMe,
                s.me.as_deref(),
                None,
                None
            )["assignee_id"],
            ME
        );
        let t = ticket_of(&d, &b, "ENG-1").await.ticket.id;
        d.take_ticket(t, &linear, "Atlas", &crate::crew::By::you())
            .await
            .unwrap();
        assert_eq!(
            linear.calls_of("linear.issue_update"),
            vec![json!({ "identifier": "ENG-1", "assignee_id": ME })]
        );
        // Changed to Ana, then cleared: back to the email, which names no one.
        *linear.chosen.lock().unwrap() = Some((ANA.to_string(), "Ana".to_string()));
        d.recheck_me(b.id, &linear, "Atlas").await.unwrap();
        assert_eq!(
            d.board_sync(b.id).await.unwrap().unwrap().1.me.as_deref(),
            Some(ANA)
        );
        *linear.chosen.lock().unwrap() = None;
        d.recheck_me(b.id, &linear, "Atlas").await.unwrap();
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        assert_eq!((s.me, s.me_source), (None, None));
    })
    .await;
}

#[tokio::test]
async fn no_member_several_or_an_unverified_email_names_no_one() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let linear = FakeLinear::default();
        *linear.viewer.lock().unwrap() = Some(viewer(false));
        linear.set(1, "unstarted", 1, None);
        let b = taker(&d, &linear).await;
        let shared = "Atlas uses a shared Linear key, so it can't say who you are.";
        // No verified email.
        d.recheck_me(b.id, &linear, "Atlas").await.unwrap();
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        assert_eq!((s.me, s.me_note.as_deref()), (None, Some(shared)));
        // A verified email no member has.
        *linear.verified_email.lock().unwrap() = Some("sam@example.com".into());
        d.recheck_me(b.id, &linear, "Atlas").await.unwrap();
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        assert_eq!((s.me, s.me_note.as_deref()), (None, Some(shared)));
        // Two members with it: neither.
        linear.members.lock().unwrap().extend([
            (
                ME.to_string(),
                "Sam".to_string(),
                "sam@example.com".to_string(),
            ),
            (
                ANA.to_string(),
                "Sam A".to_string(),
                "SAM@example.com".to_string(),
            ),
        ]);
        d.recheck_me(b.id, &linear, "Atlas").await.unwrap();
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        assert_eq!((s.me, s.me_note.as_deref()), (None, Some(shared)));
        let t = ticket_of(&d, &b, "ENG-1").await.ticket.id;
        refused(
            d.take_ticket(t, &linear, "Atlas", &crate::crew::By::you())
                .await,
            "Use Check who you are on the board",
        );
        assert!(linear.calls_of("linear.issue_update").is_empty());
    })
    .await;
}

/// A board with tickets of its own links when asked to keep them: they stay
/// on Desk only, with their own keys, never made issues even once edited,
/// and the issues come in beside them.
#[tokio::test]
async fn a_board_with_tickets_links_and_keeps_them_beside_the_issues() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = d
            .create_board(NewBoard {
                name: "Acme".into(),
                group: None,
                prefix: Some("ACM".into()),
            })
            .await
            .unwrap();
        let you = crate::crew::By::you();
        let own = d
            .create_ticket(
                crate::service::NewTicket {
                    board: b.id,
                    title: "Our own".into(),
                    body: String::new(),
                    state: None,
                    needs: vec![],
                },
                &you,
            )
            .await
            .unwrap();
        // Refused unless asked to keep them.
        assert!(matches!(
            d.link_board(b.id, link(None), "sam").await,
            Err(DeskError::Conflict(_))
        ));
        let linked = d
            .link_board(
                b.id,
                LinkBoard {
                    keep_tickets: true,
                    ..link(None)
                },
                "sam",
            )
            .await
            .unwrap();
        assert_eq!(linked.prefix.as_deref(), Some("ACM"));
        assert!(linked.sync.unwrap().local_before.is_some());
        let remote = jira(2, &[]);
        d.pull_board(b.id, &remote).await.unwrap();
        let keys: HashSet<String> = d
            .tickets(b.id, &Default::default())
            .await
            .unwrap()
            .into_iter()
            .filter_map(|t| t.key)
            .collect();
        assert_eq!(
            keys,
            HashSet::from(["ACM-1".into(), "ACME-1".into(), "ACME-2".into()])
        );
        // Edited after: still Desk's only.
        d.edit_ticket(
            own.ticket.id,
            crate::service::TicketPatch {
                title: Some("Our own, edited".into()),
                ..Default::default()
            },
            &you,
        )
        .await
        .unwrap();
        // Never even queued to go out.
        assert!(d.nothing_to_push(b.id).await.unwrap());
        d.push_board(b.id, &remote).await.unwrap();
        let after = d.ticket(own.ticket.id).await.unwrap();
        assert!(after.ticket.external.is_none());
        assert_eq!(after.key.as_deref(), Some("ACM-1"));
    })
    .await;
}

/// The member a shared key's verified email named when the board was linked
/// isn't who it names now: Take it assigns no one, and says so.
#[tokio::test]
async fn take_it_refuses_when_the_matched_member_changes() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let linear = FakeLinear::default();
        *linear.viewer.lock().unwrap() = Some(viewer(false));
        *linear.verified_email.lock().unwrap() = Some("sam@example.com".into());
        linear
            .members
            .lock()
            .unwrap()
            .push((ME.into(), "Sam".into(), "sam@example.com".into()));
        linear.set(1, "unstarted", 1, None);
        let b = taker(&d, &linear).await;
        // The email now belongs to someone else there.
        {
            let mut m = linear.members.lock().unwrap();
            m.clear();
            m.push((ANA.into(), "Ana".into(), "sam@example.com".into()));
        }
        let t = ticket_of(&d, &b, "ENG-1").await.ticket.id;
        refused(
            d.take_ticket(t, &linear, "Atlas", &crate::crew::By::you())
                .await,
            "now says you are Ana",
        );
        assert!(linear.calls_of("linear.issue_update").is_empty());
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        assert_eq!(
            s.me.as_deref(),
            Some(ME),
            "Take it changes nothing about who the board knows"
        );
    })
    .await;
}

/// What was read of a board's issues' files as one person, through one hub,
/// goes when the link reads as another or through another hub, and with
/// the link: an agent changing alone keeps them.
#[tokio::test]
async fn a_board_s_remote_files_go_when_its_link_reads_as_someone_else() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        d.pull_board(b.id, &jira(1, &[])).await.unwrap();
        let t = d.ticket_by_key("ACME-1").await.unwrap().ticket.id;
        let file = crate::ticket_files::RemoteFile {
            source: crate::ticket_files::FileSource::Jira,
            remote_id: "10001".into(),
            name: "a.png".into(),
            media_type: "image/png".into(),
            size: Some(3),
            url: None,
            thumb: false,
        };
        let party = crate::board_sync::remote_party(System::Jira, "ACME-1");
        let put = || async {
            d.0.store
                .put_remote_files(t, std::slice::from_ref(&file), &party)
                .await
                .unwrap();
            let id = d.0.store.files_of(&[t]).await.unwrap()[&t][0].id;
            d.0.store
                .keep_file_bytes(id, false, &[1, 2, 3])
                .await
                .unwrap();
            id
        };
        let store = &d.0.store;
        let held = |id: Uuid| async move { store.file_bytes(id, false).await.unwrap().is_some() };
        let id = put().await;
        d.change_sync(b.id, |s| s.agent = Some("scout".into()))
            .await
            .unwrap();
        assert!(
            held(id).await,
            "another agent on the same hub, as the same person"
        );
        d.change_sync(b.id, |s| s.person = "alex".into())
            .await
            .unwrap();
        assert!(
            !d.0.store.files_of(&[t]).await.unwrap().contains_key(&t),
            "read as someone else: forgotten"
        );
        let id = put().await;
        d.change_sync(b.id, |s| s.via = Via::Hub { hub: "acme".into() })
            .await
            .unwrap();
        assert!(!held(id).await, "through another hub: forgotten");
        let id = put().await;
        d.unlink_board(b.id).await.unwrap();
        assert!(!held(id).await, "unlinked: forgotten");
    })
    .await;
}

/// A Jira whose search carries what an issue says of itself: ACME-1 is a
/// story in the epic ACME-9, blocked by ACME-2 while `blocked` holds.
struct RichJira {
    blocked: Mutex<bool>,
    /// `jira.list_fields` fails while set.
    fields_fail: Mutex<bool>,
    updated: Mutex<String>,
    calls: Mutex<Vec<(String, Value)>>,
}

#[async_trait]
impl Remote for RichJira {
    async fn tools(&self) -> Result<Vec<String>, RemoteError> {
        Ok(crate::board_sync::callable(System::Jira)
            .iter()
            .map(|t| t.to_string())
            .collect())
    }

    async fn call(&self, tool: &str, args: Value) -> Result<Value, RemoteError> {
        self.calls
            .lock()
            .unwrap()
            .push((tool.to_string(), args.clone()));
        let updated = self.updated.lock().unwrap().clone();
        let blocked = *self.blocked.lock().unwrap();
        match tool {
            "jira.list_fields" if *self.fields_fail.lock().unwrap() => {
                Err(RemoteError::Failed("503".into()))
            }
            "jira.list_fields" => Ok(json!({
                "fields": { "points": "customfield_10016", "sprint": "customfield_10020",
                    "start_date": null, "development": "customfield_10000" },
                "priorities": [{ "id": "1", "name": "Now" }, { "id": "2", "name": "Soon" },
                    { "id": "3", "name": "Next" }, { "id": "4", "name": "Later" }],
            })),
            "jira.search" => {
                let links = match blocked {
                    true => json!([{ "key": "ACME-2", "summary": "Shared role model",
                        "issuetype": "Story", "status": "In Progress",
                        "status_category": "indeterminate", "direction": "inward",
                        "link_type": "Blocks", "relation": "is blocked by" }]),
                    false => json!([]),
                };
                Ok(json!({ "is_last": true, "issues": [
                    { "key": "ACME-1", "summary": "Map hub roles", "status": "To Do",
                      "status_category": "new", "updated": updated, "labels": ["web"],
                      "description_html": "<p>Roles.</p>", "issuetype": "Story",
                      "issuetype_level": 0, "priority": "Next",
                      "parent": { "key": "ACME-9", "summary": "Permissions", "issuetype": "Epic",
                        "hierarchy_level": 1, "status": "In Progress",
                        "status_category": "indeterminate" },
                      "subtasks": [{ "key": "ACME-3", "status_category": "done" },
                        { "key": "ACME-4", "status_category": "new" }],
                      "issuelinks": links, "components": ["API"], "fix_versions": [],
                      "comment_total": 4, "created": "2026-11-01T09:00:00.000+0000",
                      "reporter": "Tom Reed", "points": 5.0,
                      "sprint": [{ "name": "Sprint 41", "state": "closed",
                          "start": "2026-10-20T09:00:00.000Z", "end": "2026-11-02T17:00:00.000Z" },
                        { "name": "Sprint 42", "state": "active",
                          "start": "2026-11-03T09:00:00.000Z", "end": "2026-11-16T17:00:00.000Z" }],
                      "development": { "count": 2, "state": "MERGED", "open": false } },
                    { "key": "ACME-2", "summary": "Shared role model", "status": "To Do",
                      "updated": "2026-11-09T09:00:00.000+0000", "labels": ["web"],
                      "description_html": "<p>One model.</p>", "issuelinks": [] },
                ]}))
            }
            other => Err(RemoteError::NotAllowed(other.into())),
        }
    }
}

/// What an issue says of itself comes in with the listing, and what blocks
/// it there becomes its need, seen as sent, and goes when it no longer does.
#[tokio::test]
async fn an_issue_s_facts_and_blockers_come_in_with_the_pull() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        let remote = RichJira {
            blocked: Mutex::new(true),
            fields_fail: Mutex::new(false),
            updated: Mutex::new("2026-11-09T10:00:00.000+0000".into()),
            calls: Mutex::new(vec![]),
        };
        d.pull_board(b.id, &remote).await.unwrap();
        // The site's fields were read once, and asked for by role.
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        let fields = s.fields.clone().unwrap();
        assert_eq!(fields.points.as_deref(), Some("customfield_10016"));
        assert_eq!(fields.priorities.len(), 4);
        let calls = remote.calls.lock().unwrap().clone();
        let search = calls.iter().find(|(t, _)| t == "jira.search").unwrap();
        assert_eq!(search.1["custom_fields"]["sprint"], "customfield_10020");
        assert!(search.1["fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "issuelinks"));
        let tickets = d.tickets(b.id, &Default::default()).await.unwrap();
        let of = |key: &str| {
            tickets
                .iter()
                .find(|t| t.key.as_deref() == Some(key))
                .unwrap()
                .clone()
        };
        let one = of("ACME-1");
        let two = of("ACME-2");
        let e = one.ticket.external.clone().unwrap();
        let f = &e.details.facts;
        assert_eq!(f.kind.as_deref(), Some("Story"));
        assert_eq!(f.epic.as_ref().map(|r| r.key.as_str()), Some("ACME-9"));
        assert_eq!(f.points, Some(5.0));
        assert_eq!(
            f.sprint.as_ref().and_then(|s| s.name.as_deref()),
            Some("Sprint 42")
        );
        assert_eq!(
            f.sub_items,
            Some(crate::board::Progress {
                done: 1,
                total: 2,
                started: 0
            })
        );
        assert_eq!(f.comments, Some(4));
        assert_eq!(f.priority_rank, Some(3), "Next is third of four");
        assert_eq!(
            f.prs.as_ref().map(|p| (p.count, p.state.as_str())),
            Some((2, "merged"))
        );
        assert_eq!(f.reporter.as_deref(), Some("Tom Reed"));
        // ACME-2 blocks it there: a need, seen as sent.
        assert_eq!(one.ticket.needs, vec![two.ticket.id]);
        assert_eq!(e.seen.needs, vec!["ACME-2".to_string()]);
        assert!(crate::board_sync::push_of(&one.ticket, &["ACME-2".into()]).is_empty());
        // Taken off here by hand: it stays off, though Jira still says so.
        d.need_ticket(one.ticket.id, two.ticket.id, false, &By::you())
            .await
            .unwrap();
        *remote.updated.lock().unwrap() = "2026-11-09T10:30:00.000+0000".into();
        d.pull_board(b.id, &remote).await.unwrap();
        let again = d.ticket(one.ticket.id).await.unwrap();
        assert!(again.ticket.needs.is_empty(), "{:?}", again.ticket.needs);
        // Nor after a change of its own goes out.
        let mut t = again.ticket.clone();
        let e = t.external.as_mut().unwrap();
        let sent = crate::board::Seen {
            title: "Renamed here".into(),
            needs: vec![],
            ..e.seen.clone()
        };
        crate::board_sync::pushed(e, &sent, None, false);
        assert_eq!(e.seen.needs, vec!["ACME-2".to_string()]);
        // Unblocked there: the need a pull made goes.
        *remote.blocked.lock().unwrap() = false;
        *remote.updated.lock().unwrap() = "2026-11-09T11:00:00.000+0000".into();
        d.pull_board(b.id, &remote).await.unwrap();
        let one = d.ticket(one.ticket.id).await.unwrap();
        assert!(one.ticket.needs.is_empty(), "{:?}", one.ticket.needs);
        assert!(one.ticket.external.unwrap().seen.needs.is_empty());
        // The fields aren't read again within the day.
        let reads = remote
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t == "jira.list_fields")
            .count();
        assert_eq!(reads, 1);
        // A day on, a failed read keeps what was read before.
        d.change_sync(b.id, |s| {
            if let Some(f) = s.fields.as_mut() {
                f.read_at -= chrono::Duration::days(2);
            }
        })
        .await
        .unwrap();
        *remote.fields_fail.lock().unwrap() = true;
        d.pull_board(b.id, &remote).await.unwrap();
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        let kept = s.fields.unwrap();
        assert_eq!(kept.points.as_deref(), Some("customfield_10016"));
        assert_eq!(kept.priorities.len(), 4);
        assert!(chrono::Utc::now() - kept.read_at < chrono::Duration::hours(1));
    })
    .await;
}

#[tokio::test]
async fn a_pulled_blocker_never_makes_a_loop_of_needs() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        let remote = RichJira {
            blocked: Mutex::new(true),
            fields_fail: Mutex::new(false),
            updated: Mutex::new("2026-11-09T10:00:00.000+0000".into()),
            calls: Mutex::new(vec![]),
        };
        d.pull_board(b.id, &remote).await.unwrap();
        let tickets = d.tickets(b.id, &Default::default()).await.unwrap();
        let id = |key: &str| {
            tickets
                .iter()
                .find(|t| t.key.as_deref() == Some(key))
                .unwrap()
                .ticket
                .id
        };
        let (one, two) = (id("ACME-1"), id("ACME-2"));
        // Pulled, ACME-1 needs ACME-2. Unblocked there, the need goes; then
        // ACME-2 is made to need ACME-1 here, and Jira blocks ACME-1 on
        // ACME-2 again: the pull leaves it rather than make a loop.
        assert_eq!(d.ticket(one).await.unwrap().ticket.needs, vec![two]);
        *remote.blocked.lock().unwrap() = false;
        *remote.updated.lock().unwrap() = "2026-11-09T11:00:00.000+0000".into();
        d.pull_board(b.id, &remote).await.unwrap();
        assert!(d.ticket(one).await.unwrap().ticket.needs.is_empty());
        d.need_ticket(two, one, true, &By::you()).await.unwrap();
        *remote.blocked.lock().unwrap() = true;
        *remote.updated.lock().unwrap() = "2026-11-09T12:00:00.000+0000".into();
        d.pull_board(b.id, &remote).await.unwrap();
        assert!(d.ticket(one).await.unwrap().ticket.needs.is_empty());
        assert_eq!(d.ticket(two).await.unwrap().ticket.needs, vec![one]);
    })
    .await;
}

/// A Jira board with the epic ACME-9 and its story ACME-1, which has the
/// sub-task ACME-3. ACME-9's children are ACME-1 (under way), ACME-50
/// (blocked by ACME-60, not done) and ACME-51 (done): the last two are
/// outside the board's listing.
struct EpicJira {
    calls: Mutex<Vec<(String, Value)>>,
    /// The epic's children it says, of ACME-1, ACME-50 and ACME-51.
    kids: Mutex<Vec<&'static str>>,
    /// Its children's search says there is another page.
    more: Mutex<bool>,
}

const EPIC_UPDATED: &str = "2026-11-09T10:00:00.000+0000";

impl EpicJira {
    fn new() -> Self {
        EpicJira {
            calls: Mutex::new(vec![]),
            kids: Mutex::new(vec!["ACME-1", "ACME-50", "ACME-51"]),
            more: Mutex::new(false),
        }
    }

    fn searches(&self, words: &str) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, a)| t == "jira.search" && a["jql"].as_str().unwrap().contains(words))
            .map(|(_, a)| a.clone())
            .collect()
    }

    fn story() -> Value {
        json!({ "key": "ACME-1", "summary": "Map hub roles", "status": "In Progress",
            "status_category": "indeterminate", "updated": EPIC_UPDATED, "labels": [],
            "description_html": "<p>Roles.</p>", "issuetype": "Story", "issuetype_level": 0,
            "parent": { "key": "ACME-9", "summary": "Permissions", "issuetype": "Epic",
                "hierarchy_level": 1, "status": "In Progress", "status_category": "indeterminate" },
            "subtasks": [{ "key": "ACME-3", "summary": "Write the migration", "issuetype": "Sub-task",
                "hierarchy_level": -1, "status": "In Review", "status_category": "indeterminate" }],
            "issuelinks": [] })
    }
}

#[async_trait]
impl Remote for EpicJira {
    async fn tools(&self) -> Result<Vec<String>, RemoteError> {
        Ok(crate::board_sync::callable(System::Jira)
            .iter()
            .map(|t| t.to_string())
            .collect())
    }

    async fn call(&self, tool: &str, args: Value) -> Result<Value, RemoteError> {
        self.calls
            .lock()
            .unwrap()
            .push((tool.to_string(), args.clone()));
        let jql = args["jql"].as_str().unwrap_or_default().to_string();
        let child = |key: &str, title: &str, status: &str, category: &str, points: f64| {
            json!({ "key": key, "summary": title, "status": status,
                "status_category": category, "issuetype": "Task", "issuetype_level": 0,
                "updated": EPIC_UPDATED, "points": points, "issuelinks": [],
                "parent": { "key": "ACME-9", "summary": "Permissions", "issuetype": "Epic",
                    "hierarchy_level": 1 } })
        };
        match tool {
            "jira.list_fields" => Ok(json!({
                "fields": { "points": "customfield_10016" },
                "priorities": [{ "name": "Now" }, { "name": "Soon" }, { "name": "Next" },
                    { "name": "Later" }],
            })),
            "jira.search" if jql.starts_with("parent in (ACME-9)") => {
                let mut one = child(
                    "ACME-1",
                    "Map hub roles",
                    "In Progress",
                    "indeterminate",
                    3.0,
                );
                one["assignee"] = json!("Ana Diaz");
                one["priority"] = json!("Now");
                one["issuetype"] = json!("Story");
                let mut blocked = child("ACME-50", "Audit old roles", "To Do", "new", 5.0);
                blocked["issuelinks"] = json!([{ "key": "ACME-60", "summary": "Export roles",
                    "status": "To Do", "status_category": "new", "direction": "inward",
                    "link_type": "Blocks", "relation": "is blocked by" }]);
                let done = child("ACME-51", "Drop the old table", "Done", "done", 2.0);
                let kids = self.kids.lock().unwrap().clone();
                let issues: Vec<Value> = [done, blocked, one]
                    .into_iter()
                    .filter(|c| kids.contains(&c["key"].as_str().unwrap()))
                    .collect();
                if *self.more.lock().unwrap() {
                    return Ok(
                        json!({ "is_last": false, "next_page_token": "p2", "issues": issues }),
                    );
                }
                Ok(json!({ "is_last": true, "issues": issues }))
            }
            "jira.search" if jql.starts_with("parent in (ACME-1)") => Ok(json!({
                "is_last": true,
                "issues": [{ "key": "ACME-3", "summary": "Write the migration",
                    "status": "In Review", "status_category": "indeterminate",
                    "issuetype": "Sub-task", "issuetype_level": -1, "assignee": "Lee Park",
                    "priority": "Soon", "points": 1.0, "updated": EPIC_UPDATED,
                    "parent": { "key": "ACME-1" } }],
            })),
            "jira.search" => Ok(json!({ "is_last": true, "issues": [
                { "key": "ACME-9", "summary": "Permissions", "status": "In Progress",
                  "status_category": "indeterminate", "updated": EPIC_UPDATED, "labels": [],
                  "description_html": "<p>All of it.</p>", "issuetype": "Epic",
                  "issuetype_level": 1, "subtasks": [], "issuelinks": [] },
                EpicJira::story(),
            ]})),
            "jira.get_issue" if args["issue_key"] == "ACME-1" => Ok(EpicJira::story()),
            other => Err(RemoteError::NotAllowed(other.into())),
        }
    }
}

/// An epic's children come in with one more search a pass, whatever the
/// board's filter: listed on its ticket with what they add up to, never
/// imported; and a pass that finds them unchanged says nothing.
#[tokio::test]
async fn an_epic_s_children_come_in_with_one_search_and_are_never_imported() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        let remote = EpicJira::new();
        d.pull_board(b.id, &remote).await.unwrap();
        let searches = remote.searches("parent in");
        assert_eq!(searches.len(), 1, "one search over the board's epics");
        assert_eq!(searches[0]["jql"], "parent in (ACME-9) ORDER BY key ASC");
        let fields = searches[0]["fields"].as_array().unwrap();
        assert!(fields.iter().any(|f| f == "parent"));
        assert!(
            !fields.iter().any(|f| f == "description" || f == "comment"),
            "no bodies: {fields:?}"
        );
        let tickets = d.tickets(b.id, &Default::default()).await.unwrap();
        let mut keys: Vec<_> = tickets.iter().filter_map(|t| t.key.clone()).collect();
        keys.sort();
        assert_eq!(keys, ["ACME-1", "ACME-9"], "its children aren't imported");
        let epic = tickets
            .iter()
            .find(|t| t.key.as_deref() == Some("ACME-9"))
            .unwrap();
        let f = &epic.ticket.external.as_ref().unwrap().details.facts;
        let children: Vec<&str> = f.children.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(children, ["ACME-1", "ACME-50", "ACME-51"], "in key order");
        let one = &f.children[0];
        assert_eq!(one.assignee.as_deref(), Some("Ana Diaz"));
        assert_eq!(one.priority_rank, Some(1));
        assert_eq!(one.state_kind.as_deref(), Some("indeterminate"));
        assert!(one.started && !one.done);
        assert_eq!(
            f.sub_items,
            Some(crate::board::Progress {
                done: 1,
                total: 3,
                started: 1
            })
        );
        assert_eq!(
            f.child_points,
            Some(crate::board::Progress {
                done: 2,
                total: 10,
                started: 3
            })
        );
        assert_eq!(f.child_blocked, Some(1), "ACME-50 waits on ACME-60");
        // A pass that isn't a sweep leaves them be.
        d.pull_board(b.id, &remote).await.unwrap();
        assert_eq!(remote.searches("parent in").len(), 1, "only on a sweep");
        // Unchanged next sweep: the epic keeps its children, and isn't
        // announced again.
        d.change_sync(b.id, |s| s.swept_at = None).await.unwrap();
        let (_, mut events) = d.subscribe();
        d.pull_board(b.id, &remote).await.unwrap();
        assert_eq!(remote.searches("parent in").len(), 2);
        while let Ok(e) = events.try_recv() {
            if let DeskEvent::Ticket { ticket } = e {
                assert_ne!(ticket.ticket.id, epic.ticket.id, "nothing changed");
            }
        }
        let again = d.ticket(epic.ticket.id).await.unwrap();
        assert_eq!(&again.ticket.external.unwrap().details.facts, f);
    })
    .await;
}

/// A pass whose calls are spent skips the epics' children: nothing is
/// asked, and what the epic held stays.
#[tokio::test]
async fn a_pass_out_of_calls_skips_the_epics_children() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        let remote = EpicJira::new();
        d.pull_board(b.id, &remote).await.unwrap();
        let (b, s) = d.board_sync(b.id).await.unwrap().unwrap();
        let held = d.ticket_by_key("ACME-9").await.unwrap().ticket;
        let asked = remote.searches("parent in").len();
        d.pull_children(&b, &s, &remote, &mut Budget(0))
            .await
            .unwrap();
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        assert_eq!(s.children_next, 0, "the same search goes first next time");
        assert_eq!(remote.searches("parent in").len(), asked);
        let now = d.ticket_by_key("ACME-9").await.unwrap().ticket;
        assert_eq!(now.external, held.external);
    })
    .await;
}

/// Opening a story reads who has each of its sub-tasks, which its own
/// answer never says, and a pass after keeps them.
#[tokio::test]
async fn opening_a_story_fills_in_who_has_its_sub_tasks_and_a_pass_keeps_them() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        let remote = EpicJira::new();
        d.pull_board(b.id, &remote).await.unwrap();
        let story = d.ticket_by_key("ACME-1").await.unwrap().ticket;
        let sub = |t: &crate::board::Ticket| {
            t.external.as_ref().unwrap().details.facts.children[0].clone()
        };
        let listed = sub(&story);
        assert_eq!(listed.key, "ACME-3");
        assert_eq!(listed.title, "Write the migration");
        assert_eq!(listed.state.as_deref(), Some("In Review"));
        assert_eq!(listed.kind_level, Some(-1));
        assert_eq!(listed.assignee, None, "a search row doesn't say");
        d.read_ticket_issue(story.id, &remote, true).await.unwrap();
        assert_eq!(remote.searches("parent in (ACME-1)").len(), 1);
        let opened = sub(&d.ticket(story.id).await.unwrap().ticket);
        assert_eq!(opened.assignee.as_deref(), Some("Lee Park"));
        assert_eq!(opened.priority_rank, Some(2));
        assert_eq!(opened.points, Some(1.0));
        d.pull_board(b.id, &remote).await.unwrap();
        let kept = sub(&d.ticket(story.id).await.unwrap().ticket);
        assert_eq!(kept, opened);
    })
    .await;
}

/// The ticket views of `ticket` announced on `events` so far.
fn heard(events: &mut tokio::sync::broadcast::Receiver<DeskEvent>, ticket: uuid::Uuid) -> usize {
    let mut n = 0;
    while let Ok(e) = events.try_recv() {
        if matches!(e, DeskEvent::Ticket { ticket: v } if v.ticket.id == ticket) {
            n += 1;
        }
    }
    n
}

/// A children's search cut short (another page, no calls left) changes
/// nothing, and says nothing.
#[tokio::test]
async fn a_children_s_search_cut_short_changes_nothing() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        let remote = EpicJira::new();
        d.pull_board(b.id, &remote).await.unwrap();
        let held = d.ticket_by_key("ACME-9").await.unwrap().ticket;
        *remote.kids.lock().unwrap() = vec!["ACME-1"];
        *remote.more.lock().unwrap() = true;
        let (b, s) = d.board_sync(b.id).await.unwrap().unwrap();
        let (_, mut events) = d.subscribe();
        d.pull_children(&b, &s, &remote, &mut Budget(1))
            .await
            .unwrap();
        let now = d.ticket_by_key("ACME-9").await.unwrap().ticket;
        assert_eq!(now.external, held.external);
        assert_eq!(heard(&mut events, held.id), 0);
    })
    .await;
}

/// A child gone from its epic there goes from its ticket, and what the
/// children add up to is counted again, down to none at all.
#[tokio::test]
async fn a_child_that_left_its_epic_goes_and_the_counts_follow() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        let remote = EpicJira::new();
        d.pull_board(b.id, &remote).await.unwrap();
        let facts = || async {
            d.ticket_by_key("ACME-9")
                .await
                .unwrap()
                .ticket
                .external
                .unwrap()
                .details
                .facts
        };
        assert_eq!(facts().await.children.len(), 3);
        // ACME-50, the blocked one, left.
        *remote.kids.lock().unwrap() = vec!["ACME-1", "ACME-51"];
        d.change_sync(b.id, |s| s.swept_at = None).await.unwrap();
        d.pull_board(b.id, &remote).await.unwrap();
        let f = facts().await;
        let keys: Vec<&str> = f.children.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, ["ACME-1", "ACME-51"]);
        assert_eq!(
            f.sub_items,
            Some(crate::board::Progress {
                done: 1,
                total: 2,
                started: 1
            })
        );
        assert_eq!(
            f.child_points,
            Some(crate::board::Progress {
                done: 2,
                total: 5,
                started: 3
            })
        );
        assert_eq!(f.child_blocked, None);
        // None left at all.
        *remote.kids.lock().unwrap() = vec![];
        d.change_sync(b.id, |s| s.swept_at = None).await.unwrap();
        d.pull_board(b.id, &remote).await.unwrap();
        let f = facts().await;
        assert!(f.children.is_empty());
        assert_eq!(
            (f.sub_items, f.child_points, f.child_blocked),
            (None, None, None)
        );
    })
    .await;
}

/// A Jira board of eleven epics, ACME-101 to ACME-111, each with one child
/// (ACME-201 to ACME-211) once `answer` holds; a search naming ACME-101
/// fails while `refuse` holds.
struct ManyEpics {
    answer: Mutex<bool>,
    refuse: Mutex<bool>,
    calls: Mutex<usize>,
}

#[async_trait]
impl Remote for ManyEpics {
    async fn tools(&self) -> Result<Vec<String>, RemoteError> {
        Ok(crate::board_sync::callable(System::Jira)
            .iter()
            .map(|t| t.to_string())
            .collect())
    }

    async fn call(&self, tool: &str, args: Value) -> Result<Value, RemoteError> {
        *self.calls.lock().unwrap() += 1;
        let jql = args["jql"].as_str().unwrap_or_default().to_string();
        match tool {
            "jira.list_fields" => Ok(json!({ "fields": {}, "priorities": [] })),
            "jira.search" if jql.starts_with("parent in (") => {
                if *self.refuse.lock().unwrap() && jql.contains("ACME-101") {
                    return Err(RemoteError::Failed(
                        "The issue key 'ACME-101' does not exist".into(),
                    ));
                }
                let answer = *self.answer.lock().unwrap();
                let inner = jql["parent in (".len()..].split(')').next().unwrap();
                let issues: Vec<Value> = inner
                    .split(", ")
                    .filter(|_| answer)
                    .map(|epic| {
                        let n: u32 = epic.trim_start_matches("ACME-").parse().unwrap();
                        json!({ "key": format!("ACME-{}", n + 100), "summary": "A child",
                            "status": "To Do", "status_category": "new",
                            "updated": EPIC_UPDATED, "parent": { "key": epic } })
                    })
                    .collect();
                Ok(json!({ "is_last": true, "issues": issues }))
            }
            "jira.search" => Ok(json!({ "is_last": true, "issues": (101..=111)
                .map(|n| json!({ "key": format!("ACME-{n}"), "summary": format!("Epic {n}"),
                    "status": "To Do", "status_category": "new", "updated": EPIC_UPDATED,
                    "labels": [], "description_html": "", "issuetype": "Epic",
                    "issuetype_level": 1, "issuelinks": [] }))
                .collect::<Vec<_>>() })),
            other => Err(RemoteError::NotAllowed(other.into())),
        }
    }
}

/// Epics are searched ten at a time: a search read to its end is kept at
/// once though the calls run out in the next, which goes first next time;
/// one Jira refuses doesn't stop the others.
#[tokio::test]
async fn each_search_of_epics_read_through_is_kept_though_the_next_runs_out() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        let remote = ManyEpics {
            answer: Mutex::new(false),
            refuse: Mutex::new(false),
            calls: Mutex::new(0),
        };
        d.pull_board(b.id, &remote).await.unwrap();
        let kids = |key: &'static str| {
            let d = &d;
            async move {
                d.ticket_by_key(key)
                    .await
                    .unwrap()
                    .ticket
                    .external
                    .unwrap()
                    .details
                    .facts
                    .children
                    .len()
            }
        };
        assert_eq!(kids("ACME-101").await, 0);
        *remote.answer.lock().unwrap() = true;
        let (b, s) = d.board_sync(b.id).await.unwrap().unwrap();
        let spent = d.pull_children(&b, &s, &remote, &mut Budget(1)).await;
        assert!(spent.is_ok(), "{spent:?}");
        assert_eq!(kids("ACME-101").await, 1);
        assert_eq!(kids("ACME-110").await, 1);
        assert_eq!(kids("ACME-111").await, 0, "its search ran out of calls");
        let (b, s) = d.board_sync(b.id).await.unwrap().unwrap();
        assert_eq!(s.children_next, 1, "it goes first next time");
        // Next time: ACME-111's search first, then round to the others,
        // whose first search Jira now refuses.
        *remote.refuse.lock().unwrap() = true;
        d.pull_children(&b, &s, &remote, &mut Budget(5))
            .await
            .unwrap();
        assert_eq!(kids("ACME-111").await, 1);
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        assert_eq!(s.children_next, 1, "round to where it began");
        // A search that names no epic is never made.
        let calls = *remote.calls.lock().unwrap();
        let none = d
            .read_children(&s, &remote, &mut Budget(5), &mut Budget(5), &["x".into()])
            .await
            .unwrap();
        assert_eq!(none, Some(Default::default()));
        assert_eq!(*remote.calls.lock().unwrap(), calls);
    })
    .await;
}

// ── A description read as ADF ────────────────────────────────────────────

/// A Jira whose one issue's description is both HTML (as a search and a
/// read give it) and ADF (as a read gives it when asked).
struct AdfJira {
    html: Mutex<String>,
    adf: Mutex<Value>,
    updated: Mutex<String>,
}

impl AdfJira {
    fn says(&self, html: &str, words: &str, updated: &str) {
        *self.html.lock().unwrap() = html.into();
        *self.adf.lock().unwrap() = json!({ "type": "doc", "version": 1, "content": [
            { "type": "panel", "attrs": { "panelType": "info" }, "content": [
                { "type": "paragraph", "content": [{ "type": "text", "text": words }] } ] } ] });
        *self.updated.lock().unwrap() = updated.into();
    }
}

#[async_trait]
impl Remote for AdfJira {
    async fn tools(&self) -> Result<Vec<String>, RemoteError> {
        Ok(crate::board_sync::JIRA_TOOLS
            .iter()
            .map(|t| t.to_string())
            .collect())
    }

    async fn call(&self, tool: &str, args: Value) -> Result<Value, RemoteError> {
        let mut v = json!({ "key": "ACME-1", "summary": "Fix the login", "status": "To Do",
            "updated": *self.updated.lock().unwrap(), "labels": ["web"],
            "description_html": *self.html.lock().unwrap() });
        match tool {
            // As the adapter gives it: its description cut at 8 000.
            "jira.search" => {
                let html = self.html.lock().unwrap().clone();
                if html.chars().count() > crate::board_sync::JIRA_SEARCH_HTML_MAX {
                    let cut: String = html
                        .chars()
                        .take(crate::board_sync::JIRA_SEARCH_HTML_MAX)
                        .collect();
                    v["description_html"] = json!(format!("{cut}…"));
                }
                Ok(json!({ "issues": [v], "is_last": true }))
            }
            "jira.get_issue" => {
                if args["description_adf"] == true {
                    v["description_adf"] = json!(self.adf.lock().unwrap().to_string());
                }
                Ok(v)
            }
            other => Err(RemoteError::NotAllowed(other.into())),
        }
    }
}

/// Opened, a ticket's description is read as ADF; a pull that reads the
/// same HTML again leaves it and says nothing; one whose description
/// changed there replaces it with the HTML; opened again, it is ADF again.
#[tokio::test]
async fn a_body_read_as_adf_stays_through_pulls_until_its_description_changes() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        let jira = AdfJira {
            html: Mutex::new(String::new()),
            adf: Mutex::new(Value::Null),
            updated: Mutex::new(String::new()),
        };
        jira.says(
            "<p>It fails.</p>",
            "It fails.",
            "2026-11-09T10:00:00.000+0000",
        );
        d.pull_board(b.id, &jira).await.unwrap();
        let t = d
            .tickets(b.id, &Default::default())
            .await
            .unwrap()
            .remove(0);
        assert_eq!(t.ticket.body, "It fails.");
        let id = t.ticket.id;

        let v = d.read_ticket_issue(id, &jira, true).await.unwrap();
        assert_eq!(v.ticket.body, ":::panel{tone=info}\nIt fails.\n:::");
        let e = v.ticket.external.clone().unwrap();
        assert_eq!(e.html_read, crate::board_sync::ADF_READ);
        assert_eq!(e.seen.body, v.ticket.body, "never sent back unedited");
        let at = v.ticket.updated_at;

        // Changed there, not its words.
        jira.says(
            "<p>It fails.</p>",
            "It fails.",
            "2026-11-09T11:00:00.000+0000",
        );
        d.pull_board(b.id, &jira).await.unwrap();
        let t = d.ticket_of(id).await.unwrap();
        assert_eq!(t.body, ":::panel{tone=info}\nIt fails.\n:::");
        assert_eq!(t.updated_at, at, "no edit is said");
        assert_eq!(
            t.external.as_ref().unwrap().html_read,
            crate::board_sync::ADF_READ
        );

        // Its words changed there: the pull's HTML stands, read as HTML.
        jira.says(
            "<p>It works.</p>",
            "It works.",
            "2026-11-09T12:00:00.000+0000",
        );
        d.pull_board(b.id, &jira).await.unwrap();
        let t = d.ticket_of(id).await.unwrap();
        assert_eq!(t.body, "It works.");
        assert!(t.updated_at > at, "the edit is said");
        let e = t.external.as_ref().unwrap();
        assert_eq!(e.html_read, crate::board_sync::HTML_READ);
        assert!(e.html_prints.is_empty());

        // Opened again: ADF again.
        let v = d.read_ticket_issue(id, &jira, true).await.unwrap();
        assert_eq!(v.ticket.body, ":::panel{tone=info}\nIt works.\n:::");

        // A long one, which a search gives cut: a change there that isn't
        // its words leaves it too.
        let long = "word ".repeat(2_000);
        jira.says(
            &format!("<p>{long}</p>"),
            &long,
            "2026-11-09T13:00:00.000+0000",
        );
        d.pull_board(b.id, &jira).await.unwrap();
        let t = d.ticket_of(id).await.unwrap();
        assert!(t.body.ends_with('…'), "the search's cut body");
        let v = d.read_ticket_issue(id, &jira, true).await.unwrap();
        assert!(v.ticket.body.starts_with(":::panel{tone=info}\nword word"));
        let at = v.ticket.updated_at;
        jira.says(
            &format!("<p>{long}</p>"),
            &long,
            "2026-11-09T14:00:00.000+0000",
        );
        d.pull_board(b.id, &jira).await.unwrap();
        let t = d.ticket_of(id).await.unwrap();
        assert_eq!(t.body, v.ticket.body, "kept");
        assert_eq!(t.updated_at, at, "no edit is said");
        assert_eq!(
            t.external.as_ref().unwrap().html_read,
            crate::board_sync::ADF_READ
        );
        let v = d.read_ticket_issue(id, &jira, true).await.unwrap();
        assert_eq!(v.ticket.body, t.body, "and opened again, the same");
    })
    .await;
}

// ── A synced board's order there ─────────────────────────────────────────

/// A Jira project whose issues carry their Rank, ranked by the rank call
/// (`jira.update_issue`'s `rank_after_issue` / `rank_before_issue`): one
/// ranked moves just after the issue it is ranked after (or before the one
/// it goes before), unless it refuses.
struct RankJira {
    /// (key, Rank), all updated at once.
    issues: Mutex<Vec<(String, String)>>,
    /// What the rank call fails with, when it does.
    refuse: Option<RemoteError>,
    /// The hub holds the rank call for the person's approval.
    hold: Mutex<bool>,
    /// The board's filter's ORDER BY, when `jira.board_order` answers.
    order_by: Mutex<Option<String>>,
    calls: Mutex<Vec<(String, Value)>>,
}

impl RankJira {
    fn new(ranks: &[(&str, &str)]) -> Self {
        Self {
            issues: Mutex::new(
                ranks
                    .iter()
                    .map(|(k, r)| (k.to_string(), r.to_string()))
                    .collect(),
            ),
            refuse: None,
            hold: Mutex::new(false),
            order_by: Mutex::new(None),
            calls: Mutex::new(vec![]),
        }
    }

    fn row(key: &str, rank: &str, full: bool) -> Value {
        let mut v = json!({ "key": key, "summary": format!("Issue {key}"), "status": "To Do",
            "updated": "2026-11-09T10:00:00.000+0000", "labels": ["web"], "rank": rank });
        if full {
            v["description_html"] = json!("<p>About it.</p>");
        }
        v
    }

    fn ranked(&self) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, a)| {
                t == "jira.update_issue"
                    && (a.get("rank_after_issue").is_some() || a.get("rank_before_issue").is_some())
            })
            .map(|(_, a)| a.clone())
            .collect()
    }
}

#[async_trait]
impl Remote for RankJira {
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
        let mut issues = self.issues.lock().unwrap();
        match tool {
            "jira.search" => Ok(json!({
                "issues": issues.iter().map(|(k, r)| Self::row(k, r, false)).collect::<Vec<_>>(),
                "is_last": true,
            })),
            "jira.get_issue" => {
                let key = args["issue_key"].as_str().unwrap_or_default();
                issues
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(k, r)| Self::row(k, r, true))
                    .ok_or_else(|| RemoteError::Failed("404".into()))
            }
            "jira.update_issue" => {
                if let Some(e) = &self.refuse {
                    return Err(e.clone());
                }
                let key = args["issue_key"].as_str().unwrap().to_string();
                let ranks = args.get("rank_after_issue").is_some()
                    || args.get("rank_before_issue").is_some();
                if ranks && *self.hold.lock().unwrap() {
                    return Ok(json!({ "status": "approval_pending", "request_id": "r-1" }));
                }
                let mut sorted: Vec<String> = issues.iter().map(|(_, r)| r.clone()).collect();
                sorted.sort();
                let rank_of = |k: &str| issues.iter().find(|(x, _)| x == k).unwrap().1.clone();
                let new = match (
                    args["rank_after_issue"].as_str(),
                    args["rank_before_issue"].as_str(),
                ) {
                    (Some(a), _) => {
                        let a = rank_of(a);
                        let next = sorted.iter().find(|r| **r > a).cloned();
                        crate::board::place_between(Some(&a), next.as_deref())
                    }
                    (None, Some(b)) => {
                        let b = rank_of(b);
                        let prev = sorted.iter().rev().find(|r| **r < b).cloned();
                        crate::board::place_between(prev.as_deref(), Some(&b))
                    }
                    _ => return Ok(json!({ "key": key, "updated": true })),
                }
                .unwrap();
                issues.iter_mut().find(|(k, _)| *k == key).unwrap().1 = new;
                Ok(json!({ "key": key, "updated": false, "ranked": true }))
            }
            "jira.board_order" => match self.order_by.lock().unwrap().clone() {
                Some(clause) if clause == "!offline" => Err(RemoteError::Offline("down".into())),
                Some(clause) => Ok(json!({ "boards": [{ "id": 7, "name": "ACME board" }],
                    "board": { "id": 7, "name": "ACME board" }, "order_by": clause,
                    "rank_field": "customfield_10019" })),
                None => Err(RemoteError::Missing(tool.into())),
            },
            "jira.list_transitions" => Ok(json!({ "transitions": [] })),
            "jira.add_comment" => Ok(json!({ "id": "c-1" })),
            other => Err(RemoteError::NotAllowed(other.into())),
        }
    }
}

/// The keys of board `b`'s tickets in `state`, in the board's order.
async fn column_keys(d: &Desk, b: &Board, state: TicketState) -> Vec<String> {
    d.0.store
        .board_tickets(b.id)
        .await
        .unwrap()
        .into_iter()
        .filter(|t| t.state == state)
        .filter_map(|t| t.external.map(|e| e.key))
        .collect()
}

async fn id_of(d: &Desk, b: &Board, key: &str) -> uuid::Uuid {
    ticket_of(d, b, key).await.ticket.id
}

/// Issues come onto the board in their Rank's order there, whatever order
/// they changed in, and each ticket says its place as `upstreamRank`.
#[tokio::test]
async fn a_synced_board_shows_its_issues_in_the_source_s_order() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        let jira = RankJira::new(&[("ACME-1", "0|c"), ("ACME-2", "0|a"), ("ACME-3", "0|b")]);
        d.pull_board(b.id, &jira).await.unwrap();
        assert_eq!(
            column_keys(&d, &b, TicketState::Todo).await,
            ["ACME-2", "ACME-3", "ACME-1"]
        );
        let v = serde_json::to_value(ticket_of(&d, &b, "ACME-2").await).unwrap();
        assert_eq!(v["upstreamRank"], "0|a");
        // Moved there by someone else: the next pull's order wins.
        jira.issues.lock().unwrap()[0].1 = "0|0".into();
        d.pull_board(b.id, &jira).await.unwrap();
        assert_eq!(
            column_keys(&d, &b, TicketState::Todo).await,
            ["ACME-1", "ACME-2", "ACME-3"]
        );
    })
    .await;
}

/// A drop within a column sorts the card where it fell at once, ranks its
/// issue there after the one above it, and the next pull reads the Rank
/// Jira gave it.
#[tokio::test]
async fn a_drop_within_a_column_moves_the_issue_there() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        let jira = RankJira::new(&[("ACME-1", "0|c"), ("ACME-2", "0|a"), ("ACME-3", "0|b")]);
        d.pull_board(b.id, &jira).await.unwrap();
        let (one, two, three) = (
            id_of(&d, &b, "ACME-1").await,
            id_of(&d, &b, "ACME-2").await,
            id_of(&d, &b, "ACME-3").await,
        );
        let mv = crate::service::Move {
            state: TicketState::Todo,
            before: Some(two),
            after: Some(three),
        };
        let v = d.move_ticket(one, mv, &By::you()).await.unwrap();
        let place = v.ticket.upstream_rank.clone().unwrap();
        assert!("0|a" < place.as_str() && place.as_str() < "0|b", "{place}");
        assert_eq!(
            column_keys(&d, &b, TicketState::Todo).await,
            ["ACME-2", "ACME-1", "ACME-3"]
        );
        due_now(&d, b.id).await;
        d.push_board(b.id, &jira).await.unwrap();
        assert_eq!(
            jira.ranked(),
            [json!({ "issue_key": "ACME-1", "rank_after_issue": "ACME-2" })]
        );
        let t = d.ticket(one).await.unwrap().ticket;
        let e = t.external.as_ref().unwrap();
        assert!(e.rank_move.is_none());
        assert!(e.rank_stale, "Jira's new Rank is read back");
        assert!(d.nothing_to_push(b.id).await.unwrap());
        let theirs = jira.issues.lock().unwrap()[0].1.clone();
        d.pull_board(b.id, &jira).await.unwrap();
        let t = d.ticket(one).await.unwrap().ticket;
        assert_eq!(t.upstream_rank.as_deref(), Some(theirs.as_str()));
        assert!(!t.external.unwrap().rank_stale);
        assert_eq!(
            column_keys(&d, &b, TicketState::Todo).await,
            ["ACME-2", "ACME-1", "ACME-3"]
        );
        // Dropped at the top, it goes before the issue below it.
        let mv = crate::service::Move {
            state: TicketState::Todo,
            before: None,
            after: Some(two),
        };
        d.move_ticket(three, mv, &By::you()).await.unwrap();
        due_now(&d, b.id).await;
        d.push_board(b.id, &jira).await.unwrap();
        assert_eq!(
            jira.ranked()[1],
            json!({ "issue_key": "ACME-3", "rank_before_issue": "ACME-2" })
        );
        assert!(d
            .board_of(b.id)
            .await
            .unwrap()
            .sync
            .unwrap()
            .order_problem
            .is_none());
    })
    .await;
}

/// A drop Jira won't take (no right to rank, a hub without the call) stands
/// here, is said once on the sync bar, never pauses the board or is sent
/// again, and the next pull puts the card back where its issue is.
#[tokio::test]
async fn a_drop_that_can_t_go_there_stands_here_and_says_so() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        for refuse in [
            RemoteError::Failed(
                "Couldn't rank ACME-1: You don't have permission to rank issues.".into(),
            ),
            RemoteError::Missing("jira.update_issue".into()),
        ] {
            let mut jira =
                RankJira::new(&[("ACME-1", "0|c"), ("ACME-2", "0|a"), ("ACME-3", "0|b")]);
            jira.refuse = Some(refuse);
            d.pull_board(b.id, &jira).await.unwrap();
            let (one, two) = (id_of(&d, &b, "ACME-1").await, id_of(&d, &b, "ACME-2").await);
            let mv = crate::service::Move {
                state: TicketState::Todo,
                before: Some(two),
                after: None,
            };
            d.move_ticket(one, mv, &By::you()).await.unwrap();
            due_now(&d, b.id).await;
            d.push_board(b.id, &jira).await.unwrap();
            assert_eq!(jira.ranked().len(), 1);
            let s = d.board_of(b.id).await.unwrap().sync.unwrap();
            assert!(s.paused.is_none(), "{:?}", s.paused);
            assert!(s.backoff_until.is_none());
            let said = s.order_problem.clone().unwrap();
            assert!(said.contains("ACME-1"), "{said}");
            assert!(d.nothing_to_push(b.id).await.unwrap(), "never sent again");
            assert_eq!(
                column_keys(&d, &b, TicketState::Todo).await,
                ["ACME-2", "ACME-1", "ACME-3"],
                "the drop stands here"
            );
            let t = d.ticket(one).await.unwrap().ticket;
            assert!(t.external.as_ref().unwrap().rank_stale);
            d.pull_board(b.id, &jira).await.unwrap();
            assert_eq!(
                column_keys(&d, &b, TicketState::Todo).await,
                ["ACME-2", "ACME-3", "ACME-1"],
                "its place there wins"
            );
            let s = d.board_of(b.id).await.unwrap().sync.unwrap();
            assert_eq!(
                s.order_problem.as_deref(),
                Some(said.as_str()),
                "said until a drop goes"
            );
            assert_eq!(jira.ranked().len(), 1);
        }
    })
    .await;
}

/// A board whose sync is paused never moves an issue there: the drop stands
/// here, and once it is resumed the issue's place there wins.
#[tokio::test]
async fn a_paused_board_never_moves_an_issue_there() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        let jira = RankJira::new(&[("ACME-1", "0|c"), ("ACME-2", "0|a"), ("ACME-3", "0|b")]);
        d.pull_board(b.id, &jira).await.unwrap();
        d.pause_board(b.id, true).await.unwrap();
        let (one, two) = (id_of(&d, &b, "ACME-1").await, id_of(&d, &b, "ACME-2").await);
        let mv = crate::service::Move {
            state: TicketState::Todo,
            before: Some(two),
            after: None,
        };
        let v = d.move_ticket(one, mv, &By::you()).await.unwrap();
        let e = v.ticket.external.as_ref().unwrap();
        assert!(e.rank_move.is_none() && e.rank_stale);
        d.pause_board(b.id, false).await.unwrap();
        due_now(&d, b.id).await;
        d.push_board(b.id, &jira).await.unwrap();
        assert!(jira.ranked().is_empty());
        d.pull_board(b.id, &jira).await.unwrap();
        assert_eq!(
            column_keys(&d, &b, TicketState::Todo).await,
            ["ACME-2", "ACME-3", "ACME-1"]
        );
    })
    .await;
}

/// A move to another column changes only its state there, as ever.
#[tokio::test]
async fn a_move_to_another_column_ranks_nothing_there() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        let jira = RankJira::new(&[("ACME-1", "0|c"), ("ACME-2", "0|a")]);
        d.pull_board(b.id, &jira).await.unwrap();
        let one = id_of(&d, &b, "ACME-1").await;
        let mv = crate::service::Move {
            state: TicketState::Doing,
            before: None,
            after: None,
        };
        let v = d.move_ticket(one, mv, &By::you()).await.unwrap();
        let e = v.ticket.external.as_ref().unwrap();
        assert!(e.rank_move.is_none() && !e.rank_stale);
        assert_eq!(v.ticket.upstream_rank.as_deref(), Some("0|c"));
        due_now(&d, b.id).await;
        d.push_board(b.id, &jira).await.unwrap();
        assert!(jira.ranked().is_empty());
    })
    .await;
}

/// On Linear a drop takes the sortOrder between its neighbours' (lower
/// first, negatives too), sent with `linear.issue_update`, and is its place.
#[tokio::test]
async fn a_linear_drop_takes_the_sort_order_between_its_neighbours() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = whole_team(&d, Some(ME)).await;
        let linear = FakeLinear::default();
        for (n, at) in [(1, -5.0), (2, 10.0), (3, 2.0)] {
            linear.set(n, "unstarted", n, None);
            linear.sorts.lock().unwrap().insert(n, at);
        }
        d.pull_board(b.id, &linear).await.unwrap();
        assert_eq!(
            column_keys(&d, &b, TicketState::Todo).await,
            ["ENG-1", "ENG-3", "ENG-2"]
        );
        let (one, two, three) = (
            id_of(&d, &b, "ENG-1").await,
            id_of(&d, &b, "ENG-2").await,
            id_of(&d, &b, "ENG-3").await,
        );
        let mv = crate::service::Move {
            state: TicketState::Todo,
            before: Some(one),
            after: Some(three),
        };
        let v = d.move_ticket(two, mv, &By::you()).await.unwrap();
        assert_eq!(v.ticket.upstream_rank, crate::board::linear_rank_of(-1.5));
        due_now(&d, b.id).await;
        d.push_board(b.id, &linear).await.unwrap();
        let sent: Vec<Value> = linear
            .calls_of("linear.issue_update")
            .into_iter()
            .filter(|a| a.get("sort_order").is_some())
            .collect();
        assert_eq!(sent, [json!({ "identifier": "ENG-2", "sort_order": -1.5 })]);
        let t = d.ticket(two).await.unwrap().ticket;
        let e = t.external.as_ref().unwrap();
        assert!(
            e.rank_move.is_none() && !e.rank_stale,
            "its place is the one sent"
        );
        assert_eq!(
            column_keys(&d, &b, TicketState::Todo).await,
            ["ENG-1", "ENG-2", "ENG-3"]
        );
    })
    .await;
}

/// A drop whose rank call the hub holds for approval waits for the next
/// push; what went out with it (a move's comment there) is kept as sent, so
/// the retry sends only the rank.
#[tokio::test]
async fn a_held_rank_waits_and_what_went_with_it_never_goes_twice() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = d
            .create_board(NewBoard {
                name: "Acme".into(),
                group: None,
                prefix: None,
            })
            .await
            .unwrap();
        let mut l = link(None);
        l.states.insert(TicketState::Review, "In Review".into());
        let b = d.link_board(b.id, l, "sam").await.unwrap();
        let jira = RankJira::new(&[("ACME-1", "0|c"), ("ACME-2", "0|a"), ("ACME-3", "0|b")]);
        d.pull_board(b.id, &jira).await.unwrap();
        let (one, two) = (id_of(&d, &b, "ACME-1").await, id_of(&d, &b, "ACME-2").await);
        // Dropped within To do, then moved on to Review before it went out.
        let mv = crate::service::Move {
            state: TicketState::Todo,
            before: Some(two),
            after: None,
        };
        d.move_ticket(one, mv, &By::you()).await.unwrap();
        let mv = crate::service::Move {
            state: TicketState::Review,
            before: None,
            after: None,
        };
        d.move_ticket(one, mv, &By::you()).await.unwrap();
        *jira.hold.lock().unwrap() = true;
        due_now(&d, b.id).await;
        let pushed = d.push_board(b.id, &jira).await.unwrap();
        assert_eq!(pushed.waiting, 1);
        let comments = |j: &RankJira| {
            j.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(t, _)| t == "jira.add_comment")
                .count()
        };
        assert_eq!(comments(&jira), 1);
        assert_eq!(jira.ranked().len(), 1, "asked, and held");
        let t = d.ticket(one).await.unwrap().ticket;
        let e = t.external.as_ref().unwrap();
        assert!(e.rank_move.is_some(), "the drop still waits");
        assert_eq!(
            e.seen.state,
            Some(TicketState::Review),
            "the move is kept as sent"
        );
        *jira.hold.lock().unwrap() = false;
        // Held, it waits its usual while: not sent again at once.
        d.push_board(b.id, &jira).await.unwrap();
        assert_eq!(
            jira.ranked().len(),
            1,
            "not retried before its wait is over"
        );
        due_now(&d, b.id).await;
        d.push_board(b.id, &jira).await.unwrap();
        assert_eq!(comments(&jira), 1, "never commented twice");
        assert_eq!(jira.ranked().len(), 2);
        assert_eq!(
            jira.ranked()[1],
            json!({ "issue_key": "ACME-1", "rank_after_issue": "ACME-2" })
        );
        let t = d.ticket(one).await.unwrap().ticket;
        assert!(t.external.unwrap().rank_move.is_none());
        assert!(d.nothing_to_push(b.id).await.unwrap());
    })
    .await;
}

/// On Linear, a held sortOrder change waits too, and a need that went out
/// with it as a link is never linked twice.
#[tokio::test]
async fn a_held_linear_sort_order_waits_and_its_link_goes_once() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = whole_team(&d, Some(ME)).await;
        let linear = FakeLinear::default();
        for (n, at) in [(1, -5.0), (2, 10.0), (3, 2.0)] {
            linear.set(n, "unstarted", n, None);
            linear.sorts.lock().unwrap().insert(n, at);
        }
        d.pull_board(b.id, &linear).await.unwrap();
        let (one, two, three) = (
            id_of(&d, &b, "ENG-1").await,
            id_of(&d, &b, "ENG-2").await,
            id_of(&d, &b, "ENG-3").await,
        );
        let mv = crate::service::Move {
            state: TicketState::Todo,
            before: Some(one),
            after: Some(three),
        };
        d.move_ticket(two, mv, &By::you()).await.unwrap();
        d.need_ticket(two, one, true, &By::you()).await.unwrap();
        *linear.hold_sort.lock().unwrap() = true;
        due_now(&d, b.id).await;
        d.push_board(b.id, &linear).await.unwrap();
        assert_eq!(linear.calls_of("linear.issue_link").len(), 1);
        let sorts = |l: &FakeLinear| {
            l.calls_of("linear.issue_update")
                .into_iter()
                .filter(|a| a.get("sort_order").is_some())
                .count()
        };
        assert_eq!(sorts(&linear), 1);
        *linear.hold_sort.lock().unwrap() = false;
        due_now(&d, b.id).await;
        d.push_board(b.id, &linear).await.unwrap();
        assert_eq!(linear.calls_of("linear.issue_link").len(), 1, "linked once");
        assert_eq!(sorts(&linear), 2);
        let t = d.ticket(two).await.unwrap().ticket;
        assert!(t.external.unwrap().rank_move.is_none());
        assert_eq!(
            column_keys(&d, &b, TicketState::Todo).await,
            ["ENG-1", "ENG-2", "ENG-3"]
        );
    })
    .await;
}

// ── GitHub ───────────────────────────────────────────────────────────────

/// A GitHub project, as the hub's board tools answer (fixtures built from
/// their shapes, never recorded): items in its own order, each with its
/// issue's Status; every call kept.
struct FakeGithub {
    /// Key (`acme/web#N`), item id, Status, updated.
    items: Mutex<Vec<(String, String, String, String)>>,
    /// Items listed raw after these (one the key can't see, say).
    extra: Mutex<Vec<Value>>,
    /// Issues gone from the repository, not only from the project.
    deleted: Mutex<Vec<String>>,
    calls: Mutex<Vec<(String, Value)>>,
}

impl FakeGithub {
    fn new(items: &[(&str, &str, &str)]) -> Self {
        Self {
            items: Mutex::new(
                items
                    .iter()
                    .map(|(k, id, st)| {
                        (
                            k.to_string(),
                            id.to_string(),
                            st.to_string(),
                            "2026-11-18T10:00:00Z".to_string(),
                        )
                    })
                    .collect(),
            ),
            extra: Mutex::new(vec![]),
            deleted: Mutex::new(vec![]),
            calls: Mutex::new(vec![]),
        }
    }

    fn called(&self, tool: &str) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t == tool)
            .map(|(_, a)| a.clone())
            .collect()
    }

    fn item(&self, i: &(String, String, String, String), full: bool) -> Value {
        let (repo, n) = i.0.split_once('#').unwrap();
        let mut v = json!({ "item_id": i.1, "type": "ISSUE", "updated_at": i.3,
            "repo": repo, "number": n.parse::<u64>().unwrap() });
        if full {
            v["title"] = json!(format!("Issue {}", i.0));
            v["body"] = json!("About it.");
            v["state"] = json!("OPEN");
            v["labels"] = json!(["web"]);
            v["assignees"] = json!([]);
            v["url"] = json!(format!("https://github.com/{repo}/issues/{n}"));
            v["status"] = json!(i.2);
        }
        v
    }
}

#[async_trait]
impl Remote for FakeGithub {
    async fn tools(&self) -> Result<Vec<String>, RemoteError> {
        Ok(crate::github_sync::TOOLS
            .iter()
            .map(|t| t.to_string())
            .collect())
    }

    async fn call(&self, tool: &str, args: Value) -> Result<Value, RemoteError> {
        self.calls
            .lock()
            .unwrap()
            .push((tool.to_string(), args.clone()));
        let items = self.items.lock().unwrap().clone();
        match tool {
            "github.project_items" => match args["ids"].as_array() {
                Some(ids) => Ok(json!({ "items": items.iter()
                    .filter(|i| ids.iter().any(|id| id == &json!(i.1)))
                    .map(|i| self.item(i, true)).collect::<Vec<_>>() })),
                None => {
                    let mut listed: Vec<Value> =
                        items.iter().map(|i| self.item(i, false)).collect();
                    listed.extend(self.extra.lock().unwrap().iter().cloned());
                    Ok(json!({
                        "items": listed,
                        "page_info": { "has_next_page": false, "end_cursor": null },
                    }))
                }
            },
            // One issue read by its number, as the repository has it: off
            // the project or not, it is there until deleted.
            "github.issues_changed" => {
                let key = format!(
                    "{}/{}#{}",
                    args["owner"].as_str().unwrap_or_default(),
                    args["repo"].as_str().unwrap_or_default(),
                    args["number"]
                );
                if self.deleted.lock().unwrap().contains(&key) {
                    return Ok(json!({ "issues": [] }));
                }
                let (repo, n) = key.split_once('#').unwrap();
                Ok(json!({ "issues": [{
                    "repo": repo, "number": n.parse::<u64>().unwrap(),
                    "title": format!("Issue {key}"), "body": "About it.", "state": "OPEN",
                    "labels": [], "assignees": [], "updated_at": "2026-11-18T10:00:00Z",
                    "url": format!("https://github.com/{repo}/issues/{n}"),
                }] }))
            }
            "github.project_item_status" | "github.issue_update" | "github.issue_comment" => {
                Ok(json!({ "ok": true }))
            }
            "github.project_item_move" => Ok(json!({ "moved": true, "item_id": args["item_id"] })),
            other => Err(RemoteError::NotAllowed(other.into())),
        }
    }
}

async fn github_board(d: &Desk) -> Board {
    let b = d
        .create_board(NewBoard {
            name: "Web".into(),
            group: None,
            prefix: None,
        })
        .await
        .unwrap();
    let mut l = link(None);
    l.system = System::Github;
    l.target = "acme/projects/7".into();
    l.target_id = Some("PVT_kwDOA".into());
    l.filter = Filter::All {
        include_done: false,
    };
    l.states = BTreeMap::from([
        (TicketState::Todo, "Todo".to_string()),
        (TicketState::Doing, "In Progress".to_string()),
        (TicketState::Done, "Done".to_string()),
    ]);
    d.link_board(b.id, l, "sam").await.unwrap()
}

/// A project's issues come on in its own order, in their Status's column;
/// moved there, they move here on the next pass with no issue read again.
#[tokio::test]
async fn a_github_project_comes_on_in_its_own_order() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = github_board(&d).await;
        let gh = FakeGithub::new(&[
            ("acme/web#3", "PVTI_3", "Todo"),
            ("acme/api#1", "PVTI_1", "Todo"),
            ("acme/web#2", "PVTI_2", "In Progress"),
        ]);
        let pulled = d.pull_board(b.id, &gh).await.unwrap();
        assert_eq!(pulled.imported, 3, "{pulled:?}");
        assert_eq!(
            column_keys(&d, &b, TicketState::Todo).await,
            ["acme/web#3", "acme/api#1"]
        );
        assert_eq!(
            column_keys(&d, &b, TicketState::Doing).await,
            ["acme/web#2"]
        );
        let t = ticket_of(&d, &b, "acme/web#3").await.ticket;
        assert_eq!(t.upstream_rank.as_deref(), Some("0000000000"));
        assert_eq!(t.external.unwrap().id.as_deref(), Some("PVTI_3"));
        // Reordered there, nothing else changed.
        gh.items.lock().unwrap().swap(0, 1);
        let full_before = gh
            .called("github.project_items")
            .iter()
            .filter(|a| a.get("ids").is_some())
            .count();
        d.pull_board(b.id, &gh).await.unwrap();
        assert_eq!(
            column_keys(&d, &b, TicketState::Todo).await,
            ["acme/api#1", "acme/web#3"]
        );
        let full_after = gh
            .called("github.project_items")
            .iter()
            .filter(|a| a.get("ids").is_some())
            .count();
        assert_eq!(
            full_before, full_after,
            "only the lean listing is read for order"
        );
    })
    .await;
}

/// A drag moves the item there after the card above it, or to the top; a
/// move to another column sets its Status, and Done closes its issue.
#[tokio::test]
async fn a_github_drop_and_move_go_out_as_the_project_s_own() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = github_board(&d).await;
        let gh = FakeGithub::new(&[
            ("acme/web#1", "PVTI_1", "Todo"),
            ("acme/web#2", "PVTI_2", "Todo"),
            ("acme/web#3", "PVTI_3", "Todo"),
        ]);
        d.pull_board(b.id, &gh).await.unwrap();
        let (one, two, three) = (
            id_of(&d, &b, "acme/web#1").await,
            id_of(&d, &b, "acme/web#2").await,
            id_of(&d, &b, "acme/web#3").await,
        );
        // #1 dropped between #2 and #3.
        let mv = crate::service::Move {
            state: TicketState::Todo,
            before: Some(two),
            after: Some(three),
        };
        d.move_ticket(one, mv, &By::you()).await.unwrap();
        assert_eq!(
            column_keys(&d, &b, TicketState::Todo).await,
            ["acme/web#2", "acme/web#1", "acme/web#3"]
        );
        due_now(&d, b.id).await;
        d.push_board(b.id, &gh).await.unwrap();
        assert_eq!(
            gh.called("github.project_item_move"),
            [json!({ "project_id": "PVT_kwDOA", "item_id": "PVTI_1", "after_id": "PVTI_2" })]
        );
        // #3 dropped at the top: no item before it there.
        let mv = crate::service::Move {
            state: TicketState::Todo,
            before: None,
            after: Some(two),
        };
        d.move_ticket(three, mv, &By::you()).await.unwrap();
        due_now(&d, b.id).await;
        d.push_board(b.id, &gh).await.unwrap();
        assert_eq!(
            gh.called("github.project_item_move")[1],
            json!({ "project_id": "PVT_kwDOA", "item_id": "PVTI_3", "after_id": null })
        );
        // Done: its Status, then its issue closed as completed.
        let mv = crate::service::Move {
            state: TicketState::Done,
            before: None,
            after: None,
        };
        d.move_ticket(two, mv, &By::you()).await.unwrap();
        due_now(&d, b.id).await;
        d.push_board(b.id, &gh).await.unwrap();
        let status = gh.called("github.project_item_status");
        assert_eq!(status.last().unwrap()["status"], "Done");
        assert_eq!(status.last().unwrap()["item_id"], "PVTI_2");
        let closed = gh.called("github.issue_update");
        assert_eq!(closed.last().unwrap()["state"], "closed");
        assert_eq!(closed.last().unwrap()["number"], 2);
    })
    .await;
}

/// The sweep keeps what is still on the project, changed or not, and puts
/// an item taken off it on the Left-the-filter shelf.
#[tokio::test]
async fn a_github_sweep_shelves_only_what_left_the_project() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = github_board(&d).await;
        let gh = FakeGithub::new(&[
            ("acme/web#1", "PVTI_1", "Todo"),
            ("acme/web#2", "PVTI_2", "Todo"),
        ]);
        d.pull_board(b.id, &gh).await.unwrap();
        d.change_sync(b.id, |s| s.swept_at = None).await.unwrap();
        let pulled = d.pull_board(b.id, &gh).await.unwrap();
        assert_eq!(pulled.detached, 0, "{pulled:?}");
        gh.items.lock().unwrap().remove(1);
        d.change_sync(b.id, |s| s.swept_at = None).await.unwrap();
        let pulled = d.pull_board(b.id, &gh).await.unwrap();
        assert_eq!(pulled.detached, 1, "{pulled:?}");
        let t = ticket_of(&d, &b, "acme/web#2").await.ticket;
        assert!(!t.external.unwrap().synced);
        // Its issue was read before it was shelved: still there, only off
        // the project.
        assert_eq!(gh.called("github.issues_changed").len(), 1);
        assert!(
            ticket_of(&d, &b, "acme/web#1")
                .await
                .ticket
                .external
                .unwrap()
                .synced
        );
    })
    .await;
}

/// A listing that can't say what isn't on the project (an item the key
/// can't see, or no items at all) shelves nothing.
#[tokio::test]
async fn a_github_sweep_on_a_listing_it_cant_trust_shelves_nothing() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = github_board(&d).await;
        let gh = FakeGithub::new(&[
            ("acme/web#1", "PVTI_1", "Todo"),
            ("acme/web#2", "PVTI_2", "Todo"),
        ]);
        d.pull_board(b.id, &gh).await.unwrap();
        // The key lost the repository: its items show as redacted.
        gh.items.lock().unwrap().remove(1);
        gh.extra.lock().unwrap().push(json!({
            "item_id": "PVTI_2", "type": "REDACTED", "updated_at": "2026-11-18T10:00:00Z"
        }));
        d.change_sync(b.id, |s| s.swept_at = None).await.unwrap();
        let pulled = d.pull_board(b.id, &gh).await.unwrap();
        assert_eq!(pulled.detached, 0, "{pulled:?}");
        assert_eq!(pulled.dropped, 0, "{pulled:?}");
        // Nothing listed at all: asked again later, not every card shelved.
        gh.items.lock().unwrap().clear();
        gh.extra.lock().unwrap().clear();
        d.change_sync(b.id, |s| s.swept_at = None).await.unwrap();
        let pulled = d.pull_board(b.id, &gh).await.unwrap();
        assert_eq!(pulled.detached, 0, "{pulled:?}");
        for key in ["acme/web#1", "acme/web#2"] {
            assert!(ticket_of(&d, &b, key).await.ticket.external.unwrap().synced);
        }
    })
    .await;
}

/// Dropped closes the issue as not planned even with no column mapped to
/// it, closed before its Status is set.
#[tokio::test]
async fn a_github_drop_to_dropped_closes_it_not_planned() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = github_board(&d).await;
        let gh = FakeGithub::new(&[("acme/web#1", "PVTI_1", "Todo")]);
        d.pull_board(b.id, &gh).await.unwrap();
        let one = id_of(&d, &b, "acme/web#1").await;
        let mv = crate::service::Move {
            state: TicketState::Dropped,
            before: None,
            after: None,
        };
        d.move_ticket(one, mv, &By::you()).await.unwrap();
        due_now(&d, b.id).await;
        d.push_board(b.id, &gh).await.unwrap();
        let closed = gh.called("github.issue_update");
        assert_eq!(closed.len(), 1, "{:?}", gh.calls.lock().unwrap());
        assert_eq!(closed[0]["state"], "closed");
        assert_eq!(closed[0]["state_reason"], "not_planned");
        assert!(gh.called("github.project_item_status").is_empty());
    })
    .await;
}

/// A board ordered there by something other than Rank says so, and a drop
/// within a column then never moves its issue there; once it is by Rank
/// again (read on the fields' cadence), a drop does.
#[tokio::test]
async fn a_drop_moves_nothing_there_while_the_board_is_ordered_by_more_than_rank() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        let jira = RankJira::new(&[("ACME-1", "0|c"), ("ACME-2", "0|a"), ("ACME-3", "0|b")]);
        *jira.order_by.lock().unwrap() = Some("created DESC, Rank ASC".into());
        d.pull_board(b.id, &jira).await.unwrap();
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        let o = s.ordering.clone().unwrap();
        assert_eq!(o.by[0].field, crate::board::OrderField::Created);
        assert_eq!(o.from.as_deref(), Some("ACME board's filter"));
        assert!(!s.moves_by_hand());
        let (one, two, three) = (
            id_of(&d, &b, "ACME-1").await,
            id_of(&d, &b, "ACME-2").await,
            id_of(&d, &b, "ACME-3").await,
        );
        let mv = || crate::service::Move {
            state: TicketState::Todo,
            before: Some(two),
            after: Some(three),
        };
        d.move_ticket(one, mv(), &By::you()).await.unwrap();
        let t = d.ticket(one).await.unwrap().ticket;
        assert!(t.external.as_ref().unwrap().rank_move.is_none());
        assert_eq!(
            t.upstream_rank.as_deref(),
            Some("0|c"),
            "its place there stands"
        );
        due_now(&d, b.id).await;
        d.push_board(b.id, &jira).await.unwrap();
        assert!(jira.ranked().is_empty());
        // By Rank there again, read a day on: a drop moves its issue.
        *jira.order_by.lock().unwrap() = Some("Rank ASC".into());
        d.change_sync(b.id, |s| {
            if let Some(o) = s.ordering.as_mut() {
                o.read_at -= chrono::Duration::days(2);
            }
        })
        .await
        .unwrap();
        d.pull_board(b.id, &jira).await.unwrap();
        assert!(d.board_sync(b.id).await.unwrap().unwrap().1.moves_by_hand());
        d.move_ticket(one, mv(), &By::you()).await.unwrap();
        let t = d.ticket(one).await.unwrap().ticket;
        assert!(t.external.as_ref().unwrap().rank_move.is_some());
    })
    .await;
}

/// A hub that can't say how the board there is ordered leaves it by Rank,
/// said so, and a board most of whose issues have no place yet sweeps on
/// its next pass once the ordering is read.
#[tokio::test]
async fn a_board_without_its_order_or_places_says_so_and_catches_up() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        let jira = RankJira::new(&[("ACME-1", "0|c"), ("ACME-2", "0|a"), ("ACME-3", "0|b")]);
        d.pull_board(b.id, &jira).await.unwrap();
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        let o = s.ordering.clone().unwrap();
        assert!(o.by.is_empty());
        assert!(o.note.as_deref().unwrap().contains("Jira's Rank"), "{o:?}");
        assert!(s.moves_by_hand());
        // Their places lost (a board linked before they were kept), its
        // last sweep just now: the pass after the ordering is read again
        // sweeps, and reads them.
        for k in ["ACME-1", "ACME-2", "ACME-3"] {
            let id = id_of(&d, &b, k).await;
            let mut t = d.ticket(id).await.unwrap().ticket;
            t.upstream_rank = None;
            d.save_ticket(&mut t).await.unwrap();
        }
        let now = crate::crew::now();
        d.change_sync(b.id, |s| {
            s.swept_at = Some(now);
            s.places_swept_at = Some(now - chrono::Duration::minutes(1));
            if let Some(o) = s.ordering.as_mut() {
                o.read_at = now;
            }
        })
        .await
        .unwrap();
        d.pull_board(b.id, &jira).await.unwrap();
        let t = d
            .ticket(id_of(&d, &b, "ACME-2").await)
            .await
            .unwrap()
            .ticket;
        assert_eq!(t.upstream_rank.as_deref(), Some("0|a"));
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        assert!(s.places_swept_at.unwrap() >= now);
    })
    .await;
}

/// A passing failure to read the order there keeps what was read and asks
/// again soon; one before anything was read says so and asks soon too.
/// Fields read since an ordering was read are read with again.
#[tokio::test]
async fn a_passing_failure_keeps_the_order_and_asks_again_soon() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = synced(&d, "Acme", None).await;
        let jira = RankJira::new(&[("ACME-1", "0|c")]);
        *jira.order_by.lock().unwrap() = Some("!offline".into());
        d.pull_board(b.id, &jira).await.unwrap();
        let o = d
            .board_sync(b.id)
            .await
            .unwrap()
            .unwrap()
            .1
            .ordering
            .unwrap();
        let now = crate::crew::now();
        assert!(o.by.is_empty());
        assert!(
            !crate::board_sync::ordering_fresh(&o, now + chrono::Duration::minutes(16)),
            "asked again within a quarter of an hour"
        );
        // Read: by priority.
        *jira.order_by.lock().unwrap() = Some("priority DESC".into());
        d.change_sync(b.id, |s| {
            s.fields = Some(crate::board::JiraFields {
                priorities: vec!["High".into(), "Low".into()],
                rank_asked: true,
                rank: Some("customfield_10019".into()),
                read_at: now,
                ..Default::default()
            });
            if let Some(o) = s.ordering.as_mut() {
                o.read_at -= chrono::Duration::days(2);
                o.retry_at = None;
            }
        })
        .await
        .unwrap();
        d.pull_board(b.id, &jira).await.unwrap();
        let o = d
            .board_sync(b.id)
            .await
            .unwrap()
            .unwrap()
            .1
            .ordering
            .unwrap();
        assert_eq!(o.by[0].field, crate::board::OrderField::Priority);
        // A day on, the hub offline: still by priority, asked again soon.
        d.change_sync(b.id, |s| {
            if let Some(o) = s.ordering.as_mut() {
                o.read_at -= chrono::Duration::days(2);
                o.retry_at = None;
            }
        })
        .await
        .unwrap();
        *jira.order_by.lock().unwrap() = Some("!offline".into());
        d.pull_board(b.id, &jira).await.unwrap();
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        assert_eq!(
            s.ordering.as_ref().unwrap().by[0].field,
            crate::board::OrderField::Priority
        );
        assert!(!s.moves_by_hand());
        // Fields read after the ordering: the ordering is read with them.
        *jira.order_by.lock().unwrap() = Some("Rank ASC".into());
        let later = crate::crew::now() + chrono::Duration::seconds(1);
        d.change_sync(b.id, |s| {
            if let (Some(f), Some(o)) = (s.fields.as_mut(), s.ordering.as_mut()) {
                o.read_at = later - chrono::Duration::minutes(1);
                o.retry_at = None;
                // One read before the site's priorities were.
                o.wants_priorities = true;
                f.read_at = later;
            }
        })
        .await
        .unwrap();
        d.pull_board(b.id, &jira).await.unwrap();
        let s = d.board_sync(b.id).await.unwrap().unwrap().1;
        assert_eq!(
            s.ordering.unwrap().by[0].field,
            crate::board::OrderField::Rank
        );
    })
    .await;
}
