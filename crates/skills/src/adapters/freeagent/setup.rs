//! Agent-facing OAuth driver for the FreeAgent skill.
//!
//! FreeAgent only supports the redirect (web) flow, so onboarding is two tool
//! calls: `login` returns an authorization URL and records pending state in
//! the shared [`OAuthStateStore`]; the hub callback exchanges the code and
//! persists tokens; `login_poll` reports `authorized` once they land. The
//! remaining actions inspect or manage an existing connection.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::{eyre, Result};
use serde_json::{json, Value};
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::settings::SecretStore;
use workforce_oauth_client::{OAuthClient, OAuthStateStore, STATE_TTL};

use super::client::build_http_client;
use super::provider::FreeAgentProvider;
use super::token_store::{build_token_store, list_workspaces, WORKSPACE_PREFIX};
use super::{read_client_creds, read_sandbox};

pub const SETUP_TOOL_NAME: &str = "freeagent.setup";

/// The setup tool's description — shared by the `Tool` impl and the skill
/// definition so they cannot drift.
pub(super) fn description() -> &'static str {
    "Connect and manage FreeAgent companies. \
     \
     ADMIN PREREQUISITE (one-time): the hub admin registers a FreeAgent OAuth app and \
     populates skills/freeagent/oauth_client/{client_id,client_secret,redirect_uri} in the \
     vault, with redirect_uri pointing at https://<hub>/api/auth/freeagent/callback. \
     \
     PER-USER SETUP — two steps: (1) call action='login'; relay the returned \
     authorization_url to the user verbatim (the 'next_step' field has the exact wording) and \
     have them open it and approve. (2) call action='login_poll' until status='authorized'. \
     \
     OTHER ACTIONS: status (one company), list (all connected), logout (remove tokens), \
     refresh (force an access-token refresh). The 'workspace' parameter (default 'default') \
     namespaces multiple FreeAgent companies per user."
}

/// The setup tool's input schema — shared by the `Tool` impl and the skill
/// definition.
pub(super) fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["login", "login_poll", "logout", "list", "status", "refresh"],
                "description": "login: start the web OAuth flow (returns authorization_url). login_poll: check whether the connection completed. status/list/logout/refresh: inspect or manage existing connections."
            },
            "workspace": {
                "type": "string",
                "description": "Company name (default: 'default'). Use a per-company name when connecting more than one."
            }
        },
        "required": ["action"]
    })
}

/// Per-workspace vault fields removed on logout.
const WORKSPACE_FIELDS: &[&str] = &[
    "refresh_token",
    "access_token",
    "expires_at",
    "scopes",
    "company",
];

pub struct FreeAgentSetupTool {
    store: Arc<dyn SecretStore>,
    state_store: Option<Arc<dyn OAuthStateStore>>,
}

impl FreeAgentSetupTool {
    pub fn new(store: Arc<dyn SecretStore>) -> Self {
        Self {
            store,
            state_store: None,
        }
    }

    /// Wire the hub-shared pending-flow store. Without it, `login` errors —
    /// the web-redirect flow only works where a callback is served.
    pub fn with_state_store(mut self, store: Arc<dyn OAuthStateStore>) -> Self {
        self.state_store = Some(store);
        self
    }

    fn field(&self, workspace: &str, field: &str) -> String {
        format!("{WORKSPACE_PREFIX}{workspace}/{field}")
    }
}

#[async_trait]
impl Tool for FreeAgentSetupTool {
    fn name(&self) -> &str {
        SETUP_TOOL_NAME
    }

    fn description(&self) -> &str {
        description()
    }

    fn input_schema(&self) -> Value {
        input_schema()
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let action = input["action"].as_str().unwrap_or("");
        let workspace = input["workspace"].as_str().unwrap_or("default").to_string();

        let result = match action {
            "login" => self.action_login(&workspace),
            "login_poll" => Ok(self.action_login_poll(&workspace)),
            "logout" => self.action_logout(&workspace),
            "list" => Ok(self.action_list()),
            "status" => Ok(self.action_status(&workspace)),
            "refresh" => self.action_refresh(&workspace).await,
            _ => Err(eyre!(
                "unknown action '{action}'. Valid: login, login_poll, logout, list, status, refresh"
            )),
        };

        let duration = start.elapsed().as_millis() as u64;
        match result {
            Ok(val) => Ok(ToolResult::success(val, duration)),
            Err(e) => Ok(ToolResult::failure(format!("{e:#}"), duration)),
        }
    }
}

