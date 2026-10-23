//! A circuit breaker for something a search asks and can do without: a
//! vector index that accepts a question and does not answer.
//!
//! Closed, every caller asks. After `trip_after` failures in a row it
//! opens, and for `rest` nobody asks: each caller goes on without, at
//! once. When the rest is over one caller is let through to try it (half
//! open); if that succeeds it closes, and if it fails it opens for another
//! rest. One failure does not open it, so a single slow answer costs the
//! one caller that waited for it and nobody else.
//!
//! Each caller let through is given a [`Ticket`], and reports how it went
//! with it. A report counts only if nothing has changed the breaker's state
//! since that caller was let in: a slow answer to a question asked before
//! the breaker opened, or before a probe closed it, says nothing of how
//! things stand now, and is dropped. So, however many callers are in
//! flight, one failure in a row too many opens it, and one probe decides.

use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Asked by everyone; so many failures in a row so far.
    Closed { failures: u32 },
    /// Asked by nobody until then.
    Open { until: Instant },
    /// One caller is trying it, since then.
    HalfOpen { since: Instant },
}

/// Leave to ask, from one admission ([`Breaker::admit`]), reported once
/// with [`Breaker::succeeded`] or [`Breaker::failed`]. It cannot be copied
/// or reported twice:
///
/// ```compile_fail
/// use std::time::Duration;
/// use workforce_common::breaker::Breaker;
/// let breaker = Breaker::new(1, Duration::from_secs(1));
/// let ticket = breaker.admit().unwrap();
/// breaker.failed(ticket);
/// breaker.failed(ticket);
/// ```
///
/// ```compile_fail
/// use std::time::Duration;
/// use workforce_common::breaker::Breaker;
/// let breaker = Breaker::new(1, Duration::from_secs(1));
/// let ticket = breaker.admit().unwrap();
/// let again = ticket.clone();
/// ```
#[derive(Debug, PartialEq, Eq)]
#[must_use = "report how it went with `succeeded` or `failed`"]
pub struct Ticket {
    /// The breaker's generation when it was given.
    generation: u64,
    probe: bool,
}

impl Ticket {
    /// Whether this caller is the one trying it after a rest.
    pub fn is_probe(&self) -> bool {
        self.probe
    }
}

#[derive(Debug)]
struct Inner {
    state: State,
    /// Moved on at every change of state: a report from an earlier
    /// generation is of a state since left, and is dropped.
    generation: u64,
}

impl Inner {
    fn to(&mut self, state: State) {
        self.state = state;
        self.generation += 1;
    }
}

#[derive(Debug)]
pub struct Breaker {
    inner: Mutex<Inner>,
    trip_after: u32,
    rest: Duration,
}

impl Breaker {
    /// Opens after `trip_after` failures in a row (at least one), for
    /// `rest`.
    pub fn new(trip_after: u32, rest: Duration) -> Self {
        Self {
            inner: Mutex::new(Inner {
                state: State::Closed { failures: 0 },
                generation: 0,
            }),
            trip_after: trip_after.max(1),
            rest,
        }
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether the caller may ask now: a ticket to report with if so,
    /// `None` if it is being left alone.
    ///
    /// A probe whose ticket is never reported (its caller gone) holds the
    /// breaker half open for one rest: nobody asks meanwhile, and after it
    /// another probe is let through, the first one's ticket counting for
    /// nothing from then on.
    #[must_use = "report how it went with `succeeded` or `failed`"]
    pub fn admit(&self) -> Option<Ticket> {
        let mut inner = self.inner();
        let now = Instant::now();
        let probe = match inner.state {
            State::Closed { .. } => false,
            State::Open { until } if now < until => return None,
            State::HalfOpen { since } if now < since + self.rest => return None,
            State::Open { .. } | State::HalfOpen { .. } => {
                inner.to(State::HalfOpen { since: now });
                true
            }
        };
        Some(Ticket {
            generation: inner.generation,
            probe,
        })
    }

    /// It answered.
    pub fn succeeded(&self, ticket: Ticket) {
        let mut inner = self.inner();
        if ticket.generation != inner.generation {
            return;
        }
        match inner.state {
            State::Closed { .. } => inner.state = State::Closed { failures: 0 },
            State::HalfOpen { .. } => inner.to(State::Closed { failures: 0 }),
            // Nobody is let in while it is open.
            State::Open { .. } => {}
        }
    }

    /// It did not answer in time. Returns whether that opened the breaker.
    pub fn failed(&self, ticket: Ticket) -> bool {
        let mut inner = self.inner();
        if ticket.generation != inner.generation {
            return false;
        }
        let open = State::Open {
            until: Instant::now() + self.rest,
        };
        match inner.state {
            State::Closed { failures } if failures + 1 < self.trip_after => {
                inner.state = State::Closed {
                    failures: failures + 1,
                };
                false
            }
            State::Closed { .. } | State::HalfOpen { .. } => {
                inner.to(open);
                true
            }
            State::Open { .. } => false,
        }
    }

    /// `"closed"`, `"open"` or `"half_open"`, for status reports.
    pub fn told(&self) -> &'static str {
        match self.inner().state {
            State::Closed { .. } => "closed",
            State::Open { until } if Instant::now() < until => "open",
            // Its rest over, the next caller tries it.
            State::Open { .. } | State::HalfOpen { .. } => "half_open",
        }
    }

