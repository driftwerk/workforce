//! Sessions' running accounts, kept by the hub's cheap model for every
//! session a boss oversees: at each turn's end from the exchanges the
//! harvest kept (the transcript's tail and the screen when it has none),
//! while a turn runs only its `now` every few minutes, and on a read that
//! finds one stale. One refresh per session at a time; a write lands only
//! on the version it read, and one that loses merges onto the newer
//! account. Past the day's ceiling, or for a short turn, an account is
//! said plainly. The floor's turn-end gist is said from the same answer.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use uuid::Uuid;

use super::{Desk, DeskError};
use crate::account::{self, Account, Accountant, Call, Context, Limits, Read, Tail, Written};
use crate::account_store::{CheapKind, Put};

/// How often running turns are looked at for a `now` to write.
const LOOK_EVERY: Duration = Duration::from_secs(60);
/// How long a look or an input keeps a session attended.
const ATTENDED_FOR: Duration = Duration::from_secs(60 * 60);
/// The most exchanges read back to find the unread ones.
const READ_BACK: i64 = 100;
/// The most recent exchanges a question reads from.
const QUESTION_READ_BACK: i64 = 40;
/// The most refreshes one turn's end makes to catch up on a backlog.
const CATCH_UP_ROUNDS: usize = 3;
/// The longest one look at a running turn may take.
const LOOK_WITHIN: Duration = Duration::from_secs(90);

/// Why an account is refreshed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Moment {
    /// While its turn runs: `now` alone.
    MidTurn,
    /// Someone reads it and found it stale.
    Read,
    /// Its turn ended, and the harvest kept what it said.
    TurnEnd,
}

/// What a refresh gives its callers.
type Shared = Option<Result<Option<Account>, String>>;

/// One session's refresh under way: who waits on it, and whether a turn
/// that ended meanwhile wants one more.
struct Flight {
    tx: tokio::sync::watch::Sender<Shared>,
    again: Option<Moment>,
}

/// The account writer's share of the desk.
pub(super) struct AccountRun {
    pub(super) accountant: Option<Arc<dyn Accountant>>,
    flights: Mutex<HashMap<Uuid, Flight>>,
    limits: Mutex<Limits>,
    /// When each running turn's `now` was last written, and what its tail
    /// was then.
    mid_turn: Mutex<HashMap<Uuid, (Instant, u64)>>,
    /// When the person last looked at each session.
    looked: Mutex<HashMap<Uuid, Instant>>,
    /// When each session's turn last ended.
    pub(super) ended: Mutex<HashMap<Uuid, DateTime<Utc>>>,
    /// When a reader last refreshed each session's account.
    read_refreshed: Mutex<HashMap<Uuid, Instant>>,
    /// Model calls made, for tests.
    pub(super) calls: std::sync::atomic::AtomicUsize,
}

