//! Moves end to end in the desk (boss-gaps W8): what a preview says, that
//! a move is made only as previewed and once, that the note goes only as
//! the person sent it, and that nothing written before a move reaches the
//! new group's boss: activity, the ledger, lines and session memory.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use parking_lot::Mutex;
use uuid::Uuid;
use workforce_core::budget::{BudgetScope, Period, Unit};
use workforce_postgres::test_support::with_test_pool;

use super::tests::{new_session, options};
use super::*;
use crate::board::TicketState;
use crate::charge::How;
use crate::crew::{self, Party};
use crate::ledger::{Billing, Priced, Spend, Tokens};
use crate::ledger_store::Kept;
use crate::line::{Asked, LineKind, LineMessage};
use crate::memory::record::{Asker, Exchange};
use crate::memory_store::RecordWrite;
use crate::moves::{Choices, Effect, Handover, Item};
use crate::scope::Scope;
use crate::store::build_pool;

fn boss(agent: &str, name: &str, scope: Scope, level: &str) -> Boss {
    Boss {
        agent: agent.into(),
        name: name.into(),
        scope,
        level: level.into(),
    }
}

fn acme() -> Scope {
    Scope::Group("acme".into())
}

fn atlas() -> Scope {
    Scope::Group("atlas".into())
}

/// The hub as the moves see it: what it was asked to draft, what it
/// delivered, and the bosses it moved.
#[derive(Default)]
struct Hub {
    desk: Mutex<Option<Desk>>,
    asked: Mutex<Vec<Handover>>,
    delivered: Mutex<Vec<Handover>>,
    moved: Mutex<Vec<(String, Option<String>, bool)>>,
}

#[async_trait::async_trait]
impl crew::Crew for Hub {
    fn overseer(&self) -> Option<crew::Overseer> {
        None
    }
    fn agents(&self) -> Vec<crew::CrewAgent> {
        Vec::new()
    }
    fn midnight(&self) -> chrono::DateTime<Utc> {
        Utc::now()
    }
    async fn accept(&self, _: &crew::Suggestion) -> Result<(), String> {
        Ok(())
    }
    async fn ask_handover(&self, h: &Handover, _facts: &str) {
        self.asked.lock().push(h.clone());
    }
    async fn deliver_handover(&self, h: &Handover) -> Result<(), String> {
        self.delivered.lock().push(h.clone());
        Ok(())
    }
    async fn move_boss(&self, agent: &str, to: Option<&str>, replace: bool) -> Result<(), String> {
        self.moved
            .lock()
            .push((agent.to_string(), to.map(str::to_string), replace));
        let d = self.desk.lock().clone().expect("a desk");
        // As the hub does: the config is written, the bosses read again,
        // and the group it left opened to the main boss from now on.
        let mut bosses: Vec<Boss> = d
            .bosses()
            .into_iter()
            .filter(|b| b.agent != agent && b.scope.group() != to)
            .collect();
        let left = d.scope_of_boss(agent);
        bosses.push(boss(
            agent,
            "Sarah",
            to.map_or(Scope::Ungrouped, |g| Scope::Group(g.into())),
            "act",
        ));
        d.set_bosses(bosses);
        if let Some(Scope::Group(g)) = left {
            d.unbound_group(&g).await;
        }
        Ok(())
    }
}

