//! Which of the person's Claude accounts each credential is
//! ([`crate::identity`]): this computer's own CLI sign-in is one of them,
//! never one more. Plan readings, standing and needs are kept by that
//! account, whichever credential of it a session ran on, and by the
//! credential a session actually runs on: one signed in again inside it
//! (`/login`) runs on the CLI's own sign-in from then on.

use chrono::Utc;
use uuid::Uuid;

use super::{Desk, DeskError, State};
use crate::engine::Engine;
use crate::identity::{Identity, Link, LinkedBy, Print, Verdict};

impl Desk {
    /// This computer's CLI sign-in was read: who it is signed in as. `None`
    /// when the read said nothing (it is kept as it was).
    pub(super) fn own_identity_read(&self, own: Option<Option<Identity>>) {
        let Some(own) = own else {
            return;
        };
        let changed = {
            let mut st = self.0.state.lock();
            let changed = st.own_identity != own;
            if changed {
                // Another sign-in's readings are another plan's.
                st.prints.remove(&None);
                st.own_identity = own;
            }
            changed
        };
        if changed {
            self.announce_accounts();
        }
    }

    /// The account the identity `key` is known as: a manual link first.
    fn account_of(st: &State, key: &str) -> Option<Uuid> {
        let mut found: Vec<(&Uuid, &Link)> = st
            .links
            .iter()
            .filter(|(id, l)| {
                l.identity.key == key
                    && st
                        .accounts
                        .iter()
                        .any(|a| a.id == **id && a.engine == Engine::Claude)
            })
            .collect();
        found.sort_by_key(|(_, l)| l.by != LinkedBy::Manual);
        found.first().map(|(id, _)| **id)
    }

    /// The account this computer's own CLI sign-in is, when that is known.
    pub(super) fn own_account(st: &State) -> Option<Uuid> {
        Self::account_of(st, &st.own_identity.as_ref()?.key)
    }

    /// The side a reading, a limit or a need is kept by: the CLI's own
    /// sign-in is the account it is known to be.
    pub(super) fn canonical(st: &State, side: Option<Uuid>) -> Option<Uuid> {
        side.or_else(|| Self::own_account(st))
    }

    /// The credential session `id` runs on now: its account's, unless it
    /// was signed in again inside (`/login`), which runs it on the CLI's
    /// own sign-in.
    pub(super) fn running_on(st: &State, id: Uuid) -> Option<Uuid> {
        let r = st.records.get(&id)?;
        match st.live.get(&id) {
            Some(l) if l.relogged => None,
            _ => r.account,
        }
    }

    /// The side session `id` counts on: [`Self::running_on`], as the account
    /// it is known to be.
    pub(super) fn side_of_session(st: &State, id: Uuid) -> Option<Uuid> {
        Self::canonical(st, Self::running_on(st, id))
    }

    /// The ids a reading on `side` is kept by in the hub: its identity's,
    /// whichever credential of it reported it, else the account's own.
    /// `None` for the CLI's own sign-in when its identity isn't known.
    pub(super) fn plan_ids(st: &State, side: Option<Uuid>) -> Option<Vec<String>> {
        match side {
            Some(account) => Some(vec![st.links.get(&account).map_or_else(
                || super::accounts::plan_id(account),
                |l| l.identity.plan_id(),
            )]),
            None => Some(vec![st.own_identity.as_ref()?.plan_id()]),
        }
    }

    /// A reading from `running_on` (the credential it came from, not yet
    /// made canonical): kept to tell, by usage, which account the CLI's own
    /// sign-in is. Two readings that agree link an unlinked account to it;
    /// one that disagrees unlinks an account linked so. A manual link is
    /// never changed.
    pub(super) fn observe_print(
        &self,
        running_on: Option<Uuid>,
        windows: &[crate::usage::LimitWindow],
    ) {
        let Some(print) = Print::of(windows, Utc::now()) else {
            return;
        };
        let mut changed: Vec<(Uuid, Option<Link>)> = Vec::new();
        {
            let mut guard = self.0.state.lock();
            let st = &mut *guard;
            let Some(own) = st.own_identity.clone() else {
                return;
            };
            let claude: Vec<Uuid> = st
                .accounts
                .iter()
                .filter(|a| a.engine == Engine::Claude)
                .map(|a| a.id)
                .collect();
            let own_prints: Vec<Print> = match running_on {
                None => {
                    st.prints.entry(None).or_default().keep(print);
                    vec![print]
                }
                Some(account) => {
                    st.prints.entry(Some(account)).or_default().keep(print);
                    st.prints
                        .get(&None)
                        .map(|p| p.recent.iter().copied().collect())
                        .unwrap_or_default()
                }
            };
            let compared: Vec<Uuid> = match running_on {
                None => claude,
                Some(account) => vec![account],
            };
            let manual = st
                .links
                .values()
                .any(|l| l.by == LinkedBy::Manual && l.identity.key == own.key);
            for account in compared {
                let link = st.links.get(&account).cloned();
                if link.as_ref().is_some_and(|l| l.by == LinkedBy::Manual) {
                    continue;
                }
                let prints = st.prints.entry(Some(account)).or_default();
                let verdicts: Vec<Verdict> = own_prints.iter().map(|p| prints.against(p)).collect();
                let agrees = prints.agrees();
                let differs = verdicts.contains(&Verdict::Different);
                let linked_here = link.as_ref().is_some_and(|l| l.identity.key == own.key);
                if differs {
                    prints.agreed.clear();
                    if linked_here {
                        tracing::info!(%account, "an account's usage no longer matches this computer's sign-in: unlinked");
                        changed.push((account, None));
                    }
                } else if agrees && link.is_none() && !manual {
                    // Never two accounts as the one sign-in.
                    let taken = st
                        .links
                        .iter()
                        .any(|(id, l)| *id != account && l.identity.key == own.key);
                    if !taken {
                        tracing::info!(%account, "an account's usage matches this computer's sign-in: linked");
                        changed.push((
                            account,
                            Some(Link {
                                identity: own.clone(),
                                by: LinkedBy::Usage,
                                at: Utc::now(),
                            }),
                        ));
                    }
                }
            }
            for (account, link) in &changed {
                match link {
                    Some(l) => st.links.insert(*account, l.clone()),
                    None => st.links.remove(account),
                };
            }
        }
        if !changed.is_empty() {
            self.keep_links(changed);
        }
    }

