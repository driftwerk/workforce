//! The loop a boss runs on the person's Desk work, end to end, through
//! Desk's own paths: the person's tickets, a plan the boss proposes on them
//! that the person approves, its sessions started by the plan's runner
//! (what each start carries judged and kept as it is), what Desk shares,
//! what the boss's turn reads of it, what that arms, what the boss then
//! says to its sessions on Desk's line and types into them, and what that
//! makes of the next read. A boss running the person's own tickets reads
//! its work quietly round after round; only the turn that opens what may
//! carry outside content is armed, and only what that turn says carries it
//! on.
//!
//! Each exchange is written as the harvest keeps it (`memory_exchanges`):
//! the harvest reads an engine's own transcripts, which the stand-in
//! engine here doesn't write. What each is asked is what Desk put in.

use std::sync::Arc;

use chrono::{DateTime, Duration as Span, Utc};
use workforce_agent::runner::{caller_content_guard, with_content_guard};
use workforce_agent::security::monitor::{ContentGuard, ContentScope};
use workforce_agent::tools::Tool;
use workforce_desk::board::TicketState;
use workforce_desk::crew::Party;
use workforce_desk::knows::{Digest, Rule};
use workforce_desk::line::{marker, Send, Target, When};
use workforce_desk::plan::{Draft, DraftTicket};
use workforce_desk::{KnowsGather, LineSender, NewBoard, NewPlan, NewTicket};
use workforce_postgres::PostgresPool;

use super::plans::{hub_with, ACTS};
use super::*;
use crate::desk_knows::store::{Item, MemoryKnows, Owner, Reader, TRANSCRIPT};
use crate::desk_knows::tools::{tools, Work};

const AGENT: &str = "allan";
const SESSIONS: usize = 8;

/// [`ACTS`], running as many sessions at once as the plan has.
fn acting() -> String {
    format!("{ACTS}[oversight.work]\nparallel = {SESSIONS}\n")
}

fn owner() -> Owner {
    Owner {
        person: uuid::Uuid::from_u128(1),
        source: "mac".into(),
        agent: AGENT.into(),
    }
}

/// One exchange of `session`, as the harvest keeps it.
#[allow(clippy::too_many_arguments)]
async fn exchange(
    pool: &PostgresPool,
    session: uuid::Uuid,
    key: &str,
    at: DateTime<Utc>,
    asker: &str,
    ask: &str,
    steps: Value,
) {
    pool.get()
        .await
        .unwrap()
        .execute(
            "INSERT INTO memory_exchanges (id, engine, key, session, group_id, project, folder, \
               seq, at, ended, asker, ask, answer, steps) \
             VALUES ($1, 'claude', $2, $3, NULL, 'x', '/x', 1, $4, $4, $5, $6, 'Deployed it.', $7)",
            &[
                &uuid::Uuid::new_v4(),
                &key,
                &session,
                &at,
                &asker,
                &ask,
                &steps,
            ],
        )
        .await
        .unwrap();
}

