//! When a turn has no login to run on: telling the person it is for how to
//! connect one, starting a ChatGPT sign-in for them, and answering their
//! message once they have.
//!
//! The sign-in's code never goes where the turn's progress or answer goes —
//! a shared conversation reaches other people — only to the person: their
//! own web session reads it through an owner-checked route, and Slack sends
//! it by direct message.

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_agent::llm::types::TokenUsage;
use workforce_agent::runner::ProgressEvent;
use workforce_common::session::{MessageRole, Session, TurnPrincipal};
use workforce_common::subscription::{ConnectPrompt, SubscriptionKind};

use super::{done_of, ChainOutcome};
use crate::agent_registry::AgentRegistry;
use crate::claude_code_setup_token_tool::CONNECT_CLAUDE;
use crate::serve::AgentService;
use crate::slack_manager::{session_surfaces, WorkspaceSurfaces};
use crate::subscription::chatgpt_auth::PASTE_FALLBACK;
use crate::subscription::resolve::Miss;
use crate::subscription::{DeviceFlow, DeviceLoginListener, FlowState, LoginTarget, NoLogin};

/// What a re-run turn is asked once the person has connected.
pub const CONNECTED_PROMPT: &str = "[SYSTEM: The user has just connected the plan this agent \
     runs on. Answer their pending request above.]";
/// What a Slack thread waiting on a sign-in is told when its code expires.
const CODE_EXPIRED: &str =
    "The ChatGPT sign-in code expired before it was used. Send your message again for a new one.";
/// What a Slack thread is told when a sign-in completes after the
/// conversation moved on past the message that waited on it.
const MOVED_ON: &str =
    "Your ChatGPT sign-in finished; send your message again if you still need it.";

/// A turn no route served, for a person who can make one serve: what they
/// are told as the turn's answer, and the prompt a surface offers them.
pub(super) struct Connect {
    text: String,
    prompt: ConnectPrompt,
}

impl Connect {
    /// The turn's notice, offered to whoever watches the turn.
    pub(super) async fn notice(
        self,
        progress: Option<&mpsc::Sender<ProgressEvent>>,
    ) -> ChainOutcome {
        if let Some(progress) = progress {
            for event in [
                ProgressEvent::ConnectRequired {
                    prompt: self.prompt,
                },
                ProgressEvent::Response {
                    text: self.text.clone(),
                },
                done_of(&TokenUsage::default(), 0, 0),
            ] {
                let _ = progress.send(event).await;
            }
        }
        ChainOutcome::Notice { text: self.text }
    }
}

/// A message that waits on a ChatGPT sign-in, kept on the flow: answered
/// when the sign-in completes, unless the conversation moved on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectWaiter {
    pub agent_id: String,
    pub session_id: String,
    pub message_id: String,
    pub principal: TurnPrincipal,
}

/// The logins of the person's own that a sign-in would make serve the call,
/// in the order the agent tries them: the tiers with no usable login, when
/// nothing is cooling, which only time fixes.
pub(super) fn connectable(agent_id: &str, no_login: &NoLogin) -> Vec<LoginTarget> {
    if no_login.cooling_until().is_some() {
        return Vec::new();
    }
    no_login
        .misses
        .iter()
        .filter(|(tier, miss)| !tier.is_shared() && miss.needs_sign_in())
        .filter_map(|(tier, _)| LoginTarget::new(*tier, Some(agent_id.to_string())))
        .collect()
}

/// A plan the person has a login for whose routes still did not serve the
/// turn, and why: told before they are asked to connect another plan.
pub(super) struct Unserved {
    pub kind: SubscriptionKind,
    /// Person-facing, one per route that ran on the plan.
    pub reasons: Vec<String>,
}

impl Unserved {
    /// "Your ChatGPT plan couldn't serve this: …"
    fn sentence(&self) -> String {
        let why = match self.reasons.is_empty() {
            true => "none of its routes could run this turn".to_string(),
            false => self.reasons.join("; "),
        };
        format!(
            "Your {} plan couldn't serve this: {}.",
            self.kind.label(),
            why.trim_end_matches('.')
        )
    }
}

