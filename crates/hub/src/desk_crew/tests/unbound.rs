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
    hand_off = true, set_mode = true }\n\
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

/// The hook URL of `id`'s CLI started with `--permission-mode <mode>`.
async fn started_in(dir: &Path, id: uuid::Uuid, mode: &str) -> String {
    let wanted = format!("--permission-mode\n{mode}\n");
    let marker = format!("/hook/{id}/");
    for _ in 0..250 {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            if !entry.file_name().to_string_lossy().starts_with("args-") {
                continue;
            }
            let args = std::fs::read_to_string(entry.path()).unwrap_or_default();
            if let (true, Some(at)) = (args.contains(&wanted), args.find(&marker)) {
                let start = args[..at].rfind("http://").unwrap();
                let token: String = args[at + marker.len()..]
                    .chars()
                    .take_while(char::is_ascii_hexdigit)
                    .collect();
                return args[start..at + marker.len() + token.len()].to_string();
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{id} never started in {mode} mode");
}

/// Unbound, the overseer puts a session in auto mode and back: Desk
/// starts it again in place in each, reads back the mode it says it runs
/// in, and says each change in Activity.
#[tokio::test(flavor = "multi_thread")]
async fn unbound_puts_a_session_in_auto_and_back_and_says_so() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, _crew, d, _llm) = overseeing(&base, pool, "ok", UNBOUND, &[]).await;
        eventually("the overseer's tools", async || {
            overseer.tools.has("crew.set_mode").then_some(())
        })
        .await;
        let s = new_session(&d, &base, "acme-web").await;
        hook_url(&base, s.id).await;
        let boss = Party::manager("desk", &super::super::name_of(&overseer));
        d.take_on(s.id, &boss, How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        // Desk's grant says it is unbound once the crew has synced.
        eventually("Desk to know it acts anywhere", async || {
            d.acts_fully_in(s.id, "desk").then_some(())
        })
        .await;
        let tool = overseer.tools.get("crew.set_mode").unwrap();
        for (mode, title) in [("auto", "auto"), ("default", "manual")] {
            // Claude Code says the mode it started in on its first hook.
            let said = {
                let base = base.clone();
                let mode = mode.to_string();
                tokio::spawn(async move {
                    let url = started_in(&base, s.id, &mode).await;
                    hook(
                        &url,
                        json!({"hook_event_name": "SessionStart", "session_id": "cli-1", "source": "resume", "permission_mode": mode}),
                    )
                    .await
                })
            };
            let done = tool
                .execute(json!({"session_id": s.id.to_string(), "mode": mode, "reason": "Routine test run."}))
                .await
                .unwrap();
            said.await.unwrap();
            let text = done.output["done"].as_str().unwrap_or_default().to_string();
            assert!(
                text.contains(&format!("in {title} mode, its conversation kept; it says it runs in {title} mode")),
                "{done:?}"
            );
            assert_eq!(
                d.mode_seen(s.id),
                workforce_desk::mode::Mode::parse(mode)
            );
            // Its card shows a mode that doesn't ask.
            assert_eq!(
                d.view(s.id).unwrap().mode,
                workforce_desk::mode::Mode::parse(mode).filter(|m| !m.asks())
            );
            eventually("the change in Activity", async || {
                d.activity(Filter::All, None, 50)
                    .await
                    .unwrap()
                    .events
                    .iter()
                    .any(|e| {
                        e.text.starts_with(&format!("put acme-web in {title} mode"))
                            && e.reason.as_deref().is_some_and(|r| r.starts_with("Routine test run."))
                    })
                    .then_some(())
            })
            .await;
        }
    })
    .await;
}

