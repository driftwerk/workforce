//! The relay (remote bosses, RB3): Sarah, Acme's boss on the Mac, reaches
//! Acme's sessions on devbox through Desk's native side (stood in for here
//! by a poller), and devbox decides each call under its own grant.

use super::*;
use workforce_desk::grant::BossRef;

use super::super::relay::{RelayAnswer, RelayCall};
use super::super::served::ServedStatus;

const SARAH: &str = "[oversight]\nenabled = true\ngroup = \"acme\"\nlevel = \"act\"\n[oversight.act]\ninterrupt = true\n";

async fn post(app: &axum::Router, uri: &str, bearer: &str, body: Value) -> (StatusCode, Value) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("authorization", format!("Bearer {bearer}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// The hub's own token, as Desk's native side holds it.
struct HubToken;

#[async_trait::async_trait]
impl Authorize for HubToken {
    async fn authorize(
        &self,
        _: &HeaderMap,
        _: Option<&str>,
    ) -> Result<Option<String>, StatusCode> {
        Ok(None)
    }
    fn is_hub_token(&self, _: &HeaderMap) -> bool {
        true
    }
}

async fn put_grant(d: &Desk, boss: &str, members: &[uuid::Uuid]) {
    let app = workforce_desk::sessions_router(d.clone(), Arc::new(HubToken));
    let res = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/desk/grants/acme")
                .header("authorization", "Bearer hub")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"grantVersion": 1, "revision": 1, "name": "Acme", "boss": boss,
                           "level": "act", "sees": "replies", "members": members,
                           "act": {"interrupt": true}})
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

/// On devbox: what the home relays is decided under devbox's grant, for
/// the home's own boss alone, each call once.
#[tokio::test(flavor = "multi_thread")]
async fn a_machine_serves_the_homes_boss_its_grant_names_and_nothing_else() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        // devbox runs no agents of its own that oversee.
        let (_any, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = false\n",
            &[],
        )
        .await;
        let s = new_session(&d, dir.path(), "acme-api").await;
        let item = workforce_desk::session_item(s.id);
        d.change_layout(|l| l.create("acme", "Acme", std::slice::from_ref(&item), None))
            .await
            .unwrap();
        put_grant(&d, "local:sarah", &[s.id]).await;
        let app = super::super::relay::routes(Arc::clone(&crew.state), Arc::clone(&crew));
        let token = crew.state.auth_token.clone();
        let call = |boss: &str, id: &str| {
            json!({"group": "acme", "tool": "crew.overview", "input": {}, "boss": boss,
                   "linkId": "home-relay", "id": id, "level": "act", "act": {"interrupt": true}})
        };

        let (code, out) = post(
            &app,
            "/api/desk/crew/served",
            &token,
            call("local:sarah", "c-1"),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{out}");
        assert_eq!(out["status"], "ok", "{out}");
        assert!(out["body"]["sessions"]
            .as_str()
            .unwrap()
            .contains(&s.id.to_string()));

        // Another boss than the grant's, or one on another hub, is not.
        let (_, out) = post(
            &app,
            "/api/desk/crew/served",
            &token,
            call("local:mia", "c-2"),
        )
        .await;
        assert_eq!(out["status"], "refused", "{out}");
        let (code, _) = post(
            &app,
            "/api/desk/crew/served",
            &token,
            call("remote:driftwerk/sarah", "c-3"),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        // Its link and id are always said.
        let mut no_id = call("local:sarah", "");
        no_id["id"] = json!(" ");
        let (code, _) = post(&app, "/api/desk/crew/served", &token, no_id).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        // Only with the hub's own token, from its header.
        let (code, _) = post(
            &app,
            "/api/desk/crew/served",
            "not-it",
            call("local:sarah", "c-4"),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        let (code, _) = post(
            &app,
            &format!("/api/desk/crew/relay/{}", uuid::Uuid::new_v4()),
            "not-it",
            json!({"ok": {}}),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        let (code, _) = post(
            &app,
            &format!("/api/desk/crew/served?token={token}"),
            &token,
            call("local:sarah", "c-5"),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);

        // The home's own level is the most it does there: at Watch it does
        // nothing, whatever the grant says.
        let mut watching = call("local:sarah", "c-6");
        watching["tool"] = json!("crew.interrupt");
        watching["input"] = json!({"session_id": s.id, "reason": "Loops."});
        watching["level"] = json!("watch");
        let (_, out) = post(&app, "/api/desk/crew/served", &token, watching).await;
        assert_eq!(out["status"], "refused", "{out}");
        // Only on the home's relay link, and only with the level said.
        let mut other_link = call("local:sarah", "c-7");
        other_link["linkId"] = json!("link-of-mine");
        let (code, _) = post(&app, "/api/desk/crew/served", &token, other_link).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        let mut no_level = call("local:sarah", "c-8");
        no_level.as_object_mut().unwrap().remove("level");
        let (code, _) = post(&app, "/api/desk/crew/served", &token, no_level).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);

        // The same call sent again is never done twice.
        let (_, first) = post(
            &app,
            "/api/desk/crew/served",
            &token,
            call("local:sarah", "c-9"),
        )
        .await;
        let (_, again) = post(
            &app,
            "/api/desk/crew/served",
            &token,
            call("local:sarah", "c-9"),
        )
        .await;
        assert_eq!(first["status"], "ok");
        assert_eq!(again["body"]["again"], json!(true), "{again}");
    })
    .await;
}

/// On the Mac: Sarah's calls on devbox's sessions go to Desk's native side
/// and come back as devbox answered them; nothing of them is done here,
/// and nothing outside Acme is sent.
#[tokio::test(flavor = "multi_thread")]
async fn a_local_boss_reaches_its_groups_machine_sessions_through_desk() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (sarah, crew, d, _llm) = overseeing(dir.path(), pool, "ok", SARAH, &[]).await;
        let remote = uuid::Uuid::new_v4();
        let elsewhere = uuid::Uuid::new_v4();
        d.change_layout(|l| {
            l.create("acme", "Acme", &[format!("session:devbox/{remote}")], None)?
                .create("globex", "Globex", &[format!("session:devbox/{elsewhere}")], None)
        })
        .await
        .unwrap();
        eventually("Sarah's tools", async || sarah.tools.get("crew.interrupt").map(drop)).await;
        let tool = |name: &str| sarah.tools.get(name).unwrap();
        let interrupt = |id: String| json!({"session_id": id, "reason": "It loops."});

        // Desk isn't relaying: said at once, nothing queued.
        let r = tool("crew.interrupt")
            .execute(interrupt(format!("devbox/{remote}")))
            .await
            .unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("can't be reached"));

        // Desk's native side, standing by.
        let seen: Arc<parking_lot::Mutex<Vec<RelayCall>>> = Arc::default();
        let poller = {
            let (crew, seen) = (Arc::clone(&crew), Arc::clone(&seen));
            tokio::spawn(async move {
                loop {
                    for c in crew.relay_take(Duration::from_millis(200)).await {
                        seen.lock().push(c.clone());
                        let body = match c.tool.as_str() {
                            "crew.overview" => json!({"sessions": format!("[{{\"id\":\"{remote}\"}}]")}),
                            _ => json!({"done": "interrupted acme-api"}),
                        };
                        assert!(crew.relay_answer(
                            c.id,
                            RelayAnswer {
                                status: ServedStatus::Ok,
                                body
                            }
                        ));
                        // Answered once: a second answer is dropped.
                        assert!(!crew.relay_answer(
                            c.id,
                            RelayAnswer {
                                status: ServedStatus::Refused,
                                body: json!({})
                            }
                        ));
                    }
                }
            })
        };
        eventually("Desk polling", async || {
            crew.relay_polled.lock().is_some().then_some(())
        })
        .await;

        let r = tool("crew.interrupt")
            .execute(interrupt(format!("devbox/{remote}")))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        assert!(r.output["done"].as_str().unwrap().contains("interrupted acme-api"));
        let sent = seen.lock().last().cloned().unwrap();
        assert_eq!(sent.machine, "devbox");
        assert_eq!(sent.group, "acme");
        assert_eq!(sent.boss, BossRef::Local { agent: "desk".into() }.key());
        assert_eq!(sent.link_id, "home-relay");
        assert_eq!(sent.level, workforce_common::oversight::Level::Act);
        assert!(sent.act.interrupt);
        assert_eq!(sent.input["session_id"], json!(remote.to_string()));
        // It says when the Mac stops waiting for it, about its wait from now.
        let left = sent.deadline - chrono::Utc::now();
        assert!(left > chrono::Duration::seconds(20) && left <= chrono::Duration::seconds(30), "{left}");
        let wire = serde_json::to_value(&sent).unwrap();
        assert!(wire["deadline"].is_string(), "{wire}");

        // Too large to carry: refused here, nothing queued.
        let before = seen.lock().len();
        let r = tool("crew.interrupt")
            .execute(json!({"session_id": format!("devbox/{remote}"),
                            "reason": "x".repeat(300 * 1024)}))
            .await
            .unwrap();
        assert!(!r.success);
        assert!(r.error.as_deref().unwrap_or_default().contains("too large"), "{r:?}");
        // The whole call is measured, as Desk carries it: an input just under
        // the most, with the rest of the call, is over it.
        let r = tool("crew.interrupt")
            .execute(json!({"session_id": format!("devbox/{remote}"),
                            "reason": "x".repeat(256 * 1024 - 200)}))
            .await
            .unwrap();
        assert!(r.error.as_deref().unwrap_or_default().contains("too large"), "{r:?}");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(seen.lock().len(), before);

        // The overview has devbox's part of Acme, said as devbox said it.
        let r = tool("crew.overview").execute(json!({})).await.unwrap();
        assert!(r.success, "{r:?}");
        assert_eq!(r.output["machines"][0]["machine"], "devbox", "{r:?}");
        assert!(!super::super::relay::machine_ok(".devbox"));
        assert!(r.output["machines"][0]["sessions"].as_str().unwrap().contains(&remote.to_string()));

        // A session outside its group, or a tool not relayed, sends nothing.
        let before = seen.lock().len();
        let r = tool("crew.interrupt")
            .execute(interrupt(format!("devbox/{elsewhere}")))
            .await
            .unwrap();
        assert!(!r.success);
        let r = tool("crew.start_session")
            .execute(json!({"session_id": format!("devbox/{remote}"), "folder": "/x", "goal": "g", "reason": "r"}))
            .await
            .unwrap();
        assert!(!r.success);
        assert_eq!(seen.lock().len(), before);
        poller.abort();

        // Taken but never answered: said, in time.
        *crew.relay_wait.lock() = Duration::from_millis(300);
        let (taker, crew2) = (Arc::clone(&crew), Arc::clone(&crew));
        let took = tokio::spawn(async move { taker.relay_take(Duration::from_secs(5)).await });
        let r = tool("crew.interrupt")
            .execute(interrupt(format!("devbox/{remote}")))
            .await
            .unwrap();
        assert!(!r.success);
        // Desk took it: it may have been done, so it isn't said unreached.
        let said = r.error.unwrap();
        assert!(said.contains("outcome isn't known"), "{said}");
        // Its late answer goes nowhere.
        let late = took.await.unwrap();
        assert_eq!(late.len(), 1);
        assert!(!crew2.relay_answer(
            late[0].id,
            RelayAnswer {
                status: ServedStatus::Ok,
                body: json!({})
            }
        ));
    })
    .await;
}

