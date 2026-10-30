//! Plan readings: what the hub's logins' calls and this Mac's CLIs report of
//! their plans, kept as each login's latest.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use parking_lot::Mutex;
use serde_json::Value;
use workforce_common::channels::desk::DeskSignal;
use workforce_common::subscription::{CredentialTier, LoginOwner, SubscriptionKind};
use workforce_common::usage::{
    account_key, claude_mod_usage, codex_usage, LoginRef, PlanKind, Reading, Window,
};
use workforce_desk::usage::{MacAccount, MacPlan, PlanSink};

use super::postgres::PgFloorStore;
use super::signal::Signals;
use crate::hub_core::logins::plan_usage::PlanUsage;
use crate::hub_core::logins::resolve::{Credential, Login};

/// How long a login's unchanged reading goes unwritten.
const REWRITE_EVERY: Duration = Duration::from_secs(60);
/// How far a window must move, in points, to be written at once.
const MOVED: f64 = 1.0;

/// Keeps readings in the floor's store, each login's no more often than it
/// says something new, telling whose plan it is when one is kept.
pub struct Plans {
    store: Arc<PgFloorStore>,
    signals: Signals,
    written: Mutex<HashMap<String, (Vec<Window>, Instant)>>,
}

/// Whether `windows` say something the reading last written didn't: a
/// window moved a point, a reset moved, a window came or went, or a
/// minute passed.
fn worth_writing(last: Option<&(Vec<Window>, Instant)>, windows: &[Window]) -> bool {
    let Some((before, at)) = last else {
        return true;
    };
    at.elapsed() >= REWRITE_EVERY
        || before.len() != windows.len()
        || before.iter().zip(windows).any(|(b, w)| {
            b.minutes != w.minutes
                || b.resets_at != w.resets_at
                || (b.used_percent - w.used_percent).abs() >= MOVED
        })
}

impl Plans {
    pub fn new(store: Arc<PgFloorStore>, signals: Signals) -> Arc<Self> {
        Arc::new(Self {
            store,
            signals,
            written: Mutex::new(HashMap::new()),
        })
    }

    fn keep(&self, reading: Reading) {
        let key = super::postgres::login_key(&reading);
        {
            let mut written = self.written.lock();
            if !worth_writing(written.get(&key), &reading.windows) {
                return;
            }
            written.insert(key, (reading.windows.clone(), Instant::now()));
        }
        let store = Arc::clone(&self.store);
        let signals = self.signals.clone();
        tokio::spawn(async move {
            match store.record(&reading).await {
                Ok(true) => signals.send(DeskSignal::Usage, reading.login.owner()),
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(target: "floor", error = %format!("{e:#}"), "keeping a plan reading failed")
                }
            }
        });
    }
}

fn plan_kind(kind: SubscriptionKind) -> PlanKind {
    match kind {
        SubscriptionKind::Claude => PlanKind::Claude,
        SubscriptionKind::ChatGpt => PlanKind::Chatgpt,
    }
}

/// The person a login is per-user for: none for a login an agent or the
/// hub shares.
fn per_user(login: &Login) -> Option<String> {
    match (login.tier, &login.owner) {
        (CredentialTier::AgentUser | CredentialTier::GlobalUser, LoginOwner::Person(p)) => {
            Some(p.to_string())
        }
        _ => None,
    }
}

impl PlanUsage for Plans {
    fn observe(&self, login: &Login, plan: Option<&str>, windows: Vec<Window>) {
        let kind = plan_kind(login.kind);
        let account = match &login.credential {
            Credential::ChatGpt(auth) => auth.account_id().and_then(|id| account_key(kind, &[id])),
            Credential::Token(_) => None,
        };
        let Some(key) = account_key(kind, std::slice::from_ref(&login.key.fingerprint)) else {
            return;
        };
        self.keep(Reading {
            kind,
            login: LoginRef::Hub {
                login: key,
                owner: per_user(login),
            },
            account,
            plan: plan.map(str::to_string),
            windows,
            observed_at: Utc::now(),
        });
    }
}