impl FreeAgentSetupTool {
    fn action_login(&self, workspace: &str) -> Result<Value> {
        let creds = read_client_creds(self.store.as_ref())?;
        let redirect_uri = creds.redirect_uri.clone().ok_or_else(|| {
            eyre!(
                "FreeAgent web login requires `{}redirect_uri` (point it at \
                 https://<hub>/api/auth/freeagent/callback and register the same URL on the \
                 FreeAgent OAuth app)",
                super::OAUTH_CLIENT_PREFIX
            )
        })?;
        let Some(state_store) = &self.state_store else {
            return Err(eyre!(
                "FreeAgent web login is only available on the hub (no callback is served here)"
            ));
        };
        let uid = workforce_agent::runner::CURRENT_USER_ID
            .try_with(Clone::clone)
            .map_err(|_| {
                eyre!("freeagent.setup login requires an active user identity; this call has none")
            })?;

        let sandbox = read_sandbox(self.store.as_ref());
        let http = build_http_client()?;
        let oauth = OAuthClient::new(Arc::new(FreeAgentProvider::new(sandbox)), creds, http);
        let started = oauth.start_web_flow(&redirect_uri, &[]);
        state_store.insert(
            started.state.clone(),
            "freeagent".to_string(),
            uid,
            workspace.to_string(),
            started.verifier,
        )?;

        Ok(json!({
            "status": "pending_authorization",
            "workspace": workspace,
            "authorization_url": started.authorization_url,
            "expires_in_seconds": STATE_TTL.as_secs(),
            "next_step": format!(
                "Give the user this URL to open in their browser: {}. They sign into the \
                 FreeAgent company they want to connect and approve; the hub captures the result \
                 automatically. Then call freeagent.setup(action='login_poll', workspace='{}') — \
                 it returns 'authorized' once the tokens land.",
                started.authorization_url, workspace
            )
        }))
    }

    fn action_login_poll(&self, workspace: &str) -> Value {
        if self
            .store
            .get(&self.field(workspace, "refresh_token"))
            .is_some()
        {
            return json!({
                "status": "authorized",
                "workspace": workspace,
                "company": self.store.get(&self.field(workspace, "company")),
                "message": "FreeAgent connected. All freeagent.* tools can now use this company."
            });
        }
        let web_flow_ready = self.state_store.is_some()
            && read_client_creds(self.store.as_ref())
                .ok()
                .and_then(|c| c.redirect_uri)
                .is_some();
        if web_flow_ready {
            json!({
                "status": "pending",
                "workspace": workspace,
                "poll_interval_seconds": 3,
                "message": "Waiting for the user to approve in their browser. Call freeagent.setup(action='login_poll') again in a few seconds."
            })
        } else {
            json!({
                "status": "no_pending",
                "workspace": workspace,
                "message": "No login in progress. Call freeagent.setup(action='login') to start one."
            })
        }
    }

    fn action_logout(&self, workspace: &str) -> Result<Value> {
        for field in WORKSPACE_FIELDS {
            self.store.delete(&self.field(workspace, field))?;
        }
        Ok(json!({
            "status": "ok",
            "workspace": workspace,
            "message": "Tokens removed."
        }))
    }

    fn action_list(&self) -> Value {
        let companies: Vec<Value> = list_workspaces(self.store.as_ref())
            .into_iter()
            .map(|ws| {
                let company = self.store.get(&self.field(&ws, "company"));
                json!({ "workspace": ws, "company": company })
            })
            .collect();
        json!({
            "companies": companies,
            "hint": "Call freeagent.setup(action='login', workspace='<name>') to connect another company."
        })
    }

    fn action_status(&self, workspace: &str) -> Value {
        let authenticated = self
            .store
            .get(&self.field(workspace, "refresh_token"))
            .is_some();
        let expires_at = self
            .store
            .get(&self.field(workspace, "expires_at"))
            .and_then(|s| s.parse::<u64>().ok());
        json!({
            "workspace": workspace,
            "authenticated": authenticated,
            "company": self.store.get(&self.field(workspace, "company")),
            "access_token_expires_at": expires_at,
            "hint": if authenticated {
                "Ready to use."
            } else {
                "Not connected. Call freeagent.setup(action='login', workspace='<name>')."
            }
        })
    }

