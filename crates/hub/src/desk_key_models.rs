//! The models a Desk key provider lists for the person's own account now, so
//! a model released after Desk was shows up beside Desk's table:
//!
//! - an Anthropic key: `GET /v1/models` on that key;
//! - Amazon Bedrock: the hub's Bedrock catalogue (`ListFoundationModels` and
//!   `ListInferenceProfiles`) on the account's own keys or AWS profile.
//!
//! Only Claude models are taken. A key's list is kept like a plan's
//! ([`PlanCatalog`]); a read that fails leaves Desk's own table standing.

use std::collections::{BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use eyre::{eyre, Result};
use workforce_agent::llm::bedrock::resolve_region;
use workforce_agent::llm::bedrock_capabilities::strip_region_prefix;
use workforce_agent::llm::bedrock_catalog::{BedrockCatalogStore, BedrockModel, CatalogState};
use workforce_agent::llm::bedrock_credentials::CredentialResolver;

use crate::subscription::catalog::{PlanCatalog, PlanModel};

/// The service name an Anthropic key's list is kept under.
pub const ANTHROPIC: &str = "anthropic";

const ANTHROPIC_BASE: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Pages of 1000 read before giving up on the rest.
const MAX_PAGES: usize = 3;

/// What a model Desk had no words for is described as.
const ANTHROPIC_LINE: &str = "Listed by your Anthropic account";
const BEDROCK_LINE: &str = "Listed by your AWS account";

/// A name for a key in the in-memory cache, never the key: a hash is enough
/// to tell two accounts apart and cannot be turned back into a key.
pub fn fingerprint(secret: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    secret.hash(&mut hasher);
    format!("key:{:016x}", hasher.finish())
}

fn plan_model(id: String, name: Option<String>, description: &str) -> PlanModel {
    PlanModel {
        name: name
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| id.clone()),
        id,
        resolves_to: None,
        description: description.to_string(),
        efforts: Vec::new(),
        default_effort: None,
        context_window: None,
        fast: None,
    }
}

/// The Claude models in one page of `GET /v1/models`, and the id to read the
/// next page after, when there is one.
pub fn parse_anthropic(body: &serde_json::Value) -> (Vec<PlanModel>, Option<String>) {
    let models = body["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m["id"].as_str()?;
            id.starts_with("claude-").then(|| {
                plan_model(
                    id.to_string(),
                    m["display_name"].as_str().map(str::to_string),
                    ANTHROPIC_LINE,
                )
            })
        })
        .collect();
    let next = body["has_more"]
        .as_bool()
        .unwrap_or(false)
        .then(|| body["last_id"].as_str().map(str::to_string))
        .flatten();
    (models, next)
}

/// The Claude models `key` lists at `base` (Anthropic's API; another only in
/// tests).
pub async fn read_anthropic(base: &str, key: &str) -> Result<Vec<PlanModel>> {
    let client = reqwest::Client::builder()
        .timeout(READ_TIMEOUT)
        .build()
        .map_err(|e| eyre!("building the client: {e}"))?;
    let mut models = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut request = client
            .get(format!("{}/v1/models", base.trim_end_matches('/')))
            .query(&[("limit", "1000")])
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("x-api-key", key);
        if let Some(after) = &after {
            request = request.query(&[("after_id", after.as_str())]);
        }
        let response = request
            .send()
            .await
            .map_err(|e| eyre!("the Anthropic models list couldn't be reached: {e}"))?;
        let status = response.status();
        if !status.is_success() {
            // The body can echo what was sent: only the status is kept.
            return Err(eyre!("Anthropic answered the models list with {status}"));
        }
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| eyre!("the Anthropic models list isn't JSON: {e}"))?;
        let (page, next) = parse_anthropic(&body);
        models.extend(page);
        match next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    Ok(models)
}

/// The Claude models of a Bedrock catalogue by the id Desk offers them under:
/// the `anthropic.claude-…` id (a version suffix such as `:0` is part of it), however it is listed (a foundation model
/// or a `us.`/`eu.`/`global.` inference profile, which are one model).
pub fn bedrock_claude(models: &[BedrockModel]) -> Vec<PlanModel> {
    let mut seen = BTreeSet::new();
    models
        .iter()
        .filter_map(|m| {
            let id = strip_region_prefix(&m.id);
            (id.starts_with("anthropic.claude-") && seen.insert(id.to_string())).then(|| {
                plan_model(
                    id.to_string(),
                    m.name
                        .as_deref()
                        .map(|n| n.trim_start_matches("Anthropic ").to_string()),
                    BEDROCK_LINE,
                )
            })
        })
        .collect()
}

/// The Claude models the Anthropic key `key` lists now: kept like a plan's
/// list, so a refused or offline read leaves Desk's own table standing.
pub async fn anthropic_listed(key: &str) -> Option<Arc<Vec<PlanModel>>> {
    PlanCatalog::global()
        .models_of(ANTHROPIC, &fingerprint(key), || {
            read_anthropic(ANTHROPIC_BASE, key)
        })
        .await
}

/// One catalogue per AWS identity, so its hour of memory is kept between
/// Desk's refreshes.
static BEDROCK_STORES: LazyLock<parking_lot::Mutex<HashMap<String, Arc<BedrockCatalogStore>>>> =
    LazyLock::new(Default::default);

