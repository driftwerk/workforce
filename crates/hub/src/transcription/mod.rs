//! Hub-side wiring for transcription provider construction.
//!
//! Mirrors [`crate::vision::HubVisionBuilder`]: takes an
//! [`LlmTranscriptionConfig`], resolves tier shorthand against the
//! transcription tier registry and constructs the provider client, wrapped in
//! a [`CachingTranscriber`]. The client reads its API key on every
//! transcription, for the person it is for.
//!
//! Built once per agent at startup and stored on `AgentService` as
//! `Arc<dyn TranscriptionProvider>` so the adapter's LRU cache spans turns.

use std::sync::Arc;

use eyre::{eyre, Result};
use tracing::info;

use workforce_agent::llm::chat_transcription::ChatTranscriptionClient;
use workforce_agent::llm::transcription_tiers::{TranscriptionTierRegistry, CUSTOM_PROVIDER};
use workforce_agent::llm::whisper::WhisperClient;
use workforce_agent::transcription::{
    CachingTranscriber, ChatTranscriptionProvider, TranscriptionProvider,
    TranscriptionUsageObserver, WhisperTranscriptionProvider,
};
use workforce_common::cost::{CostEntry, CostTracker, TokenCounts};
use workforce_common::secret::KeySlot;
use workforce_common::settings::LlmTranscriptionConfig;
use workforce_common::subscription::Origin;

use crate::credentials::{Legacy, Modality};
use crate::vault::Vault;

/// Builds a [`TranscriptionProvider`] from an [`LlmTranscriptionConfig`].
/// When a [`CostTracker`] and `agent_id` are supplied, every uncached
/// transcription's audio duration is recorded to that agent's session ledger
/// so the call cost shows up in cost analytics.
pub struct HubTranscriptionBuilder {
    config: LlmTranscriptionConfig,
    tiers: Arc<TranscriptionTierRegistry>,
    cost_tracker: Option<Arc<CostTracker>>,
    agent_id: Option<String>,
    agent_vault: Option<Arc<Vault>>,
    global_vault: Option<Arc<Vault>>,
}

impl HubTranscriptionBuilder {
    pub fn new(config: LlmTranscriptionConfig, tiers: Arc<TranscriptionTierRegistry>) -> Self {
        Self {
            config,
            tiers,
            cost_tracker: None,
            agent_id: None,
            agent_vault: None,
            global_vault: None,
        }
    }

    /// The vaults the provider's API key is read from, per transcription.
    pub fn with_vaults(
        mut self,
        agent_vault: Option<Arc<Vault>>,
        global_vault: Option<Arc<Vault>>,
    ) -> Self {
        self.agent_vault = agent_vault;
        self.global_vault = global_vault;
        self
    }

    pub fn with_cost_attribution(
        mut self,
        cost_tracker: Arc<CostTracker>,
        agent_id: String,
    ) -> Self {
        self.cost_tracker = Some(cost_tracker);
        self.agent_id = Some(agent_id);
        self
    }

    pub fn build(self) -> Result<Arc<dyn TranscriptionProvider>> {
        let (backend, provider_name, resolved_model) = route(&self.config, &self.tiers)?;

        info!(
            model_spec = %self.config.model,
            resolved_model = %resolved_model,
            backend = ?backend,
            cache_size = self.config.cache_size,
            "Transcription provider configured"
        );

        let key = crate::credentials::slot(
            Modality::Transcription,
            &provider_name,
            self.agent_vault.clone(),
            self.global_vault.clone(),
            legacy(
                &self.config,
                backend,
                &provider_name,
                self.agent_vault.clone(),
                self.global_vault.clone(),
            ),
        );
        let inner = backend.build_provider(&self.config, &resolved_model, key.clone());
        let adapter = CachingTranscriber::new(inner, self.config.cache_size);
        let adapter = match backend.is_hosted() {
            true => adapter.with_payer(key),
            false => adapter,
        };
        let adapter = match self.cost_observer() {
            Some(observer) => adapter.with_usage_observer(observer),
            None => adapter,
        };
        Ok(Arc::new(adapter))
    }

    fn cost_observer(&self) -> Option<TranscriptionUsageObserver> {
        let tracker = self.cost_tracker.clone()?;
        let agent_id = self.agent_id.clone()?;
        Some(Arc::new(move |model, duration_seconds| {
            // Convention (see default_transcription_pricing.toml header):
            // 1 fake "input token" = 1 millisecond of audio. CostTracker
            // multiplies tokens × $/M-tokens, which then equals $/min × min
            // of audio at the rates we register for Whisper-style models.
            let pseudo_input_tokens = (duration_seconds * 1000.0).round().max(0.0) as u64;
            let description = format!("transcription:{}s", duration_seconds.round() as u64);
            let identity = crate::user_config::current_identity_id();
            tracker.record(CostEntry {
                task_type: Some("transcription"),
                identity_id: identity.as_deref(),
                description: Some(&description),
                ..CostEntry::new(
                    &agent_id,
                    model,
                    TokenCounts {
                        input: pseudo_input_tokens,
                        ..TokenCounts::default()
                    },
                    Origin::Transcription,
                )
            });
        }))
    }
}

