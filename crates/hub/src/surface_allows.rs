//! The surface gate's allow list: the writes a person has said an agent may
//! always make from their Desk turns without asking, kept per person, agent,
//! tool, platform and destination in its own table (schema `surface`), apart
//! from the permission system so it holds with that off.
//! A person sees and revokes their own through `GET /api/desk/allows` and
//! `DELETE /api/desk/allows/{id}`.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_postgres::Row;
use tracing::warn;
use uuid::Uuid;
use workforce_agent::tools::dispatch::SurfaceAllows;
use workforce_agent::tools::Destination;
use workforce_common::settings::WorkforceConfig;
use workforce_postgres::{
    MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
};

use crate::gateway::{resolve_caller_context, AuthFailure, GatewayState};

// Own directory, never a subdir of another embed root: refinery's
// embed_migrations is recursive.
mod embedded {
    refinery::embed_migrations!("migrations_surface");
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

/// One tool's writes to one destination a person always allows an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Allow {
    pub id: Uuid,
    pub agent_id: String,
    pub tool: String,
    pub platform: String,
    pub destination: String,
    pub created_at: DateTime<Utc>,
}

fn allow_of(row: &Row) -> Result<Allow> {
    Ok(Allow {
        id: row.try_get("id")?,
        agent_id: row.try_get("agent_id")?,
        tool: row.try_get("tool")?,
        platform: row.try_get("platform")?,
        destination: row.try_get("destination")?,
        created_at: row.try_get("created_at")?,
    })
}

pub struct PgSurfaceAllows {
    pool: PostgresPool,
}

impl PgSurfaceAllows {
    pub const SCHEMA: &'static str = "surface";

    pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
        PostgresPool::from_config_with_session_init(
            &config.small(),
            &["SET search_path TO surface, public"],
        )
    }

    pub async fn new(config: &PostgresConfig) -> Result<Self> {
        let pool = Self::build_pool(config).context("building the surface allows pool")?;
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
        .context("running the surface allows migrations")?;
        Ok(Self { pool })
    }

    /// Whether `person` always allows `agent` to write to `to` with `tool`.
    pub async fn allowed(
        &self,
        person: &str,
        agent: &str,
        tool: &str,
        to: &Destination,
    ) -> Result<bool> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                "SELECT 1 FROM allows \
                 WHERE person = $1 AND agent_id = $2 AND tool = $3 \
                   AND platform = $4 AND destination = $5",
                &[&person, &agent, &tool, &to.platform, &to.target],
            )
            .await?;
        Ok(row.is_some())
    }

    /// `person` always allows `agent` to write to `to` with `tool` from now
    /// on.
    pub async fn keep(
        &self,
        person: &str,
        agent: &str,
        tool: &str,
        to: &Destination,
    ) -> Result<()> {
        let conn = self.pool.get().await?;
        conn.execute(
            "INSERT INTO allows (id, person, agent_id, tool, platform, destination, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, now()) \
             ON CONFLICT (person, agent_id, tool, platform, destination) DO NOTHING",
            &[
                &Uuid::new_v4(),
                &person,
                &agent,
                &tool,
                &to.platform,
                &to.target,
            ],
        )
        .await?;
        Ok(())
    }

    /// What `person` always allows, newest first.
    pub async fn of(&self, person: &str) -> Result<Vec<Allow>> {
        let conn = self.pool.get().await?;
        conn.query(
            "SELECT id, agent_id, tool, platform, destination, created_at FROM allows \
             WHERE person = $1 ORDER BY created_at DESC",
            &[&person],
        )
        .await?
        .iter()
        .map(allow_of)
        .collect()
    }

    /// Withdraws `person`'s allow `id`; whether it was theirs to withdraw.
    pub async fn revoke(&self, person: &str, id: Uuid) -> Result<bool> {
        let conn = self.pool.get().await?;
        let removed = conn
            .execute(
                "DELETE FROM allows WHERE id = $1 AND person = $2",
                &[&id, &person],
            )
            .await?;
        Ok(removed == 1)
    }

    /// For everyone with allows, how many.
    pub async fn holders(&self) -> Result<std::collections::BTreeMap<String, usize>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query("SELECT person, count(*) FROM allows GROUP BY person", &[])
            .await?;
        Ok(rows
            .iter()
            .map(|row| {
                let held: i64 = row.get(1);
                (row.get(0), usize::try_from(held).unwrap_or_default())
            })
            .collect())
    }

    /// Withdraws every allow of `person`'s; how many went.
    pub async fn forget(&self, person: &str) -> Result<u64> {
        let conn = self.pool.get().await?;
        Ok(conn
            .execute("DELETE FROM allows WHERE person = $1", &[&person])
            .await?)
    }

    /// Withdraws every allow anyone kept for `agent`; how many went.
    pub async fn forget_agent(&self, agent: &str) -> Result<u64> {
        let conn = self.pool.get().await?;
        Ok(conn
            .execute("DELETE FROM allows WHERE agent_id = $1", &[&agent])
            .await?)
    }

    fn block_on<T>(&self, fut: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
    }
}

