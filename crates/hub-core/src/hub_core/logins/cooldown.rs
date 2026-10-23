//! Logins skipped: at their usage limit until their reset, or refused for
//! good until their slot holds another sign-in.
//!
//! In memory and process-wide: a replica or a deploy that has not seen the
//! limit or the refusal spends at most one refused call finding it out, and
//! correctness never depends on the store, because the provider still reports
//! both.

use std::collections::HashMap;
use std::sync::LazyLock;

use chrono::{DateTime, TimeDelta, Utc};
use parking_lot::RwLock;

use super::resolve::{LoginKey, VaultSlot};

/// How long a login cools when the limit came without a reset time.
const UNKNOWN_RESET_COOLDOWN: TimeDelta = TimeDelta::minutes(15);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LimitStatus {
    /// The call was refused for the limit.
    Rejected,
    /// Still served, close to the limit. `utilization` is a fraction (0–1).
    Warning { utilization: Option<f64> },
}

/// A usage-limit report from a provider or CLI.
#[derive(Debug, Clone, PartialEq)]
pub struct LimitSignal {
    pub status: LimitStatus,
    pub resets_at: Option<DateTime<Utc>>,
    /// The limit's window as the provider names it (`five_hour`, `seven_day`,
    /// `primary`, …).
    pub window: Option<String>,
}

#[derive(Default)]
pub struct CooldownStore {
    cooling: RwLock<HashMap<LoginKey, DateTime<Utc>>>,
    /// The value each slot held when it was refused for good.
    dead: RwLock<HashMap<String, String>>,
}

impl CooldownStore {
    pub fn global() -> &'static Self {
        static GLOBAL: LazyLock<CooldownStore> = LazyLock::new(CooldownStore::default);
        &GLOBAL
    }

    /// Cool `key` until the signal's reset (or a default window when it
    /// names none). Only a refusal cools; a warning is served as usual.
    pub fn note(&self, key: &LoginKey, signal: &LimitSignal, now: DateTime<Utc>) {
        if signal.status != LimitStatus::Rejected {
            return;
        }
        let until = signal
            .resets_at
            .filter(|reset| *reset > now)
            .unwrap_or(now + UNKNOWN_RESET_COOLDOWN);
        let mut cooling = self.cooling.write();
        let entry = cooling.entry(key.clone()).or_insert(until);
        *entry = (*entry).max(until);
        tracing::warn!(
            kind = key.kind.label(),
            fingerprint = %key.fingerprint,
            window = signal.window.as_deref().unwrap_or("unknown"),
            %until,
            "subscription login at its usage limit"
        );
    }

    /// When `key` stops cooling, if it is cooling at `now`.
    pub fn until(&self, key: &LoginKey, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let until = self.cooling.read().get(key).copied()?;
        if until > now {
            return Some(until);
        }
        self.cooling.write().retain(|_, until| *until > now);
        None
    }

    /// Skip `slot` while it holds the value fingerprinted `value`: the
    /// provider refused that login for good.
    pub fn note_dead(&self, slot: &VaultSlot, value: &str) {
        tracing::warn!(slot = %slot.key, "subscription login refused; skipped until it is signed in again");
        self.dead.write().insert(slot.location(), value.to_string());
    }

    /// Whether `slot` still holds the value it was refused with. A new value
    /// clears the mark.
    pub fn is_dead(&self, slot: &VaultSlot, value: &str) -> bool {
        let location = slot.location();
        let marked = self.dead.read().get(&location).cloned();
        match marked {
            Some(dead) if dead == value => true,
            Some(_) => {
                self.dead.write().remove(&location);
                false
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::subscription::SubscriptionKind;

    fn key(fingerprint: &str) -> LoginKey {
        LoginKey {
            kind: SubscriptionKind::ChatGpt,
            fingerprint: fingerprint.into(),
        }
    }

    fn rejected(resets_at: Option<DateTime<Utc>>) -> LimitSignal {
        LimitSignal {
            status: LimitStatus::Rejected,
            resets_at,
            window: None,
        }
    }

    #[test]
    fn rejected_signal_cools_until_its_reset_then_expires() {
        let store = CooldownStore::default();
        let now = Utc::now();
        let reset = now + TimeDelta::hours(3);
        store.note(&key("a"), &rejected(Some(reset)), now);
        assert_eq!(store.until(&key("a"), now), Some(reset));
        assert_eq!(store.until(&key("b"), now), None);
        assert_eq!(store.until(&key("a"), reset), None);
        assert!(
            store.cooling.read().is_empty(),
            "expired entries are pruned"
        );
    }

    #[test]
    fn unknown_reset_cools_for_default_window() {
        let store = CooldownStore::default();
        let now = Utc::now();
        store.note(&key("a"), &rejected(None), now);
        assert_eq!(
            store.until(&key("a"), now),
            Some(now + UNKNOWN_RESET_COOLDOWN)
        );
        store.note(&key("b"), &rejected(Some(now - TimeDelta::minutes(1))), now);
        assert_eq!(
            store.until(&key("b"), now),
            Some(now + UNKNOWN_RESET_COOLDOWN),
            "a reset already past is no reset"
        );
    }

    #[test]
    fn a_warning_does_not_cool_and_a_shorter_reset_does_not_shorten() {
        let store = CooldownStore::default();
        let now = Utc::now();
        store.note(
            &key("a"),
            &LimitSignal {
                status: LimitStatus::Warning {
                    utilization: Some(0.95),
                },
                resets_at: Some(now + TimeDelta::hours(1)),
                window: None,
            },
            now,
        );
        assert_eq!(store.until(&key("a"), now), None);

        let weekly = now + TimeDelta::days(3);
        store.note(&key("a"), &rejected(Some(weekly)), now);
        store.note(&key("a"), &rejected(Some(now + TimeDelta::hours(1))), now);
        assert_eq!(store.until(&key("a"), now), Some(weekly));
    }
}