/// A desk with Acme (Sarah) and Atlas (Mia) as boundaries, Desk the main
/// boss, a redactor for notes, and the hub above.
async fn desk_with_hub(
    pool: workforce_postgres::PostgresPool,
    dir: &std::path::Path,
) -> (Desk, Arc<Hub>) {
    crate::store::DeskStore::migrate(&pool).await.unwrap();
    let d = Desk::start(
        crate::store::DeskStore::from_pool(pool),
        options(dir),
        Floor {
            redactor: Some(Arc::new(crate::memory::record::tests::Plain)),
            ..Default::default()
        },
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    d.count_budgets_in(chrono_tz::UTC);
    d.change_layout(|l| {
        l.create("acme", "Acme", &[], None)?
            .create("atlas", "Atlas", &[], None)?
            .create("infra", "Infra", &[], None)
    })
    .await
    .unwrap();
    d.bound_groups(["acme".to_string(), "atlas".to_string()])
        .await;
    d.set_bosses(vec![
        boss("desk", "Desk", Scope::Ungrouped, "act"),
        boss("sarah", "Sarah", acme(), "act"),
        boss("mia", "Mia", atlas(), "act"),
    ]);
    let hub = Arc::new(Hub::default());
    *hub.desk.lock() = Some(d.clone());
    let crew: Arc<dyn crew::Crew> = hub.clone();
    d.answer_lines_with(&crew);
    // Kept alive by the test: the desk holds it weakly.
    std::mem::forget(crew);
    for _ in 0..200 {
        if d.charges_loaded() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    (d, hub)
}

fn kept_at(at: chrono::DateTime<Utc>, usd: f64) -> Kept {
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

fn exchange(key: &str, at: chrono::DateTime<Utc>, ask: &str) -> Exchange {
    Exchange {
        key: key.into(),
        at,
        ended: Some(at),
        asker: Asker::Person,
        ask: ask.into(),
        answer: "Done.".into(),
        steps: Vec::new(),
        files: Default::default(),
        commands: Vec::new(),
        errors: Vec::new(),
        compaction: false,
        redacted: 0,
        flagged: Default::default(),
    }
}

async fn said(d: &Desk, session: Uuid, x: &Exchange) {
    d.store()
        .record_write(&RecordWrite {
            session,
            engine: crate::engine::Engine::Claude,
            source: "/records/t.jsonl",
            cursor: &Default::default(),
            rows: &[],
            closed: std::slice::from_ref(x),
            open: None,
            project: "/dev/acme-api",
            folder: "/dev/acme-api",
            name: "acme-api",
            cli_session_id: None,
        })
        .await
        .unwrap();
}

async fn exchange_group(d: &Desk, key: &str) -> Option<String> {
    let client = d.store().pool().get().await.unwrap();
    client
        .query_one(
            "SELECT group_id FROM memory_exchanges WHERE key = $1",
            &[&key],
        )
        .await
        .unwrap()
        .get(0)
}

/// An Acme session Sarah runs, on ACM-1, with a question to her and spend.
struct Acme {
    session: Uuid,
    ticket: Uuid,
    ask: Uuid,
    budget: Uuid,
}

async fn acme_session(d: &Desk, dir: &std::path::Path) -> Acme {
    let s = new_session(d, dir).await;
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
                title: "Fix the Acme checkout".into(),
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
        "Which Acme port?".into(),
        1,
    );
    ask.asked = Some(Asked::Open);
    ask.group = Some("acme".into());
    d.store().put_line(&ask).await.unwrap();
    d.take_on(
        s.id,
        &Party::manager("sarah", "Sarah"),
        How::Handed,
        &acme(),
        &By::you(),
    )
    .await
    .unwrap();
    let budget = d
        .put_budget(
            NewBudget {
                scope: BudgetScope::Session(s.id),
                group: None,
                period: Period::Day,
                unit: Unit::Usd,
                amount: 10.0,
                warn_at: None,
                on_exceed: None,
            },
            By::you(),
        )
        .await
        .unwrap();
    Acme {
        session: s.id,
        ticket: t.ticket.id,
        ask: ask.id,
        budget: budget.id,
    }
}

fn to(group: &str, item: Item) -> MoveAsk {
    MoveAsk {
        item,
        to: Some(group.into()),
        choices: Choices::default(),
    }
}

fn kinds(p: &MovePreview) -> Vec<&'static str> {
    p.effects
        .iter()
        .map(|e| match e.effect {
            Effect::ChargeTo { .. } => "charge_to",
            Effect::Release => "release",
            Effect::StaysUnassigned { .. } => "stays_unassigned",
            Effect::TicketGoes { .. } => "ticket_goes",
            Effect::LeavesPlan { .. } => "leaves_plan",
            Effect::Unassigned { .. } => "unassigned",
            Effect::LedgerStays { .. } => "ledger_stays",
            Effect::BudgetFollows { .. } => "budget_follows",
            Effect::AsksWithdrawn { .. } => "asks_withdrawn",
            Effect::Replaces { .. } => "replaces",
            Effect::AsksToReplace { .. } => "asks_to_replace",
            Effect::PassesToMain { .. } => "passes_to_main",
            Effect::LeftWithNoBoss { .. } => "left_with_no_boss",
            Effect::MemoryStays => "memory_stays",
            Effect::Handover { .. } => "handover",
            Effect::Tell => "tell",
            Effect::Refused { .. } => "refused",
            Effect::NeedsKept { .. } => "needs_kept",
        })
        .collect()
}

async fn events_seen_by(d: &Desk, scope: &Scope) -> Vec<crew::Event> {
    // Events are kept a moment after they are noted.
    tokio::time::sleep(Duration::from_millis(300)).await;
    d.store()
        .events_since(
            Utc::now() - chrono::Duration::hours(1),
            &[],
            500,
            Some(&d.seen_by(scope)),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn a_session_moved_to_another_boss_takes_only_the_note_and_leaves_everything_written_before()
{
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, hub) = desk_with_hub(pool, dir.path()).await;
        let a = acme_session(&d, dir.path()).await;
        // Said and spent in Acme, after the session was put there.
        tokio::time::sleep(Duration::from_millis(5)).await;
        let before = crew::now();
        tokio::time::sleep(Duration::from_millis(5)).await;
        d.store()
            .ledger_write(
                a.session,
                crate::engine::Engine::Claude,
                None,
                &[kept_at(before, 2.5)],
            )
            .await
            .unwrap();
        said(
            &d,
            a.session,
            &exchange("x-before", before, "Acme canary: the old pricing"),
        )
        .await;

        let p = d
            .preview_move(to("atlas", Item::Session(a.session)))
            .await
            .unwrap();
        assert_eq!(
            kinds(&p),
            [
                "stays_unassigned",
                "asks_withdrawn",
                "charge_to",
                "ledger_stays",
                "budget_follows",
                "handover",
                "tell"
            ]
        );
        assert!(p
            .effects
            .iter()
            .any(|e| e.effect == Effect::BudgetFollows { budget: a.budget }));
        assert!(p.blocked.is_none());
        let note = p.handover.clone().expect("a note for Mia");
        assert!(note.drafting, "Sarah is asked to write it");
        assert_eq!(note.handover.to_boss.as_deref(), Some("mia"));
        assert!(
            note.handover.note.contains("fix the race"),
            "{}",
            note.handover.note
        );
        // Asked of Sarah; nothing reaches Mia before the move.
        for _ in 0..100 {
            if !hub.asked.lock().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(hub.asked.lock().len(), 1);
        assert!(hub.delivered.lock().is_empty());
        // Only Sarah writes it.
        let id = note.handover.id;
        assert!(matches!(
            d.handover_drafted(id, &Party::manager("mia", "Mia"), "Mia's words")
                .await,
            Err(DeskError::NotFound)
        ));
        let drafted = d
            .handover_drafted(
                id,
                &Party::manager("sarah", "Sarah"),
                "Sarah's draft: port 8080",
            )
            .await
            .unwrap();
        assert_eq!(drafted.drafted_by.id, "sarah");
        assert!(!d.handover_view(id).await.unwrap().drafting);
        assert!(hub.delivered.lock().is_empty(), "still nothing delivered");

        let done = d
            .commit_move(MoveCommit {
                preview: p.id,
                choices: Choices::default(),
                note: Some("Mia: port 8080, ticket stays with Acme.".into()),
                notes: vec![],
            })
            .await
            .unwrap();
        assert!(done.problems.is_empty(), "{:?}", done.problems);
        // Exactly what the person sent, once.
        let delivered = hub.delivered.lock().clone();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].note, "Mia: port 8080, ticket stays with Acme.");
        assert!(delivered[0].edited);
        assert!(matches!(
            d.commit_move(MoveCommit {
                preview: p.id,
                choices: Choices::default(),
                note: Some("again".into()),
                notes: vec![],
            })
            .await,
            Err(DeskError::Conflict(_))
        ));
        assert_eq!(hub.delivered.lock().len(), 1, "made once");

        // What it left behind.
        let t = d.ticket(a.ticket).await.unwrap();
        assert_eq!(
            (t.ticket.state, t.ticket.assignee),
            (TicketState::Todo, None)
        );
        assert_eq!(t.key.as_deref(), Some("ACM-1"));
        let ask = d.store().line_message(a.ask).await.unwrap().unwrap();
        assert_eq!(ask.asked, Some(Asked::Withdrawn));
        assert_eq!(ask.group.as_deref(), Some("acme"), "its line stays Acme's");
        let charge = d.charge_of(a.session).unwrap();
        assert!(charge.live());
        assert_eq!(
            (charge.boss.as_str(), charge.group.as_deref()),
            ("mia", Some("atlas"))
        );
        assert!(d
            .budget_list()
            .iter()
            .any(|b| b.id == a.budget && b.scope == BudgetScope::Session(a.session)));

        // Rows about before the move, read after it, are Acme's still.
        d.store()
            .ledger_write(
                a.session,
                crate::engine::Engine::Claude,
                None,
                &[
                    kept_at(before + chrono::Duration::milliseconds(1), 1.0),
                    kept_at(crew::now(), 4.0),
                ],
            )
            .await
            .unwrap();
        said(
            &d,
            a.session,
            &exchange(
                "x-late",
                before + chrono::Duration::milliseconds(1),
                "Acme canary: late read",
            ),
        )
        .await;
        said(
            &d,
            a.session,
            &exchange("x-after", crew::now(), "the Atlas work"),
        )
        .await;
        assert_eq!(
            exchange_group(&d, "x-before").await.as_deref(),
            Some("acme")
        );
        assert_eq!(exchange_group(&d, "x-late").await.as_deref(), Some("acme"));
        assert_eq!(
            exchange_group(&d, "x-after").await.as_deref(),
            Some("atlas")
        );
        // Session memory as Atlas's boss, and an Atlas session, reads none
        // of it; Acme's boss still does.
        let recall = crate::service::memory_recall::Recall {
            q: "canary".into(),
            limit: 10,
            tool: "test",
            ..Default::default()
        };
        let atlas_session = crate::memory::reach::Reader::Session {
            id: Uuid::new_v4(),
            group: Some("atlas".into()),
            project: "/dev/acme-api".into(),
            work: vec![],
        };
        for reader in [
            crate::memory::reach::Reader::Overseer(d.seen_by(&atlas())),
            atlas_session,
        ] {
            let found = d.memory_recall(&reader, &recall).await.unwrap();
            assert!(
                found.hits.is_empty(),
                "{:?} read {:?}",
                reader.label(),
                found.hits
            );
        }
        let acme_found = d
            .memory_recall(
                &crate::memory::reach::Reader::Overseer(d.seen_by(&acme())),
                &recall,
            )
            .await
            .unwrap();
        assert_eq!(
            acme_found.hits.len(),
            2,
            "Acme's boss reads what was said in Acme"
        );
        let page = d
            .ledger_page(
                before - chrono::Duration::minutes(1),
                Utc::now() + chrono::Duration::minutes(1),
                &crate::ledger_store::Filter {
                    seen: Some(d.seen_by(&atlas())),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let usd: Vec<f64> = page.rows.iter().filter_map(|r| r.usd).collect();
        assert_eq!(usd, [4.0], "Atlas's boss reads only what was spent there");

        // Atlas's boss's activity has the move and nothing before it.
        let atlas_rows = events_seen_by(&d, &atlas()).await;
        assert!(atlas_rows
            .iter()
            .any(|e| e.kind == crew::Kind::Moved && e.text.contains("here")));
        for e in &atlas_rows {
            assert!(!e.text.contains("Acme"), "{e:?}");
            assert!(!e.text.contains("ACM-1"), "{e:?}");
            assert!(!e.text.contains("Fix the Acme checkout"), "{e:?}");
        }
        let acme_rows = events_seen_by(&d, &acme()).await;
        assert!(acme_rows
            .iter()
            .any(|e| e.kind == crew::Kind::Moved && e.text.contains("to another group")));
        assert!(
            acme_rows.iter().all(|e| !e.text.contains("Atlas")),
            "Acme's side never names where it went"
        );
        // The session is told on its line, in Atlas.
        let thread = d.line_thread(a.session, None, None).await.unwrap();
        let told = thread
            .iter()
            .find(|m| m.text.starts_with("You were moved to Atlas"))
            .expect("told");
        assert_eq!(told.group.as_deref(), Some("atlas"));
    })
    .await;
}

#[tokio::test]
async fn a_move_is_made_only_as_previewed_with_each_note_as_sent_and_no_secret() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, hub) = desk_with_hub(pool, dir.path()).await;
        let a = acme_session(&d, dir.path()).await;
        let p = d
            .preview_move(to("atlas", Item::Session(a.session)))
            .await
            .unwrap();
        let commit = |choices: Choices, note: Option<&str>| MoveCommit {
            preview: p.id,
            choices,
            note: note.map(str::to_string),
            notes: vec![],
        };
        // Other choices than previewed: what it does would differ.
        let goes = Choices {
            release: true,
            ..Choices::default()
        };
        assert!(matches!(
            d.commit_move(commit(goes, Some("ok"))).await,
            Err(DeskError::Conflict(_))
        ));
        assert!(matches!(
            d.commit_move(commit(Choices::default(), None)).await,
            Err(DeskError::Invalid(_))
        ));
        assert!(matches!(
            d.commit_move(commit(Choices::default(), Some(" "))).await,
            Err(DeskError::Invalid(_))
        ));
        let long = "x".repeat(crate::moves::MAX_NOTE + 1);
        assert!(matches!(
            d.commit_move(commit(Choices::default(), Some(&long))).await,
            Err(DeskError::Invalid(_))
        ));
        let secret = format!("the key is {}{}", "SECRET-", "acmeprod");
        match d
            .commit_move(commit(Choices::default(), Some(&secret)))
            .await
        {
            Err(DeskError::Invalid(why)) => assert!(why.contains("secret"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert!(
            d.handover_drafted(
                p.handover.as_ref().unwrap().handover.id,
                &Party::manager("sarah", "Sarah"),
                &secret
            )
            .await
            .is_err(),
            "nor from the boss"
        );
        // The ticket changed since: what the move does changed.
        d.move_ticket(
            a.ticket,
            Move {
                state: TicketState::Done,
                before: None,
                after: None,
            },
            &By::you(),
        )
        .await
        .unwrap();
        match d.commit_move(commit(Choices::default(), Some("ok"))).await {
            Err(DeskError::Conflict(why)) => assert!(why.contains("look at it again"), "{why}"),
            other => panic!("{other:?}"),
        }
        // Nothing moved, and nothing was delivered.
        assert_eq!(
            d.view(a.session).unwrap().group.map(|g| g.id).as_deref(),
            Some("acme")
        );
        assert_eq!(d.charge_of(a.session).unwrap().boss, "sarah");
        assert!(hub.delivered.lock().is_empty());
        // A preview that ran out is refused.
        let p2 = d
            .preview_move(to("atlas", Item::Session(a.session)))
            .await
            .unwrap();
        let client = d.store().pool().get().await.unwrap();
        client
            .execute(
                "UPDATE moves SET expires_at = now() - interval '1 second' WHERE id = $1",
                &[&p2.id],
            )
            .await
            .unwrap();
        assert!(matches!(
            d.commit_move(MoveCommit {
                preview: p2.id,
                choices: Choices::default(),
                note: Some("ok".into()),
                notes: vec![],
            })
            .await,
            Err(DeskError::Conflict(_))
        ));
    })
    .await;
}

