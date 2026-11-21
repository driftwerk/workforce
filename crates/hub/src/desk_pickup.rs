//! Calls one hub queues for Desk's native side to carry, and the answers it
//! waits for: the crew relay's to the person's machines, and a machine's
//! sessions' Linear and Jira calls to the Mac. Desk takes queued calls with
//! a long poll ([`Pickup::take`]), each once, and posts each answer once
//! ([`Pickup::answer`]). While Desk hasn't asked lately nothing is queued,
//! so a caller hears at once that nothing will carry it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use uuid::Uuid;

/// A call with the id its answer comes back by.
pub(crate) trait Ticketed {
    fn ticket(&self) -> Uuid;
}

/// How much a pickup holds, and how long Desk's poll waits.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    /// The most calls waiting to be taken at once.
    pub most_queued: usize,
    /// The most calls waiting for their answers at once, taken or not.
    pub most_waiting: usize,
    /// The most calls one poll takes.
    pub most_taken: usize,
    /// How long a poll waits for a call when none is queued: Desk is said
    /// to carry calls while it asked within twice this.
    pub poll_for: Duration,
}

/// Why no answer came back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unsent {
    /// Desk isn't taking calls now, or too many wait already.
    Unqueued,
    /// Queued, and never taken in time: nothing of it was done.
    NotTaken,
    /// Desk took it: it may have been done.
    Taken,
}

struct Held<C, A> {
    queued: VecDeque<(C, Instant)>,
    waiting: HashMap<Uuid, tokio::sync::oneshot::Sender<A>>,
    taken: HashSet<Uuid>,
}

impl<C, A> Default for Held<C, A> {
    fn default() -> Self {
        Self {
            queued: VecDeque::new(),
            waiting: HashMap::new(),
            taken: HashSet::new(),
        }
    }
}

pub(crate) struct Pickup<C, A> {
    held: Mutex<Held<C, A>>,
    queued: tokio::sync::Notify,
    polled: Mutex<Option<Instant>>,
    limits: Limits,
}

impl<C: Ticketed, A> Pickup<C, A> {
    pub(crate) fn new(limits: Limits) -> Self {
        Self {
            held: Mutex::new(Held::default()),
            queued: tokio::sync::Notify::new(),
            polled: Mutex::new(None),
            limits,
        }
    }

    /// Whether Desk carries calls now: it asked for them lately.
    pub(crate) fn polling(&self) -> bool {
        self.polled
            .lock()
            .is_some_and(|at| at.elapsed() < self.limits.poll_for * 2)
    }

    /// Whether Desk ever asked for calls.
    #[cfg(test)]
    pub(crate) fn polled_ever(&self) -> bool {
        self.polled.lock().is_some()
    }

