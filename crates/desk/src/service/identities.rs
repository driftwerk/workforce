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

/// Who wrote a credential's standing: a session of this computer's, the
/// hub's own call on the account, another computer, or a report no session
/// made (a terminal's own Claude Code).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "camelCase")]
pub(crate) enum Writer {
    Session(Uuid),
    Pool,
    Elsewhere,
    Unattributed,
}

/// Who wrote a standing's last reading and its last stop on the limit
/// (`None`: not known, as kept before this was).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct WrittenBy {
    pub read: Option<Writer>,
    pub hit: Option<Writer>,
}

/// What a session reported while its transcript was read for a `/login`
/// inside it: filed once that is known, as it came.
#[derive(Debug, Clone)]
pub(crate) enum Waiting {
    Usage(serde_json::Value, Uuid, DateTime<Utc>),
    Hit(Uuid, Option<crate::limit::LimitHit>, DateTime<Utc>),
}

impl Waiting {
    fn session(&self) -> Uuid {
        match self {
            Waiting::Usage(_, id, _) | Waiting::Hit(id, _, _) => *id,
        }
    }
}

/// What a session that turned out to run on another sign-in takes back
/// from its account: this computer's plan reading of it taken at `exact`,
/// with `instead` (the account's own stop on its limit, as a reading) kept.
struct TakenBack {
    ids: Option<Vec<String>>,
    exact: Option<DateTime<Utc>>,
    instead: Option<(serde_json::Value, DateTime<Utc>)>,
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

    /// The side a need counts the CLI sign-in on now: the account it is
    /// known to be.
    pub(super) fn canonical(st: &State, side: Option<Uuid>) -> Option<Uuid> {
        side.or_else(|| Self::own_account(st))
    }