impl SurfaceAllows for PgSurfaceAllows {
    /// A lookup that fails is no allow: the write is asked about.
    fn allows(&self, person: &str, agent: &str, tool: &str, to: &Destination) -> bool {
        self.block_on(self.allowed(person, agent, tool, to))
            .inspect_err(|e| warn!(error = %format!("{e:#}"), "reading the surface allows failed"))
            .unwrap_or(false)
    }

    fn allow(&self, person: &str, agent: &str, tool: &str, to: &Destination) -> Result<()> {
        self.block_on(self.keep(person, agent, tool, to))
    }
}

static INSTALLED: OnceLock<Arc<PgSurfaceAllows>> = OnceLock::new();

/// The hub's allow list, once [`install`] opened it.
pub fn installed() -> Option<Arc<dyn SurfaceAllows>> {
    store().map(|store| store as Arc<dyn SurfaceAllows>)
}

/// The hub's allow list as its store, for what reads or changes it by person.
pub fn store() -> Option<Arc<PgSurfaceAllows>> {
    INSTALLED.get().cloned()
}

/// Opens the allow list once when the hub has Postgres.
pub async fn install(config: &WorkforceConfig) {
    static OPENED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    OPENED
        .get_or_init(|| async {
            let Some(pg) = config.postgres.as_ref().filter(|p| p.enabled) else {
                return;
            };
            match PgSurfaceAllows::new(pg).await {
                Ok(store) => {
                    let _ = INSTALLED.set(Arc::new(store));
                }
                Err(e) => warn!(
                    error = %format!("{e:#}"),
                    "surface allows unavailable; Always allow is not offered for writes off Desk"
                ),
            }
        })
        .await;
}

struct Routes {
    state: Arc<GatewayState>,
    store: Option<Arc<PgSurfaceAllows>>,
}

type S = State<Arc<Routes>>;

/// `GET /api/desk/allows` and `DELETE /api/desk/allows/{id}`: the caller's
/// own allows, answering 404 where the hub keeps none.
pub fn router(state: Arc<GatewayState>, store: Option<Arc<PgSurfaceAllows>>) -> Router {
    Router::new()
        .route("/api/desk/allows", get(list))
        .route("/api/desk/allows/{id}", delete(revoke))
        .with_state(Arc::new(Routes { state, store }))
}

/// The hub's own routes, over its installed list.
pub fn routes(state: Arc<GatewayState>) -> Router {
    router(state, store())
}

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

fn refusal(status: StatusCode, error: &str) -> Response {
    (status, Json(json!({ "error": error }))).into_response()
}

/// The person asking, as their Desk turns run (their identity, else their
/// paired device), and the list, or why there is neither.
async fn reader(
    routes: &Routes,
    headers: &HeaderMap,
    token: Option<&str>,
) -> Result<(String, Arc<PgSurfaceAllows>), Response> {
    let caller = resolve_caller_context(&routes.state, headers, token).await;
    let person = match caller.auth_failure {
        Some(AuthFailure::InvalidCredentials) => None,
        Some(AuthFailure::StoreError) => {
            return Err(refusal(StatusCode::SERVICE_UNAVAILABLE, "unavailable"))
        }
        None => caller.identity_id.or(caller.device_id),
    }
    .ok_or_else(|| refusal(StatusCode::UNAUTHORIZED, "unauthorized"))?;
    let store = routes
        .store
        .clone()
        .ok_or_else(|| refusal(StatusCode::NOT_FOUND, "no_allows"))?;
    Ok((person, store))
}