async fn get(app: &axum::Router, uri: &str, bearer: &str) -> (StatusCode, Value) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header("authorization", format!("Bearer {bearer}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// On the Mac: each machine is to hold, for Acme's boss here, a grant
/// naming it `local:<agent>` with exactly its level, switches and Sees
/// here and only that machine's Acme sessions; a group with no boss of
/// its own gets none, and nothing is ever wider than the boss's own.
#[tokio::test(flavor = "multi_thread")]
async fn each_machine_is_to_hold_its_own_sessions_of_the_group_at_the_bosss_own_settings() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (sarah, crew, d, _llm) = overseeing(dir.path(), pool, "ok", SARAH, &[]).await;
        let (a, b, c) = (
            uuid::Uuid::from_u128(1),
            uuid::Uuid::from_u128(2),
            uuid::Uuid::from_u128(3),
        );
        let (other, loose) = (uuid::Uuid::from_u128(4), uuid::Uuid::from_u128(5));
        let here = new_session(&d, dir.path(), "acme-web").await;
        d.change_layout(|l| {
            l.create(
                "acme",
                "Acme",
                &[
                    format!("session:devbox/{b}"),
                    format!("session:devbox/{a}"),
                    workforce_desk::session_item(here.id),
                    format!("session:atlas/{c}"),
                ],
                None,
            )?
            .create(
                "globex",
                "Globex",
                &[format!("session:devbox/{other}")],
                None,
            )
        })
        .await
        .unwrap();
        d.change_layout(|l| {
            let mut l = l.clone();
            l.ungrouped.items.push(format!("session:devbox/{loose}"));
            Ok(l)
        })
        .await
        .unwrap();
        eventually("Acme's boss", async || {
            crew.overseer_named("desk").map(drop)
        })
        .await;

        let app = super::super::relay::routes(Arc::clone(&crew.state), Arc::clone(&crew));
        let token = crew.state.auth_token.clone();
        eventually("the grants", async || crew.machine_grants().map(drop)).await;
        let (code, w) = get(&app, "/api/desk/machine-grants", &token).await;
        assert_eq!(code, StatusCode::OK, "{w}");
        assert_eq!(w["grantVersion"], 1);
        let devbox = &w["machines"]["devbox"];
        let grants = devbox["grants"].as_array().unwrap();
        assert_eq!(grants.len(), 1, "Globex has no boss of its own: {w}");
        assert_eq!(grants[0]["group"], "acme");
        assert_eq!(grants[0]["agent"], "desk");
        let g = &grants[0]["grant"];
        assert_eq!(g["boss"], "local:desk");
        assert_eq!(g["level"], "act");
        assert_eq!(g["sees"], "everything");
        assert_eq!(g["members"], json!([a, b]), "devbox's Acme sessions alone");
        assert_eq!(g["act"]["interrupt"], json!(true));
        assert_eq!(g["act"]["parallel"], json!(4));
        assert!(g["act"].get("budget_usd_per_day").is_none());
        assert_eq!(
            (&g["folders"], &g["repos"], &g["recall"]),
            (&json!([]), &json!([]), &json!(false))
        );
        for never in ["resume", "hours", "revision", "leaving"] {
            assert!(g.get(never).is_none(), "{never}: {g}");
        }
        assert_eq!(devbox["listed"], json!([a, b, other, loose]));
        assert_eq!(
            w["machines"]["atlas"]["grants"][0]["grant"]["members"],
            json!([c])
        );
        // What it says is a grant the machine takes, once given a revision.
        let mut put = g.clone();
        put["revision"] = json!(1);
        serde_json::from_value::<workforce_desk::grant::GrantPut>(put).unwrap();

        // The boss's own settings are the grant's: Watch here, Watch there.
        write_config(
            &sarah,
            "[oversight]\nenabled = true\ngroup = \"acme\"\nlevel = \"watch\"\n",
        );
        eventually("Acme's boss watching", async || {
            (crew.machine_grants().unwrap().machines["devbox"].grants[0].grant["level"] == "watch")
                .then_some(())
        })
        .await;
        // Taken off the group: no grant is wanted for it anywhere.
        write_config(&sarah, "[oversight]\nenabled = false\n");
        eventually("Acme without a boss", async || {
            crew.machine_grants()
                .is_some_and(|w| w.machines.values().all(|m| m.grants.is_empty()))
                .then_some(())
        })
        .await;

        // Only with the hub's own token, from its header.
        let (code, _) = get(&app, "/api/desk/machine-grants", "not-it").await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        let (code, _) = get(
            &app,
            &format!("/api/desk/machine-grants?token={token}"),
            &token,
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
    })
    .await;
}

