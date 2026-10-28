//! What a subscription is to the hub: the kinds, logins, vaults and findings it shows.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::hub_core::logins::chatgpt_backend::ChatGptBackend;
use crate::hub_core::logins::device_login::{DeviceAuthClient, DeviceLogins};
use crate::hub_core::pools::SubscriptionPools;
use crate::vault::Vault;
use workforce_agent::llm::provider::ProviderKind;
use workforce_agent::llm::responses::{ChatGptModel, UnsupportedModel};
use workforce_agent::llm::tiers::{
    parse_provider_spec, split_routing_prefix, ModelTier, TierRegistry,
};
use workforce_agent::llm::{image, speech, video};
use workforce_agent::tools::image_gen::IMAGE_TOOL_NAME;
use workforce_agent::tools::speech_gen::SPEECH_TOOL_NAME;
use workforce_agent::tools::video_gen::VIDEO_TOOL_NAME;
use workforce_common::secret::KeySlot;
use workforce_common::settings::{
    is_removed_provider, AgentWorkspaceConfig, CodingCodexConfig, CodingEngineKind, ExecutorKind,
    LlmConfig, WorkforceConfig, REMOVED_PROVIDER_POINTER,
};
use workforce_common::subscription::{SubscriptionKind, SubscriptionRoute};

use crate::hub_core::resolved_config::ResolvedConfig;
use serde::Serialize;
use workforce_agent::llm::oai_common::budget_to_effort;

/// A CLI that runs an agent's whole turn, or one tool-less completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CliKind {
    ClaudeCode,
    Codex,
}

impl CliKind {
    /// The CLI a provider kind's calls run on, if they run on one.
    pub fn of(kind: ProviderKind) -> Option<Self> {
        kind.subscription_route().and_then(Self::of_route)
    }

    pub const fn route(self) -> SubscriptionRoute {
        match self {
            Self::ClaudeCode => SubscriptionRoute::ClaudeCode,
            Self::Codex => SubscriptionRoute::Codex,
        }
    }

    /// The subscription whose login pays for the CLI's calls.
    pub fn login(self) -> SubscriptionKind {
        self.route().kind()
    }

    /// Where a session keeps the transcript its turns on this CLI continue.
    pub const fn slot_key(self) -> &'static str {
        match self {
            Self::ClaudeCode => "agent_engine:claude-code",
            Self::Codex => "agent_engine:codex",
        }
    }

    /// The CLI a `route` runs on, if it runs on one.
    const fn of_route(route: SubscriptionRoute) -> Option<Self> {
        match route {
            SubscriptionRoute::ClaudeCode => Some(Self::ClaudeCode),
            SubscriptionRoute::Codex => Some(Self::Codex),
            SubscriptionRoute::ChatGpt => None,
        }
    }

    /// The level the CLI is asked for an agent's configured `effort`: the
    /// level itself when the CLI takes it, else the highest it takes below
    /// it (Claude Code has no `xhigh`), else the least it takes.
    pub fn effort_for(self, effort: workforce_agent::llm::anthropic::Effort) -> &'static str {
        let efforts = self.efforts();
        let rank = |name: &str| workforce_agent::llm::anthropic::Effort::parse(name);
        efforts
            .iter()
            .copied()
            .rfind(|name| rank(name).is_some_and(|level| level <= effort))
            .or_else(|| efforts.first().copied())
            .unwrap_or(effort.as_str())
    }

    /// Whether the CLI takes the level `name`.
    pub fn accepts(self, name: &str) -> bool {
        self.efforts()
            .iter()
            .any(|e| e.eq_ignore_ascii_case(name.trim()))
    }

    fn efforts(self) -> &'static [&'static str] {
        match self {
            Self::ClaudeCode => CodingEngineKind::ClaudeCode,
            Self::Codex => CodingEngineKind::Codex,
        }
        .efforts()
    }

    /// The effort a thinking `budget` asks of the CLI, when it takes one: a
    /// budget that turns thinking off asks for the least it takes.
    pub fn effort(self, budget: u32) -> Option<&'static str> {
        let efforts = match self {
            Self::ClaudeCode => CodingEngineKind::ClaudeCode,
            Self::Codex => CodingEngineKind::Codex,
        }
        .efforts();
        match budget_to_effort(budget) {
            "none" => efforts.first().copied(),
            asked => efforts
                .iter()
                .copied()
                .find(|e| e.eq_ignore_ascii_case(asked)),
        }
    }
}

