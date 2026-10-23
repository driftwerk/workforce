//! A person's subscription logins from the web. `/api/me/subscriptions`
//! lists their own; `/api/me/subscriptions/{kind}/login` stores or removes a
//! pasted login in one tier, through the guarded store; and
//! `/api/me/subscriptions/chatgpt/device…` signs them in to ChatGPT with a
//! device code: start (or rejoin), read where it stands, cancel. A flow
//! belongs to the person who started it; anyone else is told it does not
//! exist. These routes act for a person signed in themselves, never for a
//! paired device. `/api/agents/{id}/logins` shows which logins an agent runs
//! on and which one serves the caller. Tokens are never sent back.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use workforce_common::settings::{AgentWorkspaceConfig, ExecutorKind, LoginPolicy, VaultActor};
use workforce_common::subscription::{
    split_per_user_key, AgentOwner, CredentialTier, Payer, SubscriptionKind,
};

use crate::api_handlers::identity_display::{resolve_agent_display, resolve_principal, AgentRef};
use crate::api_handlers::my_settings::{
    enforce_action_rate_limit, list_agent_per_user_entries, MAX_SECRET_SIZE,
};
use crate::authz::Capability;
use crate::gateway::{AgentViewer, CallerContext, GatewayState, WsQuery};
use crate::hub_core::logins::chatgpt_auth::{Account, AuthJson, API_KEY_FILE};
use crate::hub_core::logins::cooldown::CooldownStore;
use crate::hub_core::logins::device_login::{
    unsupported_reason, DeviceFlow, DeviceLoginError, DeviceLogins, FlowState, LoginTarget,
};
use crate::hub_core::logins::resolve::{
    login_state, resolve, slot_for, Login, Miss, VaultSlot, Vaults,
};
use crate::hub_core::logins::store::LoginRefusal;
use crate::hub_core::subscriptions::{route_models, routed_subscriptions};
use crate::serve::AgentService;
use crate::user_config::USER_PREFIX;
use crate::vault::Vault;

type ApiError = (StatusCode, Json<serde_json::Value>);
type ApiResult<T> = Result<Json<T>, ApiError>;

fn err(status: StatusCode, msg: &str) -> ApiError {
    (status, Json(serde_json::json!({"error": msg})))
}

/// Where a login stands, as its tier holds it for the person asking. Cooling
/// and refused marks are this server's own.
#[derive(Debug, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LoginStatus {
    Connected { account: Option<Account> },
    Cooling { until: DateTime<Utc> },
    Invalid { reason: String },
    NotSet,
}

