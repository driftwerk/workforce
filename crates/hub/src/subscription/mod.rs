//! Agents' subscription logins: which login pays for a call ([`resolve`]),
//! logins skipped at their usage limit or once refused ([`CooldownStore`]), limit reports
//! ([`limits`]), the ChatGPT `auth.json` lifecycle ([`chatgpt_auth`]), the
//! `chatgpt` provider that runs on them ([`llm`]) and the one-shot
//! `claude-code` completions ([`completion`]).

pub use crate::hub_core::logins::chatgpt_auth;
mod cli_kind;
pub mod completion;
pub use crate::hub_core::logins::cooldown;
pub use crate::hub_core::logins::device_login;
#[cfg(test)]
mod device_login_tests;
pub use crate::hub_core::logins::limits;
pub mod llm;
pub mod logins;
pub use crate::hub_core::logins::resolve;
pub use crate::hub_core::logins::store;
pub use crate::hub_core::pools;
#[cfg(test)]
pub(crate) mod test_support;

pub use chatgpt_auth::{
    store_login, write_back, AuthJson, RefreshError, RefreshFailure, StoreError, TokenRefresher,
};
pub use cli_kind::CliKind;
pub use completion::CliCompletion;
pub use cooldown::{CooldownStore, LimitSignal, LimitStatus};
pub use device_login::{
    DeviceAuthClient, DeviceFlow, DeviceLoginListener, DeviceLogins, FlowState, LoginTarget,
    SlackDelivery,
};
pub use llm::{
    ChatGptBackend, CredentialScope, PlanLogins, SubscriptionError, SubscriptionProvider,
};
pub use logins::{AgentLogins, OwnerOverride};
pub use pools::{Pool, SubscriptionPools};
pub use resolve::{
    known_slots, login_state, resolve, slot_for, Credential, Login, LoginKey, Miss, NoLogin,
    VaultSlot, Vaults,
};

use std::sync::Arc;

use workforce_agent::llm::provider::LlmProvider;
use workforce_agent::llm::tiers::TierRegistry;
use workforce_common::settings::WorkforceConfig;

use crate::coding_sandbox::{remote_launcher_for, McpSurface, SandboxLauncher};
pub use crate::hub_core::subscriptions::{
    chain_specs, review_agent_llm, route_models, routed_kind, routed_subscriptions, tier_models,
    unknown_operator, validate_agent_llm, LOGINS_ARE_NOT_ACCOUNTS,
};
pub use crate::hub_core::subscriptions::{Finding, Install, Severity, SharedSubscriptions};
use crate::serve::AgentService;
use crate::vault::Vault;

/// The hub's subscriptions as composition holds them: what every agent's
/// service shares, and the coding worker one-shot Codex completions run on.
#[derive(Clone)]
pub struct Subscriptions {
    shared: SharedSubscriptions,
    codex_worker: Option<Arc<dyn SandboxLauncher>>,
}

impl Subscriptions {
    /// The subscriptions of a process that is not the hub itself, such as
    /// `mcp-serve`: no coding worker for Codex completions, and no sign-ins.
    pub fn new(config: &WorkforceConfig) -> Self {
        Self::of(config, None, None)
    }

    fn of(
        config: &WorkforceConfig,
        codex_worker: Option<Arc<dyn SandboxLauncher>>,
        global_vault: Option<&Arc<Vault>>,
    ) -> Self {
        Self {
            shared: SharedSubscriptions::new(config, global_vault),
            codex_worker,
        }
    }

    /// The hub's subscriptions, built once at boot and shared by every
    /// agent — with the coding worker Codex completions run on and the
    /// ChatGPT sign-ins kept in `global_vault` — clearing what completions a
    /// crash left behind. When the install's `[llm]` runs on a
    /// subscription, the hub's own login becomes the artifact tagger before
    /// any agent can offer its provider.
    pub fn boot(config: &WorkforceConfig, global_vault: Option<&Arc<Vault>>) -> Self {
        let codex_worker = remote_launcher_for(
            &config.coding_session.executor,
            &config.exec,
            McpSurface::none(),
        )
        .inspect_err(|e| {
            tracing::warn!(error = %e, "codex completions and agent turns off: no coding worker")
        })
        .ok()
        .flatten();
        let subscriptions = Self::of(config, codex_worker, global_vault);
        completion::sweep_leftovers(subscriptions.shared().completions_dir());
        if let Some(tagger) = global_vault.and_then(|vault| subscriptions.tagger(config, vault)) {
            crate::artifact_index::set_tagger_llm(tagger);
        }
        subscriptions
    }

