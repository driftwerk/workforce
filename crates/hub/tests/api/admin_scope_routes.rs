//! What the admin API gives a caller who holds the `admin` scope through a
//! wildcard token but is no administrator: their own cost and run records,
//! and none of the routes over everyone's tasks, audit trail, jobs, sessions
//! or logs, nor the operator's event sources and knowledge index.

use std::sync::Arc;
use workforce_hub::serve::Compose as _;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};

use workforce_common::persistence::{CostRecord, TaskTelemetryRecord};
use workforce_common::settings::{AccessMode, AgentAccessConfig};
use workforce_hub::agent_registry::AgentRegistry;
use workforce_hub::gateway::{require_admin_auth, CallerContext, GatewayState};
use workforce_hub::scheduler::{JobPayload, Schedule, Scheduler};
use workforce_hub::serve::AgentService;
use workforce_hub::state::HubState;
use workforce_identity::sqlite::SqliteIdentityStore;
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityRole, IdentityStatus};

use crate::support::{hub_state, send, token_for, wildcard_token_for, GatewayParts};

struct Fixture {
    gateway: Arc<GatewayState>,
    hub: Arc<HubState>,
    scheduler: Arc<Scheduler>,
    admin: Identity,
    owner: Identity,
    other: Identity,
    _data: tempfile::TempDir,
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
        let gateway = GatewayParts::new(
            AgentRegistry::from_parts(
                [("scout".to_string(), service)].into_iter().collect(),
                "scout".to_string(),
            ),
            Arc::new(identities),
            data.path().join("skills"),
        )
        .state();
        let mut hub = hub_state(data.path());
        hub.scheduler = Some(Arc::clone(&scheduler));
        Self {
            gateway,
            hub: Arc::new(hub),
            scheduler,
            admin,
            owner,
            other,
            _data: data,
        }
    }

    /// The admin API behind its gate, as the gateway mounts it.
    fn app(&self) -> axum::Router {
        axum::Router::new()
            .nest(
                "/api/admin",
                workforce_hub::api::router(Arc::clone(&self.hub)),
            )
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(&self.gateway),
                require_admin_auth,
            ))
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        bearer: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
            .unwrap();
        let (status, _, text) = send(self.app(), request).await;
        (status, serde_json::from_str(&text).unwrap_or_default())
    }

    /// What `path` answers `caller`, admitted with no gateway handed over.
    async fn get_without_gateway(
        &self,
        path: &str,
        caller: CallerContext,
    ) -> (StatusCode, serde_json::Value) {
        let app = axum::Router::new()
            .nest(
                "/api/admin",
                workforce_hub::api::router(Arc::clone(&self.hub)),
            )
            .layer(axum::Extension(caller));
        let request = Request::get(path).body(Body::empty()).unwrap();
        let (status, _, text) = send(app, request).await;
        (status, serde_json::from_str(&text).unwrap_or_default())
    }

    /// What `path` answers a wildcard device linked to nobody.
    async fn unlinked_device_status(&self, path: &str) -> StatusCode {
        self.get_without_gateway(
            path,
            CallerContext {
                device_id: Some("phone".to_string()),
                scopes: vec!["*".to_string()],
                ..Default::default()
            },
        )
        .await
        .0
    }

    async fn get(&self, path: &str, bearer: &str) -> (StatusCode, serde_json::Value) {
        self.call("GET", path, bearer, None).await
    }

    async fn owners_job(&self) -> String {
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
                    channel_id: None,
                    thread_id: None,
                    model: None,
                    user_id: None,
                    max_rounds: None,
                    agent_owns_delivery: false,
                    tools: None,
                },
                Some(self.owner.id.to_string()),
                Some("global".to_string()),
            )
            .await
            .unwrap()
    }

    /// One call billed to each of the owner, the other member and nobody, and
    /// one scheduled run done as each.
    fn record_each_persons_spend(&self) {
        let people = [
            Some(self.owner.id.to_string()),
            Some(self.other.id.to_string()),
            None,
        ];
        for (n, identity_id) in people.into_iter().enumerate() {
            self.hub
                .telemetry_store
                .record_cost(&CostRecord {
                    agent_id: "scout".into(),
                    model: "gpt-5.5".into(),
                    cost_usd: 0.25,
                    identity_id: identity_id.clone(),
                    ..Default::default()
                })
                .unwrap();
            self.hub
                .telemetry_store
                .record_task_telemetry(&scheduled_run(n, identity_id), &[], &[])
                .unwrap();
        }
    }
}

