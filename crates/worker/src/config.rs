//! `worker.toml`: where the daemon listens, what it materialises, which
//! secrets it may read at refresh time, and which cloud holds those secrets
//! and the weekly bucket images. Loaded once per command; relative paths
//! resolve against the file's directory, as the hub's config does.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use eyre::{bail, Context, Result};
use serde::Deserialize;
use workforce_agent::tools::exec_runner::validate_overrides;

#[derive(Debug, Clone, Deserialize)]
pub struct WorkerConfig {
    pub listen: String,
    pub accept_common_name: String,
    pub ca_file: PathBuf,
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
    pub pool: String,
    pub state_dir: PathBuf,
    pub toolbox_image_file: PathBuf,
    #[serde(default)]
    pub registry_mirror: Option<String>,
    /// The most scopes at once; with `max_light_scopes`, the most full ones
    /// (those checking repos out).
    pub max_scopes: u32,
    /// The most light scopes (those with no repos: agent turns) at once; 0,
    /// the default, counts them against `max_scopes` as full ones are.
    #[serde(default)]
    pub max_light_scopes: u32,
    /// Admission by the host's memory and CPU headroom on top of the
    /// ceilings; unset, the ceilings alone decide.
    #[serde(default)]
    pub admission: Option<AdmissionConfig>,
    /// Whether a scope starts at seat only the services it needs: none for a
    /// scope with no repos, a golden's `services` for one with repos. The
    /// rest are left to start on first use, which needs a toolbox image whose
    /// toolbox-init reads `TOOLBOX_LAZY_SERVICES`; turn this on only once the
    /// deployed image does. Off, the default, every scope starts every
    /// service, as before.
    #[serde(default)]
    pub lazy_services: bool,
    pub pool_free_floor_gib: u64,
    pub scope_memory_gib: u32,
    pub scope_cpus: u32,
    pub snapshot_retention_days: u32,
    pub backup_pool: String,
    /// Where the weekly full images go: `s3://…` under `aws`, `gs://…`
    /// under `gcp`.
    pub backup_bucket_prefix: String,
    /// Which cloud the secrets and the bucket live in; `aws` when unset.
    #[serde(default)]
    pub cloud: CloudKind,
    /// The project bare secret ids resolve in under `gcp`; unset, gcloud's
    /// own default project (the VM's, on GCE) applies.
    #[serde(default)]
    pub gcp_project: Option<String>,
    #[serde(default)]
    pub golden: Vec<GoldenConfig>,
    #[serde(default)]
    pub database: Vec<DatabaseConfig>,
    /// Stateless services every scope starts beside its databases.
    #[serde(default)]
    pub services: Vec<Service>,
    /// How old a golden's newest snapshot may be before the doctor fails it.
    #[serde(default = "default_max_golden_age_hours")]
    pub max_golden_age_hours: u32,
    #[serde(default)]
    pub secrets: SecretsConfig,
    /// The share of `max_scopes` one person (or agent working for nobody)
    /// may hold while others queue for a seat.
    #[serde(default = "default_fair_share")]
    pub fair_share: f64,
}

fn default_max_golden_age_hours() -> u32 {
    36
}

fn default_fair_share() -> f64 {
    0.25
}

/// `[worker.admission]`: what each scope is charged against the host's
/// memory, the memory that must stay free, and the CPU pressure past which
/// no scope is seated.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AdmissionConfig {
    pub full_reserve_gib: u32,
    pub light_reserve_gib: u32,
    pub memory_floor_gib: u32,
    /// The highest PSI `cpu some avg10`, in percent, a new scope is seated
    /// at; unset, CPU is not checked.
    #[serde(default)]
    pub cpu_pressure_max: Option<f64>,
}

/// Where secrets are read and bucket images stored: SSM and S3 through
/// `aws`, or Secret Manager and GCS through `gcloud`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloudKind {
    #[default]
    Aws,
    Gcp,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GoldenConfig {
    pub name: String,
    pub slug: String,
    pub branch: String,
    #[serde(default)]
    pub install: Option<String>,
    /// Names from `[worker.secrets.env]` every scope checking this repo out
    /// is given.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// Plain settings for this repo, exported to its scopes as
    /// `TOOLBOX_REPO_ENV_<ALIAS>`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Paths on `branch` whose change makes the golden stale.
    #[serde(default)]
    pub watch: Vec<String>,
    /// With `lazy_services`, the services a scope checking this repo out
    /// starts with it, by name (`mysql`, `postgres`, `redis`, `ministack`);
    /// unset, every configured one. The others start on first use. It must
    /// cover what `install` and a repo's setup need at seat, as they run
    /// before anything else in the scope.
    #[serde(default)]
    pub services: Option<Vec<String>>,
}

