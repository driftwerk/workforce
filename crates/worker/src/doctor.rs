//! One command that says whether the host is fit to serve: certificates,
//! pool, goldens, image, runtime. Every check catches its own failure so a
//! broken host reports every problem, not the first.

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use eyre::{bail, Result};
use workforce_agent::tools::exec_runner::{certificate_expiry, Acceptor, Listener};

use crate::config::{GoldenConfig, WorkerConfig, HOME_GOLDEN};
use crate::ports::{Cloud, Containers, Dataset, Repos, Zfs};
use crate::refresh::{github_token, watched_shas};
use crate::scopes::first_line;
use crate::serve::identity;
use crate::state::{GoldenRecord, GoldenStamp, GoldenStore};

const CERTIFICATE_WARNING: Duration = Duration::days(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

fn check(name: &str, outcome: Result<String>) -> Check {
    match outcome {
        Ok(detail) => Check {
            name: name.into(),
            ok: true,
            detail,
        },
        Err(e) => Check {
            name: name.into(),
            ok: false,
            detail: e.to_string(),
        },
    }
}

pub async fn doctor(
    config: &WorkerConfig,
    zfs: &dyn Zfs,
    containers: &dyn Containers,
    cloud: &dyn Cloud,
    repos: &dyn Repos,
    now: DateTime<Utc>,
) -> Vec<Check> {
    let mut checks = vec![
        check("tls identity", tls_identity(config).await),
        check("certificate expiry", expiry(config, now)),
        check(
            "pool",
            zfs.free_bytes(&config.pool)
                .await
                .map(|free| format!("{} GiB free", free >> 30)),
        ),
    ];
    for name in [
        "golden",
        "work",
        "caches",
        "state",
        "docker",
        crate::scopes::HOMES,
    ] {
        let dataset = Dataset(format!("{}/{name}", config.pool));
        checks.push(check(
            &format!("dataset {dataset}"),
            zfs.exists(&dataset).await.and_then(|exists| {
                exists
                    .then(|| "present".to_string())
                    .ok_or_else(|| eyre::eyre!("missing"))
            }),
        ));
    }
    let records = GoldenStore::new(&config.state_dir).load();
    let recorded = |name: &str| {
        records
            .as_ref()
            .ok()
            .and_then(|records| records.get(name))
            .cloned()
            .unwrap_or_default()
    };
    checks.push(check(
        "golden records",
        records
            .as_ref()
            .map(|records| format!("{} golden(s) recorded", records.len()))
            .map_err(|e| eyre::eyre!("{e:#}")),
    ));
    let goldens = config
        .golden
        .iter()
        .map(|g| g.name.as_str())
        .chain(config.database.iter().map(|d| d.name.as_str()))
        .chain([HOME_GOLDEN]);
    for name in goldens {
        checks.push(check(
            &format!("golden {name}"),
            golden_freshness(zfs, config, name, &recorded(name), now).await,
        ));
    }
    let watching: Vec<&GoldenConfig> = config
        .golden
        .iter()
        .filter(|golden| !golden.watch.is_empty())
        .collect();
    if !watching.is_empty() {
        let token = github_token(config, cloud).await;
        for golden in watching {
            checks.push(check(
                &format!("golden {} lockfiles", golden.name),
                lockfiles(repos, golden, &token, recorded(&golden.name).stamp.as_ref()).await,
            ));
        }
    }
    checks.push(check(
        "homes",
        zfs.children(&Dataset(format!(
            "{}/{}",
            config.pool,
            crate::scopes::HOMES
        )))
        .await
        .map(|homes| format!("{} kept", homes.len())),
    ));
    checks.push(check(
        "toolbox image",
        toolbox_image(config, containers).await,
    ));
    checks.push(check(
        "container runtime",
        containers
            .list(crate::scopes::SCOPE_LABEL)
            .await
            .map(|live| format!("{} scope container(s)", live.len())),
    ));
    checks
}

async fn tls_identity(config: &WorkerConfig) -> Result<String> {
    let identity = identity(config)?;
    let probe = Listener::Tls {
        addr: "127.0.0.1:0".parse()?,
        identity,
        accept_common_name: config.accept_common_name.clone(),
    };
    Acceptor::bind(probe).await?;
    Ok(format!(
        "serves {} and admits {}",
        config.listen, config.accept_common_name
    ))
}

fn expiry(config: &WorkerConfig, now: DateTime<Utc>) -> Result<String> {
    let pem = std::fs::read_to_string(&config.cert_file)?;
    let expires = certificate_expiry(&pem)?;
    if expires <= now {
        bail!("expired at {expires}");
    }
    if expires - now < CERTIFICATE_WARNING {
        bail!(
            "expires at {expires}, within {} days",
            CERTIFICATE_WARNING.num_days()
        );
    }
    Ok(format!("valid until {expires}"))
}

/// Whether a golden has a snapshot young enough to serve and no refresh has
/// failed since its last success, naming every problem found.
async fn golden_freshness(
    zfs: &dyn Zfs,
    config: &WorkerConfig,
    name: &str,
    record: &GoldenRecord,
    now: DateTime<Utc>,
) -> Result<String> {
    let dataset = Dataset(format!("{}/golden/{name}", config.pool));
    let newest = if zfs.exists(&dataset).await? {
        zfs.snapshots(&dataset)
            .await?
            .pop()
            .ok_or("no snapshot yet")
    } else {
        Err("never refreshed")
    };
    let age = newest.as_ref().ok().map(|newest| now - newest.created);
    let max_age = Duration::hours(i64::from(config.max_golden_age_hours));
    let problems: Vec<String> = [
        record.failure.as_ref().map(|failure| {
            format!(
                "refresh failed at {}: {}",
                failure.at.to_rfc3339_opts(SecondsFormat::Secs, true),
                failure.error
            )
        }),
        newest.as_ref().err().map(|missing| missing.to_string()),
        age.filter(|age| *age > max_age).map(|age| {
            format!(
                "snapshot {}h old, older than {}h",
                age.num_hours(),
                max_age.num_hours()
            )
        }),
    ]
    .into_iter()
    .flatten()
    .collect();
    match (problems.is_empty(), age) {
        (true, Some(age)) => Ok(format!("snapshot {}h old", age.num_hours())),
        _ => bail!("{}", problems.join("; ")),
    }
}

/// Whether each watched path on the golden's branch is still what its newest
/// snapshot was built from. A path that cannot be read now is noted, not
/// failed: the next prepare still reconciles the checkout.
async fn lockfiles(
    repos: &dyn Repos,
    golden: &GoldenConfig,
    token: &Result<Option<String>>,
    stamp: Option<&GoldenStamp>,
) -> Result<String> {
    let token = match token {
        Ok(token) => token.as_deref(),
        Err(e) => bail!("github token unreadable: {}", first_line(&e.to_string())),
    };
    let Some(stamp) = stamp else {
        bail!("no refresh has recorded {} yet", golden.watch.join(", "));
    };
    let (current, unread) = watched_shas(repos, golden, token).await;
    let stale: Vec<&str> = current
        .iter()
        .filter(|(path, sha)| stamp.watched.get(*path) != Some(*sha))
        .map(|(path, _)| path.as_str())
        .collect();
    if !stale.is_empty() {
        bail!(
            "stale: {} changed on {} since the snapshot",
            stale.join(", "),
            golden.branch
        );
    }
    if unread.is_empty() {
        Ok(format!(
            "unchanged on {} since {}",
            golden.branch, stamp.snapshot
        ))
    } else {
        Ok(format!("not checked: {}", unread.join("; ")))
    }
}

async fn toolbox_image(config: &WorkerConfig, containers: &dyn Containers) -> Result<String> {
    let image = config.toolbox_image()?;
    if !containers.has_image(&image).await? {
        bail!("{image} is not pulled");
    }
    Ok(image)
}

pub fn describe(checks: &[Check]) -> String {
    checks
        .iter()
        .map(|c| {
            format!(
                "{} {}: {}",
                if c.ok { "ok  " } else { "FAIL" },
                c.name,
                c.detail
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::Harness;
    use crate::refresh::{refresh, Selection};

    async fn run(h: &Harness, now: DateTime<Utc>) -> Vec<Check> {
        doctor(
            h.worker.config(),
            &*h.zfs,
            &*h.containers,
            &*h.cloud,
            &*h.repos,
            now,
        )
        .await
    }

    fn named<'a>(checks: &'a [Check], name: &str) -> &'a Check {
        checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no check {name}: {checks:?}"))
    }

    #[tokio::test]
    async fn the_doctor_reports_every_problem_not_the_first() {
        let h = Harness::new().await;
        let checks = run(&h, Utc::now()).await;
        let failed: Vec<&str> = checks
            .iter()
            .filter(|c| !c.ok)
            .map(|c| c.name.as_str())
            .collect();
        assert!(failed.contains(&"tls identity"), "{failed:?}");
        assert!(failed.contains(&"golden rails"), "{failed:?}");
        assert!(
            checks.iter().any(|c| c.ok && c.name == "pool"),
            "{checks:?}"
        );
        assert!(
            checks.iter().any(|c| c.ok && c.name == "toolbox image"),
            "{checks:?}"
        );
        assert!(describe(&checks).contains("FAIL golden rails: never refreshed"));
        assert!(named(&checks, "golden records").ok);
    }

    #[tokio::test]
    async fn a_fresh_golden_passes_and_one_past_max_golden_age_hours_fails() {
        let h = Harness::new().await;
        let made = Utc::now();
        h.zfs
            .add_snapshot_at("tank/golden/rails", "fresh", made - Duration::hours(1));
        let checks = run(&h, made).await;
        assert_eq!(named(&checks, "golden rails").detail, "snapshot 1h old");
        assert!(named(&checks, "golden rails").ok);

        let checks = run(&h, made + Duration::hours(36)).await;
        let rails = named(&checks, "golden rails");
        assert!(!rails.ok);
        assert_eq!(rails.detail, "snapshot 37h old, older than 36h");

        let checks = run(&h, made + Duration::minutes(35 * 60 + 30)).await;
        let rails = named(&checks, "golden rails");
        assert!(!rails.ok, "36h30m is past a 36h limit: {rails:?}");
        assert_eq!(rails.detail, "snapshot 36h old, older than 36h");

        let h = Harness::with(|config| config.max_golden_age_hours = 48).await;
        h.zfs
            .add_snapshot_at("tank/golden/rails", "fresh", made - Duration::hours(1));
        assert!(named(&run(&h, made + Duration::hours(36)).await, "golden rails").ok);
    }

    #[tokio::test]
    async fn the_doctor_reports_a_failed_refresh_until_a_later_one_succeeds() {
        let h = Harness::new().await;
        let night = Utc::now() - Duration::hours(2);
        h.zfs
            .add_snapshot_at("tank/golden/rails", "previous", night);
        h.containers.fail_next_exec();
        refresh(&h.worker, Selection::Named("rails"), night)
            .await
            .unwrap();
        let checks = run(&h, Utc::now()).await;
        let rails = named(&checks, "golden rails");
        assert!(!rails.ok);
        assert_eq!(
            rails.detail,
            format!(
                "refresh failed at {}: `chown 1001:1000 /work/rails` in /work failed (exit 1): boom",
                night.to_rfc3339_opts(SecondsFormat::Secs, true)
            )
        );

        refresh(&h.worker, Selection::Named("rails"), Utc::now())
            .await
            .unwrap();
        assert!(named(&run(&h, Utc::now()).await, "golden rails").ok);
    }

    #[tokio::test]
    async fn the_doctor_reports_a_golden_whose_lockfile_moved() {
        let h = Harness::new().await;
        let checks = run(&h, Utc::now()).await;
        let lockfiles = named(&checks, "golden rails lockfiles");
        assert!(!lockfiles.ok);
        assert_eq!(lockfiles.detail, "no refresh has recorded Gemfile.lock yet");

        refresh(&h.worker, Selection::Named("rails"), Utc::now())
            .await
            .unwrap();
        let checks = run(&h, Utc::now()).await;
        assert!(named(&checks, "golden rails lockfiles").ok, "{checks:?}");

        h.repos.set_sha("acme/rails", "Gemfile.lock", "lock-2");
        let checks = run(&h, Utc::now()).await;
        let lockfiles = named(&checks, "golden rails lockfiles");
        assert!(!lockfiles.ok);
        assert_eq!(
            lockfiles.detail,
            "stale: Gemfile.lock changed on main since the snapshot"
        );

        h.repos.remove("acme/rails", "Gemfile.lock");
        let checks = run(&h, Utc::now()).await;
        let lockfiles = named(&checks, "golden rails lockfiles");
        assert!(lockfiles.ok);
        assert_eq!(
            lockfiles.detail,
            "not checked: Gemfile.lock: Gemfile.lock on acme/rails@main → 404 Not Found"
        );
    }

    #[tokio::test]
    async fn an_unreadable_github_token_fails_the_lockfile_check() {
        let h = Harness::with(|config| {
            config.secrets.github_token = Some("/workforce/prod/worker/missing".into())
        })
        .await;
        let checks = run(&h, Utc::now()).await;
        assert_eq!(
            named(&checks, "golden rails lockfiles").detail,
            "github token unreadable: no parameter /workforce/prod/worker/missing"
        );
    }
}
