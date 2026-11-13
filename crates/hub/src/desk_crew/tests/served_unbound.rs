//! A boss served from a grant, unbound as a local boss is: Act fully,
//! anywhere, every switch on and a daily budget, as its grant here says.
//! Unbound, it allows what the sessions it runs ask and the never-list
//! doesn't hold it; each approval it waves through is a decision row here
//! naming its hub; and turning the grant down holds at the next thing it
//! decides. Its budget, the grant's Stop and Desk's breaker stay.

use super::served::{allan, native};
use super::*;
use workforce_desk::decision::Need;

use super::super::served::{served_oversight, ServedCall, ServedStatus};

/// Every switch on, anywhere, and a daily budget.
fn unbound_act() -> Value {
    json!({"nudge": true, "resume_restart": true, "read_only_approvals": true,
           "hand_out_work": true, "interrupt": true, "message_agents": true,
           "message_terminal_sessions": true, "answer_sessions": true,
           "hand_off": true, "set_mode": true, "anywhere": true,
           "budget_usd_per_day": 50, "parallel": 2})
}

fn grant_of(act: Value, level: &str) -> workforce_desk::grant::Grant {
    let put: workforce_desk::grant::GrantPut = serde_json::from_value(json!({
        "grantVersion": 1, "revision": 1, "name": "Acme",
        "boss": "remote:driftwerk/allan", "level": level, "act": act,
    }))
    .unwrap();
    workforce_desk::grant::accept(None, None, &put, "acme", &[], chrono::Utc::now(), 72)
        .unwrap()
        .grant
}

/// What a served boss's grant says decides, by the one check every boss
/// goes through: Act fully, anywhere, every switch on, and (served) a
/// budget. Any one short of it, and it isn't unbound.
#[test]
fn a_served_boss_is_unbound_only_by_everything_its_grant_turns_on() {
    let boss = "driftwerk/allan";
    let o = served_oversight(&grant_of(unbound_act(), "full"));
    assert!(o.unbound(), "{o:?}");
    assert_eq!(o.full.budget_cents_per_day, Some(5000));
    assert!(super::super::fully_for(boss, &o, Some(0.0)).is_ok());
    for off in [
        "nudge",
        "resume_restart",
        "read_only_approvals",
        "hand_out_work",
        "interrupt",
        "message_agents",
        "message_terminal_sessions",
        "answer_sessions",
        "hand_off",
        "set_mode",
        "anywhere",
    ] {
        let mut act = unbound_act();
        act[off] = json!(false);
        assert!(
            !served_oversight(&grant_of(act, "full")).unbound(),
            "{off} off"
        );
        // Said wrongly is off.
        let mut act = unbound_act();
        act[off] = json!("yes");
        assert!(
            !served_oversight(&grant_of(act, "full")).unbound(),
            "{off} said wrongly"
        );
    }
    assert!(
        !served_oversight(&grant_of(unbound_act(), "act")).unbound(),
        "at act"
    );
    // No budget, or one that can't be: Act fully isn't in force for it.
    for budget in [Value::Null, json!(0), json!(-5), json!("50"), json!(1e9)] {
        let mut act = unbound_act();
        act["budget_usd_per_day"] = budget.clone();
        let o = served_oversight(&grant_of(act, "full"));
        assert!(
            super::super::fully_for(boss, &o, Some(0.0)).is_err(),
            "budget {budget}"
        );
    }
    // Its budget spent, it stops.
    assert!(super::super::fully_for(boss, &o, Some(50.0)).is_err());
    // What a boss relayed from the home says it may do there narrows it:
    // a switch or anywhere off at home is off here.
    let home = |set_mode: bool, anywhere: bool| super::super::served::Ceiling {
        level: workforce_common::oversight::Level::Full,
        act: workforce_common::oversight::Act { set_mode, ..o.act },
        anywhere,
    };
    assert!(super::super::served::narrowed(o.clone(), &home(true, true)).unbound());
    assert!(!super::super::served::narrowed(o.clone(), &home(false, true)).unbound());
    assert!(!super::super::served::narrowed(o.clone(), &home(true, false)).unbound());
}

