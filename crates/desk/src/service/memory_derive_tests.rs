//! Derivation against Postgres and a fake model: what the record bears out
//! is kept and nothing else, a supersede closes the window, the queue
//! survives a restart, the day's calls are capped, the off switches send
//! nothing, and the person's changes to a note.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::json;
use uuid::Uuid;
use workforce_postgres::test_support::with_test_pool;

use super::memory_derive::{ItemChange, Noting, DERIVE_GROUP_OFF, DERIVE_OFF, DERIVE_PAUSED};
use super::Desk;
use crate::engine::Engine;
use crate::ledger_store::Cursor;
use crate::memory::derive::fake::FakeDeriver;
use crate::memory::record::{Asker, Exchange};
use crate::memory::render::MARKER;
use crate::memory_store::{group_key, RecordWrite, PAUSED};
use crate::store::{build_pool, tests::record_fixture, DeskStore};

/// A closed session Desk keeps.
async fn session(store: &DeskStore, name: &str) -> Uuid {
    let mut r = record_fixture(name);
    r.state = crate::model::SessionState::Closed;
    store.put(&r).await.unwrap();
    r.id
}

/// Keeps a closed exchange of `session`, `minutes_ago`; its id.
async fn said(store: &DeskStore, session: Uuid, key: &str, ask: &str, minutes_ago: i64) -> Uuid {
    let at = Utc::now() - chrono::Duration::minutes(minutes_ago);
    let x = Exchange {
        key: key.into(),
        at,
        ended: Some(at + chrono::Duration::seconds(30)),
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
    };
    store
        .record_write(&RecordWrite {
            session,
            engine: Engine::Claude,
            source: "/records/t.jsonl",
            cursor: &Cursor::default(),
            rows: &[],
            closed: std::slice::from_ref(&x),
            open: None,
            project: "/dev/acme-web",
            folder: "/dev/acme-web",
            name: "acme-web",
            cli_session_id: None,
        })
        .await
        .unwrap();
    q(
        store,
        "SELECT id FROM memory_exchanges WHERE key = $1",
        &[&key],
    )
    .await
}

/// Puts `session`'s exchanges in `group`.
async fn grouped(store: &DeskStore, session: Uuid, group: &str) {
    let client = store.pool().get().await.unwrap();
    client
        .execute(
            "UPDATE memory_exchanges SET group_id = $2 WHERE session = $1",
            &[&session, &group],
        )
        .await
        .unwrap();
}

async fn q<T: for<'a> tokio_postgres::types::FromSql<'a>>(
    store: &DeskStore,
    sql: &str,
    params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
) -> T {
    let client = store.pool().get().await.unwrap();
    client.query_one(sql, params).await.unwrap().get(0)
}

async fn derived_to(store: &DeskStore, session: Uuid) -> i32 {
    let client = store.pool().get().await.unwrap();
    client
        .query_opt(
            "SELECT derived_to FROM memory_derive WHERE session = $1",
            &[&session],
        )
        .await
        .unwrap()
        .map_or(0, |r| r.get(0))
}

async fn start(
    pool: &workforce_postgres::PostgresPool,
    dir: &std::path::Path,
    deriver: &Arc<FakeDeriver>,
    tune: impl FnOnce(&mut super::Options),
) -> (Desk, tokio_util::sync::CancellationToken) {
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut opts = crate::testing::options(dir);
    tune(&mut opts);
    let d = Desk::start(
        DeskStore::from_pool(pool.clone()),
        opts,
        super::Floor {
            deriver: Some(Arc::clone(deriver) as Arc<dyn crate::memory::derive::Deriver>),
            redactor: Some(Arc::new(crate::memory::record::tests::Plain)),
            ..Default::default()
        },
        cancel.clone(),
    )
    .await
    .unwrap();
    (d, cancel)
}