/// What `[llm.transcription]` read its key from before the vault credential:
/// `api_key`, `TRANSCRIPTION_API_KEY`, the provider's environment variable,
/// the provider's LLM account. The account stays last: an install that
/// transcribes on an environment key may hold an unrelated chat account for
/// the same provider.
pub(crate) fn legacy(
    config: &LlmTranscriptionConfig,
    backend: TranscriptionBackend,
    provider_name: &str,
    agent_vault: Option<Arc<Vault>>,
    global_vault: Option<Arc<Vault>>,
) -> Legacy {
    Legacy::default()
        .config("[llm.transcription] api_key", config.api_key.clone())
        .env(&["TRANSCRIPTION_API_KEY"])
        .env(backend.env())
        .account(agent_vault, global_vault, provider_name)
}

/// The backend, provider name and model `[llm.transcription]` names. A route
/// that cannot transcribe is an error naming it: no provider stands in for
/// another.
pub(crate) fn route(
    config: &LlmTranscriptionConfig,
    tiers: &TranscriptionTierRegistry,
) -> Result<(TranscriptionBackend, String, String)> {
    if config.model.is_empty() {
        return Err(eyre!(
            "[llm.transcription].model is empty — set a tier shorthand like 'openrouter:google:cheap'"
        ));
    }
    let (provider_name, model) = tiers
        .resolve_routed_transcription_model(&config.model, config.provider.as_deref())
        .map_err(|e| eyre!("[llm.transcription] model {e}"))?;
    let backend = TranscriptionBackend::resolve(&provider_name, &config.model, config)?;
    Ok((backend, provider_name, model))
}

