//! The artifact mirror's writes (DW-676, PR 4): a second index, vellev's,
//! kept from its own pending list beside the serving one.

use super::*;

/// What a pending list holds: each artifact noted, and the hash noted.
async fn listed(store: &KnowledgeStore, table: &str) -> Vec<(String, String)> {
    let conn = store.pool.get().await.unwrap();
    let rows = conn
        .query(
            &format!("SELECT artifact_id, text_hash FROM {table} ORDER BY artifact_id"),
            &[],
        )
        .await
        .unwrap();
    rows.iter().map(|row| (row.get(0), row.get(1))).collect()
}

async fn mirrored(store: &KnowledgeStore) -> Vec<(String, String)> {
    listed(store, "artifact_index_pending_mirror").await
}

async fn serving(store: &KnowledgeStore) -> Vec<(String, String)> {
    listed(store, "artifact_index_pending").await
}

const HOUR: std::time::Duration = std::time::Duration::from_secs(3600);

/// End the lease as a hub that stopped renewing it leaves it: lapsed.
async fn lapse(store: &KnowledgeStore) {
    sql(
        store,
        "UPDATE shadow_control SET mirrored_until = now() - interval '1 second'",
    )
    .await;
}

/// Whether the lease holds now.
async fn leased(store: &KnowledgeStore) -> bool {
    let conn = store.pool.get().await.unwrap();
    conn.query_one(
        "SELECT coalesce(mirrored_until > now(), false) FROM shadow_control",
        &[],
    )
    .await
    .unwrap()
    .get(0)
}

async fn sql(store: &KnowledgeStore, sql: &str) {
    let conn = store.pool.get().await.unwrap();
    conn.batch_execute(sql).await.unwrap();
}

fn alice(id: &str) -> Artifact {
    artifact(id, "alice", Visibility::Public)
}

/// The writes a user makes, each noted for the serving index whatever the
/// mirror's state: an artifact made, changed and deleted.
async fn writes_noted(store: &KnowledgeStore, id: &str, state: &str) {
    let owner = Caller::user("alice");
    store
        .create(&alice(id))
        .await
        .unwrap_or_else(|e| panic!("{state}: create failed: {e:#}"));
    store
        .update(id, "changed", "alice", &owner)
        .await
        .unwrap_or_else(|e| panic!("{state}: update failed: {e:#}"));
    assert!(
        serving(store).await.iter().any(|(noted, _)| noted == id),
        "{state}: the serving index was not noted"
    );
    store
        .delete(id, &owner)
        .await
        .unwrap_or_else(|e| panic!("{state}: delete failed: {e:#}"));
    assert!(
        serving(store)
            .await
            .contains(&(id.to_string(), String::new())),
        "{state}: the delete was not noted"
    );
}

/// No state of the mirror's can fail a user's write or keep it from being
/// noted for the index the hub serves: off, on, its control row gone, and
/// on with its list dropped by hand.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_is_noted_for_the_serving_index_whatever_the_mirror_state() {
    with_store(|store| async move {
        writes_noted(&store, "w0", "off, as made").await;
        sql(&store, "SELECT artifact_mirror_renew(interval '1 hour')").await;
        writes_noted(&store, "w1", "on").await;
        sql(&store, "DELETE FROM shadow_control").await;
        writes_noted(&store, "w2", "no control row").await;
        // Turning it on puts the row back, and writes are noted again.
        sql(&store, "SELECT artifact_mirror_renew(interval '1 hour')").await;
        store.create(&alice("w2b")).await.unwrap();
        assert!(
            mirrored(&store).await.iter().any(|(id, _)| id == "w2b"),
            "not noted once turned on again"
        );
        sql(&store, "DROP TABLE artifact_index_pending_mirror").await;
        writes_noted(&store, "w3", "on, the mirror's list dropped").await;
    })
    .await;
}