    async fn action_refresh(&self, workspace: &str) -> Result<Value> {
        let creds = read_client_creds(self.store.as_ref())?;
        let sandbox = read_sandbox(self.store.as_ref());
        let http = build_http_client()?;
        let tokens = build_token_store(Arc::clone(&self.store), workspace, creds, http, sandbox);
        tokens.force_refresh().await?;
        Ok(json!({
            "status": "ok",
            "workspace": workspace,
            "message": "Access token refreshed."
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_support::InMem;
    use workforce_oauth_client::InMemoryOAuthStateStore;

    fn configured() -> Arc<InMem> {
        InMem::from(&[
            ("skills/freeagent/oauth_client/client_id", "cid"),
            ("skills/freeagent/oauth_client/client_secret", "secret"),
            (
                "skills/freeagent/oauth_client/redirect_uri",
                "https://hub.example/api/auth/freeagent/callback",
            ),
        ])
    }

    #[tokio::test]
    async fn login_without_state_store_errors() {
        let tool = FreeAgentSetupTool::new(configured() as Arc<dyn SecretStore>);
        let res = tool.execute(json!({ "action": "login" })).await.unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("only available on the hub"));
    }

    #[tokio::test]
    async fn login_without_redirect_uri_errors() {
        let store = InMem::from(&[
            ("skills/freeagent/oauth_client/client_id", "cid"),
            ("skills/freeagent/oauth_client/client_secret", "secret"),
        ]);
        let tool = FreeAgentSetupTool::new(store as Arc<dyn SecretStore>)
            .with_state_store(Arc::new(InMemoryOAuthStateStore::new()));
        let res = tool.execute(json!({ "action": "login" })).await.unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("redirect_uri"));
    }

    #[tokio::test]
    async fn login_records_state_and_returns_url() {
        let state = Arc::new(InMemoryOAuthStateStore::new());
        let tool = FreeAgentSetupTool::new(configured() as Arc<dyn SecretStore>)
            .with_state_store(state.clone());
        let out = workforce_agent::runner::CURRENT_USER_ID
            .scope("alice".to_string(), async {
                tool.execute(json!({ "action": "login", "workspace": "acme" }))
                    .await
                    .unwrap()
            })
            .await;
        assert!(out.success, "{:?}", out.error);
        let url = out.output["authorization_url"].as_str().unwrap();
        assert!(url.contains("approve_app"));
        assert!(url.contains("response_type=code"));
        assert!(!url.contains("code_challenge"));
        assert_eq!(out.output["status"], "pending_authorization");
    }

    #[tokio::test]
    async fn poll_reports_authorized_when_token_present() {
        let store = configured();
        store
            .set("skills/freeagent/workspaces/acme/refresh_token", "rt")
            .unwrap();
        store
            .set("skills/freeagent/workspaces/acme/company", "Acme Ltd")
            .unwrap();
        let tool = FreeAgentSetupTool::new(store as Arc<dyn SecretStore>)
            .with_state_store(Arc::new(InMemoryOAuthStateStore::new()));
        let out = tool
            .execute(json!({ "action": "login_poll", "workspace": "acme" }))
            .await
            .unwrap();
        assert_eq!(out.output["status"], "authorized");
        assert_eq!(out.output["company"], "Acme Ltd");
    }

    #[tokio::test]
    async fn logout_removes_tokens() {
        let store = configured();
        store
            .set("skills/freeagent/workspaces/default/refresh_token", "rt")
            .unwrap();
        let tool = FreeAgentSetupTool::new(store.clone() as Arc<dyn SecretStore>);
        tool.execute(json!({ "action": "logout" })).await.unwrap();
        assert!(store
            .get("skills/freeagent/workspaces/default/refresh_token")
            .is_none());
    }

    #[tokio::test]
    async fn status_reports_unauthenticated_when_empty() {
        let tool = FreeAgentSetupTool::new(configured() as Arc<dyn SecretStore>);
        let out = tool.execute(json!({ "action": "status" })).await.unwrap();
        assert_eq!(out.output["authenticated"], false);
    }
}
