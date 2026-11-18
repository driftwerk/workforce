//! People's route settings in Postgres, under the `person_settings` schema,
//! shared by every hub instance.

use async_trait::async_trait;
use eyre::{Context, Result};
use uuid::Uuid;
use workforce_common::personal_route::{
    AgentRoute, PersonalRoute, PersonalRouteKind, RouteFallback,
};
use workforce_postgres::{
    MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
};

use super::{PersonRouteStore, RouteChange, RouteSettings};

// Own directory, never a subdir of another embed root — refinery's
// embed_migrations is recursive.
mod embedded {
    refinery::embed_migrations!("migrations_personal_routes");
}

struct Migrations;

#[async_trait]
impl MigrationRun for Migrations {
    async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
        workforce_postgres::run_embedded(embedded::migrations::runner(), client).await?;
        Ok(())
    }
}

const SEARCH_PATH: &str = "SET search_path TO person_settings, public";

pub struct PostgresRouteStore {
    pool: PostgresPool,
}

fn kind(value: &str) -> Result<PersonalRouteKind> {
    PersonalRouteKind::ALL
        .into_iter()
        .find(|kind| kind.as_str() == value)
        .ok_or_else(|| eyre::eyre!("unknown route kind {value:?}"))
}

fn fallback(value: &str) -> Result<RouteFallback> {
    [RouteFallback::Wait, RouteFallback::Hub]
        .into_iter()
        .find(|fallback| fallback.as_str() == value)
        .ok_or_else(|| eyre::eyre!("unknown route fallback {value:?}"))
}

fn route(kind_value: &str, fallback_value: &str) -> Result<PersonalRoute> {
    Ok(PersonalRoute {
        kind: kind(kind_value)?,
        fallback: fallback(fallback_value)?,
    })
}

fn agent_route(row: &tokio_postgres::Row) -> Result<AgentRoute> {
    match row.get::<_, &str>("llm_route") {
        "hub" => Ok(AgentRoute::Hub),
        _ => Ok(AgentRoute::Personal(route(
            row.get::<_, Option<&str>>("llm_kind").unwrap_or_default(),
            row.get::<_, Option<&str>>("llm_fallback")
                .unwrap_or_default(),
        )?)),
    }
}

fn as_json(route: Option<AgentRoute>) -> Option<String> {
    route.map(|route| serde_json::to_string(&route).unwrap_or_default())
}

fn from_json(value: Option<String>) -> Option<AgentRoute> {
    value.and_then(|value| {
        serde_json::from_str(&value)
            .inspect_err(|e| tracing::warn!(target: "llm_route", error = %e, "an audit row's route can't be read; shown as none"))
            .ok()
    })
}

impl PostgresRouteStore {
    pub const SCHEMA: &'static str = "person_settings";

    pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
        PostgresPool::from_config_with_session_init(&config.small(), &[SEARCH_PATH])
    }

    pub async fn new(config: &PostgresConfig) -> Result<Self> {
        let pool = Self::build_pool(config).context("building person settings pool")?;
        Self::with_pool(pool).await
    }

    pub async fn with_pool(pool: PostgresPool) -> Result<Self> {
        MigrationRunner::run(
            &pool,
            &[MigrationSet::new(
                Self::SCHEMA,
                Box::new(Migrations) as Box<dyn MigrationRun>,
            )],
        )
        .await
        .context("running person settings migrations")?;
        Ok(Self { pool })
    }
}

const NOTE_CHANGE: &str =
    "INSERT INTO llm_route_changes (identity_id, agent_id, before, after, actor) \
     VALUES ($1, $2, $3, $4, $5)";

#[async_trait]
impl PersonRouteStore for PostgresRouteStore {
    async fn settings(&self, person: Uuid, agent_id: Option<&str>) -> Result<RouteSettings> {
        let conn = self.pool.get().await?;
        let person = person.to_string();
        let every_agent = conn
            .query_opt(
                "SELECT llm_kind, llm_fallback FROM user_settings WHERE identity_id = $1",
                &[&person],
            )
            .await?
            .map(|row| route(row.get("llm_kind"), row.get("llm_fallback")))
            .transpose()?;
        let this_agent = match agent_id {
            Some(agent_id) => conn
                .query_opt(
                    "SELECT llm_route, llm_kind, llm_fallback FROM user_agent_settings \
                     WHERE identity_id = $1 AND agent_id = $2",
                    &[&person, &agent_id],
                )
                .await?
                .map(|row| agent_route(&row))
                .transpose()?,
            None => None,
        };
        Ok(RouteSettings {
            every_agent,
            this_agent,
        })
    }