    /// The artifact tagger on the hub's own logins, when the install's
    /// `[llm]` provider is a subscription one.
    fn tagger(&self, config: &WorkforceConfig, vault: &Arc<Vault>) -> Option<Arc<dyn LlmProvider>> {
        let kind = routed_kind(&config.llm.provider).filter(|kind| kind.is_subscription())?;
        let (_, model) = TierRegistry::new(None)
            .resolve_routed_model(&config.llm.default_model, kind.canonical_name());
        PlanLogins::new(
            self.clone(),
            CredentialScope::system(Arc::clone(vault), &config.subscription),
        )
        .provider(kind, Some(&model), None)
        .inspect_err(|e| tracing::warn!(provider = kind.canonical_name(), error = %e, "artifact tagging off: the install's [llm] provider can't serve it"))
        .ok()
    }

    #[cfg(test)]
    pub(crate) fn with_chatgpt(self, chatgpt: ChatGptBackend) -> Self {
        Self {
            shared: self.shared.with_chatgpt(chatgpt),
            codex_worker: self.codex_worker,
        }
    }

    /// These subscriptions with their sign-ins kept by `device_logins`.
    pub fn with_device_logins(self, device_logins: DeviceLogins) -> Self {
        Self {
            shared: self.shared.with_device_logins(device_logins),
            codex_worker: self.codex_worker,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_codex_worker(self, worker: Arc<dyn SandboxLauncher>) -> Self {
        Self {
            codex_worker: Some(worker),
            ..self
        }
    }

    /// What every agent's service holds of these.
    pub fn shared(&self) -> &SharedSubscriptions {
        &self.shared
    }

    /// The coding worker one-shot Codex completions run on, when the
    /// install has one.
    pub fn codex_worker(&self) -> Option<&Arc<dyn SandboxLauncher>> {
        self.codex_worker.as_ref()
    }
}

/// The line under a turn's answer saying what ran it on `model`, a
/// metered turn counting as a fallback when this agent's default runs on
/// a subscription.
pub(crate) fn route_line(
    service: &AgentService,
    model: &str,
    served: Option<&workforce_common::subscription::Served>,
) -> Option<String> {
    let default_route = crate::subscription::routed_kind(&service.original_default_model.read())
        .unwrap_or(*service.provider_kind.read());
    let lookup = crate::vault_view::identity_lookup(service.identity_store.read().clone());
    workforce_common::subscription::route_line(
        model,
        served,
        default_route.is_subscription(),
        |payer| uuid::Uuid::parse_str(payer).ok().and_then(&*lookup),
    )
}

#[cfg(test)]
mod tests {
    use workforce_common::settings::{
        CodingCodexConfig, ExecutorKind, LlmConfig, REMOVED_PROVIDER_POINTER,
    };

    use super::*;
    use crate::hub_core::resolved_config::ResolvedConfig;
    use workforce_common::subscription::CredentialTier;

    #[test]
    fn an_operator_naming_no_identity_is_reported() {
        use workforce_identity::store::IdentityStore;
        let identities = workforce_identity::sqlite::SqliteIdentityStore::in_memory().unwrap();
        let person = workforce_identity::types::Identity::new(
            "Operator",
            workforce_identity::types::IdentityRole::SystemAdmin,
        );
        identities.create(&person).unwrap();
        let operator = |operator| workforce_common::settings::SubscriptionConfig {
            operator,
            ..Default::default()
        };

        assert_eq!(unknown_operator(&operator(None), &identities), None);
        assert_eq!(
            unknown_operator(&operator(Some(person.id)), &identities),
            None
        );
        let stranger = uuid::Uuid::new_v4();
        assert!(unknown_operator(&operator(Some(stranger)), &identities)
            .is_some_and(|warning| warning.contains(&stranger.to_string())));
    }

    const CODEX: &CodingCodexConfig = &CodingCodexConfig {
        tool_timeout_secs: 0,
        models: std::collections::BTreeMap::new(),
    };
    const LOCAL: Install<'static> = Install {
        executor: &ExecutorKind::Local,
        chatgpt_direct: true,
        codex: CODEX,
    };
    const REMOTE: Install<'static> = Install {
        executor: &ExecutorKind::Remote,
        chatgpt_direct: true,
        codex: CODEX,
    };

