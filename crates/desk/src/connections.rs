//! What each connection to a service last said when it was checked
//! (`desk` schema, V024; one per adapter instance since V059). The keys are in the hub's vault only: a row here
//! holds the account a check named, its status and when, never a secret.
//! `detail` reaches here already tokenised by the hub.

use chrono::{DateTime, Utc};
use eyre::Result;
use serde::Serialize;
use tokio_postgres::Row;

use crate::store::DeskStore;

/// Where a connection's keys are kept: for all the person's agents, or for
/// one agent only.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Level {
    All,
    Agent(String),
}

impl Level {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Agent(_) => "agent",
        }
    }

    /// The agent column: empty for all the person's agents.
    pub fn agent(&self) -> &str {
        match self {
            Self::All => "",
            Self::Agent(agent) => agent,
        }
    }

    fn of(level: &str, agent: String) -> Option<Self> {
        match level {
            "all" => Some(Self::All),
            "agent" if !agent.is_empty() => Some(Self::Agent(agent)),
            _ => None,
        }
    }
}

/// What the last check of a connection found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Connected,
    /// The service refused the key: it has to be connected again.
    NeedsReconnect,
    /// The service couldn't be reached, or answered with an error.
    Failing,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::NeedsReconnect => "needs_reconnect",
            Self::Failing => "failing",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "connected" => Some(Self::Connected),
            "needs_reconnect" => Some(Self::NeedsReconnect),
            "failing" => Some(Self::Failing),
            _ => None,
        }
    }
}

/// One connection's last check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub connector: String,
    pub level: Level,
    /// The adapter instance it is (`acme` of `skills/jira/workspaces/acme/…`);
    /// `default` for a service with one.
    pub instance: String,
    pub person: String,
    pub account: Option<String>,
    pub status: Status,
    pub detail: Option<String>,
    pub connected_at: DateTime<Utc>,
    pub checked_at: Option<DateTime<Utc>>,
}

fn checked(row: &Row) -> Result<Option<Checked>> {
    let level: String = row.try_get("level")?;
    let status: String = row.try_get("status")?;
    let (Some(level), Some(status)) = (
        Level::of(&level, row.try_get("agent")?),
        Status::parse(&status),
    ) else {
        return Ok(None);
    };
    Ok(Some(Checked {
        connector: row.try_get("connector")?,
        level,
        instance: row.try_get("instance")?,
        person: row.try_get("person")?,
        account: row.try_get("account")?,
        status,
        detail: row.try_get("detail")?,
        connected_at: row.try_get("connected_at")?,
        checked_at: row.try_get("checked_at")?,
    }))
}

