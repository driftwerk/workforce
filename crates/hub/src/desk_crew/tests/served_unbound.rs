//! A boss on another hub at Act fully, as a local boss is since #279: each
//! request of a session it runs is decided by its own model, one decision
//! call made on its own hub (here, the link it goes up), and nothing is
//! allowed for it by default. What it decides is done as it, recorded with
//! its reason naming its hub; a call that fails is tried once more (unless
//! its hub's code says that changes nothing), then the request is the
//! person's; and what it decided is dropped once it is no
//! longer its to decide. This engine judges all of it, as it acts.

use super::served::native;
use super::*;
use workforce_desk::decision::{How, Need};

use super::super::served::{budget_of, ServedCall, ServedStatus};

const BOSS: &str = "driftwerk/allan";

fn act(budget: Option<f64>) -> Value {
    let mut a = json!({"nudge": true, "interrupt": true, "answer_sessions": true,
                       "hand_out_work": true, "resume_restart": true, "parallel": 2});
    if let Some(b) = budget {
        a["budget_usd_per_day"] = json!(b);
    }
    a
}

fn put_grant(folder: &Path, revision: i64, level: &str, sees: &str, act: Value) -> Value {
    json!({
        "grantVersion": 1, "revision": revision, "name": "Acme",
        "boss": "remote:driftwerk/allan", "level": level, "sees": sees,
        "members": [], "folders": [folder.to_string_lossy()], "act": act,
    })
}

#[test]
fn a_grant_gives_a_daily_budget_only_in_dollars_within_act_fullys_most() {
    assert_eq!(budget_of(&json!({"budget_usd_per_day": 50})), Some(5000));
    assert_eq!(budget_of(&json!({"budget_usd_per_day": 0.004})), Some(1));
    for bad in [
        json!({}),
        json!({"budget_usd_per_day": 0}),
        json!({"budget_usd_per_day": -5}),
        json!({"budget_usd_per_day": "50"}),
        json!({"budget_usd_per_day": 1e12}),
    ] {
        assert_eq!(budget_of(&bad), None, "{bad}");
    }
}

fn pre(command: &str) -> Value {
    pre_in("default", command)
}

/// A PreToolUse hook of a session running in permission `mode`.
fn pre_in(mode: &str, command: &str) -> Value {
    json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "permission_mode": mode,
        "tool_use_id": uuid::Uuid::new_v4().to_string(),
        "tool_input": {"command": command}})
}

fn ask(url: &str, command: &str) -> tokio::task::JoinHandle<Value> {
    let url = url.to_string();
    let body = json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash",
        "tool_input": {"command": command}});
    tokio::spawn(async move { hook(&url, body).await })
}

async fn pending(d: &Desk, s: uuid::Uuid, command: &str) -> uuid::Uuid {
    eventually(command, async || {
        d.view(s).unwrap().pending.filter(|p| p.detail == command)
    })
    .await
    .id
}

/// The next `decide` frame sent up, soon.
async fn decide_frame(rx: &mut tokio::sync::mpsc::Receiver<Value>) -> Value {
    loop {
        let f = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("a decide frame")
            .expect("open");
        if f["type"] == "decide" {
            return f;
        }
    }
}

fn answer(frame: &Value, text: &str) -> Value {
    json!({"type": "decided", "linkId": frame["linkId"], "decideId": frame["decideId"], "text": text})
}