#[tokio::test]
async fn a_boss_that_only_watches_isnt_asked_and_desks_facts_note_stands() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, hub) = desk_with_hub(pool, dir.path()).await;
        let a = acme_session(&d, dir.path()).await;
        d.set_bosses(vec![
            boss("desk", "Desk", Scope::Ungrouped, "act"),
            boss("sarah", "Sarah", acme(), "watch"),
            boss("mia", "Mia", atlas(), "act"),
        ]);
        let p = d
            .preview_move(to("atlas", Item::Session(a.session)))
            .await
            .unwrap();
        let note = p.handover.unwrap();
        assert!(!note.drafting);
        assert!(note.handover.asked_until.is_none());
        assert_eq!(note.handover.drafted_by, Party::desk());
        assert!(note.handover.note.starts_with("Handing over auth-fix."));
        assert!(note.handover.note.contains("Ticket: ACM-1"));
        assert!(note
            .handover
            .note
            .contains("Open questions: Which Acme port?"));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(hub.asked.lock().is_empty());
        assert!(
            d.handover_drafted(note.handover.id, &Party::manager("sarah", "Sarah"), "late")
                .await
                .is_err(),
            "not asked, so never written by the boss"
        );
        // Sent unchanged: not edited.
        d.commit_move(MoveCommit {
            preview: p.id,
            choices: Choices::default(),
            note: Some(note.handover.note.clone()),
            notes: vec![],
        })
        .await
        .unwrap();
        let delivered = hub.delivered.lock().clone();
        assert_eq!(delivered.len(), 1);
        assert!(!delivered[0].edited);
    })
    .await;
}

