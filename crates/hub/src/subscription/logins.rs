//! One agent's view of the subscription logins: its vaults, its owner and its
//! `[llm.logins]` policy, live across reloads and owner changes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use workforce_agent::runner::{CURRENT_SESSION_ID, CURRENT_SESSION_KEY};
use workforce_common::settings::OwnerType;
use workforce_common::subscription::{AgentOwner, Payer, SubscriptionKind};

use super::cooldown::CooldownStore;
use super::resolve::{holding, resolve, Login, NoLogin, Vaults};
use crate::def_write_auth::TeamStoreHandle;
use crate::resolved_config::ConfigCell;
use crate::vault::Vault;

pub use crate::hub_core::owners::OwnerOverride;

/// How long an owning team's members are trusted before they are read again.
const TEAM_MEMBERS_TTL: Duration = Duration::from_secs(60);

/// The owner as last looked up, and for which configured or overridden owner.
struct CachedOwner {
    of: (Option<String>, OwnerType),
    owner: AgentOwner,
    at: Instant,
}

impl CachedOwner {
    fn serves(&self, of: &(Option<String>, OwnerType)) -> bool {
        self.of == *of
            && (!matches!(self.owner, AgentOwner::Team { .. })
                || self.at.elapsed() < TEAM_MEMBERS_TTL)
    }
}

/// Everything that decides which login pays for one of an agent's calls.
pub struct AgentLogins {
    agent_id: String,
    vault: Option<Arc<Vault>>,
    global_vault: Option<Arc<Vault>>,
    configured_owner: (Option<String>, OwnerType),
    owner_override: OwnerOverride,
    teams: TeamStoreHandle,
    config: ConfigCell,
    owner: parking_lot::Mutex<Option<CachedOwner>>,
}

impl AgentLogins {
    pub fn new(
        agent_id: &str,
        vaults: Vaults<'_>,
        configured_owner: (Option<String>, OwnerType),
        owner_override: OwnerOverride,
        teams: TeamStoreHandle,
        config: ConfigCell,
    ) -> Self {
        Self {
            agent_id: agent_id.to_string(),
            vault: vaults.agent.cloned(),
            global_vault: vaults.global.cloned(),
            configured_owner,
            owner_override,
            teams,
            config,
            owner: parking_lot::Mutex::new(None),
        }
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub fn vaults(&self) -> Vaults<'_> {
        Vaults {
            agent: self.vault.as_ref(),
            global: self.global_vault.as_ref(),
        }
    }

    /// The effective owner (live override or config).
    pub fn effective_owner(&self) -> (Option<String>, OwnerType) {
        self.owner_override
            .read()
            .clone()
            .unwrap_or_else(|| self.configured_owner.clone())
    }

    /// Whether `person` may set the login every user of this agent shares:
    /// the agent's owner, or an admin.
    pub fn may_share(&self, person: Option<&str>, is_admin: bool) -> bool {
        is_admin
            || Payer::classify(person)
                .person()
                .is_some_and(|person| self.owner().includes(&person))
    }

    /// The agent's owner, as its shared subscription login sees it: a user,
    /// a team with its members, or nobody. Looked up again only when the
    /// owner changes, or an owning team's members are older than
    /// [`TEAM_MEMBERS_TTL`].
    pub fn owner(&self) -> AgentOwner {
        let effective = self.effective_owner();
        let mut cached = self.owner.lock();
        if let Some(hit) = cached.as_ref().filter(|hit| hit.serves(&effective)) {
            return hit.owner.clone();
        }
        let owner = self.look_up_owner(&effective);
        *cached = Some(CachedOwner {
            of: effective,
            owner: owner.clone(),
            at: Instant::now(),
        });
        owner
    }

    fn look_up_owner(&self, (id, owner_type): &(Option<String>, OwnerType)) -> AgentOwner {
        let Some(id) = id else {
            return AgentOwner::Unowned;
        };
        let Ok(owner) = uuid::Uuid::parse_str(id) else {
            tracing::warn!(agent = %self.agent_id, owner = %id, "agent owner is not an identity; its shared login serves background work only");
            return AgentOwner::Unowned;
        };
        match owner_type {
            OwnerType::User => AgentOwner::User(owner),
            OwnerType::Team => AgentOwner::Team {
                id: owner,
                members: self
                    .teams
                    .read()
                    .clone()
                    .and_then(|store| {
                        store
                            .get_team(owner)
                            .inspect_err(|e| tracing::warn!(agent = %self.agent_id, team = %owner, error = %e, "owning team unreadable; its members are not served"))
                            .ok()
                            .flatten()
                    })
                    .map(|team| team.members.into_iter().collect())
                    .unwrap_or_default(),
            },
        }
    }

    /// The login that pays for a `kind` call made now: the caller's own
    /// first, then the agent's shared one, as the agent's `[llm.logins]`
    /// allows, skipping logins at their usage limit or refused for good.
    pub fn resolve(&self, kind: SubscriptionKind) -> Result<Login, NoLogin> {
        let payer = crate::user_config::current_payer();
        let login = resolve(
            kind,
            &self.vaults(),
            &payer,
            &self.owner(),
            &self.config.snapshot().logins,
            CooldownStore::global(),
            chrono::Utc::now(),
        )?;
        if payer == Payer::Unset {
            tracing::warn!(
                agent = %self.agent_id,
                session_key = %CURRENT_SESSION_KEY.try_with(String::clone).unwrap_or_default(),
                session_id = %CURRENT_SESSION_ID.try_with(String::clone).unwrap_or_default(),
                tier = login.tier.as_str(),
                kind = kind.label(),
                "a subscription login served a call with no identity in scope"
            );
        }
        Ok(login)
    }

