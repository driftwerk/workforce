//! At Act fully the boss decides every request its sessions ask, at once
//! (`decide`), as the person set it up on 11/13: a list of repositories,
//! not anywhere, switches short of every one, a few check-ins an hour.
//!
//! - A read-only command with a variable in it is handed whole to the
//!   boss's decision call the moment it is asked, allowed as the boss
//!   said, with its reason kept as a decision, and with no work turn woken
//!   for it. The same holds while the person drives the session.
//! - A deny reaches the session with what to do instead.
//! - An ask stays the person's, and the DM says what the boss asks.
//! - A decision call that fails is tried once more, then the DM says so.
//! - What Desk refuses (a write to its own settings; a session its
//!   breaker paused) waits, said once in the DM, and goes ahead as the
//!   boss decided once Desk lets it.

use super::*;
use workforce_desk::charge::How;
use workforce_desk::crew::Party;

/// The person's own `[oversight]` that day, as their config had it.
pub(super) const THEIRS: &str = "[oversight]\nenabled = true\nlevel = \"full\"\n\
    max_checkins_per_day = 150\nmax_checkins_per_hour = 6\n\
    [oversight.wake]\nwaiting_minutes = 5\nquiet_minutes = 30\n\
    [oversight.act]\nnudge = true\nresume_restart = true\nread_only_approvals = true\n\
    hand_out_work = true\ninterrupt = true\nmessage_agents = true\n\
    message_terminal_sessions = true\nanswer_sessions = true\n\
    [oversight.work]\ntake_on = \"all\"\n\
    [oversight.full]\nrepos = [\"mantis-crm/monorepo\"]\nworkflows = []\n";

/// What the session asked, as it did: a variable, then a read of it.
const READS: &str = "M=/x/memory; cat $M/man-567.md $M/production-change-window.md | head -80";

fn asked(url: &str, command: &str) -> tokio::task::JoinHandle<Value> {
    let url = url.to_string();
    let body = json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash",
        "tool_input": {"command": command}});
    tokio::spawn(async move { hook(&url, body).await })
}

fn behavior(v: &Value) -> Value {
    v["hookSpecificOutput"]["decision"]["behavior"].clone()
}

/// The boss's model answers a decision call naming `about` with `answer`.
pub(super) async fn decides(llm: &wiremock::MockServer, about: &str, answer: &str) {
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, ResponseTemplate};
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains(super::super::decide::MARK))
        .and(body_string_contains(about))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "d1", "object": "chat.completion", "model": "mock",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": answer},
                "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        })))
        .with_priority(1)
        .mount(llm)
        .await;
}

/// The decision calls made about `about`.
pub(super) async fn calls_about(llm: &wiremock::MockServer, about: &str) -> Vec<String> {
    llm.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| String::from_utf8_lossy(&r.body).to_string())
        .filter(|b| b.contains(super::super::decide::MARK) && b.contains(about))
        .collect()
}

/// How many notices in the boss's DM say all of `parts`.
pub(super) fn said(overseer: &AgentService, parts: &[&str]) -> usize {
    overseer
        .sessions
        .find(DESK_CHANNEL, PERSON)
        .ok()
        .flatten()
        .map_or(0, |dm| {
            dm.messages
                .iter()
                .filter(|m| parts.iter().all(|p| m.content.contains(p)))
                .count()
        })
}

