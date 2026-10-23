//! Who may change a scheduled job over HTTP, and who is shown what it costs.

use std::sync::Arc;
use workforce_hub::serve::Compose as _;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};

use workforce_common::persistence::CostRecord;
use workforce_hub::agent_registry::AgentRegistry;
use workforce_hub::gateway::{require_admin_auth, router, GatewayState};
use workforce_hub::scheduler::{JobPayload, Schedule, ScheduledJob, Scheduler};
use workforce_hub::serve::AgentService;
use workforce_identity::sqlite::SqliteIdentityStore;
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityRole};

use crate::support::{hub_state, send, token_for, GatewayParts};

struct Fixture {
    state: Arc<GatewayState>,
    scheduler: Arc<Scheduler>,
    admin: Identity,
    owner: Identity,
    other: Identity,
    data: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let mut config = workforce_common::settings::WorkforceConfig::default();
        config.hub.data_dir = data.path().to_string_lossy().to_string();
        config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        config.llm.api_key = Some("test-key".into());
        let (service, scheduler) = AgentService::from_config_with_scheduler(config, None)
            .await
            .expect("test service");
        let identities = SqliteIdentityStore::in_memory().expect("in-memory identity store");
        let admin = Identity::new("Admin", IdentityRole::SystemAdmin);
        let owner = Identity::new("Owner", IdentityRole::Member);
        let other = Identity::new("Other", IdentityRole::Member);
        for identity in [&admin, &owner, &other] {
            identities.create(identity).unwrap();
        }
        let registry = AgentRegistry::from_parts(
            [("scout".to_string(), service)].into_iter().collect(),
            "scout".to_string(),
        );
        let state = GatewayParts::new(
            registry,
            Arc::new(identities),
            std::path::PathBuf::from("/tmp/schedule-routes-test-skills"),
        )
        .state();
        Self {
            state,
            scheduler,
            admin,
            owner,
            other,
            data,
        }
    }

    /// An hourly job the owner created, run with their access.
    async fn owners_job(&self, visibility: &str) -> String {
        self.scheduler
            .add_job_owned(
                "queue",
                None,
                Schedule::Every {
                    every_secs: 3600,
                    anchor: None,
                },
                JobPayload::AgentTurn {
                    message: "check the queue".into(),
                    channel: None,
                    channel_id: Some("D123".into()),
                    thread_id: None,
                    model: None,
                    user_id: None,
                    max_rounds: None,
                    agent_owns_delivery: false,
                    tools: Some(vec!["queue".into()]),
                },
                Some(self.owner.id.to_string()),
                Some(visibility.to_string()),
            )
            .await
            .unwrap()
    }

    async fn job(&self, id: &str) -> ScheduledJob {
        self.scheduler.get_job(id).await.unwrap()
    }

    /// The gateway with the admin API mounted in it, as `serve` mounts it.
    fn app(&self) -> axum::Router {
        let mut hub = hub_state(self.data.path());
        hub.scheduler = Some(Arc::clone(&self.scheduler));
        let admin_api = axum::Router::new()
            .nest("/api/admin", workforce_hub::api::router(Arc::new(hub)))
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(&self.state),
                require_admin_auth,
            ));
        router(Arc::clone(&self.state)).merge(admin_api)
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        caller: Option<&Identity>,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json");
        let request = match caller {
            Some(identity) => request.header(
                header::AUTHORIZATION,
                format!("Bearer {}", token_for(identity)),
            ),
            None => request,
        }
        .body(Body::from(body.to_string()))
        .unwrap();
        let (status, _, text) = send(self.app(), request).await;
        (status, serde_json::from_str(&text).unwrap_or_default())
    }
}

/// Every edit that changes what a job does, who sees it or what it replays.
fn edits() -> [serde_json::Value; 5] {
    [
        serde_json::json!({ "tools": [] }),
        serde_json::json!({ "tools": ["exec"] }),
        serde_json::json!({ "session": "pinned" }),
        serde_json::json!({ "visibility": "private" }),
        serde_json::json!({ "name": "theirs", "enabled": false }),
    ]
}