impl From<Result<Login, Miss>> for LoginStatus {
    fn from(state: Result<Login, Miss>) -> Self {
        match state {
            Ok(login) => Self::Connected {
                account: login.auth().map(AuthJson::account),
            },
            Err(Miss::Cooling { until }) => Self::Cooling { until },
            Err(miss) if miss != Miss::Absent && miss.needs_sign_in() => Self::Invalid {
                reason: miss.to_string(),
            },
            Err(_) => Self::NotSet,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct LoginSlotView {
    kind: SubscriptionKind,
    tier: CredentialTier,
    agent_id: Option<String>,
    agent_name: Option<String>,
    status: LoginStatus,
    updated_at: Option<DateTime<Utc>>,
    updated_by: Option<String>,
}

impl LoginSlotView {
    /// The slot as someone who may not change it sees it: where it stands,
    /// not whose account it is or who set it.
    fn withheld(self) -> Self {
        Self {
            status: match self.status {
                LoginStatus::Connected { .. } => LoginStatus::Connected { account: None },
                status => status,
            },
            updated_by: None,
            ..self
        }
    }
}

#[derive(Debug, Serialize)]
pub struct MySubscriptions {
    chatgpt_direct: bool,
    logins: Vec<LoginSlotView>,
}

#[derive(Debug, Serialize)]
pub struct LoginPolicyView {
    #[serde(flatten)]
    policy: LoginPolicy,
    overridden: bool,
}

#[derive(Debug, Serialize)]
pub struct InstallView {
    chatgpt_direct: bool,
    remote_worker: bool,
}

#[derive(Debug, Serialize)]
pub struct LoginMiss {
    tier: CredentialTier,
    reason: String,
}

/// Which login a call of `kind` from the caller would run on now, or why
/// none would.
#[derive(Debug, Serialize)]
pub struct ServingView {
    kind: SubscriptionKind,
    tier: Option<CredentialTier>,
    misses: Vec<LoginMiss>,
}

/// Someone with a login of their own on an agent, by name only.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct LoginUserView {
    kind: SubscriptionKind,
    tier: CredentialTier,
    display_name: String,
}

#[derive(Debug, Serialize)]
pub struct AgentLoginsView {
    policy: LoginPolicyView,
    install: InstallView,
    kinds: Vec<SubscriptionKind>,
    serving_you: Vec<ServingView>,
    shared: Vec<LoginSlotView>,
    can_manage_shared: bool,
    can_manage_install: bool,
    users: Option<Vec<LoginUserView>>,
    route_options: Vec<String>,
}

/// A login route's tier, and the agent for an agent tier.
#[derive(Debug, Deserialize)]
pub struct TargetQuery {
    tier: CredentialTier,
    #[serde(default)]
    agent_id: Option<String>,
}

/// The sign-in to read: the caller's one kept flow, or with a tier, theirs
/// for that target only.
#[derive(Debug, Deserialize)]
pub struct CurrentFlowQuery {
    #[serde(default)]
    tier: Option<CredentialTier>,
    #[serde(default)]
    agent_id: Option<String>,
}

#[derive(Deserialize)]
pub struct LoginWrite {
    #[serde(flatten)]
    target: TargetQuery,
    value: String,
}

impl std::fmt::Debug for LoginWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginWrite")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

/// Where a flow signs in to: the tier, the agent for an agent tier, and how
/// that agent reads.
#[derive(Debug, Serialize)]
pub struct FlowTargetView {
    #[serde(flatten)]
    target: LoginTarget,
    agent_name: Option<String>,
}

/// A flow as its owner sees it: while pending, the code and where to enter
/// it; once settled, how it ended, and never its code.
#[derive(Debug, Serialize)]
pub struct DeviceFlowView {
    flow_id: Uuid,
    target: FlowTargetView,
    #[serde(flatten)]
    state: FlowStateView,
}

#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FlowStateView {
    Pending {
        user_code: String,
        verification_url: String,
        expires_at: DateTime<Utc>,
        interval_secs: u64,
    },
    Completed {
        account: Account,
    },
    Expired {
        reason: String,
    },
    Failed {
        reason: String,
    },
    Cancelled {
        reason: String,
    },
    Unsupported {
        reason: String,
    },
}

impl DeviceFlowView {
    fn new(flow: DeviceFlow, agent_name: impl FnOnce(&str) -> String) -> Self {
        let state = match flow.state {
            FlowState::Pending => FlowStateView::Pending {
                user_code: flow.user_code,
                verification_url: flow.verification_url,
                expires_at: flow.expires_at,
                interval_secs: flow.interval_secs,
            },
            FlowState::Completed { account } => FlowStateView::Completed { account },
            FlowState::Expired => FlowStateView::Expired {
                reason: "the code expired before it was used".into(),
            },
            FlowState::Failed { reason } => FlowStateView::Failed { reason },
            FlowState::Cancelled => FlowStateView::Cancelled {
                reason: "the sign-in was cancelled".into(),
            },
            FlowState::Replaced => FlowStateView::Cancelled {
                reason: "a newer sign-in took its place".into(),
            },
            FlowState::Unsupported => FlowStateView::Unsupported {
                reason: unsupported_reason(),
            },
        };
        Self {
            flow_id: flow.id,
            target: FlowTargetView {
                agent_name: flow.target.agent_id().map(agent_name),
                target: flow.target,
            },
            state,
        }
    }