    fn resolved() -> ResolvedConfig {
        ResolvedConfig::resolve(None, None, &WorkforceConfig::default(), "anthropic", None)
    }

    #[test]
    fn codex_route_refused_without_remote_worker() {
        let refused_locally = |llm: &LlmConfig, resolved: &ResolvedConfig| {
            let err = validate_agent_llm(llm, resolved, &LOCAL)
                .unwrap_err()
                .to_string();
            assert!(err.contains("codex") && err.contains("remote"), "{err}");
            validate_agent_llm(llm, resolved, &REMOTE)
                .expect("on the remote worker codex serves turns, tool loops and background work");
        };
        for spec in ["codex:gpt-6.1-sol", "codex"] {
            refused_locally(
                &LlmConfig {
                    default_model: "powerful".into(),
                    failover: vec![spec.into()],
                    ..LlmConfig::default()
                },
                &resolved(),
            );
        }
        refused_locally(
            &LlmConfig {
                provider: "codex".into(),
                default_model: "gpt-6.1-sol".into(),
                ..LlmConfig::default()
            },
            &resolved(),
        );
        let positions: [fn(&mut ResolvedConfig); 8] = [
            |r| r.model_preferences = Some([("coding".into(), "codex:powerful".into())].into()),
            |r| r.voice_model = Some("codex:gpt-6.1-sol".into()),
            |r| r.self_learning.model = Some("codex:gpt-6.1-sol".into()),
            |r| r.claim_grounding.model = Some("codex:gpt-6.1-sol".into()),
            |r| r.compaction.compaction_model = Some("codex:gpt-6.1-sol".into()),
            |r| r.memory.passive_model = Some("codex:gpt-6.1-sol".into()),
            |r| r.memory.consolidation_model = Some("codex:gpt-6.1-sol".into()),
            |r| r.memory.lesson_extract_model = Some("codex:cheap".into()),
        ];
        for set in positions {
            let mut config = resolved();
            set(&mut config);
            refused_locally(&LlmConfig::default(), &config);
        }
    }

    #[test]
    fn other_routes_pass_locally() {
        let llm = LlmConfig {
            default_model: "chatgpt:cheap".into(),
            failover: vec![
                "chatgpt:gpt-6.1-sol".into(),
                "openrouter:openai:cheap".into(),
                "codex-mini-latest".into(),
                "anthropic.claude-haiku-4-5-20251001-v1:0".into(),
            ],
            ..LlmConfig::default()
        };
        validate_agent_llm(&llm, &resolved(), &LOCAL).unwrap();
    }

    #[test]
    fn claude_code_turn_routes_pass_validation() {
        let passes = |llm: &LlmConfig, resolved: &ResolvedConfig| {
            for install in [&LOCAL, &REMOTE] {
                validate_agent_llm(llm, resolved, install)
                    .expect("a claude-code route serves turns, tool loops and background work");
            }
        };
        for spec in ["claude-code:sonnet", "claude_code:powerful"] {
            passes(
                &LlmConfig {
                    failover: vec![spec.into()],
                    ..LlmConfig::default()
                },
                &resolved(),
            );
        }
        passes(
            &LlmConfig {
                provider: "claude-code".into(),
                default_model: "sonnet".into(),
                ..LlmConfig::default()
            },
            &resolved(),
        );
        let positions: [fn(&mut ResolvedConfig); 8] = [
            |r| r.model_preferences = Some([("coding".into(), "claude-code:opus".into())].into()),
            |r| r.voice_model = Some("claude-code:haiku".into()),
            |r| r.self_learning.model = Some("claude-code:sonnet".into()),
            |r| r.claim_grounding.model = Some("claude-code:sonnet".into()),
            |r| r.compaction.compaction_model = Some("claude-code:haiku".into()),
            |r| r.memory.passive_model = Some("claude-code:haiku".into()),
            |r| r.memory.consolidation_model = Some("claude-code:sonnet".into()),
            |r| r.memory.lesson_extract_model = Some("claude-code:cheap".into()),
        ];
        for set in positions {
            let mut config = resolved();
            set(&mut config);
            passes(&LlmConfig::default(), &config);
        }
    }

