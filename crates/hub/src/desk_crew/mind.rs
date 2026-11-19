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
//!   says so: a wedged boss is worse than two minds. Work the loops hold
//!   back while the mind is busy (line turns, check-ins, plan turns) is
//!   started anyway once it has waited [`STARVE`], and queues for the mind
//!   in turn; the person's own turn holds crew work back for at most
//!   [`PERSON_YIELD`].
//! - **Never leaked.** What holds the mind is marked by a guard: a stretch
//!   that panics or is dropped frees it as one that ends does.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::CrewHub;

/// The longest anything waits for a boss's mind before it goes on without
/// it.
pub(super) const WAIT_MAX: Duration = Duration::from_secs(20 * 60);

/// The longest a focused call waits for its boss's mind: past it the item
/// is handed on (to a work turn, or to the session's line).
pub(super) const FOCUSED_WAIT: Duration = Duration::from_secs(60);

/// How long work the loops hold back for a busy mind waits before it is
/// started anyway, to queue for the mind in turn.
pub(super) const STARVE: Duration = Duration::from_secs(2 * 60);

/// The longest one of the person's turns holds the boss's crew work back.
pub(super) const PERSON_YIELD: Duration = Duration::from_secs(10 * 60);

/// A wait this long is worth a warning.
const WAIT_SAID: Duration = Duration::from_secs(60);

/// What a boss's mind is doing, while it is held.
#[derive(Debug, Clone)]
pub(super) struct Holding {
    pub what: String,
    pub since: Instant,
}

/// Every boss's mind, and who holds it.
pub(super) struct Minds {
    locks: parking_lot::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// What each held mind is doing.
    holding: parking_lot::Mutex<HashMap<String, Holding>>,
    /// How many wait for each boss's mind.
    waiting: parking_lot::Mutex<HashMap<String, usize>>,
    /// The runner sessions of turns running under a boss's mind: the boss,
    /// and the runner session the turn began on (it may roll over).
    turns: parking_lot::Mutex<HashMap<String, (String, String)>>,
    /// The person's own turns running in each boss's DM, by mark, with
    /// when each began.
    person: parking_lot::Mutex<HashMap<String, HashMap<u64, Instant>>>,
    marks: AtomicU64,
    /// [`WAIT_MAX`], shorter in tests.
    wait_max: parking_lot::Mutex<Duration>,
    /// The stretch each boss's bound was last said for, by when it began:
    /// what waits on one wedged stretch is said once, not once a waiter.
    told: parking_lot::Mutex<HashMap<String, Instant>>,
}

impl Default for Minds {
    fn default() -> Self {
        Self {
            locks: Default::default(),
            holding: Default::default(),
            waiting: Default::default(),
            turns: Default::default(),
            person: Default::default(),
            marks: AtomicU64::new(0),
            wait_max: parking_lot::Mutex::new(WAIT_MAX),
            told: Default::default(),
        }
    }
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

