//! Renaming a boss changes only what it is called: its plans, charges,
//! group, grant and memories stay its id's.

use super::*;
use workforce_common::locking::LockManager;
use workforce_common::oversight::{Level, Scope};
use workforce_desk::charge::How;
use workforce_desk::crew::{Event, Party};
use workforce_memory::types::{Memory, MemoryFilter, MemoryLayer};

use crate::agent_rename::{RenameError, Renamer};

/// The hub's base config, as Desk's hub writes it on every start.
fn base_config(dir: &Path, agents: &[(&str, &str)]) {
    let data = dir.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let entries: String = agents
        .iter()
        .map(|(id, name)| format!("\n[[agents]]\nid = \"{id}\"\nname = \"{name}\"\n"))
        .collect();
    std::fs::write(
        dir.join("workforce.toml"),
        format!(
            "[hub]\ndata_dir = {data:?}\n\n[memory]\ndb_path = {db:?}\n\n[llm]\napi_key = \"test-key\"\n{entries}",
            data = data.display().to_string(),
            db = data.join("memory.db").display().to_string(),
        ),
    )
    .unwrap();
}

/// Renames agent `id` to `name` the way Desk's settings do.
async fn rename(crew: &CrewHub, dir: &Path, id: &str, name: &str) -> Result<String, RenameError> {
    Renamer {
        registry: &*crew.state.registry,
        config_path: &dir.join("workforce.toml"),
        data_dir: &dir.join("data"),
        lock_manager: &LockManager::new(),
        def_store: None,
        actor: None,
        via: "web",
    }
    .rename(id, name, None)
    .await
}

fn tool(crew: &Arc<CrewHub>, boss: &str, name: &str) -> super::super::tools::CrewTool {
    super::super::tools::CrewTool::for_level(crew, boss, &Default::default(), false)
        .into_iter()
        .find(|t| workforce_agent::tools::Tool::name(t) == name)
        .unwrap()
}

async fn remembered(service: &AgentService, canary: &str) -> bool {
    service
        .memory_store
        .as_ref()
        .unwrap()
        .list(MemoryFilter {
            system_unscoped: true,
            limit: Some(20),
            ..MemoryFilter::default()
        })
        .await
        .unwrap()
        .iter()
        .any(|m| m.content.contains(canary))
}

fn says_name(service: &AgentService, name: &str) -> bool {
    std::fs::read_to_string(service.workspace.join("IDENTITY.md"))
        .unwrap()
        .contains(&format!("**Name:** {name}"))
}

const ACTS: &str = "[oversight]\nenabled = true\nlevel = \"act\"\n\
                    wake = { waiting_minutes = 0, quiet_minutes = 0 }\n\
                    [oversight.act]\nnudge = true\nanswer_sessions = true\n";

const FULL: &str = "[oversight]\nenabled = true\nlevel = \"full\"\n\
                    [oversight.full]\nbudget_usd_per_day = 25\n";

