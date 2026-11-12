//! Hub adapter for `workforce-desk`: the session registry in Postgres,
//! system-admin-only access (a desk session runs commands as the hub's
//! user), the vault keys behind cloud voice sessions and spoken replies, the
//! credentials of the providers a Desk agent thinks with, and the `/api/desk`
//! routes on the gateway.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::{HeaderMap, StatusCode};
use axum::Router;
use eyre::{Context, Result};
use tokio_util::sync::CancellationToken;
use workforce_agent::llm::provider::{AccountExtras, ProviderKind};
use workforce_agent::llm::speech::{provider_from_config, SpeechRequest};
use workforce_agent::llm::vertex_credentials::{
    adc_file, resolve_source, VertexCredentialSource, CHAIN_SENTINEL,
};
use workforce_common::secret::KeySlot;
use workforce_common::settings::{
    is_claude_oat, DeskLineReach, DeskRestore, LlmSpeechConfig, VaultActor, WorkforceConfig,
};
use workforce_common::subscription::SubscriptionKind;
use workforce_desk::engine::Engine;
use workforce_desk::local::{self, Server};
use workforce_desk::providers::Group;
use workforce_desk::{
    AccountError, Accounts, Audio, Authorize, Desk, DeskStore, Fields, HolderOptions, HubInternal,
    KeySource, Options, Provider, Realtime, RestorePolicy, Speak, SpeakError, Spec, Speech,
    SpeechProvider, Standing, VoiceKey,
};

use crate::agent_llm::vault_provider_name;
use crate::credentials::{how_to_add, Caller, Legacy};
use crate::desk_key_check::{refusal, Live, TestCall, Tried};
use crate::desk_tools::DeskTool;
use crate::gateway::{resolve_caller_context, GatewayState};
use crate::hub_core::logins::store as login_store;
use crate::projects::layout::WorkRoots;
use crate::serve::AgentService;
use crate::skill_loader::CredScope;
use crate::vault::Vault;

/// What a voice with `markup` is given of `text`, a sentence Desk has already
/// made speakable (its `speakable`): only the directions the voice performs are
/// checked. It isn't made speakable again, which would say `my_var` "myvar", nor
/// are bracketed words guessed at: Desk says every word of a reply.
fn spoken(markup: workforce_agent::llm::speech::Markup, text: &str) -> String {
    markup.apply_with(workforce_agent::llm::speech::Asides::Said, text)
}

/// The hub's one desk, once `[desk]` has started it.
static DESK: OnceLock<Desk> = OnceLock::new();

/// The hub's desk, when `[desk]` started one.
pub(crate) fn desk() -> Option<&'static Desk> {
    DESK.get()
}

/// Makes `desk` the hub's, as `[desk]` would; whether it was the first.
#[cfg(test)]
pub(crate) fn install_desk_for_test(desk: Desk) -> bool {
    DESK.set(desk).is_ok()
}

/// Gives `service` the `desk.*` tools when desk sessions are on. Called for
/// every agent at boot and for agents spawned later.
pub fn register_tools(service: &AgentService) {
    let Some(desk) = DESK.get() else { return };
    // Its connectors read their keys from the tiers its `[skills]` names.
    crate::connector_pins::pin(service);
    for tool in DeskTool::all(desk, service.agent_id.as_str()) {
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
    ) -> Result<Option<String>, StatusCode> {
        let caller = resolve_caller_context(&self.0, headers, query_token).await;
        match caller {
            c if c.is_admin => Ok(c.identity_id),
            c if c.identity_id.is_some() || c.device_id.is_some() => Err(StatusCode::FORBIDDEN),
            _ => Err(StatusCode::UNAUTHORIZED),
        }
    }

    /// Only the person who holds the hub, as the floor's decide route reads
    /// them ([`crate::desk_floor::holds_the_hub`]): an administrator's own
    /// sign-in or the device the Desk app paired with it, never another
    /// administrator's device or the gateway token alone.
    async fn authorize_holder(
        &self,
        headers: &HeaderMap,
        query_token: Option<&str>,
    ) -> Result<Option<String>, StatusCode> {
        let caller = resolve_caller_context(&self.0, headers, query_token).await;
        if crate::desk_floor::holds_the_hub(
            self.0.identity_store.as_ref(),
            true,
            caller.identity_id.as_deref(),
        ) {
            return Ok(caller.identity_id);
        }
        match caller {
            c if c.identity_id.is_some() || c.device_id.is_some() => Err(StatusCode::FORBIDDEN),
            _ => Err(StatusCode::UNAUTHORIZED),
        }
    }

    fn is_hub_token(&self, headers: &HeaderMap) -> bool {
        bearer_is(headers, &self.0.auth_token)
    }
}

/// Whether `headers` carry exactly `token` as their bearer, compared in
/// constant time; an empty token is never anyone's.
fn bearer_is(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|given| crate::auth::constant_time_token_eq(given, token))
}

/// Voice keys from the vault alone: the provider's speech key at
/// `speech/<provider>/api_key` (the person's own, then shared tiers), then
/// the provider's LLM account. Never the environment.
struct VaultKeys {
    vault: Option<Arc<Vault>>,
    /// How a voice key is tried before it is kept.
    calls: Arc<dyn TestCall>,
}

fn speech_key_path(provider: &str) -> String {
    format!("speech/{provider}/api_key")
}

impl VaultKeys {
    fn new(vault: Option<Arc<Vault>>) -> Self {
        Self {
            vault,
            calls: Arc::new(Live),
        }
    }

    /// The key for `provider`: Desk's voice key, else the provider's
    /// `llm/accounts/*` key (the Gemini key agents think with also talks).
    fn speech_key(&self, provider: &str) -> Option<String> {
        Caller::new(None, None, self.vault.clone())
            .resolve(&speech_key_path(provider), CredScope::Either)
            .or_else(|| {
                Legacy::default()
                    .account(None, self.vault.clone(), provider)
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
/// defaults for whatever the request leaves out.
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
        // A text that was nothing but directions is the caller's mistake, not
        // the provider's.
        let text = spoken(speaker.markup(), &request.text);
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

    async fn save_key(&self, key: VoiceKey, value: &str) -> Result<(), SpeakError> {
        let vault = self
            .vault
            .as_ref()
            .ok_or_else(|| SpeakError::Invalid("Desk can't keep keys here yet".into()))?;
        let answered = match key {
            VoiceKey::Gemini => {
                let gemini = workforce_desk::providers::spec("gemini")
                    .and_then(Spec::default_model)
                    .unwrap_or("gemini-3.8-flash");
                self.calls
                    .think(
                        ProviderKind::Gemini,
                        value,
                        &AccountExtras::default(),
                        gemini,
                    )
                    .await
            }
            VoiceKey::Speech(provider) => self.calls.speak(provider.name(), value).await,
        };
        if let Err(error) = answered {
            let tried = Tried {
                name: key.label(),
                kind: (key == VoiceKey::Gemini).then_some(ProviderKind::Gemini),
                hint: voice_key_hint(key),
                ..Tried::default()
            };
            if let Some(said) = refusal(&tried, &error) {
                // Never what the provider said: it can quote the key.
                tracing::info!(
                    provider = key.name(),
                    "desk voice key refused by its test call"
                );
                return Err(SpeakError::Refused(said));
            }
        }
        vault
            .set_with_actor(&speech_key_path(key.name()), value, VaultActor::RestOwner)
            .map_err(|e| {
                tracing::warn!(provider = key.name(), error = %e, "desk voice key not stored");
                SpeakError::Invalid(format!("Desk couldn't keep the {} key", key.label()))
            })
    }
}

/// Where a voice key is made.
fn voice_key_hint(key: VoiceKey) -> &'static str {
    match key {
        VoiceKey::Speech(SpeechProvider::FishAudio) => "Create a key at fish.audio under API keys",
        VoiceKey::Speech(SpeechProvider::ElevenLabs) => {
            "Create a key at elevenlabs.io under Developers → API keys"
        }
        VoiceKey::Speech(SpeechProvider::OpenAi) => {
            "Create a key at platform.openai.com under API keys"
        }
        VoiceKey::Speech(SpeechProvider::Soniox) => "Create a key at console.soniox.com",
        VoiceKey::Gemini => "Create a key at aistudio.google.com",
    }
}

/// The credentials of the providers a Desk agent thinks with, in the global
/// vault where every agent reads them: a plan's login at its
/// [`SubscriptionKind::vault_key`] (where Desk's sign-ins write it), a key
/// provider's `llm/accounts/*` record, a local server's address in one too.
struct HubAccounts {
    vault: Option<Arc<Vault>>,
    /// The vaults a login is checked against for duplicates.
    known: Arc<dyn Fn() -> Vec<Arc<Vault>> + Send + Sync>,
    /// Where the person's Google login (Application Default Credentials) is
    /// on this machine, if they have one.
    google_login: GoogleLogin,
    /// Where a plan is asked which models it lists now.
    plans: Option<PlanSource>,
    /// How a key is tried before it is stored.
    calls: Arc<dyn TestCall>,
    /// The ports this hub listens on: a server address on this computer at
    /// one of them is the hub itself, never a model server.
    own_ports: Vec<u16>,
}

type GoogleLogin = Arc<dyn Fn() -> Option<PathBuf> + Send + Sync>;

/// How Desk asks a plan which models it lists: on the hub's shared
/// subscriptions, with the login the hub's own work would run on (the one
/// Desk's sign-in stores).
#[derive(Clone)]
struct PlanSource {
    shared: crate::subscription::SharedSubscriptions,
    scope: crate::subscription::CredentialScope,
}

impl PlanSource {
    async fn listed(
        &self,
        kind: SubscriptionKind,
    ) -> Option<Vec<workforce_desk::providers::Listed>> {
        use crate::subscription::catalog;
        let models = match kind {
            SubscriptionKind::ChatGpt => catalog::chatgpt_models(&self.shared, &self.scope).await,
            SubscriptionKind::Claude => catalog::claude_models(&self.shared, &self.scope).await,
        }?;
        Some(listed(&models))
    }
}

/// `models` as Desk words a model its provider lists.
fn listed(
    models: &[crate::subscription::catalog::PlanModel],
) -> Vec<workforce_desk::providers::Listed> {
    models
        .iter()
        .map(|m| workforce_desk::providers::Listed {
            id: m.id.clone(),
            resolves_to: m.resolves_to.clone(),
            name: m.name.clone(),
            description: m.description.clone(),
            efforts: m.efforts.clone(),
            default_effort: m.default_effort.clone(),
            fast: m.fast.clone(),
        })
        .collect()
}

/// The subscription a plan provider signs in to.
fn plan(spec: &Spec) -> Option<SubscriptionKind> {
    match ProviderKind::parse(spec.id).ok()? {
        ProviderKind::ClaudeCode => Some(SubscriptionKind::Claude),
        ProviderKind::ChatGpt => Some(SubscriptionKind::ChatGpt),
        _ => None,
    }
}

/// The provider kind of a key or local provider, and its account name.
fn metered(spec: &Spec) -> Result<(ProviderKind, &'static str), AccountError> {
    ProviderKind::parse(spec.id)
        .ok()
        .filter(|kind| !kind.is_subscription())
        .map(|kind| (kind, vault_provider_name(kind)))
        .ok_or_else(|| AccountError::Invalid(format!("{} takes no key", spec.name)))
}

fn field<'a>(fields: &'a Fields, name: &str) -> Option<&'a str> {
    fields.get(name).map(String::as_str)
}

