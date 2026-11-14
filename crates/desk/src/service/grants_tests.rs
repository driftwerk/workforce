//! Tests of grants on Desk's side (remote bosses, RB1): only the hub's own
//! token writes them; on a machine they, not its old layout, say its
//! sessions' groups, and the first one restamps its rows once; on the
//! person's own computer they never touch the sidebar; and a stop or the
//! pause holds through whatever the home sends after.

use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use serde_json::json;
use uuid::Uuid;
use workforce_postgres::test_support::with_test_pool;

use super::*;
use crate::grant::{BossOptions, StoppedFrom};
use crate::store::build_pool;

async fn engine(pool: workforce_postgres::PostgresPool, dir: &Path, machine: bool) -> Desk {
    DeskStore::migrate(&pool).await.unwrap();
    let mut opts = crate::testing::options(dir);
    opts.bosses = BossOptions {
        machine,
        grant_hours: 72,
    };
    Desk::start(
        DeskStore::from_pool(pool),
        opts,
        Default::default(),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap()
}

fn body(revision: i64, name: &str, members: &[Uuid]) -> serde_json::Value {
    json!({
        "grantVersion": 1,
        "revision": revision,
        "name": name,
        "boss": "remote:driftwerk/allan",
        "level": "suggest",
        "sees": "replies",
        "members": members,
    })
}

fn native() -> Native {
    Native::routes()
}

async fn put(d: &Desk, group: &str, b: serde_json::Value) -> GrantWritten {
    d.put_grant(native(), group, b).await.unwrap()
}

async fn sql(d: &Desk, q: &str, p: &[&(dyn tokio_postgres::types::ToSql + Sync)]) -> u64 {
    d.store()
        .pool()
        .get()
        .await
        .unwrap()
        .execute(q, p)
        .await
        .unwrap()
}

async fn groups_of(d: &Desk, table: &str, s: Uuid) -> Vec<Option<String>> {
    d.store()
        .pool()
        .get()
        .await
        .unwrap()
        .query(
            &format!("SELECT group_id FROM {table} WHERE session = $1 ORDER BY 1"),
            &[&s],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

/// A ledger row and a remembered exchange of `s`, written an hour ago in
/// `group`.
async fn rows_of(d: &Desk, s: Uuid, group: &str, key: &str) {
    let at = Utc::now() - chrono::Duration::hours(1);
    sql(
        d,
        "INSERT INTO ledger (engine, key, session, at, priced, billing, group_id) \
         VALUES ('claude', $1, $2, $3, 'none', 'plan', $4)",
        &[&key, &s, &at, &group],
    )
    .await;
    sql(
        d,
        "INSERT INTO memory_exchanges (id, engine, key, session, group_id, project, folder, seq, \
           at, ended, asker, ask, answer, steps, indexed_at) \
         VALUES (gen_random_uuid(), 'claude', $1, $2, $4, 'acme-api', '/src/acme-api', 1, $3, $3, \
           'person', 'what broke', 'the build', '[]', $3)",
        &[&key, &s, &at, &group],
    )
    .await;
}

/// How a row written now for `s` would be stamped.
async fn stamped_now(d: &Desk, s: Uuid) -> Option<String> {
    let q = format!("SELECT {}", crate::store::group_at_sql("$1::uuid", "now()"));
    d.store()
        .pool()
        .get()
        .await
        .unwrap()
        .query_one(&q, &[&s])
        .await
        .unwrap()
        .get(0)
}

/// The machine's own old layout: Acme (its id, not the Mac's) holding `s`.
async fn old_layout(d: &Desk, s: &[Uuid]) {
    let items: Vec<String> = s.iter().map(|s| session_item(*s)).collect();
    d.change_layout(|l| l.create("old-acme", "Acme", &items, None))
        .await
        .unwrap();
}

#[tokio::test]
async fn on_the_persons_own_computer_a_grant_never_touches_the_sidebar() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = engine(pool, dir.path(), false).await;
        let (s, other) = (Uuid::new_v4(), Uuid::new_v4());
        old_layout(&d, &[s]).await;
        rows_of(&d, s, "old-acme", "k-1").await;
        let before = d.layout();
        let w = put(&d, "g-acme", body(1, "Acme", &[s, other])).await;
        assert_eq!(w.restamped, 0);
        assert!(w.grant.live, "{:?}", w.grant.why);
        assert_eq!(d.layout(), before, "the person's sidebar is theirs");
        assert!(!d.stamps_from_grants());
        assert!(d.layout_refused().is_none());
        assert_eq!(groups_of(&d, "ledger", s).await, [Some("old-acme".into())]);
        assert_eq!(stamped_now(&d, s).await.as_deref(), Some("old-acme"));
        assert!(d.grant_live("g-acme").is_ok());
    })
    .await;
}

