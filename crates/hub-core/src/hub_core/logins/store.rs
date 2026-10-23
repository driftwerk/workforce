//! The one way a login reaches the vault, or leaves it.
//!
//! Every write, move, copy and delete of a login key (`claude_code/oauth_token`
//! or `codex/auth_json`, bare or under a user) comes through here: a value is
//! checked to be a whole login, a ChatGPT sign-in already stored elsewhere is
//! refused (its refresh token works for one copy only), and the slot's
//! refresh lock is held so an in-flight refresh can neither overwrite a new
//! login nor bring back one just removed. Any other key passes straight
//! through to the vault.

use std::sync::{Arc, Weak};

use workforce_common::settings::{is_claude_oat, VaultActor, VaultSource, CLAUDE_OAT_PREFIX};
use workforce_common::subscription::{split_per_user_key, SubscriptionKind};

use super::chatgpt_auth::{refuse_elsewhere, store_login, AuthJson, StoreError, PASTE_FALLBACK};
use super::resolve::{known_slots, VaultSlot};
use crate::user_config::{LayeredUserVault, USER_PREFIX};
use crate::vault::Vault;

const ELLIPSES: [&str; 2] = ["…", "..."];

/// Why a login was not written.
#[derive(Debug, thiserror::Error)]
pub enum LoginRefusal {
    #[error("not a usable login: {0}")]
    Invalid(String),
    #[error("{0}")]
    Duplicate(String),
    #[error("a ChatGPT sign-in can't be copied: a refresh token works for one copy only, so each place needs its own sign-in: connect from My Settings, or {PASTE_FALLBACK}")]
    CopyForbidden,
    #[error("only this agent's owner or an admin can change the login everyone on it shares; store your own instead")]
    SharedOnly,
    #[error("only an admin can change the install's own login; store your own instead")]
    InstallOnly,
    #[error(transparent)]
    Vault(#[from] eyre::Report),
}

impl LoginRefusal {
    /// [`Self::SharedOnly`] as a login tool says it: with the scope that
    /// stores the caller's own login instead.
    pub fn shared_only_by_tool() -> String {
        format!("{} (scope='user')", Self::SharedOnly)
    }
}

impl From<StoreError> for LoginRefusal {
    fn from(error: StoreError) -> Self {
        match error {
            duplicate @ StoreError::Duplicate(_) => Self::Duplicate(duplicate.to_string()),
            StoreError::Vault(e) => Self::Vault(e),
        }
    }
}

/// Whether `value` is a whole `kind` login: a Claude OAuth token as
/// `claude setup-token` prints it, or a ChatGPT `auth.json` with every token
/// whole.
fn check_login(kind: SubscriptionKind, value: &str) -> Result<(), String> {
    let value = value.trim();
    match kind {
        SubscriptionKind::Claude if !is_claude_oat(value) => Err(format!(
            "a Claude login is the token `claude setup-token` prints, starting `{CLAUDE_OAT_PREFIX}`; an API key (`sk-ant-api…`) can't drive the claude CLI"
        )),
        SubscriptionKind::Claude
            if ELLIPSES.iter().any(|e| value.contains(e)) || value.contains(char::is_whitespace) =>
        {
            Err("the token is not whole (it was abbreviated or wrapped on the way in); paste it exactly as `claude setup-token` printed it".to_string())
        }
        SubscriptionKind::Claude => Ok(()),
        SubscriptionKind::ChatGpt => AuthJson::parse(value).map(drop),
    }
}

/// Store a checked `kind` login in `slot`. A ChatGPT sign-in already stored
/// at any of `known` is refused.
pub async fn store_checked(
    slot: &VaultSlot,
    kind: SubscriptionKind,
    value: &str,
    known: &[VaultSlot],
    actor: VaultActor<'_>,
) -> Result<(), LoginRefusal> {
    check_login(kind, value).map_err(LoginRefusal::Invalid)?;
    Ok(store_login(slot, value.trim(), known, actor).await?)
}

/// Remove `slot`'s login under its lock, along with any refreshed copy held
/// in memory, so a refresh in flight finds the slot empty and stores
/// nothing. Returns whether a login was stored.
async fn remove_login(slot: &VaultSlot, actor: VaultActor<'_>) -> eyre::Result<bool> {
    let _guard = slot.lock().await?;
    let removed = slot.vault.delete_with_actor(&slot.key, actor)?;
    slot.forget_unstored();
    Ok(removed)
}

/// Remove `slot`'s login if it is still `value`, which a refresh would have
/// replaced. Returns whether it was.
pub(super) async fn withdraw_login(
    slot: &VaultSlot,
    value: &str,
    actor: VaultActor<'_>,
) -> eyre::Result<bool> {
    let _guard = slot.lock().await?;
    slot.vault.sync()?;
    let unchanged = slot.vault.get(&slot.key).as_deref() == Some(value);
    if unchanged {
        slot.vault.delete_with_actor(&slot.key, actor)?;
        slot.forget_unstored();
    }
    Ok(unchanged)
}

/// Move the login in `from` to `to` as one unit, holding both slots' locks
/// (taken in a fixed order, so two moves never wait on each other). The
/// destination's other copies in `known` are checked as for a new login.
async fn move_login(
    from: &VaultSlot,
    to: &VaultSlot,
    known: &[VaultSlot],
    actor: VaultActor<'_>,
) -> Result<(), LoginRefusal> {
    let kind = SubscriptionKind::of_vault_key(&from.key)
        .filter(|kind| SubscriptionKind::of_vault_key(&to.key) == Some(*kind))
        .ok_or_else(|| {
            LoginRefusal::Invalid("a login moves only to another slot of its own kind".into())
        })?;
    if from.location() == to.location() {
        return Ok(());
    }
    let (first, second) = match from.location() < to.location() {
        true => (from, to),
        false => (to, from),
    };
    let _first = first.lock().await?;
    let _second = second.lock().await?;
    from.vault.sync()?;
    to.vault.sync()?;
    let value = from
        .vault
        .get(&from.key)
        .ok_or_else(|| LoginRefusal::Invalid(format!("nothing stored at {}", from.key)))?;
    check_login(kind, &value).map_err(LoginRefusal::Invalid)?;
    let elsewhere: Vec<VaultSlot> = known
        .iter()
        .filter(|slot| slot.location() != from.location())
        .cloned()
        .collect();
    refuse_elsewhere(value.trim(), to, &elsewhere)?;
    crate::vault::move_value(
        (&from.vault, &from.key),
        (&to.vault, &to.key),
        &value,
        actor,
    )?;
    from.forget_unstored();
    to.forget_unstored();
    Ok(())
}

/// The slot a vault key is, when it holds a login.
fn login_slot(vault: &Arc<Vault>, key: &str) -> Option<(SubscriptionKind, VaultSlot)> {
    SubscriptionKind::of_vault_key(key).map(|kind| {
        (
            kind,
            VaultSlot {
                vault: Arc::clone(vault),
                key: key.to_string(),
            },
        )
    })
}

/// Whose own slots a write at `key` concerns: the user the key is under,
/// else the person writing it.
fn person_of<'a>(key: &'a str, actor: VaultActor<'a>) -> Option<&'a str> {
    split_per_user_key(key).map(|(uid, _)| uid).or(match actor {
        VaultActor::User(uid) => Some(uid),
        _ => None,
    })
}

fn known_for(
    kind: SubscriptionKind,
    key: &str,
    vaults: &[Arc<Vault>],
    actor: VaultActor<'_>,
) -> Vec<VaultSlot> {
    known_slots(kind, vaults, person_of(key, actor))
}

/// Write `value` at `key` in `vault`: a login through [`store_checked`],
/// checked against its copies in the vaults `known` lists; anything else as
/// is.
pub async fn set(
    vault: &Arc<Vault>,
    key: &str,
    value: &str,
    known: impl FnOnce() -> Vec<Arc<Vault>>,
    actor: VaultActor<'_>,
) -> Result<(), LoginRefusal> {
    match login_slot(vault, key) {
        Some((kind, slot)) => {
            let known = known_for(kind, key, &known(), actor);
            store_checked(&slot, kind, value, &known, actor).await
        }
        None => Ok(vault.set_with_actor(key, value, actor)?),
    }
}

/// Delete `key` from `vault`, a login through [`remove_login`]. Returns
/// whether anything was stored.
pub async fn delete(vault: &Arc<Vault>, key: &str, actor: VaultActor<'_>) -> eyre::Result<bool> {
    match login_slot(vault, key) {
        Some((_, slot)) => remove_login(&slot, actor).await,
        None => vault.delete_with_actor(key, actor),
    }
}

/// Move `value` from one vault key to another: a login only between slots of
/// its kind, through [`move_login`]; anything else as one write.
pub async fn relocate(
    from: (&Arc<Vault>, &str),
    to: (&Arc<Vault>, &str),
    value: &str,
    known: impl FnOnce() -> Vec<Arc<Vault>>,
    actor: VaultActor<'_>,
) -> Result<(), LoginRefusal> {
    match (login_slot(from.0, from.1), login_slot(to.0, to.1)) {
        (None, None) => Ok(crate::vault::move_value(
            (from.0, from.1),
            (to.0, to.1),
            value,
            actor,
        )?),
        (Some((kind, source)), Some((_, target))) => {
            let known = known_for(kind, to.1, &known(), actor);
            move_login(&source, &target, &known, actor).await
        }
        _ => Err(LoginRefusal::Invalid(
            "a login moves only to another slot of its own kind".into(),
        )),
    }
}

/// Refuse a copy of `path`'s value to other slots when it is a ChatGPT
/// sign-in, which works in one slot only.
pub fn refuse_copy(path: &str) -> Result<(), LoginRefusal> {
    match SubscriptionKind::of_vault_key(path) {
        Some(SubscriptionKind::ChatGpt) => Err(LoginRefusal::CopyForbidden),
        _ => Ok(()),
    }
}

/// `vaults` once each.
pub fn distinct_vaults(vaults: impl IntoIterator<Item = Arc<Vault>>) -> Vec<Arc<Vault>> {
    let mut seen = std::collections::HashSet::new();
    vaults
        .into_iter()
        .filter(|vault| seen.insert(vault.location()))
        .collect()
}

pub use crate::hub_core::owners::KnownVaults;

/// The vaults an agent knows a login may sit in: its own, the global one
/// and, once `registry` is wired, every agent's.
pub fn known_vaults(
    own: Option<Arc<Vault>>,
    global: Option<Arc<Vault>>,
    registry: crate::hosts::RegistryCell,
) -> KnownVaults {
    Arc::new(move || {
        let registered = registry
            .get()
            .and_then(Weak::upgrade)
            .map(|registry| registry.login_vaults(None))
            .unwrap_or_default();
        distinct_vaults(own.iter().chain(global.iter()).cloned().chain(registered))
    })
}

/// Whether the caller may change the login everyone on an agent shares.
pub type MayShare = Arc<dyn Fn() -> bool + Send + Sync>;

/// How an agent's vault tools write a login key: to the slot its tier names
/// for the identity in scope, through this module. A slot an agent or the
/// hub shares is changed only by whom [`MayShare`] allows.
#[derive(Clone)]
pub struct LoginWrites {
    layered: Arc<LayeredUserVault>,
    known: KnownVaults,
    may_share: MayShare,
}

impl LoginWrites {
    pub fn new(layered: Arc<LayeredUserVault>, known: KnownVaults, may_share: MayShare) -> Self {
        Self {
            layered,
            known,
            may_share,
        }
    }

