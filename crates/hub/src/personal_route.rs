//! A person's own route: a call billed to them runs on their own
//! subscription login when they chose that, the hub's switch allows it and
//! the agent hasn't opted out. One decision ([`chosen`]), made where a
//! turn's routes are resolved ([`crate::turn_loop::turn_chain`]) and where a
//! tool-less call reaches the agent's provider ([`PersonalRouteLlm`]).
//! Everything after it is the path a `claude-code` or `codex` route always
//! takes, on the person's own logins only: never a login an agent or the hub
//! shares. A `codex` route runs only where the hub has its coding worker.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use tokio::sync::mpsc;
use uuid::Uuid;
use workforce_agent::llm::provider::{LlmProvider, ProviderKind};
use workforce_agent::llm::streaming::StreamEvent;
use workforce_agent::llm::tiers::TierRegistry;
use workforce_agent::llm::types::{LlmMessage, LlmResponse, LlmToolDef};
use workforce_common::personal_route::{
    PersonalRoute, PersonalRouteKind, RouteFallback, RouteWork,
};
use workforce_common::subscription::Payer;

use crate::hub_core::personal_routes::PersonalRoutes;
use crate::hub_core::subscriptions::CliKind;
use crate::resolved_config::ConfigCell;
use crate::subscription::{personal_logins_only, PlanLogins};

/// The kinds a person's route runs on so far.
const ROUTED: [PersonalRouteKind; 2] = [PersonalRouteKind::ClaudeCode, PersonalRouteKind::Codex];

/// Why a person's own Codex can't take their work on an agent: Codex's own
/// shell can't be switched off in the hub's process, so it runs only on the
/// hub's coding worker.
pub const CODEX_NEEDS_WORKER: &str =
    "running your work on your ChatGPT (Codex) needs this hub's coding worker, and it has none";

/// The CLI a person's route of `kind` runs on, once it runs on one.
pub fn cli_of(kind: PersonalRouteKind) -> Option<CliKind> {
    match kind {
        PersonalRouteKind::ClaudeCode => Some(CliKind::ClaudeCode),
        PersonalRouteKind::Codex => Some(CliKind::Codex),
        PersonalRouteKind::Devin => None,
    }
}

/// The provider kind a CLI's calls run on.
pub fn provider_of(cli: CliKind) -> ProviderKind {
    match cli {
        CliKind::ClaudeCode => ProviderKind::ClaudeCode,
        CliKind::Codex => ProviderKind::Codex,
    }
}

/// A person's own route, chosen for one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chosen {
    pub person: Uuid,
    pub route: PersonalRoute,
    /// The CLI the route runs on.
    pub cli: CliKind,
}

/// What decides one agent's calls' routes.
#[derive(Clone)]
pub struct RouteDecider {
    pub agent_id: String,
    pub routes: PersonalRoutes,
    pub config: ConfigCell,
}

impl RouteDecider {
    /// What decides `service`'s calls' routes.
    pub fn of(service: &crate::serve::AgentService) -> Self {
        Self {
            agent_id: service.agent_id.to_string(),
            routes: service.subscriptions().personal_routes().clone(),
            config: service.resolved.clone(),
        }
    }

    /// The person a call made now is billed to, when it runs on their own
    /// plan: the payer logins are drawn for, unless the call is billed to
    /// another person, whose route it is not.
    fn payer() -> Option<Uuid> {
        let person = crate::user_config::current_payer().person()?;
        match crate::user_config::explicit_billing_identity()
            .as_deref()
            .map(|bill| Payer::classify(Some(bill)))
        {
            Some(Payer::Person(other)) if other != person => None,
            _ => Some(person),
        }
    }

    /// The person's own route a call made now takes, if any: the hub's
    /// switch on for this agent, the payer a person with a route chosen
    /// here of a kind it allows. Anything else, a failed read included, is
    /// the hub's route.
    pub async fn chosen(&self) -> Option<Chosen> {
        self.chosen_for(Self::payer()?).await
    }

    /// The person's own route `person`'s calls on this agent take: the one
    /// decision [`Self::chosen`] makes for a call's payer, made for a
    /// person named, as what an agent is shown to run on for them says it.
    pub async fn chosen_for(&self, person: Uuid) -> Option<Chosen> {
        let switch = self
            .routes
            .for_agent(self.config.snapshot().personal_routes.as_ref());
        if !switch.enabled {
            return None;
        }
        let settings = self
            .routes
            .settings(person, &self.agent_id)
            .await
            .inspect_err(|e| tracing::warn!(agent = %self.agent_id, error = %format!("{e:#}"), "a person's route could not be read; the call runs on the hub's route"))
            .ok()?;
        let route = settings.chosen()?;
        let cli = cli_of(route.kind)?;
        (switch.admits(route.kind) && ROUTED.contains(&route.kind)).then_some(Chosen {
            person,
            route,
            cli,
        })
    }
}