/// A server address as the client is handed it.
/// `value` as a region, location or project name: lowercase letters,
/// digits and dashes only, since it becomes part of the provider's address.
fn place_name(label: &str, value: Option<String>) -> Result<Option<String>, AccountError> {
    match value {
        Some(v)
            if v.is_empty()
                || !v
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') =>
        {
            Err(AccountError::Invalid(format!(
                "the {label} is lowercase letters, digits and dashes, like us-east-1"
            )))
        }
        v => Ok(v),
    }
}

/// `url` as it may be said back: never a user or password typed into it.
fn shown_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut u) => {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.to_string().trim_end_matches('/').to_string()
        }
        Err(_) => "that address".into(),
    }
}

/// Whether `url` reaches this computer at one of `ports`.
fn is_own(url: &str, ports: &[u16]) -> bool {
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    let local = match u.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback() || ip.is_unspecified(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback() || ip.is_unspecified(),
        None => false,
    };
    local
        && u.port_or_known_default()
            .is_some_and(|p| ports.contains(&p))
}

/// The ports in listen addresses like `0.0.0.0:8080`.
fn ports_of<'a>(listens: impl IntoIterator<Item = &'a str>) -> Vec<u16> {
    listens
        .into_iter()
        .filter_map(|l| l.rsplit(':').next()?.parse().ok())
        .collect()
}

fn server_url(fields: &Fields) -> Result<String, AccountError> {
    let url = field(fields, "url").unwrap_or_default();
    match url::Url::parse(url) {
        Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => {
            Ok(url.trim_end_matches('/').to_string())
        }
        _ => Err(AccountError::Invalid(
            "the server URL is an http:// or https:// address".into(),
        )),
    }
}

/// The `llm/accounts/*` record `fields` make for `kind`: its `api_key` slot
/// as [`workforce_agent::llm::provider::create_provider_with`] reads it, and
/// the [`AccountExtras`] beside it.
fn account(
    kind: ProviderKind,
    fields: &Fields,
    google_login: &GoogleLogin,
) -> Result<serde_json::Value, AccountError> {
    let required = |name: &str| {
        field(fields, name)
            .map(str::to_string)
            .ok_or_else(|| AccountError::Invalid(format!("{name} is missing")))
    };
    let mut extras = AccountExtras::default();
    let api_key = match kind {
        ProviderKind::Bedrock if field(fields, "profile").is_some() => {
            let profile = required("profile")?;
            if !profile
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.@+".contains(c))
            {
                return Err(AccountError::Invalid(
                    "an AWS profile name is letters, digits and - _ . @ +, as in ~/.aws/config"
                        .into(),
                ));
            }
            extras.region = place_name("region", field(fields, "region").map(str::to_string))?;
            extras.profile = Some(profile);
            // The profile, not the hub's environment or default chain, is
            // where its credentials come from.
            workforce_agent::llm::bedrock_credentials::CHAIN_SENTINEL.to_string()
        }
        ProviderKind::Bedrock => {
            extras.region = place_name("region", field(fields, "region").map(str::to_string))?;
            format!(
                "{}:{}",
                required("access_key_id")?,
                required("secret_access_key")?
            )
        }
        ProviderKind::VertexAnthropic if field(fields, "service_account_json").is_none() => {
            extras.project =
                place_name("Project ID", field(fields, "project").map(str::to_string))?;
            extras.project = Some(extras.project.take().ok_or_else(|| {
                AccountError::Invalid(
                    "using your Google login needs the Project ID: a login names no project".into(),
                )
            })?);
            if google_login().is_none() {
                return Err(AccountError::Invalid(
                    "no Google login was found on this Mac: run gcloud auth \
                     application-default login, or paste a service account key"
                        .into(),
                ));
            }
            extras.location = place_name(
                "location",
                Some(field(fields, "location").unwrap_or("global").to_string()),
            )?;
            CHAIN_SENTINEL.to_string()
        }
        ProviderKind::VertexAnthropic => {
            let key = required("service_account_json")?;
            let Ok(source @ VertexCredentialSource::ServiceAccountKey(_)) = resolve_source(&key)
            else {
                return Err(AccountError::Invalid(
                    "the service account key is the whole JSON key file, starting with {".into(),
                ));
            };
            extras.project = place_name(
                "Project ID",
                field(fields, "project")
                    .map(str::to_string)
                    .or_else(|| source.project_id()),
            )?;
            if extras.project.is_none() {
                return Err(AccountError::Invalid(
                    "the key names no project: give the Project ID".into(),
                ));
            }
            extras.location = place_name(
                "location",
                Some(field(fields, "location").unwrap_or("global").to_string()),
            )?;
            key
        }
        ProviderKind::Ollama => server_url(fields)?,
        ProviderKind::Vllm => {
            extras.token = field(fields, "api_key").map(str::to_string);
            server_url(fields)?
        }
        ProviderKind::Anthropic => {
            let key = required("api_key")?;
            if is_claude_oat(&key) {
                return Err(AccountError::Invalid(
                    "that is a Claude subscription token, not an API key: connect the Claude \
                     plan instead"
                        .into(),
                ));
            }
            key
        }
        _ => required("api_key")?,
    };
    let mut record = serde_json::to_value(extras)
        .map_err(|e| AccountError::Internal(format!("building the account: {e}")))?;
    record["provider"] = vault_provider_name(kind).into();
    record["api_key"] = api_key.into();
    Ok(record)
}

impl HubAccounts {
    fn vault(&self) -> Result<&Arc<Vault>, AccountError> {
        self.vault
            .as_ref()
            .ok_or_else(|| AccountError::Unavailable("Desk can't keep keys here yet".into()))
    }

    /// Where a local server is, and its bearer key: the vault account's URL
    /// (the one Desk set), else a URL set in the hub's environment, else
    /// Ollama's usual port.
    fn endpoint(&self, kind: ProviderKind) -> Option<(String, Option<String>)> {
        self.endpoint_with(kind, |var| std::env::var(var).ok())
    }

    /// [`Self::endpoint`] with `env` as the environment.
    fn endpoint_with(
        &self,
        kind: ProviderKind,
        env: impl Fn(&str) -> Option<String>,
    ) -> Option<(String, Option<String>)> {
        let (url_var, key_var, default) = match kind {
            ProviderKind::Ollama => (
                "OLLAMA_URL",
                "OLLAMA_API_KEY",
                Some("http://localhost:11434"),
            ),
            ProviderKind::Vllm => ("VLLM_URL", "VLLM_API_KEY", None),
            _ => return None,
        };
        let env_key = || env(key_var);
        let record = self
            .vault
            .as_deref()
            .and_then(|vault| vault.find_account(vault_provider_name(kind)))
            .and_then(|(_, record)| {
                let url = record["api_key"].as_str()?.trim().to_string();
                let token = record["token"].as_str().map(str::to_string);
                (!url.is_empty()).then_some((url, token))
            });
        if let Some((url, token)) = record {
            return Some((url, token.or_else(env_key)));
        }
        if let Some(url) = env(url_var).filter(|u| !u.trim().is_empty()) {
            return Some((url, env_key()));
        }
        default.map(|url| (url.to_string(), env_key()))
    }

    /// One call on `record` as an agent would make it, before it is stored:
    /// why not, in the person's words, when the provider refused it. Ollama
    /// is left to its standing, which asks it each time.
    async fn tried(
        &self,
        spec: &Spec,
        kind: ProviderKind,
        record: &serde_json::Value,
    ) -> Result<(), AccountError> {
        let api_key = record["api_key"].as_str().unwrap_or_default();
        let extras: AccountExtras = serde_json::from_value(record.clone()).unwrap_or_default();
        match kind {
            ProviderKind::Ollama => return Ok(()),
            ProviderKind::Vllm if is_own(api_key, &self.own_ports) => {
                return Err(AccountError::Invalid(
                    "that address is Desk itself, not a vLLM server: give the address vLLM listens on"
                        .into(),
                ));
            }
            ProviderKind::Vllm => {
                return match self
                    .calls
                    .reach(Server::Vllm, api_key, extras.token.as_deref())
                    .await
                {
                    true => Ok(()),
                    false => Err(AccountError::Refused(format!(
                        "Desk couldn't get the models from the vLLM server at {}. Check it's running, the address, and the API key if it asks for one.",
                        shown_url(api_key)
                    ))),
                };
            }
            _ => {}
        }
        let Some(model) = spec.default_model() else {
            return Ok(());
        };
        let Err(error) = self.calls.think(kind, api_key, &extras, model).await else {
            return Ok(());
        };
        let login = extras.profile.is_some()
            || (kind == ProviderKind::VertexAnthropic && api_key == CHAIN_SENTINEL);
        let tried = Tried {
            name: spec.name,
            kind: Some(kind),
            hint: spec.connect.hint,
            model: Some(model),
            region: extras.region.as_deref().or(extras.location.as_deref()),
            project: extras.project.as_deref(),
            login,
        };
        match refusal(&tried, &error) {
            None => Ok(()),
            Some(said) => {
                // Never what the provider said: it can quote the key.
                tracing::info!(
                    provider = spec.id,
                    "desk provider key refused by its test call"
                );
                Err(AccountError::Refused(said))
            }
        }
    }