/// Waits until `check` holds, poking the worker meanwhile.
async fn until(d: &Desk, what: &str, mut check: impl AsyncFnMut() -> bool) {
    for _ in 0..600 {
        if check().await {
            return;
        }
        d.derive_poke();
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("never: {what}");
}

/// Lets a moment pass for the worker to do what it would.
async fn settle(d: &Desk) {
    for _ in 0..8 {
        d.derive_poke();
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn a_fake_models_notes_are_kept_only_as_the_record_bears_them_out_and_a_supersede_applies() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let acme = session(&store, "acme-web").await;
        let globex = session(&store, "globex-api").await;
        let x1 = said(&store, acme, "a-1", "Where should the session token live?", 30).await;
        said(&store, acme, "a-2", "Run the tests.", 20).await;
        let g1 = said(&store, globex, "g-1", "The Globex billing key rotates monthly.", 25).await;
        grouped(&store, acme, "g-acme").await;
        grouped(&store, globex, "g-globex").await;
        // Acme's sessions may be read for notes; Globex's group never said.
        store
            .put_setting(&group_key("g-acme"), &json!({"derive": true}))
            .await
            .unwrap();
        let deriver = Arc::new(FakeDeriver::answering([json!({"add": [
            {"kind": "decision", "text": "Keep the session token in an HttpOnly cookie.", "sources": ["x1"]},
            {"kind": "decision", "text": "Ship on Friday.", "sources": []},
            {"kind": "fact", "text": "Billing keys rotate monthly.", "sources": [g1.to_string()]},
        ]})
        .to_string()]));
        *deriver.purpose.lock() = Some("Making sign-in safe.".into());

        let (d, _cancel) = start(&pool, dir.path(), &deriver, |_| {}).await;
        until(&d, "acme's exchanges are read", async || {
            derived_to(&store, acme).await == 2
        })
        .await;
        settle(&d).await;
        assert_eq!(deriver.calls(), 1, "Globex's group isn't read until allowed");
        let asked = deriver.asked.lock()[0].clone();
        assert_eq!(asked.0, acme);
        assert!(asked.1.contains(MARKER), "the record is wrapped");
        assert!(asked.1.contains("Making sign-in safe."));
        assert!(!asked.1.contains("Globex billing"));
        let notes = d.session_memory(acme).await.unwrap().notes;
        assert_eq!(notes.counts.decisions, 1);
        assert!(notes.facts.is_empty(), "the note citing Globex's exchange is dropped");
        assert_eq!(notes.provider.as_deref(), Some("anthropic"), "whose model writes them");
        assert!(!notes.fallback_providers);
        let kept = &notes.decisions[0];
        assert_eq!(kept.text, "Keep the session token in an HttpOnly cookie.");
        assert!(kept.noted_by_desk && !kept.confirmed && !kept.source_gone);
        let x1_at: DateTime<Utc> = q(&store, "SELECT at FROM memory_exchanges WHERE id = $1", &[&x1]).await;
        assert_eq!(kept.at, x1_at, "since its latest source");
        let (by, group): (String, Option<String>) = {
            let client = store.pool().get().await.unwrap();
            let r = client
                .query_one("SELECT by, group_id FROM memory_items WHERE id = $1", &[&kept.id])
                .await
                .unwrap();
            (r.get(0), r.get(1))
        };
        assert_eq!(by, "model:fake-cheap");
        assert_eq!(group.as_deref(), Some("g-acme"));
        assert_eq!(
            q::<i64>(&store, "SELECT COUNT(*) FROM memory_items WHERE session = $1", &[&globex]).await,
            0
        );

        // The session says more; the panel opening reads it at once, and
        // the model replaces the note.
        said(&store, acme, "a-3", "Keep it server-side instead.", 5).await;
        grouped(&store, acme, "g-acme").await;
        deriver.answers.lock().push(Ok(json!({
            "add": [{"kind": "decision", "text": "Keep the session in the server's store.", "sources": ["x1"]}],
            "supersede": [{"id": "i1", "by": 0}],
        })
        .to_string()));
        d.derive_now(acme).await.unwrap();
        until(&d, "the new exchange is read", async || {
            derived_to(&store, acme).await == 3
        })
        .await;
        let client = store.pool().get().await.unwrap();
        let r = client
            .query_one(
                "SELECT o.valid_to, o.superseded_by, n.id, n.valid_from FROM memory_items o \
                 JOIN memory_items n ON n.id = o.superseded_by WHERE o.id = $1",
                &[&kept.id],
            )
            .await
            .unwrap();
        let (valid_to, by, new, from): (DateTime<Utc>, Uuid, Uuid, DateTime<Utc>) =
            (r.get(0), r.get(1), r.get(2), r.get(3));
        assert_eq!(by, new);
        assert_eq!(valid_to, from, "the window closes where the replacement's opens");
        let notes = d.session_memory(acme).await.unwrap().notes;
        assert_eq!(
            notes.decisions.iter().map(|n| n.id).collect::<Vec<_>>(),
            [new],
            "only what is still held is listed"
        );
        assert_eq!(deriver.calls(), 2);
        assert!(
            !deriver.asked.lock()[1].1.contains("Where should the session token live?"),
            "what was read already isn't read again"
        );
        assert!(deriver.asked.lock()[1].1.contains("i1 · decision · held"));
    })
    .await;
}

