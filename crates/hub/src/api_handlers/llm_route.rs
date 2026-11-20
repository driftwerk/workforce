//! A person's own route from the web: whether their work runs on the hub's
//! models or on their own subscription. `/api/me/llm-route` is their
//! setting for every agent; `/api/me/agents/{id}/llm-route` their setting
//! for one agent, which wins. A route of a kind can only be chosen while
//! the hub's `[llm.personal_routes]` switch (and the agent) offers it and
//! the person has a working login of that kind of their own. Every change
//! is kept in the audit table beside the settings. These routes act for a
//! person signed in themselves, never for a paired device.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use workforce_agent::llm::provider::ProviderKind;
use workforce_agent::llm::responses::ChatGptModel;
use workforce_agent::llm::tiers::{ModelTier, TierRegistry};
use workforce_common::personal_route::{
    AgentRoute, PersonalRoute, PersonalRouteKind, PersonalRoutesConfig, RouteFallback,
};
use workforce_common::settings::{ExecutorKind, LoginPolicy};
use workforce_common::subscription::{AgentOwner, CredentialTier, Payer, SharedServes};

use crate::gateway::{AgentViewer, GatewayState};
use crate::hub_core::logins::cooldown::CooldownStore;
use crate::hub_core::logins::resolve::{holding, Login, Vaults};
use crate::hub_core::personal_routes::{PersonRouteStore, PersonalRoutes};
use crate::hub_core::subscriptions::SharedSubscriptions;
use crate::serve::AgentService;
use crate::subscription::catalog::PlanModel;

type ApiError = (StatusCode, Json<serde_json::Value>);
type ApiResult<T> = Result<Json<T>, ApiError>;

fn err(status: StatusCode, msg: &str) -> ApiError {
    (status, Json(serde_json::json!({"error": msg})))
}

/// The kinds a route can run on today; the others are refused with a plain
/// reason rather than kept and silently left on the hub's route.
const ROUTABLE: [PersonalRouteKind; 2] = [PersonalRouteKind::ClaudeCode, PersonalRouteKind::Codex];

/// One kind of route as the person may pick it where they are: every kind
/// is listed, each with why it can't be picked when it can't, so a picker
/// greys it out with the reason rather than leaving it off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteOption {
    kind: PersonalRouteKind,
    /// How the kind reads to a person: "Claude".
    title: &'static str,
    /// Why it can't be picked here; none when it can.
    refused: Option<String>,
    /// The ids of the models a route of this kind can be set to run,
    /// beside the tiers (`cheap`, `standard`, `powerful`), as `choices`
    /// lists them: kept for a page from before `choices`.
    models: Vec<String>,
    /// The models a route of this kind can be set to run, beside the
    /// tiers, each with the name a picker shows: on Claude, the ones the
    /// person's own login runs, in its order.
    choices: Vec<RouteModel>,
    /// Where `choices` come from.
    models_from: ModelsFrom,
    /// Why `choices` aren't the login's own, when they were asked for and
    /// couldn't be read.
    models_note: Option<String>,
    /// What each kind of work runs on when the person sets none.
    defaults: RouteDefaults,
}

/// One model a route can be set to run, as a picker names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteModel {
    /// What a route's setting names: `claude-fable-5-2`, `opus`.
    id: String,
    /// What a person reads: "Fable 5.2".
    name: String,
    /// The other ids the login lists that run this same model (the full id
    /// behind an alias, or the alias for a full id), so a setting naming
    /// one of them reads as this choice.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    same_as: Vec<String>,
}

/// Where a route's models come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelsFrom {
    /// The person's own login listed them.
    Login,
    /// The hub's own list: its tiers' Claude models, or the ChatGPT models
    /// it knows.
    Hub,
}

/// The models a person's Claude login lists, as far as they were asked.
#[derive(Debug, Clone, Default)]
enum ClaudeList {
    /// Not asked: the route can't be picked here, or there's no login.
    #[default]
    Unasked,
    /// Asked, and never read.
    Unread,
    Read(Arc<Vec<PlanModel>>),
}

/// The models a route of a kind offers, and the ids it accepts beside the
/// tiers and `chat`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Offered {
    choices: Vec<RouteModel>,
    accepted: Vec<String>,
    from: ModelsFrom,
    note: Option<String>,
}

/// Said when a Claude login's models couldn't be read: a sentence a
/// picker shows as it is.
const UNREAD_NOTE: &str =
    "Your Claude login's models couldn't be read just now, so the hub's own Claude models are offered.";

/// How long a settings request waits for a Claude login's models before it
/// answers with the hub's: the read goes on, and fills the list for the
/// next request.
const LIST_WAIT: std::time::Duration = std::time::Duration::from_secs(4);

/// What each kind of work on a route runs when the person sets none, as a
/// picker says it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteDefaults {
    conversation: String,
    light: String,
    heavy: String,
}

/// The person's every-agent setting as they see it.
#[derive(Debug, Serialize)]
pub struct MyRouteView {
    /// Whether this hub runs people's work on their own subscriptions.
    available: bool,
    /// The kinds they may pick here.
    allow: Vec<PersonalRouteKind>,
    route: Option<PersonalRoute>,
    /// Every kind, and whether they may pick it for every agent now.
    options: Vec<RouteOption>,
}

/// The person's setting for one agent as they see it.
#[derive(Debug, Serialize)]
pub struct MyAgentRouteView {
    agent_id: String,
    /// Whether the agent runs people's work on their own subscriptions.
    available: bool,
    allow: Vec<PersonalRouteKind>,
    /// Their setting for this agent: a route, `"hub"`, or none (as in their
    /// every-agent setting).
    route: Option<AgentRoute>,
    every_agent: Option<PersonalRoute>,
    /// The route their work on this agent takes; none is the hub's.
    effective: Option<PersonalRoute>,
    /// Every kind, and whether they may pick it for this agent now.
    options: Vec<RouteOption>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteWrite<T> {
    /// The new setting; `null` clears it. The key is required, so an empty
    /// body is refused rather than read as a clear.
    #[serde(
        deserialize_with = "present",
        bound(deserialize = "T: Deserialize<'de>")
    )]
    route: Option<T>,
}

fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

fn routes(state: &GatewayState) -> Result<PersonalRoutes, ApiError> {
    state
        .registry
        .host()
        .map(|host| host.subscriptions().personal_routes().clone())
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "no agents on this hub"))
}

fn store(routes: &PersonalRoutes) -> Result<Arc<dyn PersonRouteStore>, ApiError> {
    routes.store().ok_or_else(|| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "this hub keeps no route settings (it has no Postgres)",
        )
    })
}

