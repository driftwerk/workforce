//! The board end to end in the desk: numbering, moves, dependencies,
//! assignment (goal, link, line, ledger stamp), the `ticket` tool on a
//! line, pull requests, snooze, search and history.

use std::path::Path;
use std::time::Duration;

use chrono::Utc;
use serde_json::json;
use uuid::Uuid;
use workforce_postgres::test_support::with_test_pool;

use super::super::tests::desk;
use super::*;
use crate::board::{Pr, PrState, TicketState};
use crate::crew::{By, Crew, CrewAgent, Kind, Overseer, Party, Suggestion};
use crate::ledger::{Billing, Priced, Spend, Tokens};
use crate::ledger_store::{Filter, Kept};
use crate::line::LineKind;
use crate::model::{Engine, NewSession};
use crate::store::build_pool;

async fn session(d: &Desk, dir: &Path, name: &str, goal: &str) -> SessionView {
    d.create(
        NewSession {
            engine: Engine::Claude,
            view: None,
            name: name.into(),
            cwd: dir.to_string_lossy().into_owned(),
            goal: goal.into(),
            worktree: None,
            prompt: None,
            approvals: None,
            approve_for_me: false,
            account: None,
            env: Default::default(),
            group: None,
        },
        None,
    )
    .await
    .unwrap()
}

async fn board(d: &Desk, name: &str) -> crate::board::Board {
    d.create_board(NewBoard {
        name: name.into(),
        group: None,
        prefix: None,
    })
    .await
    .unwrap()
}

async fn ticket(d: &Desk, b: Uuid, title: &str, state: TicketState) -> TicketView {
    d.create_ticket(
        NewTicket {
            board: b,
            title: title.into(),
            body: String::new(),
            state: Some(state),
            needs: vec![],
        },
        &By::you(),
    )
    .await
    .unwrap()
}

fn to(state: TicketState) -> Move {
    Move {
        state,
        before: None,
        after: None,
    }
}