#[tokio::test]
async fn the_queue_survives_a_restart_mid_call_and_is_read_once() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let s = session(&store, "acme-web").await;
        said(&store, s, "a-1", "Use Postgres for the queue.", 10).await;
        // A worker claimed it and stopped mid-call: no other claim takes it
        // until its lease is up.
        let claim = store.derive_claim(1.0).await.unwrap().expect("it was due");
        assert_eq!((claim.session, claim.derived_to), (s, 0));
        assert!(
            store.derive_claim(1.0).await.unwrap().is_none(),
            "held by its lease"
        );
        let deriver = Arc::new(FakeDeriver::answering([json!({"add": [
            {"kind": "fact", "text": "The queue is in Postgres.", "sources": ["x1"]},
        ]})
        .to_string()]));
        let (d, _cancel) = start(&pool, dir.path(), &deriver, |_| {}).await;
        until(&d, "read once the lease is up", async || {
            derived_to(&store, s).await == 1
        })
        .await;
        settle(&d).await;
        assert_eq!(deriver.calls(), 1);
        assert_eq!(d.session_memory(s).await.unwrap().notes.counts.facts, 1);
        let (due, at): (Option<DateTime<Utc>>, Option<DateTime<Utc>>) = {
            let client = store.pool().get().await.unwrap();
            let r = client
                .query_one(
                    "SELECT due_at, derived_at FROM memory_derive WHERE session = $1",
                    &[&s],
                )
                .await
                .unwrap();
            (r.get(0), r.get(1))
        };
        assert!(due.is_none() && at.is_some(), "nothing more is due");

        // A turn's end now is due only ten minutes after it was last read.
        said(&store, s, "a-2", "And the outbox too.", 0).await;
        let due: DateTime<Utc> = q(
            &store,
            "SELECT due_at FROM memory_derive WHERE session = $1",
            &[&s],
        )
        .await;
        assert!(due > Utc::now() + chrono::Duration::minutes(9));
        settle(&d).await;
        assert_eq!(deriver.calls(), 1, "debounced");
    })
    .await;
}

#[tokio::test]
async fn the_days_calls_are_capped_and_the_panel_says_noting_is_paused() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let day = Utc::now().date_naive();
        assert!(store.derive_take_call(day, 2).await.unwrap());
        assert!(store.derive_take_call(day, 2).await.unwrap());
        assert!(!store.derive_take_call(day, 2).await.unwrap());
        store.derive_untake_call(day).await.unwrap();
        assert_eq!(store.derive_calls(day).await.unwrap(), 1);
        assert!(!store.derive_take_call(day, 0).await.unwrap());

        let a = session(&store, "acme-web").await;
        let b = session(&store, "acme-api").await;
        said(&store, a, "a-1", "Fix the cookie flag.", 10).await;
        said(&store, b, "b-1", "Fix the logout path.", 10).await;
        let deriver = Arc::new(FakeDeriver::default());
        // One call was made today already: one more may be.
        let (d, _cancel) = start(&pool, dir.path(), &deriver, |o| {
            o.memory.derive_calls_per_day = 2;
        })
        .await;
        until(&d, "one session is read", async || deriver.calls() == 1).await;
        settle(&d).await;
        assert_eq!(deriver.calls(), 1, "the day's calls are spent");
        assert_eq!(store.derive_calls(day).await.unwrap(), 2);
        let waiting = match derived_to(&store, a).await {
            0 => a,
            _ => b,
        };
        assert_eq!(derived_to(&store, waiting).await, 0);
        let notes = d.session_memory(waiting).await.unwrap().notes;
        assert_eq!(
            (notes.noting, notes.why.as_deref(), notes.unread),
            (Noting::Paused, Some(DERIVE_PAUSED), 1)
        );
    })
    .await;
}

#[tokio::test]
async fn nothing_is_sent_while_noting_is_off_for_the_desk_its_group_or_memory() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let s = session(&store, "acme-web").await;
        said(&store, s, "a-1", "Fix the cookie flag.", 10).await;
        grouped(&store, s, "g-acme").await;
        let deriver = Arc::new(FakeDeriver::default());

        // Off for the Desk.
        let (d, cancel) = start(&pool, dir.path(), &deriver, |o| o.memory.derive = false).await;
        settle(&d).await;
        assert_eq!(deriver.calls(), 0);
        let notes = d.session_memory(s).await.unwrap().notes;
        assert_eq!(
            (notes.noting, notes.why.as_deref()),
            (Noting::Off, Some(DERIVE_OFF))
        );
        cancel.cancel();

        // A group's sessions aren't read until the person lets them be.
        let (d, cancel) = start(&pool, dir.path(), &deriver, |_| {}).await;
        until(&d, "the claim is let go", async || {
            q::<Option<DateTime<Utc>>>(
                &store,
                "SELECT due_at FROM memory_derive WHERE session = $1",
                &[&s],
            )
            .await
            .is_none()
        })
        .await;
        assert_eq!(deriver.calls(), 0);
        assert_eq!(derived_to(&store, s).await, 0, "what it said waits");
        let notes = d.session_notes(s, Some("g-acme")).await.unwrap();
        assert_eq!(
            (notes.noting, notes.why.as_deref()),
            (Noting::Off, Some(DERIVE_GROUP_OFF))
        );
        store
            .put_setting(
                &group_key("g-acme"),
                &json!({"harvest": true, "derive": false}),
            )
            .await
            .unwrap();
        d.derive_now(s).await.unwrap();
        settle(&d).await;
        assert_eq!(deriver.calls(), 0, "derive = false is off");

        // Memory paused: nothing is sent either, though the group allows it.
        store
            .put_setting(&group_key("g-acme"), &json!({"derive": true}))
            .await
            .unwrap();
        store.put_setting(PAUSED, &json!(true)).await.unwrap();
        d.derive_now(s).await.unwrap();
        settle(&d).await;
        assert_eq!(deriver.calls(), 0);
        store.put_setting(PAUSED, &json!(false)).await.unwrap();
        d.derive_now(s).await.unwrap();
        until(&d, "read once allowed", async || {
            derived_to(&store, s).await == 1
        })
        .await;
        assert_eq!(deriver.calls(), 1);
        let notes = d.session_notes(s, Some("g-acme")).await.unwrap();
        assert_eq!(notes.noting, Noting::On);
        cancel.cancel();
    })
    .await;
}