#[tokio::test]
async fn a_machines_rows_follow_its_grants_and_the_first_restamps_them_once() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = engine(pool, dir.path(), true).await;
        let (api, web, loose) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        old_layout(&d, &[api, web, loose]).await;
        for (s, k) in [(api, "k-api"), (web, "k-web"), (loose, "k-loose")] {
            rows_of(&d, s, "old-acme", k).await;
        }
        sql(
            &d,
            "INSERT INTO budgets (id, scope, group_id, period, unit, amount, updated_at) \
             VALUES (gen_random_uuid(), 'desk', 'old-acme', 'day', 'usd', 25, now())",
            &[],
        )
        .await;
        // Acme had a looser budget of its own here already.
        sql(
            &d,
            "INSERT INTO budgets (id, scope, group_id, period, unit, amount, on_exceed, updated_at) \
             VALUES (gen_random_uuid(), 'desk', 'g-acme', 'day', 'usd', 100, 'warn', now())",
            &[],
        )
        .await;
        // Before any grant, the machine is as it was.
        assert!(!d.stamps_from_grants());

        let w = put(&d, "g-acme", body(1, "Acme", &[api, web])).await;
        assert_eq!(w.restamped, 2);
        assert!(d.stamps_from_grants());
        // Its groups are the grants': the old Acme is gone.
        let ids: Vec<String> = d.layout().groups.into_iter().map(|g| g.id).collect();
        assert_eq!(ids, ["g-acme"]);
        assert_eq!(
            d.group_of_member(&session_item(api)).as_deref(),
            Some("g-acme")
        );
        assert_eq!(d.group_of_member(&session_item(loose)), None);
        // Rows written from now on are stamped from the grant.
        assert_eq!(stamped_now(&d, api).await.as_deref(), Some("g-acme"));
        assert_eq!(stamped_now(&d, loose).await, None);
        // What was written before is restamped: ledger and memory, which is
        // indexed again in its new group and taken out of its old one.
        for t in ["ledger", "memory_exchanges"] {
            assert_eq!(groups_of(&d, t, api).await, [Some("g-acme".into())], "{t}");
            assert_eq!(
                groups_of(&d, t, loose).await,
                [Some("old-acme".into())],
                "{t}"
            );
        }
        let pending: i64 = d
            .store()
            .pool()
            .get()
            .await
            .unwrap()
            .query_one(
                "SELECT COUNT(*) FROM memory_exchanges WHERE indexed_at IS NULL",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(pending, 2);
        let purges: i64 = d
            .store()
            .pool()
            .get()
            .await
            .unwrap()
            .query_one(
                "SELECT COUNT(*) FROM memory_purges WHERE collection = $1",
                &[&crate::memory::passage::collection_of(Some("old-acme"))],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(purges, 1);
        // Its old group's budget goes along: the limit is kept.
        let budgets: i64 = d
            .store()
            .pool()
            .get()
            .await
            .unwrap()
            .query_one(
                "SELECT COUNT(*) FROM budgets WHERE group_id = 'g-acme' AND amount = 25 \
                 AND on_exceed = 'pause'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(budgets, 1);
        // Its sidebar is the home's to change, not the page's.
        assert!(matches!(d.layout_refused(), Some(DeskError::Conflict(_))));

        // The same grant again restamps nothing.
        let again = put(&d, "g-acme", body(1, "Acme", &[api, web])).await;
        assert_eq!(again.restamped, 0);
        assert_eq!(again.change, crate::grant::Change::Renewed);

        // web moves to Globex: what it did in Acme stays Acme's.
        put(&d, "g-acme", body(2, "Acme", &[api])).await;
        let globex = put(&d, "g-globex", body(1, "Globex", &[web, loose])).await;
        assert_eq!(globex.restamped, 1, "loose alone is new to grants");
        assert_eq!(groups_of(&d, "ledger", web).await, [Some("g-acme".into())]);
        assert_eq!(stamped_now(&d, web).await.as_deref(), Some("g-globex"));
        assert_eq!(
            groups_of(&d, "ledger", loose).await,
            [Some("g-globex".into())]
        );

        // Old Acme's sessions are in two groups now: its budget is split
        // between them, never doubled.
        let amounts: Vec<(String, f64)> = d
            .store()
            .pool()
            .get()
            .await
            .unwrap()
            .query(
                "SELECT group_id, amount FROM budgets WHERE group_id IN ('g-acme', 'g-globex') \
                 ORDER BY group_id",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| (r.get(0), r.get(1)))
            .collect();
        assert_eq!(
            amounts,
            [("g-acme".to_string(), 12.5), ("g-globex".to_string(), 12.5)]
        );

        // Taken out of every grant, nothing written before goes anywhere.
        d.delete_grant(native(), "g-globex").await.unwrap();
        assert_eq!(groups_of(&d, "ledger", web).await, [Some("g-acme".into())]);
        assert_eq!(
            groups_of(&d, "ledger", loose).await,
            [Some("g-globex".into())]
        );
        assert_eq!(d.group_of_member(&session_item(web)), None);
        put(&d, "g-atlas", body(1, "Atlas", &[web, loose])).await;
        assert_eq!(groups_of(&d, "ledger", web).await, [Some("g-acme".into())]);
        assert_eq!(
            groups_of(&d, "ledger", loose).await,
            [Some("g-globex".into())]
        );
    })
    .await;
}

