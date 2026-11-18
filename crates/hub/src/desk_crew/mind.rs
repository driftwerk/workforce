//! One mind per boss (designs/boss-orders-and-serial-turns.md §2): the
//! boss's own deciding, whatever woke it, runs one stretch at a time. A
//! line turn, a check-in, a plan turn, a handover's delivery and Act
//! fully's focused calls that tell a session something each hold the
//! boss's mind while they decide, so no two of them act on the same
//! picture at once and tell two sessions the same thing.
//!
//! - **The person's own turns never wait on it.** A turn they typed in the
//!   boss's DM starts at once; while one runs, crew turns aren't started
//!   ([`CrewHub::mind_free`]), and their words reach a crew turn already
//!   running through the picture.
//! - **Decisions on requests don't take it.** A session waits on them; the
//!   boss's assignments guard what they allow.
//! - **Reentrant.** A turn's tools run in tasks of their own, so the
//!   holder is known by the turn's runner session, as the picture's
//!   bases are: a step for the same boss inside a turn holding its mind
//!   goes on without waiting.
//! - **Bounded.** Past [`WAIT_MAX`] a waiter goes on without the mind and
//!   says so: a wedged boss is worse than two minds.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::CrewHub;

/// The longest anything waits for a boss's mind before it goes on without
/// it.
pub(super) const WAIT_MAX: Duration = Duration::from_secs(20 * 60);

/// The longest a focused call waits for its boss's mind: past it the item
/// is left for the boss's next look, or its full turn.
pub(super) const FOCUSED_WAIT: Duration = Duration::from_secs(60);

/// A wait this long is worth a warning.
const WAIT_SAID: Duration = Duration::from_secs(60);

/// What a boss's mind is doing, while it is held.
#[derive(Debug, Clone)]
pub(super) struct Holding {
    pub what: String,
    pub since: Instant,
}

/// Every boss's mind, and who holds it.
#[derive(Default)]
pub(super) struct Minds {
    locks: parking_lot::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// What each held mind is doing.
    holding: parking_lot::Mutex<HashMap<String, Holding>>,
    /// How many wait for each boss's mind.
    waiting: parking_lot::Mutex<HashMap<String, usize>>,
    /// The runner sessions of turns running under a boss's mind: the boss,
    /// and the runner session the turn began on (it may roll over).
    turns: parking_lot::Mutex<HashMap<String, (String, String)>>,
    /// The person's own turns running in each boss's DM.
    person: parking_lot::Mutex<HashMap<String, usize>>,
}

tokio::task_local! {
    /// The boss whose mind the task holds: a turn it starts runs under it.
    static HELD: String;
}

impl Minds {
    fn lock_of(&self, boss: &str) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(self.locks.lock().entry(boss.to_string()).or_default())
    }

    /// Whether the code running now already holds `boss`'s mind: the task
    /// that took it, or a tool of a turn running under it.
    fn held_here(&self, boss: &str) -> bool {
        if HELD.try_with(|b| b == boss).unwrap_or(false) {
            return true;
        }
        workforce_agent::runner::CURRENT_SESSION_ID
            .try_with(|sid| self.turns.lock().get(sid).is_some_and(|(b, _)| b == boss))
            .unwrap_or(false)
    }
}

/// Marks a turn's runner session as running under a boss's mind, while it
/// runs.
pub(super) struct TurnUnder<'a> {
    minds: &'a Minds,
    sid: String,
}

impl Drop for TurnUnder<'_> {
    fn drop(&mut self) {
        self.minds
            .turns
            .lock()
            .retain(|_, (_, began)| *began != self.sid);
    }
}

/// A person's turn in a boss's DM, counted while it runs.
pub(super) struct PersonTurn {
    crew: std::sync::Weak<CrewHub>,
    boss: String,
}

impl Drop for PersonTurn {
    fn drop(&mut self) {
        let Some(crew) = self.crew.upgrade() else {
            return;
        };
        let mut person = crew.minds.person.lock();
        if let Some(n) = person.get_mut(&self.boss) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                person.remove(&self.boss);
            }
        }
    }
}

/// Counts one waiter for a boss's mind while it waits.
struct Waiting<'a> {
    minds: &'a Minds,
    boss: String,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        let mut w = self.minds.waiting.lock();
        if let Some(n) = w.get_mut(&self.boss) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                w.remove(&self.boss);
            }
        }
    }
}