#[tokio::test(flavor = "multi_thread")]
async fn act_fully_hands_each_request_whole_to_the_boss_at_once_and_does_as_it_decides() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, crew, d, llm) = overseeing(&base, pool, "ok", THEIRS, &[]).await;
        // As the hub sets it at start.
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        decides(&llm, "man-567", "ALLOW: it only reads its own notes").await;
        decides(&llm, "gh pr view 551", "ALLOW: it reads a pull request").await;
        decides(
            &llm,
            "rm -rf /srv/prod-data",
            "DENY: it deletes production data\nINSTEAD: work on a copy under /tmp",
        )
        .await;
        decides(
            &llm,
            "kubectl rollout restart",
            "ASK: restart production now, mid-day?",
        )
        .await;
        decides(&llm, "config.toml", "ALLOW: fine").await;
        decides(&llm, "ls -la /srv/acme", "ALLOW: it lists a folder").await;
        let (_, o) = crew.overseer_service().unwrap();
        assert!(
            crew.unbound(&o),
            "Act fully is unbound, its lists and switches aside"
        );
        eventually("Desk to know it is unbound", async || {
            d.boss_unbound("desk").then_some(())
        })
        .await;
        let since = chrono::Utc::now();

        // A session it oversees but doesn't run.
        let s = new_session(&d, &base, "Cufflink Migration").await;
        let url = hook_url(&base, s.id).await;
        let answer = tokio::time::timeout(Duration::from_secs(10), asked(&url, READS))
            .await
            .expect("decided within seconds")
            .unwrap();
        assert_eq!(behavior(&answer), "allow", "{answer}");
        assert!(
            d.view(s.id).unwrap().pending.is_none(),
            "nothing left waiting"
        );
        let call = calls_about(&llm, "man-567").await;
        assert_eq!(call.len(), 1, "one decision call");
        assert!(
            call[0].contains("production-change-window.md | head -80"),
            "the whole request, never cut short: {}",
            call[0]
        );
        let kept = eventually("its decision", async || {
            let rows = d.store().decisions_since("desk", since, 50).await.unwrap();
            rows.into_iter().find(|r| r.session == Some(s.id))
        })
        .await;
        assert_eq!(kept.why, "it only reads its own notes", "{kept:?}");
        // Past the coalesce window, no work turn was woken for it: a turn
        // the boss's own sweep wakes may carry it in Desk's notes on its
        // crew, never as what it is woken for.
        tokio::time::sleep(FAST.coalesce * 4).await;
        let woken: Vec<String> = llm
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter_map(|r| serde_json::from_slice::<Value>(&r.body).ok())
            .flat_map(|b| b["messages"].as_array().cloned().unwrap_or_default())
            .filter(|m| m["role"] == "user")
            .map(|m| m["content"].to_string())
            .filter(|c| {
                !c.contains(super::super::decide::MARK)
                    && c.rsplit("END SYSTEM CONTEXT")
                        .next()
                        .is_some_and(|asked| asked.contains("man-567"))
            })
            .collect();
        assert!(woken.is_empty(), "woken for a decided request: {woken:?}");

        // A session it runs while the person drives it: decided all the same.
        let boss = Party::manager("desk", &super::super::name_of(&overseer));
        d.take_on(s.id, &boss, How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        d.drive(s.id).await.unwrap();
        let answer = tokio::time::timeout(Duration::from_secs(10), asked(&url, "gh pr view 551"))
            .await
            .expect("decided within seconds")
            .unwrap();
        assert_eq!(behavior(&answer), "allow", "{answer}");

        // Denied, the session is told why and what to do instead.
        let answer = tokio::time::timeout(
            Duration::from_secs(10),
            asked(&url, "rm -rf /srv/prod-data"),
        )
        .await
        .expect("decided within seconds")
        .unwrap();
        assert_eq!(behavior(&answer), "deny", "{answer}");
        let message = answer["hookSpecificOutput"]["decision"]["message"].to_string();
        assert!(
            message.contains("it deletes production data") && message.contains("work on a copy"),
            "{message}"
        );

        // Asked, it is the person's, and the DM says what the boss asks.
        let ask = asked(&url, "kubectl rollout restart deploy/api");
        eventually("the DM to give the person its question", async || {
            (said(&overseer, &["restart production now, mid-day?"]) > 0).then_some(())
        })
        .await;
        let p = d.view(s.id).unwrap().pending.expect("still the person's");
        d.decide(
            s.id,
            p.id,
            workforce_desk::model::Decision::Deny,
            Some(By::you()),
        )
        .unwrap();
        assert_eq!(behavior(&ask.await.unwrap()), "deny");

        // A call that fails is tried once more, then the DM says so.
        let failing = asked(&url, "make deploy-staging");
        eventually("the DM to say it couldn't decide", async || {
            (said(&overseer, &["make deploy-staging", "couldn't decide"]) > 0).then_some(())
        })
        .await;
        assert_eq!(
            calls_about(&llm, "make deploy-staging").await.len(),
            2,
            "tried twice"
        );
        let p = d.view(s.id).unwrap().pending.expect("still the person's");
        d.decide(
            s.id,
            p.id,
            workforce_desk::model::Decision::Deny,
            Some(By::you()),
        )
        .unwrap();
        assert_eq!(behavior(&failing.await.unwrap()), "deny");

        // A write to its own settings, where its budget is, waits however
        // the command reaches them, and its DM says so once.
        let ws = overseer.workspace.display().to_string();
        let own = format!("M={ws}; sed -i s/budget/x/ $M/config.toml");
        let waits = asked(&url, &own);
        let p = eventually("its own settings, waiting", async || {
            d.view(s.id).unwrap().pending.filter(|p| p.detail == own)
        })
        .await;
        let refused = ["is waiting on you to allow", "config.toml"];
        eventually("the DM to say it waits", async || {
            (said(&overseer, &refused) > 0).then_some(())
        })
        .await;
        // Several sweeps on, it is still said once, and still waits.
        tokio::time::sleep(Duration::from_millis(4500)).await;
        assert_eq!(said(&overseer, &refused), 1, "said once");
        assert!(d.view(s.id).unwrap().pending.is_some_and(|q| q.id == p.id));
        assert_eq!(
            calls_about(&llm, "config.toml").await.len(),
            1,
            "decided once"
        );
        d.decide(
            s.id,
            p.id,
            workforce_desk::model::Decision::Deny,
            Some(By::you()),
        )
        .unwrap();
        assert_eq!(behavior(&waits.await.unwrap()), "deny");

        // A session Desk's breaker paused goes on only at the person's
        // word: what it asks waits, and goes ahead as the boss decided
        // once they say Continue.
        d.set_breaker_rules(workforce_desk::Rules { loops: true })
            .await
            .unwrap();
        for _ in 0..12 {
            hook(
                &url,
                json!({"hook_event_name": "PreToolUse", "tool_name": "Bash",
                    "tool_input": {"command": "cargo test -p acme"}}),
            )
            .await;
        }
        let subject = format!("session:{}", s.id);
        eventually("the breaker to pause it", async || {
            d.breaker_needs()
                .into_iter()
                .find(|n| n.id == format!("breaker:{subject}"))
        })
        .await;
        let paused = asked(&url, "ls -la /srv/acme");
        eventually("the DM to say the breaker holds it", async || {
            (said(&overseer, &["`Bash(ls -la /srv/acme)`", "breaker"]) > 0).then_some(())
        })
        .await;
        assert!(
            d.view(s.id).unwrap().pending.is_some(),
            "the breaker holds it"
        );
        d.breaker_continue(&subject, By::you()).unwrap();
        let answer = tokio::time::timeout(Duration::from_secs(10), paused)
            .await
            .expect("allowed once the person lets it go on")
            .unwrap();
        assert_eq!(behavior(&answer), "allow", "{answer}");
    })
    .await;
}

