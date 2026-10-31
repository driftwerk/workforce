//! Backend resolver for the self-learning store.
//!
//! Single source of truth for "open a `SelfLearningStore` from
//! `WorkforceConfig`", mirroring [`crate::training::backend`]: the SQLite
//! vs Postgres decision lives in one place so `AgentService::build` and any
//! later CLI/direct caller delegate here.

use std::path::PathBuf;
use std::sync::Arc;

#[cfg(feature = "postgres")]
use eyre::Context;
use eyre::Result;
use workforce_common::backend::{resolve_optional_store_backend, OptionalStoreBackend};
use workforce_common::postgres::PostgresConfig;
use workforce_common::settings::{SelfLearningConfig, WorkforceConfig};
use workforce_self_learning::{DeepExpertiseStore, SelfLearningStore};

/// One concrete store viewed through both curation traits. The backend builds a
/// single concrete object (one DB connection) and hands out two trait-object
/// views of it: the flat atomic-item store and the deep-expertise tree store.
/// Opening twice would mean two connections to the same DB and two lazy-open
/// lifecycles to keep in sync, so it never does.
#[derive(Clone)]
pub struct CurationStores {
    pub self_learning: Arc<dyn SelfLearningStore>,
    pub deep_expertise: Arc<dyn DeepExpertiseStore>,
}

/// Pure resolution, shared with every other store: which backend, given the
/// config? Fails closed — `[self_learning] backend = "postgres"` errors when
/// the `[postgres]` pool isn't enabled rather than silently using SQLite.
pub fn select_backend(
    cfg: &SelfLearningConfig,
    postgres: Option<&PostgresConfig>,
) -> Result<OptionalStoreBackend> {
    resolve_optional_store_backend(
        "self_learning",
        cfg.enabled,
        &cfg.backend,
        postgres.is_some_and(|p| p.enabled),
    )
}

/// Open the configured self-learning store from the global config. `None`
/// when self-learning is disabled, or when its SQLite init fails (kept as the
/// "unavailable this run" outcome for an optional feature). A configured
/// Postgres backend that can't be reached propagates the error. Projects the
/// atomic-item view out of [`CurationStores`] for the flat-item callers.
pub async fn open_self_learning_store(
    wf: &WorkforceConfig,
) -> Result<Option<Arc<dyn SelfLearningStore>>> {
    Ok(open_effective(&wf.self_learning, wf)
        .await?
        .map(|s| s.self_learning))
}

/// Open the store from an EFFECTIVE (per-agent → global resolved)
/// self-learning config, drawing the data_dir / postgres pool from the global
/// `wf`. This is what makes global-off + per-agent-on work: the per-agent
/// resolved `cfg.enabled` is `true`, so the backend resolves a store even
/// though `wf.self_learning.enabled` is `false`. Returns both trait-object
/// views of the one concrete store (see [`CurationStores`]).
pub async fn open_effective(
    cfg: &SelfLearningConfig,
    wf: &WorkforceConfig,
) -> Result<Option<CurationStores>> {
    Ok(match select_backend(cfg, wf.postgres.as_ref())? {
        OptionalStoreBackend::Disabled => None,
        OptionalStoreBackend::Postgres => Some(open_postgres(wf).await?),
        OptionalStoreBackend::Sqlite => open_sqlite(cfg, wf),
    })
}

fn open_sqlite(cfg: &SelfLearningConfig, wf: &WorkforceConfig) -> Option<CurationStores> {
    let path = resolve_sqlite_path(cfg, wf);
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent)
            .and_then(|()| workforce_common::private_fs::ensure_private_sqlite(&path))
        {
            tracing::warn!(
                path = %parent.display(),
                error = %e,
                "Self-learning store: failed to create its directory or database file; self-learning disabled this run",
            );
            return None;
        }
    }
    match workforce_self_learning::sqlite::SqliteSelfLearningStore::new(&path) {
        Ok(store) => {
            tracing::info!(path = %path.display(), "Self-learning store opened (sqlite)");
            let store = Arc::new(store);
            Some(CurationStores {
                self_learning: store.clone(),
                deep_expertise: store,
            })
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Self-learning store init failed; self-learning disabled this run",
            );
            None
        }
    }
}

/// The process's Postgres self-learning store: the hub's agents share it,
/// where each agent built (and each hot-enable) opened a pool of its own.
#[cfg(feature = "postgres")]
static POSTGRES_STORES: tokio::sync::OnceCell<CurationStores> = tokio::sync::OnceCell::const_new();

#[cfg(feature = "postgres")]
async fn open_postgres(wf: &WorkforceConfig) -> Result<CurationStores> {
    POSTGRES_STORES
        .get_or_try_init(|| async {
            let pg = wf
                .postgres
                .as_ref()
                .filter(|p| p.enabled)
                .ok_or_else(|| eyre::eyre!("[postgres] not configured"))?;
            let store = workforce_self_learning::postgres::PostgresSelfLearningStore::new(pg)
                .await
                .context("constructing PostgresSelfLearningStore")?;
            tracing::info!("Self-learning store opened (postgres)");
            Ok::<_, eyre::Report>(CurationStores {
                self_learning: store.clone(),
                deep_expertise: store,
            })
        })
        .await
        .cloned()
}

#[cfg(not(feature = "postgres"))]
async fn open_postgres(_wf: &WorkforceConfig) -> Result<CurationStores> {
    Err(eyre::eyre!(
        "Postgres self-learning backend not compiled in (rebuild with --features postgres)"
    ))
}

/// Resolve the SQLite DB path, joining against `[hub].data_dir` when the
/// configured `db_path` is relative.
pub(crate) fn resolve_sqlite_path(cfg: &SelfLearningConfig, wf: &WorkforceConfig) -> PathBuf {
    let raw = std::path::Path::new(&cfg.db_path);
    if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        std::path::Path::new(&wf.hub.data_dir).join(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool, backend: &str) -> SelfLearningConfig {
        SelfLearningConfig {
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
    fn select_backend_disabled_when_off() {
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