    fn of(state: &GatewayState, flow: DeviceFlow) -> Json<Self> {
        Json(Self::new(flow, |agent_id| {
            resolve_agent_display(&state.registry, agent_id).display_name
        }))
    }
}

/// The person calling a login route, as their own sign-in names them.
struct Caller {
    person: Uuid,
    context: CallerContext,
}

impl Caller {
    fn id(&self) -> String {
        self.person.to_string()
    }

    fn shares_vault(&self, state: &GatewayState) -> bool {
        crate::gateway::caller_holds(state, &self.context, Capability::VaultSharedWrite)
    }
}

fn caller(state: &GatewayState, headers: &HeaderMap) -> Result<Caller, ApiError> {
    let (person, claims) = super::identity::require_person(state, headers)?;
    Ok(Caller {
        person,
        context: claims.into(),
    })
}

/// A tier a login route acts on, checked for the caller: the agent it is
/// on, for an agent tier, which must keep logins.
struct Place {
    tier: CredentialTier,
    agent: Option<(String, Arc<AgentService>)>,
}

fn place(state: &GatewayState, caller: &Caller, query: &TargetQuery) -> Result<Place, ApiError> {
    let agent = match query.tier {
        CredentialTier::GlobalUser | CredentialTier::SystemShared => None,
        CredentialTier::AgentUser | CredentialTier::AgentShared => {
            Some(agent_keeping_logins(state, caller, query)?)
        }
    };
    let refusal = match (query.tier, &agent) {
        (CredentialTier::AgentShared, Some((_, service)))
            if !service
                .logins
                .may_share(Some(&caller.id()), caller.shares_vault(state)) =>
        {
            Some(LoginRefusal::SharedOnly)
        }
        (CredentialTier::SystemShared, _) if !caller.shares_vault(state) => {
            Some(LoginRefusal::InstallOnly)
        }
        _ => None,
    };
    if let Some(refusal) = refusal {
        crate::permissions_audit::emit_capability_deny(
            Some(&caller.id()),
            &format!("subscription_login.{}", query.tier.as_str()),
            Capability::VaultSharedWrite.key(),
        );
        return Err(crate::gateway::login_refusal(refusal));
    }
    Ok(Place {
        tier: query.tier,
        agent,
    })
}

/// The agent `query` names, when the caller may use it and it keeps logins.
fn agent_keeping_logins(
    state: &GatewayState,
    caller: &Caller,
    query: &TargetQuery,
) -> Result<(String, Arc<AgentService>), ApiError> {
    let agent_id = query.agent_id.as_deref().ok_or_else(|| {
        err(
            StatusCode::BAD_REQUEST,
            &format!("agent_id is required for {}", query.tier.as_str()),
        )
    })?;
    let service = state
        .registry
        .get(agent_id)
        .filter(|_| AgentViewer::new(state, caller.context.clone()).can_see(agent_id))
        .ok_or_else(|| crate::gateway::not_found_agent(agent_id))?;
    if service.vault.is_none() {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "this agent keeps no logins (no vault)",
        ));
    }
    Ok((agent_id.to_string(), service))
}

impl Place {
    /// Let the agents this place's logins serve offer, or withdraw, what a
    /// change here unlocks.
    fn reload(&self, state: &GatewayState) {
        crate::hub_core::logins::device_login::reload_coding_sessions(
            state.registry.as_ref(),
            self.agent.as_ref().map(|(id, _)| id.as_str()),
        );
    }

    /// Where the tier keeps `kind`'s login for `caller`.
    fn slot(
        &self,
        state: &GatewayState,
        kind: SubscriptionKind,
        caller: &Caller,
    ) -> Result<VaultSlot, ApiError> {
        let vaults = match &self.agent {
            Some((_, service)) => service.logins.vaults(),
            None => Vaults {
                agent: None,
                global: state.vault.as_ref(),
            },
        };
        slot_for(
            self.tier,
            kind,
            &vaults,
            Some(caller.person),
            &AgentOwner::Unowned,
        )
        .map(|(slot, _)| slot)
        .map_err(|_| err(StatusCode::SERVICE_UNAVAILABLE, "Vault not configured"))
    }