/// What the boss decided but Desk refused for now is done only while the
/// boss still decides it: the level lowered meanwhile, the request is the
/// person's again, and the DM says so.
#[tokio::test(flavor = "multi_thread")]
async fn a_decision_desk_refused_is_dropped_once_act_fully_isnt_in_force() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, _crew, d, llm) = overseeing(&base, pool, "ok", THEIRS, &[]).await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        decides(&llm, "ls -la /srv/acme", "ALLOW: it lists a folder").await;
        eventually("Desk to know it is unbound", async || {
            d.boss_unbound("desk").then_some(())
        })
        .await;
        let s = new_session(&d, &base, "acme-web").await;
        let url = hook_url(&base, s.id).await;
        d.set_breaker_rules(workforce_desk::Rules { loops: true })
            .await
            .unwrap();
        for _ in 0..12 {
            hook(
                &url,
                json!({"hook_event_name": "PreToolUse", "tool_name": "Bash",
                    "tool_input": {"command": "cargo test -p acme"}}),
            )
            .await;
        }
        let subject = format!("session:{}", s.id);
        eventually("the breaker to pause it", async || {
            d.breaker_needs()
                .into_iter()
                .find(|n| n.id == format!("breaker:{subject}"))
        })
        .await;
        let paused = asked(&url, "ls -la /srv/acme");
        eventually("the DM to say the breaker holds it", async || {
            (said(&overseer, &["`Bash(ls -la /srv/acme)`", "breaker"]) > 0).then_some(())
        })
        .await;
        // The person lowers the level, then lets the session go on.
        write_config(
            &overseer,
            &THEIRS.replace("level = \"full\"", "level = \"act\""),
        );
        eventually("Act fully to end", async || {
            (!d.boss_unbound("desk")).then_some(())
        })
        .await;
        d.breaker_continue(&subject, By::you()).unwrap();
        eventually("the DM to say it is the person's again", async || {
            (said(&overseer, &["yours again"]) > 0).then_some(())
        })
        .await;
        tokio::time::sleep(Duration::from_millis(4500)).await;
        let p = d.view(s.id).unwrap().pending.expect("still the person's");
        d.decide(
            s.id,
            p.id,
            workforce_desk::model::Decision::Deny,
            Some(By::you()),
        )
        .unwrap();
        assert_eq!(behavior(&paused.await.unwrap()), "deny");
    })
    .await;
}