#[tokio::test]
async fn the_person_confirms_corrects_and_drops_notes_through_the_guarded_route() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let s = session(&store, "acme-web").await;
        said(&store, s, "a-1", "Fix the cookie flag.", 10).await;
        let deriver = Arc::new(FakeDeriver::answering([json!({"add": [
            {"kind": "decision", "text": "Retry sign-in once.", "sources": ["x1"]},
            {"kind": "todo", "text": "Check the logout path.", "sources": ["x1"]},
            {"kind": "fact", "text": "Sessions live in Redis.", "sources": ["x1"]},
        ]})
        .to_string()]));
        let (d, _cancel) = start(&pool, dir.path(), &deriver, |_| {}).await;
        until(&d, "noted", async || derived_to(&store, s).await == 1).await;
        let notes = d.session_memory(s).await.unwrap().notes;
        let (decision, todo, fact) = (notes.decisions[0].id, notes.todos[0].id, notes.facts[0].id);
        assert_eq!(notes.counts.open_todos, 1);

        let denied = crate::api::sessions_router(d.clone(), crate::testing::deny_all());
        let app = crate::api::sessions_router(d.clone(), crate::testing::allow_all());
        let patch = |app: axum::Router, id: Uuid, body: serde_json::Value| async move {
            use tower::ServiceExt;
            let res = app
                .oneshot(
                    axum::http::Request::builder()
                        .method("PATCH")
                        .uri(format!("/api/desk/memory/items/{id}"))
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = res.status().as_u16();
            let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap();
            (
                status,
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap_or_default(),
            )
        };
        assert_eq!(
            patch(denied, decision, json!({"action": "confirm"}))
                .await
                .0,
            401
        );

        let (status, body) = patch(app.clone(), decision, json!({"action": "confirm"})).await;
        assert_eq!(status, 200);
        assert_eq!(body["item"]["confirmed"], true);

        let (status, body) = patch(
            app.clone(),
            fact,
            json!({"action": "correct", "text": "Sessions live in Postgres."}),
        )
        .await;
        assert_eq!(status, 200);
        let corrected: Uuid = body["item"]["id"].as_str().unwrap().parse().unwrap();
        assert_ne!(corrected, fact);
        assert_eq!(body["item"]["notedByDesk"], false);
        assert_eq!(body["item"]["confirmed"], true);
        let (by, superseded): (String, Option<Uuid>) = {
            let client = store.pool().get().await.unwrap();
            let r = client
                .query_one(
                    "SELECT n.by, o.superseded_by FROM memory_items o, memory_items n \
                     WHERE o.id = $1 AND n.id = $2",
                    &[&fact, &corrected],
                )
                .await
                .unwrap();
            (r.get(0), r.get(1))
        };
        assert_eq!((by.as_str(), superseded), ("person", Some(corrected)));
        assert_eq!(
            patch(
                app.clone(),
                fact,
                json!({"action": "correct", "text": "Again."})
            )
            .await
            .0,
            409,
            "it was replaced already"
        );
        assert_eq!(
            patch(
                app.clone(),
                corrected,
                json!({"action": "correct", "text": "  "})
            )
            .await
            .0,
            400
        );
        // A correction is kept and searched with its secrets hidden.
        let (status, body) = patch(
            app.clone(),
            corrected,
            json!({"action": "correct", "text": "Sessions live in Postgres, key SECRET-pg0xA1b2."}),
        )
        .await;
        assert_eq!(status, 200);
        let corrected: Uuid = body["item"]["id"].as_str().unwrap().parse().unwrap();
        assert_eq!(
            body["item"]["text"],
            "Sessions live in Postgres, key [REDACTED TEST_KEY]."
        );
        assert_eq!(
            q::<i64>(
                &store,
                "SELECT COUNT(*) FROM memory_items WHERE text LIKE '%pg0xA1b2%'",
                &[]
            )
            .await,
            0,
            "no part of the secret is kept"
        );

        assert_eq!(
            patch(app.clone(), todo, json!({"action": "drop"})).await.0,
            204
        );
        assert_eq!(
            patch(app.clone(), todo, json!({"action": "drop"})).await.0,
            404
        );
        assert_eq!(
            patch(app.clone(), decision, json!({"action": "forget"}))
                .await
                .0,
            400
        );

        let notes = d.session_memory(s).await.unwrap().notes;
        assert!(notes.todos.is_empty());
        assert_eq!(
            notes.facts.iter().map(|n| n.id).collect::<Vec<_>>(),
            [corrected]
        );
        assert!(notes.decisions[0].confirmed);

        // The model may not replace what the person confirmed or wrote.
        said(&store, s, "a-2", "Actually retry twice.", 1).await;
        deriver.answers.lock().push(Ok(json!({
            "add": [{"kind": "decision", "text": "Retry sign-in twice.", "sources": ["x1"]}],
            "supersede": [{"id": "i1", "by": 0}, {"id": "i2", "by": 0}],
        })
        .to_string()));
        d.derive_now(s).await.unwrap();
        until(&d, "read again", async || derived_to(&store, s).await == 2).await;
        let notes = d.session_memory(s).await.unwrap().notes;
        assert_eq!(notes.counts.decisions, 2, "both stand");
        assert_eq!(notes.counts.facts, 1);

        // A note whose sources are all swept keeps its text.
        let client = store.pool().get().await.unwrap();
        client
            .execute("DELETE FROM memory_exchanges WHERE session = $1", &[&s])
            .await
            .unwrap();
        let notes = d.session_memory(s).await.unwrap().notes;
        assert!(notes.facts[0].source_gone);
        assert_eq!(
            notes.facts[0].text,
            "Sessions live in Postgres, key [REDACTED TEST_KEY]."
        );

        // Forgetting the session forgets its notes and its queue.
        d.forget_memory(s).await.unwrap();
        for table in ["memory_items", "memory_derive"] {
            assert_eq!(
                q::<i64>(
                    &store,
                    &format!("SELECT COUNT(*) FROM {table} WHERE session = $1"),
                    &[&s]
                )
                .await,
                0,
                "{table}"
            );
        }
        assert!(matches!(
            d.change_memory_item(decision, &ItemChange::Confirm).await,
            Err(super::DeskError::NotFound)
        ));
    })
    .await;
}

