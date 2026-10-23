//! `claude_code.setup_token` — agent-callable Claude plan OAuth token install.
//!
//! Wraps a `vault.set` of `claude_code/oauth_token` (the credential the
//! Claude Code CLI subprocess consumes for `advisor` source = "claude_code"
//! and `coding.session`) and immediately triggers re-registration of
//! `coding.session` so the tool appears without a hub restart.
//!
//! Exists because the manual `vault.set path=… scope=… value=…` invocation
//! is non-obvious — the path, scope choice, and "Claude Code OAuth ≠ API key
//! ≠ LLM account token" distinction are easy to get wrong (see the in-Slack
//! exchange where Candy guessed Slack tokens, API keys, and LLM-pool tokens
//! before failing). A dedicated tool with a precise schema makes the
//! intent self-documenting.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;

use workforce_agent::tools::{Tool, ToolResult};

use workforce_common::settings::VaultActor;
use workforce_common::subscription::SubscriptionKind;

pub use crate::hub_core::coding_sessions::ReloadCodingSession;
use crate::hub_core::logins::resolve::VaultSlot;
use crate::hub_core::logins::store::store_checked;
use crate::user_config::{current_identity_id, per_user_key};
use crate::vault::Vault;

const KEY: &str = SubscriptionKind::Claude.vault_key();

/// How a person connects their Claude plan, as the clause a notice ends on.
/// How a person connects their Claude plan when the agent cannot run for
/// them at all: only My Settings works then, since a token pasted to the
/// agent needs a turn to save it.
pub const CONNECT_CLAUDE: &str =
    "run `claude setup-token` on your computer, then add the token under My Settings → Claude";

/// [`CONNECT_CLAUDE`] plus the paste route, for a person the agent is
/// already answering: its turn can save the token for them.
pub const CONNECT_CLAUDE_OR_PASTE: &str =
    "run `claude setup-token` on your computer, then add the token under \
     My Settings → Claude, or send it to me in a private conversation and ask me to save it \
     with `claude_code.setup_token`";

/// `claude_code.setup_token` tool.
pub struct ClaudeCodeSetupTokenTool {
    vault: Arc<Vault>,
    reload_coding_session: ReloadCodingSession,
    agent_id: String,
}

