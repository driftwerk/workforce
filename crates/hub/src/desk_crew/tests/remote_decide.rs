//! A remote boss's decision goes up its link as a `decide` frame, its
//! secrets hidden, and only the `decided` for it, on its link of the same
//! socket, answers it, once; with no link up, or the socket gone, there is
//! no decision.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_decision_goes_up_its_link_and_only_its_own_answer_comes_back() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_desk, crew, _d, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"act\"\n",
            &[],
        )
        .await;
        let boss = "driftwerk/allan";
        // No link up: no decision, said so.
        let none = crew
            .remote_decision_call(boss, "acme", "[Desk decision] x")
            .await;
        assert!(none.unwrap_err().contains("isn't connected"));

        let link = uuid::Uuid::new_v4();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let up = crew.decide_up(vec![(link, boss.into(), "acme".into())], tx);
        assert!(crew.decides_remotely(boss, "acme"));
        assert!(!crew.decides_remotely(boss, "other"));
        let secret = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let asking = {
            let crew = Arc::clone(&crew);
            let prompt = format!("[Desk decision] run `curl -H 'x-api-key: {secret}'`");
            tokio::spawn(async move { crew.remote_decision_call(boss, "acme", &prompt).await })
        };
        let frame = rx.recv().await.unwrap();
        assert_eq!(frame["type"], "decide");
        assert_eq!(frame["linkId"], json!(link));
        let sent = frame["prompt"].as_str().unwrap();
        assert!(sent.starts_with("[Desk decision]"), "{sent}");
        assert!(!sent.contains(secret), "its secrets stay here: {sent}");
        // An answer on another link, or for another decision, is no answer.
        let id = frame["decideId"].clone();
        crew.decided(
            &up,
            &json!({"type": "decided", "linkId": uuid::Uuid::new_v4(), "decideId": id, "text": "ALLOW: x"}),
        );
        crew.decided(
            &up,
            &json!({"type": "decided", "linkId": link, "decideId": uuid::Uuid::new_v4(), "text": "ALLOW: x"}),
        );
        assert!(!asking.is_finished());
        crew.decided(
            &up,
            &json!({"type": "decided", "linkId": link, "decideId": id, "text": "DENY: no\nINSTEAD: ask"}),
        );
        assert_eq!(asking.await.unwrap().unwrap(), "DENY: no\nINSTEAD: ask");

        // Longer than its hub takes: never sent, so its hub's socket isn't
        // charged with it.
        let long = format!(
            "[Desk decision] {}",
            "x".repeat(crate::boss_links::decide::MAX_PROMPT)
        );
        let refused = crew.remote_decision_call(boss, "acme", &long).await;
        assert_eq!(refused.unwrap_err(), crate::desk_crew::remote_decide::TOO_LONG);
        assert!(rx.try_recv().is_err(), "nothing went up");

        // A second socket for the same boss and group, dropped, leaves the
        // first's in place.
        let (tx2, _rx2) = tokio::sync::mpsc::channel(8);
        let up2 = crew.decide_up(vec![(uuid::Uuid::new_v4(), boss.into(), "acme".into())], tx2);
        drop(up2);
        assert!(crew.decides_remotely(boss, "acme"));

        // The socket gone: what waits hears so, and nothing goes up.
        let asking = {
            let crew = Arc::clone(&crew);
            tokio::spawn(async move { crew.remote_decision_call(boss, "acme", "[Desk decision] y").await })
        };
        let _ = rx.recv().await.unwrap();
        drop(up);
        assert!(asking.await.unwrap().unwrap_err().contains("went down"));
        assert!(!crew.decides_remotely(boss, "acme"));
    })
    .await;
}
