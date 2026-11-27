//! The loop a boss runs on the person's Desk work, end to end: what Desk
//! shares, what the boss's turn reads of it, what that arms, what the boss
//! then says to its sessions, and what that makes of the next read. A boss
//! running the person's own tickets reads its work quietly round after
//! round; only the turn that opens what may carry outside content is armed,
//! and only what that turn says carries it on.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use uuid::Uuid;
use workforce_agent::runner::{caller_content_guard, with_content_guard};
use workforce_agent::security::monitor::{ContentGuard, ContentScope};
use workforce_agent::tools::Tool;
use workforce_desk::crew::{By, Party};
use workforce_desk::knows::{Digest, Rule};
use workforce_desk::line::{marker, LineKind, LineMessage};
use workforce_desk::{Desk, KnowsGather};
use workforce_postgres::test_support::with_test_pool;
use workforce_postgres::PostgresPool;

use super::store::{Item, MemoryKnows, Owner, Reader, TRANSCRIPT};
use super::tools::{tools, Work};

const AGENT: &str = "allan";
const SESSIONS: u128 = 8;

fn owner() -> Owner {
    Owner {
        person: Uuid::from_u128(1),
        source: "mac".into(),
        agent: AGENT.into(),
    }
}

async fn session(d: &Desk, pool: &PostgresPool, id: Uuid, name: &str) {
    pool.get()
        .await
        .unwrap()
        .execute(
            "INSERT INTO sessions (id, engine, name, cwd, origin, state, approvals, memory, \
               created_at, updated_at, state_at) \
             VALUES ($1, 'claude', $2, '/Users/me/dev/x', 'desk', 'stopped', 'desk', 'on', now(), now(), now())",
            &[&id, &name],
        )
        .await
        .unwrap();
    d.store().put_session_start(id, None).await.unwrap();
}

#[allow(clippy::too_many_arguments)]
async fn exchange(
    pool: &PostgresPool,
    session: Uuid,
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
            &[&Uuid::new_v4(), &key, &session, &at, &asker, &ask, &steps],
        )
        .await
        .unwrap();
}

/// The boss tells `to` to go on, as Desk's line records it: carrying in
/// what the boss's turn calling Desk had read. What its exchange is asked.
async fn steer(d: &Desk, boss: &By, to: Uuid, at: DateTime<Utc>) -> String {
    let to_party = Party {
        kind: workforce_desk::crew::PartyKind::Session,
        id: to.to_string(),
        name: "to".into(),
        engine: None,
        origin: None,
    };
    let mut m = LineMessage::new(
        boss.party.clone(),
        to_party,
        LineKind::Tell,
        "Deploy it.".into(),
        1,
    );
    m.at = at;
    m.outside = d.carried_by(boss);
    d.store().put_line(&m).await.unwrap();
    format!("{} Allan tells you: Deploy it.", marker(m.id))
}

/// What Desk shares now, sent to the agent's hub and marked sent.
async fn sync(d: &Desk, w: &Work, sessions: &[Uuid]) -> Vec<Item> {
    let places: Vec<String> = sessions.iter().map(|s| format!("session:{s}")).collect();
    let rule = Rule::new(&places, true, Digest::Off).unwrap();
    let cap = KnowsGather {
        items: 512,
        bytes: 4 * 1024 * 1024,
        backfill: Duration::days(14),
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

#[tokio::test]
async fn a_boss_running_the_persons_own_tickets_reads_its_work_quietly_round_after_round() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = crate::desk_crew::tests::desk(pool.clone(), dir.path()).await;
        d.read_outside_with(Arc::new(|| {
            caller_content_guard().and_then(|guard| guard.armed())
        }));
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
        let boss = By::manager("allan", "Allan", "");
        let ids: Vec<Uuid> = (1..=SESSIONS).map(|n| Uuid::from_u128(100 + n)).collect();
        let fetched = Uuid::from_u128(200);
        let start = Utc::now() - Duration::hours(3);
        for (n, id) in ids.iter().enumerate() {
            session(&d, &pool, *id, &format!("eng-{n}")).await;
            exchange(
                &pool,
                *id,
                &format!("eng-{n}-1"),
                start,
                "person",
                &format!("ENG-{n} is yours: deploy the release"),
                json!([{"tool": "Bash", "input": "cargo test"}]),
            )
            .await;
        }
        session(&d, &pool, fetched, "docs").await;
        exchange(
            &pool,
            fetched,
            "docs-1",
            start,
            "person",
            "read the deploy docs",
            json!([{"tool": "WebFetch", "input": "https://docs.example/deploy"}]),
        )
        .await;
        let mut sessions = ids.clone();
        sessions.push(fetched);
        let outside = |items: &[Item]| -> Vec<String> {
            items
                .iter()
                .filter(|i| i.body["outside"].as_array().is_none_or(|w| !w.is_empty()))
                .map(|i| i.reference.clone())
                .collect()
        };
        assert_eq!(outside(&sync(&d, &w, &sessions).await), ["claude:docs-1"]);

        // Rounds of the boss reading its work and steering every session:
        // none arms, so nothing it says carries anything in.
        for round in 2..=4 {
            let guard = Arc::new(ContentGuard::new(ContentScope::default()));
            let at = start + Duration::minutes(round * 10);
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
                    asks.push(steer(&d, &boss, *id, at).await);
                }
                asks
            })
            .await;
            assert_eq!(guard.armed(), None, "round {round}");
            for (n, (id, ask)) in ids.iter().zip(asks).enumerate() {
                exchange(
                    &pool,
                    *id,
                    &format!("eng-{n}-{round}"),
                    at + Duration::minutes(1),
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
        let at = start + Duration::minutes(60);
        let ask = with_content_guard(Some(Arc::clone(&guard)), async {
            read(
                &guard,
                transcript.as_ref(),
                json!({"session": fetched.to_string()}),
            )
            .await;
            steer(&d, &boss, ids[0], at).await
        })
        .await;
        assert_eq!(guard.armed().as_deref(), Some("desk_work.transcript"));
        exchange(
            &pool,
            ids[0],
            "eng-0-5",
            at + Duration::minutes(1),
            "desk",
            &ask,
            json!([]),
        )
        .await;
        let sent = sync(&d, &w, &sessions).await;
        assert_eq!(outside(&sent), ["claude:eng-0-5"]);

        // The next turn's read of the work stays quiet: it is only named.
        let guard = Arc::new(ContentGuard::new(ContentScope::default()));
        with_content_guard(Some(Arc::clone(&guard)), async {
            read(&guard, overview.as_ref(), json!({})).await;
            let found = read(&guard, search.as_ref(), json!({"query": "deploy"})).await;
            assert!(found.contains("claude:eng-0-5"), "{found}");
        })
        .await;
        assert_eq!(guard.armed(), None);
    })
    .await;
}
