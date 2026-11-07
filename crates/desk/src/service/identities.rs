//! Which of the person's Claude accounts each credential is
//! ([`crate::identity`]): this computer's own CLI sign-in is one of them,
//! never one more.
//!
//! What each credential reports is kept as that credential's own
//! ([`Raw`]): an account's by its id, the CLI sign-in's by the identity it
//! was signed in as at the time. Nothing is moved or merged on write.
//! Standing, moves and needs are read through the links as they are now
//! ([`Desk::effective_standing`]), so linking or unlinking loses nothing,
//! and another sign-in's reading never stands for this one's.
//!
//! A session signed in again inside (`/login`, confirmed by Claude Code)
//! runs on the CLI's own sign-in from then on, and what it reports is that
//! sign-in's.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Desk, DeskError, State};
use crate::engine::Engine;
use crate::engine_accounts::{MovedOff, Standing};
use crate::identity::{Identity, Link, LinkedBy, Print, Prints, Verdict};

/// The credential a reading, a stop on a limit or a move came from, as it
/// is kept: one of the person's accounts, or this computer's own CLI
/// sign-in as the identity it was signed in as then (`""`: Claude Code
/// didn't say).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "camelCase")]
pub(crate) enum Raw {
    Account(Uuid),
    Own(String),
}

/// A move off a full plan, between the credentials as kept.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawMove {
    pub engine: Engine,
    pub from: Raw,
    pub to: Raw,
}

/// Where the person's answers and the prints are kept, across restarts.
pub(super) const OWN_NONE: &str = "accounts.ownNone";
pub(super) const RELOGGED: &str = "accounts.relogged";
pub(super) const PRINTS: &str = "accounts.prints";

impl Desk {
    /// This computer's CLI sign-in was read: who it is signed in as. `None`
    /// when the read said nothing (it is kept as it was). Its readings held
    /// until now are filed.
    pub(super) fn own_identity_read(&self, own: Option<Option<Identity>>) {
        // Read, whatever it said: what was held is filed now, under the
        // identity said, or (it said nothing) under none, as before.
        let (changed, waiting) = {
            let mut st = self.0.state.lock();
            let changed = own.as_ref().is_some_and(|o| st.own_identity != *o);
            if let Some(own) = own {
                st.own_identity = own;
            }
            st.own_read = true;
            (changed, std::mem::take(&mut st.own_waiting))
        };
        for (limits, session, at) in waiting {
            self.report_usage_at(limits, session, at);
        }
        if changed {
            self.announce_accounts();
        }
    }

    /// The identity key this computer's CLI sign-in has now (`""`: unknown).
    pub(super) fn own_key(st: &State) -> String {
        st.own_identity
            .as_ref()
            .map_or_else(String::new, |i| i.key.clone())
    }

