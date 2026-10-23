//! The HTTP trigger receiver, end to end over the real router.
//!
//! A trigger now answers on a minted path (`t.{hex}`) as well as the
//! owner-derived path it has always answered on. Live external systems are
//! posting to the owner-derived ones, so the guarantee this file exists to
//! hold is that **every address that resolved before still resolves**.
//!
//! The Postgres suite does not run in CI (`WORKFORCE_SKIP_POSTGRES_TESTS`), so
//! that guarantee is pinned twice. The store-less cases below need no database
//! and are the CI-enforced half; the store-backed ones cover the member form
//! and the minted paths, and are never the only coverage of a behaviour.

use std::sync::Arc;
use workforce_hub::serve::Compose as _;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use tower::ServiceExt; // for oneshot

use workforce_common::postgres::PostgresPool;
use workforce_common::settings::WorkforceConfig;
use workforce_core::access::Visibility;
use workforce_hub::serve::{create_trigger_router, AgentService};
use workforce_hub::workflow_runtime::WorkflowRuntime;
use workforce_orchestrator::workflow_store::{
    mint_trigger_path, EditContext, RunAsWrite, WorkflowStore, KIND_WORKFLOW,
};

const TRIGGER_TOKEN: &str = "a-trigger-bearer-token";
const VAULT_KEY: &str = "workflows/trigger-token";
/// The default response mode is SSE; the cases that assert on a started
/// instance want the `202` one.
const FIRE_FORGET: &str = "response = \"fire_forget\"\n";

/// A workflow with one HTTP trigger and one report-only step, so starting it
/// touches no model. `extra` adds trigger fields (match rules, response mode).
fn workflow_toml(id: &str, trigger_id: &str, extra: &str) -> String {
    format!(
        "[workflow]\nid = \"{id}\"\nname = \"{id}\"\n\n\
         [[trigger]]\ntype = \"http\"\nid = \"{trigger_id}\"\n{extra}\
         [trigger.auth]\nmode = \"bearer\"\ntoken = \"[[vault:trigger-token]]\"\n\n\
         [[step]]\nid = \"say\"\nname = \"Say\"\ntask = \"\"\n\
         report = \"started\"\nreport_render = \"template\"\nexit = true\n"
    )
}

/// A workflow with no trigger at all.
fn untriggered_toml(id: &str) -> String {
    format!(
        "[workflow]\nid = \"{id}\"\nname = \"{id}\"\n\n\
         [[step]]\nid = \"say\"\nname = \"Say\"\ntask = \"\"\n\
         report = \"quiet\"\nreport_render = \"template\"\nexit = true\n"
    )
}

struct Harness {
    service: Arc<AgentService>,
    runtime: Arc<WorkflowRuntime>,
    router: Router,
    _dir: tempfile::TempDir,
}

impl Harness {
    /// A hub whose `workflows_dir` holds `disk` — system-owned definitions,
    /// loaded with no definition store and therefore no minted paths.
    async fn with_disk_workflows(disk: &[String]) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let workflows_dir = dir.path().join("workflows");
        std::fs::create_dir_all(&workflows_dir).expect("workflows dir");
        for (n, toml) in disk.iter().enumerate() {
            std::fs::write(workflows_dir.join(format!("wf{n}.toml")), toml).expect("write def");
        }

        let mut config = WorkforceConfig::default();
        config.hub.data_dir = dir.path().to_string_lossy().to_string();
        config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        config.llm.api_key = Some("test-key".into());
        let service = AgentService::from_config(config, None)
            .await
            .expect("agent service");
        service
            .vault
            .as_ref()
            .expect("the default config enables a vault")
            .set(VAULT_KEY, TRIGGER_TOKEN)
            .expect("seed the trigger secret");

