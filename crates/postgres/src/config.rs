use serde::{Deserialize, Serialize};

/// Default size of the shared connection pool when not specified in
/// config. Used by both the manual `Default` impl and as the
/// `serde(default)` source for the `pool_size` field.
const DEFAULT_POOL_SIZE: usize = 16;

/// TLS mode for the Postgres connection.
///
/// Defaults to [`SslMode::Disable`] so existing deployments (which connect
/// without TLS — e.g. directly to a non-TLS Postgres) are unaffected. Only an
/// install that explicitly opts in gets TLS; nothing is enforced globally.
/// `require` is needed for connections that mandate TLS, such as an AWS RDS
/// Proxy endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SslMode {
    /// Plaintext connection (`NoTls`). The default — preserves prior behavior.
    #[default]
    Disable,
    /// Encrypt the connection but do not verify the server certificate.
    /// Defeats passive sniffing; suitable for in-VPC links to an endpoint
    /// that requires TLS but presents a private/AWS CA cert.
    Require,
}

/// Connection settings for the optional Postgres backend.
///
/// Workforce uses **one installation per tenant** — there is no shared
/// multi-tenant deployment, no row-level isolation. The configured
/// database holds data for exactly one organisation.
///
/// Either supply a full `url` (`postgres://user:pass@host:port/db`) or
/// the individual `host`/`port`/`database`/`user`/`password` fields.
/// `password` supports `${VAR}` process-env expansion via the settings
/// loader (`WorkforceConfig::from_toml` → `config_expand::expand_env_vars`),
/// e.g. `password = "${WORKFORCE_PG_PASSWORD}"`. NOTE: this only happens
/// when the config is loaded through `WorkforceConfig::load`/`from_toml`,
/// not a bare `toml::from_str`. `${vault:...}` refs are NOT resolved for
/// this field (vault expansion isn't applied to the root config).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostgresConfig {
    /// Master switch. When false, the shared pool is never constructed
    /// and individual stores fall back to their SQLite/file backends
    /// regardless of their per-store `backend` field.
    #[serde(default)]
    pub enabled: bool,

    /// Full connection URL. Takes precedence over the individual fields.
    #[serde(default)]
    pub url: Option<String>,

    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    /// Supports `${VAR}` process-env expansion when loaded via
    /// `WorkforceConfig::load`/`from_toml` (not bare `toml::from_str`).
    #[serde(default)]
    pub password: Option<String>,

    /// Maximum connections in the pool.
    #[serde(default = "default_pool_size")]
    pub pool_size: usize,

    /// Optional `application_name` reported to Postgres for connection
    /// identification in `pg_stat_activity` etc.
    #[serde(default)]
    pub application_name: Option<String>,

    /// TLS mode. Defaults to `disable` (plaintext), so existing installs are
    /// unaffected. Set `require` to encrypt (e.g. for an RDS Proxy endpoint).
    #[serde(default)]
    pub sslmode: SslMode,

    /// How long startup keeps retrying the initial connection before failing
    /// closed. Long enough to ride out a cold cluster-wide restart where
    /// Postgres is still coming up alongside the hub.
    #[serde(default = "default_backend_connect_timeout_secs")]
    pub connect_timeout_secs: u64,
}

fn default_pool_size() -> usize {
    DEFAULT_POOL_SIZE
}

/// The most connections a store that is seldom asked opens
/// ([`PostgresConfig::small`]).
pub const SMALL_POOL_SIZE: usize = 4;

impl PostgresConfig {
    /// These settings for a store that is seldom asked: at most
    /// [`SMALL_POOL_SIZE`] connections, fewer when `pool_size` says so, and
    /// never fewer than two, since migrations hold one for their lock and
    /// ask for another. Every store's pool otherwise took `pool_size`, and
    /// between them a hub's pools could open far more than the server's
    /// `max_connections`.
    pub fn small(&self) -> Self {
        Self {
            pool_size: self.pool_size.clamp(2, SMALL_POOL_SIZE),
            ..self.clone()
        }
    }
}

/// Shared default startup connection budget (seconds) for the real backends.
pub fn default_backend_connect_timeout_secs() -> u64 {
    300
}

impl Default for PostgresConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: None,
            host: None,
            port: None,
            database: None,
            user: None,
            password: None,
            pool_size: DEFAULT_POOL_SIZE,
            application_name: None,
            sslmode: SslMode::Disable,
            connect_timeout_secs: default_backend_connect_timeout_secs(),
        }
    }
}