#[tokio::test]
async fn a_group_not_read_yet_holds_only_its_own_place() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let s = session(&store, "acme-web").await;
        // Its first work was in Acme's group, whose sessions aren't read
        // yet; then it was moved out, and went on ungrouped.
        said(&store, s, "a-1", "Rotate Acme's signing keys.", 30).await;
        grouped(&store, s, "g-acme").await;
        said(&store, s, "u-1", "Fix the flaky test.", 10).await;
        let deriver = Arc::new(FakeDeriver::answering([json!({"add": [
            {"kind": "fact", "text": "The test was flaky from a race.", "sources": ["x1"]},
        ]})
        .to_string()]));
        let (d, _cancel) = start(&pool, dir.path(), &deriver, |_| {}).await;
        until(&d, "its ungrouped work is noted", async || {
            d.session_memory(s).await.unwrap().notes.counts.facts == 1
        })
        .await;
        assert_eq!(deriver.calls(), 1);
        assert!(
            !deriver.asked.lock()[0].1.contains("signing keys"),
            "Acme's exchange isn't sent"
        );
        let reads = |g: &'static str| {
            let store = DeskStore::from_pool(pool.clone());
            async move {
                q::<Option<i32>>(
                    &store,
                    "SELECT (SELECT derived_to FROM memory_derive_reads WHERE session = $1 AND group_key = $2)",
                    &[&s, &g],
                )
                .await
            }
        };
        assert_eq!(reads("").await, Some(2));
        assert_eq!(reads("g-acme").await, None, "Acme's place is where it was");
        let notes = d.session_memory(s).await.unwrap().notes;
        assert_eq!(
            (notes.noting, notes.why.as_deref(), notes.unread),
            (Noting::Off, Some(DERIVE_GROUP_OFF), 1),
            "said of the group the next reading is in"
        );

        // Once Acme's are allowed, its exchange is read, on its own.
        store
            .put_setting(&group_key("g-acme"), &json!({"derive": true}))
            .await
            .unwrap();
        deriver.answers.lock().push(Ok(json!({"add": [
            {"kind": "todo", "text": "Rotate the signing keys.", "sources": ["x1"]},
        ]})
        .to_string()));
        d.derive_now(s).await.unwrap();
        until(&d, "Acme's is read", async || reads("g-acme").await == Some(1)).await;
        assert!(deriver.asked.lock()[1].1.contains("signing keys"));
        assert!(!deriver.asked.lock()[1].1.contains("flaky"));
        let group: Option<String> = q(
            &store,
            "SELECT group_id FROM memory_items WHERE kind = 'todo' AND session = $1",
            &[&s],
        )
        .await;
        assert_eq!(group.as_deref(), Some("g-acme"));
    })
    .await;
}