/// A stateless service toolbox-init can start on loopback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Service {
    Redis,
    Ministack,
}

impl Service {
    pub fn name(self) -> &'static str {
        match self {
            Self::Redis => "redis",
            Self::Ministack => "ministack",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseKind {
    Mysql,
    Postgres,
}

impl DatabaseKind {
    /// Where the toolbox expects this database's directory to be mounted.
    pub fn mount_path(self) -> &'static str {
        match self {
            Self::Mysql => "/var/lib/toolbox/mysql",
            Self::Postgres => "/var/lib/toolbox/postgres",
        }
    }

    /// The service name toolbox-init starts for it.
    pub fn service(self) -> &'static str {
        match self {
            Self::Mysql => "mysql",
            Self::Postgres => "postgres",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    pub name: String,
    pub kind: DatabaseKind,
    #[serde(default)]
    pub seed_from: Option<String>,
    #[serde(default)]
    pub seed: Option<String>,
}

/// The secrets a refresh reads, by name in the configured cloud: SSM
/// parameter names under `aws`; Secret Manager secret ids, or full
/// `projects/<p>/secrets/<id>[/versions/<v>]` resources, under `gcp`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SecretsConfig {
    #[serde(default)]
    pub github_token: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct WorkerFile {
    worker: WorkerConfig,
}

/// The most scopes one worker may be configured for, of both weights.
pub const MAX_SCOPES_CEILING: u32 = 500;

/// The dataset under the pool that holds the agent's home for every scope.
pub const HOME_GOLDEN: &str = "home";

/// Whether `name` can be one component of a dataset name, a container name
/// and a mount path without quoting.
pub fn dataset_safe(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Whether `value` can be one `KEY=value` line of an env file.
pub fn fits_one_line(value: &str) -> bool {
    !value.contains(['\n', '\r', '\0'])
}

fn repo_relative(path: &str) -> bool {
    !path.is_empty()
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

impl WorkerConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let file: WorkerFile =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let mut config = file.worker;
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        for field in [
            &mut config.ca_file,
            &mut config.cert_file,
            &mut config.key_file,
            &mut config.state_dir,
            &mut config.toolbox_image_file,
        ] {
            if field.is_relative() {
                *field = base.join(&*field);
            }
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        self.listen
            .parse::<SocketAddr>()
            .with_context(|| format!("[worker] listen {:?} is not host:port", self.listen))?;
        for (key, value) in [
            ("accept_common_name", &self.accept_common_name),
            ("pool", &self.pool),
            ("backup_pool", &self.backup_pool),
            ("backup_bucket_prefix", &self.backup_bucket_prefix),
        ] {
            if value.trim().is_empty() {
                bail!("[worker] {key} must be set");
            }
        }
        match self.cloud {
            CloudKind::Aws => {
                if self.backup_bucket_prefix.starts_with("gs://") {
                    bail!(
                        "[worker] backup_bucket_prefix {:?} is a GCS bucket but cloud is \"aws\"; use an s3:// prefix or set cloud = \"gcp\"",
                        self.backup_bucket_prefix
                    );
                }
                if self.gcp_project.is_some() {
                    bail!(
                        "[worker] gcp_project is set but cloud is \"aws\"; set cloud = \"gcp\" or remove gcp_project"
                    );
                }
            }
            CloudKind::Gcp => {
                if self.backup_bucket_prefix.starts_with("s3://") {
                    bail!(
                        "[worker] backup_bucket_prefix {:?} is an S3 bucket but cloud is \"gcp\"; use a gs:// prefix or set cloud = \"aws\"",
                        self.backup_bucket_prefix
                    );
                }
                if self
                    .gcp_project
                    .as_deref()
                    .is_some_and(|project| project.trim().is_empty())
                {
                    bail!("[worker] gcp_project is empty; set a project or remove the key");
                }
            }
        }
        for (key, value) in [
            ("max_scopes", self.max_scopes),
            ("scope_memory_gib", self.scope_memory_gib),
            ("scope_cpus", self.scope_cpus),
            ("snapshot_retention_days", self.snapshot_retention_days),
            ("max_golden_age_hours", self.max_golden_age_hours),
        ] {
            if value == 0 {
                bail!("[worker] {key} must be at least 1");
            }
        }
        if !(self.fair_share > 0.0 && self.fair_share <= 1.0) {
            bail!("[worker] fair_share must be above 0 and at most 1");
        }
        if self.max_scopes.saturating_add(self.max_light_scopes) > MAX_SCOPES_CEILING {
            bail!(
                "[worker] max_scopes and max_light_scopes together must be at most {MAX_SCOPES_CEILING}"
            );
        }
        if let Some(admission) = &self.admission {
            if admission.full_reserve_gib == 0 || admission.light_reserve_gib == 0 {
                bail!(
                    "[worker.admission] full_reserve_gib and light_reserve_gib must be at least 1"
                );
            }
            if admission
                .cpu_pressure_max
                .is_some_and(|max| !(0.0..=100.0).contains(&max))
            {
                bail!("[worker.admission] cpu_pressure_max is a percentage, 0 to 100");
            }
        }
        let mut services = std::collections::HashSet::new();
        if let Some(twice) = self.services.iter().find(|s| !services.insert(**s)) {
            bail!("[worker] services lists {:?} twice", twice.name());
        }
        let mut names = std::collections::HashSet::from([HOME_GOLDEN]);
        for golden in &self.golden {
            if !dataset_safe(&golden.name) || golden.slug.split('/').count() != 2 {
                bail!(
                    "[[worker.golden]] {:?} needs a dataset-safe name and an owner/name slug",
                    golden.name
                );
            }
            if !names.insert(golden.name.as_str()) {
                bail!("[[worker.golden]] name {:?} is used twice", golden.name);
            }
            self.validate_golden(golden)?;
        }
        let mut kinds = std::collections::HashSet::new();
        for database in &self.database {
            if !dataset_safe(&database.name) {
                bail!(
                    "[[worker.database]] {:?} is not a dataset-safe name",
                    database.name
                );
            }
            if !kinds.insert(database.kind) {
                bail!(
                    "[[worker.database]] {:?} is a second {} database; the toolbox runs one of each kind",
                    database.name,
                    database.kind.service()
                );
            }
            if !names.insert(database.name.as_str()) {
                bail!(
                    "[[worker.database]] name {:?} clashes with another dataset",
                    database.name
                );
            }
            match (&database.seed_from, &database.seed) {
                (Some(from), Some(_)) if !self.golden.iter().any(|g| &g.name == from) => {
                    bail!(
                        "[[worker.database]] {:?} seeds from unknown golden {from:?}",
                        database.name
                    )
                }
                (Some(_), None) | (None, Some(_)) => {
                    bail!(
                        "[[worker.database]] {:?} needs both seed_from and seed, or neither",
                        database.name
                    )
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn validate_golden(&self, golden: &GoldenConfig) -> Result<()> {
        let name = &golden.name;
        if let Some(secret) = golden
            .secrets
            .iter()
            .find(|secret| !self.secrets.env.contains_key(*secret))
        {
            bail!("[[worker.golden]] {name:?} secret {secret:?} is not in [worker.secrets.env]");
        }
        let env: Vec<(String, String)> = golden
            .env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        validate_overrides(&env)
            .map_err(|reason| eyre::eyre!("[[worker.golden]] {name:?} env: {reason}"))?;
        if let Some(key) = golden
            .env
            .iter()
            .find_map(|(key, value)| (!fits_one_line(value)).then_some(key))
        {
            bail!("[[worker.golden]] {name:?} env {key} must be one line without NUL");
        }
        if let Some(path) = golden.watch.iter().find(|path| !repo_relative(path)) {
            bail!("[[worker.golden]] {name:?} watch {path:?} is not a path inside the repository");
        }
        if !golden.watch.is_empty() && self.secrets.github_token.is_none() {
            bail!(
                "[[worker.golden]] {name:?} watches lockfiles, which needs [worker.secrets] github_token"
            );
        }
        let configured = self.service_names();
        if let Some(service) = golden
            .services
            .iter()
            .flatten()
            .find(|service| !configured.contains(&service.as_str()))
        {
            bail!(
                "[[worker.golden]] {name:?} services names {service:?}, which is not a configured service (configured: {})",
                configured.join(", ")
            );
        }
        Ok(())
    }

    /// Every service a scope may run, as toolbox-init names them: the
    /// databases, then the stateless services.
    pub fn service_names(&self) -> Vec<&'static str> {
        self.database
            .iter()
            .map(|database| database.kind.service())
            .chain(self.services.iter().map(|service| service.name()))
            .collect()
    }

    pub fn listen_addr(&self) -> SocketAddr {
        self.listen.parse().expect("validated at load")
    }

    pub fn pool_free_floor_bytes(&self) -> u64 {
        self.pool_free_floor_gib * 1024 * 1024 * 1024
    }

    pub fn golden(&self, name: &str) -> Option<&GoldenConfig> {
        self.golden.iter().find(|g| g.name == name)
    }

    pub fn golden_for_slug(&self, slug: &str) -> Option<&GoldenConfig> {
        self.golden
            .iter()
            .find(|g| g.slug.eq_ignore_ascii_case(slug))
    }

    /// The image every scope and refresh runs, as installed by the host's
    /// bootstrap, which resolves the newest deployed toolbox tag.
    pub fn toolbox_image(&self) -> Result<String> {
        let image = std::fs::read_to_string(&self.toolbox_image_file)
            .with_context(|| format!("reading {}", self.toolbox_image_file.display()))?;
        let image = image.trim().to_string();
        if image.is_empty() {
            bail!("{} names no image", self.toolbox_image_file.display());
        }
        Ok(image)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sample() -> WorkerConfig {
        WorkerConfig {
            listen: "0.0.0.0:7444".into(),
            accept_common_name: "workforce-hub".into(),
            ca_file: "/etc/workforce/ca.pem".into(),
            cert_file: "/etc/workforce/worker.pem".into(),
            key_file: "/etc/workforce/worker-key.pem".into(),
            pool: "tank".into(),
            state_dir: "/tank/state".into(),
            toolbox_image_file: "/etc/workforce/toolbox-image".into(),
            registry_mirror: Some("http://host.docker.internal:5000".into()),
            max_scopes: 2,
            max_light_scopes: 0,
            admission: None,
            lazy_services: true,
            pool_free_floor_gib: 1,
            scope_memory_gib: 8,
            scope_cpus: 4,
            snapshot_retention_days: 7,
            backup_pool: "backup".into(),
            backup_bucket_prefix: "s3://bucket/golden".into(),
            cloud: CloudKind::Aws,
            gcp_project: None,
            golden: vec![GoldenConfig {
                name: "rails".into(),
                slug: "acme/rails".into(),
                branch: "main".into(),
                install: Some("bundle install".into()),
                secrets: vec!["RAILS_MASTER_KEY".into()],
                env: BTreeMap::from([("TZ".into(), "Europe/London".into())]),
                watch: vec!["Gemfile.lock".into()],
                services: None,
            }],
            database: vec![
                DatabaseConfig {
                    name: "mysql".into(),
                    kind: DatabaseKind::Mysql,
                    seed_from: Some("rails".into()),
                    seed: Some("bin/rails db:schema:load".into()),
                },
                DatabaseConfig {
                    name: "pg".into(),
                    kind: DatabaseKind::Postgres,
                    seed_from: None,
                    seed: None,
                },
            ],
            services: Vec::new(),
            max_golden_age_hours: 36,
            fair_share: 0.25,
            secrets: SecretsConfig {
                github_token: Some("/workforce/prod/worker/github-read-token".into()),
                env: BTreeMap::from([(
                    "RAILS_MASTER_KEY".into(),
                    "/workforce/prod/worker/rails-dev-master-key".into(),
                )]),
            },
        }
    }

    #[test]
    fn a_complete_config_validates() {
        assert!(sample().validate().is_ok());
    }

    #[test]
    fn validate_names_the_offending_key() {
        let mut config = sample();
        config.listen = "nowhere".into();
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("listen"));

        let mut config = sample();
        config.max_scopes = 0;
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("max_scopes"));

        for share in [0.0, 1.5, f64::NAN] {
            let mut config = sample();
            config.fair_share = share;
            assert!(config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("fair_share"));
        }

        let mut config = sample();
        config.golden.push(config.golden[0].clone());
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("used twice"));

        let mut config = sample();
        config.database[0].seed_from = Some("kotlin".into());
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("unknown golden"));

        let mut config = sample();
        config.database[0].seed = None;
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("both seed_from and seed"));

        let mut config = sample();
        config.golden[0].name = HOME_GOLDEN.into();
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("used twice"));

        let mut config = sample();
        config.golden[0].name = "rails/app".into();
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("dataset-safe"));

        let mut config = sample();
        config.database[1].kind = DatabaseKind::Mysql;
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("second mysql"));

