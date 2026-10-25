//! `workforce shadow` (DW-676, PR 9c): shadow mode's status and weekly
//! report, read from Postgres as the hub keeps it, and what pausing or
//! resuming takes.
//!
//! `pause` and `resume` change nothing: `[vellev.shadow] paused` is read
//! once at start-up, so they print the config change and the restart it
//! needs (a live switch would be a mechanism of its own, not built).
//!
//! `promote` and `rollback` (DW-676, PR 11) hand a shadowed store to
//! vellev, or back to Weaviate, with every hub stopped.

use std::path::Path;
use std::sync::Arc;

use chrono::{NaiveDate, Utc};
use clap::Subcommand;
use eyre::{eyre, Result};
use workforce_common::settings::WorkforceConfig;
use workforce_hub::{shadow_admin, shadow_report, shadow_status};

/// A store shadow mode compares on vellev.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ShadowedStore {
    Memories,
    Artifacts,
}

#[derive(Subcommand)]
pub enum ShadowCommand {
    /// What is shadowed, whether paused, the memory mirror's backlog, and
    /// the weekly verdicts recorded; with `--hub`, that hub process's own
    /// counts too.
    Status {
        /// Workforce config file (reads `[postgres]`, `[memory]`,
        /// `[artifacts]` and `[vellev.shadow]`).
        #[arg(short, long, default_value = "workforce.toml")]
        config: std::path::PathBuf,
        /// A hub's base URL (e.g. http://localhost:8080), whose
        /// `/api/health` shadow block is shown: the counts of whichever
        /// replica answers, never summed.
        #[arg(long)]
        hub: Option<String>,
        /// Gateway auth token for `--hub` (or set WORKFORCE_GATEWAY_TOKEN).
        #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN", hide_env_values = true)]
        token: Option<String>,
    },
    /// A store's week judged against the exit criteria, from Postgres.
    Report {
        #[arg(short, long, default_value = "workforce.toml")]
        config: std::path::PathBuf,
        /// `memories` or `artifacts`.
        #[arg(long)]
        store: String,
        /// The week's first UTC day (YYYY-MM-DD); the seven days before
        /// today when left out.
        #[arg(long)]
        week: Option<NaiveDate>,
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print how to pause shadow mode: nothing is changed.
    Pause {
        #[arg(short, long, default_value = "workforce.toml")]
        config: std::path::PathBuf,
    },
    /// Print how to resume shadow mode: nothing is changed.
    Resume {
        #[arg(short, long, default_value = "workforce.toml")]
        config: std::path::PathBuf,
    },
    /// Make vellev serve a shadowed store, with every hub stopped: refused
    /// unless no hub runs, the week's verdict passed, and a last reconcile
    /// and a second, clean pass leave nothing to fix. Run on the shadow
    /// config, then switch the store's backend to "vellev" and start the
    /// hubs. See docs/vellev.md, "Promoting vellev".
    Promote {
        store: ShadowedStore,
        /// Workforce config file: the shadow config the hubs ran on.
        #[arg(short, long, default_value = "workforce.toml")]
        config: std::path::PathBuf,
        /// Promote the artifact index on a "too few to judge" verdict: the
        /// owner's sign-off under A14 (design r2 section 6.5). Never for
        /// memories.
        #[arg(long)]
        accept_too_few: bool,
    },
    /// Hand a promoted store back to Weaviate, with every hub stopped:
    /// memories written on vellev since the promotion are carried back
    /// first. Run on the shadow config the hubs start on again.
    Rollback {
        store: ShadowedStore,
        /// Workforce config file.
        #[arg(short, long, default_value = "workforce.toml")]
        config: std::path::PathBuf,
    },
}

/// What `pause` (`want` true) or `resume` prints, given whether the config
/// at `path` is paused now. It changes nothing.
pub fn switch_advice(path: &Path, paused_now: bool, want: bool) -> String {
    let (verb, value) = if want {
        ("pause", "true")
    } else {
        ("resume", "false")
    };
    let mut out = String::new();
    if paused_now == want {
        out.push_str(&format!(
            "{} already has [vellev.shadow] paused = {paused_now}; a hub started since reads it so.\n",
            path.display()
        ));
    }
    out.push_str(&format!(
        "Nothing was changed. To {verb} shadow mode:\n\
         \x20 1. set, in {} on every hub host:\n\
         \x20      [vellev.shadow]\n\
         \x20      paused = {value}\n\
         \x20 2. restart every hub replica: paused is read once at start-up.\n\
         Paused stops the read comparisons, the memory mirror's noter and applier, the reconcile and the known-item runs. \
         Erasure still reaches vellev's replica, and the artifact mirror keeps vellev's artifact index in step, paused or not.\n",
        path.display()
    ));
    out
}

pub async fn handle(command: ShadowCommand) -> Result<()> {
    match command {
        ShadowCommand::Pause { config } => {
            let paused = WorkforceConfig::load(&config)?.vellev.shadow.paused;
            print!("{}", switch_advice(&config, paused, true));
        }
        ShadowCommand::Resume { config } => {
            let paused = WorkforceConfig::load(&config)?.vellev.shadow.paused;
            print!("{}", switch_advice(&config, paused, false));
        }
        ShadowCommand::Status { config, hub, token } => {
            status(&config, hub.as_deref(), token.as_deref()).await?
        }
        ShadowCommand::Report {
            config,
            store,
            week,
            json,
        } => {
            let wf = WorkforceConfig::load(&config)?;
            let store = shadow_report::store_named(&store)
                .ok_or_else(|| eyre!("no shadowed store {store:?}: memories or artifacts"))?;
            // Read only: this never builds or migrates the tables it reads.
            let tables = workforce_hub::shadow::read_only_store(&wf)
                .await?
                .ok_or_else(|| eyre!("nothing is shadowed in {}", config.display()))?;
            let from = week.unwrap_or_else(|| shadow_admin::default_week(Utc::now().date_naive()));
            let report =
                shadow_report::judge(&shadow_report::gather(&tables, &wf, store, from).await?);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&shadow_report::to_json(&report))?
                );
            } else {
                print!("{}", shadow_report::to_text(&report));
            }
        }
        ShadowCommand::Promote {
            store,
            config,
            accept_too_few,
        } => promote(store, &config, accept_too_few).await?,
        ShadowCommand::Rollback { store, config } => {
            use workforce_hub::promotion;
            let say = |done: &str| println!("  {done}");
            let wf_config = WorkforceConfig::load(&config)?;
            match store {
                ShadowedStore::Memories => promotion::rollback_memories(&wf_config, &say).await?,
                ShadowedStore::Artifacts => promotion::rollback_artifacts(&wf_config, &say).await?,
            }
        }
    }
    Ok(())
}