#[tokio::test(flavor = "multi_thread")]
async fn a_renamed_boss_answers_directs_checks_in_and_acts_fully_as_kat() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (scout, _l) = agent(dir.path(), "scout", "ok").await;
        let (overseer, crew, d, llm) =
            overseeing(dir.path(), pool, "5433", ACTS, std::slice::from_ref(&scout)).await;
        base_config(dir.path(), &[("desk", "Desk"), ("scout", "Scout")]);
        overseer
            .memory_store
            .as_ref()
            .unwrap()
            .remember(Memory::new(
                "katcanary: the staging port is 5433",
                MemoryLayer::Semantic,
                overseer.agent_id.clone(),
            ))
            .await
            .unwrap();

        assert_eq!(rename(&crew, dir.path(), "desk", " Kat ").await.unwrap(), "Kat");
        assert_eq!(super::super::name_of(&overseer), "Kat");
        assert!(says_name(&overseer, "Kat"));
        crew.overseers();
        assert_eq!(
            d.bosses().iter().map(|b| (b.agent.as_str(), b.name.as_str())).collect::<Vec<_>>(),
            [("desk", "Kat")]
        );
        assert!(remembered(&overseer, "katcanary").await, "its memories are its id's");

        // Its next turn is told who it is.
        overseer
            .process_message(
                "web", PERSON, "Who are you?", None, None, None, None, None, None, None, None,
                None, None,
            )
            .await
            .unwrap();
        let asked: Vec<String> = llm
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).into_owned())
            .collect();
        assert!(asked.last().unwrap().contains("**Name:** Kat"));

        // A session reaches it as "desk", and its answer is Kat's.
        let s = new_session(&d, dir.path(), "acme-web").await;
        let answered = d
            .line_call(
                s.id,
                "ask",
                &json!({"to": "desk", "text": "Which port does the test database use?", "wait_seconds": 30}),
                None,
            )
            .await
            .unwrap();
        assert_eq!((answered["answer"].as_str(), answered["from"].as_str()), (Some("5433"), Some("Kat")));

        // It directs the session, signed Kat.
        let told = workforce_agent::tools::Tool::execute(
            &tool(&crew, "desk", "crew.nudge"),
            json!({"session_id": s.id.to_string(), "message": "Run the suite against 5433.", "reason": "It asked."}),
        )
        .await
        .unwrap();
        assert!(told.success, "{told:?}");
        let thread = d.line_thread(s.id, None, None).await.unwrap();
        let nudge = thread.iter().find(|m| m.text == "Run the suite against 5433.").unwrap();
        assert_eq!((nudge.from.id.as_str(), nudge.from.name.as_str()), ("desk", "Kat"));

        // It checks in as itself.
        let finished = session_party("acme-api");
        d.note(Event::new(Kind::Finished, finished.clone(), Some(finished), "finished · all green"));
        eventually("a check-in", async || {
            d.store().latest_check_in("desk").await.unwrap()
        })
        .await;

        // Act fully applies under its new name.
        std::thread::sleep(Duration::from_millis(1100));
        write_config(&overseer, FULL);
        crew.read_spent_today(&crew.overseers()).await;
        let (service, o) = crew.overseer_service().unwrap();
        assert_eq!(service.agent_id.as_str(), "desk");
        assert_eq!(crew.level_now("desk", &o), (Level::Full, None));

        // Another agent can't take its name, in any case; it can take its own back.
        assert_eq!(rename(&crew, dir.path(), "scout", "kat").await, Err(RenameError::Taken));
        assert_eq!(rename(&crew, dir.path(), "desk", "Desk").await.unwrap(), "Desk");
        assert_eq!(super::super::name_of(&overseer), "Desk");
        assert!(says_name(&overseer, "Desk"));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_renamed_boss_runs_its_plan_as_before() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, state, d, _llm) =
            super::plans::hub_with(dir.path(), pool, super::plans::ACTS).await;
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        base_config(dir.path(), &[("desk", "Desk")]);
        rename(&crew, dir.path(), "desk", "Kat").await.unwrap();
        eventually("the plan tools", async || {
            overseer.tools.has("crew.plan").then_some(())
        })
        .await;
        let tickets: Vec<Value> = (1..=2)
            .map(|i| json!({ "title": format!("Fix flaky test {i}"), "folder": dir.path().to_string_lossy() }))
            .collect();
        let r = overseer
            .tools
            .get("crew.plan")
            .unwrap()
            .execute(json!({ "goal": "Make the suite green", "tickets": tickets }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        let id: uuid::Uuid = r.output["proposed"]["plan_id"].as_str().unwrap().parse().unwrap();
        d.approve_plan(id, 1, &By::you()).await.unwrap();
        let started = eventually("its sessions", async || {
            let s = super::plans::started(&d);
            (s.len() == 2).then_some(s)
        })
        .await;
        for v in &started {
            let c = d.charge_of(v.id).unwrap();
            assert_eq!((c.boss.as_str(), c.how), ("desk", How::Started));
        }
        let plan = d.plan(id).await.unwrap().summary;
        assert_eq!((plan.plan.boss.as_str(), plan.boss_name.as_str()), ("desk", "Kat"));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_renamed_group_boss_keeps_its_group_charges_and_grant_across_its_rebuild() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (main, _l1) = agent(dir.path(), "desk", "ok").await;
        let (sarah, _l2) = agent(dir.path(), "sarah", "ok").await;
        let d = desk(pool, dir.path()).await;
        d.change_layout(|l| l.create("acme", "Acme", &[], None))
            .await
            .unwrap();
        write_config(&main, "[oversight]\nenabled = true\nlevel = \"act\"\n");
        write_config(
            &sarah,
            &FULL.replace("enabled = true\n", "enabled = true\ngroup = \"acme\"\n"),
        );
        let state = hub(&[Arc::clone(&main), Arc::clone(&sarah)]);
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        base_config(dir.path(), &[("desk", "Desk"), ("sarah", "Sarah")]);
        eventually("two bosses", async || (d.bosses().len() == 2).then_some(())).await;
        sarah
            .memory_store
            .as_ref()
            .unwrap()
            .remember(Memory::new(
                "rowancanary in Acme",
                MemoryLayer::Semantic,
                sarah.agent_id.clone(),
            ))
            .await
            .unwrap();
        let s = new_session(&d, dir.path(), "acme-api").await;
        d.change_layout(|l| l.put(&workforce_desk::session_item(s.id), "acme", None))
            .await
            .unwrap();
        let acme = Scope::Group("acme".into());
        d.take_on(
            s.id,
            &Party::manager("sarah", "Sarah"),
            How::Handed,
            &acme,
            &By::you(),
        )
        .await
        .unwrap();

        rename(&crew, dir.path(), "sarah", "Rowan").await.unwrap();
        let (rebuilt, _l3) = agent_configured(dir.path(), "sarah", "ok", &[], |c| {
            c.agents[0].name = Some("Rowan".into());
        })
        .await;
        rebuilt.set_agent_registry(Arc::downgrade(&crew.state.registry));
        crew.state.registry.insert("sarah", Arc::clone(&rebuilt));

        let (boss, o) = crew.overseer_for(&acme).unwrap();
        assert!(Arc::ptr_eq(&boss, &rebuilt));
        assert_eq!(super::super::name_of(&boss), "Rowan");
        assert_eq!(d.scope_of_boss("sarah"), Some(acme));
        assert_eq!(
            d.bosses()
                .iter()
                .find(|b| b.agent == "sarah")
                .map(|b| b.name.clone()),
            Some("Rowan".into())
        );
        assert_eq!(d.charge_of(s.id).unwrap().boss, "sarah");
        assert_eq!(d.charges_busy("sarah"), 1);
        crew.read_spent_today(&crew.overseers()).await;
        assert_eq!(crew.level_now("sarah", &o), (Level::Full, None));
        assert!(remembered(&rebuilt, "rowancanary").await);
    })
    .await;
}