/// A group whose boss is on another hub is that boss's: no grant naming
/// a boss here is wanted for it, whoever oversees it here.
#[tokio::test(flavor = "multi_thread")]
async fn a_group_bossed_from_another_hub_is_never_wanted_for_a_boss_here() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_sarah, crew, d, _llm) = overseeing(dir.path(), pool, "ok", SARAH, &[]).await;
        let remote = uuid::Uuid::new_v4();
        d.change_layout(|l| l.create("acme", "Acme", &[format!("session:devbox/{remote}")], None))
            .await
            .unwrap();
        eventually("Acme's boss", async || {
            crew.overseer_named("desk").map(drop)
        })
        .await;
        eventually("the grants", async || crew.machine_grants().map(drop)).await;
        assert_eq!(
            crew.machine_grants().unwrap().machines["devbox"]
                .grants
                .len(),
            1
        );
        put_grant(&d, "remote:driftwerk/allan", &[]).await;
        assert!(crew.machine_grants().unwrap().machines["devbox"]
            .grants
            .is_empty());
    })
    .await;
}

/// Act fully here is never carried to a machine yet: each machine would
/// count its own spend against the whole budget. The grant acts within
/// Desk, with no budget and nothing to ship from, in force here or not.
#[tokio::test(flavor = "multi_thread")]
async fn act_fully_here_is_act_fully_on_a_machine_with_its_one_budget() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let full = "[oversight]\nenabled = true\ngroup = \"acme\"\nlevel = \"full\"\n\
                    [oversight.full]\nrepos = [\"acme/acme-web\"]\nbudget_usd_per_day = 25\n";
        let (sarah, crew, d, _llm) = overseeing(dir.path(), pool, "ok", full, &[]).await;
        let remote = uuid::Uuid::new_v4();
        d.change_layout(|l| l.create("acme", "Acme", &[format!("session:devbox/{remote}")], None))
            .await
            .unwrap();
        eventually("Acme's boss", async || {
            crew.overseer_named("desk").map(drop)
        })
        .await;
        let wanted = async || eventually("the grants", async || crew.machine_grants()).await;
        // Act fully as chosen, even before today's spend is read: whether
        // it is in force is each take's to say, so the grant isn't
        // rewritten (and the person asked again) as that changes.
        let w = wanted().await;
        let g = &w.machines["devbox"].grants[0].grant;
        assert_eq!(g["level"], "full", "{g}");
        assert_eq!(g["act"]["budget_usd_per_day"], 25.0);
        // Read: Act fully, with its budget, which the machines count once
        // with this computer.
        crew.read_spent_today(&crew.overseers()).await;
        assert_eq!(
            crew.level_now("desk", &crew.overseer_named("desk").unwrap().1)
                .0,
            workforce_common::oversight::Level::Full,
            "in force here"
        );
        let w = wanted().await;
        let wg = &w.machines["devbox"].grants[0];
        assert_eq!(wg.grant["level"], "full", "{}", wg.grant);
        assert_eq!(wg.grant["act"]["budget_usd_per_day"], 25.0);
        assert_eq!(
            wg.grant["repos"],
            json!([]),
            "Act fully ships nowhere from a grant"
        );
        assert!(
            wg.agent_since.is_some(),
            "this computer says when Sarah was made"
        );
        assert!(serde_json::from_value::<workforce_desk::grant::GrantPut>({
            let mut g = wg.grant.clone();
            g["revision"] = json!(1);
            g
        })
        .is_ok());
        // With no budget set here, the most Act fully allows.
        let none = "[oversight]\nenabled = true\ngroup = \"acme\"\nlevel = \"full\"\n";
        write_config(&sarah, none);
        let w = eventually("no budget", async || {
            crew.machine_grants().filter(|w| {
                w.machines["devbox"].grants[0].grant["act"]["budget_usd_per_day"]
                    == workforce_common::oversight::MAX_FULL_BUDGET_USD
            })
        })
        .await;
        assert_eq!(w.machines["devbox"].grants[0].grant["level"], "full");
    })
    .await;
}

