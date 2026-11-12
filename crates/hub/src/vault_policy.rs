//! Reserved-prefix policy for `vault.set` — paths that must live in the
//! hub's global vault and never be written from an agent context.
//!
//! Background: code paths that read the raw global vault directly (rather
//! than through [`crate::user_config::LayeredUserVault`]) cannot see
//! per-agent overrides. If an agent-vault entry shadows the global bare
//! path, resolution and the raw read disagree — for the Google OAuth
//! callback this manifests as `unauthorized_client` at token exchange.

/// Path prefixes that may only be written via the hub CLI or the
/// `/api/vault/{path}` REST handler. Add a prefix iff a non-agent reader
/// resolves it through the raw global vault rather than `LayeredUserVault`.
const ORG_WIDE_PREFIXES: &[&str] = &[
    // Read by `google_oauth_callback::complete_exchange` via the raw
    // global vault.
    "skills/google/oauth_client/",
    // Forward compat for any future Claude Code OAuth callback.
    "claude_code/oauth_client/",
    // ChatGPT device-code sign-ins in flight: written only by the hub's
    // device-login store, which names the slot each one is stored in.
    crate::subscription::device_login::RECORD_PREFIX,
    // The hub-wide embedder reads its key from the global vault alone.
    "embeddings/",
];

/// Returns `true` if `path` is reserved for the hub's global vault.
pub fn is_org_wide(path: &str) -> bool {
    ORG_WIDE_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

/// OAuth app material that is fine at an agent tier but never per-user.
const SHARED_ONLY_PREFIXES: &[&str] = &["skills/freeagent/oauth_client/"];

/// Returns `true` if `path` may only exist in a shared tier: org-wide
/// prefixes, shared-only OAuth app material, and the `default_*` routing
/// selectors that shape a client every user of an agent shares. The gate
/// behind every per-user write a member can reach; admin-only relocation
/// endpoints are not gated, and loaders ignore a per-user value regardless.
pub fn is_shared_only(path: &str) -> bool {
    is_org_wide(path)
        || SHARED_ONLY_PREFIXES
            .iter()
            .any(|prefix| path.starts_with(prefix))
        || crate::skill_loader::is_default_selector(path)
}

pub fn shared_only_rejection_message(path: &str) -> String {
    format!(
        "'{path}' is shared configuration and cannot be set per user: it shapes the \
         client every user of this agent shares. Set it with scope='agent_specific' \
         (no admin needed) or scope='global' (admin), or from the Vault page."
    )
}

/// Reasons the agent-facing vault tools may refuse a read.
///
/// Policy enforcement layered on top of `LayeredUserVault`'s own resolution
/// logic — a request can be technically resolvable (the value exists at
/// some layer) but still denied because the active agent identity isn't
/// authorised to see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentReadDenial {
    /// The path lives in the WASM skill private namespace
    /// (`wasm-skills/...`). Skills own that tree; the LLM-side `vault.get`
    /// must never read it. Skills access their own keys via
    /// `wf_vault_get`, which applies independent path scoping in
    /// `crates/wasm-runtime/src/host.rs`.
    GuestPrivateNamespace,
    /// The path is a database connection credential (`databases/...`). These
    /// hold connection strings with embedded passwords for the `database.*`
    /// tools, which resolve them server-side; the agent names an instance and
    /// must never read the secret back into context.
    DatabaseCredentials,
    /// The path is outbound HTTP endpoint configuration (`http/...`). These hold
    /// the base URLs and credentials the `http.*` tools resolve server-side; the
    /// agent names an endpoint and must never read its credential back into
    /// context, nor point one at a host of its choosing by writing config.
    HttpEndpointCredentials,
    /// The path is a per-user entry under `users/{uid}/...` for a `uid`
    /// that isn't the active `CURRENT_USER_ID`. Without this guard,
    /// `vault.get path=users/alice/...` invoked while bob is active would
    /// surface alice's secret.
    CrossUser,
    /// The path is a ChatGPT sign-in in flight
    /// (`subscription/device_login/...`), whose device code would let the
    /// agent finish someone's sign-in.
    DeviceLogin,
    /// The path holds a subscription login (`claude_code/oauth_token`,
    /// `codex/auth_json`), whose token runs as its owner's plan wherever it
    /// is read back.
    Login,
    /// The path holds a Desk link to another hub (`desk/hubs/...`,
    /// `desk/links/...`, shared or per-user): the `wfr_` token Desk's board
    /// sync calls that hub with as the person. Only the sync reads it, never
    /// an agent, the overseer included.
    DeskLink,
    /// The path is a model account (`llm/accounts/...`, shared or per-user):
    /// which key or AWS profile pays for the agent's thinking. The person
    /// sets it, in Desk or with the account tools; an agent never reads or
    /// writes it through the vault tools.
    LlmAccount,
    /// On a hub running Desk, the path says where a connection sends its
    /// key (a Jira site, a GitHub or Datadog address, a Metabase site, a
    /// mailbox's server). The person sets it in Settings → Connections, or
    /// the agent with `connections.*`, which asks first; an agent writing it
    /// here could send the person's key to a host of its choosing.
    ConnectionAddress,
    /// No `CURRENT_USER_ID` is in scope but the path is per-user. System
    /// jobs must opt into a synthetic identity via
    /// `workforce_agent::runner::with_system_identity` rather than read
    /// per-user paths anonymously.
    NoUserIdentity,
}

