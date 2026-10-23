//! `GET /api/agents/{id}/settings` answers whoever can read the agent, with
//! install-wide rows for an admin and per-user rows for the caller alone.

use std::sync::Arc;
use workforce_hub::serve::Compose as _;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::Value;

use workforce_common::settings::{AgentConfig, WorkforceConfig};
use workforce_hub::gateway::router;
use workforce_hub::serve::AgentService;
use workforce_identity::types::{Identity, IdentityRole};

use crate::support::{send, token_for, Gateway, GATEWAY_TOKEN};

const INSTALL_KEY: &str = "install-llm-key-0f3a9c2e5b8d4f1a";

struct Fixture {
    gateway: Gateway,
    owner: Identity,
    outsider: Identity,
    _data: tempfile::TempDir,
}

/// A gateway serving `scout`, an agent `owner` owns and `outsider` cannot
/// read.
async fn fixture() -> Fixture {
    let data = tempfile::tempdir().unwrap();
    let owner = Identity::new("Owner", IdentityRole::Member);
    let outsider = Identity::new("Outsider", IdentityRole::Member);
    let mut config = WorkforceConfig::default();
    config.hub.data_dir = data.path().to_string_lossy().to_string();
    config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
        .join("memory.db")
        .to_string_lossy()
        .into_owned();
    config.llm.api_key = Some(INSTALL_KEY.into());
    config.agents.push(AgentConfig {
        id: "scout".into(),
        default: true,
        owner_id: Some(owner.id.to_string()),
        ..Default::default()
    });
    let service = AgentService::from_config(config, None)
        .await
        .expect("test service");
    let gateway = Gateway::new(vec![("scout", service)], None);
    let identities = gateway.state.identity_store.as_ref().unwrap();
    identities.create(&owner).unwrap();
    identities.create(&outsider).unwrap();
    Fixture {
        gateway,
        owner,
        outsider,
        _data: data,
    }
}

impl Fixture {
    async fn get(&self, path: &str, bearer: Option<&str>) -> (StatusCode, Value) {
        let request = bearer
            .into_iter()
            .fold(Request::builder().uri(path), |builder, bearer| {
                builder.header(header::AUTHORIZATION, format!("Bearer {bearer}"))
            })
            .body(Body::empty())
            .unwrap();
        let (status, _, text) = send(router(Arc::clone(&self.gateway.state)), request).await;
        assert!(!text.contains(INSTALL_KEY), "the install's key in {text}");
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }
}

fn sections(report: &Value) -> Vec<&str> {
    report["sections"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect()
}

fn has_row(report: &Value, section: &str, key: &str) -> bool {
    report["rows"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["section"] == section && row["key"] == key)
}

#[tokio::test]
async fn settings_route_answers_by_who_can_read_the_agent() {
    let fx = fixture().await;
    let path = "/api/agents/scout/settings";

    let (status, _) = fx.get(path, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = fx.get(path, Some(&token_for(&fx.outsider))).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "an agent the caller cannot read does not exist to them: {body}"
    );

    let (status, mine) = fx.get(path, Some(&token_for(&fx.owner))).await;
    assert_eq!(status, StatusCode::OK, "{mine}");
    assert_eq!(
        (&mine["agent"], &mine["viewer"]),
        (&"scout".into(), &"member".into())
    );
    assert!(sections(&mine).contains(&"llm") && !sections(&mine).contains(&"embeddings"));
    assert!(has_row(&mine, "budget", "you.daily_usd"));
    assert!(has_row(&mine, "llm", "api_key"));
    assert!(!has_row(&mine, "budget", "hub_daily_usd"));
    assert!(!has_row(&mine, "memory", "db_path"));

    let (status, admin) = fx.get(path, Some(&fx.gateway.admin_token())).await;
    assert_eq!(status, StatusCode::OK, "{admin}");
    assert_eq!(admin["viewer"], "admin");
    assert!(sections(&admin).contains(&"embeddings"));
    assert!(has_row(&admin, "budget", "you.daily_usd"));
    assert!(has_row(&admin, "budget", "hub_daily_usd"));

    let (status, owner_token) = fx.get(path, Some(GATEWAY_TOKEN)).await;
    assert_eq!(status, StatusCode::OK, "{owner_token}");
    assert_eq!(owner_token["viewer"], "admin");
    assert!(
        !has_row(&owner_token, "budget", "you.daily_usd"),
        "the owner token is nobody: it has no per-user rows"
    );
}

#[tokio::test]
async fn settings_route_narrows_to_a_section_or_a_key_and_names_what_it_cannot_find() {
    let fx = fixture().await;
    let member = token_for(&fx.owner);
    let get = |query: &'static str| {
        let (fx, member) = (&fx, member.clone());
        async move {
            fx.get(
                &format!("/api/agents/scout/settings?{query}"),
                Some(&member),
            )
            .await
        }
    };

    let (status, section) = get("section=know_how").await;
    assert_eq!(status, StatusCode::OK, "{section}");
    assert!(section["rows"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["section"] == "know_how"));

    let (status, explained) = get("key=llm.max_rounds").await;
    assert_eq!(status, StatusCode::OK, "{explained}");
    assert_eq!(explained["rows"].as_array().unwrap().len(), 1);
    assert_eq!(explained["rows"][0]["chain"][0]["source"], "default");

    let (status, hidden) = get("section=embeddings").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a member is not told an install-wide section exists: {hidden}"
    );
    let (status, unknown) = get("key=llm.max_round").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(unknown["error"], "no setting \"llm.max_round\" (did you mean llm.max_rounds?): a key is a config path such as llm.vision.model, and showing a section (config, llm, memory, compaction, advisor, know_how, tools, research, handoff, code_context, claim_grounding, self_learning, session_digest, worker_read_sources, attachments, decision, scheduler, evals, budget, credentials, slack) lists its keys");
    let (status, off) = get("key=know_how.max_entries").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        off["error"]
            .as_str()
            .unwrap()
            .contains("(did you mean know_how.state?)"),
        "a table that is off points at its state: {off}"
    );
}
