//! How an agent builds its LLM providers of every kind: metered providers on
//! their API keys, subscription providers on the agent's logins. One place, so the
//! boot, hot-reload and `system.set_provider` paths build the same providers.

use std::collections::BTreeMap;
use std::sync::Arc;

use eyre::Result;
use tracing::{info, warn};
use workforce_agent::llm::failover::FailoverProvider;
use workforce_agent::llm::pool::PooledProvider;
use workforce_agent::llm::provider::{
    chain_credential_fallback, create_provider_with, provider_env_key, provider_env_var_hint,
    AccountExtras, LlmProvider, ProviderKind, ProviderParseError,
};
use workforce_agent::llm::tiers::{extract_routing_provider, Provider};
use workforce_common::settings::{is_claude_oat, oat_refusal, LlmConfig};

use crate::resolved_config::ResolvedConfig;
use crate::subscription::{chain_specs, PlanLogins};
use crate::vault::Vault;

/// Vault `llm/accounts/*` provider label for a [`ProviderKind`] — the value
/// matched against each stored account's `provider` field.
pub(crate) fn vault_provider_name(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::OpenRouter => "openrouter",
        ProviderKind::Grok => "grok",
        ProviderKind::Gemini => "gemini",
        ProviderKind::Ollama => "ollama",
        ProviderKind::Vllm => "vllm",
        ProviderKind::Cerebras => "cerebras",
        ProviderKind::Zai => "zai",
        ProviderKind::Bedrock => "bedrock",
        ProviderKind::VertexAnthropic => "vertex-anthropic",
        ProviderKind::ChatGpt | ProviderKind::ClaudeCode | ProviderKind::Codex => {
            kind.canonical_name()
        }
    }
}

/// The provider of the `llm/accounts/*` record at `path` (vault and key),
/// which names
/// `provider` and holds `key`. A provider this hub no longer runs, or an
/// `anthropic` account holding a Claude subscription token, fails the agent's
/// load rather than reaching the API client; an unknown name is `anthropic`.
pub(crate) fn account_kind(path: &str, provider: &str, key: &str) -> Result<ProviderKind> {
    let kind = match ProviderKind::parse(provider) {
        Ok(kind) => kind,
        Err(removed @ ProviderParseError::Removed { .. }) => {
            return Err(eyre::eyre!("{path}: {removed}"))
        }
        Err(ProviderParseError::Unknown(_)) => ProviderKind::Anthropic,
    };
    match kind == ProviderKind::Anthropic && is_claude_oat(key) {
        true => Err(eyre::eyre!(oat_refusal(&format!(
            "{path}: the anthropic account's key"
        )))),
        false => Ok(kind),
    }
}

/// The provider a routed model spec names: a `provider:model` or
/// `provider:tier` prefix, or a bare provider name (a failover entry).
fn routed_provider(spec: &str) -> Option<String> {
    extract_routing_provider(spec).or_else(|| Provider::from_name(spec).map(|_| spec.to_string()))
}

/// The providers `specs` route to beyond the primary, by routing name. The
/// primary's own metered kind rides the pool; a subscription kind is always
/// listed, since the chain never hands a subscription route to the primary.
pub fn external_routes<'a>(
    primary: ProviderKind,
    specs: impl IntoIterator<Item = &'a String>,
) -> BTreeMap<String, ProviderKind> {
    specs
        .into_iter()
        .filter_map(|spec| routed_provider(spec))
        .filter_map(|name| match ProviderKind::from_str_loose(&name) {
            Some(kind) => Some((name, kind)),
            None => {
                warn!(provider = %name, "Unknown provider in routing config, skipping");
                None
            }
        })
        .filter(|(_, kind)| {
            kind.is_subscription() || vault_provider_name(*kind) != vault_provider_name(primary)
        })
        .collect()
}

/// `kind`'s key from an environment variable someone set: for Ollama and
/// vLLM, whose [`provider_env_key`] falls back on a built-in local URL, only
/// a URL set in the environment, so a vault account's URL beats the default.
fn explicit_env_key(kind: ProviderKind) -> Option<String> {
    match kind {
        ProviderKind::Ollama => std::env::var("OLLAMA_URL").ok(),
        ProviderKind::Vllm => std::env::var("VLLM_URL").ok(),
        _ => provider_env_key(kind),
    }
    .filter(|key| !key.trim().is_empty())
}