#[tokio::test]
async fn a_session_named_by_one_grant_is_refused_by_another() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = engine(pool, dir.path(), true).await;
        let s = Uuid::new_v4();
        put(&d, "g-acme", body(1, "Acme", &[s])).await;
        let e = d
            .put_grant(native(), "g-globex", body(1, "Globex", &[s]))
            .await
            .unwrap_err();
        assert!(matches!(e, DeskError::Conflict(_)), "{e}");
        assert_eq!(
            d.group_of_member(&session_item(s)).as_deref(),
            Some("g-acme")
        );
        // An older revision is refused too, and audited.
        put(&d, "g-acme", body(3, "Acme", &[s])).await;
        assert!(d
            .put_grant(native(), "g-acme", body(2, "Acme", &[]))
            .await
            .is_err());
        let calls = d.store().boss_calls_latest(10).await.unwrap();
        assert!(calls
            .iter()
            .any(|(tool, outcome, caller, _)| tool == "grant.put"
                && outcome == "refused"
                && caller == "desk"));
    })
    .await;
}

#[tokio::test]
async fn a_stop_holds_through_the_homes_next_sync_and_the_pause_through_a_restart() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = engine(pool.clone(), dir.path(), true).await;
        let s = Uuid::new_v4();
        put(&d, "g-acme", body(1, "Acme", &[s])).await;
        assert!(d.grant_live("g-acme").is_ok());

        d.stop_bosses(native(), Some("g-acme"), StoppedFrom::Machine)
            .await
            .unwrap();
        let why = d.grant_live("g-acme").unwrap_err();
        assert!(why.contains("stopped on this machine"), "{why}");
        // Desk's next sync renews it, then revises it: still stopped.
        put(&d, "g-acme", body(1, "Acme", &[s])).await;
        put(&d, "g-acme", body(2, "Acme", &[s])).await;
        assert!(d.grant_live("g-acme").is_err());
        // A stopped grant still says who is in the group.
        assert_eq!(
            d.group_of_member(&session_item(s)).as_deref(),
            Some("g-acme")
        );
        let mut resume = body(3, "Acme", &[s]);
        resume["resume"] = json!(true);
        put(&d, "g-acme", resume).await;
        assert!(d.grant_live("g-acme").is_ok());

        // Stop all: every boss here paused, and nothing the home writes
        // clears it.
        d.stop_bosses(native(), None, StoppedFrom::Machine)
            .await
            .unwrap();
        assert!(d.bosses_paused());
        let mut resume = body(4, "Acme", &[s]);
        resume["resume"] = json!(true);
        put(&d, "g-acme", resume).await;
        assert!(d.grant_live("g-acme").unwrap_err().contains("paused"));

        // It is kept: the engine comes back paused.
        let again = engine(pool.clone(), dir.path(), true).await;
        assert!(again.bosses_paused());
        assert!(again.stamps_from_grants());
        assert!(again
            .grant_view(&native(), "g-acme")
            .unwrap()
            .grant
            .members
            .contains(&s));

        again.clear_bosses_pause(native()).await.unwrap();
        assert!(again.grant_live("g-acme").is_ok());
        let calls = again.store().boss_calls_latest(20).await.unwrap();
        for tool in ["bosses.stop", "bosses.pause_clear", "grant.put"] {
            assert!(calls.iter().any(|c| c.0 == tool), "{tool}: {calls:?}");
        }
    })
    .await;
}