/// Before this hub has read its agents, it says nothing of what machines
/// should hold, so Desk takes nothing off them.
#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_said_of_machine_grants_before_the_bosses_are_read() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let state = hub(&[]);
        let d = desk(pool, dir.path()).await;
        let crew = CrewHub::start(state, d, FAST, CancellationToken::new());
        assert!(crew.machine_grants().is_none());
        let app = super::super::relay::routes(Arc::clone(&crew.state), Arc::clone(&crew));
        let token = crew.state.auth_token.clone();
        let (code, _) = get(&app, "/api/desk/machine-grants", &token).await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    })
    .await;
}

/// At home: a job held by a session here or on a machine is one job, an
/// approval relayed to the machine carries the jobs held here, and a
/// relayed order is never settled as unsent for carrying no line message.
#[tokio::test(flavor = "multi_thread")]
async fn a_job_held_here_or_on_a_machine_is_one_job_and_an_approval_there_carries_the_jobs() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let raw = pool.clone();
        let dir = tempfile::tempdir().unwrap();
        let nudges = "[oversight]\nenabled = true\ngroup = \"acme\"\nlevel = \"act\"\n[oversight.act]\nnudge = true\nanswer_sessions = true\n";
        let (sarah, crew, d, _llm) = overseeing(dir.path(), pool, "ok", nudges, &[]).await;
        d.picture_load().await;
        let local = new_session(&d, dir.path(), "acme-api").await;
        let remote = uuid::Uuid::new_v4();
        d.change_layout(|l| {
            l.create(
                "acme",
                "Acme",
                &[
                    format!("session:devbox/{remote}"),
                    workforce_desk::session_item(local.id),
                ],
                None,
            )
        })
        .await
        .unwrap();
        eventually("Sarah's tools", async || sarah.tools.get("crew.nudge").map(drop)).await;
        let tool = |name: &str| sarah.tools.get(name).unwrap();
        let seen: Arc<parking_lot::Mutex<Vec<RelayCall>>> = Arc::default();
        let poller = {
            let (crew, seen) = (Arc::clone(&crew), Arc::clone(&seen));
            tokio::spawn(async move {
                loop {
                    for c in crew.relay_take(Duration::from_millis(200)).await {
                        seen.lock().push(c.clone());
                        crew.relay_answer(
                            c.id,
                            RelayAnswer {
                                status: ServedStatus::Ok,
                                body: json!({"done": "told it"}),
                            },
                        );
                    }
                }
            })
        };
        eventually("Desk polling", async || crew.relay_polled.lock().is_some().then_some(()))
            .await;
        let release = |key: &str| json!({ "kind": "release", "target": "acme/api", "key": key });
        let nudge = |to: String, message: &str, key: &str| {
            json!({"session_id": to, "message": message, "reason": "Sam's order.",
                   "job": release(key), "irreversible": true})
        };

        // Held here: the machine's session isn't given it, and nothing goes.
        let r = tool("crew.nudge")
            .execute(nudge(local.id.to_string(), "Dispatch release 4 now.", "release-4"))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        let r = tool("crew.nudge")
            .execute(nudge(format!("devbox/{remote}"), "Start release 4 now.", "4"))
            .await
            .unwrap();
        let why = r.error.unwrap_or_default();
        assert!(why.starts_with("Not sent: acme-api holds this job"), "{why}");
        assert!(seen.lock().iter().all(|c| c.tool != "crew.nudge"));

        // Held on the machine: not given here.
        let r = tool("crew.nudge")
            .execute(nudge(format!("devbox/{remote}"), "Dispatch release 5 now.", "5"))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        let sent = seen.lock().iter().rfind(|c| c.tool == "crew.nudge").cloned().unwrap();
        assert!(sent.input.get("job").is_none(), "its order stays at home: {}", sent.input);
        let r = tool("crew.nudge")
            .execute(nudge(local.id.to_string(), "Start release 5 now.", "r5"))
            .await
            .unwrap();
        let why = r.error.unwrap_or_default();
        assert!(why.starts_with("Not sent: devbox's session"), "{why}");

        // An approval there carries the jobs held here and there.
        let r = tool("crew.approve")
            .execute(json!({"session_id": format!("devbox/{remote}"),
                            "approval_id": uuid::Uuid::new_v4().to_string(), "reason": "Ok."}))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        let sent = seen.lock().iter().rfind(|c| c.tool == "crew.approve").cloned().unwrap();
        let held: Vec<String> = sent.input["held_jobs"]
            .as_array()
            .expect("the jobs held here")
            .iter()
            .filter_map(|a| a["jobKey"].as_str().map(str::to_string))
            .collect();
        assert!(held.contains(&"acme/api/release/4".to_string()), "{held:?}");
        assert!(held.contains(&"acme/api/release/5".to_string()), "{held:?}");
        poller.abort();

        // Left being sent: kept, as it carries no line message here.
        let machine = d
            .store()
            .assignments_of("desk", true)
            .await
            .unwrap()
            .into_iter()
            .find(|a| a.machine.is_some())
            .unwrap();
        raw.get()
            .await
            .unwrap()
            .execute(
                "UPDATE boss_assignments SET state = 'sending', sent_at = now() - interval '10 minutes' WHERE id = $1",
                &[&machine.id],
            )
            .await
            .unwrap();
        eventually("the relayed order to be settled", async || {
            let a = d.store().assignment(machine.id).await.unwrap().unwrap();
            (a.state != workforce_desk::assignment::AssignmentState::Sending).then_some(a.state)
        })
        .await;
        assert_eq!(
            d.store().assignment(machine.id).await.unwrap().unwrap().state,
            workforce_desk::assignment::AssignmentState::Open
        );
    })
    .await;
}

