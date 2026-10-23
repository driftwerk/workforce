//! Shadow mode's place in the hub (DW-676): the Postgres its comparisons
//! are kept in, and the one sink each hub writes them through.

use std::sync::Arc;

use eyre::{eyre, Context, Result};
use workforce_common::postgres::PostgresConfig;
use workforce_common::settings::WorkforceConfig;
use workforce_common::shadow_postgres::{PostgresShadowSink, ShadowStore, POOL_SIZE};
use workforce_common::telemetry_postgres::PostgresTelemetryStore;

/// The shadow tables, when a store is shadowed: `None` otherwise, and then
/// nothing is opened. Config refuses `"shadow"` without `[postgres]
/// enabled`; a config built in code is refused here too.
pub async fn store(config: &WorkforceConfig) -> Result<Option<Arc<ShadowStore>>> {
    let shadowed = config.shadowed();
    let Some(setting) = shadowed.first() else {
        return Ok(None);
    };
    let pg = config
        .postgres
        .as_ref()
        .filter(|p| p.enabled)
        .ok_or_else(|| eyre!("{setting} = \"shadow\" needs [postgres] enabled = true"))?;
    let pool = PostgresTelemetryStore::build_pool(&pool_config(pg))
        .context("building the shadow tables' pool")?;
    Ok(Some(ShadowStore::with_pool(pool).await?))
}

/// Comparisons a hub's sink holds that are not written yet: about a
/// minute of a busy hub's searches. Beyond it they are dropped and
/// counted.
pub const SINK_CAPACITY: usize = 1024;
/// How long one batch's insert may take before Postgres gives it up.
pub const SINK_INSERT_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

static SHARED: tokio::sync::OnceCell<Arc<ShadowStore>> = tokio::sync::OnceCell::const_new();
static SINK: tokio::sync::OnceCell<Arc<PostgresShadowSink>> = tokio::sync::OnceCell::const_new();

/// As [`store`], opened once per process and shared: by retention and by
/// the sink, over one small pool.
pub async fn shared_store(config: &WorkforceConfig) -> Result<Option<Arc<ShadowStore>>> {
    if config.shadowed().is_empty() {
        return Ok(None);
    }
    let store = SHARED
        .get_or_try_init(|| async {
            store(config)
                .await?
                .ok_or_else(|| eyre!("nothing is shadowed"))
        })
        .await?;
    Ok(Some(Arc::clone(store)))
}

/// The sink every shadow of this hub writes its comparisons through, made
/// once: `Err` when nothing is shadowed or the shadow tables cannot be
/// opened.
pub async fn sink(config: &WorkforceConfig) -> Result<Arc<PostgresShadowSink>> {
    let sink = SINK
        .get_or_try_init(|| async {
            let store = shared_store(config)
                .await?
                .ok_or_else(|| eyre!("nothing is shadowed, so there is nothing to compare"))?;
            eyre::Ok(store.sink(SINK_CAPACITY, SINK_INSERT_LIMIT))
        })
        .await?;
    Ok(Arc::clone(sink))
}

/// [`sink`] opened on its own, for a caller that must not wait for it.
pub fn sink_for(
    config: &WorkforceConfig,
) -> impl std::future::Future<Output = Result<Arc<dyn workforce_common::shadow::ShadowSink>>>
       + Send
       + 'static {
    let config = config.clone();
    async move {
        let sink: Arc<dyn workforce_common::shadow::ShadowSink> = sink(&config).await?;
        Ok(sink)
    }
}

/// The shadow tables' pool: of its own, so that a hung shadow table ties
/// up none of the others' connections, and small ([`POOL_SIZE`]), so that
/// the hub's total stays within production's `max_connections`.
fn pool_config(pg: &PostgresConfig) -> PostgresConfig {
    PostgresConfig {
        pool_size: POOL_SIZE,
        ..pg.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shadow tables' pool is the hub's Postgres, with two connections
    /// whatever the pool size the other stores are given.
    #[test]
    fn the_shadow_pool_is_small() {
        let pg = PostgresConfig {
            enabled: true,
            pool_size: 16,
            url: Some("postgres://hub@db:5432/workforce".into()),
            ..PostgresConfig::default()
        };
        let shadow = pool_config(&pg);
        assert_eq!(shadow.pool_size, 2);
        assert_eq!(shadow.url, pg.url);
    }

    /// Nothing shadowed, nothing opened; shadowed without Postgres, refused
    /// by name.
    #[tokio::test]
    async fn the_shadow_tables_are_opened_only_for_a_shadowed_store() {
        assert!(store(&WorkforceConfig::default()).await.unwrap().is_none());
        let mut config = WorkforceConfig::default();
        config.artifacts.index_backend = "shadow".into();
        let refused = store(&config).await.err().expect("refused").to_string();
        assert!(
            refused.contains("[artifacts] index_backend = \"shadow\" needs [postgres] enabled"),
            "{refused}"
        );
    }
}
