//! Which subscription login serves a call.
//!
//! Deliberately not `resolve_flat`: the tiers are walked in the agent's
//! `[llm.logins]` order — by default the payer's own logins (agent, then
//! global) before the agent's shared one and the install's — and the
//! `CLAUDE_CODE_OAUTH_TOKEN` env var is never consulted for an agent: a login
//! serves whom its place in the vaults says it serves, and no one else.

use std::fmt;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use workforce_common::settings::{LoginPolicy, SecretStore};
use workforce_common::subscription::{
    AgentOwner, CredentialTier, LoginOwner, Payer, Served, SharedServes, SubscriptionKind,
    SubscriptionRoute,
};

use super::chatgpt_auth::{unstored, AuthJson, API_KEY_FILE};
use super::cooldown::{CooldownStore, LimitSignal, LimitStatus};
use super::errors::{SubscriptionError, THROTTLE_COOLDOWN};
use super::limits::{classify, Notice};
use crate::user_config::per_user_key;
use crate::vault::Vault;

/// The vaults an agent's logins live in.
#[derive(Clone, Copy, Default)]
pub struct Vaults<'a> {
    pub agent: Option<&'a Arc<Vault>>,
    pub global: Option<&'a Arc<Vault>>,
}

impl<'a> Vaults<'a> {
    /// The vault `tier` keeps its logins in, when there is one.
    pub fn of(&self, tier: CredentialTier) -> Option<&'a Arc<Vault>> {
        match tier {
            CredentialTier::AgentUser | CredentialTier::AgentShared => self.agent,
            CredentialTier::GlobalUser | CredentialTier::SystemShared => self.global,
        }
    }
}

/// One vault key: where a login was found and where its refresh goes back.
#[derive(Clone)]
pub struct VaultSlot {
    pub vault: Arc<Vault>,
    pub key: String,
}

impl VaultSlot {
    /// Where this slot is, across vaults and replicas.
    pub fn location(&self) -> String {
        self.vault.write_location(&self.key)
    }
}

impl fmt::Debug for VaultSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VaultSlot")
            .field("vault", &self.vault.location())
            .field("key", &self.key)
            .finish()
    }
}

/// Identifies a login across refreshes, for its limit cooldown: a ChatGPT
/// login by its account and member, since a Team account is shared.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LoginKey {
    pub kind: SubscriptionKind,
    pub fingerprint: String,
}

impl LoginKey {
    /// The key `credential` cools under: a Claude token by its value, a
    /// ChatGPT sign-in by its account and member. `None` for a sign-in that
    /// names no account.
    pub fn of(credential: &Credential) -> Option<Self> {
        match credential {
            Credential::Token(_) => Some(Self {
                kind: SubscriptionKind::Claude,
                fingerprint: credential.fingerprint(),
            }),
            Credential::ChatGpt(auth) => Self::of_chatgpt(auth),
        }
    }

    /// The key a ChatGPT sign-in cools under: its account and member.
    pub fn of_chatgpt(auth: &AuthJson) -> Option<Self> {
        auth.account_id().map(|account| Self {
            kind: SubscriptionKind::ChatGpt,
            fingerprint: format!("{account}:{}", auth.member().unwrap_or_default()),
        })
    }
}

/// What a login authenticates with.
#[derive(Clone)]
pub enum Credential {
    /// A Claude OAuth token.
    Token(String),
    /// A ChatGPT sign-in, as `codex login` writes it.
    ChatGpt(AuthJson),
}

impl Credential {
    /// Identifies this very value, which a refresh or a new sign-in replaces.
    pub fn fingerprint(&self) -> String {
        match self {
            Self::Token(token) => hex::encode(&Sha256::digest(token.as_bytes())[..8]),
            Self::ChatGpt(auth) => auth.fingerprint(),
        }
    }
}

/// A resolved login and the tier it came from.
#[derive(Clone)]
pub struct Login {
    pub kind: SubscriptionKind,
    pub tier: CredentialTier,
    pub owner: LoginOwner,
    pub credential: Credential,
    pub slot: VaultSlot,
    pub key: LoginKey,
}

