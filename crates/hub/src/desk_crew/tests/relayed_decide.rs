//! A boss relayed from the home at Act fully (designs/relayed-boss-act-
//! fully-design.md): Sarah, Acme's boss on the Mac, decides each request of
//! her devbox sessions with her own model at home, the Mac taking them by
//! the relay. Both sides must hold: devbox's grant at Act fully, and the
//! home saying lately that it is in force there. Stop, either side, holds
//! everything; what is decided is judged by devbox as it is done.

use super::served::native;
use super::*;
use workforce_desk::decision::Need;
use workforce_desk::grant::BossRef;

use super::super::relay::{RelayAnswer, RelayCall};
use super::super::relayed_decide::{DECIDED, FRESH, TAKE};
use super::super::served::{ServedCall, ServedStatus};

const SARAH: &str = "home/sarah";

fn grant(revision: i64, level: &str, folder: &Path) -> Value {
    json!({
        "grantVersion": 1, "revision": revision, "name": "Acme",
        "boss": "local:sarah", "level": level, "sees": "everything",
        "members": [], "folders": [folder.to_string_lossy()],
        "act": {"nudge": true, "interrupt": true, "answer_sessions": true,
                "hand_out_work": true, "resume_restart": true, "parallel": 2,
                "budget_usd_per_day": 50},
    })
}

/// The home's relayed call to devbox, as Desk's native side posts it.
fn relayed(tool: &str, input: Value, boss: &str, level: &str) -> Value {
    json!({"group": "acme", "tool": tool, "input": input, "boss": boss,
           "linkId": "home-relay", "id": uuid::Uuid::new_v4().to_string(),
           "level": level, "act": {}})
}

async fn post(app: &axum::Router, token: &str, body: Value) -> Value {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/desk/crew/served")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn ask(url: &str, command: &str) -> tokio::task::JoinHandle<Value> {
    let url = url.to_string();
    let body = json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash",
        "tool_input": {"command": command}});
    tokio::spawn(async move { hook(&url, body).await })
}

fn pre(command: &str) -> Value {
    json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "permission_mode": "default",
        "tool_use_id": uuid::Uuid::new_v4().to_string(),
        "tool_input": {"command": command}})
}

async fn pending(d: &Desk, s: uuid::Uuid, command: &str) -> uuid::Uuid {
    eventually(command, async || {
        d.view(s).unwrap().pending.filter(|p| p.detail == command)
    })
    .await
    .id
}

async fn waits_on_person(d: &Desk, s: uuid::Uuid) {
    eventually("left to the person", async || {
        d.view(s).unwrap().waits_on_you.is_some().then_some(())
    })
    .await;
}