    #[test]
    fn codex_turns_run_only_on_models_the_chatgpt_backend_serves() {
        let refused = |llm: &LlmConfig, resolved: &ResolvedConfig, reason: &str| {
            let err = validate_agent_llm(llm, resolved, &REMOTE)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(reason) && err.contains("agent turns run codex:gpt-6.1-sol"),
                "{err}"
            );
        };
        for served in [
            "codex:gpt-6.1-sol",
            "codex:gpt-6-astra",
            "codex:gpt-5.6-sol",
            "codex:sol",
            "codex:astra",
            "codex:powerful",
            "codex:standard",
            "codex:cheap",
        ] {
            validate_agent_llm(
                &LlmConfig {
                    failover: vec![served.into()],
                    ..LlmConfig::default()
                },
                &resolved(),
                &REMOTE,
            )
            .unwrap_or_else(|e| panic!("{served} runs turns: {e}"));
        }
        let positions: [fn(&mut ResolvedConfig); 6] = [
            |r| r.model_preferences = Some([("coding".into(), "codex:luna".into())].into()),
            |r| r.voice_model = Some("codex:gpt-5.6-luna".into()),
            |r| r.self_learning.model = Some("codex:sol".into()),
            |r| r.claim_grounding.model = Some("codex:gpt-6-astra".into()),
            |r| r.compaction.compaction_model = Some("codex:terra".into()),
            |r| r.memory.passive_model = Some("codex:gpt-6-luna".into()),
        ];
        for set in positions {
            let mut config = resolved();
            set(&mut config);
            validate_agent_llm(&LlmConfig::default(), &config, &REMOTE)
                .expect("every listed model runs turns, tool loops and completions");
        }
        refused(
            &LlmConfig {
                failover: vec!["codex:gpt-5.5".into()],
                ..LlmConfig::default()
            },
            &resolved(),
            "gpt-5.5 retired from ChatGPT sign-ins on 2026-10-14",
        );
        refused(
            &LlmConfig {
                provider: "codex".into(),
                default_model: "gpt-5.4".into(),
                ..LlmConfig::default()
            },
            &resolved(),
            "gpt-5.4 retired",
        );
        for unverified in ["codex:gpt-4o", "codex:o3"] {
            refused(
                &LlmConfig {
                    failover: vec![unverified.into()],
                    ..LlmConfig::default()
                },
                &resolved(),
                "not verified for agent turns",
            );
        }
        let renamed = CodingCodexConfig {
            models: [("house".to_string(), "gpt-4o".to_string())].into(),
            ..CodingCodexConfig::default()
        };
        let err = validate_agent_llm(
            &LlmConfig {
                failover: vec!["codex:house".into()],
                ..LlmConfig::default()
            },
            &resolved(),
            &Install {
                codex: &renamed,
                ..REMOTE
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("gpt-4o"), "{err}");
        let vision = LlmConfig {
            vision: Some(toml::from_str("model = \"codex:gpt-6.1-sol\"").unwrap()),
            ..LlmConfig::default()
        };
        assert!(validate_agent_llm(&vision, &resolved(), &REMOTE)
            .unwrap_err()
            .to_string()
            .contains("no images"));
    }

