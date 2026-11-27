//! Every route the gateway serves, with its class, as checked in at
//! `tests/route_manifest.txt`, and what each class means where it is
//! served. `UPDATE_ROUTE_MANIFEST=1` rewrites the snapshot.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use tower::ServiceExt;
use workforce_hub::agent_registry::AgentRegistry;
use workforce_hub::gateway::GatewayState;
use workforce_hub::serve::{all_routes, combined, webhook_state, AgentService, Mounts};
use workforce_hub::state::HubState;
use workforce_hub::surface::{Bound, Declared, RouteClass, Routes, Surface};
use workforce_identity::sqlite::SqliteIdentityStore;
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityRole};
use workforce_pairing::{DevicePairingService, PairingConfig, SqlitePairingStore};

use crate::support::{agent_service, hub_state, token_for, GatewayParts};

const SNAPSHOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/route_manifest.txt");

/// What Desk on another hub calls there as the person signed in.
const DESK: &[(&str, &str)] = &[
    ("GET", "/api/me"),
    ("GET", "/api/agents"),
    ("GET", "/api/commands"),
    ("GET", "/api/agents/{id}/picture"),
    ("GET", "/api/me/picture"),
    ("PUT", "/api/me/picture"),
    ("DELETE", "/api/me/picture"),
    ("GET", "/api/agents/{id}/attachments/{artifact}"),
    ("POST", "/api/agents/{id}/attachments"),
    ("GET", "/api/me/llm-route"),
    ("PUT", "/api/me/llm-route"),
    ("GET", "/api/me/agents/{id}/llm-route"),
    ("PUT", "/api/me/agents/{id}/llm-route"),
    ("GET", "/api/desk/conversations"),
    ("GET", "/api/desk/conversations/{agent}"),
    ("PUT", "/api/desk/conversations/{agent}/read"),
    ("POST", "/api/desk/conversations/{agent}/close"),
    ("GET", "/api/desk/conversations/{agent}/threads"),
    ("POST", "/api/desk/conversations/{agent}/messages"),
    ("GET", "/api/desk/conversations/{agent}/search"),
    (
        "DELETE",
        "/api/desk/conversations/{agent}/messages/{message}",
    ),
    (
        "GET",
        "/api/desk/conversations/{agent}/messages/{message}/memories",
    ),
    ("POST", "/api/approvals/{request_id}/decision"),
    ("POST", "/api/pair/device/revoke-self"),
    ("GET", "/api/desk/floor"),
    ("GET", "/api/desk/usage"),
    ("GET", "/api/desk/usage/work"),
    ("POST", "/api/desk/needs/decide"),
    ("GET", "/api/me/desk-theme-jobs"),
    ("POST", "/api/me/desk-theme-jobs/{id}"),
    ("GET", "/api/me/desk-key-copies"),
    ("POST", "/api/me/desk-key-copies/take"),
    ("POST", "/api/me/remote-mcp-tokens/desk"),
    ("DELETE", "/api/me/remote-mcp-tokens/desk/{id}"),
    ("POST", "/api/me/desk-knows-tokens"),
    ("DELETE", "/api/me/desk-knows-tokens/{id}"),
    ("GET", "/api/me/desk-knows-tokens/engines"),
    ("DELETE", "/api/me/desk-knows-tokens/engines/{engine}"),
    ("GET", "/api/me/desk-boss-links"),
    ("POST", "/api/me/desk-boss-links"),
    ("DELETE", "/api/me/desk-boss-links/{id}"),
    ("GET", "/api/monitors/sort-rules"),
    ("PUT", "/api/monitors/{id}/enabled"),
    ("DELETE", "/api/monitors/{id}"),
    ("GET", "/api/agents/{id}/tools"),
    ("POST", "/api/pair/device/request"),
    ("GET", "/api/pair/device/status/{code}"),
    ("GET", "/ws"),
    ("GET", "/ws/desk"),
];

