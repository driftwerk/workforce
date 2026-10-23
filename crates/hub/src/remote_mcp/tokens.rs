//! Minting, listing and revoking remote MCP tokens, plus the connect lines a
//! user pastes into Claude Code or `workforce-local-mcp`. One service backs
//! the `cmd: remote-mcp` command and the `/api/me/remote-mcp-tokens` API so
//! the two can never disagree.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use workforce_common::settings::RemoteMcpConfig;
use workforce_identity::remote_mcp_token::{self, RemoteMcpToken, RemoteMcpTokenStore};
use workforce_identity::store::IdentityStore;
use workforce_identity::types::IdentityStatus;

use crate::gateway::GatewayState;
use crate::serve::AgentService;

pub const TOKEN_SCOPE: &str = "remote_mcp";
const LABEL_MAX_CHARS: usize = 64;
/// Stands in for `[hub].public_url` in any URL shown before an operator has
/// set it, so what a user copies is visibly incomplete rather than wrong.
const PUBLIC_URL_PLACEHOLDER: &str = "https://<hub.public_url>";

/// The base a public URL is built on: `[hub].public_url` without a trailing
/// slash, or the placeholder while it is unset.
pub(crate) fn public_base_url(public_url: Option<&str>) -> &str {
    public_url.map_or(PUBLIC_URL_PLACEHOLDER, |base| base.trim_end_matches('/'))
}

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("remote MCP is not enabled on this install (`[remote_mcp] enabled = true`)")]
    Disabled,
    #[error("label must be 1–{LABEL_MAX_CHARS} printable characters")]
    InvalidLabel,
    #[error("only an active identity can hold remote MCP tokens")]
    IdentityNotActive,
    #[error("you already hold {0} live tokens; revoke one first")]
    TokenCap(u32),
    #[error("token store error: {0}")]
    Store(#[from] eyre::Report),
}

/// The two commands that connect a client to this hub. `public_url_known` is
/// `false` when the install has no `hub.public_url`, in which case the lines
/// carry a placeholder the user must replace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectInstructions {
    pub claude_code: String,
    pub workforce_local: String,
    pub public_url_known: bool,
}

#[derive(Clone)]
pub struct Minted {
    pub plaintext: String,
    pub record: RemoteMcpToken,
    pub connect: ConnectInstructions,
}

impl std::fmt::Debug for Minted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Minted")
            .field("record", &self.record)
            .finish_non_exhaustive()
    }
}

fn valid_label(label: &str) -> bool {
    let chars = label.chars().count();
    (1..=LABEL_MAX_CHARS).contains(&chars) && !label.chars().any(char::is_control)
}

/// Short name of this install for the client-side server name, derived from
/// the public host (`workforce.harbor.example` → `harbor`).
pub fn install_name(public_url: Option<&str>) -> String {
    let host = public_url
        .and_then(|u| url::Url::parse(u).ok())
        .and_then(|u| u.host_str().map(str::to_owned));
    let label = host
        .as_deref()
        .map(|h| h.split('.').collect::<Vec<_>>())
        .and_then(|labels| match labels.as_slice() {
            ["workforce", name, _, ..] => Some(*name),
            [name, ..] => Some(*name),
            [] => None,
        })
        .unwrap_or("hub");
    let slug: String = label
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    slug.trim_matches('-').to_string()
}

pub fn connect_instructions(public_url: Option<&str>, plaintext: &str) -> ConnectInstructions {
    let base = public_base_url(public_url);
    let install = install_name(public_url);
    ConnectInstructions {
        claude_code: format!(
            "claude mcp add --transport http workforce-{install} {base}/mcp --header \"Authorization: Bearer {plaintext}\""
        ),
        workforce_local: format!(
            "workforce-local-mcp config workforce add {install} --url {base} --token {plaintext}"
        ),
        public_url_known: public_url.is_some(),
    }
}

pub struct RemoteMcpTokenService {
    store: Arc<dyn RemoteMcpTokenStore>,
    identities: Arc<dyn IdentityStore>,
    config: RemoteMcpConfig,
    public_url: Option<String>,
}

impl RemoteMcpTokenService {
    pub fn new(
        store: Arc<dyn RemoteMcpTokenStore>,
        identities: Arc<dyn IdentityStore>,
        config: RemoteMcpConfig,
        public_url: Option<String>,
    ) -> Self {
        Self {
            store,
            identities,
            config,
            public_url,
        }
    }

    /// The service an agent's `cmd:` path uses; `None` until the CLI has bound
    /// the token store.
    pub fn for_agent(service: &AgentService) -> Option<Self> {
        Some(Self::new(
            service.remote_mcp_tokens.read().clone()?,
            service.identity_store.read().clone()?,
            service.config.remote_mcp.clone(),
            service.config.hub.public_url.clone(),
        ))
    }

