//! The index worker against Postgres and an index in memory: the outbox
//! drains once, an outage loses nothing, forgets and sweeps reach the
//! index, reconcile mends it, and a rebuild starts it again.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;
use workforce_postgres::test_support::with_test_pool;

use super::Desk;
use crate::memory::index::fake::FakeIndex;
use crate::memory::passage::{collection_of, Passage, PassageKind, UNGROUPED_COLLECTION};
use crate::memory::record::tests::Plain;
use crate::store::{build_pool, tests::record_fixture, DeskStore};

const CLAUDE: &str = concat!(
    include_str!("../../fixtures/ledger/claude.jsonl"),
    include_str!("../../fixtures/memory/claude-more.jsonl")
);

/// A closed session whose record is the fixture, as its own conversation.
fn session(dir: &std::path::Path, name: &str) -> crate::model::SessionRecord {
    let t = dir.join(format!("{name}.jsonl"));
    std::fs::write(
        &t,
        CLAUDE.replace("\"uuid\":\"", &format!("\"uuid\":\"{name}-")),
    )
    .unwrap();
    let mut r = record_fixture(name);
    r.cwd = dir.to_string_lossy().into_owned();
    r.transcript = Some(t.to_string_lossy().into_owned());
    r.state = crate::model::SessionState::Closed;
    r
}

async fn start(
    pool: &workforce_postgres::PostgresPool,
    dir: &std::path::Path,
    index: &Arc<FakeIndex>,
) -> (Desk, tokio_util::sync::CancellationToken) {
    let cancel = tokio_util::sync::CancellationToken::new();
    let d = Desk::start(
        DeskStore::from_pool(pool.clone()),
        crate::testing::options(dir),
        super::Floor {
            redactor: Some(Arc::new(Plain)),
            index: Some(Arc::clone(index) as Arc<dyn crate::memory::index::MemoryIndex>),
            ..Default::default()
        },
        cancel.clone(),
    )
    .await
    .unwrap();
    (d, cancel)
}

async fn q<T: for<'a> tokio_postgres::types::FromSql<'a>>(
    store: &DeskStore,
    sql: &str,
    params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
) -> T {
    let client = store.pool().get().await.unwrap();
    client.query_one(sql, params).await.unwrap().get(0)
}

/// `id`'s closed exchanges.
async fn closed(store: &DeskStore, id: Uuid) -> Vec<Uuid> {
    let client = store.pool().get().await.unwrap();
    client
        .query(
            "SELECT id FROM memory_exchanges WHERE session = $1 AND ended IS NOT NULL ORDER BY id",
            &[&id],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

/// Waits until `check` holds, poking the worker meanwhile.
async fn until(d: &Desk, what: &str, mut check: impl AsyncFnMut() -> bool) {
    for _ in 0..600 {
        if check().await {
            return;
        }
        d.memory_index_poke();
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("never: {what}");
}

async fn harvested(d: &Desk, store: &DeskStore, id: Uuid, n: usize) {
    until(d, "harvested", async || closed(store, id).await.len() == n).await;
}

#[tokio::test]
async fn closed_exchanges_reach_the_index_once_and_an_outage_loses_none() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let web = session(dir.path(), "acme-web");
        store.put(&web).await.unwrap();
        let index = Arc::new(FakeIndex::default());
        index.down.store(true, Ordering::SeqCst);

        let (d, cancel) = start(&pool, dir.path(), &index).await;
        harvested(&d, &store, web.id, 6).await;
        until(&d, "the outage is said", async || {
            d.memory_index_status().await.unwrap().failing.is_some()
        })
        .await;
        let status = d.memory_index_status().await.unwrap();
        assert!(status.installed);
        assert_eq!(
            status.backlog, 6,
            "nothing is marked written while the index is down"
        );
        assert!(index.all().is_empty());

        index.down.store(false, Ordering::SeqCst);
        until(&d, "the outbox drains", async || {
            d.memory_index_status().await.unwrap().backlog == 0
        })
        .await;
        assert_eq!(
            index.refs(UNGROUPED_COLLECTION),
            closed(&store, web.id).await
        );
        assert!(index
            .all()
            .iter()
            .all(|p: &Passage| p.session == web.id && p.valid));
        assert!(index
            .all()
            .iter()
            .any(|p| p.kind == PassageKind::Compaction));
        assert!(d.memory_index_status().await.unwrap().failing.is_none());
        cancel.cancel();

        // A restart reconciles, finds the index whole, and writes nothing.
        let writes = index.writes.load(Ordering::SeqCst);
        let (d, _cancel) = start(&pool, dir.path(), &index).await;
        until(&d, "reconciled", async || {
            d.memory_index_status().await.unwrap().reconciled.is_some()
        })
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(index.writes.load(Ordering::SeqCst), writes);
    })
    .await;
}