/// A desk with the search index in memory beside the fake model.
async fn start_indexed(
    pool: &workforce_postgres::PostgresPool,
    dir: &std::path::Path,
    deriver: &Arc<FakeDeriver>,
    index: &Arc<crate::memory::index::fake::FakeIndex>,
) -> (Desk, tokio_util::sync::CancellationToken) {
    let cancel = tokio_util::sync::CancellationToken::new();
    let d = Desk::start(
        DeskStore::from_pool(pool.clone()),
        crate::testing::options(dir),
        super::Floor {
            deriver: Some(Arc::clone(deriver) as Arc<dyn crate::memory::derive::Deriver>),
            index: Some(Arc::clone(index) as Arc<dyn crate::memory::index::MemoryIndex>),
            ..Default::default()
        },
        cancel.clone(),
    )
    .await
    .unwrap();
    (d, cancel)
}

/// Waits until `check` holds, poking both workers meanwhile.
async fn until_indexed(d: &Desk, what: &str, mut check: impl AsyncFnMut() -> bool) {
    for _ in 0..600 {
        if check().await {
            return;
        }
        d.derive_poke();
        d.memory_index_poke();
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("never: {what}");
}

#[tokio::test]
async fn notes_reach_the_index_and_recall_ranked_above_exchanges_and_go_when_forgotten() {
    use crate::memory::index::fake::FakeIndex;
    use crate::memory::passage::{PassageKind, UNGROUPED_COLLECTION};
    use crate::memory_store::MemoryQuery;
    use std::sync::atomic::Ordering;

    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let s = session(&store, "acme-web").await;
        {
            let at = Utc::now() - chrono::Duration::minutes(10);
            let x = Exchange {
                key: "a-1".into(),
                at,
                ended: Some(at),
                asker: Asker::Person,
                ask: "Where should the session token live?".into(),
                answer: "In an HttpOnly cookie.".into(),
                steps: Vec::new(),
                files: Default::default(),
                commands: Vec::new(),
                errors: Vec::new(),
                compaction: false,
                redacted: 0,
                flagged: Default::default(),
            };
            store
                .record_write(&RecordWrite {
                    session: s,
                    engine: Engine::Claude,
                    source: "/records/t.jsonl",
                    cursor: &Cursor::default(),
                    rows: &[],
                    closed: std::slice::from_ref(&x),
                    open: None,
                    project: "/dev/acme-web",
                    folder: "/dev/acme-web",
                    name: "acme-web",
                    cli_session_id: None,
                })
                .await
                .unwrap();
        }
        let deriver = Arc::new(FakeDeriver::answering([json!({"add": [
            {"kind": "decision", "text": "Keep the session token in an HttpOnly cookie.", "sources": ["x1"]},
            {"kind": "todo", "text": "Check the HttpOnly flag on logout.", "sources": ["x1"]},
        ]})
        .to_string()]));
        let index = Arc::new(FakeIndex::default());
        let (d, cancel) = start_indexed(&pool, dir.path(), &deriver, &index).await;
        until_indexed(&d, "the notes and the exchange are in the index", async || {
            index.refs(UNGROUPED_COLLECTION).len() == 3
        })
        .await;
        let notes = d.session_memory(s).await.unwrap().notes;
        let (decision, todo) = (notes.decisions[0].id, notes.todos[0].id);
        let held = index.all();
        let p = held.iter().find(|p| p.of == decision).unwrap();
        assert_eq!((p.kind, p.valid, p.session), (PassageKind::Decision, true, s));
        assert_eq!(
            held.iter().find(|p| p.of == todo).unwrap().kind,
            PassageKind::Todo
        );

        let search = |q: &str, kinds: Option<Vec<PassageKind>>| {
            let d = d.clone();
            let q = q.to_string();
            async move {
                d.memory_search(&MemoryQuery {
                    q,
                    kinds,
                    limit: 10,
                    ..Default::default()
                })
                .await
                .unwrap()
            }
        };
        let found = search("HttpOnly cookie", None).await;
        assert!(!found.keyword_only);
        assert_eq!(found.hits[0].hit.kind, "decision", "a decision above the exchange it came from");
        assert!(found.hits[0].hit.noted_by_desk);
        assert!(found.hits.iter().any(|h| h.hit.kind == "exchange"));
        let only = search("HttpOnly", Some(vec![PassageKind::Decision])).await;
        assert_eq!(
            only.hits.iter().map(|h| h.hit.kind).collect::<Vec<_>>(),
            ["decision"]
        );
        let exchanges = search("HttpOnly", Some(PassageKind::EXCHANGES.to_vec())).await;
        assert!(exchanges.hits.iter().all(|h| h.hit.kind == "exchange"));

        // A restart reconciles the index with Postgres: its notes aren't
        // taken for orphans.
        cancel.cancel();
        let (d, cancel) = start_indexed(&pool, dir.path(), &deriver, &index).await;
        until_indexed(&d, "reconciled", async || {
            d.memory_index_status().await.unwrap().reconciled.is_some()
        })
        .await;
        assert_eq!(
            q::<i64>(&store, "SELECT COUNT(*) FROM memory_purges WHERE done_at IS NULL", &[]).await,
            0,
            "nothing queued to go"
        );
        assert_eq!(index.refs(UNGROUPED_COLLECTION).len(), 3);

        // A replaced note leaves the index (Postgres keeps it), and only its
        // replacement is recalled.
        let client = store.pool().get().await.unwrap();
        client
            .execute(
                "INSERT INTO memory_exchanges (id, engine, key, session, project, folder, seq, at, \
                   ended, asker, ask, answer, steps) \
                 VALUES (gen_random_uuid(), 'claude', 'a-2', $1, '/dev/acme-web', '/dev/acme-web', 2, \
                   now(), now(), 'person', 'Keep it in the server session instead.', 'Done.', '[]')",
                &[&s],
            )
            .await
            .unwrap();
        deriver.answers.lock().push(Ok(json!({
            "add": [{"kind": "decision", "text": "Keep the session token server-side, not in an HttpOnly cookie.", "sources": ["x1"]}],
            "supersede": [{"id": decision.to_string(), "by": 0}],
        })
        .to_string()));
        d.derive_now(s).await.unwrap();
        until_indexed(&d, "the replaced note goes and its replacement is written", async || {
            let refs = index.refs(UNGROUPED_COLLECTION);
            !refs.contains(&decision) && refs.len() == 4
        })
        .await;
        assert!(index.all().iter().all(|p| p.valid));
        let found = search("HttpOnly cookie", Some(vec![PassageKind::Decision])).await;
        assert_eq!(found.hits.len(), 1);
        assert!(!found.hits[0].hit.superseded);
        assert!(found.hits[0].hit.answer.contains("server-side"));
        assert_eq!(
            q::<i64>(&store, "SELECT COUNT(*) FROM memory_items WHERE id = $1 AND valid_to IS NOT NULL", &[&decision]).await,
            1,
            "Postgres keeps what was replaced"
        );
        // A restart's reconcile doesn't write it again.
        cancel.cancel();
        let (d, cancel) = start_indexed(&pool, dir.path(), &deriver, &index).await;
        until_indexed(&d, "reconciled again", async || {
            d.memory_index_status().await.unwrap().reconciled.is_some()
        })
        .await;
        for _ in 0..8 {
            d.memory_index_poke();
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(!index.refs(UNGROUPED_COLLECTION).contains(&decision));

        // Dropped: its passage goes.
        d.change_memory_item(todo, &ItemChange::Drop).await.unwrap();
        until_indexed(&d, "the dropped note's passage goes", async || {
            !index.refs(UNGROUPED_COLLECTION).contains(&todo)
        })
        .await;

        // While the index is down, notes are found by keyword.
        index.down.store(true, Ordering::SeqCst);
        d.memory_index_failed(&eyre::eyre!("down"));
        let found = search("server-side", None).await;
        assert!(found.keyword_only);
        assert_eq!(found.hits[0].hit.kind, "decision");
        index.down.store(false, Ordering::SeqCst);

        // Forgotten: every passage of the session goes, notes too.
        d.forget_memory(s).await.unwrap();
        until_indexed(&d, "the session's passages go", async || {
            index.refs(UNGROUPED_COLLECTION).is_empty()
        })
        .await;
        assert_eq!(
            q::<i64>(&store, "SELECT COUNT(*) FROM memory_items WHERE session = $1", &[&s]).await,
            0
        );
        cancel.cancel();
    })
    .await;
}

