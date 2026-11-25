//! At Act fully the boss's standing verdicts answer the ordinary steps of
//! a session at once (`standing`): the first request of a class goes to
//! the boss's own decision while its verdicts are asked for beside it, one
//! call a session; the next of a class it allows is allowed with no call
//! of its own, and one of a class never cached (a push to the default
//! branch) always reaches the boss's own decision.

use super::decides_at_once::{asked, behavior, calls_about, decides, THEIRS};
use super::*;

/// Its verdicts' call, as the boss's model is given it.
const STANDING: &str = "Give your standing verdict";

#[tokio::test(flavor = "multi_thread")]
async fn a_standing_verdict_allows_its_class_at_once_and_never_a_push_to_main() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let origin = tempfile::tempdir().unwrap();
        let run = |at: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(at)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
        };
        run(origin.path(), &["init", "-q", "--bare", "-b", "main"]);
        for args in [
            &["init", "-q", "-b", "main"][..],
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "x",
            ],
            &["remote", "add", "origin", &origin.path().to_string_lossy()],
            &["push", "-q", "origin", "main"],
            &["remote", "set-head", "origin", "main"],
            &["switch", "-q", "-c", "feat/x"],
        ] {
            run(&base, args);
        }
        let (_overseer, _crew, d, llm) = overseeing(&base, pool, "ok", THEIRS, &[]).await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        decides(
            &llm,
            STANDING,
            "commit: allow ordinary work on its branch\nlocal_git: allow\ntest_build: ask",
        )
        .await;
        decides(&llm, "git commit -m first", "ALLOW: its own work").await;
        decides(
            &llm,
            "git push origin main",
            "DENY: not to main\nINSTEAD: push your branch",
        )
        .await;
        eventually("Desk to know it is unbound", async || {
            d.boss_unbound("desk").then_some(())
        })
        .await;
        let s = new_session(&d, &base, "Date Picker").await;
        let url = hook_url(&base, s.id).await;
        let decided = async |command: &str| {
            let answer = tokio::time::timeout(Duration::from_secs(20), asked(&url, command))
                .await
                .expect("decided within seconds")
                .unwrap();
            behavior(&answer)
        };

        // The first of its class: the boss's own call, and its verdicts
        // asked for beside it, once.
        assert_eq!(decided("git commit -m first").await, "allow");
        assert_eq!(calls_about(&llm, "git commit -m first").await.len(), 1);
        eventually("its standing verdicts", async || {
            (calls_about(&llm, STANDING).await.len() == 1).then_some(())
        })
        .await;

        // The next of a class it allows: at once, with no call.
        assert_eq!(decided("git commit -m second").await, "allow");
        assert!(calls_about(&llm, "git commit -m second").await.is_empty());
        assert_eq!(calls_about(&llm, STANDING).await.len(), 1, "asked once");

        // Never cached: the boss's own decision every time.
        assert_eq!(decided("git push origin main").await, "deny");
        let call = calls_about(&llm, "git push origin main").await;
        assert_eq!(call.len(), 1);
        assert!(call[0].contains("default_branch_push"), "its class is said");
    })
    .await;
}