#[tokio::test]
async fn a_ticket_that_goes_along_takes_a_new_key_and_its_history_stays() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, hub) = desk_with_hub(pool, dir.path()).await;
        let a = acme_session(&d, dir.path()).await;
        d.create_board(NewBoard {
            name: "Atlas".into(),
            group: Some("atlas".into()),
            prefix: Some("ATL".into()),
        })
        .await
        .unwrap();
        let choices = Choices {
            ticket_goes: true,
            ..Choices::default()
        };
        let p = d
            .preview_move(MoveAsk {
                item: Item::Session(a.session),
                to: Some("atlas".into()),
                choices,
            })
            .await
            .unwrap();
        assert!(kinds(&p).contains(&"ticket_goes"));
        d.commit_move(MoveCommit {
            preview: p.id,
            choices,
            note: Some("It comes with its ticket.".into()),
            notes: vec![],
        })
        .await
        .unwrap();
        let t = d.ticket(a.ticket).await.unwrap();
        assert_eq!(t.key.as_deref(), Some("ATL-1"));
        assert_eq!(t.ticket.session(), Some(a.session), "still its session's");
        // Its history before is Acme's; Atlas reads only its arrival.
        let atlas_rows = events_seen_by(&d, &atlas()).await;
        assert!(atlas_rows
            .iter()
            .any(|e| e.text.contains("brought ATL-1 here")));
        assert!(!atlas_rows.iter().any(|e| e.text.contains("ACM-1")));
        let acme_rows = events_seen_by(&d, &acme()).await;
        assert!(acme_rows
            .iter()
            .any(|e| e.text.contains("moved ACM-1 to another group's board")));
        assert_eq!(hub.delivered.lock().len(), 1);
        // A ticket moved on its own, back within no scope it can go to.
        let p = d
            .preview_move(to("infra", Item::Ticket(a.ticket)))
            .await
            .unwrap();
        assert!(p.blocked.is_some(), "Infra has no board");
    })
    .await;
}

