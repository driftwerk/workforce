//! Unbound (`level = "full"`, `[oversight.full] anywhere`, every
//! `[oversight.act]` switch on): the overseer allows whatever its sessions
//! ask, publishing, force pushes and secrets included, but never what reads
//! or changes an agent's own settings; it sends whatever it says; every
//! crew action runs; and it sweeps its scope. With any one switch off, Act
//! fully is as it was.

use super::super::actions::{Action, Gate, Target};
use super::act_fully::repo;
use super::*;
use workforce_desk::charge::How;
use workforce_desk::crew::{Event, Party};

pub(super) const UNBOUND: &str = "[oversight]\nenabled = true\nlevel = \"full\"\n\
    act = { nudge = true, resume_restart = true, read_only_approvals = true, hand_out_work = true, \
    interrupt = true, message_agents = true, message_terminal_sessions = true, answer_sessions = true, \
    hand_off = true }\n\
    [oversight.full]\nanywhere = true\n";

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

fn decided(v: Value) -> Value {
    v["hookSpecificOutput"]["decision"]["behavior"].clone()
}

fn approve(s: uuid::Uuid, a: uuid::Uuid) -> Value {
    json!({"session_id": s.to_string(), "approval_id": a.to_string(), "reason": "Keep it moving."})
}

#[tokio::test(flavor = "multi_thread")]
async fn unbound_allows_what_it_is_asked_but_its_own_settings_and_says_so() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let acme = base.join("acme-web");
        repo(&acme, "git@github.com:acme/acme-web.git");
        let (overseer, crew, d, _llm) = overseeing(&base, pool, "ok", UNBOUND, &[]).await;
        eventually("the overseer's tools", async || {
            overseer.tools.has("crew.approve").then_some(())
        })
        .await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        let (_, o) = crew.overseer_service().unwrap();
        assert!(crew.unbound(&o), "no budget, no repositories: unbound all the same");
        let tool = |name: &str| overseer.tools.get(name).unwrap();
        let boss = Party::manager("desk", &super::super::name_of(&overseer));

        let s = new_session(&d, &acme, "acme-web").await;
        let url = hook_url(&base, s.id).await;
        d.take_on(s.id, &boss, How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        eventually("Desk to know it acts anywhere", async || {
            d.acts_fully_in(s.id, "desk").then_some(())
        })
        .await;
        for command in [
            "cargo publish -p acme-core",
            "git push --force origin main",
            "source ~/.aws-creds && aws s3 ls",
            "gh workflow run cleanup.yml",
            "npm run deploy",
        ] {
            let asked = ask(&url, command);
            let a = pending(&d, s.id, command).await;
            let done = tool("crew.approve").execute(approve(s.id, a)).await.unwrap();
            assert!(done.output["done"].is_string(), "{command}: {done:?}");
            assert_eq!(decided(asked.await.unwrap()), "allow", "{command}");
        }
        // A session it doesn't run, too.
        let other = new_session(&d, &acme, "acme-web two").await;
        let other_url = hook_url(&base, other.id).await;
        let asked = ask(&other_url, "npm publish");
        let a = pending(&d, other.id, "npm publish").await;
        let done = tool("crew.approve").execute(approve(other.id, a)).await.unwrap();
        assert!(done.output["done"].is_string(), "{done:?}");
        assert_eq!(decided(asked.await.unwrap()), "allow");

        // Its own settings, where its budget is, stay the person's.
        let own = format!("cat {}", overseer.workspace.join("config.toml").display());
        let asked = ask(&url, &own);
        let a = pending(&d, s.id, &own).await;
        let held = tool("crew.approve").execute(approve(s.id, a)).await.unwrap();
        assert!(held.output["done"].is_null(), "{held:?}");
        tool("crew.deny").execute(approve(s.id, a)).await.unwrap();
        assert_eq!(decided(asked.await.unwrap()), "deny");

        // The overnight note goes as it is.
        let sent = tool("crew.nudge")
            .execute(json!({"session_id": s.id.to_string(), "message": OVERNIGHT_NOTE, "reason": "Overnight."}))
            .await
            .unwrap();
        assert!(sent.output["done"].is_string(), "{sent:?}");

        // Every crew action runs, handing a session to another engine too.
        for action in [
            Action::Stop { session: s.id },
            Action::Interrupt { session: s.id },
            Action::Handoff {
                session: s.id,
                engine: Engine::Codex,
                account: None,
            },
        ] {
            assert_eq!(crew.gate(&action, &o), Gate::Run, "{action:?}");
        }

        // Activity says it was done unbound.
        eventually("unbound in Activity", async || {
            d.activity(Filter::All, None, 50)
                .await
                .unwrap()
                .events
                .iter()
                .any(|e| {
                    e.reason
                        .as_deref()
                        .is_some_and(|r| r.ends_with("(unbound: no limits)"))
                })
                .then_some(())
        })
        .await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn with_one_switch_off_act_fully_is_as_it_was() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let acme = base.join("acme-web");
        repo(&acme, "git@github.com:acme/acme-web.git");
        let one_off = UNBOUND.replace("interrupt = true", "interrupt = false");
        let (overseer, crew, d, _llm) = overseeing(&base, pool, "ok", &one_off, &[]).await;
        eventually("the overseer's tools", async || {
            overseer.tools.has("crew.approve").then_some(())
        })
        .await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        let (_, o) = crew.overseer_service().unwrap();
        assert!(!crew.unbound(&o));
        let boss = Party::manager("desk", &super::super::name_of(&overseer));
        let s = new_session(&d, &acme, "acme-web").await;
        let url = hook_url(&base, s.id).await;
        d.take_on(s.id, &boss, How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        let tool = |name: &str| overseer.tools.get(name).unwrap();
        for command in ["cargo publish -p acme-core", "git push --force origin main"] {
            let asked = ask(&url, command);
            let a = pending(&d, s.id, command).await;
            let held = tool("crew.approve")
                .execute(approve(s.id, a))
                .await
                .unwrap();
            assert!(held.output["suggested"].is_string(), "{command}: {held:?}");
            tool("crew.deny").execute(approve(s.id, a)).await.unwrap();
            assert_eq!(decided(asked.await.unwrap()), "deny", "{command}");
        }
        assert!(matches!(
            crew.gate(
                &Action::Handoff {
                    session: s.id,
                    engine: Engine::Codex,
                    account: None
                },
                &o
            ),
            Gate::Suggest(_)
        ));
        // Its messages still go whatever their words, as at Act.
        let note = Action::Nudge {
            to: Target::Session(s.id),
            message: OVERNIGHT_NOTE.into(),
            reply_to: None,
        };
        assert_eq!(crew.gate(&note, &o), Gate::Run);
    })
    .await;
}