impl std::fmt::Display for AgentReadDenial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentReadDenial::GuestPrivateNamespace => write!(
                f,
                "vault path is in the WASM skill private namespace and is not \
                 accessible from the agent vault tool. A skill credential is \
                 provisioned by the USER typing `cmd: secret <name> <value>` then \
                 `cmd: skill grant <skill> <name>` in chat — do not ask for the \
                 value or try to store it yourself"
            ),
            AgentReadDenial::HttpEndpointCredentials => write!(
                f,
                "vault path holds outbound HTTP endpoint configuration and is not \
                 accessible from the agent vault tool; use the http.* tools, which \
                 name an endpoint rather than exposing its credential. An operator \
                 configures endpoints from the host"
            ),
            AgentReadDenial::DatabaseCredentials => write!(
                f,
                "vault path holds a database connection credential and is not \
                 accessible from the agent vault tool; use the database.* tools, \
                 which name an instance rather than exposing its connection string"
            ),
            AgentReadDenial::CrossUser => write!(
                f,
                "vault path belongs to a different user than the active identity"
            ),
            AgentReadDenial::DeviceLogin => write!(
                f,
                "vault path holds a ChatGPT sign-in in flight and is not accessible \
                 from the agent vault tool; use chatgpt.login"
            ),
            AgentReadDenial::Login => write!(
                f,
                "vault path holds a subscription login, which is never read back; \
                 store a new one with claude_code.setup_token, codex.setup_auth or My Settings"
            ),
            AgentReadDenial::DeskLink => write!(
                f,
                "vault path holds a Desk link to another hub and is not \
                 accessible from the agent vault tool"
            ),
            AgentReadDenial::LlmAccount => write!(
                f,
                "vault path holds a model account, which says what pays for the \
                 agent's thinking, and is the person's to set, in Desk or with \
                 system.add_account; it is not read or written from the agent vault tool"
            ),
            AgentReadDenial::ConnectionAddress => write!(
                f,
                "vault path says where a connection sends its key, which the agent vault \
                 tools don't change on Desk. Set the connection up with connections.setup, \
                 or ask the person to change it in Settings → Connections"
            ),
            AgentReadDenial::NoUserIdentity => write!(
                f,
                "vault path is per-user but no current user identity is set"
            ),
        }
    }
}