#[tokio::test]
async fn forgetting_and_sweeping_take_out_what_the_index_held() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let (web, api) = (
            session(dir.path(), "acme-web"),
            session(dir.path(), "acme-api"),
        );
        for r in [&web, &api] {
            store.put(r).await.unwrap();
        }
        // `api` is in Acme's group, as the sidebar keeps it: its memory is
        // stamped Acme's, and kept in Acme's collection.
        {
            let client = store.pool().get().await.unwrap();
            client
                .execute(
                    "INSERT INTO groups (id, name, position, updated_at) VALUES ('g-acme', 'Acme', 0, now())",
                    &[],
                )
                .await
                .unwrap();
            client
                .execute(
                    "INSERT INTO group_members (item, group_id, position) VALUES ($1, 'g-acme', 0)",
                    &[&format!("session:{}", api.id)],
                )
                .await
                .unwrap();
        }
        let index = Arc::new(FakeIndex::default());
        let (d, _cancel) = start(&pool, dir.path(), &index).await;
        harvested(&d, &store, web.id, 6).await;
        harvested(&d, &store, api.id, 6).await;
        assert_eq!(
            q::<i64>(&store, "SELECT COUNT(*) FROM memory_exchanges WHERE group_id = 'g-acme' AND session = $1", &[&api.id]).await,
            6,
            "a closed session keeps its group"
        );
        until(&d, "both indexed", async || {
            d.memory_index_status().await.unwrap().backlog == 0
        })
        .await;
        let acme = collection_of(Some("g-acme"));
        assert_eq!(
            (index.refs(&acme), index.refs(UNGROUPED_COLLECTION)),
            (closed(&store, api.id).await, closed(&store, web.id).await)
        );

        d.forget_memory(api.id).await.unwrap();
        until(&d, "Acme's collection emptied", async || {
            index.refs(&acme).is_empty()
        })
        .await;
        assert_eq!(
            index.refs(UNGROUPED_COLLECTION).len(),
            6,
            "the other session's stays"
        );
        assert_eq!(
            q::<i64>(
                &store,
                "SELECT COUNT(*) FROM memory_purges WHERE done_at IS NULL",
                &[]
            )
            .await,
            0
        );

        // Past its keeping: swept from Postgres and then from the index.
        let old = closed(&store, web.id).await[0];
        {
            let client = store.pool().get().await.unwrap();
            client
                .execute(
                    "UPDATE memory_exchanges SET at = now() - interval '400 days', \
                       ended = now() - interval '400 days' WHERE id = $1",
                    &[&old],
                )
                .await
                .unwrap();
        }
        let swept = store.sweep_memory(180).await.unwrap();
        assert_eq!((swept.gone, swept.purges), (1, 1));
        until(&d, "the swept exchange leaves the index", async || {
            !index.refs(UNGROUPED_COLLECTION).contains(&old)
        })
        .await;
        assert_eq!(index.refs(UNGROUPED_COLLECTION).len(), 5);
    })
    .await;
}

#[tokio::test]
async fn reconcile_mends_the_index_and_a_rebuild_starts_it_again() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let web = session(dir.path(), "acme-web");
        store.put(&web).await.unwrap();
        let index = Arc::new(FakeIndex::default());
        let (d, cancel) = start(&pool, dir.path(), &index).await;
        harvested(&d, &store, web.id, 6).await;
        until(&d, "indexed", async || {
            index.refs(UNGROUPED_COLLECTION).len() == 6
        })
        .await;
        cancel.cancel();

        // The index loses one passage, changes another, holds one Postgres
        // never had, and a collection of a group Desk no longer has.
        let ids = closed(&store, web.id).await;
        let orphan = Uuid::new_v4();
        let stray = collection_of(Some("g-gone"));
        {
            let mut held = index.held.lock();
            let c = held.get_mut(UNGROUPED_COLLECTION).unwrap();
            let lost = c
                .keys()
                .find(|k| k.starts_with(&ids[0].to_string()))
                .cloned()
                .unwrap();
            c.remove(&lost);
            let changed = c.values_mut().find(|p| p.of == ids[1]).unwrap();
            changed.hash = "0000000000000000".into();
            let mut extra = c.values().next().unwrap().clone();
            extra.of = orphan;
            extra.key = format!("{orphan}#0");
            c.insert(extra.key.clone(), extra.clone());
            held.entry(stray.clone())
                .or_default()
                .insert(extra.key.clone(), extra);
        }
        let writes = index.writes.load(Ordering::SeqCst);
        let (d, _cancel) = start(&pool, dir.path(), &index).await;
        until(&d, "mended", async || {
            let held = index.held.lock();
            let c = &held[UNGROUPED_COLLECTION];
            !held.contains_key(&stray)
                && c.values().all(|p| p.of != orphan)
                && c.values().any(|p| p.of == ids[0])
                && c.values()
                    .filter(|p| p.of == ids[1])
                    .all(|p| p.hash != "0000000000000000")
        })
        .await;
        assert_eq!(index.refs(UNGROUPED_COLLECTION), ids);
        assert!(index.writes.load(Ordering::SeqCst) > writes);

        // A rebuild drops the collection and writes everything again.
        let queued = d.rebuild_memory_index().await.unwrap();
        assert_eq!(queued, 6);
        until(&d, "rebuilt", async || {
            d.memory_index_status().await.unwrap().backlog == 0
                && index.refs(UNGROUPED_COLLECTION) == ids
        })
        .await;
    })
    .await;
}

