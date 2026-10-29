//! The crew on a hub with agents on a mock model and Desk sessions on a
//! stand-in CLI that calls its hooks as Claude Code does.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use workforce_common::channels::DESK_CHANNEL;
use workforce_common::settings::{AgentConfig, AgentWorkspaceConfig};
use workforce_desk::crew::{By, Crew, Filter, ItemState, Kind, PartyKind, SuggestionState};
use workforce_desk::model::{Engine, NewSession};
use workforce_desk::{Authorize, Desk, DeskStore, Options, RestorePolicy};
use workforce_postgres::test_support::with_test_pool;

use super::{CrewHub, Timing};
use crate::agent_registry::AgentRegistry;
use crate::gateway::GatewayState;
use crate::serve::AgentService;
use crate::serve::Compose as _;

const FAST: Timing = Timing {
    tick: Duration::from_millis(50),
    coalesce: Duration::from_millis(150),
    minute: Duration::from_millis(100),
};

const PERSON: &str = "person-1";

/// An agent `id` whose model answers everything with `reply`.
async fn agent(dir: &Path, id: &str, reply: &str) -> (Arc<AgentService>, wiremock::MockServer) {
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, ResponseTemplate};
    let (config, server) = crate::serve::testing::mock_llm_config(&dir.join(id), |c| {
        c.agents = vec![AgentConfig {
            id: id.into(),
            ..Default::default()
        }];
    })
    .await;
    let chunk = |body: Value| format!("data: {body}\n\n");
    let sse = [
        chunk(json!({"id": "c1", "object": "chat.completion.chunk", "model": "mock",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": reply}, "finish_reason": null}]})),
        chunk(json!({"id": "c1", "object": "chat.completion.chunk", "model": "mock",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}})),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({"stream": true})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c1", "object": "chat.completion", "model": "mock",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": reply}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        })))
        .with_priority(2)
        .mount(&server)
        .await;
    let service = AgentService::from_config(config, None).await.unwrap();
    std::fs::create_dir_all(&service.workspace).unwrap();
    (service, server)
}

fn write_config(service: &AgentService, toml: &str) {
    AgentWorkspaceConfig::write(&service.workspace, toml).unwrap();
}

/// A gateway serving `agents`, each knowing the registry.
fn hub(agents: &[Arc<AgentService>]) -> Arc<GatewayState> {
    let fixture = crate::remote_mcp::testing::fixture(false);
    let map: HashMap<String, Arc<AgentService>> = agents
        .iter()
        .map(|s| (s.agent_id.as_str().to_string(), Arc::clone(s)))
        .collect();
    let state = Arc::new(GatewayState {
        registry: Arc::new(AgentRegistry::from_parts(
            map,
            agents[0].agent_id.as_str().to_string(),
        )),
        broadcast_tx: tokio::sync::broadcast::channel(4096).0,
        ..Arc::try_unwrap(fixture.state)
            .ok()
            .expect("the fixture's only handle")
    });
    for s in agents {
        s.set_agent_registry(Arc::downgrade(&state.registry));
    }
    state
}

/// A stand-in for Claude Code: it writes its arguments (the hook settings
/// among them) where the test reads them, and stays up.
fn fake_cli(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("fake-claude");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}/args-$$\"\nexec sleep 60\n",
            dir.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