/// The person's standing orders are the rule, both ways, and reach the
/// decision call in their own words: an order that forbids production
/// gets a converge run denied, with what to do instead; one that wants
/// work shipped gets it allowed. Desk holds no stance of its own on
/// production.
#[tokio::test(flavor = "multi_thread")]
async fn the_persons_orders_decide_a_production_step_both_ways() {
    for (order, verdict, behaves) in [
        (
            "Do not let anything touch production tonight",
            "DENY: the person said nothing may touch production tonight\nINSTEAD: run the converge against staging",
            "deny",
        ),
        (
            "Shipping the cufflink converge to production tonight is vital, as long as nothing breaks",
            "ALLOW: the person wants it shipped; the session checks nothing broke",
            "allow",
        ),
    ] {
        with_test_pool(workforce_desk::store::build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let base = std::fs::canonicalize(dir.path()).unwrap();
            let (_overseer, crew, d, llm) = overseeing(&base, pool, "ok", THEIRS, &[]).await;
            d.boss_bounds_with(workforce_desk::charge::Bounds {
                home: base.clone(),
                ..Default::default()
            });
            // It decides only what the person's own words, quoted, say.
            decides(&llm, &format!("“{order}”"), verdict).await;
            let (_, o) = crew.overseer_service().unwrap();
            assert!(crew.unbound(&o));
            eventually("Desk to know it is unbound", async || {
                d.boss_unbound("desk").then_some(())
            })
            .await;
            d.store()
                .item_open(&workforce_desk::open_item::NewItem::order(
                    "desk",
                    order,
                    vec![],
                    Some("m1"),
                ))
                .await
                .unwrap();
            let s = new_session(&d, &base, "Cufflink Migration").await;
            let url = hook_url(&base, s.id).await;
            let answer = tokio::time::timeout(
                Duration::from_secs(10),
                asked(&url, "make converge ENV=production"),
            )
            .await
            .expect("decided within seconds")
            .unwrap();
            assert_eq!(behavior(&answer), behaves, "{order}: {answer}");
            if behaves == "deny" {
                let message = answer["hookSpecificOutput"]["decision"]["message"].to_string();
                assert!(message.contains("against staging"), "{message}");
            }
            let call = calls_about(&llm, "make converge ENV=production").await;
            assert_eq!(call.len(), 1, "one decision call");
            assert!(
                !call[0].contains("a production or irreversible step you judge they must make"),
                "no stance of Desk's own on production"
            );
        })
        .await;
    }
}