    /// Whether the agent's `[llm.logins]` admits a tier with a vault to hold
    /// a login.
    pub fn may_hold_logins(&self) -> bool {
        let vaults = self.vaults();
        self.config
            .snapshot()
            .logins
            .tiers
            .iter()
            .any(|&tier| vaults.of(tier).is_some())
    }

    /// Whether anyone has a `kind` login stored at a tier the agent's
    /// `[llm.logins]` reads: a shared tier's key, or any person's own.
    pub fn stored_for_anyone(&self, kind: SubscriptionKind) -> bool {
        let key = kind.vault_key();
        let own = format!("/{key}");
        let vaults = self.vaults();
        self.config.snapshot().logins.tiers.iter().any(|&tier| {
            vaults.of(tier).is_some_and(|vault| match tier.is_shared() {
                true => vault.get(key).is_some(),
                false => vault
                    .list(Some(crate::user_config::USER_PREFIX))
                    .iter()
                    .any(|path| path.ends_with(&own)),
            })
        })
    }

    /// The agent's provider chain's cap on one `model` call, if it sets one.
    pub fn request_cap(&self, model: &str) -> Option<Duration> {
        self.config
            .snapshot()
            .llm_timeouts
            .request_timeout_for(model)
    }

    /// The first `kind` login that may serve the call made now and that
    /// `accepts`, cooling or not: the login a round collected after a
    /// restart goes on with.
    pub fn holding(
        &self,
        kind: SubscriptionKind,
        accepts: impl Fn(&Login) -> bool,
    ) -> Option<Login> {
        holding(
            kind,
            &self.vaults(),
            &crate::user_config::current_payer(),
            &self.owner(),
            &self.config.snapshot().logins,
            accepts,
        )
    }
}

impl crate::hosts::LoginHost for AgentLogins {
    fn agent_id(&self) -> &str {
        AgentLogins::agent_id(self)
    }

    fn resolve(&self, kind: SubscriptionKind) -> Result<Login, NoLogin> {
        AgentLogins::resolve(self, kind)
    }

    fn holding(&self, kind: SubscriptionKind, accepts: &dyn Fn(&Login) -> bool) -> Option<Login> {
        AgentLogins::holding(self, kind, accepts)
    }

    fn owner(&self) -> AgentOwner {
        AgentLogins::owner(self)
    }

    fn effective_owner(&self) -> (Option<String>, OwnerType) {
        AgentLogins::effective_owner(self)
    }

    fn vaults(&self) -> Vaults<'_> {
        AgentLogins::vaults(self)
    }

    fn may_share(&self, person: Option<&str>, is_admin: bool) -> bool {
        AgentLogins::may_share(self, person, is_admin)
    }

    fn may_hold_logins(&self) -> bool {
        AgentLogins::may_hold_logins(self)
    }

    fn stored_for_anyone(&self, kind: SubscriptionKind) -> bool {
        AgentLogins::stored_for_anyone(self, kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::settings::WorkforceConfig;

    const ALICE: &str = "6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11";
    const BOB: &str = "0b7f3d52-8f0a-4c2f-9d44-3c1e6a1f4e22";

    fn logins(owner: Option<&str>, owner_override: OwnerOverride) -> AgentLogins {
        AgentLogins::new(
            "allan",
            Vaults::default(),
            (owner.map(str::to_string), OwnerType::User),
            owner_override,
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
        )
    }

    #[test]
    fn owner_is_looked_up_once_and_again_after_an_override() {
        let owner_override: OwnerOverride = Arc::new(parking_lot::RwLock::new(None));
        let logins = logins(Some(ALICE), Arc::clone(&owner_override));
        let alice = AgentOwner::User(uuid::Uuid::parse_str(ALICE).unwrap());
        assert_eq!(logins.owner(), alice);
        assert!(logins
            .owner
            .lock()
            .as_ref()
            .is_some_and(|hit| hit.owner == alice));

        *owner_override.write() = Some((Some(BOB.into()), OwnerType::User));
        assert_eq!(
            logins.owner(),
            AgentOwner::User(uuid::Uuid::parse_str(BOB).unwrap())
        );
        *owner_override.write() = None;
        assert_eq!(logins.owner(), alice);
    }

    #[test]
    fn a_non_identity_owner_is_unowned_and_cached() {
        let logins = logins(Some("U0123ABCD"), Arc::new(parking_lot::RwLock::new(None)));
        assert_eq!(logins.owner(), AgentOwner::Unowned);
        let first = logins.owner.lock().as_ref().map(|hit| hit.at);
        assert_eq!(logins.owner(), AgentOwner::Unowned);
        assert_eq!(
            logins.owner.lock().as_ref().map(|hit| hit.at),
            first,
            "looked up once, so warned once"
        );
    }
}