    #[test]
    fn codex_turn_tiers_resolve_before_the_check() {
        let refused = |llm: &LlmConfig, resolved: &ResolvedConfig| {
            validate_agent_llm(llm, resolved, &REMOTE).is_err()
        };
        let on = |default_model: &str, failover: &str| LlmConfig {
            default_model: default_model.into(),
            failover: vec![failover.into()],
            ..LlmConfig::default()
        };
        for default_model in ["sonnet", "opus", "powerful"] {
            assert!(
                !refused(&on(default_model, "codex"), &resolved()),
                "a bare entry runs at its default model's tier, each of which the backend serves"
            );
        }

        let remapped = ResolvedConfig::resolve(
            None,
            None,
            &WorkforceConfig::default(),
            "anthropic",
            Some(&[("standard".to_string(), vec!["codex:gpt-5.5".to_string()])].into()),
        );
        assert!(
            refused(&on("sonnet", "codex:standard"), &remapped),
            "the agent's equivalences decide what a tier runs: here a retired model"
        );
        assert!(refused(&on("sonnet", "codex"), &remapped));
        assert!(!refused(&on("sonnet", "codex:cheap"), &remapped));
    }
    #[test]
    fn claude_max_config_fails_load_with_pointer() {
        let refused = |llm: &LlmConfig, resolved: &ResolvedConfig| {
            validate_agent_llm(llm, resolved, &REMOTE)
                .unwrap_err()
                .to_string()
        };
        let named = |err: String, spec: &str| {
            assert!(
                err.contains(spec)
                    && err.contains("was removed")
                    && err.contains(REMOVED_PROVIDER_POINTER),
                "{err}"
            )
        };
        named(
            refused(
                &LlmConfig {
                    provider: "claude-max".into(),
                    ..LlmConfig::default()
                },
                &resolved(),
            ),
            "claude-max",
        );
        for spec in ["claude-max:sonnet", "max", "OAuth:powerful"] {
            named(
                refused(
                    &LlmConfig {
                        failover: vec!["gemini:cheap".into(), spec.into()],
                        ..LlmConfig::default()
                    },
                    &resolved(),
                ),
                spec,
            );
        }
        let mut preferring = resolved();
        preferring.model_preferences =
            Some([("research".into(), "subscription:opus".into())].into());
        named(
            refused(&LlmConfig::default(), &preferring),
            "subscription:opus",
        );
        validate_agent_llm(
            &LlmConfig {
                failover: vec!["claude-code:sonnet".into(), "anthropic:cheap".into()],
                ..LlmConfig::default()
            },
            &resolved(),
            &REMOTE,
        )
        .expect("the routes that replace it load");
    }

    #[test]
    fn every_listed_chatgpt_model_loads_and_a_retired_one_warns() {
        for served in [
            "chatgpt:gpt-5.6-sol",
            "chatgpt:gpt-6-astra",
            "chatgpt:gpt-6.1-sol",
        ] {
            let routed = LlmConfig {
                failover: vec![served.into()],
                ..LlmConfig::default()
            };
            validate_agent_llm(&routed, &resolved(), &LOCAL)
                .unwrap_or_else(|e| panic!("{served}: {e}"));
            assert!(review_agent_llm(&routed, &resolved(), &LOCAL).is_empty());
        }
        let primary = LlmConfig {
            provider: "chatgpt".into(),
            default_model: "gpt-6-astra".into(),
            ..LlmConfig::default()
        };
        validate_agent_llm(&primary, &resolved(), &LOCAL).unwrap();
        let tiered = LlmConfig {
            provider: "chatgpt".into(),
            default_model: "standard".into(),
            ..LlmConfig::default()
        };
        validate_agent_llm(&tiered, &resolved(), &LOCAL).unwrap();

        let retired = LlmConfig {
            failover: vec!["chatgpt:gpt-5.5".into()],
            ..LlmConfig::default()
        };
        assert_eq!(
            review_agent_llm(&retired, &resolved(), &LOCAL),
            [Finding {
                severity: Severity::Warning,
                spec: "chatgpt:gpt-5.5".into(),
                message: "\"chatgpt:gpt-5.5\": gpt-5.5 retired from ChatGPT sign-ins on 2026-10-14; use gpt-6.1-sol; calls for this model skip the chatgpt route".into(),
            }]
        );
        validate_agent_llm(&retired, &resolved(), &LOCAL)
            .expect("a retired model only warns: its calls move on down the chain");
    }