/// What the iPhone app calls.
const IPHONE: &[(&str, &str)] = &[
    ("GET", "/api/sessions"),
    ("GET", "/api/sessions/{id}/transcript"),
    ("GET", "/api/sessions/{id}/stats"),
    ("GET", "/api/agents"),
    ("GET", "/api/livez"),
    ("POST", "/api/auth/login"),
    ("GET", "/api/me"),
    ("POST", "/api/auth/refresh"),
    ("POST", "/api/auth/link-device"),
    ("POST", "/api/pair/device/request"),
    ("POST", "/api/pair/device/rotate"),
    ("GET", "/api/pair/device/status/{code}"),
    ("GET", "/ws"),
];

/// What the dashboard's member pages call that stays theirs.
const DASHBOARD: &[(&str, &str)] = &[
    ("DELETE", "/api/artifacts/{id}"),
    ("DELETE", "/api/consolidation-templates/{id}"),
    ("DELETE", "/api/me/remote-mcp-tokens/{id}"),
    ("DELETE", "/api/me/secret"),
    ("DELETE", "/api/me/subscriptions/chatgpt/device/{flow_id}"),
    ("DELETE", "/api/me/subscriptions/{kind}/login"),
    ("DELETE", "/api/me/vault/{*path}"),
    ("DELETE", "/api/monitors/{id}"),
    ("DELETE", "/api/playbooks/{id}"),
    ("DELETE", "/api/schedules/{id}"),
    ("DELETE", "/api/workflows/{id}"),
    ("GET", "/api/agents"),
    ("GET", "/api/agents/{id}"),
    ("GET", "/api/agents/{id}/config"),
    ("GET", "/api/agents/{id}/config/revisions"),
    ("GET", "/api/agents/{id}/config/revisions/{rev}"),
    ("GET", "/api/agents/{id}/logins"),
    ("GET", "/api/agents/{id}/settings"),
    ("GET", "/api/agents/{id}/slack"),
    ("GET", "/api/agents/{id}/stats"),
    ("GET", "/api/agents/{id}/tools"),
    ("GET", "/api/artifacts"),
    ("GET", "/api/artifacts/search"),
    ("GET", "/api/artifacts/{id}"),
    ("GET", "/api/auth/config"),
    ("GET", "/api/auth/oidc/login"),
    ("GET", "/api/capabilities/catalog"),
    ("GET", "/api/consolidation-templates"),
    ("GET", "/api/consolidation-templates/{id}/revisions"),
    ("GET", "/api/consolidation-templates/{id}/revisions/{rev}"),
    ("GET", "/api/consolidation-templates/{id}/source"),
    ("GET", "/api/identity/teams"),
    ("GET", "/api/identity/teams/{id}"),
    ("GET", "/api/me/agents/{id}/llm-route"),
    ("GET", "/api/me/capabilities"),
    ("GET", "/api/me/integrations"),
    ("GET", "/api/me/llm-route"),
    ("GET", "/api/me/relationship"),
    ("GET", "/api/me/remote-mcp-tokens"),
    ("GET", "/api/me/subscriptions"),
    ("GET", "/api/me/subscriptions/chatgpt/device"),
    ("GET", "/api/me/subscriptions/chatgpt/device/{flow_id}"),
    ("GET", "/api/me/vault"),
    ("GET", "/api/models/aliases"),
    ("GET", "/api/models/health"),
    ("GET", "/api/monitors"),
    ("GET", "/api/monitors/{id}/activity"),
    ("GET", "/api/monitors/{id}/revisions"),
    ("GET", "/api/monitors/{id}/revisions/{rev}"),
    ("GET", "/api/monitors/{id}/source"),
    ("GET", "/api/pair/device/pending/{code}"),
    ("GET", "/api/pipelines"),
    ("GET", "/api/pipelines/{scope}/{id}"),
    ("GET", "/api/pipelines/{scope}/{id}/items"),
    ("GET", "/api/pipelines/{scope}/{id}/items/{item}/costs"),
    ("GET", "/api/pipelines/{scope}/{id}/items/{item}/timings"),
    ("GET", "/api/playbooks"),
    ("GET", "/api/playbooks/{id}/revisions"),
    ("GET", "/api/playbooks/{id}/revisions/{rev}"),
    ("GET", "/api/playbooks/{id}/source"),
    ("GET", "/api/schedules"),
    ("GET", "/api/schedules/{id}/history"),
    ("GET", "/api/sessions"),
    ("GET", "/api/sessions/{id}/export"),
    ("GET", "/api/sessions/{id}/stats"),
    ("GET", "/api/sessions/{id}/transcript"),
    ("GET", "/api/skills"),
    ("GET", "/api/skills/catalog"),
    ("GET", "/api/skills/{name}"),
    ("GET", "/api/skills/{name}/config"),
    ("GET", "/api/stats/tasks/{task_id}"),
    ("GET", "/api/vault/catalog"),
    ("GET", "/api/version"),
    ("GET", "/api/workflows"),
    ("GET", "/api/workflows/{id}/revisions"),
    ("GET", "/api/workflows/{id}/revisions/{rev}"),
    ("GET", "/api/workflows/{id}/source"),
    ("GET", "/games/api/config"),
    ("GET", "/games/api/rooms"),
    ("POST", "/api/artifacts"),
    ("POST", "/api/consolidation-templates/parse-xlsx"),
    ("POST", "/api/consolidation-templates/upload"),
    (
        "POST",
        "/api/consolidation-templates/{id}/revisions/{rev}/restore",
    ),
    ("POST", "/api/me/remote-mcp-tokens"),
    ("POST", "/api/me/secret"),
    ("POST", "/api/me/subscriptions/chatgpt/device"),
    ("POST", "/api/me/subscriptions/{kind}/login"),
    ("POST", "/api/me/vault/move"),
    ("POST", "/api/me/vault/{*path}"),
    ("POST", "/api/monitors/{id}/revisions/{rev}/restore"),
    ("POST", "/api/pair/device/approve-self"),
    ("POST", "/api/pair/device/reject-self"),
    ("POST", "/api/playbooks/{id}/revisions/{rev}/restore"),
    ("POST", "/api/schedules"),
    ("POST", "/api/schedules/{id}/run"),
    ("POST", "/api/sessions/{id}/upload"),
    ("POST", "/api/workflows/{id}/revisions/{rev}/restore"),
    ("POST", "/api/workflows/{id}/run"),
    ("POST", "/games/api/rooms"),
    ("PUT", "/api/artifacts/{id}"),
    ("PUT", "/api/consolidation-templates/{id}"),
    ("PUT", "/api/me/agents/{id}/llm-route"),
    ("PUT", "/api/me/llm-route"),
    ("PUT", "/api/monitors/{id}"),
    ("PUT", "/api/playbooks/{id}"),
    ("PUT", "/api/schedules/{id}"),
    ("PUT", "/api/workflows/{id}"),
];