#[tokio::test]
async fn a_room_never_crosses_to_another_bosss_scope() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, _hub) = desk_with_hub(pool, dir.path()).await;
        let room = d
            .create_room(crate::rooms::NewRoom {
                name: "Launch".into(),
                members: vec![],
                default_member: None,
                group: None,
                share_with: vec![],
            })
            .await
            .unwrap();
        let p = d
            .preview_move(to("acme", Item::Room(room.id)))
            .await
            .unwrap();
        assert_eq!(kinds(&p), ["refused"]);
        assert!(p.blocked.unwrap().contains("keeps its conversation"));
        assert!(matches!(
            d.update_room(
                room.id,
                crate::rooms::RoomPatch {
                    group: Some(Some("acme".into())),
                    ..Default::default()
                }
            )
            .await,
            Err(DeskError::Conflict(_))
        ));
        // Within the main boss's scope it moves.
        let p = d
            .preview_move(to("infra", Item::Room(room.id)))
            .await
            .unwrap();
        assert!(p.effects.is_empty());
        d.commit_move(MoveCommit {
            preview: p.id,
            choices: Choices::default(),
            note: None,
            notes: vec![],
        })
        .await
        .unwrap();
        assert_eq!(
            d.store()
                .room(room.id)
                .await
                .unwrap()
                .unwrap()
                .group
                .as_deref(),
            Some("infra")
        );
    })
    .await;
}

