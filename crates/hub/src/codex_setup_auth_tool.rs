//! `codex.setup_auth` — agent-callable install of a user's Codex login.
//!
//! `codex login --device-auth` writes `$CODEX_HOME/auth.json`. The file is
//! kilobytes of JWTs, which a chat model abbreviates in passing, so the tool
//! never takes it as an argument: the user stores it in the vault first (the
//! Vault page preset, or `vault.set`), and this tool reads it back, checks it
//! is whole, and copies it to where the `codex` engine looks. Mirrors
//! `claude_code.setup_token` otherwise, re-registering `coding.session` so an
//! install whose whole chain is Codex needs no restart.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::settings::VaultActor;
use workforce_common::subscription::SubscriptionKind;

use crate::authz::IsAdminFn;
use crate::claude_code_setup_token_tool::ReloadCodingSession;
use crate::coding_engine::codex::AUTH_VAULT_KEY;
use crate::hosts::LoginHost;
use crate::hub_core::logins::resolve::{known_slots, VaultSlot};
use crate::hub_core::logins::store::{store_checked, KnownVaults, LoginRefusal};
use crate::user_config::{current_identity_id, per_user_key, UserAwareVault};
use crate::vault::Vault;

pub struct CodexSetupAuthTool {
    vault: Arc<Vault>,
    logins: Arc<dyn LoginHost>,
    is_admin: IsAdminFn,
    known_vaults: KnownVaults,
    reload_coding_session: ReloadCodingSession,
}

impl CodexSetupAuthTool {
    pub fn new(
        vault: Arc<Vault>,
        logins: Arc<dyn LoginHost>,
        is_admin: IsAdminFn,
        known_vaults: KnownVaults,
        reload_coding_session: ReloadCodingSession,
    ) -> Self {
        Self {
            vault,
            logins,
            is_admin,
            known_vaults,
            reload_coding_session,
        }
    }

