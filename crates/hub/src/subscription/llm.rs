//! The subscription providers of a scope ([`PlanLogins`]), and the `chatgpt`
//! one: a ChatGPT login driving the ChatGPT backend's Responses API. The
//! login is resolved on every call — the payer's own, the agent's shared one,
//! or the hub's for its own work — so one provider serves every user of an
//! agent, each on their own plan.

pub use crate::hub_core::logins::chatgpt_backend::ChatGptBackend;
pub use crate::hub_core::logins::errors::SubscriptionError;
use crate::hub_core::logins::errors::THROTTLE_COOLDOWN;
use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{TimeDelta, Utc};
use eyre::Result;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use uuid::Uuid;
use workforce_agent::llm::provider::{LlmProvider, ProviderKind};
use workforce_agent::llm::responses::{
    Call, ChatGptAccess, ChatGptModel, LimitSnapshot, ResponsesError, UnsupportedModel,
};
use workforce_agent::llm::streaming::{collect_stream, StreamEvent};
use workforce_agent::llm::types::{
    has_foreign_reasoning, strip_foreign_reasoning, LlmMessage, LlmResponse, LlmToolDef,
};
use workforce_agent::runner::{note_served, CURRENT_SESSION_ID};
use workforce_common::settings::{LoginPolicy, SubscriptionConfig};
use workforce_common::subscription::{
    AgentOwner, CredentialTier, Payer, Served, SharedServes, SubscriptionKind, SubscriptionRoute,
};

use super::chatgpt_auth::{AuthJson, RefreshError};
use super::completion::CliCompletion;
use super::cooldown::{CooldownStore, LimitSignal, LimitStatus};
use super::limits::{chatgpt_limit, chatgpt_usage_limit};
use super::logins::AgentLogins;
use super::resolve::{resolve, Login, NoLogin, Vaults};
use super::CliKind;
use super::Subscriptions;
use crate::vault::Vault;

/// A login is refreshed when its access token expires within this window.
const REFRESH_HORIZON: TimeDelta = TimeDelta::minutes(5);
/// The longest throttle waited out inside the provider before the login
/// cools and the chain moves on.
const MAX_THROTTLE_WAIT: Duration = Duration::from_secs(20);
const DEFAULT_MODEL: ChatGptModel = ChatGptModel::Gpt61Sol;
const RELAY_CAPACITY: usize = 128;
/// Who the hub's own calls are made for, in cache keys and limit reports.
const SYSTEM_CALLER: &str = "system";

/// Where a provider's calls find the login that pays for them.
#[derive(Clone)]
pub enum CredentialScope {
    /// The payer's own login on the agent, else the agent's shared one.
    Agent(Arc<AgentLogins>),
    /// The hub's own non-agent work, in the global vault: the paying
    /// person's own login, else the hub's at the bare key.
    System {
        vault: Arc<Vault>,
        /// Who pays when no person is in scope (`[subscription] operator`).
        operator: Option<Uuid>,
    },
}

impl CredentialScope {
    pub fn system(vault: Arc<Vault>, config: &SubscriptionConfig) -> Self {
        Self::System {
            vault,
            operator: config.operator,
        }
    }

    /// The login that pays for a `kind` call made now.
    pub(crate) fn resolve(&self, kind: SubscriptionKind) -> std::result::Result<Login, NoLogin> {
        match self {
            Self::Agent(logins) => logins.resolve(kind),
            Self::System { vault, operator } => {
                let payer = match crate::user_config::current_payer() {
                    person @ Payer::Person(_) => person,
                    _ => operator.map_or_else(
                        || Payer::Background(SYSTEM_CALLER.to_string()),
                        Payer::Person,
                    ),
                };
                resolve(
                    kind,
                    &Vaults {
                        agent: None,
                        global: Some(vault),
                    },
                    &payer,
                    &AgentOwner::Unowned,
                    &LoginPolicy {
                        tiers: vec![CredentialTier::GlobalUser, CredentialTier::SystemShared],
                        shared_serves: SharedServes::Anyone,
                    },
                    CooldownStore::global(),
                    Utc::now(),
                )
            }
        }
    }

    /// The cap a provider chain puts on one `model` call of this scope: an
    /// agent's, as its `[llm.retry]` sets it; none for the hub's own calls,
    /// which run on no chain.
    pub(crate) fn request_cap(&self, model: &str) -> Option<Duration> {
        match self {
            Self::Agent(logins) => logins.request_cap(model),
            Self::System { .. } => None,
        }
    }

