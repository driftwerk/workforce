//! Agent export/import: bundle an agent's config, memories and transcripts into
//! a portable zip and merge it into another install. The reconcile/remap layer
//! is the substance;

mod bundle;
mod export;
mod import;
mod model;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use workforce_common::locking::LockManager;
use workforce_common::settings::WorkforceConfig;
use workforce_identity::store::IdentityStore;
use workforce_memory::store::MemoryStore;
use workforce_orchestrator::workflow_store::WorkflowStore;

pub use bundle::{BundleReader, BundleWriter};
pub use export::export_bundle;
pub use import::{apply_import, plan_import};
pub use model::{
    AgentPlan, Counts, IdentityResolution, IdentityResolutionKind, ImportOptions, ImportOutcome,
    Manifest, MergePlan,
};

/// Borrows the stores a transfer touches and identifies *this* install.
/// `config` must reflect the *effective* config (base ⊕ overlay) so collision
/// detection sees previously-imported agents — the CLI reloads it per
/// invocation, the API after each registry reload.
pub struct TransferContext<'a> {
    pub data_dir: PathBuf,
    pub base_config_path: PathBuf,
    pub config: &'a WorkforceConfig,
    pub identity_store: &'a dyn IdentityStore,
    pub memory_store: &'a dyn MemoryStore,
    pub lock_manager: &'a LockManager,
    pub def_store: Option<&'a WorkflowStore>,
    pub actor: Option<String>,
    pub install_id: String,
    pub slack_team_id: Option<String>,
}

impl TransferContext<'_> {
    /// A Slack user-id is a safe identity join only when the bundle's source
    /// workspace matches this install's — U-ids are workspace-scoped.
    fn slack_workspace_matches(&self, source_team: Option<&str>) -> bool {
        matches!(
            (source_team, self.slack_team_id.as_deref()),
            (Some(a), Some(b)) if a == b
        )
    }
}

fn resolve_workspace(ctx: &TransferContext<'_>, agent_id: &str) -> PathBuf {
    match ctx
        .config
        .agent_by_id(agent_id)
        .and_then(|a| a.workspace.as_ref())
    {
        Some(workspace) => {
            let path = Path::new(workspace);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                ctx.base_config_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(path)
            }
        }
        None => ctx.data_dir.join("agents").join(agent_id),
    }
}