    /// Whether the caller may set the login every user of this agent runs
    /// on: the agent's owner or a system admin.
    fn may_share(&self, uid: Option<&str>) -> bool {
        self.logins.may_share(uid, (self.is_admin)())
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SetupScope {
    User,
    AgentShared,
}

impl SetupScope {
    fn as_str(self) -> &'static str {
        match self {
            SetupScope::User => "user",
            SetupScope::AgentShared => "agent_shared",
        }
    }
}

#[derive(Deserialize)]
struct Input {
    #[serde(default)]
    vault_path: Option<String>,
    #[serde(default)]
    scope: Option<SetupScope>,
}

pub const DESCRIPTION: &str =
    "Install the user's Codex login so this agent can run the `codex` coding \
     engine (workflow steps with engine = \"codex\", `coding.session` on a Codex \
     chain, `/cx`). Never pass the login to this tool: the user first stores the \
     content of `$CODEX_HOME/auth.json` (written by `codex login --device-auth`) \
     in the vault — the Vault page's \"OpenAI Codex login\" preset, or `vault.set` \
     at the per-user path `codex/auth_json` — and this tool reads it from \
     `vault_path` (default `codex/auth_json`; with `scope='user'` only the \
     caller's own copy), checks every token is whole, and copies it to where \
     the engine looks. Default `scope='user'` keeps it per user; \
     `scope='agent_shared'` copies it to the shared agent key so everyone on \
     this agent runs Codex under one login, is only for when the user says so, \
     and only the agent's owner or an admin may use it. A sign-in already \
     stored elsewhere is refused: each place needs its own sign-in.";

#[async_trait]
impl Tool for CodexSetupAuthTool {
    fn name(&self) -> &str {
        "codex.setup_auth"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "vault_path": {
                    "type": "string",
                    "description": "Where the user stored auth.json: a path relative to their per-user scope. With scope `user` only their own copy is read; with `agent_shared` their own copy first and the shared copy second. Default `codex/auth_json`."
                },
                "scope": {
                    "type": "string",
                    "enum": ["user", "agent_shared"],
                    "description": "Where the engine will find it. `user` (default) keeps `users/{uid}/codex/auth_json`; `agent_shared` copies it to `codex/auth_json` for every user of this agent (the agent's owner or an admin only)."
                }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;
        let params: Input = serde_json::from_value(input)?;
        let source = params
            .vault_path
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .unwrap_or(AUTH_VAULT_KEY)
            .to_string();
        let uid = current_identity_id();
        let scope = params.scope.unwrap_or(match uid.as_deref() {
            Some(_) => SetupScope::User,
            None => SetupScope::AgentShared,
        });
        let (reader, effective_path) = match (scope, uid.as_deref()) {
            (SetupScope::User, Some(uid)) => (
                UserAwareVault::for_user_strict(Arc::clone(&self.vault), uid),
                per_user_key(uid, AUTH_VAULT_KEY),
            ),
            (SetupScope::User, None) => {
                return Ok(ToolResult::failure(
                    "Could not write the per-user login: no user identity is in scope. Retry with scope='agent_shared'.",
                    elapsed(),
                ));
            }
            (SetupScope::AgentShared, uid) if !self.may_share(uid) => {
                return Ok(ToolResult::failure(
                    LoginRefusal::shared_only_by_tool(),
                    elapsed(),
                ));
            }
            (SetupScope::AgentShared, _) => (
                UserAwareVault::from_current_user(Arc::clone(&self.vault)),
                AUTH_VAULT_KEY.to_string(),
            ),
        };
        let Some(auth_json) = reader.resolve(&source) else {
            return Ok(ToolResult::failure(
                format!(
                    "Nothing stored at '{source}'. Store auth.json there first (Vault page preset \"OpenAI Codex login\", or vault.set), then call this tool again."
                ),
                elapsed(),
            ));
        };
        let slot = VaultSlot {
            vault: Arc::clone(&self.vault),
            key: effective_path.clone(),
        };
        let known = known_slots(
            SubscriptionKind::ChatGpt,
            &(self.known_vaults)(),
            uid.as_deref(),
        );
        let actor = uid
            .as_deref()
            .map_or(VaultActor::System("codex_setup_auth"), VaultActor::User);
        match store_checked(&slot, SubscriptionKind::ChatGpt, &auth_json, &known, actor).await {
            Ok(()) => {}
            Err(LoginRefusal::Vault(e)) => {
                return Ok(ToolResult::failure(
                    format!("Could not store the login at {effective_path}: {e}"),
                    elapsed(),
                ));
            }
            Err(refused) => return Ok(ToolResult::failure(refused.to_string(), elapsed())),
        }
        (self.reload_coding_session)();
        tracing::info!(
            target: "sentinel.audit",
            agent = %self.logins.agent_id(),
            source = %source,
            vault_path = %effective_path,
            scope = scope.as_str(),
            "Codex login installed via codex.setup_auth"
        );
        let note = match scope {
            SetupScope::User => "Stored under your per-user key; your Codex rounds use this login. Other users install their own with the same tool.",
            SetupScope::AgentShared => "Stored under the shared agent key: everyone using this agent runs Codex under this login.",
        };
        Ok(ToolResult::success(
            serde_json::json!({
                "path": effective_path,
                "scope": scope.as_str(),
                "note": note,
                "coding_session_registered": true,
            }),
            elapsed(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subscription::AgentLogins;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use workforce_common::settings::{OwnerType, WorkforceConfig};

    use crate::hub_core::logins::chatgpt_auth::AuthJson;
    use crate::hub_core::logins::resolve::Vaults;
    use crate::hub_core::logins::test_support::jwt;

    const OWNER: &str = "6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11";
    const BOB: &str = "0b7f3d52-8f0a-4c2f-9d44-3c1e6a1f4e22";

    fn temp_vault() -> (Arc<Vault>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(&dir.path().join("vault.enc"), "test-key").unwrap();
        (Arc::new(vault), dir)
    }

    /// The tool on an agent owned by [`OWNER`], whose caller is an admin
    /// when `admin` says so.
    fn tool_with(vault: Arc<Vault>, admin: bool) -> (CodexSetupAuthTool, Arc<AtomicUsize>) {
        let reloads = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&reloads);
        let reload: ReloadCodingSession = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        let logins = Arc::new(AgentLogins::new(
            "agent",
            Vaults {
                agent: Some(&vault),
                global: None,
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
        let known: KnownVaults = {
            let vault = Arc::clone(&vault);
            Arc::new(move || vec![Arc::clone(&vault)])
        };
        (
            CodexSetupAuthTool::new(vault, logins, Arc::new(move || admin), known, reload),
            reloads,
        )
    }

    fn tool(vault: Arc<Vault>) -> (CodexSetupAuthTool, Arc<AtomicUsize>) {
        tool_with(vault, false)
    }

    fn token(tag: &str) -> String {
        jwt(serde_json::json!({ "sub": format!("user-{tag}"), "tag": tag }))
    }

    fn sign_in(refresh_token: &str) -> String {
        serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": token("id"),
                "access_token": token("access"),
                "refresh_token": refresh_token,
                "account_id": "acct"
            },
            "last_refresh": "2026-09-13T00:00:00Z"
        })
        .to_string()
    }

    fn login() -> String {
        sign_in("rt-0123456789abcdef")
    }

    async fn as_user<T>(uid: &str, fut: impl std::future::Future<Output = T>) -> T {
        workforce_agent::runner::CURRENT_USER_ID
            .scope(uid.to_string(), fut)
            .await
    }

    #[test]
    fn the_file_is_accepted_only_whole_and_in_the_shapes_the_cli_writes() {
        let validate = |text: &str| AuthJson::parse(text).map(drop);
        assert!(validate(&login()).is_ok());
        assert!(validate(r#"{"OPENAI_API_KEY":"sk-proj-0123456789abcdefghij"}"#).is_ok());
        assert!(validate("sk-ant-oat01-not-json")
            .unwrap_err()
            .contains("not JSON"));
        assert!(validate(r#"{"tokens":{}}"#)
            .unwrap_err()
            .contains("device-auth"));
        let abbreviated = login().replace(&token("access"), "eyJhbG…1ojA");
        let err = validate(&abbreviated).unwrap_err();
        assert!(err.contains("tokens.access_token"), "{err}");
        let dotted = login().replace(&token("id"), "eyJhbG...kw4g");
        assert!(validate(&dotted).unwrap_err().contains("tokens.id_token"));
        assert!(validate(r#"{"OPENAI_API_KEY":"sk-…"}"#)
            .unwrap_err()
            .contains("OPENAI_API_KEY"));
        let refresh_cut = login().replace("rt-0123456789abcdef", "rt-0123…cdef");
        assert!(validate(&refresh_cut)
            .unwrap_err()
            .contains("tokens.refresh_token"));
    }

    #[tokio::test]
    async fn with_no_identity_in_scope_nothing_is_shared() {
        let (vault, _dir) = temp_vault();
        vault.set("codex/staged", &login()).unwrap();
        let (tool, reloads) = tool(Arc::clone(&vault));
        let result = tool
            .execute(serde_json::json!({ "vault_path": "codex/staged" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(vault.get(AUTH_VAULT_KEY).is_none());
        assert_eq!(reloads.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn the_default_path_is_the_engines_own_key() {
        let (vault, _dir) = temp_vault();
        vault.set(AUTH_VAULT_KEY, &login()).unwrap();
        let (tool, reloads) = tool(Arc::clone(&vault));
        let result = as_user(
            OWNER,
            tool.execute(serde_json::json!({ "scope": "agent_shared" })),
        )
        .await
        .unwrap();
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.output["scope"], "agent_shared");
        assert_eq!(result.output["path"], AUTH_VAULT_KEY);
        assert_eq!(reloads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_abbreviated_token_is_refused_naming_the_field_without_a_copy() {
        let (vault, _dir) = temp_vault();
        let cut = login().replace(&token("access"), "eyJhbG…1ojA");
        vault.set("codex/staged", &cut).unwrap();
        let (tool, reloads) = tool(Arc::clone(&vault));
        let result = as_user(
            OWNER,
            tool.execute(
                serde_json::json!({ "vault_path": "codex/staged", "scope": "agent_shared" }),
            ),
        )
        .await
        .unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("tokens.access_token"));
        assert!(vault.get(AUTH_VAULT_KEY).is_none());
        assert_eq!(reloads.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_users_own_login_is_stored_under_their_key() {
        let (vault, _dir) = temp_vault();
        vault
            .set(&per_user_key(BOB, "codex/staged"), &login())
            .unwrap();
        let (tool, reloads) = tool(Arc::clone(&vault));
        let result = as_user(
            BOB,
            tool.execute(serde_json::json!({ "vault_path": "codex/staged" })),
        )
        .await
        .unwrap();
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.output["scope"], "user");
        let own = per_user_key(BOB, AUTH_VAULT_KEY);
        assert_eq!(result.output["path"], own.as_str());
        assert_eq!(vault.get(&own).as_deref(), Some(login().as_str()));
        assert_eq!(reloads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_user_scope_never_copies_the_agents_shared_login() {
        let (vault, _dir) = temp_vault();
        vault.set(AUTH_VAULT_KEY, &login()).unwrap();
        let (tool, reloads) = tool(Arc::clone(&vault));
        let result = as_user(BOB, tool.execute(serde_json::json!({})))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("Nothing stored"),
            "{:?}",
            result.error
        );
        assert!(vault.get(&per_user_key(BOB, AUTH_VAULT_KEY)).is_none());
        assert_eq!(reloads.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_sign_in_already_stored_elsewhere_is_refused() {
        let (vault, _dir) = temp_vault();
        vault.set(AUTH_VAULT_KEY, &login()).unwrap();
        vault
            .set(&per_user_key(BOB, "codex/staged"), &login())
            .unwrap();
        let (tool, _) = tool(Arc::clone(&vault));
        let result = as_user(
            BOB,
            tool.execute(serde_json::json!({ "vault_path": "codex/staged" })),
        )
        .await
        .unwrap();
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("already stored"),
            "{:?}",
            result.error
        );
        assert!(vault.get(&per_user_key(BOB, AUTH_VAULT_KEY)).is_none());
    }

    #[tokio::test]
    async fn only_the_owner_or_an_admin_sets_the_shared_login() {
        let (vault, _dir) = temp_vault();
        vault.set("codex/staged", &login()).unwrap();
        let shared = serde_json::json!({ "vault_path": "codex/staged", "scope": "agent_shared" });

        let (tool, _) = tool(Arc::clone(&vault));
        let refused = as_user(BOB, tool.execute(shared.clone())).await.unwrap();
        assert!(!refused.success);
        assert!(
            refused
                .error
                .as_deref()
                .unwrap_or("")
                .contains("owner or an admin"),
            "{:?}",
            refused.error
        );
        assert!(vault.get(AUTH_VAULT_KEY).is_none());

        let owner = as_user(OWNER, tool.execute(shared.clone())).await.unwrap();
        assert!(owner.success, "{:?}", owner.error);

        vault.delete(AUTH_VAULT_KEY).unwrap();
        let (admin_tool, _) = tool_with(Arc::clone(&vault), true);
        let admin = as_user(BOB, admin_tool.execute(shared)).await.unwrap();
        assert!(admin.success, "{:?}", admin.error);
    }

    #[tokio::test]
    async fn a_missing_path_says_where_to_store_the_file() {
        let (vault, _dir) = temp_vault();
        let (tool, reloads) = tool(Arc::clone(&vault));
        let result = as_user(
            BOB,
            tool.execute(serde_json::json!({ "vault_path": "codex/nowhere" })),
        )
        .await
        .unwrap();
        assert!(!result.success);
        let err = result.error.as_deref().unwrap_or("");
        assert!(
            err.contains("codex/nowhere") && err.contains("Vault page"),
            "{err}"
        );
        assert_eq!(reloads.load(Ordering::SeqCst), 0);
    }
}
