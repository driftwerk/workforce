//! Node presence in Postgres, so a turn on any hub instance sees its
//! person's nodes. The instance holding a node's connection puts its row
//! when the node registers and every 30 s after, and marks it offline when
//! the connection ends; a row not put for 90 s is offline whatever it says,
//! so a crashed instance's nodes lapse on their own.

use std::sync::Arc;

use tracing::warn;
use workforce_common::settings::WorkforceConfig;

use crate::hub_core::node_relay::NodePresence;

/// The install's presence store; `None` without Postgres, where the nodes
/// this instance holds are all there are.
pub async fn open(config: &WorkforceConfig) -> Option<Arc<dyn NodePresence>> {
    #[cfg(feature = "postgres")]
    if let Some(pg) = config.postgres.as_ref().filter(|p| p.enabled) {
        return match postgres::PostgresPresence::new(pg).await {
            Ok(presence) => Some(Arc::new(presence)),
            Err(e) => {
                warn!(
                    error = %format!("{e:#}"),
                    "Node presence store unavailable; nodes reach this instance's turns alone"
                );
                None
            }
        };
    }
    let _ = config;
    None
}

#[cfg(feature = "postgres")]
pub mod postgres {
    use async_trait::async_trait;
    use eyre::{Context, Result};
    use tokio_postgres::Row;
    use workforce_core::computer::OnlineDesk;
    use workforce_postgres::{
        MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
    };

    use crate::hub_core::node_relay::{Desks, NodePresence, PresenceKind, PresentAgent};

    // Own directory, never a subdir of another embed root — refinery's
    // embed_migrations is recursive.
    mod embedded {
        refinery::embed_migrations!("migrations_nodes");
    }

    struct Migrations;