/// The ticket's history once at least `n` events are in, newest first.
async fn history(d: &Desk, id: Uuid, n: usize) -> Vec<crate::crew::Event> {
    for _ in 0..200 {
        let page = d.ticket_history(id, None, 50).await.unwrap();
        if page.events.len() >= n {
            return page.events;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("fewer than {n} events were recorded");
}

async fn spend(d: &Desk, session: Uuid, at: chrono::DateTime<Utc>) -> String {
    let key = Uuid::new_v4().to_string();
    d.0.store
        .ledger_write(
            session,
            Engine::Claude,
            None,
            &[Kept {
                spend: Spend {
                    key: key.clone(),
                    at,
                    model: Some("claude-opus-5-5".into()),
                    tokens: Tokens {
                        input: 100,
                        ..Default::default()
                    },
                    usd: Some(0.01),
                    priced: Priced::Reported,
                    billing: None,
                },
                billing: Billing::Plan,
                account: None,
            }],
        )
        .await
        .unwrap();
    key
}

#[tokio::test]
async fn tickets_are_numbered_once_and_a_numbered_boards_prefix_stays() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = board(&d, "Acme").await;
        assert_eq!(b.prefix.as_deref(), Some("ACM"));
        // Before it has tickets, its prefix may change.
        let b = d
            .update_board(
                b.id,
                BoardPatch {
                    prefix: Some("acme".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(b.prefix.as_deref(), Some("ACME"));
        let one = ticket(&d, b.id, "Fix the login", TicketState::Todo).await;
        let two = ticket(&d, b.id, "Add a test", TicketState::Todo).await;
        assert_eq!(one.key.as_deref(), Some("ACME-1"));
        assert_eq!(two.key.as_deref(), Some("ACME-2"));
        d.delete_ticket(two.ticket.id, &By::you()).await.unwrap();
        let three = ticket(&d, b.id, "Again", TicketState::Todo).await;
        assert_eq!(three.key.as_deref(), Some("ACME-3"), "keys never repeat");
        // Its keys are out there now.
        let refused = d
            .update_board(
                b.id,
                BoardPatch {
                    prefix: Some("ACM".into()),
                    ..Default::default()
                },
            )
            .await;
        assert!(
            matches!(&refused, Err(DeskError::Conflict(m)) if m.contains("keys stay ACME-")),
            "{refused:?}"
        );
        // A prefix another board has is refused in plain words.
        let taken = d
            .create_board(NewBoard {
                name: "Another".into(),
                group: None,
                prefix: Some("ACME".into()),
            })
            .await;
        assert!(
            matches!(&taken, Err(DeskError::Conflict(m)) if m.contains("already start with ACME")),
            "{taken:?}"
        );
        let bad = d
            .create_board(NewBoard {
                name: "Odd".into(),
                group: None,
                prefix: Some("A".into()),
            })
            .await;
        assert!(matches!(bad, Err(DeskError::Invalid(_))));
        // A group is one the sidebar has.
        let nowhere = d
            .create_board(NewBoard {
                name: "Atlas".into(),
                group: Some("g-atlas".into()),
                prefix: None,
            })
            .await;
        assert!(matches!(nowhere, Err(DeskError::Invalid(_))));
        d.change_layout(|l| l.create("g-atlas", "Atlas", &[], None))
            .await
            .unwrap();
        let atlas = d
            .create_board(NewBoard {
                name: "Atlas".into(),
                group: Some("g-atlas".into()),
                prefix: None,
            })
            .await
            .unwrap();
        assert_eq!(atlas.group.as_deref(), Some("g-atlas"));
        // A later layout write keeps it.
        d.change_layout(|l| l.edit("g-atlas", Some("Atlas Co"), None))
            .await
            .unwrap();
        let boards = d.boards().await.unwrap();
        let kept = boards.iter().find(|x| x.id == atlas.id).unwrap();
        assert_eq!(kept.group.as_deref(), Some("g-atlas"));
    })
    .await;
}

#[tokio::test]
async fn a_drop_lands_where_it_was_dropped_and_needs_never_loop() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = board(&d, "Acme").await;
        let a = ticket(&d, b.id, "A", TicketState::Todo).await.ticket.id;
        let c = ticket(&d, b.id, "C", TicketState::Todo).await.ticket.id;
        let x = ticket(&d, b.id, "X", TicketState::Backlog).await.ticket.id;
        // X goes into To do between A and C.
        d.move_ticket(
            x,
            Move {
                state: TicketState::Todo,
                before: Some(a),
                after: None,
            },
            &By::you(),
        )
        .await
        .unwrap();
        let order = |d: Desk| async move {
            let mut todo = d
                .tickets(
                    b.id,
                    &TicketQuery {
                        state: Some(TicketState::Todo),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            todo.sort_by(|p, q| p.ticket.rank.total_cmp(&q.ticket.rank));
            todo.into_iter().map(|v| v.ticket.title).collect::<Vec<_>>()
        };
        assert_eq!(order(d.clone()).await, ["A", "X", "C"]);
        // To the top, by the card below it.
        d.move_ticket(
            c,
            Move {
                state: TicketState::Todo,
                before: None,
                after: Some(a),
            },
            &By::you(),
        )
        .await
        .unwrap();
        assert_eq!(order(d.clone()).await, ["C", "A", "X"]);
        // Ranks with no room between them are spaced out first.
        for id in [a, x] {
            let mut t = d.0.store.ticket(id).await.unwrap().unwrap();
            t.rank = 7.0;
            d.0.store.put_ticket(&t).await.unwrap();
        }
        d.move_ticket(
            c,
            Move {
                state: TicketState::Todo,
                before: Some(a),
                after: None,
            },
            &By::you(),
        )
        .await
        .unwrap();
        assert_eq!(order(d.clone()).await, ["A", "C", "X"]);

        // A needs X: blocked until X is done.
        let v = d.need_ticket(a, x, true, &By::you()).await.unwrap();
        assert!(!v.ready);
        assert_eq!(v.blockers, vec![x]);
        let refused = d.need_ticket(x, a, true, &By::you()).await;
        assert!(
            matches!(&refused, Err(DeskError::Conflict(m)) if m == "ACM-1 already needs ACM-3."),
            "{refused:?}"
        );
        assert!(d.need_ticket(a, a, true, &By::you()).await.is_err());
        d.move_ticket(x, to(TicketState::Done), &By::you())
            .await
            .unwrap();
        let v = d.ticket(a).await.unwrap();
        assert!(v.ready);
        assert!(v.blockers.is_empty());
        // Dropping the need lets it loop the other way.
        d.need_ticket(a, x, false, &By::you()).await.unwrap();
        d.need_ticket(x, a, true, &By::you()).await.unwrap();

        // The history says it all, by key.
        let said: Vec<String> = history(&d, a, 3)
            .await
            .into_iter()
            .map(|e| e.text)
            .collect();
        assert!(
            said.contains(&"made ACM-1 need ACM-3".to_string()),
            "{said:?}"
        );
        assert!(said.contains(&"created ACM-1 “A”".to_string()), "{said:?}");
    })
    .await;
}

#[tokio::test]
async fn assigning_a_session_links_it_sets_its_goal_tells_it_and_stamps_its_spend() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = board(&d, "Acme").await;
        let need = ticket(&d, b.id, "Set up the database", TicketState::Done).await;
        let t = d
            .create_ticket(
                NewTicket {
                    board: b.id,
                    title: "Fix the login".into(),
                    body: "Sign-in fails on Safari.".into(),
                    state: Some(TicketState::Todo),
                    needs: vec![need.ticket.id],
                },
                &By::you(),
            )
            .await
            .unwrap();
        let s = session(&d, dir.path(), "acme-web", "").await;
        let before = spend(&d, s.id, Utc::now() - chrono::Duration::seconds(5)).await;
        let v = d
            .assign_ticket(t.ticket.id, Some(AssignTo::Session(s.id)), &By::you())
            .await
            .unwrap();
        assert_eq!(v.ticket.state, TicketState::Doing);
        assert_eq!(v.ticket.session(), Some(s.id));
        assert!(v
            .ticket
            .links
            .contains(&crate::board::Link::Session { session: s.id }));
        assert_eq!(d.view(s.id).unwrap().goal, "ACM-2 · Fix the login");
        // Told on its line, from the person, with the ticket's words.
        let thread = d.line_thread(s.id, None, None).await.unwrap();
        let told = thread.iter().find(|m| m.kind == LineKind::Tell).unwrap();
        assert!(told.text.starts_with("ACM-2 is yours: Fix the login"));
        assert!(told.text.contains("Sign-in fails on Safari."));
        assert!(told.text.contains("ACM-1 (Done)"));
        // Its spend from then on is the ticket's, and only that.
        let after = spend(&d, s.id, Utc::now() + chrono::Duration::seconds(1)).await;
        let rows =
            d.0.store
                .ledger_rows(
                    Utc::now() - chrono::Duration::hours(1),
                    Utc::now() + chrono::Duration::hours(1),
                    &Filter {
                        session: Some(s.id),
                        ..Default::default()
                    },
                    None,
                )
                .await
                .unwrap();
        let stamp = |k: &str| rows.iter().find(|r| r.key == k).unwrap().ticket;
        assert_eq!(stamp(&after), Some(t.ticket.id));
        assert_eq!(stamp(&before), None);
        // A call made before the assignment and read only now isn't stamped.
        let late = spend(&d, s.id, Utc::now() - chrono::Duration::minutes(5)).await;
        let rows =
            d.0.store
                .ledger_rows(
                    Utc::now() - chrono::Duration::hours(1),
                    Utc::now() + chrono::Duration::hours(1),
                    &Filter {
                        session: Some(s.id),
                        ..Default::default()
                    },
                    None,
                )
                .await
                .unwrap();
        assert_eq!(rows.iter().find(|r| r.key == late).unwrap().ticket, None);

        // A second ticket assigned later takes the session's spend from then.
        let next = ticket(&d, b.id, "Then the logout", TicketState::Todo).await;
        d.assign_ticket(next.ticket.id, Some(AssignTo::Session(s.id)), &By::you())
            .await
            .unwrap();
        assert_eq!(
            d.view(s.id).unwrap().goal,
            "ACM-2 · Fix the login",
            "a goal it had stays"
        );
        let later = spend(&d, s.id, Utc::now() + chrono::Duration::seconds(2)).await;
        let rows =
            d.0.store
                .ledger_rows(
                    Utc::now() - chrono::Duration::hours(1),
                    Utc::now() + chrono::Duration::hours(1),
                    &Filter {
                        session: Some(s.id),
                        ..Default::default()
                    },
                    None,
                )
                .await
                .unwrap();
        assert_eq!(
            rows.iter().find(|r| r.key == later).unwrap().ticket,
            Some(next.ticket.id)
        );

        // History by ticket and by session.
        let events = history(&d, t.ticket.id, 2).await;
        assert_eq!(events[0].kind, Kind::TicketAssigned);
        assert_eq!(events[0].text, "gave ACM-2 to acme-web");
        let of = d.history_of(&s.id.to_string(), None, 50).await.unwrap();
        assert!(of
            .events
            .iter()
            .any(|e| e.kind == Kind::TicketAssigned && e.ticket == Some(next.ticket.id)));

        // Taken off it: no one's, and its spend is no longer stamped.
        let v = d
            .assign_ticket(next.ticket.id, None, &By::you())
            .await
            .unwrap();
        assert!(v.ticket.assignee.is_none());
    })
    .await;
}