fn scheduled_run(n: usize, identity_id: Option<String>) -> TaskTelemetryRecord {
    let now = chrono::Utc::now().to_rfc3339();
    TaskTelemetryRecord {
        agent_id: "scout".into(),
        model: String::new(),
        provider: "scheduler".into(),
        input_tokens: 0,
        output_tokens: 0,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        cost_usd: 0.0,
        rounds: 1,
        duration_ms: 100,
        tool_invocation_count: 0,
        success: true,
        error_type: None,
        retry_count: 0,
        task_type: Some("scheduled".into()),
        task_id: Some(format!("run-{n}")),
        started_at: now.clone(),
        completed_at: now,
        forecast_cost_usd: None,
        forecast_rounds: None,
        forecast_confidence: None,
        team_id: None,
        session_id: None,
        model_switches: None,
        ticket_id: None,
        identity_id,
        description: Some("scheduled: queue".into()),
    }
}

/// The people named in a cost or run listing's `records`, sorted.
fn people_in(body: &serde_json::Value) -> Vec<Option<String>> {
    let mut people: Vec<Option<String>> = body["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["user_id"].as_str().map(str::to_string))
        .collect();
    people.sort();
    people
}

#[tokio::test]
async fn a_wildcard_member_is_refused_the_routes_over_everyones_data() {
    let fx = Fixture::new().await;
    let id = fx.owners_job().await;
    let routes = [
        ("GET", "/api/admin/tasks".to_string(), None),
        ("GET", "/api/admin/audit".to_string(), None),
        ("GET", "/api/admin/scheduler".to_string(), None),
        (
            "POST",
            "/api/admin/scheduler".to_string(),
            Some(serde_json::json!({
                "name": "theirs",
                "message": "hello",
                "schedule": { "kind": "every", "every_secs": 86400 },
            })),
        ),
        ("GET", "/api/admin/scheduler/history".to_string(), None),
        ("GET", format!("/api/admin/scheduler/{id}"), None),
        (
            "PUT",
            format!("/api/admin/scheduler/{id}"),
            Some(serde_json::json!({ "name": "renamed" })),
        ),
        ("DELETE", format!("/api/admin/scheduler/{id}"), None),
        ("POST", format!("/api/admin/scheduler/{id}/run"), None),
        ("GET", format!("/api/admin/scheduler/{id}/history"), None),
        ("GET", "/api/admin/sessions/history".to_string(), None),
        ("POST", "/api/admin/sessions/redact".to_string(), None),
        ("GET", "/api/admin/logs".to_string(), None),
        ("GET", "/api/admin/events/sources".to_string(), None),
        (
            "POST",
            "/api/admin/events/sources/slack/start".to_string(),
            None,
        ),
        (
            "POST",
            "/api/admin/events/sources/slack/stop".to_string(),
            None,
        ),
        (
            "POST",
            "/api/admin/events/sources/slack/restart".to_string(),
            None,
        ),
        ("GET", "/api/admin/artifacts/index".to_string(), None),
        ("GET", "/api/admin/knowledge/vectors".to_string(), None),
        ("GET", "/api/admin/knowledge/status".to_string(), None),
        (
            "POST",
            "/api/admin/knowledge/index".to_string(),
            Some(serde_json::json!({})),
        ),
        (
            "POST",
            "/api/admin/knowledge/index/cancel".to_string(),
            Some(serde_json::json!({})),
        ),
        ("GET", "/api/admin/shadow/status".to_string(), None),
        (
            "GET",
            "/api/admin/shadow/report?store=memories".to_string(),
            None,
        ),
    ];
    let jobs = fx.scheduler.list_jobs().await.len();

    for (method, path, body) in &routes {
        for member in [&fx.owner, &fx.other] {
            let (status, _) = fx
                .call(method, path, &wildcard_token_for(member), body.clone())
                .await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{method} {path} for {}",
                member.display_name
            );
        }
    }
    let job = fx.scheduler.get_job(&id).await.unwrap();
    assert_eq!(job.name, "queue");
    assert_eq!(job.run_count, 0);
    assert_eq!(fx.scheduler.list_jobs().await.len(), jobs);

    let admin = token_for(&fx.admin);
    for path in [
        "/api/admin/tasks".to_string(),
        "/api/admin/audit".to_string(),
        "/api/admin/scheduler".to_string(),
        "/api/admin/scheduler/history".to_string(),
        format!("/api/admin/scheduler/{id}"),
        format!("/api/admin/scheduler/{id}/history"),
        "/api/admin/sessions/history".to_string(),
        "/api/admin/logs".to_string(),
        "/api/admin/events/sources".to_string(),
        "/api/admin/artifacts/index".to_string(),
        "/api/admin/knowledge/vectors".to_string(),
        "/api/admin/shadow/status".to_string(),
    ] {
        let (status, body) = fx.get(&path, &admin).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
    }
    let (status, _) = fx.get("/api/admin/knowledge/status", &admin).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "let through to a hub with no knowledge service"
    );
    let (status, _) = fx
        .get("/api/admin/shadow/report?store=memories", &admin)
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "let through to a hub that shadows nothing"
    );
    let (status, body) = fx
        .call(
            "POST",
            "/api/admin/sessions/redact?dry_run=true",
            &admin,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = fx
        .call(
            "DELETE",
            &format!("/api/admin/scheduler/{id}"),
            &admin,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(fx.scheduler.get_job(&id).await.is_none());
}

/// An administrator's token outlives their standing; once they are suspended
/// or made a member, the admin API reads them as the member they now are.
#[tokio::test]
async fn an_administrator_token_minted_before_a_demotion_reads_nothing_here() {
    let fx = Fixture::new().await;
    let identities = fx.gateway.identity_store.as_ref().unwrap();
    let demoted = |change: fn(&mut Identity)| {
        let mut admin = Identity::new("Former admin", IdentityRole::SystemAdmin);
        identities.create(&admin).unwrap();
        let bearer = token_for(&admin);
        change(&mut admin);
        identities.update(&admin).unwrap();
        bearer
    };

    for bearer in [
        demoted(|admin| admin.status = IdentityStatus::Suspended),
        demoted(|admin| admin.set_role(IdentityRole::Member)),
    ] {
        for path in ["/api/admin/audit", "/api/admin/cost/detail"] {
            let (status, _) = fx.get(path, &bearer).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
        }
    }
}

#[tokio::test]
async fn a_wildcard_member_submits_a_task_as_themselves_but_names_no_team() {
    let fx = Fixture::new().await;
    let owner = wildcard_token_for(&fx.owner);
    let task = serde_json::json!({ "title": "t", "description": "summarise the week" });

    let (status, body) = fx
        .call("POST", "/api/admin/tasks", &owner, Some(task.clone()))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = fx.get("/api/admin/tasks", &owner).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the listing beside it stays an administrator's"
    );

    let mut for_a_team = task;
    for_a_team["team_id"] = serde_json::json!("finance");
    let (status, _) = fx
        .call("POST", "/api/admin/tasks", &owner, Some(for_a_team))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn cost_detail_shows_a_wildcard_member_only_their_own_calls() {
    let fx = Fixture::new().await;
    fx.record_each_persons_spend();
    let (owner_id, other_id) = (fx.owner.id.to_string(), fx.other.id.to_string());
    let everyone = {
        let mut everyone = vec![None, Some(owner_id.clone()), Some(other_id.clone())];
        everyone.sort();
        everyone
    };
    let admin = token_for(&fx.admin);

    let (status, body) = fx.get("/api/admin/cost/detail", &admin).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(people_in(&body), everyone);
    assert_eq!(body["distinct_users"].as_array().unwrap().len(), 2);
    let (_, body) = fx
        .get(
            &format!("/api/admin/cost/detail?user_id={other_id}"),
            &admin,
        )
        .await;
    assert_eq!(people_in(&body), [Some(other_id.clone())]);

    for (member, own, someone_else) in [
        (&fx.owner, &owner_id, &other_id),
        (&fx.other, &other_id, &owner_id),
    ] {
        let bearer = wildcard_token_for(member);
        for path in [
            "/api/admin/cost/detail".to_string(),
            format!("/api/admin/cost/detail?user_id={someone_else}"),
        ] {
            let (status, body) = fx.get(&path, &bearer).await;
            assert_eq!(status, StatusCode::OK, "{path}: {body}");
            assert_eq!(people_in(&body), [Some(own.clone())], "{path}");
            assert_eq!(body["total"], 1, "{path}");
            let listed: Vec<&str> = body["distinct_users"]
                .as_array()
                .unwrap()
                .iter()
                .map(|user| user["id"].as_str().unwrap())
                .collect();
            assert_eq!(listed, [own.as_str()], "{path}");
        }
    }

    assert_eq!(
        fx.unlinked_device_status("/api/admin/cost/detail").await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn scheduled_job_history_shows_a_wildcard_member_only_their_own_runs() {
    let fx = Fixture::new().await;
    fx.record_each_persons_spend();
    let (owner_id, other_id) = (fx.owner.id.to_string(), fx.other.id.to_string());
    let path = "/api/admin/scheduler/jobs/history";

    let (status, body) = fx.get(path, &token_for(&fx.admin)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let everyone = {
        let mut everyone = vec![None, Some(owner_id.clone()), Some(other_id.clone())];
        everyone.sort();
        everyone
    };
    assert_eq!(people_in(&body), everyone);

    for (member, own) in [(&fx.owner, owner_id), (&fx.other, other_id)] {
        let (status, body) = fx.get(path, &wildcard_token_for(member)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(people_in(&body), [Some(own)]);
        assert_eq!(body["total"], 1);
    }

    assert_eq!(fx.unlinked_device_status(path).await, StatusCode::FORBIDDEN);
}

/// The sorted keys of a cost summary's `by_agent`.
fn agents_in(body: &serde_json::Value) -> Vec<String> {
    let mut agents: Vec<String> = body["by_agent"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    agents.sort();
    agents
}

#[tokio::test]
async fn cost_figures_name_to_a_wildcard_member_only_the_agents_they_may_see() {
    let fx = Fixture::new().await;
    let service = fx.gateway.registry.get("scout").unwrap();
    fx.gateway.registry.insert("quill", service);
    fx.gateway.access_gate.upsert(
        "quill".into(),
        AgentAccessConfig {
            mode: AccessMode::Allowlist,
            allow: vec![fx.owner.id.to_string()],
            ..Default::default()
        },
    );
    for (agent, cost_usd) in [("scout", 0.25), ("quill", 0.5)] {
        fx.hub
            .telemetry_store
            .record_cost(&CostRecord {
                agent_id: agent.into(),
                model: "gpt-5.5".into(),
                cost_usd,
                ..Default::default()
            })
            .unwrap();
    }
    let both = ["quill", "scout"];

    for (bearer, agents, top) in [
        (token_for(&fx.admin), &both[..], "quill"),
        (wildcard_token_for(&fx.owner), &both[..], "quill"),
        (wildcard_token_for(&fx.other), &["scout"][..], "scout"),
    ] {
        let (status, cost) = fx.get("/api/admin/cost", &bearer).await;
        assert_eq!(status, StatusCode::OK, "{cost}");
        assert_eq!(agents_in(&cost), agents);
        assert_eq!(
            cost["total_cost_usd"], 0.75,
            "a hidden agent's spend stays in the total"
        );
        let (_, dashboard) = fx.get("/api/admin/dashboard", &bearer).await;
        assert_eq!(dashboard["cost"]["top_agent"], top);
        let (_, detail) = fx.get("/api/admin/cost/detail", &bearer).await;
        let mut listed: Vec<&str> = detail["distinct_agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|agent| agent["id"].as_str().unwrap())
            .collect();
        listed.sort();
        assert_eq!(listed, agents);
    }

    let member = CallerContext {
        identity_id: Some(fx.owner.id.to_string()),
        scopes: vec!["*".to_string()],
        ..Default::default()
    };
    let (_, cost) = fx.get_without_gateway("/api/admin/cost", member).await;
    assert!(agents_in(&cost).is_empty(), "no gateway, no agent named");
    assert_eq!(cost["total_cost_usd"], 0.75);
}