#[tokio::test]
async fn without_an_index_nothing_is_queued_for_one_and_rebuild_says_why() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = crate::testing::desk_on(pool, dir.path()).await;
        let status = d.memory_index_status().await.unwrap();
        assert!(!status.installed);
        assert!(matches!(
            d.rebuild_memory_index().await,
            Err(super::DeskError::Invalid(_))
        ));
    })
    .await;
}

#[tokio::test]
async fn a_full_memory_holds_the_harvest_and_the_floor_says_so_first() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let web = session(dir.path(), "acme-web");
        store.put(&web).await.unwrap();
        let index = Arc::new(FakeIndex::default());
        let (d, _cancel) = start(&pool, dir.path(), &index).await;
        // The worker's own first measure, at start, is in.
        until(&d, "measured", async || {
            d.0.index.disk.lock().bytes.is_some()
        })
        .await;
        let mb = 1024 * 1024;
        let max = d.0.opts.memory.max_disk_mb;
        d.disk_measured(max * mb / 2);
        assert!(d.memory_needs().is_empty());
        d.disk_measured(max * mb * 95 / 100);
        let needs = d.memory_needs();
        assert_eq!(needs.len(), 1);
        assert_eq!(needs[0].ask, crate::memory::NEARLY_FULL);
        assert_eq!(
            needs[0].answer,
            crate::floor::Answer::Reply,
            "nothing to click but freeing room"
        );
        let since = needs[0].since;
        d.disk_measured(max * mb);
        let needs = d.memory_needs();
        assert_eq!(
            (needs[0].ask.as_str(), needs[0].since),
            (crate::memory::FULL, since)
        );
        let m = d.session_memory(web.id).await.unwrap();
        assert_eq!(
            (m.remembering, m.why.as_deref()),
            (false, Some(crate::memory::FULL))
        );
        d.disk_measured(0);
        assert!(d.memory_needs().is_empty());
    })
    .await;
}

async fn reads(store: &DeskStore, reader: &str) -> i64 {
    q::<i64>(
        store,
        "SELECT COUNT(*) FROM memory_reads WHERE reader = $1",
        &[&reader],
    )
    .await
}

fn query(q: &str) -> crate::memory_store::MemoryQuery {
    crate::memory_store::MemoryQuery {
        q: q.into(),
        limit: 10,
        ..Default::default()
    }
}

#[tokio::test]
async fn the_person_searches_the_index_and_keywords_stand_in_when_it_is_down() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let web = session(dir.path(), "acme-web");
        store.put(&web).await.unwrap();
        let index = Arc::new(FakeIndex::default());
        index.down.store(true, Ordering::SeqCst);
        let (d, _cancel) = start(&pool, dir.path(), &index).await;
        harvested(&d, &store, web.id, 6).await;

        // Not written yet, and the index down: by keyword, and said so.
        until(&d, "the outage is said", async || {
            d.memory_index_status().await.unwrap().failing.is_some()
        })
        .await;
        let found = d.memory_search(&query("cookie")).await.unwrap();
        assert!(found.keyword_only);
        assert_eq!(found.notice, Some(crate::memory::KEYWORD_ONLY));
        assert!(found.hits.iter().any(|h| h.hit.answer.contains("HttpOnly")));
        assert_eq!(reads(&store, "person").await, 1, "every search is a read");

        index.down.store(false, Ordering::SeqCst);
        until(&d, "indexed", async || {
            d.memory_index_status().await.unwrap().backlog == 0
                && d.memory_index_status().await.unwrap().failing.is_none()
        })
        .await;
        let found = d.memory_search(&query("cookie HttpOnly")).await.unwrap();
        assert!(!found.keyword_only);
        assert_eq!(found.notice, None);
        assert!(
            found.hits[0].hit.answer.contains("HttpOnly"),
            "{:?}",
            found.hits
        );
        assert_eq!(found.hits[0].name.as_deref(), Some("acme-web"));

        // A row Postgres no longer has, its deletion not yet run in the
        // index, is never returned.
        index.down.store(true, Ordering::SeqCst);
        {
            let client = store.pool().get().await.unwrap();
            client
                .execute(
                    "DELETE FROM memory_exchanges WHERE answer LIKE '%HttpOnly%'",
                    &[],
                )
                .await
                .unwrap();
        }
        index.down.store(false, Ordering::SeqCst);
        let found = d.memory_search(&query("cookie HttpOnly")).await.unwrap();
        assert!(found
            .hits
            .iter()
            .all(|h| !h.hit.answer.contains("HttpOnly")));

        // Something new, not yet in the index, is found by its words.
        index.down.store(true, Ordering::SeqCst);
        {
            let client = store.pool().get().await.unwrap();
            client
                .execute(
                    "INSERT INTO memory_exchanges (id, engine, key, session, project, folder, seq, \
                       at, ended, asker, ask, answer, steps) VALUES (gen_random_uuid(), 'claude', \
                       'k-new', $1, '/dev/acme', '/dev/acme', 99, now(), now(), 'person', \
                       'rotate the signing keys', 'Rotated.', '[]')",
                    &[&web.id],
                )
                .await
                .unwrap();
        }
        index.down.store(false, Ordering::SeqCst);
        let found = d.memory_search(&query("signing keys")).await.unwrap();
        if !found.keyword_only {
            assert_eq!(found.notice, Some(crate::memory::CATCHING_UP));
        }
        assert_eq!(found.hits.len(), 1);
        assert_eq!(found.hits[0].hit.ask, "rotate the signing keys");
    })
    .await;
}