impl CrewHub {
    /// Runs `f` holding `boss`'s mind, doing `what`, waiting for it as long
    /// as [`WAIT_MAX`]; past that `f` runs without it, said.
    ///
    /// `f` is boxed as it is handed over: the turns it wraps are large, and
    /// the future returned holds only the box, so wrapping a step never
    /// makes the task that awaits it any bigger.
    pub(super) fn with_mind<'a, F: Future + 'a>(
        &'a self,
        boss: &'a str,
        what: &'a str,
        f: F,
    ) -> impl Future<Output = F::Output> + 'a {
        let f = Box::pin(f);
        async move {
            match self.mind_held(boss, what, WAIT_MAX, f).await {
                Ok(out) => out,
                Err(f) => {
                    let holder = self.mind_holder(boss);
                    tracing::warn!(
                        %boss,
                        %what,
                        waited_secs = WAIT_MAX.as_secs(),
                        holder = ?holder.as_ref().map(|h| &h.what),
                        held_secs = ?holder.as_ref().map(|h| h.since.elapsed().as_secs()),
                        "a boss's mind stayed held too long; this goes on without it"
                    );
                    f.await
                }
            }
        }
    }

    /// Runs `f` holding `boss`'s mind, doing `what`, if it is had within
    /// `wait`; else `f` back, not run. Boxed as [`Self::with_mind`] is.
    pub(super) fn mind_within<'a, F: Future + 'a>(
        &'a self,
        boss: &'a str,
        what: &'a str,
        wait: Duration,
        f: F,
    ) -> impl Future<Output = Result<F::Output, Pin<Box<F>>>> + 'a {
        self.mind_held(boss, what, wait, Box::pin(f))
    }

    async fn mind_held<F: Future>(
        &self,
        boss: &str,
        what: &str,
        wait: Duration,
        f: Pin<Box<F>>,
    ) -> Result<F::Output, Pin<Box<F>>> {
        if self.minds.held_here(boss) {
            return Ok(f.await);
        }
        let lock = self.minds.lock_of(boss);
        let asked = Instant::now();
        let guard = {
            *self
                .minds
                .waiting
                .lock()
                .entry(boss.to_string())
                .or_default() += 1;
            let _waiting = Waiting {
                minds: &self.minds,
                boss: boss.to_string(),
            };
            match tokio::time::timeout(wait, lock.lock_owned()).await {
                Ok(guard) => guard,
                Err(_) => return Err(f),
            }
        };
        let waited = asked.elapsed();
        if waited >= WAIT_SAID {
            tracing::warn!(%boss, %what, waited_secs = waited.as_secs(), "a boss's step waited for its mind");
        }
        self.minds.holding.lock().insert(
            boss.to_string(),
            Holding {
                what: what.to_string(),
                since: Instant::now(),
            },
        );
        let out = HELD.scope(boss.to_string(), f).await;
        self.minds.holding.lock().remove(boss);
        drop(guard);
        Ok(out)
    }

    /// The turn on runner session `sid` of `boss`'s runs under its mind,
    /// when the code starting it holds the mind: its tools are the mind's.
    pub(super) fn turn_under_mind(&self, boss: &str, sid: &str) -> Option<TurnUnder<'_>> {
        if !HELD.try_with(|b| b == boss).unwrap_or(false) {
            return None;
        }
        self.minds
            .turns
            .lock()
            .insert(sid.to_string(), (boss.to_string(), sid.to_string()));
        Some(TurnUnder {
            minds: &self.minds,
            sid: sid.to_string(),
        })
    }

    /// The turn on runner session `old` goes on as `new`: still under the
    /// mind it began under, until the turn is done.
    pub(super) fn mind_turn_rolled(&self, old: &str, new: &str) {
        let mut turns = self.minds.turns.lock();
        if let Some(held) = turns.get(old).cloned() {
            turns.insert(new.to_string(), held);
        }
    }

    /// A turn the person typed in `boss`'s DM began: counted until the
    /// returned mark goes.
    pub(super) fn person_turn_began(&self, boss: &str) -> PersonTurn {
        *self
            .minds
            .person
            .lock()
            .entry(boss.to_string())
            .or_default() += 1;
        PersonTurn {
            crew: self.me.clone(),
            boss: boss.to_string(),
        }
    }

    /// Whether crew work for `boss` may start now: nothing holds or waits
    /// for its mind, and none of the person's turns runs in its DM.
    pub(super) fn mind_free(&self, boss: &str) -> bool {
        !self.minds.holding.lock().contains_key(boss)
            && !self.minds.waiting.lock().contains_key(boss)
            && !self.minds.person.lock().contains_key(boss)
    }

    /// What `boss`'s mind is doing, while held.
    pub(super) fn mind_holder(&self, boss: &str) -> Option<Holding> {
        self.minds.holding.lock().get(boss).cloned()
    }
}