/// Below unbound a mode is the person's to change, but at Act with
/// `set_mode` on, to one that still asks: never auto or bypass.
#[tokio::test(flavor = "multi_thread")]
async fn below_unbound_only_the_act_switch_changes_a_mode_and_only_to_one_that_asks() {
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
        let s = new_session(&d, dir.path(), "acme-web").await;
        let (_, mut o) = crew.overseer_service().unwrap();
        let set = |mode| Action::SetMode {
            session: s.id,
            mode,
        };
        use workforce_desk::mode::Mode;
        // A mode that asks is suggested; one that doesn't isn't even that,
        // as Desk would refuse it from the person's yes.
        for mode in Mode::ALL {
            let gate = crew.gate(&set(mode), &o);
            match mode.asks() {
                true => assert!(matches!(gate, Gate::Suggest(_)), "{mode:?}"),
                false => assert_eq!(gate, Gate::Refuse, "{mode:?}"),
            }
        }
        o.act.set_mode = true;
        for mode in [Mode::Default, Mode::AcceptEdits, Mode::Plan] {
            assert_eq!(crew.gate(&set(mode), &o), Gate::Run, "{mode:?}");
        }
        for mode in [Mode::Auto, Mode::BypassPermissions] {
            assert_eq!(crew.gate(&set(mode), &o), Gate::Refuse, "{mode:?}");
        }
        o.level = workforce_common::oversight::Level::Suggest;
        assert!(matches!(crew.gate(&set(Mode::Plan), &o), Gate::Suggest(_)));
        assert_eq!(crew.gate(&set(Mode::Auto), &o), Gate::Refuse);
    })
    .await;
}

/// A mode is set between turns: a session waiting on an answer keeps its
/// request. Unbound, Claude Code goes as far as bypass, on a session it
/// runs; on one it doesn't run, a mode that doesn't ask is refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_mode_waits_for_the_turn_and_bypass_is_only_on_a_session_it_runs() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, _crew, d, _llm) = overseeing(&base, pool, "ok", UNBOUND, &[]).await;
        eventually("the overseer's tools", async || {
            overseer.tools.has("crew.set_mode").then_some(())
        })
        .await;
        let s = new_session(&d, &base, "acme-web").await;
        let url = hook_url(&base, s.id).await;
        let tool = overseer.tools.get("crew.set_mode").unwrap();
        let set = |mode: &str| json!({"session_id": s.id.to_string(), "mode": mode, "reason": "Routine."});
        let not_run = tool.execute(set("bypassPermissions")).await.unwrap();
        assert!(not_run.output["done"].is_null(), "{not_run:?}");
        let boss = Party::manager("desk", &super::super::name_of(&overseer));
        d.take_on(s.id, &boss, How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        eventually("Desk to know it acts anywhere", async || {
            d.acts_fully_in(s.id, "desk").then_some(())
        })
        .await;
        let asked = ask(&url, "npm test");
        let a = pending(&d, s.id, "npm test").await;
        let held = tool.execute(set("auto")).await.unwrap();
        assert!(held.output["done"].is_null(), "{held:?}");
        assert_eq!(d.view(s.id).unwrap().pending.map(|p| p.id), Some(a), "its request stays");
        overseer
            .tools
            .get("crew.deny")
            .unwrap()
            .execute(approve(s.id, a))
            .await
            .unwrap();
        asked.await.unwrap();
        // Its turn ends.
        hook(&url, json!({"hook_event_name": "Stop"})).await;
        eventually("its turn to end", async || {
            let v = d.view(s.id).unwrap();
            (v.pending.is_none() && v.state != workforce_desk::model::SessionState::Working)
                .then_some(())
        })
        .await;
        let done = tool.execute(set("bypassPermissions")).await.unwrap();
        assert!(done.output["done"].is_string(), "{done:?}");
        started_with(&base, s.id, "--dangerously-skip-permissions\n").await;
        assert_eq!(
            d.view(s.id).unwrap().mode,
            Some(workforce_desk::mode::Mode::BypassPermissions)
        );
    })
    .await;
}

