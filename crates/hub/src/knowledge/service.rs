//! Hub-side wiring for [`workforce_knowledge_service::KnowledgeService`].
//!
//! The core service lives in `workforce-knowledge-service` so it can be
//! reused by the local MCP server without dragging in the full agent /
//! LLM stack. This module supplies the LLM-backed summariser that the
//! service needs when `generate_summaries` is enabled.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use tracing::info;

use workforce_agent::llm::ollama::OllamaClient;
use workforce_agent::llm::provider::{create_provider, LlmProvider, ProviderKind};
use workforce_agent::llm::tiers::TierRegistry;
use workforce_agent::llm::types::{ContentBlock, LlmMessage, Role};
use workforce_agent::runner::with_served_log;
use workforce_codebase_knowledge::indexer::summary_generator::SummaryLlm;
use workforce_common::cost::{CostEntry, CostTracker, TokenCounts};
use workforce_common::secret::KeySlot;
use workforce_common::settings::KnowledgeSummariserConfig;
use workforce_common::subscription::Origin;
use workforce_knowledge_service::SummariserBuilder;

use crate::credentials::{Legacy, SUMMARISER_API_KEY};
use crate::keyed_provider::KeyedProvider;
use crate::subscription::PlanLogins;
use crate::vault::Vault;

/// Adapts a `workforce-agent` `LlmProvider` to the
/// [`SummaryLlm`] trait consumed by the indexing pipeline.
struct LlmProviderSummaryAdapter {
    provider: Arc<dyn LlmProvider>,
    model: String,
    cost_tracker: Option<Arc<CostTracker>>,
    agent_id: Option<String>,
}

#[async_trait]
impl SummaryLlm for LlmProviderSummaryAdapter {
    async fn generate_text(&self, prompt: &str, max_tokens: usize) -> Result<String> {
        let messages = vec![LlmMessage {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: prompt.to_string(),
            }],
        }];
        let (response, served) = with_served_log(self.provider.create_message(
            None,
            &messages,
            &[],
            Some(&self.model),
            Some(max_tokens as u32),
            false,
        ))
        .await;
        let response = response?;

        if let Some(ref ct) = self.cost_tracker {
            let identity = crate::user_config::current_identity_id();
            ct.record(CostEntry {
                task_type: Some("knowledge_summary"),
                identity_id: identity.as_deref(),
                description: Some("knowledge:summary"),
                served,
                ..CostEntry::new(
                    self.agent_id.as_deref().unwrap_or("knowledge"),
                    &self.model,
                    TokenCounts::from(&response.usage),
                    Origin::Knowledge,
                )
            });
        }

        response
            .text()
            .ok_or_else(|| eyre::eyre!("No text in summariser LLM response"))
    }

    fn model_name(&self) -> &str {
        &self.model
    }
}

/// `SummariserBuilder` impl that resolves `KnowledgeSummariserConfig` (including
/// `provider:tier` shorthand) into a concrete LLM-backed summariser. A
/// provider that takes an API key reads it from the global vault on every
/// summary ([`SUMMARISER_API_KEY`]), then from the install's config and
/// environment, so a rotated key needs no restart.
pub struct HubSummariserBuilder {
    config: KnowledgeSummariserConfig,
    cost_tracker: Option<Arc<CostTracker>>,
    agent_id: Option<String>,
    plans: Option<PlanLogins>,
    global_vault: Option<Arc<Vault>>,
}

impl HubSummariserBuilder {
    pub fn new(config: KnowledgeSummariserConfig) -> Self {
        Self {
            config,
            cost_tracker: None,
            agent_id: None,
            plans: None,
            global_vault: None,
        }
    }

    /// The hub's vault, where the summariser's key is read.
    pub fn with_global_vault(mut self, global_vault: Option<Arc<Vault>>) -> Self {
        self.global_vault = global_vault;
        self
    }

    /// The hub's own logins, for a subscription summariser, when the hub
    /// has them.
    pub fn with_plans(mut self, plans: Option<PlanLogins>) -> Self {
        self.plans = plans;
        self
    }

    /// Attach a cost tracker so knowledge summarisation calls are recorded.
    pub fn with_cost_tracker(mut self, cost_tracker: Arc<CostTracker>, agent_id: String) -> Self {
        self.cost_tracker = Some(cost_tracker);
        self.agent_id = Some(agent_id);
        self
    }