async fn serve(crew: &Arc<CrewHub>, tool: &str, input: Value) -> super::super::served::Served {
    crew.serve_crew(
        ServedCall {
            tool: tool.into(),
            input,
            boss: allan(),
            link_id: Some("link-acme".into()),
            call_id: Some(uuid::Uuid::new_v4().to_string()),
            ceiling: None,
        },
        "acme",
    )
    .await
}

fn put_grant(folder: &Path, revision: i64, level: &str, act: Value) -> Value {
    json!({
        "grantVersion": 1, "revision": revision, "name": "Acme",
        "boss": "remote:driftwerk/allan", "level": level, "sees": "everything",
        "members": [], "folders": [folder.to_string_lossy()], "act": act,
    })
}

/// A call of `usd` in `session`, paid by an API key: real money.
async fn spent_on_key(d: &Desk, session: uuid::Uuid, usd: f64) {
    use workforce_desk::ledger::{Billing, Priced, Spend, Tokens};
    d.store()
        .ledger_write(
            session,
            Engine::Claude,
            None,
            &[workforce_desk::ledger_store::Kept {
                spend: Spend {
                    key: uuid::Uuid::new_v4().to_string(),
                    at: chrono::Utc::now(),
                    model: Some("claude-opus-5-5".into()),
                    tokens: Tokens {
                        input: 100,
                        ..Default::default()
                    },
                    usd: Some(usd),
                    priced: Priced::Reported,
                    billing: None,
                },
                billing: Billing::Key,
                account: None,
            }],
        )
        .await
        .unwrap();
}