#[tokio::test]
async fn a_new_session_is_started_with_the_ticket_as_its_goal_and_prompt() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        d.change_layout(|l| l.create("g-acme", "Acme", &[], None))
            .await
            .unwrap();
        let b = d
            .create_board(NewBoard {
                name: "Acme".into(),
                group: Some("g-acme".into()),
                prefix: None,
            })
            .await
            .unwrap();
        let t = ticket(&d, b.id, "Fix the login", TicketState::Todo).await;
        let v = d
            .assign_ticket(
                t.ticket.id,
                Some(AssignTo::NewSession(NewSessionFor {
                    path: dir.path().to_string_lossy().into_owned(),
                    engine: Engine::Claude,
                })),
                &By::you(),
            )
            .await
            .unwrap();
        let s = d.view(v.ticket.session().unwrap()).unwrap();
        assert_eq!(s.name, "ACM-1");
        assert_eq!(
            s.group.map(|g| g.id).as_deref(),
            Some("g-acme"),
            "where its board is"
        );
        assert_eq!(s.goal, "ACM-1 · Fix the login");
        assert_eq!(v.ticket.state, TicketState::Doing);
        let thread = d.line_thread(s.id, None, None).await.unwrap();
        assert!(
            thread.iter().all(|m| m.kind != LineKind::Tell),
            "its prompt is the ticket"
        );
    })
    .await;
}