/// Decide whether the agent-facing `vault.{get,list,set,delete,inspect}`
/// tools may touch `path`. Independent of the existing `is_org_wide` write
/// guard — this one applies to **reads** as well.
///
/// Rules:
/// 1. `wasm-skills/*` is always denied — skills own that namespace.
/// 2. `users/{uid}/...` is allowed only when `uid` matches `current_uid`
///    (or `current_uid == "system"`, which we treat as a service identity
///    that may inspect any per-user path for system-job operations).
/// 3. All other paths fall through to the existing read path.
///
/// Used by `crates/hub/src/vault_tool.rs` to gate `VaultGetTool`,
/// `VaultListTool`, `VaultSetTool`, `VaultDeleteTool`, `VaultInspectTool`.
pub fn agent_can_read(path: &str, current_uid: Option<&str>) -> Result<(), AgentReadDenial> {
    // Defense-in-depth: a `..` segment in a vault path doesn't escape the
    // store (vault keys are flat strings, not filesystem paths) but a
    // path like `users/alice/../bob/...` would still pass the
    // first-segment uid check below and hit the underlying store with a
    // surprising literal key. Reject it here.
    if path.split('/').any(|seg| seg == "..") {
        return Err(AgentReadDenial::CrossUser);
    }
    if path.starts_with("wasm-skills/") {
        return Err(AgentReadDenial::GuestPrivateNamespace);
    }
    // Database connection credentials are off-limits whether shared
    // (`databases/...`) or per-user (`users/{uid}/databases/...`).
    let logical = path
        .strip_prefix("users/")
        .and_then(|rest| rest.split_once('/'))
        .map_or(path, |(_, rest)| rest);
    if logical == "databases" || logical.starts_with("databases/") {
        return Err(AgentReadDenial::DatabaseCredentials);
    }
    // Outbound HTTP endpoint configuration is off-limits on the same terms: the
    // `http.*` tools resolve base URL and credential server-side, and letting the
    // agent write here would let it point a credential at a host of its choosing.
    if logical == "http" || logical.starts_with("http/") {
        return Err(AgentReadDenial::HttpEndpointCredentials);
    }
    if logical.starts_with(crate::subscription::device_login::RECORD_PREFIX) {
        return Err(AgentReadDenial::DeviceLogin);
    }
    // Before the per-user branch, so no identity (`system` included) reads
    // a Desk link.
    if is_desk_link(path) {
        return Err(AgentReadDenial::DeskLink);
    }
    if is_llm_account(path) {
        return Err(AgentReadDenial::LlmAccount);
    }
    if let Some(rest) = path.strip_prefix("users/") {
        let uid = rest.split('/').next().unwrap_or("");
        if uid.is_empty() {
            return Err(AgentReadDenial::CrossUser);
        }
        match current_uid {
            None => Err(AgentReadDenial::NoUserIdentity),
            // The synthetic `system` identity may inspect any per-user
            // path (audit/sweeper jobs).
            Some("system") => Ok(()),
            Some(active) if active == uid => Ok(()),
            Some(_) => Err(AgentReadDenial::CrossUser),
        }
    } else {
        Ok(())
    }
}

pub use workforce_common::vault_scope::{is_desk_link, DESK_LINK_PREFIXES};

/// The fields that say where a connection sends its key: its address, and
/// a mailbox's servers and how it reaches them.
const ADDRESS_FIELDS: &[(&str, &[&str])] = &[
    ("github", &["base_url"]),
    ("jira", &["site"]),
    ("datadog", &["site", "base_url"]),
    ("metabase", &["site_url"]),
    (
        "email",
        &["imap_host", "smtp_host", "imap_port", "smtp_port", "tls"],
    ),
];