    /// Whether the person said this computer's sign-in is none of their
    /// accounts.
    pub(super) fn own_is_none(st: &State) -> bool {
        st.own_identity
            .as_ref()
            .is_some_and(|o| st.own_none.contains(&o.key))
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

    /// Who wrote `raw`'s last reading.
    pub(super) fn wrote_read(st: &mut State, raw: &Raw, by: Writer) {
        st.written_by.entry(raw.clone()).or_default().read = Some(by);
    }

    /// Who wrote `raw`'s last stop on its limit.
    pub(super) fn wrote_hit(st: &mut State, raw: &Raw, by: Writer) {
        st.written_by.entry(raw.clone()).or_default().hit = Some(by);
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
        self.login_seen(id, login, at, None, false).await;
    }

    /// The whole transcript of a session taken back was read for a `/login`
    /// since its process started (`None`: none, or it couldn't be read):
    /// what it reported meanwhile is filed now, on the sign-in it runs on.
    pub(super) async fn login_scan_done(
        &self,
        id: Uuid,
        found: Option<crate::transcript::SignedInAgain>,
    ) {
        match found {
            Some(f) => {
                self.login_seen(id, f.last_id, f.first_at, f.hit_before, true)
                    .await
            }
            None => {
                let waiting = Self::scan_released(&mut self.0.state.lock(), id);
                self.replay(waiting);
            }
        }
    }

    /// The scan of session `id` is over: what waited on it.
    fn scan_released(st: &mut State, id: Uuid) -> Vec<Waiting> {
        st.scan_pending.remove(&id);
        let (mine, others) = std::mem::take(&mut st.scan_waiting)
            .into_iter()
            .partition(|w| w.session() == id);
        st.scan_waiting = others;
        mine
    }

    /// Files what waited, as it came.
    fn replay(&self, waiting: Vec<Waiting>) {
        let mut hit = false;
        for w in waiting {
            match w {
                Waiting::Usage(limits, id, at) => self.report_usage_at(limits, Some(id), at),
                Waiting::Hit(id, limit, at) => {
                    let mut st = self.0.state.lock();
                    let raw = Self::raw_running(&st, id);
                    Self::mark_session_hit_at(&mut st, id, &raw, limit.as_ref(), at);
                    hit = true;
                }
            }
        }
        if hit {
            self.keep_standing();
            let desk = self.clone();
            tokio::spawn(async move { desk.accounts_moved().await });
        }
    }

    /// A `/login` in session `id`, the last `last` and the first since its
    /// process started at `since`: a new one is a sign-in inside it. From
    /// `since` it ran on the sign-in made there, so what it reported as its
    /// account's since is taken back (unless it signed in to that account
    /// again), and the limit it stopped on before (`hit_before`) is the
    /// account's. `release`: the scan it waited on is over.
    async fn login_seen(
        &self,
        id: Uuid,
        last: String,
        since: DateTime<Utc>,
        hit_before: Option<(DateTime<Utc>, crate::limit::LimitHit)>,
        release: bool,
    ) {
        let new = {
            let mut st = self.0.state.lock();
            let started = st.live.get(&id).map(|l| l.started_at);
            let before = st.logins_seen.insert(id, last.clone());
            match before {
                Some(seen) => seen != last,
                None => started.is_some_and(|s| since >= s),
            }
        };
        if !new {
            if release {
                let waiting = Self::scan_released(&mut self.0.state.lock(), id);
                self.replay(waiting);
            }
            return;
        }
        // Which account it signed in to there, as the CLI says now. One
        // found as the hub took it back was before this hub started: the
        // read the hub starts with (or one since) says it.
        if release {
            self.read_engine_since(Engine::Claude, self.read_asked(), super::Since::Ended)
                .await;
        } else {
            self.read_engine_now(Engine::Claude).await;
        }
        // Taken back, moved to the sign-in and what waited filed in one go:
        // nothing it reports in between is filed on the account.
        let (moved, taken, waiting) = {
            let mut guard = self.0.state.lock();
            let st = &mut *guard;
            let was_on = Self::running_on(st, id);
            let moved = was_on.is_some() && st.relogged.insert(id);
            let key = Self::own_key(st);
            if let Some(l) = st.live.get_mut(&id) {
                l.own_key = Some(key);
            }
            let taken = was_on
                .filter(|account| Self::own_account(st) != Some(*account))
                .map(|account| Self::take_back_locked(st, id, account, since, hit_before));
            let waiting = if release {
                Self::scan_released(st, id)
            } else {
                Vec::new()
            };
            (moved, taken.map(|t| (was_on, t)), waiting)
        };
        if moved {
            tracing::info!(session = %id, "a session signed in again inside: it runs on this computer's sign-in now");
            self.keep_relogged();
        }
        if let Some((Some(account), t)) = taken {
            tracing::info!(session = %id, %account, forgot = t.exact.is_some(), "a session ran on this computer's sign-in since a /login inside it: what it reported since is no longer its account's");
            if let (Some(plans), Some(ids)) = (&self.0.plans, t.ids) {
                if t.exact.is_some() || t.instead.is_some() {
                    plans.forget_local(
                        crate::usage::LocalPlan::Claude,
                        crate::usage::LocalAccount { ids, plan: None },
                        t.exact,
                        t.instead,
                    );
                }
            }
            self.keep_standing();
            self.keep_prints();
        }
        self.replay(waiting);
        self.announce_accounts();
        let desk = self.clone();
        tokio::spawn(async move { desk.accounts_moved().await });
    }

    /// Session `id` ran on another sign-in from `since`, though it was filed
    /// on `account`: what it wrote of the account since is taken back (its
    /// last reading, its last stop, its prints), never what another session,
    /// the hub's own call or another computer wrote. Kept from before this
    /// was known, an unknown writer is taken for it only while no other
    /// session runs on the account. The limit it stopped on before `since`
    /// is the account's.
    fn take_back_locked(
        st: &mut State,
        id: Uuid,
        account: Uuid,
        since: DateTime<Utc>,
        hit_before: Option<(DateTime<Utc>, crate::limit::LimitHit)>,
    ) -> TakenBack {
        let raw = Raw::Account(account);
        let others = st.records.values().any(|r| {
            r.id != id && st.live.contains_key(&r.id) && Self::running_on(st, r.id) == Some(account)
        });
        let by = st.written_by.get(&raw).cloned().unwrap_or_default();
        let its = |w: Option<Writer>| match w {
            Some(Writer::Session(s)) => s == id,
            None => !others,
            Some(_) => false,
        };
        let hit_before = hit_before.filter(|(at, _)| *at < since);
        let s = st.standing.entry(Some(account)).or_default();
        let mut exact = None;
        if s.read_at.is_some_and(|r| r >= since) && its(by.read) {
            exact = s.read_at.take();
            s.windows.clear();
        }
        let mut hit_by = by.hit;
        if s.hit_at.is_some_and(|h| h >= since) && its(by.hit) {
            s.hit_at = None;
            s.hit_until = None;
            s.hit_window = None;
            hit_by = None;
        }
        if let Some((at, hit)) = &hit_before {
            if s.hit_at.is_none_or(|h| h < *at) {
                s.hit_at = Some(*at);
                s.hit_until = hit.resets_at;
                s.hit_window = hit.minutes();
                hit_by = Some(Writer::Session(id));
            }
        }
        // The limit it stopped on, as a reading of the account's own: the
        // window full until it resets. Said by the scan, else by the stop
        // the account's standing keeps from before; never from a model's
        // own limit, which isn't the plan's window.
        let instead = match &hit_before {
            Some((at, hit)) => hit.minutes().zip(hit.resets_at).map(|w| (w, *at)),
            None => s
                .hit_window
                .zip(s.hit_until)
                .zip(s.hit_at)
                .filter(|&((_, until), at)| at < since && until > since),
        }
        .and_then(|((minutes, resets), at)| {
            let kind = match minutes {
                300 => "five_hour",
                10_080 => "seven_day",
                _ => return None,
            };
            Some((
                serde_json::json!([{ "kind": kind, "percentUsed": 100, "resetsAt": resets.to_rfc3339() }]),
                at,
            ))
        });
        let read_by = if exact.is_some() { None } else { by.read };
        st.written_by.insert(
            raw.clone(),
            WrittenBy {
                read: read_by,
                hit: hit_by,
            },
        );
        if exact.is_some() {
            if let Some(p) = st.prints.get_mut(&raw) {
                p.recent.retain(|p| p.at < since);
                p.agreed.retain(|(_, mine)| *mine < since);
            }
        }
        TakenBack {
            ids: Self::plan_ids(st, &raw),
            exact,
            instead,
        }
    }
}