impl PostgresConfig {
    /// Translate the config into a `tokio_postgres::Config`.
    ///
    /// Order of precedence:
    ///   1. `url`, if set, parsed via `tokio_postgres::Config::from_str`.
    ///   2. Individual fields, with reasonable Postgres defaults
    ///      (`host=localhost`, `port=5432`).
    ///
    /// Returns an error if neither `url` nor enough individual fields
    /// are present to form a valid connection.
    #[cfg(feature = "postgres")]
    pub fn to_tokio_postgres_config(&self) -> Result<tokio_postgres::Config, super::PostgresError> {
        use std::str::FromStr;
        use std::time::Duration;

        let mut cfg = match self.url.as_deref() {
            Some(url) if !url.is_empty() => tokio_postgres::Config::from_str(url)
                .map_err(|e| super::PostgresError::Config(format!("invalid url: {e}")))?,
            _ => {
                let mut c = tokio_postgres::Config::new();
                c.host(self.host.as_deref().unwrap_or("localhost"));
                c.port(self.port.unwrap_or(5432));
                let database = self
                    .database
                    .as_deref()
                    .ok_or_else(|| super::PostgresError::Config("database is required".into()))?;
                c.dbname(database);
                let user = self
                    .user
                    .as_deref()
                    .ok_or_else(|| super::PostgresError::Config("user is required".into()))?;
                c.user(user);
                if let Some(password) = self.password.as_deref() {
                    c.password(password);
                }
                c
            }
        };
        if let Some(ref name) = self.application_name {
            cfg.application_name(name);
        }

        // tokio-postgres defaults keepalives_idle to 2h — longer than a Docker
        // Swarm overlay's service-VIP idle window, so idle pooled connections
        // get reaped and surface as `deadpool.postgres: Connection could not be
        // recycled` warnings on next use. Probe sooner to keep them warm.
        cfg.keepalives_idle(Duration::from_secs(30));
        cfg.keepalives_interval(Duration::from_secs(10));
        cfg.keepalives_retries(3);

        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_small_store_takes_at_most_four_connections_and_at_least_two() {
        let sized = |pool_size| {
            PostgresConfig {
                pool_size,
                ..PostgresConfig::default()
            }
            .small()
            .pool_size
        };
        assert_eq!(sized(16), SMALL_POOL_SIZE);
        assert_eq!(sized(3), 3);
        assert_eq!(sized(1), 2);
    }

    #[test]
    fn default_pool_size_is_sensible() {
        let cfg = PostgresConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.pool_size, default_pool_size());
        assert!(cfg.url.is_none());
    }

    #[test]
    fn deserialises_from_toml_with_url() {
        let toml = r#"
            enabled = true
            url = "postgres://wf:secret@db.internal:5432/workforce"
            pool_size = 32
            application_name = "workforce-hub-prod"
        "#;
        let cfg: PostgresConfig = toml::from_str(toml).unwrap();
        assert!(cfg.enabled);
        assert_eq!(
            cfg.url.as_deref(),
            Some("postgres://wf:secret@db.internal:5432/workforce")
        );
        assert_eq!(cfg.pool_size, 32);
        assert_eq!(cfg.application_name.as_deref(), Some("workforce-hub-prod"));
    }

    #[test]
    fn deserialises_from_toml_with_individual_fields() {
        let toml = r#"
            enabled = true
            host = "db.internal"
            port = 5433
            database = "workforce"
            user = "wf"
            password = "secret"
        "#;
        let cfg: PostgresConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.host.as_deref(), Some("db.internal"));
        assert_eq!(cfg.port, Some(5433));
        assert_eq!(cfg.database.as_deref(), Some("workforce"));
        assert_eq!(cfg.user.as_deref(), Some("wf"));
        assert_eq!(cfg.password.as_deref(), Some("secret"));
    }

    #[test]
    fn sslmode_defaults_to_disable_and_parses() {
        let default: PostgresConfig = toml::from_str("enabled = true").unwrap();
        assert_eq!(default.sslmode, SslMode::Disable);
        let require: PostgresConfig =
            toml::from_str("enabled = true\nsslmode = \"require\"").unwrap();
        assert_eq!(require.sslmode, SslMode::Require);
        let disable: PostgresConfig = toml::from_str("sslmode = \"disable\"").unwrap();
        assert_eq!(disable.sslmode, SslMode::Disable);
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn to_tokio_postgres_config_parses_url() {
        let cfg = PostgresConfig {
            enabled: true,
            url: Some("postgres://wf:secret@db.internal:5432/workforce".to_string()),
            application_name: Some("test-app".to_string()),
            ..PostgresConfig::default()
        };
        let pg = cfg.to_tokio_postgres_config().unwrap();
        assert_eq!(pg.get_dbname(), Some("workforce"));
        assert_eq!(pg.get_user(), Some("wf"));
        assert_eq!(pg.get_application_name(), Some("test-app"));
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn to_tokio_postgres_config_sets_short_keepalive_idle() {
        use std::time::Duration;
        // Both the url and individual-field branches must get the short
        // keepalive so overlay idle-reaping can't silently drop pooled conns.
        let from_url = PostgresConfig {
            enabled: true,
            url: Some("postgres://wf:secret@db.internal:5432/workforce".to_string()),
            ..PostgresConfig::default()
        }
        .to_tokio_postgres_config()
        .unwrap();
        assert_eq!(from_url.get_keepalives_idle(), Duration::from_secs(30));

        let from_fields = PostgresConfig {
            enabled: true,
            host: Some("db.internal".to_string()),
            database: Some("workforce".to_string()),
            user: Some("wf".to_string()),
            ..PostgresConfig::default()
        }
        .to_tokio_postgres_config()
        .unwrap();
        assert_eq!(from_fields.get_keepalives_idle(), Duration::from_secs(30));
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn to_tokio_postgres_config_uses_individual_fields() {
        let cfg = PostgresConfig {
            enabled: true,
            host: Some("db.internal".to_string()),
            port: Some(5433),
            database: Some("workforce".to_string()),
            user: Some("wf".to_string()),
            password: Some("secret".to_string()),
            ..PostgresConfig::default()
        };
        let pg = cfg.to_tokio_postgres_config().unwrap();
        assert_eq!(pg.get_dbname(), Some("workforce"));
        assert_eq!(pg.get_user(), Some("wf"));
        assert_eq!(pg.get_ports(), &[5433]);
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn to_tokio_postgres_config_requires_user_and_database_when_no_url() {
        let cfg = PostgresConfig {
            enabled: true,
            host: Some("db.internal".to_string()),
            ..PostgresConfig::default()
        };
        assert!(cfg.to_tokio_postgres_config().is_err());
    }
}