async fn promote(store: ShadowedStore, config: &Path, accept_too_few: bool) -> Result<()> {
    use workforce_hub::promotion;
    let say = |done: &str| println!("  {done}");
    let wf_config = WorkforceConfig::load(config)?;
    let verdicts: Arc<dyn promotion::VerdictSource> = Arc::new(promotion::Unrecorded);
    match store {
        ShadowedStore::Memories if accept_too_few => {
            eyre::bail!(
                "--accept-too-few is for the artifact index only: memories are promoted on a pass"
            )
        }
        ShadowedStore::Memories => {
            promotion::promote_memories(&wf_config, verdicts, &say).await?;
            println!(
                "Next: set [memory] backend = \"vellev\" in {} and start the hubs.",
                config.display()
            );
        }
        ShadowedStore::Artifacts => {
            promotion::promote_artifacts(&wf_config, verdicts, accept_too_few, &say).await?;
            println!(
                "Next: set [artifacts] index_backend = \"vellev\" in {} and start the hubs.",
                config.display()
            );
        }
    }
    Ok(())
}

/// The request `--hub` reads health with: the gateway token, when given,
/// as a bearer, as every other hub call the CLI makes.
pub fn health_request(
    client: &reqwest::Client,
    hub: &str,
    token: Option<&str>,
) -> reqwest::RequestBuilder {
    let url = format!("{}/api/health", hub.trim_end_matches('/'));
    let request = client.get(url).timeout(std::time::Duration::from_secs(10));
    match token {
        Some(token) => request.bearer_auth(token),
        None => request,
    }
}