/// With no lease, nothing is noted for the mirror; leased, every write is,
/// with the hash the serving list has; lapsed, nothing more.
#[tokio::test(flavor = "multi_thread")]
async fn writes_are_noted_for_the_mirror_only_while_it_is_on() {
    with_store(|store| async move {
        let owner = Caller::user("alice");
        store.create(&alice("a")).await.unwrap();
        assert!(mirrored(&store).await.is_empty(), "noted while off");

        store.renew_mirror_lease(HOUR).await.unwrap();
        store.create(&alice("b")).await.unwrap();
        store.update("a", "changed", "alice", &owner).await.unwrap();
        let noted = mirrored(&store).await;
        assert_eq!(noted, serving(&store).await, "the same as the serving list");
        assert_eq!(
            noted.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );

        lapse(&store).await;
        store.create(&alice("c")).await.unwrap();
        assert!(
            !mirrored(&store).await.iter().any(|(id, _)| id == "c"),
            "noted while off again"
        );
    })
    .await;
}

/// The flag cannot ask the trigger to note into a list that is not there.
#[tokio::test(flavor = "multi_thread")]
async fn the_mirror_cannot_be_turned_on_without_its_list() {
    with_store(|store| async move {
        sql(&store, "DROP TABLE artifact_index_pending_mirror").await;
        let refused = store.renew_mirror_lease(HOUR).await.unwrap_err();
        assert!(
            format!("{refused:#}").contains("artifact_index_pending_mirror"),
            "{refused:#}"
        );
        let conn = store.pool.get().await.unwrap();
        let held: bool = conn
            .query_one("SELECT mirrored_until IS NOT NULL FROM shadow_control", &[])
            .await
            .unwrap()
            .get(0);
        assert!(!held);
    })
    .await;
}