#[tokio::test]
async fn a_grant_of_an_unknown_version_serves_nothing_and_keeps_its_members() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = engine(pool, dir.path(), true).await;
        let s = Uuid::new_v4();
        put(&d, "g-acme", body(1, "Acme", &[s])).await;
        let mut v2 = body(2, "Acme", &[]);
        v2["grantVersion"] = json!(2);
        let e = d.put_grant(native(), "g-acme", v2).await.unwrap_err();
        assert!(e.to_string().contains("serves nothing"), "{e}");
        assert!(d.grant_live("g-acme").unwrap_err().contains("version"));
        // No version, known or not, stores a revision past the cap.
        for (version, revision) in [(2, i64::MAX), (2, 0), (1, crate::grant::MAX_REVISION + 1)] {
            let mut b = body(1, "Acme", &[s]);
            b["grantVersion"] = json!(version);
            b["revision"] = json!(revision);
            assert!(d.put_grant(native(), "g-acme", b).await.is_err());
        }
        assert_eq!(
            d.grant_view(&native(), "g-acme").unwrap().grant.revision,
            2,
            "the copy keeps its own"
        );
        put(&d, "g-acme", body(3, "Acme", &[s])).await;
        assert_eq!(
            d.group_of_member(&session_item(s)).as_deref(),
            Some("g-acme")
        );
    })
    .await;
}