impl fmt::Debug for Login {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Login")
            .field("kind", &self.kind)
            .field("tier", &self.tier)
            .field("owner", &self.owner)
            .field("slot", &self.slot)
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl Login {
    /// Whose plan the login is, as a cost row names it.
    pub fn payer(&self, agent_id: &str) -> String {
        self.owner.payer(agent_id)
    }

    /// What a call this login pays for records on its cost row.
    pub fn served(&self, route: SubscriptionRoute, agent_id: &str) -> Served {
        Served {
            route,
            tier: self.tier,
            payer: self.payer(agent_id),
        }
    }

    /// The ChatGPT sign-in, for a ChatGPT login.
    pub fn auth(&self) -> Option<&AuthJson> {
        match &self.credential {
            Credential::ChatGpt(auth) => Some(auth),
            Credential::Token(_) => None,
        }
    }

    /// The same login, now holding `auth`: a refresh of its sign-in.
    pub fn refreshed(&self, auth: AuthJson) -> Self {
        Self {
            credential: Credential::ChatGpt(auth),
            ..self.clone()
        }
    }

    /// How the login is named in errors and logs: whose plan, from which
    /// tier.
    pub fn describe(&self, caller: &str) -> String {
        format!(
            "the {} login of {} ({})",
            self.kind.label(),
            self.payer(caller),
            self.tier.as_str()
        )
    }

    /// Cool the login for `signal`'s refusal, so every route that would
    /// resolve it skips it, and name when it frees up.
    pub fn cool(&self, signal: &LimitSignal, caller: &str) -> SubscriptionError {
        let now = Utc::now();
        let cooldowns = CooldownStore::global();
        cooldowns.note(&self.key, signal, now);
        SubscriptionError::LimitReached {
            login: self.describe(caller),
            until: cooldowns.until(&self.key, now),
        }
    }

    /// The provider refused the login for good: skip its slot until it holds
    /// another sign-in.
    pub fn mark_refused(&self) {
        CooldownStore::global().note_dead(&self.slot, &self.credential.fingerprint());
    }

    /// [`Self::mark_refused`], naming why.
    pub fn refuse(&self, reason: String, caller: &str) -> SubscriptionError {
        self.mark_refused();
        SubscriptionError::LoginInvalid {
            login: self.describe(caller),
            reason,
        }
    }

    /// What a CLI round on this login says about it: a refusal its limit
    /// report names cools it until the reset, whatever the round's end; a
    /// round that failed is read by the CLI's own notice — at its limit it
    /// is cooled, refused it is marked so for good. `None` otherwise: a round
    /// that timed out or hit its turn cap says nothing of its login.
    pub fn settle(&self, round: &RoundEnd<'_>, caller: &str) -> Option<SubscriptionError> {
        self.observe_usage(None, round.usage.clone());
        let refused = |resets_at| LimitSignal {
            status: LimitStatus::Rejected,
            resets_at,
            window: None,
        };
        let rejected = round
            .limit
            .clone()
            .filter(|signal| signal.status == LimitStatus::Rejected);
        let limit = match (rejected, round.failed) {
            (Some(signal), _) => signal,
            (None, true) => match classify(self.kind, round.notice, round.stderr_tail) {
                Notice::Capped => refused(None),
                Notice::Throttled => refused(Some(Utc::now() + THROTTLE_COOLDOWN)),
                Notice::Auth => return Some(self.refuse(round.failure_line.clone(), caller)),
                Notice::Other => return None,
            },
            (None, false) => return None,
        };
        Some(self.cool(&limit, caller))
    }
}

/// What a CLI round says about the login it ran on: the limit its report
/// names, whether it failed, and what to read a failure by.
pub struct RoundEnd<'a> {
    pub limit: Option<LimitSignal>,
    /// How full the plan's windows were, as the round last reported them.
    pub usage: Vec<workforce_common::usage::Window>,
    pub failed: bool,
    pub notice: &'a str,
    pub stderr_tail: &'a str,
    /// The line a refusal names as its reason.
    pub failure_line: String,
}

/// Why one tier did not serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Miss {
    Absent,
    /// A personal tier, and the call has no person to look up.
    NoPayer,
    /// The shared login may not serve this payer.
    NotServed(SharedServes),
    /// The login is at its usage limit until then.
    Cooling {
        until: DateTime<Utc>,
    },
    /// The login was refused for good and must be signed in again.
    Invalid,
    Malformed(String),
}

impl Miss {
    /// Whether signing in again makes the tier serve: nothing usable is
    /// stored there, and nothing but time or the policy stands in the way.
    pub fn needs_sign_in(&self) -> bool {
        matches!(self, Self::Absent | Self::Invalid | Self::Malformed(_))
    }
}

