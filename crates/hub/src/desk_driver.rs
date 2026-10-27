//! Hub adapter for `workforce-desk`: the session registry in Postgres,
//! system-admin-only access (a desk session runs commands as the hub's
//! user), the vault keys behind cloud voice sessions and spoken replies, the
//! tier models the agent picker offers, and the `/api/desk` routes on the
//! gateway.

use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::{HeaderMap, StatusCode};
use axum::Router;
use eyre::{Context, Result};
use tokio_util::sync::CancellationToken;
use workforce_agent::llm::provider::ProviderKind;
use workforce_agent::llm::speech::{provider_from_config, SpeechRequest};
use workforce_agent::llm::tiers::TierRegistry;
use workforce_common::secret::KeySlot;
use workforce_common::settings::{DeskRestore, LlmSpeechConfig, WorkforceConfig};
use workforce_desk::{
    Audio, Authorize, Desk, DeskStore, KeySource, Models, Options, Provider, Realtime,
    RestorePolicy, Speak, SpeakError, Speech, TierModel,
};

use crate::credentials::{how_to_add, Caller, Legacy};
use crate::desk_tools::DeskTool;
use crate::gateway::{resolve_caller_context, GatewayState};
use crate::hub_core::formatting::normalize_for_tts;
use crate::hub_core::subscriptions::tiered_models;
use crate::serve::AgentService;
use crate::skill_loader::CredScope;
use crate::vault::Vault;

/// The hub's one desk, once `[desk]` has started it.
static DESK: OnceLock<Desk> = OnceLock::new();

/// Gives `service` the `desk.*` tools when desk sessions are on. Called for
/// every agent at boot and for agents spawned later.
pub fn register_tools(service: &AgentService) {
    let Some(desk) = DESK.get() else { return };
    for tool in DeskTool::all(desk) {
        if !service.tools.has(workforce_agent::tools::Tool::name(&tool)) {
            service.tools.register(Arc::new(tool));
        }
    }
}

struct SystemAdmins(Arc<GatewayState>);

#[async_trait]
impl Authorize for SystemAdmins {
    async fn authorize(
        &self,
        headers: &HeaderMap,
        query_token: Option<&str>,
    ) -> Result<(), StatusCode> {
        let caller = resolve_caller_context(&self.0, headers, query_token).await;
        match caller {
            c if c.is_admin => Ok(()),
            c if c.identity_id.is_some() || c.device_id.is_some() => Err(StatusCode::FORBIDDEN),
            _ => Err(StatusCode::UNAUTHORIZED),
        }
    }
}

/// Voice keys from the vault alone: the provider's speech key at
/// `speech/<provider>/api_key` (the person's own, then shared tiers), then
/// the provider's LLM account. Never the environment.
struct VaultKeys(Option<Arc<Vault>>);

fn speech_key_path(provider: &str) -> String {
    format!("speech/{provider}/api_key")
}

impl VaultKeys {
    fn speech_key(&self, provider: &str) -> Option<String> {
        Caller::new(None, None, self.0.clone())
            .resolve(&speech_key_path(provider), CredScope::Either)
            .or_else(|| {
                Legacy::default()
                    .account(None, self.0.clone(), provider)
                    .key()
            })
    }
}

impl KeySource for VaultKeys {
    fn api_key(&self, provider: Provider) -> Option<String> {
        self.speech_key(provider.name())
    }

    fn how_to_add(&self, provider: Provider) -> String {
        how_to_add(&speech_key_path(provider.name()))
    }
}

/// Spoken replies on the hub's speech providers, with the provider's
/// defaults for whatever the request leaves out, and its text made speakable
/// as voice clips' is.
#[async_trait]
impl Speech for VaultKeys {
    async fn speak(&self, request: &Speak) -> Result<Audio, SpeakError> {
        let provider = request.provider.name();
        let key = self
            .speech_key(provider)
            .ok_or(SpeakError::NoKey(request.provider))?;
        let config = LlmSpeechConfig {
            provider: provider.into(),
            model: request.model.clone(),
            voice: request.voice.clone(),
            ..LlmSpeechConfig::default()
        };
        let speaker = provider_from_config(&config, Path::new(""), |_| KeySlot::fixed(&*key))
            .map_err(|e| SpeakError::Invalid(e.to_string()))?;
        let text = normalize_for_tts(&request.text);
        let text = text.trim();
        if text.is_empty() {
            return Err(SpeakError::Invalid(
                "nothing speakable is left once formatting is stripped".into(),
            ));
        }
        let spoken = speaker
            .synthesize(&SpeechRequest {
                text,
                speed: request.speed,
                voice: None,
            })
            .await
            .map_err(|e| SpeakError::Upstream(e.to_string()))?;
        Ok(Audio {
            bytes: spoken.bytes,
            media_type: spoken.media_type.into(),
        })
    }
}

/// The hub's own tiers: `[llm.equivalences]` belongs to an agent's config,
/// and a desk session is not a hub agent.
struct Tiers(TierRegistry);

