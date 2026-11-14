//! What each machine last said it spent today for a boss on this hub
//! (`machine_spent`), so one Act fully budget is counted across this
//! computer and every machine, through a restart of the hub.

use chrono::{DateTime, Utc};
use eyre::Result;

use crate::store::DeskStore;

/// One machine's count for one boss: since `count_from`, `usd`.
#[derive(Debug, Clone, PartialEq)]
pub struct MachineSpent {
    pub agent: String,
    pub machine: String,
    pub count_from: DateTime<Utc>,
    pub usd: f64,
}

impl DeskStore {
    /// Keeps `s`: for the same `count_from`, the larger of what was kept
    /// and what came (a late answer never lowers it); a later
    /// `count_from` replaces it; an earlier one changes nothing. What was
    /// kept after it, as kept.
    pub async fn machine_spent_put(&self, s: &MachineSpent) -> Result<MachineSpent> {
        let client = self.pool().get().await?;
        let row = client
            .query_one(
                "INSERT INTO machine_spent (agent, machine, count_from, usd, updated_at) \
                 VALUES ($1, $2, $3, $4, now()) \
                 ON CONFLICT (agent, machine) DO UPDATE SET \
                   usd = CASE WHEN EXCLUDED.count_from > machine_spent.count_from THEN EXCLUDED.usd \
                              WHEN EXCLUDED.count_from = machine_spent.count_from \
                                THEN GREATEST(machine_spent.usd, EXCLUDED.usd) \
                              ELSE machine_spent.usd END, \
                   count_from = GREATEST(machine_spent.count_from, EXCLUDED.count_from), \
                   updated_at = now() \
                 RETURNING agent, machine, count_from, usd",
                &[&s.agent, &s.machine, &s.count_from, &s.usd],
            )
            .await?;
        Ok(MachineSpent {
            agent: row.try_get("agent")?,
            machine: row.try_get("machine")?,
            count_from: row.try_get("count_from")?,
            usd: row.try_get("usd")?,
        })
    }

    /// Every machine's count, for every boss.
    pub async fn machine_spent_all(&self) -> Result<Vec<MachineSpent>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT agent, machine, count_from, usd FROM machine_spent",
                &[],
            )
            .await?
            .iter()
            .map(|row| {
                Ok(MachineSpent {
                    agent: row.try_get("agent")?,
                    machine: row.try_get("machine")?,
                    count_from: row.try_get("count_from")?,
                    usd: row.try_get("usd")?,
                })
            })
            .collect()
    }
}
