//! Who a hub's logins are held for: the owner override and the vaults it can
//! reach.

use std::sync::Arc;

use workforce_common::settings::OwnerType;

use crate::vault::Vault;

/// An agent's owner: the configured one unless a runtime override is set.
pub type OwnerOverride = Arc<parking_lot::RwLock<Option<(Option<String>, OwnerType)>>>;

/// Every vault the hub holds, where a copy of a login may already sit.
pub type KnownVaults = Arc<dyn Fn() -> Vec<Arc<Vault>> + Send + Sync>;