impl Models for Tiers {
    fn tiers(&self, provider: &str) -> Vec<TierModel> {
        ProviderKind::parse(provider)
            .map(|kind| {
                tiered_models(kind, &self.0)
                    .into_iter()
                    .map(|(tier, model)| TierModel {
                        tier: tier.as_str(),
                        model,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn options(config: &WorkforceConfig) -> Options {
    let desk = &config.desk;
    Options {
        claude: desk.claude.clone().into(),
        codex: desk.codex.clone().into(),
        env: std::env::vars().collect(),
        run_dir: std::env::temp_dir().join("wf-desk"),
        scrollback_bytes: desk.scrollback_kb.saturating_mul(1024),
        approval_timeout: Duration::from_secs(desk.approval_timeout_secs.max(10)),
        restore: match desk.restore {
            DeskRestore::Ask => RestorePolicy::Ask,
            DeskRestore::Auto => RestorePolicy::Auto,
            DeskRestore::Off => RestorePolicy::Off,
        },
        adopt_every: (desk.adopt_interval_secs > 0)
            .then(|| Duration::from_secs(desk.adopt_interval_secs)),
    }
}

/// Merges the desk routes into `app` when `[desk]` is enabled. Fails closed
/// without Postgres: the registry is what restore reads.
pub async fn mount(
    app: Router,
    state: &Arc<GatewayState>,
    config: &WorkforceConfig,
    cancel: CancellationToken,
) -> Result<Router> {
    if !config.desk.enabled {
        return Ok(app);
    }
    let pg = config
        .postgres
        .as_ref()
        .filter(|p| p.enabled)
        .ok_or_else(|| eyre::eyre!("[desk] is enabled but [postgres] is not"))?;
    let store = DeskStore::connect(pg)
        .await
        .context("opening the desk registry")?;
    let desk = Desk::start(store, options(config), cancel)
        .await
        .context("starting desk sessions")?;
    if DESK.set(desk.clone()).is_err() {
        eyre::bail!("desk sessions started twice");
    }
    for service in state.registry.all_services() {
        register_tools(&service);
    }
    tracing::info!("desk sessions enabled");
    let voice = Realtime::new(Arc::new(VaultKeys(state.vault.clone())));
    Ok(app.merge(workforce_desk::router(
        desk,
        Arc::new(SystemAdmins(Arc::clone(state))),
        Arc::new(voice),
        Arc::new(VaultKeys(state.vault.clone())),
        Arc::new(Tiers(TierRegistry::new(None))),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::Modality;
    use axum::response::IntoResponse;
    use workforce_agent::llm::speech::canonical_speech_provider;
    use workforce_desk::SpeechProvider;

    fn vault(dir: &std::path::Path) -> Arc<Vault> {
        Arc::new(Vault::open(&dir.join("global"), "test-key").unwrap())
    }

    #[test]
    fn voice_keys_come_from_the_speech_path_in_the_vault() {
        let dir = tempfile::tempdir().unwrap();
        let global = vault(dir.path());
        let keys = VaultKeys(Some(Arc::clone(&global)));
        assert_eq!(keys.api_key(Provider::Gemini), None);
        global.set("speech/gemini/api_key", "gm-test").unwrap();
        assert_eq!(keys.api_key(Provider::Gemini).as_deref(), Some("gm-test"));
        assert_eq!(keys.api_key(Provider::OpenAi), None);
        assert!(keys
            .how_to_add(Provider::OpenAi)
            .contains("speech/openai/api_key"));
    }

    #[test]
    fn no_vault_means_no_voice_keys() {
        assert_eq!(VaultKeys(None).api_key(Provider::OpenAi), None);
    }

    fn speak(provider: SpeechProvider) -> Speak {
        Speak {
            provider,
            model: None,
            voice: None,
            text: "hello".into(),
            speed: None,
        }
    }

    #[tokio::test]
    async fn speaking_without_a_vault_key_says_to_add_one_in_desk() {
        let dir = tempfile::tempdir().unwrap();
        let global = vault(dir.path());
        global.set("speech/elevenlabs/api_key", "el-test").unwrap();
        let keys = VaultKeys(Some(global));
        for provider in SpeechProvider::ALL
            .into_iter()
            .filter(|p| *p != SpeechProvider::ElevenLabs)
        {
            let e = keys.speak(&speak(provider)).await.unwrap_err();
            assert_eq!(e, SpeakError::NoKey(provider));
            assert!(e.to_string().contains(&format!(
                "add a {} key under Voices in Desk Settings",
                provider.label()
            )));
            assert_eq!(e.into_response().status(), StatusCode::SERVICE_UNAVAILABLE);
        }
        let e = keys
            .speak(&speak(SpeechProvider::ElevenLabs))
            .await
            .unwrap_err();
        assert!(
            matches!(&e, SpeakError::Invalid(m) if m.contains("voice")),
            "{e}"
        );
        assert_eq!(e.into_response().status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn desk_speech_providers_are_the_hubs_keyed_ones() {
        for provider in SpeechProvider::ALL {
            assert_eq!(
                canonical_speech_provider(provider.name()),
                Some(provider.name())
            );
            assert!(
                crate::credentials::for_provider(Modality::Speech, provider.name()).is_some(),
                "{}",
                provider.name()
            );
        }
    }

    #[test]
    fn the_picker_offers_each_local_providers_tiers() {
        let tiers = Tiers(TierRegistry::new(None));
        let named = |provider| {
            tiers
                .tiers(provider)
                .into_iter()
                .map(|m| (m.tier, m.model))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            named("claude-code"),
            [
                ("powerful", "opus".to_string()),
                ("standard", "sonnet".to_string()),
                ("cheap", "haiku".to_string()),
            ]
        );
        assert_eq!(
            named("chatgpt"),
            [
                ("powerful", "gpt-6.1-sol".to_string()),
                ("standard", "gpt-6-sol".to_string()),
                ("cheap", "gpt-6-luna".to_string()),
            ]
        );
        assert!(named("not-a-provider").is_empty());
    }
}
