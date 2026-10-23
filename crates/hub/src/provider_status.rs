//! What an agent's providers run on, for the person asking: every provider
//! its settings route to, the settings naming it, and the credential a call
//! would use now. A metered provider reports whether a key is set and where
//! it was found; a plan provider reports which login of the caller's serves
//! it, or per tier why none does. `system.llm_status` carries it, so an
//! agent asked "is my Claude login set up?" can answer from the same
//! resolver the turn uses instead of guessing from its config.

use serde::Serialize;
use workforce_agent::llm::provider::{provider_env_var_hint, ProviderKind};
use workforce_common::settings::LlmConfig;
use workforce_common::subscription::{CredentialTier, SubscriptionKind};

use crate::agent_llm::{AgentProviders, KeySource};
use crate::hub_core::subscriptions::routed_kind;
use crate::resolved_config::ResolvedConfig;

/// A setting that names a model spec, and the spec it names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SpecUse {
    pub setting: String,
    pub spec: String,
}

/// Why a tier holds no login of the caller's for a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LoginMiss {
    pub tier: CredentialTier,
    pub reason: String,
}

/// The credential a provider's calls authenticate with now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CredentialStatus {
    /// A metered provider: whether a key is set, and where it was found.
    ApiKey {
        present: bool,
        source: Option<KeySource>,
        env_var: &'static str,
    },
    /// A plan provider: the tier whose login of the caller's serves it, or
    /// per tier why none does.
    Login {
        plan: SubscriptionKind,
        serves_you: Option<CredentialTier>,
        misses: Vec<LoginMiss>,
        stored_for_anyone: bool,
    },
}

/// One provider an agent's settings route to, and what its calls run on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderStatus {
    pub provider: &'static str,
    pub uses: Vec<SpecUse>,
    pub credential: CredentialStatus,
    pub summary: String,
}

/// Every setting of the agent's that names a model spec, in the order the
/// chain consults them: the default model and failover entries, then the
/// per-task, voice, self-learning and claim-grounding models a turn may run
/// on, the tool-less background models, and the vision model. `failover` is
/// the live chain when the agent has one, else the configured list.
pub fn spec_uses(llm: &LlmConfig, resolved: &ResolvedConfig, failover: &[String]) -> Vec<SpecUse> {
    let mut uses = vec![SpecUse {
        setting: "default_model".into(),
        spec: llm.default_model.clone(),
    }];
    let mut push = |setting: String, spec: Option<&String>| {
        if let Some(spec) = spec.filter(|spec| !spec.trim().is_empty()) {
            uses.push(SpecUse {
                setting,
                spec: spec.clone(),
            });
        }
    };
    for (i, spec) in failover.iter().enumerate() {
        push(format!("failover[{i}]"), Some(spec));
    }
    if let Some(preferences) = &resolved.model_preferences {
        let mut tasks: Vec<_> = preferences.iter().collect();
        tasks.sort();
        for (task, spec) in tasks {
            push(format!("model_preferences.{task}"), Some(spec));
        }
    }
    push("voice_model".into(), resolved.voice_model.as_ref());
    push(
        "self_learning.model".into(),
        resolved.self_learning.model.as_ref(),
    );
    push(
        "claim_grounding.model".into(),
        resolved.claim_grounding.model.as_ref(),
    );
    push(
        "compaction.compaction_model".into(),
        resolved.compaction.compaction_model.as_ref(),
    );
    push(
        "memory.passive_model".into(),
        resolved.memory.passive_model.as_ref(),
    );
    push(
        "memory.consolidation_model".into(),
        resolved.memory.consolidation_model.as_ref(),
    );
    push(
        "memory.lesson_extract_model".into(),
        resolved.memory.lesson_extract_model.as_ref(),
    );
    if let Some(vision) = &llm.vision {
        push("vision.provider".into(), vision.provider.as_ref());
        push("vision.model".into(), Some(&vision.model));
    }
    uses
}

