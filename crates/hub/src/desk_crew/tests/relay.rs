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
                   "linkId": "home-relay", "id": id})
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
        assert_eq!(code, StatusCode::FORBIDDEN);
        let (code, _) = post(
            &app,
            &format!("/api/desk/crew/served?token={token}"),
            &token,
            call("local:sarah", "c-5"),
        )
        .await;
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
        assert_eq!(sent.input["session_id"], json!(remote.to_string()));

        // The overview has devbox's part of Acme, said as devbox said it.
        let r = tool("crew.overview").execute(json!({})).await.unwrap();
        assert!(r.success, "{r:?}");
        assert_eq!(r.output["machines"][0]["machine"], "devbox", "{r:?}");
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
        assert!(r.error.unwrap().contains("can't be reached"));
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
