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
    /// A statement (a query or a migration's) gave up waiting for a lock,
    /// under `lock_timeout`: `55P03`, `lock_not_available`.
    LockNotAvailable(String),
}

impl PostgresError {
    /// Whether this is Postgres giving up on a lock: worth trying again once
    /// whatever held it is done.
    pub fn is_lock_not_available(&self) -> bool {
        matches!(self, Self::LockNotAvailable(_))
    }
}

/// Whether `err`, or anything it was caused by, is Postgres giving up on a
/// lock.
fn locked_out(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut cause = Some(err);
    while let Some(e) = cause {
        if e.downcast_ref::<tokio_postgres::Error>()
            .and_then(tokio_postgres::Error::code)
            == Some(&tokio_postgres::error::SqlState::LOCK_NOT_AVAILABLE)
        {
            return true;
        }
        cause = e.source();
    }
    false
}

impl fmt::Display for PostgresError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(m) => write!(f, "postgres config error: {m}"),
            Self::Pool(m) => write!(f, "postgres pool error: {m}"),
            Self::Query(m) => write!(f, "postgres query error: {m}"),
            Self::Migration(m) => write!(f, "postgres migration error: {m}"),
            Self::LockNotAvailable(m) => write!(f, "postgres lock not available: {m}"),
        }
    }
}

impl std::error::Error for PostgresError {}

impl From<tokio_postgres::Error> for PostgresError {
    fn from(err: tokio_postgres::Error) -> Self {
        match locked_out(&err) {
            true => Self::LockNotAvailable(err.to_string()),
            false => Self::Query(err.to_string()),
        }
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
        match locked_out(&err) {
            true => Self::LockNotAvailable(err.to_string()),
            false => Self::Migration(err.to_string()),
        }
    }
}