/// The args files of `id`'s CLI starts.
fn starts_of(dir: &Path, id: uuid::Uuid) -> Vec<String> {
    let marker = format!("/hook/{id}/");
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("args-"))
        .map(|e| std::fs::read_to_string(e.path()).unwrap_or_default())
        .filter(|a| a.contains(&marker))
        .collect()
}

/// Waits for a start of `id`'s CLI whose arguments hold `wanted`.
async fn started_with(dir: &Path, id: uuid::Uuid, wanted: &str) {
    eventually(wanted, async || {
        starts_of(dir, id)
            .iter()
            .any(|a| a.contains(wanted))
            .then_some(())
    })
    .await;
}

/// Waits for a start of `id`'s CLI other than those `before`, and returns
/// its arguments.
async fn next_start(dir: &Path, id: uuid::Uuid, before: &[String]) -> String {
    eventually("another start", async || {
        starts_of(dir, id).into_iter().find(|a| !before.contains(a))
    })
    .await
}

async fn stopped(d: &Desk, id: uuid::Uuid) {
    d.stop(id, None).unwrap();
    eventually("it to stop", async || {
        (!d.view(id).unwrap().live).then_some(())
    })
    .await;
}

/// Auto mode lasts only while the boss that set it is unbound and runs the
/// session: the person resuming it, or the boss no longer unbound, starts
/// it in its own mode again, and Desk forgets the mode.
#[tokio::test(flavor = "multi_thread")]
async fn a_kept_auto_mode_ends_with_unbound_and_with_the_persons_resume() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, crew, d, _llm) = overseeing(&base, pool, "ok", UNBOUND, &[]).await;
        eventually("the overseer's tools", async || {
            overseer.tools.has("crew.set_mode").then_some(())
        })
        .await;
        let boss = Party::manager("desk", &super::super::name_of(&overseer));
        let tool = overseer.tools.get("crew.set_mode").unwrap();
        let mut sessions = Vec::new();
        for name in ["acme-web", "acme-api"] {
            let s = new_session(&d, &base, name).await;
            hook_url(&base, s.id).await;
            d.take_on(s.id, &boss, How::Handed, &Default::default(), &By::you())
                .await
                .unwrap();
            eventually("Desk to know it acts anywhere", async || {
                d.acts_fully_in(s.id, "desk").then_some(())
            })
            .await;
            let done = tool
                .execute(
                    json!({"session_id": s.id.to_string(), "mode": "auto", "reason": "Routine."}),
                )
                .await
                .unwrap();
            assert!(done.output["done"].is_string(), "{done:?}");
            started_with(&base, s.id, "--permission-mode\nauto\n").await;
            sessions.push(s.id);
        }
        let (web, api) = (sessions[0], sessions[1]);

        // The person resumes one themselves: it starts in its own mode.
        stopped(&d, web).await;
        let before = starts_of(&base, web);
        d.resume(web, None, Some(By::you())).await.unwrap();
        let args = next_start(&base, web, &before).await;
        assert!(!args.contains("--permission-mode"), "{args}");
        assert_eq!(d.view(web).unwrap().mode, None);

        // A box unticked: it is no longer unbound, and the other starts in
        // its own mode at its next start, whoever starts it.
        write_config(
            &overseer,
            &UNBOUND.replace("interrupt = true", "interrupt = false"),
        );
        eventually("no longer unbound", async || {
            let (_, o) = crew.overseer_service()?;
            (!crew.unbound(&o)).then_some(())
        })
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        stopped(&d, api).await;
        let before = starts_of(&base, api);
        d.resume(api, None, None).await.unwrap();
        let args = next_start(&base, api, &before).await;
        assert!(!args.contains("--permission-mode"), "{args}");
        assert_eq!(d.view(api).unwrap().mode, None);

        // Unbound ends too with Anywhere off, or its budget spent.
        let (_, mut o) = crew.overseer_service().unwrap();
        o.act.interrupt = true;
        o.full.anywhere = false;
        assert!(!crew.unbound(&o));
        o.full.anywhere = true;
        o.full.budget_cents_per_day = Some(500);
        assert!(super::super::fully_given(&o, Some(5.0)).is_err(), "spent");
        assert!(super::super::fully_given(&o, Some(1.0)).is_ok());
    })
    .await;
}