    pub fn rest(&self) -> Duration {
        self.rest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REST: Duration = Duration::from_millis(60);

    fn rested() {
        std::thread::sleep(REST + Duration::from_millis(20));
    }

    /// A ticket, which the breaker must give.
    fn admitted(breaker: &Breaker) -> Ticket {
        breaker.admit().expect("let through")
    }

    /// It takes several failures in a row to open; open, nobody asks; its
    /// rest over, one caller alone tries it, and how that goes decides.
    #[test]
    fn a_breaker_opens_after_failures_in_a_row_and_is_tried_by_one_caller_after_a_rest() {
        let breaker = Breaker::new(3, REST);
        let first = admitted(&breaker);
        assert!(!first.is_probe(), "closed, nobody probes");
        assert_eq!(breaker.told(), "closed");
        assert!(!breaker.failed(first));
        assert!(!breaker.failed(admitted(&breaker)));
        // A success between failures starts the count again.
        breaker.succeeded(admitted(&breaker));
        assert!(!breaker.failed(admitted(&breaker)));
        assert!(!breaker.failed(admitted(&breaker)));
        assert!(breaker.failed(admitted(&breaker)), "the third in a row");
        assert_eq!((breaker.admit(), breaker.told()), (None, "open"));

        rested();
        assert_eq!(breaker.told(), "half_open");
        let probe = admitted(&breaker);
        assert!(probe.is_probe(), "the one trying it is told so");
        assert_eq!(breaker.admit(), None, "one caller at a time");
        assert!(breaker.failed(probe), "the probe failed: another rest");
        assert_eq!(breaker.admit(), None);

        rested();
        let probe = admitted(&breaker);
        assert!(probe.is_probe(), "the one trying it is told so");
        breaker.succeeded(probe);
        let after = admitted(&breaker);
        assert!(!after.is_probe(), "closed, nobody probes");
        assert_eq!(breaker.told(), "closed");
        assert!(!breaker.failed(after), "and it takes three in a row again");
    }

    /// A probe whose caller went away, its ticket dropped unreported,
    /// holds the breaker half open for one rest, and no more: then another
    /// is let through.
    #[test]
    fn a_probe_never_reported_holds_the_breaker_for_one_rest() {
        let breaker = Breaker::new(1, REST);
        assert!(breaker.failed(admitted(&breaker)));
        rested();
        let gone = admitted(&breaker);
        assert!(gone.is_probe(), "the one trying it is told so");
        drop(gone);
        assert_eq!(breaker.admit(), None, "within the rest");
        rested();
        let next = admitted(&breaker);
        assert!(next.is_probe(), "the next caller tries it");
        breaker.succeeded(next);
        assert_eq!(breaker.told(), "closed");
    }

    /// A slow success of a question asked before a fast failure opened the
    /// breaker does not close it: only a probe does.
    #[test]
    fn a_success_from_before_the_breaker_opened_does_not_close_it() {
        let breaker = Breaker::new(1, REST);
        let slow = admitted(&breaker);
        let fast = admitted(&breaker);
        assert!(breaker.failed(fast));
        breaker.succeeded(slow);
        assert_eq!(breaker.told(), "open", "closed with no probe");
        assert_eq!(breaker.admit(), None);
    }

    /// A late failure of a question asked before the breaker opened does
    /// not open it again over the probe that closed it.
    #[test]
    fn a_failure_from_before_a_probe_closed_the_breaker_does_not_open_it() {
        let breaker = Breaker::new(1, REST);
        let late = admitted(&breaker);
        assert!(breaker.failed(admitted(&breaker)));
        rested();
        let probe = admitted(&breaker);
        assert!(probe.is_probe(), "the one trying it is told so");
        breaker.succeeded(probe);
        assert!(!breaker.failed(late), "opened over the probe");
        assert_eq!(breaker.told(), "closed");
    }

    /// Answers to questions asked before the breaker opened, coming in
    /// while the probe is out, decide nothing: the probe does.
    #[test]
    fn reports_from_before_the_breaker_opened_do_not_decide_for_the_probe() {
        let breaker = Breaker::new(1, REST);
        let slow = admitted(&breaker);
        let late = admitted(&breaker);
        assert!(breaker.failed(admitted(&breaker)));
        rested();
        let probe = admitted(&breaker);
        breaker.succeeded(slow);
        assert_eq!(breaker.admit(), None, "closed while the probe was out");
        assert!(!breaker.failed(late), "opened while the probe was out");
        assert_eq!(breaker.admit(), None, "still the probe's to decide");
        breaker.succeeded(probe);
        assert_eq!(breaker.told(), "closed");
    }

    /// Two probes abandoned in turn, each reporting late, decide nothing:
    /// the third, the one let through now, does.
    #[test]
    fn abandoned_probes_reporting_late_decide_nothing() {
        let breaker = Breaker::new(1, REST);
        assert!(breaker.failed(admitted(&breaker)));
        rested();
        let first = admitted(&breaker);
        rested();
        let second = admitted(&breaker);
        rested();
        let third = admitted(&breaker);
        breaker.succeeded(first);
        assert_eq!(breaker.admit(), None, "closed by the first, abandoned");
        assert!(!breaker.failed(second), "opened by the second, abandoned");
        assert_eq!(breaker.admit(), None, "still the third's to decide");
        breaker.succeeded(third);
        assert_eq!(breaker.told(), "closed");
    }

    /// A failure from before the breaker last changed state does not count
    /// toward opening it again: after a probe closes it, it takes
    /// `trip_after` failures of questions asked since.
    #[test]
    fn a_failure_from_before_the_breaker_closed_does_not_count_toward_opening_it() {
        let breaker = Breaker::new(2, REST);
        let late = admitted(&breaker);
        assert!(!breaker.failed(admitted(&breaker)));
        assert!(breaker.failed(admitted(&breaker)));
        rested();
        breaker.succeeded(admitted(&breaker));
        assert!(!breaker.failed(late));
        assert!(
            !breaker.failed(admitted(&breaker)),
            "opened on one of its own"
        );
        assert!(breaker.failed(admitted(&breaker)), "the second in a row");
    }

    /// A success from before the breaker last changed state does not break
    /// a run of failures since: closed, it does not start the count again.
    #[test]
    fn a_success_from_before_the_breaker_closed_does_not_start_the_count_again() {
        let breaker = Breaker::new(2, REST);
        let late = admitted(&breaker);
        assert!(!breaker.failed(admitted(&breaker)));
        assert!(breaker.failed(admitted(&breaker)));
        rested();
        breaker.succeeded(admitted(&breaker));
        assert!(!breaker.failed(admitted(&breaker)));
        breaker.succeeded(late);
        assert!(
            breaker.failed(admitted(&breaker)),
            "a run of two broken by a success from before"
        );
    }
}