    /// Refuse a change to a shared slot by someone who may not make it.
    fn guard(&self, keys: &[&str]) -> Result<(), LoginRefusal> {
        match keys.iter().any(|key| !key.starts_with(USER_PREFIX)) && !(self.may_share)() {
            true => Err(LoginRefusal::SharedOnly),
            false => Ok(()),
        }
    }

    /// Store `value` at `path` on `tier`.
    pub async fn set(
        &self,
        path: &str,
        value: &str,
        tier: VaultSource,
        actor: VaultActor<'_>,
    ) -> Result<(), LoginRefusal> {
        let (vault, key) = self.layered.write_target(path, tier)?;
        self.guard(&[&key])?;
        set(&vault, &key, value, &*self.known, actor).await
    }

    /// Remove `path` from the most specific tier holding it.
    pub async fn delete(&self, path: &str, actor: VaultActor<'_>) -> Result<bool, LoginRefusal> {
        let Some((vault, key)) = self.layered.locate(path) else {
            return Ok(false);
        };
        self.guard(&[&key])?;
        Ok(delete(&vault, &key, actor).await?)
    }

    /// Move `value` between two tier-addressed paths.
    pub async fn relocate(
        &self,
        from: (&str, VaultSource),
        to: (&str, VaultSource),
        value: &str,
        actor: VaultActor<'_>,
    ) -> Result<(), LoginRefusal> {
        let (source, source_key) = self.layered.write_target(from.0, from.1)?;
        let (target, target_key) = self.layered.write_target(to.0, to.1)?;
        self.guard(&[&source_key, &target_key])?;
        relocate(
            (&source, &source_key),
            (&target, &target_key),
            value,
            &*self.known,
            actor,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer};

    use crate::hub_core::logins::chatgpt_auth::TokenRefresher;
    use crate::hub_core::logins::test_support::{login, BlocksTheVault};

    const CHATGPT: &str = SubscriptionKind::ChatGpt.vault_key();
    const ALICE: &str = "6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11";

    fn vault(dir: &tempfile::TempDir, name: &str) -> Arc<Vault> {
        Arc::new(Vault::open(&dir.path().join(name), "test-key").unwrap())
    }

    fn slot(vault: &Arc<Vault>, key: &str) -> VaultSlot {
        VaultSlot {
            vault: Arc::clone(vault),
            key: key.to_string(),
        }
    }

    #[test]
    fn claude_logins_are_checked_whole() {
        assert!(check_login(SubscriptionKind::Claude, "sk-ant-oat01-abc").is_ok());
        for refused in [
            "sk-ant-api03-abc",
            "sk-ant-oat01-ab…cd",
            "sk-ant-oat01 abc",
            "",
        ] {
            assert!(
                check_login(SubscriptionKind::Claude, refused).is_err(),
                "{refused}"
            );
        }
        assert!(check_login(SubscriptionKind::ChatGpt, "{}").is_err());
    }

    #[tokio::test]
    async fn removed_login_is_not_resurrected_by_in_flight_write_back() {
        let dir = tempfile::tempdir().unwrap();
        let held = vault(&dir, "vault.enc");
        let slot = slot(&held, CHATGPT);
        let vault_file = dir.path().join("vault.enc");
        let aside = dir.path().join("vault.enc.aside");
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(BlocksTheVault {
                vault_file: vault_file.clone(),
                aside: aside.clone(),
            })
            .mount(&server)
            .await;
        let refresher = TokenRefresher::at(
            reqwest::Client::new(),
            format!("{}/oauth/token", server.uri()),
        );
        let stale = login("acct-1", "a0", "rt-0");
        held.set(CHATGPT, &stale).unwrap();
        let refreshed = refresher
            .fresh(
                &slot,
                &AuthJson::parse(&stale).unwrap(),
                TimeDelta::hours(2),
            )
            .await
            .unwrap();
        assert_eq!(refreshed.refresh_token(), Some("rt-1"));
        std::fs::remove_dir_all(&vault_file).unwrap();
        std::fs::rename(&aside, &vault_file).unwrap();

        let guard = slot.lock().await.unwrap();
        let removal = tokio::spawn({
            let slot = slot.clone();
            async move { remove_login(&slot, VaultActor::Cli).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !removal.is_finished(),
            "a removal waits for the slot's lock"
        );
        drop(guard);
        assert!(removal.await.unwrap().unwrap());

        refresher
            .fresh(&slot, &refreshed, TimeDelta::minutes(5))
            .await
            .unwrap();
        held.sync().unwrap();
        assert!(
            held.get(CHATGPT).is_none(),
            "the refresh held in memory is gone with the login"
        );
    }

    #[tokio::test]
    async fn move_login_leaves_one_copy() {
        let dir = tempfile::tempdir().unwrap();
        let (agent, global) = (vault(&dir, "agent.enc"), vault(&dir, "global.enc"));
        let own = format!("users/{ALICE}/{CHATGPT}");
        let sign_in = login("acct-1", "a0", "rt-0");
        global.set(&own, &sign_in).unwrap();
        let vaults = [Arc::clone(&agent), Arc::clone(&global)];
        let known = known_slots(SubscriptionKind::ChatGpt, &vaults, Some(ALICE));

        move_login(
            &slot(&global, &own),
            &slot(&agent, &own),
            &known,
            VaultActor::User(ALICE),
        )
        .await
        .unwrap();

        assert!(global.get(&own).is_none());
        assert_eq!(agent.get(&own).as_deref(), Some(sign_in.as_str()));
        let copy = store_checked(
            &slot(&global, &own),
            SubscriptionKind::ChatGpt,
            &sign_in,
            &known,
            VaultActor::User(ALICE),
        )
        .await;
        let refused = copy.unwrap_err();
        assert!(matches!(refused, LoginRefusal::Duplicate(_)), "{refused:?}");
        let told = refused.to_string();
        assert!(told.contains("a person's own login"), "{told}");
        assert!(
            !told.contains(".enc") && !told.contains(&dir.path().display().to_string()),
            "a refusal names the kind of slot, never where it is stored: {told}"
        );
        assert!(matches!(
            move_login(
                &slot(&agent, &own),
                &slot(&agent, "skills/elsewhere"),
                &known,
                VaultActor::User(ALICE)
            )
            .await,
            Err(LoginRefusal::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn a_login_relocates_and_copies_only_to_a_slot_of_its_own_kind() {
        let dir = tempfile::tempdir().unwrap();
        let agent = vault(&dir, "agent.enc");
        let claude = SubscriptionKind::Claude.vault_key();
        agent.set(claude, "sk-ant-oat01-alice").unwrap();
        agent.set(CHATGPT, &login("acct-1", "a0", "rt-0")).unwrap();
        for (from, to) in [
            (claude, CHATGPT),
            (claude, "skills/elsewhere"),
            (CHATGPT, claude),
            (CHATGPT, "skills/elsewhere"),
        ] {
            let value = agent.get(from).unwrap();
            let moved = relocate(
                (&agent, from),
                (&agent, to),
                &value,
                Vec::new,
                VaultActor::User(ALICE),
            )
            .await;
            assert!(
                matches!(moved, Err(LoginRefusal::Invalid(_))),
                "{from} → {to}: {moved:?}"
            );
            assert_eq!(agent.get(from), Some(value), "{from} stays where it was");
        }
        assert!(
            refuse_copy(CHATGPT).is_err(),
            "a ChatGPT sign-in works in one slot"
        );
        assert!(
            refuse_copy(claude).is_ok(),
            "a Claude token copies to the same key of other users only"
        );
    }

    #[tokio::test]
    async fn withdrawal_removes_only_the_login_it_stored() {
        let dir = tempfile::tempdir().unwrap();
        let held = vault(&dir, "vault.enc");
        let slot = slot(&held, CHATGPT);
        let stored = login("acct-1", "a0", "rt-0");
        held.set(CHATGPT, &stored).unwrap();
        assert!(withdraw_login(&slot, &stored, VaultActor::Cli)
            .await
            .unwrap());
        assert!(held.get(CHATGPT).is_none());

        held.set(CHATGPT, &stored).unwrap();
        let refreshed = login("acct-1", "a0", "rt-1");
        held.set(CHATGPT, &refreshed).unwrap();
        assert!(!withdraw_login(&slot, &stored, VaultActor::Cli)
            .await
            .unwrap());
        assert_eq!(held.get(CHATGPT).as_deref(), Some(refreshed.as_str()));
    }
}