/// Two indexes, each with its keeper: the serving one's notes are taken by
/// its keeper alone, and the mirror's by the mirror's alone, whichever
/// index is down meanwhile.
#[tokio::test(flavor = "multi_thread")]
async fn neither_keeper_takes_the_others_notes() {
    workforce_postgres::test_support::with_test_pool(
        KnowledgeStore::build_pool,
        |pool| async move {
            let weaviate = Held::on(&pool);
            let vellev = Held::on(&pool);
            let store = KnowledgeStore::new(pool.clone(), Some(weaviate.clone()))
                .await
                .unwrap();
            let mirror = store
                .open_mirror(pool.clone(), vellev.clone(), HOUR)
                .await
                .unwrap();
            store.keep_index_complete(QUICK);
            mirror.keep_index_complete(QUICK);
            // Both keepers' first compares over before anything is written:
            // a compare that read the index before a write was taken could
            // note it again after (harmless, and not what this test is of).
            until("both first compares", || {
                store.index_state().reconciled_at.is_some()
                    && mirror.index_state().reconciled_at.is_some()
            })
            .await;

            // The mirror down: the serving index takes the write, and the
            // mirror's note waits for it.
            vellev.down.store(true, Ordering::SeqCst);
            store.create(&alice("a")).await.unwrap();
            until_none_pending(&store).await;
            assert!(weaviate.content("a").is_some());
            assert_eq!(
                mirrored(&store)
                    .await
                    .iter()
                    .map(|(id, _)| id.as_str())
                    .collect::<Vec<_>>(),
                ["a"],
                "the serving keeper took the mirror's note"
            );

            // The serving index down: the mirror takes both, and the serving
            // note for the new one waits.
            weaviate.down.store(true, Ordering::SeqCst);
            vellev.down.store(false, Ordering::SeqCst);
            store.create(&alice("b")).await.unwrap();
            let begun = std::time::Instant::now();
            while !mirrored(&store).await.is_empty() {
                assert!(
                    begun.elapsed() < std::time::Duration::from_secs(10),
                    "the mirror did not catch up"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(vellev.content("a").is_some() && vellev.content("b").is_some());
            assert_eq!(
                serving(&store)
                    .await
                    .iter()
                    .map(|(id, _)| id.as_str())
                    .collect::<Vec<_>>(),
                ["b"],
                "the mirror's keeper took the serving note"
            );
            assert!(weaviate.content("b").is_none());
        },
    )
    .await;
}

/// A write through the serving store wakes the mirror's keeper, which
/// would otherwise wait out its watch (an hour here): the mirror is not
/// left a watch behind.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_wakes_the_mirror_keeper() {
    workforce_postgres::test_support::with_test_pool(
        KnowledgeStore::build_pool,
        |pool| async move {
            let weaviate = Held::on(&pool);
            let vellev = Held::on(&pool);
            let store = KnowledgeStore::new(pool.clone(), Some(weaviate.clone()))
                .await
                .unwrap();
            let mirror = store
                .open_mirror(pool.clone(), vellev.clone(), HOUR)
                .await
                .unwrap();
            mirror.keep_index_complete(QUICK);
            // Its first round compared (nothing to do) and it is waiting.
            until("the mirror's first round", || vellev.is_complete()).await;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;

            store.create(&alice("a")).await.unwrap();
            until("the mirror indexed the write", || {
                vellev.content("a").is_some()
            })
            .await;
        },
    )
    .await;
}

/// Seeding the mirror fills it and leaves both pending lists as they were:
/// the serving keeper's notes are not its to take.
#[tokio::test(flavor = "multi_thread")]
async fn seeding_the_mirror_leaves_both_lists_alone() {
    workforce_postgres::test_support::with_test_pool(
        KnowledgeStore::build_pool,
        |pool| async move {
            let vellev = Held::on(&pool);
            let store = KnowledgeStore::new(pool.clone(), None).await.unwrap();
            store.renew_mirror_lease(HOUR).await.unwrap();
            stored(&store, 3).await;
            let (serving_before, mirrored_before) = (serving(&store).await, mirrored(&store).await);
            assert_eq!(serving_before.len(), 3);

            let mirror = MirrorStore::seeding(pool.clone(), vellev.clone())
                .await
                .unwrap();
            let done = mirror.seed_index(|_, _| {}).await.unwrap();
            assert_eq!(done.indexed, 3);
            assert_eq!(vellev.ids_held().len(), 3);
            assert_eq!(serving(&store).await, serving_before);
            assert_eq!(mirrored(&store).await, mirrored_before);
        },
    )
    .await;
}

/// The mirror's compares run under a lock of their own: one goes ahead
/// while the serving index's compare lock is held elsewhere.
#[tokio::test(flavor = "multi_thread")]
async fn the_mirror_compares_while_the_serving_compare_lock_is_held() {
    workforce_postgres::test_support::with_test_pool(
        KnowledgeStore::build_pool,
        |pool| async move {
            let vellev = Held::on(&pool);
            let store = KnowledgeStore::new(pool.clone(), None).await.unwrap();
            stored(&store, 2).await;
            let holder = deadpool_postgres::Object::take(pool.get().await.unwrap());
            holder
                .execute("SELECT pg_advisory_lock($1)", &[&BACKFILL_LOCK])
                .await
                .unwrap();
            let mirror = store
                .open_mirror(pool.clone(), vellev.clone(), HOUR)
                .await
                .unwrap();
            mirror.keep_index_complete(QUICK);
            until("the mirror compared and filled", || {
                vellev.is_complete() && vellev.ids_held().len() == 2
            })
            .await;
            drop(holder);
        },
    )
    .await;
}

/// Tags the serving index adds are written back to the artifact, which
/// notes it afresh for the mirror too: that wakes the mirror's keeper as
/// well, so it is not left with the earlier version for a watch.
#[tokio::test(flavor = "multi_thread")]
async fn tags_written_back_after_a_write_wake_the_mirror_keeper() {
    workforce_postgres::test_support::with_test_pool(
        KnowledgeStore::build_pool,
        |pool| async move {
            let weaviate = Held::on(&pool);
            let vellev = Held::on(&pool);
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            *weaviate.tag_gate.lock().unwrap() = Some(Arc::clone(&gate));
            *weaviate.tagging.lock().unwrap() = Some("added");
            let store = KnowledgeStore::new(pool.clone(), Some(weaviate.clone()))
                .await
                .unwrap();
            let mirror = store
                .open_mirror(pool.clone(), vellev.clone(), HOUR)
                .await
                .unwrap();
            mirror.keep_index_complete(QUICK);
            until("the mirror's first round", || vellev.is_complete()).await;

            store.create(&alice("a")).await.unwrap();
            // The write's own version, before its tags are written back.
            until("the mirror took the write", || {
                vellev.content("a").is_some()
            })
            .await;
            let begun = std::time::Instant::now();
            while !mirrored(&store).await.is_empty() {
                assert!(begun.elapsed() < std::time::Duration::from_secs(10));
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let tagged = |vellev: &Held| {
                vellev
                    .held
                    .lock()
                    .unwrap()
                    .get("a")
                    .is_some_and(|vector| vector.tags.iter().any(|tag| tag == "added"))
            };
            assert!(!tagged(&vellev));

            gate.add_permits(1);
            until("the mirror took the tags written back", || tagged(&vellev)).await;
        },
    )
    .await;
}

// ---- The lease (B1), its keeper (B2) and the keeper's progress (B3) ---------
//
// These run at a watch of 1 s (a lease of 3 s) and give each outcome tens of
// seconds: under load a shorter span lapses the lease for real (B4).

/// The watch the lease tests run at: a 3 s lease.
const WATCH: std::time::Duration = std::time::Duration::from_secs(1);

/// As [`until`], given 30 s: the lease tests wait out several spans.
async fn until_long(what: &str, met: impl Fn() -> bool) {
    let begun = std::time::Instant::now();
    while !met() {
        assert!(
            begun.elapsed() < std::time::Duration::from_secs(30),
            "not within 30 s: {what}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// When the lease ends, as the database has it.
async fn lease_end(store: &KnowledgeStore) -> Option<DateTime<Utc>> {
    let conn = store.pool.get().await.unwrap();
    conn.query_one("SELECT mirrored_until FROM shadow_control", &[])
        .await
        .unwrap()
        .get(0)
}

/// The mirror kept at the lease tests' pace, its tries given `deadline`.
async fn kept_mirror(
    store: &KnowledgeStore,
    pool: &PostgresPool,
    vellev: &Arc<Held>,
    deadline: std::time::Duration,
) -> MirrorStore {
    let mirror = store
        .open_mirror(pool.clone(), vellev.clone(), WATCH * LEASE_WATCHES)
        .await
        .unwrap();
    mirror.keep_index_complete(IndexPace {
        watch: WATCH,
        deadline,
        ..QUICK
    });
    mirror
}

/// The lease's end stops moving: over three spans it is renewed no more,
/// and has lapsed.
async fn lease_stops(store: &KnowledgeStore) {
    let span = WATCH * LEASE_WATCHES;
    let last = lease_end(store).await;
    tokio::time::sleep(span * 3).await;
    assert_eq!(lease_end(store).await, last, "still renewed");
    assert!(!leased(store).await, "not lapsed");
}

/// Not renewed, the lease lapses by itself, and writes are no longer noted
/// for the mirror: abandoning the shadow needs nothing turned off.
#[tokio::test(flavor = "multi_thread")]
async fn the_lease_lapses_when_it_is_not_renewed() {
    with_store(|store| async move {
        store
            .renew_mirror_lease(std::time::Duration::from_secs(3))
            .await
            .unwrap();
        store.create(&alice("a")).await.unwrap();
        assert!(mirrored(&store).await.iter().any(|(id, _)| id == "a"));
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        assert!(!leased(&store).await);
        store.create(&alice("b")).await.unwrap();
        assert!(
            !mirrored(&store).await.iter().any(|(id, _)| id == "b"),
            "noted after the lease lapsed"
        );
    })
    .await;
}

/// The mirror's keeper renews the lease, every watch for three watches,
/// for as long as it runs: its end keeps moving, and it never lapses.
#[tokio::test(flavor = "multi_thread")]
async fn the_mirror_keeper_renews_the_lease() {
    workforce_postgres::test_support::with_test_pool(
        KnowledgeStore::build_pool,
        |pool| async move {
            let vellev = Held::on(&pool);
            let store = KnowledgeStore::new(pool.clone(), None).await.unwrap();
            let mirror = kept_mirror(&store, &pool, &vellev, WATCH).await;
            let first = lease_end(&store).await;
            let begun = std::time::Instant::now();
            while begun.elapsed() < WATCH * 8 {
                assert!(leased(&store).await, "lapsed after {:?}", begun.elapsed());
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            assert!(lease_end(&store).await > first, "never renewed");
            assert_eq!(mirror.lease().lapses, 0);
            assert!(mirror.lease().keeper_alive && !mirror.lease().keeper_stalled);
        },
    )
    .await;
}

/// A lease found lapsed after the keeper held it (renewals stalled, or
/// ended by hand) is said and counted, where health reads it, and taken
/// again.
#[tokio::test(flavor = "multi_thread")]
async fn a_lapsed_lease_is_counted_where_health_reads_it() {
    workforce_postgres::test_support::with_test_pool(
        KnowledgeStore::build_pool,
        |pool| async move {
            let vellev = Held::on(&pool);
            let store = KnowledgeStore::new(pool.clone(), None).await.unwrap();
            let mirror = kept_mirror(&store, &pool, &vellev, WATCH).await;
            assert_eq!(mirror.lease().lapses, 0);
            lapse(&store).await;
            until_long("the lapse counted", || mirror.lease().lapses == 1).await;
            let begun = std::time::Instant::now();
            while !leased(&store).await {
                assert!(
                    begun.elapsed() < std::time::Duration::from_secs(30),
                    "not taken again"
                );
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        },
    )
    .await;
}

/// A renewal never shortens a lease another hub renewed for longer: two
/// shadow hubs at different paces each keep it at least as long as their
/// own span.
#[tokio::test(flavor = "multi_thread")]
async fn a_renewal_never_shortens_the_lease() {
    with_store(|store| async move {
        store.renew_mirror_lease(HOUR).await.unwrap();
        store
            .renew_mirror_lease(std::time::Duration::from_secs(1))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        assert!(leased(&store).await, "shortened by a shorter renewal");
    })
    .await;
}

/// A keeper that dies (here by a panic in its round) stops the lease being
/// renewed: its end stops moving over several spans and it lapses, writes
/// stop being noted with nobody to take them, and health says the keeper
/// is not running.
#[tokio::test(flavor = "multi_thread")]
async fn a_keeper_that_panics_lets_the_lease_lapse() {
    workforce_postgres::test_support::with_test_pool(
        KnowledgeStore::build_pool,
        |pool| async move {
            let vellev = Held::on(&pool);
            let store = KnowledgeStore::new(pool.clone(), None).await.unwrap();
            let mirror = kept_mirror(&store, &pool, &vellev, WATCH).await;
            until_long("the mirror's first round", || vellev.is_complete()).await;
            assert!(mirror.lease().keeper_alive);

            vellev.panics_on_look.store(true, Ordering::SeqCst);
            // A write wakes the keeper, whose round now panics.
            store.create(&alice("a")).await.unwrap();
            until_long("the keeper said dead", || !mirror.lease().keeper_alive).await;
            // Not renewed from the next tick on: well within the stall bound
            // (its try deadline and three watches), so for its death itself,
            // not for the stall that follows it.
            tokio::time::sleep(WATCH + WATCH / 2).await;
            let last = lease_end(&store).await;
            tokio::time::sleep(WATCH * 2).await;
            assert_eq!(lease_end(&store).await, last, "renewed for a dead keeper");
            // Within one span it lapses; then its end stays put.
            tokio::time::sleep(WATCH * LEASE_WATCHES).await;
            lease_stops(&store).await;
            store.create(&alice("b")).await.unwrap();
            assert!(
                !mirrored(&store).await.iter().any(|(id, _)| id == "b"),
                "noted with nobody to take it"
            );
        },
    )
    .await;
}

/// A keeper that runs and is stuck, here on a look at the index that never
/// returns, is as bad as one that died: once it is past when it was to
/// make progress (its try deadline and three watches), the lease is not
/// renewed, and health says it is stalled.
#[tokio::test(flavor = "multi_thread")]
async fn a_keeper_that_makes_no_progress_lets_the_lease_lapse() {
    workforce_postgres::test_support::with_test_pool(
        KnowledgeStore::build_pool,
        |pool| async move {
            let vellev = Held::on(&pool);
            let store = KnowledgeStore::new(pool.clone(), None).await.unwrap();
            let mirror = kept_mirror(&store, &pool, &vellev, WATCH).await;
            until_long("the mirror's first round", || vellev.is_complete()).await;

            vellev.look_hangs.store(true, Ordering::SeqCst);
            // A write wakes the keeper, whose round now never ends.
            store.create(&alice("a")).await.unwrap();
            until_long("the keeper said stalled", || mirror.lease().keeper_stalled).await;
            assert!(mirror.lease().keeper_alive, "stuck, not dead");
            tokio::time::sleep(WATCH * LEASE_WATCHES + WATCH).await;
            lease_stops(&store).await;
        },
    )
    .await;
}

/// A keeper waiting by its own choice, longer than any step may take
/// (here its index down, retried after 6 s against a stall bound of 4 s),
/// is not taken for stuck, and its lease is renewed meanwhile. (Whether the
/// lease ever lapsed here is Postgres's speed under load, not the keeper's:
/// these tests are of the keeper.)
#[tokio::test(flavor = "multi_thread")]
async fn a_keeper_backing_off_is_not_taken_for_stuck() {
    workforce_postgres::test_support::with_test_pool(
        KnowledgeStore::build_pool,
        |pool| async move {
            let vellev = Held::on(&pool);
            let store = KnowledgeStore::new(pool.clone(), None).await.unwrap();
            let mirror = store
                .open_mirror(pool.clone(), vellev.clone(), WATCH * LEASE_WATCHES)
                .await
                .unwrap();
            vellev.down.store(true, Ordering::SeqCst);
            mirror.keep_index_complete(IndexPace {
                watch: WATCH,
                deadline: WATCH,
                retry: std::time::Duration::from_secs(6),
                ..QUICK
            });
            let first_end = lease_end(&store).await;
            let begun = std::time::Instant::now();
            while begun.elapsed() < std::time::Duration::from_secs(14) {
                assert!(
                    !mirror.lease().keeper_stalled,
                    "taken for stuck after {:?}",
                    begun.elapsed()
                );
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            assert!(
                lease_end(&store).await > first_end,
                "the lease was never renewed meanwhile"
            );
        },
    )
    .await;
}

/// A round of many tries, longer in all than any one step may take (ten
/// artifacts at 0.7 s each against a stall bound of 4 s), is progress try
/// by try: the keeper is not taken for stuck, and the lease is kept.
#[tokio::test(flavor = "multi_thread")]
async fn a_long_round_of_tries_is_progress() {
    workforce_postgres::test_support::with_test_pool(
        KnowledgeStore::build_pool,
        |pool| async move {
            let vellev = Held::on(&pool);
            let store = KnowledgeStore::new(pool.clone(), None).await.unwrap();
            let mirror = kept_mirror(&store, &pool, &vellev, WATCH).await;
            until_long("the mirror's first round", || vellev.is_complete()).await;
            *vellev.slow.lock().unwrap() = Some(std::time::Duration::from_millis(700));
            for n in 0..10 {
                store.create(&alice(&format!("r{n}"))).await.unwrap();
            }
            let first_end = lease_end(&store).await;
            let begun = std::time::Instant::now();
            while vellev.ids_held().len() < 10 {
                assert!(
                    !mirror.lease().keeper_stalled,
                    "taken for stuck after {:?}",
                    begun.elapsed()
                );
                assert!(
                    begun.elapsed() < std::time::Duration::from_secs(60),
                    "never took them all"
                );
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            assert!(
                lease_end(&store).await > first_end,
                "the lease was never renewed meanwhile"
            );
        },
    )
    .await;
}

/// A keeper meeting a lock held by a session left open (the mirror's row
/// of an artifact the index keeps failing on, held FOR UPDATE) is not held
/// by it for ever: its statements are given up by Postgres, its rounds go
/// on, the lease stays held, and once the lock goes the artifact is taken.
#[tokio::test(flavor = "multi_thread")]
async fn a_keeper_meeting_a_held_lock_keeps_going() {
    let config = Arc::new(std::sync::Mutex::new(None));
    let kept = Arc::clone(&config);
    workforce_postgres::test_support::with_test_pool(
        move |pg| {
            *kept.lock().unwrap() = Some(pg.clone());
            KnowledgeStore::build_pool(pg)
        },
        |pool| async move {
            let pg = config.lock().unwrap().clone().unwrap();
            let bounded =
                KnowledgeStore::build_mirror_pool(&pg, std::time::Duration::from_millis(500))
                    .unwrap();
            let vellev = Held::on(&pool);
            let store = KnowledgeStore::new(pool.clone(), None).await.unwrap();
            let mirror = kept_mirror(&store, &bounded, &vellev, WATCH).await;
            until_long("the mirror's first round", || vellev.is_complete()).await;

            // The index fails on it, and a session holds its row.
            vellev.failing.lock().unwrap().insert("a".into(), usize::MAX);
            store.create(&alice("a")).await.unwrap();
            let holder = pool.get().await.unwrap();
            holder
                .batch_execute(
                    "BEGIN; SELECT * FROM artifact_index_pending_mirror WHERE artifact_id = 'a' FOR UPDATE",
                )
                .await
                .unwrap();
            let first_end = lease_end(&store).await;
        let begun = std::time::Instant::now();
            while begun.elapsed() < WATCH * 12 {
                assert!(
                    !mirror.lease().keeper_stalled,
                    "stuck on the lock after {:?}",
                    begun.elapsed()
                );
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            assert!(
                lease_end(&store).await > first_end,
                "the lease was never renewed meanwhile"
            );
            holder.batch_execute("COMMIT").await.unwrap();
            vellev.failing.lock().unwrap().clear();
            until_long("taken once the lock went", || vellev.content("a").is_some()).await;
        },
    )
    .await;
}

/// A lapse found by a renewal has the mirror compared at once, finding the
/// writes made meanwhile, rather than at the next hourly reconcile. The
/// write here changes an artifact the mirror holds, so the counts stay in
/// step: only a compare by text finds it.
#[tokio::test(flavor = "multi_thread")]
async fn a_lapse_found_has_the_mirror_compared_at_once() {
    workforce_postgres::test_support::with_test_pool(
        KnowledgeStore::build_pool,
        |pool| async move {
            let vellev = Held::on(&pool);
            let store = KnowledgeStore::new(pool.clone(), None).await.unwrap();
            let mirror = kept_mirror(&store, &pool, &vellev, WATCH).await;
            store.create(&alice("x")).await.unwrap();
            until_long("the mirror took it", || vellev.content("x").is_some()).await;
            until_long("the first compare", || {
                mirror.index_state().reconciled_at.is_some()
            })
            .await;

            // Lapsed, and a change made meanwhile, not noted for the mirror.
            lapse(&store).await;
            store
                .update(
                    "x",
                    "changed while the lease had lapsed",
                    "alice",
                    &Caller::user("alice"),
                )
                .await
                .unwrap();
            until_long("the lapse found", || mirror.lease().lapses == 1).await;
            until_long("the change found, well before the hour", || {
                vellev.content("x").as_deref() == Some("changed while the lease had lapsed")
            })
            .await;
        },
    )
    .await;
}

/// A renewal stuck on a lock gives up within its limit (a third of its
/// span, at least a second), to be tried again at the next tick, rather
/// than holding the ticker.
#[tokio::test(flavor = "multi_thread")]
async fn a_renewal_stuck_on_a_lock_gives_up() {
    with_store(|store| async move {
        let holder = store.pool.get().await.unwrap();
        holder
            .batch_execute("BEGIN; LOCK TABLE shadow_control IN ACCESS EXCLUSIVE MODE")
            .await
            .unwrap();
        let begun = std::time::Instant::now();
        let renewed = store
            .renew_mirror_lease(std::time::Duration::from_secs(3))
            .await;
        assert!(renewed.is_err());
        assert!(
            begun.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            begun.elapsed()
        );
        holder.batch_execute("COMMIT").await.unwrap();
        assert!(store.renew_mirror_lease(HOUR).await.is_ok());
    })
    .await;
}

/// Whether the lease had lapsed, and its new end, are the database's: never
/// held, it had; held, it had not; past, it had.
#[tokio::test(flavor = "multi_thread")]
async fn a_renewal_says_by_the_databases_clock_whether_the_lease_had_lapsed() {
    with_store(|store| async move {
        let first = store.renew_mirror_lease(HOUR).await.unwrap();
        assert!(first.had_lapsed, "never held");
        let second = store.renew_mirror_lease(HOUR).await.unwrap();
        assert!(!second.had_lapsed);
        assert!(second.until >= first.until);
        lapse(&store).await;
        assert!(store.renew_mirror_lease(HOUR).await.unwrap().had_lapsed);
    })
    .await;
}

/// A renewal reads the lease as it stands once a change to it is
/// committed (FOR UPDATE): a lease ended in a transaction still open when
/// the renewal comes is seen ended, and the renewal says it had lapsed,
/// rather than reading the lease from before the change and healing the
/// lapse unseen.
#[tokio::test(flavor = "multi_thread")]
async fn a_renewal_sees_a_lease_ended_while_it_waited() {
    with_store(|store| async move {
        store.renew_mirror_lease(HOUR).await.unwrap();
        let holder = store.pool.get().await.unwrap();
        holder
            .batch_execute("BEGIN; UPDATE shadow_control SET mirrored_until = NULL")
            .await
            .unwrap();
        let renewing = {
            let store = Arc::clone(&store);
            tokio::spawn(async move { store.renew_mirror_lease(HOUR).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        holder.batch_execute("COMMIT").await.unwrap();
        let renewal = renewing.await.unwrap().unwrap();
        assert!(
            renewal.had_lapsed,
            "the lease's end before the change was read"
        );
    })
    .await;
}

/// A compare is progress page by page: one whose pages are slow (five at
/// 1.5 s, against a stall bound of 4 s) is not taken for stuck while it
/// runs.
#[tokio::test(flavor = "multi_thread")]
async fn a_compare_of_slow_pages_is_progress() {
    workforce_postgres::test_support::with_test_pool(
        KnowledgeStore::build_pool,
        |pool| async move {
            let vellev = Held::on(&pool);
            let store = KnowledgeStore::new(pool.clone(), None).await.unwrap();
            // Stored before the mirror's lease is taken: its only work is the
            // first compare, five pages of a hundred.
            stored(&store, 450).await;
            *vellev.slow_pages.lock().unwrap() = Some(std::time::Duration::from_millis(1500));
            let mirror = kept_mirror(&store, &pool, &vellev, WATCH).await;
            let begun = std::time::Instant::now();
            while mirror.index_state().reconciled_at.is_none() {
                assert!(
                    !mirror.lease().keeper_stalled,
                    "taken for stuck after {:?} of comparing",
                    begun.elapsed()
                );
                assert!(
                    begun.elapsed() < std::time::Duration::from_secs(60),
                    "the compare never ended"
                );
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            assert!(
                begun.elapsed() > std::time::Duration::from_secs(5),
                "the compare outran the stall bound's test: {:?}",
                begun.elapsed()
            );
        },
    )
    .await;
}
