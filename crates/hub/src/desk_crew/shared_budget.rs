//! One Act fully budget for a boss on this hub, counted across this
//! computer and every grant it has on every machine: its group's, and each
//! of its plans' (designs/act-fully-machines-design.md). Each take the Mac
//! relays under a grant ([`super::relayed_decide`]) carries where today's
//! count starts here (`countFrom`) and what was spent everywhere but that
//! grant there (`spentElsewhereUsd`); the machine answers with that grant's
//! sessions' spend since then (`spentUsd`). The Mac keeps each grant's
//! latest answer, here and in `machine_grant_spent`, and
//! [`CrewHub::fully`] adds them all to this computer's own spend: when the
//! total reaches the budget, Act fully ends here, and the next take ends it
//! on every machine.
//!
//! - A count is a grant's whole spend since `countFrom`, never a delta,
//!   so a lost or repeated answer changes nothing; for the same
//!   `countFrom` the larger is kept, so a late answer never lowers it.
//!   Two grants on one machine are two counts, added up.
//! - A machine's one count from before grants were told apart
//!   ([`ALL`]) is what it spent in all then: that machine counts at least
//!   that much.
//! - Each count is kept for its own `countFrom`, so a count for one start
//!   never stands in for another's (a clear, midnight, or a change of time
//!   zone moving the start either way). Counts over [`KEPT_FOR`] old go;
//!   none goes because its machine went offline, or its grant went (a
//!   session left the group, a plan ended): that money was spent.
//! - Until what was kept has been read back after a start, no boss here
//!   acts fully ([`CrewHub::machines_spent`] says nothing), so a restart
//!   never forgets an offline machine's spend.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use workforce_common::oversight::Oversight;
use workforce_desk::machine_spent_store::{MachineSpent, ALL};

use super::CrewHub;

/// The most a machine's count may say, in dollars: anything above is not a
/// count.
pub(crate) const MOST_USD: f64 = 1_000_000.0;

/// How long a count is kept after its start: a day's count is never needed
/// once its day is well over.
pub(crate) const KEPT_FOR: chrono::Duration = chrono::Duration::days(3);

/// A grant's count, by machine, grant and where it starts.
type Slot = (String, String, DateTime<Utc>);

/// Each grant's counts, by boss, machine, grant and start.
#[derive(Debug, Default)]
pub(crate) struct Machines {
    loaded: bool,
    counts: HashMap<String, HashMap<Slot, f64>>,
    /// Counts not yet kept in the store: written again with the next answer.
    unsaved: std::collections::HashSet<(String, Slot)>,
}

impl Machines {
    /// Takes `s` as the store keeps it: for its start, the larger of what
    /// was kept and what came. Whether it changed what is counted.
    fn take(&mut self, s: &MachineSpent) -> bool {
        let slot = self
            .counts
            .entry(s.agent.clone())
            .or_default()
            .entry((s.machine.clone(), s.grant.clone(), s.count_from))
            .or_insert(f64::NEG_INFINITY);
        let changed = s.usd > *slot;
        if changed {
            *slot = s.usd;
        }
        changed
    }

    /// What `agent`'s grants on `machine` but `except` spent since `from`,
    /// by grant: its count from before grants were told apart left out.
    fn on(
        &self,
        agent: &str,
        machine: &str,
        from: DateTime<Utc>,
        except: &str,
    ) -> HashMap<String, f64> {
        self.counts
            .get(agent)
            .into_iter()
            .flatten()
            .filter(|((m, g, f), _)| m == machine && g != except && g != ALL && *f == from)
            .map(|((_, g, _), usd)| (g.clone(), *usd))
            .collect()
    }

    /// Forgets counts that started over [`KEPT_FOR`] before `now`.
    fn tidy(&mut self, now: DateTime<Utc>) {
        for c in self.counts.values_mut() {
            c.retain(|(.., f), _| now - *f <= KEPT_FOR);
        }
        self.counts.retain(|_, c| !c.is_empty());
        self.unsaved.retain(|(_, (.., f))| now - *f <= KEPT_FOR);
    }