    /// Whether one of the person's turns in `boss`'s DM runs, begun within
    /// [`PERSON_YIELD`].
    fn person_running(&self, boss: &str) -> bool {
        self.person
            .lock()
            .get(boss)
            .is_some_and(|t| t.values().any(|at| at.elapsed() < PERSON_YIELD))
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
    mark: u64,
}

impl Drop for PersonTurn {
    fn drop(&mut self) {
        let Some(crew) = self.crew.upgrade() else {
            return;
        };
        let mut person = crew.minds.person.lock();
        if let Some(t) = person.get_mut(&self.boss) {
            t.remove(&self.mark);
            if t.is_empty() {
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

/// Says what holds a boss's mind while it is held: gone with the stretch,
/// however it ends (done, dropped, or panicked).
struct HoldingMark<'a> {
    minds: &'a Minds,
    boss: String,
}

impl Drop for HoldingMark<'_> {
    fn drop(&mut self) {
        self.minds.holding.lock().remove(&self.boss);
    }
}

impl CrewHub {
    /// Says, in the activity and so on the Floor, that `what` waited `wait`
    /// for `boss` while `holder` kept it busy, and started anyway: two of
    /// its turns may now overlap, which the person should know of. Once a
    /// busy stretch, however many wait on it: whether it was said.
    pub(super) fn mind_bound_hit(
        &self,
        boss: &str,
        what: &str,
        wait: Duration,
        holder: Option<&Holding>,
    ) -> bool {
        if let Some(h) = holder {
            let mut told = self.minds.told.lock();
            if told.get(boss) == Some(&h.since) {
                return false;
            }
            told.insert(boss.to_string(), h.since);
        }
        let mins = |d: Duration| (d.as_secs() / 60).max(1);
        let text = match holder {
            Some(h) => format!(
                "stopped waiting for {} (busy {} min) after {} min and started {what} alongside it; the two may overlap, so check what each told the crew",
                h.what,
                mins(h.since.elapsed()),
                mins(wait)
            ),
            None => format!(
                "waited {} min to start {what} and started it anyway; it may overlap another of its turns, so check what each told the crew",
                mins(wait)
            ),
        };
        let name = self
            .state
            .registry
            .get(boss)
            .map(|s| super::name_of(&s))
            .unwrap_or_else(|| boss.to_string());
        let me = workforce_desk::crew::Party::manager(boss, &name);
        self.desk.note(workforce_desk::crew::Event::new(
            workforce_desk::crew::Kind::Quiet,
            me,
            None,
            text,
        ));
        true
    }

    /// Runs `f` holding `boss`'s mind, doing `what`, waiting for it as long
    /// as [`WAIT_MAX`]; past that `f` runs without it, said. What `f`
    /// starts runs as part of it (a nested step, its turn's tools): it
    /// never waits the whole bound again behind the same busy stretch.
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
            let wait = *self.minds.wait_max.lock();
            match self.mind_held(boss, what, wait, f).await {
                Ok(out) => out,
                Err(f) => {
                    let holder = self.mind_holder(boss);
                    // Freed just as the wait ran out: taken after all.
                    let f = match holder {
                        Some(_) => f,
                        None => match self.mind_held(boss, what, Duration::ZERO, f).await {
                            Ok(out) => return out,
                            Err(f) => f,
                        },
                    };
                    let holder = holder.or_else(|| self.mind_holder(boss));
                    tracing::warn!(
                        %boss,
                        %what,
                        waited_secs = wait.as_secs(),
                        holder = ?holder.as_ref().map(|h| &h.what),
                        held_secs = ?holder.as_ref().map(|h| h.since.elapsed().as_secs()),
                        "a boss's mind stayed held too long; this goes on without it"
                    );
                    self.mind_bound_hit(boss, what, wait, holder.as_ref());
                    HELD.scope(boss.to_string(), f).await
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
            // A turn it starts runs under the mind too.
            return Ok(HELD.scope(boss.to_string(), f).await);
        }
        let lock = self.minds.lock_of(boss);
        let asked = Instant::now();
        let (guard, holding) = {
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
            let guard = match tokio::time::timeout(wait, lock.lock_owned()).await {
                Ok(guard) => guard,
                Err(_) => return Err(f),
            };
            // Marked held before it stops being waited for: never free
            // between the two.
            self.minds.holding.lock().insert(
                boss.to_string(),
                Holding {
                    what: what.to_string(),
                    since: Instant::now(),
                },
            );
            let mark = HoldingMark {
                minds: &self.minds,
                boss: boss.to_string(),
            };
            (guard, mark)
        };
        let waited = asked.elapsed();
        if waited >= WAIT_SAID {
            tracing::warn!(%boss, %what, waited_secs = waited.as_secs(), "a boss's step waited for its mind");
        }
        let out = HELD.scope(boss.to_string(), f).await;
        // The mark goes before the lock: the next holder's own mark is
        // never removed by this one.
        drop(holding);
        drop(guard);
        Ok(out)
    }

    /// The turn on runner session `sid` of `boss`'s runs under its mind,
    /// when the code starting it holds the mind: its tools are the mind's.
    pub(super) fn turn_under_mind(&self, boss: &str, sid: &str) -> Option<TurnUnder<'_>> {
        if !self.minds.held_here(boss) {
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
        let mark = self.minds.marks.fetch_add(1, Ordering::Relaxed);
        self.minds
            .person
            .lock()
            .entry(boss.to_string())
            .or_default()
            .insert(mark, Instant::now());
        PersonTurn {
            crew: self.me.clone(),
            boss: boss.to_string(),
            mark,
        }
    }

    /// Whether one of the person's own turns runs in `boss`'s DM (begun
    /// within [`PERSON_YIELD`]): crew work yields to it.
    #[cfg(test)]
    pub(super) fn person_turn_running(&self, boss: &str) -> bool {
        self.minds.person_running(boss)
    }

    /// Waits, at most [`PERSON_YIELD`], while one of the person's own turns runs
    /// in `boss`'s DM: crew work started on its own (a plan's turn, an
    /// update's or a summary's check-in) yields to them, as the loops'
    /// does. Nothing waits that already holds the mind.
    pub(super) async fn yield_to_person(&self, boss: &str) {
        if self.minds.held_here(boss) {
            return;
        }
        let since = Instant::now();
        while self.minds.person_running(boss) && since.elapsed() < PERSON_YIELD {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        // Any of their turns still running, however old: `person_running`
        // no longer counts one past the yield.
        let still = self
            .minds
            .person
            .lock()
            .get(boss)
            .is_some_and(|t| !t.is_empty());
        if still {
            tracing::warn!(
                %boss,
                waited_secs = PERSON_YIELD.as_secs(),
                "crew work yielded to the person's own turn as long as it does; it goes on beside it"
            );
        }
    }

    /// Whether crew work for `boss` may start now: nothing holds or waits
    /// for its mind, and none of the person's turns runs in its DM.
    pub(super) fn mind_free(&self, boss: &str) -> bool {
        !self.minds.holding.lock().contains_key(boss)
            && !self.minds.waiting.lock().contains_key(boss)
            && !self.minds.person_running(boss)
    }

    /// Whether work for `boss` held back since `due` may start now: its
    /// mind is free, or it has waited [`STARVE`] and queues for it (never
    /// while one of the person's own turns runs, for at most
    /// [`PERSON_YIELD`]).
    pub(super) fn mind_may_start(&self, boss: &str, due: Instant) -> bool {
        self.mind_free(boss) || (due.elapsed() >= STARVE && !self.minds.person_running(boss))
    }

    /// What `boss`'s mind is doing, while held.
    pub(super) fn mind_holder(&self, boss: &str) -> Option<Holding> {
        self.minds.holding.lock().get(boss).cloned()
    }

    /// Shortens [`WAIT_MAX`], for tests.
    #[cfg(test)]
    pub(super) fn set_mind_wait_max(&self, wait: Duration) {
        *self.minds.wait_max.lock() = wait;
    }
}