/// What a person hears before being asked to connect a `no_login` plan: why
/// each plan they do have a login for did not serve (`unserved`), and that
/// a login of theirs for this plan is stored but refused, so they sign in
/// again rather than wonder why one is being asked for.
fn lead(no_login: &NoLogin, unserved: &[Unserved]) -> String {
    let mut lines: Vec<String> = unserved.iter().map(Unserved::sentence).collect();
    let refused: Vec<String> = no_login
        .misses
        .iter()
        .filter(|(tier, miss)| !tier.is_shared() && miss.needs_sign_in())
        .filter_map(|(tier, miss)| match miss {
            Miss::Invalid => Some(format!("{} was refused", tier.as_str())),
            Miss::Malformed(reason) => Some(format!("{} is unusable ({reason})", tier.as_str())),
            _ => None,
        })
        .collect();
    if !refused.is_empty() {
        lines.push(format!(
            "Your stored {} login must be signed in again: {}.",
            no_login.kind.label(),
            refused.join("; ")
        ));
    }
    lines.into_iter().map(|line| format!("{line} ")).collect()
}

/// How `person` connects a `kind` login for `service`'s turns in `session`.
/// A ChatGPT login is signed in from the hub, the message the turn answers
/// waiting on it: a sign-in they have pending for any of `serving` is
/// joined, else one is started for `start`. The notice opens with why the
/// plans they already have a login for (`unserved`) did not serve.
pub(super) async fn prompt(
    service: &AgentService,
    session: &Session,
    person: Uuid,
    kind: SubscriptionKind,
    (start, serving): (LoginTarget, &[LoginTarget]),
    no_login: &NoLogin,
    unserved: &[Unserved],
) -> Connect {
    let agent_id = service.agent_id.as_str();
    let (text, flow_id, resumes) = match kind {
        SubscriptionKind::Claude => (claude_notice(agent_id), None, false),
        SubscriptionKind::ChatGpt => {
            let waiter = waiter_of(agent_id, session, person);
            let waits = waiter.is_some();
            let device = service.subscriptions().device_logins();
            let flow = match device.join(person, serving, waiter.clone()).await {
                Ok(Some(flow)) => Ok(flow),
                Ok(None) => device.start_or_join(person, start, waiter).await,
                Err(e) => Err(e),
            };
            match flow {
                Ok(flow) => (
                    chatgpt_notice(agent_id, &flow),
                    Some(flow.id),
                    waits && flow.state != FlowState::Unsupported,
                ),
                Err(e) => {
                    tracing::warn!(agent = %agent_id, error = %e, "no ChatGPT sign-in started for a turn");
                    (chatgpt_settings_notice(agent_id), None, false)
                }
            }
        }
    };
    Connect {
        text: format!("{}{text}", lead(no_login, unserved)),
        prompt: ConnectPrompt {
            kind,
            agent_id: agent_id.to_string(),
            flow_id,
            reason: no_login.to_string(),
            private: !session.is_shared_conversation(),
            resumes,
        },
    }
}

/// The message this turn answers, as a waiter run as and billed to whoever
/// the turn is: the session's last message from a person, which in a shared
/// conversation is the last one from this turn's person, or the one still
/// unattributed because this turn is answering it.
fn waiter_of(agent_id: &str, session: &Session, person: Uuid) -> Option<serde_json::Value> {
    let principal = crate::user_config::current_principal();
    let asker = principal
        .run_as
        .clone()
        .unwrap_or_else(|| person.to_string());
    let shared = session.is_shared_conversation();
    let message = session.messages.iter().rev().find(|message| {
        message.role == MessageRole::User
            && (!shared || message.actor.as_deref().is_none_or(|actor| actor == asker))
    })?;
    serde_json::to_value(ConnectWaiter {
        agent_id: agent_id.to_string(),
        session_id: session.id.clone(),
        message_id: message.id.clone(),
        principal,
    })
    .ok()
}

