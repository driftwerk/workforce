//! The definition store the workflow, playbook, monitor and consolidation
//! runtimes share, one per process.

use std::sync::Arc;

use tracing::{info, warn};
use workforce_common::settings::WorkforceConfig;

/// Process-wide shared workflow/playbook definition store (DW-550). Built
/// lazily from `[postgres]`; on first build it seeds existing disk TOMLs into
/// the DB (DW-548). `None` when Postgres is disabled — runtimes then fall back
/// to their on-disk definition directories.
static SHARED_WORKFLOW_STORE: tokio::sync::OnceCell<
    Option<Arc<workforce_orchestrator::workflow_store::WorkflowStore>>,
> = tokio::sync::OnceCell::const_new();

pub async fn shared_workflow_store(
    config: &WorkforceConfig,
) -> Option<Arc<workforce_orchestrator::workflow_store::WorkflowStore>> {
    use workforce_orchestrator::workflow_store::WorkflowStore;
    SHARED_WORKFLOW_STORE
        .get_or_init(|| async {
            let pg = config.postgres.as_ref().filter(|p| p.enabled)?;
            let pool = match WorkflowStore::build_pool(pg) {
                Ok(p) => p,
                Err(e) => {
                    warn!(error = ?e, "WorkflowStore pool init failed; disk fallback");
                    return None;
                }
            };
            let store =
                match WorkflowStore::with_retention(pool, config.revisions.max_per_def).await {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(error = ?e, "WorkflowStore init failed; disk fallback");
                        return None;
                    }
                };
            let data_dir = std::path::PathBuf::from(&config.hub.data_dir);
            seed_workflow_defs(&store, &data_dir.join("workflows")).await;
            seed_playbook_defs(&store, &data_dir.join("playbooks")).await;
            info!("Workflow/playbook definition store initialised");
            Some(store)
        })
        .await
        .clone()
}

/// Seed disk workflow TOMLs into `workflow_defs` (idempotent, DW-548/550).
async fn seed_workflow_defs(
    store: &workforce_orchestrator::workflow_store::WorkflowStore,
    dir: &std::path::Path,
) {
    use workforce_orchestrator::workflow::definition::WorkflowDefinition;
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut n = 0usize;
    for entry in rd.flatten() {
        let p = entry.path();
        if p.extension().is_some_and(|e| e == "toml") {
            if let Ok(content) = std::fs::read_to_string(&p) {
                if let Ok(def) = WorkflowDefinition::from_toml(&content) {
                    if let Ok(true) = store.seed(&def.workflow.id, "workflow", &content).await {
                        n += 1;
                    }
                }
            }
        }
    }
    if n > 0 {
        info!(seeded = n, "Seeded workflow definitions into DB");
    }
}

/// Seed disk playbook TOMLs into `workflow_defs` (idempotent, DW-548/550).
async fn seed_playbook_defs(
    store: &workforce_orchestrator::workflow_store::WorkflowStore,
    dir: &std::path::Path,
) {
    use workforce_orchestrator::playbook::PlaybookDefinition;
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut n = 0usize;
    for entry in rd.flatten() {
        let p = entry.path();
        if p.extension().is_some_and(|e| e == "toml") {
            if let Ok(content) = std::fs::read_to_string(&p) {
                if let Ok(def) = PlaybookDefinition::from_toml(&content) {
                    if let Ok(true) = store.seed(&def.playbook.id, "playbook", &content).await {
                        n += 1;
                    }
                }
            }
        }
    }
    if n > 0 {
        info!(seeded = n, "Seeded playbook definitions into DB");
    }
}