    /// Convenience: wrap `self` in an `Arc<dyn SummariserBuilder>`.
    pub fn into_arc(self) -> Arc<dyn SummariserBuilder> {
        Arc::new(self)
    }
}

/// Resolve the summariser's `(provider, model)` from its config.
///
/// Handles `provider:tier` shorthand (`"gemini:cheap"`,
/// `"openrouter:deepseek:powerful"`), bare tier names resolved against the
/// `provider` field (`provider = "gemini"`, `model = "cheap"`), and concrete
/// model names passed through unchanged.
fn resolve_provider_and_model(config: &KnowledgeSummariserConfig) -> (ProviderKind, String) {
    let model_spec = &config.model;
    if model_spec.contains(':') {
        let tiers = TierRegistry::from_defaults().unwrap_or_else(|_| TierRegistry::new(None));
        let (routing_key, model) = tiers.resolve_routed_model(model_spec, "anthropic");
        if let Some(ref rk) = routing_key {
            let provider_kind = ProviderKind::from_str_loose(rk).unwrap_or(ProviderKind::Ollama);
            (provider_kind, model)
        } else if model != *model_spec {
            // Resolved via default provider (e.g. "anthropic:powerful")
            let provider_kind =
                ProviderKind::from_str_loose(&config.provider).unwrap_or(ProviderKind::Anthropic);
            (provider_kind, model)
        } else {
            // Didn't resolve as tier shorthand — fall back to explicit provider field
            let kind =
                ProviderKind::from_str_loose(&config.provider).unwrap_or(ProviderKind::Ollama);
            (kind, model_spec.clone())
        }
    } else {
        let kind = ProviderKind::from_str_loose(&config.provider).unwrap_or(ProviderKind::Ollama);
        // A bare tier name ("cheap") resolves against the provider's tier
        // table; anything else passes through as a concrete model name.
        let tiers = TierRegistry::from_defaults().unwrap_or_else(|_| TierRegistry::new(None));
        (kind, tiers.resolve_model(model_spec, &config.provider))
    }
}

/// Whether the summariser `config` names authenticates with an API key, and
/// so reads [`SUMMARISER_API_KEY`].
pub(crate) fn summariser_takes_key(config: &KnowledgeSummariserConfig) -> bool {
    resolve_provider_and_model(config).0.takes_api_key()
}

/// What the summariser read its key from before the vault credential:
/// `[knowledge.summariser] api_key` (which the hub fills from
/// `SUMMARISER_API_KEY` at boot), then the provider's environment variable.
pub(crate) fn summariser_legacy(config: &KnowledgeSummariserConfig) -> Legacy {
    let (kind, _) = resolve_provider_and_model(config);
    Legacy::default()
        .config("[knowledge.summariser] api_key", config.api_key.clone())
        .env(crate::credentials::provider_env(kind))
}

/// The summariser's key: the global vault's on every summary, then `legacy`.
fn summariser_key(global_vault: Option<Arc<Vault>>, legacy: Legacy) -> KeySlot {
    KeySlot::new(
        crate::credentials::source_then(&SUMMARISER_API_KEY, None, global_vault, move || {
            legacy.key()
        }),
        crate::credentials::unavailable("Knowledge summarisation", &SUMMARISER_API_KEY),
    )
}

