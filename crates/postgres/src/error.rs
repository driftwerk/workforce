use std::fmt;

/// Errors emitted by the shared Postgres infrastructure.
///
/// Wraps the underlying `tokio_postgres`, `deadpool`, and `refinery`
/// error types in a single shape so callers don't have to depend on
/// any of those crates directly. Implements [`std::error::Error`] so
/// eyre's blanket `From<E: std::error::Error>` impl picks it up
/// automatically — call sites that already use `eyre::Result<T>` can
/// stay as-is.
#[derive(Debug)]
pub enum PostgresError {
    Config(String),
    Pool(String),
    Query(String),
    Migration(String),
}

impl fmt::Display for PostgresError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(m) => write!(f, "postgres config error: {m}"),
            Self::Pool(m) => write!(f, "postgres pool error: {m}"),
            Self::Query(m) => write!(f, "postgres query error: {m}"),
            Self::Migration(m) => write!(f, "postgres migration error: {m}"),
        }
    }
}

impl std::error::Error for PostgresError {}

impl From<tokio_postgres::Error> for PostgresError {
    fn from(err: tokio_postgres::Error) -> Self {
        Self::Query(err.to_string())
    }
}

impl From<deadpool_postgres::PoolError> for PostgresError {
    fn from(err: deadpool_postgres::PoolError) -> Self {
        Self::Pool(err.to_string())
    }
}

impl From<deadpool_postgres::CreatePoolError> for PostgresError {
    fn from(err: deadpool_postgres::CreatePoolError) -> Self {
        Self::Pool(err.to_string())
    }
}

impl From<refinery::Error> for PostgresError {
    fn from(err: refinery::Error) -> Self {
        Self::Migration(err.to_string())
    }
}