#[tokio::test]
async fn a_session_started_on_a_granted_machine_is_born_in_its_group() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = engine(pool, dir.path(), true).await;
        put(&d, "g-acme", body(1, "Acme", &[])).await;
        let s = Uuid::new_v4();
        assert!(d.born_in(s, "g-acme").await.unwrap());
        assert_eq!(
            d.group_of_member(&session_item(s)).as_deref(),
            Some("g-acme")
        );
        assert!(matches!(
            d.born_in(Uuid::new_v4(), "g-globex").await,
            Err(DeskError::NotFound)
        ));
        // The home's next write that doesn't list it keeps it born.
        put(&d, "g-acme", body(2, "Acme", &[])).await;
        assert_eq!(
            d.group_of_member(&session_item(s)).as_deref(),
            Some("g-acme")
        );
    })
    .await;
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<serde_json::Value>,
) -> (u16, serde_json::Value) {
    use tower::ServiceExt;
    let mut req = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(b) = bearer {
        req = req.header("authorization", format!("Bearer {b}"));
    }
    let response = app
        .clone()
        .oneshot(
            req.body(body.map_or_else(axum::body::Body::empty, |b| {
                axum::body::Body::from(b.to_string())
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn only_the_hubs_own_token_in_its_header_writes_or_reads_grants() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = engine(pool, dir.path(), true).await;
        let s = Uuid::new_v4();
        let uri = "/api/desk/grants/g-acme";
        let b = body(1, "Acme", &[s]);

        // A signed-in administrator, or anyone the hub doesn't let in.
        for auth in [
            crate::testing::identified("acme-admin"),
            crate::testing::deny_all(),
        ] {
            let app = crate::api::sessions_router(d.clone(), auth);
            for (method, path) in [
                ("PUT", uri),
                ("GET", uri),
                ("DELETE", uri),
                ("GET", "/api/desk/grants"),
                ("POST", "/api/desk/bosses/stop"),
                ("DELETE", "/api/desk/bosses/paused"),
            ] {
                let (code, out) = call(&app, method, path, Some("x"), Some(b.clone())).await;
                assert_eq!(code, 403, "{method} {path}: {out}");
            }
        }
        // The hub's token in the address, which logs keep.
        let app = crate::api::sessions_router(d.clone(), Arc::new(crate::testing::Bearer));
        let (code, _) = call(
            &app,
            "PUT",
            &format!("{uri}?token=hub-token"),
            None,
            Some(b.clone()),
        )
        .await;
        assert_eq!(code, 400);
        let (code, _) = call(&app, "PUT", uri, Some("hub-token-not"), Some(b.clone())).await;
        assert_eq!(code, 403);
        assert!(
            d.grant_view(&native(), "g-acme").is_none(),
            "nothing written"
        );

        // The hub's own token, in its header.
        let (code, out) = call(&app, "PUT", uri, Some("hub-token"), Some(b.clone())).await;
        assert_eq!(code, 200, "{out}");
        assert_eq!(out["grant"]["members"], json!([s]));
        assert_eq!(out["grant"]["live"], json!(true));
        let (code, out) = call(&app, "GET", "/api/desk/bosses", Some("hub-token"), None).await;
        assert_eq!(code, 200);
        assert!(
            out["said"][0].as_str().unwrap().starts_with("Acme: "),
            "{out}"
        );
        // The sidebar's routes are refused on this machine now.
        let (code, out) = call(
            &app,
            "POST",
            "/api/desk/groups",
            Some("hub-token"),
            Some(json!({"name": "Globex"})),
        )
        .await;
        assert_eq!(code, 409, "{out}");
        let (code, out) = call(
            &app,
            "POST",
            "/api/desk/bosses/stop",
            Some("hub-token"),
            Some(json!({"group": "g-acme"})),
        )
        .await;
        assert_eq!(code, 200, "{out}");
        assert_eq!(out["grants"][0]["live"], json!(false));
        let (code, _) = call(&app, "DELETE", uri, Some("hub-token"), None).await;
        assert_eq!(code, 204);
        let (code, _) = call(&app, "GET", uri, Some("hub-token"), None).await;
        assert_eq!(code, 404);
    })
    .await;
}

#[tokio::test]
async fn grants_that_cant_be_read_pause_the_bosses_and_take_no_new_one() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let first = engine(pool.clone(), dir.path(), true).await;
        sql(&first, "DROP TABLE boss_grants", &[]).await;
        let d = engine(pool, dir.path(), true).await;
        assert!(d.bosses_paused());
        // Its sidebar stays closed, as a granted machine's.
        assert!(d.stamps_from_grants());
        assert!(d.layout_refused().is_some());
        let e = d
            .put_grant(native(), "g-acme", body(1, "Acme", &[Uuid::new_v4()]))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("can't read the grants"), "{e}");
        assert!(d.clear_bosses_pause(native()).await.is_err());
        assert!(d.bosses_paused());
        // A stop still works, one group's too: kept as the pause.
        d.stop_bosses(native(), Some("g-acme"), StoppedFrom::Machine)
            .await
            .unwrap();
        d.stop_bosses(native(), None, StoppedFrom::Machine)
            .await
            .unwrap();
        assert!(d.bosses_paused());
    })
    .await;
}

#[tokio::test]
async fn on_a_granted_machine_only_a_grant_moves_a_session_between_groups() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = engine(pool, dir.path(), true).await;
        let s = super::tests::new_session(&d, dir.path()).await.id;
        put(&d, "g-acme", body(1, "Acme", &[s])).await;
        put(&d, "g-globex", body(1, "Globex", &[])).await;
        let item = session_item(s);
        for change in [
            Box::new(|l: &Layout| l.put(&item, "g-globex", None))
                as Box<dyn FnOnce(&Layout) -> Result<Layout, DeskError> + Send>,
            Box::new(|l: &Layout| l.remove(&item)),
        ] {
            let e = d.change_layout(change).await.unwrap_err();
            assert!(matches!(e, DeskError::Conflict(_)), "{e}");
        }
        assert_eq!(d.group_of_member(&item).as_deref(), Some("g-acme"));
        // Showing a group folded changes no one's group.
        d.change_layout(|l| l.edit("g-acme", None, Some(true)))
            .await
            .unwrap();
        // Ungrouped is where a session starts with no grant to say otherwise.
        assert!(!d.born_in(Uuid::new_v4(), UNGROUPED).await.unwrap());
    })
    .await;
}