    /// The device sign-in that fills this tier.
    fn target(self) -> Result<LoginTarget, ApiError> {
        LoginTarget::new(self.tier, self.agent.map(|(agent_id, _)| agent_id)).ok_or_else(|| {
            err(
                StatusCode::BAD_REQUEST,
                "the install's own login is pasted, not signed in to from here",
            )
        })
    }
}

fn actor_name(state: &GatewayState) -> impl Fn(&str) -> String {
    let display = crate::vault_view::actor_display(state.identity_store.clone());
    move |actor| display(actor).unwrap_or_else(|| actor.to_string())
}

/// Reads logins for one person, as of one moment.
struct SlotReader<'a> {
    person: Option<Uuid>,
    owner: &'a AgentOwner,
    actor_name: &'a dyn Fn(&str) -> String,
    now: DateTime<Utc>,
}

impl SlotReader<'_> {
    /// `tier`'s login of `kind` in `vaults`, on `agent` for an agent tier.
    fn view(
        &self,
        vaults: &Vaults<'_>,
        kind: SubscriptionKind,
        tier: CredentialTier,
        agent: Option<&AgentRef>,
    ) -> LoginSlotView {
        let entry = slot_for(tier, kind, vaults, self.person, self.owner)
            .ok()
            .and_then(|(slot, _)| slot.vault.get_entry(&slot.key));
        let status = login_state(
            kind,
            tier,
            vaults,
            self.person,
            self.owner,
            CooldownStore::global(),
            self.now,
        );
        LoginSlotView {
            kind,
            tier,
            agent_id: agent.map(|agent| agent.id.clone()),
            agent_name: agent.map(|agent| agent.display_name.clone()),
            status: status.into(),
            updated_at: entry.as_ref().map(|entry| entry.updated_at),
            updated_by: entry.map(|entry| (self.actor_name)(&entry.updated_by)),
        }
    }
}

fn chatgpt_direct(state: &GatewayState) -> bool {
    state
        .registry
        .default_agent()
        .is_some_and(|service| service.config.subscription.chatgpt_direct)
}

/// `GET /api/me/subscriptions` — the caller's global login of each kind, and
/// the logins of their own they keep on agents they may use.
pub async fn my_subscriptions(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> ApiResult<MySubscriptions> {
    let caller = caller(&state, &headers)?;
    let actor_name = actor_name(&state);
    let reader = SlotReader {
        person: Some(caller.person),
        owner: &AgentOwner::Unowned,
        actor_name: &actor_name,
        now: Utc::now(),
    };
    let global = Vaults {
        agent: None,
        global: state.vault.as_ref(),
    };
    let own_global = SubscriptionKind::ALL
        .into_iter()
        .map(|kind| reader.view(&global, kind, CredentialTier::GlobalUser, None));
    let viewer = AgentViewer::new(&state, caller.context.clone());
    let own_on_agents = list_agent_per_user_entries(&state, &caller.id(), &|_| None)
        .into_iter()
        .filter_map(|entry| {
            let kind = SubscriptionKind::of_vault_key(&entry.path)?;
            let agent_id = entry.agent_id.filter(|id| viewer.can_see(id))?;
            let service = state.registry.get(&agent_id)?;
            let agent = resolve_agent_display(&state.registry, &agent_id);
            Some(reader.view(
                &service.logins.vaults(),
                kind,
                CredentialTier::AgentUser,
                Some(&agent),
            ))
        })
        .filter(|view| view.status != LoginStatus::NotSet);
    Ok(Json(MySubscriptions {
        chatgpt_direct: chatgpt_direct(&state),
        logins: own_global.chain(own_on_agents).collect(),
    }))
}

/// Everyone with a login of their own of one of `kinds` in `vault`, at `tier`.
fn holders<'a>(
    vault: Option<&Arc<Vault>>,
    tier: CredentialTier,
    kinds: &'a [SubscriptionKind],
    name: &'a dyn Fn(&str) -> String,
) -> impl Iterator<Item = LoginUserView> + 'a {
    vault
        .map(|vault| vault.list(Some(USER_PREFIX)))
        .unwrap_or_default()
        .into_iter()
        .filter_map(move |key| {
            let kind = SubscriptionKind::of_vault_key(&key).filter(|kind| kinds.contains(kind))?;
            let (person, _) = split_per_user_key(&key)?;
            Some(LoginUserView {
                kind,
                tier,
                display_name: name(person),
            })
        })
}