        let telemetry = Arc::clone(&service.telemetry_store);
        let runtime = Arc::new(WorkflowRuntime::new(
            Arc::clone(&service),
            telemetry,
            workflows_dir,
        ));
        let router = create_trigger_router(
            Arc::clone(&service),
            Arc::clone(&runtime) as Arc<dyn workforce_hub::hosts::WorkflowHost>,
        );
        Self {
            service,
            runtime,
            router,
            _dir: dir,
        }
    }

    async fn post(&self, path: &str, token: &str, body: &str) -> axum::response::Response {
        self.router
            .clone()
            .oneshot(
                Request::post(format!("/triggers/{path}"))
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .expect("request"),
            )
            .await
            .expect("response")
    }

    /// The workflow the instance in a `202` body belongs to.
    async fn started_workflow(&self, response: axum::response::Response) -> String {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        let instance_id = json["instance_id"].as_str().expect("instance_id");
        self.service
            .telemetry_store
            .get_workflow_instance(instance_id)
            .expect("instance lookup")
            .expect("the started instance is persisted")
            .workflow_id
    }

    /// The minted path segment of `def_key`'s only HTTP trigger.
    async fn stable_path(&self, def_key: &str) -> String {
        let url = self
            .runtime
            .trigger_urls(def_key)
            .await
            .first()
            .expect("a definition with an HTTP trigger")
            .url
            .clone()
            .expect("a minted path");
        url.rsplit_once("/triggers/")
            .expect("an absolute trigger URL")
            .1
            .to_string()
    }
}

/// **The test the whole change exists to make pass**, and the only end-to-end
/// one CI runs. Deleting it removes CI's proof that live webhook URLs still
/// resolve.
#[tokio::test]
async fn a_legacy_system_url_still_resolves_after_the_change() {
    let hub =
        Harness::with_disk_workflows(&[workflow_toml("deployer", "deploy", FIRE_FORGET)]).await;

    let response = hub.post("deploy", TRIGGER_TOKEN, "{}").await;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(hub.started_workflow(response).await, "deployer");
}

#[tokio::test]
async fn an_unknown_path_is_404_and_a_bad_token_is_401() {
    let hub =
        Harness::with_disk_workflows(&[workflow_toml("deployer", "deploy", FIRE_FORGET)]).await;

    assert_eq!(
        hub.post("nothing-here", TRIGGER_TOKEN, "{}").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        hub.post("deploy", "wrong-token", "{}").await.status(),
        StatusCode::UNAUTHORIZED
    );
}

/// A minted-looking path that was never minted must fail closed, not fall
/// through to a lookup of whatever follows the prefix.
#[tokio::test]
async fn an_unknown_stable_shaped_path_is_404() {
    let hub =
        Harness::with_disk_workflows(&[workflow_toml("deployer", "deploy", FIRE_FORGET)]).await;

    for path in [mint_trigger_path().as_str(), "t.deploy"] {
        assert_eq!(
            hub.post(path, TRIGGER_TOKEN, "{}").await.status(),
            StatusCode::NOT_FOUND,
            "{path} was never minted"
        );
    }
}