/// The main overseer: it runs no group, and messages its sessions.
const MAIN: &str = "[oversight]\nenabled = true\nlevel = \"act\"\n[oversight.act]\nnudge = true\nhand_out_work = false\n";

/// Proposes a plan of one ticket on devbox, in `folder` there, as `boss`
/// does, and approves it: its ticket's session there, once started.
async fn planned_on_devbox(
    boss: &AgentService,
    d: &Desk,
    goal: &str,
    folder: &str,
) -> (uuid::Uuid, String) {
    let r = boss
        .tools
        .get("crew.plan")
        .unwrap()
        .execute(json!({"goal": goal, "machine": "devbox",
                        "tickets": [{"title": goal, "folder": folder}]}))
        .await
        .unwrap();
    assert!(r.success, "{r:?}");
    let id: uuid::Uuid = r.output["proposed"]["plan_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    d.approve_plan(id, 1, &By::you()).await.unwrap();
    let session = eventually("its session on devbox", async || {
        let t = d.plan(id).await.ok()?.tickets[0].ticket.ticket.clone();
        t.assignee
            .filter(|_| t.state == workforce_desk::board::TicketState::Doing)
            .map(|a| a.id)
    })
    .await;
    (id, session)
}

/// The grants the Mac says devbox should hold, once it holds `group`'s.
async fn wanted_with(crew: &CrewHub, group: &str) -> Vec<super::super::machine_grants::Wanted> {
    eventually("devbox's grants", async || {
        let w = crew.machine_grants()?;
        let g = w.machines.get("devbox")?.grants.clone();
        g.iter().any(|g| g.group == group).then_some(g)
    })
    .await
}

/// The main overseer reaches its own plan's ticket session on devbox end
/// to end: Desk carries the plan's grant there, and each call through the
/// relay to devbox's served route, where devbox decides it under that
/// grant. It reads it at once, messages it once the person hands it over
/// there, and reaches nothing else; at Watch it only reads; the grant
/// goes with the plan.
#[tokio::test(flavor = "multi_thread")]
async fn the_main_overseer_reads_and_messages_its_plans_session_on_a_machine_and_nothing_else() {
    with_test_pool(workforce_desk::store::build_pool, |mac_pool| async move {
        with_test_pool(workforce_desk::store::build_pool, |devbox_pool| async move {
            let (mac_dir, devbox_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let (main, state, d, _llm) =
                super::plans::hub_with(mac_dir.path(), mac_pool, MAIN).await;
            let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
            // devbox, a machine whose groups are its grants, with a main
            // overseer of its own.
            let (theirs, _llm) = agent(devbox_dir.path(), "desk", "ok").await;
            write_config(&theirs, MAIN);
            let there_state = hub(std::slice::from_ref(&theirs));
            let there = machine_desk(devbox_pool, devbox_dir.path()).await;
            let there_crew =
                CrewHub::start(Arc::clone(&there_state), there.clone(), FAST, CancellationToken::new());
            let served = super::super::relay::routes(Arc::clone(&there_state), Arc::clone(&there_crew));
            let there_token = there_state.auth_token.clone();
            eventually("the plan tools", async || main.tools.has("crew.plan").then_some(())).await;

            // Desk's native side: devbox's sessions and starts, and each
            // crew call posted to devbox as it carries one.
            let carried: Arc<parking_lot::Mutex<Vec<RelayCall>>> = Arc::default();
            let poller = {
                let (crew, there, carried, served, token, dir) = (
                    Arc::clone(&crew),
                    there.clone(),
                    Arc::clone(&carried),
                    served.clone(),
                    there_token.clone(),
                    devbox_dir.path().to_path_buf(),
                );
                tokio::spawn(async move {
                    loop {
                        for c in crew.relay_take(Duration::from_millis(200)).await {
                            let answer = match c.tool.as_str() {
                                "desk.sessions" => RelayAnswer {
                                    status: ServedStatus::Ok,
                                    body: serde_json::to_value(there.list()).unwrap(),
                                },
                                "desk.session_start" => {
                                    let name = c.input["name"].as_str().unwrap();
                                    RelayAnswer {
                                        status: ServedStatus::Ok,
                                        body: serde_json::to_value(new_session(&there, &dir, name).await)
                                            .unwrap(),
                                    }
                                }
                                _ => {
                                    carried.lock().push(c.clone());
                                    let w = serde_json::to_value(&c).unwrap();
                                    let body = json!({"group": w["group"], "tool": w["tool"],
                                        "input": w["input"], "boss": w["boss"], "linkId": w["linkId"],
                                        "id": w["id"], "level": w["level"], "act": w["act"]});
                                    let (code, out) = post(&served, "/api/desk/crew/served", &token, body).await;
                                    assert_eq!(code, StatusCode::OK, "{out}");
                                    serde_json::from_value(out).unwrap()
                                }
                            };
                            crew.relay_answer(c.id, answer);
                        }
                    }
                })
            };

            let folder = devbox_dir.path().to_string_lossy().into_owned();
            let (plan, session) = planned_on_devbox(&main, &d, "Speed up recall", &folder).await;
            let (_, id) = super::super::relay::machine_session(&session).unwrap();
            let key = super::super::machine_grants::plan_key(plan);

            // Desk carries the plan's grant to devbox as the Mac says it.
            let grants = wanted_with(&crew, &key).await;
            let g = grants.iter().find(|g| g.group == key).unwrap();
            assert_eq!(g.plan, Some(plan));
            assert_eq!(g.grant["boss"], "local:desk");
            assert_eq!(g.grant["level"], "act");
            assert_eq!(g.grant["name"], "Speed up recall");
            assert_eq!(g.grant["members"], json!([id]));
            let mut put = g.grant.clone();
            put["revision"] = json!(1);
            let (code, out) =
                super::served::native(&there, "PUT", &format!("/api/desk/grants/{key}"), put).await;
            assert_eq!(code, StatusCode::OK, "{out}");
            assert_eq!(
                there.view(id).unwrap().group.map(|g| g.id),
                Some(key.clone()),
                "a group of its own there"
            );

            let call = |tool: &'static str, input: Value| {
                let main = Arc::clone(&main);
                async move { main.tools.get(tool).unwrap().execute(input).await.unwrap() }
            };
            let nudge = |to: &str| json!({"session_id": to, "message": "Profile recall first.", "reason": "The plan says so."});

            // It reads its plan's session there.
            let r = call("crew.history", json!({"session_id": session})).await;
            assert!(r.success, "{r:?}");
            let name = there.view(id).unwrap().name;
            assert!(r.output["done"].as_str().unwrap().contains(&name), "{r:?}");
            let sent = carried.lock().last().cloned().unwrap();
            assert_eq!((sent.group.as_str(), sent.boss.as_str()), (key.as_str(), "local:desk"));
            // It messages it: the person's to do until they hand it over
            // there, as for any boss of a machine's sessions; then done.
            let r = call("crew.nudge", nudge(&session)).await;
            assert!(r.success && r.output.get("suggested").is_some(), "{r:?}");
            there.stop(id, None).unwrap();
            eventually("it to stop", async || (!there.view(id).unwrap().live).then_some(())).await;
            let (code, out) =
                super::served::native(&there, "POST", &format!("/api/desk/sessions/{id}/charge"), json!({})).await;
            assert_eq!(code, StatusCode::OK, "{out}");
            assert_eq!(there.charge_of(id).unwrap().boss, "home/desk");
            let r = call("crew.nudge", nudge(&session)).await;
            assert!(r.success && r.output.get("done").is_some(), "{r:?}");

            // A session there of no plan is never sent.
            let loose = new_session(&there, devbox_dir.path(), "loose").await;
            let before = carried.lock().len();
            let r = call("crew.history", json!({"session_id": format!("devbox/{}", loose.id)})).await;
            assert!(!r.success);
            assert!(r.error.unwrap().contains("only your own plans' ticket sessions"));
            assert_eq!(carried.lock().len(), before);
            // devbox serves the plan's grant to its boss, for its members
            // alone: never another boss, nor another session.
            let direct = |boss: &str, s: uuid::Uuid, n: &str| {
                json!({"group": key, "tool": "crew.history", "input": {"session_id": s},
                       "boss": boss, "linkId": "home-relay", "id": n, "level": "act", "act": {}})
            };
            let (_, out) = post(&served, "/api/desk/crew/served", &there_token, direct("local:sarah", id, "d-1")).await;
            assert_eq!(out["status"], "refused", "{out}");
            let (_, out) =
                post(&served, "/api/desk/crew/served", &there_token, direct("local:desk", loose.id, "d-2")).await;
            assert_eq!(out["status"], "refused", "{out}");

            // At Watch it only reads.
            write_config(&main, "[oversight]\nenabled = true\nlevel = \"watch\"\n");
            eventually("watching", async || {
                (crew.overseer_named("desk")?.1.level == workforce_common::oversight::Level::Watch).then_some(())
            })
            .await;
            let r = call("crew.history", json!({"session_id": session})).await;
            assert!(r.success, "{r:?}");
            let before = carried.lock().len();
            let r = call("crew.nudge", nudge(&session)).await;
            assert!(!r.success);
            assert!(r.error.unwrap().contains("only watch"));
            assert_eq!(carried.lock().len(), before);
            assert_eq!(wanted_with(&crew, &key).await.iter().find(|g| g.group == key).unwrap().grant["level"], "watch");

            // The plan ends: its grant is no longer wanted, and once Desk
            // takes it off devbox, the session is let go of there.
            d.drop_plan(plan, workforce_desk::OnDrop::Leave, &By::you()).await.unwrap();
            eventually("the plan's grant gone", async || {
                crew.machine_grants()?
                    .machines
                    .get("devbox")
                    .is_none_or(|m| m.grants.iter().all(|g| g.group != key))
                    .then_some(())
            })
            .await;
            let r = call("crew.history", json!({"session_id": session})).await;
            assert!(!r.success);
            let (code, out) =
                super::served::native(&there, "DELETE", &format!("/api/desk/grants/{key}"), json!({})).await;
            assert!(code.is_success(), "{out}");
            assert!(there.charge_of(id).is_none_or(|c| !c.live()));
            poller.abort();
        })
        .await;
    })
    .await;
}

/// On the Mac: the main overseer's plan on devbox has a grant of its own
/// there, holding its ticket's session alone; a group boss's plan session
/// there is its group's, as before. Each reaches only its own: the main
/// overseer never another boss's plan's session, nor one of no plan.
#[tokio::test(flavor = "multi_thread")]
async fn a_plans_grant_holds_its_bosss_own_ticket_sessions_and_a_group_bosss_are_its_groups() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (main, _llm) = agent(dir.path(), "desk", "ok").await;
        write_config(&main, MAIN);
        let (sarah, _sarah_llm) = agent(dir.path(), "sarah", "ok").await;
        write_config(
            &sarah,
            "[oversight]\nenabled = true\ngroup = \"acme\"\nlevel = \"act\"\n[oversight.act]\nnudge = true\nhand_out_work = false\n",
        );
        let state = hub(&[Arc::clone(&main), Arc::clone(&sarah)]);
        let d = desk_running(
            pool,
            dir.path(),
            Default::default(),
            installed_cli(dir.path()),
            not_installed(dir.path()),
        )
        .await;
        d.change_layout(|l| l.create("acme", "Acme", &[], None))
            .await
            .unwrap();
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        for boss in [&main, &sarah] {
            eventually("the plan tools", async || boss.tools.has("crew.plan").then_some(())).await;
        }
        // devbox: its sessions, each start made, each crew call done.
        let carried: Arc<parking_lot::Mutex<Vec<RelayCall>>> = Arc::default();
        let listed: Arc<parking_lot::Mutex<Vec<Value>>> = Arc::default();
        let poller = {
            let (crew, carried, listed) = (Arc::clone(&crew), Arc::clone(&carried), Arc::clone(&listed));
            tokio::spawn(async move {
                loop {
                    for c in crew.relay_take(Duration::from_millis(200)).await {
                        let body = match c.tool.as_str() {
                            "desk.sessions" => Value::Array(listed.lock().clone()),
                            "desk.session_start" => {
                                let s = json!({"id": uuid::Uuid::new_v4(), "name": c.input["name"],
                                               "cwd": c.input["cwd"], "state": "working"});
                                listed.lock().push(s.clone());
                                s
                            }
                            _ => {
                                carried.lock().push(c.clone());
                                json!({"done": "read"})
                            }
                        };
                        crew.relay_answer(c.id, RelayAnswer { status: ServedStatus::Ok, body });
                    }
                }
            })
        };
        let (mine_plan, mine) = planned_on_devbox(&main, &d, "Speed up recall", "/home/dev/vellev").await;
        let (_, hers) = planned_on_devbox(&sarah, &d, "Ship Acme's API", "/home/dev/acme").await;
        let (_, mine_id) = super::super::relay::machine_session(&mine).unwrap();
        let (_, hers_id) = super::super::relay::machine_session(&hers).unwrap();
        let key = super::super::machine_grants::plan_key(mine_plan);
        eventually("Sarah's session in Acme", async || {
            (d.group_of_member(&format!("session:{hers}")).as_deref() == Some("acme")).then_some(())
        })
        .await;

        let grants = eventually("both grants", async || {
            let g = wanted_with(&crew, &key).await;
            g.iter().any(|g| g.group == "acme").then_some(g)
        })
        .await;
        assert_eq!(grants.len(), 2, "{grants:?}");
        let acme = grants.iter().find(|g| g.group == "acme").unwrap();
        assert_eq!((acme.agent.as_str(), acme.plan), ("sarah", None));
        assert_eq!(acme.grant["members"], json!([hers_id]), "the group's, as before");
        assert!(serde_json::to_value(acme).unwrap().get("plan").is_none());
        let ours = grants.iter().find(|g| g.group == key).unwrap();
        assert_eq!((ours.agent.as_str(), ours.plan), ("desk", Some(mine_plan)));
        assert_eq!(ours.grant["members"], json!([mine_id]), "its own plan's alone");
        assert!(workforce_desk::grant::group_id_ok(&key));
        assert_eq!(super::super::machine_grants::plan_of(&key), Some(mine_plan));
        assert_eq!(super::super::machine_grants::plan_of(&key.to_uppercase()), None);

        eventually("Desk polling", async || crew.relay_polled.lock().is_some().then_some(())).await;
        let history = |boss: &Arc<AgentService>, s: &str| {
            let (boss, s) = (Arc::clone(boss), s.to_string());
            async move {
                boss.tools
                    .get("crew.history")
                    .unwrap()
                    .execute(json!({"session_id": s}))
                    .await
                    .unwrap()
            }
        };
        // Each reaches its own: the main overseer under its plan's grant,
        // Sarah under Acme's.
        let r = history(&main, &mine).await;
        assert!(r.success, "{r:?}");
        assert_eq!(carried.lock().last().unwrap().group, key);
        let r = history(&sarah, &hers).await;
        assert!(r.success, "{r:?}");
        assert_eq!(carried.lock().last().unwrap().group, "acme");
        // Nothing else is sent: another boss's plan's session, one of no
        // plan, nor the main overseer's to Sarah.
        let before = carried.lock().len();
        for (boss, s) in [
            (&main, hers.clone()),
            (&main, format!("devbox/{}", uuid::Uuid::new_v4())),
            (&sarah, mine.clone()),
        ] {
            let r = history(boss, &s).await;
            assert!(!r.success, "{s}: {r:?}");
        }
        assert_eq!(carried.lock().len(), before);
        poller.abort();
    })
    .await;
}

