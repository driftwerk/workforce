//! Pictures in Postgres, under the `pictures` schema.

use std::collections::HashMap;

use async_trait::async_trait;
use eyre::{Context, Result};
use workforce_postgres::{
    MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
};

use super::{Image, Kept, Kind, Owner, Picture, PictureStore, SlackPicture, Source};

// Own directory, never a subdir of another embed root — refinery's
// embed_migrations is recursive.
mod embedded {
    refinery::embed_migrations!("migrations_pictures");
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

const SEARCH_PATH: &str = "SET search_path TO pictures, public";

pub struct PostgresPictures {
    pool: PostgresPool,
}

/// The table and id column of an owner kind's pictures.
const fn table(kind: Kind) -> (&'static str, &'static str) {
    match kind {
        Kind::Agent => ("agent_pictures", "agent_id"),
        Kind::Identity => ("identity_pictures", "identity_id"),
    }
}

fn kept(row: &tokio_postgres::Row) -> Result<Kept> {
    let source: String = row.get("source");
    Ok(Kept {
        source: Source::parse(&source)
            .ok_or_else(|| eyre::eyre!("unknown picture source {source:?}"))?,
        etag: row.get("etag"),
        at: row.get("at"),
        origin: row.get("origin"),
        workspace: row.get("workspace"),
    })
}

impl PostgresPictures {
    pub const SCHEMA: &'static str = "pictures";

    pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
        PostgresPool::from_config_with_session_init(&config.small(), &[SEARCH_PATH])
    }

    pub async fn new(config: &PostgresConfig) -> Result<Self> {
        let pool = Self::build_pool(config).context("building picture store pool")?;
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
        .context("running picture migrations")?;
        Ok(Self { pool })
    }

    async fn put(
        &self,
        owner: &Owner,
        source: Source,
        image: &Image,
        origin: Option<&str>,
        workspace: Option<&str>,
    ) -> Result<()> {
        let (table, id) = table(owner.kind);
        let conn = self.pool.get().await?;
        conn.execute(
            &format!(
                "INSERT INTO {table} ({id}, source, media_type, bytes, etag, origin, workspace, at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, now()) \
                 ON CONFLICT ({id}, source) DO UPDATE SET media_type = EXCLUDED.media_type, \
                     bytes = EXCLUDED.bytes, etag = EXCLUDED.etag, origin = EXCLUDED.origin, \
                     workspace = EXCLUDED.workspace, at = EXCLUDED.at"
            ),
            &[
                &owner.id,
                &source.as_str(),
                &image.media_type,
                &image.bytes,
                &image.etag,
                &origin,
                &workspace,
            ],
        )
        .await?;
        Ok(())
    }
}

#[async_trait]
impl PictureStore for PostgresPictures {
    async fn shown(&self, owner: &Owner) -> Result<Option<Picture>> {
        let (table, id) = table(owner.kind);
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                &format!(
                    "SELECT source, media_type, bytes, etag FROM {table} WHERE {id} = $1 \
                     ORDER BY source = 'chosen' DESC LIMIT 1"
                ),
                &[&owner.id],
            )
            .await?;
        row.map(|row| {
            let source: String = row.get("source");
            Ok(Picture {
                source: Source::parse(&source)
                    .ok_or_else(|| eyre::eyre!("unknown picture source {source:?}"))?,
                image: Image {
                    media_type: row.get("media_type"),
                    bytes: row.get("bytes"),
                    etag: row.get("etag"),
                },
            })
        })
        .transpose()
    }

    async fn kept(&self, kind: Kind) -> Result<HashMap<String, Vec<Kept>>> {
        let (table, id) = table(kind);
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                &format!("SELECT {id} AS owner, source, etag, at, origin, workspace FROM {table}"),
                &[],
            )
            .await?;
        let mut out: HashMap<String, Vec<Kept>> = HashMap::new();
        for row in &rows {
            out.entry(row.get("owner")).or_default().push(kept(row)?);
        }
        Ok(out)
    }

    async fn kept_for(&self, owner: &Owner) -> Result<Vec<Kept>> {
        let (table, id) = table(owner.kind);
        let conn = self.pool.get().await?;
        conn.query(
            &format!("SELECT source, etag, at, origin, workspace FROM {table} WHERE {id} = $1"),
            &[&owner.id],
        )
        .await?
        .iter()
        .map(kept)
        .collect()
    }

    async fn choose(&self, owner: &Owner, image: &Image) -> Result<()> {
        self.put(owner, Source::Chosen, image, None, None).await
    }

    async fn put_slack(&self, owner: &Owner, picture: &SlackPicture) -> Result<()> {
        self.put(
            owner,
            Source::Slack,
            &picture.image,
            Some(&picture.origin),
            Some(&picture.workspace),
        )
        .await
    }

    async fn remove(&self, owner: &Owner, source: Source) -> Result<bool> {
        let (table, id) = table(owner.kind);
        let conn = self.pool.get().await?;
        let removed = conn
            .execute(
                &format!("DELETE FROM {table} WHERE {id} = $1 AND source = $2"),
                &[&owner.id, &source.as_str()],
            )
            .await?;
        Ok(removed > 0)
    }

    async fn forget(&self, owner: &Owner) -> Result<u64> {
        let (table, id) = table(owner.kind);
        let conn = self.pool.get().await?;
        Ok(conn
            .execute(
                &format!("DELETE FROM {table} WHERE {id} = $1"),
                &[&owner.id],
            )
            .await?)
    }

    async fn hand_over(&self, from: &Owner, to: &Owner) -> Result<bool> {
        if from.kind != to.kind {
            eyre::bail!("a picture can't go from {from} to {to}");
        }
        let (table, id) = table(from.kind);
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        let moved = tx
            .execute(
                &format!(
                    "UPDATE {table} SET {id} = $2 WHERE {id} = $1 AND source = 'chosen' \
                     AND NOT EXISTS (SELECT 1 FROM {table} WHERE {id} = $2 AND source = 'chosen')"
                ),
                &[&from.id, &to.id],
            )
            .await?;
        tx.execute(&format!("DELETE FROM {table} WHERE {id} = $1"), &[&from.id])
            .await?;
        tx.commit().await?;
        Ok(moved > 0)
    }
}