    /// The service the REST API uses; `None` when the gateway has no identity
    /// store (and therefore no token store).
    pub fn for_gateway(state: &GatewayState) -> Option<Self> {
        Some(Self::new(
            state.remote_mcp_tokens.clone()?,
            state.identity_store.clone()?,
            state.remote_mcp.clone(),
            state
                .registry
                .default_agent()
                .and_then(|svc| svc.config.hub.public_url.clone()),
        ))
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    pub fn config(&self) -> &RemoteMcpConfig {
        &self.config
    }

    pub fn store(&self) -> &Arc<dyn RemoteMcpTokenStore> {
        &self.store
    }

    pub fn connect_instructions(&self, plaintext: &str) -> ConnectInstructions {
        connect_instructions(self.public_url.as_deref(), plaintext)
    }

    fn require_enabled(&self) -> Result<(), TokenError> {
        self.enabled().then_some(()).ok_or(TokenError::Disabled)
    }

    fn require_active(&self, identity_id: Uuid) -> Result<(), TokenError> {
        self.identities
            .get_by_id(identity_id)?
            .filter(|i| i.status == IdentityStatus::Active)
            .map(|_| ())
            .ok_or(TokenError::IdentityNotActive)
    }

    pub fn mint(&self, identity_id: Uuid, label: &str) -> Result<Minted, TokenError> {
        self.require_enabled()?;
        self.require_active(identity_id)?;
        let label = label.trim();
        if !valid_label(label) {
            return Err(TokenError::InvalidLabel);
        }
        let now = Utc::now();
        let cap = self.config.max_tokens_per_user;
        if self
            .store
            .count_live_tokens_for_identity(identity_id, now)?
            >= cap as usize
        {
            return Err(TokenError::TokenCap(cap));
        }
        let minted = remote_mcp_token::mint();
        let record = RemoteMcpToken {
            id: Uuid::new_v4(),
            identity_id,
            label: label.to_string(),
            token_hash: minted.hash,
            token_prefix: minted.prefix,
            scopes: vec![TOKEN_SCOPE.to_string()],
            created_at: now,
            expires_at: now + Duration::days(i64::from(self.config.token_ttl_days)),
            last_used_at: None,
            revoked_at: None,
        };
        self.store.create_token(&record)?;
        tracing::info!(
            target: "sentinel.audit",
            identity_id = %identity_id,
            token_id = %record.id,
            label = %record.label,
            expires_at = %record.expires_at,
            "remote MCP token minted"
        );
        Ok(Minted {
            connect: self.connect_instructions(&minted.plaintext),
            plaintext: minted.plaintext,
            record,
        })
    }

    pub fn list(&self, identity_id: Uuid) -> Result<Vec<RemoteMcpToken>, TokenError> {
        self.require_enabled()?;
        Ok(self.store.list_tokens_for_identity(identity_id)?)
    }

    pub fn revoke(&self, identity_id: Uuid, id: Uuid) -> Result<bool, TokenError> {
        self.require_enabled()?;
        let revoked = self.store.revoke_token(id, Some(identity_id), Utc::now())?;
        if revoked {
            tracing::info!(target: "sentinel.audit", identity_id = %identity_id, token_id = %id, "remote MCP token revoked");
        }
        Ok(revoked)
    }

    pub fn list_all(&self) -> Result<Vec<RemoteMcpToken>, TokenError> {
        self.require_enabled()?;
        Ok(self.store.list_all_tokens()?)
    }

    pub fn revoke_any(&self, id: Uuid) -> Result<bool, TokenError> {
        self.require_enabled()?;
        let revoked = self.store.revoke_token(id, None, Utc::now())?;
        if revoked {
            tracing::info!(target: "sentinel.audit", token_id = %id, "remote MCP token revoked by admin");
        }
        Ok(revoked)
    }

    pub fn display_name(&self, identity_id: Uuid) -> Option<String> {
        self.identities
            .get_by_id(identity_id)
            .ok()
            .flatten()
            .map(|i| i.display_name)
    }
}

pub fn is_expired(token: &RemoteMcpToken, now: DateTime<Utc>) -> bool {
    now >= token.expires_at
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use workforce_identity::sqlite::SqliteIdentityStore;
    use workforce_identity::store::IdentityAndTeamStore;
    use workforce_identity::types::{Identity, IdentityRole, ProvisioningSource};

    pub(crate) fn identity(status: IdentityStatus) -> Identity {
        let now = Utc::now();
        Identity {
            id: Uuid::new_v4(),
            display_name: "Kingsley".into(),
            email: None,
            password_hash: None,
            role: IdentityRole::Member,
            role_key: "member".into(),
            status,
            provisioning_source: ProvisioningSource::Manual,
            linked_ids: Default::default(),
            created_at: now,
            updated_at: now,
        }
    }

    pub(crate) fn service(
        config: RemoteMcpConfig,
        public_url: Option<&str>,
    ) -> (RemoteMcpTokenService, Arc<SqliteIdentityStore>) {
        let store = Arc::new(SqliteIdentityStore::in_memory().unwrap());
        let service = RemoteMcpTokenService::new(
            store.clone().as_remote_mcp_token_store(),
            store.clone().as_identity_store(),
            config,
            public_url.map(str::to_owned),
        );
        (service, store)
    }

    pub(crate) fn enabled_config() -> RemoteMcpConfig {
        RemoteMcpConfig {
            enabled: true,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn install_name_strips_the_workforce_label_and_falls_back_to_hub() {
        assert_eq!(
            install_name(Some("https://workforce.harbor.example")),
            "harbor"
        );
        assert_eq!(install_name(Some("https://workforce.acme.dev/")), "acme");
        assert_eq!(
            install_name(Some("https://Blue_Sky.example.com")),
            "blue-sky"
        );
        assert_eq!(install_name(Some("not a url")), "hub");
        assert_eq!(install_name(None), "hub");
    }

    #[test]
    fn connect_instructions_render_both_commands() {
        let c = connect_instructions(Some("https://workforce.acme.dev/"), "wfr_abc");
        assert_eq!(
            c.claude_code,
            "claude mcp add --transport http workforce-acme https://workforce.acme.dev/mcp --header \"Authorization: Bearer wfr_abc\""
        );
        assert_eq!(
            c.workforce_local,
            "workforce-local-mcp config workforce add acme --url https://workforce.acme.dev --token wfr_abc"
        );
        assert!(c.public_url_known);

        let unknown = connect_instructions(None, "wfr_abc");
        assert!(!unknown.public_url_known);
        assert!(unknown.claude_code.contains(PUBLIC_URL_PLACEHOLDER));
    }

    #[test]
    fn mint_requires_the_feature_and_an_active_identity() {
        let (disabled, store) = service(RemoteMcpConfig::default(), None);
        let active = identity(IdentityStatus::Active);
        store.create(&active).unwrap();
        assert!(matches!(
            disabled.mint(active.id, "laptop"),
            Err(TokenError::Disabled)
        ));

        let (enabled, store) = service(enabled_config(), None);
        assert!(matches!(
            enabled.mint(Uuid::new_v4(), "laptop"),
            Err(TokenError::IdentityNotActive)
        ));
        let suspended = identity(IdentityStatus::Suspended);
        store.create(&suspended).unwrap();
        assert!(matches!(
            enabled.mint(suspended.id, "laptop"),
            Err(TokenError::IdentityNotActive)
        ));
    }

    #[test]
    fn mint_validates_labels_and_enforces_the_live_cap() {
        let config = RemoteMcpConfig {
            max_tokens_per_user: 2,
            ..enabled_config()
        };
        let (svc, store) = service(config, Some("https://workforce.acme.dev"));
        let me = identity(IdentityStatus::Active);
        store.create(&me).unwrap();

        assert!(matches!(
            svc.mint(me.id, "   "),
            Err(TokenError::InvalidLabel)
        ));
        assert!(matches!(
            svc.mint(me.id, &"x".repeat(65)),
            Err(TokenError::InvalidLabel)
        ));
        assert!(matches!(
            svc.mint(me.id, "lap\ntop"),
            Err(TokenError::InvalidLabel)
        ));

        let first = svc.mint(me.id, "  laptop ").unwrap();
        assert!(!format!("{first:?}").contains("wfr_"));
        assert_eq!(first.record.label, "laptop");
        assert_eq!(first.record.scopes, [TOKEN_SCOPE]);
        assert!(first.plaintext.starts_with(remote_mcp_token::TOKEN_PREFIX));
        assert!(first.connect.claude_code.contains(&first.plaintext));
        assert_eq!(
            first.record.expires_at - first.record.created_at,
            Duration::days(90)
        );
        svc.mint(me.id, "ci").unwrap();
        assert!(matches!(
            svc.mint(me.id, "third"),
            Err(TokenError::TokenCap(2))
        ));

        assert!(svc.revoke(me.id, first.record.id).unwrap());
        assert!(svc.mint(me.id, "third").is_ok());
    }

    #[test]
    fn revoke_is_owner_scoped_and_admin_revoke_is_not() {
        let (svc, store) = service(enabled_config(), None);
        let me = identity(IdentityStatus::Active);
        let other = identity(IdentityStatus::Active);
        store.create(&me).unwrap();
        store.create(&other).unwrap();
        let mine = svc.mint(me.id, "laptop").unwrap();

        assert!(!svc.revoke(other.id, mine.record.id).unwrap());
        assert_eq!(svc.list(me.id).unwrap().len(), 1);
        assert!(svc.revoke_any(mine.record.id).unwrap());
        assert!(svc.list(me.id).unwrap().is_empty());
        assert!(!svc.revoke_any(mine.record.id).unwrap());
        assert_eq!(svc.display_name(me.id).as_deref(), Some("Kingsley"));
    }
}
