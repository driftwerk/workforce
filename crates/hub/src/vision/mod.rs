//! Hub-side wiring for vision LLM construction.
//!
//! Mirrors `hub::knowledge::HubSummariserBuilder`: takes an [`LlmVisionConfig`],
//! resolves its route against the vision tier registry and constructs the
//! [`LlmProvider`] for it, wrapped in a [`LlmProviderVisionAdapter`]. A
//! provider that takes an API key reads it on every description, for the
//! person the description is for ([`KeyedProvider`]); a subscription route
//! runs on the plan logins; the rest are built once with what the install
//! configured.
//!
//! Built once per agent at startup and stored on `AgentService` as
//! `Arc<dyn VisionLlm>` so the adapter's LRU cache spans turns.

use std::sync::Arc;

use eyre::{eyre, Result};
use tracing::info;

use workforce_agent::llm::provider::{create_provider, LlmProvider, ProviderKind};
use workforce_agent::llm::responses::ChatGptModel;
use workforce_agent::llm::vision_tiers::VisionTierRegistry;
use workforce_agent::vision::{LlmProviderVisionAdapter, UsageObserver, VisionLlm};
use workforce_common::cost::{CostEntry, CostTracker};
use workforce_common::settings::LlmVisionConfig;
use workforce_common::subscription::Origin;

use crate::credentials::{Legacy, Modality};
use crate::keyed_provider::KeyedProvider;
use crate::subscription::PlanLogins;
use crate::vault::Vault;

/// Builds a [`VisionLlm`] from an [`LlmVisionConfig`]. When a [`CostTracker`]
/// and `agent_id` are supplied, every uncached vision call's usage is recorded
/// to that agent's session ledger so the description cost shows up in cost
/// analytics alongside the primary turn.
pub struct HubVisionBuilder {
    config: LlmVisionConfig,
    tiers: Arc<VisionTierRegistry>,
    cost_tracker: Option<Arc<CostTracker>>,
    agent_id: Option<String>,
    plans: Option<PlanLogins>,
    agent_vault: Option<Arc<Vault>>,
    global_vault: Option<Arc<Vault>>,
}

impl HubVisionBuilder {
    pub fn new(config: LlmVisionConfig, tiers: Arc<VisionTierRegistry>) -> Self {
        Self {
            config,
            tiers,
            cost_tracker: None,
            agent_id: None,
            plans: None,
            agent_vault: None,
            global_vault: None,
        }
    }

    /// The vaults a keyed route's API key is read from, per description.
    pub fn with_vaults(
        mut self,
        agent_vault: Option<Arc<Vault>>,
        global_vault: Option<Arc<Vault>>,
    ) -> Self {
        self.agent_vault = agent_vault;
        self.global_vault = global_vault;
        self
    }