    /// Writes changed links, and tells every Desk.
    fn keep_links(&self, changed: Vec<(Uuid, Option<Link>)>) {
        let desk = self.clone();
        tokio::spawn(async move {
            for (account, link) in changed {
                if let Err(e) = desk.0.store.put_account_link(account, link.as_ref()).await {
                    tracing::warn!(%account, error = %format!("{e:#}"), "couldn't keep an account's identity");
                }
            }
            desk.announce_accounts();
            desk.accounts_moved().await;
        });
    }

    /// The person says which of their accounts this computer's Claude Code
    /// is signed in to (`None`: none of them). Kept, and never changed by
    /// usage.
    pub async fn link_own_sign_in(&self, account: Option<Uuid>) -> Result<(), DeskError> {
        if let Some(id) = account {
            let a = self.account(id)?;
            if a.engine != Engine::Claude {
                return Err(DeskError::Invalid(
                    "only a Claude account can be this computer's Claude sign-in".into(),
                ));
            }
        }
        let changed = {
            let mut st = self.0.state.lock();
            let Some(own) = st.own_identity.clone() else {
                return Err(DeskError::Conflict(
                    "Claude Code doesn't say which account this computer is signed in to: sign in there with /login first"
                        .into(),
                ));
            };
            let mut changed: Vec<(Uuid, Option<Link>)> = st
                .links
                .iter()
                .filter(|(id, l)| l.identity.key == own.key && Some(**id) != account)
                .map(|(id, _)| (*id, None))
                .collect();
            if let Some(id) = account {
                changed.push((
                    id,
                    Some(Link {
                        identity: own,
                        by: LinkedBy::Manual,
                        at: Utc::now(),
                    }),
                ));
            }
            for (id, link) in &changed {
                match link {
                    Some(l) => st.links.insert(*id, l.clone()),
                    None => st.links.remove(id),
                };
                st.prints.remove(&Some(*id));
            }
            changed
        };
        for (id, link) in &changed {
            self.0
                .store
                .put_account_link(*id, link.as_ref())
                .await
                .map_err(DeskError::Internal)?;
        }
        self.announce_accounts();
        self.accounts_moved().await;
        Ok(())
    }

    /// Session `id` was signed in again inside (`/login`, Claude Code's
    /// `auth_success`): it runs on the CLI's own sign-in now, which has
    /// just changed and is read again.
    pub(super) async fn signed_in_inside(&self, id: Uuid) {
        let moved = {
            let mut st = self.0.state.lock();
            let on_account = st.records.get(&id).is_some_and(|r| r.account.is_some());
            match st.live.get_mut(&id) {
                Some(l) if on_account && !l.relogged => {
                    l.relogged = true;
                    true
                }
                _ => false,
            }
        };
        if moved {
            tracing::info!(session = %id, "a session signed in again inside: it runs on this computer's sign-in now");
        }
        self.read_engine_now(Engine::Claude).await;
        self.announce_accounts();
    }

    /// What a session's transcript said of its last `/login`: one not seen
    /// before is a sign-in inside it. The first look at a transcript counts
    /// only one run lately, not its past.
    pub(super) async fn login_in_transcript(
        &self,
        id: Uuid,
        last_login: Option<(String, chrono::DateTime<Utc>)>,
    ) {
        let Some((login, at)) = last_login else {
            return;
        };
        let new = {
            let mut st = self.0.state.lock();
            let before = st.logins_seen.insert(id, login.clone());
            match before {
                Some(seen) => seen != login,
                None => Utc::now() - at < chrono::Duration::minutes(10),
            }
        };
        if new {
            self.signed_in_inside(id).await;
        }
    }
}