struct Agents;

#[async_trait::async_trait]
impl Crew for Agents {
    fn overseer(&self) -> Option<Overseer> {
        None
    }
    fn agents(&self) -> Vec<CrewAgent> {
        vec![CrewAgent {
            id: "scout".into(),
            name: "Scout".into(),
            live: None,
        }]
    }
    fn midnight(&self) -> chrono::DateTime<Utc> {
        Utc::now()
    }
    async fn accept(&self, _: &Suggestion) -> Result<(), String> {
        Ok(())
    }
    async fn assign(&self, agent: &str, task: &str) -> Result<String, String> {
        assert!(task.starts_with("ACM-1 is yours"));
        Ok(format!("gave {agent}"))
    }
}

#[tokio::test]
async fn an_agent_is_given_the_ticket_through_the_crew() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = board(&d, "Acme").await;
        let t = ticket(&d, b.id, "Fix the login", TicketState::Todo).await;
        let refused = d
            .assign_ticket(
                t.ticket.id,
                Some(AssignTo::Agent("scout".into())),
                &By::you(),
            )
            .await;
        assert!(
            matches!(refused, Err(DeskError::Conflict(_))),
            "no crew yet"
        );
        let crew: std::sync::Arc<dyn Crew> = std::sync::Arc::new(Agents);
        d.answer_lines_with(&crew);
        let v = d
            .assign_ticket(
                t.ticket.id,
                Some(AssignTo::Agent("scout".into())),
                &By::you(),
            )
            .await
            .unwrap();
        assert_eq!(v.ticket.assignee, Some(Party::agent("scout", "Scout")));
        assert_eq!(v.ticket.state, TicketState::Doing);
        let nobody = d
            .assign_ticket(
                t.ticket.id,
                Some(AssignTo::Agent("ghost".into())),
                &By::you(),
            )
            .await;
        assert!(matches!(nobody, Err(DeskError::Invalid(_))));
    })
    .await;
}

