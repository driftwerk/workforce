//! Who is seated next on a full worker: the prepares that asked to queue,
//! each keeping its place while it asks again, ordered so nobody holds more
//! than a share of the worker while others wait.
//!
//! The queue decides only the order. Whether anyone can be seated at all is
//! the worker's capacity test, asked for the prepare at the head.

use chrono::{DateTime, Utc};
use workforce_agent::tools::exec_runner::{ScopeId, QUEUE_HOLD};

/// One prepare waiting for a seat.
#[derive(Debug, Clone)]
struct Waiter {
    scope: ScopeId,
    owner: String,
    /// When it first asked: first come, first seated, among owners alike.
    since: DateTime<Utc>,
    /// When it last asked; a place not asked for within [`QUEUE_HOLD`] goes.
    asked: DateTime<Utc>,
}

#[derive(Debug, Default)]
pub(crate) struct Queue {
    waiters: Vec<Waiter>,
}

impl Queue {
    /// Keep `scope`'s place for `owner`, joining at the back when it has
    /// none, and say how many prepares go first. Places not asked for within
    /// [`QUEUE_HOLD`] are dropped first. An owner holding `share` scopes or
    /// more (`held` says how many each holds now) goes behind every owner
    /// holding fewer; among the rest, fewer held goes first, then whoever
    /// asked first.
    pub(crate) fn place(
        &mut self,
        scope: &ScopeId,
        owner: &str,
        now: DateTime<Utc>,
        held: impl Fn(&str) -> u32,
        share: u32,
    ) -> u32 {
        let hold = chrono::Duration::from_std(QUEUE_HOLD).unwrap_or_default();
        self.waiters
            .retain(|waiter| waiter.asked + hold > now || waiter.scope == *scope);
        match self.waiters.iter_mut().find(|w| w.scope == *scope) {
            Some(waiter) => {
                waiter.asked = now;
                waiter.owner = owner.to_string();
            }
            None => self.waiters.push(Waiter {
                scope: scope.clone(),
                owner: owner.to_string(),
                since: now,
                asked: now,
            }),
        }
        let order = |waiter: &Waiter| {
            let holds = held(&waiter.owner);
            (holds >= share, holds, waiter.since, waiter.scope.clone())
        };
        let mine = self
            .waiters
            .iter()
            .find(|w| w.scope == *scope)
            .map(order)
            .expect("placed above");
        self.waiters.iter().filter(|w| order(w) < mine).count() as u32
    }

    /// `scope` is seated, or no longer waiting: it gives up its place.
    pub(crate) fn leave(&mut self, scope: &ScopeId) {
        self.waiters.retain(|waiter| waiter.scope != *scope);
    }

    /// How many prepares hold a place.
    pub(crate) fn len(&self) -> usize {
        self.waiters.len()
    }
}

/// The most scopes one owner may hold while others wait: `fair_share` of
/// `max_scopes`, and at least one.
pub(crate) fn share_of(fair_share: f64, max_scopes: u32) -> u32 {
    ((fair_share * f64::from(max_scopes)).floor() as u32).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(scope: &str) -> ScopeId {
        ScopeId::new(scope)
    }

    #[test]
    fn first_come_is_first_seated_and_a_place_is_kept_while_asked_for() {
        let mut queue = Queue::default();
        let now = Utc::now();
        let none = |_: &str| 0;
        assert_eq!(queue.place(&id("a"), "person-a", now, none, 1), 0);
        assert_eq!(queue.place(&id("b"), "person-b", now, none, 1), 1);
        let later = now + chrono::Duration::seconds(5);
        assert_eq!(queue.place(&id("c"), "person-c", later, none, 1), 2);
        assert_eq!(
            queue.place(&id("b"), "person-b", later, none, 1),
            1,
            "asking again keeps the place, it does not join at the back"
        );
        queue.leave(&id("a"));
        assert_eq!(queue.place(&id("c"), "person-c", later, none, 1), 1);
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn a_place_not_asked_for_within_the_hold_is_dropped() {
        let mut queue = Queue::default();
        let now = Utc::now();
        let none = |_: &str| 0;
        queue.place(&id("gone"), "person-a", now, none, 1);
        let later = now + chrono::Duration::from_std(QUEUE_HOLD).unwrap();
        assert_eq!(
            queue.place(&id("b"), "person-b", later, none, 1),
            0,
            "the prepare that stopped asking holds nobody up"
        );
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn an_owner_at_its_share_waits_behind_owners_holding_less() {
        let mut queue = Queue::default();
        let now = Utc::now();
        let held = |owner: &str| match owner {
            "person-busy" => 2,
            "person-some" => 1,
            _ => 0,
        };
        assert_eq!(queue.place(&id("busy"), "person-busy", now, held, 2), 0);
        let later = now + chrono::Duration::seconds(1);
        assert_eq!(queue.place(&id("some"), "person-some", later, held, 2), 0);
        let latest = later + chrono::Duration::seconds(1);
        assert_eq!(
            queue.place(&id("none"), "person-none", latest, held, 2),
            0,
            "holding nothing goes before holding one, whoever asked first"
        );
        assert_eq!(queue.place(&id("some"), "person-some", latest, held, 2), 1);
        assert_eq!(
            queue.place(&id("busy"), "person-busy", latest, held, 2),
            2,
            "the owner at its share goes last though it asked first"
        );
    }

    #[test]
    fn a_share_is_a_fraction_of_the_worker_and_never_none() {
        assert_eq!(share_of(0.25, 14), 3);
        assert_eq!(share_of(0.25, 2), 1);
        assert_eq!(share_of(1.0, 14), 14);
    }
}