/// `GET /api/agents/{id}/logins` — the logins an agent's routes run on, as
/// its `config.toml` sets them: its policy, the shared logins, and which
/// login serves the caller.
pub async fn agent_logins(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: HeaderMap,
) -> ApiResult<AgentLoginsView> {
    let (service, context) =
        crate::gateway::authorise_agent_reader(&state, &headers, query.token.as_deref(), &id)
            .await?;
    let person = context.identity_id.as_deref();
    let can_manage_install =
        crate::gateway::caller_holds(&state, &context, Capability::VaultSharedWrite);
    let can_manage_shared = service.logins.may_share(person, can_manage_install);
    let unloadable = |e: eyre::Report| {
        let shown = if can_manage_shared {
            format!("this agent's config.toml does not load: {e}")
        } else {
            "this agent's config.toml does not load; its owner can fix it".to_string()
        };
        err(StatusCode::CONFLICT, &shown)
    };
    let workspace = AgentWorkspaceConfig::load(&service.workspace).map_err(unloadable)?;
    let overridden = workspace
        .as_ref()
        .and_then(|ws| ws.llm.as_ref())
        .is_some_and(|llm| llm.logins.is_some());
    let prospective = service
        .admission
        .prospective(workspace.as_ref())
        .map_err(unloadable)?;
    let policy = prospective.next.logins.clone();
    let kinds = routed_subscriptions(&prospective.effective, &prospective.next);

    let vaults = service.logins.vaults();
    let owner = service.logins.owner();
    let payer = Payer::classify(person);
    let now = Utc::now();
    let agent = resolve_agent_display(&state.registry, &id);
    let actor_name = actor_name(&state);

    let serving_you = kinds
        .iter()
        .map(|&kind| {
            match resolve(
                kind,
                &vaults,
                &payer,
                &owner,
                &policy,
                CooldownStore::global(),
                now,
            ) {
                Ok(login) => ServingView {
                    kind,
                    tier: Some(login.tier),
                    misses: Vec::new(),
                },
                Err(no_login) => ServingView {
                    kind,
                    tier: None,
                    misses: no_login
                        .misses
                        .into_iter()
                        .map(|(tier, miss)| LoginMiss {
                            tier,
                            reason: miss.to_string(),
                        })
                        .collect(),
                },
            }
        })
        .collect();
    let shared_reader = SlotReader {
        person: None,
        owner: &owner,
        actor_name: &actor_name,
        now,
    };
    let install_tier = policy
        .tiers
        .contains(&CredentialTier::SystemShared)
        .then_some(CredentialTier::SystemShared);
    let shared = kinds
        .iter()
        .flat_map(|&kind| {
            std::iter::once(CredentialTier::AgentShared)
                .chain(install_tier)
                .map(move |tier| (kind, tier))
        })
        .map(|(kind, tier)| {
            let view = shared_reader.view(
                &vaults,
                kind,
                tier,
                (tier == CredentialTier::AgentShared).then_some(&agent),
            );
            let manages = match tier {
                CredentialTier::AgentShared => can_manage_shared,
                _ => can_manage_install,
            };
            if manages {
                view
            } else {
                view.withheld()
            }
        })
        .collect();
    let users = can_manage_shared.then(|| {
        let name =
            |person: &str| resolve_principal(state.identity_store.as_ref(), person).display_name;
        [
            (CredentialTier::AgentUser, vaults.agent),
            (CredentialTier::GlobalUser, vaults.global),
        ]
        .into_iter()
        .filter(|(tier, _)| policy.tiers.contains(tier))
        .flat_map(|(tier, vault)| holders(vault, tier, &kinds, &name))
        .collect()
    });

    Ok(Json(AgentLoginsView {
        policy: LoginPolicyView { policy, overridden },
        install: InstallView {
            chatgpt_direct: service.config.subscription.chatgpt_direct,
            remote_worker: service.config.coding_session.executor.kind == ExecutorKind::Remote,
        },
        serving_you,
        shared,
        can_manage_shared,
        can_manage_install,
        users,
        route_options: route_models(&prospective.next.tiers),
        kinds,
    }))
}