    pub(crate) fn caller(&self) -> &str {
        match self {
            Self::Agent(logins) => logins.agent_id(),
            Self::System { .. } => SYSTEM_CALLER,
        }
    }
}

/// A scope's subscription logins: what every subscription provider of one
/// agent (or of the hub's own work) is built from.
#[derive(Clone)]
pub struct PlanLogins {
    subscriptions: Subscriptions,
    scope: CredentialScope,
}

impl PlanLogins {
    pub fn new(subscriptions: Subscriptions, scope: CredentialScope) -> Self {
        Self {
            subscriptions,
            scope,
        }
    }

    pub fn subscriptions(&self) -> &Subscriptions {
        &self.subscriptions
    }

    pub(crate) fn scope(&self) -> &CredentialScope {
        &self.scope
    }

    /// The login that pays for a `kind` call made now, or why none would.
    pub fn resolve(&self, kind: SubscriptionKind) -> std::result::Result<Login, NoLogin> {
        self.scope.resolve(kind)
    }

    /// Whether anyone has a `kind` login stored where this scope reads: an
    /// agent's at a tier its `[llm.logins]` admits; the hub's own at the
    /// global vault's bare key or under any person.
    pub fn stored_for_anyone(&self, kind: SubscriptionKind) -> bool {
        match &self.scope {
            CredentialScope::Agent(logins) => logins.stored_for_anyone(kind),
            CredentialScope::System { vault, .. } => {
                let key = kind.vault_key();
                let own = format!("/{key}");
                vault.get(key).is_some()
                    || vault
                        .list(Some(crate::user_config::USER_PREFIX))
                        .iter()
                        .any(|path| path.ends_with(&own))
            }
        }
    }

    /// A `kind` provider on these logins, serving `model` unless a call
    /// names another, with the configured thinking budget. A metered kind is
    /// refused.
    pub fn provider(
        &self,
        kind: ProviderKind,
        model: Option<&str>,
        thinking_budget: Option<u32>,
    ) -> Result<Arc<dyn LlmProvider>> {
        match (kind, CliKind::of(kind)) {
            (_, Some(cli)) => Ok(Arc::new(CliCompletion::new(
                self.clone(),
                cli,
                model,
                thinking_budget,
            ))),
            (ProviderKind::ChatGpt, None) => Ok(Arc::new(self.chatgpt(model, thinking_budget))),
            (metered, None) => Err(eyre::eyre!(
                "{} does not run on a subscription login",
                metered.canonical_name()
            )),
        }
    }

    /// The `chatgpt` provider on these logins.
    pub(crate) fn chatgpt(
        &self,
        model: Option<&str>,
        thinking_budget: Option<u32>,
    ) -> SubscriptionProvider {
        SubscriptionProvider {
            plans: self.clone(),
            model: model.map(str::to_string),
            thinking_budget,
        }
    }
}

/// The retries one call may spend, each at most once.
#[derive(Default)]
struct Retried {
    renewed: bool,
    waited: bool,
    stripped: bool,
}

/// An [`LlmProvider`] on the ChatGPT backend that resolves its login per
/// call. Built by [`PlanLogins::provider`].
pub struct SubscriptionProvider {
    plans: PlanLogins,
    model: Option<String>,
    thinking_budget: Option<u32>,
}

impl SubscriptionProvider {
    /// The model a call runs on. A model this backend does not serve moves
    /// the chain on without counting against the model.
    fn model(&self, requested: Option<&str>) -> Result<ChatGptModel, SubscriptionError> {
        requested
            .or(self.model.as_deref())
            .map_or(Ok(DEFAULT_MODEL), ChatGptModel::parse)
            .map_err(|refused| {
                let (UnsupportedModel::Retired(model) | UnsupportedModel::Unknown(model)) =
                    &refused;
                SubscriptionError::ModelNotInPlan {
                    model: model.clone(),
                    login: None,
                    detail: refused.to_string(),
                }
            })
    }