    async fn write(&self, path: &str, value: &str) -> Result<(), AccountError> {
        let vault = self.vault()?;
        let known = Arc::clone(&self.known);
        login_store::set(vault, path, value, move || known(), VaultActor::RestOwner)
            .await
            .map_err(|e| {
                tracing::warn!(path, error = %e, "desk provider credential not stored");
                AccountError::Internal("Desk couldn't keep the key. Try again in a moment.".into())
            })
    }

    async fn delete(&self, path: &str) -> Result<(), AccountError> {
        login_store::delete(self.vault()?, path, VaultActor::RestOwner)
            .await
            .map(drop)
            .map_err(|e| {
                tracing::warn!(path, error = %e, "desk provider credential not removed");
                AccountError::Internal(
                    "Desk couldn't remove the key. Try again in a moment.".into(),
                )
            })
    }
}

#[async_trait]
impl Accounts for HubAccounts {
    async fn standing(&self, spec: &Spec) -> Standing {
        let Some(vault) = self.vault.as_deref() else {
            return Standing::default();
        };
        if let Some(kind) = plan(spec) {
            let connected = vault
                .get(kind.vault_key())
                .is_some_and(|v| !v.trim().is_empty());
            let listed = match (&self.plans, connected) {
                (Some(plans), true) => plans.listed(kind).await,
                _ => None,
            };
            return Standing {
                connected,
                installed: Vec::new(),
                listed,
            };
        }
        let Ok((kind, name)) = metered(spec) else {
            return Standing::default();
        };
        if kind == ProviderKind::VertexAnthropic {
            // A key's account, or one on the person's Google login.
            return Standing {
                connected: vault.find_account(name).is_some(),
                installed: Vec::new(),
                listed: None,
            };
        }
        if spec.group != Group::Local {
            let key = vault
                .find_account_api_key(name)
                .filter(|key| !key.trim().is_empty());
            // What the person's own account lists now, for the providers
            // that can say (Desk's table stands when they can't).
            let listed = match (kind, key.as_deref()) {
                (ProviderKind::Anthropic, Some(key)) => {
                    crate::desk_key_models::anthropic_listed(key)
                        .await
                        .map(|models| listed(&models))
                }
                (ProviderKind::Bedrock, Some(key)) => {
                    let extras: AccountExtras = vault
                        .find_account(name)
                        .and_then(|(_, record)| serde_json::from_value(record).ok())
                        .unwrap_or_default();
                    fn given(v: &Option<String>) -> Option<&str> {
                        v.as_deref().map(str::trim).filter(|v| !v.is_empty())
                    }
                    crate::desk_key_models::bedrock_listed(
                        key,
                        given(&extras.profile),
                        given(&extras.region),
                    )
                    .await
                    .map(|models| listed(&models))
                }
                _ => None,
            };
            return Standing {
                connected: key.is_some(),
                installed: Vec::new(),
                listed,
            };
        }
        let (Some(server), Some((url, token))) = (Server::of(spec.id), self.endpoint(kind)) else {
            return Standing::default();
        };
        let installed =
            local::installed(server, &url, token.as_deref(), local::PROBE_TIMEOUT).await;
        Standing {
            // Ollama is connected when it answers; vLLM when it has an address.
            connected: installed.is_some() || server == Server::Vllm,
            installed: installed.unwrap_or_default(),
            listed: None,
        }
    }

    fn efforts(&self, provider: &str) -> Vec<&'static str> {
        ProviderKind::parse(provider)
            .map(|kind| kind.efforts().to_vec())
            .unwrap_or_default()
    }

    fn model_efforts(&self, provider: &str, model: &str) -> Vec<&'static str> {
        ProviderKind::parse(provider)
            .map(|kind| kind.model_efforts(model).to_vec())
            .unwrap_or_default()
    }

    async fn connect(&self, spec: &Spec, fields: &Fields) -> Result<(), AccountError> {
        let (kind, name) = metered(spec)?;
        let record = account(kind, fields, &self.google_login)?;
        // Nowhere to keep it: say so before any call is made with it.
        self.vault()?;
        self.tried(spec, kind, &record).await?;
        let path = self
            .vault()?
            .find_account(name)
            .map(|(path, _)| path)
            .unwrap_or_else(|| format!("llm/accounts/{name}"));
        self.write(&path, &record.to_string()).await
    }

    async fn disconnect(&self, spec: &Spec) -> Result<(), AccountError> {
        if let Some(kind) = plan(spec) {
            return self.delete(kind.vault_key()).await;
        }
        let (_, name) = metered(spec)?;
        for path in self.vault()?.account_paths(name) {
            self.delete(&path).await?;
        }
        Ok(())
    }
}

/// Where Desk's per-session sockets, sessions' token FIFOs and sign-ins'
/// browsers go: a folder of this user's in the temp folder, never one every
/// user shares. (Not `XDG_RUNTIME_DIR`: on a machine without linger it goes
/// when the last login does, under the hub's running sessions.)
fn run_dir() -> PathBuf {
    #[cfg(unix)]
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    #[cfg(not(unix))]
    let uid = 0;
    run_dir_in(&std::env::temp_dir(), uid, workforce_desk::private_run_dir)
}

/// [`run_dir`] under `temp` for `uid`: `wf-desk-<uid>` (as root, as in a
/// container, the `wf-desk` it has always been), once `private` takes it as
/// this user's own. When it doesn't (someone else made it first, or it is a
/// link), a new folder of this user's with a name no one can guess, so
/// sessions still start.
fn run_dir_in(temp: &Path, uid: u32, private: impl Fn(&Path) -> eyre::Result<()>) -> PathBuf {
    let known = match uid {
        0 => temp.join("wf-desk"),
        uid => temp.join(format!("wf-desk-{uid}")),
    };
    match private(&known) {
        Ok(()) => return known,
        Err(e) => tracing::warn!(
            dir = %known.display(),
            error = %format!("{e:#}"),
            "Desk's run folder isn't this user's own: using a new private one"
        ),
    }
    for _ in 0..8 {
        // Short: a session's socket path there is capped at 104 bytes on macOS.
        let name = uuid::Uuid::new_v4().simple().to_string();
        let dir = temp.join(format!("wfd-{}", &name[..10]));
        #[cfg(unix)]
        let made = {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().mode(0o700).create(&dir)
        };
        #[cfg(not(unix))]
        let made = std::fs::create_dir(&dir);
        if made.is_ok() && private(&dir).is_ok() {
            return dir;
        }
    }
    known
}

fn options(config: &WorkforceConfig) -> Options {
    let desk = &config.desk;
    // The Claude Code the hub's own work runs (a plan's model list among it)
    // is the one Desk's sessions run.
    crate::claude_code::use_program_named(&desk.claude);
    Options {
        programs: programs(desk),
        env: std::env::vars().collect(),
        run_dir: run_dir(),
        scrollback_bytes: desk.scrollback_kb.saturating_mul(1024),
        approval_timeout: Duration::from_secs(desk.approval_timeout_secs.max(10)),
        restore: match desk.restore {
            DeskRestore::Ask => RestorePolicy::Ask,
            DeskRestore::Auto => RestorePolicy::Auto,
            DeskRestore::Off => RestorePolicy::Off,
        },
        adopt_every: (desk.adopt_interval_secs > 0)
            .then(|| Duration::from_secs(desk.adopt_interval_secs)),
        holder: holder_options(desk),
        hub_internal: hub_internal(config),
        account_keys: account_keys(&desk.accounts),
        line_reach: match desk.line_reach {
            DeskLineReach::Overseer => workforce_desk::line::Reach::Overseer,
            DeskLineReach::Group => workforce_desk::line::Reach::Group,
            DeskLineReach::All => workforce_desk::line::Reach::All,
        },
        memory: workforce_desk::memory::MemoryOptions {
            enabled: desk.memory.enabled,
            backfill_days: desk.memory.backfill_days,
            retain_days: desk.memory.retain_days,
            exclude: desk.memory.exclude.clone(),
            max_disk_mb: desk.memory.max_disk_mb,
            index_per_minute: desk.memory.index_per_minute,
            session_reach: workforce_desk::memory::reach::Reach::parse(&desk.memory.session_reach)
                .unwrap_or(workforce_desk::memory::reach::Reach::Project),
            derive: desk.memory.derive != "off",
            derive_calls_per_day: desk.memory.derive_calls_per_day,
            machine: may_be_machine(desk),
            brief_max_tokens: desk.memory.brief_max_tokens,
        },
        bosses: workforce_desk::grant::BossOptions {
            machine: is_machine(desk),
            grant_hours: desk.bosses.grant_hours,
        },
    }
}

/// Whether Desk's helper said this hub is a machine's (`[desk] machine =
/// true`), which is when its grants say its sessions' groups. Unlike
/// [`may_be_machine`], unset is not a machine here: a grant must never
/// rewrite the person's own sidebar.
fn is_machine(desk: &workforce_common::settings::DeskConfig) -> bool {
    desk.machine == Some(true)
}

/// Whether this hub might be a machine's: unless Desk's helper said it is
/// the person's own computer (`[desk] machine = false`), and nothing else
/// says it is a machine's (`agents = false`, `restore = "auto"`, as the
/// helper writes a machine's), it is taken to be one. Fails closed: a
/// config written before `machine` was, or edited by hand, is a machine's.
fn may_be_machine(desk: &workforce_common::settings::DeskConfig) -> bool {
    desk.machine != Some(false)
        || !desk.agents
        || desk.restore == workforce_common::settings::DeskRestore::Auto
}

/// Where `[desk.accounts]` says the person's engine accounts' sign-ins are
/// kept: Desk Helper names the store Desk chose for its own secrets. None
/// (and a warning) when it names one this computer hasn't.
fn account_keys(
    config: &workforce_common::settings::DeskAccounts,
) -> Option<Arc<dyn workforce_desk::engine_accounts::Keys>> {
    use workforce_common::settings::DeskAccountsStore;
    use workforce_desk::engine_accounts as accounts;
    match config.store? {
        DeskAccountsStore::File => match config.file.as_deref().map(Path::new) {
            Some(file) if file.is_absolute() => Some(Arc::new(accounts::FileKeys::new(file))),
            _ => {
                tracing::warn!(
                    "[desk.accounts] store = \"file\" needs an absolute file: no accounts kept"
                );
                None
            }
        },
        #[cfg(target_os = "macos")]
        DeskAccountsStore::System => Some(Arc::new(accounts::SystemKeys::new(
            config.service.as_deref().unwrap_or("xyz.driftwerk.desk"),
        ))),
        #[cfg(not(target_os = "macos"))]
        DeskAccountsStore::System => {
            tracing::warn!(
                "[desk.accounts] store = \"system\" is the macOS keychain: no accounts kept"
            );
            None
        }
    }
}