/// What a person is told when a call of theirs has no `kind` login: how to
/// connect one when a sign-in of their own would serve it, else why none did.
pub fn no_login_notice(agent_id: &str, no_login: &NoLogin) -> String {
    match (connectable(agent_id, no_login).is_empty(), no_login.kind) {
        (true, _) => no_login.to_string(),
        (false, SubscriptionKind::Claude) => {
            format!("{}{}", lead(no_login, &[]), claude_notice(agent_id))
        }
        (false, SubscriptionKind::ChatGpt) => {
            format!(
                "{}{}",
                lead(no_login, &[]),
                chatgpt_settings_notice(agent_id)
            )
        }
    }
}

/// What a person is told when `agent_id` cannot answer them until they
/// connect their `kind` plan, and `how`.
fn connect_notice(agent_id: &str, kind: SubscriptionKind, how: impl std::fmt::Display) -> String {
    format!(
        "I can't answer on {agent_id} until you connect your {} plan: {how}.",
        kind.label()
    )
}

fn claude_notice(agent_id: &str) -> String {
    connect_notice(
        agent_id,
        SubscriptionKind::Claude,
        format_args!("{CONNECT_CLAUDE}. Then send your message again"),
    )
}

/// How long is left until `until`, in whole minutes rounded up: never less
/// than one.
fn minutes_left(until: DateTime<Utc>, now: DateTime<Utc>) -> i64 {
    ((until - now).num_seconds().max(1) + 59) / 60
}

fn chatgpt_notice(agent_id: &str, flow: &DeviceFlow) -> String {
    let how = match flow.state {
        FlowState::Unsupported => format!(
            "signing in from here isn't enabled for your ChatGPT account, so {PASTE_FALLBACK}. \
             Then send your message again"
        ),
        _ => format!(
            "I've started a ChatGPT sign-in for you. Its one-time code is shown only to you — \
             under My Settings → ChatGPT, or in a direct message on Slack. Enter it at {} within \
             {} and I'll answer this message then",
            flow.verification_url,
            match minutes_left(flow.expires_at, Utc::now()) {
                1 => "1 minute".to_string(),
                minutes => format!("{minutes} minutes"),
            }
        ),
    };
    connect_notice(agent_id, SubscriptionKind::ChatGpt, how)
}

fn chatgpt_settings_notice(agent_id: &str) -> String {
    connect_notice(
        agent_id,
        SubscriptionKind::ChatGpt,
        "connect it under My Settings → ChatGPT, then send your message again",
    )
}

/// Whether the person has said nothing in `session` since `message_id`: the
/// message still waits for the answer a connected login can give.
pub(crate) fn unanswered(session: &Session, message_id: &str) -> bool {
    session
        .messages
        .iter()
        .position(|message| message.id == message_id)
        .is_some_and(|at| {
            session.messages[at + 1..]
                .iter()
                .all(|message| message.role != MessageRole::User)
        })
}

/// A message still waiting on `flow`, with the agent and session it is in.
struct Waiting {
    service: Arc<AgentService>,
    session: Session,
    waiter: ConnectWaiter,
}

impl Waiting {
    /// Whether the person has said nothing since the message that waited.
    fn unanswered(&self) -> bool {
        unanswered(&self.session, &self.waiter.message_id)
    }
}

/// The messages that waited on `flow` whose agent and session remain, the
/// latest one per session, whether or not the conversation moved on.
fn waiters(registry: &AgentRegistry, flow: &DeviceFlow) -> Vec<Waiting> {
    flow.waiters
        .iter()
        .rev()
        .filter_map(|waiter| serde_json::from_value::<ConnectWaiter>(waiter.clone()).ok())
        .fold(Vec::<ConnectWaiter>::new(), |mut latest, waiter| {
            if latest
                .iter()
                .all(|kept| kept.session_id != waiter.session_id)
            {
                latest.push(waiter);
            }
            latest
        })
        .into_iter()
        .filter_map(|waiter| {
            let service = registry.get(&waiter.agent_id)?;
            let session = service.sessions.get(&waiter.session_id).ok().flatten()?;
            Some(Waiting {
                service,
                session,
                waiter,
            })
        })
        .collect()
}

