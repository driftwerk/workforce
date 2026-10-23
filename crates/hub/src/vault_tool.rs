//! Vault tools — agent-callable tools for secrets management.
//!
//! Tools: `vault.get`, `vault.set`, `vault.list`, `vault.delete`,
//! `vault.inspect`, `vault.my_credentials`, `vault.move`, `vault.rename`,
//! `vault.copy`. The last three reorganise existing credentials;
//! `vault.rename`/`vault.copy` require the SystemAdmin role (see
//! [`IsAdminFn`]) and `vault.copy` shares its fan-out engine with the
//! `/api/agents/{id}/vault/copy` HTTP handler via [`crate::vault_copy`].
//!
//! Two policy layers sit between the LLM and the underlying vault:
//!
//! 1. **Cross-user / guest-namespace gate** ([`crate::vault_policy::agent_can_read`]):
//!    rejects reads of `wasm-skills/*` (skill-private) and other-user
//!    `users/*` paths, and is also applied to `vault.list` results. Used
//!    by every tool here.
//! 2. **Output redaction** ([`crate::redacting_vault::RedactionPolicy`]):
//!    when enabled (`[vault] redaction = "redact"` in cloud mode), the
//!    value returned to the LLM is `abcd…1234` rather than cleartext.
//!    Built-in adapters and WASM skills bypass this layer — they read
//!    the raw vault to construct HTTP `Authorization` headers.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::settings::{SecretStore, VaultActor, VaultEntry, VaultSource};

use crate::hub_core::logins::store::{refuse_copy, LoginRefusal, LoginWrites};
use crate::redacting_vault::{Disclosure, PathMatcher, RedactionMode};
use crate::vault_audit::value_fingerprint;
use crate::vault_copy::{
    copied_count, copy_credential, copy_outcome_json, dedup_destinations, logical_path_error,
    CopyRequest,
};
use crate::vault_policy::{agent_can_read, agent_can_read_value, AgentReadDenial};

pub use crate::authz::{always_admin, never_admin, IsAdminFn};

/// Closure that resolves a `VaultEntry::created_by`/`updated_by` actor tag
/// (e.g. `user:<uuid>`, `cli`, `system:slack_token_sync`) to a human-readable
/// display name. Returns `None` for actor tags it can't enrich; callers
/// substitute a sensible default at render time.
pub type OwnerResolverFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Default `OwnerResolverFn` that returns `None` for every actor tag.
pub fn no_owner_resolver() -> OwnerResolverFn {
    Arc::new(|_| None)
}

/// Output redaction policy applied to the value field returned by
/// `vault.get` / `vault.inspect`. Cheap to clone — the matcher is `Arc`'d.
#[derive(Clone)]
pub struct RedactionPolicy {
    mode: RedactionMode,
    non_secret_paths: Arc<PathMatcher>,
}

impl RedactionPolicy {
    pub fn new(mode: RedactionMode, non_secret_paths: Arc<PathMatcher>) -> Self {
        Self {
            mode,
            non_secret_paths,
        }
    }

    /// Default cloud configuration: `Redact` mode with the built-in
    /// non-secret allowlist.
    pub fn default_cloud() -> Self {
        Self::new(
            RedactionMode::Redact,
            Arc::new(PathMatcher::default_non_secret()),
        )
    }

    /// Single-tenant CLI configuration: cleartext, no redaction.
    pub fn off() -> Self {
        Self::new(RedactionMode::Off, Arc::new(PathMatcher::default()))
    }

    /// Apply this policy to a freshly-read value. Returns:
    /// - `Some((value, false))` for cleartext.
    /// - `Some((redacted, true))` when redaction was applied.
    /// - `None` when `Deny` mode hides the value entirely.
    pub fn apply(&self, path: &str, value: String) -> Option<(String, bool)> {
        let disclosure = if self.non_secret_paths.matches(path) {
            Disclosure::Cleartext
        } else {
            match self.mode {
                RedactionMode::Off => Disclosure::Cleartext,
                RedactionMode::Redact => Disclosure::Redacted,
                RedactionMode::Deny => Disclosure::Denied,
            }
        };
        match disclosure {
            Disclosure::Cleartext => Some((value, false)),
            Disclosure::Redacted => Some((crate::redacting_vault::redact(&value), true)),
            Disclosure::Denied => None,
        }
    }
}

/// Render an [`AgentReadDenial`] as a `ToolResult::failure` payload.
fn deny_to_result(start: Instant, path: &str, denial: AgentReadDenial) -> ToolResult {
    tracing::warn!(
        target: "sentinel.audit",
        vault_path = %path,
        denial = %denial,
        "agent vault tool rejected: cross-user / guest-private read"
    );
    ToolResult::failure(
        format!("vault path '{path}' is not accessible: {denial}"),
        start.elapsed().as_millis() as u64,
    )
}

pub use crate::hub_core::vault_scopes::VaultScope;
use crate::user_config::current_identity_id as current_uid;

/// The guarded writer a login key at `path` goes through, when it is one: a
/// tool built without one refuses it.
fn login_writes<'a>(
    logins: Option<&'a LoginWrites>,
    path: &str,
) -> Option<std::result::Result<&'a LoginWrites, String>> {
    workforce_common::subscription::SubscriptionKind::of_vault_key(path).map(|kind| {
        logins.ok_or_else(|| {
            format!(
                "'{path}' holds a {} login, which this vault surface can't store; use claude_code.setup_token, codex.setup_auth or My Settings",
                kind.label()
            )
        })
    })
}

const SCOPE_ENUM: &[&str] = &[
    "agent_per_user",
    "global_per_user",
    "agent_specific",
    "global",
    "user",
    "agent_shared",
];

const SCOPE_DESCRIPTION: &str = "Tier where the secret lives. The four \
    fixed tiers are: 'agent_per_user' (this agent only, this user only — \
    users/{uid}/<path> in the agent vault), 'global_per_user' (any agent, \
    this user only — users/{uid}/<path> in the global vault, default for \
    writes when a user identity is present), 'agent_specific' (this agent \
    only, all users — bare <path> in the agent vault) and 'global' (any \
    agent, all users — bare <path> in the global vault, admin role required \
    to write). Legacy aliases: 'user' = 'global_per_user', 'agent_shared' = \
    'agent_specific'.";

// ---------------------------------------------------------------------------
// vault.get
// ---------------------------------------------------------------------------

pub struct VaultGetTool {
    store: Arc<dyn SecretStore>,
    redaction: RedactionPolicy,
}

impl VaultGetTool {
    /// Construct with no redaction (cleartext). Single-tenant / CLI use.
    pub fn new(store: Arc<dyn SecretStore>) -> Self {
        Self {
            store,
            redaction: RedactionPolicy::off(),
        }
    }

    /// Construct with an explicit redaction policy. Cloud deployments use
    /// `RedactionPolicy::default_cloud()`.
    pub fn with_redaction(store: Arc<dyn SecretStore>, redaction: RedactionPolicy) -> Self {
        Self { store, redaction }
    }
}

#[derive(Deserialize)]
struct VaultGetInput {
    path: String,
}

#[async_trait]
impl Tool for VaultGetTool {
    fn name(&self) -> &str {
        "vault.get"
    }