/// Each engine's program as `[desk]` names it. A custom command names its
/// own, so it has none here.
fn programs(desk: &workforce_common::settings::DeskConfig) -> BTreeMap<Engine, PathBuf> {
    Engine::ALL
        .into_iter()
        .filter_map(|engine| {
            let program = match engine {
                Engine::Claude => &desk.claude,
                Engine::Codex => &desk.codex,
                Engine::Gemini => &desk.gemini,
                Engine::Copilot => &desk.copilot,
                Engine::OpenCode => &desk.opencode,
                Engine::Cursor => &desk.cursor,
                Engine::Devin => &desk.devin,
                Engine::Custom => return None,
            };
            Some((engine, PathBuf::from(program)))
        })
        .collect()
}

/// What this hub runs Claude Code in and as itself: its own folders and its
/// own process. Its sessions there are the hub's (a completion, an agent's
/// turn, a coding round), never the person's for Desk to adopt.
fn hub_internal(config: &WorkforceConfig) -> HubInternal {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    HubInternal::new(
        hub_dirs(
            Path::new(&config.hub.data_dir),
            config.hub.worktree_root_path(),
        ),
        home.as_deref(),
    )
    .started_by(std::process::id())
}

/// The folders only the hub writes, made absolute: its data directory
/// (completions, agents' work trees, worktrees, everything else it keeps),
/// and with a `worktree_root` its hot work trees and checked-out worktrees
/// there — never that root itself, which may be shared.
fn hub_dirs(data_dir: &Path, worktree_root: Option<&Path>) -> Vec<PathBuf> {
    use crate::coding_sandbox::absolutise_data_dir;
    let roots = WorkRoots::new(data_dir.to_path_buf(), worktree_root);
    let worktrees =
        worktree_root.map(|_| crate::code::worktree::worktrees_dir(data_dir, worktree_root));
    std::iter::once(roots.data_dir)
        .chain(roots.hot)
        .chain(worktrees)
        .map(absolutise_data_dir)
        .collect()
}

/// The PTY holder, when `[desk]` names its socket and a token.
fn holder_options(desk: &workforce_common::settings::DeskConfig) -> Option<HolderOptions> {
    let socket = desk.pty_holder_socket.as_deref().map(str::trim)?;
    let token = desk.pty_holder_token.as_deref().map(str::trim)?;
    if socket.is_empty() || token.is_empty() || token.starts_with("${") {
        if !socket.is_empty() {
            tracing::warn!(
                "[desk] pty_holder_socket is set without a token; terminals run in the hub"
            );
        }
        return None;
    }
    Some(HolderOptions {
        socket: socket.into(),
        token: token.into(),
    })
}

/// Merges the desk routes into `app` when `[desk]` is enabled. Fails closed
/// without Postgres: the registry is what restore reads.
pub async fn mount(
    app: Router,
    state: &Arc<GatewayState>,
    config: &WorkforceConfig,
    cancel: CancellationToken,
) -> Result<Router> {
    let app = app.merge(crate::gateway::hardened(
        crate::surface_allows::routes(Arc::clone(state)).merge(crate::desk_floor::router(
            Arc::clone(state),
            crate::desk_floor::Sources::installed(),
        )),
    ));
    if let Some(floor) = crate::floor::installed() {
        floor.gists.serve(Arc::new(crate::floor::gists::HubModel {
            state: Arc::clone(state),
            fallback: config.floor.fallback_providers,
        }));
    }
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
    let cancel_crew = cancel.clone();
    let cancel_uplink = cancel.clone();
    let cancel_sync = cancel.clone();
    let floor = workforce_desk::Floor {
        pricer: Some(Arc::new(crate::desk_ledger::HubPricer::new(Arc::clone(
            &state.registry,
        )))),
        redactor: Some(Arc::new(crate::desk_memory::HubRedactor)),
        index: crate::desk_memory_index::VellevDeskIndex::installed(config)
            .map(|i| i as Arc<dyn workforce_desk::memory::index::MemoryIndex>),
        deriver: Some(Arc::new(crate::desk_memory::HubDeriver {
            model: crate::floor::gists::HubModel {
                state: Arc::clone(state),
                fallback: config.floor.fallback_providers,
            },
        })),
        ..crate::floor::desk_floor()
    };
    let desk = Desk::start(store, options(config), floor, cancel)
        .await
        .context("starting desk sessions")?;
    // Budgets' days are the person's, as the cost governor counts them.
    desk.count_budgets_in(config.user_timezone());
    if DESK.set(desk.clone()).is_err() {
        eyre::bail!("desk sessions started twice");
    }
    // The hub's own Claude calls fail over across the person's accounts.
    if let Some(vault) = state.vault.as_ref() {
        crate::desk_pool::install(vault, &desk, state.identity_store.as_ref()).await;
    }
    // The person's budgets reach agents through the cost governor, and
    // agents' use counts toward Desk's totals: only on a hub running Desk.
    let telemetry = state.telemetry_store.clone().or_else(|| {
        state
            .registry
            .host()
            .map(|s| Arc::clone(&s.telemetry_store))
    });
    if let Some(telemetry) = telemetry {
        desk.spend_agents_with(Arc::new(crate::desk_ceilings::HubAgentSpend(telemetry)));
    }
    crate::cost_governor::install_ceilings(Arc::new(crate::desk_ceilings::DeskCeilings {
        desk: desk.clone(),
        registry: Arc::clone(&state.registry),
    }));
    desk.serve_lines(crate::desk_line_mcp::LineMcp::new(desk.clone()));
    let computer = desk.computer();
    crate::desk_computer::prune_daily(computer.clone(), cancel_crew.clone());
    let browser_routes =
        crate::desk_browser::install(state, config, computer.clone(), cancel_crew.clone());
    crate::desk_computer::install(computer, Arc::new(config.clone()));
    desk.boss_bounds_with(crate::desk_computer::boss_bounds(config));
    // The Codex sessions an agent starts run as fast as it does.
    desk.agent_fast_with({
        let registry = Arc::clone(&state.registry);
        Arc::new(move |agent: &str| {
            registry
                .get(agent)
                .is_some_and(|service| service.resolved.snapshot().fast)
        })
    });
    for service in state.registry.all_services() {
        register_tools(&service);
    }
    tracing::info!("desk sessions enabled");
    let crew = crate::desk_crew::CrewHub::start(
        Arc::clone(state),
        desk.clone(),
        crate::desk_crew::Timing::default(),
        cancel_crew,
    );
    // The boss links this engine holds, its key, and its uplinks to their
    // boss hubs.
    let app = app.merge(crate::gateway::hardened(crate::desk_uplink::routes(
        Arc::clone(state),
    )));
    crate::desk_uplink::start(
        crate::desk_uplink::Uplink {
            state: Arc::clone(state),
            crew: Arc::clone(&crew),
            limits: config.desk.bosses.clone(),
            #[cfg(test)]
            dial: None,
            rates: Default::default(),
        },
        cancel_uplink,
    );
    // The relay to the person's machines, Desk's native side's alone.
    let app = app.merge(crate::gateway::hardened(crate::desk_crew::relay::routes(
        Arc::clone(state),
        Arc::clone(&crew),
    )));
    let voice = Realtime::new(Arc::new(VaultKeys::new(state.vault.clone())));
    let app = app.merge(crate::gateway::hardened(crate::desk_connections::install(
        state,
        config,
        desk.clone(),
    )));
    let app = app.merge(crate::gateway::hardened(browser_routes));
    let (board_sync, board_sync_routes) = crate::desk_board_sync::install(state, desk.clone());
    board_sync.start(cancel_sync);
    let app = app.merge(crate::gateway::hardened(board_sync_routes));
    Ok(app.merge(workforce_desk::router(
        desk,
        Arc::new(SystemAdmins(Arc::clone(state))),
        Arc::new(voice),
        Arc::new(VaultKeys::new(state.vault.clone())),
        Arc::new(HubAccounts {
            vault: state.vault.clone(),
            known: {
                let state = Arc::clone(state);
                Arc::new(move || crate::gateway::login_vaults(&state))
            },
            google_login: Arc::new(adc_file),
            calls: Arc::new(Live),
            own_ports: ports_of([config.hub.listen.as_str(), config.gateway.listen.as_str()]),
            plans: state
                .registry
                .host()
                .zip(state.vault.clone())
                .map(|(host, vault)| PlanSource {
                    shared: host.subscriptions().clone(),
                    scope: crate::subscription::CredentialScope::system(
                        vault,
                        &config.subscription,
                    ),
                }),
        }),
        crew,
    )))
}

#[cfg(test)]
mod tests {

    /// A session may recall only on a hub Desk said is the person's own
    /// computer, and nothing else marks a machine's.
    #[test]
    fn only_a_hub_desk_says_is_this_computer_is_no_machine() {
        let parse = |toml: &str| {
            toml::from_str::<workforce_common::settings::WorkforceConfig>(toml)
                .unwrap()
                .desk
        };
        let cases = [
            ("[desk]\nenabled = true\nmachine = false\n", false),
            // Written before `machine` was, or by hand: unknown.
            ("[desk]\nenabled = true\n", true),
            ("[desk]\nenabled = true\nmachine = true\n", true),
            // A machine's, as the helper writes it, whatever `machine` says.
            ("[desk]\nmachine = false\nagents = false\n", true),
            ("[desk]\nmachine = false\nrestore = \"auto\"\n", true),
        ];
        for (toml, machine) in cases {
            assert_eq!(super::may_be_machine(&parse(toml)), machine, "{toml}");
        }
    }