/// Whether the logins a call made now draws on, outside a turn's own
/// routes, must be the payer's own: a coding session, a fan-out or a
/// `claude_code` call billed to a person whose own route is on for this
/// agent. Such work starts in a tool call that came back through the gated
/// MCP, so it never inherits the turn's [`personal_logins_only`] scope; it
/// decides again here from the principal that call restored.
///
/// With `fallback = wait` it is their own logins only, so the work stops
/// and says to connect one rather than draw on a login the agent or the
/// hub shares. With `fallback = hub` the person chose to let the hub's route
/// serve when theirs can't, so the agent's logins resolve as they always do:
/// the person's own first, then a shared one as `[llm.logins]` allows.
///
/// It narrows every kind of login, not only the route's: a Codex round for
/// a person on a `claude-code` route that waits runs on their own ChatGPT
/// login too, since everything billed to them stays on their own plans.
pub(crate) async fn own_logins_only(service: &crate::serve::AgentService) -> bool {
    RouteDecider::of(service)
        .chosen()
        .await
        .is_some_and(|chosen| chosen.route.fallback == RouteFallback::Wait)
}

/// `work` on the payer's own logins only when [`own_logins_only`] says so.
pub(crate) async fn own_logins_if_routed<F: std::future::Future>(
    service: &crate::serve::AgentService,
    work: F,
) -> F::Output {
    let own = own_logins_only(service).await;
    crate::agent_turn::personal_if(own, work).await
}

/// The model a call on `spec` for `work` runs on `route`, on `cli`: the
/// model the person chose for that kind of work when they chose one, else
/// the route's default. On Claude Code the default is the spec's own tier
/// (a tier name, a `provider:tier`, or the tier its model sits at in the
/// agent's registry). On Codex every tier runs the one ChatGPT-plan model
/// ([`crate::subscription::completion::CODEX_PLAN_MODEL`]), so the agent
/// behaves the same whatever tier it asked for.
pub fn personal_model(
    tiers: &TierRegistry,
    tier_key: &str,
    spec: &str,
    cli: CliKind,
    route: &PersonalRoute,
    work: RouteWork,
) -> String {
    if let Some(chosen) = route.for_work(work) {
        return own_model(tiers, chosen, cli);
    }
    if cli == CliKind::Codex {
        return crate::subscription::completion::CODEX_PLAN_MODEL.to_string();
    }
    let (_, model) = tiers.resolve_routed_model(spec, tier_key);
    claude_at(tiers, tiers.classify(&model))
}

/// The Claude Code model at `tier`, as the agent's registry names it.
fn claude_at(tiers: &TierRegistry, tier: workforce_agent::llm::tiers::ModelTier) -> String {
    let kind = ProviderKind::ClaudeCode;
    tiers
        .resolve_for_tier(tier, kind.canonical_name())
        .or_else(|| {
            crate::subscription::tier_models(kind, tiers)
                .into_iter()
                .next()
        })
        .unwrap_or_else(|| "sonnet".to_string())
}

/// The model a person's own choice `chosen` names on `cli`: a tier is the
/// route's model at it, anything else the model itself.
fn own_model(tiers: &TierRegistry, chosen: &str, cli: CliKind) -> String {
    match (
        workforce_agent::llm::tiers::ModelTier::from_name(chosen),
        cli,
    ) {
        (Some(_), CliKind::Codex) => crate::subscription::completion::CODEX_PLAN_MODEL.to_string(),
        (Some(tier), CliKind::ClaudeCode) => claude_at(tiers, tier),
        (None, _) => chosen.to_string(),
    }
}

/// The kind of background work a tool-less call on `spec` is: light when it
/// runs the agent's light model, else heavy (an agent whose light and heavy
/// models are one reads as light).
pub fn background_work(
    spec: &str,
    background: &crate::resolved_config::BackgroundModels,
) -> RouteWork {
    if background.light.as_deref() == Some(spec) {
        RouteWork::Light
    } else {
        RouteWork::Heavy
    }
}