/// On devbox: Sarah's requests at Act fully wait for her home's take, and
/// only her home's answer, once, decides each; nothing while her home
/// doesn't hold it, or after Stop.
#[tokio::test(flavor = "multi_thread")]
async fn a_machine_unbinds_the_homes_boss_only_while_its_home_holds_it_and_takes_its_answers_once()
{
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        // devbox runs no agents of its own that oversee.
        let (_any, crew, d, llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = false\n",
            &[],
        )
        .await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: dir.path().to_path_buf(),
            ..Default::default()
        });
        let acme_api = dir.path().join("acme-api");
        std::fs::create_dir_all(&acme_api).unwrap();
        let anchor = new_session(&d, dir.path(), "acme-web").await;
        let item = workforce_desk::session_item(anchor.id);
        d.change_layout(|l| l.create("acme", "Acme", std::slice::from_ref(&item), None))
            .await
            .unwrap();
        let put = |revision, level: &'static str| {
            let d = d.clone();
            let body = grant(revision, level, &acme_api);
            async move {
                let (code, out) = native(&d, "PUT", "/api/desk/grants/acme", body).await;
                assert_eq!(code, StatusCode::OK, "{out}");
            }
        };
        put(1, "act").await;
        let sarah = BossRef::Local {
            agent: "sarah".into(),
        };
        // Sarah starts a session at Act: she runs it.
        let r = crew
            .serve_crew(
                ServedCall {
                    tool: "crew.start_session".into(),
                    input: json!({"folder": acme_api.to_string_lossy(), "goal": "Ship the fix",
                                  "engine": "claude", "reason": "Next up."}),
                    boss: sarah.clone(),
                    link_id: Some("home-relay".into()),
                    call_id: Some(uuid::Uuid::new_v4().to_string()),
                    ceiling: None,
                },
                "acme",
            )
            .await;
        assert_eq!(r.status, ServedStatus::Ok, "{:?}", r.body);
        let started = eventually("Sarah's session", async || {
            d.charges()
                .into_iter()
                .find(|c| c.boss == SARAH)
                .map(|c| c.session)
        })
        .await;
        let url = hook_url(dir.path(), started).await;
        let decision = |v: &Value| v["hookSpecificOutput"]["permissionDecision"].clone();
        let app = super::super::relay::routes(Arc::clone(&crew.state), Arc::clone(&crew));
        let token = crew.state.auth_token.clone();
        let take = |in_force: bool, level: &'static str| {
            let (app, token) = (app.clone(), token.clone());
            async move {
                post(
                    &app,
                    &token,
                    relayed(TAKE, json!({"inForce": in_force, "sentAt": chrono::Utc::now().timestamp_millis()}), "local:sarah", level),
                )
                .await
            }
        };
        let answer = |id: Value, input: Value| {
            let (app, token) = (app.clone(), token.clone());
            let mut input = input;
            input["decideId"] = id;
            async move { post(&app, &token, relayed(DECIDED, input, "local:sarah", "full")).await }
        };
        /// Takes until what waits is handed back.
        async fn taken(take: impl AsyncFn() -> Value) -> Value {
            for _ in 0..100 {
                let out = take().await;
                if let Some(d) = out["body"]["decisions"].as_array().and_then(|d| d.first()) {
                    return d.clone();
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("nothing was handed back");
        }

        // At Act fully on devbox, but its home hasn't said so: bound.
        put(2, "full").await;
        crew.read_spent_today(&crew.overseers()).await;
        assert!(crew.served_full(SARAH).is_none());
        let said = hook(&url, pre("git push --force origin main")).await;
        assert_eq!(decision(&said), "deny", "{said}");
        // Its home says so at Act only, or not in force: still bound.
        let out = take(true, "act").await;
        assert_eq!(out["body"]["full"], false, "{out}");
        assert!(crew.served_full(SARAH).is_none());
        take(false, "full").await;
        assert!(crew.served_full(SARAH).is_none());
        // Another boss of the home is no one's here.
        let out = post(
            &app,
            &token,
            relayed(TAKE, json!({"inForce": true, "sentAt": chrono::Utc::now().timestamp_millis()}), "local:bob", "full"),
        )
        .await;
        assert_eq!(out["status"], "refused", "{out}");
        assert!(!crew.home_holds_full("home/bob", "acme"));
        // A take held up on its way, or that doesn't say when it was sent,
        // says nothing of now: still bound.
        for sent in [
            json!(chrono::Utc::now().timestamp_millis() - 10_000),
            json!(chrono::Utc::now().timestamp_millis() + 60_000),
            Value::Null,
        ] {
            let out = post(
                &app,
                &token,
                relayed(TAKE, json!({"inForce": true, "sentAt": sent}), "local:sarah", "full"),
            )
            .await;
            assert_eq!(out["body"]["full"], false, "{out}");
            assert!(crew.served_full(SARAH).is_none());
        }

        // Both hold: unbound, and nothing allowed for her by default.
        let out = take(true, "full").await;
        assert_eq!(out["body"]["full"], true, "{out}");
        let full = crew.served_full(SARAH).expect("unbound");
        assert!(full.unbound && !full.anywhere && full.repos.is_empty());
        let said = hook(&url, pre("git push --force origin main")).await;
        assert_eq!(decision(&said), "ask", "{said}");

        // A request waits for its home's take, whole, its secrets hidden,
        // and her home's ALLOW is done as Sarah, said to be at home.
        let secret = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let command = format!("git push --force origin main # {secret}");
        let asked = ask(&url, &command);
        let a1 = pending(&d, started, &command).await;
        let given = taken(async || take(true, "full").await).await;
        let prompt = given["prompt"].as_str().unwrap();
        assert!(prompt.starts_with(super::super::decide::MARK), "{prompt}");
        assert!(prompt.contains("git push --force origin main"), "{prompt}");
        assert!(!prompt.contains(secret), "{prompt}");
        // Handed out once.
        let again = take(true, "full").await;
        assert_eq!(again["body"]["decisions"], json!([]), "{again}");
        // Another boss's answer, or one for no decision, decides nothing.
        let out = post(
            &app,
            &token,
            relayed(
                DECIDED,
                json!({"decideId": given["decideId"], "text": "ALLOW: x"}),
                "local:bob",
                "full",
            ),
        )
        .await;
        assert_eq!(out["status"], "refused", "{out}");
        let out = answer(json!(uuid::Uuid::new_v4()), json!({"text": "ALLOW: x"})).await;
        assert_eq!(out["status"], "refused", "{out}");
        assert!(!asked.is_finished());
        let out = answer(
            given["decideId"].clone(),
            json!({"text": "ALLOW: the fix is ready to ship"}),
        )
        .await;
        assert_eq!(out["status"], "ok", "{out}");
        let answered = asked.await.unwrap();
        assert_eq!(
            answered["hookSpecificOutput"]["decision"]["behavior"], "allow",
            "{answered}"
        );
        // Sent again: taken once.
        let out = answer(given["decideId"].clone(), json!({"text": "ALLOW: again"})).await;
        assert_eq!(out["status"], "refused", "{out}");
        let rows = d
            .store()
            .decisions_since(SARAH, chrono::DateTime::UNIX_EPOCH, 50)
            .await
            .unwrap();
        let row = rows
            .iter()
            .find(|r| r.need == Need::Approval && r.need_ref == Some(a1.to_string()))
            .unwrap_or_else(|| panic!("{rows:?}"));
        assert!(row.what.contains("(by sarah at home)"), "{}", row.what);
        assert!(row.why.contains("the fix is ready to ship"), "{}", row.why);

        // Its home holding it for its budget, as its code says: not asked
        // again, and the person's.
        let asked = ask(&url, "make docs");
        let a2 = pending(&d, started, "make docs").await;
        let given = taken(async || take(true, "full").await).await;
        answer(
            given["decideId"].clone(),
            json!({"error": "spent", "code": "budget"}),
        )
        .await;
        waits_on_person(&d, started).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(take(true, "full").await["body"]["decisions"], json!([]));
        d.decide(started, a2, workforce_desk::model::Decision::Deny, None)
            .unwrap();
        let _ = asked.await;

        // Its home says it isn't in force while a decision waits: it ends
        // at once, and the request is the person's.
        let asked = ask(&url, "make release");
        let a3 = pending(&d, started, "make release").await;
        take(false, "full").await;
        assert!(crew.served_full(SARAH).is_none());
        assert!(!d.boss_unbound(SARAH));
        waits_on_person(&d, started).await;
        assert!(d.view(started).unwrap().pending.is_some_and(|p| p.id == a3));
        d.decide(started, a3, workforce_desk::model::Decision::Deny, None)
            .unwrap();
        let _ = asked.await;

        // Its home goes quiet (the Mac asleep, Desk quit): bound again
        // within FRESH.
        take(true, "full").await;
        assert!(crew.served_full(SARAH).is_some());
        tokio::time::sleep(FRESH + Duration::from_millis(200)).await;
        assert!(crew.served_full(SARAH).is_none());

        // Stop on devbox while her decision is out: her ALLOW, come after,
        // isn't done, and the request stays the person's.
        take(true, "full").await;
        let asked = ask(&url, "git tag v1");
        let a4 = pending(&d, started, "git tag v1").await;
        let given = taken(async || take(true, "full").await).await;
        let (code, _) = native(
            &d,
            "POST",
            "/api/desk/bosses/stop",
            json!({"group": "acme", "from": "desk"}),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert!(crew.served_full(SARAH).is_none());
        answer(given["decideId"].clone(), json!({"text": "ALLOW: tag it"})).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(d.view(started).unwrap().pending.is_some_and(|p| p.id == a4));
        assert!(!asked.is_finished(), "nothing was done for it");
        d.decide(started, a4, workforce_desk::model::Decision::Deny, None)
            .unwrap();
        let _ = asked.await;

        // devbox's own model was never asked to decide for Sarah.
        let mine = llm
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| String::from_utf8_lossy(&r.body).contains(super::super::decide::MARK))
            .count();
        assert_eq!(mine, 0);
    })
    .await;
}

