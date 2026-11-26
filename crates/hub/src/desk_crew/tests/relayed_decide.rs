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
use super::super::relayed_decide::{Call, DECIDED, FRESH, TAKE};
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

/// A take's count of the one budget: from an hour ago, nothing spent
/// elsewhere.
fn counted(mut take: Value) -> Value {
    take["countFrom"] = json!((chrono::Utc::now() - chrono::Duration::hours(1)).timestamp_millis());
    take["spentElsewhereUsd"] = json!(0.0);
    take["takes"] = json!(["question", "plan"]);
    take
}

pub(super) async fn post(app: &axum::Router, token: &str, body: Value) -> Value {
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
        let take_sent = |in_force: bool, level: &'static str, sent: i64| {
            let (app, token) = (app.clone(), token.clone());
            async move {
                post(
                    &app,
                    &token,
                    relayed(
                        TAKE,
                        counted(json!({"inForce": in_force, "sentAt": sent})),
                        "local:sarah",
                        level,
                    ),
                )
                .await
            }
        };
        let take = |in_force: bool, level: &'static str| {
            take_sent(in_force, level, chrono::Utc::now().timestamp_millis())
        };
        let answer = |id: Value, input: Value| {
            let (app, token) = (app.clone(), token.clone());
            let mut input = input;
            input["decideId"] = id;
            // As the home sends it: with the jobs its boss holds there (none).
            if input.get("heldJobs").is_none() {
                input["heldJobs"] = json!([]);
            }
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
            relayed(
                TAKE,
                counted(json!({"inForce": true, "sentAt": chrono::Utc::now().timestamp_millis()})),
                "local:bob",
                "full",
            ),
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
                relayed(
                    TAKE,
                    counted(json!({"inForce": true, "sentAt": sent})),
                    "local:sarah",
                    "full",
                ),
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
        // Kept once it is done: the session has its answer first.
        let row = eventually("its decision kept", async || {
            d.store()
                .decisions_since(SARAH, chrono::DateTime::UNIX_EPOCH, 50)
                .await
                .unwrap()
                .into_iter()
                .find(|r| r.need == Need::Approval && r.need_ref == Some(a1.to_string()))
        })
        .await;
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
        let ended = chrono::Utc::now().timestamp_millis();
        take_sent(false, "full", ended).await;
        assert!(crew.served_full(SARAH).is_none());
        assert!(!d.boss_unbound(SARAH));
        // A take its home sent before that one, come late, says nothing
        // of now: it doesn't bind it again.
        let late = take_sent(true, "full", ended - 1).await;
        assert_eq!(late["body"]["decisions"], json!([]), "{late}");
        assert!(crew.served_full(SARAH).is_none(), "{late}");
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

        // One budget across the Mac and every machine: devbox counts its
        // own spend since where the home counts from, says it, and acts
        // fully only while that and what the home says was spent elsewhere
        // are under the grant's budget ($50).
        let from = chrono::Utc::now() - chrono::Duration::hours(1);
        let take_at = |elsewhere: Value, from: Option<chrono::DateTime<chrono::Utc>>| {
            let (app, token) = (app.clone(), token.clone());
            let mut input = json!({"inForce": true, "sentAt": chrono::Utc::now().timestamp_millis(),
                                   "spentElsewhereUsd": elsewhere});
            if let Some(f) = from {
                input["countFrom"] = json!(f.timestamp_millis());
            }
            async move { post(&app, &token, relayed(TAKE, input, "local:sarah", "full")).await }
        };
        let out = take_at(json!(0.0), Some(from)).await;
        assert_eq!(out["body"]["full"], true, "{out}");
        assert_eq!(out["body"]["spentUsd"], 0.0, "{out}");
        assert_eq!(out["body"]["countFrom"], json!(from.timestamp_millis()));
        super::act_fully_budget::spent(&d, started, workforce_desk::ledger::Billing::Key, 30.0)
            .await;
        // A plan's use is never real money.
        super::act_fully_budget::spent(&d, started, workforce_desk::ledger::Billing::Plan, 500.0)
            .await;
        let out = take_at(json!(15.0), Some(from)).await;
        assert_eq!(out["body"]["full"], true, "{out}");
        assert_eq!(out["body"]["spentUsd"], 30.0, "{out}");
        assert_eq!(crew.relayed_spent(SARAH), Some(45.0));
        assert!(crew.served_full(SARAH).is_some());
        // Spent elsewhere too: $55 of $50, so devbox no longer acts fully.
        let out = take_at(json!(25.0), Some(from)).await;
        assert_eq!(out["body"]["full"], false, "{out}");
        assert_eq!(out["body"]["spentUsd"], 30.0, "it still says what it spent");
        assert!(crew.served_full(SARAH).is_none());
        // Counted from after that spend (the person cleared it): in force.
        let cleared = chrono::Utc::now();
        let out = take_at(json!(0.0), Some(cleared)).await;
        assert_eq!(out["body"]["full"], true, "{out}");
        assert_eq!(out["body"]["spentUsd"], 0.0, "{out}");
        // A home that says nothing of the count (older), or of what was
        // spent elsewhere, or a count from the future, never puts it at Act
        // fully.
        for (elsewhere, from) in [
            (json!(0.0), None),
            (Value::Null, Some(from)),
            (json!(-1.0), Some(from)),
            (
                json!(0.0),
                Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            ),
        ] {
            let out = take_at(elsewhere, from).await;
            assert_eq!(out["body"]["full"], false, "{out}");
            assert!(crew.served_full(SARAH).is_none());
        }
        // The watcher's own read never gives a relayed boss a figure of its
        // own: only a fresh take does.
        crew.read_spent_today(&crew.overseers()).await;
        assert!(crew.served_full(SARAH).is_none());
        assert_eq!(crew.relayed_spent(SARAH), None);

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
        let (sarah, crew, d, llm) = overseeing(dir.path(), pool, "ok", SARAH_FULL, &[]).await;
        super::decides_at_once::decides(&llm, "git tag v9", "ALLOW: it tags the release").await;
        super::decides_at_once::decides(&llm, "Ship it?", "ANSWER: Yes, ship it.\nWHY: it is built")
            .await;
        super::decides_at_once::decides(&llm, "Plan: tag v9", "APPROVE: it tags the release").await;
        let remote = uuid::Uuid::new_v4();
        d.change_layout(|l| l.create("acme", "Acme", &[format!("session:devbox/{remote}")], None))
            .await
            .unwrap();
        crew.read_spent_today(&crew.overseers()).await;
        let decide_id = uuid::Uuid::new_v4();
        let bad_id = uuid::Uuid::new_v4();
        let question_id = uuid::Uuid::new_v4();
        let odd_id = uuid::Uuid::new_v4();
        let plan_id = uuid::Uuid::new_v4();
        let note_id = uuid::Uuid::new_v4();
        let seen: Arc<parking_lot::Mutex<Vec<RelayCall>>> = Arc::default();
        let handed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Desk's native side, and devbox, standing in.
        let poller = {
            let (crew, seen, handed) = (Arc::clone(&crew), Arc::clone(&seen), Arc::clone(&handed));
            tokio::spawn(async move {
                let note = json!({"id": note_id, "kind": "asks", "session": "acme-web",
                                  "ask": "which customer gets the first invite? sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789"});
                loop {
                    for c in crew.relay_take(Duration::from_millis(200)).await {
                        seen.lock().push(c.clone());
                        let counts = json!({"countFrom": c.input["countFrom"], "spentUsd": 0.0});
                        let in_force = c.input["inForce"] == true;
                        let mut body = match c.tool.as_str() {
                            TAKE if in_force
                                && !handed.swap(true, std::sync::atomic::Ordering::SeqCst) =>
                            {
                                json!({"full": true, "decisions": [
                                    {"decideId": decide_id, "prompt": format!(
                                        "{} decide `git tag v9`", super::super::decide::MARK)},
                                    {"decideId": bad_id, "prompt": "write me a poem"},
                                    {"decideId": question_id, "kind": "question", "prompt": format!(
                                        "{} It asks: Ship it?", super::super::decide::MARK)},
                                    {"decideId": odd_id, "kind": "poem", "prompt": format!(
                                        "{} It asks: rhyme?", super::super::decide::MARK)},
                                    {"decideId": plan_id, "kind": "plan", "prompt": format!(
                                        "{} Review it: Plan: tag v9", super::super::decide::MARK)},
                                ], "notes": [note]})
                            }
                            // Handed again on every take until one says it
                            // was taken.
                            TAKE if c.input["notesTaken"]
                                .as_array()
                                .is_some_and(|t| t.contains(&json!(note_id))) =>
                            {
                                json!({"full": in_force, "decisions": []})
                            }
                            TAKE => json!({"full": in_force, "decisions": [], "notes": [note]}),
                            _ => json!({"taken": true}),
                        };
                        if c.tool == TAKE {
                            body["countFrom"] = counts["countFrom"].clone();
                            body["spentUsd"] = counts["spentUsd"].clone();
                        }
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
        // A question is answered with the answerer's own instructions, and
        // what devbox says for the person is said in Sarah's DM.
        let answered = decided(question_id).await;
        assert_eq!(answered.input["text"], "ANSWER: Yes, ship it.\nWHY: it is built");
        let asked = super::decides_at_once::calls_about(&llm, "Ship it?").await;
        assert_eq!(asked.len(), 1);
        assert!(
            asked[0].contains("you answer it yourself, now, as the person would"),
            "{}",
            asked[0]
        );
        let odd = decided(odd_id).await;
        assert_eq!(odd.input["code"], "not_a_decision", "{:?}", odd.input);
        // A plan is reviewed with the plan reviewer's own.
        let reviewed = decided(plan_id).await;
        assert_eq!(reviewed.input["text"], "APPROVE: it tags the release");
        let review = super::decides_at_once::calls_about(&llm, "Plan: tag v9").await;
        assert!(
            review.len() == 1 && review[0].contains("you review it yourself, now, as the person"),
            "{review:?}"
        );
        // Said once in Sarah's DM, in the Mac's words, its secret hidden,
        // and the next take says it was taken.
        eventually("devbox's notice taken", async || {
            seen.lock()
                .iter()
                .any(|c| c.tool == TAKE && c.input["notesTaken"] == json!([note_id]))
                .then_some(())
        })
        .await;
        let notice = ["On devbox: acme-web asks: which customer gets the first invite?"];
        assert_eq!(super::decides_at_once::said(&sarah, &notice), 1);
        assert_eq!(super::decides_at_once::said(&sarah, &["sk-ant-api03"]), 0);
        assert!(seen
            .lock()
            .iter()
            .filter(|c| c.tool == TAKE)
            .all(|c| c.input["takes"] == json!(["question", "plan"])));
        assert_eq!(
            super::decides_at_once::calls_about(&llm, "poem")
                .await
                .len(),
            0,
            "no call for what isn't a decision"
        );
        // The first take asks devbox what it spent and says nothing is in
        // force; once devbox has said, every take says Act fully is in
        // force, at full, with the count.
        let takes: Vec<RelayCall> = seen
            .lock()
            .iter()
            .filter(|c| c.tool == TAKE)
            .cloned()
            .collect();
        assert!(takes.len() >= 2);
        assert_eq!(takes[0].input["inForce"], false, "{:?}", takes[0].input);
        assert!(takes[0].input["countFrom"].is_i64());
        assert!(takes[1..]
            .iter()
            .all(|c| c.input["inForce"] == true && c.machine == "devbox"));
        assert!(takes.iter().all(|c| c.input["spentElsewhereUsd"] == 0.0));
        let seen_before = seen.lock().len();

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
            seen.lock()[seen_before..]
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

/// `agent` holding `n` more live jobs that can't be undone, each with
/// `brief` and `targets` targets, as kept on the Mac.
pub(super) async fn holding(
    d: &Desk,
    agent: &str,
    n: usize,
    brief: &str,
    targets: usize,
) -> Vec<workforce_desk::assignment::Assignment> {
    let mut jobs = Vec::new();
    for _ in 0..n {
        let mut a = workforce_desk::assignment::Assignment::new(agent, uuid::Uuid::new_v4(), brief);
        a.job_key = Some(format!("acme/api/release/{}", a.id.simple()));
        a.targets = (0..targets)
            .map(|t| format!("run:{}{t:04}", a.id.as_u128() % 1_000_000_000))
            .collect();
        a.irreversible = true;
        a.claims = true;
        let kept = d.store().assignment_reserve(&a, None).await.unwrap();
        assert!(
            matches!(
                kept,
                workforce_desk::assignment_store::Reserved::Kept { .. }
            ),
            "{kept:?}"
        );
        jobs.push(a);
    }
    jobs
}

/// Whether `jobs`, as a machine compares a request with them, are more than
/// one call carries.
pub(super) fn too_many(jobs: &[workforce_desk::assignment::Assignment]) -> bool {
    let compared: Vec<_> = jobs
        .iter()
        .cloned()
        .map(workforce_desk::assignment::Assignment::compared)
        .collect();
    json!(compared).to_string().len() > 256 * 1024
}

/// On the Mac: Sarah holding many jobs that can't be undone, each with a
/// long brief, still answers devbox, with each job as devbox compares a
/// request with it; with more than its call carries, without them (devbox
/// then leaves what can't be undone to the person). An answer that may not
/// have reached devbox is said in her DM, once.
#[tokio::test(flavor = "multi_thread")]
async fn the_homes_answer_reaches_its_machine_however_many_long_jobs_its_boss_holds() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (sarah, crew, d, llm) = overseeing(dir.path(), pool, "ok", SARAH_FULL, &[]).await;
        super::decides_at_once::decides(&llm, "git tag v9", "ALLOW: it tags the release").await;
        super::decides_at_once::decides(&llm, "git push --tags", "ALLOW: it pushes the tags").await;
        let remote = uuid::Uuid::new_v4();
        d.change_layout(|l| l.create("acme", "Acme", &[format!("session:devbox/{remote}")], None))
            .await
            .unwrap();
        crew.read_spent_today(&crew.overseers()).await;
        *crew.relay_wait.lock() = Duration::from_millis(400);
        let agent = crew
            .overseer_for(&workforce_common::oversight::Scope::Group("acme".into()))
            .expect("Sarah oversees Acme")
            .0
            .agent_id
            .as_str()
            .to_string();
        let brief = format!(
            "Dispatch release 4 now.\n{}",
            "Watch it through and send Desk each run id. ".repeat(2_500)
        );
        let jobs = holding(&d, &agent, 10, &brief, 0).await;
        assert!(json!(jobs).to_string().len() > 1024 * 1024, "far over a call");
        let (answered_id, lost_id, unheld_id) =
            (uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let decision = |id: uuid::Uuid, request: &str| {
            json!({"decideId": id, "prompt": format!("{} decide `{request}`", super::super::decide::MARK)})
        };
        let seen: Arc<parking_lot::Mutex<Vec<RelayCall>>> = Arc::default();
        let queued: Arc<parking_lot::Mutex<Vec<Value>>> = Arc::new(parking_lot::Mutex::new(vec![
            decision(answered_id, "git tag v9"),
            decision(lost_id, "git push --tags"),
        ]));
        // Desk's native side, and devbox, standing in: devbox hands what is
        // queued, and never answers the lost decision's answer.
        let poller = {
            let (crew, seen, queued) = (Arc::clone(&crew), Arc::clone(&seen), Arc::clone(&queued));
            tokio::spawn(async move {
                loop {
                    for c in crew.relay_take(Duration::from_millis(200)).await {
                        seen.lock().push(c.clone());
                        let in_force = c.input["inForce"] == true;
                        let mut body = match c.tool.as_str() {
                            TAKE if in_force => json!({"full": true,
                                                       "decisions": std::mem::take(&mut *queued.lock())}),
                            TAKE => json!({"full": false, "decisions": []}),
                            _ if c.input["decideId"] == json!(lost_id.to_string()) => continue,
                            _ => json!({"taken": true}),
                        };
                        if c.tool == TAKE {
                            body["countFrom"] = c.input["countFrom"].clone();
                            body["spentUsd"] = json!(0.0);
                        }
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
        let ok = decided(answered_id).await;
        assert_eq!(ok.input["text"], "ALLOW: it tags the release");
        let held: Vec<workforce_desk::assignment::Assignment> =
            serde_json::from_value(ok.input["heldJobs"].clone())
                .expect("the jobs, as devbox reads them");
        assert_eq!(held.len(), jobs.len());
        for (h, a) in held.iter().zip(&jobs) {
            assert_eq!((h.id, h.session, &h.job_key), (a.id, a.session, &a.job_key));
            assert!(h.irreversible && h.state.live());
            assert_eq!(h.about(), a.about());
            assert!(h.text.len() < 100, "{}", h.text);
        }
        // Devbox didn't answer: said in her DM, once.
        decided(lost_id).await;
        let notice = [
            "On devbox: Desk decided a session's request there",
            "may not have reached it",
        ];
        eventually("said in her DM", async || {
            (super::decides_at_once::said(&sarah, &notice) > 0).then_some(())
        })
        .await;

        // More than its call carries: answered, without them.
        let more = holding(&d, &agent, 500, "Dispatch release 5 now.", 12).await;
        assert!(too_many(&more));
        queued.lock().push(decision(unheld_id, "git tag v9"));
        let unheld = decided(unheld_id).await;
        assert_eq!(unheld.input["text"], "ALLOW: it tags the release");
        assert!(unheld.input.get("heldJobs").is_none(), "{}", unheld.input);
        assert_eq!(super::decides_at_once::said(&sarah, &notice), 1, "said once");
        poller.abort();
    })
    .await;
}

/// On devbox: a request that can't be taken back, beside a job Sarah's home
/// says one of her sessions holds, is the person's to allow, by her relayed
/// approve or by her home's decision; and so when her home doesn't say.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_beside_a_job_the_home_holds_is_the_persons_on_the_machine() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_any, crew, d, _llm) = overseeing(
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
        put(1, "full").await;
        let sarah = BossRef::Local {
            agent: "sarah".into(),
        };
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
        let app = super::super::relay::routes(Arc::clone(&crew.state), Arc::clone(&crew));
        let token = crew.state.auth_token.clone();
        // Release 4, held by another of Sarah's sessions at home.
        // As her home sends it: as devbox compares a request with it.
        let mut home = workforce_desk::assignment::Assignment::new(
            "desk",
            uuid::Uuid::new_v4(),
            &format!("Dispatch release 4 now.\n{}", "Watch it through. ".repeat(1_000)),
        );
        home.job_key = Some("acme/api/release/4".into());
        home.job = Some("release 4 of acme/api".into());
        home.irreversible = true;
        home.claims = true;
        home.state = workforce_desk::assignment::AssignmentState::Open;
        let held = json!([home.compared()]);

        // Her relayed approve: with the job, or without the jobs, the
        // person's; with none held, allowed.
        // Read as one that can't be undone, and nothing that ships: R5
        // alone decides it.
        let command = "echo Dispatch release 4 now";
        let asked = ask(&url, command);
        let a1 = pending(&d, started, command).await;
        let approve = |approval: uuid::Uuid, jobs: Option<Value>| {
            let (app, token) = (app.clone(), token.clone());
            let mut input = json!({"session_id": started.to_string(),
                                   "approval_id": approval.to_string(), "reason": "Sam ordered it."});
            if let Some(j) = jobs {
                input["held_jobs"] = j;
            }
            async move { post(&app, &token, relayed("crew.approve", input, "local:sarah", "full")).await }
        };
        for jobs in [Some(held.clone()), None] {
            let out = approve(a1, jobs).await;
            assert_ne!(out["status"], "ok", "{out}");
            assert!(out.to_string().contains("Not allowed"), "{out}");
        }
        assert!(!asked.is_finished(), "nothing allowed it");
        let out = approve(a1, Some(json!([]))).await;
        assert_eq!(out["status"], "ok", "{out}");
        let answered = asked.await.unwrap();
        assert_eq!(
            answered["hookSpecificOutput"]["decision"]["behavior"], "allow",
            "{answered}"
        );

        // Her home's decision at Act fully, with the job: the person's.
        put(2, "full").await;
        crew.read_spent_today(&crew.overseers()).await;
        let take = || {
            let (app, token) = (app.clone(), token.clone());
            async move {
                post(
                    &app,
                    &token,
                    relayed(TAKE, counted(json!({"inForce": true, "sentAt": chrono::Utc::now().timestamp_millis()})), "local:sarah", "full"),
                )
                .await
            }
        };
        take().await;
        // With the job, or without the jobs (too many to send with it).
        for (revision, release, jobs) in [(3, 5, Some(held.clone())), (5, 6, None)] {
            let command = format!("printf 'Dispatch release {release} now' > dispatched.txt");
            let asked = ask(&url, &command);
            let a = pending(&d, started, &command).await;
            let given = eventually("the request handed to her home", async || {
                take().await["body"]["decisions"]
                    .as_array()
                    .and_then(|d| d.first().cloned())
            })
            .await;
            let mut input = json!({"decideId": given["decideId"], "text": "ALLOW: Sam ordered it"});
            if let Some(j) = jobs {
                input["heldJobs"] = j;
            }
            let out = post(&app, &token, relayed(DECIDED, input, "local:sarah", "full")).await;
            assert_eq!(out["status"], "ok", "{out}");
            waits_on_person(&d, started).await;
            assert!(!asked.is_finished(), "nothing allowed it");
            // Let go at Act, then at Act fully again.
            put(revision, "act").await;
            let out = approve(a, Some(json!([]))).await;
            assert_eq!(out["status"], "ok", "{out}");
            asked.await.unwrap();
            put(revision + 1, "full").await;
            crew.read_spent_today(&crew.overseers()).await;
            take().await;
        }
    })
    .await;
}

/// On devbox: at Act fully, a question Sarah's session ends its turn on is
/// answered by her own model at home, as her questions on the Mac are
/// (`answer`): the answer is said on its line as hers, and her work turns
/// aren't woken for it; what only the person can answer goes to her DM at
/// home with her next take.
#[tokio::test(flavor = "multi_thread")]
async fn a_question_of_a_session_the_homes_boss_runs_on_a_machine_is_answered_by_its_home() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_any, crew, d, _llm) = overseeing(
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
        let (code, out) = native(
            &d,
            "PUT",
            "/api/desk/grants/acme",
            grant(1, "full", &acme_api),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{out}");
        let r = crew
            .serve_crew(
                ServedCall {
                    tool: "crew.start_session".into(),
                    input: json!({"folder": acme_api.to_string_lossy(), "goal": "Ship the fix",
                                  "engine": "claude", "reason": "Next up."}),
                    boss: BossRef::Local {
                        agent: "sarah".into(),
                    },
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
        let app = super::super::relay::routes(Arc::clone(&crew.state), Arc::clone(&crew));
        let token = crew.state.auth_token.clone();
        let take = || {
            let (app, token) = (app.clone(), token.clone());
            async move {
                post(
                    &app,
                    &token,
                    relayed(
                        TAKE,
                        counted(json!({"inForce": true,
                                       "sentAt": chrono::Utc::now().timestamp_millis()})),
                        "local:sarah",
                        "full",
                    ),
                )
                .await
            }
        };
        let answer = |id: Value, text: &'static str| {
            let (app, token) = (app.clone(), token.clone());
            async move {
                post(
                    &app,
                    &token,
                    relayed(
                        DECIDED,
                        json!({"decideId": id, "text": text, "heldJobs": []}),
                        "local:sarah",
                        "full",
                    ),
                )
                .await
            }
        };
        // Takes, holding Act fully in force at home, until a question is
        // handed back; what each take said for the person is kept.
        let notes: Arc<parking_lot::Mutex<Vec<Value>>> = Arc::default();
        let asked = async |about: &str| {
            for _ in 0..200 {
                let out = take().await;
                notes
                    .lock()
                    .extend(out["body"]["notes"].as_array().cloned().unwrap_or_default());
                let handed = out["body"]["decisions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|d| d["prompt"].as_str().is_some_and(|p| p.contains(about)))
                    .cloned();
                if let Some(d) = handed {
                    return d;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("{about} wasn't handed back");
        };
        let ends_asking = async |question: &str| {
            hook(
                &url,
                json!({"hook_event_name": "Stop",
                       "last_assistant_message": format!("Built it. {question}")}),
            )
            .await;
        };

        // A home that says it takes no questions is handed none: they stay
        // the machine's to settle.
        let mut older = counted(json!({"inForce": true,
                                       "sentAt": chrono::Utc::now().timestamp_millis()}));
        older["takes"] = json!([]);
        post(&app, &token, relayed(TAKE, older, "local:sarah", "full")).await;
        assert!(crew.home_holds_full(SARAH, "acme"));
        assert!(!crew.home_takes(SARAH, "acme", Call::Question));
        take().await;
        assert!(crew.served_full(SARAH).is_some(), "unbound");
        assert!(crew.home_takes(SARAH, "acme", Call::Plan));
        ends_asking("Ship it?").await;
        let q = asked("Ship it?").await;
        assert_eq!(q["kind"], "question", "{q}");
        assert!(
            q["prompt"]
                .as_str()
                .unwrap()
                .starts_with(super::super::decide::MARK),
            "{q}"
        );
        // Her answerer has it: her work turns hear neither its end nor it.
        let work = post(
            &app,
            &token,
            relayed(
                super::super::relayed_work::WORK_TAKE,
                json!({"peek": true}),
                "local:sarah",
                "full",
            ),
        )
        .await;
        let run = &work["body"]["sessions"][0];
        assert_eq!(run["answered"], true, "{work}");
        assert!(
            work["body"]["events"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| !e["line"].as_str().unwrap_or_default().contains("finished")),
            "{work}"
        );
        let out = answer(
            q["decideId"].clone(),
            "ANSWER: Yes, ship it.\nWHY: it is built",
        )
        .await;
        assert_eq!(out["status"], "ok", "{out}");
        let line = eventually("her answer on its line", async || {
            d.line_thread(started, None, None)
                .await
                .unwrap_or_default()
                .into_iter()
                .find(|m| m.text.contains("Yes, ship it."))
        })
        .await;
        assert_eq!(line.text, "Yes, ship it.", "{line:?}");
        assert_eq!(line.kind, workforce_desk::line::LineKind::Answer);
        assert_eq!(line.from.id, SARAH);

        // Her plan is reviewed by her too, within the dialog's hold, once
        // the crew's sweep has said she answers its dialogs.
        take().await;
        tokio::time::sleep(Duration::from_millis(2500)).await;
        let plan = json!({"hook_event_name": "PreToolUse", "tool_name": "ExitPlanMode",
                          "tool_use_id": "toolu_p1",
                          "tool_input": {"plan": "# Plan: tag v9\n\n1. Tag it."}});
        let reviewed = tokio::spawn({
            let url = url.clone();
            async move { hook(&url, plan).await }
        });
        let p = asked("Plan: tag v9").await;
        assert_eq!(p["kind"], "plan", "{p}");
        answer(p["decideId"].clone(), "CHANGES: tag it on staging first.").await;
        let out = tokio::time::timeout(Duration::from_secs(20), reviewed)
            .await
            .expect("reviewed within its hold")
            .unwrap();
        let o = &out["hookSpecificOutput"];
        assert_eq!(o["permissionDecision"], "deny", "{out}");
        assert!(
            o["permissionDecisionReason"]
                .as_str()
                .is_some_and(|r| r.contains("tag it on staging first.")),
            "{out}"
        );

        // What only the person can answer goes to her DM at home.
        hook(
            &url,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "carry on"}),
        )
        .await;
        ends_asking("Which customer gets the first invite?").await;
        let q = asked("Which customer gets the first invite?").await;
        answer(
            q["decideId"].clone(),
            "ASK: which customer should get the first invite?",
        )
        .await;
        let note = eventually("her notice for the person", async || {
            take().await["body"]["notes"]
                .as_array()
                .into_iter()
                .flatten()
                .chain(notes.lock().iter())
                .find(|n| n["ask"] == "which customer should get the first invite?")
                .cloned()
        })
        .await;
        // Its fields alone: the Mac says them in its own words.
        assert_eq!(note["kind"], "asks", "{note}");
        assert_eq!(
            note["session"],
            json!(d.view(started).unwrap().label()),
            "{note}"
        );
        assert!(
            d.view(started).unwrap().asked.is_some(),
            "still the person's"
        );
        // Handed until a take says it was taken, then never again.
        let mut acked = counted(json!({"inForce": true,
                                       "sentAt": chrono::Utc::now().timestamp_millis()}));
        acked["notesTaken"] = json!([note["id"]]);
        post(&app, &token, relayed(TAKE, acked, "local:sarah", "full")).await;
        assert!(
            take().await["body"]["notes"]
                .as_array()
                .into_iter()
                .flatten()
                .all(|n| n["id"] != note["id"]),
            "taken"
        );
    })
    .await;
}