/// Act fully anywhere, but not unbound: it ships in a repository nobody
/// named and runs a workflow nobody listed, and what no overseer does
/// stays the person's.
#[tokio::test(flavor = "multi_thread")]
async fn anywhere_ships_in_any_repository_and_the_hard_limits_still_hold() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let globex = base.join("globex-api");
        repo(&globex, "git@github.com:globex/globex-api.git");
        let (overseer, crew, d, _llm) = overseeing(
            &base,
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"full\"\n[oversight.full]\nanywhere = true\n",
            &[],
        )
        .await;
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
        let s = new_session(&d, &globex, "globex-api").await;
        let url = hook_url(&base, s.id).await;
        d.take_on(s.id, &boss, How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        eventually("Desk to know it acts anywhere", async || {
            d.acts_fully_in(s.id, "desk").then_some(())
        })
        .await;
        let tool = |name: &str| overseer.tools.get(name).unwrap();
        for command in ["gh workflow run cleanup.yml", "gh pr merge 42 --squash"] {
            let asked = ask(&url, command);
            let a = pending(&d, s.id, command).await;
            let done = tool("crew.approve")
                .execute(approve(s.id, a))
                .await
                .unwrap();
            assert!(done.output["done"].is_string(), "{command}: {done:?}");
            assert_eq!(decided(asked.await.unwrap()), "allow", "{command}");
        }
        let asked = ask(&url, "cargo publish");
        let a = pending(&d, s.id, "cargo publish").await;
        let held = tool("crew.approve")
            .execute(approve(s.id, a))
            .await
            .unwrap();
        assert!(held.output["suggested"].is_string(), "{held:?}");
        tool("crew.deny").execute(approve(s.id, a)).await.unwrap();
        assert_eq!(decided(asked.await.unwrap()), "deny");
    })
    .await;
}

/// What ends an unbound boss's hold over a running session in auto mode.
#[derive(Debug, Clone, Copy)]
enum Ends {
    BoxUnticked,
    AnywhereOff,
    BudgetSpent,
    Released,
    AnotherBoss,
}