/// Whether `path` (shared or under `users/{uid}/`) says where one of Desk's
/// connections sends its key: flat (`skills/jira/site`) or an instance's
/// (`skills/jira/workspaces/acme/site`).
pub fn is_connection_address(path: &str) -> bool {
    let logical = path
        .strip_prefix("users/")
        .and_then(|rest| rest.split_once('/'))
        .map_or(path, |(_, rest)| rest);
    let Some(rest) = logical.strip_prefix("skills/") else {
        return false;
    };
    let Some((connector, tail)) = rest.split_once('/') else {
        return false;
    };
    let Some((_, fields)) = ADDRESS_FIELDS.iter().find(|(c, _)| *c == connector) else {
        return false;
    };
    let field = match tail.rsplit_once('/') {
        // An instance's: `<collection>/<key>/<field>`, one level deep.
        Some((within, field)) if within.split('/').count() == 2 => field,
        Some(_) => return false,
        None => tail,
    };
    fields.contains(&field)
}

/// [`agent_can_read`] for a tool that writes `path` (sets, deletes, or moves
/// or copies to or from it). On a hub running Desk (`desk`), where a
/// connection sends its key is never written from the agent vault tools.
pub fn agent_can_write(
    path: &str,
    current_uid: Option<&str>,
    desk: bool,
) -> Result<(), AgentReadDenial> {
    agent_can_read(path, current_uid)?;
    if desk && is_connection_address(path) {
        return Err(AgentReadDenial::ConnectionAddress);
    }
    Ok(())
}

/// Whether this hub runs Desk, whose connections the agent vault tools
/// leave to Connections.
pub fn desk_running() -> bool {
    #[cfg(feature = "postgres")]
    {
        crate::desk_connections::installed().is_some()
    }
    #[cfg(not(feature = "postgres"))]
    {
        false
    }
}

/// Whether `path` is a model account, `llm/accounts/...` shared or under
/// `users/{uid}/`, however its case or slashes are spelled.
pub fn is_llm_account(path: &str) -> bool {
    let segments: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect();
    let logical = match segments.first().map(String::as_str) {
        Some("users") => segments.get(2..).unwrap_or_default(),
        _ => &segments[..],
    };
    logical.first().map(String::as_str) == Some("llm")
        && logical.get(1).map(String::as_str) == Some("accounts")
}

/// [`agent_can_read`] for a tool that hands the value back: never a
/// subscription login's, whatever the redaction policy, since its token is
/// its owner's plan.
pub fn agent_can_read_value(path: &str, current_uid: Option<&str>) -> Result<(), AgentReadDenial> {
    agent_can_read(path, current_uid)?;
    match workforce_common::subscription::SubscriptionKind::of_vault_key(path) {
        Some(_) => Err(AgentReadDenial::Login),
        None => Ok(()),
    }
}

