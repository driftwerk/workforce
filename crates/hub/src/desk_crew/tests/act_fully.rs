//! Act fully (`level = "full"`, remote-bosses design §8): in the
//! repositories the person named, the overseer allows and answers what
//! merges, tags, promotes and deploys, in the sessions it runs and the
//! others; in any other repository that stays a suggestion; and the hard
//! limits (`workforce_desk::never`) stay the person's at every level.

use super::super::actions::{Action, Gate, Target};
use super::*;
use workforce_common::oversight::{Level, Oversight, FULL_NO_BUDGET, FULL_NO_REPOS};

const FULL: &str = "[oversight]\nenabled = true\nlevel = \"full\"\n\
                    [oversight.full]\nrepos = [\"acme/acme-web\"]\nbudget_usd_per_day = 25\n";

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// A checkout at `dir` on `feat/x`, its `origin` at `origin`, whose default
/// branch is `main`.
fn repo(dir: &Path, origin: &str) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "feat/x"]);
    git(dir, &["remote", "add", "origin", origin]);
    git(
        dir,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn act_fully_ships_in_the_named_repositories_and_never_past_the_hard_limits() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let acme = base.join("acme-web");
        let globex = base.join("globex-api");
        repo(&acme, "git@github.com:acme/acme-web.git");
        repo(&globex, "git@github.com:globex/globex-api.git");
        let (overseer, crew, d, _llm) = overseeing(&base, pool, "ok", FULL, &[]).await;
        eventually("the overseer's tools", async || {
            overseer.tools.has("crew.approve").then_some(())
        })
        .await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        let id = overseer.agent_id.as_str().to_string();
        // In force once today's spend is read, as the sync loop does.
        crew.read_spent_today(&crew.overseers()).await;
        let (_, o) = crew.overseer_service().unwrap();
        assert_eq!(crew.level_now(&id, &o), (Level::Full, None));

        let boss = workforce_desk::crew::Party::manager("desk", &super::super::name_of(&overseer));
        let take = |s: uuid::Uuid| {
            let d = d.clone();
            let boss = boss.clone();
            async move {
                d.take_on(
                    s,
                    &boss,
                    workforce_desk::charge::How::Handed,
                    &Default::default(),
                    &By::you(),
                )
                .await
                .unwrap();
            }
        };
        let tool = |name: &str| overseer.tools.get(name).unwrap();
        let ask = |url: &str, command: &str| {
            let url = url.to_string();
            let body = json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash",
                "tool_input": {"command": command}});
            tokio::spawn(async move { hook(&url, body).await })
        };
        let pending = |s: uuid::Uuid, command: &'static str| {
            let d = d.clone();
            async move {
                eventually(command, async || {
                    d.view(s).unwrap().pending.filter(|p| p.detail == command)
                })
                .await
                .id
            }
        };
        let decided = |v: Value| v["hookSpecificOutput"]["decision"]["behavior"].clone();
        let approve = |s: uuid::Uuid, a: uuid::Uuid| {
            json!({"session_id": s.to_string(), "approval_id": a.to_string(), "reason": "Tests pass."})
        };

        // A session it runs in acme-web: merging, tagging, promoting and
        // pushing to main are its to allow, and Desk agrees.
        let s = new_session(&d, &acme, "acme-web").await;
        let url = hook_url(&base, s.id).await;
        take(s.id).await;
        for command in [
            "gh pr merge 42 --squash",
            "git tag -a v1.2.0 -m 'Date picker'",
            "git push origin v1.2.0",
            "gh workflow run promote-staging.yml",
            "git push origin HEAD:main",
            "npm run deploy",
        ] {
            let asked = ask(&url, command);
            let a = pending(s.id, command).await;
            let done = tool("crew.approve").execute(approve(s.id, a)).await.unwrap();
            assert!(done.output["done"].is_string(), "{command}: {done:?}");
            assert_eq!(decided(asked.await.unwrap()), "allow", "{command}");
        }
        // Said in Activity as done at Act fully.
        eventually("at Act fully in Activity", async || {
            d.activity(Filter::All, None, 50)
                .await
                .unwrap()
                .events
                .iter()
                .any(|e| e.reason.as_deref().is_some_and(|r| r.ends_with("(at Act fully)")))
                .then_some(())
        })
        .await;

        // The hard limits stay the person's, Act fully or not.
        for (command, why) in [
            ("cargo publish -p acme-core", "package registry"),
            ("source ~/.aws-creds && aws s3 ls", "secrets"),
            ("git push --force origin main", "can't be undone"),
            ("gh release delete v1.1.0 --yes", "on the remote"),
            ("gh pr merge 42 --squash --delete-branch", "on the remote"),
        ] {
            let asked = ask(&url, command);
            let a = pending(s.id, command).await;
            let held = tool("crew.approve").execute(approve(s.id, a)).await.unwrap();
            assert!(held.output["suggested"].is_string(), "{command}: {held:?}");
            let note = held.output["note"].as_str().unwrap();
            assert!(note.contains(why), "{command}: {note}");
            tool("crew.deny").execute(approve(s.id, a)).await.unwrap();
            assert_eq!(decided(asked.await.unwrap()), "deny", "{command}");
        }

        // Its own settings, and a credentials file read by a tool rather
        // than a command, stay the person's too.
        let own_config = overseer.workspace.join("config.toml");
        let env_file = acme.join(".env");
        for (tool_name, input, why) in [
            // Withheld as Desk's data is: reading another folder is
            // otherwise the boss's to allow.
            (
                "Bash",
                json!({"command": format!("cat {}", own_config.display())}),
                "secrets",
            ),
            (
                "Bash",
                json!({"command": format!("cat {}", env_file.display())}),
                "secrets",
            ),
        ] {
            let url = url.clone();
            let body = json!({"hook_event_name": "PermissionRequest", "tool_name": tool_name,
                "tool_input": input});
            let asked = tokio::spawn(async move { hook(&url, body).await });
            let detail = input["command"].as_str().unwrap().to_string();
            let a = eventually(&detail, async || {
                d.view(s.id).unwrap().pending.filter(|p| p.detail == detail)
            })
            .await
            .id;
            let held = tool("crew.approve").execute(approve(s.id, a)).await.unwrap();
            assert!(held.output["suggested"].is_string(), "{tool_name}: {held:?}");
            let note = held.output["note"].as_str().unwrap();
            assert!(note.contains(why), "{tool_name}: {note}");
            tool("crew.deny").execute(approve(s.id, a)).await.unwrap();
            asked.await.unwrap();
        }

        // A repository not named: shipping is a suggestion.
        let g = new_session(&d, &globex, "globex-api").await;
        let g_url = hook_url(&base, g.id).await;
        take(g.id).await;
        let asked = ask(&g_url, "gh pr merge 7 --squash");
        let a = pending(g.id, "gh pr merge 7 --squash").await;
        let held = tool("crew.approve").execute(approve(g.id, a)).await.unwrap();
        assert!(held.output["suggested"].is_string(), "{held:?}");
        assert!(held.output["note"].as_str().unwrap().contains("publishes or deploys"));
        tool("crew.deny").execute(approve(g.id, a)).await.unwrap();
        asked.await.unwrap();

        // A session in acme-web it doesn't run: with answer_sessions on,
        // judged the same way.
        let other = new_session(&d, &acme, "acme-web hotfix").await;
        let other_url = hook_url(&base, other.id).await;
        let merging = ask(&other_url, "gh pr merge 43 --squash");
        let a = pending(other.id, "gh pr merge 43 --squash").await;
        let merge = Action::Approve {
            session: other.id,
            approval: a,
        };
        assert_eq!(crew.gate(&merge, &o), Gate::Run);
        let mut off = o.clone();
        off.act.answer_sessions = false;
        assert!(matches!(crew.gate(&merge, &off), Gate::Suggest(_)));
        d.decide(other.id, a, workforce_desk::model::Decision::Deny, Some(By::you()))
            .unwrap();
        merging.await.unwrap();
        let publishing = ask(&other_url, "cargo publish");
        let a = pending(other.id, "cargo publish").await;
        let publish = Action::Approve {
            session: other.id,
            approval: a,
        };
        assert!(matches!(crew.gate(&publish, &o), Gate::Suggest(_)));
        d.decide(other.id, a, workforce_desk::model::Decision::Deny, Some(By::you()))
            .unwrap();
        publishing.await.unwrap();

        // Answering in words: shipping in acme-web is its to say; a
        // registry, secrets, a force push, or another repository aren't.
        let answer = |session: uuid::Uuid, question: &str, message: &str| {
            let ask = uuid::Uuid::new_v4();
            crew.questions.lock().insert(ask, question.to_string());
            Action::Nudge {
                to: Target::Session(session),
                message: message.into(),
                reply_to: Some(ask),
            }
        };
        let ship = "Tests pass on feat/x. Shall I merge PR 42, tag v1.2.0 and promote it to staging?";
        assert_eq!(crew.gate(&answer(s.id, ship, "Yes: merge, tag and promote."), &o), Gate::Run);
        assert_eq!(crew.gate(&answer(other.id, ship, "Yes, merge it."), &o), Gate::Run);
        for (session, question, message) in [
            (s.id, "Shall I publish acme-core 1.2.0 to crates.io?", "Yes."),
            (s.id, "Can I source ~/.aws-creds to reach the bucket?", "Yes."),
            (s.id, "The branch diverged; may I force push to main?", "Yes."),
            (s.id, ship, "Yes, and delete the old staging data."),
            (g.id, ship, "Yes, merge it."),
        ] {
            assert!(
                matches!(crew.gate(&answer(session, question, message), &o), Gate::Suggest(_)),
                "{question} / {message}"
            );
        }
        // At Act the same answer to ship is the person's.
        let mut act = o.clone();
        act.level = Level::Act;
        assert!(matches!(
            crew.gate(&answer(s.id, ship, "Yes: merge, tag and promote."), &act),
            Gate::Suggest(_)
        ));
    })
    .await;
}

