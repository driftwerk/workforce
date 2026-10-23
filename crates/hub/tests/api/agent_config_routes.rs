//! The agent config.toml routes write only what the agent would load, and
//! only over the file the caller read.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};

use workforce_hub::gateway::router;
use workforce_hub::serve::AgentService;
use workforce_orchestrator::workflow_store::{EditContext, WorkflowStore, KIND_AGENT_CONFIG};

use crate::support::{agent_service, send, Gateway, GATEWAY_TOKEN};

/// Runs coding rounds locally, so a `codex` route refuses to load.
const UNLOADABLE: &str = "[llm]\nfailover = [\"codex:gpt-5.5\"]\n";
/// Loads, with a finding: calls for the model skip the `chatgpt` route.
const WARNED: &str = "[llm]\nfailover = [\"chatgpt:gpt-4o\"]\n";
const KEPT: &str = "[llm]\nmax_rounds = 12\n";
/// Written by a caller that read the file before a tool changed it.
const UNSEEN: &str = "[llm]\nmax_rounds = 20\n";

struct Fixture {
    gateway: Gateway,
    service: Arc<AgentService>,
    defs: Arc<WorkflowStore>,
    _data: tempfile::TempDir,
}

impl Fixture {
    fn config_path(&self) -> std::path::PathBuf {
        self.service.workspace.join("config.toml")
    }

    fn on_disk(&self) -> String {
        std::fs::read_to_string(self.config_path()).unwrap_or_default()
    }

