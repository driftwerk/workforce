//! Where an agent's memory fence is read from: its own config.toml's
//! `[oversight]`, read again whenever the file changes (boss-gaps W8).
//! While it oversees, its memory is its scope's
//! ([`workforce_memory::group_tagged`]); otherwise it is open.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use parking_lot::Mutex;
use workforce_common::oversight::Oversight;
use workforce_common::settings::AgentWorkspaceConfig;
use workforce_core::types::AgentId;
use workforce_memory::group_tagged::{group_tag, Fence, FenceSource, GroupTagged};
use workforce_memory::store::MemoryStore;

/// A config file as last read: its mtime and length.
type Stamp = Option<(SystemTime, u64)>;

/// The fence of the agent whose workspace is `workspace`.
pub(crate) struct ConfigFence {
    workspace: PathBuf,
    /// The fence, by the config file's mtime and length when read.
    read: Mutex<Option<(Stamp, Fence)>>,
}

/// What an `[oversight]` makes of an agent's memory.
pub(crate) fn fence_of(o: &Oversight) -> Fence {
    match o.enabled {
        true => Fence::Boss(group_tag(o.scope.group())),
        false => Fence::Open,
    }
}

fn stamp(workspace: &Path) -> Stamp {
    let m =
        std::fs::metadata(workspace.join(workforce_common::config_files::WORKSPACE_CONFIG)).ok()?;
    Some((m.modified().ok()?, m.len()))
}

impl ConfigFence {
    pub(crate) fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            read: Mutex::new(None),
        }
    }
}

impl FenceSource for ConfigFence {
    fn fence(&self) -> Fence {
        let at = stamp(&self.workspace);
        if let Some((seen, fence)) = self.read.lock().as_ref() {
            if *seen == at {
                return fence.clone();
            }
        }
        // A config that doesn't read oversees nothing, as the crew reads it.
        let fence = AgentWorkspaceConfig::load(&self.workspace)
            .ok()
            .flatten()
            .and_then(|c| c.oversight().ok())
            .map_or(Fence::Open, |o| fence_of(&o));
        *self.read.lock() = Some((at, fence.clone()));
        fence
    }
}

/// `store` as the agent `agent` of `workspace` is handed it: fenced to its
/// scope while it oversees.
pub(crate) fn fenced(
    store: Arc<dyn MemoryStore>,
    agent: &AgentId,
    workspace: &Path,
) -> Arc<dyn MemoryStore> {
    Arc::new(GroupTagged::new(
        store,
        agent.clone(),
        Arc::new(ConfigFence::new(workspace.to_path_buf())),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_boss_is_fenced_to_its_scope_and_anyone_else_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join(workforce_common::config_files::WORKSPACE_CONFIG);
        let fence = ConfigFence::new(dir.path().to_path_buf());
        assert_eq!(fence.fence(), Fence::Open, "no config");
        std::fs::write(&path, "[oversight]\nenabled = true\ngroup = \"g-acme\"\n").unwrap();
        assert_eq!(fence.fence(), Fence::Boss("desk-group:g-acme".into()));
        std::fs::write(
            &path,
            "[oversight]\nenabled = true\ngroup = \"g-atlas\"\nlevel = \"act\"\n",
        )
        .unwrap();
        assert_eq!(
            fence.fence(),
            Fence::Boss("desk-group:g-atlas".into()),
            "read again once the file changes"
        );
        std::fs::write(&path, "[oversight]\nenabled = true\n").unwrap();
        assert_eq!(fence.fence(), Fence::Boss("desk-group:ungrouped".into()));
        std::fs::write(&path, "[oversight]\nenabled = false\ngroup = \"g-acme\"\n").unwrap();
        assert_eq!(fence.fence(), Fence::Open);
    }
}