#[tokio::test]
async fn a_boss_moved_into_a_bossed_group_asks_first_and_what_it_ran_passes_to_the_main_boss() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, hub) = desk_with_hub(pool, dir.path()).await;
        let a = acme_session(&d, dir.path()).await;
        let plan = d
            .propose_plan(
                NewPlan {
                    draft: crate::plan::Draft {
                        goal: "Ship the Acme checkout".into(),
                        tickets: vec![crate::plan::DraftTicket {
                            id: None,
                            title: "Wire the Acme form".into(),
                            body: String::new(),
                            engine: None,
                            folder: dir.path().to_string_lossy().into_owned(),
                            view: None,
                            needs: vec![],
                        }],
                    },
                    board: None,
                    parallel: None,
                    engine: crate::engine::Engine::Claude,
                    group: Some("acme".into()),
                },
                &Party::manager("sarah", "Sarah"),
                &By::manager("sarah", "Sarah", "asked"),
            )
            .await
            .unwrap()
            .summary
            .plan
            .id;
        // An Acme room Sarah is in, and a session Mia runs in Atlas.
        let room = d
            .create_room(crate::rooms::NewRoom {
                name: "Acme launch".into(),
                members: vec![crate::rooms::Member {
                    hub: crate::rooms::LOCAL_HUB.into(),
                    agent: "sarah".into(),
                }],
                default_member: None,
                group: Some("acme".into()),
                share_with: vec![],
            })
            .await
            .unwrap();
        let mias = new_session(&d, dir.path()).await;
        d.change_layout(|l| l.put(&session_item(mias.id), "atlas", None))
            .await
            .unwrap();
        d.take_on(
            mias.id,
            &Party::manager("mia", "Mia"),
            How::Handed,
            &atlas(),
            &By::you(),
        )
        .await
        .unwrap();
        let sarah = Item::Boss("sarah".into());
        let p = d.preview_move(to("atlas", sarah.clone())).await.unwrap();
        assert_eq!(kinds(&p), ["asks_to_replace"]);
        assert_eq!(
            p.effects[0].text,
            "Atlas already has Mia. Replace them with Sarah? Mia stops overseeing."
        );
        assert!(matches!(
            d.commit_move(MoveCommit {
                preview: p.id,
                choices: Choices::default(),
                note: None,
                notes: vec![],
            })
            .await,
            Err(DeskError::Conflict(_))
        ));
        assert!(hub.moved.lock().is_empty(), "never replaced silently");

        let replace = Choices {
            replace: true,
            ..Choices::default()
        };
        let p = d
            .preview_move(MoveAsk {
                item: sarah,
                to: Some("atlas".into()),
                choices: replace,
            })
            .await
            .unwrap();
        assert_eq!(
            kinds(&p),
            ["replaces", "passes_to_main", "handover", "memory_stays"]
        );
        assert_eq!(p.handovers.len(), 1, "one note per plan");
        let note = &p.handovers[0];
        assert_eq!(note.handover.item, format!("plan:{plan}"));
        assert_eq!(note.handover.to_boss.as_deref(), Some("desk"));
        let done = d
            .commit_move(MoveCommit {
                preview: p.id,
                choices: replace,
                note: None,
                notes: vec![SentNote {
                    id: note.handover.id,
                    note: "Desk: the checkout plan is yours now.".into(),
                }],
            })
            .await
            .unwrap();
        assert!(done.problems.is_empty(), "{:?}", done.problems);
        assert_eq!(
            hub.moved.lock().clone(),
            [("sarah".to_string(), Some("atlas".to_string()), true)]
        );
        // Sarah took none of Acme's along: it's the main boss's now.
        let c = d.charge_of(a.session).unwrap();
        assert!(c.live());
        assert_eq!(c.boss, "desk");
        let p = d.plan(plan).await.unwrap().summary.plan;
        assert_eq!(p.boss, "desk");
        assert_eq!(p.paused, None);
        let delivered = hub.delivered.lock().clone();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].note, "Desk: the checkout plan is yours now.");
        assert_eq!(d.scope_of_boss("sarah"), Some(atlas()));
        // Mia, replaced, let go of what she ran in the same move.
        assert!(!d.charge_of(mias.id).unwrap().live());
        // Sarah left Acme's room.
        let r = d.store().room(room.id).await.unwrap().unwrap();
        assert!(
            r.members.iter().all(|m| m.agent != "sarah"),
            "{:?}",
            r.members
        );
        // Nor is she let into it whatever a Desk app sends; a room this
        // desk doesn't keep is another's to bound.
        assert!(!d.agent_reaches_room(room.id, "sarah").await);
        assert!(d.agent_reaches_room(Uuid::new_v4(), "sarah").await);
    })
    .await;
}