/// Where a metered provider's credential comes from, in the order
/// [`AgentProviders::metered_key`] looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeySource {
    /// The provider's environment variable on the hub.
    Env,
    /// An `llm/accounts/*` record in the agent's vault.
    AgentVault,
    /// An `llm/accounts/*` record in the hub's shared vault.
    GlobalVault,
    /// The provider's own credential chain (Bedrock, Vertex).
    ProviderChain,
}

impl KeySource {
    /// Where the key sits, as a notice says it.
    pub fn describe(self, kind: ProviderKind) -> String {
        match self {
            Self::Env => format!("the {} env var", provider_env_var_hint(kind)),
            Self::AgentVault => "an account in this agent's vault".to_string(),
            Self::GlobalVault => "an account in the shared vault".to_string(),
            Self::ProviderChain => "the provider's own credential chain".to_string(),
        }
    }
}

/// An agent's credentials for building providers.
pub struct AgentProviders {
    vault: Option<Arc<Vault>>,
    global_vault: Option<Arc<Vault>>,
    plans: PlanLogins,
}

impl AgentProviders {
    pub fn new(
        vault: Option<Arc<Vault>>,
        global_vault: Option<Arc<Vault>>,
        plans: PlanLogins,
    ) -> Self {
        Self {
            vault,
            global_vault,
            plans,
        }
    }

    /// The agent's subscription logins, for providers built elsewhere
    /// (vision).
    pub fn plans(&self) -> &PlanLogins {
        &self.plans
    }

    /// Where [`Self::metered_key`] finds `kind`'s credential, without the
    /// credential: the first place it looks that holds one.
    pub fn metered_key_source(&self, kind: ProviderKind) -> Option<KeySource> {
        let name = vault_provider_name(kind);
        let held = |vault: &Option<Arc<Vault>>| {
            vault
                .as_deref()
                .is_some_and(|vault| vault.find_account_api_key(name).is_some())
        };
        if explicit_env_key(kind).is_some() {
            Some(KeySource::Env)
        } else if held(&self.vault) {
            Some(KeySource::AgentVault)
        } else if held(&self.global_vault) {
            Some(KeySource::GlobalVault)
        } else if provider_env_key(kind).is_some() {
            Some(KeySource::Env)
        } else if chain_credential_fallback(kind).is_some() {
            Some(KeySource::ProviderChain)
        } else {
            None
        }
    }

    /// The credential a metered `kind` authenticates with: its env var, then
    /// the agent vault's `llm/accounts/*`, then the global vault's, then the
    /// built-in local URL (Ollama, vLLM) or the provider's credential chain
    /// (Bedrock, Vertex).
    pub fn metered_key(&self, kind: ProviderKind) -> Option<String> {
        self.metered_account(kind).map(|(key, _)| key)
    }

    /// [`Self::metered_key`], with what the vault account holds beside it.
    fn metered_account(&self, kind: ProviderKind) -> Option<(String, AccountExtras)> {
        explicit_env_key(kind)
            .map(|key| (key, AccountExtras::default()))
            .or_else(|| {
                let record = crate::vault::find_account_layered(
                    self.vault.as_deref(),
                    self.global_vault.as_deref(),
                    vault_provider_name(kind),
                )?;
                let key = record["api_key"].as_str()?.to_string();
                let extras = serde_json::from_value(record).unwrap_or_default();
                Some((key, extras))
            })
            .or_else(|| provider_env_key(kind).map(|key| (key, AccountExtras::default())))
            .or_else(|| chain_credential_fallback(kind).map(|key| (key, AccountExtras::default())))
    }

    /// The agent's provider of `kind`, serving `model` by default: a
    /// subscription provider on the agent's logins, a metered provider on
    /// `api_key`, else on its [`Self::metered_key`].
    pub async fn provider(
        &self,
        kind: ProviderKind,
        model: Option<&str>,
        thinking_budget: Option<u32>,
        zai_subscription: bool,
        api_key: Option<String>,
    ) -> Result<Arc<dyn LlmProvider>> {
        if kind.is_subscription() {
            return self.plans.provider(kind, model, thinking_budget);
        }
        let (api_key, extras) = api_key
            .map(|key| (key, AccountExtras::default()))
            .or_else(|| self.metered_account(kind))
            .ok_or_else(|| {
                eyre::eyre!(
                    "no API key for provider {}: set {} or add an account",
                    kind.canonical_name(),
                    provider_env_var_hint(kind)
                )
            })?;
        create_provider_with(
            kind,
            &api_key,
            &extras,
            model,
            thinking_budget,
            zai_subscription,
        )
        .map(Arc::from)
    }