/// A boss's check on devbox, carried as Desk carries it, runs at devbox's
/// own run route; carried to its served route instead, as an older Desk
/// did, it is refused there for what it is, never as a group's call.
#[tokio::test(flavor = "multi_thread")]
async fn a_boss_check_runs_at_the_machines_own_route_and_is_never_a_groups_call() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (boss, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"act\"\n",
            &[],
        )
        .await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: d.home_folder().unwrap(),
            ..Default::default()
        });
        // devbox: its own routes, its run route and its served route both.
        let devbox = workforce_desk::sessions_router(d.clone(), Arc::new(HubToken)).merge(
            super::super::relay::routes(Arc::clone(&crew.state), Arc::clone(&crew)),
        );
        let token = crew.state.auth_token.clone();
        use std::sync::atomic::{AtomicBool, Ordering};
        let older = Arc::new(AtomicBool::new(false));
        let poller = {
            let (crew, older) = (Arc::clone(&crew), Arc::clone(&older));
            tokio::spawn(async move {
                loop {
                    for c in crew.relay_take(Duration::from_millis(100)).await {
                        let (uri, body) = match older.load(Ordering::SeqCst) {
                            false => ("/api/desk/run", c.input.clone()),
                            true => (
                                "/api/desk/crew/served",
                                json!({"group": c.group, "tool": c.tool, "input": c.input,
                                       "boss": c.boss, "linkId": c.link_id, "id": c.id.to_string(),
                                       "level": "act", "act": {}}),
                            ),
                        };
                        let (code, out) = post(&devbox, uri, &token, body).await;
                        let answer = match code {
                            StatusCode::OK => RelayAnswer {
                                status: ServedStatus::Ok,
                                body: out,
                            },
                            _ => RelayAnswer {
                                status: ServedStatus::Refused,
                                body: json!({"error": out["error"]}),
                            },
                        };
                        crew.relay_answer(c.id, answer);
                    }
                }
            })
        };
        eventually("Desk polling", async || {
            crew.relay_polled.lock().is_some().then_some(())
        })
        .await;
        eventually("the run tool", async || boss.tools.get("crew.machine_run")).await;
        let run = boss.tools.get("crew.machine_run").unwrap();
        let check =
            json!({"machine": "devbox", "command": "echo devbox-ok", "reason": "Is it there?"});

        let r = run.execute(check.clone()).await.unwrap();
        assert!(r.success, "{r:?}");
        assert!(
            r.output["ran"].as_str().unwrap().contains("devbox-ok"),
            "{r:?}"
        );

        older.store(true, Ordering::SeqCst);
        let r = run.execute(check).await.unwrap();
        assert!(!r.success, "{r:?}");
        let said = r.error.unwrap_or_default();
        assert!(said.contains("goes to its own route"), "{said}");
        assert!(!said.contains("can't be a group"), "{said}");
        poller.abort();
    })
    .await;
}