fn store_failed(e: eyre::Report) -> ApiError {
    tracing::error!(error = %format!("{e:#}"), "route settings store failed");
    err(
        StatusCode::INTERNAL_SERVER_ERROR,
        "the route settings could not be read or saved",
    )
}

fn agent_for(
    state: &GatewayState,
    claims: crate::jwt::Claims,
    agent_id: &str,
) -> Result<Arc<AgentService>, ApiError> {
    state
        .registry
        .get(agent_id)
        .filter(|_| AgentViewer::new(state, claims.into()).can_see(agent_id))
        .ok_or_else(|| crate::gateway::not_found_agent(agent_id))
}

/// The switch as it stands for `service`: the hub's, narrowed by the
/// agent's own `[llm.personal_routes]`.
fn switch_for(routes: &PersonalRoutes, service: &AgentService) -> PersonalRoutesConfig {
    routes.for_agent(service.resolved.snapshot().personal_routes.as_ref())
}

/// `person`'s own working login for `kind`, the first at one of `tiers` in
/// `vaults`, each read once: one at its usage limit still counts; one never
/// set, or refused, does not.
fn own_login(
    kind: PersonalRouteKind,
    vaults: &Vaults<'_>,
    tiers: &[CredentialTier],
    person: Uuid,
) -> Option<Login> {
    let kind = kind.login_kind()?;
    let now = Utc::now();
    let cooldowns = CooldownStore::global();
    holding(
        kind,
        vaults,
        &Payer::Person(person),
        &AgentOwner::Unowned,
        &LoginPolicy {
            tiers: tiers.to_vec(),
            shared_serves: SharedServes::Anyone,
        },
        |login| {
            cooldowns
                .dead_at(&login.slot, &login.credential.fingerprint(), now)
                .is_none()
        },
    )
}

/// Whether `person` has a working login of their own for `kind`
/// ([`own_login`]).
fn has_own_login(
    kind: PersonalRouteKind,
    vaults: &Vaults<'_>,
    tiers: &[CredentialTier],
    person: Uuid,
) -> bool {
    own_login(kind, vaults, tiers, person).is_some()
}

/// The models `login` (the person's own Claude login, when they have one)
/// lists, asked only when `asked`: a route that can't be picked here
/// never starts a CLI. A read that takes longer than `wait` goes on in the
/// background, and this answers [`ClaudeList::Unread`] meanwhile.
async fn claude_list(
    shared: &SharedSubscriptions,
    login: Option<Login>,
    asked: bool,
    wait: std::time::Duration,
) -> ClaudeList {
    let (Some(login), true) = (login, asked) else {
        return ClaudeList::Unasked;
    };
    let shared = shared.clone();
    let read = tokio::spawn(crate::claude_code::with_program(async move {
        crate::subscription::catalog::claude_models_of(&shared, &login).await
    }));
    match tokio::time::timeout(wait, read).await {
        Ok(Ok(Some(models))) => ClaudeList::Read(models),
        Ok(Ok(None)) => ClaudeList::Unread,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "a Claude login's model list read ended early");
            ClaudeList::Unread
        }
        Err(_) => ClaudeList::Unread,
    }
}

/// Whether `service` runs Codex: only on the hub's coding worker.
fn has_worker(service: &AgentService) -> bool {
    service.config.coding_session.executor.kind == ExecutorKind::Remote
}

/// Why `route` can't be chosen where `switch` holds, the person holding
/// a working login of its kind or not, on a hub with its coding worker or
/// not; `None` when it can.
fn refusal(
    route: &PersonalRoute,
    hub_on: bool,
    switch: &PersonalRoutesConfig,
    has_login: bool,
    worker: bool,
) -> Option<String> {
    let title = route.kind.title();
    if !hub_on {
        return Some("this hub doesn't run people's work on their own subscriptions".into());
    }
    if !switch.enabled {
        return Some("this agent always runs on the hub's own route".into());
    }
    if !switch.allow.contains(&route.kind) {
        return Some(format!(
            "running your work on your {title} isn't offered here"
        ));
    }
    if !ROUTABLE.contains(&route.kind) {
        return Some(format!(
            "running your work on your {title} isn't available yet"
        ));
    }
    if route.kind == PersonalRouteKind::Codex && !worker {
        return Some(crate::personal_route::CODEX_NEEDS_WORKER.into());
    }
    if !has_login {
        return Some(format!(
            "connect your own {title} login in My Settings first"
        ));
    }
    None
}

/// The models a route of `kind` can be set to run beside the tiers: on
/// Claude, the ones the person's own login lists (`claude`), else the
/// Claude Code models the hub's tiers name; on Codex, the models a ChatGPT
/// plan serves (never one retired from it).
fn route_models(kind: PersonalRouteKind, tiers: &TierRegistry, claude: &ClaudeList) -> Offered {
    let hub = |ids: Vec<String>, name: fn(&str) -> String, note: Option<&str>| Offered {
        choices: ids
            .iter()
            .map(|id| RouteModel {
                id: id.clone(),
                name: name(id),
                same_as: Vec::new(),
            })
            .collect(),
        accepted: ids,
        from: ModelsFrom::Hub,
        note: note.map(str::to_string),
    };
    match (kind, claude) {
        (PersonalRouteKind::ClaudeCode, ClaudeList::Read(listed)) => login_models(listed),
        (PersonalRouteKind::ClaudeCode, unread) => hub(
            crate::subscription::tier_models(ProviderKind::ClaudeCode, tiers),
            str::to_string,
            matches!(unread, ClaudeList::Unread).then_some(UNREAD_NOTE),
        ),
        (PersonalRouteKind::Codex, _) => hub(
            ChatGptModel::ALL
                .iter()
                .map(|model| model.id().to_string())
                .collect(),
            chatgpt_name,
            None,
        ),
        (PersonalRouteKind::Devin, _) => hub(Vec::new(), str::to_string, None),
    }
}

/// The models a Claude login lists, as a picker offers them: in the
/// login's order, each once. The CLI lists an alias beside the model it
/// runs now (`sonnet` and `claude-sonnet-5-5`); the first listed stands for
/// both, and names the other as the same model. Ids are folded only when
/// one runs the other, never by name. Every id listed is accepted, and
/// every model an alias runs.
fn login_models(listed: &[PlanModel]) -> Offered {
    // What an id runs: an alias's model, else the id itself.
    let runs = |m: &PlanModel| m.resolves_to.clone().unwrap_or_else(|| m.id.clone());
    let mut choices: Vec<(RouteModel, String)> = Vec::new();
    for model in listed {
        let target = runs(model);
        match choices
            .iter_mut()
            .find(|(c, ran)| c.id == model.id || *ran == target)
        {
            Some((choice, _)) => {
                for id in std::iter::once(&model.id).chain(&model.resolves_to) {
                    if *id != choice.id && !choice.same_as.contains(id) {
                        choice.same_as.push(id.clone());
                    }
                }
            }
            None => {
                let same_as = model
                    .resolves_to
                    .iter()
                    .filter(|r| **r != model.id)
                    .cloned()
                    .collect();
                choices.push((
                    RouteModel {
                        id: model.id.clone(),
                        name: model.name.clone(),
                        same_as,
                    },
                    target,
                ));
            }
        }
    }
    let mut accepted: Vec<String> = Vec::new();
    for id in listed
        .iter()
        .flat_map(|m| std::iter::once(&m.id).chain(&m.resolves_to))
    {
        if !accepted.contains(id) {
            accepted.push(id.clone());
        }
    }
    Offered {
        choices: choices.into_iter().map(|(choice, _)| choice).collect(),
        accepted,
        from: ModelsFrom::Login,
        note: None,
    }
}

