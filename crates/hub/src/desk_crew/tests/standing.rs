//! At Act fully the boss's standing verdicts answer the ordinary steps of
//! a session at once (`standing`): the first request of a class goes to
//! the boss's own decision while its verdicts are asked for beside it, one
//! call a session; the next of a class it allows is allowed with no call
//! of its own, and one of a class never cached (a push to the default
//! branch) always reaches the boss's own decision.

use super::act_fully::git;
use super::decides_at_once::{asked, behavior, calls_about, decides, THEIRS};
use super::*;

/// Its verdicts' call, as the boss's model is given it.
const STANDING: &str = "Give your standing verdict";

/// `base` as a repository on its own branch `feat/x`, `main` its
/// default on an `origin` beside it, which it returns.
fn on_a_branch(base: &std::path::Path) -> tempfile::TempDir {
    let origin = tempfile::tempdir().unwrap();
    git(origin.path(), &["init", "-q", "--bare", "-b", "main"]);
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["commit", "-q", "--allow-empty", "-m", "x"],
        &["remote", "add", "origin", &origin.path().to_string_lossy()],
        &["push", "-q", "origin", "main"],
        &["remote", "set-head", "origin", "main"],
        &["switch", "-q", "-c", "feat/x"],
    ] {
        git(base, args);
    }
    origin
}

#[tokio::test(flavor = "multi_thread")]
async fn a_standing_verdict_allows_its_class_at_once_and_never_a_push_to_main() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let _origin = on_a_branch(&base);
        let (_overseer, crew, d, llm) = overseeing(&base, pool, "ok", THEIRS, &[]).await;
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
            (calls_about(&llm, STANDING).await.len() == 1 && crew.standings.given(s.id))
                .then_some(())
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

        // Desk's own refusal (an agent's own settings, as the hub serves
        // them) wins over a standing allow of its class: never allowed.
        let settings = crew
            .services()
            .iter()
            .flat_map(|s| crate::desk_computer::own_config(&s.workspace))
            .next()
            .expect("the agent's own settings");
        let held = asked(&url, &format!("git add {}", settings.display()));
        tokio::time::sleep(Duration::from_secs(3)).await;
        if held.is_finished() {
            let answer = held.await.unwrap();
            assert_ne!(behavior(&answer), "allow", "{answer}");
        }
    })
    .await;
}

/// What a standing verdict never answers: a session whose person spoke to
/// it since, the boss in a chat turn with the person (and after it, until
/// its verdicts are asked again), a held session, and a directive that
/// holds only sometimes. Desk's own refusals (its breaker) win over one
/// that allows.
#[tokio::test(flavor = "multi_thread")]
async fn a_standing_verdict_gives_way_to_the_person_a_hold_a_conditional_directive_and_desk() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let _origin = on_a_branch(&base);
        let (_overseer, crew, d, llm) = overseeing(&base, pool, "ok", THEIRS, &[]).await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        decides(&llm, STANDING, "commit: allow ordinary work on its branch").await;
        decides(&llm, "git commit -m", "ALLOW: its own work").await;
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
            assert_eq!(behavior(&answer), "allow", "{command}: {answer}");
            calls_about(&llm, command).await.len()
        };
        let verdicts = async |n: usize, of: uuid::Uuid| {
            eventually("its standing verdicts", async || {
                (calls_about(&llm, STANDING).await.len() == n && crew.standings.given(of))
                    .then_some(())
            })
            .await
        };
        assert_eq!(decided("git commit -m c1").await, 1);
        verdicts(1, s.id).await;
        assert_eq!(decided("git commit -m c2").await, 0, "standing");

        // The person's words to it since: asked again.
        hook(
            &url,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "only small commits from now on"}),
        )
        .await;
        assert_eq!(decided("git commit -m c3").await, 1);
        verdicts(2, s.id).await;
        assert_eq!(decided("git commit -m c4").await, 0);

        // The boss in a turn with the person: its own call, and once the
        // turn ends, its verdicts are asked again.
        let turn = crew.person_turn_began("desk");
        assert_eq!(decided("git commit -m c5").await, 1);
        drop(turn);
        assert_eq!(decided("git commit -m c6").await, 1);
        verdicts(3, s.id).await;
        assert_eq!(decided("git commit -m c7").await, 0);

        // Held: every request is the boss's own call.
        let directive = d
            .store()
            .directive_own(workforce_desk::rooms::YOU, "main", "Releases need my OK")
            .await
            .unwrap()
            .unwrap();
        d.store()
            .hold_put(&workforce_desk::hold_store::Hold {
                session: s.id,
                boss: "desk".into(),
                owner: workforce_desk::rooms::YOU.into(),
                directive_id: directive.id,
                question: uuid::Uuid::new_v4(),
                until: workforce_desk::hold_store::Until::DirectiveWithdrawn,
            })
            .await
            .unwrap();
        assert_eq!(decided("git commit -m c8").await, 1);
        d.store().hold_end(s.id).await.unwrap();

        // Desk's breaker holds a session whatever a standing verdict says.
        let other = new_session(&d, &base, "Date Picker Docs").await;
        let other_url = hook_url(&base, other.id).await;
        let first = tokio::time::timeout(
            Duration::from_secs(20),
            asked(&other_url, "git commit -m o1"),
        )
        .await
        .expect("decided within seconds")
        .unwrap();
        assert_eq!(behavior(&first), "allow", "{first}");
        verdicts(4, other.id).await;
        d.set_breaker_rules(workforce_desk::Rules { loops: true })
            .await
            .unwrap();
        for _ in 0..12 {
            hook(
                &other_url,
                json!({"hook_event_name": "PreToolUse", "tool_name": "Bash",
                    "tool_input": {"command": "cargo test -p acme"}}),
            )
            .await;
        }
        let subject = format!("session:{}", other.id);
        eventually("the breaker to pause it", async || {
            d.breaker_needs()
                .into_iter()
                .find(|n| n.id == format!("breaker:{subject}"))
        })
        .await;
        let paused = asked(&other_url, "git commit -m c11");
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(!paused.is_finished(), "the breaker holds it");
        assert!(
            calls_about(&llm, "git commit -m c11").await.is_empty(),
            "its standing verdict allows it, and Desk refuses that"
        );
        d.breaker_continue(&subject, By::you()).unwrap();
        let answer = tokio::time::timeout(Duration::from_secs(20), paused)
            .await
            .expect("decided once it goes on")
            .unwrap();
        assert_eq!(behavior(&answer), "allow", "{answer}");

        // A directive that holds only sometimes: never answered standing.
        d.store()
            .item_open(&workforce_desk::open_item::NewItem::order(
                "desk",
                "commit only if CI is green",
                vec![s.id],
                Some("m1"),
            ))
            .await
            .unwrap();
        let before = calls_about(&llm, STANDING).await.len();
        assert_eq!(decided("git commit -m c9").await, 1);
        assert_eq!(decided("git commit -m c10").await, 1);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(calls_about(&llm, STANDING).await.len(), before, "none asked");

        // A session gone from Desk: its verdicts go with it.
        assert!(crew.standings.holds(other.id));
        d.remove(other.id, workforce_desk::Removal::default())
            .await
            .unwrap();
        decided("git commit -m c12").await;
        assert!(!crew.standings.holds(other.id));

    })
    .await;
}

