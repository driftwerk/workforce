//! The hub's own Claude calls (the overseer, the Floor's gists) on the
//! person's own hub run over the person's Claude accounts as Desk keeps
//! them ([`crate::hub_core::logins::install_pool`]): when one is at its
//! limit, the next serves, and each side tells the other what it found.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use uuid::Uuid;
use workforce_desk::Desk;

use crate::hub_core::logins::install_pool::{self, InstallPool, PoolAccount};
use crate::vault::Vault;
use workforce_identity::store::{IdentityFilter, IdentityStore};
use workforce_identity::types::IdentityStatus;

/// Desk's accounts, as the install tier's pool, for the one person who
/// holds the hub (its only administrator, whose Desk this is) alone.
pub struct DeskPool {
    desk: Desk,
    owner: Uuid,
}

impl InstallPool for DeskPool {
    fn claude_accounts(&self) -> Vec<PoolAccount> {
        self.desk
            .pool_accounts()
            .into_iter()
            .map(|(id, full_until)| PoolAccount {
                id: id.to_string(),
                full_until,
            })
            .collect()
    }

    fn token(&self, id: &str) -> Option<String> {
        let id = Uuid::parse_str(id).ok()?;
        self.desk.pool_token(id).map(|t| t.expose().to_string())
    }

    fn serves(&self, person: Uuid) -> bool {
        person == self.owner
    }

    fn cooling(&self, id: &str, until: DateTime<Utc>) {
        if let Ok(id) = Uuid::parse_str(id) {
            self.desk.hub_cooling(id, until);
        }
    }
}

/// Makes `desk`'s Claude accounts the install tier's pool on the hub whose
/// global vault is `vault`: only where Desk keeps accounts and the hub has
/// exactly one administrator, the person they then serve alone. A hub with
/// several administrators, or none, has no pool: whose accounts they would
/// be can't be told.
pub async fn install(vault: &Arc<Vault>, desk: &Desk, identities: Option<&Arc<dyn IdentityStore>>) {
    let (Some(_), Some(identities)) = (desk.accounts_store(), identities) else {
        return;
    };
    let Some(owner) = holder(identities).await else {
        return;
    };
    install_pool::set(
        vault,
        Some(Arc::new(DeskPool {
            desk: desk.clone(),
            owner,
        })),
    );
    tracing::info!("the hub's own Claude calls run over its holder's accounts");
}

/// The one person who holds the hub: its only active administrator. None
/// with several, none, or a store that can't say.
async fn holder(identities: &Arc<dyn IdentityStore>) -> Option<Uuid> {
    let store = Arc::clone(identities);
    let listed = tokio::task::spawn_blocking(move || {
        store.list(&IdentityFilter {
            status: Some(IdentityStatus::Active),
            ..Default::default()
        })
    })
    .await
    .ok()?;
    let admins: Vec<Uuid> = match listed {
        Ok(all) => all
            .into_iter()
            .filter(|i| crate::authz::is_effective_admin(i.role, i.status))
            .map(|i| i.id)
            .collect(),
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "couldn't tell who holds the hub; its own Claude calls stay on its own login");
            return None;
        }
    };
    match admins[..] {
        [owner] => Some(owner),
        _ => {
            tracing::info!(
                admins = admins.len(),
                "the hub has no single holder; its own Claude calls stay on its own login"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_identity::sqlite::SqliteIdentityStore;
    use workforce_identity::types::{Identity, IdentityRole};

    fn person(role: IdentityRole, status: IdentityStatus) -> Identity {
        let mut i = Identity::new("P", role);
        i.status = status;
        i
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_pool_serves_the_one_person_who_holds_the_hub_or_nobody() {
        let store: Arc<dyn IdentityStore> = Arc::new(SqliteIdentityStore::in_memory().unwrap());
        assert_eq!(holder(&store).await, None, "nobody holds it");
        let owner = person(IdentityRole::SystemAdmin, IdentityStatus::Active);
        store.create(&owner).unwrap();
        store
            .create(&person(IdentityRole::Member, IdentityStatus::Active))
            .unwrap();
        store
            .create(&person(
                IdentityRole::SystemAdmin,
                IdentityStatus::Suspended,
            ))
            .unwrap();
        assert_eq!(
            holder(&store).await,
            Some(owner.id),
            "members and a suspended admin aside"
        );
        store
            .create(&person(IdentityRole::SystemAdmin, IdentityStatus::Active))
            .unwrap();
        assert_eq!(
            holder(&store).await,
            None,
            "two hold it: whose accounts can't be told"
        );
    }
}
