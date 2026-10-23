use eyre::Result;
use uuid::Uuid;

use crate::types::{Identity, IdentityRole, IdentityStatus};

/// Filter for listing identities. An empty filter returns the first page of
/// every identity; `search` matches `display_name` or `email` case-insensitively.
#[derive(Debug, Clone, Default)]
pub struct IdentityFilter {
    pub role: Option<IdentityRole>,
    pub status: Option<IdentityStatus>,
    pub search: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

/// Trait for identity persistence.
pub trait IdentityStore: Send + Sync {
    fn create(&self, identity: &Identity) -> Result<()>;
    fn get_by_id(&self, id: Uuid) -> Result<Option<Identity>>;
    fn get_by_email(&self, email: &str) -> Result<Option<Identity>>;
    fn get_by_linked_id(&self, platform: &str, platform_id: &str) -> Result<Option<Identity>>;
    fn update(&self, identity: &Identity) -> Result<()>;
    fn delete(&self, id: Uuid) -> Result<()>;
    fn list(&self, filter: &IdentityFilter) -> Result<Vec<Identity>>;

    /// Total identities matching `filter`, ignoring its `limit`/`offset` — the
    /// page total for paginated listings.
    fn count_filtered(&self, filter: &IdentityFilter) -> Result<usize>;

    /// Total identities across the store.
    fn count(&self) -> Result<usize> {
        self.count_filtered(&IdentityFilter::default())
    }
}

/// Combined identity + team store.
///
/// Both `SqliteIdentityStore` and `PostgresIdentityStore` implement
/// every method on both [`IdentityStore`] and
/// [`crate::team::TeamStore`], so consumers that need both can hold
/// a single `Arc<dyn IdentityAndTeamStore>` instead of two separate
/// trait objects pointing at the same backend.
pub trait IdentityAndTeamStore:
    IdentityStore
    + crate::team::TeamStore
    + crate::remote_mcp_token::RemoteMcpTokenStore
    + crate::lifecycle::LifecycleOpStore
{
    /// Upcast to a bare `IdentityStore` trait object. The blanket
    /// `Arc::clone` + `as`-cast doesn't work across two unrelated
    /// trait objects, so each backend provides this explicitly.
    fn as_identity_store(self: std::sync::Arc<Self>) -> std::sync::Arc<dyn IdentityStore>;

    /// Upcast to a bare `TeamStore` trait object.
    fn as_team_store(self: std::sync::Arc<Self>) -> std::sync::Arc<dyn crate::team::TeamStore>;

    /// Upcast to a bare `RemoteMcpTokenStore` trait object.
    fn as_remote_mcp_token_store(
        self: std::sync::Arc<Self>,
    ) -> std::sync::Arc<dyn crate::remote_mcp_token::RemoteMcpTokenStore>;

    /// Upcast to a bare `LifecycleOpStore` trait object.
    fn as_lifecycle_op_store(
        self: std::sync::Arc<Self>,
    ) -> std::sync::Arc<dyn crate::lifecycle::LifecycleOpStore>;
}