/// How a finding bears on a config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// The agent cannot load the config.
    Refused,
    /// The config loads, but some calls will not run as written.
    Warning,
}

/// Something an agent's LLM config does that its operator should know
/// about, against the spec (or `[llm.logins…]` section) it concerns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub severity: Severity,
    pub spec: String,
    pub message: String,
}

impl Finding {
    pub fn refused(spec: &str, message: String) -> Self {
        Self {
            severity: Severity::Refused,
            spec: spec.to_string(),
            message,
        }
    }

    pub fn warning(spec: &str, message: String) -> Self {
        Self {
            severity: Severity::Warning,
            spec: spec.to_string(),
            message,
        }
    }

    pub fn is_refused(&self) -> bool {
        self.severity == Severity::Refused
    }
}

/// The install-wide settings an agent's config is reviewed against.
#[derive(Debug, Clone, Copy)]
pub struct Install<'a> {
    pub executor: &'a ExecutorKind,
    pub chatgpt_direct: bool,
    /// The short names Codex models go by.
    pub codex: &'a CodingCodexConfig,
}

impl<'a> Install<'a> {
    pub fn of(config: &'a WorkforceConfig) -> Self {
        Self {
            executor: &config.coding_session.executor.kind,
            chatgpt_direct: config.subscription.chatgpt_direct,
            codex: &config.coding_session.codex,
        }
    }
}

/// Why a subscription provider takes no pool account or token.
pub const LOGINS_ARE_NOT_ACCOUNTS: &str =
    "logins aren't accounts; connect a plan under My Settings, or store a Claude login with \
     claude_code.setup_token or a ChatGPT login with chatgpt.login or codex.setup_auth";

/// The provider kind a model spec routes to, when it names one.
pub fn routed_kind(spec: &str) -> Option<ProviderKind> {
    parse_provider_spec(spec.trim())
        .ok()
        .flatten()
        .map(|(kind, _)| kind)
}

/// The provider kinds that run on a subscription login, in the order an
/// editor offers them.
pub const SUBSCRIPTION_PROVIDERS: [ProviderKind; 3] = [
    ProviderKind::ChatGpt,
    ProviderKind::ClaudeCode,
    ProviderKind::Codex,
];

/// The models `kind`'s tiers name, most capable first, each once at the
/// highest tier that names it.
pub fn tiered_models(kind: ProviderKind, tiers: &TierRegistry) -> Vec<(ModelTier, String)> {
    ModelTier::ALL
        .into_iter()
        .filter_map(|tier| {
            tiers
                .resolve_for_tier(tier, kind.canonical_name())
                .map(|model| (tier, model))
        })
        .fold(Vec::new(), |mut models, (tier, model)| {
            if !models.iter().any(|(_, seen)| *seen == model) {
                models.push((tier, model));
            }
            models
        })
}

/// The models `kind`'s tiers name, most capable first, once each.
pub fn tier_models(kind: ProviderKind, tiers: &TierRegistry) -> Vec<String> {
    tiered_models(kind, tiers)
        .into_iter()
        .map(|(_, model)| model)
        .collect()
}