    /// The session's id, so the backend's prompt cache follows the
    /// conversation; a call outside any session keys on its instructions.
    fn cache_key(&self, system: Option<&str>) -> String {
        CURRENT_SESSION_ID
            .try_with(String::clone)
            .ok()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| {
                let digest = hex::encode(Sha256::digest(system.unwrap_or_default().as_bytes()));
                format!("{}:{}", self.plans.scope.caller(), &digest[..16])
            })
    }

    fn describe(&self, login: &Login) -> String {
        login.describe(self.plans.scope.caller())
    }

    fn refresh_failed(
        &self,
        login: &Login,
        auth: &AuthJson,
        error: RefreshError,
    ) -> SubscriptionError {
        match error {
            RefreshError::Transient(reason) => SubscriptionError::RefreshFailed {
                login: self.describe(login),
                reason,
            },
            refused => self.invalid(login, auth, refused.to_string()),
        }
    }

    /// `login`, holding `auth`, was refused for good: skip it until it is
    /// signed in again.
    fn invalid(&self, login: &Login, auth: &AuthJson, reason: String) -> SubscriptionError {
        login
            .refreshed(auth.clone())
            .refuse(reason, self.plans.scope.caller())
    }

    /// Cool `login` for `signal`'s refusal and name when it frees up.
    fn cool(&self, login: &Login, signal: &LimitSignal) -> SubscriptionError {
        login.cool(signal, self.plans.scope.caller())
    }

    /// Keep the plan state a response reports: how full its windows are, a
    /// window at its limit cools the login, one close to it is logged.
    fn observe(&self, login: &Login, snapshot: &LimitSnapshot) {
        login.observe_usage(
            snapshot.plan.as_deref(),
            crate::subscription::limits::chatgpt_usage(snapshot),
        );
        let Some(signal) = chatgpt_limit(snapshot) else {
            return;
        };
        if let LimitStatus::Warning { utilization } = signal.status {
            tracing::warn!(
                login = %self.describe(login),
                window = signal.window.as_deref().unwrap_or("unknown"),
                utilization,
                resets_at = ?signal.resets_at,
                "ChatGPT plan close to its usage limit"
            );
        }
        CooldownStore::global().note(&login.key, &signal, Utc::now());
    }

    /// One request, relayed to `tx`; also reports whether any of it reached
    /// `tx`, after which the call can no longer be retried.
    async fn attempt(
        &self,
        auth: &AuthJson,
        call: Call<'_>,
        tx: &mpsc::Sender<StreamEvent>,
        limits: &(dyn Fn(LimitSnapshot) + Send + Sync),
    ) -> (std::result::Result<(), ResponsesError>, bool) {
        let (Some(access_token), Some(account_id)) = (auth.access_token(), auth.account_id())
        else {
            return (
                Err(ResponsesError::Unauthorized(
                    "the login carries no ChatGPT access token or account".to_string(),
                )),
                false,
            );
        };
        let access = ChatGptAccess {
            access_token,
            account_id: &account_id,
        };
        let (relay_tx, mut relay_rx) = mpsc::channel(RELAY_CAPACITY);
        let relay = async {
            let mut forwarded = false;
            while let Some(event) = relay_rx.recv().await {
                forwarded = true;
                if tx.send(event).await.is_err() {
                    break;
                }
            }
            forwarded
        };
        tokio::join!(
            self.plans
                .subscriptions
                .shared()
                .chatgpt()
                .client()
                .stream(&access, call, relay_tx, limits),
            relay
        )
    }

    async fn serve(
        &self,
        system: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: ChatGptModel,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<Served> {
        let backend = self.plans.subscriptions.shared().chatgpt();
        if !backend.enabled() {
            return Err(SubscriptionError::Disabled.into());
        }
        let login = self
            .plans
            .scope
            .resolve(SubscriptionKind::ChatGpt)
            .map_err(SubscriptionError::NoLogin)?;
        let refresher = backend.refresher();
        let seen = login
            .auth()
            .ok_or_else(|| SubscriptionError::LoginInvalid {
                login: self.describe(&login),
                reason: "not a ChatGPT sign-in".into(),
            })?;
        let mut auth = refresher
            .fresh(&login.slot, seen, REFRESH_HORIZON)
            .await
            .map_err(|e| self.refresh_failed(&login, seen, e))?;
        let cache_key = self.cache_key(system);
        let last_snapshot = parking_lot::Mutex::new(None);
        let limits = |snapshot: LimitSnapshot| {
            self.observe(&login, &snapshot);
            *last_snapshot.lock() = Some(snapshot);
        };
        let mut messages = Cow::Borrowed(messages);
        let mut retried = Retried::default();
        loop {
            let call = Call {
                system,
                messages: &messages,
                tools,
                model,
                cache_key: &cache_key,
                thinking_budget: self.thinking_budget,
            };
            let (result, forwarded) = self.attempt(&auth, call, &tx, &limits).await;
            let Err(error) = result else {
                return Ok(login.served(SubscriptionRoute::ChatGpt, self.plans.scope.caller()));
            };
            let fresh = !forwarded;
            match error {
                ResponsesError::Unauthorized(_) if fresh && !retried.renewed => {
                    retried.renewed = true;
                    auth = refresher
                        .renew(&login.slot, &auth)
                        .await
                        .map_err(|e| self.refresh_failed(&login, &auth, e))?;
                }
                ResponsesError::Unauthorized(reason) => {
                    return Err(self.invalid(&login, &auth, reason).into());
                }
                ResponsesError::Throttled {
                    retry_after: Some(wait),
                } if fresh && !retried.waited && wait <= MAX_THROTTLE_WAIT => {
                    retried.waited = true;
                    let _ = tx
                        .send(StreamEvent::RateLimitWait {
                            wait_secs: wait.as_secs_f64().ceil() as u64,
                            attempt: 1,
                        })
                        .await;
                    tokio::time::sleep(wait).await;
                }
                ResponsesError::Throttled { retry_after } => {
                    let wait = retry_after
                        .and_then(|wait| TimeDelta::from_std(wait).ok())
                        .unwrap_or(THROTTLE_COOLDOWN);
                    let signal = LimitSignal {
                        status: LimitStatus::Rejected,
                        resets_at: Some(Utc::now() + wait),
                        window: None,
                    };
                    return Err(self.cool(&login, &signal).into());
                }
                ResponsesError::UsageLimit { resets_at, .. } => {
                    let signal = chatgpt_usage_limit(resets_at, last_snapshot.lock().as_ref());
                    return Err(self.cool(&login, &signal).into());
                }
                ResponsesError::InvalidReasoning(_)
                    if fresh && !retried.stripped && has_foreign_reasoning(&messages) =>
                {
                    retried.stripped = true;
                    messages = Cow::Owned(strip_foreign_reasoning(&messages).into_owned());
                }
                ResponsesError::ModelNotInPlan(detail) => {
                    return Err(SubscriptionError::ModelNotInPlan {
                        model: model.id().to_string(),
                        login: Some(self.describe(&login)),
                        detail,
                    }
                    .into());
                }
                ResponsesError::Forbidden(detail) => {
                    return Err(SubscriptionError::Forbidden {
                        login: self.describe(&login),
                        detail,
                    }
                    .into());
                }
                other => return Err(other.into()),
            }
        }
    }
}