impl AccountRun {
    pub(super) fn new(accountant: Option<Arc<dyn Accountant>>) -> Self {
        Self {
            accountant,
            flights: Mutex::default(),
            limits: Mutex::default(),
            mid_turn: Mutex::default(),
            looked: Mutex::default(),
            ended: Mutex::default(),
            read_refreshed: Mutex::default(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

/// A session's account as a reader is given it.
#[derive(Debug, Clone)]
pub struct AccountRead {
    /// None: none is kept yet.
    pub account: Option<Account>,
    /// Refreshed for this reader.
    pub refreshed: bool,
    /// Older than it should be: say how fresh it is.
    pub stale: bool,
}

/// What a refresh found to say.
struct Said {
    written: Written,
    by_model: bool,
    reads: Vec<Read>,
    /// Closed exchanges are left unread for another round.
    more: bool,
}

fn fingerprint(tail: &Tail) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    tail.said.hash(&mut h);
    tail.screen.hash(&mut h);
    h.finish()
}

impl Desk {
    /// The ceilings the cheap model is held to: the main boss's
    /// `[oversight.updates]`, as the hub reads it.
    pub fn set_account_limits(&self, limits: Limits) {
        *self.0.accounts.limits.lock() = limits;
    }

    /// The person looked at session `id` (read it, typed in it).
    pub(super) fn account_looked(&self, id: Uuid) {
        self.0.accounts.looked.lock().insert(id, Instant::now());
    }

    /// Whether session `id` gets an account: a model writes them, it is
    /// the person's (not the hub's own), and a boss oversees its scope. The
    /// floor hands its turn-end gist to the account writer exactly then.
    pub fn account_kept_for(&self, id: Uuid) -> bool {
        if self.0.accounts.accountant.is_none() || self.hub_internal(id) {
            return false;
        }
        let Ok(v) = self.view(id) else {
            return false;
        };
        if crate::ledger::agent_turn(&v.cwd, &v.name) {
            return false;
        }
        let group = self.group_of_member(&super::session_item(id));
        self.bosses()
            .iter()
            .any(|b| self.seen_by(&b.scope).sees(group.as_deref()))
    }

    /// Whether anyone is about session `id` now: the person looked at it
    /// or gave it input within the hour, a boss runs it, or its last turn
    /// failed. Off-peak, an account is written at turn ends alone.
    fn account_attended(&self, id: Uuid) -> bool {
        let looked = self
            .0
            .accounts
            .looked
            .lock()
            .get(&id)
            .is_some_and(|t| t.elapsed() < ATTENDED_FOR);
        let input = {
            let st = self.0.state.lock();
            Self::last_input(&st, id)
        }
        .is_some_and(|t| {
            Utc::now() - t < chrono::Duration::from_std(ATTENDED_FOR).unwrap_or_default()
        });
        let failed = self.view(id).is_ok_and(|v| v.failure.is_some());
        looked || input || failed || self.charge_of(id).is_some()
    }

    /// Session `id`'s account as kept, refreshed by none.
    pub async fn session_account(&self, id: Uuid) -> Result<Option<Account>, DeskError> {
        Ok(self.0.store.session_account(id).await?)
    }

    /// The accounts kept of sessions `ids`.
    pub async fn session_accounts_of(&self, ids: &[Uuid]) -> Result<Vec<Account>, DeskError> {
        Ok(self.0.store.session_accounts(ids).await?)
    }

    /// Session `id`'s account for a reader (the boss, before it says
    /// anything of it): refreshed first when it is stale (older than
    /// [`account::FRESH_FOR`] while it works, or a turn ended since it was
    /// written), at most once per session in that time, within
    /// [`account::WRITE_WITHIN`]; else the one kept, said to be stale.
    pub async fn account_read(&self, id: Uuid) -> Result<AccountRead, DeskError> {
        let kept = self.0.store.session_account(id).await?;
        let now = crate::crew::now();
        let working = self
            .view(id)
            .is_ok_and(|v| v.state == crate::model::SessionState::Working);
        let ended = self.0.accounts.ended.lock().get(&id).copied();
        let stale = |a: &Option<Account>| match a {
            None => true,
            Some(a) => a.stale(now, working) || ended.is_some_and(|e| e > a.written_at),
        };
        if !stale(&kept) || !self.account_kept_for(id) {
            let stale = kept.is_some() && stale(&kept);
            return Ok(AccountRead {
                account: kept,
                refreshed: false,
                stale,
            });
        }
        // One reader's refresh per session in the floor's time: a later
        // reader gets what it wrote.
        let due = {
            let mut read = self.0.accounts.read_refreshed.lock();
            match read.get(&id) {
                Some(at) if at.elapsed() < account::FRESH_FOR => false,
                _ => {
                    read.insert(id, Instant::now());
                    true
                }
            }
        };
        if !due {
            let stale = kept.is_some() && stale(&kept);
            return Ok(AccountRead {
                account: kept,
                refreshed: false,
                stale,
            });
        }
        let within = account::WRITE_WITHIN + Duration::from_secs(5);
        match tokio::time::timeout(within, self.account_refresh(id, Moment::Read)).await {
            Ok(Ok(Some(a))) => Ok(AccountRead {
                account: Some(a),
                refreshed: true,
                stale: false,
            }),
            Ok(Ok(None)) => Ok(AccountRead {
                account: kept,
                refreshed: false,
                stale: false,
            }),
            Ok(Err(e)) => {
                tracing::info!(session = %id, error = %e, "a session's account couldn't be refreshed for a reader; the kept one is given");
                Ok(AccountRead {
                    stale: kept.is_some(),
                    account: kept,
                    refreshed: false,
                })
            }
            Err(_) => Ok(AccountRead {
                stale: kept.is_some(),
                account: kept,
                refreshed: false,
            }),
        }
    }

    /// The answer to `question` about session `id`, from its account and
    /// up to about 12k tokens of its recent conversation, by the cheap
    /// model within the day's share of questions. None when no model may
    /// be asked or the record doesn't say.
    pub async fn account_question(
        &self,
        id: Uuid,
        question: &str,
    ) -> Result<Option<String>, DeskError> {
        let v = self.view(id)?;
        let kept = self.0.store.session_account(id).await?;
        let recent: Vec<account::Exchange> = self
            .0
            .store
            .account_exchanges(id, None, QUESTION_READ_BACK, false)
            .await
            ?
            .into_iter()
            // What reads as instructions to a model is never given to one.
            .filter(|(_, flagged)| !flagged)
            .map(|(x, _)| x)
            .collect();
        if recent.is_empty() && kept.is_none() {
            return Ok(None);
        }
        let nonce = Uuid::new_v4().simple().to_string()[..16].to_string();
        let prompt = account::question_prompt(kept.as_ref(), v.label(), &recent, question, &nonce);
        let answer = self
            .account_ask(
                id,
                v.engine,
                CheapKind::Question,
                Call::Question,
                account::QUESTION_SYSTEM,
                &prompt,
            )
            .await?;
        let answer = answer
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty() && !a.contains(account::NOT_IN_RECORD));
        Ok(answer.map(|a| self.account_redacted_text(a)))
    }

    /// A turn of session `id` ended and the harvest read it: its account
    /// is refreshed off the caller.
    pub(super) fn account_turn_ended(&self, id: Uuid) {
        if !self.account_kept_for(id) {
            return;
        }
        self.0.accounts.mid_turn.lock().remove(&id);
        self.0.accounts.ended.lock().insert(id, crate::crew::now());
        let desk = self.clone();
        tokio::spawn(async move {
            if let Err(e) = desk.account_refresh(id, Moment::TurnEnd).await {
                tracing::info!(session = %id, error = %e, "couldn't refresh a session's account");
            }
        });
    }

    /// Refreshes session `id`'s account for `moment`, once at a time: a
    /// caller that finds one under way waits for its result, and a turn
    /// end that comes meanwhile gets one more refresh after it. None for a
    /// session that gets no account.
    pub async fn account_refresh(
        &self,
        id: Uuid,
        moment: Moment,
    ) -> Result<Option<Account>, DeskError> {
        let mut rx = {
            let mut flights = self.0.accounts.flights.lock();
            match flights.get_mut(&id) {
                Some(f) => {
                    if moment == Moment::TurnEnd {
                        f.again = Some(moment);
                    }
                    f.tx.subscribe()
                }
                None => {
                    let (tx, rx) = tokio::sync::watch::channel(None);
                    flights.insert(id, Flight { tx, again: None });
                    // The refresh runs on its own, so a caller that stops
                    // waiting never leaves the others waiting.
                    let desk = self.clone();
                    tokio::spawn(async move {
                        let flight = desk.clone();
                        let run =
                            tokio::spawn(async move { flight.account_flight(id, moment).await });
                        if run.await.is_err() {
                            // It panicked: its callers are let go (the
                            // sender drops) and the next refresh starts anew.
                            tracing::warn!(session = %id, "a session's account refresh stopped short");
                            desk.0.accounts.flights.lock().remove(&id);
                        }
                    });
                    rx
                }
            }
        };
        let shared = rx
            .wait_for(Option::is_some)
            .await
            .map_err(|_| DeskError::Invalid("the account's refresh stopped".into()))?
            .clone();
        match shared {
            Some(Ok(a)) => Ok(a),
            Some(Err(e)) => Err(DeskError::Invalid(e)),
            None => Ok(None),
        }
    }

    /// Runs one session's refreshes until no caller wants another, then
    /// tells every caller the last result.
    async fn account_flight(&self, id: Uuid, first: Moment) {
        let mut moment = first;
        loop {
            let result = self
                .account_refresh_once(id, moment)
                .await
                .map_err(|e| format!("{e:#}"));
            let mut flights = self.0.accounts.flights.lock();
            let again = flights.get_mut(&id).and_then(|f| f.again.take());
            match again {
                Some(next) => moment = next,
                None => {
                    if let Some(f) = flights.remove(&id) {
                        let _ = f.tx.send(Some(result));
                    }
                    return;
                }
            }
        }
    }

    /// One refresh of session `id`'s account for `moment`.
    async fn account_refresh_once(
        &self,
        id: Uuid,
        moment: Moment,
    ) -> eyre::Result<Option<Account>> {
        if !self.account_kept_for(id) {
            return Ok(None);
        }
        let v = self.view(id).map_err(|e| eyre::eyre!("{e}"))?;
        let at = crate::crew::now();
        let kept = self
            .0
            .store
            .session_account(id)
            .await?
            .unwrap_or_else(|| Account::new(id, "", at));
        let group = self.group_of_member(&super::session_item(id));
        let notes = self
            .0
            .store
            .held_items(id, group.as_deref())
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|i| {
                matches!(i.kind, crate::memory::derive::ItemKind::Decision)
                    || matches!(i.state, crate::memory::derive::ItemState::Open)
            })
            .map(|i| format!("{}: {}", i.kind.as_str(), i.text))
            .collect();
        let ctx = Context {
            name: v.label().to_string(),
            engine: v.engine.title().to_string(),
            goal: Some(v.goal.clone()).filter(|g| !g.trim().is_empty()),
            state: match (v.state.is_running(), v.failure.is_some()) {
                (_, true) => "its last turn failed".into(),
                _ => format!("it is {}", v.state.as_str()),
            },
            notes,
        };
        let working = v.state == crate::model::SessionState::Working;
        let mut kept = kept;
        let mut handed_by_model = kept.by_model;
        // A backlog is caught up on in a few rounds, the oldest first.
        for _ in 0..CATCH_UP_ROUNDS {
            let said = match moment {
                Moment::MidTurn => match self.account_now(id, &kept, &ctx).await? {
                    Some(s) => s,
                    None => return Ok(Some(kept)),
                },
                Moment::Read | Moment::TurnEnd => {
                    let ended = moment == Moment::TurnEnd || !working;
                    match self
                        .account_delta(id, &v, &kept, &ctx, ended, working)
                        .await?
                    {
                        Some(s) => s,
                        None => break,
                    }
                }
            };
            let more = said.more && moment != Moment::MidTurn;
            handed_by_model = said.by_model;
            kept = self.account_write(id, &kept, said, at).await?;
            if !more {
                break;
            }
        }
        // The floor's gist comes from the account, also when nothing new
        // was said.
        if moment == Moment::TurnEnd && kept.version > 0 {
            self.account_hand_gist(id, &kept, handed_by_model).await;
        }
        Ok((kept.version > 0).then_some(kept))
    }

    /// Writes what `said` says onto `kept`, compare-and-set: the account
    /// as kept after.
    async fn account_write(
        &self,
        id: Uuid,
        kept: &Account,
        said: Said,
        at: chrono::DateTime<chrono::Utc>,
    ) -> eyre::Result<Account> {
        let asked_by = match kept.asked_by.is_empty() {
            true => Some(self.account_asked_by(id)),
            false => None,
        };
        let written = self.account_redacted(said.written);
        let mut fresh = account::merge(kept, &written, at);
        if let Some(a) = &asked_by {
            fresh.asked_by = a.clone();
        }
        fresh.cursor = kept.cursor.with(said.reads.iter().cloned());
        // A plain write never says the model's words are gone.
        fresh.by_model = said.by_model || kept.by_model;
        fresh.stale_after = None;
        Ok(match self.0.store.session_account_put(&fresh).await? {
            Put::Kept(v) => Account {
                version: v,
                ..fresh
            },
            Put::Lost => {
                // Another refresh wrote first: this one's delta goes onto
                // it, once; it never overwrites.
                let newer = self
                    .0
                    .store
                    .session_account(id)
                    .await?
                    .ok_or_else(|| eyre::eyre!("the account was forgotten meanwhile"))?;
                let mut again = account::merge(&newer, &written, at);
                if newer.asked_by.is_empty() {
                    if let Some(a) = asked_by {
                        again.asked_by = a;
                    }
                }
                again.cursor = newer.cursor.with(said.reads.iter().cloned());
                again.by_model = said.by_model || newer.by_model;
                match self.0.store.session_account_put(&again).await? {
                    Put::Kept(v) => Account {
                        version: v,
                        ..again
                    },
                    Put::Lost => {
                        tracing::info!(session = %id, "a session's account moved on twice while it was refreshed; the newer one stands");
                        self.0.store.session_account(id).await?.unwrap_or(newer)
                    }
                }
            }
        })
    }

    /// Hands the floor session `id`'s turn-end gist, said from `a`.
    async fn account_hand_gist(&self, id: Uuid, a: &Account, by_model: bool) {
        if let Some(accountant) = &self.0.accounts.accountant {
            let (purpose, now) = account::gist_of(a);
            accountant.gist(id, purpose, now, by_model).await;
        }
    }

    /// Who asked for session `id`: a boss that runs it, else the person,
    /// with when it began.
    fn account_asked_by(&self, id: Uuid) -> String {
        let at = self.record(id).ok().map(|r| r.created_at).map(|t| {
            t.with_timezone(&self.timezone())
                .format("%H:%M")
                .to_string()
        });
        let who = match self.charge_of(id) {
            Some(c) => format!("Desk ({})", c.boss),
            None => "you".to_string(),
        };
        match at {
            Some(at) => format!("{who}, {at}"),
            None => who,
        }
    }

    /// What the model wrote, as safely kept as what it read.
    fn account_redacted_text(&self, text: String) -> String {
        match &self.0.ledger.redactor {
            Some(r) => r.stored(&text).text.into_owned(),
            None => text,
        }
    }

    fn account_redacted(&self, mut w: Written) -> Written {
        let Some(r) = self.0.ledger.redactor.clone() else {
            return w;
        };
        let hide = |s: &mut String| *s = r.stored(s).text.into_owned();
        for s in [
            &mut w.purpose,
            &mut w.plan,
            &mut w.now,
            &mut w.waiting_on,
            &mut w.next,
        ]
        .into_iter()
        .flatten()
        {
            hide(s);
        }
        for (text, refs) in &mut w.done {
            hide(text);
            for r in refs {
                hide(&mut r.id);
                if let Some(url) = &mut r.url {
                    hide(url);
                }
            }
        }
        for (what, why) in &mut w.decisions {
            hide(what);
            hide(why);
        }
        w
    }

    /// Session `id`'s transcript's latest words and the end of its screen.
    async fn account_tail(&self, id: Uuid) -> Tail {
        let screen = self.terminal_text(id, 3000).ok();
        let said = match self.session_record(id).await {
            Some(r) => {
                let desk = self.clone();
                tokio::task::spawn_blocking(move || {
                    let path = desk.records_of(&r).into_iter().next_back()?;
                    let tail = match r.engine {
                        crate::engine::Engine::Codex => crate::transcript::read_codex_tail(&path),
                        _ => crate::transcript::read_tail(&path),
                    };
                    tail.ok().and_then(|t| t.said.or(t.last_reply))
                })
                .await
                .ok()
                .flatten()
            }
            None => None,
        };
        Tail { said, screen }
    }

    /// Takes one of the day's cheap calls of `kind`; false past a ceiling.
    pub(super) async fn account_take(&self, kind: CheapKind) -> eyre::Result<bool> {
        let limits = *self.0.accounts.limits.lock();
        let share = match kind {
            CheapKind::Question => Some(limits.account_questions_per_day),
            CheapKind::Account => None,
        };
        self.0
            .store
            .cheap_take(self.derive_day(), kind, limits.cheap_calls_per_day, share)
            .await
    }

    /// Whether the day's cheap calls are spent.
    pub async fn account_thin(&self) -> bool {
        let cap = self.0.accounts.limits.lock().cheap_calls_per_day;
        self.0
            .store
            .cheap_calls(self.derive_day())
            .await
            .is_ok_and(|n| n >= i64::from(cap))
    }

    /// Asks the model `system` and `user` about session `id` for `call`,
    /// within the day's ceiling: None when no model may be asked (none,
    /// past the ceiling, it failed or gave no answer in time).
    pub(super) async fn account_ask(
        &self,
        id: Uuid,
        engine: crate::engine::Engine,
        kind: CheapKind,
        call: Call,
        system: &str,
        user: &str,
    ) -> eyre::Result<Option<String>> {
        let Some(accountant) = self.0.accounts.accountant.clone() else {
            return Ok(None);
        };
        match self.account_take(kind).await {
            Ok(true) => {}
            Ok(false) => return Ok(None),
            Err(e) => {
                // The day's count can't be read: no model is asked, and
                // the account is said plainly.
                tracing::warn!(session = %id, error = format!("{e:#}"), "couldn't count the cheap model's calls; a session's account is said plainly");
                return Ok(None);
            }
        }
        self.0
            .accounts
            .calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let asked = tokio::time::timeout(
            account::WRITE_WITHIN,
            accountant.answer(id, engine, system, user, call),
        )
        .await;
        match asked {
            Ok(Ok(text)) => Ok(Some(text)),
            Ok(Err(e)) => {
                // No model took it: the day's count is given back.
                self.0.store.cheap_untake(self.derive_day(), kind).await?;
                tracing::info!(session = %id, error = format!("{e:#}"), "the model couldn't write a session's account; it is said plainly");
                Ok(None)
            }
            Err(_) => {
                tracing::info!(session = %id, "the model gave no account in time; it is said plainly");
                Ok(None)
            }
        }
    }

    /// The changes a turn's end (or a read) makes: from the exchanges not
    /// yet read, the transcript and screen when they don't say enough;
    /// plainly for a short turn or past the ceiling. None with nothing new.
    async fn account_delta(
        &self,
        id: Uuid,
        v: &crate::model::SessionView,
        kept: &Account,
        ctx: &Context,
        ended: bool,
        working: bool,
    ) -> eyre::Result<Option<Said>> {
        // A first account starts from the newest exchanges (its goal says
        // what came before); after that every exchange is read in order,
        // the oldest unread first, so none is passed over.
        let first = kept.cursor.newest.is_none();
        let rows = self
            .0
            .store
            .account_exchanges(id, kept.cursor.since(), READ_BACK, !first)
            .await?;
        let mut unread: Vec<(account::Exchange, bool)> = rows
            .into_iter()
            .filter(|(x, _)| x.ended.is_none() || kept.cursor.unread(&x.key, x.at))
            .collect();
        if first {
            unread.reverse();
        }
        // What reads as instructions to a model is never given to one.
        let given: Vec<account::Exchange> = unread
            .iter()
            .filter(|(_, flagged)| !flagged)
            .map(|(x, _)| x.clone())
            .collect();
        let delta = match first {
            true => account::fit(&given),
            false => account::fit_oldest(&given),
        };
        // Read now: every closed exchange up to the last one given (all of
        // them for a first account); one still running is read again once
        // it closes. Closed ones past it wait for the next round.
        let upto = match first {
            true => unread.len(),
            false => match delta.last() {
                Some(last) => unread
                    .iter()
                    .position(|(x, _)| x.key == last.key)
                    .map_or(unread.len(), |i| i + 1),
                None => unread.len(),
            },
        };
        let reads: Vec<Read> = unread[..upto]
            .iter()
            .filter(|(x, _)| x.ended.is_some())
            .map(|(x, _)| Read {
                key: x.key.clone(),
                at: x.at,
            })
            .collect();
        let more = unread[upto..].iter().any(|(x, _)| x.ended.is_some());
        // The harvest is off or behind, or the turn still runs: what the
        // transcript and screen say fills in.
        let closed = delta.iter().any(|x| x.ended.is_some());
        let tail = match !closed || working {
            true => self.account_tail(id).await,
            false => Tail::default(),
        };
        if delta.is_empty() && tail.is_empty() {
            // Only flagged exchanges: read, so they aren't looked at again.
            return Ok((!reads.is_empty()).then(|| Said {
                written: Written::default(),
                by_model: kept.by_model,
                reads,
                more,
            }));
        }
        if account::short(delta) {
            return Ok(Some(Said {
                written: account::plainly(kept, ctx, delta, &tail, ended),
                by_model: false,
                reads,
                more,
            }));
        }
        let nonce = Uuid::new_v4().simple().to_string()[..16].to_string();
        let prompt = account::prompt(kept, ctx, delta, &tail, &nonce);
        let answer = self
            .account_ask(
                id,
                v.engine,
                CheapKind::Account,
                Call::Refresh,
                account::SYSTEM,
                &prompt,
            )
            .await?;
        let written = answer.as_deref().and_then(account::parse);
        Ok(Some(match written {
            Some(w) => Said {
                written: w,
                by_model: true,
                reads,
                more,
            },
            None => Said {
                written: account::plainly(kept, ctx, delta, &tail, ended),
                by_model: false,
                reads,
                more,
            },
        }))
    }

    /// A running turn's `now`, from the transcript's latest words and the
    /// screen: None when neither moved since it was last written.
    async fn account_now(
        &self,
        id: Uuid,
        kept: &Account,
        ctx: &Context,
    ) -> eyre::Result<Option<Said>> {
        let tail = self.account_tail(id).await;
        let print = fingerprint(&tail);
        if tail.is_empty()
            || self
                .0
                .accounts
                .mid_turn
                .lock()
                .get(&id)
                .is_some_and(|(_, p)| *p == print)
        {
            return Ok(None);
        }
        self.0
            .accounts
            .mid_turn
            .lock()
            .insert(id, (Instant::now(), print));
        let short = tail
            .said
            .as_deref()
            .is_some_and(|s| s.chars().count() < account::SHORT_REPLY)
            && tail.screen.as_deref().is_none_or(|s| s.trim().is_empty());
        let plain = || Said {
            written: Written {
                now: account::plainly(kept, ctx, &[], &tail, false).now,
                ..Written::default()
            },
            by_model: false,
            reads: vec![],
            more: false,
        };
        if short {
            return Ok(Some(plain()));
        }
        let engine = self.view(id).map_err(|e| eyre::eyre!("{e}"))?.engine;
        let nonce = Uuid::new_v4().simple().to_string()[..16].to_string();
        let answer = self
            .account_ask(
                id,
                engine,
                CheapKind::Account,
                Call::Now,
                account::NOW_SYSTEM,
                &account::now_prompt(kept, ctx, &tail, &nonce),
            )
            .await?;
        Ok(Some(match answer.as_deref().and_then(account::parse_now) {
            Some(now) => Said {
                written: Written {
                    now: Some(now),
                    ..Written::default()
                },
                by_model: true,
                reads: vec![],
                more: false,
            },
            None => plain(),
        }))
    }

    /// Writes running turns' `now` every few minutes, for the sessions
    /// someone is about (off-peak ones wait for their turn's end), until
    /// the desk stops.
    pub(super) async fn account_loop(self) {
        if self.0.accounts.accountant.is_none() {
            return;
        }
        let mut tick = tokio::time::interval(LOOK_EVERY);
        loop {
            tokio::select! {
                _ = self.0.cancel.cancelled() => return,
                _ = tick.tick() => {}
            }
            let due: Vec<Uuid> = self
                .list()
                .into_iter()
                .filter(|v| v.state == crate::model::SessionState::Working)
                .map(|v| v.id)
                .filter(|id| {
                    self.0
                        .accounts
                        .mid_turn
                        .lock()
                        .get(id)
                        .is_none_or(|(t, _)| t.elapsed() >= account::MID_TURN_EVERY)
                })
                .filter(|id| self.account_kept_for(*id) && self.account_attended(*id))
                .collect();
            for id in due {
                match tokio::time::timeout(LOOK_WITHIN, self.account_refresh(id, Moment::MidTurn))
                    .await
                {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        tracing::debug!(session = %id, error = %e, "couldn't write a running turn's now")
                    }
                    Err(_) => {
                        tracing::info!(session = %id, "a running turn's now took too long; the others are looked at")
                    }
                }
            }
        }
    }
}