        let mut config = sample();
        config.scope_cpus = 0;
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("scope_cpus"));
    }

    #[test]
    fn config_rejects_a_golden_secret_missing_from_secrets_env() {
        let mut config = sample();
        config.golden[0]
            .secrets
            .push("LOCALSTACK_AUTH_TOKEN".into());
        let error = config.validate().unwrap_err().to_string();
        assert_eq!(
            error,
            "[[worker.golden]] \"rails\" secret \"LOCALSTACK_AUTH_TOKEN\" is not in [worker.secrets.env]"
        );
    }

    #[test]
    fn a_golden_env_needs_env_shaped_names_and_one_line_values() {
        let mut config = sample();
        config.golden[0]
            .env
            .insert("rack_env".into(), "test".into());
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("\"rails\" env") && error.contains("[A-Z_][A-Z0-9_]*"),
            "{error}"
        );

        let mut config = sample();
        config.golden[0]
            .env
            .insert("RACK_ENV".into(), "test\nEVIL=1".into());
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("env RACK_ENV must be one line") && !error.contains("EVIL"),
            "{error}"
        );
    }

    #[test]
    fn watched_paths_stay_inside_the_repository_and_need_the_github_token() {
        for path in [
            "",
            "/etc/passwd",
            "../Gemfile.lock",
            "a//b",
            "./Gemfile.lock",
        ] {
            let mut config = sample();
            config.golden[0].watch = vec![path.into()];
            assert!(
                config
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("is not a path inside the repository"),
                "{path:?}"
            );
        }

        let mut config = sample();
        config.secrets.github_token = None;
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("needs [worker.secrets] github_token"));
        config.golden[0].watch.clear();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn a_golden_may_name_only_configured_services() {
        let mut config = sample();
        config.golden[0].services = Some(vec!["mysql".into(), "postgres".into()]);
        assert!(config.validate().is_ok());
        config.golden[0].services = Some(Vec::new());
        assert!(config.validate().is_ok());
        config.golden[0].services = Some(vec!["redis".into()]);
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("\"redis\"") && error.contains("mysql, postgres"),
            "{error}"
        );
        config.services = vec![Service::Redis];
        assert!(config.validate().is_ok());
    }

    #[test]
    fn services_are_redis_or_ministack_each_at_most_once() {
        let mut config = sample();
        config.services = vec![Service::Redis, Service::Ministack];
        assert!(config.validate().is_ok());
        config.services.push(Service::Redis);
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("services lists \"redis\" twice"));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("worker.toml");
        std::fs::write(
            &path,
            format!("{MINIMAL}backup_bucket_prefix = \"s3://b/golden\"\nservices = [\"redis\", \"mongo\"]\n"),
        )
        .unwrap();
        let error = format!("{:#}", WorkerConfig::load(&path).unwrap_err());
        assert!(
            error.contains("mongo") && error.contains("ministack"),
            "{error}"
        );
    }

    #[test]
    fn the_bucket_and_the_project_must_belong_to_the_configured_cloud() {
        let mut config = sample();
        config.backup_bucket_prefix = "gs://bucket/golden".into();
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("backup_bucket_prefix") && error.contains("cloud is \"aws\""),
            "{error}"
        );

        let mut config = sample();
        config.gcp_project = Some("contoso".into());
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("gcp_project") && error.contains("cloud is \"aws\""),
            "{error}"
        );

        let mut config = sample();
        config.cloud = CloudKind::Gcp;
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("backup_bucket_prefix") && error.contains("cloud is \"gcp\""),
            "{error}"
        );

        config.backup_bucket_prefix = "gs://bucket/golden".into();
        assert!(config.validate().is_ok());
        config.gcp_project = Some("contoso".into());
        assert!(config.validate().is_ok());
        config.gcp_project = Some(" ".into());
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("gcp_project is empty"));
    }

    /// Every required key but the bucket prefix.
    const MINIMAL: &str = r#"
