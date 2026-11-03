//! Charges in Postgres (`charges`): one row per session, written whole on
//! every change. A released charge keeps its row.

use eyre::Result;
use tokio_postgres::Row;
use uuid::Uuid;

use crate::charge::{Charge, How, Wheel};
use crate::store::DeskStore;

const COLUMNS: &str =
    "session, group_id, boss, how, wheel, wheel_since, since, released_at, updated_at";

fn charge(row: &Row) -> Result<Charge> {
    Ok(Charge {
        session: row.try_get("session")?,
        group: row.try_get("group_id")?,
        boss: row.try_get("boss")?,
        how: How::parse(row.try_get("how")?)?,
        wheel: Wheel::parse(row.try_get("wheel")?)?,
        wheel_since: row.try_get("wheel_since")?,
        since: row.try_get("since")?,
        released_at: row.try_get("released_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

impl DeskStore {
    /// Writes `c` whole, new or changed.
    pub async fn put_charge(&self, c: &Charge) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                &format!(
                    "INSERT INTO charges ({COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
                     ON CONFLICT (session) DO UPDATE SET group_id = EXCLUDED.group_id, \
                     boss = EXCLUDED.boss, how = EXCLUDED.how, wheel = EXCLUDED.wheel, \
                     wheel_since = EXCLUDED.wheel_since, since = EXCLUDED.since, \
                     released_at = EXCLUDED.released_at, updated_at = EXCLUDED.updated_at"
                ),
                &[
                    &c.session,
                    &c.group,
                    &c.boss,
                    &c.how.as_str(),
                    &c.wheel.as_str(),
                    &c.wheel_since,
                    &c.since,
                    &c.released_at,
                    &c.updated_at,
                ],
            )
            .await?;
        Ok(())
    }

    /// Every charge, released ones too.
    pub async fn charges(&self) -> Result<Vec<Charge>> {
        let client = self.pool().get().await?;
        client
            .query(&format!("SELECT {COLUMNS} FROM charges"), &[])
            .await?
            .iter()
            .map(charge)
            .collect()
    }

    pub async fn charge(&self, session: Uuid) -> Result<Option<Charge>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!("SELECT {COLUMNS} FROM charges WHERE session = $1"),
                &[&session],
            )
            .await?
            .as_ref()
            .map(charge)
            .transpose()
    }
}