#[tokio::test]
async fn the_member_route_edits_a_job_only_for_its_owner_or_an_admin() {
    let fx = Fixture::new().await;
    let id = fx.owners_job("global").await;
    let path = format!("/api/schedules/{id}");
    let stored = serde_json::to_value(fx.job(&id).await).unwrap();

    for edit in edits() {
        let (status, _) = fx.call("PUT", &path, Some(&fx.other), edit.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{edit}");
        let (status, _) = fx.call("PUT", &path, None, edit.clone()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{edit}");
    }
    assert_eq!(
        serde_json::to_value(fx.job(&id).await).unwrap(),
        stored,
        "a job another member can see is not theirs to change"
    );

    let (status, body) = fx
        .call(
            "PUT",
            &path,
            Some(&fx.owner),
            serde_json::json!({ "tools": ["queue", "notes"], "session": "pinned", "confirmed": true }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["job"]["session"], "pinned");
    let (status, body) = fx
        .call(
            "PUT",
            &path,
            Some(&fx.admin),
            serde_json::json!({ "session": "fresh", "visibility": "private" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let job = fx.job(&id).await;
    assert_eq!(serde_json::to_value(job.session_mode()).unwrap(), "fresh");
    assert_eq!(job.effective_visibility(), "private");
    assert_eq!(
        job.edited_by,
        Some(fx.admin.id.to_string()),
        "an admin's change to what the job does makes the admin its editor"
    );
    assert_eq!(job.owner(), Some(fx.owner.id.to_string().as_str()));
    assert!(
        matches!(&job.payload, JobPayload::AgentTurn { tools: Some(tools), .. } if tools == &["queue", "notes"])
    );
}

#[tokio::test]
async fn the_member_route_takes_no_session_key_and_no_unknown_visibility() {
    let fx = Fixture::new().await;
    let id = fx.owners_job("private").await;
    let path = format!("/api/schedules/{id}");
    let stored = serde_json::to_value(fx.job(&id).await).unwrap();

    let (status, _) = fx
        .call(
            "PUT",
            &path,
            Some(&fx.owner),
            serde_json::json!({ "session": "web" }),
        )
        .await;
    assert!(status.is_client_error(), "{status}");
    let (status, body) = fx
        .call(
            "PUT",
            &path,
            Some(&fx.owner),
            serde_json::json!({ "name": "renamed", "visibility": "everyone" }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap().contains("everyone"),
        "{body}"
    );
    let (status, _) = fx
        .call(
            "PUT",
            &path,
            Some(&fx.owner),
            serde_json::json!({ "channel": "web", "payload": { "channel": "web" } }),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "fields the edit does not have are ignored"
    );
    let mut resaved = serde_json::to_value(fx.job(&id).await).unwrap();
    assert_eq!(
        resaved["edited_by"],
        fx.owner.id.to_string(),
        "an empty save by the creator only re-approves it"
    );
    resaved["edited_by"] = serde_json::Value::Null;
    assert_eq!(resaved, stored);
}

#[tokio::test]
async fn the_admin_route_edits_a_job_only_for_an_admin() {
    let fx = Fixture::new().await;
    let id = fx.owners_job("global").await;
    let path = format!("/api/admin/scheduler/{id}");
    let stored = serde_json::to_value(fx.job(&id).await).unwrap();

    for edit in edits() {
        for member in [&fx.owner, &fx.other] {
            let (status, _) = fx.call("PUT", &path, Some(member), edit.clone()).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{edit}");
        }
        let (status, _) = fx.call("PUT", &path, None, edit.clone()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{edit}");
    }
    assert_eq!(serde_json::to_value(fx.job(&id).await).unwrap(), stored);

    let (status, body) = fx
        .call(
            "PUT",
            &path,
            Some(&fx.admin),
            serde_json::json!({ "tools": [], "session": "pinned", "confirmed": true }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["job"]["session"], "pinned");
    let job = fx.job(&id).await;
    assert_eq!(job.owner(), Some(fx.owner.id.to_string().as_str()));
    assert!(matches!(
        &job.payload,
        JobPayload::AgentTurn { tools: None, .. }
    ));
    assert_eq!(job.edited_by, Some(fx.admin.id.to_string()));

    let (status, _) = fx
        .call(
            "PUT",
            &format!("/api/schedules/{id}"),
            Some(&fx.owner),
            serde_json::json!({ "name": "mine again" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        fx.job(&id).await.edited_by,
        Some(fx.owner.id.to_string()),
        "the creator's own save makes it theirs again"
    );
}

#[tokio::test]
async fn an_edit_that_makes_a_job_cost_more_waits_for_confirmation_on_both_routes() {
    let fx = Fixture::new().await;
    let id = fx.owners_job("private").await;
    let stored = serde_json::to_value(fx.job(&id).await).unwrap();
    // 288 runs a day at the $0.12 default estimate is about $35 a day, over
    // the $5 threshold; a kept session always asks.
    let costlier = [
        serde_json::json!({ "schedule": { "kind": "every", "every_secs": 300 } }),
        serde_json::json!({ "session": "pinned" }),
    ];

    for (path, caller) in [
        (format!("/api/schedules/{id}"), &fx.owner),
        (format!("/api/admin/scheduler/{id}"), &fx.admin),
    ] {
        for edit in &costlier {
            let (status, body) = fx.call("PUT", &path, Some(caller), edit.clone()).await;
            assert_eq!(status, StatusCode::CONFLICT, "{path} {edit}: {body}");
            assert_eq!(body["status"], "needs_confirmation", "{body}");
            assert!(body["reason"].is_string(), "{body}");
            assert!(body["estimate"]["per_day_usd"].is_number(), "{body}");
        }
    }
    assert_eq!(
        serde_json::to_value(fx.job(&id).await).unwrap(),
        stored,
        "nothing is applied before the cost is approved"
    );

    let (status, _) = fx
        .call(
            "PUT",
            &format!("/api/schedules/{id}"),
            Some(&fx.owner),
            serde_json::json!({ "schedule": { "kind": "every", "every_secs": 86400 } }),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a cheaper schedule is not asked about"
    );
    for edit in costlier {
        let mut approved = edit.clone();
        approved["confirmed"] = true.into();
        let (status, body) = fx
            .call(
                "PUT",
                &format!("/api/schedules/{id}"),
                Some(&fx.owner),
                approved,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{edit}: {body}");
    }
    let job = fx.job(&id).await;
    assert!(matches!(
        job.schedule,
        Schedule::Every {
            every_secs: 300,
            ..
        }
    ));
    assert!(job.pinned_session().is_some());
}

#[tokio::test]
async fn a_jobs_spend_is_listed_for_its_owner_and_an_admin_alone() {
    let fx = Fixture::new().await;
    let id = fx.owners_job("global").await;
    fx.scheduler
        .service()
        .telemetry_store
        .record_cost(&CostRecord {
            agent_id: "scout".to_string(),
            model: "m".to_string(),
            cost_usd: 1.5,
            task_id: Some(format!("scheduler:{id}:1")),
            ..Default::default()
        })
        .unwrap();

    for (caller, spent) in [
        (&fx.owner, serde_json::json!(1.5)),
        (&fx.admin, serde_json::json!(1.5)),
        (&fx.other, serde_json::Value::Null),
    ] {
        let (status, body) = fx
            .call("GET", "/api/schedules", Some(caller), serde_json::json!({}))
            .await;
        assert_eq!(status, StatusCode::OK);
        let listed = &body["jobs"][0];
        assert_eq!(listed["id"], id.as_str(), "a global job is listed for all");
        assert_eq!(listed["spent_usd"], spent, "{}", caller.display_name);
    }
}
