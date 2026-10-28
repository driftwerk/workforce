//! Hub adapter for `workforce-desk`: the session registry in Postgres,
//! system-admin-only access (a desk session runs commands as the hub's
//! user), the vault keys behind cloud voice sessions and spoken replies, the
//! credentials of the providers a Desk agent thinks with, and the `/api/desk`
//! routes on the gateway.

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
    is_claude_oat, DeskRestore, LlmSpeechConfig, VaultActor, WorkforceConfig,
};
use workforce_common::subscription::SubscriptionKind;
use workforce_desk::local::{self, Server};
use workforce_desk::providers::Group;
use workforce_desk::{
    AccountError, Accounts, Audio, Authorize, Desk, DeskStore, Fields, KeySource, Options,
    Provider, Realtime, RestorePolicy, Speak, SpeakError, Spec, Speech, Standing,
};

use crate::agent_llm::vault_provider_name;
use crate::credentials::{how_to_add, Caller, Legacy};
use crate::desk_tools::DeskTool;
use crate::gateway::{resolve_caller_context, GatewayState};
use crate::hub_core::formatting::normalize_for_tts;
use crate::hub_core::logins::store as login_store;
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
}

type GoogleLogin = Arc<dyn Fn() -> Option<PathBuf> + Send + Sync>;

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
        ProviderKind::Bedrock => {
            extras.region = field(fields, "region").map(str::to_string);
            format!(
                "{}:{}",
                required("access_key_id")?,
                required("secret_access_key")?
            )
        }
        ProviderKind::VertexAnthropic if field(fields, "service_account_json").is_none() => {
            extras.project = Some(field(fields, "project").map(str::to_string).ok_or_else(
                || {
                    AccountError::Invalid(
                        "using your Google login needs the Project ID: a login names no project"
                            .into(),
                    )
                },
            )?);
            if google_login().is_none() {
                return Err(AccountError::Invalid(
                    "no Google login was found on this Mac: run gcloud auth \
                     application-default login, or paste a service account key"
                        .into(),
                ));
            }
            extras.location = Some(field(fields, "location").unwrap_or("global").to_string());
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
            extras.project = field(fields, "project")
                .map(str::to_string)
                .or_else(|| source.project_id());
            if extras.project.is_none() {
                return Err(AccountError::Invalid(
                    "the key names no project: give the Project ID".into(),
                ));
            }
            extras.location = Some(field(fields, "location").unwrap_or("global").to_string());
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
            .ok_or_else(|| AccountError::Unavailable("the hub has no vault".into()))
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

    async fn write(&self, path: &str, value: &str) -> Result<(), AccountError> {
        let vault = self.vault()?;
        let known = Arc::clone(&self.known);
        login_store::set(vault, path, value, move || known(), VaultActor::RestOwner)
            .await
            .map_err(|e| {
                tracing::warn!(path, error = %e, "desk provider credential not stored");
                AccountError::Internal(format!("saving {path} failed"))
            })
    }

    async fn delete(&self, path: &str) -> Result<(), AccountError> {
        login_store::delete(self.vault()?, path, VaultActor::RestOwner)
            .await
            .map(drop)
            .map_err(|e| {
                tracing::warn!(path, error = %e, "desk provider credential not removed");
                AccountError::Internal(format!("removing {path} failed"))
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
            return Standing {
                connected: vault
                    .get(kind.vault_key())
                    .is_some_and(|v| !v.trim().is_empty()),
                installed: Vec::new(),
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
            };
        }
        if spec.group != Group::Local {
            return Standing {
                connected: vault
                    .find_account_api_key(name)
                    .is_some_and(|key| !key.trim().is_empty()),
                installed: Vec::new(),
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
        Arc::new(HubAccounts {
            vault: state.vault.clone(),
            known: {
                let state = Arc::clone(state);
                Arc::new(move || crate::gateway::login_vaults(&state))
            },
            google_login: Arc::new(adc_file),
        }),
        Arc::new(crate::desk_conversations::HubConversations(Arc::clone(
            state,
        ))),
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
                ProviderKind::VertexAnthropic => &["GOOGLE_APPLICATION_CREDENTIALS"],
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