/// The routes past Open that answer someone signed out with a 2xx, and why
/// that gives them nothing.
const SIGNED_OUT_ANSWERS: &[(&str, &str, &str)] = &[
    (
        "GET",
        "/api/agents",
        "an empty roster: no agent is visible to no one",
    ),
    (
        "GET",
        "/api/auth/spotify/callback",
        "a page saying the sign-in failed",
    ),
    ("DELETE", "/mcp", "closing a session that holds no state"),
    (
        "DELETE",
        "/mcp/agents/{agent_id}",
        "closing a session that holds no state",
    ),
    (
        "POST",
        "/webhooks/{source}",
        "acknowledged at once; each trigger checks its own signature",
    ),
    (
        "POST",
        "/webhooks/{source}/{event_type}",
        "acknowledged at once; each trigger checks its own signature",
    ),
];

struct Hub {
    state: Arc<GatewayState>,
    service: Arc<AgentService>,
    hub: Arc<HubState>,
    admin: Identity,
    member: Identity,
    device: String,
    _dir: tempfile::TempDir,
}

async fn hub() -> Hub {
    let dir = tempfile::tempdir().unwrap();
    let service = agent_service(dir.path()).await;
    let identities = SqliteIdentityStore::in_memory().unwrap();
    let admin = Identity::new("Admin", IdentityRole::SystemAdmin);
    let member = Identity::new("Member", IdentityRole::Member);
    identities.create(&admin).unwrap();
    identities.create(&member).unwrap();
    let pairing = Arc::new(DevicePairingService::new(
        Arc::new(SqlitePairingStore::in_memory().unwrap()),
        PairingConfig {
            device_pairing_enabled: true,
            ..Default::default()
        },
    ));
    let asked = pairing
        .initiate("Desk", vec!["*".into()], None, None, None, None)
        .unwrap();
    let (_, device) = pairing.approve(&asked.code, &admin.id.to_string()).unwrap();
    let state = GatewayParts {
        device_pairing: Some(pairing),
        ..GatewayParts::new(
            AgentRegistry::from_parts(
                [("scout".to_string(), Arc::clone(&service))]
                    .into_iter()
                    .collect(),
                "scout".into(),
            ),
            Arc::new(identities) as Arc<dyn IdentityStore>,
            dir.path().join("skills"),
        )
    }
    .state();
    Hub {
        state,
        hub: Arc::new(hub_state(dir.path())),
        service,
        admin,
        member,
        device,
        _dir: dir,
    }
}

