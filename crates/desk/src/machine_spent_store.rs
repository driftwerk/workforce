//! What each machine last said each of a boss's grants there spent today,
//! for a boss on this hub (`machine_grant_spent`), so one Act fully budget
//! is counted across this computer and every grant on every machine,
//! through a restart of the hub. A machine's one count from before grants
//! were told apart (`machine_spent`) is read as its grant [`ALL`].

use chrono::{DateTime, Utc};
use eyre::Result;

use crate::store::DeskStore;

/// The grant a machine's count from before grants were told apart is kept
/// under: all of that boss's there.
pub const ALL: &str = "";

/// One machine's count for one boss's grant there: since `count_from`,
/// `usd`.
#[derive(Debug, Clone, PartialEq)]
pub struct MachineSpent {
    pub agent: String,
    pub machine: String,
    pub grant: String,
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
            "INSERT INTO machine_grant_spent (agent, machine, grant_id, count_from, usd, updated_at) \
             VALUES ($1, $2, $3, $4, $5, now()) \
             ON CONFLICT (agent, machine, grant_id, count_from) DO UPDATE SET \
               usd = GREATEST(machine_grant_spent.usd, EXCLUDED.usd), updated_at = now()",
            &[&s.agent, &s.machine, &s.grant, &s.count_from, &s.usd],
        )
        .await?;
        for table in ["machine_grant_spent", "machine_spent"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE count_from < now() - interval '3 days'"),
                &[],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Every machine's count, for every boss's grant.
    pub async fn machine_spent_all(&self) -> Result<Vec<MachineSpent>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT agent, machine, grant_id, count_from, usd FROM machine_grant_spent \
                 UNION ALL \
                 SELECT agent, machine, $1::text, count_from, usd FROM machine_spent",
                &[&ALL],
            )
            .await?
            .iter()
            .map(|row| {
                Ok(MachineSpent {
                    agent: row.try_get("agent")?,
                    machine: row.try_get("machine")?,
                    grant: row.try_get("grant_id")?,
                    count_from: row.try_get("count_from")?,
                    usd: row.try_get("usd")?,
                })
            })
            .collect()
    }
}