impl PlanSink for Plans {
    fn mac(&self, plan: MacPlan, payload: Value, account: MacAccount) {
        let (kind, windows) = match plan {
            MacPlan::Claude => (PlanKind::Claude, claude_mod_usage(&payload)),
            MacPlan::Codex => (PlanKind::Chatgpt, codex_usage(&payload)),
        };
        if windows.is_empty() {
            return;
        }
        self.keep(Reading {
            kind,
            login: LoginRef::Mac,
            account: account_key(kind, &account.ids),
            plan: account.plan,
            windows,
            observed_at: Utc::now(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(used: f64, reset: i64) -> Window {
        Window {
            used_percent: used,
            resets_at: chrono::DateTime::from_timestamp(reset, 0),
            minutes: workforce_common::usage::FIVE_HOUR,
        }
    }

    #[test]
    fn a_reading_is_written_when_it_says_something_new() {
        let last = (vec![window(40.0, 100)], Instant::now());
        assert!(worth_writing(None, &[window(40.0, 100)]));
        assert!(!worth_writing(Some(&last), &[window(40.6, 100)]));
        assert!(worth_writing(Some(&last), &[window(41.0, 100)]), "a point");
        assert!(
            worth_writing(Some(&last), &[window(40.0, 200)]),
            "a new reset"
        );
        assert!(worth_writing(Some(&last), &[]), "a window gone");
        let old = (
            vec![window(40.0, 100)],
            Instant::now().checked_sub(REWRITE_EVERY).unwrap(),
        );
        assert!(
            worth_writing(Some(&old), &[window(40.0, 100)]),
            "a minute on"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_logins_reading_is_kept_as_whose_it_is_and_this_macs_as_its_account() {
        use crate::hub_core::logins::test_support::claude_login;
        use workforce_postgres::test_support::with_test_pool;
        with_test_pool(PgFloorStore::build_pool, |pool| async move {
            let store = Arc::new(PgFloorStore::with_pool(pool).await.unwrap());
            let mut pushed = crate::session_push::subscribe_for_tests();
            let plans = Plans::new(Arc::clone(&store), crate::floor::signal::Signals::start());
            let person = uuid::Uuid::new_v4();
            let theirs = Login {
                tier: CredentialTier::GlobalUser,
                owner: LoginOwner::Person(person),
                ..claude_login("their-token")
            };
            plans.observe(&theirs, None, vec![window(62.0, 100)]);
            plans.observe(&claude_login("shared-token"), None, vec![window(90.0, 100)]);
            plans.observe(&theirs, None, vec![]);
            plans.mac(
                MacPlan::Codex,
                serde_json::json!({"rateLimits": {"primary": {"usedPercent": 12, "windowDurationMins": 300}}}),
                MacAccount {
                    ids: vec!["acct-1".into()],
                    plan: Some("pro".into()),
                },
            );
            plans.mac(MacPlan::Claude, serde_json::json!([]), MacAccount::default());
            let kept = loop {
                let kept = store.readings().await.unwrap();
                if kept.len() == 3 {
                    break kept;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            };
            let owners: Vec<_> = kept
                .iter()
                .map(|r| match &r.login {
                    LoginRef::Hub { login, owner } => {
                        assert_eq!(login.len(), 16, "a login is kept by its key, never its token");
                        (r.kind, owner.clone())
                    }
                    LoginRef::Mac => (r.kind, Some("mac".into())),
                })
                .collect();
            assert!(owners.contains(&(PlanKind::Claude, Some(person.to_string()))));
            assert!(owners.contains(&(PlanKind::Claude, None)), "a shared login is no one's");
            let told = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let frame = pushed.recv().await.unwrap();
                    if frame.msg_type == "usage_changed"
                        && frame.target_identity.as_deref() == Some(person.to_string().as_str())
                    {
                        break frame;
                    }
                }
            })
            .await
            .expect("the person is told their plan's usage changed");
            assert!(told.for_links_only(), "the person whose plan it is is told");
            let mac = kept.iter().find(|r| r.login == LoginRef::Mac).unwrap();
            assert_eq!(
                (mac.kind, mac.plan.as_deref(), mac.account.clone()),
                (
                    PlanKind::Chatgpt,
                    Some("pro"),
                    account_key(PlanKind::Chatgpt, &["acct-1".into()])
                )
            );
        })
        .await;
    }
}