[worker]
listen = "127.0.0.1:7444"
accept_common_name = "workforce-hub"
ca_file = "ca.pem"
cert_file = "worker.pem"
key_file = "worker-key.pem"
pool = "tank"
state_dir = "state"
toolbox_image_file = "toolbox-image"
max_scopes = 4
pool_free_floor_gib = 60
scope_memory_gib = 8
scope_cpus = 4
snapshot_retention_days = 7
backup_pool = "backup"
"#;

    #[test]
    fn admission_loads_from_its_table_and_is_checked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("worker.toml");
        std::fs::write(
            &path,
            format!(
                "{}backup_bucket_prefix = \"s3://b/golden\"\n\
                 [worker.admission]\nfull_reserve_gib = 3\nlight_reserve_gib = 1\nmemory_floor_gib = 6\ncpu_pressure_max = 80\n",
                MINIMAL.replace("[worker]\n", "[worker]\nmax_light_scopes = 40\n")
            ),
        )
        .unwrap();
        let config = WorkerConfig::load(&path).unwrap_or_else(|e| panic!("{e:#}"));
        assert_eq!(config.max_light_scopes, 40);
        assert_eq!(
            config.admission,
            Some(AdmissionConfig {
                full_reserve_gib: 3,
                light_reserve_gib: 1,
                memory_floor_gib: 6,
                cpu_pressure_max: Some(80.0),
            })
        );

        let mut config = sample();
        assert!(config.admission.is_none() && config.max_light_scopes == 0);
        config.admission = Some(AdmissionConfig {
            full_reserve_gib: 3,
            light_reserve_gib: 0,
            memory_floor_gib: 6,
            cpu_pressure_max: None,
        });
        assert!(config.validate().is_err());
        config.admission = Some(AdmissionConfig {
            light_reserve_gib: 1,
            cpu_pressure_max: Some(180.0),
            ..config.admission.clone().unwrap()
        });
        assert!(config.validate().is_err());
    }

    #[test]
    fn lazy_services_and_a_goldens_services_load_and_an_older_file_has_neither() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("worker.toml");
        std::fs::write(
            &path,
            format!("{MINIMAL}backup_bucket_prefix = \"s3://b/golden\"\n"),
        )
        .unwrap();
        let older = WorkerConfig::load(&path).unwrap();
        assert!(!older.lazy_services, "off unless asked for");

        let newer = MINIMAL.replace(
            "[worker]\n",
            "[worker]\nlazy_services = true\nservices = [\"redis\"]\n",
        );
        std::fs::write(
            &path,
            format!(
                "{newer}backup_bucket_prefix = \"s3://b/golden\"\n\
                 [[worker.golden]]\nname = \"web\"\nslug = \"acme/web\"\nbranch = \"main\"\nservices = []\n\
                 [[worker.golden]]\nname = \"api\"\nslug = \"acme/api\"\nbranch = \"main\"\nservices = [\"redis\"]\n"
            ),
        )
        .unwrap();
        let newer = WorkerConfig::load(&path).unwrap_or_else(|e| panic!("{e:#}"));
        assert!(newer.lazy_services);
        assert_eq!(newer.golden[0].services, Some(Vec::new()));
        assert_eq!(newer.golden[1].services, Some(vec!["redis".to_string()]));
    }

    #[test]
    fn load_anchors_relative_paths_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("worker.toml");
        std::fs::write(
            &path,
            format!("{MINIMAL}backup_bucket_prefix = \"s3://b/golden\"\n"),
        )
        .unwrap();
        let config = WorkerConfig::load(&path).unwrap();
        assert_eq!(config.ca_file, dir.path().join("ca.pem"));
        assert_eq!(config.state_dir, dir.path().join("state"));
        assert!(config.golden.is_empty());
        assert!(config.services.is_empty());
        assert_eq!(config.max_golden_age_hours, 36);
        assert!(config.secrets.env.is_empty());
        assert_eq!(config.cloud, CloudKind::Aws);
        assert_eq!(config.gcp_project, None);
    }

    #[test]
    fn a_gcp_worker_names_its_cloud_and_project() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("worker.toml");
        std::fs::write(
            &path,
            format!(
                "{MINIMAL}backup_bucket_prefix = \"gs://b/golden\"\ncloud = \"gcp\"\ngcp_project = \"contoso\"\n"
            ),
        )
        .unwrap();
        let config = WorkerConfig::load(&path).unwrap();
        assert_eq!(config.cloud, CloudKind::Gcp);
        assert_eq!(config.gcp_project.as_deref(), Some("contoso"));

        std::fs::write(
            &path,
            format!("{MINIMAL}backup_bucket_prefix = \"s3://b/golden\"\ncloud = \"azure\"\n"),
        )
        .unwrap();
        assert!(WorkerConfig::load(&path).is_err());
    }
}