/// Every route, as `serve` assembles them: the gateway's, the admin API's
/// and the webhooks' answering, every other part only declared.
fn every_route(hub: &Hub) -> Routes {
    all_routes(Mounts {
        gateway: Arc::clone(&hub.state),
        admin: Bound::To(Arc::clone(&hub.hub)),
        webhooks: Bound::To(webhook_state(Arc::clone(&hub.service), None)),
        triggers: Some(Bound::Declared),
        mcp_callback: Bound::Declared,
        mcp_servers: Bound::Declared,
        games: Some(Bound::Declared),
        desk: Some(Bound::Declared),
        teams: Some(Bound::Declared),
        cors: tower_http::cors::CorsLayer::new(),
    })
}

/// `path` with each of its parameters filled in.
fn concrete(path: &str) -> String {
    path.split('/')
        .map(|segment| match segment.starts_with('{') {
            true => "zz-param",
            false => segment,
        })
        .collect::<Vec<_>>()
        .join("/")
}

async fn status(app: &axum::Router, route: &Declared, bearer: Option<&str>) -> StatusCode {
    let request = Request::builder()
        .method(route.method.clone())
        .uri(concrete(&route.path))
        .header("content-type", "application/json");
    let request = match bearer {
        Some(token) => request.header("authorization", format!("Bearer {token}")),
        None => request,
    };
    tokio::time::timeout(
        Duration::from_secs(10),
        app.clone().oneshot(request.body(Body::empty()).unwrap()),
    )
    .await
    .unwrap_or_else(|_| panic!("{route} answers in time"))
    .unwrap()
    .status()
}

fn guarded(hub: &Hub, routes: Routes) -> axum::Router {
    routes.render(
        Surface::Admin,
        axum::middleware::from_fn_with_state(
            Arc::clone(&hub.state),
            workforce_hub::auth::require_beyond_member,
        ),
    )
}

#[tokio::test]
async fn every_route_is_in_the_checked_in_manifest() {
    let hub = hub().await;
    let routes = every_route(&hub);
    let manifest = routes.manifest();
    let mut keys: Vec<_> = manifest
        .iter()
        .map(|r| (&r.path, r.method.as_str()))
        .collect();
    keys.dedup();
    assert_eq!(keys.len(), manifest.len(), "a route is declared twice");
    let text: String = manifest.iter().map(|route| format!("{route}\n")).collect();
    let _ = combined(routes);
    if std::env::var_os("UPDATE_ROUTE_MANIFEST").is_some() {
        std::fs::write(SNAPSHOT, &text).unwrap();
    }
    let checked_in = std::fs::read_to_string(SNAPSHOT).unwrap_or_default();
    if text != checked_in {
        println!("---ROUTE MANIFEST---\n{text}---END ROUTE MANIFEST---");
    }
    assert!(
        text == checked_in,
        "the routes differ from tests/route_manifest.txt (printed above); if that is meant, rerun \
         UPDATE_ROUTE_MANIFEST=1 cargo test -p workforce-hub --all-features --test api route_manifest \
         and review the diff"
    );
}

#[tokio::test]
async fn no_admin_route_is_on_the_member_surface() {
    let hub = hub().await;
    let routes = every_route(&hub);
    let admin: Vec<_> = routes
        .manifest()
        .into_iter()
        .filter(|route| route.class == RouteClass::Admin)
        .collect();
    assert!(!admin.is_empty());
    let public = routes.render(Surface::Public, tower::layer::util::Identity::new());
    let mut served = Vec::new();
    for route in &admin {
        let status = status(&public, route, Some(&hub.state.auth_token)).await;
        if status != StatusCode::NOT_FOUND {
            served.push(format!("{route}: {status}"));
        }
    }
    assert!(served.is_empty(), "served on Public: {served:#?}");
}