/// Every spec routing to a subscription provider on one of its tier models,
/// such as `claude-code:sonnet`.
pub fn route_models(tiers: &TierRegistry) -> Vec<String> {
    SUBSCRIPTION_PROVIDERS
        .into_iter()
        .flat_map(|kind| {
            tier_models(kind, tiers)
                .into_iter()
                .map(move |model| format!("{}:{model}", kind.canonical_name()))
        })
        .collect()
}

/// The model choices a turn or a tool loop runs on: the agent's provider and
/// its [`turn_models`].
pub fn turn_specs<'a>(
    llm: &'a LlmConfig,
    resolved: &'a ResolvedConfig,
) -> impl Iterator<Item = &'a String> + 'a {
    std::iter::once(&llm.provider).chain(turn_models(llm, resolved))
}

/// The models a turn or a tool loop runs on: the agent's default model,
/// failover chain, per-task preferences and voice model, its self-learning
/// model (run as an isolated turn) and its claim-grounding model (a tool
/// loop).
pub fn turn_models<'a>(
    llm: &'a LlmConfig,
    resolved: &'a ResolvedConfig,
) -> impl Iterator<Item = &'a String> + 'a {
    std::iter::once(&llm.default_model)
        .chain(&llm.failover)
        .chain(
            resolved
                .model_preferences
                .iter()
                .flat_map(|prefs| prefs.values()),
        )
        .chain(&resolved.voice_model)
        .chain(&resolved.self_learning.model)
        .chain(&resolved.claim_grounding.model)
}

/// Every model choice an agent's calls route through its provider chain: its
/// [`turn_specs`] and its tool-less background models.
pub fn chain_specs<'a>(
    llm: &'a LlmConfig,
    resolved: &'a ResolvedConfig,
) -> impl Iterator<Item = &'a String> + 'a {
    let memory = &resolved.memory;
    turn_specs(llm, resolved)
        .chain(&resolved.compaction.compaction_model)
        .chain(&memory.passive_model)
        .chain(&memory.consolidation_model)
        .chain(&memory.lesson_extract_model)
}

/// The subscriptions whose logins an agent's calls draw on, as its
/// [`chain_specs`] route them, each once in [`SubscriptionKind::ALL`] order.
pub fn routed_subscriptions(llm: &LlmConfig, resolved: &ResolvedConfig) -> Vec<SubscriptionKind> {
    let routed: Vec<SubscriptionKind> = chain_specs(llm, resolved)
        .filter_map(|spec| {
            routed_kind(spec)?
                .subscription_route()
                .map(SubscriptionRoute::kind)
        })
        .collect();
    SubscriptionKind::ALL
        .into_iter()
        .filter(|kind| routed.contains(kind))
        .collect()
}

/// The specs an agent's `[llm.vision]` names.
pub fn vision_specs(llm: &LlmConfig) -> impl Iterator<Item = &String> + '_ {
    llm.vision
        .iter()
        .flat_map(|v| v.provider.iter().chain([&v.model]))
}

/// Every model choice an agent's config routes: its [`chain_specs`] and its
/// vision model, which builds a provider of its own.
pub fn routed_specs<'a>(
    llm: &'a LlmConfig,
    resolved: &'a ResolvedConfig,
) -> impl Iterator<Item = &'a String> + 'a {
    chain_specs(llm, resolved).chain(vision_specs(llm))
}

/// A spec naming the removed `claude-max` provider, as its routing prefix
/// or as a whole.
pub fn removed_provider_finding(spec: &str) -> Option<Finding> {
    let name = spec.split(':').next().unwrap_or_default();
    is_removed_provider(name).then(|| {
        Finding::refused(
            spec,
            format!("{spec:?}: provider '{name}' was removed: {REMOVED_PROVIDER_POINTER}"),
        )
    })
}