#[tokio::test(flavor = "multi_thread")]
async fn a_remote_boss_at_act_fully_decides_each_request_on_its_own_hub_and_nothing_by_default() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_desk, crew, d, llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"act\"\n",
            &[],
        )
        .await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: dir.path().to_path_buf(),
            ..Default::default()
        });
        let acme_api = dir.path().join("acme-api");
        std::fs::create_dir_all(&acme_api).unwrap();
        let mine = new_session(&d, dir.path(), "acme-web").await;
        let item = workforce_desk::session_item(mine.id);
        d.change_layout(|l| l.create("acme", "Acme", std::slice::from_ref(&item), None))
            .await
            .unwrap();
        let put = |revision, level: &'static str, sees: &'static str, act: Value| {
            let d = d.clone();
            let body = put_grant(&acme_api, revision, level, sees, act);
            async move {
                let (code, out) = native(&d, "PUT", "/api/desk/grants/acme", body).await;
                assert_eq!(code, StatusCode::OK, "{out}");
            }
        };
        // At Act, Allan starts a session: it runs it.
        put(1, "act", "everything", act(Some(50.0))).await;
        let r = crew
            .serve_crew(
                ServedCall {
                    tool: "crew.start_session".into(),
                    input: json!({"folder": acme_api.to_string_lossy(), "goal": "Ship the fix",
                                  "engine": "claude", "reason": "Next up."}),
                    boss: super::served::allan(),
                    link_id: Some("link-acme".into()),
                    call_id: Some(uuid::Uuid::new_v4().to_string()),
                    ceiling: None,
                },
                "acme",
            )
            .await;
        assert_eq!(r.status, ServedStatus::Ok, "{:?}", r.body);
        let started = eventually("Allan's session", async || {
            d.charges()
                .into_iter()
                .find(|c| c.boss == BOSS)
                .map(|c| c.session)
        })
        .await;
        let url = hook_url(dir.path(), started).await;
        let decision = |v: &Value| v["hookSpecificOutput"]["permissionDecision"].clone();

        // Below Act fully nothing of it is unbound: a call Desk doesn't
        // judge safe is refused before it runs.
        crew.read_spent_today(&crew.overseers()).await;
        assert!(crew.served_full(BOSS).is_none());
        let said = hook(&url, pre("git push --force origin main")).await;
        assert_eq!(decision(&said), "deny", "{said}");

        // At Act fully, but no budget, or Sees that hide what it asks: not
        // in force.
        put(2, "full", "everything", act(None)).await;
        crew.read_spent_today(&crew.overseers()).await;
        assert!(crew.served_full(BOSS).is_none());
        put(3, "full", "status", act(Some(50.0))).await;
        crew.read_spent_today(&crew.overseers()).await;
        assert!(crew.served_full(BOSS).is_none());

        // In force: it names nowhere to ship, so Desk judges nothing safe on
        // its account; what isn't is asked, never allowed by Desk.
        put(4, "full", "everything", act(Some(50.0))).await;
        crew.read_spent_today(&crew.overseers()).await;
        let full = crew.served_full(BOSS).expect("unbound");
        assert!(full.unbound && !full.anywhere && full.repos.is_empty());
        assert!(d.boss_unbound(BOSS));
        let said = hook(&url, pre("git push --force origin main")).await;
        assert_eq!(decision(&said), "ask", "{said}");
        let said = hook(&url, pre_in("auto", "git push --force origin main")).await;
        assert_eq!(decision(&said), "ask", "{said}");
        // In a mode that asks nothing, an asked call might run unasked: it
        // is refused there, as below Act fully.
        for mode in ["bypassPermissions", "acceptEdits", "someNewMode"] {
            let said = hook(&url, pre_in(mode, "git push --force origin main")).await;
            assert_eq!(decision(&said), "deny", "{mode}: {said}");
        }
        // Nor in a hook that says no mode: the one its last said isn't taken.
        let mut unsaid = pre("git push --force origin main");
        unsaid.as_object_mut().unwrap().remove("permission_mode");
        let said = hook(&url, unsaid).await;
        assert_eq!(decision(&said), "deny", "{said}");
        let said = hook(&url, pre("git push --force origin main")).await;
        assert_eq!(decision(&said), "ask", "{said}");

        // Its hub not connected: its request can't be decided, and is the
        // person's, said in a decision row.
        let asked = ask(&url, "git push origin main");
        let a0 = pending(&d, started, "git push origin main").await;
        // Nor does a turn of its own allow it besides its decision call.
        let r = crew
            .serve_crew(
                ServedCall {
                    tool: "crew.approve".into(),
                    input: json!({"session_id": started, "approval_id": a0, "reason": "Ship."}),
                    boss: super::served::allan(),
                    link_id: Some("link-acme".into()),
                    call_id: Some(uuid::Uuid::new_v4().to_string()),
                    ceiling: None,
                },
                "acme",
            )
            .await;
        assert_eq!(r.status, ServedStatus::Refused, "{:?}", r.body);
        assert!(d.view(started).unwrap().pending.is_some_and(|p| p.id == a0));
        eventually("left to the person", async || {
            d.view(started)
                .unwrap()
                .waits_on_you
                .is_some()
                .then_some(())
        })
        .await;
        let rows = d
            .store()
            .decisions_since(BOSS, chrono::DateTime::UNIX_EPOCH, 50)
            .await
            .unwrap();
        let left = rows
            .iter()
            .find(|r| r.need_ref == Some(a0.to_string()))
            .unwrap_or_else(|| panic!("{rows:?}"));
        assert_eq!(left.how, How::Escalated);
        assert!(left.why.contains("isn't connected"), "{}", left.why);
        assert!(
            left.what.contains("(by allan on driftwerk)"),
            "{}",
            left.what
        );
        d.decide(started, a0, workforce_desk::model::Decision::Deny, None)
            .unwrap();
        let _ = asked.await;

        // Its link up: the request goes up whole, fenced as the session's
        // words, and its model's ALLOW is done as Allan, recorded naming its
        // hub.
        let link = uuid::Uuid::new_v4();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let up = crew.decide_up(vec![(link, BOSS.into(), "acme".into())], tx);
        let asked = ask(&url, "git push --force origin main");
        let a1 = pending(&d, started, "git push --force origin main").await;
        let f = decide_frame(&mut rx).await;
        assert_eq!(f["linkId"], json!(link));
        let prompt = f["prompt"].as_str().unwrap();
        assert!(prompt.starts_with(super::super::decide::MARK), "{prompt}");
        assert!(prompt.contains("git push --force origin main"), "{prompt}");
        assert!(prompt.contains("Ship the fix"), "{prompt}");
        crew.decided(&up, &answer(&f, "ALLOW: the fix is ready to ship"));
        let answered = asked.await.unwrap();
        assert_eq!(
            answered["hookSpecificOutput"]["decision"]["behavior"], "allow",
            "{answered}"
        );
        let rows = d
            .store()
            .decisions_since(BOSS, chrono::DateTime::UNIX_EPOCH, 50)
            .await
            .unwrap();
        let row = rows
            .iter()
            .find(|r| r.need == Need::Approval && r.need_ref == Some(a1.to_string()))
            .unwrap_or_else(|| panic!("{rows:?}"));
        assert!(row.what.contains("allan on driftwerk"), "{}", row.what);
        assert!(row.why.contains("the fix is ready to ship"), "{}", row.why);
        assert_eq!(row.session, Some(started));

        // DENY, with what to do instead: the session hears why.
        let asked = ask(&url, "rm -rf target");
        pending(&d, started, "rm -rf target").await;
        let f = decide_frame(&mut rx).await;
        crew.decided(
            &up,
            &answer(&f, "DENY: it isn't needed\nINSTEAD: run cargo clean"),
        );
        let answered = asked.await.unwrap();
        let behaviour = &answered["hookSpecificOutput"]["decision"];
        assert_eq!(behaviour["behavior"], "deny", "{answered}");
        assert!(
            behaviour["message"]
                .as_str()
                .unwrap_or_default()
                .contains("cargo clean"),
            "{answered}"
        );

        // Its model fails twice: tried once more, then the person's.
        let asked = ask(&url, "make release");
        let a3 = pending(&d, started, "make release").await;
        for _ in 0..2 {
            let f = decide_frame(&mut rx).await;
            crew.decided(
                &up,
                &json!({"type": "decided", "linkId": f["linkId"], "decideId": f["decideId"],
                        "error": "its model failed"}),
            );
        }
        eventually("left to the person", async || {
            d.view(started)
                .unwrap()
                .waits_on_you
                .is_some()
                .then_some(())
        })
        .await;
        assert!(d.view(started).unwrap().pending.is_some_and(|p| p.id == a3));
        d.decide(started, a3, workforce_desk::model::Decision::Deny, None)
            .unwrap();
        let _ = asked.await;

        // Its hub holds it for its pace, as its code says: not asked again
        // at once, whatever its words, and the person's.
        let asked = ask(&url, "make docs");
        let a5 = pending(&d, started, "make docs").await;
        let f = decide_frame(&mut rx).await;
        crew.decided(
            &up,
            &json!({"type": "decided", "linkId": f["linkId"], "decideId": f["decideId"],
                    "error": "words that say nothing of a pace", "code": "paced"}),
        );
        eventually("left to the person", async || {
            d.view(started)
                .unwrap()
                .waits_on_you
                .is_some()
                .then_some(())
        })
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        while let Ok(f) = rx.try_recv() {
            assert_ne!(f["type"], "decide", "asked again: {f}");
        }
        assert!(d.view(started).unwrap().pending.is_some_and(|p| p.id == a5));
        d.decide(started, a5, workforce_desk::model::Decision::Deny, None)
            .unwrap();
        let _ = asked.await;

        // Turned down to Act while its call is out: what it decided isn't
        // done, and the request stays the person's.
        let asked = ask(&url, "git tag v1");
        let a4 = pending(&d, started, "git tag v1").await;
        let f = decide_frame(&mut rx).await;
        put(5, "act", "everything", act(Some(50.0))).await;
        assert!(crew.served_full(BOSS).is_none());
        crew.decided(&up, &answer(&f, "ALLOW: tag it"));
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(d.view(started).unwrap().pending.is_some_and(|p| p.id == a4));
        assert!(!asked.is_finished());
        d.decide(started, a4, workforce_desk::model::Decision::Deny, None)
            .unwrap();
        let _ = asked.await;

        // Back at Act fully, then Stop: nothing is its to decide.
        put(6, "full", "everything", act(Some(50.0))).await;
        crew.read_spent_today(&crew.overseers()).await;
        assert!(crew.served_full(BOSS).is_some());
        let (code, _) = native(
            &d,
            "POST",
            "/api/desk/bosses/stop",
            json!({"group": "acme", "from": "desk"}),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert!(crew.served_full(BOSS).is_none());
        assert!(!d.boss_unbound(BOSS));

        // This hub's own boss's model was never asked to decide for Allan.
        let mine = llm
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| String::from_utf8_lossy(&r.body).contains(super::super::decide::MARK))
            .count();
        assert_eq!(mine, 0);
        drop(up);
    })
    .await;
}