    /// The account the identity `key` is known as: a manual link first.
    fn account_of(st: &State, key: &str) -> Option<Uuid> {
        if key.is_empty() {
            return None;
        }
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

    /// The account known as the identity `key`.
    pub(super) fn account_of_identity(st: &State, key: &str) -> Option<Uuid> {
        Self::account_of(st, key)
    }

    /// The account this computer's own CLI sign-in is, when that is known.
    pub(super) fn own_account(st: &State) -> Option<Uuid> {
        Self::account_of(st, &Self::own_key(st))
    }

    /// The credential `side` is as kept: an account, or the CLI sign-in as
    /// it is signed in now.
    pub(super) fn raw_of(st: &State, side: Option<Uuid>) -> Raw {
        match side {
            Some(account) => Raw::Account(account),
            None => Raw::Own(Self::own_key(st)),
        }
    }

    /// The side `raw` counts on now: its account, the CLI sign-in's account
    /// when known, or the CLI sign-in itself. `None` for a sign-in the CLI
    /// no longer has and no account is known as: nobody's side now.
    pub(super) fn side_of_raw(st: &State, raw: &Raw) -> Option<Option<Uuid>> {
        match raw {
            Raw::Account(id) => Some(Some(*id)),
            Raw::Own(key) => match Self::account_of(st, key) {
                Some(account) => Some(Some(account)),
                None if *key == Self::own_key(st) => Some(None),
                None => None,
            },
        }
    }

    /// The credential session `id` runs on now: its account's, unless it
    /// was signed in again inside (`/login`), which runs it on the CLI's
    /// own sign-in.
    pub(super) fn running_on(st: &State, id: Uuid) -> Option<Uuid> {
        let r = st.records.get(&id)?;
        match st.relogged.contains(&id) {
            true => None,
            false => r.account,
        }
    }

    /// The credential session `id` runs on, as kept: on the CLI's own
    /// sign-in, the identity it was signed in as when the session started
    /// (or signed in again) on it, which a `/login` elsewhere since may not
    /// have changed for it.
    pub(super) fn raw_running(st: &State, id: Uuid) -> Raw {
        match Self::running_on(st, id) {
            Some(account) => Raw::Account(account),
            None => Raw::Own(
                st.live
                    .get(&id)
                    .and_then(|l| l.own_key.clone())
                    .unwrap_or_else(|| Self::own_key(st)),
            ),
        }
    }

    /// The side session `id` counts on now: `None` while it runs on a
    /// sign-in the CLI no longer has, which counts for nobody.
    pub(super) fn side_of_session(st: &State, id: Uuid) -> Option<Option<Uuid>> {
        Self::side_of_raw(st, &Self::raw_running(st, id))
    }

    /// The standing kept for `raw` alone, to write to.
    pub(super) fn standing_mut<'a>(st: &'a mut State, raw: &Raw) -> &'a mut Standing {
        match raw {
            Raw::Account(id) => st.standing.entry(Some(*id)).or_default(),
            Raw::Own(key) => st.own_standing.entry(key.clone()).or_default(),
        }
    }

    /// Each side's standing as it reads now: an account's own, with what
    /// the CLI sign-in reported while it was signed in as that account; the
    /// CLI sign-in's own while it is none of them. A sign-in it no longer
    /// has counts for nobody.
    pub(super) fn effective_standing(st: &State) -> HashMap<Option<Uuid>, Standing> {
        let mut out: HashMap<Option<Uuid>, Standing> = st
            .standing
            .iter()
            .filter(|(side, _)| side.is_some())
            .map(|(side, s)| (*side, s.clone()))
            .collect();
        for (key, s) in &st.own_standing {
            if let Some(side) = Self::side_of_raw(st, &Raw::Own(key.clone())) {
                let merged = out.remove(&side).unwrap_or_default().merged(s);
                out.insert(side, merged);
            }
        }
        out
    }

    /// One side's standing as it reads now.
    pub(super) fn standing_of(st: &State, side: Option<Uuid>) -> Standing {
        Self::effective_standing(st)
            .remove(&side)
            .unwrap_or_default()
    }

    /// The moves off a full plan as they read now: between sides, each
    /// once, none from a side to itself.
    pub(super) fn effective_moves(st: &State) -> Vec<MovedOff> {
        let mut out: Vec<MovedOff> = Vec::new();
        for m in &st.moved_off {
            let (Some(from), Some(to)) =
                (Self::side_of_raw(st, &m.from), Self::side_of_raw(st, &m.to))
            else {
                continue;
            };
            let moved = MovedOff {
                engine: m.engine,
                from,
                to,
            };
            if from != to && !out.contains(&moved) {
                out.push(moved);
            }
        }
        out
    }

    /// The ids a reading of `raw` is kept by in the hub: its identity's,
    /// whichever credential of it reported it, else the account's own.
    /// `None` for the CLI's own sign-in when its identity isn't known.
    pub(super) fn plan_ids(st: &State, raw: &Raw) -> Option<Vec<String>> {
        match raw {
            Raw::Account(account) => Some(vec![st.links.get(account).map_or_else(
                || super::accounts::plan_id(*account),
                |l| l.identity.plan_id(),
            )]),
            Raw::Own(key) if key.is_empty() => None,
            Raw::Own(key) => Some(vec![format!("identity:{key}")]),
        }
    }

    /// The ids this computer's CLI sign-in's plan readings are kept by, as
    /// the Floor names its card.
    pub(super) fn own_plan_ids(st: &State) -> Option<Vec<String>> {
        Self::plan_ids(st, &Raw::Own(Self::own_key(st)))
    }

    /// A reading from `raw` (the credential it came from): kept to tell, by
    /// usage, which account the CLI's own sign-in is. Two distinct readings
    /// on each side that agree link an unlinked account to it; one that
    /// disagrees unlinks an account linked so. A manual link, or the
    /// person's "none of these", is never changed. A reading of a CLI
    /// sign-in the CLI no longer has says nothing of it.
    pub(super) fn observe_print(
        &self,
        raw: &Raw,
        windows: &[crate::usage::LimitWindow],
        at: DateTime<Utc>,
    ) {
        let Some(print) = Print::of(windows, at) else {
            return;
        };
        let mut changed: Vec<(Uuid, Option<Link>)> = Vec::new();
        {
            let mut guard = self.0.state.lock();
            let st = &mut *guard;
            let Some(own) = st.own_identity.clone() else {
                return;
            };
            if matches!(raw, Raw::Own(key) if *key != own.key) {
                return;
            }
            st.prints.entry(raw.clone()).or_default().keep(print);
            let own_raw = Raw::Own(own.key.clone());
            let own_prints: Vec<Print> = match raw {
                Raw::Own(_) => vec![print],
                Raw::Account(_) => st
                    .prints
                    .get(&own_raw)
                    .map(|p| p.recent.iter().copied().collect())
                    .unwrap_or_default(),
            };
            let compared: Vec<Uuid> = match raw {
                Raw::Own(_) => st
                    .accounts
                    .iter()
                    .filter(|a| a.engine == Engine::Claude)
                    .map(|a| a.id)
                    .collect(),
                Raw::Account(account) => vec![*account],
            };
            let answered = st.own_none.contains(&own.key)
                || st
                    .links
                    .values()
                    .any(|l| l.by == LinkedBy::Manual && l.identity.key == own.key);
            for account in compared {
                let link = st.links.get(&account).cloned();
                if link.as_ref().is_some_and(|l| l.by == LinkedBy::Manual) {
                    continue;
                }
                let prints = st.prints.entry(Raw::Account(account)).or_default();
                let verdicts: Vec<Verdict> = own_prints.iter().map(|p| prints.against(p)).collect();
                let agrees = prints.agrees();
                let differs = verdicts.contains(&Verdict::Different);
                let linked_here = link.as_ref().is_some_and(|l| l.identity.key == own.key);
                if differs {
                    prints.agreed.clear();
                    if linked_here {
                        tracing::info!(%account, "an account's usage no longer matches this computer's sign-in: unlinked");
                        st.links.remove(&account);
                        changed.push((account, None));
                    }
                } else if agrees && link.is_none() && !answered {
                    // Never two accounts as the one sign-in.
                    let taken = st
                        .links
                        .iter()
                        .any(|(id, l)| *id != account && l.identity.key == own.key);
                    if !taken {
                        tracing::info!(%account, "an account's usage matches this computer's sign-in: linked");
                        let link = Link {
                            identity: own.clone(),
                            by: LinkedBy::Usage,
                            at: Utc::now(),
                        };
                        // Seen by the next account in this pass: never two.
                        st.links.insert(account, link.clone());
                        changed.push((account, Some(link)));
                    }
                }
            }
        }
        self.keep_prints();
        if !changed.is_empty() {
            self.keep_links(changed);
        }
    }

    /// Writes the prints, so later evidence can still undo a link made by
    /// usage after a restart.
    fn keep_prints(&self) {
        let prints: Vec<(Raw, Prints)> = self
            .0
            .state
            .lock()
            .prints
            .iter()
            .map(|(r, p)| (r.clone(), p.clone()))
            .collect();
        self.keep_state(PRINTS, serde_json::json!(prints));
    }

    /// Writes `value` under `key` in Desk's own state, in the background.
    pub(super) fn keep_state(&self, key: &'static str, value: serde_json::Value) {
        let store = std::sync::Arc::clone(&self.0.store);
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                if let Err(e) = store.put_state(key, &value).await {
                    tracing::warn!(key, error = %format!("{e:#}"), "couldn't keep Desk's accounts state");
                }
            });
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
    /// usage; what each side reported stays its own, so nothing is lost
    /// either way.
    pub async fn link_own_sign_in(&self, account: Option<Uuid>) -> Result<(), DeskError> {
        if let Some(id) = account {
            let a = self.account(id)?;
            if a.engine != Engine::Claude {
                return Err(DeskError::Invalid(
                    "only a Claude account can be this computer's Claude sign-in".into(),
                ));
            }
        }
        let (changed, none) = {
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
                        identity: own.clone(),
                        by: LinkedBy::Manual,
                        at: Utc::now(),
                    }),
                ));
                st.own_none.remove(&own.key);
            } else {
                st.own_none.insert(own.key.clone());
            }
            for (id, link) in &changed {
                match link {
                    Some(l) => st.links.insert(*id, l.clone()),
                    None => st.links.remove(id),
                };
                if let Some(p) = st.prints.get_mut(&Raw::Account(*id)) {
                    p.agreed.clear();
                }
            }
            (changed, st.own_none.iter().cloned().collect::<Vec<_>>())
        };
        self.keep_state(OWN_NONE, serde_json::json!(none));
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

    /// Session `id` was signed in again inside, as Claude Code confirmed
    /// (`auth_success`, or "Login successful" after a `/login` in its
    /// transcript): it runs on the CLI's own sign-in now, which has just
    /// changed and is read again. Kept across a restart of the hub.
    pub(super) async fn signed_in_inside(&self, id: Uuid) {
        let moved = {
            let mut st = self.0.state.lock();
            let on_account = st.records.get(&id).is_some_and(|r| r.account.is_some());
            on_account && st.relogged.insert(id)
        };
        if moved {
            tracing::info!(session = %id, "a session signed in again inside: it runs on this computer's sign-in now");
            self.keep_relogged();
        }
        self.read_engine_now(Engine::Claude).await;
        {
            // It runs on the sign-in as it is now.
            let mut st = self.0.state.lock();
            let key = Self::own_key(&st);
            if let Some(l) = st.live.get_mut(&id) {
                l.own_key = Some(key);
            }
        }
        self.announce_accounts();
    }

    /// Session `id` started afresh: on its account again, whatever it was
    /// signed in to inside before; on the CLI's own sign-in, as it is now.
    pub(super) fn started_afresh(&self, id: Uuid) {
        let was = {
            let mut st = self.0.state.lock();
            let key = Self::own_key(&st);
            if let Some(l) = st.live.get_mut(&id) {
                l.own_key = Some(key);
            }
            st.relogged.remove(&id)
        };
        if was {
            self.keep_relogged();
        }
    }

    fn keep_relogged(&self) {
        let ids: Vec<Uuid> = self.0.state.lock().relogged.iter().copied().collect();
        self.keep_state(RELOGGED, serde_json::json!(ids));
    }

    /// What a session's transcript said of its last confirmed `/login`: one
    /// not seen before, since its process started here, is a sign-in inside
    /// it.
    pub(super) async fn login_in_transcript(
        &self,
        id: Uuid,
        last_login: Option<(String, DateTime<Utc>)>,
    ) {
        let Some((login, at)) = last_login else {
            return;
        };
        let new = {
            let mut st = self.0.state.lock();
            let started = st.live.get(&id).map(|l| l.started_at);
            let before = st.logins_seen.insert(id, login.clone());
            match before {
                Some(seen) => seen != login,
                None => started.is_some_and(|s| at >= s),
            }
        };
        if new {
            self.signed_in_inside(id).await;
        }
    }
}
