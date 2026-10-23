//! Running the daemon: the real ports, a reconciled worker, the sweeper, and
//! the TLS listener.

use std::sync::Arc;
use std::time::Duration;

use eyre::{Context, Result};
use tokio::signal::unix::{signal, SignalKind};
use tracing::{info, warn};
use workforce_agent::tools::exec_runner::{serve as serve_runner, Listener, TlsIdentity};

use crate::config::WorkerConfig;
use crate::handler::WorkerHandler;
use crate::host::{self, HostContainers, HostRepos, HostZfs};
use crate::scopes::Worker;

const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

pub fn identity(config: &WorkerConfig) -> Result<TlsIdentity> {
    let read = |path: &std::path::Path| {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
    };
    Ok(TlsIdentity {
        ca_pem: read(&config.ca_file)?,
        cert_pem: read(&config.cert_file)?,
        key_pem: read(&config.key_file)?,
    })
}

/// A worker over the host's real pool, runtime and cloud.
pub async fn worker(config: WorkerConfig) -> Result<Arc<Worker>> {
    let cloud = host::cloud(&config);
    Ok(Arc::new(
        Worker::new(
            Arc::new(config),
            Arc::new(HostZfs),
            Arc::new(HostContainers),
            cloud,
            Arc::new(HostRepos::default()),
        )
        .await?,
    ))
}

pub async fn serve(config: WorkerConfig) -> Result<()> {
    let listener = Listener::Tls {
        addr: config.listen_addr(),
        identity: identity(&config)?,
        accept_common_name: config.accept_common_name.clone(),
    };
    let worker = worker(config).await?;
    worker.reconcile().await?;
    worker.resume_jobs().await?;
    info!(
        scopes = worker.scopes().len(),
        jobs = worker.running_jobs(),
        image = worker.image(),
        "worker reconciled"
    );
    tokio::spawn(sweep_forever(Arc::clone(&worker)));
    let mut terminate = signal(SignalKind::terminate()).context("watching for SIGTERM")?;
    tokio::select! {
        served = serve_runner(listener, Arc::new(WorkerHandler::new(worker))) => served,
        _ = terminate.recv() => {
            info!("stopping; durable jobs keep running and are resumed at the next start");
            Ok(())
        }
    }
}

async fn sweep_forever(worker: Arc<Worker>) {
    let mut interval = tokio::time::interval(SWEEP_INTERVAL);
    loop {
        interval.tick().await;
        match worker.sweep(chrono::Utc::now()).await {
            Ok(released) if !released.is_empty() => {
                info!(count = released.len(), "swept expired scopes")
            }
            Ok(_) => {}
            Err(e) => warn!(error = %e, "sweep failed"),
        }
    }
}