#[tokio::test]
async fn a_session_moves_and_comments_on_its_own_ticket_and_no_one_elses() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = board(&d, "Acme").await;
        let mine = ticket(&d, b.id, "Fix the login", TicketState::Todo).await;
        let theirs = ticket(&d, b.id, "Fix the logout", TicketState::Todo).await;
        let s = session(&d, dir.path(), "acme-web", "").await;
        let other = session(&d, dir.path(), "acme-api", "").await;
        d.assign_ticket(mine.ticket.id, Some(AssignTo::Session(s.id)), &By::you())
            .await
            .unwrap();
        d.assign_ticket(theirs.ticket.id, Some(AssignTo::Session(other.id)), &By::you())
            .await
            .unwrap();
        let call = |input: serde_json::Value| {
            let d = d.clone();
            async move { d.line_call(s.id, "ticket", &input, None).await }
        };
        let listed = call(json!({"action": "mine"})).await.unwrap();
        assert_eq!(listed["tickets"][0]["key"], "ACM-1");
        let got = call(json!({"action": "get", "key": "acm-2"})).await.unwrap();
        assert_eq!(got["assignee"], "acme-api");
        let moved = call(json!({"action": "update", "state": "review"}))
            .await
            .unwrap();
        assert_eq!(moved["state"], "review");
        assert_eq!(
            call(json!({"action": "update", "state": "dropped"})).await,
            Err("Only the person drops a ticket; tell them why it should go.".into())
        );
        assert_eq!(
            call(json!({"action": "update", "key": "ACM-2", "state": "done"})).await,
            Err("ACM-2 isn't yours: it's acme-api's.".into())
        );
        assert_eq!(
            d.ticket(theirs.ticket.id).await.unwrap().ticket.state,
            TicketState::Doing
        );
        call(json!({"action": "comment", "key": "ACM-2", "text": "the logout shares the cookie code"}))
            .await
            .unwrap();
        let events = history(&d, theirs.ticket.id, 3).await;
        let comment = events
            .iter()
            .find(|e| e.kind == Kind::TicketCommented)
            .unwrap();
        assert_eq!(comment.actor.name, "acme-web");
        assert!(comment.text.contains("the logout shares the cookie code"));
        let made = call(json!({"action": "create", "text": "Add a test\nfor Safari's sign-in"}))
            .await
            .unwrap();
        assert_eq!(made["key"], "ACM-3");
        let v = d.search_tickets("ACM-3").await.unwrap();
        assert_eq!(v[0].ticket.state, TicketState::Backlog);
        assert_eq!(v[0].ticket.created_by.name, "acme-web");
        assert_eq!(v[0].ticket.body, "for Safari's sign-in");
        // Handed on, a ticket the session wrote is its note, not the
        // person's words.
        d.assign_ticket(v[0].ticket.id, Some(AssignTo::Session(other.id)), &By::you())
            .await
            .unwrap();
        let thread = d.line_thread(other.id, None, None).await.unwrap();
        let told = thread
            .iter()
            .find(|m| m.kind == LineKind::Tell && m.text.starts_with("ACM-3"))
            .unwrap();
        assert!(told.text.contains("acme-web added it to the board"));
        assert!(told.text.contains("> for Safari's sign-in"));
        // Another group's board is out of its reach.
        d.change_layout(|l| l.create("g-atlas", "Atlas", &[], None))
            .await
            .unwrap();
        let atlas = d
            .create_board(NewBoard {
                name: "Atlas".into(),
                group: Some("g-atlas".into()),
                prefix: None,
            })
            .await
            .unwrap();
        ticket(&d, atlas.id, "Atlas's own", TicketState::Todo).await;
        assert_eq!(
            call(json!({"action": "get", "key": "ATL-1"})).await,
            Err("No board you can reach has keys starting ATL.".into())
        );
        assert!(call(json!({"action": "create", "key": "ATL", "text": "Sneak in"}))
            .await
            .is_err());
        // The tool is on the line's list.
        assert!(crate::line::TOOLS.iter().any(|t| t.name == "ticket"));
    })
    .await;
}