async fn desk(pool: workforce_postgres::PostgresPool, dir: &Path) -> Desk {
    let cli = fake_cli(dir);
    Desk::start(
        DeskStore::with_pool(pool).await.unwrap(),
        Options {
            claude: cli.clone(),
            codex: cli,
            env: BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
            run_dir: dir.join("run"),
            scrollback_bytes: 4096,
            approval_timeout: Duration::from_secs(60),
            restore: RestorePolicy::Off,
            adopt_every: None,
            holder: None,
        },
        Default::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap()
}

/// The hook URL a session's CLI was started with.
async fn hook_url(dir: &Path, id: uuid::Uuid) -> String {
    for _ in 0..200 {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("args-") {
                continue;
            }
            let args = std::fs::read_to_string(entry.path()).unwrap_or_default();
            let marker = format!("/hook/{id}/");
            if let Some(at) = args.find(&marker) {
                let start = args[..at].rfind("http://").unwrap();
                let token: String = args[at + marker.len()..]
                    .chars()
                    .take_while(char::is_ascii_hexdigit)
                    .collect();
                let end = at + marker.len() + token.len();
                return args[start..end].to_string();
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the session's CLI never started");
}

async fn hook(url: &str, body: Value) -> Value {
    let response = reqwest::Client::new()
        .post(url)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let text = response.text().await.unwrap();
    serde_json::from_str(&text).unwrap_or_else(|_| panic!("{url} answered {status}: {text:?}"))
}

async fn eventually<T>(what: &str, mut check: impl AsyncFnMut() -> Option<T>) -> T {
    for _ in 0..400 {
        if let Some(found) = check().await {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{what} never happened");
}

struct Anyone;

#[async_trait::async_trait]
impl Authorize for Anyone {
    async fn authorize(&self, _: &HeaderMap, _: Option<&str>) -> Result<(), StatusCode> {
        Ok(())
    }
}

async fn new_session(d: &Desk, dir: &Path, name: &str) -> workforce_desk::model::SessionView {
    d.create(
        NewSession {
            engine: Engine::Claude,
            name: name.into(),
            cwd: dir.to_string_lossy().into_owned(),
            goal: "Upgrade the date picker".into(),
            worktree: None,
            prompt: None,
            approvals: None,
        },
        Some(By::you()),
    )
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agents_turns_are_activity_and_its_question_needs_you() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (scout, _llm) = agent(
            dir.path(),
            "scout",
            "I've built release/4.2.\n\nShould I deploy main or release/4.2 to staging?",
        )
        .await;
        let state = hub(std::slice::from_ref(&scout));
        let d = desk(pool, dir.path()).await;
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        let reply = scout
            .process_message(
                "web",
                PERSON,
                "Prepare the staging deploy. Use the usual checks.",
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        assert!(reply.contains("Should I deploy"), "{reply}");
        let events = eventually("the turn's events", async || {
            let page = d.activity(Filter::All, None, 10).await.unwrap();
            (page.events.len() >= 2).then_some(page.events)
        })
        .await;
        assert_eq!(
            (events[1].kind, events[1].text.as_str()),
            (Kind::Started, "started “Prepare the staging deploy.”")
        );
        assert_eq!(
            (events[0].kind, events[0].text.as_str()),
            (
                Kind::Question,
                "asked “Should I deploy main or release/4.2 to staging?”"
            )
        );
        assert_eq!(events[0].actor.kind, PartyKind::Agent);
        let overview = d.overview(crew.as_ref()).await.unwrap();
        let item = overview.items.iter().find(|i| i.id == "scout").unwrap();
        assert_eq!(item.state, ItemState::NeedsYou);
        assert_eq!(item.task.as_deref(), Some("Prepare the staging deploy."));
        let question = item.question.clone().unwrap();
        assert_eq!(question.options.unwrap(), ["main", "release/4.2"]);
        assert_eq!(overview.counts.needs_you, 1);
        assert_eq!(overview.brief, None, "no overseer, no brief");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn one_agent_oversees_and_has_the_tools_its_level_allows() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (first, _a) = agent(dir.path(), "desk", "ok").await;
        let (second, _b) = agent(dir.path(), "scout", "ok").await;
        write_config(
            &first,
            "# mine\n[oversight]\nenabled = true\nlevel = \"watch\"\n",
        );
        let state = hub(&[Arc::clone(&first), Arc::clone(&second)]);
        let d = desk(pool, dir.path()).await;
        let crew = CrewHub::start(state, d, FAST, CancellationToken::new());
        eventually("the overseer's tools", async || {
            first.tools.has("crew.overview").then_some(())
        })
        .await;
        assert!(!first.tools.has("crew.nudge"), "watching only reads");
        assert_eq!(crew.overseer().unwrap().level, "watch");

        let body = "[oversight]\nenabled = true\nlevel = \"suggest\"\n[oversight.wake]\nquiet_minutes = 0\n";
        crate::agent_config_write::save(
            &second,
            "scout",
            None,
            body,
            workforce_orchestrator::workflow_store::EditContext::new("web", None),
            |_| true,
        )
        .await
        .unwrap();
        let before = std::fs::read_to_string(first.workspace.join("config.toml")).unwrap();
        assert!(before.starts_with("# mine\n[oversight]\nenabled = false"), "{before}");
        let manager = crew.overseer().unwrap();
        assert_eq!((manager.id.as_str(), manager.level.as_str()), ("scout", "suggest"));
        assert_eq!(manager.quiet_minutes, 0);
        eventually("the tools to move", async || {
            (second.tools.has("crew.nudge") && !first.tools.has("crew.overview")).then_some(())
        })
        .await;
        assert!(!second.tools.has("desk.approve"));

        let refused = crate::agent_config_write::save(
            &second,
            "scout",
            None,
            "[oversight]\nlevel = \"boss\"\n",
            workforce_orchestrator::workflow_store::EditContext::new("web", None),
            |_| true,
        )
        .await
        .unwrap_err();
        assert_eq!(
            refused.to_string(),
            "the agent would not load that: How much the overseer does on its own must be watch, suggest or act."
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn actions_run_or_become_suggestions_as_the_level_says() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, _llm) = agent(dir.path(), "desk", "ok").await;
        write_config(
            &overseer,
            "[oversight]\nenabled = true\nlevel = \"act\"\nact = { read_only_approvals = true, nudge = false }\n",
        );
        let state = hub(std::slice::from_ref(&overseer));
        let d = desk(pool, dir.path()).await;
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("the overseer's tools", async || {
            overseer.tools.has("crew.approve").then_some(())
        })
        .await;
        let s = new_session(&d, dir.path(), "acme-web").await;
        let url = hook_url(dir.path(), s.id).await;
        let tool = |name: &str| overseer.tools.get(name).unwrap();

        // A read inside its folder is allowed without asking.
        let asked = tokio::spawn({
            let url = url.clone();
            async move {
                hook(&url, json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash", "tool_input": {"command": "git status"}})).await
            }
        });
        let pending = eventually("the request", async || d.view(s.id).unwrap().pending).await;
        let done = tool("crew.approve")
            .execute(json!({"session_id": s.id.to_string(), "approval_id": pending.id.to_string(), "reason": "It only reads."}))
            .await
            .unwrap();
        assert!(done.success, "{done:?}");
        assert_eq!(
            asked.await.unwrap()["hookSpecificOutput"]["decision"]["behavior"],
            "allow"
        );

        // An install is the person's call: it becomes a suggestion.
        let asked = tokio::spawn({
            let url = url.clone();
            async move {
                hook(&url, json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash", "tool_input": {"command": "npm install @acme/ds@4.2.0"}})).await
            }
        });
        let pending = eventually("the second request", async || {
            d.view(s.id).unwrap().pending.filter(|p| p.detail.starts_with("npm"))
        })
        .await;
        let suggested = tool("crew.approve")
            .execute(json!({"session_id": s.id.to_string(), "approval_id": pending.id.to_string(), "reason": "It is the planned upgrade."}))
            .await
            .unwrap();
        assert!(suggested.success);
        assert_eq!(
            suggested.output["suggested"],
            "Allow acme-web: Run npm install @acme/ds@4.2.0"
        );
        // Not allowed to nudge, and pushing is never automatic.
        let nudge = tool("crew.nudge")
            .execute(json!({"session_id": s.id.to_string(), "message": "How is it going?", "reason": "Quiet."}))
            .await
            .unwrap();
        assert!(nudge.output["suggested"].is_string(), "{nudge:?}");
        let missing_reason = tool("crew.stop")
            .execute(json!({"session_id": s.id.to_string()}))
            .await
            .unwrap();
        assert!(!missing_reason.success);

        let list = d.store().suggestions(10).await.unwrap();
        assert_eq!(list.len(), 2);
        let install = list
            .iter()
            .find(|s| s.text.contains("npm install"))
            .unwrap()
            .clone();
        assert_eq!(install.subject.as_ref().unwrap().id, s.id.to_string());

        // You accept it: it runs as you.
        crew.accept(&install).await.unwrap();
        assert_eq!(
            asked.await.unwrap()["hookSpecificOutput"]["decision"]["behavior"],
            "allow"
        );
        let events = eventually("the actions' events", async || {
            let page = d.activity(Filter::All, None, 50).await.unwrap();
            page.events
                .iter()
                .any(|e| e.kind == Kind::Approved && e.actor.kind == PartyKind::You)
                .then_some(page.events)
        })
        .await;
        let by_manager: Vec<_> = events
            .iter()
            .filter(|e| e.actor.kind == PartyKind::Manager)
            .map(|e| (e.kind, e.text.clone(), e.reason.clone()))
            .collect();
        assert!(by_manager.contains(&(
            Kind::Approved,
            "allowed acme-web to run git status".into(),
            Some("It only reads.".into())
        )));
        assert!(by_manager
            .iter()
            .any(|(k, t, _)| *k == Kind::Suggested && t.contains("acme-web")));
        let yours = events
            .iter()
            .find(|e| e.kind == Kind::Approved && e.actor.kind == PartyKind::You)
            .unwrap();
        assert_eq!(yours.suggestion_id.as_deref(), Some(install.id.as_str()));
        assert_eq!(yours.approval_id.as_deref(), Some(pending.id.to_string().as_str()));
        assert_eq!(
            d.store().suggestion(&install.id).await.unwrap().unwrap().state,
            SuggestionState::Open,
            "the route marks it done; accepting only does it"
        );
    })
    .await;
}

/// A Desk session goes blocked; Activity and the overview show it; past the
/// overseer's threshold it gets a check-in whose answer offers the request
/// to allow; allowing it through Desk's route is recorded as yours.
#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_session_wakes_the_overseer_and_its_answer_offers_the_approval() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, _llm) = agent(
            dir.path(),
            "desk",
            "acme-web is waiting on you to run npm install. It is upgrading the date picker, so it looks expected.\n\nNothing else needs you.",
        )
        .await;
        write_config(
            &overseer,
            "[oversight]\nenabled = true\nlevel = \"suggest\"\n\n[oversight.wake]\nwaiting_minutes = 1\nquiet_minutes = 0\non_finish = false\n",
        );
        let state = hub(std::slice::from_ref(&overseer));
        let d = desk(pool, dir.path()).await;
        let crew = CrewHub::start(Arc::clone(&state), d.clone(), FAST, CancellationToken::new());
        // The person's DM with the overseer, open.
        let dm = overseer
            .sessions
            .get_or_create(DESK_CHANNEL, PERSON, "desk")
            .unwrap();
        overseer.sessions.save(&dm).unwrap();
        let mut frames = state.broadcast_tx.subscribe();

        let s = new_session(&d, dir.path(), "acme-web").await;
        let url = hook_url(dir.path(), s.id).await;
        let asked = tokio::spawn(async move {
            hook(&url, json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash", "tool_input": {"command": "npm install @acme/ds@4.2.0"}})).await
        });
        let pending = eventually("the request", async || d.view(s.id).unwrap().pending).await;

        let needs = eventually("Activity to show it", async || {
            let page = d.activity(Filter::NeedsYou, None, 10).await.unwrap();
            page.events.into_iter().next()
        })
        .await;
        assert_eq!(needs.text, "wants to run npm install @acme/ds@4.2.0");
        assert_eq!(needs.approval_id, Some(pending.id.to_string()));
        let overview = d.overview(crew.as_ref()).await.unwrap();
        let item = overview.items.iter().find(|i| i.id == s.id.to_string()).unwrap();
        assert_eq!(item.state, ItemState::NeedsYou);
        assert_eq!(
            item.approval.as_ref().unwrap().summary,
            "Run npm install @acme/ds@4.2.0?"
        );

        // Past the threshold, the overseer checks in.
        let answer = eventually("the check-in", async || {
            d.store().latest_check_in("desk").await.unwrap()
        })
        .await;
        assert!(answer.prompt.starts_with(super::CHECK_IN));
        assert!(answer.prompt.contains("has waited on you"), "{}", answer.prompt);
        let mut offered = None;
        while let Ok(frame) = frames.try_recv() {
            if frame.session_id == dm.id && frame.msg_type == "stream_end" {
                offered = frame.desk.clone();
            }
        }
        let offered = offered.expect("the answer went out live with what it offers");
        assert_eq!(
            offered["approvals"][0],
            json!({"sessionId": s.id.to_string(), "approvalId": pending.id.to_string(),
                   "summary": "Run npm install @acme/ds@4.2.0?"})
        );
        // It is in the DM's history, the prompt left out, with what it offers.
        let page = crate::desk_conversations::page(
            overseer.sessions.as_ref(),
            &workforce_desk::Speaker::agent("desk", "desk"),
            PERSON,
            None,
            50,
        ).unwrap();
        assert_eq!(page.messages.len(), 1, "{page:?}");
        let kept = eventually("the extras to be kept", async || {
            let mut page = page.clone();
            crate::desk_conversations::with_extras(&d, &mut page).await;
            page.messages[0].desk.clone()
        })
        .await;
        assert_eq!(kept.approvals[0].approval_id, pending.id.to_string());
        let mut read = page.clone();
        crate::desk_conversations::with_extras(&d, &mut read).await;
        let wire = serde_json::to_value(&read).unwrap();
        assert_eq!(
            wire["messages"][0]["desk"]["approvals"][0]["sessionId"],
            s.id.to_string()
        );
        // Its first paragraph is the brief.
        let brief = d.overview(crew.as_ref()).await.unwrap().brief.unwrap();
        assert_eq!(
            brief.text,
            "acme-web is waiting on you to run npm install. It is upgrading the date picker, so it looks expected."
        );
        assert_eq!(brief.by.id, "desk");

        // You allow it through Desk's route.
        let app = workforce_desk::sessions_router(d.clone(), Arc::new(Anyone));
        let response = app
            .oneshot(
                Request::post(format!("/api/desk/sessions/{}/approvals/{}", s.id, pending.id))
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"decision": "allow"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            asked.await.unwrap()["hookSpecificOutput"]["decision"]["behavior"],
            "allow"
        );
        let yours = eventually("your approval in Activity", async || {
            let page = d.activity(Filter::All, None, 20).await.unwrap();
            page.events
                .into_iter()
                .find(|e| e.actor.kind == PartyKind::You && e.kind == Kind::Approved)
        })
        .await;
        assert_eq!(yours.text, "allowed acme-web to run npm install @acme/ds@4.2.0");
        assert_eq!(yours.approval_id, Some(pending.id.to_string()));
        // The check-in counted against the day's cap, and the overseer's own
        // turn is not crew activity.
        assert_eq!(d.store().wake_ups_since(crew.midnight()).await.unwrap(), 1);
        let page = d.activity(Filter::All, None, 50).await.unwrap();
        assert!(page
            .events
            .iter()
            .all(|e| e.subject.as_ref().is_none_or(|s| s.id != "desk")));
    })
    .await;
}

/// An overseer with `oversight`, and a person's DM with it.
async fn overseeing(
    dir: &Path,
    pool: workforce_postgres::PostgresPool,
    reply: &str,
    oversight: &str,
    crew_agents: &[Arc<AgentService>],
) -> (Arc<AgentService>, Arc<CrewHub>, Desk, wiremock::MockServer) {
    let (overseer, llm) = agent(dir, "desk", reply).await;
    write_config(&overseer, oversight);
    let mut all = vec![Arc::clone(&overseer)];
    all.extend(crew_agents.iter().cloned());
    let state = hub(&all);
    let d = desk(pool, dir).await;
    let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
    let dm = overseer
        .sessions
        .get_or_create(DESK_CHANNEL, PERSON, "desk")
        .unwrap();
    overseer.sessions.save(&dm).unwrap();
    (overseer, crew, d, llm)
}

fn session_party(name: &str) -> workforce_desk::crew::Party {
    workforce_desk::crew::Party {
        kind: PartyKind::Session,
        id: uuid::Uuid::new_v4().to_string(),
        name: name.into(),
        engine: Some(Engine::Claude),
        origin: Some(workforce_desk::model::Origin::Desk),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn only_what_the_settings_ask_wakes_the_overseer_and_never_its_own_doing() {
    use workforce_desk::crew::{Event, Party};
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_, crew, _, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = true\ntrack = { crew = false }\nwake = { on_failure = false }\n",
            &[],
        )
        .await;
        let (_, o) = crew.overseer_service().unwrap();
        let web = session_party("acme-web");
        let own = |kind: Kind, who: &Party| Event::new(kind, who.clone(), Some(who.clone()), "finished");
        assert!(crew.trigger_for(&own(Kind::Finished, &web), "desk", &o).is_some());
        assert!(crew.trigger_for(&own(Kind::Question, &web), "desk", &o).is_some());
        assert!(crew.trigger_for(&own(Kind::Failed, &web), "desk", &o).is_none(), "on_failure is off");
        assert!(crew.trigger_for(&own(Kind::Working, &web), "desk", &o).is_none());
        let scout = Party::agent("scout", "Scout");
        assert!(crew.trigger_for(&own(Kind::Finished, &scout), "desk", &o).is_none(), "the crew isn't tracked");
        let me = Party::agent("desk", "Desk");
        assert!(crew.trigger_for(&own(Kind::Finished, &me), "desk", &o).is_none(), "its own turns");
        let by_it = Event::new(Kind::Finished, Party::manager("desk", "Desk"), Some(web.clone()), "x");
        assert!(crew.trigger_for(&by_it, "desk", &o).is_none(), "its own actions");
        let by_you = Event::new(Kind::Finished, Party::you(), Some(web.clone()), "x");
        assert!(crew.trigger_for(&by_you, "desk", &o).is_none(), "yours");
        let handed_on = own(Kind::Finished, &web).hop(super::MAX_HOPS);
        assert!(crew.trigger_for(&handed_on, "desk", &o).is_none(), "the loop guard");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn what_happens_together_is_one_check_in_and_the_day_has_a_cap() {
    use workforce_desk::crew::Event;
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_, _crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "Both sessions finished.",
            "[oversight]\nenabled = true\nmax_checkins_per_day = 1\nwake = { waiting_minutes = 0, quiet_minutes = 0 }\n",
            &[],
        )
        .await;
        for name in ["acme-web", "workforce-e7"] {
            let s = session_party(name);
            d.note(Event::new(Kind::Finished, s.clone(), Some(s), "finished · all green"));
        }
        let first = eventually("a check-in", async || {
            d.store().latest_check_in("desk").await.unwrap()
        })
        .await;
        assert!(first.prompt.contains("acme-web") && first.prompt.contains("workforce-e7"), "{}", first.prompt);
        let s = session_party("dw-676");
        d.note(Event::new(Kind::Finished, s.clone(), Some(s), "finished · merged"));
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(
            d.store().wake_ups_since(chrono::Utc::now() - chrono::Duration::hours(1)).await.unwrap(),
            1,
            "a day's check-ins are capped"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn summaries_are_scheduled_as_set_and_run_in_the_dm() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "Quiet night: nothing needs you.\n\nLedger finished the AWS check.",
            "[oversight]\nenabled = true\nsummaries = { morning = \"08:30\", evening = \"\" }\n",
            &[],
        )
        .await;
        let scheduler = crate::scheduler::Scheduler::new(Arc::clone(&overseer));
        let (_, o) = crew.overseer_service().unwrap();
        super::watcher::sync_summaries(&scheduler, Some(&o)).await;
        let jobs = crate::hosts::SchedulerHost::list_jobs(&scheduler).await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "crew-summary-morning");
        assert_eq!(
            jobs[0].schedule,
            crate::scheduler::Schedule::Cron {
                expr: "30 8 * * *".into(),
                tz: None
            }
        );
        assert!(matches!(
            jobs[0].payload,
            crate::scheduler::JobPayload::CrewSummary { evening: false }
        ));
        super::watcher::sync_summaries(&scheduler, None).await;
        assert!(crate::hosts::SchedulerHost::list_jobs(&scheduler)
            .await
            .is_empty());

        let reply = super::run_summary(&overseer, false).await.unwrap();
        assert!(reply.starts_with("Quiet night"));
        let summary = eventually("the summary in Activity", async || {
            let page = d.activity(Filter::Manager, None, 10).await.unwrap();
            page.events.into_iter().find(|e| e.kind == Kind::Summary)
        })
        .await;
        assert_eq!(summary.text, "sent your morning summary");
        assert_eq!(summary.subject, None);
        let brief = d.overview(crew.as_ref()).await.unwrap().brief.unwrap();
        assert_eq!(brief.text, "Quiet night: nothing needs you.");
        assert_eq!(
            d.store().wake_ups_since(crew.midnight()).await.unwrap(),
            0,
            "a summary is no check-in"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_overseer_messages_the_crew_within_the_loop_guard() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (scout, _s) = agent(dir.path(), "scout", "Nearly done.").await;
        let dm = scout.sessions.get_or_create(DESK_CHANNEL, PERSON, "scout").unwrap();
        scout.sessions.save(&dm).unwrap();
        let (overseer, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"act\"\n",
            &[Arc::clone(&scout)],
        )
        .await;
        eventually("the overseer's tools", async || overseer.tools.has("crew.nudge").then_some(())).await;
        let tool = overseer.tools.get("crew.nudge").unwrap();
        let nudge = || {
            tool.execute(
                json!({"agent_id": "scout", "message": "How is the staging check going?", "reason": "Quiet for 40 min."}),
            )
        };
        let mut frames = crew.state.broadcast_tx.subscribe();
        let done = nudge().await.unwrap();
        assert_eq!(done.output["done"], "asked scout", "{done:?}");
        // The overseer's message goes out live to Scout's open DM before the
        // answer starts, and is the overseer's in the DM's history.
        let (said, before) = relayed_frame(&mut frames, &dm.id).await;
        assert!(!before.iter().any(|t| t.starts_with("stream")), "{before:?}");
        assert_eq!(said.content, "desk asks: How is the staging check going?");
        assert_eq!(
            said.author,
            Some(json!({"kind": "overseer", "id": "desk", "name": "desk"}))
        );
        let item = history_item(&crew, &scout, PERSON, said.id.as_deref().unwrap()).await;
        assert_eq!(item["from"], "user");
        assert_eq!(item["author"], said.author.clone().unwrap());
        assert_eq!(item["text"], said.content);
        let started = eventually("Scout's turn", async || {
            let page = d.activity(Filter::All, None, 20).await.unwrap();
            page.events.into_iter().find(|e| e.kind == Kind::Started && e.actor.id == "scout")
        })
        .await;
        assert_eq!(started.text, "started “desk asks: How is the staging check going?”");
        assert_eq!(started.hop, 1, "a hand-off from the overseer");
        let asked = d
            .activity(Filter::Manager, None, 5)
            .await
            .unwrap()
            .events
            .into_iter()
            .find(|e| e.kind == Kind::Nudged)
            .unwrap();
        assert_eq!(asked.text, "asked scout “How is the staging check going?”");
        assert_eq!(asked.reason.as_deref(), Some("Quiet for 40 min."));

        for _ in 1..super::TURN_BUDGET {
            assert!(nudge().await.unwrap().success);
        }
        let held = nudge().await.unwrap();
        assert!(!held.success);
        assert!(format!("{:?}", held).contains("Loop guard"), "{held:?}");
        crew.person_spoke();
        assert!(nudge().await.unwrap().success, "the person spoke again");
    })
    .await;
}

/// The next `user_message` frame on `sid`, and the types of the frames on
/// `sid` before it.
async fn relayed_frame(
    frames: &mut tokio::sync::broadcast::Receiver<crate::broadcast::BroadcastMessage>,
    sid: &str,
) -> (crate::broadcast::BroadcastMessage, Vec<String>) {
    let mut before = Vec::new();
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(20), frames.recv())
            .await
            .expect("a user_message frame")
            .expect("the broadcast is open");
        if frame.session_id != sid {
            continue;
        }
        if frame.msg_type == super::deliver::USER_MESSAGE {
            return (frame, before);
        }
        before.push(frame.msg_type);
    }
}