#[tokio::test]
async fn what_desk_the_iphone_and_member_pages_call_is_for_members() {
    let hub = hub().await;
    let manifest = every_route(&hub).manifest();
    let class = |method: &str, path: &str| {
        manifest
            .iter()
            .find(|route| route.method.as_str() == method && route.path == path)
            .map(|route| route.class)
    };
    let wrong: Vec<_> = [("desk", DESK), ("iphone", IPHONE), ("dashboard", DASHBOARD)]
        .into_iter()
        .flat_map(|(caller, calls)| calls.iter().map(move |call| (caller, call)))
        .filter(|(_, (method, path))| {
            !matches!(
                class(method, path),
                Some(RouteClass::Member | RouteClass::Open)
            )
        })
        .map(|(caller, (method, path))| {
            format!("{caller}: {method} {path}: {:?}", class(method, path))
        })
        .collect();
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test]
async fn no_route_but_an_open_one_answers_someone_signed_out() {
    let hub = hub().await;
    let routes = every_route(&hub);
    let manifest = routes.manifest();
    let app = combined(routes);
    let mut answered = Vec::new();
    for route in manifest
        .iter()
        .filter(|route| route.class != RouteClass::Open)
    {
        if status(&app, route, None).await.is_success() {
            answered.push(format!("{} {}", route.method, route.path));
        }
    }
    let known: Vec<_> = SIGNED_OUT_ANSWERS
        .iter()
        .map(|(method, path, _)| format!("{method} {path}"))
        .collect();
    assert_eq!(answered, known, "what answers someone signed out");
}

#[tokio::test]
async fn the_admin_guard_refuses_members_and_devices_on_every_admin_route() {
    let hub = hub().await;
    let routes = every_route(&hub);
    let admin: Vec<_> = routes
        .manifest()
        .into_iter()
        .filter(|route| route.class == RouteClass::Admin)
        .collect();
    let app = guarded(&hub, routes);
    let member = token_for(&hub.member);
    let mut let_through = Vec::new();
    for route in &admin {
        for (who, bearer) in [
            ("no one", None),
            ("a member", Some(member.as_str())),
            ("a device", Some(hub.device.as_str())),
        ] {
            let status = status(&app, route, bearer).await;
            if !matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
                let_through.push(format!("{route} for {who}: {status}"));
            }
        }
    }
    assert!(let_through.is_empty(), "{let_through:#?}");
}

#[tokio::test]
async fn the_admin_guard_admits_anyone_beyond_a_member_by_header_or_query() {
    let hub = hub().await;
    let app = guarded(
        &hub,
        Routes::new().admin(
            "/beyond",
            workforce_hub::surface::routing::get(|| async { "admitted" }),
        ),
    );
    let admin = token_for(&hub.admin);
    let member = token_for(&hub.member);
    let call = |query: Option<&str>, bearer: Option<&str>| {
        let uri = query.map_or("/beyond".to_string(), |token| {
            format!("/beyond?token={token}")
        });
        let request = Request::builder().method(Method::GET).uri(uri);
        let request = match bearer {
            Some(token) => request.header("authorization", format!("Bearer {token}")),
            None => request,
        };
        let app = app.clone();
        async move {
            app.oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status()
        }
    };
    for (query, bearer, expected) in [
        (None, Some(hub.state.auth_token.as_str()), StatusCode::OK),
        (None, Some(admin.as_str()), StatusCode::OK),
        (Some(admin.as_str()), None, StatusCode::OK),
        (Some(member.as_str()), Some(admin.as_str()), StatusCode::OK),
        (Some(admin.as_str()), Some(member.as_str()), StatusCode::OK),
        (Some(member.as_str()), None, StatusCode::FORBIDDEN),
        (None, Some(hub.device.as_str()), StatusCode::FORBIDDEN),
        (
            Some("not-a-token"),
            Some("nor-this"),
            StatusCode::UNAUTHORIZED,
        ),
    ] {
        assert_eq!(call(query, bearer).await, expected, "{query:?} {bearer:?}");
    }
}