/// The provider `spec` routes to: its `provider:` prefix, a bare provider
/// name, else `primary`, which serves an unprefixed tier or model id.
pub fn provider_of(spec: &str, primary: ProviderKind) -> ProviderKind {
    routed_kind(spec)
        .or_else(|| ProviderKind::from_str_loose(spec.trim()))
        .unwrap_or(primary)
}

/// `uses` grouped by the provider each routes to, in first-use order.
pub fn providers_in_use(
    uses: Vec<SpecUse>,
    primary: ProviderKind,
) -> Vec<(ProviderKind, Vec<SpecUse>)> {
    let mut grouped: Vec<(ProviderKind, Vec<SpecUse>)> = Vec::new();
    for spec_use in uses {
        let kind = provider_of(&spec_use.spec, primary);
        match grouped.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, uses)) => uses.push(spec_use),
            None => grouped.push((kind, vec![spec_use])),
        }
    }
    grouped
}

/// What a `kind` call of the caller's would authenticate with now.
pub fn credential(providers: &AgentProviders, kind: ProviderKind) -> CredentialStatus {
    match kind.subscription_route() {
        Some(route) => {
            let plan = route.kind();
            let plans = providers.plans();
            let (serves_you, misses) = match plans.resolve(plan) {
                Ok(login) => (Some(login.tier), Vec::new()),
                Err(no_login) => (
                    None,
                    no_login
                        .misses
                        .into_iter()
                        .map(|(tier, miss)| LoginMiss {
                            tier,
                            reason: miss.to_string(),
                        })
                        .collect(),
                ),
            };
            CredentialStatus::Login {
                plan,
                serves_you,
                misses,
                stored_for_anyone: plans.stored_for_anyone(plan),
            }
        }
        None => {
            let source = providers.metered_key_source(kind);
            CredentialStatus::ApiKey {
                present: source.is_some(),
                source,
                env_var: provider_env_var_hint(kind),
            }
        }
    }
}

/// `credential`, as one line an agent can repeat to the person.
pub fn summary(kind: ProviderKind, credential: &CredentialStatus) -> String {
    let name = kind.canonical_name();
    match credential {
        CredentialStatus::ApiKey {
            source: Some(source),
            ..
        } => format!("{name}: API key from {}", source.describe(kind)),
        CredentialStatus::ApiKey { env_var, .. } => format!(
            "{name}: no API key; set {env_var} on the hub or add an account with system.add_account"
        ),
        CredentialStatus::Login {
            plan,
            serves_you: Some(tier),
            ..
        } => format!(
            "{name}: your {} plan login serves it ({})",
            plan.label(),
            tier.as_str()
        ),
        CredentialStatus::Login {
            plan,
            misses,
            stored_for_anyone,
            ..
        } => {
            let why: Vec<String> = misses
                .iter()
                .map(|miss| format!("{} {}", miss.tier.as_str(), miss.reason))
                .collect();
            let others = if *stored_for_anyone {
                "; someone else's is stored, which does not serve you"
            } else {
                ""
            };
            format!(
                "{name}: no {} plan login of yours serves it ({}){others}",
                plan.label(),
                why.join("; ")
            )
        }
    }
}