#[tokio::test]
async fn two_previews_of_one_item_never_both_go_through() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, hub) = desk_with_hub(pool, dir.path()).await;
        let a = acme_session(&d, dir.path()).await;
        let one = d
            .preview_move(to("atlas", Item::Session(a.session)))
            .await
            .unwrap();
        let two = d
            .preview_move(to("infra", Item::Session(a.session)))
            .await
            .unwrap();
        let commit = |p: &MovePreview| MoveCommit {
            preview: p.id,
            choices: Choices::default(),
            note: Some("ok".into()),
            notes: vec![],
        };
        let (r1, r2) = tokio::join!(d.commit_move(commit(&one)), d.commit_move(commit(&two)));
        let made = [r1.is_ok(), r2.is_ok()];
        assert_eq!(made.iter().filter(|m| **m).count(), 1, "{r1:?} {r2:?}");
        assert!(matches!(
            if made[0] { r2 } else { r1 },
            Err(DeskError::Conflict(_))
        ));
        assert_eq!(hub.delivered.lock().len(), 1);
    })
    .await;
}

#[tokio::test]
async fn nothing_is_granted_while_what_a_move_leaves_stands() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, _hub) = desk_with_hub(pool, dir.path()).await;
        let a = acme_session(&d, dir.path()).await;
        // A ticket to leave that can't be: it is gone.
        let effects = [
            Effect::StaysUnassigned {
                ticket: Uuid::new_v4(),
                synced: false,
            },
            Effect::ChargeTo { boss: "mia".into() },
            Effect::Tell,
        ];
        let mut problems = Vec::new();
        assert!(d
            .move_session(
                a.session,
                Some("atlas"),
                &effects,
                &By::you(),
                &mut problems
            )
            .await
            .is_err());
        assert_eq!(
            d.view(a.session).unwrap().group.map(|g| g.id).as_deref(),
            Some("acme"),
            "still in Acme"
        );
        assert_eq!(d.charge_of(a.session).unwrap().boss, "sarah", "never Mia's");
    })
    .await;
}