/// Unbound, what waits on an answer wakes the overseer at once, and it
/// sweeps its scope every few minutes while there is anything in it; at
/// Act, neither.
#[tokio::test(flavor = "multi_thread")]
async fn unbound_wakes_at_once_for_what_waits_and_sweeps_its_scope() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_, crew, d, _llm) = overseeing(dir.path(), pool, "ok", UNBOUND, &[]).await;
        let (_, o) = crew.overseer_service().unwrap();
        let web = new_session(&d, dir.path(), "acme-web").await;
        let party = Party::session(&web);
        let waits = Event::new(
            Kind::NeedsYou,
            party.clone(),
            Some(party.clone()),
            "needs you",
        );
        assert!(crew.trigger_for(&waits, "desk", &o).is_some());
        let swept = eventually("a sweep", async || {
            let c = d.store().latest_check_in("desk").await.unwrap()?;
            c.prompt.contains("Regular sweep").then_some(c)
        })
        .await;
        assert!(swept.prompt.contains("Be the boss"), "{}", swept.prompt);
        assert!(
            swept.prompt.contains("parallel limit (4)"),
            "{}",
            swept.prompt
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn at_act_what_waits_waits_its_minutes_and_nothing_sweeps() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"act\"\n",
            &[],
        )
        .await;
        let (_, o) = crew.overseer_service().unwrap();
        let web = new_session(&d, dir.path(), "acme-web").await;
        let party = Party::session(&web);
        let waits = Event::new(
            Kind::NeedsYou,
            party.clone(),
            Some(party.clone()),
            "needs you",
        );
        assert!(crew.trigger_for(&waits, "desk", &o).is_none());
        // Several sweeps' worth of minutes.
        tokio::time::sleep(Duration::from_millis(800)).await;
        let latest = d.store().latest_check_in("desk").await.unwrap();
        assert!(
            latest
                .as_ref()
                .is_none_or(|c| !c.prompt.contains("Regular sweep")),
            "{latest:?}"
        );
    })
    .await;
}