/// `POST /api/me/subscriptions/{kind}/login` with `{tier, agent_id?, value}`
/// — store a pasted login in one tier: the caller's own, or one everyone
/// shares when they may change it.
pub async fn save_login(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(kind): Path<SubscriptionKind>,
    Json(body): Json<LoginWrite>,
) -> ApiResult<serde_json::Value> {
    let caller = caller(&state, &headers)?;
    enforce_action_rate_limit(&state, &caller.id())?;
    if body.value.len() > MAX_SECRET_SIZE {
        return Err(err(StatusCode::PAYLOAD_TOO_LARGE, "Secret value too large"));
    }
    let api_key_file = kind == SubscriptionKind::ChatGpt
        && AuthJson::parse(body.value.trim()).is_ok_and(|auth| auth.tokens.is_none());
    if api_key_file {
        return Err(err(StatusCode::BAD_REQUEST, API_KEY_FILE));
    }
    let place = place(&state, &caller, &body.target)?;
    let slot = place.slot(&state, kind, &caller)?;
    let person = caller.id();
    crate::hub_core::logins::store::set(
        &slot.vault,
        &slot.key,
        &body.value,
        || crate::gateway::login_vaults(&state),
        VaultActor::User(&person),
    )
    .await
    .map_err(crate::gateway::login_refusal)?;
    place.reload(&state);
    tracing::info!(identity_id = %person, kind = kind.label(), tier = place.tier.as_str(), "Subscription login stored");
    Ok(Json(serde_json::json!({ "stored": place.tier })))
}

/// `DELETE /api/me/subscriptions/{kind}/login?tier=&agent_id=` — remove the
/// login one tier holds, with the same gates as storing it.
pub async fn remove_login(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(kind): Path<SubscriptionKind>,
    Query(query): Query<TargetQuery>,
) -> ApiResult<serde_json::Value> {
    let caller = caller(&state, &headers)?;
    enforce_action_rate_limit(&state, &caller.id())?;
    let place = place(&state, &caller, &query)?;
    let slot = place.slot(&state, kind, &caller)?;
    let person = caller.id();
    let removed =
        crate::hub_core::logins::store::delete(&slot.vault, &slot.key, VaultActor::User(&person))
            .await
            .map_err(|e| crate::gateway::vault_write_rejection(&e))?;
    place.reload(&state);
    tracing::info!(identity_id = %person, kind = kind.label(), tier = place.tier.as_str(), removed, "Subscription login removed");
    Ok(Json(serde_json::json!({ "removed": removed })))
}

fn device_logins(state: &GatewayState) -> Result<DeviceLogins, ApiError> {
    state
        .registry
        .default_agent()
        .map(|service| service.subscriptions().device_logins().clone())
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "no agents on this hub"))
}

fn refusal(error: DeviceLoginError) -> ApiError {
    match error {
        DeviceLoginError::NoVault => err(StatusCode::SERVICE_UNAVAILABLE, &error.to_string()),
        DeviceLoginError::Start(_) => err(StatusCode::BAD_GATEWAY, &error.to_string()),
        DeviceLoginError::Vault(e) => crate::gateway::vault_write_rejection(&e),
    }
}

