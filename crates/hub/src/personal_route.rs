//! A person's own route: a call billed to them runs on their own
//! subscription login when they chose that, the hub's switch allows it and
//! the agent hasn't opted out. One decision ([`chosen`]), made where a
//! turn's routes are resolved ([`crate::turn_loop::turn_chain`]) and where a
//! tool-less call reaches the agent's provider ([`PersonalRouteLlm`]).
//! Everything after it is the path a `claude-code` route always takes, on
//! the person's own logins only: never a login an agent or the hub shares.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use tokio::sync::mpsc;
use uuid::Uuid;
use workforce_agent::llm::provider::{LlmProvider, ProviderKind};
use workforce_agent::llm::streaming::StreamEvent;
use workforce_agent::llm::tiers::TierRegistry;
use workforce_agent::llm::types::{LlmMessage, LlmResponse, LlmToolDef};
use workforce_common::personal_route::{PersonalRoute, PersonalRouteKind, RouteFallback};
use workforce_common::subscription::Payer;

use crate::hub_core::personal_routes::PersonalRoutes;
use crate::resolved_config::ConfigCell;
use crate::subscription::{personal_logins_only, PlanLogins};

/// The kinds a person's route runs on so far.
const ROUTED: [PersonalRouteKind; 1] = [PersonalRouteKind::ClaudeCode];

/// A person's own route, chosen for one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chosen {
    pub person: Uuid,
    pub route: PersonalRoute,
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
        let switch = self
            .routes
            .for_agent(self.config.snapshot().personal_routes.as_ref());
        if !switch.enabled {
            return None;
        }
        let person = Self::payer()?;
        let settings = self
            .routes
            .settings(person, &self.agent_id)
            .await
            .inspect_err(|e| tracing::warn!(agent = %self.agent_id, error = %format!("{e:#}"), "a person's route could not be read; the call runs on the hub's route"))
            .ok()?;
        let route = settings.chosen()?;
        (switch.admits(route.kind) && ROUTED.contains(&route.kind))
            .then_some(Chosen { person, route })
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

/// The model a call on `spec` runs on a person's `claude-code` route: the
/// spec's own tier (a tier name, a `provider:tier`, or the tier its model
/// sits at in the agent's registry), on Claude Code.
pub fn personal_model(tiers: &TierRegistry, tier_key: &str, spec: &str) -> String {
    let (_, model) = tiers.resolve_routed_model(spec, tier_key);
    let tier = tiers.classify(&model);
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

/// The agent's provider, with its tool-less calls made for a person on
/// their own route as one-shot completions on their own Claude login: a
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
        let spec = model.map_or_else(|| self.default_model.read().clone(), str::to_string);
        let model = personal_model(
            &TierRegistry::from_cell(&self.tiers),
            &self.tier_key.read(),
            &spec,
        );
        match self
            .plans
            .provider(ProviderKind::ClaudeCode, Some(&model), None)
        {
            Ok(provider) => Some(Ok((provider, model, fallback))),
            Err(e) => {
                tracing::warn!(agent = %self.decider.agent_id, error = %e, "a person's Claude route can't be built");
                // Waiting means the call never reaches the hub's route.
                (fallback == RouteFallback::Wait).then(|| Err(e))
            }
        }
    }

    fn fell_back(&self, error: &eyre::Report) {
        tracing::info!(
            agent = %self.decider.agent_id,
            error = %format!("{error:#}"),
            "a person's own Claude route couldn't serve a background call; it runs on the hub's route as they chose"
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
