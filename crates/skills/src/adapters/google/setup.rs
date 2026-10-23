//! Agent-facing OAuth driver for the Google skill.
//!
//! Device-flow login is split across two tool calls so a chat agent can
//! drive the full flow without blocking on the user:
//!
//! 1. `login` (no_browser=true) requests a device code and returns
//!    `user_code` + `verification_url` immediately. Pending state is
//!    persisted to the vault so the poll phase can resume from any
//!    subsequent call, even after a process restart.
//! 2. `login_poll` polls the token endpoint for up to [`POLL_BUDGET`] and
//!    resolves to `authorized` / `pending` / `expired` / `denied`.
//!
//! The CLI keeps using the blocking [`oauth::run_login`] helper directly.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use serde_json::json;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::settings::SecretStore;

use super::oauth::{self, DevicePollOutcome, LoginOptions, OAuthClientCreds};
use super::token_store;
use super::web_flow::WebFlowStateStore;

/// Upper bound on how long a single `login_poll` call blocks. Stays well
/// under typical LLM tool-call timeouts (~60s+) while giving the user time
/// to complete OAuth in one or two polls.
const POLL_BUDGET: Duration = Duration::from_secs(25);

/// Agent-callable setup tool, modelled after `email.setup`.
pub struct GoogleSetupTool {
    store: Arc<dyn SecretStore>,
    web_flow_store: Option<Arc<WebFlowStateStore>>,
}

impl GoogleSetupTool {
    pub fn new(store: Arc<dyn SecretStore>) -> Self {
        Self {
            store,
            web_flow_store: None,
        }
    }

    /// Enable the hub-hosted web-redirect flow. When both this store is set
    /// and `skills/google/oauth_client/redirect_uri` is present in the vault,
    /// `action='login'` emits an authorization URL instead of a device code.
    pub fn with_web_flow_store(mut self, store: Arc<WebFlowStateStore>) -> Self {
        self.web_flow_store = Some(store);
        self
    }
}

#[async_trait]
impl Tool for GoogleSetupTool {
    fn name(&self) -> &str {
        "google.setup"
    }