    /// What `agent`'s grants on every machine but `except` (a machine and
    /// grant) spent since `from`: each machine its grants' counts added up,
    /// and never less than its count from before grants were told apart.
    fn sum(&self, agent: &str, from: DateTime<Utc>, except: Option<(&str, &str)>) -> f64 {
        let Some(counts) = self.counts.get(agent) else {
            return 0.0;
        };
        let mut machines: HashMap<&str, (f64, f64)> = HashMap::new();
        for ((machine, grant, _), usd) in counts.iter().filter(|((.., f), _)| *f == from) {
            let (all, grants) = machines.entry(machine.as_str()).or_default();
            match grant.as_str() {
                ALL => *all = all.max(*usd),
                _ => *grants += usd,
            }
        }
        let left_out = except
            .and_then(|(m, g)| counts.get(&(m.to_string(), g.to_string(), from)))
            .copied()
            .unwrap_or(0.0);
        let total: f64 = machines
            .values()
            .map(|(all, grants)| all.max(*grants))
            .sum();
        (total - left_out).max(0.0)
    }
}

/// A count as a machine says it: dollars, finite, from 0 to [`MOST_USD`].
pub(crate) fn usd_of(v: &serde_json::Value) -> Option<f64> {
    v.as_f64()
        .filter(|u| u.is_finite() && (0.0..=MOST_USD).contains(u))
}

impl CrewHub {
    /// Where today's count of `o`'s Act fully budget starts here: the
    /// person's midnight, or when they cleared it. None when it isn't at
    /// Act fully.
    pub(crate) fn count_from(&self, o: &Oversight) -> Option<DateTime<Utc>> {
        let full = o.fully().ok()?;
        let now = Utc::now();
        let midnight = self
            .desk
            .period_start(workforce_desk::ledger::Period::Day, now);
        Some(super::counted_from(midnight, now, full))
    }

    /// Reads back what was kept of the machines' counts, once after a
    /// start: until then no boss here acts fully.
    pub(crate) async fn load_machines_spent(&self) {
        if self.machine_spent.lock().loaded {
            return;
        }
        match self.desk.store().machine_spent_all().await {
            Ok(rows) => {
                let mut m = self.machine_spent.lock();
                for r in &rows {
                    m.take(r);
                }
                m.tidy(Utc::now());
                m.loaded = true;
            }
            Err(e) => tracing::warn!(
                error = format!("{e:#}"),
                "the machines' Act fully spend couldn't be read; Act fully waits until it can"
            ),
        }
    }

    /// Forgets what was read back, as a start of the hub does.
    #[cfg(test)]
    pub(crate) fn forget_machines_spent(&self) {
        *self.machine_spent.lock() = Machines::default();
    }

    /// What `agent`'s machines spent since `from`, as they last said; none
    /// until what was kept has been read back.
    pub(crate) fn machines_spent(&self, agent: &str, from: DateTime<Utc>) -> Option<f64> {
        let m = self.machine_spent.lock();
        m.loaded.then(|| m.sum(agent, from, None))
    }

    /// What `agent` spent since `from` everywhere but its grant `grant` on
    /// `machine`: this computer's own spend, as last read, and every other
    /// grant's count, on that machine too. None while either isn't known.
    pub(crate) fn spent_elsewhere(
        &self,
        agent: &str,
        machine: &str,
        grant: &str,
        from: DateTime<Utc>,
    ) -> Option<f64> {
        let here = self.spent_today.lock().get(agent).copied()?;
        let m = self.machine_spent.lock();
        m.loaded
            .then(|| here + m.sum(agent, from, Some((machine, grant))))
    }

    /// What `agent`'s grants on `machine` but `grant` spent since `from`, as
    /// they last said, by grant: what [`Self::spent_elsewhere`] counts of
    /// them, for the machine to add what they spent since.
    pub(crate) fn grants_spent_on(
        &self,
        agent: &str,
        machine: &str,
        grant: &str,
        from: DateTime<Utc>,
    ) -> HashMap<String, f64> {
        self.machine_spent.lock().on(agent, machine, from, grant)
    }