/// `base` on its own branch `feat/x` off `main`, its `origin` on GitHub,
/// which has `main`, as its `origin/HEAD`: work reaches it by pull request.
fn on_github(base: &std::path::Path) {
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["commit", "-q", "--allow-empty", "-m", "x"],
        &["remote", "add", "origin", "https://github.com/acme/web.git"],
        &["update-ref", "refs/remotes/origin/main", "HEAD"],
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
        &["switch", "-q", "-c", "feat/x"],
    ] {
        git(base, args);
    }
}

/// Where pull requests merge into the default branch, a session's push
/// straight to it is the boss's own refusal at once, with no call, in
/// favour of review; a push to a release branch is still the boss's to
/// decide.
#[tokio::test(flavor = "multi_thread")]
async fn a_push_straight_to_a_default_branch_pull_requests_merge_into_is_refused_at_once() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        on_github(&base);
        let (_overseer, _crew, d, llm) = overseeing(&base, pool, "ok", THEIRS, &[]).await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        decides(&llm, STANDING, "commit: allow ordinary work on its branch").await;
        decides(
            &llm,
            "git push origin HEAD:release-2026",
            "DENY: not yet\nINSTEAD: wait for the release",
        )
        .await;
        eventually("Desk to know it is unbound", async || {
            d.boss_unbound("desk").then_some(())
        })
        .await;
        let s = new_session(&d, &base, "Date Picker").await;
        let url = hook_url(&base, s.id).await;
        let decided = async |command: &str| {
            tokio::time::timeout(Duration::from_secs(20), asked(&url, command))
                .await
                .expect("decided within seconds")
                .unwrap()
        };

        for _ in 0..2 {
            let answer = decided("git push origin main").await;
            assert_eq!(behavior(&answer), "deny", "{answer}");
            let why = answer["hookSpecificOutput"]["decision"]["message"].to_string();
            assert!(why.contains(workforce_desk::plan::DEFAULT_PUSH), "{why}");
        }
        assert!(calls_about(&llm, "git push origin main").await.is_empty());

        let release = decided("git push origin HEAD:release-2026").await;
        assert_eq!(behavior(&release), "deny");
        assert_eq!(
            calls_about(&llm, "git push origin HEAD:release-2026")
                .await
                .len(),
            1,
            "the boss's own call"
        );
    })
    .await;
}