/// The agent's provider, with its tool-less calls made for a person on
/// their own route as one-shot completions on their own Claude or Codex
/// login (Codex's on the hub's coding worker): a
/// background call (compaction, a summary, a classifier) billed to them.
/// A call with tools, or one with no person's route behind it, reaches the
/// agent's provider untouched; a turn's own routes are chosen apart from
/// this, in [`crate::turn_loop::turn_chain`].
pub struct PersonalRouteLlm {
    inner: Arc<dyn LlmProvider>,
    decider: RouteDecider,
    plans: PlanLogins,
    tiers: workforce_agent::llm::tiers::EquivalencesCell,
    tier_key: Arc<parking_lot::RwLock<String>>,
    default_model: Arc<parking_lot::RwLock<String>>,
}

impl PersonalRouteLlm {
    pub fn new(
        inner: Arc<dyn LlmProvider>,
        decider: RouteDecider,
        plans: PlanLogins,
        tiers: workforce_agent::llm::tiers::EquivalencesCell,
        tier_key: Arc<parking_lot::RwLock<String>>,
        default_model: Arc<parking_lot::RwLock<String>>,
    ) -> Self {
        Self {
            inner,
            decider,
            plans,
            tiers,
            tier_key,
            default_model,
        }
    }

    /// The person's own one-shot provider for a tool-less call on `model`
    /// made now, and the model it runs on; `None` for the agent's provider.
    async fn personal(
        &self,
        tools: &[LlmToolDef],
        model: Option<&str>,
    ) -> Option<Result<(Arc<dyn LlmProvider>, String, RouteFallback)>> {
        if !tools.is_empty() {
            return None;
        }
        let chosen = self.decider.chosen().await?;
        let fallback = chosen.route.fallback;
        let cli = chosen.cli;
        let spec = model.map_or_else(|| self.default_model.read().clone(), str::to_string);
        let work = background_work(&spec, &self.decider.config.snapshot().background);
        let model = personal_model(
            &TierRegistry::from_cell(&self.tiers),
            &self.tier_key.read(),
            &spec,
            cli,
            &chosen.route,
            work,
        );
        match self.plans.provider(provider_of(cli), Some(&model), None) {
            Ok(provider) => Some(Ok((provider, model, fallback))),
            Err(e) => {
                tracing::warn!(agent = %self.decider.agent_id, route = chosen.route.kind.as_str(), error = %e, "a person's own route can't be built");
                // Waiting means the call never reaches the hub's route.
                (fallback == RouteFallback::Wait).then(|| Err(e))
            }
        }
    }

    fn fell_back(&self, error: &eyre::Report) {
        tracing::info!(
            agent = %self.decider.agent_id,
            error = %format!("{error:#}"),
            "a person's own route couldn't serve a background call; it runs on the hub's route as they chose"
        );
    }
}

#[async_trait]
impl LlmProvider for PersonalRouteLlm {
    async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        enable_cache: bool,
    ) -> Result<LlmResponse> {
        if let Some(personal) = self.personal(tools, model).await {
            let (provider, own_model, fallback) = personal?;
            let answer = personal_logins_only(provider.create_message(
                system_prompt,
                messages,
                tools,
                Some(&own_model),
                max_tokens,
                enable_cache,
            ))
            .await;
            match (answer, fallback) {
                (Err(e), RouteFallback::Hub) => self.fell_back(&e),
                (answer, _) => return answer,
            }
        }
        self.inner
            .create_message(
                system_prompt,
                messages,
                tools,
                model,
                max_tokens,
                enable_cache,
            )
            .await
    }

    async fn create_message_stream(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        enable_cache: bool,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        if let Some(personal) = self.personal(tools, model).await {
            let (provider, own_model, fallback) = personal?;
            // A one-shot completion streams its answer only once it has
            // it, so a failure has sent nothing the hub's route would
            // repeat.
            let answer = personal_logins_only(provider.create_message_stream(
                system_prompt,
                messages,
                tools,
                Some(&own_model),
                max_tokens,
                enable_cache,
                tx.clone(),
            ))
            .await;
            match (answer, fallback) {
                (Err(e), RouteFallback::Hub) => self.fell_back(&e),
                (answer, _) => return answer,
            }
        }
        self.inner
            .create_message_stream(
                system_prompt,
                messages,
                tools,
                model,
                max_tokens,
                enable_cache,
                tx,
            )
            .await
    }

    async fn verify_credentials(&self) -> Result<()> {
        self.inner.verify_credentials().await
    }

    async fn context_floor(&self, model: &str) -> Option<u32> {
        self.inner.context_floor(model).await
    }
}

#[cfg(test)]
mod tests;