impl ClaudeCodeSetupTokenTool {
    pub fn new(
        vault: Arc<Vault>,
        reload_coding_session: ReloadCodingSession,
        agent_id: String,
    ) -> Self {
        Self {
            vault,
            reload_coding_session,
            agent_id,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SetupScope {
    /// `users/{uid}/claude_code/oauth_token` — recommended default. Ties the
    /// Claude plan to the active user; only that user's runtime
    /// invocations of `advisor` / `coding.session` will use it.
    User,
    /// `claude_code/oauth_token` — shared across every user of this agent.
    /// Use only when the agent is explicitly single-tenant or the human has
    /// stated the Claude plan is shared across all users of this agent.
    AgentShared,
}

#[derive(Deserialize)]
struct Input {
    token: String,
    #[serde(default)]
    scope: Option<SetupScope>,
}

/// Tool description, surfaced to the LLM. Kept as a module const so the
/// regression guard can assert it never recommends the `/secret` slash
/// command (which silently fails in Slack threads) — the only thread-safe
/// way to set a named secret is `cmd: secret <name> <value>`.
pub const DESCRIPTION: &str = "Install the user's Claude plan OAuth token so this agent can use \
     Claude Code (the `advisor` tool with source='claude_code' and the \
     `coding.session` tool). The token is NOT an Anthropic API key, NOT \
     a Slack token, NOT an LLM-pool token — it is the OAuth token the \
     user obtains by running `claude setup-token` locally in their \
     terminal. The token starts with `sk-ant-oat` (e.g. `sk-ant-oat01-…`). \
     After a successful \
     write `coding.session` is re-registered immediately (no hub \
     restart).\n\n\
     When the user asks 'help me set up my Claude Code / Max / \
     coding.session / advisor token', this is the tool to call. The \
     user has two safe ways to hand you the value:\n\
     (a) `cmd: secret cc_token <value>` first (the thread-safe form — it works \
     on every surface, including Slack threads, where a leading-`/` slash command \
     silently fails; never tell the user to type a `/`-prefixed command), then pass \
     `token='[[secret:cc_token]]'` to this tool — the platform \
     substitutes the marker for the real value at the sink boundary.\n\
     (b) Paste the `sk-ant-oat…` token in their next message — the \
     ingress redaction auto-tokenizes it before you see it, so you \
     pass through whatever `[[secret:…]]` placeholder appears in the \
     user message; same sink substitution applies.\n\
     Default `scope='user'` (per-user, ties the Claude plan to \
     the active identity); only pass `scope='agent_shared'` if the \
     user explicitly says everyone on this agent should drive Claude \
     Code under one subscription. See AGENTS.md for the full script.\n\n\
     To check whether a token is already installed, call \
     `system.llm_status` and read `credentials_summary`: it says which \
     Claude plan login serves the person asking, or why none does. \
     Never test a token by starting a coding session.";

#[async_trait]
impl Tool for ClaudeCodeSetupTokenTool {
    fn name(&self) -> &str {
        "claude_code.setup_token"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "token": {
                    "type": "string",
                    "description": "The Claude plan OAuth token from `claude setup-token`. Starts with `sk-ant-oat` (e.g. `sk-ant-oat01-…`). NEVER paste an API key (`sk-ant-api…`) here — they look similar but are not interchangeable for Claude Code."
                },
                "scope": {
                    "type": "string",
                    "enum": ["user", "agent_shared"],
                    "description": "Where to store the token. Defaults to `user` (writes `users/{uid}/claude_code/oauth_token`) so each user's Claude plan stays tied to their identity. Use `agent_shared` only when the human has explicitly stated the Claude plan is shared across every user of this agent — `agent_shared` lets anyone using this agent drive Claude Code under one person's subscription."
                }
            },
            "required": ["token"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: Input = serde_json::from_value(input)?;

        let token = params.token.trim();
        let uid = current_identity_id();
        let scope = params.scope.unwrap_or(match uid.as_deref() {
            Some(_) => SetupScope::User,
            None => SetupScope::AgentShared,
        });
        let effective_path = match (scope, uid.as_deref()) {
            (SetupScope::User, Some(uid)) => per_user_key(uid, KEY),
            (SetupScope::User, None) => {
                return Ok(ToolResult::failure(
                    "Could not write the per-user token: no user identity is in scope. Retry with scope='agent_shared'.",
                    start.elapsed().as_millis() as u64,
                ));
            }
            (SetupScope::AgentShared, _) => KEY.to_string(),
        };
        let slot = VaultSlot {
            vault: Arc::clone(&self.vault),
            key: effective_path.clone(),
        };
        let actor = uid.as_deref().map_or(
            VaultActor::System("claude_code_setup_token"),
            VaultActor::User,
        );
        if let Err(refused) =
            store_checked(&slot, SubscriptionKind::Claude, token, &[], actor).await
        {
            return Ok(ToolResult::failure(
                refused.to_string(),
                start.elapsed().as_millis() as u64,
            ));
        }

        // Re-register `coding.session` so the tool appears this turn. The
        // closure is idempotent: it no-ops if already registered, and now
        // sees a configured token via the relaxed startup gate.
        (self.reload_coding_session)();

        tracing::info!(
            target: "sentinel.audit",
            agent = %self.agent_id,
            vault_path = %effective_path,
            scope = ?scope,
            "Claude Code OAuth token stored via claude_code.setup_token"
        );

        let msg = match scope {
            SetupScope::User => "Stored under your per-user key. `advisor` (when source='claude_code') and `coding.session` will use this token for your invocations. Other users still need to install their own token via the same tool.",
            SetupScope::AgentShared => "Stored under the shared agent key. Anyone using this agent will drive Claude Code under this Claude plan — confirm that is what the user intended.",
        };

        Ok(ToolResult::success(
            serde_json::json!({
                "path": effective_path,
                "scope": match scope {
                    SetupScope::User => "user",
                    SetupScope::AgentShared => "agent_shared",
                },
                "note": msg,
                "coding_session_registered": true,
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_vault() -> (Arc<Vault>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(&dir.path().join("vault.enc"), "test-key").unwrap();
        (Arc::new(vault), dir)
    }

    fn noop_reload() -> (ReloadCodingSession, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&count);
        let reload: ReloadCodingSession = Arc::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
        });
        (reload, count)
    }

    #[tokio::test]
    async fn rejects_empty_token() {
        let (vault, _t) = temp_vault();
        let (reload, count) = noop_reload();
        let tool = ClaudeCodeSetupTokenTool::new(vault, reload, "test".into());
        let res = tool
            .execute(serde_json::json!({"token": "   "}))
            .await
            .unwrap();
        assert!(!res.success, "empty token must fail");
        assert_eq!(count.load(Ordering::SeqCst), 0, "no reload on failure");
    }

    #[tokio::test]
    async fn rejects_non_oauth_prefix() {
        let (vault, _t) = temp_vault();
        let (reload, count) = noop_reload();
        let tool = ClaudeCodeSetupTokenTool::new(vault, reload, "test".into());
        let res = tool
            .execute(serde_json::json!({"token": "sk-ant-api03-abcdef"}))
            .await
            .unwrap();
        assert!(!res.success, "API key prefix must be rejected");
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn accepts_versioned_oat_prefix() {
        // Real tokens from `claude setup-token` are `sk-ant-oat01-…` today,
        // and the suffix is versioned — `oat`, `oat01`, `oat02`, … must all
        // pass. A fixed `sk-ant-oat-` check rejected `oat01` (regression that
        // broke token install in chat).
        let (vault, _t) = temp_vault();
        let (reload, count) = noop_reload();
        let tool = ClaudeCodeSetupTokenTool::new(Arc::clone(&vault), reload, "test".into());
        let token = concat!("sk-ant", "-oat01-versioned-value");
        let res = tool
            .execute(serde_json::json!({"token": token}))
            .await
            .unwrap();
        assert!(
            res.success,
            "versioned oat01 token must be accepted: {res:?}"
        );
        assert_eq!(vault.get("claude_code/oauth_token").as_deref(), Some(token));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn writes_agent_shared_when_no_identity_scope() {
        let (vault, _t) = temp_vault();
        let (reload, count) = noop_reload();
        let tool = ClaudeCodeSetupTokenTool::new(Arc::clone(&vault), reload, "test".into());
        let token = "sk-ant-oat-test-value";
        let res = tool
            .execute(serde_json::json!({"token": token}))
            .await
            .unwrap();
        assert!(res.success, "{:?}", res);
        assert_eq!(vault.get("claude_code/oauth_token").as_deref(), Some(token));
        assert_eq!(count.load(Ordering::SeqCst), 1, "reload triggered once");
    }

    #[tokio::test]
    async fn writes_user_scoped_path_under_identity_scope() {
        let (vault, _t) = temp_vault();
        let (reload, count) = noop_reload();
        let tool = ClaudeCodeSetupTokenTool::new(Arc::clone(&vault), reload, "test".into());
        let token = "sk-ant-oat-user-value";
        let res = workforce_agent::runner::CURRENT_USER_ID
            .scope("uid-42".to_string(), async {
                tool.execute(serde_json::json!({"token": token})).await
            })
            .await
            .unwrap();
        assert!(res.success, "{:?}", res);
        assert_eq!(
            vault.get("users/uid-42/claude_code/oauth_token").as_deref(),
            Some(token)
        );
        assert!(
            vault.get("claude_code/oauth_token").is_none(),
            "shared key must not be written for user-scope set"
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn explicit_agent_shared_overrides_user_default() {
        let (vault, _t) = temp_vault();
        let (reload, _count) = noop_reload();
        let tool = ClaudeCodeSetupTokenTool::new(Arc::clone(&vault), reload, "test".into());
        let token = "sk-ant-oat-shared-value";
        let res = workforce_agent::runner::CURRENT_USER_ID
            .scope("uid-42".to_string(), async {
                tool.execute(serde_json::json!({
                    "token": token,
                    "scope": "agent_shared",
                }))
                .await
            })
            .await
            .unwrap();
        assert!(res.success, "{:?}", res);
        assert_eq!(vault.get("claude_code/oauth_token").as_deref(), Some(token));
        assert!(vault.get("users/uid-42/claude_code/oauth_token").is_none());
    }
}