/// A ChatGPT model the backend doesn't serve (retired from ChatGPT sign-ins,
/// or never offered) is a warning, since a call for it moves on down the
/// chain. Tier and provider names are left to the tiers.
pub fn chatgpt_model_finding(spec: &str, model: &str) -> Option<Finding> {
    if ModelTier::from_name(model).is_some() || ProviderKind::from_str_loose(model).is_some() {
        return None;
    }
    ChatGptModel::parse(model).err().map(|unknown| {
        Finding::warning(
            spec,
            format!("{spec:?}: {unknown}; calls for this model skip the chatgpt route"),
        )
    })
}

/// A Codex turn or tool loop runs as a plain function-calling loop, verified
/// on the models the ChatGPT backend serves: any other model is refused there.
pub fn codex_turn_model_finding(spec: &str, model: &str) -> Option<Finding> {
    let reason = match ChatGptModel::parse(model) {
        Ok(_) => return None,
        Err(UnsupportedModel::Retired(id)) => {
            format!("{id} retired from ChatGPT sign-ins on 2026-10-14")
        }
        Err(UnsupportedModel::Unknown(id)) => format!("{id} is not verified for agent turns"),
    };
    Some(Finding::refused(
        spec,
        format!("{spec:?}: {reason}; agent turns run codex:gpt-6.1-sol"),
    ))
}

/// The model each of an agent's Codex turn and tool-loop choices runs on,
/// against the spec naming it: a tier through the agent's tiers, a bare
/// `codex` entry at the tier of its default model, as a failover entry
/// runs, and a short name through Codex's own.
pub fn codex_turn_models<'a>(
    llm: &'a LlmConfig,
    resolved: &'a ResolvedConfig,
    codex: &'a CodingCodexConfig,
) -> impl Iterator<Item = (&'a str, String)> + 'a {
    let tiers = &resolved.tiers;
    let table = ProviderKind::Codex.canonical_name();
    let primary = ModelTier::from_name(&llm.default_model)
        .unwrap_or_else(|| tiers.classify(&llm.default_model));
    let on_primary = routed_kind(&llm.provider) == Some(ProviderKind::Codex);
    models_on(
        ProviderKind::Codex,
        turn_models(llm, resolved),
        turn_models(llm, resolved),
        on_primary,
    )
    .filter_map(move |(spec, model)| {
        let id = match (ModelTier::from_name(model), routed_kind(model)) {
            (Some(tier), _) => tiers.resolve_for_tier(tier, table)?,
            (None, Some(_)) => tiers.resolve_for_tier(primary, table)?,
            (None, None) => codex.resolve_model(model),
        };
        Some((spec, id))
    })
}

/// The models `kind` is sent, each against the spec naming it: every one of
/// `routed` that routes to `kind`, a bare `kind` among them, and, when
/// `on_primary` (`kind` is the agent's provider), every one of `unprefixed`
/// that routes nowhere.
pub fn models_on<'a>(
    kind: ProviderKind,
    routed: impl Iterator<Item = &'a String> + 'a,
    unprefixed: impl Iterator<Item = &'a String> + 'a,
    on_primary: bool,
) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
    let routed = routed
        .filter(move |spec| routed_kind(spec) == Some(kind))
        .map(|spec| (spec.as_str(), split_routing_prefix(spec.trim()).1));
    let unprefixed = unprefixed
        .filter(move |_| on_primary)
        .filter(|spec| routed_kind(spec).is_none())
        .map(|spec| (spec.as_str(), spec.trim()));
    routed.chain(unprefixed)
}

/// The ChatGPT models an agent's config names: every `chatgpt:`-routed spec,
/// and, when the agent's provider is `chatgpt`, every unprefixed spec its
/// chain sends to that provider.
pub fn chatgpt_models<'a>(
    llm: &'a LlmConfig,
    resolved: &'a ResolvedConfig,
) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
    models_on(
        ProviderKind::ChatGpt,
        routed_specs(llm, resolved),
        chain_specs(llm, resolved),
        routed_kind(&llm.provider) == Some(ProviderKind::ChatGpt),
    )
}