    async fn revisions(&self) -> usize {
        self.defs
            .list_revisions(KIND_AGENT_CONFIG, "scout", None)
            .await
            .unwrap()
            .len()
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (StatusCode, axum::http::HeaderMap, String) {
        let request = headers
            .iter()
            .fold(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header(header::AUTHORIZATION, format!("Bearer {GATEWAY_TOKEN}")),
                |builder, (name, value)| builder.header(*name, *value),
            )
            .body(Body::from(body.to_string()))
            .unwrap();
        send(router(Arc::clone(&self.gateway.state)), request).await
    }

    async fn put(&self, headers: &[(&str, &str)], body: &str) -> (StatusCode, serde_json::Value) {
        let (status, _, text) = self
            .call("PUT", "/api/agents/scout/config", headers, body)
            .await;
        (status, serde_json::from_str(&text).unwrap())
    }
}

async fn with_fixture<F, Fut>(test: F)
where
    F: FnOnce(Fixture) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    workforce_postgres::test_support::with_test_pool(
        WorkflowStore::build_pool,
        |pool| async move {
            let data = tempfile::tempdir().unwrap();
            let service = agent_service(data.path()).await;
            let defs = WorkflowStore::new(pool).await.expect("def store");
            let gateway = Gateway::new(
                vec![("scout", Arc::clone(&service))],
                Some(Arc::clone(&defs)),
            );
            test(Fixture {
                gateway,
                service,
                defs,
                _data: data,
            })
            .await;
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn config_put_refusing_unloadable_toml_leaves_file_and_revisions() {
    with_fixture(|fx| async move {
        assert_eq!(fx.put(&[], KEPT).await.0, StatusCode::OK);
        let revisions = fx.revisions().await;

        let (status, body) = fx.put(&[], UNLOADABLE).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body["error"].as_str().unwrap().contains("remote worker"),
            "{body}"
        );
        assert_eq!(body["findings"][0]["severity"], "refused");
        assert_eq!(body["findings"][0]["spec"], "codex:gpt-5.5");
        assert_eq!(fx.on_disk(), KEPT, "the file is untouched");
        assert_eq!(fx.revisions().await, revisions, "and so is its history");

        let (status, body) = fx.put(&[], "[llm\n").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.get("findings").is_none(), "{body}");
        assert_eq!(fx.on_disk(), KEPT);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn config_put_returns_warnings_with_saved() {
    with_fixture(|fx| async move {
        let (status, body) = fx.put(&[], WARNED).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["saved"], true);
        assert_eq!(body["findings"][0]["severity"], "warning");
        assert_eq!(body["findings"][0]["spec"], "chatgpt:gpt-4o");
        assert_eq!(fx.on_disk(), WARNED);

        let (status, _, text) = fx
            .call("POST", "/api/agents/scout/config/check", &[], UNLOADABLE)
            .await;
        assert_eq!(status, StatusCode::OK, "a check is a dry run: {text}");
        let checked: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(checked["findings"][0]["severity"], "refused");
        assert_eq!(fx.on_disk(), WARNED, "a check writes nothing");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn config_restore_of_unloadable_revision_is_409() {
    with_fixture(|fx| async move {
        assert_eq!(fx.put(&[], KEPT).await.0, StatusCode::OK);
        let bad = fx
            .defs
            .record_revision(
                KIND_AGENT_CONFIG,
                "scout",
                None,
                UNLOADABLE,
                false,
                EditContext::new("oob", None),
                None,
            )
            .await
            .unwrap()
            .expect("a new revision");
        let revisions = fx.revisions().await;

        let (status, _, text) = fx
            .call(
                "POST",
                &format!("/api/agents/scout/config/revisions/{bad}/restore"),
                &[],
                "",
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{text}");
        let body: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .starts_with(&format!("revision {bad} would not load:")),
            "{body}"
        );
        assert_eq!(fx.on_disk(), KEPT);
        assert_eq!(fx.revisions().await, revisions);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn config_put_with_stale_if_match_is_409() {
    with_fixture(|fx| async move {
        assert_eq!(fx.put(&[], KEPT).await.0, StatusCode::OK);
        let (status, headers, text) = fx.call("GET", "/api/agents/scout/config", &[], "").await;
        assert_eq!((status, text.as_str()), (StatusCode::OK, KEPT));
        let read = headers[header::ETAG].to_str().unwrap().to_string();

        std::fs::write(fx.config_path(), WARNED).unwrap();
        let (status, body) = fx.put(&[("if-match", &read)], KEPT).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body["current_hash"].is_string(), "{body}");
        assert_eq!(fx.on_disk(), WARNED, "the newer save survives");

        let current = body["current_hash"].as_str().unwrap().to_string();
        let (status, body) = fx.put(&[("if-match", &current)], KEPT).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(fx.on_disk(), KEPT);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn config_put_returns_the_etag_a_read_serves() {
    with_fixture(|fx| async move {
        let (status, saved) = fx.put(&[], KEPT).await;
        assert_eq!(status, StatusCode::OK, "{saved}");
        let (_, headers, _) = fx.call("GET", "/api/agents/scout/config", &[], "").await;
        assert_eq!(saved["etag"], headers[header::ETAG].to_str().unwrap());

        let etag = saved["etag"].as_str().unwrap().to_string();
        let (status, body) = fx.put(&[("if-match", &etag)], WARNED).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(fx.on_disk(), WARNED);
    })
    .await;
}

/// A compressing proxy suffixes a transformable response's ETag with its
/// coding (Caddy: `"h"` → `"h-zstd"`), so the editor's `If-Match` never
/// matched and every save was refused as stale. The read forbids transforms.
#[tokio::test(flavor = "multi_thread")]
async fn config_read_forbids_proxy_transforms_of_its_etag() {
    with_fixture(|fx| async move {
        assert_eq!(fx.put(&[], KEPT).await.0, StatusCode::OK);
        let (status, headers, _) = fx.call("GET", "/api/agents/scout/config", &[], "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CACHE_CONTROL], "no-transform");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn config_put_if_match_star_needs_an_existing_file() {
    with_fixture(|fx| async move {
        let (status, body) = fx.put(&[("if-match", "*")], KEPT).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(!fx.config_path().exists());

        assert_eq!(fx.put(&[], KEPT).await.0, StatusCode::OK);
        assert_eq!(fx.put(&[("if-match", "*")], WARNED).await.0, StatusCode::OK);
        assert_eq!(fx.on_disk(), WARNED);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn config_put_replaces_the_file_rather_than_rewriting_it() {
    use std::os::unix::fs::MetadataExt;
    with_fixture(|fx| async move {
        assert_eq!(fx.put(&[], KEPT).await.0, StatusCode::OK);
        let before = std::fs::metadata(fx.config_path()).unwrap().ino();
        assert_eq!(fx.put(&[], WARNED).await.0, StatusCode::OK);
        assert_ne!(
            std::fs::metadata(fx.config_path()).unwrap().ino(),
            before,
            "a reader sees the old file or the new one, never a torn one"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn config_put_checks_if_match_under_the_lock_tool_writes_hold() {
    use workforce_common::settings::AgentWorkspaceConfig;
    with_fixture(|fx| async move {
        assert_eq!(fx.put(&[], KEPT).await.0, StatusCode::OK);
        let (_, headers, _) = fx.call("GET", "/api/agents/scout/config", &[], "").await;
        let read = headers[header::ETAG].to_str().unwrap().to_string();

        let workspace = fx.service.workspace.clone();
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let tool_write = std::thread::spawn(move || {
            AgentWorkspaceConfig::locked(&workspace, || {
                held_tx.send(()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(500));
                AgentWorkspaceConfig::write(&workspace, WARNED).unwrap();
            })
        });
        held_rx.recv().unwrap();

        let (status, body) = fx.put(&[("if-match", &read)], UNSEEN).await;
        tool_write.join().unwrap();
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(
            fx.on_disk(),
            WARNED,
            "the write that held the lock survives"
        );
    })
    .await;
}

// ── system.config: the agent's own tool saves through the same write ──

const COMMENTED: &str = "# Per-agent configuration — change it in the agent's settings.\n\
[llm]\n\
max_rounds = 12 # tool rounds per turn\n";

async fn config_tool(fx: &Fixture, input: serde_json::Value) -> workforce_agent::tools::ToolResult {
    use workforce_agent::tools::Tool;
    workforce_hub::config_tool::ConfigTool::new(Arc::clone(&fx.service), Some(Arc::clone(&fx.defs)))
        .execute(input)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn config_tool_set_keeps_comments_records_a_revision_and_applies() {
    with_fixture(|fx| async move {
        assert_eq!(fx.put(&[], COMMENTED).await.0, StatusCode::OK);
        // The tool records under the agent's own id.
        let history = || async {
            fx.defs
                .list_revisions(KIND_AGENT_CONFIG, fx.service.agent_id.as_str(), None)
                .await
                .unwrap()
        };
        let revisions = history().await.len();

        let result = config_tool(
            &fx,
            serde_json::json!({"action": "set", "key": "llm.max_rounds", "value": 30}),
        )
        .await;
        assert!(result.success, "{:?}", result.output);
        assert_eq!(
            fx.on_disk(),
            COMMENTED.replace("max_rounds = 12", "max_rounds = 30")
        );
        let latest = history().await;
        assert!(latest.len() > revisions, "the change is in its history");
        assert_eq!(latest[0].edited_via, "agent_tool");
        assert_eq!(
            fx.service.config_applied.read().as_deref(),
            Some(workforce_orchestrator::workflow_store::content_hash(&fx.on_disk()).as_str()),
            "the running agent applied the saved file"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn config_tool_refusals_leave_file_and_revisions() {
    with_fixture(|fx| async move {
        assert_eq!(fx.put(&[], COMMENTED).await.0, StatusCode::OK);
        let history = || async {
            fx.defs
                .list_revisions(KIND_AGENT_CONFIG, fx.service.agent_id.as_str(), None)
                .await
                .unwrap()
                .len()
        };
        let revisions = history().await;
        for (input, why) in [
            (
                serde_json::json!({"action": "set", "key": "llm.failover", "value": ["codex:gpt-5.5"]}),
                "would not load",
            ),
            (
                serde_json::json!({"action": "set", "key": "llm.voice_modle", "value": "x"}),
                "not a setting",
            ),
            (
                serde_json::json!({"action": "set", "key": "llm.max_rounds", "value": "many"}),
                "cannot take that value",
            ),
            (
                serde_json::json!({"action": "set", "key": "llm.api_key", "value": "sk-x"}),
                "vault",
            ),
        ] {
            let result = config_tool(&fx, input.clone()).await;
            assert!(!result.success, "{input}");
            let message = result.error.clone().unwrap_or_default();
            assert!(message.contains(why), "{input}: {message}");
        }
        assert_eq!(fx.on_disk(), COMMENTED);
        assert_eq!(history().await, revisions);
    })
    .await;
}

/// The tool saves over the body it read and nothing else: an edit that lands
/// in between survives, as a UI save over a stale ETag does.
#[tokio::test(flavor = "multi_thread")]
async fn a_config_write_over_a_stale_read_is_refused() {
    use workforce_hub::agent_config_write::{save, Refused};
    with_fixture(|fx| async move {
        assert_eq!(fx.put(&[], KEPT).await.0, StatusCode::OK);
        assert_eq!(fx.put(&[], WARNED).await.0, StatusCode::OK);
        let revisions = fx.revisions().await;

        let refused = save(
            &fx.service,
            "scout",
            Some(&fx.defs),
            UNSEEN,
            EditContext::new("agent_tool", None),
            |current| current == Some(KEPT),
        )
        .await
        .unwrap_err();
        assert!(matches!(refused, Refused::Stale { .. }), "{refused:?}");
        assert_eq!(fx.on_disk(), WARNED);
        assert_eq!(fx.revisions().await, revisions);
    })
    .await;
}