    /// The logins a subscription vision route runs on: the agent's, so the
    /// turn's payer pays for the description.
    pub fn with_plans(mut self, plans: PlanLogins) -> Self {
        self.plans = Some(plans);
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

    pub fn build(self) -> Result<Arc<dyn VisionLlm>> {
        let (kind, resolved_model) = route(&self.config, &self.tiers)?;

        info!(
            model_spec = %self.config.model,
            resolved_model = %resolved_model,
            provider = kind.canonical_name(),
            cache_size = self.config.cache_size,
            "Vision provider configured"
        );

        let legacy = legacy(&self.config, kind);
        let adapter = |provider: Arc<dyn LlmProvider>| {
            LlmProviderVisionAdapter::new(provider, resolved_model.clone(), self.config.cache_size)
        };
        let adapter = match self.plans.as_ref().filter(|_| kind.is_subscription()) {
            Some(plans) => adapter(plans.provider(kind, Some(&resolved_model), None)?),
            None if kind.takes_api_key() => {
                let key = crate::credentials::slot(
                    Modality::Vision,
                    kind.canonical_name(),
                    self.agent_vault.clone(),
                    self.global_vault.clone(),
                    legacy,
                );
                let model = resolved_model.clone();
                let provider = KeyedProvider::new(key.clone(), move |key| {
                    create_provider(kind, key, Some(&model), None, false).map(Arc::from)
                });
                adapter(Arc::new(provider)).with_payer(key)
            }
            None => adapter(Arc::from(create_provider(
                kind,
                &legacy.key().unwrap_or_default(),
                Some(&resolved_model),
                None,
                false,
            )?)),
        };
        let adapter = match self.cost_observer() {
            Some(observer) => adapter.with_usage_observer(observer),
            None => adapter,
        };

        Ok(Arc::new(adapter))
    }

    fn cost_observer(&self) -> Option<UsageObserver> {
        let tracker = self.cost_tracker.clone()?;
        let agent_id = self.agent_id.clone()?;
        Some(Arc::new(move |model, usage, served| {
            let description = format!("vision:{model}");
            let identity = crate::user_config::current_identity_id();
            tracker.record(CostEntry {
                task_type: Some("vision"),
                identity_id: identity.as_deref(),
                description: Some(&description),
                served: served.cloned(),
                ..CostEntry::new(&agent_id, model, usage.into(), Origin::Vision)
            });
        }))
    }
}

/// The provider and model `[llm.vision]` names. A route the hub cannot call
/// is an error naming it: no provider stands in for another.
pub(crate) fn route(
    config: &LlmVisionConfig,
    tiers: &VisionTierRegistry,
) -> Result<(ProviderKind, String)> {
    if config.model.is_empty() {
        return Err(eyre!(
            "[llm.vision].model is empty — set a tier shorthand like 'openrouter:anthropic:cheap'"
        ));
    }
    let (kind, model) = tiers
        .resolve_routed_vision_model(&config.model, config.provider.as_deref())
        .map_err(|e| eyre!("[llm.vision] model {e}"))?;
    if !kind.describes_images() {
        return Err(eyre!(
            "[llm.vision] model '{}' is routed to {}, whose requests carry no images, so it \
             would describe one unseen",
            config.model,
            kind.canonical_name()
        ));
    }
    if kind == ProviderKind::ChatGpt {
        ChatGptModel::parse(&model).map_err(|e| eyre!("[llm.vision] model: {e}"))?;
    }
    Ok((kind, model))
}

/// What `[llm.vision]` read its key from before the vault credential:
/// `api_key`, `VISION_API_KEY`, the provider's environment variable.
pub(crate) fn legacy(config: &LlmVisionConfig, kind: ProviderKind) -> Legacy {
    let provider_env: &[&'static str] = match kind {
        ProviderKind::Vllm => &["VLLM_API_KEY"],
        ProviderKind::Bedrock => &["AWS_ACCESS_KEY_ID"],
        ProviderKind::VertexAnthropic => &["GOOGLE_APPLICATION_CREDENTIALS"],
        keyed => crate::credentials::provider_env(keyed),
    };
    Legacy::default()
        .config("[llm.vision] api_key", config.api_key.clone())
        .env(&["VISION_API_KEY"])
        .env(provider_env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_turn::tests::{hub, ALICE};
    use workforce_agent::attachments::{Describer, Transcriber};
    use workforce_agent::runner::SessionContext;

    const STRANGER: &str = "0b7f3d52-8f0a-4c2f-9d44-3c1e6a1f4e22";

    fn vision(model: &str) -> LlmVisionConfig {
        toml::from_str(&format!("model = {model:?}")).unwrap()
    }

    fn routed(config: &LlmVisionConfig) -> Result<(ProviderKind, String)> {
        route(config, &VisionTierRegistry::from_defaults().unwrap())
    }

    #[test]
    fn every_provider_route_builds_its_own_provider_or_is_refused_by_name() {
        for &kind in ProviderKind::ALL {
            let spec = format!("{}:gpt-6.1-sol", kind.canonical_name());
            match routed(&vision(&spec)) {
                Ok(route) => {
                    assert!(kind.describes_images(), "{spec}");
                    assert_eq!(route, (kind, "gpt-6.1-sol".to_string()), "{spec}");
                }
                Err(error) => {
                    assert!(!kind.describes_images(), "{spec}: {error}");
                    assert!(
                        error.to_string().contains(&format!(
                            "'{spec}' is routed to {}, whose requests carry no images",
                            kind.canonical_name()
                        )),
                        "{error}"
                    );
                }
            }
        }
        assert_eq!(
            routed(&vision("openrouter:anthropic:cheap")).unwrap(),
            (
                ProviderKind::OpenRouter,
                "anthropic/claude-haiku-5.5".to_string()
            )
        );
        let bare = LlmVisionConfig {
            provider: Some("vllm".into()),
            ..vision("Qwen/Qwen2.5-VL-7B-Instruct")
        };
        assert_eq!(
            routed(&bare).unwrap(),
            (
                ProviderKind::Vllm,
                "Qwen/Qwen2.5-VL-7B-Instruct".to_string()
            )
        );
    }

    #[test]
    fn an_unroutable_spec_is_an_error_naming_it_never_another_provider() {
        for (spec, names) in [
            (
                "nonesuch:gpt-6.1-sol",
                "'nonesuch:gpt-6.1-sol' names no known provider",
            ),
            ("gpt-6.1-sol", "'gpt-6.1-sol' names no known provider"),
            ("cheap", "'cheap' names no known provider"),
            ("grok:cheap", "the cheap tier of 'grok'"),
            ("openai:standard", "the standard tier of 'openai'"),
            ("chatgpt:gpt-4o", "gpt-4o is not served on a ChatGPT plan"),
        ] {
            let config = vision(spec);
            let error = routed(&config).unwrap_err().to_string();
            assert!(
                error.starts_with("[llm.vision]") && error.contains(names),
                "{spec}: {error}"
            );
            assert!(
                HubVisionBuilder::new(
                    config,
                    Arc::new(VisionTierRegistry::from_defaults().unwrap())
                )
                .build()
                .is_err(),
                "{spec}"
            );
        }
    }

    #[tokio::test]
    async fn a_chatgpt_describer_runs_on_the_plan_login_of_the_person_in_scope() {
        let hub = hub(|config| config.llm.vision = Some(vision("chatgpt:gpt-6.1-sol"))).await;
        hub.chatgpt_login(ALICE).await;
        let Describer::Ready(llm, _) = &hub.service.describer else {
            panic!("[llm.vision] chatgpt:gpt-6.1-sol builds a describer");
        };
        assert_eq!(llm.model_name(), "gpt-6.1-sol");

        let nobody = llm
            .describe_image("image/png", "AAAA", None)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            nobody.contains("no ChatGPT login for no identity") && !nobody.contains("Anthropic"),
            "{nobody}"
        );
        let stranger = SessionContext {
            user_id: Some(STRANGER.into()),
            ..SessionContext::default()
        }
        .scope(llm.describe_image("image/png", "AAAA", None))
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(
            stranger,
            format!(
                "subscription unavailable: no ChatGPT login for {STRANGER}: agent_user none \
                 stored; global_user none stored; agent_shared none stored; system_shared none \
                 stored"
            ),
            "the reason alone: no other person's login, no token, no sign-in code"
        );
        assert!(
            hub.server.received_requests().await.unwrap().is_empty(),
            "nothing is sent, and no sign-in is started, for someone with no login"
        );

        let described = SessionContext {
            user_id: Some(ALICE.into()),
            ..SessionContext::default()
        }
        .scope(llm.describe_image("image/png", "AAAA", None))
        .await
        .unwrap();
        assert_eq!(described, crate::subscription::test_support::SKY);
        let requests = hub.server.received_requests().await.unwrap();
        let sent: serde_json::Value = requests
            .iter()
            .find(|r| r.url.path() == "/responses")
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .expect("the ChatGPT backend was called");
        assert_eq!(sent["model"], "gpt-6.1-sol");
        assert_eq!(
            sent["input"][0]["content"][0],
            serde_json::json!({"type": "input_image", "image_url": "data:image/png;base64,AAAA"})
        );
    }

    #[tokio::test]
    async fn a_plan_describer_beside_a_metered_transcriber_both_build() {
        let hub = hub(|config| {
            config.llm.default_model = "chatgpt:gpt-6.1-sol".into();
            config.llm.vision = Some(vision("chatgpt:gpt-6.1-sol"));
            config.llm.transcription =
                Some(toml::from_str(r#"model = "openrouter:google:cheap""#).unwrap());
        })
        .await;
        assert!(matches!(hub.service.describer, Describer::Ready(..)));
        let Transcriber::Ready(transcriber, _) = &hub.service.transcriber else {
            panic!("[llm.transcription] openrouter:google:cheap builds a transcriber");
        };
        assert_eq!(transcriber.model_name(), "google/gemini-3.5-flash-lite");
    }

    #[tokio::test]
    async fn a_misrouted_transcriber_is_kept_as_the_reason_it_cannot_run() {
        let hub = hub(|config| {
            config.llm.transcription = Some(toml::from_str(r#"model = "whisper-1""#).unwrap());
        })
        .await;
        let Transcriber::Misconfigured(why) = &hub.service.transcriber else {
            panic!("a spec naming no provider builds no transcriber");
        };
        assert!(why.contains("'whisper-1' names no known provider"), "{why}");
    }

    #[tokio::test]
    async fn a_keyed_describer_is_ready_with_no_key_and_refuses_whoever_has_none() {
        let hub =
            hub(|install| install.llm.vision = Some(vision("openrouter:anthropic:cheap"))).await;
        let Describer::Ready(llm, config) = &hub.service.describer else {
            panic!("a missing key is a per-call outcome, not a build-time state");
        };
        assert_eq!(
            legacy(config, ProviderKind::OpenRouter).key(),
            None,
            "this test needs VISION_API_KEY and OPENROUTER_API_KEY unset"
        );
        hub.service
            .global_vault
            .as_ref()
            .unwrap()
            .set(
                &format!("users/{ALICE}/vision/openrouter/api_key"),
                "sk-or-alices-own",
            )
            .unwrap();

        let refused = SessionContext {
            user_id: Some(STRANGER.into()),
            ..SessionContext::default()
        }
        .scope(llm.describe_image("image/png", "AAAA", None))
        .await
        .unwrap_err();
        assert!(refused.is::<workforce_common::secret::NoKey>());
        assert_eq!(
            refused.to_string(),
            "Image description is not available to you: no API key for it is set. A person \
             adds their own in My Settings (vault path vision/openrouter/api_key), or an admin \
             sets one for the agent or the install. Do not ask for the key in chat or try to \
             store it yourself.",
            "the turn's person is who the key is read for, and nobody else's is used"
        );
    }

    #[tokio::test]
    async fn a_turns_image_is_left_undescribed_with_one_sentence_for_a_person_with_no_key() {
        use workforce_agent::attachments::{
            build_task_content, MediaStrategies, TranscriptionStrategy, VisionStrategy,
        };
        use workforce_agent::llm::types::ContentBlock;
        use workforce_common::session::SessionAttachment;

        let hub =
            hub(|install| install.llm.vision = Some(vision("openrouter:anthropic:cheap"))).await;
        let Describer::Ready(llm, config) = &hub.service.describer else {
            panic!("a keyed describer is ready");
        };
        assert_eq!(
            legacy(config, ProviderKind::OpenRouter).key(),
            None,
            "this test needs VISION_API_KEY and OPENROUTER_API_KEY unset"
        );
        hub.service
            .global_vault
            .as_ref()
            .unwrap()
            .set(
                &format!("users/{ALICE}/vision/openrouter/api_key"),
                "sk-or-alices-own",
            )
            .unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let turn = SessionContext {
            user_id: Some(STRANGER.into()),
            ..SessionContext::default()
        };
        let attachments = Some(vec![SessionAttachment {
            filename: "chart.png".into(),
            mime_type: "image/png".into(),
            data: "AAAA".into(),
            artifact_id: None,
        }]);
        let content = turn
            .clone()
            .scope(build_task_content(
                None,
                None,
                "what is this?",
                &attachments,
                workspace.path(),
                MediaStrategies {
                    vision: VisionStrategy::DescribeWith(llm.as_ref(), config),
                    transcription: TranscriptionStrategy::Disabled,
                    video_frames: None,
                },
                None,
                &turn,
            ))
            .await;
        let said: Vec<&str> = content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } if text.contains("chart.png") => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            said,
            [
                "Image `chart.png` was attached but not described: Image description is not \
              available to you: no API key for it is set. A person adds their own in My \
              Settings (vault path vision/openrouter/api_key), or an admin sets one for the \
              agent or the install. Do not ask for the key in chat or try to store it yourself."
            ],
            "{content:?}"
        );
        assert!(!format!("{content:?}").contains("sk-or-"));
    }

    #[tokio::test]
    async fn a_describers_key_is_the_turn_persons_own_then_the_configs() {
        let dir = tempfile::tempdir().unwrap();
        let global = Arc::new(Vault::open(&dir.path().join("g.enc"), "test-key").unwrap());
        let config = LlmVisionConfig {
            api_key: Some("configured".into()),
            ..vision("openrouter:anthropic:cheap")
        };
        let key = crate::credentials::slot(
            Modality::Vision,
            "openrouter",
            None,
            Some(Arc::clone(&global)),
            legacy(&config, ProviderKind::OpenRouter),
        );
        let in_turn_of = |person: &str| {
            SessionContext {
                user_id: Some(person.into()),
                ..SessionContext::default()
            }
            .scope(async { key.get() })
        };
        assert_eq!(in_turn_of(ALICE).await.as_deref(), Ok("configured"));
        global
            .set(
                &format!("users/{ALICE}/vision/openrouter/api_key"),
                "alices-own",
            )
            .unwrap();
        assert_eq!(in_turn_of(ALICE).await.as_deref(), Ok("alices-own"));
        assert_eq!(in_turn_of(STRANGER).await.as_deref(), Ok("configured"));
    }

    #[test]
    fn a_route_with_no_bearer_key_keeps_the_environment_it_always_read() {
        for (kind, var) in [
            (ProviderKind::Bedrock, "AWS_ACCESS_KEY_ID"),
            (ProviderKind::Vllm, "VLLM_API_KEY"),
            (
                ProviderKind::VertexAnthropic,
                "GOOGLE_APPLICATION_CREDENTIALS",
            ),
        ] {
            assert!(!kind.takes_api_key() && crate::credentials::provider_env(kind).is_empty());
            let config = LlmVisionConfig {
                api_key: Some(var.into()),
                ..vision("x:y")
            };
            assert_eq!(legacy(&config, kind).key().as_deref(), Some(var));
        }
    }

    #[tokio::test]
    async fn a_misrouted_describer_is_kept_as_the_reason_it_cannot_run() {
        let hub = hub(|config| config.llm.vision = Some(vision("nonesuch:gpt-6.1-sol"))).await;
        let Describer::Misconfigured(why) = &hub.service.describer else {
            panic!("an unknown route builds no describer");
        };
        assert!(
            why.contains("'nonesuch:gpt-6.1-sol' names no known provider"),
            "{why}"
        );
    }
}
