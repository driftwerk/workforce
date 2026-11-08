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

/// A config file as last read: its mtime, length and inode, so a file
/// replaced by another of the same size within the clock's grain is read
/// again.
type Stamp = Option<(SystemTime, u64, u64)>;

/// The fence of the agent `agent`, whose workspace is `workspace`.
pub(crate) struct ConfigFence {
    agent: AgentId,
    workspace: PathBuf,
    /// The fence as last read, by the config file's stamp then. A read that
    /// fails keeps the last good one.
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
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(&m);
    #[cfg(not(unix))]
    let ino = 0;
    Some((m.modified().ok()?, m.len(), ino))
}

/// A fence as [`workforce_agent::runner::CURRENT_MEMORY_FENCE`] carries it.
pub(crate) fn encode(agent: &AgentId, fence: &Fence) -> String {
    let f = match fence {
        Fence::Boss(tag) => tag.as_str(),
        Fence::Open => "open",
        Fence::Unknown => "unknown",
    };
    format!("{}\t{f}", agent.as_str())
}

/// The fence `s` carries for `agent`; none when it is another agent's.
fn decode(s: &str, agent: &AgentId) -> Option<Fence> {
    let (who, f) = s.split_once('\t')?;
    if who != agent.as_str() {
        return None;
    }
    Some(match f {
        "open" => Fence::Open,
        "unknown" => Fence::Unknown,
        tag => Fence::Boss(tag.to_string()),
    })
}

/// The fence of the agent of `workspace` as its config says now, for a
/// turn about to start: what the turn and the work it spawns are tagged
/// with ([`workforce_agent::runner::CURRENT_MEMORY_FENCE`]).
pub(crate) fn turn_fence(agent: &AgentId, workspace: &Path) -> String {
    encode(
        agent,
        &ConfigFence::new(agent.clone(), workspace.to_path_buf()).now(),
    )
}

impl ConfigFence {
    pub(crate) fn new(agent: AgentId, workspace: PathBuf) -> Self {
        Self {
            agent,
            workspace,
            read: Mutex::new(None),
        }
    }

    /// The fence as the config says now: read again when the file changed;
    /// a file that won't read keeps the last good fence, and with none the
    /// fence is unknown, which fails closed.
    fn now(&self) -> Fence {
        let at = stamp(&self.workspace);
        let last = self.read.lock().clone();
        if let Some((seen, fence)) = &last {
            if *seen == at {
                return fence.clone();
            }
        }
        let read = match AgentWorkspaceConfig::load(&self.workspace) {
            Ok(None) => Some(Fence::Open),
            Ok(Some(c)) => c.oversight().ok().map(|o| fence_of(&o)),
            Err(_) => None,
        };
        match read {
            Some(fence) => {
                *self.read.lock() = Some((at, fence.clone()));
                fence
            }
            None => {
                tracing::warn!(agent_id = %self.agent, "an agent's config doesn't read, so its memory keeps the fence it last had");
                last.map_or(Fence::Unknown, |(_, f)| f)
            }
        }
    }
}

impl FenceSource for ConfigFence {
    fn fence(&self) -> Fence {
        // A turn keeps the fence it began under.
        let of_turn = workforce_agent::runner::CURRENT_MEMORY_FENCE
            .try_with(|s| decode(s, &self.agent))
            .ok()
            .flatten();
        of_turn.unwrap_or_else(|| self.now())
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
        Arc::new(ConfigFence::new(agent.clone(), workspace.to_path_buf())),
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
        let fence = ConfigFence::new(AgentId::new("sarah"), dir.path().to_path_buf());
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

    #[tokio::test]
    async fn a_config_that_wont_read_keeps_the_last_fence_and_a_turn_keeps_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join(workforce_common::config_files::WORKSPACE_CONFIG);
        let sarah = AgentId::new("sarah");
        let fresh = ConfigFence::new(sarah.clone(), dir.path().to_path_buf());
        std::fs::write(&path, "this isn't toml [[[").unwrap();
        assert_eq!(
            fresh.fence(),
            Fence::Unknown,
            "nothing good read yet: closed"
        );
        let fence = ConfigFence::new(sarah.clone(), dir.path().to_path_buf());
        std::fs::write(&path, "[oversight]\nenabled = true\ngroup = \"g-acme\"\n").unwrap();
        assert_eq!(fence.fence(), Fence::Boss("desk-group:g-acme".into()));
        std::fs::write(&path, "[oversight]\nenabled = tru\n").unwrap();
        assert_eq!(
            fence.fence(),
            Fence::Boss("desk-group:g-acme".into()),
            "the last good one"
        );

        // A turn begun as Acme's boss stays Acme's, whatever the file says.
        let began = encode(&sarah, &Fence::Boss("desk-group:g-acme".into()));
        std::fs::write(&path, "[oversight]\nenabled = true\ngroup = \"g-atlas\"\n").unwrap();
        let seen = workforce_agent::runner::CURRENT_MEMORY_FENCE
            .scope(began.clone(), async { fence.fence() })
            .await;
        assert_eq!(seen, Fence::Boss("desk-group:g-acme".into()));
        // Another agent's turn says nothing of this one's.
        let mias = encode(&AgentId::new("mia"), &Fence::Open);
        let seen = workforce_agent::runner::CURRENT_MEMORY_FENCE
            .scope(mias, async { fence.fence() })
            .await;
        assert_eq!(seen, Fence::Boss("desk-group:g-atlas".into()));
    }
}