/// The item of `owner`'s DM with `agent` that is message `id`, as
/// `GET /api/desk/conversations/{agent}` serves it.
async fn history_item(crew: &CrewHub, agent: &AgentService, owner: &str, id: &str) -> Value {
    let speaker = workforce_desk::Speaker::agent(agent.agent_id.as_str(), &super::name_of(agent));
    let mut page =
        crate::desk_conversations::page(agent.sessions.as_ref(), &speaker, owner, None, 50)
            .unwrap();
    crate::desk_conversations::with_authors(&crew.desk, &mut page).await;
    let wire = serde_json::to_value(&page).unwrap();
    wire["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("message {id} is in the history: {wire}"))
}

/// Saying yes to a suggestion to ask an agent something sends it as yours,
/// worded with your name for the agent reading it.
#[tokio::test(flavor = "multi_thread")]
async fn a_suggestion_said_yes_to_is_yours_and_worded_with_your_name() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (scout, _s) = agent(dir.path(), "scout", "On it.").await;
        let (_overseer, crew, _d, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"suggest\"\n",
            &[Arc::clone(&scout)],
        )
        .await;
        let morgan = workforce_identity::types::Identity::new(
            "Morgan Lee",
            workforce_identity::types::IdentityRole::Member,
        );
        crew.state
            .identity_store
            .as_ref()
            .unwrap()
            .create(&morgan)
            .unwrap();
        let owner = morgan.id.to_string();
        let dm = scout.sessions.get_or_create(DESK_CHANNEL, &owner, "scout").unwrap();
        scout.sessions.save(&dm).unwrap();
        assert_eq!(crew.person_name(&owner), "Morgan Lee");
        assert_eq!(crew.person_name(PERSON), "The person you work for");
        assert_eq!(
            crew.person_name(&uuid::Uuid::new_v4().to_string()),
            "The person you work for"
        );

        let mut frames = crew.state.broadcast_tx.subscribe();
        let suggestion = workforce_desk::crew::Suggestion {
            id: workforce_desk::crew::new_id(),
            at: workforce_desk::crew::now(),
            text: "Ask scout: “Can you check staging?”".into(),
            action: json!({"tool": "crew.nudge", "input": {"agent_id": "scout", "message": "Can you check staging?"}}),
            state: SuggestionState::Open,
            subject: None,
            agent_id: "desk".into(),
        };
        crew.accept(&suggestion).await.unwrap();
        let (said, _) = relayed_frame(&mut frames, &dm.id).await;
        assert_eq!(said.content, "Morgan Lee asks: Can you check staging?");
        assert_eq!(
            said.author,
            Some(json!({"kind": "you", "id": "you", "name": "You"}))
        );
        let item = history_item(&crew, &scout, &owner, said.id.as_deref().unwrap()).await;
        assert_eq!(item["author"]["kind"], "you");
        assert_eq!(item["text"], "Morgan Lee asks: Can you check staging?");
        // Scout's answer is Scout's.
        let answer = eventually("Scout's answer", async || {
            let speaker = workforce_desk::Speaker::agent("scout", &super::name_of(&scout));
            let page =
                crate::desk_conversations::page(scout.sessions.as_ref(), &speaker, &owner, None, 50)
                    .unwrap();
            page.messages.into_iter().find(|m| m.text == "On it.")
        })
        .await;
        assert_eq!(answer.author, workforce_desk::Speaker::agent("scout", "scout"));
    })
    .await;
}

