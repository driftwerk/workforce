//! Fail-closed backend resolution shared by every store.
//!
//! A store is allowed to use SQLite/in-memory only when that is the
//! *configured* backend. When configuration names a real backend
//! (Postgres/Weaviate) the hub retries the connection with an adequate
//! budget and then fails startup — it never silently degrades to a lesser
//! store while reporting healthy.

use std::future::Future;
use std::time::{Duration, Instant};

use eyre::{bail, Result, WrapErr};

/// Backend a non-optional store must use, resolved fail-closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreBackend {
    Sqlite,
    Postgres,
}

/// Resolve a `[section] backend` string against whether Postgres can be
/// used here. A store explicitly set to `"postgres"` never downgrades to
/// SQLite: when Postgres is unavailable the caller gets an error and fails
/// closed.
pub fn resolve_store_backend(
    section: &str,
    backend: &str,
    postgres_available: bool,
) -> Result<StoreBackend> {
    match backend {
        "postgres" if postgres_available => Ok(StoreBackend::Postgres),
        "postgres" => bail!(
            "[{section}] backend = \"postgres\" but Postgres is unavailable; refusing to fall back \
             to SQLite. Bring Postgres up, or set [{section}] backend = \"sqlite\", then restart."
        ),
        "sqlite" | "" => Ok(StoreBackend::Sqlite),
        other => {
            bail!("[{section}] unknown backend {other:?}; expected \"postgres\" or \"sqlite\"")
        }
    }
}

/// Backend for an `enabled`-gated store. `Disabled` is a legitimate
/// "feature off" outcome, kept distinct from the configured-but-unavailable
/// error so a turned-off feature never aborts startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionalStoreBackend {
    Disabled,
    Sqlite,
    Postgres,
}

/// [`resolve_store_backend`] layered with an enable gate evaluated first.
pub fn resolve_optional_store_backend(
    section: &str,
    enabled: bool,
    backend: &str,
    postgres_available: bool,
) -> Result<OptionalStoreBackend> {
    if !enabled {
        return Ok(OptionalStoreBackend::Disabled);
    }
    Ok(
        match resolve_store_backend(section, backend, postgres_available)? {
            StoreBackend::Sqlite => OptionalStoreBackend::Sqlite,
            StoreBackend::Postgres => OptionalStoreBackend::Postgres,
        },
    )
}

/// Run `attempt` until it succeeds or `budget` elapses, sleeping `interval`
/// between tries. Each attempt is itself bounded by `interval`, so a backend
/// that accepts the connection then hangs can't consume the whole budget in a
/// single try. Returns the last error (wrapped) on timeout so callers fail
/// closed by `?`-ing the result. Always makes at least one attempt.
pub async fn connect_with_retry<T, F, Fut>(
    label: &str,
    budget: Duration,
    interval: Duration,
    mut attempt: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let deadline = Instant::now() + budget;
    let mut tries = 0u32;
    loop {
        tries += 1;
        let error = match tokio::time::timeout(interval, attempt()).await {
            Ok(Ok(value)) => return Ok(value),
            Ok(Err(error)) => error,
            Err(_elapsed) => {
                eyre::eyre!("attempt timed out after {}s", interval.as_secs())
            }
        };
        if Instant::now() >= deadline {
            return Err(error).wrap_err(format!(
                "{label} unreachable after {tries} attempt(s) over {}s",
                budget.as_secs()
            ));
        }
        tracing::warn!(backend = label, attempt = tries, error = %error, "backend not ready; retrying");
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn postgres_configured_and_available_resolves_postgres() {
        assert_eq!(
            resolve_store_backend("identity", "postgres", true).unwrap(),
            StoreBackend::Postgres
        );
    }

    #[test]
    fn postgres_configured_but_unavailable_fails_closed() {
        let err = resolve_store_backend("identity", "postgres", false).unwrap_err();
        assert!(err.to_string().contains("refusing to fall back"));
    }

    #[test]
    fn sqlite_and_empty_resolve_sqlite_regardless_of_postgres() {
        assert_eq!(
            resolve_store_backend("identity", "sqlite", false).unwrap(),
            StoreBackend::Sqlite
        );
        assert_eq!(
            resolve_store_backend("identity", "", true).unwrap(),
            StoreBackend::Sqlite
        );
    }

    #[test]
    fn unknown_backend_fails_closed() {
        assert!(resolve_store_backend("identity", "mysql", true).is_err());
    }

    #[test]
    fn disabled_short_circuits_before_any_availability_check() {
        assert_eq!(
            resolve_optional_store_backend("training", false, "postgres", false).unwrap(),
            OptionalStoreBackend::Disabled
        );
    }

    #[test]
    fn enabled_optional_store_inherits_fail_closed() {
        assert_eq!(
            resolve_optional_store_backend("training", true, "postgres", true).unwrap(),
            OptionalStoreBackend::Postgres
        );
        assert!(resolve_optional_store_backend("training", true, "postgres", false).is_err());
        assert_eq!(
            resolve_optional_store_backend("training", true, "sqlite", false).unwrap(),
            OptionalStoreBackend::Sqlite
        );
    }

    #[tokio::test]
    async fn retry_returns_immediately_on_first_success() {
        let calls = AtomicU32::new(0);
        let value = connect_with_retry(
            "x",
            Duration::from_secs(1),
            Duration::from_millis(1),
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Ok::<_, eyre::Report>(7) }
            },
        )
        .await
        .unwrap();
        assert_eq!(value, 7);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retry_succeeds_after_transient_failures_within_budget() {
        let calls = AtomicU32::new(0);
        let value = connect_with_retry(
            "x",
            Duration::from_secs(5),
            Duration::from_millis(1),
            || {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n < 2 {
                        bail!("not ready")
                    } else {
                        Ok(n)
                    }
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(value, 2);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retry_fails_closed_after_budget() {
        let result: Result<()> = connect_with_retry(
            "x",
            Duration::from_millis(20),
            Duration::from_millis(5),
            || async { bail!("never ready") },
        )
        .await;
        let err = result.unwrap_err();
        assert!(err.to_string().contains("unreachable"));
    }
}