/// Every provider the agent's settings route to, with what its calls run
/// on for the caller, in first-use order.
pub fn status(
    providers: &AgentProviders,
    primary: ProviderKind,
    llm: &LlmConfig,
    resolved: &ResolvedConfig,
    failover: &[String],
) -> Vec<ProviderStatus> {
    providers_in_use(spec_uses(llm, resolved, failover), primary)
        .into_iter()
        .map(|(kind, uses)| {
            let credential = credential(providers, kind);
            ProviderStatus {
                provider: kind.canonical_name(),
                summary: summary(kind, &credential),
                uses,
                credential,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::settings::WorkforceConfig;

    fn resolved() -> ResolvedConfig {
        ResolvedConfig::resolve(None, None, &WorkforceConfig::default(), "anthropic", None)
    }

    #[test]
    fn every_model_setting_is_listed_with_its_name_in_chain_order() {
        let llm = LlmConfig {
            default_model: "powerful".into(),
            vision: Some(toml::from_str(r#"model = "chatgpt:gpt-6.1-sol""#).unwrap()),
            ..LlmConfig::default()
        };
        let mut config = resolved();
        config.model_preferences = Some(
            [
                ("research".to_string(), "claude-code:opus".to_string()),
                ("coding".to_string(), "codex:powerful".to_string()),
            ]
            .into(),
        );
        config.voice_model = Some("gemini:cheap".into());
        config.compaction.compaction_model = Some("chatgpt:cheap".into());
        config.memory.passive_model = Some(String::new());

        let uses = spec_uses(&llm, &config, &["chatgpt:powerful".to_string()]);
        let named: Vec<(&str, &str)> = uses
            .iter()
            .map(|u| (u.setting.as_str(), u.spec.as_str()))
            .collect();
        assert_eq!(
            named,
            [
                ("default_model", "powerful"),
                ("failover[0]", "chatgpt:powerful"),
                ("model_preferences.coding", "codex:powerful"),
                ("model_preferences.research", "claude-code:opus"),
                ("voice_model", "gemini:cheap"),
                ("compaction.compaction_model", "chatgpt:cheap"),
                ("vision.model", "chatgpt:gpt-6.1-sol"),
            ],
            "an empty setting is not a use"
        );
    }

    #[test]
    fn unprefixed_specs_belong_to_the_primary_and_bare_names_to_themselves() {
        assert_eq!(
            provider_of("chatgpt:powerful", ProviderKind::Anthropic),
            ProviderKind::ChatGpt
        );
        assert_eq!(
            provider_of("codex", ProviderKind::Anthropic),
            ProviderKind::Codex
        );
        assert_eq!(
            provider_of("powerful", ProviderKind::Gemini),
            ProviderKind::Gemini
        );
        assert_eq!(
            provider_of("claude-sonnet-5", ProviderKind::Anthropic),
            ProviderKind::Anthropic
        );
        let grouped = providers_in_use(
            spec_uses(
                &LlmConfig::default(),
                &resolved(),
                &["claude-code:sonnet".to_string(), "cheap".to_string()],
            ),
            ProviderKind::ChatGpt,
        );
        let kinds: Vec<(ProviderKind, usize)> =
            grouped.iter().map(|(k, uses)| (*k, uses.len())).collect();
        assert_eq!(
            kinds,
            [(ProviderKind::ChatGpt, 2), (ProviderKind::ClaudeCode, 1)],
            "the default model and the unprefixed failover entry both run on the primary"
        );
    }

    #[test]
    fn summaries_say_what_a_person_can_act_on() {
        let key = CredentialStatus::ApiKey {
            present: true,
            source: Some(KeySource::GlobalVault),
            env_var: "GEMINI_API_KEY",
        };
        assert_eq!(
            summary(ProviderKind::Gemini, &key),
            "gemini: API key from an account in the shared vault"
        );
        let no_key = CredentialStatus::ApiKey {
            present: false,
            source: None,
            env_var: "ANTHROPIC_API_KEY",
        };
        assert_eq!(
            summary(ProviderKind::Anthropic, &no_key),
            "anthropic: no API key; set ANTHROPIC_API_KEY on the hub or add an account with system.add_account"
        );
        let served = CredentialStatus::Login {
            plan: SubscriptionKind::Claude,
            serves_you: Some(CredentialTier::GlobalUser),
            misses: Vec::new(),
            stored_for_anyone: true,
        };
        assert_eq!(
            summary(ProviderKind::ClaudeCode, &served),
            "claude-code: your Claude plan login serves it (global_user)"
        );
        let unserved = CredentialStatus::Login {
            plan: SubscriptionKind::ChatGpt,
            serves_you: None,
            misses: vec![
                LoginMiss {
                    tier: CredentialTier::AgentUser,
                    reason: "none stored".into(),
                },
                LoginMiss {
                    tier: CredentialTier::GlobalUser,
                    reason: "none stored".into(),
                },
            ],
            stored_for_anyone: true,
        };
        assert_eq!(
            summary(ProviderKind::Codex, &unserved),
            "codex: no ChatGPT plan login of yours serves it (agent_user none stored; global_user none stored); someone else's is stored, which does not serve you"
        );
    }
}