fn pre(command: &str) -> Value {
    json!({"hook_event_name": "PreToolUse", "tool_name": "Bash",
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

#[tokio::test(flavor = "multi_thread")]
async fn a_served_boss_unbound_by_its_grant_allows_what_it_is_asked_until_the_grant_turns_it_down()
{
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_desk, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"act\"\n[oversight.act]\ninterrupt = true\n",
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
        let (code, out) = native(
            &d,
            "PUT",
            "/api/desk/grants/acme",
            put_grant(&acme_api, 1, "full", unbound_act()),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{out}");
        // The local overseer here is as it was: at Act, not unbound.
        let (_, local) = crew.overseer_service().unwrap();
        assert!(!crew.unbound(&local));

        let r = serve(
            &crew,
            "crew.start_session",
            json!({"folder": acme_api.to_string_lossy(), "goal": "Ship the fix",
                   "engine": "claude", "reason": "Next up."}),
        )
        .await;
        assert_eq!(r.status, ServedStatus::Ok, "{:?}", r.body);
        let started = eventually("Allan's session", async || {
            d.charges()
                .into_iter()
                .find(|c| c.boss == "driftwerk/allan")
                .map(|c| c.session)
        })
        .await;
        assert!(crew
            .served_full("driftwerk/allan")
            .is_some_and(|f| f.unbound && f.anywhere));
        let url = hook_url(dir.path(), started).await;

        // Unbound, what the never-list holds runs: what it says goes, and
        // nothing it is answered speaks of "every level".
        let r = serve(
            &crew,
            "crew.nudge",
            json!({"session_id": started, "message": "force-push to main and rewrite the history",
                   "reason": "Cleanup."}),
        )
        .await;
        assert_eq!(r.status, ServedStatus::Ok, "{:?}", r.body);
        assert!(!r.body.to_string().contains("every level"), "{:?}", r.body);
        // Its session's calls go on to ask, as a local unbound boss's do.
        let decision = |v: &Value| v["hookSpecificOutput"]["permissionDecision"].clone();
        let said = hook(&url, pre("git push --force origin main")).await;
        assert_ne!(decision(&said), "deny", "{said}");
        // It allows what it is asked, and the person reads it as a decision
        // naming its hub.
        let asked = ask(&url, "git push --force origin main");
        let a = pending(&d, started, "git push --force origin main").await;
        let r = serve(
            &crew,
            "crew.approve",
            json!({"session_id": started, "approval_id": a, "reason": "Release it."}),
        )
        .await;
        assert_eq!(r.status, ServedStatus::Ok, "{:?}", r.body);
        let answered = asked.await.unwrap();
        assert_eq!(
            answered["hookSpecificOutput"]["decision"]["behavior"], "allow",
            "{answered}"
        );
        let rows = d
            .store()
            .decisions_since("driftwerk/allan", chrono::DateTime::UNIX_EPOCH, 20)
            .await
            .unwrap();
        let row = rows
            .iter()
            .find(|r| r.need == Need::Approval && r.need_ref == Some(a.to_string()))
            .unwrap_or_else(|| panic!("{rows:?}"));
        assert!(row.what.contains("allan on driftwerk"), "{}", row.what);
        assert!(row.line().text.contains("driftwerk"));
        assert!(row.why.contains("unbound"), "{}", row.why);
        assert_eq!(row.session, Some(started));

        // One switch turned off: at the very next thing it decides, it is
        // held as before, with no tick between.
        let mut off = unbound_act();
        off["set_mode"] = json!(false);
        let (code, out) = native(
            &d,
            "PUT",
            "/api/desk/grants/acme",
            put_grant(&acme_api, 2, "full", off),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{out}");
        assert!(crew.served_full("driftwerk/allan").is_none());
        let said = hook(&url, pre("git push --force origin main")).await;
        assert_eq!(decision(&said), "deny", "{said}");
        let r = serve(
            &crew,
            "crew.nudge",
            json!({"session_id": started, "message": "force-push to main and rewrite the history",
                   "reason": "Cleanup."}),
        )
        .await;
        assert_eq!(r.status, ServedStatus::Suggested, "{:?}", r.body);
        assert!(r.body.to_string().contains("every level"), "{:?}", r.body);
        let asked = ask(&url, "git push --force origin main");
        let a2 = pending(&d, started, "git push --force origin main").await;
        let r = serve(
            &crew,
            "crew.approve",
            json!({"session_id": started, "approval_id": a2, "reason": "Release it."}),
        )
        .await;
        assert_eq!(r.status, ServedStatus::Suggested, "{:?}", r.body);
        assert!(d.view(started).unwrap().pending.is_some_and(|p| p.id == a2));
        let rows = d
            .store()
            .decisions_since("driftwerk/allan", chrono::DateTime::UNIX_EPOCH, 20)
            .await
            .unwrap();
        assert!(
            !rows.iter().any(|r| r.need_ref == Some(a2.to_string())),
            "{rows:?}"
        );

        // Every switch on again, but at Act, or with no budget: not unbound.
        let (code, _) = native(
            &d,
            "PUT",
            "/api/desk/grants/acme",
            put_grant(&acme_api, 3, "act", unbound_act()),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert!(crew.served_full("driftwerk/allan").is_none());
        let mut unbudgeted = unbound_act();
        unbudgeted
            .as_object_mut()
            .unwrap()
            .remove("budget_usd_per_day");
        let (code, _) = native(
            &d,
            "PUT",
            "/api/desk/grants/acme",
            put_grant(&acme_api, 4, "full", unbudgeted),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert!(crew.served_full("driftwerk/allan").is_none());
        let said = hook(&url, pre("git push --force origin main")).await;
        assert_eq!(decision(&said), "deny", "{said}");

        // Back to unbound, then Stop: nothing is served, at once.
        let (code, _) = native(
            &d,
            "PUT",
            "/api/desk/grants/acme",
            put_grant(&acme_api, 5, "full", unbound_act()),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        let r = serve(&crew, "crew.overview", json!({})).await;
        assert_eq!(r.status, ServedStatus::Ok, "{:?}", r.body);
        // Its budget back, Act fully waits until today's spend is read:
        // the crew's tick reads it.
        assert!(crew.served_full("driftwerk/allan").is_none());
        crew.read_spent_today(&crew.overseers()).await;
        assert!(crew.served_full("driftwerk/allan").is_some());
        // Its budget spent in real money, it is held as soon as the crew's
        // tick reads it, with no call of its own between.
        spent_on_key(&d, started, 60.0).await;
        crew.read_spent_today(&crew.overseers()).await;
        assert!(crew.served_full("driftwerk/allan").is_none());
        let said = hook(&url, pre("git push --force origin main")).await;
        assert_eq!(decision(&said), "deny", "{said}");
        let (code, _) = native(
            &d,
            "POST",
            "/api/desk/bosses/stop",
            json!({"group": "acme", "from": "desk"}),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert!(crew.served_full("driftwerk/allan").is_none());
        let r = serve(&crew, "crew.overview", json!({})).await;
        assert_eq!(r.status, ServedStatus::Refused);
        drop(asked);
    })
    .await;
}