/// On devbox: every machine call, carried to its served route, is refused
/// there by name, whatever group it says.
#[tokio::test(flavor = "multi_thread")]
async fn every_machine_call_is_refused_at_the_served_route() {
    use super::super::machines::MACHINE_CALLS;
    use super::super::relay::DESK_CALLS;
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_any, crew, _d, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = false\n",
            &[],
        )
        .await;
        let app = super::super::relay::routes(Arc::clone(&crew.state), Arc::clone(&crew));
        let token = crew.state.auth_token.clone();
        for (i, (tool, group)) in DESK_CALLS
            .iter()
            .chain(MACHINE_CALLS.iter())
            .flat_map(|t| [(*t, ""), (*t, "acme")])
            .enumerate()
        {
            let (code, out) = post(
                &app,
                "/api/desk/crew/served",
                &token,
                json!({"group": group, "tool": tool, "input": {}, "boss": "local:sarah",
                       "linkId": "home-relay", "id": format!("m-{i}"), "level": "act", "act": {}}),
            )
            .await;
            assert_eq!(code, StatusCode::BAD_REQUEST, "{tool}: {out}");
            let said = out["error"].as_str().unwrap_or_default();
            assert!(
                said.starts_with(&format!("{tool} goes to its own route")),
                "{said}"
            );
        }
    })
    .await;
}