fn failed(e: eyre::Report) -> Response {
    warn!(error = %format!("{e:#}"), "the surface allows failed");
    refusal(StatusCode::INTERNAL_SERVER_ERROR, "unavailable")
}

async fn list(State(r): S, headers: HeaderMap, Query(q): Query<TokenQuery>) -> Response {
    let (person, store) = match reader(&r, &headers, q.token.as_deref()).await {
        Ok(found) => found,
        Err(refused) => return refused,
    };
    match store.of(&person).await {
        Ok(allows) => Json(json!({ "allows": allows })).into_response(),
        Err(e) => failed(e),
    }
}

async fn revoke(
    State(r): S,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(q): Query<TokenQuery>,
) -> Response {
    let (person, store) = match reader(&r, &headers, q.token.as_deref()).await {
        Ok(found) => found,
        Err(refused) => return refused,
    };
    match store.revoke(&person, id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => refusal(StatusCode::NOT_FOUND, "unknown"),
        Err(e) => failed(e),
    }
}

#[cfg(test)]
mod tests {
    use workforce_postgres::test_support::with_test_pool;

    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn an_allow_is_one_persons_for_one_agent_tool_and_destination_until_revoked() {
        with_test_pool(PgSurfaceAllows::build_pool, |pool| async move {
            let store = PgSurfaceAllows::with_pool(pool).await.unwrap();
            let ops = Destination::of("slack", "#ops").unwrap();
            let other = Destination::of("slack", "#general").unwrap();
            let send = "slack.send";
            assert!(!store.allows("alice", "scout", send, &ops));
            store.allow("alice", "scout", send, &ops).unwrap();
            store.allow("alice", "scout", send, &ops).unwrap();
            assert!(store.allows("alice", "scout", send, &ops));
            assert!(
                !store.allows("alice", "scout", "slack.archive_channel", &ops),
                "another tool"
            );
            assert!(
                !store.allows("alice", "scout", send, &other),
                "another destination"
            );
            assert!(!store.allows("alice", "atlas", send, &ops), "another agent");
            assert!(!store.allows("bob", "scout", send, &ops), "another person");
            let kept = store.of("alice").await.unwrap();
            assert_eq!(kept.len(), 1, "kept once");
            assert_eq!(
                (
                    kept[0].agent_id.as_str(),
                    kept[0].tool.as_str(),
                    kept[0].platform.as_str(),
                    kept[0].destination.as_str()
                ),
                ("scout", send, "slack", "ops")
            );
            assert!(store.of("bob").await.unwrap().is_empty());
            assert!(
                !store.revoke("bob", kept[0].id).await.unwrap(),
                "not bob's to revoke"
            );
            assert!(store.allows("alice", "scout", send, &ops));
            assert!(store.revoke("alice", kept[0].id).await.unwrap());
            assert!(!store.allows("alice", "scout", send, &ops));
            store.allow("alice", "scout", send, &ops).unwrap();
            store.allow("alice", "atlas", send, &other).unwrap();
            store.allow("bob", "scout", send, &ops).unwrap();
            assert_eq!(
                store.holders().await.unwrap(),
                std::collections::BTreeMap::from([
                    ("alice".to_string(), 2),
                    ("bob".to_string(), 1)
                ])
            );
            assert_eq!(store.forget("alice").await.unwrap(), 2);
            assert!(store.of("alice").await.unwrap().is_empty());
            assert!(store.allows("bob", "scout", send, &ops), "another's stay");
            store.allow("bob", "atlas", send, &ops).unwrap();
            assert_eq!(store.forget_agent("scout").await.unwrap(), 1);
            assert!(
                !store.allows("bob", "scout", send, &ops),
                "the agent's went"
            );
            assert!(
                store.allows("bob", "atlas", send, &ops),
                "another agent's stay"
            );
        })
        .await;
    }
}