/// `POST /api/me/subscriptions/chatgpt/device` with `{tier, agent_id?}`.
pub async fn start_device_login(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<TargetQuery>,
) -> ApiResult<DeviceFlowView> {
    let caller = caller(&state, &headers)?;
    enforce_action_rate_limit(&state, &caller.id())?;
    let target = place(&state, &caller, &body)?.target()?;
    device_logins(&state)?
        .start_or_join(caller.person, target, None)
        .await
        .map(|flow| DeviceFlowView::of(&state, flow))
        .map_err(refusal)
}

/// `GET /api/me/subscriptions/chatgpt/device[?tier=&agent_id=]` — the
/// caller's one kept sign-in, with what it signs in to, or theirs for that
/// target only.
pub async fn current_device_login(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Query(query): Query<CurrentFlowQuery>,
) -> ApiResult<DeviceFlowView> {
    let caller = caller(&state, &headers)?;
    let logins = device_logins(&state)?;
    let flow = match query.tier {
        Some(tier) => {
            let target = TargetQuery {
                tier,
                agent_id: query.agent_id,
            };
            let target = place(&state, &caller, &target)?.target()?;
            logins.current(caller.person, &target)
        }
        None => logins.latest(caller.person),
    };
    flow.map(|flow| DeviceFlowView::of(&state, flow))
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no ChatGPT sign-in for this login"))
}

/// `GET /api/me/subscriptions/chatgpt/device/{flow_id}`.
pub async fn get_device_login(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(flow): Path<Uuid>,
) -> ApiResult<DeviceFlowView> {
    let caller = caller(&state, &headers)?;
    device_logins(&state)?
        .get(caller.person, flow)
        .map(|flow| DeviceFlowView::of(&state, flow))
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no such sign-in"))
}