    /// `machine` said `agent`'s sessions there under its grant `grant`
    /// spent `usd` since `from`: counted at once, and kept.
    pub(crate) async fn machine_reported(
        &self,
        agent: &str,
        machine: &str,
        grant: &str,
        from: DateTime<Utc>,
        usd: f64,
    ) {
        let s = MachineSpent {
            agent: agent.to_string(),
            machine: machine.to_string(),
            grant: grant.to_string(),
            count_from: from,
            usd,
        };
        let key = (
            agent.to_string(),
            (machine.to_string(), grant.to_string(), from),
        );
        let write = {
            let mut m = self.machine_spent.lock();
            m.tidy(Utc::now());
            let changed = m.take(&s);
            changed || m.unsaved.contains(&key)
        };
        if !write {
            return;
        }
        match self.desk.store().machine_spent_put(&s).await {
            Ok(()) => {
                self.machine_spent.lock().unsaved.remove(&key);
            }
            Err(e) => {
                self.machine_spent.lock().unsaved.insert(key);
                tracing::warn!(
                    %machine,
                    error = format!("{e:#}"),
                    "a machine's Act fully spend wasn't kept yet; it is written again with its next answer"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(h: u32) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(&format!("2026-11-14T{h:02}:00:00Z"))
            .unwrap()
            .to_utc()
    }

    fn s(machine: &str, from: DateTime<Utc>, usd: f64) -> MachineSpent {
        on(machine, "acme", from, usd)
    }

    fn on(machine: &str, grant: &str, from: DateTime<Utc>, usd: f64) -> MachineSpent {
        MachineSpent {
            agent: "sarah".into(),
            machine: machine.into(),
            grant: grant.into(),
            count_from: from,
            usd,
        }
    }

    #[test]
    fn a_count_only_grows_within_its_day_and_starts_again_at_the_next() {
        let mut m = Machines::default();
        m.take(&s("devbox", at(0), 10.0));
        m.take(&s("devbox", at(0), 4.0));
        assert_eq!(
            m.sum("sarah", at(0), None),
            10.0,
            "a late answer never lowers it"
        );
        m.take(&s("buildbox", at(0), 5.0));
        assert_eq!(m.sum("sarah", at(0), None), 15.0);
        assert_eq!(m.sum("sarah", at(0), Some(("devbox", "acme"))), 5.0);
        // Cleared at 09:00: the old counts say nothing of the new day.
        m.take(&s("devbox", at(9), 1.0));
        assert_eq!(m.sum("sarah", at(9), None), 1.0);
        m.take(&s("devbox", at(0), 50.0));
        assert_eq!(
            m.sum("sarah", at(9), None),
            1.0,
            "each start is its own count"
        );
        // The start moved back (a time zone changed, a clear undone): the
        // count for it still counts, and grows on its own.
        assert_eq!(m.sum("sarah", at(0), None), 55.0);
        m.tidy(at(0) + KEPT_FOR + chrono::Duration::hours(1));
        assert_eq!(m.sum("sarah", at(0), None), 0.0, "old counts go");
        assert_eq!(m.sum("sarah", at(9), None), 1.0);
        assert_eq!(m.sum("other", at(9), None), 0.0);
    }

    #[test]
    fn a_bosss_grants_on_one_machine_add_up_and_one_gone_still_counts() {
        let mut m = Machines::default();
        m.take(&on("devbox", "plan-a", at(0), 10.0));
        m.take(&on("devbox", "plan-b", at(0), 4.0));
        m.take(&on("buildbox", "plan-a", at(0), 1.0));
        assert_eq!(m.sum("sarah", at(0), None), 15.0, "one total, not the most");
        assert_eq!(m.sum("sarah", at(0), Some(("devbox", "plan-a"))), 5.0);
        assert_eq!(m.sum("sarah", at(0), Some(("devbox", "plan-b"))), 11.0);
        // plan-a's grant went: nothing more comes for it, and what it spent
        // still counts as plan-b spends on.
        m.take(&on("devbox", "plan-b", at(0), 12.0));
        assert_eq!(m.sum("sarah", at(0), None), 23.0);
    }

    #[test]
    fn a_count_from_before_grants_were_told_apart_is_its_machines_least() {
        let mut m = Machines::default();
        m.take(&on("devbox", ALL, at(0), 20.0));
        m.take(&on("devbox", "acme", at(0), 12.0));
        assert_eq!(
            m.sum("sarah", at(0), None),
            20.0,
            "never less than it said in all"
        );
        assert_eq!(m.sum("sarah", at(0), Some(("devbox", "acme"))), 8.0);
        m.take(&on("devbox", "plan-b", at(0), 15.0));
        assert_eq!(
            m.sum("sarah", at(0), None),
            27.0,
            "once its grants say more"
        );
        assert_eq!(m.sum("sarah", at(0), Some(("devbox", "plan-b"))), 12.0);
    }

    #[test]
    fn only_a_plain_amount_is_a_count() {
        assert_eq!(usd_of(&serde_json::json!(2.5)), Some(2.5));
        assert_eq!(usd_of(&serde_json::json!(0)), Some(0.0));
        assert_eq!(usd_of(&serde_json::json!(-1)), None);
        assert_eq!(usd_of(&serde_json::json!("3")), None);
        assert_eq!(usd_of(&serde_json::json!(MOST_USD * 2.0)), None);
        assert_eq!(usd_of(&serde_json::Value::Null), None);
    }
}