#[tokio::test]
async fn a_ticket_moved_keeps_what_waits_on_it_waiting() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, _hub) = desk_with_hub(pool, dir.path()).await;
        let a = acme_session(&d, dir.path()).await;
        d.create_board(NewBoard {
            name: "Atlas".into(),
            group: Some("atlas".into()),
            prefix: Some("ATL".into()),
        })
        .await
        .unwrap();
        let acme_board = d.ticket(a.ticket).await.unwrap().ticket.board;
        let after = d
            .create_ticket(
                NewTicket {
                    board: acme_board,
                    title: "After the checkout".into(),
                    body: String::new(),
                    state: Some(TicketState::Todo),
                    needs: vec![a.ticket],
                },
                &By::you(),
            )
            .await
            .unwrap();
        let p = d
            .preview_move(to("atlas", Item::Ticket(a.ticket)))
            .await
            .unwrap();
        let kept = p
            .effects
            .iter()
            .find(|e| matches!(e.effect, Effect::NeedsKept { .. }))
            .expect("said in the preview");
        assert_eq!(
            kept.text,
            "1 ticket on Acme's board still wait on it, as a ticket now in another group."
        );
        d.commit_move(MoveCommit {
            preview: p.id,
            choices: Choices::default(),
            note: Some("It goes to Atlas.".into()),
            notes: vec![],
        })
        .await
        .unwrap();
        let waiting = d.ticket(after.ticket.id).await.unwrap();
        assert_eq!(waiting.ticket.needs, vec![a.ticket]);
        assert!(!waiting.ready, "still waits on it");
    })
    .await;
}

/// Previews and moves are the hub holder's alone, as deciding a need is.
#[tokio::test]
async fn only_the_person_who_holds_the_hub_previews_or_makes_a_move() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    struct NotTheHolder;
    #[async_trait::async_trait]
    impl crate::api::Authorize for NotTheHolder {
        async fn authorize(
            &self,
            _: &axum::http::HeaderMap,
            _: Option<&str>,
        ) -> Result<Option<String>, StatusCode> {
            Ok(Some("another-admin".into()))
        }
        async fn authorize_holder(
            &self,
            _: &axum::http::HeaderMap,
            _: Option<&str>,
        ) -> Result<Option<String>, StatusCode> {
            Err(StatusCode::FORBIDDEN)
        }
    }
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, _hub) = desk_with_hub(pool, dir.path()).await;
        let a = acme_session(&d, dir.path()).await;
        let app = crate::move_api::api_routes(d.clone(), Arc::new(NotTheHolder));
        let body =
            serde_json::json!({ "item": { "kind": "session", "id": a.session }, "to": "atlas" });
        let commit = serde_json::json!({ "preview": Uuid::new_v4() });
        for (uri, body) in [
            ("/api/desk/moves/preview", &body),
            ("/api/desk/moves", &commit),
        ] {
            let r = app
                .clone()
                .oneshot(
                    Request::post(uri)
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::FORBIDDEN, "{uri}");
        }
        assert_eq!(
            d.view(a.session).unwrap().group.map(|g| g.id).as_deref(),
            Some("acme")
        );
    })
    .await;
}