/// The Claude models a Bedrock account lists in its region: its keys
/// (`api_key`, packed as the vault keeps them) or its AWS `profile`. None
/// when AWS couldn't be asked (no permission, signed out, offline).
pub async fn bedrock_listed(
    api_key: &str,
    profile: Option<&str>,
    region: Option<&str>,
) -> Option<Vec<PlanModel>> {
    let region = resolve_region(region);
    let identity = fingerprint(&format!("{api_key}\u{0}{}", profile.unwrap_or_default()));
    let store = Arc::clone(BEDROCK_STORES.lock().entry(identity).or_insert_with(|| {
        BedrockCatalogStore::new(match profile {
            Some(profile) => CredentialResolver::from_profile(profile),
            None => CredentialResolver::from_packed(api_key),
        })
    }));
    match store.get(&region).await {
        CatalogState::Ready(catalog) => {
            let claude = bedrock_claude(catalog.models());
            (!claude.is_empty()).then_some(claude)
        }
        CatalogState::Unavailable { reason, .. } => {
            tracing::warn!(%reason, "the Bedrock account's models couldn't be listed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use workforce_agent::llm::bedrock_catalog::ModelKind;

    fn bedrock(id: &str, name: &str, kind: ModelKind) -> BedrockModel {
        BedrockModel {
            id: id.into(),
            name: Some(name.into()),
            kind,
        }
    }

    #[test]
    fn an_anthropic_page_keeps_claude_models_and_says_where_the_next_begins() {
        let (models, next) = parse_anthropic(&json!({
            "data": [
                {"type": "model", "id": "claude-fable-5-2", "display_name": "Claude Fable 5.2"},
                {"type": "model", "id": "claude-opus-5-5"},
                {"type": "model", "id": "embed-1"},
                {"type": "model"}
            ],
            "has_more": true,
            "last_id": "claude-opus-5-5"
        }));
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["claude-fable-5-2", "claude-opus-5-5"]
        );
        assert_eq!(models[0].name, "Claude Fable 5.2");
        assert_eq!(models[1].name, "claude-opus-5-5", "no name: the id");
        assert_eq!(next.as_deref(), Some("claude-opus-5-5"));
        let (_, last) = parse_anthropic(&json!({"data": [], "has_more": false}));
        assert_eq!(last, None);
    }

    #[test]
    fn bedrock_profiles_and_foundation_models_are_one_bare_claude_model() {
        let listed = bedrock_claude(&[
            bedrock(
                "global.anthropic.claude-fable-5-2",
                "Anthropic Claude Fable 5.2",
                ModelKind::InferenceProfile,
            ),
            bedrock(
                "us.anthropic.claude-fable-5-2",
                "Anthropic Claude Fable 5.2",
                ModelKind::InferenceProfile,
            ),
            bedrock(
                "anthropic.claude-fable-5-2",
                "Claude Fable 5.2",
                ModelKind::FoundationModel,
            ),
            bedrock(
                "anthropic.claude-opus-5-5-v1:0",
                "Claude Opus 5.5",
                ModelKind::FoundationModel,
            ),
            bedrock(
                "amazon.titan-embed-text-v2:0",
                "Titan",
                ModelKind::FoundationModel,
            ),
            bedrock("deepseek.v3.2", "DeepSeek", ModelKind::FoundationModel),
        ]);
        let ids: Vec<&str> = listed.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "anthropic.claude-fable-5-2",
                "anthropic.claude-opus-5-5-v1:0"
            ]
        );
        // The family a model is routed by is the first word of its id, never
        // a region the profile was listed under.
        assert!(ids.iter().all(|id| id.starts_with("anthropic.")));
        assert_eq!(listed[0].name, "Claude Fable 5.2");
    }

    #[test]
    fn a_key_is_named_in_the_cache_by_a_hash_that_shows_none_of_it() {
        let a = fingerprint("sk-ant-secret-one");
        assert_eq!(a, fingerprint("sk-ant-secret-one"));
        assert_ne!(a, fingerprint("sk-ant-secret-two"));
        assert!(!a.contains("sk-ant") && !a.contains("secret"));
    }

    #[tokio::test]
    async fn a_key_is_sent_as_the_key_header_and_a_refusal_keeps_only_its_status() {
        use axum::{http::HeaderMap, routing::get, Json, Router};
        let app = Router::new().route(
            "/v1/models",
            get(|headers: HeaderMap| async move {
                if headers.get("x-api-key").and_then(|v| v.to_str().ok()) == Some("good") {
                    Ok(Json(json!({"data": [
                        {"id": "claude-fable-5-2", "display_name": "Claude Fable 5.2"}
                    ], "has_more": false})))
                } else {
                    Err((
                        axum::http::StatusCode::UNAUTHORIZED,
                        "bad key: nope".to_string(),
                    ))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let models = read_anthropic(&base, "good").await.unwrap();
        assert_eq!(models[0].id, "claude-fable-5-2");
        let refused = read_anthropic(&base, "wrong-key-value").await.unwrap_err();
        let text = format!("{refused:#}");
        assert!(text.contains("401"), "{text}");
        assert!(
            !text.contains("wrong-key-value") && !text.contains("nope"),
            "{text}"
        );
    }
}