/// What Desk shares now, sent to the agent's hub and marked sent: its
/// transcripts.
async fn sync(d: &Desk, w: &Work, sessions: &[uuid::Uuid]) -> Vec<Item> {
    let places: Vec<String> = sessions.iter().map(|s| format!("session:{s}")).collect();
    let rule = Rule::new(&places, true, Digest::Off).unwrap();
    let cap = KnowsGather {
        items: 512,
        bytes: 4 * 1024 * 1024,
        backfill: Span::days(14),
    };
    let pending = d
        .knows_pending(
            AGENT,
            &rule,
            &cap,
            &crate::desk_memory::HubRedactor,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
    let items: Vec<Item> = pending
        .iter()
        .map(|o| Item {
            kind: o.held.kind.clone(),
            reference: o.held.reference.clone(),
            session: o.held.session.map(|s| s.to_string()),
            group_name: o.group_name.clone(),
            at: o.at,
            body: o.body.clone(),
        })
        .collect();
    w.store.push(&owner(), &items, Utc::now()).await.unwrap();
    d.knows_mark_sent(AGENT, &pending, Utc::now())
        .await
        .unwrap();
    items.into_iter().filter(|i| i.kind == TRANSCRIPT).collect()
}

/// One boss turn reading with `tool` (`input`): what its guard is armed by.
async fn read(guard: &Arc<ContentGuard>, tool: &dyn Tool, input: Value) -> String {
    let out = tool.execute(input).await.unwrap();
    assert!(out.success, "{}: {out:?}", tool.name());
    let said = out.output.to_string();
    guard.returned(
        tool.name(),
        tool.reads_outside_content(),
        out.success,
        &said,
    );
    said
}

/// The boss tells `to` to go on, on Desk's line: what its exchange is
/// asked, as Desk puts it in.
async fn steer(d: &Desk, boss: &By, to: uuid::Uuid) -> String {
    let m = d
        .line_send(
            LineSender::Overseer(boss.clone()),
            Send::Tell {
                to: Target::Session(to),
                text: "Deploy it.".into(),
                when: When::Queue,
            },
        )
        .await
        .unwrap();
    format!("{} {}", marker(m.id), m.text)
}

/// The boss types `text` into `to` (`desk.prompt`): what its exchange is
/// asked, as Desk typed it.
async fn typed(d: &Desk, pool: &PostgresPool, boss: &By, to: uuid::Uuid, text: &str) -> String {
    d.prompt_for(to, text, boss.clone()).await.unwrap();
    let mark: uuid::Uuid = pool
        .get()
        .await
        .unwrap()
        .query_one(
            "SELECT id FROM typed_prompts WHERE session = $1 ORDER BY at DESC LIMIT 1",
            &[&to],
        )
        .await
        .unwrap()
        .get(0);
    format!("{} {text}", marker(mark))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_boss_running_the_persons_own_tickets_reads_its_work_quietly_round_after_round() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let raw = pool.clone();
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, state, d, _llm) = hub_with(dir.path(), pool, &acting()).await;
        d.read_outside_with(Arc::new(|| {
            caller_content_guard().and_then(|guard| guard.armed())
        }));
        let _crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        let boss = By::manager("desk", "Desk", "Sam asked for the release.");

        // The person's own tickets, and the boss's plan on them, approved.
        let board = d
            .create_board(NewBoard {
                name: "Engineering".into(),
                group: None,
                prefix: None,
            })
            .await
            .unwrap()
            .id;
        let mut drafts = vec![];
        for n in 0..SESSIONS {
            let t = d
                .create_ticket(
                    NewTicket {
                        board,
                        title: format!("Deploy service {n}"),
                        body: "Roll it out to staging, then production.".into(),
                        state: Some(TicketState::Todo),
                        needs: vec![],
                    },
                    &By::you(),
                )
                .await
                .unwrap();
            drafts.push(DraftTicket {
                adopt: Some(t.ticket.id),
                folder: dir.path().to_string_lossy().into_owned(),
                ..Default::default()
            });
        }
        let plan = d
            .propose_plan(
                NewPlan {
                    draft: Draft {
                        goal: "Ship the release".into(),
                        tickets: drafts,
                    },
                    board: None,
                    parallel: Some(SESSIONS as i16),
                    engine: Engine::Claude,
                    group: None,
                    budget: None,
                    result: None,
                    goal_ticket: None,
                },
                &Party::manager("desk", "Desk"),
                &boss,
            )
            .await
            .unwrap()
            .summary
            .plan
            .id;
        d.approve_plan(plan, 1, &By::you()).await.unwrap();
        let ids: Vec<uuid::Uuid> = eventually("the plan's sessions", async || {
            let held: Vec<uuid::Uuid> = d
                .plan(plan)
                .await
                .ok()?
                .tickets
                .iter()
                .filter_map(|t| t.ticket.ticket.session())
                .collect();
            (held.len() == SESSIONS).then_some(held)
        })
        .await;
        // Each start judged as its runner made it: the person's own words.
        for id in &ids {
            assert_eq!(
                d.store().session_start(*id).await.unwrap(),
                Some(None),
                "{id}"
            );
        }
        // And a session of the person's own that read the web.
        let docs = new_session(&d, dir.path(), "docs").await.id;
        let mut sessions = ids.clone();
        sessions.push(docs);
        raw.get()
            .await
            .unwrap()
            .execute(
                "UPDATE sessions SET memory = 'on' WHERE id = ANY($1)",
                &[&sessions],
            )
            .await
            .unwrap();

        let start = Utc::now() - Span::hours(3);
        for (n, id) in ids.iter().enumerate() {
            exchange(
                &raw,
                *id,
                &format!("eng-{n}-1"),
                start,
                "person",
                &format!("Deploy service {n}: roll it out"),
                json!([{"tool": "Bash", "input": "cargo test"}]),
            )
            .await;
        }
        exchange(
            &raw,
            docs,
            "docs-1",
            start,
            "person",
            "read the deploy docs",
            json!([{"tool": "WebFetch", "input": "https://docs.example/deploy"}]),
        )
        .await;
        let w = Work {
            store: MemoryKnows::new(),
            reader: Reader {
                person: owner().person,
                agent: AGENT.into(),
            },
        };
        let all = tools(&w);
        let tool = |name: &str| Arc::clone(all.iter().find(|t| t.name() == name).unwrap());
        let (overview, search, transcript) = (
            tool("desk_work.overview"),
            tool("desk_work.search"),
            tool("desk_work.transcript"),
        );
        let outside = |items: &[Item]| -> Vec<String> {
            items
                .iter()
                .filter(|i| i.body["outside"].as_array().is_none_or(|w| !w.is_empty()))
                .map(|i| i.reference.clone())
                .collect()
        };
        assert_eq!(outside(&sync(&d, &w, &sessions).await), ["claude:docs-1"]);

        // Rounds of the boss reading its work and steering every session,
        // on the line and, the last, typed in: none arms, so nothing it
        // says carries anything in.
        for round in 2..=5 {
            let guard = Arc::new(ContentGuard::new(ContentScope::default()));
            let at = start + Span::minutes(round * 10);
            let asks = with_content_guard(Some(Arc::clone(&guard)), async {
                read(&guard, overview.as_ref(), json!({})).await;
                read(&guard, search.as_ref(), json!({"query": "deploy"})).await;
                let found = read(&guard, search.as_ref(), json!({"query": "docs"})).await;
                assert!(
                    found.contains("claude:docs-1") && !found.contains("Deployed it"),
                    "the page read is only named: {found}"
                );
                let mut asks = vec![];
                for id in &ids {
                    asks.push(match round {
                        5 => typed(&d, &raw, &boss, *id, "run the deploy").await,
                        _ => steer(&d, &boss, *id).await,
                    });
                }
                asks
            })
            .await;
            assert_eq!(guard.armed(), None, "round {round}");
            for (n, (id, ask)) in ids.iter().zip(asks).enumerate() {
                exchange(
                    &raw,
                    *id,
                    &format!("eng-{n}-{round}"),
                    at + Span::minutes(1),
                    "desk",
                    &ask,
                    json!([{"tool": "Bash", "input": "git push"}]),
                )
                .await;
            }
            let sent = sync(&d, &w, &sessions).await;
            assert_eq!(sent.len(), ids.len(), "round {round}");
            assert!(outside(&sent).is_empty(), "round {round}: {sent:?}");
        }

        // The turn that opens the page read is armed, and only what it says
        // next carries that in.
        let guard = Arc::new(ContentGuard::new(ContentScope::default()));
        let at = start + Span::minutes(70);
        let ask = with_content_guard(Some(Arc::clone(&guard)), async {
            read(
                &guard,
                transcript.as_ref(),
                json!({"session": docs.to_string()}),
            )
            .await;
            steer(&d, &boss, ids[0]).await
        })
        .await;
        assert_eq!(guard.armed().as_deref(), Some("desk_work.transcript"));
        exchange(
            &raw,
            ids[0],
            "eng-0-6",
            at + Span::minutes(1),
            "desk",
            &ask,
            json!([]),
        )
        .await;
        let sent = sync(&d, &w, &sessions).await;
        assert_eq!(outside(&sent), ["claude:eng-0-6"]);

        // The next turn's read of the work stays quiet: it is only named.
        let guard = Arc::new(ContentGuard::new(ContentScope::default()));
        with_content_guard(Some(Arc::clone(&guard)), async {
            read(&guard, overview.as_ref(), json!({})).await;
            let found = read(&guard, search.as_ref(), json!({"query": "deploy"})).await;
            assert!(found.contains("claude:eng-0-6"), "{found}");
        })
        .await;
        assert_eq!(guard.armed(), None);
    })
    .await;
}