    #[test]
    fn unprefixed_specs_on_a_chatgpt_primary_are_checked() {
        let on_chatgpt = |failover: &str| LlmConfig {
            provider: "chatgpt".into(),
            default_model: "gpt-6.1-sol".into(),
            failover: vec![failover.into()],
            ..LlmConfig::default()
        };
        let warned = |llm: &LlmConfig, resolved: &ResolvedConfig, spec: &str| {
            review_agent_llm(llm, resolved, &LOCAL)
                .iter()
                .any(|f| f.severity == Severity::Warning && f.spec == spec)
        };
        assert!(warned(&on_chatgpt("gpt-5.5"), &resolved(), "gpt-5.5"));
        assert!(warned(&on_chatgpt("gpt-4o"), &resolved(), "gpt-4o"));
        validate_agent_llm(&on_chatgpt("gpt-4o"), &resolved(), &LOCAL)
            .expect("an unknown model only warns: its calls move on down the chain");
        assert!(review_agent_llm(&on_chatgpt("gemini"), &resolved(), &LOCAL).is_empty());

        let mut preferring = resolved();
        preferring.model_preferences = Some([("coding".into(), "gpt-5.5".into())].into());
        assert!(warned(&on_chatgpt("gemini"), &preferring, "gpt-5.5"));
        let mut voiced = resolved();
        voiced.voice_model = Some("gpt-5.4".into());
        assert!(warned(&on_chatgpt("gemini"), &voiced, "gpt-5.4"));
        assert!(
            review_agent_llm(&LlmConfig::default(), &voiced, &LOCAL).is_empty(),
            "an unprefixed spec is the primary's, and this primary is not chatgpt"
        );
    }

    #[test]
    fn invalid_workspace_logins_are_refused() {
        let mut refused = resolved();
        refused.logins.tiers = vec![CredentialTier::AgentShared, CredentialTier::AgentShared];
        assert!(validate_agent_llm(&LlmConfig::default(), &refused, &REMOTE).is_err());
    }

    #[test]
    fn chatgpt_direct_off_is_a_warning_not_a_refusal() {
        let llm = LlmConfig {
            provider: "chatgpt".into(),
            default_model: "gpt-6.1-sol".into(),
            failover: vec!["chatgpt:gpt-6.1-sol".into(), "gemini:cheap".into()],
            ..LlmConfig::default()
        };
        let off = Install {
            chatgpt_direct: false,
            ..LOCAL
        };
        let found = review_agent_llm(&llm, &resolved(), &off);
        assert_eq!(
            found
                .iter()
                .map(|f| (f.severity, f.spec.as_str()))
                .collect::<Vec<_>>(),
            [
                (Severity::Warning, "chatgpt"),
                (Severity::Warning, "chatgpt:gpt-6.1-sol")
            ]
        );
        assert!(found[0].message.contains("chatgpt_direct off"), "{found:?}");
        validate_agent_llm(&llm, &resolved(), &off).expect("the agent still loads");
        assert!(review_agent_llm(&llm, &resolved(), &LOCAL).is_empty());
    }

    #[test]
    fn unknown_chatgpt_model_is_a_warning_finding() {
        let llm = LlmConfig {
            failover: vec!["chatgpt:gpt-4o".into()],
            ..LlmConfig::default()
        };
        assert_eq!(
            review_agent_llm(&llm, &resolved(), &LOCAL),
            [Finding {
                severity: Severity::Warning,
                spec: "chatgpt:gpt-4o".into(),
                message: "\"chatgpt:gpt-4o\": gpt-4o is not served on a ChatGPT plan here; use gpt-6.1-sol; calls for this model skip the chatgpt route".into(),
            }]
        );
    }

    #[test]
    fn validate_refuses_exactly_the_refused_findings() {
        let llm = LlmConfig {
            failover: vec![
                "chatgpt:gpt-4o".into(),
                "chatgpt:gpt-5.6-sol".into(),
                "codex:gpt-6.1-sol".into(),
            ],
            ..LlmConfig::default()
        };
        let found = review_agent_llm(&llm, &resolved(), &LOCAL);
        let refused: Vec<&Finding> = found.iter().filter(|f| f.is_refused()).collect();
        assert_eq!(
            refused.iter().map(|f| f.spec.as_str()).collect::<Vec<_>>(),
            ["codex:gpt-6.1-sol"],
            "the executor rule, once per spec; a ChatGPT model the backend doesn't serve only warns"
        );
        assert_eq!(
            validate_agent_llm(&llm, &resolved(), &LOCAL)
                .unwrap_err()
                .to_string(),
            refused[0].message
        );
        let warnings_only = LlmConfig {
            failover: vec!["chatgpt:gpt-4o".into()],
            ..LlmConfig::default()
        };
        assert!(!review_agent_llm(&warnings_only, &resolved(), &LOCAL).is_empty());
        validate_agent_llm(&warnings_only, &resolved(), &LOCAL).expect("warnings never refuse");
    }
}