    fn description(&self) -> &str {
        "Connect and manage Google accounts (Gmail/Calendar/Drive/Sheets/Docs/Tasks/Contacts/Chat/Slides). \
         \
         ADMIN PREREQUISITE (one-time): the hub admin must configure Google Cloud \
         Console and populate skills/google/oauth_client/{client_id,client_secret} \
         in the vault. If 'login' fails with 'No Google OAuth client configured', \
         relay the full error verbatim — it contains step-by-step setup \
         instructions for the admin (API enablement, consent screen, OAuth client \
         creation, vault keys). \
         \
         PER-USER SETUP — two steps for the user: (1) call action='login'. The \
         response shape depends on hub config — it contains EITHER user_code + \
         verification_url (device flow: tell the user to open the URL on any \
         device and enter the code) OR authorization_url (web flow: tell the user \
         to open the URL in their browser and complete consent). The 'next_step' \
         field in every response holds the exact wording to relay to the user — \
         use it verbatim. (2) call action='login_poll' until status='authorized'. \
         Status 'pending' means call again in a few seconds. \
         \
         OTHER ACTIONS: status (one workspace), list (all connected accounts), \
         logout (remove tokens), refresh (force access-token refresh). \
         \
         The 'workspace' parameter (default 'default') namespaces multiple Google \
         accounts per user — use the email local-part for clarity when onboarding \
         more than one."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["login", "login_poll", "logout", "list", "status", "refresh"],
                    "description": "login: start the OAuth flow. Response shape depends on hub config — always read the 'next_step' field and relay it to the user verbatim. login_poll: check authorization status (blocks up to ~25s). status/list/logout/refresh: inspect or manage existing accounts."
                },
                "workspace": {
                    "type": "string",
                    "description": "Workspace name (default: 'default'). Use a per-user name like the email local-part when onboarding multiple accounts."
                },
                "no_browser": {
                    "type": "boolean",
                    "description": "Default true; agents should always omit or pass true. Setting false attempts a localhost-loopback redirect, which only works when this tool is invoked from a machine with a display and browser (i.e. via the workforce CLI, not a hosted hub)."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let action = input["action"].as_str().unwrap_or("");
        let workspace = input["workspace"].as_str().unwrap_or("default").to_string();

        let result = match action {
            "login" => self.action_login(&workspace, &input).await,
            "login_poll" => self.action_login_poll(&workspace).await,
            "logout" => self.action_logout(&workspace).await,
            "list" => self.action_list().await,
            "status" => self.action_status(&workspace).await,
            "refresh" => self.action_refresh(&workspace).await,
            _ => Err(eyre::eyre!(
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

impl GoogleSetupTool {
    async fn action_login(
        &self,
        workspace: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let no_browser = input
            .get("no_browser")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        if !no_browser {
            let opts = LoginOptions {
                workspace: workspace.to_string(),
                no_browser: false,
                override_client: None,
            };
            let tokens = oauth::run_login(self.store.clone(), opts).await?;
            return Ok(json!({
                "status": "authorized",
                "workspace": workspace,
                "email": tokens.email,
                "message": "Google account authorized via loopback flow. Use the `account` parameter on any google.* tool to select this workspace."
            }));
        }

        let creds = OAuthClientCreds::from_store_or_bundled(self.store.as_ref())?;

        if let (Some(ref redirect_uri), Some(ref web_store)) =
            (&creds.redirect_uri, &self.web_flow_store)
        {
            return self.start_web_login(workspace, &creds, redirect_uri, web_store);
        }

        let http = super::api::build_http_client()?;
        oauth::clear_pending_device(self.store.as_ref(), workspace);
        let pending = oauth::request_device_code(&http, &creds).await?;
        oauth::save_pending_device(self.store.as_ref(), workspace, &pending)?;

        Ok(json!({
            "status": "pending_authorization",
            "workspace": workspace,
            "user_code": pending.user_code,
            "verification_url": pending.verification_url,
            "expires_in_seconds": pending.expires_at_unix.saturating_sub(oauth::now_unix()),
            "poll_interval_seconds": pending.interval_secs,
            "next_step": format!(
                "Present the user_code and verification_url to the user verbatim. Ask them to open {} on any device, sign into the Google account they want to connect, and enter the code `{}`. Then call google.setup(action='login_poll', workspace='{}') and keep calling it until status != 'pending'.",
                pending.verification_url, pending.user_code, workspace
            )
        }))
    }

    /// `login_poll` path taken when there's no device-flow pending state.
    /// Either the web-redirect callback has already landed the tokens
    /// (`authorized`), the web flow is configured but the user hasn't
    /// completed consent yet (`pending`), or no login flow is in progress at
    /// all (`no_pending`).
    fn poll_without_device_pending(&self, workspace: &str) -> serde_json::Value {
        let refresh_path = format!("skills/google/workspaces/{workspace}/refresh_token");
        if self.store.get(&refresh_path).is_some() {
            let email_path = format!("skills/google/workspaces/{workspace}/email");
            return json!({
                "status": "authorized",
                "workspace": workspace,
                "email": self.store.get(&email_path),
                "message": "Google account connected. All google.* tools can now use this workspace."
            });
        }

        let web_flow_configured = self.web_flow_store.is_some()
            && OAuthClientCreds::from_store_or_bundled(self.store.as_ref())
                .ok()
                .and_then(|c| c.redirect_uri)
                .is_some();

        if web_flow_configured {
            json!({
                "status": "pending",
                "workspace": workspace,
                "poll_interval_seconds": 3,
                "message": "Waiting for the user to complete authorization in their browser. Call google.setup(action='login_poll') again in a few seconds."
            })
        } else {
            json!({
                "status": "no_pending",
                "workspace": workspace,
                "message": "No login in progress for this workspace. Call google.setup(action='login', no_browser=true) to start one."
            })
        }
    }

    fn start_web_login(
        &self,
        workspace: &str,
        creds: &OAuthClientCreds,
        redirect_uri: &str,
        web_store: &Arc<WebFlowStateStore>,
    ) -> Result<serde_json::Value> {
        let uid = workforce_agent::runner::CURRENT_USER_ID
            .try_with(Clone::clone)
            .map_err(|_| {
                eyre::eyre!(
                    "google.setup web flow requires an active user identity; \
                     this tool call has none"
                )
            })?;
        let started = oauth::start_web_flow(creds, redirect_uri);
        web_store.insert(
            started.state.clone(),
            uid,
            workspace.to_string(),
            started.verifier,
        );
        Ok(json!({
            "status": "pending_authorization",
            "workspace": workspace,
            "authorization_url": started.authorization_url,
            "expires_in_seconds": super::web_flow::STATE_TTL.as_secs(),
            "next_step": format!(
                "Give the user this URL to open in their browser: {}. They'll sign into the Google account they want to connect and grant access; the hub will capture the result automatically. Then call google.setup(action='login_poll', workspace='{}') — it will return 'authorized' once the tokens land.",
                started.authorization_url, workspace
            )
        }))
    }

    async fn action_login_poll(&self, workspace: &str) -> Result<serde_json::Value> {
        let Some(mut pending) = oauth::load_pending_device(self.store.as_ref(), workspace) else {
            return Ok(self.poll_without_device_pending(workspace));
        };

        if oauth::now_unix() >= pending.expires_at_unix {
            oauth::clear_pending_device(self.store.as_ref(), workspace);
            return Ok(expired_response(workspace));
        }

        let creds = OAuthClientCreds::from_store_or_bundled(self.store.as_ref())?;
        let http = super::api::build_http_client()?;

        let deadline = std::time::Instant::now() + POLL_BUDGET;
        loop {
            let outcome = oauth::poll_device_token_once(
                &http,
                self.store.as_ref(),
                workspace,
                &creds,
                &pending,
            )
            .await?;

            match outcome {
                DevicePollOutcome::Authorized(tokens) => {
                    oauth::clear_pending_device(self.store.as_ref(), workspace);
                    return Ok(json!({
                        "status": "authorized",
                        "workspace": workspace,
                        "email": tokens.email,
                        "message": "Google account connected. All google.* tools can now use this workspace."
                    }));
                }
                DevicePollOutcome::Denied => {
                    oauth::clear_pending_device(self.store.as_ref(), workspace);
                    return Ok(json!({
                        "status": "denied",
                        "workspace": workspace,
                        "message": "The user declined the authorization request. If this was a mistake, call google.setup(action='login', no_browser=true) to try again."
                    }));
                }
                DevicePollOutcome::Expired => {
                    oauth::clear_pending_device(self.store.as_ref(), workspace);
                    return Ok(expired_response(workspace));
                }
                DevicePollOutcome::SlowDown { new_interval_secs } => {
                    pending.interval_secs = new_interval_secs;
                    oauth::save_pending_device(self.store.as_ref(), workspace, &pending)?;
                }
                DevicePollOutcome::Pending => {}
            }

            let remaining_budget = deadline.saturating_duration_since(std::time::Instant::now());
            let sleep_for = Duration::from_secs(pending.interval_secs);
            if remaining_budget < sleep_for + Duration::from_millis(500) {
                let remaining_auth_secs = pending.expires_at_unix.saturating_sub(oauth::now_unix());
                return Ok(json!({
                    "status": "pending",
                    "workspace": workspace,
                    "user_code": pending.user_code,
                    "verification_url": pending.verification_url,
                    "remaining_seconds": remaining_auth_secs,
                    "poll_interval_seconds": pending.interval_secs,
                    "message": format!(
                        "The user has not finished authorizing yet. Wait a few seconds and call google.setup(action='login_poll', workspace='{}') again. The code `{}` is still valid at {} for another {} seconds.",
                        workspace, pending.user_code, pending.verification_url, remaining_auth_secs
                    )
                }));
            }
            tokio::time::sleep(sleep_for).await;
        }
    }

    async fn action_logout(&self, workspace: &str) -> Result<serde_json::Value> {
        oauth::delete_workspace(self.store.as_ref(), workspace)?;
        Ok(json!({
            "status": "ok",
            "workspace": workspace,
            "message": "Tokens removed."
        }))
    }

    async fn action_list(&self) -> Result<serde_json::Value> {
        let workspaces = token_store::list_workspaces(self.store.as_ref());
        let mut accounts = Vec::new();
        for ws in &workspaces {
            let email = self
                .store
                .get(&format!("skills/google/workspaces/{ws}/email"));
            accounts.push(json!({
                "workspace": ws,
                "email": email
            }));
        }
        Ok(json!({
            "accounts": accounts,
            "hint": "Call google.setup(action='login', no_browser=true, workspace='<name>') to add another account."
        }))
    }

    async fn action_status(&self, workspace: &str) -> Result<serde_json::Value> {
        let creds = OAuthClientCreds::from_store_or_bundled(self.store.as_ref()).unwrap_or(
            OAuthClientCreds {
                client_id: String::new(),
                client_secret: String::new(),
                redirect_uri: None,
            },
        );
        let http = super::api::build_http_client()?;
        let ts =
            token_store::TokenStore::new(self.store.clone(), workspace.to_string(), creds, http);
        let status = ts.status();
        let pending_in_progress =
            oauth::load_pending_device(self.store.as_ref(), workspace).is_some();
        let hint = if status.has_refresh {
            "Ready to use."
        } else if pending_in_progress {
            "Device-flow login in progress. Call google.setup(action='login_poll', workspace='<name>') to resume."
        } else {
            "Not authenticated. Call google.setup(action='login', no_browser=true, workspace='<name>')."
        };
        Ok(json!({
            "workspace": status.workspace,
            "email": status.email,
            "authenticated": status.has_refresh,
            "pending_authorization": pending_in_progress,
            "access_token_expires_at": status.expires_at,
            "hint": hint
        }))
    }

    async fn action_refresh(&self, workspace: &str) -> Result<serde_json::Value> {
        let creds = OAuthClientCreds::from_store_or_bundled(self.store.as_ref())?;
        let http = super::api::build_http_client()?;
        let ts =
            token_store::TokenStore::new(self.store.clone(), workspace.to_string(), creds, http);
        let _token = ts.force_refresh().await?;
        Ok(json!({
            "status": "ok",
            "workspace": workspace,
            "message": "Access token refreshed."
        }))
    }
}

fn expired_response(workspace: &str) -> serde_json::Value {
    json!({
        "status": "expired",
        "workspace": workspace,
        "message": "The device code expired before the user authorized. Call google.setup(action='login', no_browser=true) to start a new one."
    })
}