/// The messages still waiting on `flow` whose agent and session remain,
/// the latest one per session.
fn waiting(registry: &AgentRegistry, flow: &DeviceFlow) -> Vec<Waiting> {
    waiters(registry, flow)
        .into_iter()
        .filter(Waiting::unanswered)
        .collect()
}

/// The messages `flow`'s completion answers: those still waiting on an
/// agent its login serves, whose answer has somewhere to go.
fn reruns(registry: &AgentRegistry, flow: &DeviceFlow) -> Vec<Waiting> {
    waiting(registry, flow)
        .into_iter()
        .filter(|waiting| {
            flow.target
                .agent_id()
                .is_none_or(|agent_id| agent_id == waiting.waiter.agent_id)
        })
        .filter(|waiting| crate::slack_manager::delivery_for(&waiting.session).is_some())
        .collect()
}

/// Whether `flow`'s completion answers a message that waited on it.
pub(crate) fn reruns_any(registry: &AgentRegistry, flow: &DeviceFlow) -> bool {
    !reruns(registry, flow).is_empty()
}

/// Answers the messages that waited on a ChatGPT sign-in once it completes,
/// and tells Slack threads when its code expired unused.
pub struct ConnectRerun {
    registry: Weak<AgentRegistry>,
    surfaces: WorkspaceSurfaces,
    push: crate::session_push::SessionPush,
}

impl ConnectRerun {
    pub fn new(
        registry: Weak<AgentRegistry>,
        surfaces: WorkspaceSurfaces,
        push: crate::session_push::SessionPush,
    ) -> Self {
        Self {
            registry,
            surfaces,
            push,
        }
    }
}

#[async_trait]
impl DeviceLoginListener for ConnectRerun {
    async fn settled(&self, flow: DeviceFlow) {
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        match flow.state {
            FlowState::Completed { .. } => {
                for Waiting { session, .. } in waiters(&registry, &flow)
                    .into_iter()
                    .filter(|waiting| !waiting.unanswered())
                {
                    if let Some(target) = crate::slack_manager::resume_target(&session) {
                        crate::slack_manager::send_to_thread(
                            &session_surfaces(&self.surfaces, &session),
                            &target,
                            MOVED_ON.to_string(),
                        )
                        .await;
                    }
                }
                for Waiting {
                    service,
                    session,
                    waiter,
                } in reruns(&registry, &flow)
                {
                    let surfaces = session_surfaces(&self.surfaces, &session);
                    let push = self.push.clone();
                    tokio::spawn(async move {
                        if let Err(e) = crate::slack_manager::rerun_unanswered(
                            &service,
                            &surfaces,
                            &push,
                            &waiter,
                            CONNECTED_PROMPT,
                        )
                        .await
                        {
                            tracing::warn!(session_id = %waiter.session_id, error = %e, "the message that waited on a sign-in was not answered");
                        }
                    });
                }
            }
            FlowState::Expired => {
                for Waiting { session, .. } in waiting(&registry, &flow) {
                    if let Some(target) =
                        crate::slack_manager::resume_target(&session).filter(|(channel, _)| {
                            flow.deliveries
                                .iter()
                                .all(|delivery| delivery.channel != *channel)
                        })
                    {
                        crate::slack_manager::send_to_thread(
                            &session_surfaces(&self.surfaces, &session),
                            &target,
                            CODE_EXPIRED.to_string(),
                        )
                        .await;
                    }
                }
            }
            _ => {}
        }
    }
}