#[tokio::test]
async fn a_session_never_recalls_another_groups_memory_whatever_it_asks() {
    use super::memory_recall::Recall;
    use crate::memory::reach::{Reach, Reader};
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let (acme, globex) = (
            session(dir.path(), "acme-web"),
            session(dir.path(), "globex-web"),
        );
        for r in [&acme, &globex] {
            store.put(r).await.unwrap();
        }
        // Each in its client's group, as the sidebar keeps them.
        {
            let client = store.pool().get().await.unwrap();
            for (i, (id, g)) in [(acme.id, "g-acme"), (globex.id, "g-globex")].into_iter().enumerate() {
                let position = i32::try_from(i).unwrap();
                client
                    .execute(
                        "INSERT INTO groups (id, name, position, updated_at) VALUES ($1, $1, $2, now())",
                        &[&g, &position],
                    )
                    .await
                    .unwrap();
                client
                    .execute(
                        "INSERT INTO group_members (item, group_id, position) VALUES ($1, $2, 0)",
                        &[&format!("session:{id}"), &g],
                    )
                    .await
                    .unwrap();
            }
        }
        let index = Arc::new(FakeIndex::default());
        let (d, _cancel) = start(&pool, dir.path(), &index).await;
        harvested(&d, &store, acme.id, 6).await;
        harvested(&d, &store, globex.id, 6).await;
        until(&d, "indexed", async || {
            let s = d.memory_index_status().await.unwrap();
            s.backlog == 0 && s.failing.is_none()
        })
        .await;
        assert_eq!(index.refs(&collection_of(Some("g-globex"))), closed(&store, globex.id).await);
        let reader = Reader::Session {
            id: acme.id,
            group: Some("g-acme".into()),
            project: dir.path().to_string_lossy().into_owned(),
            work: Vec::new(),
        };
        for down in [false, true] {
            index.down.store(down, Ordering::SeqCst);
            for (reach, group, session) in [
                (None, None, None),
                (Some(Reach::Group), None, None),
                (Some(Reach::Group), Some(Some("g-globex".to_string())), None),
                (Some(Reach::Group), None, Some(globex.id)),
            ] {
                let got = d
                    .memory_recall(
                        &reader,
                        &Recall {
                            q: "cookie HttpOnly".into(),
                            reach,
                            group,
                            session,
                            limit: 10,
                            tool: "recall",
                        },
                    )
                    .await
                    .unwrap();
                assert!(
                    got.hits
                        .iter()
                        .all(|(r, _)| r.session == acme.id && r.group.as_deref() == Some("g-acme")),
                    "down: {down}, {reach:?}"
                );
            }
        }
        index.down.store(false, Ordering::SeqCst);
        // Its own are found; the person finds both.
        let own = d
            .memory_recall(
                &reader,
                &Recall {
                    q: "HttpOnly".into(),
                    limit: 10,
                    tool: "recall",
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(!own.hits.is_empty());
        let all = d.memory_search(&query("HttpOnly")).await.unwrap();
        let sessions: std::collections::HashSet<Uuid> =
            all.hits.iter().map(|h| h.hit.session).collect();
        assert_eq!(sessions.len(), 2);
        // Every search is a read, but one asking only for another group's
        // has nothing to search.
        assert_eq!(reads(&store, &format!("session:{}", acme.id)).await, 7);
    })
    .await;
}