#[tokio::test]
async fn a_deleted_grant_keeps_its_revision_and_its_stop() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = engine(pool.clone(), dir.path(), true).await;
        let s = Uuid::new_v4();
        put(&d, "g-acme", body(3, "Acme", &[s])).await;
        d.stop_bosses(native(), Some("g-acme"), StoppedFrom::Machine)
            .await
            .unwrap();
        d.delete_grant(native(), "g-acme").await.unwrap();
        // No way back to it at its revision or below, after a restart too.
        let d = engine(pool, dir.path(), true).await;
        for rev in [1, 3] {
            let e = d
                .put_grant(native(), "g-acme", body(rev, "Acme", &[s]))
                .await
                .unwrap_err();
            assert!(matches!(e, DeskError::Conflict(_)), "{e}");
        }
        // A newer one keeps the stop until Desk resumes it.
        put(&d, "g-acme", body(4, "Acme", &[s])).await;
        assert!(d
            .grant_live("g-acme")
            .unwrap_err()
            .contains("stopped on this machine"));
        let mut resume = body(5, "Acme", &[s]);
        resume["resume"] = json!(true);
        put(&d, "g-acme", resume).await;
        assert!(d.grant_live("g-acme").is_ok());
    })
    .await;
}

#[tokio::test]
async fn a_stop_racing_a_delete_or_ahead_of_any_grant_is_kept() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = engine(pool, dir.path(), true).await;
        put(&d, "g-acme", body(1, "Acme", &[])).await;
        // A stop in force but not yet written when the delete runs.
        let stop = crate::grant::Stopped {
            at: Utc::now(),
            from: StoppedFrom::Machine,
        };
        d.0.grants.lock().stopping.insert("g-acme".into(), stop);
        d.delete_grant(native(), "g-acme").await.unwrap();
        put(&d, "g-acme", body(2, "Acme", &[])).await;
        assert!(d.grant_live("g-acme").is_err(), "the stop came along");

        // A stop of a group with no grant waits for its first.
        let view = d
            .stop_bosses(native(), Some("g-globex"), StoppedFrom::Desk)
            .await
            .unwrap();
        assert!(view.left["g-globex"].stopped.is_some());
        put(&d, "g-globex", body(1, "Globex", &[])).await;
        assert!(d
            .grant_live("g-globex")
            .unwrap_err()
            .contains("stopped in Desk"));

        // Stopping what is stopped already is still audited.
        let before = d.store().boss_calls_latest(100).await.unwrap();
        d.stop_bosses(native(), Some("g-globex"), StoppedFrom::Desk)
            .await
            .unwrap();
        let after = d.store().boss_calls_latest(100).await.unwrap();
        assert_eq!(after.len(), before.len() + 1);
        assert_eq!(after[0].0, "bosses.stop");
    })
    .await;
}

#[tokio::test]
async fn a_session_the_home_puts_in_ungrouped_keeps_its_rows_where_they_were() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = engine(pool, dir.path(), true).await;
        let s = Uuid::new_v4();
        old_layout(&d, &[s]).await;
        rows_of(&d, s, "old-acme", "k-1").await;
        let mut ungrouped = body(1, "Ungrouped", &[s]);
        ungrouped["boss"] = json!("local:desk");
        let w = put(&d, UNGROUPED, ungrouped).await;
        // A restamp only narrows: its rows stay where they were written.
        assert_eq!(w.restamped, 0);
        assert_eq!(groups_of(&d, "ledger", s).await, [Some("old-acme".into())]);
        assert_eq!(
            groups_of(&d, "memory_exchanges", s).await,
            [Some("old-acme".into())]
        );
        assert_eq!(d.group_of_member(&session_item(s)), None);
        assert_eq!(stamped_now(&d, s).await, None);
    })
    .await;
}