/// A built-in ChatGPT model's name, made from its id as Desk's own table
/// words them: `gpt-6.1-sol` is "GPT-6.1 Sol". An id of another shape is
/// shown as it is.
fn chatgpt_name(id: &str) -> String {
    let Some(rest) = id.strip_prefix("gpt-") else {
        return id.to_string();
    };
    let mut parts = rest.split('-');
    let version = parts.next().unwrap_or_default();
    if version.is_empty() {
        return id.to_string();
    }
    let words: Vec<String> = parts
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            chars
                .next()
                .map(|c| c.to_uppercase().chain(chars).collect())
                .unwrap_or_default()
        })
        .collect();
    match words.is_empty() {
        true => format!("GPT-{version}"),
        false => format!("GPT-{version} {}", words.join(" ")),
    }
}

/// What each kind of work on a route of `kind` runs when the person sets
/// none.
fn route_defaults(kind: PersonalRouteKind) -> RouteDefaults {
    let same = |s: &str| RouteDefaults {
        conversation: s.to_string(),
        light: s.to_string(),
        heavy: s.to_string(),
    };
    match kind {
        PersonalRouteKind::Codex => same(crate::subscription::completion::CODEX_PLAN_MODEL),
        // The tier the agent asks for, on Claude.
        _ => RouteDefaults {
            conversation: "the agent's own tier".into(),
            light: "the agent's light tier".into(),
            heavy: "the agent's heavy tier".into(),
        },
    }
}

/// Why the models `route` names can't be kept, checked against what a
/// login of its kind serves (`offered`); `None` when they can. A retired
/// model is refused with when it was retired and what to use instead. A
/// Claude model `before` (the setting `route` replaces) already named for
/// the same work is kept as it was, so a model since gone from the login's
/// list, or a list that can't be read now, never refuses a change to
/// something else.
fn model_refusal(
    route: &PersonalRoute,
    offered: &Offered,
    before: Option<&PersonalRoute>,
) -> Option<String> {
    if let Some(key) = route.unknown_preference() {
        return Some(format!(
            "a route sets only its conversation model under model_preferences, not \"{key}\""
        ));
    }
    let kept = before
        .filter(|before| before.kind == route.kind)
        .map(PersonalRoute::named)
        .unwrap_or_default();
    for (work, model) in route.named() {
        if ModelTier::from_name(model).is_some() {
            continue;
        }
        match route.kind {
            PersonalRouteKind::Codex => {
                if let Err(refused) = ChatGptModel::parse(model) {
                    return Some(format!("your {work} model can't be used: {refused}"));
                }
            }
            PersonalRouteKind::ClaudeCode
                if !offered.accepted.iter().any(|m| m == model)
                    && !kept.contains(&(work, model)) =>
            {
                let whose = match offered.from {
                    ModelsFrom::Login => "your Claude login runs",
                    ModelsFrom::Hub => "this hub runs",
                };
                let names: Vec<&str> = offered.choices.iter().map(|c| c.id.as_str()).collect();
                return Some(format!(
                    "your {work} model can't be used: {model} isn't a Claude model {whose} (pick one of {})",
                    names.join(", ")
                ));
            }
            _ => {}
        }
    }
    None
}

/// Every kind as `person` may pick it where `switch` holds, `has_login`
/// saying whether they hold a working login of a kind of their own, on a
/// hub with its coding worker or not.
fn options(
    hub_on: bool,
    switch: &PersonalRoutesConfig,
    has_login: impl Fn(PersonalRouteKind) -> bool,
    worker: bool,
    tiers: &TierRegistry,
    claude: &ClaudeList,
) -> Vec<RouteOption> {
    PersonalRouteKind::ALL
        .into_iter()
        .map(|kind| {
            let offered = route_models(kind, tiers, claude);
            RouteOption {
                kind,
                title: kind.title(),
                refused: kind_refusal(kind, hub_on, switch, has_login(kind), worker),
                models: offered.choices.iter().map(|c| c.id.clone()).collect(),
                choices: offered.choices,
                models_from: offered.from,
                models_note: offered.note,
                defaults: route_defaults(kind),
            }
        })
        .collect()
}

/// Why a route of `kind` can't be picked ([`refusal`]).
fn kind_refusal(
    kind: PersonalRouteKind,
    hub_on: bool,
    switch: &PersonalRoutesConfig,
    has_login: bool,
    worker: bool,
) -> Option<String> {
    refusal(
        &PersonalRoute::new(kind, Default::default()),
        hub_on,
        switch,
        has_login,
        worker,
    )
}

/// The person's own logins where a route reads them, and the hub's
/// subscriptions their models are listed through.
struct Logins<'a> {
    /// None on a hub without a host agent: nothing is listed there.
    shared: Option<&'a SharedSubscriptions>,
    vaults: Vaults<'a>,
    tiers: &'a [CredentialTier],
    person: Uuid,
}