    /// Whether an `account` joins the token pool of an agent whose provider
    /// is `primary`. A subscription primary's pool holds its login only, so
    /// a metered account rides the chain as an external instead.
    pub fn pools_as_primary(primary: ProviderKind, account: ProviderKind) -> bool {
        !primary.is_subscription() && account == ProviderKind::Anthropic
    }

    /// Switch the agent to `provider`, of `kind`: it becomes the pool's
    /// default, and the chain's externals are those `llm` and `resolved`
    /// route to beyond the new primary.
    pub async fn swap_primary(
        &self,
        pool: Option<&PooledProvider>,
        failover: Option<&FailoverProvider>,
        kind: ProviderKind,
        provider: Arc<dyn LlmProvider>,
        llm: &LlmConfig,
        resolved: &ResolvedConfig,
    ) {
        if let Some(pool) = pool {
            pool.remove("default").await;
            pool.add("default", provider).await;
        }
        if let Some(failover) = failover {
            self.register_externals(failover, external_routes(kind, chain_specs(llm, resolved)))
                .await;
        }
    }

    /// Register on `failover` the provider of each of `routes`.
    pub async fn register_externals(
        &self,
        failover: &FailoverProvider,
        routes: BTreeMap<String, ProviderKind>,
    ) {
        for (name, kind) in routes {
            match self.provider(kind, None, None, false, None).await {
                Ok(provider) => {
                    failover.add_external_provider(&name, provider).await;
                    info!(provider = %name, "External provider initialized");
                }
                Err(error) => warn!(
                    provider = %name,
                    %error,
                    "External provider unavailable for routed requests"
                ),
            }
        }
    }
}

#[async_trait::async_trait]
impl crate::hosts::ProviderHost for AgentProviders {
    async fn provider(
        &self,
        kind: ProviderKind,
        model: Option<&str>,
        thinking_budget: Option<u32>,
        zai_subscription: bool,
        api_key: Option<String>,
    ) -> Result<Arc<dyn LlmProvider>> {
        AgentProviders::provider(
            self,
            kind,
            model,
            thinking_budget,
            zai_subscription,
            api_key,
        )
        .await
    }

