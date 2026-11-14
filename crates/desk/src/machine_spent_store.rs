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
    /// Keeps `s`: for its start, the larger of what was kept and what came,
    /// so a late answer never lowers it. Counts that started over three days
    /// ago go.
    pub async fn machine_spent_put(&self, s: &MachineSpent) -> Result<()> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        tx.execute(
            "INSERT INTO machine_spent (agent, machine, count_from, usd, updated_at) \
             VALUES ($1, $2, $3, $4, now()) \
             ON CONFLICT (agent, machine, count_from) DO UPDATE SET \
               usd = GREATEST(machine_spent.usd, EXCLUDED.usd), updated_at = now()",
            &[&s.agent, &s.machine, &s.count_from, &s.usd],
        )
        .await?;
        tx.execute(
            "DELETE FROM machine_spent WHERE count_from < now() - interval '3 days'",
            &[],
        )
        .await?;
        tx.commit().await?;
        Ok(())
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
