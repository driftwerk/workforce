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
use workforce_desk::usage::{LocalAccount, LocalPlan, PlanSink};

use super::postgres::PgFloorStore;
use super::signal::Signals;
use crate::hub_core::logins::plan_usage::PlanUsage;
use crate::hub_core::logins::resolve::{Credential, Login};

/// How long a login's unchanged reading goes unwritten.
const REWRITE_EVERY: Duration = Duration::from_secs(60);
/// How far a window must move, in points, to be written at once.
const MOVED: f64 = 1.0;

/// Keeps readings in the floor's store, each login's no more often than it
/// says something new, telling whose plan it is when one is kept. Every
/// write goes through one writer, in the order it was asked for: a reading
/// forgotten is never one kept after it.
pub struct Plans {
    written: Mutex<HashMap<String, (Vec<Window>, Instant)>>,
    writes: tokio::sync::mpsc::UnboundedSender<Write>,
}

/// One write to the floor's plan readings.
enum Write {
    Keep(Reading),
    /// Forget the login's reading taken in `taken` (if it is still the one
    /// kept), then keep `instead`.
    Forget {
        login: String,
        taken: Option<(chrono::DateTime<Utc>, chrono::DateTime<Utc>)>,
        instead: Option<Reading>,
    },
}

/// How long one write may wait on the store: one stuck never holds up the
/// rest for long.
const STORE_WAIT: Duration = Duration::from_secs(10);

/// How often forgetting a reading is tried before it is given up on.
const FORGET_TRIES: u32 = 4;