async fn status(config: &Path, hub: Option<&str>, token: Option<&str>) -> Result<()> {
    let wf = WorkforceConfig::load(config)?;
    let shadowed = wf.shadowed();
    if shadowed.is_empty() {
        println!("Nothing is shadowed in {}.", config.display());
        return Ok(());
    }
    println!("Shadowed: {}", shadowed.join(", "));
    println!(
        "Paused: {} (read once at start-up: a change needs every hub replica restarted)",
        wf.vellev.shadow.paused
    );
    // Read only: this never builds or migrates the tables it reads.
    let tables = workforce_hub::shadow::read_only_store(&wf)
        .await?
        .ok_or_else(|| eyre!("nothing is shadowed in {}", config.display()))?;
    if wf.memory.backend == "shadow" {
        let read =
            shadow_status::await_backlog(tables.mirror_backlog(), shadow_status::BACKLOG_LIMIT)
                .await;
        match read {
            shadow_status::BacklogSeen::Known(b) => println!(
                "Memory mirror backlog: {} pending{}",
                b.pending,
                b.oldest
                    .map(|at| format!(", the oldest noted {at}"))
                    .unwrap_or_default()
            ),
            shadow_status::BacklogSeen::Unknown(why) => {
                println!("Memory mirror backlog: unknown ({why})")
            }
        }
    }
    let verdicts = tables.verdicts(None, 8).await?;
    if verdicts.is_empty() {
        println!("Weekly verdicts: none recorded yet");
    }
    for v in verdicts {
        println!(
            "Week {} to {}, {}: {}",
            v.week_start, v.week_end, v.store, v.verdict
        );
    }
    if let Some(hub) = hub {
        let health: serde_json::Value = health_request(&reqwest::Client::new(), hub, token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        println!(
            "This hub process only (whichever replica answered {hub}; the other's counts are its own):"
        );
        println!("{}", serde_json::to_string_pretty(&health["shadow"])?);
    } else {
        println!("Per-process counts (kernel drops, Deferred drops, the noter): pass --hub, or read /api/metrics, summed across replicas.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `pause` and `resume` print the change and the restart, and write
    /// nothing.
    #[tokio::test]
    async fn pause_and_resume_print_and_change_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workforce.toml");
        let written = "[vellev.shadow]\npaused = false\n";
        std::fs::write(&path, written).unwrap();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        handle(ShadowCommand::Pause {
            config: path.clone(),
        })
        .await
        .unwrap();
        handle(ShadowCommand::Resume {
            config: path.clone(),
        })
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), written);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

        let said = switch_advice(&path, false, true);
        assert!(said.contains("Nothing was changed"), "{said}");
        assert!(said.contains("paused = true"), "{said}");
        assert!(said.contains("restart every hub replica"), "{said}");
        let said = switch_advice(&path, true, true);
        assert!(said.contains("already has"), "{said}");
    }

    /// `--hub` reads health with the gateway token, as a bearer.
    #[test]
    fn health_is_read_with_the_token() {
        let client = reqwest::Client::new();
        let sent = health_request(&client, "http://hub:8080/", Some("t0k"))
            .build()
            .unwrap();
        assert_eq!(sent.url().as_str(), "http://hub:8080/api/health");
        assert_eq!(sent.headers()["authorization"], "Bearer t0k");
        let sent = health_request(&client, "http://hub:8080", None)
            .build()
            .unwrap();
        assert!(sent.headers().get("authorization").is_none());
    }
}