/// One request to `app`: its status and JSON body.
async fn call(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (u16, serde_json::Value) {
    use tower::ServiceExt;
    let mut req = axum::http::Request::builder().method(method).uri(uri);
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let res = app
        .oneshot(
            req.body(match body {
                Some(b) => axum::body::Body::from(b.to_string()),
                None => axum::body::Body::empty(),
            })
            .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status().as_u16();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

#[tokio::test]
async fn the_person_sets_memorys_switches_through_the_guarded_routes() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let s = session(&store, "acme-web").await;
        said(&store, s, "a-1", "Fix the cookie flag.", 10).await;
        let deriver = Arc::new(FakeDeriver::answering(std::iter::empty::<String>()));
        let (d, cancel) = start(&pool, dir.path(), &deriver, |o| o.memory.retain_days = 90).await;
        let app = crate::api::sessions_router(d.clone(), crate::testing::allow_all());
        let denied = crate::api::sessions_router(d.clone(), crate::testing::deny_all());
        let (status, _) = call(
            app.clone(),
            "POST",
            "/api/desk/groups",
            Some(json!({"id": "g-acme", "name": "Acme"})),
        )
        .await;
        assert_eq!(status, 200);

        // Its own switch.
        let memory = d.session_memory(s).await.unwrap();
        assert!(memory.memory_on);
        assert!(!memory.no_record);
        assert!(memory.last_read.is_some(), "it kept what was said");
        let uri = format!("/api/desk/sessions/{s}/memory");
        assert_eq!(
            call(denied.clone(), "PATCH", &uri, Some(json!({"on": false})))
                .await
                .0,
            401
        );
        assert_eq!(
            call(app.clone(), "PATCH", &uri, Some(json!({"on": false})))
                .await
                .0,
            204
        );
        let memory = d.session_memory(s).await.unwrap();
        assert!(!memory.memory_on);
        assert_eq!(memory.notes.reason, Some("memory_off"));
        assert_eq!(
            call(
                app.clone(),
                "PATCH",
                &format!("/api/desk/sessions/{}/memory", Uuid::new_v4()),
                Some(json!({"on": true}))
            )
            .await
            .0,
            404
        );

        // The pause, the limits and what is kept.
        assert_eq!(
            call(denied.clone(), "GET", "/api/desk/memory/settings", None)
                .await
                .0,
            401
        );
        let (status, body) = call(app.clone(), "GET", "/api/desk/memory/settings", None).await;
        assert_eq!(status, 200);
        assert_eq!(body["paused"], false);
        assert_eq!(body["limits"]["retainDays"], 90);
        assert_eq!(body["limits"]["deriveCallsPerDay"], 200);
        assert_eq!(body["limits"]["provider"], "anthropic");
        assert_eq!(body["totals"][0]["exchanges"], 1);
        assert_eq!(
            body["groups"],
            json!([{"id": "g-acme", "name": "Acme", "harvest": true, "derive": false,
                    "handoffAsk": true, "deriveDecided": false}])
        );
        let (status, body) = call(
            app.clone(),
            "PATCH",
            "/api/desk/memory/settings",
            Some(json!({"paused": true})),
        )
        .await;
        assert_eq!((status, &body["paused"]), (200, &json!(true)));
        assert_eq!(store.setting(PAUSED).await.unwrap(), Some(json!(true)));
        assert_eq!(
            call(
                app.clone(),
                "PATCH",
                "/api/desk/memory/settings",
                Some(json!({"pasued": true}))
            )
            .await
            .0,
            400
        );

        // A group's: what the change names, the rest kept.
        let (status, body) = call(
            app.clone(),
            "PATCH",
            "/api/desk/memory/groups/g-acme",
            Some(json!({"derive": true})),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(
            (&body["derive"], &body["deriveDecided"]),
            (&json!(true), &json!(true))
        );
        call(
            app.clone(),
            "PATCH",
            "/api/desk/memory/groups/g-acme",
            Some(json!({"harvest": false, "handoffAsk": false})),
        )
        .await;
        assert_eq!(
            store.setting(&group_key("g-acme")).await.unwrap(),
            Some(json!({"derive": true, "harvest": false, "handoff_ask": false}))
        );
        assert_eq!(
            call(
                app.clone(),
                "PATCH",
                "/api/desk/memory/groups/g-globex",
                Some(json!({"derive": true}))
            )
            .await
            .0,
            404
        );
        assert_eq!(
            call(
                denied,
                "PATCH",
                "/api/desk/memory/groups/g-acme",
                Some(json!({"derive": false}))
            )
            .await
            .0,
            401
        );
        cancel.cancel();
    })
    .await;
}

#[tokio::test]
async fn a_replaced_note_is_listed_with_when_and_why_noting_is_off_is_said_as_a_reason() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let s = session(&store, "acme-web").await;
        said(&store, s, "a-1", "Fix the cookie flag.", 10).await;
        let deriver = Arc::new(FakeDeriver::answering([json!({"add": [
            {"kind": "fact", "text": "Sessions live in Redis.", "sources": ["x1"]},
        ]})
        .to_string()]));
        let (d, cancel) = start(&pool, dir.path(), &deriver, |_| {}).await;
        until(&d, "noted", async || derived_to(&store, s).await == 1).await;
        let fact = d.session_memory(s).await.unwrap().notes.facts[0].id;
        d.change_memory_item(
            fact,
            &ItemChange::Correct {
                text: "Sessions live in Postgres.".into(),
            },
        )
        .await
        .unwrap();
        let notes = d.session_memory(s).await.unwrap().notes;
        assert_eq!(notes.facts[0].text, "Sessions live in Postgres.");
        assert_eq!(notes.replaced.len(), 1);
        assert_eq!(
            (notes.replaced[0].id, notes.replaced[0].text.as_str()),
            (fact, "Sessions live in Redis.")
        );
        assert_eq!(notes.reason, None);
        cancel.cancel();

        let (d, cancel) = start(&pool, dir.path(), &deriver, |o| o.memory.derive = false).await;
        let notes = d.session_memory(s).await.unwrap().notes;
        assert_eq!(
            (notes.reason, notes.why.as_deref()),
            (Some("desk_off"), Some(DERIVE_OFF))
        );
        cancel.cancel();
    })
    .await;
}