const SARAH_FULL: &str = "[oversight]\nenabled = true\ngroup = \"acme\"\nlevel = \"full\"\n\
    [oversight.full]\nrepos = []\nworkflows = []\nbudget_usd_per_day = 25\n";

/// On the Mac: while Desk relays and Act fully is in force for Sarah here,
/// she takes her decisions from each machine of her group, decides each
/// with her own model, and answers it; a prompt that isn't a decision makes
/// no call; once Act fully isn't in force, her next take says so.
#[tokio::test(flavor = "multi_thread")]
async fn the_home_takes_its_bosss_decisions_from_its_machines_and_answers_each_with_its_own_model()
{
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_sarah, crew, d, llm) = overseeing(dir.path(), pool, "ok", SARAH_FULL, &[]).await;
        super::decides_at_once::decides(&llm, "git tag v9", "ALLOW: it tags the release").await;
        let remote = uuid::Uuid::new_v4();
        d.change_layout(|l| l.create("acme", "Acme", &[format!("session:devbox/{remote}")], None))
            .await
            .unwrap();
        crew.read_spent_today(&crew.overseers()).await;
        let decide_id = uuid::Uuid::new_v4();
        let bad_id = uuid::Uuid::new_v4();
        let seen: Arc<parking_lot::Mutex<Vec<RelayCall>>> = Arc::default();
        let handed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Desk's native side, and devbox, standing in.
        let poller = {
            let (crew, seen, handed) = (Arc::clone(&crew), Arc::clone(&seen), Arc::clone(&handed));
            tokio::spawn(async move {
                loop {
                    for c in crew.relay_take(Duration::from_millis(200)).await {
                        seen.lock().push(c.clone());
                        let body = match c.tool.as_str() {
                            TAKE if !handed.swap(true, std::sync::atomic::Ordering::SeqCst) => {
                                json!({"full": true, "decisions": [
                                    {"decideId": decide_id, "prompt": format!(
                                        "{} decide `git tag v9`", super::super::decide::MARK)},
                                    {"decideId": bad_id, "prompt": "write me a poem"},
                                ]})
                            }
                            TAKE => json!({"full": true, "decisions": []}),
                            _ => json!({"taken": true}),
                        };
                        crew.relay_answer(
                            c.id,
                            RelayAnswer {
                                status: ServedStatus::Ok,
                                body,
                            },
                        );
                    }
                }
            })
        };
        let decided = |id: uuid::Uuid| {
            let seen = Arc::clone(&seen);
            async move {
                eventually("its answer relayed", async || {
                    seen.lock()
                        .iter()
                        .find(|c| c.tool == DECIDED && c.input["decideId"] == json!(id.to_string()))
                        .cloned()
                })
                .await
            }
        };
        let ok = decided(decide_id).await;
        assert_eq!(ok.machine, "devbox");
        assert_eq!(ok.group, "acme");
        let agent = crew
            .overseer_for(&workforce_common::oversight::Scope::Group("acme".into()))
            .expect("Sarah oversees Acme")
            .0
            .agent_id
            .as_str()
            .to_string();
        assert_eq!(ok.boss, format!("local:{agent}"));
        assert_eq!(
            ok.input["text"], "ALLOW: it tags the release",
            "{:?}",
            ok.input
        );
        let bad = decided(bad_id).await;
        assert_eq!(bad.input["code"], "not_a_decision", "{:?}", bad.input);
        assert!(bad.input.get("text").is_none());
        assert_eq!(
            super::decides_at_once::calls_about(&llm, "poem")
                .await
                .len(),
            0,
            "no call for what isn't a decision"
        );
        // Every take said Act fully is in force, at full.
        let takes: Vec<RelayCall> = seen
            .lock()
            .iter()
            .filter(|c| c.tool == TAKE)
            .cloned()
            .collect();
        assert!(!takes.is_empty());
        assert!(takes
            .iter()
            .all(|c| c.input["inForce"] == true && c.machine == "devbox"));

        // Act fully no longer in force here (Desk holds Sarah, over her
        // budget): the next take says so, and no take says in force after it.
        {
            use workforce_core::budget::{OnExceed, Period, Standing, Status, Unit};
            let now = chrono::Utc::now();
            let over = Standing {
                budget: uuid::Uuid::new_v4(),
                period: Period::Day,
                unit: Unit::Usd,
                used: 26.0,
                of: 25.0,
                status: Status::Over,
                on_exceed: OnExceed::Pause,
                from: now - chrono::Duration::hours(1),
                to: now + chrono::Duration::hours(10),
                shared: false,
            };
            d.agent_stand(&agent, "Sarah", vec![over], false);
            assert!(d.agent_held(&agent).is_some());
        }
        let ended = eventually("a take saying it isn't in force", async || {
            seen.lock()
                .iter()
                .find(|c| c.tool == TAKE && c.input["inForce"] == false)
                .cloned()
        })
        .await;
        assert_eq!(ended.machine, "devbox");
        let n = seen.lock().len();
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert!(
            seen.lock()[n..]
                .iter()
                .all(|c| c.tool != TAKE || c.input["inForce"] == false),
            "no take says in force after it ended"
        );
        poller.abort();
    })
    .await;
}
