//! `chatgpt.login` — start (or rejoin) a ChatGPT device-code sign-in for the
//! person, the chat twin of My Settings' ChatGPT card. The code binds
//! whoever enters it to the caller's login, so it never enters the
//! conversation: the tool hands back the flow, and the code reaches the
//! person alone — on the chat's connect card, under My Settings, or by
//! direct message on Slack.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use uuid::Uuid;
use workforce_agent::runner::{current_progress_tx, ProgressEvent, CURRENT_SESSION_ID};
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::session::SessionStore;
use workforce_common::subscription::{ConnectPrompt, SubscriptionKind};

use crate::authz::IsAdminFn;
use crate::hosts::LoginHost;
use crate::hub_core::logins::device_login::{
    unsupported_reason, DeviceLoginError, DeviceLogins, FlowState, LoginTarget,
};
use crate::hub_core::logins::store::LoginRefusal;
use crate::user_config::current_identity_id;

pub struct ChatGptLoginTool {
    device: DeviceLogins,
    logins: Arc<dyn LoginHost>,
    sessions: Arc<dyn SessionStore>,
    is_admin: IsAdminFn,
}

impl ChatGptLoginTool {
    pub fn new(
        device: DeviceLogins,
        logins: Arc<dyn LoginHost>,
        sessions: Arc<dyn SessionStore>,
        is_admin: IsAdminFn,
    ) -> Self {
        Self {
            device,
            logins,
            sessions,
            is_admin,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Scope {
    #[default]
    User,
    AgentShared,
}

#[derive(Deserialize)]
struct Input {
    #[serde(default)]
    scope: Scope,
}

pub const DESCRIPTION: &str =
    "Sign the user in to ChatGPT from the hub, so this agent can use their ChatGPT plan \
     (the chatgpt and codex routes). Starts the sign-in and returns its id and where to finish \
     it; the one-time code is never returned to you. The user sees it only themselves — on the \
     connect card in this chat, under My Settings → ChatGPT, or in a direct message on Slack — \
     and enters it at the sign-in page; the hub notices the sign-in by itself and stores it. \
     Default scope 'user' stores the sign-in as the user's own login on this agent; scope \
     'agent_shared' makes it the login everyone on this agent uses, and only the agent's owner \
     or an admin may choose it. Tell the user to continue only if they asked to sign in.";

/// Where the person finds the code of the sign-in the tool started.
const WHERE_THE_CODE_IS: &str = "The one-time code is shown only to the user: on the connect \
     card in this chat, under My Settings → ChatGPT, or in a direct message on Slack. Tell them \
     to enter it at the sign-in page before it expires; never ask them to share it.";

#[async_trait]
impl Tool for ChatGptLoginTool {
    fn name(&self) -> &str {
        "chatgpt.login"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "scope": {
                    "type": "string",
                    "enum": ["user", "agent_shared"],
                    "description": "`user` (default): the user's own login on this agent. `agent_shared`: the login every user of this agent shares (the agent's owner or an admin only)."
                }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;
        let params: Input = serde_json::from_value(input)?;
        let caller = current_identity_id();
        let Some(identity) = caller.as_deref().and_then(|uid| Uuid::parse_str(uid).ok()) else {
            return Ok(ToolResult::failure(
                "Signing in to ChatGPT needs a signed-in user; ask them to connect from My Settings.",
                elapsed(),
            ));
        };
        let agent_id = self.logins.agent_id().to_string();
        let target = match params.scope {
            Scope::User => LoginTarget::AgentUser { agent_id },
            Scope::AgentShared if self.logins.may_share(caller.as_deref(), (self.is_admin)()) => {
                LoginTarget::AgentShared { agent_id }
            }
            Scope::AgentShared => {
                return Ok(ToolResult::failure(
                    LoginRefusal::shared_only_by_tool(),
                    elapsed(),
                ));
            }
        };
        let flow = match self.device.start_or_join(identity, target, None).await {
            Ok(flow) => flow,
            Err(e @ (DeviceLoginError::NoVault | DeviceLoginError::Start(_))) => {
                return Ok(ToolResult::failure(e.to_string(), elapsed()));
            }
            Err(DeviceLoginError::Vault(e)) => return Err(e),
        };
        if flow.state == FlowState::Unsupported {
            return Ok(ToolResult::failure(unsupported_reason(), elapsed()));
        }
        if let Some(progress) = current_progress_tx() {
            let private = CURRENT_SESSION_ID
                .try_with(String::clone)
                .ok()
                .and_then(|id| self.sessions.get(&id).ok().flatten())
                .is_some_and(|session| !session.is_shared_conversation());
            let _ = progress
                .send(ProgressEvent::ConnectRequired {
                    prompt: ConnectPrompt {
                        kind: SubscriptionKind::ChatGpt,
                        agent_id: self.logins.agent_id().to_string(),
                        flow_id: Some(flow.id),
                        reason: "a ChatGPT sign-in was asked for".to_string(),
                        private,
                        resumes: false,
                    },
                })
                .await;
        }
        Ok(ToolResult::success(
            serde_json::json!({
                "flow_id": flow.id,
                "verification_url": flow.verification_url,
                "expires_at": flow.expires_at,
                "note": WHERE_THE_CODE_IS,
            }),
            elapsed(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subscription::AgentLogins;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use workforce_common::session::{InMemorySessionStore, SessionContextType};
    use workforce_common::settings::{OwnerType, WorkforceConfig};

    use crate::hub_core::logins::device_login::DeviceAuthClient;
    use crate::hub_core::logins::resolve::Vaults;
    use crate::vault::Vault;

    const OWNER: &str = "6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11";
    const BOB: &str = "0b7f3d52-8f0a-4c2f-9d44-3c1e6a1f4e22";

    struct Fixture {
        tool: ChatGptLoginTool,
        sessions: Arc<InMemorySessionStore>,
        _server: MockServer,
        _dir: tempfile::TempDir,
    }

    async fn fixture() -> Fixture {
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
        let logins = Arc::new(AgentLogins::new(
            "allan",
            Vaults {
                agent: Some(&vault),
                global: Some(&vault),
            },
            (Some(OWNER.into()), OwnerType::User),
            Arc::new(parking_lot::RwLock::new(None)),
            Arc::new(parking_lot::RwLock::new(None)),
            crate::resolved_config::ConfigCell::new(
                crate::resolved_config::ResolvedConfig::resolve(
                    None,
                    None,
                    &WorkforceConfig::default(),
                    "anthropic",
                    None,
                ),
            ),
        ));
        let sessions = Arc::new(InMemorySessionStore::new());
        let tool = ChatGptLoginTool::new(
            DeviceLogins::new(
                Some(vault),
                DeviceAuthClient::at(reqwest::Client::new(), &server.uri()),
            ),
            logins,
            Arc::clone(&sessions) as Arc<dyn SessionStore>,
            Arc::new(|| false),
        );
        Fixture {
            tool,
            sessions,
            _server: server,
            _dir: dir,
        }
    }

    impl Fixture {
        fn session(&self, owner: &str, context: Option<SessionContextType>) -> String {
            let mut session = self
                .sessions
                .get_or_create(&format!("slack:{}", Uuid::new_v4()), owner, "allan")
                .unwrap();
            session.context_type = context;
            self.sessions.save(&session).unwrap();
            session.id
        }

        async fn call(&self, uid: &str, session_id: &str, input: serde_json::Value) -> ToolResult {
            workforce_agent::runner::CURRENT_USER_ID
                .scope(
                    uid.to_string(),
                    CURRENT_SESSION_ID.scope(session_id.to_string(), self.tool.execute(input)),
                )
                .await
                .unwrap()
        }
    }

    #[tokio::test]
    async fn chatgpt_login_never_returns_the_code() {
        let fx = fixture().await;
        for (session, private) in [
            (fx.session("shared:C1:1700.1", None), false),
            (fx.session(OWNER, Some(SessionContextType::Thread)), true),
            (fx.session(OWNER, Some(SessionContextType::Dm)), true),
            (fx.session(OWNER, None), true),
        ] {
            let (tx, mut rx) = tokio::sync::mpsc::channel(8);
            let started = workforce_agent::runner::CURRENT_PROGRESS_TX
                .scope(tx, fx.call(OWNER, &session, serde_json::json!({})))
                .await;

            assert!(started.success, "{started:?}");
            assert!(!format!("{started:?}").contains("ABCD-1234"), "{started:?}");
            let flow_id = started.output["flow_id"]
                .as_str()
                .expect("the flow")
                .to_string();
            assert!(started.output["note"]
                .as_str()
                .unwrap()
                .contains("My Settings"));
            let Some(ProgressEvent::ConnectRequired { prompt }) = rx.recv().await else {
                panic!("the person is offered the sign-in");
            };
            assert_eq!(prompt.flow_id.map(|id| id.to_string()), Some(flow_id));
            assert_eq!(prompt.kind, SubscriptionKind::ChatGpt);
            assert_eq!(prompt.private, private);
            assert!(
                !prompt.resumes,
                "no message waits on a sign-in the tool started"
            );
        }
    }

    #[tokio::test]
    async fn agent_shared_login_requires_owner_or_admin() {
        let fx = fixture().await;
        let shared = serde_json::json!({"scope": "agent_shared"});
        let bob_dm = fx.session(BOB, Some(SessionContextType::Dm));
        let refused = fx.call(BOB, &bob_dm, shared.clone()).await;
        assert!(!refused.success);
        assert!(
            format!("{refused:?}").contains("owner or an admin"),
            "{refused:?}"
        );
        let owner_dm = fx.session(OWNER, Some(SessionContextType::Dm));
        let started = fx.call(OWNER, &owner_dm, shared).await;
        assert!(started.success, "{started:?}");
    }
}
