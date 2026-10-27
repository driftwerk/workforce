//! The models Desk's agent picker offers: the tiers of the providers a
//! session runs on locally, as the hub's tier registry names them.

use serde::Serialize;

/// The providers a Desk agent runs on here, in the order the picker shows
/// them.
pub const LOCAL_PROVIDERS: [&str; 2] = ["claude-code", "chatgpt"];

/// One model a provider's tier names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TierModel {
    pub tier: &'static str,
    pub model: String,
}

/// Where tier models come from. Implemented by the hub over its tier
/// registry.
pub trait Models: Send + Sync {
    /// `provider`'s models, most capable tier first, each once.
    fn tiers(&self, provider: &str) -> Vec<TierModel>;
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderModels {
    id: &'static str,
    models: Vec<TierModel>,
}

#[derive(Debug, Serialize)]
pub(crate) struct Catalog {
    providers: Vec<ProviderModels>,
}

impl Catalog {
    pub(crate) fn of(models: &dyn Models) -> Self {
        Self {
            providers: LOCAL_PROVIDERS
                .into_iter()
                .map(|id| ProviderModels {
                    id,
                    models: models.tiers(id),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use super::*;
    use crate::api::models_router;
    use crate::testing::Bearer;

    struct Tiers;

    impl Models for Tiers {
        fn tiers(&self, provider: &str) -> Vec<TierModel> {
            let named: &[(&'static str, &str)] = match provider {
                "claude-code" => &[
                    ("powerful", "opus"),
                    ("standard", "sonnet"),
                    ("cheap", "haiku"),
                ],
                "chatgpt" => &[("powerful", "gpt-6.1-sol"), ("cheap", "gpt-6-luna")],
                _ => &[("powerful", "not-offered")],
            };
            named
                .iter()
                .map(|&(tier, model)| TierModel {
                    tier,
                    model: model.into(),
                })
                .collect()
        }
    }

    async fn get(token: &str) -> (StatusCode, Value) {
        let request = Request::get("/api/desk/models")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = models_router(Arc::new(Bearer), Arc::new(Tiers))
            .oneshot(request)
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 1 << 16).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn lists_the_tier_models_of_the_local_providers() {
        let (status, body) = get("hub-token").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({ "providers": [
                { "id": "claude-code", "models": [
                    { "tier": "powerful", "model": "opus" },
                    { "tier": "standard", "model": "sonnet" },
                    { "tier": "cheap", "model": "haiku" },
                ]},
                { "id": "chatgpt", "models": [
                    { "tier": "powerful", "model": "gpt-6.1-sol" },
                    { "tier": "cheap", "model": "gpt-6-luna" },
                ]},
            ]})
        );
    }

    #[tokio::test]
    async fn needs_the_hub_token() {
        let (status, _) = get("wrong").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