impl DeskStore {
    /// Every connection `person` has checked, as last checked.
    pub async fn connections(&self, person: &str) -> Result<Vec<Checked>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                "SELECT * FROM connections WHERE person = $1 ORDER BY connector, level, agent, instance",
                &[&person],
            )
            .await?;
        Ok(rows
            .iter()
            .map(checked)
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect())
    }

    /// Keeps what a check found. `connected`: the keys were just saved, so
    /// the connection starts now; otherwise it keeps when it was connected.
    pub async fn put_connection(&self, c: &Checked, connected: bool) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO connections
                   (connector, level, agent, person, account, status, detail, connected_at, checked_at, instance)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $11)
                 ON CONFLICT (connector, level, agent, person, instance) DO UPDATE SET
                   account = COALESCE(EXCLUDED.account, connections.account),
                   status = EXCLUDED.status,
                   detail = EXCLUDED.detail,
                   checked_at = EXCLUDED.checked_at,
                   connected_at = CASE WHEN $10 THEN EXCLUDED.connected_at
                                       ELSE connections.connected_at END",
                &[
                    &c.connector,
                    &c.level.as_str(),
                    &c.level.agent(),
                    &c.person,
                    &c.account,
                    &c.status.as_str(),
                    &c.detail,
                    &c.connected_at,
                    &c.checked_at,
                    &connected,
                    &c.instance,
                ],
            )
            .await?;
        Ok(())
    }

    /// Forgets one connection's checks.
    pub async fn drop_connection(
        &self,
        connector: &str,
        level: &Level,
        instance: &str,
        person: &str,
    ) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "DELETE FROM connections
                 WHERE connector = $1 AND level = $2 AND agent = $3 AND person = $4
                   AND instance = $5",
                &[
                    &connector,
                    &level.as_str(),
                    &level.agent(),
                    &person,
                    &instance,
                ],
            )
            .await?;
        Ok(())
    }

    /// Moves a connection's checks to another instance: a flat connection
    /// becoming a named one keeps what its last check found. A check
    /// already kept there stays, and the one moved from goes either way, so
    /// a later connection at `from` never inherits it.
    pub async fn move_connection(
        &self,
        connector: &str,
        level: &Level,
        person: &str,
        from: &str,
        to: &str,
    ) -> Result<()> {
        if from == to {
            return Ok(());
        }
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE connections SET instance = $6
                 WHERE connector = $1 AND level = $2 AND agent = $3 AND person = $4
                   AND instance = $5
                   AND NOT EXISTS (SELECT 1 FROM connections c
                                   WHERE c.connector = $1 AND c.level = $2 AND c.agent = $3
                                     AND c.person = $4 AND c.instance = $6)",
                &[
                    &connector,
                    &level.as_str(),
                    &level.agent(),
                    &person,
                    &from,
                    &to,
                ],
            )
            .await?;
        client
            .execute(
                "DELETE FROM connections
                 WHERE connector = $1 AND level = $2 AND agent = $3 AND person = $4
                   AND instance = $5",
                &[&connector, &level.as_str(), &level.agent(), &person, &from],
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::build_pool;
    use workforce_postgres::test_support::with_test_pool;

    fn check(connector: &str, level: Level, status: Status, account: Option<&str>) -> Checked {
        Checked {
            connector: connector.into(),
            level,
            instance: "default".into(),
            person: "sarah".into(),
            account: account.map(str::to_string),
            status,
            detail: None,
            connected_at: Utc::now(),
            checked_at: Some(Utc::now()),
        }
    }

    #[tokio::test]
    async fn a_check_is_kept_per_connector_level_and_agent() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let all = check(
                "linear",
                Level::All,
                Status::Connected,
                Some("sarah@acme.example"),
            );
            let allan = check(
                "linear",
                Level::Agent("allan".into()),
                Status::Connected,
                Some("bot@acme.example"),
            );
            store.put_connection(&all, true).await.unwrap();
            store.put_connection(&allan, true).await.unwrap();
            assert_eq!(store.connections("sarah").await.unwrap().len(), 2);
            assert!(store.connections("ben").await.unwrap().is_empty());

            // A later failing check keeps the account and when it connected.
            let failing = Checked {
                account: None,
                detail: Some("Linear says this key is invalid".into()),
                connected_at: Utc::now() + chrono::Duration::days(1),
                ..check("linear", Level::All, Status::NeedsReconnect, None)
            };
            store.put_connection(&failing, false).await.unwrap();
            let rows = store.connections("sarah").await.unwrap();
            let row = rows.iter().find(|r| r.level == Level::All).unwrap();
            assert_eq!(row.status, Status::NeedsReconnect);
            assert_eq!(row.account.as_deref(), Some("sarah@acme.example"));
            assert_eq!(row.connected_at.timestamp(), all.connected_at.timestamp());

            // Dropping one level leaves the other.
            store
                .drop_connection("linear", &Level::All, "default", "sarah")
                .await
                .unwrap();
            let rows = store.connections("sarah").await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].level, Level::Agent("allan".into()));
        })
        .await;
    }

    #[tokio::test]
    async fn a_check_is_kept_per_instance_and_moves_with_it() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let acme = Checked {
                instance: "acme".into(),
                ..check(
                    "jira",
                    Level::All,
                    Status::Connected,
                    Some("sarah@acme.example"),
                )
            };
            let globex = Checked {
                instance: "globex".into(),
                ..check(
                    "jira",
                    Level::All,
                    Status::Failing,
                    Some("sarah@globex.example"),
                )
            };
            store.put_connection(&acme, true).await.unwrap();
            store.put_connection(&globex, true).await.unwrap();
            let rows = store.connections("sarah").await.unwrap();
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].instance, "acme");
            assert_eq!(rows[1].status, Status::Failing);

            // A flat connection's check moves with it to its instance, and
            // never over one kept there: then it goes, and the one there
            // stays.
            let flat = check(
                "linear",
                Level::All,
                Status::Connected,
                Some("sarah@acme.example"),
            );
            store.put_connection(&flat, true).await.unwrap();
            store
                .move_connection("linear", &Level::All, "sarah", "default", "acme")
                .await
                .unwrap();
            store
                .move_connection("jira", &Level::All, "sarah", "acme", "globex")
                .await
                .unwrap();
            let rows = store.connections("sarah").await.unwrap();
            let names: Vec<(&str, &str)> = rows
                .iter()
                .map(|r| (r.connector.as_str(), r.instance.as_str()))
                .collect();
            assert_eq!(names, [("jira", "globex"), ("linear", "acme")]);

            store
                .drop_connection("jira", &Level::All, "globex", "sarah")
                .await
                .unwrap();
            assert_eq!(store.connections("sarah").await.unwrap().len(), 1);
        })
        .await;
    }
}
