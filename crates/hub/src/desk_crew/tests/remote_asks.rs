//! What a remote boss's group's sessions ask the person goes up its link
//! as an `ask`, once, and the answer that comes back is taken only for its
//! ask, from its person, with its nonce, once (RB7b).

use super::served::{allan, grant, native};
use super::*;

const PERSON: &str = "4b1f7c2a-0000-4000-8000-000000000001";

fn decision(frame: &Value, choice: &str, person: &str) -> Value {
    json!({
        "type": "decision",
        "askId": frame["askId"],
        "linkId": frame["linkId"],
        "nonce": frame["nonce"],
        "choice": choice,
        "decider": {"identityId": person, "via": "slack", "slackUserId": "U1"},
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sessions_ask_goes_up_once_and_its_answer_is_taken_once_from_its_person() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_desk, crew, d, _llm) = overseeing(
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
        let mine = new_session(&d, dir.path(), "acme-web").await;
        let item = workforce_desk::session_item(mine.id);
        d.change_layout(|l| l.create("acme", "Acme", std::slice::from_ref(&item), None))
            .await
            .unwrap();
        let (code, out) = native(
            &d,
            "PUT",
            "/api/desk/grants/acme",
            grant(&[mine.id], dir.path(), "act", "replies"),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{out}");
        let link = uuid::Uuid::new_v4();
        let url = hook_url(dir.path(), mine.id).await;
        let ask = |command: &str| {
            let url = url.clone();
            let body = json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash",
                "tool_input": {"command": command}});
            tokio::spawn(async move { hook(&url, body).await })
        };
        let pending = |command: &'static str| {
            let d = d.clone();
            async move {
                eventually(command, async || {
                    d.view(mine.id)
                        .unwrap()
                        .pending
                        .filter(|p| p.detail == command)
                })
                .await
                .id
            }
        };
        let up = |again: bool| {
            let crew = Arc::clone(&crew);
            async move { crew.asks_up(link, "acme", &allan(), again).await }
        };
        let decide = |frame: Value| {
            let crew = Arc::clone(&crew);
            async move {
                crew.remote_decision(link, "acme", &allan(), Some(PERSON), &frame)
                    .await
            }
        };
        let behavior = |v: Value| v["hookSpecificOutput"]["decision"]["behavior"].clone();

        // A request goes up once, its words as Sees shows its question.
        let asked = ask("cargo build --release");
        pending("cargo build --release").await;
        let frames = up(false).await;
        assert_eq!(frames.len(), 1, "{frames:?}");
        let first = frames[0].clone();
        assert_eq!(first["type"], "ask");
        assert_eq!(first["class"], "anywhere");
        assert_eq!(first["options"], json!(["allow", "deny", "stop"]));
        assert_eq!(first["subject"], json!(mine.id));
        assert!(first["detail"].as_str().unwrap().contains("cargo build"));
        assert_eq!(first["nonce"].as_str().unwrap().len(), 64);
        assert!(up(false).await.is_empty(), "once a request");

        // Not for another link, not with another nonce, not from another
        // person, not with what it doesn't offer.
        let back = crew
            .remote_decision(
                uuid::Uuid::new_v4(),
                "acme",
                &allan(),
                Some(PERSON),
                &decision(&first, "allow", PERSON),
            )
            .await;
        assert_eq!(back["code"], "other_link", "{back}");
        let mut forged = decision(&first, "allow", PERSON);
        forged["nonce"] = json!("00".repeat(32));
        assert_eq!(decide(forged).await["code"], "wrong_nonce");
        assert_eq!(
            decide(decision(&first, "allow", "someone-else")).await["code"],
            "not_the_person"
        );
        assert_eq!(
            decide(decision(&first, "always", PERSON)).await["code"],
            "not_an_option"
        );
        // Its person, its nonce: allowed, and the session hears it; never
        // twice.
        let back = decide(decision(&first, "allow", PERSON)).await;
        assert_eq!(back["type"], "ask_closed", "{back}");
        assert_eq!(back["how"], "decided");
        assert_eq!(behavior(asked.await.unwrap()), "allow");
        let again = decide(decision(&first, "deny", PERSON)).await;
        assert_eq!(again["type"], "ask_closed", "{again}");
        assert_eq!(again["how"], "decided");

        // Answered in Desk first: the ask is closed, and a late answer
        // does nothing.
        let _asked = ask("cargo test --all");
        let a = pending("cargo test --all").await;
        let second = up(false).await.remove(0);
        d.decide(
            mine.id,
            a,
            workforce_desk::model::Decision::Deny,
            Some(By::you()),
        )
        .unwrap();
        let closed = up(false).await;
        assert_eq!(
            closed,
            vec![
                json!({"type": "ask_closed", "askId": second["askId"], "how": "answered_in_desk"})
            ]
        );
        let late = decide(decision(&second, "allow", PERSON)).await;
        assert_eq!(late["type"], "ask_closed");

        // What widens is the person's in Desk: allowing it from elsewhere
        // is refused, denying it is taken.
        let secret = ask("cat ~/.aws/credentials");
        pending("cat ~/.aws/credentials").await;
        let third = up(false).await.remove(0);
        assert_eq!(third["class"], "desk_only");
        assert_eq!(third["options"], json!(["deny", "stop"]));
        assert_eq!(
            decide(decision(&third, "allow", PERSON)).await["code"],
            "desk_only"
        );

        // On a new socket it goes up again with a new nonce; the old one is
        // good no longer.
        let renewed = up(true).await;
        assert_eq!(renewed.len(), 1, "{renewed:?}");
        assert_eq!(renewed[0]["askId"], third["askId"]);
        assert_ne!(renewed[0]["nonce"], third["nonce"]);
        assert_eq!(
            decide(decision(&third, "deny", PERSON)).await["code"],
            "wrong_nonce"
        );
        let back = decide(decision(&renewed[0], "deny", PERSON)).await;
        assert_eq!(back["how"], "decided", "{back}");
        assert_eq!(behavior(secret.await.unwrap()), "deny");

        // Each answer from elsewhere is in the engine's audit.
        let calls = d.store().boss_calls_latest(50).await.unwrap();
        assert!(
            calls.iter().any(|c| c.0 == "boss.decision" && c.1 == "ok"),
            "{calls:?}"
        );
        assert!(calls
            .iter()
            .any(|c| c.0 == "boss.decision" && c.1 == "refused"));

        // Stopped, its open asks are withdrawn.
        let _asked = ask("ls -la");
        pending("ls -la").await;
        let fourth = up(false).await.remove(0);
        let (code, _) = native(
            &d,
            "POST",
            "/api/desk/bosses/stop",
            json!({"group": "acme", "from": "desk"}),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        let closed = up(false).await;
        assert_eq!(
            closed,
            vec![json!({"type": "ask_closed", "askId": fourth["askId"], "how": "withdrawn"})]
        );
        assert_ne!(
            decide(decision(&fourth, "allow", PERSON)).await["how"],
            "decided"
        );
    })
    .await;
}