#[async_trait]
impl LlmProvider for SubscriptionProvider {
    async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        _max_tokens: Option<u32>,
        _enable_cache: bool,
    ) -> Result<LlmResponse> {
        let model = self.model(model)?;
        let (tx, rx) = mpsc::channel(RELAY_CAPACITY);
        let (served, collected) = tokio::join!(
            self.serve(system_prompt, messages, tools, model, tx),
            collect_stream(rx)
        );
        let served = served?;
        let (content, stop_reason, usage) = collected?;
        note_served(served);
        Ok(LlmResponse {
            content,
            stop_reason,
            usage,
            model: model.id().to_string(),
        })
    }

    async fn create_message_stream(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        _max_tokens: Option<u32>,
        _enable_cache: bool,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        let model = self.model(model)?;
        note_served(
            self.serve(system_prompt, messages, tools, model, tx)
                .await?,
        );
        Ok(())
    }

    async fn context_floor(&self, model: &str) -> Option<u32> {
        Some(
            ChatGptModel::parse(model)
                .unwrap_or(DEFAULT_MODEL)
                .context_window(),
        )
    }

    /// Always passes: a login is resolved per call, for a payer that only
    /// exists once a call is made.
    async fn verify_credentials(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subscription::test_support::{access, login, recorded, Metered, SKY, TEXT};
    use chrono::DateTime;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};
    use workforce_agent::llm::failover::{
        classify_error, ErrorKind, FailoverConfig, FailoverProvider,
    };
    use workforce_agent::llm::types::{ContentBlock, Role, OAI_REASONING_PREFIX};

    use crate::subscription::resolve::LoginKey;
    use workforce_common::settings::WorkforceConfig;

    const LOGIN_KEY: &str = SubscriptionKind::ChatGpt.vault_key();
    const UNAUTHORIZED: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../agent-runtime/src/llm/testdata/chatgpt/unauthorized.sse"
    ));
    const TAMPERED: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../agent-runtime/src/llm/testdata/chatgpt/reasoning_round2_tampered.sse"
    ));

    struct Harness {
        server: MockServer,
        vault: Arc<Vault>,
        provider: Arc<SubscriptionProvider>,
        account: String,
        _dir: tempfile::TempDir,
    }

    impl Harness {
        /// The hub's own login for a fresh account at the global vault's
        /// bare key, served by a mock backend.
        async fn new(enabled: bool) -> Self {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let vault = Arc::new(Vault::open(&dir.path().join("global.enc"), "test-key").unwrap());
            let account = format!("acct-{}", uuid::Uuid::new_v4());
            vault
                .set(LOGIN_KEY, &login(&account, "access-0", "rt-0"))
                .unwrap();
            let backend = ChatGptBackend::at(
                &server.uri(),
                format!("{}/oauth/token", server.uri()),
                enabled,
            );
            let provider = Arc::new(
                PlanLogins::new(
                    Subscriptions::new(&WorkforceConfig::default()).with_chatgpt(backend),
                    CredentialScope::system(Arc::clone(&vault), &Default::default()),
                )
                .chatgpt(Some("gpt-6.1-sol"), None),
            );
            Self {
                server,
                vault,
                provider,
                account,
                _dir: dir,
            }
        }

        fn key(&self) -> LoginKey {
            LoginKey {
                kind: SubscriptionKind::ChatGpt,
                fingerprint: format!("{}:user-1", self.account),
            }
        }

        fn access_token(&self) -> String {
            AuthJson::parse(&self.vault.get(LOGIN_KEY).unwrap())
                .unwrap()
                .access_token()
                .unwrap()
                .to_string()
        }

        async fn requests(&self, route: &str) -> Vec<Request> {
            self.server
                .received_requests()
                .await
                .unwrap()
                .into_iter()
                .filter(|r| r.url.path() == route)
                .collect()
        }

        async fn ask(&self, messages: &[LlmMessage]) -> Result<LlmResponse> {
            self.provider
                .create_message(Some("You are terse."), messages, &[], None, None, false)
                .await
        }
    }

    fn bearer(token: &str) -> String {
        format!("Bearer {token}")
    }

    fn subscription_error(err: &eyre::Report) -> &SubscriptionError {
        err.downcast_ref::<SubscriptionError>()
            .unwrap_or_else(|| panic!("not a subscription error: {err}"))
    }

    /// Replaces the stored login before answering 401, as another process
    /// that refreshed the login first would.
    struct PeerRefreshed {
        vault: Arc<Vault>,
        replacement: String,
    }

    impl Respond for PeerRefreshed {
        fn respond(&self, _: &Request) -> ResponseTemplate {
            self.vault.set(LOGIN_KEY, &self.replacement).unwrap();
            recorded(UNAUTHORIZED)
        }
    }

    #[tokio::test]
    async fn unauthorized_reloads_then_refreshes_then_retries_once() {
        let hub = Harness::new(true).await;
        let stale = hub.access_token();
        let refreshed = access("access-1");
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("authorization", bearer(&stale).as_str()))
            .respond_with(recorded(UNAUTHORIZED))
            .mount(&hub.server)
            .await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("authorization", bearer(&refreshed).as_str()))
            .respond_with(recorded(TEXT))
            .mount(&hub.server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"access_token": refreshed, "refresh_token": "rt-1"}),
            ))
            .expect(1)
            .mount(&hub.server)
            .await;

        let response = hub
            .ask(&[LlmMessage::user("why is the sky blue?")])
            .await
            .unwrap();
        assert_eq!(response.text().as_deref(), Some(SKY));
        assert_eq!(hub.requests("/responses").await.len(), 2);
        assert_eq!(hub.access_token(), refreshed, "the refresh is written back");

        let peer = Harness::new(true).await;
        let stale = peer.access_token();
        let replacement = login(&peer.account, "access-peer", "rt-peer");
        let reloaded = AuthJson::parse(&replacement)
            .unwrap()
            .access_token()
            .unwrap()
            .to_string();
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("authorization", bearer(&stale).as_str()))
            .respond_with(PeerRefreshed {
                vault: Arc::clone(&peer.vault),
                replacement,
            })
            .mount(&peer.server)
            .await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("authorization", bearer(&reloaded).as_str()))
            .respond_with(recorded(TEXT))
            .mount(&peer.server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&peer.server)
            .await;

        peer.ask(&[LlmMessage::user("why is the sky blue?")])
            .await
            .expect("the login another process stored is used without a refresh");
    }

    #[tokio::test]
    async fn permanent_refresh_failure_is_login_invalid() {
        let hub = Harness::new(true).await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(recorded(UNAUTHORIZED))
            .mount(&hub.server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({"error": {"code": "refresh_token_reused"}})),
            )
            .mount(&hub.server)
            .await;

        let err = hub.ask(&[LlmMessage::user("hi")]).await.unwrap_err();
        assert!(
            matches!(
                subscription_error(&err),
                SubscriptionError::LoginInvalid { .. }
            ),
            "{err}"
        );
        assert_eq!(
            classify_error(&err.to_string()),
            ErrorKind::SubscriptionUnavailable
        );
        assert_eq!(hub.requests("/responses").await.len(), 1, "never retried");

        let err = hub.ask(&[LlmMessage::user("hi")]).await.unwrap_err();
        let invalid_install_login = (
            CredentialTier::SystemShared,
            crate::subscription::Miss::Invalid,
        );
        assert!(
            matches!(subscription_error(&err), SubscriptionError::NoLogin(no)
                if no.misses.last() == Some(&invalid_install_login)),
            "{err}"
        );
        assert_eq!(hub.requests("/responses").await.len(), 1);
        assert_eq!(
            hub.requests("/oauth/token").await.len(),
            1,
            "a dead login is not refreshed again"
        );

        hub.vault
            .set(LOGIN_KEY, &login(&hub.account, "access-new", "rt-new"))
            .unwrap();
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header(
                "authorization",
                bearer(&hub.access_token()).as_str(),
            ))
            .respond_with(recorded(TEXT))
            .with_priority(1)
            .mount(&hub.server)
            .await;
        hub.ask(&[LlmMessage::user("hi")])
            .await
            .expect("a new sign-in in the slot is served again");
    }

    #[tokio::test]
    async fn cooling_login_short_circuits_without_request() {
        let hub = Harness::new(true).await;
        let now = Utc::now();
        CooldownStore::global().note(
            &hub.key(),
            &LimitSignal {
                status: LimitStatus::Rejected,
                resets_at: Some(now + TimeDelta::hours(2)),
                window: Some("primary".into()),
            },
            now,
        );

        let err = hub.ask(&[LlmMessage::user("hi")]).await.unwrap_err();
        assert!(
            matches!(subscription_error(&err), SubscriptionError::NoLogin(no) if no.cooling_until().is_some()),
            "{err}"
        );
        assert!(
            err.to_string().contains("at its usage limit until"),
            "{err}"
        );
        assert_eq!(
            classify_error(&err.to_string()),
            ErrorKind::SubscriptionUnavailable
        );
        assert!(hub.server.received_requests().await.unwrap().is_empty());
    }

    fn throttled(retry_after_secs: u64) -> ResponseTemplate {
        ResponseTemplate::new(429)
            .insert_header("retry-after", retry_after_secs.to_string().as_str())
            .set_body_json(serde_json::json!({"error": {"type": "rate_limit_exceeded", "message": "slow down"}}))
    }

    #[tokio::test]
    async fn short_throttle_retried_inside_provider_long_throttle_cools() {
        let hub = Harness::new(true).await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(throttled(1))
            .up_to_n_times(1)
            .mount(&hub.server)
            .await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(recorded(TEXT))
            .mount(&hub.server)
            .await;
        let (tx, mut rx) = mpsc::channel(256);
        hub.provider
            .create_message_stream(None, &[LlmMessage::user("hi")], &[], None, None, false, tx)
            .await
            .unwrap();
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        assert!(matches!(
            events.first(),
            Some(StreamEvent::RateLimitWait { wait_secs: 1, .. })
        ));
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::MessageDone { .. })));
        assert_eq!(hub.requests("/responses").await.len(), 2);
        assert_eq!(CooldownStore::global().until(&hub.key(), Utc::now()), None);

        let long = Harness::new(true).await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(throttled(120))
            .mount(&long.server)
            .await;
        let before = Utc::now();
        let err = long.ask(&[LlmMessage::user("hi")]).await.unwrap_err();
        assert!(
            matches!(
                subscription_error(&err),
                SubscriptionError::LimitReached { until: Some(_), .. }
            ),
            "{err}"
        );
        let until = CooldownStore::global()
            .until(&long.key(), Utc::now())
            .expect("the throttled login cools");
        assert!(
            until >= before + TimeDelta::seconds(119)
                && until <= Utc::now() + TimeDelta::seconds(120)
        );
        assert_eq!(long.requests("/responses").await.len(), 1);
    }

    #[tokio::test]
    async fn usage_limit_cools_login_and_is_never_retried() {
        let hub = Harness::new(true).await;
        let resets_at = (Utc::now() + TimeDelta::days(2)).timestamp();
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
                "error": {"type": "usage_limit_reached", "message": "The usage limit has been reached",
                          "plan_type": "plus", "resets_at": resets_at}
            })))
            .mount(&hub.server)
            .await;

        let err = hub.ask(&[LlmMessage::user("hi")]).await.unwrap_err();
        let reset = DateTime::from_timestamp(resets_at, 0).unwrap();
        assert!(
            matches!(subscription_error(&err), SubscriptionError::LimitReached { until: Some(until), .. } if *until == reset),
            "{err}"
        );
        assert_eq!(
            classify_error(&err.to_string()),
            ErrorKind::SubscriptionUnavailable
        );
        assert_eq!(hub.requests("/responses").await.len(), 1);
        assert_eq!(
            CooldownStore::global().until(&hub.key(), Utc::now()),
            Some(reset)
        );
    }

    #[tokio::test]
    async fn invalid_reasoning_retries_once_without_reasoning() {
        let hub = Harness::new(true).await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(body_string_contains("\"type\":\"reasoning\""))
            .respond_with(recorded(TAMPERED))
            .mount(&hub.server)
            .await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(recorded(TEXT))
            .mount(&hub.server)
            .await;
        let reasoning = ContentBlock::Thinking {
            thinking: "Planning".into(),
            signature: Some(format!(
                "{OAI_REASONING_PREFIX}v1:{}",
                serde_json::json!({"id": "rs_1", "summary": ["Planning"], "enc": "gAAAAtampered"})
            )),
        };
        let history = [
            LlmMessage::user("why is the sky blue?"),
            LlmMessage {
                role: Role::Assistant,
                content: vec![
                    reasoning,
                    ContentBlock::Text {
                        text: "Scattering.".into(),
                    },
                ],
            },
            LlmMessage::user("say more"),
        ];

        let response = hub.ask(&history).await.unwrap();
        assert_eq!(response.text().as_deref(), Some(SKY));
        let sent = hub.requests("/responses").await;
        assert_eq!(sent.len(), 2);
        let retried = String::from_utf8(sent[1].body.clone()).unwrap();
        assert!(!retried.contains("\"type\":\"reasoning\""), "{retried}");
        assert!(
            retried.contains("Scattering."),
            "the rest of the history is kept"
        );
    }

    #[tokio::test]
    async fn kill_switch_refuses_per_call_and_chain_moves_on() {
        let hub = Harness::new(false).await;
        let err = hub.ask(&[LlmMessage::user("hi")]).await.unwrap_err();
        assert!(matches!(
            subscription_error(&err),
            SubscriptionError::Disabled
        ));
        assert_eq!(
            classify_error(&err.to_string()),
            ErrorKind::SubscriptionUnavailable
        );

        let metered = Arc::new(Metered::default());
        let chain = FailoverProvider::new(
            Arc::clone(&metered) as Arc<dyn LlmProvider>,
            FailoverConfig {
                failover: vec!["backup-model".to_string()],
                ..Default::default()
            },
        );
        chain
            .add_external_provider("chatgpt", Arc::clone(&hub.provider) as Arc<dyn LlmProvider>)
            .await;
        let response = chain
            .create_message(
                None,
                &[LlmMessage::user("hi")],
                &[],
                Some("chatgpt:gpt-6.1-sol"),
                None,
                true,
            )
            .await
            .unwrap();
        assert_eq!(response.text().as_deref(), Some("metered"));
        assert_eq!(*metered.0.lock(), ["backup-model"]);
        assert!(hub.server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn system_work_is_paid_by_the_person_billed_else_the_operator() {
        const OPERATOR: &str = "6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11";
        const CREATOR: &str = "0b7f3d52-8f0a-4c2f-9d44-3c1e6a1f4e22";
        let hub = Harness::new(true).await;
        let personal = |person: &str| {
            let signed_in = login(&format!("acct-{person}"), person, "rt-0");
            hub.vault
                .set(
                    &crate::user_config::per_user_key(person, LOGIN_KEY),
                    &signed_in,
                )
                .unwrap();
            bearer(AuthJson::parse(&signed_in).unwrap().access_token().unwrap())
        };
        let (operator, creator) = (personal(OPERATOR), personal(CREATOR));
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(recorded(TEXT))
            .mount(&hub.server)
            .await;
        let with_operator = PlanLogins::new(
            Subscriptions::new(&WorkforceConfig::default()).with_chatgpt(ChatGptBackend::at(
                &hub.server.uri(),
                format!("{}/oauth/token", hub.server.uri()),
                true,
            )),
            CredentialScope::System {
                vault: Arc::clone(&hub.vault),
                operator: Some(Uuid::parse_str(OPERATOR).unwrap()),
            },
        )
        .chatgpt(Some("gpt-6.1-sol"), None);
        async fn ask(provider: &SubscriptionProvider) -> Result<LlmResponse> {
            let messages = [LlmMessage::user("tags?")];
            provider
                .create_message(None, &messages, &[], None, None, false)
                .await
        }
        let last_bearer = || async {
            hub.requests("/responses")
                .await
                .last()
                .and_then(|r| r.headers.get("authorization"))
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };

        ask(&with_operator).await.unwrap();
        assert_eq!(
            last_bearer().await,
            Some(operator),
            "no one in scope: the operator"
        );

        crate::user_config::billed_to(Some(Uuid::parse_str(CREATOR).unwrap()), ask(&with_operator))
            .await
            .unwrap();
        assert_eq!(
            last_bearer().await,
            Some(creator),
            "work billed to a person (an artifact's creator) runs on their own login"
        );

        ask(hub.provider.as_ref()).await.unwrap();
        assert_eq!(
            last_bearer().await,
            Some(bearer(&hub.access_token())),
            "no operator: the install's own login"
        );
    }

    #[tokio::test]
    async fn provider_verify_is_ok_without_payer() {
        let hub = Harness::new(true).await;
        hub.vault.delete(LOGIN_KEY).unwrap();
        hub.provider.verify_credentials().await.unwrap();
        assert!(hub.server.received_requests().await.unwrap().is_empty());
        assert_eq!(
            hub.provider.context_floor("gpt-6.1-sol").await,
            Some(500_000)
        );
    }

    #[tokio::test]
    async fn an_unknown_model_moves_the_chain_on_without_aborting() {
        let hub = Harness::new(true).await;
        let err = hub
            .provider
            .create_message(
                None,
                &[LlmMessage::user("hi")],
                &[],
                Some("gpt-5.4-mini"),
                None,
                false,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                subscription_error(&err),
                SubscriptionError::ModelNotInPlan { login: None, .. }
            ),
            "{err}"
        );
        assert_eq!(
            classify_error(&err.to_string()),
            ErrorKind::SubscriptionUnavailable
        );

        let metered = Arc::new(Metered::default());
        let chain = FailoverProvider::new(
            Arc::clone(&hub.provider) as Arc<dyn LlmProvider>,
            FailoverConfig {
                failover: vec!["gemini:backup-model".to_string()],
                ..Default::default()
            },
        );
        chain
            .add_external_provider("gemini", Arc::clone(&metered) as Arc<dyn LlmProvider>)
            .await;
        let response = chain
            .create_message(
                None,
                &[LlmMessage::user("hi")],
                &[],
                Some("gpt-5.4-mini"),
                None,
                true,
            )
            .await
            .unwrap();
        assert_eq!(response.text().as_deref(), Some("metered"));
        assert_eq!(*metered.0.lock(), ["backup-model"]);
        assert!(hub.server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn retired_model_moves_the_chain_on_without_a_call() {
        let hub = Harness::new(true).await;
        let err = hub
            .provider
            .create_message(
                None,
                &[LlmMessage::user("hi")],
                &[],
                Some("gpt-5.5"),
                None,
                false,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                subscription_error(&err),
                SubscriptionError::ModelNotInPlan { login: None, .. }
            ),
            "{err}"
        );
        assert!(
            err.to_string()
                .contains("gpt-5.5 retired from ChatGPT sign-ins on 2026-10-14"),
            "{err}"
        );
        assert_eq!(
            classify_error(&err.to_string()),
            ErrorKind::SubscriptionUnavailable
        );
        assert!(hub.server.received_requests().await.unwrap().is_empty());
    }
}