impl fmt::Display for Miss {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent => f.write_str("none stored"),
            Self::NoPayer => f.write_str("no person to look up"),
            Self::NotServed(serves) => write!(f, "shared login serves {serves:?} only"),
            Self::Cooling { until } => write!(f, "at its usage limit until {until}"),
            Self::Invalid => f.write_str("must be signed in again"),
            Self::Malformed(reason) => write!(f, "unusable ({reason})"),
        }
    }
}

/// No tier served: each tier tried and why it missed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoLogin {
    pub kind: SubscriptionKind,
    pub payer: Payer,
    pub misses: Vec<(CredentialTier, Miss)>,
}

impl NoLogin {
    /// The earliest time a cooling login frees up, if one is cooling.
    pub fn cooling_until(&self) -> Option<DateTime<Utc>> {
        self.misses
            .iter()
            .filter_map(|(_, miss)| match miss {
                Miss::Cooling { until } => Some(*until),
                _ => None,
            })
            .min()
    }
}

impl fmt::Display for NoLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no {} login for {}", self.kind.label(), self.payer)?;
        self.misses
            .iter()
            .enumerate()
            .try_for_each(|(i, (tier, miss))| {
                let sep = if i == 0 { ": " } else { "; " };
                write!(f, "{sep}{} {miss}", tier.as_str())
            })
    }
}

impl std::error::Error for NoLogin {}

/// The first login in `policy.tiers` order that exists, may serve `payer`,
/// was not refused for good, and is not cooling at `now`.
pub fn resolve(
    kind: SubscriptionKind,
    vaults: &Vaults<'_>,
    payer: &Payer,
    owner: &AgentOwner,
    policy: &LoginPolicy,
    cooldowns: &CooldownStore,
    now: DateTime<Utc>,
) -> Result<Login, NoLogin> {
    let mut misses = Vec::new();
    for &tier in &policy.tiers {
        let found = if !tier.is_shared() || policy.shared_serves.admits(payer, owner) {
            login_state(kind, tier, vaults, payer.person(), owner, cooldowns, now)
        } else {
            Err(Miss::NotServed(policy.shared_serves))
        };
        match found {
            Ok(login) => return Ok(login),
            Err(miss) => misses.push((tier, miss)),
        }
    }
    Err(NoLogin {
        kind,
        payer: payer.clone(),
        misses,
    })
}

/// The login `tier` holds for `person` (whom it may serve aside): usable, or
/// why not — absent, unreadable, refused for good, or cooling at `now`.
pub fn login_state(
    kind: SubscriptionKind,
    tier: CredentialTier,
    vaults: &Vaults<'_>,
    person: Option<Uuid>,
    owner: &AgentOwner,
    cooldowns: &CooldownStore,
    now: DateTime<Utc>,
) -> Result<Login, Miss> {
    let login = slot_for(tier, kind, vaults, person, owner)
        .and_then(|(slot, login_owner)| read(kind, tier, slot, login_owner))?;
    if cooldowns.is_dead(&login.slot, &login.credential.fingerprint()) {
        return Err(Miss::Invalid);
    }
    match cooldowns.until(&login.key, now) {
        Some(until) => Err(Miss::Cooling { until }),
        None => Ok(login),
    }
}

/// The first login of `policy`'s tiers that may serve `payer` and that
/// `accepts`, cooling or refused or not: the login a collected round's job
/// already runs on, which needs no fresh call to be let through.
pub fn holding(
    kind: SubscriptionKind,
    vaults: &Vaults<'_>,
    payer: &Payer,
    owner: &AgentOwner,
    policy: &LoginPolicy,
    accepts: impl Fn(&Login) -> bool,
) -> Option<Login> {
    policy
        .tiers
        .iter()
        .filter(|tier| !tier.is_shared() || policy.shared_serves.admits(payer, owner))
        .find_map(|&tier| {
            slot_for(tier, kind, vaults, payer.person(), owner)
                .and_then(|(slot, login_owner)| read(kind, tier, slot, login_owner))
                .ok()
                .filter(&accepts)
        })
}