/// Everything worth telling an operator about an agent's LLM config, in the
/// order its refusals are reported: an invalid `[llm.logins]`; a route to
/// `codex` while coding rounds run locally (Codex's patch tool cannot be
/// switched off, so it runs only on the remote worker); a turn or tool loop
/// on a Codex model the ChatGPT backend doesn't serve; a vision model on
/// `codex`, whose completions carry no images.
/// Warnings name a ChatGPT model the backend doesn't serve, a `chatgpt`
/// route on an install with the provider off, and a `[know_how]
/// source_agent` the agent ignores for not being a test copy.
pub fn review_agent_llm(
    llm: &LlmConfig,
    resolved: &ResolvedConfig,
    install: &Install<'_>,
) -> Vec<Finding> {
    let invalid_logins = resolved
        .logins
        .validate()
        .err()
        .map(|e| Finding::refused("[llm.logins]", e.to_string()));
    let removed = routed_specs(llm, resolved).filter_map(|spec| removed_provider_finding(spec));
    let routes: Vec<(&String, ProviderKind)> = routed_specs(llm, resolved)
        .filter_map(|spec| routed_kind(spec).map(|kind| (spec, kind)))
        .collect();
    let local_codex = |kind: &ProviderKind| {
        *kind == ProviderKind::Codex && *install.executor != ExecutorKind::Remote
    };
    let codex_local = routes
        .iter()
        .filter(|(_, kind)| local_codex(kind))
        .map(|(spec, _)| {
            let message = format!(
                "{spec:?} routes to codex, which runs only on the remote worker: set \
                 [coding_session.executor] kind = \"remote\""
            );
            Finding::refused(spec, message)
        });
    let codex_turns = codex_turn_models(llm, resolved, install.codex)
        .filter_map(|(spec, model)| codex_turn_model_finding(spec, &model));
    let codex_vision = vision_specs(llm)
        .filter(|spec| routed_kind(spec) == Some(ProviderKind::Codex))
        .filter(|_| *install.executor == ExecutorKind::Remote)
        .map(|spec| {
            Finding::refused(
                spec,
                format!("{spec:?}: codex completions carry no images, so [llm.vision] can't run on codex"),
            )
        });
    let models = chatgpt_models(llm, resolved)
        .filter_map(|(spec, model)| chatgpt_model_finding(spec, model));
    let direct_off = routes
        .iter()
        .filter(|(_, kind)| *kind == ProviderKind::ChatGpt && !install.chatgpt_direct)
        .map(|(spec, _)| {
            Finding::warning(
                spec,
                format!(
                    "{spec:?}: the install has chatgpt_direct off; calls on this route are refused"
                ),
            )
        });
    let (refused, warnings): (Vec<Finding>, Vec<Finding>) = invalid_logins
        .into_iter()
        .chain(removed)
        .chain(codex_local)
        .chain(codex_turns)
        .chain(codex_vision)
        .chain(models)
        .chain(direct_off)
        .chain(media_findings(llm))
        .chain(effort_finding(llm, resolved))
        .chain(
            resolved
                .workspace
                .as_ref()
                .and_then(ignored_know_how_source),
        )
        .partition(Finding::is_refused);
    refused.into_iter().chain(warnings).collect()
}