impl Logins<'_> {
    fn of(&self, kind: PersonalRouteKind) -> Option<Login> {
        own_login(kind, &self.vaults, self.tiers, self.person)
    }

    /// Every kind as the person may pick it here, their Claude login read
    /// once for whether they hold it and for its models.
    async fn options(
        &self,
        hub_on: bool,
        switch: &PersonalRoutesConfig,
        worker: bool,
        tiers: &TierRegistry,
    ) -> Vec<RouteOption> {
        let claude_login = self.of(PersonalRouteKind::ClaudeCode);
        let has_claude = claude_login.is_some();
        let claude = self
            .claude(claude_login, hub_on, switch, worker, LIST_WAIT)
            .await;
        options(
            hub_on,
            switch,
            |kind| match kind {
                PersonalRouteKind::ClaudeCode => has_claude,
                kind => self.of(kind).is_some(),
            },
            worker,
            tiers,
            &claude,
        )
    }

    /// The models `login` (the person's Claude login) lists, asked only
    /// while a Claude route can be picked here.
    async fn claude(
        &self,
        login: Option<Login>,
        hub_on: bool,
        switch: &PersonalRoutesConfig,
        worker: bool,
        wait: std::time::Duration,
    ) -> ClaudeList {
        let kind = PersonalRouteKind::ClaudeCode;
        let asked = kind_refusal(kind, hub_on, switch, login.is_some(), worker).is_none();
        match self.shared {
            Some(shared) => claude_list(shared, login, asked, wait).await,
            None => ClaudeList::Unasked,
        }
    }

    /// The models a route of `route`'s kind offers here, for checking a
    /// save: a Claude login's list is read only for a Claude route.
    async fn offered(
        &self,
        route: &PersonalRoute,
        login: Option<Login>,
        hub_on: bool,
        switch: &PersonalRoutesConfig,
        worker: bool,
        tiers: &TierRegistry,
    ) -> Offered {
        let claude = match route.kind {
            PersonalRouteKind::ClaudeCode => {
                self.claude(login, hub_on, switch, worker, LIST_WAIT).await
            }
            _ => ClaudeList::Unasked,
        };
        route_models(route.kind, tiers, &claude)
    }
}

/// The kinds `person` may pick for every agent: their own logins for the
/// whole hub, as the every-agent PUT checks them. A hub that keeps no
/// settings offers none.
async fn every_agent_options(
    state: &GatewayState,
    routes: &PersonalRoutes,
    switch: &PersonalRoutesConfig,
    person: Uuid,
) -> Vec<RouteOption> {
    let host = state.registry.host();
    let logins = every_agent_logins(state, host.as_deref(), person);
    let hub_on = switch.enabled && routes.store().is_some();
    // As the every-agent PUT checks it: an agent without the worker
    // refuses a Codex turn when it comes.
    logins
        .options(hub_on, switch, true, &hub_tiers(host.as_deref()))
        .await
}

/// The tiers models are named by for every agent: the host agent's.
fn hub_tiers(host: Option<&AgentService>) -> TierRegistry {
    host.map_or_else(|| TierRegistry::new(None), AgentService::tier_registry)
}

/// `person`'s own logins for the whole hub, as an every-agent route reads
/// them.
fn every_agent_logins<'a>(
    state: &'a GatewayState,
    host: Option<&'a AgentService>,
    person: Uuid,
) -> Logins<'a> {
    Logins {
        shared: host.map(AgentService::subscriptions),
        vaults: Vaults {
            agent: None,
            global: state.vault.as_ref(),
        },
        tiers: &[CredentialTier::GlobalUser],
        person,
    }
}

/// `person`'s own logins on `service` or the whole hub, as a per-agent
/// route reads them.
fn agent_logins(service: &AgentService, person: Uuid) -> Logins<'_> {
    Logins {
        shared: Some(service.subscriptions()),
        vaults: service.logins.vaults(),
        tiers: &[CredentialTier::AgentUser, CredentialTier::GlobalUser],
        person,
    }
}

/// The kinds `person` may pick for `service`: their own logins on the agent
/// or the whole hub, as the per-agent PUT checks them. A hub that keeps no
/// settings offers none.
async fn agent_options(
    routes: &PersonalRoutes,
    service: &AgentService,
    switch: &PersonalRoutesConfig,
    person: Uuid,
) -> Vec<RouteOption> {
    let hub_on = routes.switch().enabled && routes.store().is_some();
    agent_logins(service, person)
        .options(
            hub_on,
            switch,
            has_worker(service),
            &service.tier_registry(),
        )
        .await
}

/// What `person`'s own conversations with `service` run on, when that is
/// their own route, as an agent listing shows it to them; `None` is the
/// hub's route. The route is the one a turn of theirs takes
/// ([`crate::personal_route::RouteDecider::chosen_for`]), its model the
/// one a new conversation's chat turn runs. A Codex route on a hub without
/// its coding worker is the hub's route when it falls back to it, and said
/// refused when it waits, as a turn finds it.
pub(crate) async fn shown_route(
    service: &AgentService,
    person: Uuid,
) -> Option<workforce_gateway_client::PersonalRouteInfo> {
    let chosen = crate::personal_route::RouteDecider::of(service)
        .chosen_for(person)
        .await?;
    let refused = crate::agent_turn::personal_refusal(service, chosen.cli);
    if refused.is_some() && chosen.route.fallback == RouteFallback::Hub {
        // Every turn of theirs runs on the hub's route instead.
        return None;
    }
    // The model a new conversation's chat turn starts from: the agent's
    // conversation model, else its default, as `resolve_chat_model` picks.
    let fresh = workforce_common::session::Session::new(
        "desk",
        &person.to_string(),
        service.agent_id.as_str(),
    );
    // Picked as a turn picks it, without the lines a turn's pick logs: a
    // listing asks for every agent each time a client reads it.
    let spec =
        tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
            service.resolve_model(&fresh, Some(crate::serve::CONVERSATION), None)
        });
    let model = crate::personal_route::personal_model(
        &service.tier_registry(),
        &service.tier_key.read().clone(),
        &spec,
        chosen.cli,
        &chosen.route,
        workforce_common::personal_route::RouteWork::Conversation,
    );
    let kind = chosen.route.kind;
    Some(workforce_gateway_client::PersonalRouteInfo {
        kind: kind.as_str().to_string(),
        title: kind.title().to_string(),
        model,
        fallback: chosen.route.fallback.as_str().to_string(),
        connected: has_own_login(
            kind,
            &service.logins.vaults(),
            &[CredentialTier::AgentUser, CredentialTier::GlobalUser],
            person,
        ),
        refused: refused.map(str::to_string),
    })
}

fn refused(reason: String) -> ApiError {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({"error": reason})),
    )
}