    fn description(&self) -> &str {
        "Retrieve a secret from the vault. Resolves through the four-tier \
         cascade (agent_per_user → global_per_user → agent_specific → \
         global), returning the most-specific match. Use vault.inspect to \
         see all tiers that have a value at a given path."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Dot/slash-notation path to the secret (e.g. 'skills/jira/api_token', 'mcp/v1/hubspot/client_secret')"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: VaultGetInput = serde_json::from_value(input)?;

        let uid = current_uid();
        if let Err(denial) = agent_can_read_value(&params.path, uid.as_deref()) {
            return Ok(deny_to_result(start, &params.path, denial));
        }

        let resolved = self.store.get_with_source(&params.path);
        match resolved {
            Some((raw, source)) => match self.redaction.apply(&params.path, raw) {
                Some((shown, redacted)) => Ok(ToolResult::success(
                    serde_json::json!({
                        "path": params.path,
                        "value": shown,
                        "redacted": redacted,
                        "tier": source.label(),
                    }),
                    start.elapsed().as_millis() as u64,
                )),
                None => Ok(ToolResult::failure(
                    format!(
                        "Secret '{}' is configured but redaction policy is 'deny'",
                        params.path
                    ),
                    start.elapsed().as_millis() as u64,
                )),
            },
            None => Ok(ToolResult::failure(
                format!("Secret '{}' not found in any tier", params.path),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// vault.set
// ---------------------------------------------------------------------------

pub struct VaultSetTool {
    store: Arc<dyn SecretStore>,
    is_admin: IsAdminFn,
    /// Invoked after a successful write to an `llm/accounts/*` path so the
    /// image-generation provider rebuilds against the new credential. See
    /// [`workforce_agent::tools::image_gen::ReloadImageTool`] for why this
    /// hook exists.
    reload_image_tool: workforce_agent::tools::image_gen::ReloadImageTool,
    logins: Option<LoginWrites>,
}

impl VaultSetTool {
    pub fn new(store: Arc<dyn SecretStore>) -> Self {
        Self {
            store,
            is_admin: always_admin(),
            reload_image_tool: workforce_agent::tools::image_gen::noop_reload_image_tool(),
            logins: None,
        }
    }

    pub fn with_admin_policy(mut self, is_admin: IsAdminFn) -> Self {
        self.is_admin = is_admin;
        self
    }

    pub fn with_logins(mut self, logins: LoginWrites) -> Self {
        self.logins = Some(logins);
        self
    }

    pub fn with_reload_image_tool(
        mut self,
        reload: workforce_agent::tools::image_gen::ReloadImageTool,
    ) -> Self {
        self.reload_image_tool = reload;
        self
    }
}

#[derive(Deserialize)]
struct VaultSetInput {
    path: String,
    value: String,
    /// When omitted, defaults to `global_per_user` when a current identity is
    /// in scope; system contexts (no identity) must specify a scope.
    #[serde(default)]
    scope: Option<VaultScope>,
}

#[async_trait]
impl Tool for VaultSetTool {
    fn name(&self) -> &str {
        "vault.set"
    }

    fn description(&self) -> &str {
        "Store a secret in the vault at an explicit tier. Defaults to \
         'global_per_user' when an active user identity is in scope. Returns \
         {tier, path, stored} on success, plus a 'shadow_warning' field when \
         the new write will eclipse an existing value at a lower-priority \
         tier — surface that warning to the user before assuming the write \
         is what they wanted."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Dot/slash-notation path for the secret (e.g. 'skills/jira/api_token', 'mcp/v1/hubspot/client_secret')"
                },
                "value": {
                    "type": "string",
                    "description": "The secret value to store"
                },
                "scope": {
                    "type": "string",
                    "enum": SCOPE_ENUM,
                    "description": SCOPE_DESCRIPTION
                }
            },
            "required": ["path", "value"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: VaultSetInput = serde_json::from_value(input)?;

        let uid = current_uid();
        if let Err(denial) = agent_can_read(&params.path, uid.as_deref()) {
            return Ok(deny_to_result(start, &params.path, denial));
        }

        if crate::vault_policy::is_org_wide(&params.path) {
            tracing::warn!(
                target: "sentinel.audit",
                vault_path = %params.path,
                scope = ?params.scope,
                "vault.set rejected: org-wide path must be set from hub CLI"
            );
            return Ok(ToolResult::failure(
                crate::vault_policy::org_wide_rejection_message(&params.path),
                start.elapsed().as_millis() as u64,
            ));
        }

        let scope = match params.scope {
            Some(s) => s,
            None => match uid.as_deref() {
                Some(_) => VaultScope::GlobalPerUser,
                None => {
                    return Ok(ToolResult::failure(
                        "vault.set defaults to per-user scope but no user identity is in scope. \
                         Pass scope='agent_specific' or scope='global' explicitly when running \
                         from a system context (workflow step, scheduled job).",
                        start.elapsed().as_millis() as u64,
                    ));
                }
            },
        };

        let tier = scope.to_tier();
        if tier.is_per_user() && crate::vault_policy::is_shared_only(&params.path) {
            tracing::warn!(
                target: "sentinel.audit",
                vault_path = %params.path,
                scope = ?params.scope,
                "vault.set rejected: shared-only path written per user"
            );
            return Ok(ToolResult::failure(
                crate::vault_policy::shared_only_rejection_message(&params.path),
                start.elapsed().as_millis() as u64,
            ));
        }
        if tier.requires_admin_to_write() && !(self.is_admin)() {
            return Ok(ToolResult::failure(
                format!(
                    "vault.set scope='global' requires admin role — '{}' is shared across \
                     every user of every agent. Ask an admin to set this via the UI \
                     (Vault page) or the hub CLI:\n  \
                     workforce vault set '{}' '<value>'",
                    params.path, params.path
                ),
                start.elapsed().as_millis() as u64,
            ));
        }

        let shadow_warning = build_shadow_warning(self.store.as_ref(), &params.path, tier);
        let actor = uid
            .as_deref()
            .map(VaultActor::User)
            .unwrap_or(VaultActor::System("vault_set_tool"));
        match login_writes(self.logins.as_ref(), &params.path) {
            None => self
                .store
                .set_at_tier(&params.path, &params.value, tier, actor)?,
            Some(Err(refused)) => {
                return Ok(ToolResult::failure(
                    refused,
                    start.elapsed().as_millis() as u64,
                ))
            }
            Some(Ok(logins)) => {
                if let Err(refused) = logins.set(&params.path, &params.value, tier, actor).await {
                    return Ok(ToolResult::failure(
                        refused.to_string(),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        }

        let effective_path = render_effective_path(&params.path, tier, uid.as_deref());
        if workforce_common::redact::contains_secret(&params.value) {
            tracing::info!(
                target: "sentinel.audit",
                vault_path = %effective_path,
                tier = tier.label(),
                "Credential stored in vault (value redacted)"
            );
        }
        if is_llm_account_path(&params.path) {
            (self.reload_image_tool)();
        }

        let mut out = serde_json::json!({
            "path": params.path,
            "effective_path": effective_path,
            "tier": tier.label(),
            "stored": true,
        });
        if let Some(warning) = shadow_warning {
            out["shadow_warning"] = warning;
        }
        Ok(ToolResult::success(out, start.elapsed().as_millis() as u64))
    }
}

fn render_effective_path(path: &str, tier: VaultSource, uid: Option<&str>) -> String {
    match (tier.is_per_user(), uid) {
        (true, Some(u)) => format!("users/{u}/{path}"),
        _ => path.to_string(),
    }
}

/// Build a JSON shadow warning describing every tier that already has a
/// value at `path` and that the impending write at `target_tier` will
/// eclipse (i.e. tiers with a higher numeric rank). Returns `None` when no
/// shadowing will occur.
fn build_shadow_warning(
    store: &dyn SecretStore,
    path: &str,
    target_tier: VaultSource,
) -> Option<serde_json::Value> {
    let eclipsed: Vec<serde_json::Value> = store
        .inspect(path)
        .into_iter()
        .filter(|(t, _)| t.rank() > target_tier.rank())
        .map(|(t, v)| {
            serde_json::json!({
                "tier": t.label(),
                "value_sha256_first8": value_fingerprint(&v),
            })
        })
        .collect();
    (!eclipsed.is_empty()).then(|| {
        serde_json::json!({
            "writing_tier": target_tier.label(),
            "eclipsed": eclipsed,
            "note": "The new value will resolve first; the eclipsed entries remain in place but won't be returned by vault.get until the higher-tier value is removed."
        })
    })
}

// ---------------------------------------------------------------------------
// vault.list
// ---------------------------------------------------------------------------

pub struct VaultListTool {
    store: Arc<dyn SecretStore>,
}

impl VaultListTool {
    pub fn new(store: Arc<dyn SecretStore>) -> Self {
        Self { store }
    }
}

#[derive(Deserialize)]
struct VaultListInput {
    #[serde(default)]
    prefix: Option<String>,
}

#[async_trait]
impl Tool for VaultListTool {
    fn name(&self) -> &str {
        "vault.list"
    }

    fn description(&self) -> &str {
        "List secret paths visible to the active identity across all four \
         vault tiers (never shows values). Returns logical paths — pass any \
         path back to vault.get or vault.inspect to read it. Use \
         vault.my_credentials for a richer per-tier table with owners."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prefix": {
                    "type": "string",
                    "description": "Optional path prefix filter (e.g. 'mcp/v1/hubspot/')"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: VaultListInput = serde_json::from_value(input)?;
        let uid = current_uid();
        let raw = self.store.list(params.prefix.as_deref());
        let paths: Vec<String> = raw
            .into_iter()
            .filter(|p| agent_can_read(p, uid.as_deref()).is_ok())
            .collect();
        Ok(ToolResult::success(
            serde_json::json!({"paths": paths, "count": paths.len()}),
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ---------------------------------------------------------------------------
// vault.delete
// ---------------------------------------------------------------------------

pub struct VaultDeleteTool {
    store: Arc<dyn SecretStore>,
    /// Admin gate. Deleting a shared (T3) / global (T4) secret requires the
    /// SystemAdmin role, mirroring `vault.set`'s write gate. Defaults to
    /// `always_admin` for single-tenant CLI use; `serve.rs` threads the real
    /// identity-backed policy in cloud mode.
    is_admin: IsAdminFn,
    /// Invoked after a successful delete on an `llm/accounts/*` path so the
    /// image-generation provider rebuilds (and unregisters itself if the only
    /// matching account was just removed).
    reload_image_tool: workforce_agent::tools::image_gen::ReloadImageTool,
    logins: Option<LoginWrites>,
}

impl VaultDeleteTool {
    pub fn new(store: Arc<dyn SecretStore>) -> Self {
        Self {
            store,
            is_admin: always_admin(),
            reload_image_tool: workforce_agent::tools::image_gen::noop_reload_image_tool(),
            logins: None,
        }
    }

    pub fn with_admin_policy(mut self, is_admin: IsAdminFn) -> Self {
        self.is_admin = is_admin;
        self
    }

    pub fn with_logins(mut self, logins: LoginWrites) -> Self {
        self.logins = Some(logins);
        self
    }

    pub fn with_reload_image_tool(
        mut self,
        reload: workforce_agent::tools::image_gen::ReloadImageTool,
    ) -> Self {
        self.reload_image_tool = reload;
        self
    }
}

/// Whether a write to `path` could change the credential the image-generation
/// provider reads. Image gen sweeps the root `llm/accounts/*` namespace (the
/// same one chat consults and `system.add_account` writes); per-user variants
/// under `users/{uid}/llm/accounts/*` aren't read, so we deliberately don't
/// match them — a per-user vault.set must not trigger an agent-wide rebuild.
fn is_llm_account_path(path: &str) -> bool {
    path.starts_with("llm/accounts/")
}

#[derive(Deserialize)]
struct VaultDeleteInput {
    path: String,
}

#[async_trait]
impl Tool for VaultDeleteTool {
    fn name(&self) -> &str {
        "vault.delete"
    }

    fn description(&self) -> &str {
        "Delete a secret from the vault by its dot-notation path. Per-user secrets live at users/{uid}/<path> — pass that exact path to delete one of your own user-scoped credentials."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Full path of the secret to delete. To delete a per-user secret use 'users/{uid}/<path>' (cross-user paths are rejected)."
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: VaultDeleteInput = serde_json::from_value(input)?;

        let uid = current_uid();
        if let Err(denial) = agent_can_read(&params.path, uid.as_deref()) {
            return Ok(deny_to_result(start, &params.path, denial));
        }

        // Org-wide secrets (the shared OAuth client) can only be managed from
        // the hub CLI — block deletes for everyone, exactly as `vault.set`
        // blocks writes.
        if crate::vault_policy::is_org_wide(&params.path) {
            tracing::warn!(
                target: "sentinel.audit",
                vault_path = %params.path,
                "vault.delete rejected: org-wide path must be managed from hub CLI"
            );
            return Ok(ToolResult::failure(
                crate::vault_policy::org_wide_rejection_message(&params.path),
                start.elapsed().as_millis() as u64,
            ));
        }

        // Admin gate for shared (T3) / global (T4) secrets. A delete targets
        // the most-specific tier that holds a value (same resolution as
        // `get_with_source`), so we gate on the *entry the delete will hit*:
        //  - an explicit `users/{uid}/…` path is the caller's own per-user
        //    entry (`agent_can_read` already proved the uid is theirs);
        //  - a bare logical path that resolves to a per-user tier is the
        //    caller's own entry too (the `vault.list` self-service flow
        //    returns the bare logical form);
        //  - anything else is a shared/global secret and requires admin.
        let resolved_is_per_user = self
            .store
            .get_with_source(&params.path)
            .map(|(_, src)| src.is_per_user())
            .unwrap_or(false);
        let is_own_per_user = params.path.starts_with("users/") || resolved_is_per_user;
        if !is_own_per_user && !(self.is_admin)() {
            tracing::warn!(
                target: "sentinel.audit",
                vault_path = %params.path,
                "vault.delete rejected: shared/global secret requires admin role"
            );
            return Ok(ToolResult::failure(
                format!(
                    "vault.delete '{}' targets a shared secret (visible to other users \
                     of this agent) — admin role required. Ask an admin to remove it via \
                     the UI (Vault page) or the hub CLI:\n  workforce vault delete '{}'",
                    params.path, params.path
                ),
                start.elapsed().as_millis() as u64,
            ));
        }

        let deleted = match login_writes(self.logins.as_ref(), &params.path) {
            None => self.store.delete(&params.path)?,
            Some(Err(refused)) => {
                return Ok(ToolResult::failure(
                    refused,
                    start.elapsed().as_millis() as u64,
                ))
            }
            Some(Ok(logins)) => {
                let actor = uid
                    .as_deref()
                    .map_or(VaultActor::System("vault_delete_tool"), VaultActor::User);
                match logins.delete(&params.path, actor).await {
                    Ok(deleted) => deleted,
                    Err(LoginRefusal::Vault(e)) => return Err(e),
                    Err(refused) => {
                        return Ok(ToolResult::failure(
                            refused.to_string(),
                            start.elapsed().as_millis() as u64,
                        ))
                    }
                }
            }
        };
        if deleted {
            if is_llm_account_path(&params.path) {
                (self.reload_image_tool)();
            }
            Ok(ToolResult::success(
                serde_json::json!({"path": params.path, "deleted": true}),
                start.elapsed().as_millis() as u64,
            ))
        } else {
            Ok(ToolResult::failure(
                format!("Secret '{}' not found", params.path),
                start.elapsed().as_millis() as u64,
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// vault.inspect
// ---------------------------------------------------------------------------

pub struct VaultInspectTool {
    store: Arc<dyn SecretStore>,
    redaction: RedactionPolicy,
}

impl VaultInspectTool {
    /// Construct with no redaction (cleartext). Single-tenant / CLI use.
    pub fn new(store: Arc<dyn SecretStore>) -> Self {
        Self {
            store,
            redaction: RedactionPolicy::off(),
        }
    }

    /// Construct with an explicit redaction policy. Cloud deployments use
    /// `RedactionPolicy::default_cloud()`.
    pub fn with_redaction(store: Arc<dyn SecretStore>, redaction: RedactionPolicy) -> Self {
        Self { store, redaction }
    }
}

#[derive(Deserialize)]
struct VaultInspectInput {
    path: String,
}

#[async_trait]
impl Tool for VaultInspectTool {
    fn name(&self) -> &str {
        "vault.inspect"
    }

    fn description(&self) -> &str {
        "Inspect a secret with full provenance: which vault tier resolved it, \
         which other tiers also hold a value (shadowing detection), and the \
         resolved entry's audit metadata. Use this when diagnosing surprising \
         vault behaviour — particularly to detect agent-vault entries that \
         shadow org-wide global-vault secrets."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Dot-notation path to inspect"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: VaultInspectInput = serde_json::from_value(input)?;

        // Cross-user / guest-namespace gate.
        let uid = current_uid();
        if let Err(denial) = agent_can_read_value(&params.path, uid.as_deref()) {
            return Ok(deny_to_result(start, &params.path, denial));
        }

        let resolved = self.store.get_with_source(&params.path);
        let all_tiers = self.store.inspect(&params.path);
        let entry = self.store.get_entry(&params.path);

        let resolved_source = resolved.as_ref().map(|(_, s)| *s);
        let shadowed_by: Vec<serde_json::Value> = all_tiers
            .iter()
            .filter(|(src, _)| Some(*src) != resolved_source)
            .map(|(src, value)| {
                serde_json::json!({
                    "source": src.label(),
                    "value_sha256_first8": value_fingerprint(value),
                })
            })
            .collect();

        // Apply redaction to the resolved value before returning to the LLM.
        // `shadowed_by` already uses fingerprints and never contains
        // cleartext, so it doesn't need re-processing here.
        let resolved_json = match resolved {
            Some((raw, source)) => match self.redaction.apply(&params.path, raw) {
                Some((shown, redacted)) => serde_json::json!({
                    "value": shown,
                    "redacted": redacted,
                    "source": source.label(),
                }),
                None => serde_json::json!({
                    "value": null,
                    "redacted": true,
                    "source": source.label(),
                    "note": "value hidden by 'deny' redaction policy",
                }),
            },
            None => serde_json::Value::Null,
        };

        let metadata_json = entry.map(|e| {
            serde_json::json!({
                "created_at": e.created_at,
                "updated_at": e.updated_at,
                "created_by": e.created_by,
                "updated_by": e.updated_by,
            })
        });

        Ok(ToolResult::success(
            serde_json::json!({
                "path": params.path,
                "resolved": resolved_json,
                "shadowed_by": shadowed_by,
                "metadata": metadata_json,
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ---------------------------------------------------------------------------
// vault.my_credentials
// ---------------------------------------------------------------------------

/// Read tool that returns every credential visible to the active identity,
/// grouped by tier, with owner display names resolved out of the identity
/// store. Designed for chat-table rendering ("show me my creds").
pub struct VaultMyCredentialsTool {
    store: Arc<dyn SecretStore>,
    owner_resolver: OwnerResolverFn,
}

impl VaultMyCredentialsTool {
    pub fn new(store: Arc<dyn SecretStore>, owner_resolver: OwnerResolverFn) -> Self {
        Self {
            store,
            owner_resolver,
        }
    }
}

#[async_trait]
impl Tool for VaultMyCredentialsTool {
    fn name(&self) -> &str {
        "vault.my_credentials"
    }

    fn description(&self) -> &str {
        "Return every vault entry visible to the active user, grouped by \
         tier (agent_per_user / global_per_user / agent_specific / global). \
         Each entry includes the logical path, value fingerprint, owner \
         display name, and timestamps — never the cleartext value. Use this \
         to render a credentials table for the user."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prefix": {
                    "type": "string",
                    "description": "Optional path prefix filter (e.g. 'mcp/v1/')"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let prefix = input
            .get("prefix")
            .and_then(|v| v.as_str().map(str::to_string));
        let uid = current_uid();

        let tiers: serde_json::Map<String, serde_json::Value> = VaultSource::all()
            .into_iter()
            .map(|tier| {
                let entries: Vec<serde_json::Value> = self
                    .store
                    .list_at_tier(prefix.as_deref(), tier)
                    .into_iter()
                    .filter(|(p, _)| !p.starts_with("wasm-skills/"))
                    .map(|(path, entry)| render_entry(path, entry, self.owner_resolver.as_ref()))
                    .collect();
                (tier.label().to_string(), serde_json::Value::Array(entries))
            })
            .collect();

        Ok(ToolResult::success(
            serde_json::json!({ "user_id": uid, "tiers": tiers }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

fn render_entry(
    path: String,
    entry: VaultEntry,
    owner_resolver: &(dyn Fn(&str) -> Option<String> + Send + Sync),
) -> serde_json::Value {
    serde_json::json!({
        "path": path,
        "fingerprint": value_fingerprint(&entry.value),
        "created_by": entry.created_by,
        "created_by_display": owner_resolver(&entry.created_by),
        "updated_by": entry.updated_by,
        "updated_by_display": owner_resolver(&entry.updated_by),
        "created_at": entry.created_at,
        "updated_at": entry.updated_at,
    })
}

// ---------------------------------------------------------------------------
// vault.move
// ---------------------------------------------------------------------------

/// Move a credential from one tier to another (delete-after-write, both
/// audited). User-tier ↔ user-tier moves are allowed for the active user on
/// their own paths; any move touching a shared tier (T3/T4) requires admin.
pub struct VaultMoveTool {
    store: Arc<dyn SecretStore>,
    is_admin: IsAdminFn,
    logins: Option<LoginWrites>,
}

impl VaultMoveTool {
    pub fn new(store: Arc<dyn SecretStore>, is_admin: IsAdminFn) -> Self {
        Self {
            store,
            is_admin,
            logins: None,
        }
    }

    pub fn with_logins(mut self, logins: LoginWrites) -> Self {
        self.logins = Some(logins);
        self
    }
}

#[derive(Deserialize)]
struct VaultMoveInput {
    path: String,
    from_tier: VaultScope,
    to_tier: VaultScope,
    #[serde(default)]
    overwrite: bool,
    #[serde(default)]
    confirm_share: bool,
}

#[async_trait]
impl Tool for VaultMoveTool {
    fn name(&self) -> &str {
        "vault.move"
    }

    fn description(&self) -> &str {
        "Move a credential between tiers. User-tier moves (agent_per_user ↔ \
         global_per_user) require only the active user. Any move involving a \
         shared tier (agent_specific or global) is admin-only and — when it \
         widens visibility (per-user → shared) — also requires \
         confirm_share=true. If the destination already has a value, rejects \
         unless overwrite=true."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Logical path of the credential (no users/ prefix)"
                },
                "from_tier": {
                    "type": "string",
                    "enum": SCOPE_ENUM,
                    "description": "Tier to move from"
                },
                "to_tier": {
                    "type": "string",
                    "enum": SCOPE_ENUM,
                    "description": "Tier to move to"
                },
                "overwrite": {
                    "type": "boolean",
                    "description": "Overwrite an existing value at the destination tier (default false)"
                },
                "confirm_share": {
                    "type": "boolean",
                    "description": "Required confirmation when moving a per-user credential to a shared tier"
                }
            },
            "required": ["path", "from_tier", "to_tier"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: VaultMoveInput = serde_json::from_value(input)?;
        let from = params.from_tier.to_tier();
        let to = params.to_tier.to_tier();
        let uid = current_uid();

        if from == to {
            return Ok(ToolResult::failure(
                "from_tier and to_tier are the same — no move needed",
                start.elapsed().as_millis() as u64,
            ));
        }
        if let Err(denial) = agent_can_read(&params.path, uid.as_deref()) {
            return Ok(deny_to_result(start, &params.path, denial));
        }
        if let Err(msg) = authorize_move(from, to, params.confirm_share, &self.is_admin) {
            return Ok(ToolResult::failure(msg, start.elapsed().as_millis() as u64));
        }

        let actor = uid
            .as_deref()
            .map(VaultActor::User)
            .unwrap_or(VaultActor::System("vault_move_tool"));
        match relocate(
            self.store.as_ref(),
            self.logins.as_ref(),
            (&params.path, from),
            (&params.path, to),
            params.overwrite,
            actor,
        )
        .await
        {
            Ok(Relocated {
                moved_value_fingerprint,
                overwritten_fingerprint,
            }) => Ok(ToolResult::success(
                serde_json::json!({
                    "path": params.path,
                    "from": from.label(),
                    "to": to.label(),
                    "moved_value_sha256_first8": moved_value_fingerprint,
                    "overwrote_destination": overwritten_fingerprint.is_some(),
                    "overwritten_value_sha256_first8": overwritten_fingerprint,
                }),
                start.elapsed().as_millis() as u64,
            )),
            Err(RelocateError::SourceMissing) => Ok(ToolResult::failure(
                format!(
                    "No value to move: '{}' has no entry at tier '{}'",
                    params.path,
                    from.label()
                ),
                start.elapsed().as_millis() as u64,
            )),
            Err(RelocateError::DestinationExists(existing_fp)) => Ok(ToolResult::failure(
                format!(
                    "Destination '{}' already has a value at tier '{}' (fingerprint {}). \
                     Pass overwrite=true to replace it.",
                    params.path,
                    to.label(),
                    existing_fp
                ),
                start.elapsed().as_millis() as u64,
            )),
            Err(RelocateError::Backend(msg)) => Err(eyre::eyre!(msg)),
            Err(RelocateError::Refused(refused)) => Ok(ToolResult::failure(
                refused,
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

/// Authorisation rule for a tier-to-tier move. Returns `Ok(())` when the
/// active identity may perform it.
fn authorize_move(
    from: VaultSource,
    to: VaultSource,
    confirm_share: bool,
    is_admin: &IsAdminFn,
) -> Result<(), String> {
    let touches_shared = !from.is_per_user() || !to.is_per_user();
    if touches_shared && !is_admin() {
        return Err(format!(
            "vault.move from '{}' to '{}' touches a shared tier — admin role required",
            from.label(),
            to.label()
        ));
    }
    let widens = from.is_per_user() && !to.is_per_user();
    if widens && !confirm_share {
        return Err(format!(
            "vault.move from '{}' to '{}' widens visibility from one user to a shared tier. \
             Pass confirm_share=true once the user has explicitly approved this change.",
            from.label(),
            to.label()
        ));
    }
    Ok(())
}

/// What [`relocate`] moved: the value's fingerprint, and that of any value
/// it replaced.
struct Relocated {
    moved_value_fingerprint: String,
    overwritten_fingerprint: Option<String>,
}

enum RelocateError {
    SourceMissing,
    DestinationExists(String),
    Backend(String),
    Refused(String),
}

/// Move a credential between two tier-addressed slots as one commit
/// (`SecretStore::move_at_tier`): `vault.move` changes the tier,
/// `vault.rename` the path. The store is synced first, so the value moved and
/// the destination check see every process's writes. Without `overwrite`, an
/// occupied destination leaves both slots untouched.
async fn relocate(
    store: &dyn SecretStore,
    logins: Option<&LoginWrites>,
    from: (&str, VaultSource),
    to: (&str, VaultSource),
    overwrite: bool,
    actor: VaultActor<'_>,
) -> Result<Relocated, RelocateError> {
    store
        .sync()
        .map_err(|e| RelocateError::Backend(e.to_string()))?;
    let source_value = store
        .get_at_tier(from.0, from.1)
        .ok_or(RelocateError::SourceMissing)?;
    let existing = store.get_at_tier(to.0, to.1);
    if let (Some(existing), false) = (&existing, overwrite) {
        return Err(RelocateError::DestinationExists(value_fingerprint(
            existing,
        )));
    }
    match login_writes(logins, from.0).or_else(|| login_writes(logins, to.0)) {
        None => store
            .move_at_tier(from, to, &source_value, actor)
            .map_err(|e| RelocateError::Backend(e.to_string()))?,
        Some(Err(refused)) => return Err(RelocateError::Refused(refused)),
        Some(Ok(logins)) => logins
            .relocate(from, to, &source_value, actor)
            .await
            .map_err(|refused| RelocateError::Refused(refused.to_string()))?,
    }
    Ok(Relocated {
        moved_value_fingerprint: value_fingerprint(&source_value),
        overwritten_fingerprint: existing.as_deref().map(value_fingerprint),
    })
}

// ---------------------------------------------------------------------------
// vault.rename
// ---------------------------------------------------------------------------

/// Rename a credential within a single tier (`path` → `new_path`, same tier).
/// Admin-only: renaming reorganises shared and per-user credentials alike, so
/// it requires the SystemAdmin role — matching the UI, where rename flows
/// through the admin-gated `vault/move` endpoint. The value never moves tiers
/// and is never returned to the LLM.
pub struct VaultRenameTool {
    store: Arc<dyn SecretStore>,
    is_admin: IsAdminFn,
    logins: Option<LoginWrites>,
}

impl VaultRenameTool {
    pub fn new(store: Arc<dyn SecretStore>, is_admin: IsAdminFn) -> Self {
        Self {
            store,
            is_admin,
            logins: None,
        }
    }

    pub fn with_logins(mut self, logins: LoginWrites) -> Self {
        self.logins = Some(logins);
        self
    }
}

#[derive(Deserialize)]
struct VaultRenameInput {
    path: String,
    new_path: String,
    tier: VaultScope,
    /// Owner of the credential when `tier` is per-user. Defaults to the active
    /// user; an admin may name another user to rename that user's credential.
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    overwrite: bool,
}

#[async_trait]
impl Tool for VaultRenameTool {
    fn name(&self) -> &str {
        "vault.rename"
    }

    fn description(&self) -> &str {
        "Rename a credential within the same tier (path → new_path). System \
         admin role required. Specify the tier the credential lives in; for a \
         per-user tier the rename applies to the active user unless you pass \
         user_id to target another user. If the destination path already has a \
         value in that tier, rejects unless overwrite=true. To move a \
         credential to a different tier instead, use vault.move."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Current logical path of the credential (no users/ prefix)"
                },
                "new_path": {
                    "type": "string",
                    "description": "New logical path (no users/ prefix)"
                },
                "tier": {
                    "type": "string",
                    "enum": SCOPE_ENUM,
                    "description": SCOPE_DESCRIPTION
                },
                "user_id": {
                    "type": "string",
                    "description": "For a per-user tier, the owner whose credential is renamed (defaults to the active user)"
                },
                "overwrite": {
                    "type": "boolean",
                    "description": "Overwrite an existing value at new_path in this tier (default false)"
                }
            },
            "required": ["path", "new_path", "tier"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: VaultRenameInput = serde_json::from_value(input)?;
        let elapsed = || start.elapsed().as_millis() as u64;

        if !(self.is_admin)() {
            return Ok(ToolResult::failure(
                "vault.rename requires the system admin role".to_string(),
                elapsed(),
            ));
        }
        if params.path == params.new_path {
            return Ok(ToolResult::failure(
                "path and new_path are the same — nothing to rename".to_string(),
                elapsed(),
            ));
        }
        for p in [params.path.as_str(), params.new_path.as_str()] {
            if let Some(msg) = logical_path_error(p) {
                return Ok(ToolResult::failure(
                    format!("invalid path '{p}': {msg}"),
                    elapsed(),
                ));
            }
            // Both endpoints go through the same namespace gate the read/write
            // tools use. Without it, a rename could move an agent-writable path
            // INTO a server-resolved credential namespace (`databases/`,
            // `http/`) and so pair an operator's credential with an
            // agent-chosen target — defeating a boundary the rest of the vault
            // surface enforces.
            if let Err(denial) = agent_can_read(p, current_uid().as_deref()) {
                return Ok(ToolResult::failure(
                    format!("cannot rename '{p}': {denial}"),
                    elapsed(),
                ));
            }
        }

        let tier = params.tier.to_tier();
        // Per-user tiers resolve under a user scope; default to the acting
        // admin's own identity when no explicit owner is given.
        let scope_uid = if tier.is_per_user() {
            params.user_id.clone().or_else(current_uid)
        } else {
            None
        };
        if tier.is_per_user() && scope_uid.is_none() {
            return Ok(ToolResult::failure(
                "user_id is required to rename a per-user credential when no user identity is in scope"
                    .to_string(),
                elapsed(),
            ));
        }

        // The audit trail records *who* renamed it (the acting identity), not
        // the owner of the per-user slot.
        let actor_id = current_uid();
        let actor = actor_id
            .as_deref()
            .map(VaultActor::User)
            .unwrap_or(VaultActor::System("vault_rename_tool"));

        let relocation = relocate(
            self.store.as_ref(),
            self.logins.as_ref(),
            (&params.path, tier),
            (&params.new_path, tier),
            params.overwrite,
            actor,
        );
        let outcome = match scope_uid {
            Some(uid) => {
                workforce_agent::runner::CURRENT_USER_ID
                    .scope(uid, relocation)
                    .await
            }
            None => relocation.await,
        };

        match outcome {
            Ok(Relocated {
                moved_value_fingerprint,
                overwritten_fingerprint,
            }) => Ok(ToolResult::success(
                serde_json::json!({
                    "path": params.path,
                    "new_path": params.new_path,
                    "tier": tier.label(),
                    "renamed": true,
                    "moved_value_sha256_first8": moved_value_fingerprint,
                    "overwrote_destination": overwritten_fingerprint.is_some(),
                    "overwritten_value_sha256_first8": overwritten_fingerprint,
                }),
                elapsed(),
            )),
            Err(RelocateError::SourceMissing) => Ok(ToolResult::failure(
                format!(
                    "No value to rename: '{}' has no entry at tier '{}'",
                    params.path,
                    tier.label()
                ),
                elapsed(),
            )),
            Err(RelocateError::DestinationExists(existing_fp)) => Ok(ToolResult::failure(
                format!(
                    "Destination '{}' already has a value at tier '{}' (fingerprint {}). \
                     Pass overwrite=true to replace it.",
                    params.new_path,
                    tier.label(),
                    existing_fp
                ),
                elapsed(),
            )),
            Err(RelocateError::Backend(msg)) => Err(eyre::eyre!(msg)),
            Err(RelocateError::Refused(refused)) => Ok(ToolResult::failure(refused, elapsed())),
        }
    }
}

// ---------------------------------------------------------------------------
// vault.copy
// ---------------------------------------------------------------------------

/// Copy a credential into one or more users' per-user slots, leaving the source
/// intact. Admin-only — propagating a credential across users is the same
/// high-impact action the UI restricts to system admins (the agent identity is
/// always attributable, so `is_admin()` is sufficient; no separate named-admin
/// check is needed). The cleartext value never appears in the result.
pub struct VaultCopyTool {
    store: Arc<dyn SecretStore>,
    is_admin: IsAdminFn,
}

impl VaultCopyTool {
    pub fn new(store: Arc<dyn SecretStore>, is_admin: IsAdminFn) -> Self {
        Self { store, is_admin }
    }
}

#[derive(Deserialize)]
struct VaultCopyInput {
    path: String,
    from_tier: VaultScope,
    to_tier: VaultScope,
    /// Source owner when `from_tier` is per-user. Defaults to the active user.
    #[serde(default)]
    from_user: Option<String>,
    to_users: Vec<String>,
    #[serde(default)]
    overwrite: bool,
}

#[async_trait]
impl Tool for VaultCopyTool {
    fn name(&self) -> &str {
        "vault.copy"
    }

    fn description(&self) -> &str {
        "Copy a credential into one or more users' per-user slots, leaving the \
         source untouched. System admin role required. The destination tier \
         must be per-user (agent_per_user or global_per_user). For a per-user \
         source, from_user names the owner to copy from (defaults to the active \
         user). Existing destination values are kept unless overwrite=true. \
         Returns a per-user status (copied / exists / source / error); the \
         secret value is never returned."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Logical path of the credential (no users/ prefix)"
                },
                "from_tier": {
                    "type": "string",
                    "enum": SCOPE_ENUM,
                    "description": "Tier to copy from"
                },
                "to_tier": {
                    "type": "string",
                    "enum": SCOPE_ENUM,
                    "description": "Destination tier — must be per-user (agent_per_user or global_per_user)"
                },
                "from_user": {
                    "type": "string",
                    "description": "Source owner when from_tier is per-user (defaults to the active user)"
                },
                "to_users": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Destination user identity UUIDs to copy the credential to"
                },
                "overwrite": {
                    "type": "boolean",
                    "description": "Overwrite a destination user's existing value (default false)"
                }
            },
            "required": ["path", "from_tier", "to_tier", "to_users"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: VaultCopyInput = serde_json::from_value(input)?;
        let elapsed = || start.elapsed().as_millis() as u64;

        if !(self.is_admin)() {
            return Ok(ToolResult::failure(
                "vault.copy requires the system admin role".to_string(),
                elapsed(),
            ));
        }

        let from = params.from_tier.to_tier();
        let to = params.to_tier.to_tier();
        if !to.is_per_user() {
            return Ok(ToolResult::failure(
                "destination tier must be per-user (agent_per_user or global_per_user)".to_string(),
                elapsed(),
            ));
        }
        if let Some(msg) = logical_path_error(&params.path) {
            return Ok(ToolResult::failure(
                format!("invalid path '{}': {msg}", params.path),
                elapsed(),
            ));
        }
        if let Err(refused) = refuse_copy(&params.path) {
            return Ok(ToolResult::failure(refused.to_string(), elapsed()));
        }
        // Same namespace gate as the read/write tools: a server-resolved
        // credential (`databases/`, `http/`) is not copyable from an agent tool.
        if let Err(denial) = agent_can_read(&params.path, current_uid().as_deref()) {
            return Ok(ToolResult::failure(
                format!("cannot copy '{}': {denial}", params.path),
                elapsed(),
            ));
        }

        // A per-user source needs an owner to read from; default to the active
        // user so "copy my credential to others" needs no extra argument.
        let from_user = if from.is_per_user() {
            match params.from_user.clone().or_else(current_uid) {
                Some(u) => Some(u),
                None => {
                    return Ok(ToolResult::failure(
                        "from_user is required when from_tier is per-user and no user identity is in scope"
                            .to_string(),
                        elapsed(),
                    ));
                }
            }
        } else {
            None
        };

        let to_users = dedup_destinations(&params.to_users);
        if to_users.is_empty() {
            return Ok(ToolResult::failure(
                "to_users must contain at least one user".to_string(),
                elapsed(),
            ));
        }

        let req = CopyRequest {
            path: &params.path,
            from_tier: from,
            to_tier: to,
            from_user: from_user.as_deref(),
            to_users: &to_users,
            overwrite: params.overwrite,
        };
        let Some(outcomes) = copy_credential(self.store.as_ref(), &req) else {
            return Ok(ToolResult::failure(
                "no value at the source tier for the given user".to_string(),
                elapsed(),
            ));
        };

        Ok(ToolResult::success(
            serde_json::json!({
                "path": params.path,
                "from_tier": from.label(),
                "to_tier": to.label(),
                "source_user": from_user,
                "copied": copied_count(&outcomes),
                "results": outcomes.iter().map(copy_outcome_json).collect::<Vec<_>>(),
            }),
            elapsed(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_config::LayeredUserVault;
    use crate::vault::Vault;
    use workforce_agent::runner::CURRENT_USER_ID;

    fn new_vault() -> Arc<Vault> {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("vault.enc");
        std::mem::forget(tmp);
        Arc::new(Vault::open(&path, "test-key").unwrap())
    }

    fn single_vault_store(vault: Arc<Vault>) -> Arc<dyn SecretStore> {
        Arc::new(LayeredUserVault::new(vault, None))
    }

    fn layered_store(agent: Arc<Vault>, global: Arc<Vault>) -> Arc<dyn SecretStore> {
        Arc::new(LayeredUserVault::new(agent, Some(global)))
    }

    async fn run_set_on(
        store: Arc<dyn SecretStore>,
        path: &str,
        value: &str,
        scope: Option<&str>,
    ) -> ToolResult {
        let tool = VaultSetTool::new(store);
        let mut input = serde_json::json!({"path": path, "value": value});
        if let Some(s) = scope {
            input["scope"] = serde_json::Value::String(s.to_string());
        }
        tool.execute(input).await.unwrap()
    }

    async fn run_set(
        vault: Arc<Vault>,
        path: &str,
        value: &str,
        scope: Option<&str>,
    ) -> ToolResult {
        run_set_on(single_vault_store(vault), path, value, scope).await
    }

    async fn run_get_on(
        store: Arc<dyn SecretStore>,
        path: &str,
        redaction: RedactionPolicy,
    ) -> ToolResult {
        let tool = VaultGetTool::with_redaction(store, redaction);
        tool.execute(serde_json::json!({"path": path}))
            .await
            .unwrap()
    }

    async fn run_get(vault: Arc<Vault>, path: &str, redaction: RedactionPolicy) -> ToolResult {
        run_get_on(single_vault_store(vault), path, redaction).await
    }

    /// A store that sees a write another process made only once it syncs.
    struct Lagging {
        store: Arc<dyn SecretStore>,
        unseen: parking_lot::Mutex<Option<(&'static str, &'static str)>>,
    }

    impl Lagging {
        fn new(store: Arc<dyn SecretStore>, path: &'static str, value: &'static str) -> Self {
            Self {
                store,
                unseen: parking_lot::Mutex::new(Some((path, value))),
            }
        }
    }

    impl SecretStore for Lagging {
        fn get(&self, path: &str) -> Option<String> {
            self.store.get(path)
        }

        fn set(&self, path: &str, value: &str) -> Result<()> {
            self.store.set(path, value)
        }

        fn delete(&self, path: &str) -> Result<bool> {
            self.store.delete(path)
        }

        fn list(&self, prefix: Option<&str>) -> Vec<String> {
            self.store.list(prefix)
        }

        fn inspect(&self, path: &str) -> Vec<(VaultSource, String)> {
            self.store.inspect(path)
        }

        fn move_at_tier(
            &self,
            from: (&str, VaultSource),
            to: (&str, VaultSource),
            value: &str,
            actor: VaultActor<'_>,
        ) -> Result<()> {
            self.store.move_at_tier(from, to, value, actor)
        }

        fn sync(&self) -> Result<()> {
            self.unseen
                .lock()
                .take()
                .map_or(Ok(()), |(path, value)| self.store.set(path, value))
        }
    }

    #[tokio::test]
    async fn a_move_carries_the_value_another_process_just_wrote() {
        let (agent, global) = (new_vault(), new_vault());
        agent.set("skills/x/token", "old").unwrap();
        let store = Lagging::new(
            layered_store(Arc::clone(&agent), Arc::clone(&global)),
            "skills/x/token",
            "new",
        );

        assert!(relocate(
            &store,
            None,
            ("skills/x/token", VaultSource::AgentSpecific),
            ("skills/x/token", VaultSource::Global),
            false,
            VaultActor::Cli,
        )
        .await
        .is_ok());
        assert!(agent.get("skills/x/token").is_none());
        assert_eq!(global.get("skills/x/token").as_deref(), Some("new"));
    }

    #[tokio::test]
    async fn a_rename_sees_a_destination_another_process_just_created() {
        let vault = new_vault();
        vault.set("skills/x/token", "mine").unwrap();
        let store = Lagging::new(
            single_vault_store(Arc::clone(&vault)),
            "skills/y/token",
            "theirs",
        );

        assert!(matches!(
            relocate(
                &store,
                None,
                ("skills/x/token", VaultSource::AgentSpecific),
                ("skills/y/token", VaultSource::AgentSpecific),
                false,
                VaultActor::Cli,
            )
            .await,
            Err(RelocateError::DestinationExists(_))
        ));
        assert_eq!(vault.get("skills/x/token").as_deref(), Some("mine"));
        assert_eq!(vault.get("skills/y/token").as_deref(), Some("theirs"));
    }

    #[tokio::test]
    async fn vault_get_redacts_when_policy_is_redact() {
        let v = new_vault();
        v.set("skills/jira/api_token", "supersecret-1234567890")
            .unwrap();
        let res = run_get(
            Arc::clone(&v),
            "skills/jira/api_token",
            RedactionPolicy::default_cloud(),
        )
        .await;
        assert!(res.success);
        let out = res.output;
        assert_eq!(out["value"], "supe…7890");
        assert_eq!(out["redacted"], true);
    }

    #[tokio::test]
    async fn vault_get_passes_cleartext_when_policy_is_off() {
        let v = new_vault();
        v.set("skills/jira/api_token", "supersecret-1234567890")
            .unwrap();
        let res = run_get(
            Arc::clone(&v),
            "skills/jira/api_token",
            RedactionPolicy::off(),
        )
        .await;
        let out = res.output;
        assert_eq!(out["value"], "supersecret-1234567890");
        assert_eq!(out["redacted"], false);
    }

    #[tokio::test]
    async fn vault_get_passes_non_secret_paths_cleartext_under_redaction() {
        let v = new_vault();
        v.set("skills/jira/default_workspace", "acme").unwrap();
        let res = run_get(
            Arc::clone(&v),
            "skills/jira/default_workspace",
            RedactionPolicy::default_cloud(),
        )
        .await;
        let out = res.output;
        assert_eq!(out["value"], "acme");
        assert_eq!(out["redacted"], false);
    }

    #[tokio::test]
    async fn vault_get_never_reads_back_a_login_in_any_redaction_mode() {
        let v = new_vault();
        let logins = [
            "claude_code/oauth_token",
            "codex/auth_json",
            "users/alice/claude_code/oauth_token",
            "users/alice/codex/auth_json",
        ];
        for path in logins {
            v.set(path, concat!("sk-ant", "-oat01-or-a-refresh-token"))
                .unwrap();
        }
        for path in logins {
            for mode in [
                RedactionMode::Off,
                RedactionMode::Redact,
                RedactionMode::Deny,
            ] {
                let policy = RedactionPolicy::new(mode, Arc::new(PathMatcher::new([path.into()])));
                let input = serde_json::json!({ "path": path });
                let store = single_vault_store(Arc::clone(&v));
                let (got, inspected) = CURRENT_USER_ID
                    .scope("alice".to_string(), async {
                        (
                            VaultGetTool::with_redaction(Arc::clone(&store), policy.clone())
                                .execute(input.clone())
                                .await
                                .unwrap(),
                            VaultInspectTool::with_redaction(store, policy)
                                .execute(input)
                                .await
                                .unwrap(),
                        )
                    })
                    .await;
                for res in [got, inspected] {
                    assert!(!res.success, "{path} under {mode:?}");
                    assert!(
                        res.error.unwrap().contains("subscription login"),
                        "{path} under {mode:?}"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn vault_get_blocks_wasm_skills_namespace() {
        let v = new_vault();
        // Even if the value somehow exists, the LLM cannot see it.
        v.set("wasm-skills/orbit/users/alice/api_key", "deadbeef")
            .unwrap();
        let res = run_get(
            Arc::clone(&v),
            "wasm-skills/orbit/users/alice/api_key",
            RedactionPolicy::default_cloud(),
        )
        .await;
        assert!(!res.success, "should be denied");
        let err = res.error.unwrap();
        assert!(err.contains("wasm-skills") || err.contains("not accessible"));
    }

    #[tokio::test]
    async fn vault_get_blocks_other_user_path() {
        let v = new_vault();
        v.set("users/alice/skills/jira/api_token", "alice-token-xxxxxxxx")
            .unwrap();
        // Bob is the active user; he must not see Alice's token.
        let res = CURRENT_USER_ID
            .scope("bob".to_string(), async {
                run_get(
                    Arc::clone(&v),
                    "users/alice/skills/jira/api_token",
                    RedactionPolicy::default_cloud(),
                )
                .await
            })
            .await;
        assert!(!res.success);
    }

    #[tokio::test]
    async fn vault_get_allows_self_user_path() {
        let v = new_vault();
        v.set("users/alice/skills/jira/api_token", "alice-token-xxxxxxxx")
            .unwrap();
        let res = CURRENT_USER_ID
            .scope("alice".to_string(), async {
                run_get(
                    Arc::clone(&v),
                    "users/alice/skills/jira/api_token",
                    RedactionPolicy::default_cloud(),
                )
                .await
            })
            .await;
        assert!(res.success);
        let out = res.output;
        assert_eq!(out["value"], "alic…xxxx");
    }

    #[tokio::test]
    async fn vault_list_returns_logical_paths_filtered_by_identity() {
        // The layered store returns logical paths (with the per-user
        // `users/<uid>/` segment stripped) so the agent's prompt-side
        // reasoning uses the same shape it would pass to vault.get. Each
        // per-user tier already filters by active identity at list time,
        // so Alice's entries never appear in Bob's listing.
        let v = new_vault();
        v.set("users/alice/skills/jira/api_token", "alice-1")
            .unwrap();
        v.set("users/bob/skills/jira/api_token", "bob-1").unwrap();
        v.set("skills/jira/default_workspace", "acme").unwrap();
        v.set("wasm-skills/orbit/users/alice/api_key", "ck")
            .unwrap();

        let tool = VaultListTool::new(single_vault_store(Arc::clone(&v)));

        let res = CURRENT_USER_ID
            .scope("bob".to_string(), async {
                tool.execute(serde_json::json!({})).await.unwrap()
            })
            .await;
        assert!(res.success);
        let paths: Vec<String> = serde_json::from_value(res.output["paths"].clone()).unwrap();
        assert!(
            paths.contains(&"skills/jira/api_token".to_string()),
            "Bob's per-user jira token should appear as a logical path"
        );
        assert!(paths.contains(&"skills/jira/default_workspace".to_string()));
        assert!(
            !paths.iter().any(|p| p.contains("alice")),
            "Alice's per-user entries must not leak into Bob's listing"
        );
        assert!(!paths.iter().any(|p| p.starts_with("wasm-skills/")));
    }

    #[tokio::test]
    async fn rejects_org_wide_oauth_client_default_scope() {
        let v = new_vault();
        let res = run_set(
            Arc::clone(&v),
            "skills/google/oauth_client/client_id",
            "wrong",
            None,
        )
        .await;
        assert!(!res.success);
        assert!(v.get("skills/google/oauth_client/client_id").is_none());
        let err = res.error.unwrap();
        assert!(err.contains("skills/google/oauth_client/client_id"));
        assert!(err.contains("workforce vault set"));
    }

    #[tokio::test]
    async fn rejects_org_wide_oauth_client_at_global_tier() {
        let v = new_vault();
        let res = run_set(
            Arc::clone(&v),
            "skills/google/oauth_client/client_secret",
            "wrong",
            Some("global"),
        )
        .await;
        assert!(!res.success);
        assert!(v.get("skills/google/oauth_client/client_secret").is_none());
    }

    #[tokio::test]
    async fn rejects_org_wide_oauth_client_at_agent_specific_tier() {
        let v = new_vault();
        let res = run_set(
            Arc::clone(&v),
            "skills/google/oauth_client/redirect_uri",
            "wrong",
            Some("agent_specific"),
        )
        .await;
        assert!(!res.success);
        assert!(v.get("skills/google/oauth_client/redirect_uri").is_none());
    }

    #[tokio::test]
    async fn rejects_org_wide_oauth_client_user_scope_shadowing_via_tier_one() {
        // scope='user' would write to users/{uid}/skills/google/oauth_client/...
        // which LayeredUserVault tier 1 resolves before the global bare path,
        // creating the same shadow trap. Block.
        let v = new_vault();
        CURRENT_USER_ID
            .scope("tom".to_string(), async {
                let res = run_set(
                    Arc::clone(&v),
                    "skills/google/oauth_client/client_id",
                    "wrong",
                    Some("user"),
                )
                .await;
                assert!(!res.success);
                assert!(v
                    .get("users/tom/skills/google/oauth_client/client_id")
                    .is_none());
            })
            .await;
    }

    #[tokio::test]
    async fn rejects_claude_code_oauth_client() {
        let v = new_vault();
        let res = run_set(
            Arc::clone(&v),
            "claude_code/oauth_client/client_id",
            "wrong",
            None,
        )
        .await;
        assert!(!res.success);
    }

    #[tokio::test]
    async fn allows_per_user_workspace_refresh_token() {
        let v = new_vault();
        CURRENT_USER_ID
            .scope("alice".to_string(), async {
                let res = run_set(
                    Arc::clone(&v),
                    "skills/google/workspaces/default/refresh_token",
                    "rt-123",
                    Some("user"),
                )
                .await;
                assert!(res.success, "error: {:?}", res.error);
                assert_eq!(
                    v.get("users/alice/skills/google/workspaces/default/refresh_token")
                        .as_deref(),
                    Some("rt-123")
                );
            })
            .await;
    }

    #[tokio::test]
    async fn slack_workspace_bot_token_requires_explicit_agent_shared() {
        // Slack workspace tokens are agent-shared (one bot installs the
        // workspace). With no identity in scope and no explicit scope,
        // strict-mode auto-scope refuses rather than silently writing
        // somewhere unexpected.
        let v = new_vault();
        let res = run_set(
            Arc::clone(&v),
            "slack.T0123ABCD.bot_token",
            "xoxb-secret",
            None,
        )
        .await;
        assert!(!res.success, "expected strict-mode failure");
        assert!(v.get("slack.T0123ABCD.bot_token").is_none());

        // Explicit scope='agent_shared' writes to the bare path the
        // SlackManager listener reads.
        let res = run_set(
            Arc::clone(&v),
            "slack.T0123ABCD.bot_token",
            "xoxb-secret",
            Some("agent_shared"),
        )
        .await;
        assert!(res.success, "error: {:?}", res.error);
        assert_eq!(
            v.get("slack.T0123ABCD.bot_token").as_deref(),
            Some("xoxb-secret")
        );
    }

    #[tokio::test]
    async fn defaults_to_user_scope_when_identity_present() {
        let v = new_vault();
        CURRENT_USER_ID
            .scope("alice".to_string(), async {
                let res =
                    run_set(Arc::clone(&v), "skills/jira/api_token", "alice-token", None).await;
                assert!(res.success, "error: {:?}", res.error);
                assert_eq!(
                    v.get("users/alice/skills/jira/api_token").as_deref(),
                    Some("alice-token")
                );
                assert!(v.get("skills/jira/api_token").is_none());
            })
            .await;
    }

    #[tokio::test]
    async fn defaults_fail_when_no_identity_and_no_scope() {
        let v = new_vault();
        let res = run_set(Arc::clone(&v), "skills/jira/api_token", "no-identity", None).await;
        assert!(!res.success);
        let err = res.error.unwrap();
        assert!(err.contains("per-user scope"), "{err}");
        assert!(err.contains("scope='agent_specific'"), "{err}");
        assert!(v.get("skills/jira/api_token").is_none());
    }

    #[tokio::test]
    async fn explicit_agent_shared_overrides_user_default() {
        let v = new_vault();
        CURRENT_USER_ID
            .scope("alice".to_string(), async {
                let res = run_set(
                    Arc::clone(&v),
                    "skills/jira/api_token",
                    "shared-token",
                    Some("agent_shared"),
                )
                .await;
                assert!(res.success, "error: {:?}", res.error);
                assert_eq!(
                    v.get("skills/jira/api_token").as_deref(),
                    Some("shared-token")
                );
                assert!(v.get("users/alice/skills/jira/api_token").is_none());
            })
            .await;
    }

    #[tokio::test]
    async fn allows_jira_api_token_user_scope() {
        let v = new_vault();
        CURRENT_USER_ID
            .scope("bob".to_string(), async {
                let res = run_set(
                    Arc::clone(&v),
                    "skills/jira/api_token",
                    "atlassian-token",
                    Some("user"),
                )
                .await;
                assert!(res.success, "error: {:?}", res.error);
                assert_eq!(
                    v.get("users/bob/skills/jira/api_token").as_deref(),
                    Some("atlassian-token")
                );
            })
            .await;
    }

    #[tokio::test]
    async fn default_workspace_pointer_is_refused_per_user_and_allowed_shared() {
        let v = new_vault();
        CURRENT_USER_ID
            .scope("alice".to_string(), async {
                let res = run_set(
                    Arc::clone(&v),
                    "skills/google/default_workspace",
                    "alice-ws",
                    None,
                )
                .await;
                assert!(!res.success);
                let err = res.error.unwrap();
                assert!(err.contains("shared configuration"), "{err}");
                assert!(err.contains("skills/google/default_workspace"), "{err}");
                assert!(v
                    .get("users/alice/skills/google/default_workspace")
                    .is_none());

                let res = run_set(
                    Arc::clone(&v),
                    "skills/google/default_workspace",
                    "org-ws",
                    Some("agent_specific"),
                )
                .await;
                assert!(res.success, "error: {:?}", res.error);
                assert_eq!(
                    v.get("skills/google/default_workspace").as_deref(),
                    Some("org-ws")
                );
            })
            .await;
    }

    #[tokio::test]
    async fn agent_shared_scope_writes_to_bare_path() {
        let v = new_vault();
        let res = run_set(
            Arc::clone(&v),
            "engagement/overrides",
            "{}",
            Some("agent_shared"),
        )
        .await;
        assert!(res.success, "error: {:?}", res.error);
        assert_eq!(v.get("engagement/overrides").as_deref(), Some("{}"));
    }

    #[tokio::test]
    async fn vault_inspect_surfaces_shadowing() {
        // The canonical regression test. Tom's wrong creds in agent vault
        // shadow Kings's correct creds in global vault — vault.inspect must
        // surface both so the agent can detect the trap.
        let agent = new_vault();
        let global = new_vault();
        agent
            .set_with_actor(
                "skills/google/oauth_client/client_id",
                "tom-wrong",
                workforce_common::settings::VaultActor::Agent("agent-x"),
            )
            .unwrap();
        global
            .set_with_actor(
                "skills/google/oauth_client/client_id",
                "kings-correct",
                workforce_common::settings::VaultActor::Cli,
            )
            .unwrap();

        let layered: Arc<dyn SecretStore> = Arc::new(crate::user_config::LayeredUserVault::new(
            Arc::clone(&agent),
            Some(global),
        ));
        let tool = VaultInspectTool::new(layered);
        let res = tool
            .execute(serde_json::json!({"path": "skills/google/oauth_client/client_id"}))
            .await
            .unwrap();
        assert!(res.success);

        let resolved = res.output.get("resolved").unwrap();
        assert_eq!(resolved["value"], "tom-wrong");
        assert_eq!(resolved["source"], "agent_specific");

        let shadowed = res.output["shadowed_by"].as_array().unwrap();
        assert_eq!(shadowed.len(), 1);
        assert_eq!(shadowed[0]["source"], "global");
        assert_eq!(
            shadowed[0]["value_sha256_first8"],
            value_fingerprint("kings-correct")
        );

        let metadata = res.output.get("metadata").unwrap();
        assert_eq!(metadata["created_by"], "agent:agent-x");
    }

    #[tokio::test]
    async fn vault_inspect_returns_null_resolved_when_path_missing() {
        let agent = new_vault();
        let layered: Arc<dyn SecretStore> =
            Arc::new(crate::user_config::LayeredUserVault::new(agent, None));
        let tool = VaultInspectTool::new(layered);
        let res = tool
            .execute(serde_json::json!({"path": "nope"}))
            .await
            .unwrap();
        assert!(res.success);
        assert!(res.output["resolved"].is_null());
        assert!(res.output["shadowed_by"].as_array().unwrap().is_empty());
        assert!(res.output["metadata"].is_null());
    }

    // ── Hubspot bug regression — agent must see global tier ────────────────

    #[tokio::test]
    async fn vault_get_resolves_through_layered_when_only_global_has_path() {
        let agent = new_vault();
        let global = new_vault();
        global
            .set("mcp/v1/hubspot/client_secret", "secret-x")
            .unwrap();
        let store = layered_store(agent, global);
        let res = run_get_on(
            store,
            "mcp/v1/hubspot/client_secret",
            RedactionPolicy::off(),
        )
        .await;
        assert!(res.success, "error: {:?}", res.error);
        assert_eq!(res.output["value"], "secret-x");
        assert_eq!(res.output["tier"], "global");
    }

    #[tokio::test]
    async fn vault_list_includes_global_paths_when_layered() {
        let agent = new_vault();
        let global = new_vault();
        agent.set("mcp/v1/atlas/local_token", "agent-only").unwrap();
        global
            .set("mcp/v1/hubspot/client_id", "global-only")
            .unwrap();
        let store = layered_store(agent, global);
        let tool = VaultListTool::new(store);
        let res = tool.execute(serde_json::json!({})).await.unwrap();
        let paths: Vec<String> = serde_json::from_value(res.output["paths"].clone()).unwrap();
        assert!(paths.contains(&"mcp/v1/atlas/local_token".to_string()));
        assert!(paths.contains(&"mcp/v1/hubspot/client_id".to_string()));
    }

    // ── Admin gate on tier-4 (Global) writes ───────────────────────────────

    #[tokio::test]
    async fn vault_set_global_scope_rejects_non_admin() {
        let v = new_vault();
        let tool =
            VaultSetTool::new(single_vault_store(Arc::clone(&v))).with_admin_policy(never_admin());
        let res = tool
            .execute(serde_json::json!({
                "path": "mcp/v1/hubspot/client_secret",
                "value": "secret",
                "scope": "global",
            }))
            .await
            .unwrap();
        assert!(!res.success);
        let err = res.error.unwrap();
        assert!(err.contains("admin role"), "{err}");
        assert!(v.get("mcp/v1/hubspot/client_secret").is_none());
    }

    #[tokio::test]
    async fn vault_set_global_scope_allows_admin() {
        let v = new_vault();
        let tool =
            VaultSetTool::new(single_vault_store(Arc::clone(&v))).with_admin_policy(always_admin());
        let res = tool
            .execute(serde_json::json!({
                "path": "mcp/v1/hubspot/client_secret",
                "value": "secret",
                "scope": "global",
            }))
            .await
            .unwrap();
        assert!(res.success, "error: {:?}", res.error);
        assert_eq!(
            v.get("mcp/v1/hubspot/client_secret").as_deref(),
            Some("secret")
        );
        assert_eq!(res.output["tier"], "global");
    }

    // ── Shadow warning on overlapping tier writes ──────────────────────────

    #[tokio::test]
    async fn vault_set_emits_shadow_warning_when_overriding_lower_tier() {
        let agent = new_vault();
        let global = new_vault();
        global
            .set("mcp/v1/hubspot/client_secret", "lower-secret")
            .unwrap();
        let store = layered_store(agent, global);
        let tool = VaultSetTool::new(store).with_admin_policy(always_admin());
        let res = tool
            .execute(serde_json::json!({
                "path": "mcp/v1/hubspot/client_secret",
                "value": "higher-secret",
                "scope": "agent_specific",
            }))
            .await
            .unwrap();
        assert!(res.success, "error: {:?}", res.error);
        let warn = res
            .output
            .get("shadow_warning")
            .expect("shadow_warning present");
        assert_eq!(warn["writing_tier"], "agent_specific");
        let eclipsed = warn["eclipsed"].as_array().unwrap();
        assert_eq!(eclipsed.len(), 1);
        assert_eq!(eclipsed[0]["tier"], "global");
    }

    #[tokio::test]
    async fn vault_set_no_shadow_warning_when_lower_tiers_empty() {
        let agent = new_vault();
        let global = new_vault();
        let store = layered_store(agent, global);
        let tool = VaultSetTool::new(store).with_admin_policy(always_admin());
        let res = tool
            .execute(serde_json::json!({
                "path": "mcp/v1/hubspot/client_secret",
                "value": "first",
                "scope": "global",
            }))
            .await
            .unwrap();
        assert!(res.success, "error: {:?}", res.error);
        assert!(res.output.get("shadow_warning").is_none());
    }

    // ── vault.my_credentials ────────────────────────────────────────────────

    #[tokio::test]
    async fn vault_my_credentials_groups_entries_by_tier() {
        let agent = new_vault();
        let global = new_vault();
        agent
            .set("users/alice/skills/jira/api_token", "t1")
            .unwrap();
        global.set("users/alice/skills/jira/email", "t2").unwrap();
        agent.set("skills/jira/default_workspace", "t3").unwrap();
        global.set("mcp/v1/hubspot/client_id", "t4").unwrap();
        let store = layered_store(agent, global);

        let tool = VaultMyCredentialsTool::new(store, no_owner_resolver());
        let res = CURRENT_USER_ID
            .scope("alice".to_string(), async {
                tool.execute(serde_json::json!({})).await.unwrap()
            })
            .await;
        assert!(res.success);
        let tiers = &res.output["tiers"];
        assert_eq!(tiers["agent_per_user"][0]["path"], "skills/jira/api_token");
        assert_eq!(tiers["global_per_user"][0]["path"], "skills/jira/email");
        assert_eq!(
            tiers["agent_specific"][0]["path"],
            "skills/jira/default_workspace"
        );
        assert_eq!(tiers["global"][0]["path"], "mcp/v1/hubspot/client_id");
    }

    #[tokio::test]
    async fn vault_my_credentials_filters_other_user_per_user_entries() {
        let agent = new_vault();
        let global = new_vault();
        global
            .set("users/alice/skills/jira/api_token", "alice")
            .unwrap();
        global
            .set("users/bob/skills/jira/api_token", "bob")
            .unwrap();
        let store = layered_store(agent, global);
        let tool = VaultMyCredentialsTool::new(store, no_owner_resolver());
        let res = CURRENT_USER_ID
            .scope("bob".to_string(), async {
                tool.execute(serde_json::json!({})).await.unwrap()
            })
            .await;
        let per_user = res.output["tiers"]["global_per_user"].as_array().unwrap();
        assert_eq!(per_user.len(), 1);
        assert_eq!(per_user[0]["path"], "skills/jira/api_token");
        let any_alice = res.output.to_string().contains("alice");
        assert!(!any_alice, "Bob's view leaked Alice's per-user entries");
    }

    #[tokio::test]
    async fn vault_my_credentials_excludes_wasm_skills_namespace() {
        let agent = new_vault();
        let global = new_vault();
        agent
            .set("wasm-skills/orbit/users/alice/api_key", "ck")
            .unwrap();
        agent.set("skills/jira/default_workspace", "acme").unwrap();
        let store = layered_store(agent, global);
        let tool = VaultMyCredentialsTool::new(store, no_owner_resolver());
        let res = tool.execute(serde_json::json!({})).await.unwrap();
        let rendered = serde_json::to_string(&res.output).unwrap();
        assert!(
            !rendered.contains("wasm-skills"),
            "wasm-skills/* paths must not appear in my_credentials"
        );
        assert!(rendered.contains("skills/jira/default_workspace"));
    }

    #[tokio::test]
    async fn vault_my_credentials_resolves_owner_display_name() {
        let agent = new_vault();
        let global = new_vault();
        global
            .set_with_actor(
                "mcp/v1/hubspot/client_id",
                "id-x",
                VaultActor::User("u-tom"),
            )
            .unwrap();
        let store = layered_store(agent, global);
        let resolver: OwnerResolverFn = Arc::new(|actor: &str| {
            if actor == "user:u-tom" {
                Some("Tom Smith".into())
            } else {
                None
            }
        });
        let tool = VaultMyCredentialsTool::new(store, resolver);
        let res = tool.execute(serde_json::json!({})).await.unwrap();
        let entry = &res.output["tiers"]["global"][0];
        assert_eq!(entry["created_by_display"], "Tom Smith");
    }

    // ── vault.move ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn vault_move_user_tiers_succeeds_without_admin() {
        let agent = new_vault();
        let global = new_vault();
        global
            .set("users/alice/skills/jira/api_token", "alice-tok")
            .unwrap();
        let store = layered_store(Arc::clone(&agent), Arc::clone(&global));
        let tool = VaultMoveTool::new(store, never_admin());
        let res = CURRENT_USER_ID
            .scope("alice".to_string(), async {
                tool.execute(serde_json::json!({
                    "path": "skills/jira/api_token",
                    "from_tier": "global_per_user",
                    "to_tier": "agent_per_user",
                }))
                .await
                .unwrap()
            })
            .await;
        assert!(res.success, "error: {:?}", res.error);
        assert!(global.get("users/alice/skills/jira/api_token").is_none());
        assert_eq!(
            agent.get("users/alice/skills/jira/api_token").as_deref(),
            Some("alice-tok")
        );
    }

    #[tokio::test]
    async fn vault_move_to_shared_requires_admin() {
        let agent = new_vault();
        let global = new_vault();
        global
            .set("users/alice/mcp/v1/hubspot/client_id", "alice-id")
            .unwrap();
        let store = layered_store(agent, global);
        let tool = VaultMoveTool::new(store, never_admin());
        let res = CURRENT_USER_ID
            .scope("alice".to_string(), async {
                tool.execute(serde_json::json!({
                    "path": "mcp/v1/hubspot/client_id",
                    "from_tier": "global_per_user",
                    "to_tier": "global",
                    "confirm_share": true,
                }))
                .await
                .unwrap()
            })
            .await;
        assert!(!res.success);
        assert!(res.error.unwrap().contains("admin role"));
    }

    #[tokio::test]
    async fn vault_move_to_shared_requires_confirm_share_even_for_admin() {
        let agent = new_vault();
        let global = new_vault();
        global
            .set("users/alice/mcp/v1/hubspot/client_id", "alice-id")
            .unwrap();
        let store = layered_store(agent, Arc::clone(&global));
        let tool = VaultMoveTool::new(store, always_admin());
        let res = CURRENT_USER_ID
            .scope("alice".to_string(), async {
                tool.execute(serde_json::json!({
                    "path": "mcp/v1/hubspot/client_id",
                    "from_tier": "global_per_user",
                    "to_tier": "global",
                }))
                .await
                .unwrap()
            })
            .await;
        assert!(!res.success);
        assert!(res.error.unwrap().contains("widens visibility"));
        assert_eq!(
            global
                .get("users/alice/mcp/v1/hubspot/client_id")
                .as_deref(),
            Some("alice-id"),
            "source must remain untouched on auth rejection"
        );
    }

    #[tokio::test]
    async fn vault_move_rejects_existing_destination_without_overwrite() {
        let agent = new_vault();
        let global = new_vault();
        agent
            .set("users/alice/skills/jira/api_token", "src")
            .unwrap();
        global
            .set("users/alice/skills/jira/api_token", "dst-existing")
            .unwrap();
        let store = layered_store(Arc::clone(&agent), Arc::clone(&global));
        let tool = VaultMoveTool::new(store, never_admin());
        let res = CURRENT_USER_ID
            .scope("alice".to_string(), async {
                tool.execute(serde_json::json!({
                    "path": "skills/jira/api_token",
                    "from_tier": "agent_per_user",
                    "to_tier": "global_per_user",
                }))
                .await
                .unwrap()
            })
            .await;
        assert!(!res.success);
        assert!(res.error.unwrap().contains("Pass overwrite=true"));
        assert_eq!(
            agent.get("users/alice/skills/jira/api_token").as_deref(),
            Some("src"),
            "source must remain untouched on destination conflict"
        );
        assert_eq!(
            global.get("users/alice/skills/jira/api_token").as_deref(),
            Some("dst-existing"),
            "destination must be untouched"
        );
    }

    #[tokio::test]
    async fn vault_move_overwrite_replaces_destination() {
        let agent = new_vault();
        let global = new_vault();
        agent
            .set("users/alice/skills/jira/api_token", "src")
            .unwrap();
        global
            .set("users/alice/skills/jira/api_token", "dst-existing")
            .unwrap();
        let store = layered_store(Arc::clone(&agent), Arc::clone(&global));
        let tool = VaultMoveTool::new(store, never_admin());
        let res = CURRENT_USER_ID
            .scope("alice".to_string(), async {
                tool.execute(serde_json::json!({
                    "path": "skills/jira/api_token",
                    "from_tier": "agent_per_user",
                    "to_tier": "global_per_user",
                    "overwrite": true,
                }))
                .await
                .unwrap()
            })
            .await;
        assert!(res.success, "error: {:?}", res.error);
        assert_eq!(res.output["overwrote_destination"], true);
        assert!(agent.get("users/alice/skills/jira/api_token").is_none());
        assert_eq!(
            global.get("users/alice/skills/jira/api_token").as_deref(),
            Some("src")
        );
    }

    #[tokio::test]
    async fn vault_move_from_equals_to_rejects() {
        let agent = new_vault();
        let store = single_vault_store(agent);
        let tool = VaultMoveTool::new(store, always_admin());
        let res = tool
            .execute(serde_json::json!({
                "path": "x",
                "from_tier": "agent_specific",
                "to_tier": "agent_specific",
            }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("no move needed"));
    }

    // ── Back-compat for serde aliases ───────────────────────────────────────

    #[tokio::test]
    async fn vault_set_accepts_legacy_user_scope_alias() {
        let v = new_vault();
        CURRENT_USER_ID
            .scope("alice".to_string(), async {
                let res =
                    run_set(Arc::clone(&v), "skills/jira/api_token", "tok", Some("user")).await;
                assert!(res.success, "error: {:?}", res.error);
                assert_eq!(res.output["tier"], "global_per_user");
            })
            .await;
    }

    #[tokio::test]
    async fn vault_set_accepts_legacy_agent_shared_alias() {
        let v = new_vault();
        let res = run_set(
            Arc::clone(&v),
            "engagement/overrides",
            "{}",
            Some("agent_shared"),
        )
        .await;
        assert!(res.success, "error: {:?}", res.error);
        assert_eq!(res.output["tier"], "agent_specific");
    }

    // ── vault.delete admin gate (confirmed privilege hole) ──────────────────

    async fn run_delete(
        store: Arc<dyn SecretStore>,
        path: &str,
        is_admin: IsAdminFn,
    ) -> ToolResult {
        let tool = VaultDeleteTool::new(store).with_admin_policy(is_admin);
        tool.execute(serde_json::json!({ "path": path }))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn vault_delete_shared_secret_rejects_non_admin() {
        // The hole: a Member could delete the shared Hubspot client secret
        // (a bare T3/T4 path) without admin. Must now be denied, and the
        // secret must survive.
        let v = new_vault();
        v.set("mcp/v1/hubspot/client_secret", "shared-secret")
            .unwrap();
        let store = single_vault_store(Arc::clone(&v));
        let res = run_delete(store, "mcp/v1/hubspot/client_secret", never_admin()).await;
        assert!(!res.success, "non-admin must be denied");
        assert!(res.error.unwrap().contains("admin role"));
        assert_eq!(
            v.get("mcp/v1/hubspot/client_secret").as_deref(),
            Some("shared-secret"),
            "shared secret must survive a denied delete"
        );
    }

    #[tokio::test]
    async fn vault_delete_shared_secret_allows_admin() {
        let v = new_vault();
        v.set("mcp/v1/hubspot/client_secret", "shared-secret")
            .unwrap();
        let store = single_vault_store(Arc::clone(&v));
        let res = run_delete(store, "mcp/v1/hubspot/client_secret", always_admin()).await;
        assert!(res.success, "admin must be allowed: {:?}", res.error);
        assert_eq!(res.output["deleted"], true);
        assert!(v.get("mcp/v1/hubspot/client_secret").is_none());
    }

    #[tokio::test]
    async fn vault_delete_own_per_user_explicit_path_allows_member() {
        // Member deleting their own secret via the explicit users/{uid}/…
        // form — allowed without admin.
        let v = new_vault();
        v.set("users/alice/skills/jira/api_token", "alice-tok")
            .unwrap();
        let store = single_vault_store(Arc::clone(&v));
        let res = CURRENT_USER_ID
            .scope("alice".to_string(), async {
                run_delete(store, "users/alice/skills/jira/api_token", never_admin()).await
            })
            .await;
        assert!(
            res.success,
            "own per-user delete must be allowed: {:?}",
            res.error
        );
        assert!(v.get("users/alice/skills/jira/api_token").is_none());
    }

    #[tokio::test]
    async fn vault_delete_own_per_user_bare_logical_path_allows_member() {
        // The self-service flow: vault.list returns the bare logical path
        // (the users/{uid}/ segment stripped), and delete must accept it for
        // the caller's own per-user entry without admin.
        let v = new_vault();
        v.set("users/bob/skills/jira/api_token", "bob-tok").unwrap();
        let store = single_vault_store(Arc::clone(&v));
        let res = CURRENT_USER_ID
            .scope("bob".to_string(), async {
                run_delete(store, "skills/jira/api_token", never_admin()).await
            })
            .await;
        assert!(
            res.success,
            "bare logical own-delete must be allowed: {:?}",
            res.error
        );
        assert!(v.get("users/bob/skills/jira/api_token").is_none());
    }

    #[tokio::test]
    async fn vault_delete_org_wide_path_rejected_even_for_admin() {
        // Org-wide OAuth client lives only in the hub vault — blocked for
        // everyone, including admins, exactly like vault.set.
        let v = new_vault();
        v.set("skills/google/oauth_client/client_secret", "x")
            .unwrap();
        let store = single_vault_store(Arc::clone(&v));
        let res = run_delete(
            store,
            "skills/google/oauth_client/client_secret",
            always_admin(),
        )
        .await;
        assert!(
            !res.success,
            "org-wide delete must be blocked even for admin"
        );
        assert!(res.error.unwrap().contains("workforce vault set"));
        assert_eq!(
            v.get("skills/google/oauth_client/client_secret").as_deref(),
            Some("x"),
            "org-wide secret must survive"
        );
    }

    #[tokio::test]
    async fn vault_delete_cross_user_path_rejected() {
        // Bob cannot delete Alice's per-user secret — agent_can_read blocks
        // it before the admin gate is even reached.
        let v = new_vault();
        v.set("users/alice/skills/jira/api_token", "alice-tok")
            .unwrap();
        let store = single_vault_store(Arc::clone(&v));
        let res = CURRENT_USER_ID
            .scope("bob".to_string(), async {
                run_delete(store, "users/alice/skills/jira/api_token", never_admin()).await
            })
            .await;
        assert!(!res.success, "cross-user delete must be denied");
        assert_eq!(
            v.get("users/alice/skills/jira/api_token").as_deref(),
            Some("alice-tok")
        );
    }

    // -- vault.rename ------------------------------------------------------

    #[tokio::test]
    async fn vault_rename_requires_admin() {
        let agent = new_vault();
        agent.set("skills/shared/token", "v").unwrap();
        let store = layered_store(Arc::clone(&agent), new_vault());
        let tool = VaultRenameTool::new(store, never_admin());
        let res = tool
            .execute(serde_json::json!({
                "path": "skills/shared/token",
                "new_path": "skills/shared/renamed",
                "tier": "agent_specific",
            }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("system admin role"));
        assert_eq!(
            agent.get("skills/shared/token").as_deref(),
            Some("v"),
            "source must be untouched when denied"
        );
    }

    #[tokio::test]
    async fn vault_rename_shared_tier_succeeds_for_admin() {
        let agent = new_vault();
        agent.set("skills/shared/token", "v").unwrap();
        let store = layered_store(Arc::clone(&agent), new_vault());
        let tool = VaultRenameTool::new(store, always_admin());
        let res = tool
            .execute(serde_json::json!({
                "path": "skills/shared/token",
                "new_path": "skills/shared/renamed",
                "tier": "agent_specific",
            }))
            .await
            .unwrap();
        assert!(res.success, "error: {:?}", res.error);
        assert!(agent.get("skills/shared/token").is_none());
        assert_eq!(agent.get("skills/shared/renamed").as_deref(), Some("v"));
    }

    #[tokio::test]
    async fn vault_rename_per_user_defaults_to_active_user() {
        let agent = new_vault();
        agent
            .set("users/alice/skills/jira/api_token", "alice-tok")
            .unwrap();
        let store = layered_store(Arc::clone(&agent), new_vault());
        let tool = VaultRenameTool::new(store, always_admin());
        let res = CURRENT_USER_ID
            .scope("alice".to_string(), async {
                tool.execute(serde_json::json!({
                    "path": "skills/jira/api_token",
                    "new_path": "skills/jira/renamed",
                    "tier": "agent_per_user",
                }))
                .await
                .unwrap()
            })
            .await;
        assert!(res.success, "error: {:?}", res.error);
        assert!(agent.get("users/alice/skills/jira/api_token").is_none());
        assert_eq!(
            agent.get("users/alice/skills/jira/renamed").as_deref(),
            Some("alice-tok")
        );
    }

    #[tokio::test]
    async fn vault_rename_rejects_existing_destination_then_overwrites() {
        let agent = new_vault();
        agent.set("skills/a", "src").unwrap();
        agent.set("skills/b", "existing").unwrap();
        let store = layered_store(Arc::clone(&agent), new_vault());
        let tool = VaultRenameTool::new(store, always_admin());

        let guarded = tool
            .execute(serde_json::json!({
                "path": "skills/a",
                "new_path": "skills/b",
                "tier": "agent_specific",
            }))
            .await
            .unwrap();
        assert!(!guarded.success);
        assert!(guarded.error.unwrap().contains("Pass overwrite=true"));
        assert_eq!(agent.get("skills/a").as_deref(), Some("src"));
        assert_eq!(agent.get("skills/b").as_deref(), Some("existing"));

        let forced = tool
            .execute(serde_json::json!({
                "path": "skills/a",
                "new_path": "skills/b",
                "tier": "agent_specific",
                "overwrite": true,
            }))
            .await
            .unwrap();
        assert!(forced.success, "error: {:?}", forced.error);
        assert_eq!(forced.output["overwrote_destination"], true);
        assert!(agent.get("skills/a").is_none());
        assert_eq!(agent.get("skills/b").as_deref(), Some("src"));
    }

    #[tokio::test]
    async fn vault_rename_rejects_unsafe_paths_and_noop() {
        let tool = VaultRenameTool::new(layered_store(new_vault(), new_vault()), always_admin());
        let bad = tool
            .execute(serde_json::json!({
                "path": "users/alice/x",
                "new_path": "y",
                "tier": "agent_specific",
            }))
            .await
            .unwrap();
        assert!(!bad.success);
        assert!(bad.error.unwrap().contains("invalid path"));

        let noop = tool
            .execute(serde_json::json!({
                "path": "skills/a",
                "new_path": "skills/a",
                "tier": "agent_specific",
            }))
            .await
            .unwrap();
        assert!(!noop.success);
        assert!(noop.error.unwrap().contains("nothing to rename"));
    }

    #[tokio::test]
    async fn vault_rename_source_missing_fails() {
        let tool = VaultRenameTool::new(layered_store(new_vault(), new_vault()), always_admin());
        let res = tool
            .execute(serde_json::json!({
                "path": "skills/absent",
                "new_path": "skills/present",
                "tier": "agent_specific",
            }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("No value to rename"));
    }

    // -- vault.copy --------------------------------------------------------

    #[tokio::test]
    async fn vault_copy_requires_admin() {
        let agent = new_vault();
        agent
            .set("users/alice/skills/jira/api_token", "tok")
            .unwrap();
        let store = layered_store(Arc::clone(&agent), new_vault());
        let tool = VaultCopyTool::new(store, never_admin());
        let res = CURRENT_USER_ID
            .scope("alice".to_string(), async {
                tool.execute(serde_json::json!({
                    "path": "skills/jira/api_token",
                    "from_tier": "agent_per_user",
                    "to_tier": "agent_per_user",
                    "to_users": ["bob"],
                }))
                .await
                .unwrap()
            })
            .await;
        assert!(!res.success);
        assert!(res.error.unwrap().contains("system admin role"));
        assert!(
            agent.get("users/bob/skills/jira/api_token").is_none(),
            "nothing copied when denied"
        );
    }

    #[tokio::test]
    async fn vault_copy_fans_out_to_users_and_keeps_source() {
        let agent = new_vault();
        agent
            .set("users/alice/skills/jira/api_token", "S3CR3T-VALUE")
            .unwrap();
        let store = layered_store(Arc::clone(&agent), new_vault());
        let tool = VaultCopyTool::new(store, always_admin());
        // from_user defaults to the active user (alice).
        let res = CURRENT_USER_ID
            .scope("alice".to_string(), async {
                tool.execute(serde_json::json!({
                    "path": "skills/jira/api_token",
                    "from_tier": "agent_per_user",
                    "to_tier": "agent_per_user",
                    "to_users": ["bob", "carol"],
                }))
                .await
                .unwrap()
            })
            .await;
        assert!(res.success, "error: {:?}", res.error);
        assert_eq!(res.output["copied"], 2);
        assert_eq!(res.output["source_user"], "alice");
        for uid in ["bob", "carol", "alice"] {
            assert_eq!(
                agent
                    .get(&format!("users/{uid}/skills/jira/api_token"))
                    .as_deref(),
                Some("S3CR3T-VALUE"),
                "{uid} should resolve the copied value (source kept)"
            );
        }
        // Result must never leak the cleartext value.
        assert!(!serde_json::to_string(&res.output)
            .unwrap()
            .contains("S3CR3T-VALUE"));
    }

    #[tokio::test]
    async fn vault_copy_rejects_non_per_user_destination() {
        let tool = VaultCopyTool::new(layered_store(new_vault(), new_vault()), always_admin());
        let res = tool
            .execute(serde_json::json!({
                "path": "skills/jira/api_token",
                "from_tier": "agent_specific",
                "to_tier": "global",
                "to_users": ["bob"],
            }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res
            .error
            .unwrap()
            .contains("destination tier must be per-user"));
    }

    #[tokio::test]
    async fn vault_copy_rejects_unsafe_path_and_empty_destinations() {
        let tool = VaultCopyTool::new(layered_store(new_vault(), new_vault()), always_admin());
        let bad_path = tool
            .execute(serde_json::json!({
                "path": "users/alice/x",
                "from_tier": "agent_specific",
                "to_tier": "agent_per_user",
                "to_users": ["bob"],
            }))
            .await
            .unwrap();
        assert!(!bad_path.success);
        assert!(bad_path.error.unwrap().contains("invalid path"));

        let empty = tool
            .execute(serde_json::json!({
                "path": "skills/jira/api_token",
                "from_tier": "agent_specific",
                "to_tier": "agent_per_user",
                "to_users": ["", ""],
            }))
            .await
            .unwrap();
        assert!(!empty.success);
        assert!(empty.error.unwrap().contains("at least one user"));
    }

    #[tokio::test]
    async fn vault_copy_missing_source_fails() {
        let tool = VaultCopyTool::new(layered_store(new_vault(), new_vault()), always_admin());
        let res = tool
            .execute(serde_json::json!({
                "path": "skills/absent",
                "from_tier": "agent_specific",
                "to_tier": "agent_per_user",
                "to_users": ["bob"],
            }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("no value at the source tier"));
    }

    const ALICE: &str = "6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11";
    const CHATGPT: &str = "codex/auth_json";

    /// An agent's vault tools over its own and the global vault, with logins
    /// guarded and the caller sharing the agent's logins when `may_share`.
    struct LoginTools {
        agent: Arc<Vault>,
        global: Arc<Vault>,
        store: Arc<dyn SecretStore>,
        logins: LoginWrites,
    }

    impl LoginTools {
        fn new(may_share: bool) -> Self {
            let (agent, global) = (new_vault(), new_vault());
            let layered = Arc::new(LayeredUserVault::new(
                Arc::clone(&agent),
                Some(Arc::clone(&global)),
            ));
            let logins = LoginWrites::new(
                Arc::clone(&layered),
                crate::hub_core::logins::store::known_vaults(
                    Some(Arc::clone(&agent)),
                    Some(Arc::clone(&global)),
                    Arc::default(),
                ),
                Arc::new(move || may_share),
            );
            Self {
                agent,
                global,
                store: layered,
                logins,
            }
        }

        async fn run(&self, tool: impl Tool, input: serde_json::Value) -> ToolResult {
            CURRENT_USER_ID
                .scope(ALICE.to_string(), tool.execute(input))
                .await
                .unwrap()
        }

        async fn set(&self, value: &str, scope: &str) -> ToolResult {
            self.run(
                VaultSetTool::new(Arc::clone(&self.store)).with_logins(self.logins.clone()),
                serde_json::json!({"path": CHATGPT, "value": value, "scope": scope}),
            )
            .await
        }

        async fn delete(&self) -> ToolResult {
            self.run(
                VaultDeleteTool::new(Arc::clone(&self.store)).with_logins(self.logins.clone()),
                serde_json::json!({"path": CHATGPT}),
            )
            .await
        }
    }

    fn own() -> String {
        crate::user_config::per_user_key(ALICE, CHATGPT)
    }

    #[tokio::test]
    async fn vault_tools_write_login_keys_through_the_guarded_store() {
        let tools = LoginTools::new(false);
        let sign_in = crate::hub_core::logins::test_support::login("acct-1", "a0", "rt-0");

        assert!(tools.set(&sign_in, "global_per_user").await.success);
        assert_eq!(tools.global.get(&own()).as_deref(), Some(sign_in.as_str()));
        let copy = tools.set(&sign_in, "agent_per_user").await;
        assert!(!copy.success);
        assert!(format!("{copy:?}").contains("already stored"), "{copy:?}");
        assert!(tools.agent.get(&own()).is_none());
        let partial = tools.set("{\"tokens\": {}}", "agent_per_user").await;
        assert!(!partial.success);
        assert!(
            format!("{partial:?}").contains("not a usable login"),
            "{partial:?}"
        );

        let moved = tools
            .run(
                VaultMoveTool::new(Arc::clone(&tools.store), crate::authz::always_admin())
                    .with_logins(tools.logins.clone()),
                serde_json::json!({"path": CHATGPT, "from_tier": "global_per_user", "to_tier": "agent_per_user"}),
            )
            .await;
        assert!(moved.success, "{moved:?}");
        assert!(tools.global.get(&own()).is_none());
        assert_eq!(tools.agent.get(&own()).as_deref(), Some(sign_in.as_str()));

        let renamed = tools
            .run(
                VaultRenameTool::new(Arc::clone(&tools.store), crate::authz::always_admin())
                    .with_logins(tools.logins.clone()),
                serde_json::json!({"path": CHATGPT, "new_path": "skills/x/token", "tier": "agent_per_user"}),
            )
            .await;
        assert!(!renamed.success);
        assert!(
            format!("{renamed:?}").contains("its own kind"),
            "{renamed:?}"
        );
        assert!(tools.agent.get(&own()).is_some());

        assert!(tools.delete().await.success);
        assert!(tools.agent.get(&own()).is_none());

        let unguarded = tools
            .run(
                VaultSetTool::new(Arc::clone(&tools.store)),
                serde_json::json!({"path": CHATGPT, "value": sign_in, "scope": "global_per_user"}),
            )
            .await;
        assert!(!unguarded.success);
        assert!(
            format!("{unguarded:?}").contains("can't store"),
            "{unguarded:?}"
        );
    }

    #[tokio::test]
    async fn member_cannot_overwrite_agent_shared_login_via_vault_set() {
        let shared = crate::hub_core::logins::test_support::login("acct-1", "a0", "rt-shared");
        let member = LoginTools::new(false);
        member.agent.set(CHATGPT, &shared).unwrap();

        let overwrite = member
            .set(
                &crate::hub_core::logins::test_support::login("acct-2", "a1", "rt-mine"),
                "agent_specific",
            )
            .await;
        assert!(!overwrite.success);
        assert!(
            format!("{overwrite:?}").contains("owner or an admin"),
            "{overwrite:?}"
        );
        let removal = member.delete().await;
        assert!(!removal.success, "{removal:?}");
        assert_eq!(member.agent.get(CHATGPT).as_deref(), Some(shared.as_str()));

        let owner = LoginTools::new(true);
        let replaced = owner
            .set(
                &crate::hub_core::logins::test_support::login("acct-2", "a1", "rt-owner"),
                "agent_specific",
            )
            .await;
        assert!(replaced.success, "{replaced:?}");
    }
}