    #[async_trait]
    impl MigrationRun for Migrations {
        async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
            embedded::migrations::runner()
                .run_async(client)
                .await
                .map_err(PostgresError::from)?;
            Ok(())
        }
    }

    const SEARCH_PATH: &str = "SET search_path TO nodes, public";

    /// Whether a row is online: its holder said so, and put it lately.
    const LIVE: &str = "online AND seen_at > now() - interval '90 seconds'";

    pub struct PostgresPresence {
        pool: PostgresPool,
        /// This instance, which marks offline only the rows it put.
        instance_id: String,
    }

    impl PostgresPresence {
        pub const SCHEMA: &'static str = "nodes";

        pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
            PostgresPool::from_config_with_session_init(config, &[SEARCH_PATH])
        }

        pub async fn new(config: &PostgresConfig) -> Result<Self> {
            let pool = Self::build_pool(config).context("building node presence pool")?;
            Self::with_pool(
                pool,
                workforce_common::instance::process_instance_id().to_string(),
            )
            .await
        }

        pub async fn with_pool(pool: PostgresPool, instance_id: String) -> Result<Self> {
            MigrationRunner::run(
                &pool,
                &[MigrationSet::new(
                    Self::SCHEMA,
                    Box::new(Migrations) as Box<dyn MigrationRun>,
                )],
            )
            .await
            .context("running node presence migrations")?;
            Ok(Self { pool, instance_id })
        }
    }

    fn kind(row: &Row) -> Result<PresenceKind> {
        serde_json::from_value(row.try_get("kind")?).context("reading a node's presence")
    }

    #[async_trait]
    impl NodePresence for PostgresPresence {
        async fn put(&self, key: &str, identity: &str, kind: &PresenceKind) -> Result<()> {
            let kind = serde_json::to_value(kind)?;
            self.pool
                .get()
                .await?
                .execute(
                    "INSERT INTO presence (key, identity_id, kind, instance_id, online, seen_at) \
                     VALUES ($1, $2, $3, $4, true, now()) \
                     ON CONFLICT (key) DO UPDATE SET identity_id = $2, kind = $3, \
                     instance_id = $4, online = true, seen_at = now()",
                    &[&key, &identity, &kind, &self.instance_id],
                )
                .await?;
            Ok(())
        }

        async fn gone(&self, key: &str) -> Result<()> {
            self.pool
                .get()
                .await?
                .execute(
                    "UPDATE presence SET online = false WHERE key = $1 AND instance_id = $2",
                    &[&key, &self.instance_id],
                )
                .await?;
            Ok(())
        }

        async fn forget(&self, key: &str) -> Result<()> {
            self.pool
                .get()
                .await?
                .execute("DELETE FROM presence WHERE key = $1", &[&key])
                .await?;
            Ok(())
        }

        async fn desks_of(&self, identity: &str) -> Result<Desks> {
            let rows = self
                .pool
                .get()
                .await?
                .query(
                    &format!(
                        "SELECT key, kind, {LIVE} AS live, seen_at FROM presence \
                         WHERE identity_id = $1 AND kind->>'kind' = 'desk' \
                         ORDER BY seen_at DESC"
                    ),
                    &[&identity],
                )
                .await?;
            rows.iter().try_fold(Desks::default(), |mut desks, row| {
                let PresenceKind::Desk { device_id, node } = kind(row)? else {
                    return Ok(desks);
                };
                desks.known.push(device_id.clone());
                match row.try_get::<_, bool>("live")? {
                    true => desks.online.push(OnlineDesk {
                        key: row.try_get("key")?,
                        device_id,
                        node,
                    }),
                    false => desks.last_seen = desks.last_seen.max(Some(row.try_get("seen_at")?)),
                }
                Ok(desks)
            })
        }

        async fn agents(&self, identity: Option<&str>) -> Result<Vec<PresentAgent>> {
            let rows = self
                .pool
                .get()
                .await?
                .query(
                    &format!(
                        "SELECT key, identity_id, kind FROM presence \
                         WHERE kind->>'kind' = 'agent' AND {LIVE} \
                         AND ($1::text IS NULL OR identity_id = $1) ORDER BY key"
                    ),
                    &[&identity],
                )
                .await?;
            rows.iter()
                .filter_map(|row| match kind(row) {
                    Ok(PresenceKind::Agent(agent)) => Some(Ok((row, agent))),
                    Ok(PresenceKind::Desk { .. }) => None,
                    Err(e) => Some(Err(e)),
                })
                .map(|read| {
                    let (row, agent) = read?;
                    Ok(PresentAgent {
                        key: row.try_get("key")?,
                        identity_id: row.try_get("identity_id")?,
                        agent,
                    })
                })
                .collect()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use chrono::Utc;
        use workforce_core::computer::{DeskNode, Platform, Runtime};
        use workforce_gateway_client::protocol::NodeToolSpec;
        use workforce_postgres::test_support::with_test_pool;

        use crate::hub_core::node_relay::AgentPresence;

        fn desk(device_id: &str, name: &str, enabled: bool) -> PresenceKind {
            PresenceKind::Desk {
                device_id: device_id.into(),
                node: DeskNode {
                    name: name.into(),
                    platform: Platform::Linux,
                    runtime: Runtime::Native,
                    enabled,
                    folders: vec!["~/dev/acme-web".into()],
                    active_at: Utc::now(),
                },
            }
        }

        fn agent(name: &str) -> PresenceKind {
            PresenceKind::Agent(AgentPresence {
                name: name.into(),
                capabilities: vec!["shell".into()],
                tools: vec![NodeToolSpec {
                    name: "files_read".into(),
                    description: "Read a file".into(),
                    input_schema: serde_json::json!({"type": "object"}),
                    read_only: true,
                }],
                connected_at: Utc::now(),
            })
        }

        /// Two instances over one database.
        async fn instances(pool: PostgresPool) -> (PostgresPresence, PostgresPresence) {
            let a = PostgresPresence::with_pool(pool.clone(), "hub-a".into())
                .await
                .unwrap();
            let b = PostgresPresence::with_pool(pool, "hub-b".into())
                .await
                .unwrap();
            (a, b)
        }

        fn online_names(desks: &Desks) -> Vec<&str> {
            desks.online.iter().map(|d| d.node.name.as_str()).collect()
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_persons_desks_are_listed_online_switched_on_or_off() {
            with_test_pool(PostgresPresence::build_pool, |pool| async move {
                let (a, b) = instances(pool).await;
                a.put("desk:d1", "sarah", &desk("d1", "MacBook Pro", true))
                    .await
                    .unwrap();
                b.put("desk:d2", "sarah", &desk("d2", "Atlas", false))
                    .await
                    .unwrap();
                a.put("desk:d3", "bob", &desk("d3", "Bob's laptop", true))
                    .await
                    .unwrap();
                a.put("studio", "sarah", &agent("Studio")).await.unwrap();

                let sarahs = b.desks_of("sarah").await.unwrap();
                let mut names = online_names(&sarahs);
                names.sort();
                assert_eq!(names, ["Atlas", "MacBook Pro"]);
                assert_eq!(sarahs.last_seen, None);
                assert_eq!(sarahs.known.len(), 2);
                let atlas = sarahs.online.iter().find(|d| d.device_id == "d2").unwrap();
                assert_eq!((atlas.key.as_str(), atlas.node.enabled), ("desk:d2", false));
                assert!(b.desks_of("carol").await.unwrap().known.is_empty());
            })
            .await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_desk_gone_offline_is_last_seen_and_only_its_holder_takes_it_offline() {
            with_test_pool(PostgresPresence::build_pool, |pool| async move {
                let (a, b) = instances(pool).await;
                a.put("desk:d1", "sarah", &desk("d1", "MacBook Pro", true))
                    .await
                    .unwrap();
                b.gone("desk:d1").await.unwrap();
                assert_eq!(
                    online_names(&a.desks_of("sarah").await.unwrap()),
                    ["MacBook Pro"],
                    "another instance's teardown leaves it"
                );

                a.gone("desk:d1").await.unwrap();
                let sarahs = b.desks_of("sarah").await.unwrap();
                assert!(sarahs.online.is_empty());
                assert!(sarahs.last_seen.is_some());
                assert_eq!(sarahs.known, ["d1"]);

                b.put("desk:d1", "sarah", &desk("d1", "MacBook Pro", true))
                    .await
                    .unwrap();
                a.gone("desk:d1").await.unwrap();
                assert_eq!(
                    online_names(&a.desks_of("sarah").await.unwrap()),
                    ["MacBook Pro"],
                    "reconnected to another instance, the old one's teardown leaves it"
                );
            })
            .await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_node_not_put_for_ninety_seconds_is_offline() {
            with_test_pool(PostgresPresence::build_pool, |pool| async move {
                let (a, b) = instances(pool.clone()).await;
                a.put("desk:d1", "sarah", &desk("d1", "MacBook Pro", true))
                    .await
                    .unwrap();
                a.put("studio", "sarah", &agent("Studio")).await.unwrap();
                let age = |secs: i32| {
                    let pool = pool.clone();
                    async move {
                        pool.get()
                            .await
                            .unwrap()
                            .execute(
                                "UPDATE presence SET seen_at = now() - make_interval(secs => $1)",
                                &[&f64::from(secs)],
                            )
                            .await
                            .unwrap();
                    }
                };
                age(80).await;
                assert_eq!(b.desks_of("sarah").await.unwrap().online.len(), 1);
                assert_eq!(b.agents(Some("sarah")).await.unwrap().len(), 1);
                age(91).await;
                let sarahs = b.desks_of("sarah").await.unwrap();
                assert!(sarahs.online.is_empty());
                assert!(sarahs.last_seen.is_some());
                assert!(b.agents(None).await.unwrap().is_empty());
                a.put("studio", "sarah", &agent("Studio")).await.unwrap();
                assert_eq!(b.agents(None).await.unwrap().len(), 1, "put again");
            })
            .await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn agents_are_listed_by_person_and_an_unpaired_desk_is_forgotten() {
            with_test_pool(PostgresPresence::build_pool, |pool| async move {
                let (a, b) = instances(pool).await;
                a.put("studio", "sarah", &agent("Studio")).await.unwrap();
                b.put("bench", "bob", &agent("Bench")).await.unwrap();
                a.put("desk:d1", "sarah", &desk("d1", "MacBook Pro", true))
                    .await
                    .unwrap();

                let sarahs = b.agents(Some("sarah")).await.unwrap();
                assert_eq!(sarahs.len(), 1);
                assert_eq!(
                    (sarahs[0].key.as_str(), sarahs[0].identity_id.as_str()),
                    ("studio", "sarah")
                );
                assert_eq!(sarahs[0].agent.name, "Studio");
                assert!(sarahs[0].agent.tools[0].read_only);
                let everyone: Vec<String> = a
                    .agents(None)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|a| a.key)
                    .collect();
                assert_eq!(everyone, ["bench", "studio"]);

                b.forget("desk:d1").await.unwrap();
                assert!(a.desks_of("sarah").await.unwrap().known.is_empty());
            })
            .await;
        }
    }
}