    async fn set_every_agent(
        &self,
        person: Uuid,
        route_now: Option<PersonalRoute>,
        actor: &str,
    ) -> Result<Option<PersonalRoute>> {
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        let person = person.to_string();
        let before = tx
            .query_opt(
                "SELECT llm_kind, llm_fallback FROM user_settings WHERE identity_id = $1 FOR UPDATE",
                &[&person],
            )
            .await?
            .map(|row| route(row.get("llm_kind"), row.get("llm_fallback")))
            .transpose()?;
        match route_now {
            Some(r) => {
                tx.execute(
                    "INSERT INTO user_settings (identity_id, llm_kind, llm_fallback, updated_at) \
                     VALUES ($1, $2, $3, now()) \
                     ON CONFLICT (identity_id) DO UPDATE SET llm_kind = EXCLUDED.llm_kind, \
                         llm_fallback = EXCLUDED.llm_fallback, updated_at = EXCLUDED.updated_at",
                    &[&person, &r.kind.as_str(), &r.fallback.as_str()],
                )
                .await?;
            }
            None => {
                tx.execute(
                    "DELETE FROM user_settings WHERE identity_id = $1",
                    &[&person],
                )
                .await?;
            }
        }
        tx.execute(
            NOTE_CHANGE,
            &[
                &person,
                &None::<String>,
                &as_json(before.map(AgentRoute::Personal)),
                &as_json(route_now.map(AgentRoute::Personal)),
                &actor,
            ],
        )
        .await?;
        tx.commit().await?;
        Ok(before)
    }

    async fn set_for_agent(
        &self,
        person: Uuid,
        agent_id: &str,
        route_now: Option<AgentRoute>,
        actor: &str,
    ) -> Result<Option<AgentRoute>> {
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        let person = person.to_string();
        let before = tx
            .query_opt(
                "SELECT llm_route, llm_kind, llm_fallback FROM user_agent_settings \
                 WHERE identity_id = $1 AND agent_id = $2 FOR UPDATE",
                &[&person, &agent_id],
            )
            .await?
            .map(|row| agent_route(&row))
            .transpose()?;
        match route_now {
            Some(r) => {
                let (word, kind, fallback) = match r {
                    AgentRoute::Hub => ("hub", None, None),
                    AgentRoute::Personal(p) => {
                        ("personal", Some(p.kind.as_str()), Some(p.fallback.as_str()))
                    }
                };
                tx.execute(
                    "INSERT INTO user_agent_settings \
                         (identity_id, agent_id, llm_route, llm_kind, llm_fallback, updated_at) \
                     VALUES ($1, $2, $3, $4, $5, now()) \
                     ON CONFLICT (identity_id, agent_id) DO UPDATE SET \
                         llm_route = EXCLUDED.llm_route, llm_kind = EXCLUDED.llm_kind, \
                         llm_fallback = EXCLUDED.llm_fallback, updated_at = EXCLUDED.updated_at",
                    &[&person, &agent_id, &word, &kind, &fallback],
                )
                .await?;
            }
            None => {
                tx.execute(
                    "DELETE FROM user_agent_settings WHERE identity_id = $1 AND agent_id = $2",
                    &[&person, &agent_id],
                )
                .await?;
            }
        }
        tx.execute(
            NOTE_CHANGE,
            &[
                &person,
                &Some(agent_id),
                &as_json(before),
                &as_json(route_now),
                &actor,
            ],
        )
        .await?;
        tx.commit().await?;
        Ok(before)
    }

    async fn changes(&self, person: Uuid, limit: usize) -> Result<Vec<RouteChange>> {
        let conn = self.pool.get().await?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = conn
            .query(
                "SELECT agent_id, before, after, actor, at FROM llm_route_changes \
                 WHERE identity_id = $1 ORDER BY at DESC, id DESC LIMIT $2",
                &[&person.to_string(), &limit],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| RouteChange {
                agent_id: row.get("agent_id"),
                before: from_json(row.get("before")),
                after: from_json(row.get("after")),
                actor: row.get("actor"),
                at: row.get("at"),
            })
            .collect())
    }
}
