//! Serialisable view types for vault REST endpoints.
//!
//! Both the per-user (`/api/me/vault`) and admin (`/api/vault`,
//! `/api/agents/{id}/vault`) surfaces project [`workforce_common::settings::
//! VaultEntry`] into one of these shapes. Centralising the projection keeps
//! the JSON contract consistent across surfaces and makes the
//! identity-store enrichment a single function rather than a copy.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use workforce_common::settings::{SecretStore, VaultEntry, VaultSource};

use crate::vault_audit::value_fingerprint;

/// Single entry as projected for any vault REST response. `owner_display`
/// is the resolved [`workforce_identity::types::Identity::display_name`]
/// when `created_by`/`updated_by` matches a `user:<uuid>` tag, otherwise
/// a fixed label (`System`, `CLI`, etc.) — see [`resolve_actor_display`].
/// `agent_id` identifies which agent's vault this entry lives in for T1/T3
/// tiers; `None` for T2/T4 (global vault).
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct VaultEntryView {
    pub path: String,
    pub tier: String,
    pub fingerprint: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub created_by: String,
    pub updated_by: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by_display: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_by_display: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

impl VaultEntryView {
    /// Project one entry. `agent_id` is `Some(_)` for per-agent tiers (T1/T3)
    /// and `None` for global tiers (T2/T4). The display-resolver closure can
    /// be a no-op when owner enrichment isn't needed (per-user views always
    /// know the owner is the active user).
    pub fn from_entry(
        path: String,
        tier: VaultSource,
        entry: VaultEntry,
        resolve_display: &(dyn Fn(&str) -> Option<String> + Send + Sync),
    ) -> Self {
        Self::from_entry_with_agent(path, tier, None, entry, resolve_display)
    }

    pub fn from_entry_with_agent(
        path: String,
        tier: VaultSource,
        agent_id: Option<String>,
        entry: VaultEntry,
        resolve_display: &(dyn Fn(&str) -> Option<String> + Send + Sync),
    ) -> Self {
        Self {
            path,
            tier: tier.label().to_string(),
            fingerprint: value_fingerprint(&entry.value),
            created_at: entry.created_at,
            updated_at: entry.updated_at,
            created_by_display: resolve_display(&entry.created_by),
            updated_by_display: resolve_display(&entry.updated_by),
            created_by: entry.created_by,
            updated_by: entry.updated_by,
            agent_id,
        }
    }
}

/// Build the flat entry list for a vault REST listing across every tier of
/// `store`. Caller passes a filter closure to scope (e.g. user-only views
/// drop shared tiers; admin views keep everything).
pub fn list_all_tiers(
    store: &dyn SecretStore,
    prefix: Option<&str>,
    tier_filter: impl Fn(VaultSource) -> bool,
    resolve_display: &(dyn Fn(&str) -> Option<String> + Send + Sync),
) -> Vec<VaultEntryView> {
    VaultSource::all()
        .into_iter()
        .filter(|t| tier_filter(*t))
        .flat_map(|tier| {
            store
                .list_at_tier(prefix, tier)
                .into_iter()
                .map(move |(path, entry)| {
                    VaultEntryView::from_entry(path, tier, entry, resolve_display)
                })
        })
        .collect()
}

/// Resolve a `VaultEntry::created_by`/`updated_by` actor tag to a
/// display-friendly label. The identity-store branch is delegated so this
/// module doesn't depend on the identity crate directly.
pub fn resolve_actor_display(
    actor: &str,
    identity_lookup: &(dyn Fn(uuid::Uuid) -> Option<String> + Send + Sync),
) -> Option<String> {
    match actor {
        "cli" => Some("Hub CLI".to_string()),
        "rest:owner-token" => Some("Admin (REST owner token)".to_string()),
        "legacy" => Some("Pre-2026 migration".to_string()),
        a if a.starts_with("system:") => {
            Some(format!("System ({})", a.trim_start_matches("system:")))
        }
        a if a.starts_with("agent:") => Some(format!("Agent ({})", a.trim_start_matches("agent:"))),
        a if a.starts_with("user:") => uuid::Uuid::parse_str(a.trim_start_matches("user:"))
            .ok()
            .and_then(identity_lookup),
        _ => None,
    }
}

/// How each actor tag reads: [`resolve_actor_display`] over `store`'s
/// identities.
pub fn actor_display(
    store: Option<Arc<dyn workforce_identity::store::IdentityStore>>,
) -> impl Fn(&str) -> Option<String> {
    let lookup = identity_lookup(store);
    move |actor| resolve_actor_display(actor, lookup.as_ref())
}

/// Convenience: build the closure for [`resolve_actor_display`] from an
/// optional `IdentityStore`. Returns a closure that always returns `None`
/// when the store handle is unset.
pub fn identity_lookup(
    store: Option<Arc<dyn workforce_identity::store::IdentityStore>>,
) -> Arc<dyn Fn(uuid::Uuid) -> Option<String> + Send + Sync> {
    Arc::new(move |uid: uuid::Uuid| {
        store
            .as_ref()
            .and_then(|s| s.get_by_id(uid).ok().flatten())
            .map(|i| i.display_name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_config::LayeredUserVault;
    use crate::vault::Vault;
    use workforce_agent::runner::CURRENT_USER_ID;
    use workforce_common::settings::VaultActor;

    fn new_vault() -> Arc<Vault> {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("vault.enc");
        std::mem::forget(tmp);
        Arc::new(Vault::open(&path, "test-key").unwrap())
    }

    #[test]
    fn resolve_actor_display_fixed_labels() {
        let noop: Arc<dyn Fn(uuid::Uuid) -> Option<String> + Send + Sync> = Arc::new(|_| None);
        assert_eq!(
            resolve_actor_display("cli", noop.as_ref()).as_deref(),
            Some("Hub CLI")
        );
        assert_eq!(
            resolve_actor_display("rest:owner-token", noop.as_ref()).as_deref(),
            Some("Admin (REST owner token)")
        );
        assert_eq!(
            resolve_actor_display("legacy", noop.as_ref()).as_deref(),
            Some("Pre-2026 migration")
        );
        assert_eq!(
            resolve_actor_display("system:slack_sync", noop.as_ref()).as_deref(),
            Some("System (slack_sync)")
        );
        assert_eq!(
            resolve_actor_display("agent:atlas", noop.as_ref()).as_deref(),
            Some("Agent (atlas)")
        );
    }

    #[test]
    fn resolve_actor_display_user_via_lookup() {
        let lookup: Arc<dyn Fn(uuid::Uuid) -> Option<String> + Send + Sync> =
            Arc::new(|_| Some("Kingsley H".to_string()));
        let uid = uuid::Uuid::new_v4().to_string();
        let actor = format!("user:{uid}");
        assert_eq!(
            resolve_actor_display(&actor, lookup.as_ref()).as_deref(),
            Some("Kingsley H")
        );
    }

    #[tokio::test]
    async fn list_all_tiers_projects_each_quadrant() {
        let agent = new_vault();
        let global = new_vault();
        agent
            .set_with_actor("users/u/skills/jira/api_token", "t1", VaultActor::Cli)
            .unwrap();
        global
            .set_with_actor("users/u/skills/jira/email", "t2", VaultActor::Cli)
            .unwrap();
        agent
            .set_with_actor("skills/jira/default_workspace", "t3", VaultActor::Cli)
            .unwrap();
        global
            .set_with_actor("mcp/v1/hubspot/client_id", "t4", VaultActor::Cli)
            .unwrap();
        let store: Arc<dyn SecretStore> = Arc::new(LayeredUserVault::new(agent, Some(global)));
        let noop_display: Arc<dyn Fn(&str) -> Option<String> + Send + Sync> = Arc::new(|_| None);

        let entries = CURRENT_USER_ID
            .scope("u".to_string(), async {
                list_all_tiers(store.as_ref(), None, |_| true, noop_display.as_ref())
            })
            .await;
        let by_tier: std::collections::HashMap<_, Vec<_>> =
            entries
                .into_iter()
                .fold(std::collections::HashMap::new(), |mut acc, e| {
                    acc.entry(e.tier.clone()).or_default().push(e);
                    acc
                });
        assert_eq!(
            by_tier.get("agent_per_user").unwrap()[0].path,
            "skills/jira/api_token"
        );
        assert_eq!(
            by_tier.get("global_per_user").unwrap()[0].path,
            "skills/jira/email"
        );
        assert_eq!(
            by_tier.get("agent_specific").unwrap()[0].path,
            "skills/jira/default_workspace"
        );
        assert_eq!(
            by_tier.get("global").unwrap()[0].path,
            "mcp/v1/hubspot/client_id"
        );
    }
}
