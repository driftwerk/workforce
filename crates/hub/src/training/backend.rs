//! Backend resolver for the training store.
//!
//! Single source of truth for "open a `TrainingStore` from
//! `WorkforceConfig`". Both `AgentService::build` (hub) and the
//! `workforce training` CLI direct mode delegate here so the SQLite
//! vs Postgres decision lives in one place.

use std::path::PathBuf;
use std::sync::Arc;

#[cfg(feature = "postgres")]
use eyre::Context;
use eyre::Result;
use workforce_common::backend::{resolve_optional_store_backend, OptionalStoreBackend};
use workforce_common::postgres::PostgresConfig;
use workforce_common::settings::{TrainingConfig, WorkforceConfig};
use workforce_training::TrainingStore;

/// Pure resolution, shared with every other store: which backend, given the
/// config? Fails closed — `[training] backend = "postgres"` errors when the
/// `[postgres]` pool isn't enabled rather than silently using SQLite.
pub fn select_backend(
    training: &TrainingConfig,
    postgres: Option<&PostgresConfig>,
) -> Result<OptionalStoreBackend> {
    resolve_optional_store_backend(
        "training",
        training.enabled,
        &training.backend,
        postgres.is_some_and(|p| p.enabled),
    )
}

/// Open the configured training store. `None` when training is disabled, or
/// when its SQLite init fails (kept as the "disabled this run" outcome for an
/// optional feature). A configured Postgres backend that can't be reached
/// propagates the error.
pub async fn open_training_store(wf: &WorkforceConfig) -> Result<Option<Arc<dyn TrainingStore>>> {
    Ok(match select_backend(&wf.training, wf.postgres.as_ref())? {
        OptionalStoreBackend::Disabled => None,
        OptionalStoreBackend::Postgres => Some(open_postgres(wf).await?),
        OptionalStoreBackend::Sqlite => open_sqlite(wf),
    })
}

fn open_sqlite(wf: &WorkforceConfig) -> Option<Arc<dyn TrainingStore>> {
    let path = resolve_sqlite_path(wf);
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent)
            .and_then(|()| workforce_common::private_fs::ensure_private_sqlite(&path))
        {
            tracing::warn!(
                path = %parent.display(),
                error = %e,
                "Training store: failed to create its directory or database file; agent training disabled this run",
            );
            return None;
        }
    }
    match workforce_training::sqlite::SqliteTrainingStore::new(&path) {
        Ok(store) => {
            tracing::info!(path = %path.display(), "Training store opened (sqlite)");
            Some(Arc::new(store) as Arc<dyn TrainingStore>)
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Training store init failed; agent training disabled this run",
            );
            None
        }
    }
}

#[cfg(feature = "postgres")]
async fn open_postgres(wf: &WorkforceConfig) -> Result<Arc<dyn TrainingStore>> {
    let pg = wf
        .postgres
        .as_ref()
        .filter(|p| p.enabled)
        .ok_or_else(|| eyre::eyre!("[postgres] not configured"))?;
    let store = workforce_training::postgres::PostgresTrainingStore::new(pg)
        .await
        .context("constructing PostgresTrainingStore")?;
    tracing::info!("Training store opened (postgres)");
    Ok(store as Arc<dyn TrainingStore>)
}

#[cfg(not(feature = "postgres"))]
async fn open_postgres(_wf: &WorkforceConfig) -> Result<Arc<dyn TrainingStore>> {
    Err(eyre::eyre!(
        "Postgres training backend not compiled in (rebuild with --features postgres)"
    ))
}

/// Resolve the SQLite training DB path, joining against `[hub].data_dir`
/// when the configured `db_path` is relative.
pub(crate) fn resolve_sqlite_path(wf: &WorkforceConfig) -> PathBuf {
    let raw = std::path::Path::new(&wf.training.db_path);
    if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        std::path::Path::new(&wf.hub.data_dir).join(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool, backend: &str) -> TrainingConfig {
        TrainingConfig {
            enabled,
            backend: backend.into(),
            ..Default::default()
        }
    }

    fn pg(enabled: bool) -> PostgresConfig {
        PostgresConfig {
            enabled,
            ..Default::default()
        }
    }

    #[test]
    fn select_backend_disabled_when_training_off() {
        assert_eq!(
            select_backend(&cfg(false, "sqlite"), None).unwrap(),
            OptionalStoreBackend::Disabled
        );
        assert_eq!(
            select_backend(&cfg(false, "postgres"), Some(&pg(true))).unwrap(),
            OptionalStoreBackend::Disabled,
        );
    }

    #[test]
    fn select_backend_postgres_when_pool_available() {
        assert_eq!(
            select_backend(&cfg(true, "postgres"), Some(&pg(true))).unwrap(),
            OptionalStoreBackend::Postgres,
        );
    }

    #[test]
    fn select_backend_fails_closed_when_pool_missing() {
        assert!(select_backend(&cfg(true, "postgres"), None).is_err());
        assert!(select_backend(&cfg(true, "postgres"), Some(&pg(false))).is_err());
    }

    #[test]
    fn select_backend_defaults_to_sqlite() {
        assert_eq!(
            select_backend(&cfg(true, "sqlite"), None).unwrap(),
            OptionalStoreBackend::Sqlite
        );
        assert_eq!(
            select_backend(&cfg(true, ""), None).unwrap(),
            OptionalStoreBackend::Sqlite
        );
        assert!(select_backend(&cfg(true, "unknown"), Some(&pg(true))).is_err());
    }
}