/// Every slot a `kind` login may sit in across `vaults` that concerns
/// `person`: each vault's shared key and, for a person, their own key.
pub fn known_slots(
    kind: SubscriptionKind,
    vaults: &[Arc<Vault>],
    person: Option<&str>,
) -> Vec<VaultSlot> {
    let key = kind.vault_key();
    vaults
        .iter()
        .flat_map(|vault| {
            std::iter::once(key.to_string())
                .chain(person.map(|person| per_user_key(person, key)))
                .map(|key| VaultSlot {
                    vault: Arc::clone(vault),
                    key,
                })
        })
        .collect()
}

/// The key `tier` keeps `kind`'s login under: under `person` for a
/// personal tier, which has none without one.
pub fn slot_key(
    tier: CredentialTier,
    kind: SubscriptionKind,
    person: Option<Uuid>,
) -> Result<String, Miss> {
    let key = kind.vault_key();
    match tier {
        CredentialTier::AgentUser | CredentialTier::GlobalUser => person
            .map(|person| per_user_key(&person.to_string(), key))
            .ok_or(Miss::NoPayer),
        CredentialTier::AgentShared | CredentialTier::SystemShared => Ok(key.to_string()),
    }
}

/// Where `tier` keeps `kind`'s login for `person`, and whose plan it is.
pub fn slot_for(
    tier: CredentialTier,
    kind: SubscriptionKind,
    vaults: &Vaults<'_>,
    person: Option<Uuid>,
    owner: &AgentOwner,
) -> Result<(VaultSlot, LoginOwner), Miss> {
    let whose = match tier {
        CredentialTier::AgentUser | CredentialTier::GlobalUser => {
            LoginOwner::Person(person.ok_or(Miss::NoPayer)?)
        }
        CredentialTier::AgentShared => LoginOwner::Agent(owner.clone()),
        CredentialTier::SystemShared => LoginOwner::Install,
    };
    let vault = vaults.of(tier).ok_or(Miss::Absent)?;
    let slot = VaultSlot {
        vault: Arc::clone(vault),
        key: slot_key(tier, kind, person)?,
    };
    Ok((slot, whose))
}