/// Which backend shape a resolved provider name implies.
#[derive(Debug, Clone, Copy)]
pub(crate) enum TranscriptionBackend {
    /// Multipart upload to a `/v1/audio/transcriptions` endpoint.
    Whisper(WhisperHost),
    /// Audio sent as `input_audio` content part to a chat endpoint.
    Chat(ChatHost),
    /// User-supplied custom URL (Whisper-shaped).
    CustomWhisper,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum WhisperHost {
    OpenAi,
    Groq,
    Vllm,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ChatHost {
    Gemini,
    OpenRouter,
}

impl TranscriptionBackend {
    fn resolve(
        provider_name: &str,
        model_spec: &str,
        config: &LlmTranscriptionConfig,
    ) -> Result<Self> {
        match provider_name {
            "openai" => Ok(Self::Whisper(WhisperHost::OpenAi)),
            "groq" => Ok(Self::Whisper(WhisperHost::Groq)),
            "gemini" => Ok(Self::Chat(ChatHost::Gemini)),
            "openrouter" => Ok(Self::Chat(ChatHost::OpenRouter)),
            "vllm" => Ok(Self::Whisper(WhisperHost::Vllm)),
            CUSTOM_PROVIDER => {
                if config.url.is_some() {
                    Ok(Self::CustomWhisper)
                } else {
                    Err(eyre!(
                        "transcription provider 'custom' requires [llm.transcription].url"
                    ))
                }
            }
            other => Err(eyre!(
                "[llm.transcription] model '{model_spec}' is routed to '{other}', which does not \
                 transcribe; use openai, groq, gemini, openrouter, vllm or custom"
            )),
        }
    }

    fn env(self) -> &'static [&'static str] {
        match self {
            Self::Whisper(WhisperHost::OpenAi) => &["OPENAI_API_KEY"],
            Self::Whisper(WhisperHost::Groq) => &["GROQ_API_KEY"],
            Self::Whisper(WhisperHost::Vllm) | Self::CustomWhisper => &["VLLM_API_KEY"],
            Self::Chat(ChatHost::Gemini) => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            Self::Chat(ChatHost::OpenRouter) => &["OPENROUTER_API_KEY"],
        }
    }

    /// Whether the endpoint is a provider's own, which takes no request
    /// without a key; a self-hosted one may be open.
    fn is_hosted(self) -> bool {
        !matches!(self, Self::Whisper(WhisperHost::Vllm) | Self::CustomWhisper)
    }

    fn build_provider(
        self,
        config: &LlmTranscriptionConfig,
        resolved_model: &str,
        api_key: KeySlot,
    ) -> Box<dyn TranscriptionProvider> {
        let model = resolved_model.to_string();
        match self {
            Self::Whisper(host) => {
                let client = match host {
                    WhisperHost::OpenAi => WhisperClient::openai(api_key),
                    WhisperHost::Groq => WhisperClient::groq(api_key),
                    WhisperHost::Vllm => WhisperClient::new(
                        api_key,
                        config.url.as_deref().unwrap_or("http://localhost:8000/v1"),
                    ),
                };
                Box::new(WhisperTranscriptionProvider::new(client, model))
            }
            Self::CustomWhisper => {
                let url = config
                    .url
                    .as_deref()
                    .expect("validated in TranscriptionBackend::resolve");
                Box::new(WhisperTranscriptionProvider::new(
                    WhisperClient::new(api_key, url),
                    model,
                ))
            }
            Self::Chat(host) => {
                let client = match host {
                    ChatHost::Gemini => ChatTranscriptionClient::gemini(api_key),
                    ChatHost::OpenRouter => ChatTranscriptionClient::openrouter(api_key),
                };
                Box::new(ChatTranscriptionProvider::new(client, model))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcription(model: &str) -> LlmTranscriptionConfig {
        toml::from_str(&format!("model = {model:?}")).unwrap()
    }

    fn routed(config: &LlmTranscriptionConfig) -> Result<(String, String)> {
        route(config, &TranscriptionTierRegistry::from_defaults().unwrap())
            .map(|(_, provider, model)| (provider, model))
    }

    #[test]
    fn a_route_keeps_its_own_provider() {
        let routes = [
            (
                "openrouter:google:cheap",
                "openrouter",
                "google/gemini-3.5-flash-lite",
            ),
            ("groq:cheap", "groq", "distil-whisper-large-v3-en"),
            ("groq:whisper-large-v3", "groq", "whisper-large-v3"),
            ("openai:whisper-1", "openai", "whisper-1"),
            (
                "gemini:gemini-3-flash-preview",
                "gemini",
                "gemini-3-flash-preview",
            ),
            ("vllm:openai/whisper-base", "vllm", "openai/whisper-base"),
        ];
        for (spec, provider, model) in routes {
            assert_eq!(
                routed(&transcription(spec)).unwrap(),
                (provider.to_string(), model.to_string()),
                "{spec}"
            );
        }
        let bare = LlmTranscriptionConfig {
            provider: Some("groq".into()),
            ..transcription("whisper-large-v3")
        };
        assert_eq!(
            routed(&bare).unwrap(),
            ("groq".to_string(), "whisper-large-v3".to_string())
        );
    }

    #[test]
    fn a_stale_account_for_the_provider_stays_behind_the_installs_own_key() {
        let dir = tempfile::tempdir().unwrap();
        let global = Arc::new(Vault::open(&dir.path().join("g.enc"), "test-key").unwrap());
        global
            .set(
                "llm/accounts/chat",
                r#"{"provider":"groq","api_key":"stale-chat-account"}"#,
            )
            .unwrap();
        let config = LlmTranscriptionConfig {
            api_key: Some("configured".into()),
            ..transcription("groq:cheap")
        };
        let (backend, provider, _) = route(
            &config,
            &TranscriptionTierRegistry::from_defaults().unwrap(),
        )
        .unwrap();
        let key = |config: &LlmTranscriptionConfig| {
            crate::credentials::slot(
                Modality::Transcription,
                &provider,
                None,
                Some(Arc::clone(&global)),
                legacy(config, backend, &provider, None, Some(Arc::clone(&global))),
            )
        };
        assert_eq!(key(&config).get().as_deref(), Ok("configured"));

        global
            .set("transcription/groq/api_key", "placed-on-purpose")
            .unwrap();
        assert_eq!(
            key(&config).get().as_deref(),
            Ok("placed-on-purpose"),
            "only the credential's own path precedes the install's key"
        );
        assert!(backend.is_hosted());
        assert_eq!(backend.env(), ["GROQ_API_KEY"]);
    }

    #[test]
    fn a_keyed_route_builds_with_no_key_anywhere() {
        let built = HubTranscriptionBuilder::new(
            transcription("groq:cheap"),
            Arc::new(TranscriptionTierRegistry::from_defaults().unwrap()),
        )
        .build();
        assert!(built.is_ok(), "a missing key is a per-call outcome");
    }

    #[test]
    fn an_unroutable_spec_is_an_error_naming_it_never_another_provider() {
        for (spec, names) in [
            (
                "nonesuch:whisper-1",
                "'nonesuch:whisper-1' names no known provider",
            ),
            ("whisper-1", "'whisper-1' names no known provider"),
            (
                "chatgpt:gpt-6.1-sol",
                "routed to 'chatgpt', which does not transcribe",
            ),
            ("anthropic:cheap", "the cheap tier of 'anthropic'"),
            ("custom:my-whisper", "requires [llm.transcription].url"),
        ] {
            let config = transcription(spec);
            let error = routed(&config).unwrap_err().to_string();
            assert!(error.contains(names), "{spec}: {error}");
            assert!(
                HubTranscriptionBuilder::new(
                    config,
                    Arc::new(TranscriptionTierRegistry::from_defaults().unwrap())
                )
                .build()
                .is_err(),
                "{spec}"
            );
        }
    }
}