/// What `[llm] effort` says that the agent's provider and model cannot
/// honour: a level they do not take is refused, naming the ones they do; a
/// provider or model with no effort control ignores a level, which is said.
fn effort_finding(llm: &LlmConfig, resolved: &ResolvedConfig) -> Option<Finding> {
    const SPEC: &str = "[llm] effort";
    let effort = resolved.effort.as_deref()?;
    let (kind, _) = parse_provider_spec(llm.provider.trim()).ok().flatten()?;
    let model = llm.default_model.trim();
    let offered = kind.model_efforts(model);
    let name = kind.canonical_name();
    if offered
        .iter()
        .any(|e| e.eq_ignore_ascii_case(effort.trim()))
    {
        return None;
    }
    let list = |levels: &[&str]| levels.join(", ");
    // Without effort control the level is ignored rather than refused, so
    // an agent switched onto such a model still loads.
    let uncontrolled = match kind.efforts().is_empty() {
        true => format!("{name} has no effort control"),
        false => format!("{model} on {name} has no effort control"),
    };
    match (offered.is_empty(), resolved.effort_level()) {
        (true, Some(_)) => Some(Finding::warning(
            SPEC,
            format!("{uncontrolled}, so effort = {effort:?} is ignored"),
        )),
        (true, None) => Some(Finding::refused(
            SPEC,
            format!(
                "effort = {effort:?} is not an effort level: use one of {}",
                list(workforce_agent::llm::anthropic::Effort::NAMES)
            ),
        )),
        (false, _) => Some(Finding::refused(
            SPEC,
            format!(
                "effort = {effort:?} is not a level {name} takes: use one of {}",
                list(offered)
            ),
        )),
    }
}

/// Each enabled media block its tool cannot be built from, checked with the
/// builder its install runs; keys are read per request, never at build, so
/// none is needed. A warning, not a refusal: the agent runs, without that
/// tool, and whoever saved the config learns why instead of finding it gone.
fn media_findings(llm: &LlmConfig) -> impl Iterator<Item = Finding> {
    let no_key = || KeySlot::fixed("");
    let image = llm
        .image
        .as_ref()
        .filter(|cfg| cfg.enabled)
        .and_then(|cfg| image::provider_from_config(cfg, |_| no_key()).err())
        .map(|e| ("[llm.image]", IMAGE_TOOL_NAME, e));
    let video = llm
        .video
        .as_ref()
        .filter(|cfg| cfg.enabled)
        .and_then(|cfg| video::route(cfg).err())
        .map(|e| ("[llm.video]", VIDEO_TOOL_NAME, e));
    let speech = llm
        .speech
        .as_ref()
        .filter(|cfg| cfg.enabled)
        .and_then(|cfg| speech::provider_from_config(cfg, Path::new(""), |_| no_key()).err())
        .map(|e| ("[llm.speech]", SPEECH_TOOL_NAME, e));
    [image, video, speech]
        .into_iter()
        .flatten()
        .map(|(block, tool, e)| {
            Finding::warning(block, format!("{tool} will not be installed: {e}"))
        })
}

/// `[know_how] source_agent` on an agent that is not a test copy, which
/// keeps reading its own know-how.
pub fn ignored_know_how_source(workspace: &AgentWorkspaceConfig) -> Option<Finding> {
    let source = workspace
        .know_how_source_set()
        .filter(|_| !workspace.is_test_copy())?;
    Some(Finding::warning(
        "[know_how] source_agent",
        format!(
            "source_agent = {source:?} is ignored: only a test copy ([evals] copy = true) reads \
             another agent's know-how, and this agent reads its own"
        ),
    ))
}

/// Refuse an agent's config with its first refused finding, logging its
/// warnings.
pub fn validate_agent_llm(
    llm: &LlmConfig,
    resolved: &ResolvedConfig,
    install: &Install<'_>,
) -> eyre::Result<()> {
    let findings = review_agent_llm(llm, resolved, install);
    findings
        .iter()
        .filter(|finding| !finding.is_refused())
        .for_each(|finding| tracing::warn!(spec = %finding.spec, "{}", finding.message));
    match findings.into_iter().find(Finding::is_refused) {
        Some(refused) => Err(eyre::eyre!(refused.message)),
        None => Ok(()),
    }
}

/// Why `[subscription] operator` pays for nothing, when it names no identity
/// in `identities`: the install's own work then falls to the install's login.
pub fn unknown_operator(
    config: &workforce_common::settings::SubscriptionConfig,
    identities: &dyn workforce_identity::store::IdentityStore,
) -> Option<String> {
    let operator = config.operator?;
    match identities.get_by_id(operator) {
        Ok(Some(_)) => None,
        Ok(None) => Some(format!(
            "[subscription] operator {operator} names no identity; the install's work is not billed to a person"
        )),
        Err(e) => Some(format!(
            "[subscription] operator {operator} could not be checked: {e}"
        )),
    }
}

