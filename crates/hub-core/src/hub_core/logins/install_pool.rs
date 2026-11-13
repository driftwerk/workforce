//! On the person's own hub, the install tier's Claude login is a pool: the
//! person's own Claude accounts, as Desk keeps them (the default first), so
//! the hub's own work (the overseer, the Floor's gists) moves to the next
//! account when one is at its limit, as the person's sessions do.
//!
//! - **Tokens stay where Desk keeps them.** A login resolved to a pool
//!   account names it only by its id ([`super::resolve::Credential::Pooled`]);
//!   its token is read from Desk's store (the keychain), off the runtime's
//!   threads, only as the call's `claude` CLI starts, and handed to that CLI
//!   alone. It is never written to a vault, logged, or kept.
//! - **The person's own calls alone.** The pool serves the person whose hub
//!   it is, and the hub's own background work for them marked so
//!   ([`hub_own_work`]); never another person, a team's agent, or an
//!   outside caller, and never where the policy refused the payer.
//! - **One standing.** An account Desk knows is full is skipped. An account
//!   that cools here (a call refused at its limit) is told back to Desk,
//!   with when it was refused, which then counts it full too. Desk's word
//!   is never cooled here in turn, so a stop can't keep itself alive by
//!   going round: it ends at its reset, when the account is signed in
//!   again, or when a call on it succeeds.
//! - **Signed in again, or served since.** A refusal heard before the
//!   account's last sign-in or last success ([`PoolAccount::cleared_at`])
//!   no longer cools it here.
//! - **Only where Desk keeps accounts.** No pool is registered on a hub
//!   without them, so the install tier is its vault login alone, as before.

use std::sync::{Arc, LazyLock, Weak};

use chrono::{DateTime, Utc};
use parking_lot::RwLock;

use crate::vault::Vault;

/// One of the person's Claude accounts, as the pool lists it: never its
/// token, which is read only for the account a call is resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolAccount {
    /// Desk's id for it: what its cooldown is kept and told back by.
    pub id: String,
    /// When Desk knows it is full: until then.
    pub full_until: Option<DateTime<Utc>>,
    /// When it was last signed in, or a call on it last succeeded: a
    /// refusal heard before then is over.
    pub cleared_at: Option<DateTime<Utc>>,
}

/// Where the person's Claude accounts are kept: Desk, on their own hub.
pub trait InstallPool: Send + Sync {
    /// The accounts, in the order a switch takes them (the default first),
    /// those whose sign-in can't be used left out.
    fn claude_accounts(&self) -> Vec<PoolAccount>;

    /// Account `id`'s token, read now from where Desk keeps it, for the one
    /// call it serves. It may block (a keychain read).
    fn token(&self, id: &str) -> Option<String>;

    /// Whether `person`'s calls may run on these accounts: the person whose
    /// hub and Desk this is, never anyone else on it.
    fn serves(&self, person: uuid::Uuid) -> bool;

    /// Account `id` is at its limit until `until`, as a call here found
    /// at `at`.
    fn cooling(&self, id: &str, until: DateTime<Utc>, at: DateTime<Utc>);

    /// A call here on account `id` succeeded: whatever stop it had is over.
    fn served(&self, _id: &str) {}

    /// The key account `id`'s plan readings are kept under, as the Floor
    /// names its card: a call's reading on it is the account's.
    fn plan_key(&self, _id: &str) -> Option<String> {
        None
    }
}

/// The key a pool account cools under: its id, as its token is never read
/// to list it.
pub fn login_key(id: &str) -> super::resolve::LoginKey {
    super::resolve::LoginKey {
        kind: workforce_common::subscription::SubscriptionKind::Claude,
        fingerprint: slot_key(id),
    }
}

/// A hub's pool, by the global vault its install tier reads.
type Registered = (Weak<Vault>, Arc<dyn InstallPool>);

/// Each hub's pool: a pool serves only the hub it was registered for.
static POOLS: LazyLock<RwLock<Vec<Registered>>> = LazyLock::new(|| RwLock::new(Vec::new()));

/// Makes `pool` the install tier's Claude login of the hub whose global
/// vault is `vault` (`None`: its vault login alone).
pub fn set(vault: &Arc<Vault>, pool: Option<Arc<dyn InstallPool>>) {
    let mut pools = POOLS.write();
    pools.retain(|(v, _)| v.strong_count() > 0 && !std::ptr::eq(v.as_ptr(), Arc::as_ptr(vault)));
    if let Some(pool) = pool {
        pools.push((Arc::downgrade(vault), pool));
    }
}

/// The pool of the hub whose global vault is `vault`, when it has one.
pub fn of(vault: &Arc<Vault>) -> Option<Arc<dyn InstallPool>> {
    POOLS
        .read()
        .iter()
        .find(|(v, _)| std::ptr::eq(v.as_ptr(), Arc::as_ptr(vault)))
        .map(|(_, pool)| Arc::clone(pool))
}

tokio::task_local! {
    /// Set over the hub's own background work on the person's hub (the
    /// overseer reading a session's reply, the Floor's gists): the only work
    /// with no person in scope the pool serves.
    static HUB_OWN_WORK: ();
}

/// Runs `work` as the hub's own work for the person whose hub this is: its
/// calls may run on the person's accounts ([`InstallPool`]).
pub async fn hub_own_work<F: std::future::Future>(work: F) -> F::Output {
    HUB_OWN_WORK.scope((), work).await
}

/// Whether the hub's own work for its person is under way here.
pub fn in_hub_own_work() -> bool {
    HUB_OWN_WORK.try_with(|_| ()).is_ok()
}

/// The token of pool account `id` on the hub whose global vault is `vault`,
/// read off the runtime's threads, for the CLI of the one call it serves.
pub async fn token_of(vault: &Arc<Vault>, id: &str) -> Option<String> {
    let pool = of(vault)?;
    let id = id.to_string();
    tokio::task::spawn_blocking(move || pool.token(&id))
        .await
        .ok()
        .flatten()
}

/// The vault key a pool account's login is known by: never a key that
/// holds anything.
pub fn slot_key(id: &str) -> String {
    format!("desk-account:{id}")
}

/// Whether `key` is a pool account's, and whose.
pub fn account_of(key: &str) -> Option<&str> {
    key.strip_prefix("desk-account:")
}