/// Human-readable rejection message shown when an agent attempts an
/// org-wide write.
pub fn org_wide_rejection_message(path: &str) -> String {
    format!(
        "Org-wide secret '{path}' cannot be set from an agent. The agent \
         vault is per-agent; this credential is shared across users and \
         must live in the hub's global vault. Set it from the hub host \
         instead:\n\n  \
         workforce vault set '{path}' '<value>'\n\n\
         Or via REST with the owner token:\n\n  \
         curl -X POST -H 'Authorization: Bearer $WF_TOKEN' \\\n       \
              -d '{{\"value\":\"<value>\"}}' \\\n       \
              https://<your-hub-host>/api/vault/{path}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Desk's links to other hubs are no agent's to read, list, write or
    /// move, shared or per-user, under any identity, `system` included.
    /// Which key or AWS profile pays is the person's: no agent reads,
    /// lists, writes or moves a model account through the vault tools.
    #[test]
    fn a_model_account_is_refused_to_every_agent() {
        for path in [
            "llm/accounts/bedrock",
            "llm/accounts/",
            "llm/accounts",
            "LLM/Accounts/bedrock",
            "llm//accounts/anthropic",
            "users/sam/llm/accounts/bedrock",
        ] {
            for who in [Some("sam"), Some("system"), None, Some("bob")] {
                assert_eq!(
                    agent_can_read(path, who),
                    Err(AgentReadDenial::LlmAccount),
                    "{path} as {who:?}"
                );
            }
        }
        for path in ["llm/accountsx", "llm/notes", "skills/llm/accounts"] {
            assert!(!is_llm_account(path), "{path}");
        }
    }

    #[test]
    fn a_desk_link_is_refused_to_every_agent() {
        for path in [
            "desk/hubs/atlas/remote_mcp",
            "users/sam/desk/hubs/atlas/remote_mcp",
            "users/sam/desk/links/atlas/ungrouped",
            "desk/links/atlas",
            "Desk/Hubs/atlas/remote_mcp",
            "users/sam/desk//hubs/atlas",
            "desk/hubs",
            "desk/links/bosses/6f1c",
            "users/sam/desk/links/bosses/6f1c",
            workforce_common::vault_scope::DESK_ENGINE_KEY,
        ] {
            for who in [Some("sam"), Some("system"), None, Some("bob")] {
                assert_eq!(
                    agent_can_read(path, who),
                    Err(AgentReadDenial::DeskLink),
                    "{path} as {who:?}"
                );
            }
        }
        for path in ["desk/notes", "users/sam/desk/hubsx/a", "skills/desk/hubs"] {
            assert_ne!(
                agent_can_read(path, Some("sam")),
                Err(AgentReadDenial::DeskLink),
                "{path}"
            );
        }
        assert!(AgentReadDenial::DeskLink
            .to_string()
            .contains("holds a Desk link to another hub"));
    }

    #[test]
    fn google_oauth_client_is_org_wide() {
        assert!(is_org_wide("skills/google/oauth_client/client_id"));
        assert!(is_org_wide("skills/google/oauth_client/client_secret"));
        assert!(is_org_wide("skills/google/oauth_client/redirect_uri"));
    }

    #[test]
    fn shared_only_covers_org_wide_oauth_apps_and_routing_selectors() {
        for path in [
            "skills/google/oauth_client/client_id",
            "skills/freeagent/oauth_client/client_secret",
            "claude_code/oauth_client/client_id",
            "skills/jira/default_workspace",
            "skills/metabase/default_instance",
            "skills/sendgrid/default_workspace",
        ] {
            assert!(is_shared_only(path), "{path}");
        }
        for path in [
            "skills/jira/api_token",
            "skills/jira/site",
            "skills/postmark/base_url",
            "skills/jira/default_project",
            "skills/freeagent/workspaces/default/refresh_token",
        ] {
            assert!(!is_shared_only(path), "{path}");
        }
        assert!(
            shared_only_rejection_message("skills/jira/default_workspace")
                .contains("skills/jira/default_workspace")
        );
    }

    #[test]
    fn database_credentials_denied_shared_and_per_user() {
        assert_eq!(
            agent_can_read("databases/instances/pw-prod/dsn", Some("alice")),
            Err(AgentReadDenial::DatabaseCredentials)
        );
        assert_eq!(
            agent_can_read("databases", Some("alice")),
            Err(AgentReadDenial::DatabaseCredentials)
        );
        // Even the user's own per-user override is off-limits to vault.get.
        assert_eq!(
            agent_can_read("users/alice/databases/instances/pw/dsn", Some("alice")),
            Err(AgentReadDenial::DatabaseCredentials)
        );
        // A lookalike that isn't the database namespace stays readable.
        assert!(agent_can_read("databases-notes/x", Some("alice")).is_ok());
    }

    #[test]
    fn http_endpoint_config_denied_shared_and_per_user() {
        assert_eq!(
            agent_can_read("http/endpoints/harbor-staging/token", Some("alice")),
            Err(AgentReadDenial::HttpEndpointCredentials)
        );
        assert_eq!(
            agent_can_read("http/endpoints/svc/creds/password", Some("alice")),
            Err(AgentReadDenial::HttpEndpointCredentials)
        );
        // The base URL is not itself a secret, but writing it would let the
        // agent point a configured credential at a host of its choosing, so the
        // whole namespace is closed to the vault tools.
        assert_eq!(
            agent_can_read("http/endpoints/svc/base_url", Some("alice")),
            Err(AgentReadDenial::HttpEndpointCredentials)
        );
        assert_eq!(
            agent_can_read("http", Some("alice")),
            Err(AgentReadDenial::HttpEndpointCredentials)
        );
        assert_eq!(
            agent_can_read("users/alice/http/endpoints/svc/token", Some("alice")),
            Err(AgentReadDenial::HttpEndpointCredentials)
        );
        // Lookalikes outside the namespace stay readable.
        assert!(agent_can_read("http-notes/x", Some("alice")).is_ok());
        assert!(agent_can_read("skills/http/x", Some("alice")).is_ok());
    }

    /// `vault.rename`/`vault.copy` gate on the same namespace predicate as the
    /// read/write tools. Without it, an agent could write an attacker host to a
    /// permitted path and rename it into `http/endpoints/<name>/base_url`,
    /// pairing an operator's credential with a host of its choosing — and the
    /// same move applied to `databases/`.
    #[test]
    fn credential_namespaces_are_denied_for_a_rename_or_copy_destination() {
        for path in [
            "http/endpoints/evil/base_url",
            "http/endpoints/evil/token",
            "databases/instances/evil/dsn",
        ] {
            assert!(
                agent_can_read(path, Some("alice")).is_err(),
                "{path} must not be a legal rename/copy endpoint"
            );
        }
        assert!(agent_can_read("notes/x", Some("alice")).is_ok());
    }

    #[test]
    fn http_denial_message_points_at_the_tools_not_the_vault() {
        let msg = AgentReadDenial::HttpEndpointCredentials.to_string();
        assert!(msg.contains("http.* tools"), "{msg}");
        assert!(msg.contains("operator"), "{msg}");
    }

    #[test]
    fn claude_code_oauth_client_is_org_wide() {
        assert!(is_org_wide("claude_code/oauth_client/client_id"));
        assert!(is_org_wide("claude_code/oauth_client/anything"));
    }

    #[test]
    fn per_user_google_tokens_are_not_org_wide() {
        assert!(!is_org_wide(
            "skills/google/workspaces/default/refresh_token"
        ));
        assert!(!is_org_wide(
            "skills/google/workspaces/default/access_token"
        ));
        assert!(!is_org_wide("skills/google/workspaces/alice/email"));
    }

    #[test]
    fn slack_workspace_tokens_are_not_org_wide() {
        // Per-agent today; explicitly NOT blocked.
        assert!(!is_org_wide("slack.T0123ABCD.bot_token"));
        assert!(!is_org_wide("slack.T0123ABCD.app_token"));
        assert!(!is_org_wide("slack.T0123ABCD.signing_secret"));
    }

    #[test]
    fn default_workspace_pointers_are_not_org_wide() {
        // Per-agent / per-user; explicitly NOT blocked.
        assert!(!is_org_wide("skills/google/default_workspace"));
        assert!(!is_org_wide("skills/jira/default_workspace"));
        assert!(!is_org_wide("skills/linear/default_workspace"));
    }

    #[test]
    fn per_user_skill_creds_are_not_org_wide() {
        assert!(!is_org_wide("skills/jira/api_token"));
        assert!(!is_org_wide("skills/linear/workspaces/default/api_key"));
        assert!(!is_org_wide("claude_code/oauth_token"));
    }

    #[test]
    fn unrelated_paths_pass_through() {
        assert!(!is_org_wide("llm/active_model"));
        assert!(!is_org_wide("llm/tokens/anthropic"));
        assert!(!is_org_wide("engagement/overrides"));
        assert!(!is_org_wide(""));
    }

    #[test]
    fn rejection_message_mentions_path_and_cli() {
        let msg = org_wide_rejection_message("skills/google/oauth_client/client_id");
        assert!(msg.contains("skills/google/oauth_client/client_id"));
        assert!(msg.contains("workforce vault set"));
        assert!(msg.contains("/api/vault/"));
    }

    #[test]
    fn agent_can_read_blocks_wasm_skills() {
        let err =
            agent_can_read("wasm-skills/orbit/users/alice/api_key", Some("alice")).unwrap_err();
        assert_eq!(err, AgentReadDenial::GuestPrivateNamespace);
    }

    #[test]
    fn agent_can_read_allows_self_user_path() {
        assert!(agent_can_read("users/alice/skills/jira/api_key", Some("alice")).is_ok());
    }

    #[test]
    fn agent_can_read_blocks_cross_user_path() {
        let err = agent_can_read("users/alice/skills/jira/api_key", Some("bob")).unwrap_err();
        assert_eq!(err, AgentReadDenial::CrossUser);
    }

    #[test]
    fn agent_can_read_requires_identity_for_user_paths() {
        let err = agent_can_read("users/alice/skills/jira/api_key", None).unwrap_err();
        assert_eq!(err, AgentReadDenial::NoUserIdentity);
    }

    #[test]
    fn agent_can_read_system_identity_can_see_any_user() {
        assert!(agent_can_read("users/alice/skills/jira/api_key", Some("system")).is_ok());
        assert!(agent_can_read("users/bob/skills/jira/api_key", Some("system")).is_ok());
    }

    #[test]
    fn agent_can_read_passes_through_non_user_paths() {
        assert!(agent_can_read("skills/jira/workspaces/acme/api_key", Some("alice")).is_ok());
        assert!(agent_can_read("skills/jira/default_workspace", None).is_ok());
        assert!(agent_can_read("llm/active_model", None).is_ok());
    }

    #[test]
    fn agent_can_read_rejects_malformed_users_path() {
        let err = agent_can_read("users/", Some("alice")).unwrap_err();
        assert_eq!(err, AgentReadDenial::CrossUser);
    }

    #[test]
    fn agent_can_read_rejects_dot_dot_segments() {
        // `users/alice/../bob/...` would pass a naive prefix check but
        // the first uid segment is `alice`, hiding bob's tree behind a
        // traversal. Reject defensively.
        let err = agent_can_read("users/alice/../bob/api_key", Some("alice")).unwrap_err();
        assert_eq!(err, AgentReadDenial::CrossUser);
    }

    #[test]
    fn agent_can_read_rejects_dot_dot_in_non_users_path() {
        let err = agent_can_read("skills/jira/../google/api_key", Some("alice")).unwrap_err();
        assert_eq!(err, AgentReadDenial::CrossUser);
    }

    #[test]
    fn on_desk_an_agent_never_writes_where_a_connection_sends_its_key() {
        for path in [
            "skills/jira/site",
            "skills/jira/workspaces/acme/site",
            "users/sam/skills/jira/workspaces/acme/site",
            "skills/github/workspaces/acme/base_url",
            "skills/datadog/accounts/prod/base_url",
            "skills/datadog/site",
            "skills/metabase/instances/acme/site_url",
            "users/sam/skills/email/accounts/work/smtp_host",
        ] {
            assert!(is_connection_address(path), "{path}");
            assert_eq!(
                agent_can_write(path, Some("sam"), true),
                Err(AgentReadDenial::ConnectionAddress),
                "{path}"
            );
            // A hub without Desk: as before.
            assert_eq!(agent_can_write(path, Some("sam"), false), Ok(()), "{path}");
        }
        for path in [
            "skills/jira/workspaces/acme/api_token",
            "skills/jira/workspaces/acme/label",
            "skills/linear/workspaces/acme/api_key",
            "skills/jira/workspaces/acme/nested/site",
            "skills/postmark/workspaces/acme/base_url",
        ] {
            assert!(!is_connection_address(path), "{path}");
            assert_eq!(agent_can_write(path, Some("sam"), true), Ok(()), "{path}");
        }
        // Another person's is still another person's.
        assert_eq!(
            agent_can_write("users/ben/skills/jira/site", Some("sam"), true),
            Err(AgentReadDenial::CrossUser)
        );
    }
}