#[tokio::test]
async fn a_session_that_didnt_start_is_born_in_no_grant() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = engine(pool, dir.path(), true).await;
        put(&d, "g-acme", body(1, "Acme", &[])).await;
        let s = Uuid::new_v4();
        assert!(d.born_in(s, "g-acme").await.unwrap());
        d.born_undo(s).await;
        assert!(d
            .grant_view(&native(), "g-acme")
            .unwrap()
            .grant
            .born
            .is_empty());
        assert_eq!(d.group_of_member(&session_item(s)), None);
    })
    .await;
}

#[tokio::test]
async fn a_codex_session_is_never_handed_to_a_served_boss_even_stopped() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let first = engine(pool.clone(), dir.path(), false).await;
        let mut codex = crate::store::tests::record_fixture("acme-codex");
        codex.engine = crate::model::Engine::Codex;
        codex.state = crate::model::SessionState::Stopped;
        first.store().put(&codex).await.unwrap();
        let mut claude = crate::store::tests::record_fixture("acme-web");
        claude.state = crate::model::SessionState::Stopped;
        first.store().put(&claude).await.unwrap();
        let d = engine(pool, dir.path(), false).await;
        for _ in 0..200 {
            if d.charges_loaded() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let allan = crate::crew::Party::manager("driftwerk/allan", "Allan");
        let handed = |id| {
            let d = d.clone();
            let allan = allan.clone();
            async move {
                d.take_on(
                    id,
                    &allan,
                    crate::charge::How::Handed,
                    &crate::scope::Scope::Ungrouped,
                    &crate::crew::By::you(),
                )
                .await
            }
        };
        let e = handed(codex.id).await.unwrap_err();
        assert!(e.to_string().contains("checks every call"), "{e}");
        let ok = handed(claude.id).await;
        assert!(ok.is_ok(), "{ok:?}");
    })
    .await;
}

/// At Act fully, a grant as the routes show it says what its sessions here
/// spent today, as its daily budget counts it: only calls a key paid for.
/// Below Act fully it says nothing.
#[tokio::test]
async fn a_grant_at_act_fully_says_what_its_sessions_spent_today() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = engine(pool, dir.path(), true).await;
        let s = Uuid::new_v4();
        let mut full = body(1, "Acme", &[s]);
        full["level"] = json!("full");
        full["sees"] = json!("everything");
        full["act"] = json!({"budget_usd_per_day": 20.0});
        put(&d, "g-acme", full).await;
        put(&d, "g-docs", body(1, "Docs", &[Uuid::new_v4()])).await;
        let at = Utc::now();
        // Its own key-paid call counts; a plan's, another group's and one
        // in no group don't.
        let other = Uuid::new_v4();
        for (key, usd, billing, group) in [
            ("k1", 1.25, "key", Some("g-acme")),
            ("k2", 5.0, "plan", Some("g-acme")),
            ("k3", 7.0, "key", Some("g-docs")),
            ("k4", 9.0, "key", None),
        ] {
            let session = if group == Some("g-acme") { s } else { other };
            sql(
                &d,
                "INSERT INTO ledger (engine, key, session, at, usd, priced, billing, group_id) \
                 VALUES ('claude', $1, $2, $3, $4, 'reported', $5, $6)",
                &[&key, &session, &at, &usd, &billing, &group],
            )
            .await;
        }
        let app = crate::api::sessions_router(d.clone(), Arc::new(crate::testing::Bearer));
        let (code, out) = call(&app, "GET", "/api/desk/grants", Some("hub-token"), None).await;
        assert_eq!(code, 200, "{out}");
        let of = |group: &str| {
            out["grants"]
                .as_array()
                .unwrap()
                .iter()
                .find(|g| g["group"] == group)
                .cloned()
                .unwrap_or_else(|| panic!("{out}"))
        };
        assert_eq!(of("g-acme")["spentTodayUsd"], json!(1.25), "{out}");
        assert!(of("g-docs").get("spentTodayUsd").is_none(), "{out}");
        let (code, one) = call(
            &app,
            "GET",
            "/api/desk/grants/g-acme",
            Some("hub-token"),
            None,
        )
        .await;
        assert_eq!(code, 200, "{one}");
        assert_eq!(one["spentTodayUsd"], json!(1.25), "{one}");
    })
    .await;
}