    /// `call` queued for Desk, and its answer waited for up to `wait`.
    pub(crate) async fn send(&self, call: C, wait: Duration) -> Result<A, Unsent> {
        if !self.polling() {
            return Err(Unsent::Unqueued);
        }
        let id = call.ticket();
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut h = self.held.lock();
            h.queued.retain(|(_, at)| at.elapsed() < wait);
            if h.queued.len() >= self.limits.most_queued
                || h.waiting.len() >= self.limits.most_waiting
            {
                return Err(Unsent::Unqueued);
            }
            h.queued.push_back((call, Instant::now()));
            h.waiting.insert(id, tx);
        }
        self.queued.notify_waiters();
        let got = tokio::time::timeout(wait, rx).await;
        // Whatever came of it, nothing waits for it any more, and a call
        // no one took goes.
        let taken = {
            let mut h = self.held.lock();
            h.waiting.remove(&id);
            h.queued.retain(|(c, _)| c.ticket() != id);
            h.taken.remove(&id)
        };
        match got {
            Ok(Ok(answer)) => Ok(answer),
            _ if taken => Err(Unsent::Taken),
            _ => Err(Unsent::NotTaken),
        }
    }

    /// The queued calls, up to the limit, each taken once; waits up to
    /// `wait` for one when none is queued.
    pub(crate) async fn take(&self, wait: Duration) -> Vec<C> {
        *self.polled.lock() = Some(Instant::now());
        let take = || {
            let mut h = self.held.lock();
            let mut out = Vec::new();
            while out.len() < self.limits.most_taken {
                match h.queued.pop_front() {
                    Some((c, _)) if h.waiting.contains_key(&c.ticket()) => {
                        h.taken.insert(c.ticket());
                        out.push(c);
                    }
                    Some(_) => {}
                    None => break,
                }
            }
            out
        };
        let notified = self.queued.notified();
        let now = take();
        if !now.is_empty() {
            return now;
        }
        let _ = tokio::time::timeout(wait, notified).await;
        *self.polled.lock() = Some(Instant::now());
        take()
    }

    /// The answer to call `id`, taken once by the call that waits for it;
    /// `false` when none waits (late, or answered already).
    pub(crate) fn answer(&self, id: Uuid, answer: A) -> bool {
        match self.held.lock().waiting.remove(&id) {
            Some(tx) => tx.send(answer).is_ok(),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Clone, Debug, PartialEq)]
    struct Call(Uuid);

    impl Ticketed for Call {
        fn ticket(&self) -> Uuid {
            self.0
        }
    }

    fn pickup(most_queued: usize) -> Arc<Pickup<Call, &'static str>> {
        Arc::new(Pickup::new(Limits {
            most_queued,
            most_waiting: 8,
            most_taken: 2,
            poll_for: Duration::from_millis(200),
        }))
    }

    #[tokio::test]
    async fn nothing_is_queued_while_no_one_takes_calls() {
        let p = pickup(4);
        assert!(!p.polling() && !p.polled_ever());
        let sent = p.send(Call(Uuid::new_v4()), Duration::from_secs(5)).await;
        assert_eq!(sent, Err(Unsent::Unqueued));
    }

    #[tokio::test]
    async fn a_call_is_taken_once_and_answered_once() {
        let p = pickup(4);
        assert!(p.take(Duration::from_millis(1)).await.is_empty());
        assert!(p.polling());
        let id = Uuid::new_v4();
        let sending = tokio::spawn({
            let p = Arc::clone(&p);
            async move { p.send(Call(id), Duration::from_secs(5)).await }
        });
        let taken = p.take(Duration::from_secs(2)).await;
        assert_eq!(taken, vec![Call(id)]);
        assert!(p.take(Duration::from_millis(1)).await.is_empty());
        assert!(p.answer(id, "done"));
        assert!(!p.answer(id, "again"));
        assert_eq!(sending.await.unwrap(), Ok("done"));
    }

    #[tokio::test]
    async fn a_call_taken_and_unanswered_may_have_been_done_and_one_untaken_wasnt() {
        let p = pickup(4);
        p.take(Duration::from_millis(1)).await;
        let id = Uuid::new_v4();
        let sending = tokio::spawn({
            let p = Arc::clone(&p);
            async move { p.send(Call(id), Duration::from_millis(150)).await }
        });
        assert_eq!(p.take(Duration::from_secs(2)).await.len(), 1);
        assert_eq!(sending.await.unwrap(), Err(Unsent::Taken));
        assert!(!p.answer(id, "late"));
        let untaken = p
            .send(Call(Uuid::new_v4()), Duration::from_millis(50))
            .await;
        assert_eq!(untaken, Err(Unsent::NotTaken));
    }

    #[tokio::test]
    async fn a_full_pickup_queues_no_more() {
        let p = pickup(1);
        p.take(Duration::from_millis(1)).await;
        let first = tokio::spawn({
            let p = Arc::clone(&p);
            async move {
                p.send(Call(Uuid::new_v4()), Duration::from_millis(300))
                    .await
            }
        });
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let second = p
            .send(Call(Uuid::new_v4()), Duration::from_millis(300))
            .await;
        assert_eq!(second, Err(Unsent::Unqueued));
        assert_eq!(first.await.unwrap(), Err(Unsent::NotTaken));
    }
}
