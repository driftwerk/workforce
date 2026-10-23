//! Agents built for tests through composition: on a mock LLM, on the
//! ChatGPT backend, with identities; what tests across the hub start from.

use std::sync::Arc;

use workforce_common::locking::LockManager;
use workforce_common::settings::WorkforceConfig;
use workforce_identity::types::{Identity, IdentityRole, IdentityStatus};

use super::compose::{Compose as _, SharedAgentResources};
use super::AgentService;
use crate::vault::Vault;

/// A service whose identity store holds one active and one suspended
/// person, returned with them.
pub(crate) async fn service_with_members(
    dir: &std::path::Path,
) -> (Arc<AgentService>, Identity, Identity) {
    use workforce_identity::store::IdentityStore;
    let service = plain_service(dir).await;
    let active = Identity::new("active", IdentityRole::Member);
    let suspended =
        Identity::new("suspended", IdentityRole::Member).with_status(IdentityStatus::Suspended);
    let store = workforce_identity::sqlite::SqliteIdentityStore::in_memory().unwrap();
    store.create(&active).unwrap();
    store.create(&suspended).unwrap();
    service.set_identity_store(Arc::new(store));
    (service, active, suspended)
}

pub(crate) async fn plain_service(data_dir: &std::path::Path) -> Arc<AgentService> {
    let mut config = WorkforceConfig::default();
    config.hub.data_dir = data_dir.to_string_lossy().to_string();
    config.memory.db_path = data_dir.join("memory.db").to_string_lossy().into_owned();
    config.llm.api_key = Some("test-key".into());
    AgentService::from_config(config, None).await.unwrap()
}

/// An agent whose provider is a local OpenAI-compatible endpoint that
/// answers every call, streamed or not, with a short reply.
pub(crate) async fn service_on_mock_llm(
    data_dir: &std::path::Path,
) -> (Arc<AgentService>, wiremock::MockServer) {
    service_on_mock_llm_with(data_dir, |_| {}).await
}

/// [`service_on_mock_llm`], with `configure` applied to its config last.
pub(crate) async fn service_on_mock_llm_with(
    data_dir: &std::path::Path,
    configure: impl FnOnce(&mut WorkforceConfig),
) -> (Arc<AgentService>, wiremock::MockServer) {
    let (config, server) = mock_llm_config(data_dir, configure).await;
    (
        AgentService::from_config(config, None).await.unwrap(),
        server,
    )
}

/// The config of an agent on `data_dir` whose provider is the returned
/// mock, with `configure` applied last.
pub(crate) async fn mock_llm_config(
    data_dir: &std::path::Path,
    configure: impl FnOnce(&mut WorkforceConfig),
) -> (WorkforceConfig, wiremock::MockServer) {
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let chunk = |body: serde_json::Value| format!("data: {body}\n\n");
    let sse = [
        chunk(serde_json::json!({"id": "c1", "object": "chat.completion.chunk", "model": "mock",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": "done"}, "finish_reason": null}]})),
        chunk(serde_json::json!({"id": "c1", "object": "chat.completion.chunk", "model": "mock",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}})),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(serde_json::json!({"stream": true})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "c1", "object": "chat.completion", "model": "mock",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "done"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        })))
        .mount(&server)
        .await;

    let mut config = WorkforceConfig::default();
    config.hub.data_dir = data_dir.to_string_lossy().to_string();
    config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
        .join("memory.db")
        .to_string_lossy()
        .into_owned();
    config.llm.provider = "ollama".into();
    config.llm.api_key = Some(server.uri());
    config.llm.default_model = "mock".into();
    configure(&mut config);
    (config, server)
}

/// An agent built against the ChatGPT backend at `server`, on
/// `provider = "chatgpt"` unless `configure` says otherwise.
pub(crate) async fn chatgpt_service(
    data_dir: &std::path::Path,
    server: &wiremock::MockServer,
    configure: impl FnOnce(&mut WorkforceConfig),
) -> Arc<AgentService> {
    chatgpt_service_on(data_dir, server, configure, None).await
}

/// [`chatgpt_service`] on a hub whose Codex runs on `codex_worker`.
pub(crate) async fn chatgpt_service_on(
    data_dir: &std::path::Path,
    server: &wiremock::MockServer,
    configure: impl FnOnce(&mut WorkforceConfig),
    codex_worker: Option<Arc<dyn crate::coding_sandbox::SandboxLauncher>>,
) -> Arc<AgentService> {
    let mut config = WorkforceConfig::default();
    config.hub.data_dir = data_dir.to_string_lossy().to_string();
    config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
        .join("memory.db")
        .to_string_lossy()
        .into_owned();
    config.subscription.chatgpt_direct = true;
    config.llm.provider = "chatgpt".into();
    config.llm.default_model = "gpt-6.1-sol".into();
    configure(&mut config);
    let agent_config = config.default_agent().cloned();
    let global_vault = Arc::new(Vault::open(&data_dir.join("global.enc"), "test-key").unwrap());
    let subscriptions = crate::subscription::Subscriptions::new(&config)
        .with_chatgpt(crate::subscription::ChatGptBackend::at(
            &server.uri(),
            format!("{}/oauth/token", server.uri()),
            true,
        ))
        .with_device_logins(crate::subscription::DeviceLogins::new(
            Some(Arc::clone(&global_vault)),
            crate::subscription::DeviceAuthClient::at(reqwest::Client::new(), &server.uri()),
        ));
    let subscriptions = match codex_worker {
        Some(worker) => subscriptions.with_codex_worker(worker),
        None => subscriptions,
    };
    super::compose::build_inner(
        config,
        None,
        agent_config.as_ref(),
        None,
        Arc::new(LockManager::new()),
        SharedAgentResources {
            knowledge: None,
            local_agents: None,
            surface_registry: None,
            workflow_runtime: None,
            subscriptions,
        },
        Some(global_vault),
        true,
    )
    .await
    .unwrap()
}

/// Store `person`'s own ChatGPT login on the agent; returns its access
/// token.
pub(crate) fn store_chatgpt_login(service: &AgentService, person: &str) -> String {
    let login = crate::subscription::test_support::login(
        &format!("acct-{}", uuid::Uuid::new_v4()),
        person,
        "rt-0",
    );
    let access = crate::subscription::AuthJson::parse(&login)
        .unwrap()
        .access_token()
        .unwrap()
        .to_string();
    service
        .vault
        .as_ref()
        .expect("agent vault")
        .set(&format!("users/{person}/codex/auth_json"), &login)
        .unwrap();
    access
}

pub(crate) async fn answer_bearer(server: &wiremock::MockServer, access: &str) {
    use wiremock::matchers::{header, method, path};
    wiremock::Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("authorization", format!("Bearer {access}").as_str()))
        .respond_with(crate::subscription::test_support::recorded(
            crate::subscription::test_support::TEXT,
        ))
        .mount(server)
        .await;
}