#[tokio::test]
async fn a_ticket_deleted_while_assigned_tells_its_session_and_keeps_its_key_in_history() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = board(&d, "Acme").await;
        let need = ticket(&d, b.id, "First", TicketState::Todo).await;
        let t = ticket(&d, b.id, "Fix the login", TicketState::Todo).await;
        d.need_ticket(t.ticket.id, need.ticket.id, true, &By::you())
            .await
            .unwrap();
        let s = session(&d, dir.path(), "acme-web", "").await;
        d.assign_ticket(t.ticket.id, Some(AssignTo::Session(s.id)), &By::you())
            .await
            .unwrap();
        d.delete_ticket(t.ticket.id, &By::you()).await.unwrap();
        assert!(matches!(
            d.ticket(t.ticket.id).await,
            Err(DeskError::NotFound)
        ));
        assert!(d.0.store.ticket_edges().await.unwrap().is_empty());
        let thread = d.line_thread(s.id, None, None).await.unwrap();
        assert!(thread.iter().any(|m| m
            .text
            .contains("ACM-2 “Fix the login” was deleted from the board")));
        let events = history(&d, t.ticket.id, 4).await;
        assert_eq!(events[0].kind, Kind::TicketDeleted);
        assert_eq!(events[0].text, "deleted ACM-2 “Fix the login”");
    })
    .await;
}

#[tokio::test]
async fn a_ticket_follows_its_sessions_pull_request_forward() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = board(&d, "Acme").await;
        let t = ticket(&d, b.id, "Fix the login", TicketState::Todo).await;
        let s = session(&d, dir.path(), "acme-web", "").await;
        let lone = session(&d, dir.path(), "scratch", "").await;
        assert_eq!(
            d.ticket_pr(
                lone.id,
                Pr {
                    number: 7,
                    url: "https://example.com/pull/7".into(),
                    state: PrState::Opened
                }
            )
            .await
            .unwrap(),
            None,
            "no ticket, nothing to follow"
        );
        d.assign_ticket(t.ticket.id, Some(AssignTo::Session(s.id)), &By::you())
            .await
            .unwrap();
        let pr = |state| Pr {
            number: 41,
            url: "https://example.com/pull/41".into(),
            state,
        };
        let v = d
            .ticket_pr(s.id, pr(PrState::Opened))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v.ticket.state, TicketState::Review);
        assert!(v.ticket.links.contains(&crate::board::Link::Pr {
            number: 41,
            url: "https://example.com/pull/41".into()
        }));
        let v = d
            .ticket_pr(s.id, pr(PrState::Merged))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v.ticket.state, TicketState::Done);
        let events = history(&d, t.ticket.id, 4).await;
        assert_eq!(events[0].actor, Party::desk());
        assert_eq!(
            events[0].text,
            "moved ACM-1 to Done: pull request #41 merged"
        );
        // Never back: a reopened PR leaves it Done.
        let v = d
            .ticket_pr(s.id, pr(PrState::Opened))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v.ticket.state, TicketState::Done);
    })
    .await;
}