/// Act fully short of named repositories or a daily budget, or with its
/// budget spent or its spend unread, acts as Act and says why; Desk is told
/// it ships nowhere.
#[tokio::test(flavor = "multi_thread")]
async fn act_fully_without_repositories_or_a_budget_falls_back_to_act_and_says_why() {
    let parse = |body: &str| {
        let doc: toml::Table = toml::from_str(body).unwrap();
        Oversight::of(doc.get("oversight")).unwrap()
    };
    let no_repos = parse("[oversight]\nlevel = \"full\"\nfull = { budget_usd_per_day = 25 }\n");
    assert_eq!(
        super::super::fully_given(&no_repos, Some(0.0)).unwrap_err(),
        FULL_NO_REPOS
    );
    let no_budget =
        parse("[oversight]\nlevel = \"full\"\nfull = { repos = [\"acme/acme-web\"] }\n");
    assert_eq!(
        super::super::fully_given(&no_budget, Some(0.0)).unwrap_err(),
        FULL_NO_BUDGET
    );
    let full = parse(FULL);
    assert!(super::super::fully_given(&full, Some(24.99)).is_ok());
    assert!(super::super::fully_given(&full, Some(25.0))
        .unwrap_err()
        .contains("daily budget of $25.00 is spent"));
    assert!(super::super::fully_given(&full, None)
        .unwrap_err()
        .contains("today's spend is read"));

    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let acme = base.join("acme-web");
        repo(&acme, "git@github.com:acme/acme-web.git");
        let (overseer, crew, d, _llm) = overseeing(
            &base,
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"full\"\nfull = { repos = [\"acme/acme-web\"] }\n",
            &[],
        )
        .await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        crew.read_spent_today(&crew.overseers()).await;
        let id = overseer.agent_id.as_str().to_string();
        let (_, o) = crew.overseer_service().unwrap();
        assert_eq!(
            crew.level_now(&id, &o),
            (Level::Act, Some(FULL_NO_BUDGET.to_string()))
        );
        assert!(
            d.bosses().iter().all(|b| b.level == "act"),
            "Desk is told it acts within Desk"
        );
        let s = new_session(&d, &acme, "acme-web").await;
        assert!(!d.acts_fully_in(s.id, &id));
        let url = hook_url(&base, s.id).await;
        let body = json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash",
            "tool_input": {"command": "gh pr merge 42 --squash"}});
        let asked = tokio::spawn(async move { hook(&url, body).await });
        let a = eventually("the merge", async || d.view(s.id).unwrap().pending).await.id;
        let mut answering = o.clone();
        answering.act.answer_sessions = true;
        assert!(matches!(
            crew.gate(&Action::Approve { session: s.id, approval: a }, &answering),
            Gate::Suggest(_)
        ));
        d.decide(s.id, a, workforce_desk::model::Decision::Deny, Some(By::you()))
            .unwrap();
        asked.await.unwrap();
    })
    .await;
}