/// Settle the Slack messages that carried a ChatGPT sign-in's code, then
/// answer the messages that waited on it, and poll the hub's sign-ins while
/// this hub leads, until `stop`.
pub fn wire(
    registry: &Arc<AgentRegistry>,
    surfaces: WorkspaceSurfaces,
    push: crate::session_push::SessionPush,
    stop: CancellationToken,
) {
    let Some(default) = registry.default_agent() else {
        return;
    };
    let device = default.subscriptions().device_logins();
    crate::slack_connect::settle_codes(device, registry, surfaces.clone());
    device.on_complete(Arc::new(ConnectRerun::new(
        Arc::downgrade(registry),
        surfaces,
        push,
    )));
    let registry = Arc::downgrade(registry);
    device.spawn_poller(registry, Arc::clone(&default.leader_gate), stop);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use workforce_agent::runner::CURRENT_USER_ID;
    use workforce_common::session::SHARED_OWNER_PREFIX;

    const ALICE: &str = "6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11";
    const BOB: &str = "0b7f3d52-8f0a-4c2f-9d44-3c1e6a1f4e22";

    #[test]
    fn minutes_left_rounds_up_and_never_says_none() {
        let now = Utc::now();
        assert_eq!(minutes_left(now + TimeDelta::minutes(15), now), 15);
        assert_eq!(minutes_left(now + TimeDelta::seconds(14 * 60 + 1), now), 15);
        assert_eq!(minutes_left(now + TimeDelta::seconds(30), now), 1);
        assert_eq!(minutes_left(now - TimeDelta::minutes(1), now), 1);
    }

    #[tokio::test]
    async fn shared_thread_waiter_is_the_askers_own_message() {
        let mut session = Session::new(
            "slack:T1:C1:1700.1",
            &format!("{SHARED_OWNER_PREFIX}C1:1700.1"),
            "allan",
        );
        let asked = session.add_user_message("alice asks", None);
        session.attribute_messages_from(0, ALICE);
        let from = session.messages.len();
        session.add_user_message("bob chimes in", None);
        session.attribute_messages_from(from, BOB);

        let waiter: ConnectWaiter = serde_json::from_value(
            CURRENT_USER_ID
                .scope(ALICE.to_string(), async {
                    waiter_of("allan", &session, Uuid::parse_str(ALICE).unwrap())
                })
                .await
                .expect("a waiter"),
        )
        .unwrap();

        assert_eq!(waiter.message_id, asked);
        assert_eq!(waiter.principal.run_as.as_deref(), Some(ALICE));
    }
}

#[cfg(test)]
mod notice_tests {
    use super::*;
    use workforce_common::subscription::{CredentialTier, Payer};

    fn claude_missing(misses: Vec<(CredentialTier, Miss)>) -> NoLogin {
        NoLogin {
            kind: SubscriptionKind::Claude,
            payer: Payer::Person(Uuid::nil()),
            misses,
        }
    }

    #[test]
    fn an_absent_login_gets_only_the_connect_steps() {
        let no_login = claude_missing(vec![
            (CredentialTier::AgentUser, Miss::Absent),
            (CredentialTier::GlobalUser, Miss::Absent),
        ]);
        let notice = no_login_notice("minny", &no_login);
        assert_eq!(
            notice,
            "I can't answer on minny until you connect your Claude plan: run `claude setup-token` \
             on your computer, then add the token under My Settings → Claude. Then send your \
             message again."
        );
    }

    #[test]
    fn a_refused_stored_login_is_named_before_the_connect_steps() {
        let no_login = claude_missing(vec![
            (CredentialTier::AgentUser, Miss::Absent),
            (CredentialTier::GlobalUser, Miss::Invalid),
        ]);
        let notice = no_login_notice("minny", &no_login);
        assert!(
            notice.starts_with(
                "Your stored Claude login must be signed in again: global_user was refused. \
                 I can't answer on minny until you connect your Claude plan: run `claude setup-token`"
            ),
            "{notice}"
        );
        assert!(!notice.contains("send it to me"), "{notice}");
    }

    #[test]
    fn plans_with_a_login_that_did_not_serve_are_explained_first() {
        let no_login = claude_missing(vec![(CredentialTier::GlobalUser, Miss::Absent)]);
        let unserved = [Unserved {
            kind: SubscriptionKind::ChatGpt,
            reasons: vec![
                "gpt-5.5 is not served on a ChatGPT plan for the ChatGPT login of Alice \
                 (global_user): The 'gpt-5.5' model is not supported."
                    .to_string(),
            ],
        }];
        let text = lead(&no_login, &unserved);
        assert_eq!(
            text,
            "Your ChatGPT plan couldn't serve this: gpt-5.5 is not served on a ChatGPT plan for \
             the ChatGPT login of Alice (global_user): The 'gpt-5.5' model is not supported. "
        );
    }
}