/// `DELETE /api/me/subscriptions/chatgpt/device/{flow_id}`.
pub async fn cancel_device_login(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(flow): Path<Uuid>,
) -> ApiResult<serde_json::Value> {
    let caller = caller(&state, &headers)?;
    enforce_action_rate_limit(&state, &caller.id())?;
    match device_logins(&state)?.cancel(caller.person, flow).await {
        Ok(true) => Ok(Json(serde_json::json!({ "cancelled": flow }))),
        Ok(false) => Err(err(
            StatusCode::NOT_FOUND,
            "no pending sign-in with that id",
        )),
        Err(e) => Err(refusal(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::hub_core::logins::device_login::DeviceAuthClient;
    use crate::vault::Vault;

    #[test]
    fn a_login_write_never_prints_its_value() {
        let write: LoginWrite = serde_json::from_value(serde_json::json!({
            "tier": "global_user",
            "value": "sk-ant-oat01-secret",
        }))
        .unwrap();
        let printed = format!("{write:?}");
        assert!(!printed.contains("sk-ant-oat01-secret"), "{printed}");
        assert!(printed.contains("GlobalUser"), "{printed}");
    }

    #[tokio::test]
    async fn device_flow_view_is_per_state_and_settled_flows_carry_no_code() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(&dir.path().join("global.enc"), "test-key").unwrap());
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/accounts/deviceauth/usercode"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "device_auth_id": "dev-1",
                "user_code": "ABCD-1234",
                "interval": "5",
            })))
            .mount(&server)
            .await;
        let logins = DeviceLogins::new(
            Some(vault),
            DeviceAuthClient::at(reqwest::Client::new(), &server.uri()),
        );
        let flow = logins
            .start_or_join(
                Uuid::new_v4(),
                LoginTarget::AgentUser {
                    agent_id: "allan".into(),
                },
                Some(serde_json::json!({"session_id": "s-1"})),
            )
            .await
            .unwrap();
        let view = |flow: DeviceFlow| {
            serde_json::to_value(DeviceFlowView::new(flow, |_| "Allan".to_string())).unwrap()
        };

        assert_eq!(
            view(flow.clone()),
            serde_json::json!({
                "state": "pending",
                "flow_id": flow.id,
                "target": {"tier": "agent_user", "agent_id": "allan", "agent_name": "Allan"},
                "user_code": "ABCD-1234",
                "verification_url": format!("{}/codex/device", server.uri()),
                "expires_at": flow.expires_at,
                "interval_secs": 5,
            })
        );

        for (state, shown) in [
            (
                FlowState::Completed {
                    account: Account {
                        email: Some("alice@example.com".into()),
                        plan: Some("plus".into()),
                    },
                },
                "completed",
            ),
            (FlowState::Expired, "expired"),
            (
                FlowState::Failed {
                    reason: "refused".into(),
                },
                "failed",
            ),
            (FlowState::Cancelled, "cancelled"),
            (FlowState::Replaced, "cancelled"),
            (FlowState::Unsupported, "unsupported"),
        ] {
            let mut settled = flow.clone();
            settled.state = state;
            let view = view(settled);
            assert_eq!(view["state"], shown);
            assert_eq!(view["flow_id"], flow.id.to_string());
            assert_eq!(view["target"]["agent_id"], "allan");
            assert!(!view.to_string().contains("ABCD-1234"), "{view}");
            assert!(view.get("verification_url").is_none(), "{view}");
            match shown {
                "completed" => assert_eq!(view["account"]["email"], "alice@example.com"),
                _ => assert!(view["reason"].is_string(), "{view}"),
            }
        }
    }

    fn member() -> Caller {
        let person = Uuid::new_v4();
        Caller {
            person,
            context: CallerContext {
                identity_id: Some(person.to_string()),
                ..CallerContext::default()
            },
        }
    }

    fn place_for(
        state: &GatewayState,
        caller: &Caller,
        tier: CredentialTier,
        agent_id: Option<&str>,
    ) -> Result<Place, StatusCode> {
        place(
            state,
            caller,
            &TargetQuery {
                tier,
                agent_id: agent_id.map(str::to_string),
            },
        )
        .map_err(|(status, _)| status)
    }

    #[test]
    fn agent_user_target_needs_an_agent_that_keeps_logins() {
        let state = crate::remote_mcp::testing::fixture(false).state;
        let caller = member();
        let target = |tier, agent_id| {
            place_for(&state, &caller, tier, agent_id)
                .and_then(|place| place.target().map_err(|(status, _)| status))
        };
        assert_eq!(
            target(CredentialTier::GlobalUser, None),
            Ok(LoginTarget::GlobalUser)
        );
        assert_eq!(
            target(CredentialTier::AgentUser, None),
            Err(StatusCode::BAD_REQUEST)
        );
        assert_eq!(
            target(CredentialTier::AgentUser, Some("nobody")),
            Err(StatusCode::NOT_FOUND)
        );
        assert_eq!(
            target(CredentialTier::SystemShared, None),
            Err(StatusCode::FORBIDDEN)
        );
    }

    #[test]
    fn refused_system_shared_write_is_audited() {
        use workforce_memory::access_audit;

        let buffer = access_audit::install_ring_buffer(256);
        let _ = access_audit::set_global_sink(buffer.clone());
        let state = crate::remote_mcp::testing::fixture(false).state;
        let caller = member();
        let denials = || {
            buffer
                .snapshot()
                .into_iter()
                .filter(|event| event.requester_identity_id.as_deref() == Some(&caller.id()))
                .map(|event| (event.resource_id, event.owner_identity_id))
                .collect::<Vec<_>>()
        };

        assert!(place_for(&state, &caller, CredentialTier::GlobalUser, None).is_ok());
        assert!(denials().is_empty());
        assert_eq!(
            place_for(&state, &caller, CredentialTier::SystemShared, None).err(),
            Some(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            denials(),
            [(
                Some("subscription_login.system_shared".to_string()),
                Some(Capability::VaultSharedWrite.key().to_string())
            )]
        );
    }
}