#[tokio::test]
async fn match_rules_and_response_mode_are_unchanged() {
    let hub = Harness::with_disk_workflows(&[
        workflow_toml(
            "matcher",
            "gated",
            &format!("{FIRE_FORGET}match = {{ action = \"opened\" }}\n"),
        ),
        workflow_toml("streamer", "streamed", "response = \"streaming\"\n"),
    ])
    .await;

    assert_eq!(
        hub.post("gated", TRIGGER_TOKEN, r#"{"action":"closed"}"#)
            .await
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        hub.post("gated", TRIGGER_TOKEN, r#"{"action":"opened"}"#)
            .await
            .status(),
        StatusCode::ACCEPTED
    );

    let streamed = hub.post("streamed", TRIGGER_TOKEN, "{}").await;
    assert_eq!(streamed.status(), StatusCode::OK);
    assert_eq!(
        streamed
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
}

/// The store-backed half: a member-owned workflow with its endpoints minted.
/// `test` receives the hub, the store, the pool and the definition's
/// addressing key.
async fn with_member_workflow<F, Fut>(test: F)
where
    F: FnOnce(Harness, Arc<WorkflowStore>, PostgresPool, String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    workforce_postgres::test_support::with_test_pool(
        WorkflowStore::build_pool,
        |pool| async move {
            let hub = Harness::with_disk_workflows(&[]).await;
            let store = WorkflowStore::new(pool.clone()).await.expect("store");
            save_member_workflow(&store, &workflow_toml("deployer", "deploy", FIRE_FORGET)).await;
            hub.runtime.set_store(Arc::clone(&store));
            hub.runtime.reload_definitions().await.expect("reload");
            test(hub, store, pool, "alice/deployer".to_string()).await;
        },
    )
    .await;
}

async fn save_member_workflow(store: &WorkflowStore, toml: &str) {
    let id = toml
        .split_once("id = \"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .expect("a workflow id")
        .0;
    store
        .save(
            id,
            KIND_WORKFLOW,
            toml,
            None,
            Some("alice"),
            Visibility::Private,
            RunAsWrite::Unchanged,
            EditContext::new("web", Some("alice")),
        )
        .await
        .expect("save the member definition");
}

/// The member half of the compatibility guarantee; its CI-enforced
/// counterpart is the unit test over `TriggerIndex::build` with no overlay.
#[tokio::test(flavor = "multi_thread")]
async fn a_legacy_member_url_still_resolves_after_the_change() {
    with_member_workflow(|hub, _store, _pool, key| async move {
        let response = hub.post("alice/deploy", TRIGGER_TOKEN, "{}").await;

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(hub.started_workflow(response).await, key);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stable_url_resolves_to_the_same_workflow() {
    with_member_workflow(|hub, _store, _pool, key| async move {
        let stable = hub.stable_path(&key).await;

        let response = hub.post(&stable, TRIGGER_TOKEN, "{}").await;

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(hub.started_workflow(response).await, key);
    })
    .await;
}

/// The minted path is an address, not a credential.
#[tokio::test(flavor = "multi_thread")]
async fn both_forms_carry_the_same_authentication() {
    with_member_workflow(|hub, _store, _pool, key| async move {
        let stable = hub.stable_path(&key).await;

        for path in [stable.as_str(), "alice/deploy"] {
            assert_eq!(
                hub.post(path, "wrong-token", "{}").await.status(),
                StatusCode::UNAUTHORIZED,
                "{path} must authenticate the caller"
            );
        }
    })
    .await;
}

/// Resolution needs a loaded definition as well as a row, so a deleted
/// definition's address stops answering — and the row waits, so its owner
/// recreating it brings the same address back.
#[tokio::test(flavor = "multi_thread")]
async fn a_stable_url_404s_once_its_definition_is_deleted() {
    with_member_workflow(|hub, store, _pool, key| async move {
        let stable = hub.stable_path(&key).await;

        store
            .delete(
                "deployer",
                KIND_WORKFLOW,
                Some("alice"),
                EditContext::new("web", Some("alice")),
            )
            .await
            .expect("delete");
        hub.runtime.reload_definitions().await.expect("reload");

        assert_eq!(
            hub.post(&stable, TRIGGER_TOKEN, "{}").await.status(),
            StatusCode::NOT_FOUND
        );

        save_member_workflow(&store, &workflow_toml("deployer", "deploy", FIRE_FORGET)).await;
        hub.runtime.reload_definitions().await.expect("reload");

        assert_eq!(
            hub.stable_path(&key).await,
            stable,
            "the address comes back"
        );
        assert_eq!(
            hub.post(&stable, TRIGGER_TOKEN, "{}").await.status(),
            StatusCode::ACCEPTED
        );
    })
    .await;
}

/// Minting is best-effort — a reload must publish its definitions whatever the
/// endpoint table is doing. It must also degrade no further than it has to: a
/// failure that only a newly declared trigger provokes must leave every
/// already-minted address answering, because nothing reloads again until a
/// definition is edited.
#[tokio::test(flavor = "multi_thread")]
async fn a_mint_failure_keeps_the_addresses_already_minted() {
    with_member_workflow(|hub, store, pool, key| async move {
        let stable = hub.stable_path(&key).await;

        // The mint names this constraint in its `ON CONFLICT`, so dropping it
        // is how a reload's minting fails while its reads still succeed.
        pool.get()
            .await
            .expect("conn")
            .execute(
                "ALTER TABLE trigger_endpoints DROP CONSTRAINT trigger_endpoints_binding_key",
                &[],
            )
            .await
            .expect("drop the constraint the mint conflicts on");
        save_member_workflow(&store, &workflow_toml("notifier", "notify", FIRE_FORGET)).await;
        hub.runtime.reload_definitions().await.expect("reload");

        assert_eq!(
            hub.post(&stable, TRIGGER_TOKEN, "{}").await.status(),
            StatusCode::ACCEPTED,
            "an already-minted address must survive a failed mint"
        );
        assert_eq!(
            hub.post("alice/deploy", TRIGGER_TOKEN, "{}").await.status(),
            StatusCode::ACCEPTED,
            "and so must the owner-derived one"
        );
        assert!(
            hub.runtime.trigger_urls("alice/notifier").await[0]
                .url
                .is_none(),
            "only the trigger whose mint failed goes without a minted address"
        );
    })
    .await;
}

/// The entry `GET /api/workflows` and the `workflow` tool's `list` both emit:
/// a definition with an HTTP trigger carries both addresses, and one without
/// carries no field at all.
#[tokio::test(flavor = "multi_thread")]
async fn workflow_list_reports_trigger_urls() {
    with_member_workflow(|hub, store, _pool, _key| async move {
        save_member_workflow(&store, &untriggered_toml("quiet")).await;
        hub.runtime.reload_definitions().await.expect("reload");

        let summaries = hub.runtime.list_definitions().await;
        let summaries = hub.runtime.annotate_automation(summaries).await;
        let entries = hub.runtime.summaries_json(&summaries).await;
        let entry = |id: &str| {
            entries
                .iter()
                .find(|e| e["id"] == id && e["owner"] == "alice")
                .cloned()
                .unwrap_or_else(|| panic!("no list entry for {id}"))
        };

        let triggered = entry("deployer");
        let urls = triggered["trigger_urls"].as_array().expect("trigger_urls");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0]["trigger_id"], "deploy");
        assert!(urls[0]["url"]
            .as_str()
            .expect("a minted URL")
            .contains("/triggers/t."));
        assert_eq!(
            urls[0]["legacy_url"],
            "https://<hub.public_url>/triggers/alice/deploy"
        );

        assert!(
            entry("quiet").get("trigger_urls").is_none(),
            "a workflow with no HTTP trigger reports no field at all"
        );
    })
    .await;
}

/// The moment a user needs the address is the save that creates it: the
/// reload inside the save has already minted it, so the very first save can
/// report it.
#[tokio::test(flavor = "multi_thread")]
async fn a_first_save_can_already_report_its_stable_url() {
    with_member_workflow(|hub, _store, _pool, _key| async move {
        hub.runtime
            .save_definition_source(
                "notifier",
                &workflow_toml("notifier", "notify", FIRE_FORGET),
                Some("alice"),
                Visibility::Private,
                RunAsWrite::Unchanged,
                EditContext::new("agent_tool", Some("alice")),
            )
            .await
            .expect("save");

        let urls = hub.runtime.trigger_urls("alice/notifier").await;

        assert_eq!(urls.len(), 1);
        assert!(urls[0]
            .url
            .as_deref()
            .expect("a minted URL on the first save")
            .contains("/triggers/t."));
    })
    .await;
}
