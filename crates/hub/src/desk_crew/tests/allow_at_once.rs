//! Act fully allows every request at once (`allow`), as the person set it
//! up on 11/13: a list of repositories, not anywhere, switches short of
//! every one, a few check-ins an hour. A read-only command with a variable
//! in it (which Claude Code can't check before it runs, and the boss's own
//! turn wouldn't approve, as its view of it was cut short) is allowed the
//! moment it is asked, with no turn of the boss's, while the person drives
//! the session too, and kept as the boss's decision. What reads its own
//! settings wait, however the command reaches them, and so does what a
//! session Desk's breaker paused asks; its DM says each once.

use super::*;
use workforce_desk::charge::How;
use workforce_desk::crew::Party;

/// The person's own `[oversight]` that day, as their config had it.
const THEIRS: &str = "[oversight]\nenabled = true\nlevel = \"full\"\n\
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

#[tokio::test(flavor = "multi_thread")]
async fn act_fully_allows_a_read_with_a_variable_at_once_and_keeps_it_as_a_decision() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, crew, d, llm) = overseeing(&base, pool, "ok", THEIRS, &[]).await;
        // As the hub sets it at start.
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
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
        let calls = llm.received_requests().await.unwrap_or_default().len();
        let answer = tokio::time::timeout(Duration::from_secs(10), asked(&url, READS))
            .await
            .expect("answered within seconds")
            .unwrap();
        assert_eq!(behavior(&answer), "allow", "{answer}");
        assert!(
            d.view(s.id).unwrap().pending.is_none(),
            "nothing left waiting"
        );
        assert_eq!(
            llm.received_requests().await.unwrap_or_default().len(),
            calls,
            "no model call stood between the request and its answer"
        );
        // Past the coalesce window too, no turn was woken for it: a turn
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
            // What it is woken with, past the hub's per-turn context.
            .filter(|c| {
                c.rsplit("END SYSTEM CONTEXT")
                    .next()
                    .is_some_and(|asked| asked.contains("man-567"))
            })
            .collect();
        assert!(woken.is_empty(), "woken for an allowed request: {woken:?}");
        let kept = eventually("its decision", async || {
            let rows = d.store().decisions_since("desk", since, 50).await.unwrap();
            rows.into_iter().find(|r| r.session == Some(s.id))
        })
        .await;
        assert!(kept.why.starts_with(super::super::allow::WHY), "{kept:?}");
        assert!(kept.why.contains("M=/x/memory"), "{kept:?}");

        // A session it runs while the person drives it: allowed all the same.
        let boss = Party::manager("desk", &super::super::name_of(&overseer));
        d.take_on(s.id, &boss, How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        d.drive(s.id).await.unwrap();
        let answer = tokio::time::timeout(Duration::from_secs(10), asked(&url, "gh pr view 551"))
            .await
            .expect("answered within seconds")
            .unwrap();
        assert_eq!(behavior(&answer), "allow", "{answer}");

        // Its own settings, where its budget is, wait on the person, however
        // the command reaches them, and its DM says so once.
        let ws = overseer.workspace.display().to_string();
        let own = format!("M={ws}; sed -n 1,5p $M/config.toml");
        let waits = asked(&url, &own);
        let p = eventually("its own settings, waiting", async || {
            d.view(s.id).unwrap().pending.filter(|p| p.detail == own)
        })
        .await;
        let said = || {
            overseer
                .sessions
                .find(DESK_CHANNEL, PERSON)
                .ok()
                .flatten()
                .map_or(0, |dm| {
                    dm.messages
                        .iter()
                        .filter(|m| {
                            m.content.contains("is waiting on you to allow")
                                && m.content.contains("config.toml")
                        })
                        .count()
                })
        };
        eventually("the DM to say it waits", async || {
            (said() > 0).then_some(())
        })
        .await;
        // Several sweeps on, it is still said once, and still waits.
        tokio::time::sleep(Duration::from_millis(4500)).await;
        assert_eq!(said(), 1, "said once");
        assert!(d.view(s.id).unwrap().pending.is_some_and(|q| q.id == p.id));
        d.decide(
            s.id,
            p.id,
            workforce_desk::model::Decision::Deny,
            Some(By::you()),
        )
        .unwrap();
        assert_eq!(behavior(&waits.await.unwrap()), "deny");

        // A session Desk's breaker paused goes on only at the person's
        // word: what it asks waits, and is allowed once they say Continue.
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
        let paused = asked(&url, "ls");
        eventually("the DM to say the breaker holds it", async || {
            let dm = overseer
                .sessions
                .find(DESK_CHANNEL, PERSON)
                .ok()
                .flatten()?;
            dm.messages
                .iter()
                .any(|m| m.content.contains("`Bash(ls)`") && m.content.contains("breaker"))
                .then_some(())
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