/// The provider kind and tier key an agent with this `[llm] provider` spec
/// runs on: the ones it names, or Anthropic when it names no tier key.
pub fn agent_provider(spec: &str) -> eyre::Result<(ProviderKind, String)> {
    Ok(
        workforce_agent::llm::tiers::parse_provider_spec(spec)?.unwrap_or_else(|| {
            tracing::warn!(
                provider = %spec,
                "Provider names no tier key, falling back to anthropic"
            );
            (ProviderKind::Anthropic, "anthropic".to_string())
        }),
    )
}

/// What every agent's subscription providers share, built once at boot: the
/// ChatGPT backend, the concurrency pools, where one-shot CLI completions
/// run, how Codex runs and the ChatGPT device sign-ins.
#[derive(Clone)]
pub struct SharedSubscriptions {
    chatgpt: ChatGptBackend,
    pools: Arc<SubscriptionPools>,
    completions_dir: Arc<Path>,
    codex: Arc<CodingCodexConfig>,
    device_logins: DeviceLogins,
}

impl SharedSubscriptions {
    /// The hub's shared subscriptions, the ChatGPT sign-ins kept in
    /// `global_vault` when there is one.
    pub fn new(config: &WorkforceConfig, global_vault: Option<&Arc<Vault>>) -> Self {
        let data_dir = PathBuf::from(&config.hub.data_dir);
        let data_dir = std::path::absolute(&data_dir).unwrap_or(data_dir);
        Self {
            chatgpt: ChatGptBackend::new(&config.subscription),
            pools: Arc::new(SubscriptionPools::new(&config.subscription)),
            completions_dir: data_dir.join("cli-completions").into(),
            codex: Arc::new(config.coding_session.codex.clone()),
            device_logins: DeviceLogins::new(
                global_vault.cloned(),
                DeviceAuthClient::new(reqwest::Client::new()),
            ),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn with_chatgpt(self, chatgpt: ChatGptBackend) -> Self {
        Self { chatgpt, ..self }
    }

    /// These subscriptions with their sign-ins kept by `device_logins`.
    pub fn with_device_logins(self, device_logins: DeviceLogins) -> Self {
        Self {
            device_logins,
            ..self
        }
    }

    pub fn chatgpt(&self) -> &ChatGptBackend {
        &self.chatgpt
    }

    pub fn pools(&self) -> &SubscriptionPools {
        &self.pools
    }

    /// Where each one-shot CLI completion gets a directory of its own.
    pub fn completions_dir(&self) -> &Path {
        &self.completions_dir
    }

    /// How the Codex CLI runs on this hub.
    pub fn codex(&self) -> &CodingCodexConfig {
        &self.codex
    }

    /// The hub's ChatGPT device-code sign-ins.
    pub fn device_logins(&self) -> &DeviceLogins {
        &self.device_logins
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiered_models_keep_each_model_once_at_its_highest_tier() {
        let tiers = TierRegistry::from_toml(
            "[claude-code]\npowerful = \"opus\"\nstandard = \"opus\"\ncheap = \"haiku\"\n",
        )
        .unwrap();
        assert_eq!(
            tiered_models(ProviderKind::ClaudeCode, &tiers),
            [
                (ModelTier::Powerful, "opus".to_string()),
                (ModelTier::Cheap, "haiku".to_string()),
            ]
        );
        assert_eq!(
            tier_models(ProviderKind::ClaudeCode, &tiers),
            ["opus", "haiku"]
        );
        assert!(tiered_models(ProviderKind::ChatGpt, &tiers).is_empty());
    }
}