/// `GET /api/me/llm-route` — the caller's setting for every agent.
pub async fn get_my_route(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> ApiResult<MyRouteView> {
    let (person, _) = super::identity::require_person(&state, &headers)?;
    let routes = routes(&state)?;
    let switch = routes.switch();
    let route = match routes.store() {
        Some(store) => {
            store
                .settings(person, None)
                .await
                .map_err(store_failed)?
                .every_agent
        }
        None => None,
    };
    Ok(Json(MyRouteView {
        available: switch.enabled && routes.store().is_some(),
        options: every_agent_options(&state, &routes, &switch, person).await,
        allow: switch.allow,
        route,
    }))
}

/// `PUT /api/me/llm-route` — sets or clears the caller's setting for every
/// agent. A route needs the caller's own login of its kind for the whole
/// hub (in My Settings), since it serves every agent. A Codex route is kept
/// whatever each agent's coding worker: an agent without one refuses it
/// (per agent here, and at turn time).
pub async fn put_my_route(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(write): Json<RouteWrite<PersonalRoute>>,
) -> ApiResult<MyRouteView> {
    let (person, _) = super::identity::require_person(&state, &headers)?;
    let routes = routes(&state)?;
    let store = store(&routes)?;
    let switch = routes.switch();
    if let Some(route) = &write.route {
        let host = state.registry.host();
        let logins = every_agent_logins(&state, host.as_deref(), person);
        let login = logins.of(route.kind);
        if let Some(reason) = refusal(route, switch.enabled, &switch, login.is_some(), true) {
            return Err(refused(reason));
        }
        // Read then written without a lock: two saves at once may each
        // keep a model the other dropped, which the next save settles.
        let before = store
            .settings(person, None)
            .await
            .map_err(store_failed)?
            .every_agent;
        let offered = logins
            .offered(
                route,
                login,
                switch.enabled,
                &switch,
                true,
                &hub_tiers(host.as_deref()),
            )
            .await;
        if let Some(reason) = model_refusal(route, &offered, before.as_ref()) {
            return Err(refused(reason));
        }
    }
    let actor = person.to_string();
    let before = store
        .set_every_agent(person, write.route.clone(), &actor)
        .await
        .map_err(store_failed)?;
    routes.forget(person);
    tracing::info!(
        target: "llm_route",
        person = %person,
        before = ?before,
        after = ?write.route,
        "a person's every-agent route changed"
    );
    Ok(Json(MyRouteView {
        available: switch.enabled,
        options: every_agent_options(&state, &routes, &switch, person).await,
        allow: switch.allow,
        route: write.route,
    }))
}

async fn agent_view(
    store: Option<&Arc<dyn PersonRouteStore>>,
    switch: PersonalRoutesConfig,
    options: Vec<RouteOption>,
    person: Uuid,
    agent_id: &str,
) -> ApiResult<MyAgentRouteView> {
    let settings = match store {
        Some(store) => store
            .settings(person, Some(agent_id))
            .await
            .map_err(store_failed)?,
        None => Default::default(),
    };
    let effective = settings
        .chosen()
        .filter(|route| switch.admits(route.kind) && ROUTABLE.contains(&route.kind));
    Ok(Json(MyAgentRouteView {
        agent_id: agent_id.to_string(),
        available: switch.enabled && store.is_some(),
        allow: switch.allow,
        route: settings.this_agent,
        every_agent: settings.every_agent,
        effective,
        options,
    }))
}

/// `GET /api/me/agents/{id}/llm-route` — the caller's setting for one
/// agent, their every-agent one, and the route their work there takes.
pub async fn get_my_agent_route(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
) -> ApiResult<MyAgentRouteView> {
    let (person, claims) = super::identity::require_person(&state, &headers)?;
    let service = agent_for(&state, claims, &agent_id)?;
    let routes = routes(&state)?;
    let switch = switch_for(&routes, &service);
    let options = agent_options(&routes, &service, &switch, person).await;
    agent_view(routes.store().as_ref(), switch, options, person, &agent_id).await
}

/// `PUT /api/me/agents/{id}/llm-route` — sets the caller's setting for one
/// agent (a route, or `"hub"` to keep the agent on the hub's route), or
/// clears it back to their every-agent setting.
pub async fn put_my_agent_route(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
    Json(write): Json<RouteWrite<AgentRoute>>,
) -> ApiResult<MyAgentRouteView> {
    let (person, claims) = super::identity::require_person(&state, &headers)?;
    let service = agent_for(&state, claims, &agent_id)?;
    let routes = routes(&state)?;
    let store = store(&routes)?;
    let switch = switch_for(&routes, &service);
    if let Some(AgentRoute::Personal(route)) = &write.route {
        let logins = agent_logins(&service, person);
        let worker = has_worker(&service);
        let login = logins.of(route.kind);
        if let Some(reason) = refusal(
            route,
            routes.switch().enabled,
            &switch,
            login.is_some(),
            worker,
        ) {
            return Err(refused(reason));
        }
        // Read then written without a lock, as the every-agent save is.
        let before = match store
            .settings(person, Some(&agent_id))
            .await
            .map_err(store_failed)?
            .this_agent
        {
            Some(AgentRoute::Personal(before)) => Some(before),
            _ => None,
        };
        let hub_on = routes.switch().enabled && routes.store().is_some();
        let offered = logins
            .offered(
                route,
                login,
                hub_on,
                &switch,
                worker,
                &service.tier_registry(),
            )
            .await;
        if let Some(reason) = model_refusal(route, &offered, before.as_ref()) {
            return Err(refused(reason));
        }
    }
    let actor = person.to_string();
    let before = store
        .set_for_agent(person, &agent_id, write.route.clone(), &actor)
        .await
        .map_err(store_failed)?;
    routes.forget(person);
    tracing::info!(
        target: "llm_route",
        person = %person,
        agent = %agent_id,
        before = ?before,
        after = ?write.route,
        "a person's route for an agent changed"
    );
    let options = agent_options(&routes, &service, &switch, person).await;
    agent_view(Some(&store), switch, options, person, &agent_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::personal_route::RouteFallback;

    const CLAUDE: PersonalRoute =
        PersonalRoute::new(PersonalRouteKind::ClaudeCode, RouteFallback::Wait);

    fn on() -> PersonalRoutesConfig {
        PersonalRoutesConfig {
            enabled: true,
            ..Default::default()
        }
    }

    #[test]
    fn a_route_is_refused_off_unoffered_unbuilt_or_without_a_login() {
        assert_eq!(refusal(&CLAUDE, true, &on(), true, false), None);
        assert!(
            refusal(&CLAUDE, false, &PersonalRoutesConfig::default(), true, true)
                .unwrap()
                .contains("doesn't run")
        );
        let codex_only = PersonalRoutesConfig {
            enabled: true,
            allow: vec![PersonalRouteKind::Codex],
        };
        assert!(refusal(&CLAUDE, true, &codex_only, true, true)
            .unwrap()
            .contains("isn't offered"));
        let devin = PersonalRoute::new(PersonalRouteKind::Devin, RouteFallback::Hub);
        assert!(refusal(&devin, true, &on(), true, true)
            .unwrap()
            .contains("isn't available yet"));
        assert!(refusal(&CLAUDE, true, &on(), false, true)
            .unwrap()
            .contains("connect your own Claude login"));
    }

    #[test]
    fn a_codex_route_needs_the_hubs_coding_worker_and_her_own_login() {
        let codex = PersonalRoute::new(PersonalRouteKind::Codex, RouteFallback::Wait);
        assert_eq!(refusal(&codex, true, &on(), true, true), None);
        assert_eq!(
            refusal(&codex, true, &on(), true, false).as_deref(),
            Some(crate::personal_route::CODEX_NEEDS_WORKER),
            "refused with a plain reason off the worker"
        );
        assert!(refusal(&codex, true, &on(), false, true)
            .unwrap()
            .contains("connect your own ChatGPT (Codex) login"));
    }

    #[test]
    fn every_kind_is_listed_with_why_it_cant_be_picked() {
        let codex_and_claude = PersonalRoutesConfig {
            enabled: true,
            allow: vec![PersonalRouteKind::ClaudeCode, PersonalRouteKind::Codex],
        };
        let listed = options(
            true,
            &codex_and_claude,
            |kind| kind == PersonalRouteKind::ClaudeCode,
            false,
            &TierRegistry::new(None),
            &ClaudeList::Unasked,
        );
        let kinds: Vec<_> = listed.iter().map(|option| option.kind).collect();
        assert_eq!(
            kinds,
            PersonalRouteKind::ALL.to_vec(),
            "every kind, in order"
        );
        let refused = |kind| {
            listed
                .iter()
                .find(|option| option.kind == kind)
                .unwrap()
                .refused
                .clone()
        };
        assert_eq!(refused(PersonalRouteKind::ClaudeCode), None);
        assert!(refused(PersonalRouteKind::Devin)
            .unwrap()
            .contains("isn't offered"));
        assert!(refused(PersonalRouteKind::Codex).is_some());

        let off = options(
            false,
            &codex_and_claude,
            |_| true,
            true,
            &TierRegistry::new(None),
            &ClaudeList::Unasked,
        );
        assert!(off.iter().all(|option| option
            .refused
            .as_deref()
            .is_some_and(|r| r.contains("doesn't run"))));
    }

    #[test]
    fn a_null_route_clears_the_setting_and_an_empty_body_is_refused() {
        assert!(
            serde_json::from_str::<RouteWrite<AgentRoute>>("{}").is_err(),
            "an empty body isn't read as a clear"
        );
        let null: RouteWrite<AgentRoute> = serde_json::from_str(r#"{"route":null}"#).unwrap();
        assert_eq!(null.route, None);
        let hub: RouteWrite<AgentRoute> = serde_json::from_str(r#"{"route":"hub"}"#).unwrap();
        assert_eq!(hub.route, Some(AgentRoute::Hub));
        assert!(serde_json::from_str::<RouteWrite<PersonalRoute>>(r#"{"route":"hub"}"#).is_err());
        assert!(serde_json::from_str::<RouteWrite<AgentRoute>>(r#"{"rout":"hub"}"#).is_err());
    }

    #[test]
    fn a_routes_models_are_checked_against_what_its_login_serves() {
        let tiers = TierRegistry::new(None);
        let hubs = |kind| route_models(kind, &tiers, &ClaudeList::Unasked);
        let mut codex = PersonalRoute::new(PersonalRouteKind::Codex, RouteFallback::Wait);
        codex
            .model_preferences
            .insert("conversation".into(), "gpt-6.1-sol".into());
        codex.light_model = Some("cheap".into());
        codex.heavy_model = Some("chat".into());
        let on_codex = hubs(PersonalRouteKind::Codex);
        assert_eq!(
            model_refusal(&codex, &on_codex, None),
            None,
            "a model, a tier and chat"
        );

        let before = codex.clone();
        codex.heavy_model = Some("gpt-5.5".into());
        let retired = model_refusal(&codex, &on_codex, Some(&before)).unwrap();
        assert!(
            retired.contains("heavy")
                && retired.contains("retired")
                && retired.contains("gpt-6.1-sol"),
            "said in plain words, with what to use: {retired}"
        );

        let on_hub = hubs(PersonalRouteKind::ClaudeCode);
        let mut claude = CLAUDE.clone();
        claude.light_model = Some("gpt-6.1-sol".into());
        assert!(model_refusal(&claude, &on_hub, None)
            .unwrap()
            .contains("isn't a Claude model this hub runs"));
        let first = on_hub
            .choices
            .first()
            .expect("the hub's tiers name Claude Code models")
            .id
            .clone();
        claude.light_model = Some(first);
        assert_eq!(model_refusal(&claude, &on_hub, None), None);

        claude
            .model_preferences
            .insert("coding".into(), "opus".into());
        assert!(model_refusal(&claude, &on_hub, None)
            .unwrap()
            .contains("only its conversation model"));
    }

    fn plan(id: &str, resolves_to: Option<&str>, name: &str) -> PlanModel {
        PlanModel {
            id: id.into(),
            resolves_to: resolves_to.map(str::to_string),
            name: name.into(),
            description: String::new(),
            efforts: Vec::new(),
            default_effort: None,
            context_window: None,
            fast: None,
        }
    }

    /// What a Claude Max login's CLI lists, aliases first.
    fn listed() -> ClaudeList {
        ClaudeList::Read(Arc::new(vec![
            plan("fable", Some("claude-fable-5-2"), "Fable 5.2"),
            plan("opus", Some("claude-opus-5-5"), "Opus 5.5"),
            plan("sonnet", Some("claude-sonnet-5-5"), "Sonnet 5.5"),
            plan("claude-sonnet-5-5", None, "Sonnet 5.5"),
            plan("claude-fable-5-2", None, "Fable 5.2"),
            plan("claude-fable-5-1", None, "Fable 5.1"),
            plan("claude-haiku-4-5-20251001", None, "Haiku 4.5"),
            plan("claude-opus-4-8", None, "Opus 4.8"),
        ]))
    }

    #[test]
    fn a_claude_route_offers_its_logins_models_by_name_in_the_logins_order() {
        let offered = route_models(
            PersonalRouteKind::ClaudeCode,
            &TierRegistry::new(None),
            &listed(),
        );
        let shown: Vec<(&str, &str, Vec<&str>)> = offered
            .choices
            .iter()
            .map(|c| {
                (
                    c.id.as_str(),
                    c.name.as_str(),
                    c.same_as.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                ("fable", "Fable 5.2", vec!["claude-fable-5-2"]),
                ("opus", "Opus 5.5", vec!["claude-opus-5-5"]),
                ("sonnet", "Sonnet 5.5", vec!["claude-sonnet-5-5"]),
                ("claude-fable-5-1", "Fable 5.1", vec![]),
                ("claude-haiku-4-5-20251001", "Haiku 4.5", vec![]),
                ("claude-opus-4-8", "Opus 4.8", vec![]),
            ],
            "an alias stands for the model it runs, each once, naming the other id"
        );
        assert_eq!(offered.from, ModelsFrom::Login);
        assert_eq!(offered.note, None);
        for id in ["claude-fable-5-2", "claude-opus-5-5", "claude-sonnet-5-5"] {
            assert!(
                offered.accepted.iter().any(|a| a == id),
                "{id} runs, though shown by its alias"
            );
        }
    }

    #[test]
    fn models_fold_only_when_one_runs_the_other_never_by_name() {
        let offered = login_models(&[
            plan("claude-opus-5-5", None, "Opus 5.5"),
            plan("opus", Some("claude-opus-5-5"), "Opus 5.5"),
            plan("claude-opus-5-5[1m]", None, "Opus 5.5"),
        ]);
        let shown: Vec<(&str, Vec<&str>)> = offered
            .choices
            .iter()
            .map(|c| {
                (
                    c.id.as_str(),
                    c.same_as.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                ("claude-opus-5-5", vec!["opus"]),
                ("claude-opus-5-5[1m]", vec![]),
            ],
            "a later alias folds into the full id it runs; a same-named model stays"
        );
    }

    #[test]
    fn a_listed_versioned_model_is_kept_and_an_unlisted_one_refused() {
        let offered = route_models(
            PersonalRouteKind::ClaudeCode,
            &TierRegistry::new(None),
            &listed(),
        );
        let mut claude = CLAUDE.clone();
        claude
            .model_preferences
            .insert("conversation".into(), "claude-fable-5-2".into());
        claude.light_model = Some("claude-haiku-4-5-20251001".into());
        claude.heavy_model = Some("powerful".into());
        assert_eq!(model_refusal(&claude, &offered, None), None);

        claude.heavy_model = Some("claude-opus-3".into());
        let refused = model_refusal(&claude, &offered, None).unwrap();
        assert!(
            refused.contains("heavy")
                && refused.contains("claude-opus-3")
                && refused.contains("your Claude login runs")
                && refused.contains("claude-fable-5-1"),
            "{refused}"
        );
    }

    #[test]
    fn a_model_gone_from_the_list_stays_while_another_work_changes() {
        let mut before = CLAUDE.clone();
        before
            .model_preferences
            .insert("conversation".into(), "claude-fable-5-0".into());
        let offered = route_models(
            PersonalRouteKind::ClaudeCode,
            &TierRegistry::new(None),
            &listed(),
        );
        let mut after = before.clone();
        after.light_model = Some("claude-fable-5-1".into());
        assert_eq!(
            model_refusal(&after, &offered, Some(&before)),
            None,
            "her conversation model is kept as it was"
        );
        after.heavy_model = Some("claude-fable-5-0".into());
        assert!(
            model_refusal(&after, &offered, Some(&before))
                .unwrap()
                .contains("heavy"),
            "but isn't newly chosen for other work"
        );
        let on_codex = PersonalRoute {
            kind: PersonalRouteKind::Codex,
            ..before.clone()
        };
        assert!(
            model_refusal(&after, &offered, Some(&on_codex)).is_some(),
            "another kind's setting keeps nothing"
        );
    }

    #[test]
    fn an_unread_list_offers_the_hubs_claude_models_and_says_why() {
        let tiers = TierRegistry::new(None);
        let unread = route_models(PersonalRouteKind::ClaudeCode, &tiers, &ClaudeList::Unread);
        assert_eq!(unread.from, ModelsFrom::Hub);
        assert_eq!(unread.note.as_deref(), Some(UNREAD_NOTE));
        assert!(
            UNREAD_NOTE.starts_with("Your") && UNREAD_NOTE.ends_with('.'),
            "a finished sentence, shown as it is"
        );
        assert_eq!(
            unread.choices.iter().map(|c| &c.id).collect::<Vec<_>>(),
            crate::subscription::tier_models(ProviderKind::ClaudeCode, &tiers)
                .iter()
                .collect::<Vec<_>>(),
            "today's tier aliases"
        );
        let unasked = route_models(PersonalRouteKind::ClaudeCode, &tiers, &ClaudeList::Unasked);
        assert_eq!(unasked.note, None, "nothing to say when nothing was asked");

        let listed = options(true, &on(), |_| true, true, &tiers, &ClaudeList::Unread);
        let claude = &listed[0];
        assert_eq!(claude.kind, PersonalRouteKind::ClaudeCode);
        assert_eq!(claude.models_note.as_deref(), Some(UNREAD_NOTE));
        assert_eq!(
            claude.models,
            claude
                .choices
                .iter()
                .map(|c| c.id.clone())
                .collect::<Vec<_>>(),
            "the ids, as a page from before choices reads them"
        );
    }

    #[test]
    fn chatgpt_models_are_named_as_desk_names_them() {
        assert_eq!(chatgpt_name("gpt-6.1-sol"), "GPT-6.1 Sol");
        assert_eq!(chatgpt_name("gpt-5.6-terra"), "GPT-5.6 Terra");
        assert_eq!(chatgpt_name("gpt-7"), "GPT-7");
        assert_eq!(chatgpt_name("o9-mini"), "o9-mini");
        assert_eq!(chatgpt_name("gpt-"), "gpt-");
        let codex = route_models(
            PersonalRouteKind::Codex,
            &TierRegistry::new(None),
            &ClaudeList::Unread,
        );
        assert_eq!(codex.choices[0].name, "GPT-6.1 Sol");
        assert_eq!(codex.from, ModelsFrom::Hub);
        assert_eq!(codex.note, None, "a Claude list says nothing of Codex");
    }

    /// The hub's subscriptions, for listing through.
    fn shared(dir: &std::path::Path) -> SharedSubscriptions {
        let mut config = workforce_common::settings::WorkforceConfig::default();
        config.hub.data_dir = dir.to_string_lossy().into_owned();
        crate::subscription::Subscriptions::boot(&config, None)
            .shared()
            .clone()
    }

    #[tokio::test]
    async fn a_logins_list_is_read_once_kept_and_one_unreadable_falls_back() {
        use crate::claude_code::tests::CLAUDE;
        use crate::subscription::catalog::tests::lister;
        let dir = tempfile::tempdir().unwrap();
        let shared = shared(dir.path());
        let reply = serde_json::json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "wf-list-models",
                "response": { "models": [
                    { "value": "fable", "resolvedModel": "claude-fable-5-2", "displayName": "Fable 5.2" },
                    { "value": "claude-fable-5-1", "displayName": "Fable 5.1" }
                ] }
            }
        })
        .to_string();
        let lists = tempfile::tempdir().unwrap();
        let listing = lister(lists.path(), &reply);
        let login =
            crate::subscription::test_support::claude_login(&format!("her-{}", Uuid::new_v4()));

        let read = CLAUDE
            .scope(
                listing.clone(),
                claude_list(&shared, Some(login.clone()), true, LIST_WAIT),
            )
            .await;
        let ClaudeList::Read(models) = read else {
            panic!("listed: {read:?}")
        };
        assert_eq!(models[0].id, "fable");
        assert_eq!(models[0].resolves_to.as_deref(), Some("claude-fable-5-2"));

        // Kept for her login: a CLI that can't list isn't asked again.
        let broken = tempfile::tempdir().unwrap();
        let refusing = lister(broken.path(), r#"{"type":"assistant"}"#);
        let again = CLAUDE
            .scope(
                refusing.clone(),
                claude_list(&shared, Some(login.clone()), true, LIST_WAIT),
            )
            .await;
        assert!(matches!(again, ClaudeList::Read(ref m) if m[1].id == "claude-fable-5-1"));
        assert!(
            !broken.path().join("args").exists(),
            "the kept list is answered without a CLI"
        );

        // Another login, whose CLI can't say: the hub's models, said why.
        let other =
            crate::subscription::test_support::claude_login(&format!("his-{}", Uuid::new_v4()));
        let missing = CLAUDE
            .scope(
                std::path::PathBuf::from("/nonexistent/claude"),
                claude_list(&shared, Some(other.clone()), true, LIST_WAIT),
            )
            .await;
        assert!(matches!(missing, ClaudeList::Unread), "{missing:?}");

        // Not asked where the route can't be picked: no CLI starts.
        let unasked = tempfile::tempdir().unwrap();
        let never = lister(unasked.path(), &reply);
        let off = CLAUDE
            .scope(never, claude_list(&shared, Some(login), false, LIST_WAIT))
            .await;
        assert!(matches!(off, ClaudeList::Unasked));
        assert!(!unasked.path().join("args").exists(), "no CLI started");
        assert!(matches!(
            claude_list(&shared, None, true, LIST_WAIT).await,
            ClaudeList::Unasked
        ));
    }

    #[test]
    fn her_login_at_its_limit_is_still_hers_and_a_refused_one_is_not() {
        use crate::hub_core::logins::cooldown::{LimitSignal, LimitStatus};
        let dir = tempfile::tempdir().unwrap();
        let vault =
            Arc::new(crate::vault::Vault::open(&dir.path().join("v.enc"), "test-key").unwrap());
        let person = Uuid::new_v4();
        vault
            .set(
                &crate::user_config::per_user_key(
                    &person.to_string(),
                    workforce_common::subscription::SubscriptionKind::Claude.vault_key(),
                ),
                &format!("token-{}", Uuid::new_v4()),
            )
            .unwrap();
        let vaults = Vaults {
            agent: None,
            global: Some(&vault),
        };
        let tiers = [CredentialTier::GlobalUser];
        let login = own_login(PersonalRouteKind::ClaudeCode, &vaults, &tiers, person)
            .expect("her own login");
        CooldownStore::global().note(
            &login.key,
            &LimitSignal {
                status: LimitStatus::Rejected,
                resets_at: Some(Utc::now() + chrono::TimeDelta::hours(2)),
                window: Some("five_hour".into()),
            },
            Utc::now(),
        );
        let cooling = own_login(PersonalRouteKind::ClaudeCode, &vaults, &tiers, person)
            .expect("still hers while it cools, so its models are listed");
        assert_eq!(cooling.key, login.key);
        assert!(own_login(PersonalRouteKind::ClaudeCode, &vaults, &[], person).is_none());
        assert!(own_login(PersonalRouteKind::Devin, &vaults, &tiers, person).is_none());
    }

    #[tokio::test]
    async fn without_a_host_agent_the_hubs_models_are_offered_and_nothing_is_listed() {
        let person = Uuid::new_v4();
        let logins = Logins {
            shared: None,
            vaults: Vaults {
                agent: None,
                global: None,
            },
            tiers: &[CredentialTier::GlobalUser],
            person,
        };
        let listed = logins
            .options(true, &on(), true, &TierRegistry::new(None))
            .await;
        let claude = &listed[0];
        assert_eq!(claude.models_from, ModelsFrom::Hub);
        assert_eq!(claude.models_note, None);
        assert!(claude.refused.is_some(), "no login of hers here");
    }

    #[tokio::test]
    async fn a_codex_or_devin_save_never_lists_claude_models() {
        use crate::claude_code::tests::CLAUDE;
        use crate::subscription::catalog::tests::lister;
        let dir = tempfile::tempdir().unwrap();
        let shared = shared(dir.path());
        let watched = tempfile::tempdir().unwrap();
        let program = lister(watched.path(), r#"{"type":"assistant"}"#);
        let logins = Logins {
            shared: Some(&shared),
            vaults: Vaults {
                agent: None,
                global: None,
            },
            tiers: &[CredentialTier::GlobalUser],
            person: Uuid::new_v4(),
        };
        let login =
            crate::subscription::test_support::claude_login(&format!("her-{}", Uuid::new_v4()));
        for kind in [PersonalRouteKind::Codex, PersonalRouteKind::Devin] {
            let route = PersonalRoute::new(kind, RouteFallback::Wait);
            let offered = CLAUDE
                .scope(
                    program.clone(),
                    logins.offered(
                        &route,
                        Some(login.clone()),
                        true,
                        &on(),
                        true,
                        &TierRegistry::new(None),
                    ),
                )
                .await;
            assert_eq!(offered.from, ModelsFrom::Hub);
        }
        assert!(
            !watched.path().join("args").exists(),
            "no Claude CLI started"
        );
    }

    #[tokio::test]
    async fn a_slow_list_answers_with_the_hubs_and_fills_in_for_the_next_ask() {
        use crate::claude_code::tests::CLAUDE;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let shared = shared(dir.path());
        let reply = serde_json::json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "wf-list-models",
                "response": { "models": [ { "value": "claude-fable-5-1", "displayName": "Fable 5.1" } ] }
            }
        })
        .to_string();
        let program = dir.path().join("claude");
        std::fs::write(
            &program,
            format!("#!/bin/sh\nread line\nsleep 1\nprintf '%s\\n' '{reply}'\nsleep 30\n"),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let login =
            crate::subscription::test_support::claude_login(&format!("slow-{}", Uuid::new_v4()));

        let first = CLAUDE
            .scope(
                program.clone(),
                claude_list(
                    &shared,
                    Some(login.clone()),
                    true,
                    std::time::Duration::from_millis(100),
                ),
            )
            .await;
        assert!(matches!(first, ClaudeList::Unread), "{first:?}");

        let started = std::time::Instant::now();
        loop {
            let next = claude_list(
                &shared,
                Some(login.clone()),
                true,
                std::time::Duration::from_millis(100),
            )
            .await;
            if let ClaudeList::Read(models) = next {
                assert_eq!(models[0].id, "claude-fable-5-1", "the read went on");
                break;
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(8),
                "the list never filled in"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
}