#[tokio::test]
async fn a_snoozed_ticket_wakes_when_it_changes_and_a_pin_changes_nothing() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = board(&d, "Acme").await;
        let t = ticket(&d, b.id, "Fix the login", TicketState::Todo).await;
        let v = d
            .edit_ticket(
                t.ticket.id,
                TicketPatch {
                    pinned: Some(true),
                    snoozed_until: Some(Some(Utc::now() + chrono::Duration::days(1))),
                    ..Default::default()
                },
                &By::you(),
            )
            .await
            .unwrap();
        assert!(v.snoozed && v.ticket.pinned);
        assert_eq!(v.ticket.updated_at, t.ticket.updated_at, "not a change");
        tokio::time::sleep(Duration::from_millis(5)).await;
        let v = d
            .edit_ticket(
                t.ticket.id,
                TicketPatch {
                    body: Some("It fails on Safari only.".into()),
                    ..Default::default()
                },
                &By::you(),
            )
            .await
            .unwrap();
        assert!(!v.snoozed, "it changed: it wakes");
        let v = d
            .edit_ticket(
                t.ticket.id,
                TicketPatch {
                    snoozed_until: Some(None),
                    ..Default::default()
                },
                &By::you(),
            )
            .await
            .unwrap();
        assert_eq!(v.ticket.snoozed_until, None);
        // A dropped ticket settles at once.
        let v = d
            .move_ticket(t.ticket.id, to(TicketState::Dropped), &By::you())
            .await
            .unwrap();
        assert!(v.settled);
    })
    .await;
}

#[tokio::test]
async fn tickets_are_found_by_their_words_and_by_their_key() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = board(&d, "Acme").await;
        ticket(&d, b.id, "Fix the login on Safari", TicketState::Todo).await;
        ticket(&d, b.id, "Write the release notes", TicketState::Todo).await;
        let found = d.search_tickets("saf log").await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key.as_deref(), Some("ACM-1"));
        let found = d.search_tickets("acm-2").await.unwrap();
        assert_eq!(found[0].ticket.title, "Write the release notes");
        assert!(d.search_tickets("--").await.unwrap().is_empty());
        let listed = d
            .tickets(
                b.id,
                &TicketQuery {
                    q: Some("release".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
    })
    .await;
}

#[tokio::test]
async fn forgetting_a_session_unassigns_its_tickets_and_keeps_their_history() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let b = board(&d, "Acme").await;
        let t = ticket(&d, b.id, "Fix the login", TicketState::Todo).await;
        let s = session(&d, dir.path(), "acme-web", "").await;
        d.assign_ticket(t.ticket.id, Some(AssignTo::Session(s.id)), &By::you())
            .await
            .unwrap();
        history(&d, t.ticket.id, 2).await;
        assert_eq!(d.remove(s.id, Default::default()).await.unwrap().removed, 1);
        // The store forgets it behind.
        let mut v = d.ticket(t.ticket.id).await.unwrap();
        for _ in 0..200 {
            if v.ticket.assignee.is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            v = d.ticket(t.ticket.id).await.unwrap();
        }
        assert!(v.ticket.assignee.is_none());
        assert!(v.ticket.links.is_empty());
        let events = history(&d, t.ticket.id, 2).await;
        assert!(events.iter().any(|e| e.text == "gave ACM-1 to acme-web"));
    })
    .await;
}
