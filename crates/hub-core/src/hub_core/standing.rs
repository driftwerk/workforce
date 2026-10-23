//! Whether anything may run as a person now. Read from the identity store at
//! every check, so a suspension or a reactivation takes effect on every
//! replica whatever path changed the status: a reactivation at once, a
//! suspension within [`STANDING_TTL`]. Only a suspended identity is refused:
//! an auto-provisioned one runs as before, and a name that is not an identity
//! id (a platform id, a sentinel, `system:*`) is left to the checks that
//! already govern it.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use uuid::Uuid;
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityStatus};

/// How long a person read as not suspended is taken to stay so before the
/// store is asked again. Every tool call, turn, WebSocket frame and due job
/// asks, and a Postgres lookup under load costs milliseconds. A suspension is
/// never kept, so a reactivation is seen at once; one made on this replica is
/// [`forget`]-ed, so only one made elsewhere waits out the rest of this.
pub const STANDING_TTL: Duration = Duration::from_secs(5);

/// The people last read as not suspended, and when.
static NOT_SUSPENDED: LazyLock<parking_lot::Mutex<HashMap<Uuid, Instant>>> =
    LazyLock::new(Default::default);

/// Ask the store about `id` at the next check: its status changed here.
pub fn forget(id: Uuid) {
    NOT_SUSPENDED.lock().remove(&id);
}

fn read_recently(id: Uuid, now: Instant) -> bool {
    NOT_SUSPENDED
        .lock()
        .get(&id)
        .is_some_and(|at| now.duration_since(*at) < STANDING_TTL)
}

fn remember(id: Uuid, now: Instant) {
    let mut read = NOT_SUSPENDED.lock();
    if read.len() >= 4096 {
        read.retain(|_, at| now.duration_since(*at) < STANDING_TTL);
    }
    read.insert(id, now);
}

/// The suspended identity `principal` names, if it names one. A store error
/// or a missing identity reads as not suspended: a lookup outage must not
/// stop every person's work, and a deleted identity's holdings are the
/// lifecycle engine's to remove. An error is logged and never remembered.
pub fn suspended(store: Option<&Arc<dyn IdentityStore>>, principal: &str) -> Option<Identity> {
    let id = Uuid::parse_str(principal).ok()?;
    let store = store?;
    let now = Instant::now();
    if read_recently(id, now) {
        return None;
    }
    match store.get_by_id(id) {
        Ok(Some(identity)) if identity.status == IdentityStatus::Suspended => Some(identity),
        Ok(_) => {
            remember(id, now);
            None
        }
        Err(e) => {
            tracing::warn!(%id, error = %e, "Failed to look up whether a person is suspended; taking them to be in good standing");
            None
        }
    }
}

/// The suspended identity linked to `platform_id` on `platform`, for a
/// surface that knows the caller only by their platform id.
pub fn suspended_on(
    store: Option<&Arc<dyn IdentityStore>>,
    platform: &str,
    platform_id: &str,
) -> Option<Identity> {
    match store?.get_by_linked_id(platform, platform_id) {
        Ok(identity) => identity.filter(|i| i.status == IdentityStatus::Suspended),
        Err(e) => {
            tracing::warn!(%platform, error = %e, "Failed to look up whether a person is suspended");
            None
        }
    }
}

/// What a person, or the owner of the work, is told when something would
/// have run as a suspended identity.
pub fn refusal(identity: &Identity) -> String {
    format!(
        "{} is suspended, so nothing can run as them.",
        identity.display_name
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_identity::sqlite::SqliteIdentityStore;
    use workforce_identity::types::IdentityRole;

    fn store_with(status: IdentityStatus) -> (Arc<dyn IdentityStore>, Identity) {
        let store = SqliteIdentityStore::in_memory().unwrap();
        let identity = Identity::new("Ada", IdentityRole::Member).with_status(status);
        store.create(&identity).unwrap();
        (Arc::new(store), identity)
    }

    #[test]
    fn only_a_suspended_identity_id_is_suspended() {
        let (store, ada) = store_with(IdentityStatus::Suspended);
        assert_eq!(
            suspended(Some(&store), &ada.id.to_string()).map(|i| i.id),
            Some(ada.id)
        );
        assert!(suspended(Some(&store), "heartbeat").is_none());
        assert!(suspended(Some(&store), &uuid::Uuid::new_v4().to_string()).is_none());
        assert!(suspended(None, &ada.id.to_string()).is_none());
        for status in [IdentityStatus::Active, IdentityStatus::AutoProvisioned] {
            let (store, other) = store_with(status);
            assert!(suspended(Some(&store), &other.id.to_string()).is_none());
        }
    }

    #[test]
    fn a_reactivation_is_seen_at_once_and_a_suspension_made_elsewhere_within_the_ttl() {
        let (store, mut ada) = store_with(IdentityStatus::Active);
        let id = ada.id.to_string();
        let set = |ada: &mut Identity, status| {
            ada.status = status;
            store.update(ada).unwrap();
        };
        assert!(suspended(Some(&store), &id).is_none());

        set(&mut ada, IdentityStatus::Suspended);
        assert!(
            suspended(Some(&store), &id).is_none(),
            "suspended elsewhere: not yet read"
        );
        forget(ada.id);
        assert!(
            suspended(Some(&store), &id).is_some(),
            "suspended here: read at once"
        );

        set(&mut ada, IdentityStatus::Active);
        assert!(
            suspended(Some(&store), &id).is_none(),
            "a suspension is never kept"
        );
    }

    #[test]
    fn a_platform_id_names_a_suspended_person_only_through_their_link() {
        let store = SqliteIdentityStore::in_memory().unwrap();
        let ada = Identity::new("Ada", IdentityRole::Member)
            .with_status(IdentityStatus::Suspended)
            .with_linked_id("slack", "U1");
        let bo = Identity::new("Bo", IdentityRole::Member).with_linked_id("slack", "U2");
        store.create(&ada).unwrap();
        store.create(&bo).unwrap();
        let store: Arc<dyn IdentityStore> = Arc::new(store);
        assert_eq!(
            suspended_on(Some(&store), "slack", "U1").map(|i| i.id),
            Some(ada.id)
        );
        assert!(suspended_on(Some(&store), "slack", "U2").is_none());
        assert!(suspended_on(Some(&store), "slack", "U3").is_none());
        assert!(suspended_on(Some(&store), "teams", "U1").is_none());
    }

    #[test]
    fn the_refusal_names_the_person_in_one_sentence() {
        let (_, ada) = store_with(IdentityStatus::Suspended);
        assert_eq!(
            refusal(&ada),
            "Ada is suspended, so nothing can run as them."
        );
    }
}
