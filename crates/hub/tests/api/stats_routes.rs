//! The stats routes' billing split reads the cost ledger: every billable
//! call, not only an agent's task runs.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use workforce_common::persistence::{BillingSplit, CostRecord, CostScope, SqliteStore};
use workforce_common::subscription::{Billing, CredentialTier};
use workforce_common::telemetry::TelemetryStore;
use workforce_hub::agent_registry::AgentRegistry;
use workforce_hub::gateway::router;
use workforce_identity::sqlite::SqliteIdentityStore;
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityRole};

use crate::support::{agent_service, send, token_for, GatewayParts};

#[tokio::test]
async fn agent_stats_billing_is_the_ledger_split() {
    let data = tempfile::tempdir().unwrap();
    let service = agent_service(data.path()).await;
    let store = Arc::new(SqliteStore::in_memory().unwrap());
    for record in [
        CostRecord {
            agent_id: "scout".into(),
            model: "gpt-5.5".into(),
            cost_usd: 0.25,
            ..Default::default()
        },
        CostRecord {
            agent_id: "scout".into(),
            model: "whisper-1".into(),
            cost_usd: 0.125,
            description: Some("transcription".into()),
            ..Default::default()
        },
        CostRecord {
            agent_id: "scout".into(),
            model: "gpt-5.5".into(),
            cost_usd: 0.5,
            billing: Billing::Subscription,
            payer: Some("install".into()),
            credential_tier: Some(CredentialTier::SystemShared),
            ..Default::default()
        },
        CostRecord {
            agent_id: "other".into(),
            model: "gpt-5.5".into(),
            cost_usd: 2.0,
            ..Default::default()
        },
    ] {
        store.record_cost(&record).unwrap();
    }
    let identities = SqliteIdentityStore::in_memory().unwrap();
    let admin = Identity::new("Admin", IdentityRole::SystemAdmin);
    identities.create(&admin).unwrap();
    let state = GatewayParts {
        telemetry_store: Some(Arc::clone(&store) as Arc<dyn TelemetryStore>),
        ..GatewayParts::new(
            AgentRegistry::from_parts(
                [("scout".to_string(), service)].into_iter().collect(),
                "scout".into(),
            ),
            Arc::new(identities),
            std::path::PathBuf::from("/tmp/stats-routes-test-skills"),
        )
    }
    .state();

    let request = Request::get("/api/stats/agents/scout")
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", token_for(&admin)),
        )
        .body(Body::empty())
        .unwrap();
    let (status, _, text) = send(router(state), request).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let stats: serde_json::Value = serde_json::from_str(&text).unwrap();

    let now = chrono::Utc::now();
    let ledger = BillingSplit::of(
        &store
            .cost_split(
                now - chrono::Duration::days(30),
                now,
                CostScope {
                    agent_id: Some("scout"),
                    ..CostScope::default()
                },
            )
            .unwrap(),
    );
    assert_eq!(
        ledger,
        BillingSplit {
            metered_cost_usd: 0.375,
            subscription_cost_usd: 0.5,
            subscription_calls: 1,
        }
    );
    assert_eq!(
        stats["totals"]["billing"],
        serde_json::to_value(ledger).unwrap()
    );
    assert_eq!(stats["by_payer"][0]["plan"], "Install login");
}

#[tokio::test]
async fn cost_detail_names_the_plan_of_subscription_rows_only() {
    let data = tempfile::tempdir().unwrap();
    let state = crate::support::hub_state(data.path());
    for record in [
        CostRecord {
            agent_id: "scout".into(),
            model: "gpt-5.5".into(),
            cost_usd: 0.25,
            ..Default::default()
        },
        CostRecord {
            agent_id: "scout".into(),
            model: "gpt-5.5".into(),
            cost_usd: 0.5,
            billing: Billing::Subscription,
            payer: Some("alice".into()),
            credential_tier: Some(CredentialTier::AgentUser),
            ..Default::default()
        },
    ] {
        state.telemetry_store.record_cost(&record).unwrap();
    }
    let app = axum::Router::new()
        .nest("/api", workforce_hub::api::router(Arc::new(state)))
        .layer(axum::Extension(workforce_hub::gateway::CallerContext {
            is_admin: true,
            ..Default::default()
        }));

    let request = Request::get("/api/cost/detail")
        .body(Body::empty())
        .unwrap();
    let (status, _, text) = send(app, request).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let detail: serde_json::Value = serde_json::from_str(&text).unwrap();

    let plan_of = |billing: &str| {
        detail["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["billing"] == billing)
            .map(|row| row["plan"].clone())
            .unwrap()
    };
    assert_eq!(plan_of("metered"), serde_json::Value::Null);
    assert_eq!(plan_of("subscription"), "alice's plan (this agent)");
}