/// A session running in auto mode, which an unbound boss set, mid-turn:
/// when the boss stops being unbound over it, every boss action on it but
/// stopping it waits, and once its turn ends Desk starts it again in place
/// in its own mode.
#[tokio::test(flavor = "multi_thread")]
async fn a_running_session_goes_back_to_its_own_mode_when_unbound_ends() {
    use workforce_desk::ledger::{Billing, Priced, Spend, Tokens};
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let budgeted = UNBOUND.replace(
            "anywhere = true\n",
            "anywhere = true\nbudget_usd_per_day = 1\n",
        );
        let (overseer, crew, d, _llm) = overseeing(&base, pool, "ok", UNBOUND, &[]).await;
        eventually("the overseer's tools", async || {
            overseer.tools.has("crew.set_mode").then_some(())
        })
        .await;
        let boss = Party::manager("desk", &super::super::name_of(&overseer));
        let tool = overseer.tools.get("crew.set_mode").unwrap();
        for (i, ends) in [
            Ends::BoxUnticked,
            Ends::AnywhereOff,
            Ends::BudgetSpent,
            Ends::Released,
            Ends::AnotherBoss,
        ]
        .into_iter()
        .enumerate()
        {
            let config = match ends {
                Ends::BudgetSpent => budgeted.clone(),
                _ => UNBOUND.to_string(),
            };
            write_config(&overseer, &config);
            super::super::config_saved(&overseer);
            eventually("unbound", async || {
                let (_, o) = crew.overseer_service()?;
                crew.unbound(&o).then_some(())
            })
            .await;
            let s = new_session(&d, &base, &format!("acme-{i}")).await;
            hook_url(&base, s.id).await;
            d.take_on(s.id, &boss, How::Handed, &Default::default(), &By::you())
                .await
                .unwrap();
            eventually("Desk to know it is unbound", async || {
                d.acts_fully_in(s.id, "desk").then_some(())
            })
            .await;
            let done = tool
                .execute(
                    json!({"session_id": s.id.to_string(), "mode": "auto", "reason": "Routine."}),
                )
                .await
                .unwrap();
            assert!(done.output["done"].is_string(), "{ends:?}: {done:?}");
            let url = started_in(&base, s.id, "auto").await;
            assert_eq!(
                d.view(s.id).unwrap().mode,
                Some(workforce_desk::mode::Mode::Auto)
            );
            // It works on a turn.
            hook(
                &url,
                json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_use_id": "t1",
                    "tool_input": {"command": "npm test"}}),
            )
            .await;
            eventually("its turn", async || {
                (d.view(s.id).unwrap().state == workforce_desk::model::SessionState::Working)
                    .then_some(())
            })
            .await;
            let before = starts_of(&base, s.id);
            match ends {
                Ends::BoxUnticked => {
                    write_config(
                        &overseer,
                        &UNBOUND.replace("interrupt = true", "interrupt = false"),
                    );
                    super::super::config_saved(&overseer);
                }
                Ends::AnywhereOff => {
                    write_config(
                        &overseer,
                        &UNBOUND.replace("anywhere = true", "anywhere = false"),
                    );
                    super::super::config_saved(&overseer);
                }
                Ends::BudgetSpent => {
                    d.store()
                        .ledger_write(
                            s.id,
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
                                    usd: Some(5.0),
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
                Ends::Released => {
                    d.release(s.id, &By::you()).await.unwrap();
                }
                Ends::AnotherBoss => {
                    d.take_on(
                        s.id,
                        &Party::manager("mia", "Mia"),
                        How::Handed,
                        &Default::default(),
                        &By::you(),
                    )
                    .await
                    .unwrap();
                }
            }
            // Until it is back in its own mode, the boss may only stop it.
            let deploy = Action::Nudge {
                to: Target::Session(s.id),
                message: "Run the deploy".into(),
                reply_to: None,
            };
            let why = eventually("the boss held", async || {
                let (_, o) = crew.overseer_service()?;
                match crew.gate(&deploy, &o) {
                    Gate::Suggest(why) if why.contains("still runs in auto mode") => Some(why),
                    _ => None,
                }
            })
            .await;
            assert!(
                why.contains("put back in its own mode") || why.contains("puts it back"),
                "{why}"
            );
            let (_, o) = crew.overseer_service().unwrap();
            assert_eq!(
                crew.gate(&Action::Interrupt { session: s.id }, &o),
                Gate::Run
            );
            assert_eq!(
                d.view(s.id).unwrap().mode,
                Some(workforce_desk::mode::Mode::Auto),
                "{ends:?}: mid-turn"
            );
            // Its turn ends: it starts again in its own mode.
            hook(&url, json!({"hook_event_name": "Stop"})).await;
            let args = next_start(&base, s.id, &before).await;
            assert!(!args.contains("--permission-mode"), "{ends:?}: {args}");
            eventually("its card", async || {
                d.view(s.id).unwrap().mode.is_none().then_some(())
            })
            .await;
            eventually("said in Activity", async || {
                d.activity(Filter::All, None, 80)
                    .await
                    .unwrap()
                    .events
                    .iter()
                    .any(|e| {
                        e.text
                            .starts_with(&format!("put acme-{i} back in its own mode"))
                    })
                    .then_some(())
            })
            .await;
        }
    })
    .await;
}