/// Carries out `writes` one after the other.
async fn write(
    store: Arc<PgFloorStore>,
    signals: Signals,
    mut writes: tokio::sync::mpsc::UnboundedReceiver<Write>,
) {
    let kept = |r: Reading| {
        let (store, signals) = (Arc::clone(&store), signals.clone());
        async move {
            let kept = tokio::time::timeout(STORE_WAIT, store.record(&r))
                .await
                .unwrap_or_else(|_| Err(eyre::eyre!("the store took too long")));
            match kept {
                Ok(true) => {
                    signals.send(DeskSignal::Usage, r.login.owner());
                    true
                }
                Ok(false) => false,
                Err(e) => {
                    tracing::warn!(target: "floor", error = %format!("{e:#}"), "keeping a plan reading failed");
                    false
                }
            }
        }
    };
    while let Some(w) = writes.recv().await {
        match w {
            Write::Keep(r) => {
                kept(r).await;
            }
            Write::Forget {
                login,
                taken,
                instead,
            } => {
                let mut forgot = false;
                if let Some((since, last)) = taken {
                    let mut wait = Duration::from_millis(500);
                    for attempt in 1..=FORGET_TRIES {
                        let tried = tokio::time::timeout(
                            STORE_WAIT,
                            store.forget_reading(&login, since, last),
                        )
                        .await
                        .unwrap_or_else(|_| Err(eyre::eyre!("the store took too long")));
                        match tried {
                            Ok(gone) => {
                                forgot = gone;
                                break;
                            }
                            Err(e) if attempt < FORGET_TRIES => {
                                tracing::warn!(target: "floor", error = %format!("{e:#}"), attempt, "forgetting a plan reading failed: trying again");
                                tokio::time::sleep(wait).await;
                                wait *= 2;
                            }
                            Err(e) => {
                                tracing::warn!(target: "floor", error = %format!("{e:#}"), "forgetting a plan reading failed: given up");
                            }
                        }
                    }
                }
                let kept = match instead {
                    Some(r) => kept(r).await,
                    None => false,
                };
                if forgot && !kept {
                    signals.send(DeskSignal::Usage, None);
                }
            }
        }
    }
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
        let (writes, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(write(store, signals, rx));
        Arc::new(Self {
            written: Mutex::new(HashMap::new()),
            writes,
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
        let _ = self.writes.send(Write::Keep(reading));
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
        // Whose account it is, where that can be told: a ChatGPT sign-in
        // says; one of the person's own Claude accounts (a pooled call) is
        // known by the key Desk keeps its readings under, so it is that
        // account's card. A Claude token says nothing of its account.
        let account = match &login.credential {
            Credential::ChatGpt(auth) => auth.account_id().and_then(|id| account_key(kind, &[id])),
            Credential::Pooled(id) => crate::hub_core::logins::install_pool::of(&login.slot.vault)
                .and_then(|pool| pool.plan_key(id)),
            Credential::Token(_) => None,
        };
        // A login is its credential in its slot: one credential two people
        // keep, a shared Team account's say, is two plans' readings.
        let owner = per_user(login);
        let Some(key) = account_key(
            kind,
            &[
                login.key.fingerprint.clone(),
                owner.clone().unwrap_or_default(),
            ],
        ) else {
            return;
        };
        self.keep(Reading {
            kind,
            login: LoginRef::Hub { login: key, owner },
            account,
            plan: plan.map(str::to_string),
            windows,
            observed_at: Utc::now(),
        });
    }
}

impl PlanSink for Plans {
    fn windows(&self, plan: LocalPlan, payload: &Value) -> Vec<workforce_desk::usage::LimitWindow> {
        let windows = match plan {
            LocalPlan::Claude => claude_mod_usage(payload),
            LocalPlan::Codex => codex_usage(payload),
        };
        windows
            .into_iter()
            .map(|w| workforce_desk::usage::LimitWindow {
                used_percent: w.used_percent,
                resets_at: w.resets_at,
                minutes: w.minutes,
            })
            .collect()
    }

    fn account_key(&self, plan: LocalPlan, ids: &[String]) -> Option<String> {
        account_key(
            match plan {
                LocalPlan::Claude => PlanKind::Claude,
                LocalPlan::Codex => PlanKind::Chatgpt,
            },
            ids,
        )
    }

    fn local(&self, plan: LocalPlan, payload: Value, account: LocalAccount) {
        self.local_at(plan, payload, account, Utc::now());
    }

    fn local_at(
        &self,
        plan: LocalPlan,
        payload: Value,
        account: LocalAccount,
        at: chrono::DateTime<Utc>,
    ) {
        let (kind, windows) = match plan {
            LocalPlan::Claude => (PlanKind::Claude, claude_mod_usage(&payload)),
            LocalPlan::Codex => (PlanKind::Chatgpt, codex_usage(&payload)),
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
            observed_at: at,
        });
    }

    fn forget_local(
        &self,
        plan: LocalPlan,
        account: LocalAccount,
        taken: Option<(chrono::DateTime<Utc>, chrono::DateTime<Utc>)>,
        instead: Option<(Value, chrono::DateTime<Utc>)>,
    ) {
        let (kind, windows): (_, fn(&Value) -> Vec<Window>) = match plan {
            LocalPlan::Claude => (PlanKind::Claude, claude_mod_usage),
            LocalPlan::Codex => (PlanKind::Chatgpt, codex_usage),
        };
        let Some(key) = account_key(kind, &account.ids) else {
            return;
        };
        let reading = |windows: Vec<Window>, observed_at| Reading {
            kind,
            login: LoginRef::Mac,
            account: Some(key.clone()),
            plan: account.plan.clone(),
            windows,
            observed_at,
        };
        let instead = instead
            .map(|(payload, at)| reading(windows(&payload), at))
            .filter(|r| !r.windows.is_empty());
        let login = super::postgres::login_key(&reading(Vec::new(), Utc::now()));
        // Written again whatever was last: the one kept may be gone.
        self.written.lock().remove(&login);
        let _ = self.writes.send(Write::Forget {
            login,
            taken,
            instead,
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
    async fn a_reading_another_sign_in_made_is_forgotten_in_order_and_only_while_it_is_the_one_kept(
    ) {
        use workforce_postgres::test_support::with_test_pool;
        with_test_pool(PgFloorStore::build_pool, |pool| async move {
            let store = Arc::new(PgFloorStore::with_pool(pool).await.unwrap());
            let plans = Plans::new(Arc::clone(&store), crate::floor::signal::Signals::start());
            let account = || LocalAccount {
                ids: vec!["desk-account:star".into()],
                plan: None,
            };
            // To the second, as the store gives times back.
            let now = chrono::DateTime::from_timestamp(Utc::now().timestamp(), 0).unwrap();
            let at = |mins: i64| now - chrono::Duration::minutes(mins);
            let resets = chrono::DateTime::from_timestamp(now.timestamp() + 86_400, 0).unwrap();
            let weekly = |used: u32, resets: &str| {
                serde_json::json!([{"kind": "seven_day", "percentUsed": used, "resetsAt": resets}])
            };
            let full = || Some((weekly(100, &resets.to_rfc3339()), at(11)));
            let (p, st) = (&plans, &store);
            let settled = |used: u32| async move {
                // Every write before this one is done: the writer is in order.
                let marker = LocalAccount {
                    ids: vec!["marker".into()],
                    plan: None,
                };
                // Its own time, later each call: kept over the last one.
                let n = now + chrono::Duration::seconds(i64::from(used));
                p.local_at(LocalPlan::Claude, weekly(used, "2026-11-13T19:00:00Z"), marker, n);
                loop {
                    let kept = st.readings().await.unwrap();
                    if kept.iter().any(|r| r.account == account_key(PlanKind::Claude, &["marker".into()]) && r.observed_at == n) {
                        return kept
                            .into_iter()
                            .filter(|r| r.account == account_key(PlanKind::Claude, &["desk-account:star".into()]))
                            .collect::<Vec<_>>();
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            };
            // Another sign-in's plan, filed as this account's, and forgotten
            // at once: in order, never before it was kept.
            plans.local_at(LocalPlan::Claude, weekly(26, "2026-11-13T19:00:00Z"), account(), at(5));
            plans.forget_local(LocalPlan::Claude, account(), Some((at(10), at(5))), full());
            let kept = settled(1).await;
            assert_eq!(kept.len(), 1);
            assert_eq!(
                (kept[0].windows[0].used_percent, kept[0].windows[0].resets_at, kept[0].observed_at),
                (100.0, Some(resets), at(11)),
                "the account's own limit, as it stopped on it"
            );
            // Reported again unchanged ten seconds on: not written again,
            // so the one kept is older than the last report, and still
            // forgotten.
            plans.local_at(LocalPlan::Claude, weekly(26, "2026-11-13T19:00:00Z"), account(), at(4));
            let steady = at(4) + chrono::Duration::seconds(10);
            plans.local_at(LocalPlan::Claude, weekly(26, "2026-11-13T19:00:00Z"), account(), steady);
            let kept = settled(5).await;
            assert_eq!(kept[0].observed_at, at(4), "the second was never written");
            plans.forget_local(LocalPlan::Claude, account(), Some((at(10), steady)), full());
            let kept = settled(6).await;
            assert_eq!(
                (kept[0].windows[0].used_percent, kept[0].observed_at),
                (100.0, at(11)),
                "forgotten though its time is before the last report's"
            );
            // A later reading, someone else's, stands: only the one taken
            // then is forgotten, and the limit is never kept over it.
            plans.local_at(LocalPlan::Claude, weekly(40, "2026-11-13T19:00:00Z"), account(), at(2));
            plans.forget_local(LocalPlan::Claude, account(), Some((at(10), at(5))), full());
            let kept = settled(9).await;
            assert_eq!(
                (kept[0].windows[0].used_percent, kept[0].observed_at),
                (40.0, at(2))
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_call_on_one_of_the_persons_own_accounts_is_that_accounts_reading() {
        use crate::hub_core::logins::install_pool::{self, InstallPool, PoolAccount};
        use crate::hub_core::logins::resolve::{Credential, LoginKey, VaultSlot};
        use workforce_postgres::test_support::with_test_pool;
        struct Pool;
        impl InstallPool for Pool {
            fn claude_accounts(&self) -> Vec<PoolAccount> {
                Vec::new()
            }
            fn token(&self, _: &str) -> Option<String> {
                None
            }
            fn serves(&self, _: uuid::Uuid) -> bool {
                true
            }
            fn cooling(&self, _: &str, _: chrono::DateTime<Utc>, _: chrono::DateTime<Utc>) {}
            fn plan_key(&self, id: &str) -> Option<String> {
                (id == "starmoire").then(|| "planofstarmoire".to_string())
            }
        }
        with_test_pool(PgFloorStore::build_pool, |pool| async move {
            let store = Arc::new(PgFloorStore::with_pool(pool).await.unwrap());
            let plans = Plans::new(Arc::clone(&store), crate::floor::signal::Signals::start());
            let dir = tempfile::tempdir().unwrap();
            let vault = Arc::new(
                crate::vault::Vault::open(&dir.path().join("logins.enc"), "test-key").unwrap(),
            );
            install_pool::set(&vault, Some(Arc::new(Pool)));
            let pooled = |id: &str| {
                let credential = Credential::Pooled(id.into());
                Login {
                    tier: CredentialTier::SystemShared,
                    owner: LoginOwner::Install,
                    key: LoginKey::of(&credential).unwrap(),
                    credential,
                    slot: VaultSlot {
                        vault: Arc::clone(&vault),
                        key: SubscriptionKind::Claude.vault_key().to_string(),
                    },
                    ..crate::hub_core::logins::test_support::claude_login("unused")
                }
            };
            plans.observe(&pooled("starmoire"), None, vec![window(100.0, 100)]);
            plans.observe(&pooled("gone"), None, vec![window(5.0, 100)]);
            let kept = loop {
                let kept = store.readings().await.unwrap();
                if kept.len() == 2 {
                    break kept;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            };
            let accounts: Vec<_> = kept.iter().map(|r| r.account.clone()).collect();
            assert!(
                accounts.contains(&Some("planofstarmoire".into())),
                "the account's own card: {accounts:?}"
            );
            assert!(accounts.contains(&None), "one Desk no longer has: nobody's");
            install_pool::set(&vault, None);
        })
        .await;
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
            let someone_else = Login {
                owner: LoginOwner::Person(uuid::Uuid::new_v4()),
                ..theirs.clone()
            };
            plans.observe(&someone_else, None, vec![window(5.0, 100)]);
            plans.observe(&theirs, None, vec![]);
            plans.local(
                LocalPlan::Codex,
                serde_json::json!({"rateLimits": {"primary": {"usedPercent": 12, "windowDurationMins": 300}}}),
                LocalAccount {
                    ids: vec!["acct-1".into()],
                    plan: Some("pro".into()),
                },
            );
            plans.local(LocalPlan::Claude, serde_json::json!([]), LocalAccount::default());
            let kept = loop {
                let kept = store.readings().await.unwrap();
                if kept.len() == 4 {
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
            assert!(
                owners.contains(&(PlanKind::Claude, per_user(&someone_else))),
                "one credential in two people's slots is two readings, each theirs"
            );
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