impl SummariserBuilder for HubSummariserBuilder {
    fn build(&self) -> Result<Box<dyn SummaryLlm>> {
        let model_spec = &self.config.model;
        let (kind, resolved_model) = resolve_provider_and_model(&self.config);

        info!(
            model_spec = %model_spec,
            resolved_model = %resolved_model,
            provider = ?kind,
            "Knowledge summariser configured"
        );

        let legacy = summariser_legacy(&self.config);
        let provider: Arc<dyn LlmProvider> = match (kind, &self.plans) {
            (kind, Some(plans)) if kind.is_subscription() => {
                plans.provider(kind, Some(&resolved_model), None)?
            }
            (ProviderKind::Ollama, _) => Arc::new(
                OllamaClient::new(&self.config.url)
                    .with_api_key(legacy.key().unwrap_or_default())
                    .with_model(&resolved_model),
            ),
            (kind, _) if kind.takes_api_key() => {
                let key = summariser_key(self.global_vault.clone(), legacy);
                let model = resolved_model.clone();
                Arc::new(KeyedProvider::new(key, move |key| {
                    create_provider(kind, key, Some(&model), None, false).map(Arc::from)
                }))
            }
            _ => create_provider(
                kind,
                &legacy.key().unwrap_or_default(),
                Some(&resolved_model),
                None,
                false,
            )?
            .into(),
        };

        Ok(Box::new(LlmProviderSummaryAdapter {
            provider,
            model: resolved_model,
            cost_tracker: self.cost_tracker.clone(),
            agent_id: self.agent_id.clone(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(provider: &str, model: &str) -> KnowledgeSummariserConfig {
        KnowledgeSummariserConfig {
            provider: provider.to_string(),
            model: model.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn bare_tier_resolves_via_provider_field() {
        // The Penny misconfig shape: provider = "gemini", model = "cheap"
        // used to send the literal string "cheap" to Gemini.
        let (kind, model) = resolve_provider_and_model(&cfg("gemini", "cheap"));
        assert_eq!(kind, ProviderKind::Gemini);
        assert_ne!(model, "cheap", "bare tier must resolve to a real model");
        assert!(model.starts_with("gemini-"), "got {model}");
    }

    #[test]
    fn provider_tier_shorthand_resolves() {
        let (kind, model) = resolve_provider_and_model(&cfg("ollama", "gemini:cheap"));
        assert_eq!(kind, ProviderKind::Gemini, "shorthand overrides provider");
        assert!(model.starts_with("gemini-"), "got {model}");
    }

    #[test]
    fn concrete_model_passes_through() {
        let (kind, model) = resolve_provider_and_model(&cfg("gemini", "gemini-2.5-pro"));
        assert_eq!(kind, ProviderKind::Gemini);
        assert_eq!(model, "gemini-2.5-pro");
    }

    #[test]
    fn colon_model_that_is_no_tier_stays_literal() {
        // Default Ollama model contains a colon but is not tier shorthand.
        let (kind, model) = resolve_provider_and_model(&cfg("ollama", "qwen2.5-coder:7b"));
        assert_eq!(kind, ProviderKind::Ollama);
        assert_eq!(model, "qwen2.5-coder:7b");
    }

    #[test]
    fn the_summariser_key_is_the_global_vaults_on_every_summary_then_the_installs() {
        use workforce_agent::runner::CURRENT_USER_ID;
        let dir = tempfile::tempdir().unwrap();
        let global = Arc::new(Vault::open(&dir.path().join("g.enc"), "test-key").unwrap());
        let config = KnowledgeSummariserConfig {
            api_key: Some("from-config".into()),
            ..cfg("openrouter", "deepseek/deepseek-chat")
        };
        let key = summariser_key(Some(Arc::clone(&global)), summariser_legacy(&config));
        assert_eq!(key.get().as_deref(), Ok("from-config"));

        global
            .set("users/alice/knowledge/summariser/api_key", "alices")
            .unwrap();
        assert_eq!(
            CURRENT_USER_ID
                .sync_scope("alice".into(), || key.get())
                .as_deref(),
            Ok("from-config"),
            "hub-wide system work reads no person's key"
        );
        global.set(SUMMARISER_API_KEY.path, "vault-1").unwrap();
        assert_eq!(key.get().as_deref(), Ok("vault-1"));
        global.set(SUMMARISER_API_KEY.path, "vault-2").unwrap();
        assert_eq!(
            key.get().as_deref(),
            Ok("vault-2"),
            "rotated with no restart"
        );

        let none = KeySlot::new(
            crate::credentials::source_then(&SUMMARISER_API_KEY, None, None, || None),
            crate::credentials::unavailable("Knowledge summarisation", &SUMMARISER_API_KEY),
        );
        assert_eq!(
            none.get().unwrap_err().to_string(),
            "Knowledge summarisation is not available: no API key for it is set. An admin sets it \
             in the global vault at knowledge/summariser/api_key. Do not ask for the key in \
             chat or try to store it yourself."
        );
    }

    #[test]
    fn only_a_bearer_key_provider_reads_a_provider_variable() {
        for kind in [
            ProviderKind::Ollama,
            ProviderKind::Vllm,
            ProviderKind::Bedrock,
            ProviderKind::VertexAnthropic,
        ] {
            assert!(!kind.takes_api_key());
            assert!(crate::credentials::provider_env(kind).is_empty());
        }
        for kind in [
            ProviderKind::Anthropic,
            ProviderKind::OpenRouter,
            ProviderKind::Gemini,
            ProviderKind::Grok,
            ProviderKind::Cerebras,
            ProviderKind::Zai,
        ] {
            assert!(kind.takes_api_key());
            assert!(!crate::credentials::provider_env(kind).is_empty());
        }
    }
}