/// A DM left a few days is still the one an open Desk is on: the overseer's
/// unprompted answer goes out on it rather than on a successor it can't see.
#[tokio::test(flavor = "multi_thread")]
async fn an_unprompted_answer_reaches_a_dm_left_open_for_days() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, _d, _llm) = overseeing(
            dir.path(),
            pool,
            "Morning: nothing needs you.",
            "[oversight]\nenabled = true\n",
            &[],
        )
        .await;
        let mut dm = overseer
            .sessions
            .find(DESK_CHANNEL, PERSON)
            .unwrap()
            .unwrap();
        dm.add_user_message("anything new?", None);
        dm.add_assistant_message("All quiet.", 3);
        let old = chrono::Utc::now() - chrono::Duration::days(3);
        for m in &mut dm.messages {
            m.timestamp = old;
        }
        dm.updated_at = old;
        overseer.sessions.save(&dm).unwrap();
        let mut frames = crew.state.broadcast_tx.subscribe();
        super::run_summary(&overseer, false).await.unwrap();
        let mut ended = false;
        while let Ok(frame) = frames.try_recv() {
            ended |= frame.session_id == dm.id && frame.msg_type == "stream_end";
        }
        assert!(
            ended,
            "the answer went out on the session the open DM is on"
        );
    })
    .await;
}