fn read(
    kind: SubscriptionKind,
    tier: CredentialTier,
    slot: VaultSlot,
    owner: LoginOwner,
) -> Result<Login, Miss> {
    let secret = slot
        .vault
        .get(&slot.key)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or(Miss::Absent)?;
    let credential = match kind {
        SubscriptionKind::Claude => Credential::Token(secret),
        SubscriptionKind::ChatGpt => {
            let auth = match unstored(&slot) {
                Some(auth) => auth,
                None => AuthJson::parse(&secret).map_err(Miss::Malformed)?,
            };
            if auth.tokens.is_none() {
                return Err(Miss::Malformed(API_KEY_FILE.into()));
            }
            Credential::ChatGpt(auth)
        }
    };
    let key = LoginKey::of(&credential)
        .ok_or_else(|| Miss::Malformed("the login names no ChatGPT account".into()))?;
    Ok(Login {
        kind,
        tier,
        owner,
        credential,
        slot,
        key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub_core::logins::cooldown::{LimitSignal, LimitStatus};
    use crate::hub_core::logins::test_support::{login as chatgpt_login, member_login};
    use std::collections::BTreeSet;
    use uuid::Uuid;

    const ALICE: &str = "6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11";
    const BOB: &str = "0b7f3d52-8f0a-4c2f-9d44-3c1e6a1f4e22";
    const CLAUDE: &str = SubscriptionKind::Claude.vault_key();
    const CHATGPT: &str = SubscriptionKind::ChatGpt.vault_key();

    fn token(login: &Login) -> &str {
        match &login.credential {
            Credential::Token(token) => token,
            Credential::ChatGpt(_) => panic!("a ChatGPT login"),
        }
    }

    struct Fixture {
        agent: Arc<Vault>,
        global: Arc<Vault>,
        _dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let open =
                |name: &str| Arc::new(Vault::open(&dir.path().join(name), "test-key").unwrap());
            Self {
                agent: open("agent.enc"),
                global: open("global.enc"),
                _dir: dir,
            }
        }

        fn vaults(&self) -> Vaults<'_> {
            Vaults {
                agent: Some(&self.agent),
                global: Some(&self.global),
            }
        }

        fn resolve_with(
            &self,
            kind: SubscriptionKind,
            payer: &Payer,
            owner: &AgentOwner,
            policy: &LoginPolicy,
            cooldowns: &CooldownStore,
        ) -> Result<Login, NoLogin> {
            resolve(
                kind,
                &self.vaults(),
                payer,
                owner,
                policy,
                cooldowns,
                Utc::now(),
            )
        }

        fn resolve(&self, payer: &Payer, owner: &AgentOwner) -> Result<Login, NoLogin> {
            self.resolve_with(
                SubscriptionKind::Claude,
                payer,
                owner,
                &LoginPolicy::default(),
                &CooldownStore::default(),
            )
        }
    }

    fn person(id: &str) -> Payer {
        Payer::Person(Uuid::parse_str(id).unwrap())
    }

    fn owned_by(id: &str) -> AgentOwner {
        AgentOwner::User(Uuid::parse_str(id).unwrap())
    }

    #[test]
    fn subscription_resolve_prefers_agent_user_over_global_user() {
        let fx = Fixture::new();
        fx.agent
            .set(&format!("users/{ALICE}/{CLAUDE}"), "agent-user-token")
            .unwrap();
        fx.global
            .set(&format!("users/{ALICE}/{CLAUDE}"), "global-user-token")
            .unwrap();
        fx.agent.set(CLAUDE, "shared-token").unwrap();

        let login = fx.resolve(&person(ALICE), &owned_by(ALICE)).unwrap();
        assert_eq!(token(&login), "agent-user-token");
        assert_eq!(login.tier, CredentialTier::AgentUser);
        assert_eq!(login.slot.key, format!("users/{ALICE}/{CLAUDE}"));

        fx.agent.delete(&format!("users/{ALICE}/{CLAUDE}")).unwrap();
        let login = fx.resolve(&person(ALICE), &owned_by(ALICE)).unwrap();
        assert_eq!(login.tier, CredentialTier::GlobalUser);
        assert!(Arc::ptr_eq(&login.slot.vault, &fx.global));
    }

    /// A policy that keeps shared logins to the agent's owner.
    fn owner_only() -> LoginPolicy {
        LoginPolicy {
            shared_serves: SharedServes::Owner,
            ..LoginPolicy::default()
        }
    }

    #[test]
    fn subscription_resolve_reaches_global_shared_last_and_never_env() {
        let fx = Fixture::new();
        let err = fx.resolve(&person(ALICE), &owned_by(ALICE)).unwrap_err();
        assert_eq!(
            err.misses,
            [
                (CredentialTier::AgentUser, Miss::Absent),
                (CredentialTier::GlobalUser, Miss::Absent),
                (CredentialTier::AgentShared, Miss::Absent),
                (CredentialTier::SystemShared, Miss::Absent),
            ],
            "the tiers, and nothing else, are where a login is looked for"
        );

        fx.global.set(CLAUDE, "global-shared-token").unwrap();
        let login = fx.resolve(&person(ALICE), &owned_by(ALICE)).unwrap();
        assert_eq!(token(&login), "global-shared-token");
        assert_eq!(login.tier, CredentialTier::SystemShared);
        fx.agent.set(CLAUDE, "agent-shared-token").unwrap();
        assert_eq!(
            fx.resolve(&person(ALICE), &owned_by(ALICE)).unwrap().tier,
            CredentialTier::AgentShared,
            "the agent's own shared login comes before the install's"
        );

        let without_install = LoginPolicy {
            tiers: vec![CredentialTier::AgentUser, CredentialTier::GlobalUser],
            ..LoginPolicy::default()
        };
        assert!(fx
            .resolve_with(
                SubscriptionKind::Claude,
                &person(ALICE),
                &owned_by(ALICE),
                &without_install,
                &CooldownStore::default(),
            )
            .is_err());
    }

    #[test]
    fn agent_shared_login_serves_anyone_by_default_and_the_owner_when_narrowed() {
        let fx = Fixture::new();
        fx.agent.set(CLAUDE, "alices-plan").unwrap();

        let login = fx.resolve(&person(ALICE), &owned_by(ALICE)).unwrap();
        assert_eq!(login.tier, CredentialTier::AgentShared);
        assert_eq!(
            login.served(SubscriptionRoute::ClaudeCode, "allan").payer,
            ALICE
        );
        assert_eq!(
            token(&fx.resolve(&person(BOB), &owned_by(ALICE)).unwrap()),
            "alices-plan",
            "where the login is stored decides whom it serves"
        );

        let err = fx
            .resolve_with(
                SubscriptionKind::Claude,
                &person(BOB),
                &owned_by(ALICE),
                &owner_only(),
                &CooldownStore::default(),
            )
            .unwrap_err();
        assert_eq!(
            err.misses[2],
            (
                CredentialTier::AgentShared,
                Miss::NotServed(SharedServes::Owner)
            )
        );
    }

    #[test]
    fn team_owned_agent_shared_serves_team_members() {
        let fx = Fixture::new();
        fx.agent.set(CLAUDE, "team-plan").unwrap();
        let team_id = Uuid::new_v4();
        let team = AgentOwner::Team {
            id: team_id,
            members: BTreeSet::from([Uuid::parse_str(ALICE).unwrap()]),
        };

        let login = fx.resolve(&person(ALICE), &team).unwrap();
        assert_eq!(
            login.served(SubscriptionRoute::ClaudeCode, "allan").payer,
            format!("team:{team_id}")
        );
        assert!(fx
            .resolve_with(
                SubscriptionKind::Claude,
                &person(BOB),
                &team,
                &owner_only(),
                &CooldownStore::default(),
            )
            .is_err());
    }

    #[test]
    fn unidentified_payer_not_served_by_owner_policy() {
        let fx = Fixture::new();
        fx.agent.set(CLAUDE, "alices-plan").unwrap();
        let err = fx
            .resolve_with(
                SubscriptionKind::Claude,
                &Payer::Unidentified("U0123ABCD".into()),
                &owned_by(ALICE),
                &owner_only(),
                &CooldownStore::default(),
            )
            .unwrap_err();
        assert_eq!(
            err.misses,
            [
                (CredentialTier::AgentUser, Miss::NoPayer),
                (CredentialTier::GlobalUser, Miss::NoPayer),
                (
                    CredentialTier::AgentShared,
                    Miss::NotServed(SharedServes::Owner)
                ),
                (
                    CredentialTier::SystemShared,
                    Miss::NotServed(SharedServes::Owner)
                ),
            ]
        );
    }

    #[test]
    fn external_payer_never_served() {
        let fx = Fixture::new();
        fx.agent.set(CLAUDE, "alices-plan").unwrap();
        let anyone = LoginPolicy {
            shared_serves: SharedServes::Anyone,
            ..LoginPolicy::default()
        };
        assert!(fx
            .resolve_with(
                SubscriptionKind::Claude,
                &Payer::External("ext:cust-1".into()),
                &owned_by(ALICE),
                &anyone,
                &CooldownStore::default(),
            )
            .is_err());
    }

    #[test]
    fn unset_payer_served_by_shared_login_only_under_anyone() {
        let fx = Fixture::new();
        fx.agent.set(CLAUDE, "alices-plan").unwrap();
        let narrowed = LoginPolicy {
            shared_serves: SharedServes::AgentUsers,
            ..LoginPolicy::default()
        };
        assert!(fx
            .resolve_with(
                SubscriptionKind::Claude,
                &Payer::Unset,
                &owned_by(ALICE),
                &narrowed,
                &CooldownStore::default(),
            )
            .is_err());
        let login = fx.resolve(&Payer::Unset, &owned_by(ALICE)).unwrap();
        assert_eq!(login.tier, CredentialTier::AgentShared);
    }

    #[test]
    fn sentinel_turn_uses_agent_shared_or_fails_named() {
        let fx = Fixture::new();
        let heartbeat = Payer::Background("heartbeat".into());
        let err = fx.resolve(&heartbeat, &AgentOwner::Unowned).unwrap_err();
        assert_eq!(
            err.to_string(),
            "no Claude login for heartbeat: agent_user no person to look up; \
             global_user no person to look up; agent_shared none stored; \
             system_shared none stored"
        );

        fx.agent.set(CLAUDE, "agent-plan").unwrap();
        let login = fx.resolve(&heartbeat, &AgentOwner::Unowned).unwrap();
        assert_eq!(login.tier, CredentialTier::AgentShared);
        assert_eq!(
            login.served(SubscriptionRoute::ClaudeCode, "allan").payer,
            "agent:allan"
        );
    }

    #[test]
    fn cooling_login_skipped_and_reported_with_reset() {
        let fx = Fixture::new();
        fx.agent
            .set(&format!("users/{ALICE}/{CLAUDE}"), "alice-token")
            .unwrap();
        fx.agent.set(CLAUDE, "alices-shared").unwrap();
        let cooldowns = CooldownStore::default();
        let now = Utc::now();
        let reset = now + chrono::TimeDelta::hours(2);
        let alice = fx.resolve(&person(ALICE), &owned_by(ALICE)).unwrap();
        cooldowns.note(
            &alice.key,
            &LimitSignal {
                status: LimitStatus::Rejected,
                resets_at: Some(reset),
                window: Some("five_hour".into()),
            },
            now,
        );

        let login = fx
            .resolve_with(
                SubscriptionKind::Claude,
                &person(ALICE),
                &owned_by(ALICE),
                &LoginPolicy::default(),
                &cooldowns,
            )
            .unwrap();
        assert_eq!(login.tier, CredentialTier::AgentShared);

        fx.agent.delete(CLAUDE).unwrap();
        let err = fx
            .resolve_with(
                SubscriptionKind::Claude,
                &person(ALICE),
                &owned_by(ALICE),
                &LoginPolicy::default(),
                &cooldowns,
            )
            .unwrap_err();
        assert_eq!(err.cooling_until(), Some(reset));
        assert_eq!(
            err.misses[0],
            (CredentialTier::AgentUser, Miss::Cooling { until: reset })
        );
    }

    fn resolve_chatgpt(
        fx: &Fixture,
        payer: &Payer,
        cooldowns: &CooldownStore,
    ) -> Result<Login, NoLogin> {
        fx.resolve_with(
            SubscriptionKind::ChatGpt,
            payer,
            &owned_by(ALICE),
            &LoginPolicy::default(),
            cooldowns,
        )
    }

    #[test]
    fn chatgpt_fingerprint_survives_refresh() {
        let fx = Fixture::new();
        let key = format!("users/{ALICE}/{CHATGPT}");
        fx.agent
            .set(&key, &chatgpt_login("acct-9", "a", "rt-0"))
            .unwrap();
        let cooldowns = CooldownStore::default();
        let before = resolve_chatgpt(&fx, &person(ALICE), &cooldowns).unwrap();
        fx.agent
            .set(&key, &chatgpt_login("acct-9", "b", "rt-1"))
            .unwrap();
        let after = resolve_chatgpt(&fx, &person(ALICE), &cooldowns).unwrap();
        assert_eq!(before.key, after.key);
        assert_eq!(before.key.fingerprint, "acct-9:user-1");
    }

    #[test]
    fn team_members_on_one_account_cool_apart() {
        let fx = Fixture::new();
        let workspace = "acct-team";
        for (who, member) in [(ALICE, "user-alice"), (BOB, "user-bob")] {
            fx.agent
                .set(
                    &format!("users/{who}/{CHATGPT}"),
                    &member_login(workspace, member, who, "rt-0"),
                )
                .unwrap();
        }
        let cooldowns = CooldownStore::default();
        let now = Utc::now();
        let alice = resolve_chatgpt(&fx, &person(ALICE), &cooldowns).unwrap();
        cooldowns.note(
            &alice.key,
            &LimitSignal {
                status: LimitStatus::Rejected,
                resets_at: Some(now + chrono::TimeDelta::hours(1)),
                window: None,
            },
            now,
        );
        assert!(resolve_chatgpt(&fx, &person(ALICE), &cooldowns).is_err());
        let bob = resolve_chatgpt(&fx, &person(BOB), &cooldowns)
            .expect("one member's limit leaves the workspace's other members served");
        assert_ne!(bob.key, alice.key);
    }

    #[test]
    fn a_dead_login_is_skipped_until_its_slot_holds_another() {
        let fx = Fixture::new();
        let own = format!("users/{ALICE}/{CHATGPT}");
        fx.agent
            .set(&own, &chatgpt_login("acct-a", "a", "rt-dead"))
            .unwrap();
        fx.agent
            .set(CHATGPT, &chatgpt_login("acct-shared", "s", "rt-s"))
            .unwrap();
        let cooldowns = CooldownStore::default();
        let dead = resolve_chatgpt(&fx, &person(ALICE), &cooldowns).unwrap();
        assert_eq!(dead.tier, CredentialTier::AgentUser);
        cooldowns.note_dead(&dead.slot, &dead.credential.fingerprint());

        let fallback = resolve_chatgpt(&fx, &person(ALICE), &cooldowns).unwrap();
        assert_eq!(fallback.tier, CredentialTier::AgentShared);
        fx.agent.delete(CHATGPT).unwrap();
        let err = resolve_chatgpt(&fx, &person(ALICE), &cooldowns).unwrap_err();
        assert_eq!(err.misses[0], (CredentialTier::AgentUser, Miss::Invalid));

        fx.agent
            .set(&own, &chatgpt_login("acct-a", "a2", "rt-new-sign-in"))
            .unwrap();
        let signed_in = resolve_chatgpt(&fx, &person(ALICE), &cooldowns).unwrap();
        assert_eq!(signed_in.tier, CredentialTier::AgentUser);
    }

    #[test]
    fn system_shared_serves_an_agent_as_its_shared_serves_admits() {
        let fx = Fixture::new();
        fx.global.set(CLAUDE, "install-plan").unwrap();
        let resolve = |payer: &Payer, policy: &LoginPolicy| {
            fx.resolve_with(
                SubscriptionKind::Claude,
                payer,
                &owned_by(ALICE),
                policy,
                &CooldownStore::default(),
            )
        };

        let login = resolve(&person(BOB), &LoginPolicy::default()).unwrap();
        assert_eq!(login.tier, CredentialTier::SystemShared);
        assert_eq!(
            login.served(SubscriptionRoute::ClaudeCode, "allan").payer,
            "install"
        );
        assert_eq!(
            resolve(&person(BOB), &owner_only())
                .unwrap_err()
                .misses
                .last(),
            Some(&(
                CredentialTier::SystemShared,
                Miss::NotServed(SharedServes::Owner)
            ))
        );
        assert_eq!(
            resolve(&person(ALICE), &owner_only()).unwrap().tier,
            CredentialTier::SystemShared
        );
    }

    #[test]
    fn login_state_reports_cooling_dead_malformed_and_absent() {
        let fx = Fixture::new();
        let alice = Uuid::parse_str(ALICE).ok();
        let cooldowns = CooldownStore::default();
        let now = Utc::now();
        let state = |tier, cooldowns: &CooldownStore| {
            login_state(
                SubscriptionKind::ChatGpt,
                tier,
                &fx.vaults(),
                alice,
                &AgentOwner::Unowned,
                cooldowns,
                now,
            )
        };
        assert_eq!(
            state(CredentialTier::AgentUser, &cooldowns).unwrap_err(),
            Miss::Absent
        );
        assert_eq!(
            login_state(
                SubscriptionKind::ChatGpt,
                CredentialTier::GlobalUser,
                &fx.vaults(),
                None,
                &AgentOwner::Unowned,
                &cooldowns,
                now,
            )
            .unwrap_err(),
            Miss::NoPayer
        );

        fx.agent.set(CHATGPT, "{not json").unwrap();
        assert!(matches!(
            state(CredentialTier::AgentShared, &cooldowns).unwrap_err(),
            Miss::Malformed(_)
        ));

        fx.global
            .set(CHATGPT, &chatgpt_login("acct-i", "a", "rt-0"))
            .unwrap();
        let install = state(CredentialTier::SystemShared, &cooldowns).unwrap();
        assert_eq!(install.owner, LoginOwner::Install);
        let reset = now + chrono::TimeDelta::hours(3);
        cooldowns.note(
            &install.key,
            &LimitSignal {
                status: LimitStatus::Rejected,
                resets_at: Some(reset),
                window: None,
            },
            now,
        );
        assert_eq!(
            state(CredentialTier::SystemShared, &cooldowns).unwrap_err(),
            Miss::Cooling { until: reset }
        );
        let dead = CooldownStore::default();
        dead.note_dead(&install.slot, &install.credential.fingerprint());
        assert_eq!(
            state(CredentialTier::SystemShared, &dead).unwrap_err(),
            Miss::Invalid
        );
    }

    #[test]
    fn chatgpt_api_key_file_is_not_a_subscription_login() {
        let fx = Fixture::new();
        fx.agent
            .set(
                CHATGPT,
                r#"{"OPENAI_API_KEY":concat!("sk-pro", "j-0123456789abcdefghij")}"#,
            )
            .unwrap();
        let err = fx
            .resolve_with(
                SubscriptionKind::ChatGpt,
                &Payer::Background("heartbeat".into()),
                &AgentOwner::Unowned,
                &LoginPolicy::default(),
                &CooldownStore::default(),
            )
            .unwrap_err();
        assert!(matches!(
            err.misses[2],
            (CredentialTier::AgentShared, Miss::Malformed(_))
        ));
    }
}