    async fn swap_primary(
        &self,
        pool: Option<&PooledProvider>,
        failover: Option<&FailoverProvider>,
        kind: ProviderKind,
        provider: Arc<dyn LlmProvider>,
        llm: &LlmConfig,
        resolved: &ResolvedConfig,
    ) {
        AgentProviders::swap_primary(self, pool, failover, kind, provider, llm, resolved).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subscription::{CredentialScope, Subscriptions};
    use workforce_common::settings::WorkforceConfig;

    fn routes(primary: ProviderKind, specs: &[&str]) -> Vec<(String, ProviderKind)> {
        let specs: Vec<String> = specs.iter().map(|s| s.to_string()).collect();
        external_routes(primary, &specs).into_iter().collect()
    }

    #[test]
    fn external_routes_leave_the_primarys_own_kind_to_the_pool() {
        assert_eq!(
            routes(
                ProviderKind::Gemini,
                &[
                    "gemini:cheap",
                    "anthropic:powerful",
                    "gemini-3.1-pro-preview",
                    "grok"
                ]
            ),
            [
                ("anthropic".to_string(), ProviderKind::Anthropic),
                ("grok".to_string(), ProviderKind::Grok),
            ]
        );
    }

    #[test]
    fn subscription_routes_are_always_external() {
        assert_eq!(
            routes(
                ProviderKind::ChatGpt,
                &["chatgpt:gpt-6.1-sol", "chatgpt", "gemini:cheap"]
            ),
            [
                ("chatgpt".to_string(), ProviderKind::ChatGpt),
                ("gemini".to_string(), ProviderKind::Gemini),
            ]
        );
    }

    #[test]
    fn unknown_provider_names_are_skipped() {
        assert!(routes(ProviderKind::Gemini, &["mystery:cheap", "nowhere"]).is_empty());
    }

    #[test]
    fn only_a_metered_anthropic_primary_pools_anthropic_accounts() {
        use ProviderKind::{Anthropic, ChatGpt, Gemini};
        assert!(AgentProviders::pools_as_primary(Anthropic, Anthropic));
        assert!(AgentProviders::pools_as_primary(Gemini, Anthropic));
        assert!(!AgentProviders::pools_as_primary(Anthropic, Gemini));
        assert!(!AgentProviders::pools_as_primary(ChatGpt, Anthropic));
    }

    fn system_plans(config: &WorkforceConfig, vault: &Arc<Vault>) -> PlanLogins {
        PlanLogins::new(
            Subscriptions::new(config),
            CredentialScope::system(Arc::clone(vault), &config.subscription),
        )
    }

    #[tokio::test]
    async fn providers_build_on_logins_or_keys() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(&dir.path().join("global.enc"), "test-key").unwrap());
        let providers = AgentProviders::new(
            None,
            Some(Arc::clone(&vault)),
            system_plans(&WorkforceConfig::default(), &vault),
        );
        let chatgpt = providers
            .provider(
                ProviderKind::ChatGpt,
                Some("gpt-6.1-sol"),
                None,
                false,
                None,
            )
            .await
            .expect("chatgpt needs no key: it runs on logins");
        assert_eq!(chatgpt.context_floor("gpt-6.1-sol").await, Some(500_000));
        let claude_code = providers
            .provider(ProviderKind::ClaudeCode, Some("haiku"), None, false, None)
            .await
            .expect("claude-code completions run on logins");
        assert_eq!(claude_code.context_floor("haiku").await, Some(200_000));
        providers
            .provider(ProviderKind::Gemini, None, None, false, Some("key".into()))
            .await
            .expect("a metered provider builds on the key it is given");
        let codex = providers
            .provider(ProviderKind::Codex, Some("gpt-6.1-sol"), None, false, None)
            .await
            .expect("codex completions run on logins");
        assert_eq!(codex.context_floor("gpt-6.1-sol").await, Some(272_000));
    }

    #[tokio::test]
    async fn background_chatgpt_spec_fails_over_to_claude_code_when_its_login_cools() {
        use crate::claude_code::tests::scripted::{answer, ScriptedClaude};
        use crate::subscription::test_support::{login, Metered};
        use crate::subscription::{CooldownStore, LimitSignal, LimitStatus, LoginKey};
        use workforce_agent::llm::failover::FailoverConfig;
        use workforce_agent::llm::types::LlmMessage;
        use workforce_common::subscription::SubscriptionKind;

        let dir = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(&dir.path().join("global.enc"), "test-key").unwrap());
        let account = format!("acct-{}", uuid::Uuid::new_v4());
        vault
            .set(
                SubscriptionKind::ChatGpt.vault_key(),
                &login(&account, "access-0", "rt-0"),
            )
            .unwrap();
        let token = format!("install-{}", uuid::Uuid::new_v4());
        vault
            .set(SubscriptionKind::Claude.vault_key(), &token)
            .unwrap();
        let now = chrono::Utc::now();
        CooldownStore::global().note(
            &LoginKey {
                kind: SubscriptionKind::ChatGpt,
                fingerprint: format!("{account}:user-1"),
            },
            &LimitSignal {
                status: LimitStatus::Rejected,
                resets_at: Some(now + chrono::TimeDelta::hours(2)),
                window: Some("secondary".into()),
            },
            now,
        );
        let mut config = WorkforceConfig::default();
        config.hub.data_dir = dir.path().display().to_string();
        config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        config.subscription.chatgpt_direct = true;
        let providers = AgentProviders::new(
            None,
            Some(Arc::clone(&vault)),
            system_plans(&config, &vault),
        );

        let metered = Arc::new(Metered::default());
        let chain = FailoverProvider::new(
            Arc::clone(&metered) as Arc<dyn LlmProvider>,
            FailoverConfig {
                failover: vec!["claude-code:haiku".to_string()],
                ..Default::default()
            },
        );
        let compaction_model = "chatgpt:gpt-6.1-sol".to_string();
        let failover = "claude-code:haiku".to_string();
        providers
            .register_externals(
                &chain,
                external_routes(ProviderKind::Gemini, [&compaction_model, &failover]),
            )
            .await;

        let claude = ScriptedClaude::new();
        claude.reply(None, &answer("Summarised."));
        let response = claude
            .scoped(chain.create_message(
                Some("Summarise."),
                &[LlmMessage::user("the conversation so far")],
                &[],
                Some(&compaction_model),
                None,
                false,
            ))
            .await
            .expect("the chain moves on to the Claude plan");
        assert_eq!(response.text().as_deref(), Some("Summarised."));
        let calls = claude.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].token, token, "on the install's Claude plan");
        assert_eq!(calls[0].flag("--model"), Some("haiku"));
        assert!(
            metered.0.lock().is_empty(),
            "no metered model is asked for background work the plans cover"
        );
    }
}