    /// Grants say a hub's sessions' groups only where the helper wrote
    /// `machine = true`: an unknown hub keeps its own sidebar.
    #[test]
    fn grants_say_the_groups_only_of_a_hub_the_helper_said_is_a_machine() {
        let options = |toml: &str| {
            super::options(
                &toml::from_str::<workforce_common::settings::WorkforceConfig>(toml).unwrap(),
            )
            .bosses
        };
        assert!(options("[desk]\nmachine = true\n").machine);
        assert!(!options("[desk]\nmachine = false\n").machine);
        assert!(!options("[desk]\n").machine, "unset is not a machine here");
        assert_eq!(options("[desk]\n").grant_hours, 72);
        assert_eq!(
            options("[desk.bosses]\ngrant_hours = 336\n").grant_hours,
            336
        );
    }

    /// Moves (preview and commit) are the hub holder's alone: an
    /// administrator's own sign-in passes, anyone else is refused, and the
    /// gateway token alone is no one's.
    #[tokio::test]
    async fn only_the_person_who_holds_the_hub_is_let_make_a_move() {
        use workforce_desk::Authorize as _;
        use workforce_identity::store::IdentityStore;
        use workforce_identity::types::{Identity, IdentityRole};
        let fixture = crate::remote_mcp::testing::fixture(false);
        let jwt = crate::jwt::JwtConfig::new("a-test-secret-of-at-least-32-bytes!!".into(), 3600)
            .unwrap();
        let token = |person: &Identity| {
            fixture.identities.create(person).unwrap();
            crate::jwt::generate_token_with_identity(
                &jwt,
                &person.display_name,
                person.role,
                Some(person.id.to_string()),
            )
            .unwrap()
        };
        let holder = Identity::new("Sam", IdentityRole::SystemAdmin);
        let member = Identity::new("Mo", IdentityRole::Member);
        let (holds, doesnt) = (token(&holder), token(&member));
        let state = Arc::new(GatewayState {
            jwt_config: Some(jwt.clone()),
            ..Arc::try_unwrap(fixture.state)
                .ok()
                .expect("the fixture's only handle")
        });
        let gateway_token = state.auth_token.clone();
        let auth = SystemAdmins(state);
        let bearer = |t: &str| {
            let mut h = HeaderMap::new();
            h.insert(
                axum::http::header::AUTHORIZATION,
                format!("Bearer {t}").parse().unwrap(),
            );
            h
        };
        assert_eq!(
            auth.authorize_holder(&bearer(&holds), None).await,
            Ok(Some(holder.id.to_string()))
        );
        assert_eq!(
            auth.authorize_holder(&bearer(&doesnt), None).await,
            Err(StatusCode::FORBIDDEN)
        );
        assert!(auth
            .authorize_holder(&bearer(&gateway_token), None)
            .await
            .is_err());
        assert_eq!(
            auth.authorize_holder(&HeaderMap::new(), None).await,
            Err(StatusCode::UNAUTHORIZED)
        );
    }
    #[test]
    fn only_the_hubs_own_token_itself_is_the_hub_token() {
        let with = |v: &str| {
            let mut h = axum::http::HeaderMap::new();
            h.insert(axum::http::header::AUTHORIZATION, v.parse().unwrap());
            h
        };
        assert!(super::bearer_is(&with("Bearer gw-secret"), "gw-secret"));
        assert!(!super::bearer_is(&with("Bearer gw-secre"), "gw-secret"));
        assert!(!super::bearer_is(
            &with("Bearer eyJhbGciOi.jwt.admin"),
            "gw-secret"
        ));
        assert!(!super::bearer_is(&with("gw-secret"), "gw-secret"));
        assert!(!super::bearer_is(&with("Bearer "), ""));
        assert!(!super::bearer_is(
            &axum::http::HeaderMap::new(),
            "gw-secret"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_run_folder_someone_else_made_first_is_passed_over_for_a_new_private_one() {
        let temp = tempfile::tempdir().unwrap();
        let mine = run_dir_in(temp.path(), 1000, workforce_desk::private_run_dir);
        assert_eq!(mine, temp.path().join("wf-desk-1000"));
        assert_eq!(
            run_dir_in(temp.path(), 0, workforce_desk::private_run_dir),
            temp.path().join("wf-desk")
        );
        // Made first as a link to somewhere else (as another user could).
        let elsewhere = temp.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, temp.path().join("wf-desk-1001")).unwrap();
        let other = run_dir_in(temp.path(), 1001, workforce_desk::private_run_dir);
        assert_ne!(other, temp.path().join("wf-desk-1001"));
        assert!(other.starts_with(temp.path()));
        assert!(workforce_desk::private_run_dir(&other).is_ok());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&other).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    use super::*;
    use crate::credentials::Modality;
    use axum::response::IntoResponse;
    use workforce_agent::llm::speech::canonical_speech_provider;
    use workforce_desk::SpeechProvider;

    fn vault(dir: &std::path::Path) -> Arc<Vault> {
        Arc::new(Vault::open(&dir.join("global"), "test-key").unwrap())
    }

    #[test]
    fn the_hubs_own_folders_are_its_data_and_its_hot_trees_never_their_root() {
        assert_eq!(hub_dirs(Path::new("/data"), None), [PathBuf::from("/data")]);
        assert_eq!(
            hub_dirs(Path::new("/data"), Some(Path::new("/fast"))),
            [
                PathBuf::from("/data"),
                PathBuf::from("/fast/work"),
                PathBuf::from("/fast/worktrees")
            ]
        );
        let relative = hub_dirs(Path::new("data"), None);
        assert!(relative[0].is_absolute() && relative[0].ends_with("data"));
    }

    /// What the hub runs in its own folders is never adopted; the person's
    /// folders are, the one the hub's data sits in included.
    #[test]
    fn the_hubs_own_sessions_are_told_from_the_persons() {
        let root = tempfile::tempdir().unwrap();
        let repo = std::fs::canonicalize(root.path()).unwrap();
        let mut config = WorkforceConfig::default();
        config.hub.data_dir = repo.join("data").to_string_lossy().into_owned();
        config.hub.worktree_root = Some(repo.join("fast").to_string_lossy().into_owned());
        let hub = options(&config).hub_internal;
        let holds = |p: PathBuf| hub.holds(&p);
        assert!(holds(repo.join(
            "data/cli-completions/b369e00e-c276-4ae0-8394-6d83707380a0"
        )));
        assert!(holds(repo.join(
            "data/agents/scout/projects/_agent-turn/work/session/at-dm-d4774cbb6c89-2e"
        )));
        assert!(holds(
            repo.join("fast/work/scout/_agent-turn/session/at-dm-1")
        ));
        assert!(holds(repo.join("fast/worktrees/fix-1")));
        assert!(!holds(repo.clone()), "the folder the hub's data sits in");
        assert!(!holds(repo.join("src")));
        assert!(!holds(repo.join("fast/notes")), "the worktree root itself");
        assert!(!holds(repo.join("data-old/x")));
    }

    #[test]
    fn the_pty_holder_is_used_only_with_a_socket_and_a_token() {
        use workforce_common::settings::DeskConfig;
        let with = |socket: Option<&str>, token: Option<&str>| {
            holder_options(&DeskConfig {
                pty_holder_socket: socket.map(Into::into),
                pty_holder_token: token.map(Into::into),
                ..DeskConfig::default()
            })
        };
        assert!(with(None, None).is_none());
        assert!(with(Some("/run/desk/pty.sock"), None).is_none());
        // `${WORKFORCE_PTY_HOLDER_TOKEN}` unset expands to nothing.
        assert!(with(Some("/run/desk/pty.sock"), Some("")).is_none());
        let h = with(Some("/run/desk/pty.sock"), Some("s3cret-token-value")).unwrap();
        assert_eq!(h.socket, std::path::PathBuf::from("/run/desk/pty.sock"));
        assert!(options(&WorkforceConfig::default()).holder.is_none());
    }

    #[test]
    fn every_engine_but_a_custom_command_has_its_program_from_desk_config() {
        let programs = options(&WorkforceConfig::default()).programs;
        assert_eq!(programs.len(), Engine::ALL.len() - 1);
        assert_eq!(programs[&Engine::Claude], PathBuf::from("claude"));
        assert_eq!(programs[&Engine::Cursor], PathBuf::from("agent"));
        assert!(!programs.contains_key(&Engine::Custom));
    }

    #[test]
    fn voice_keys_come_from_the_speech_path_in_the_vault() {
        let dir = tempfile::tempdir().unwrap();
        let global = vault(dir.path());
        let keys = VaultKeys::new(Some(Arc::clone(&global)));
        assert_eq!(keys.api_key(Provider::Gemini), None);
        global.set("speech/gemini/api_key", "gm-test").unwrap();
        assert_eq!(keys.api_key(Provider::Gemini).as_deref(), Some("gm-test"));
        assert_eq!(keys.api_key(Provider::OpenAi), None);
        assert!(keys
            .how_to_add(Provider::OpenAi)
            .contains("speech/openai/api_key"));
    }

    /// Desk's sentence is said as it was sent, less what the voice can't perform.
    #[test]
    fn a_desk_sentence_is_said_as_sent() {
        use workforce_agent::llm::speech::Markup;
        let text = "(excited) Set my_var, then choose (yes) or (no) (pensive).";
        assert_eq!(
            spoken(Markup::FishS1, text),
            "(excited) Set my_var, then choose (yes) or (no)."
        );
        assert_eq!(
            spoken(Markup::Plain, text),
            "Set my_var, then choose (yes) or (no)."
        );
    }

    #[tokio::test]
    async fn a_voice_key_is_kept_only_once_its_provider_takes_it() {
        let dir = tempfile::tempdir().unwrap();
        let global = vault(dir.path());
        let typed = "el-typed-wrong-0123456789";
        let refused = VaultKeys {
            vault: Some(Arc::clone(&global)),
            calls: Arc::new(crate::desk_key_check::Answers::failing(&format!(
                "elevenlabs speech returned 401 Unauthorized: invalid_api_key {typed}"
            ))),
        };
        let e = refused
            .save_key(VoiceKey::Speech(SpeechProvider::ElevenLabs), typed)
            .await
            .unwrap_err();
        let SpeakError::Refused(said) = &e else {
            panic!("{e:?}")
        };
        assert!(
            said.starts_with("ElevenLabs didn't accept that key"),
            "{said}"
        );
        assert!(said.contains("elevenlabs.io"), "{said}");
        assert!(!said.contains(typed));
        assert!(global.get("speech/elevenlabs/api_key").is_none());

        let calls = Arc::new(crate::desk_key_check::Answers::passing());
        let passing = VaultKeys {
            vault: Some(Arc::clone(&global)),
            calls: calls.clone(),
        };
        passing
            .save_key(VoiceKey::Speech(SpeechProvider::ElevenLabs), "el-right")
            .await
            .unwrap();
        passing
            .save_key(VoiceKey::Gemini, "gm-right")
            .await
            .unwrap();
        assert_eq!(
            passing.speech_key("elevenlabs").as_deref(),
            Some("el-right")
        );
        assert_eq!(
            passing.api_key(Provider::Gemini).as_deref(),
            Some("gm-right")
        );
        let asked = calls.asked.lock().clone();
        assert_eq!(asked[0], "speak elevenlabs");
        assert!(
            asked[1].starts_with("gemini "),
            "Gemini Live's key thinks: {asked:?}"
        );
    }

    #[test]
    fn gemini_live_talks_with_the_gemini_key_agents_think_with() {
        let dir = tempfile::tempdir().unwrap();
        let global = vault(dir.path());
        global
            .set(
                "llm/accounts/gemini",
                &serde_json::json!({ "provider": "gemini", "api_key": "gm-provider" }).to_string(),
            )
            .unwrap();
        let keys = VaultKeys::new(Some(global));
        assert_eq!(
            keys.api_key(Provider::Gemini).as_deref(),
            Some("gm-provider")
        );
    }

    #[test]
    fn no_vault_means_no_voice_keys() {
        assert_eq!(VaultKeys::new(None).api_key(Provider::OpenAi), None);
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
        let keys = VaultKeys::new(Some(global));
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

    mod providers {
        use super::*;
        use crate::agent_llm::{AgentProviders, KeySource};
        use crate::subscription::{CredentialScope, PlanLogins, Subscriptions};
        use crate::workspace_admission::WorkspaceAdmission;
        use workforce_desk::providers::{route, spec, PROVIDERS};

        fn accounts(vault: &Arc<Vault>) -> HubAccounts {
            HubAccounts {
                vault: Some(Arc::clone(vault)),
                known: Arc::new(Vec::new),
                google_login: Arc::new(|| None),
                plans: None,
                calls: Arc::new(crate::desk_key_check::Answers::passing()),
                own_ports: Vec::new(),
            }
        }

        fn fields(pairs: &[(&str, &str)]) -> Fields {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        }

        const SERVICE_ACCOUNT: &str = r#"{"type":"service_account","project_id":"acme-ai","private_key_id":"k1","private_key":"-----BEGIN PRIVATE KEY-----\nMII\n-----END PRIVATE KEY-----\n","client_email":"desk@acme-ai.iam.gserviceaccount.com","token_uri":"https://oauth2.googleapis.com/token"}"#;

        /// What Desk sends to connect each key and local provider.
        fn connect_fields(id: &str) -> Fields {
            match id {
                "anthropic" => fields(&[("api_key", "sk-ant-api03-desk")]),
                "bedrock" => fields(&[
                    ("access_key_id", "AKIADESK"),
                    ("secret_access_key", "desk/secret+key"),
                    ("region", "eu-west-2"),
                ]),
                "vertex-anthropic" => fields(&[("service_account_json", SERVICE_ACCOUNT)]),
                "ollama" => fields(&[("url", "http://127.0.0.1:9/")]),
                "vllm" => fields(&[("url", "http://127.0.0.1:9"), ("api_key", "vk-desk")]),
                id => fields(&[("api_key", &format!("{id}-desk-key"))]),
            }
        }

        fn kind(id: &str) -> ProviderKind {
            ProviderKind::parse(id).unwrap()
        }

        fn agent_providers(vault: &Arc<Vault>) -> AgentProviders {
            let config = WorkforceConfig::default();
            AgentProviders::new(
                None,
                Some(Arc::clone(vault)),
                PlanLogins::new(
                    Subscriptions::new(&config),
                    CredentialScope::system(Arc::clone(vault), &config.subscription),
                ),
            )
        }

        /// The variables that would put `kind`'s key ahead of the vault.
        fn env_set(kind: ProviderKind) -> bool {
            let vars: &[&str] = match kind {
                ProviderKind::Ollama | ProviderKind::Vllm => &[],
                ProviderKind::Bedrock => &["AWS_ACCESS_KEY_ID"],
                ProviderKind::VertexAnthropic => &[],
                kind => crate::credentials::provider_env(kind),
            };
            vars.iter().any(|var| std::env::var(var).is_ok())
        }

        #[tokio::test]
        async fn every_connected_key_or_local_provider_is_read_and_built_by_its_agents() {
            for spec in PROVIDERS.iter().filter(|s| s.group != Group::Plan) {
                let dir = tempfile::tempdir().unwrap();
                let global = vault(dir.path());
                let accounts = accounts(&global);
                accounts
                    .connect(spec, &connect_fields(spec.id))
                    .await
                    .unwrap_or_else(|e| panic!("{}: {e}", spec.id));
                let kind = kind(spec.id);
                let providers = agent_providers(&global);
                if !env_set(kind) {
                    assert_eq!(
                        providers.metered_key_source(kind),
                        Some(KeySource::GlobalVault),
                        "{}",
                        spec.id
                    );
                }
                let model = spec.model_ids().next().unwrap_or("llama4:8b");
                providers
                    .provider(kind, Some(model), None, false, None)
                    .await
                    .unwrap_or_else(|e| panic!("{} does not build: {e}", spec.id));
                if spec.group == Group::Key {
                    assert!(accounts.standing(spec).await.connected, "{}", spec.id);
                }
                accounts.disconnect(spec).await.unwrap();
                assert!(global.list(Some("llm/accounts/")).is_empty(), "{}", spec.id);
            }
        }

        #[tokio::test]
        async fn bedrock_on_an_aws_profile_keeps_its_name_and_region_and_no_key() {
            let dir = tempfile::tempdir().unwrap();
            let global = vault(dir.path());
            let accounts = accounts(&global);
            let bedrock = spec("bedrock").unwrap();
            let given = workforce_desk::providers::checked(
                bedrock,
                fields(&[("profile", "work-sso"), ("region", "eu-west-2")]),
            )
            .unwrap();
            accounts.connect(bedrock, &given).await.unwrap();
            let record: serde_json::Value =
                serde_json::from_str(&global.get("llm/accounts/bedrock").unwrap()).unwrap();
            assert_eq!(
                record,
                serde_json::json!({
                    "provider": "bedrock",
                    "api_key": "none",
                    "region": "eu-west-2",
                    "profile": "work-sso",
                })
            );
            assert!(accounts.standing(bedrock).await.connected);
            let providers = agent_providers(&global);
            providers
                .provider(
                    ProviderKind::Bedrock,
                    Some("anthropic.claude-sonnet-5-5"),
                    None,
                    false,
                    None,
                )
                .await
                .expect("builds without reaching AWS");
            // Keys still connect as before, over the profile.
            accounts
                .connect(bedrock, &connect_fields("bedrock"))
                .await
                .unwrap();
            let record: serde_json::Value =
                serde_json::from_str(&global.get("llm/accounts/bedrock").unwrap()).unwrap();
            assert_eq!(record["api_key"], "AKIADESK:desk/secret+key");
            assert!(record.get("profile").is_none());
            let err = accounts
                .connect(bedrock, &fields(&[("profile", "work sso]")]))
                .await
                .unwrap_err();
            assert!(err.to_string().contains("AWS profile name"), "{err}");
        }

        #[tokio::test]
        async fn accounts_hold_what_each_client_reads_beside_its_key() {
            let dir = tempfile::tempdir().unwrap();
            let global = vault(dir.path());
            let accounts = accounts(&global);
            let record = |name: &str| -> serde_json::Value {
                serde_json::from_str(&global.get(&format!("llm/accounts/{name}")).unwrap()).unwrap()
            };
            for id in ["bedrock", "vertex-anthropic", "vllm", "ollama"] {
                accounts
                    .connect(spec(id).unwrap(), &connect_fields(id))
                    .await
                    .unwrap();
            }
            assert_eq!(
                record("bedrock"),
                serde_json::json!({
                    "provider": "bedrock",
                    "api_key": "AKIADESK:desk/secret+key",
                    "region": "eu-west-2",
                })
            );
            let vertex = record("vertex-anthropic");
            assert_eq!(vertex["project"], "acme-ai", "named by the key");
            assert_eq!(vertex["location"], "global");
            assert_eq!(vertex["api_key"], SERVICE_ACCOUNT);
            assert_eq!(
                record("vllm"),
                serde_json::json!({
                    "provider": "vllm",
                    "api_key": "http://127.0.0.1:9",
                    "token": "vk-desk",
                })
            );
            assert_eq!(record("ollama")["api_key"], "http://127.0.0.1:9");
            assert!(
                accounts.standing(spec("vllm").unwrap()).await.connected,
                "an addressed vLLM is connected"
            );
            assert!(
                !accounts.standing(spec("ollama").unwrap()).await.connected,
                "an Ollama that does not answer at its address is not"
            );
        }

        #[test]
        fn the_address_desk_set_beats_the_environments_and_the_default() {
            let dir = tempfile::tempdir().unwrap();
            let global = vault(dir.path());
            let accounts = accounts(&global);
            let env = |var: &str| match var {
                "OLLAMA_URL" => Some("http://env-ollama:1".to_string()),
                "VLLM_URL" => Some("http://env-vllm:1".to_string()),
                "VLLM_API_KEY" => Some("env-token".to_string()),
                _ => None,
            };
            let nothing = |_: &str| None;
            assert_eq!(
                accounts.endpoint_with(ProviderKind::Ollama, nothing),
                Some(("http://localhost:11434".into(), None))
            );
            assert_eq!(accounts.endpoint_with(ProviderKind::Vllm, nothing), None);
            assert_eq!(
                accounts.endpoint_with(ProviderKind::Ollama, env),
                Some(("http://env-ollama:1".into(), None))
            );
            for (id, kind) in [
                ("ollama", ProviderKind::Ollama),
                ("vllm", ProviderKind::Vllm),
            ] {
                let record = account(
                    kind,
                    &fields(&[("url", "http://desk-set:9/")]),
                    &accounts.google_login,
                )
                .unwrap();
                global
                    .set(&format!("llm/accounts/{id}"), &record.to_string())
                    .unwrap();
                let (url, _) = accounts.endpoint_with(kind, env).unwrap();
                assert_eq!(url, "http://desk-set:9", "{id}");
            }
            assert_eq!(
                accounts
                    .endpoint_with(ProviderKind::Vllm, env)
                    .unwrap()
                    .1
                    .as_deref(),
                Some("env-token"),
                "a key the account lacks still comes from the environment"
            );
        }

        #[tokio::test]
        async fn connecting_replaces_the_record_agents_read_and_disconnecting_clears_every_one() {
            let dir = tempfile::tempdir().unwrap();
            let global = vault(dir.path());
            let old = serde_json::json!({ "provider": "zai", "api_key": "old-key" }).to_string();
            global.set("llm/accounts/main", &old).unwrap();
            global
                .set(
                    "llm/accounts/spare",
                    &serde_json::json!({ "provider": "zai", "api_key": "spare" }).to_string(),
                )
                .unwrap();
            let accounts = accounts(&global);
            let zai = spec("zai").unwrap();
            accounts
                .connect(zai, &fields(&[("api_key", "new-key")]))
                .await
                .unwrap();
            assert_eq!(
                global.find_account_api_key("zai").as_deref(),
                Some("new-key")
            );
            assert!(
                global.get("llm/accounts/zai").is_none(),
                "the read record is rewritten"
            );
            accounts.disconnect(zai).await.unwrap();
            assert!(global.find_account_api_key("zai").is_none());
            assert!(!accounts.standing(zai).await.connected);
        }

        #[tokio::test]
        async fn a_key_is_tried_on_its_default_model_and_one_refused_is_never_stored() {
            let dir = tempfile::tempdir().unwrap();
            let global = vault(dir.path());
            let anthropic = spec("anthropic").unwrap();
            let typed = "sk-ant-api03-typed-wrong";
            let refused = Arc::new(crate::desk_key_check::Answers::failing(&format!(
                "Anthropic API error (401 Unauthorized): invalid x-api-key {typed}"
            )));
            let accounts = HubAccounts {
                calls: refused.clone(),
                ..accounts(&global)
            };
            let e = accounts
                .connect(anthropic, &fields(&[("api_key", typed)]))
                .await
                .unwrap_err();
            let AccountError::Refused(said) = &e else {
                panic!("a refused key is the person's to fix: {e:?}");
            };
            assert!(
                said.starts_with("Anthropic didn't accept that key"),
                "{said}"
            );
            assert!(!said.contains(typed), "the key is never repeated: {said}");
            assert!(global.find_account_api_key("anthropic").is_none());
            assert!(!accounts.standing(anthropic).await.connected);
            assert_eq!(
                *refused.asked.lock(),
                vec![format!("anthropic {}", anthropic.default_model().unwrap())]
            );

            // One the provider answers is stored, and stands connected.
            let passing = HubAccounts {
                calls: Arc::new(crate::desk_key_check::Answers::passing()),
                own_ports: Vec::new(),
                ..accounts
            };
            passing
                .connect(anthropic, &fields(&[("api_key", "sk-ant-api03-right")]))
                .await
                .unwrap();
            assert!(passing.standing(anthropic).await.connected);
        }

        #[test]
        fn a_region_location_or_project_is_only_a_name_never_part_of_an_address() {
            for ok in ["us-east-1", "global", "europe-west4", "acme-ai-123"] {
                assert_eq!(
                    place_name("region", Some(ok.into())).unwrap().as_deref(),
                    Some(ok)
                );
            }
            assert_eq!(place_name("region", None).unwrap(), None);
            for bad in [
                "evil.example.com/x",
                "us-east-1.attacker.net",
                "us-east-1#",
                "a/b",
                "US-EAST-1",
                "",
                "x@y",
            ] {
                assert!(
                    matches!(
                        place_name("region", Some(bad.into())),
                        Err(AccountError::Invalid(_))
                    ),
                    "{bad}"
                );
            }
            let bedrock = |region: &str| {
                account(
                    kind("bedrock"),
                    &fields(&[
                        ("access_key_id", "AKIA"),
                        ("secret_access_key", "s"),
                        ("region", region),
                    ]),
                    &(Arc::new(|| None) as GoogleLogin),
                )
            };
            assert!(bedrock("eu-west-2").is_ok());
            assert!(bedrock("169.254.169.254/latest").is_err());
        }

        #[test]
        fn an_address_is_said_back_without_its_user_or_password() {
            assert_eq!(
                shown_url("http://sam:hunter2@gpu.lan:8000/v1"),
                "http://gpu.lan:8000/v1"
            );
            assert_eq!(shown_url("not a url"), "that address");
        }

        #[test]
        fn the_hubs_own_ports_on_this_computer_are_never_a_model_server() {
            let own = ports_of(["127.0.0.1:8080", "0.0.0.0:3000"]);
            assert_eq!(own, vec![8080, 3000]);
            for url in [
                "http://127.0.0.1:8080",
                "http://localhost:3000/v1",
                "http://[::1]:8080",
                "http://0.0.0.0:3000",
                "http://127.0.0.2:8080",
            ] {
                assert!(is_own(url, &own), "{url}");
            }
            for url in ["http://127.0.0.1:8000", "http://gpu.lan:8080", "nope"] {
                assert!(!is_own(url, &own), "{url}");
            }
        }

        #[tokio::test]
        async fn the_hub_itself_is_refused_as_a_vllm_server_before_any_call() {
            let dir = tempfile::tempdir().unwrap();
            let global = vault(dir.path());
            let calls = Arc::new(crate::desk_key_check::Answers::passing());
            let accounts = HubAccounts {
                calls: calls.clone(),
                own_ports: vec![8080],
                ..accounts(&global)
            };
            let e = accounts
                .connect(
                    spec("vllm").unwrap(),
                    &fields(&[("url", "http://localhost:8080")]),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(&e, AccountError::Invalid(m) if m.contains("Desk itself")),
                "{e:?}"
            );
            assert!(calls.asked.lock().is_empty());
            assert!(global.find_account("vllm").is_none());
        }

        #[tokio::test]
        async fn without_a_vault_no_test_call_is_made() {
            let calls = Arc::new(crate::desk_key_check::Answers::passing());
            let e = HubAccounts {
                vault: None,
                known: Arc::new(Vec::new),
                google_login: Arc::new(|| None),
                plans: None,
                calls: calls.clone(),
                own_ports: Vec::new(),
            }
            .connect(spec("grok").unwrap(), &fields(&[("api_key", "k")]))
            .await
            .unwrap_err();
            assert!(matches!(e, AccountError::Unavailable(_)), "{e:?}");
            assert!(calls.asked.lock().is_empty());
        }

        #[tokio::test]
        async fn a_vllm_server_that_doesnt_answer_is_not_stored() {
            let dir = tempfile::tempdir().unwrap();
            let global = vault(dir.path());
            let accounts = HubAccounts {
                calls: Arc::new(crate::desk_key_check::Answers::failing("down")),
                ..accounts(&global)
            };
            let e = accounts
                .connect(
                    spec("vllm").unwrap(),
                    &fields(&[("url", "http://127.0.0.1:9")]),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(&e, AccountError::Refused(m) if m.contains("http://127.0.0.1:9")),
                "{e:?}"
            );
            let e = accounts
                .connect(
                    spec("vllm").unwrap(),
                    &fields(&[("url", "http://sam:hunter2@127.0.0.1:9")]),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(&e, AccountError::Refused(m) if !m.contains("hunter2") && !m.contains("sam")),
                "{e:?}"
            );
            assert!(global.find_account("vllm").is_none());
        }

        #[tokio::test]
        async fn a_plan_is_connected_by_its_login_and_disconnecting_signs_it_out() {
            let dir = tempfile::tempdir().unwrap();
            let global = vault(dir.path());
            let accounts = accounts(&global);
            let claude = spec("claude-code").unwrap();
            assert!(!accounts.standing(claude).await.connected);
            global
                .set(
                    SubscriptionKind::Claude.vault_key(),
                    "sk-ant-oat01-desk-login",
                )
                .unwrap();
            assert!(accounts.standing(claude).await.connected);
            assert!(!accounts.standing(spec("chatgpt").unwrap()).await.connected);
            accounts.disconnect(claude).await.unwrap();
            assert!(global.get(SubscriptionKind::Claude.vault_key()).is_none());
            assert!(!accounts.standing(claude).await.connected);
        }

        /// What `gcloud auth application-default login` writes, as a test
        /// stands it in.
        const GOOGLE_LOGIN: &str = r#"{"type":"authorized_user","client_id":"desk-test.apps.googleusercontent.com","client_secret":"desk-test-client-secret","refresh_token":"desk-test-refresh-token"}"#;

        #[tokio::test]
        async fn vertex_connects_on_the_persons_google_login_without_copying_it() {
            let dir = tempfile::tempdir().unwrap();
            let global = vault(dir.path());
            let login = dir.path().join("application_default_credentials.json");
            std::fs::write(&login, GOOGLE_LOGIN).unwrap();
            let accounts = HubAccounts {
                google_login: Arc::new(move || Some(login.clone())),
                ..accounts(&global)
            };
            let vertex = spec("vertex-anthropic").unwrap();
            let checked = workforce_desk::providers::checked(
                vertex,
                fields(&[("service_account_json", "  "), ("project", "acme-ai")]),
            )
            .expect("no key is needed");
            accounts.connect(vertex, &checked).await.unwrap();
            let stored = global.get("llm/accounts/vertex-anthropic").unwrap();
            assert!(!stored.contains("refresh"), "{stored}");
            assert!(!stored.contains("client_secret"), "{stored}");
            let record: serde_json::Value = serde_json::from_str(&stored).unwrap();
            assert_eq!(
                record,
                serde_json::json!({
                    "provider": "vertex-anthropic",
                    "api_key": CHAIN_SENTINEL,
                    "project": "acme-ai",
                    "location": "global",
                })
            );
            assert!(accounts.standing(vertex).await.connected);
            agent_providers(&global)
                .provider(
                    kind("vertex-anthropic"),
                    Some("claude-sonnet-5-5"),
                    None,
                    false,
                    None,
                )
                .await
                .expect("an agent builds on the login");
            accounts.disconnect(vertex).await.unwrap();
            assert!(!accounts.standing(vertex).await.connected);
        }

        #[tokio::test]
        async fn a_google_login_needs_a_project_and_a_login() {
            let dir = tempfile::tempdir().unwrap();
            let global = vault(dir.path());
            let vertex = spec("vertex-anthropic").unwrap();
            let login = dir.path().join("application_default_credentials.json");
            std::fs::write(&login, GOOGLE_LOGIN).unwrap();
            let signed_in = HubAccounts {
                google_login: Arc::new(move || Some(login.clone())),
                ..accounts(&global)
            };
            let e = signed_in.connect(vertex, &Fields::new()).await.unwrap_err();
            assert!(
                matches!(&e, AccountError::Invalid(m) if m.contains("Project ID")),
                "{e}"
            );
            let e = accounts(&global)
                .connect(vertex, &fields(&[("project", "acme-ai")]))
                .await
                .unwrap_err();
            assert!(
                matches!(&e, AccountError::Invalid(m) if m.contains("gcloud auth application-default login")),
                "{e}"
            );
            assert!(global.list(None).is_empty());
        }

        #[tokio::test]
        async fn a_wrong_credential_is_refused_without_repeating_it() {
            let dir = tempfile::tempdir().unwrap();
            let global = vault(dir.path());
            let accounts = accounts(&global);
            let refused = |e: AccountError, secret: &str| {
                assert!(matches!(e, AccountError::Invalid(_)), "{e:?}");
                assert!(!e.to_string().contains(secret), "{e}");
            };
            let oat = "sk-ant-oat01-pasted-plan-token";
            refused(
                accounts
                    .connect(spec("anthropic").unwrap(), &fields(&[("api_key", oat)]))
                    .await
                    .unwrap_err(),
                oat,
            );
            let half = "{\"private_key\": \"-----BEGIN";
            refused(
                accounts
                    .connect(
                        spec("vertex-anthropic").unwrap(),
                        &fields(&[("service_account_json", half)]),
                    )
                    .await
                    .unwrap_err(),
                "BEGIN",
            );
            let no_project = r#"{"type":"service_account","private_key":"-----BEGIN"}"#;
            refused(
                accounts
                    .connect(
                        spec("vertex-anthropic").unwrap(),
                        &fields(&[("service_account_json", no_project)]),
                    )
                    .await
                    .unwrap_err(),
                "BEGIN",
            );
            refused(
                accounts
                    .connect(
                        spec("ollama").unwrap(),
                        &fields(&[("url", "localhost:11434")]),
                    )
                    .await
                    .unwrap_err(),
                "\u{0}",
            );
            assert!(global.list(None).is_empty());
            let unavailable = HubAccounts {
                vault: None,
                known: Arc::new(Vec::new),
                google_login: Arc::new(|| None),
                plans: None,
                calls: Arc::new(crate::desk_key_check::Answers::passing()),
                own_ports: Vec::new(),
            }
            .connect(spec("grok").unwrap(), &fields(&[("api_key", "k")]))
            .await
            .unwrap_err();
            assert!(matches!(unavailable, AccountError::Unavailable(_)));
        }

        fn admission() -> WorkspaceAdmission {
            let mut config = WorkforceConfig::default();
            config.subscription.chatgpt_direct = true;
            WorkspaceAdmission::new(
                Arc::new(config),
                "desk-agent",
                Arc::new(parking_lot::RwLock::new("anthropic".to_string())),
            )
        }

        fn agent_toml(provider: &str, model: &str, effort: Option<&str>) -> String {
            let effort = effort
                .map(|e| format!("effort = {e:?}\n"))
                .unwrap_or_default();
            format!("[llm]\nprovider = {provider:?}\ndefault_model = {model:?}\n{effort}")
        }

        #[test]
        fn every_model_desk_offers_loads_on_its_route_at_every_effort_it_takes() {
            let admission = admission();
            let accounts = HubAccounts {
                vault: None,
                known: Arc::new(Vec::new),
                google_login: Arc::new(|| None),
                plans: None,
                calls: Arc::new(crate::desk_key_check::Answers::passing()),
                own_ports: Vec::new(),
            };
            for spec in PROVIDERS {
                let models: Vec<&str> = match spec.lists_installed() {
                    true => vec!["llama4:8b"],
                    false => spec.model_ids().collect(),
                };
                for model in models {
                    let efforts = accounts.model_efforts(spec.id, model);
                    let provider = route(spec.id, model);
                    for effort in std::iter::once(None).chain(efforts.iter().copied().map(Some)) {
                        let findings = admission
                            .review_toml(&agent_toml(&provider, model, effort))
                            .unwrap_or_else(|e| panic!("{provider} {model} {effort:?}: {e}"));
                        assert!(
                            findings.is_empty(),
                            "{provider} {model} {effort:?}: {findings:?}"
                        );
                    }
                }
            }
        }

        /// A model a plan lists now that Desk's table doesn't (one released
        /// since) loads as Desk offers it, once the plan's list is read, and
        /// so does Fast mode; before the list is read it still loads.
        #[test]
        fn a_model_released_since_and_fast_mode_load() {
            let admission = admission();
            let toml = |provider, model, effort| {
                format!("{}fast = true\n", agent_toml(provider, model, effort))
            };
            let before = admission
                .review_toml(&toml("chatgpt", "gpt-test-released", Some("high")))
                .expect("it loads before the plan's list is read");
            assert!(before.iter().all(|f| !f.is_refused()), "{before:?}");
            let body: serde_json::Value = serde_json::from_str(include_str!(
                "../../agent-runtime/src/llm/testdata/chatgpt/models_live.json"
            ))
            .unwrap();
            workforce_agent::llm::responses::ChatGptModel::learn(
                "desk-login",
                &workforce_agent::llm::responses::parse_catalog(&body),
            );
            for (provider, model, effort) in [
                ("chatgpt", "gpt-6.2-nova", Some("high")),
                ("claude-code", "claude-fable-5-2", Some("max")),
            ] {
                let findings = admission
                    .review_toml(&toml(provider, model, effort))
                    .unwrap_or_else(|e| panic!("{provider} {model}: {e}"));
                assert!(findings.is_empty(), "{provider} {model}: {findings:?}");
            }
        }

        #[test]
        fn a_level_the_provider_does_not_take_is_refused_naming_the_ones_it_does() {
            let admission = admission();
            let refusal = |provider: &str, effort: &str| {
                admission
                    .review_toml(&agent_toml(provider, "m", Some(effort)))
                    .unwrap_err()
                    .to_string()
            };
            assert_eq!(
                refusal("claude-code", "xhigh"),
                "effort = \"xhigh\" is not a level claude-code takes: use one of low, medium, high, max"
            );
            assert_eq!(
                refusal("chatgpt", "max"),
                "effort = \"max\" is not a level chatgpt takes: use one of low, medium, high, xhigh"
            );
            assert!(refusal("anthropic", "extreme")
                .contains("use one of low, medium, high, xhigh, max"));
            assert!(refusal("grok", "extreme").contains("is not an effort level"));
            let ignored = admission
                .review_toml(&agent_toml("grok", "grok-4.7", Some("high")))
                .unwrap();
            assert_eq!(ignored.len(), 1, "{ignored:?}");
            assert!(ignored[0].message.contains("grok has no effort control"));
        }

        #[test]
        fn bedrocks_open_models_take_no_effort_and_a_level_on_them_is_ignored_not_refused() {
            let accounts = HubAccounts {
                vault: None,
                known: Arc::new(Vec::new),
                google_login: Arc::new(|| None),
                plans: None,
                calls: Arc::new(crate::desk_key_check::Answers::passing()),
                own_ports: Vec::new(),
            };
            let bedrock = spec("bedrock").unwrap();
            let unhonoured: Vec<&str> = bedrock
                .model_ids()
                .filter(|m| accounts.model_efforts("bedrock", m).is_empty())
                .collect();
            assert_eq!(
                unhonoured,
                [
                    "qwen.qwen3-coder-480b-a35b-v1:0",
                    "deepseek.v3.2",
                    "zai.glm-5"
                ]
            );
            for other in PROVIDERS.iter().filter(|s| s.id != "bedrock") {
                for model in other.model_ids() {
                    assert_eq!(
                        accounts.model_efforts(other.id, model),
                        accounts.efforts(other.id),
                        "{} {model}",
                        other.id
                    );
                }
            }
            let admission = admission();
            let ignored = admission
                .review_toml(&agent_toml(
                    "bedrock:deepseek",
                    "deepseek.v3.2",
                    Some("high"),
                ))
                .unwrap();
            assert_eq!(ignored.len(), 1, "{ignored:?}");
            assert_eq!(
                ignored[0].message,
                "deepseek.v3.2 on bedrock has no effort control, so effort = \"high\" is ignored"
            );
            let refused = admission
                .review_toml(&agent_toml(
                    "bedrock:deepseek",
                    "deepseek.v3.2",
                    Some("extreme"),
                ))
                .unwrap_err();
            assert!(
                refused.to_string().contains("is not an effort level"),
                "{refused}"
            );
            assert!(admission
                .review_toml(&agent_toml(
                    "bedrock:anthropic",
                    "anthropic.claude-opus-5-5",
                    Some("max")
                ))
                .unwrap()
                .is_empty());
        }

        #[test]
        fn the_hub_knows_every_model_desk_offers() {
            let pricing = workforce_agent::llm::DEFAULT_PRICING_TOML;
            let tiers = workforce_agent::llm::tiers::TierRegistry::new(None);
            for spec in PROVIDERS {
                for model in spec.model_ids() {
                    let priced = [format!("\"{model}\""), format!("\"{}:{model}\"", spec.id)]
                        .iter()
                        .any(|quoted| pricing.contains(quoted.as_str()));
                    let tier_key = route(spec.id, model).replace(':', ".");
                    let tiered =
                        workforce_agent::llm::tiers::ModelTier::ALL
                            .into_iter()
                            .any(|tier| {
                                tiers.resolve_for_tier(tier, &tier_key).as_deref() == Some(model)
                            });
                    assert!(
                        priced || tiered,
                        "{} {model} is not one the hub knows",
                        spec.id
                    );
                }
            }
        }
    }
}
