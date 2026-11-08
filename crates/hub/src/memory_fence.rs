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

/// The session metadata key a boss's conversations carry once it left the
/// scope they were had in: the scope's tag.
pub(crate) const LEFT_SCOPE: &str = "desk_scope_left";

/// Whether `s` was had while its agent was the boss of a scope it has left
/// since: never read again, nor recalled in a digest.
pub(crate) fn left_behind(s: &workforce_common::session::Session) -> bool {
    s.metadata.get(LEFT_SCOPE).is_some()
}

/// Every conversation of `sessions`' agent is left with the scope `tag` it
/// leaves: marked as that scope's, and each one still open sealed with a
/// fresh one in its place, so nothing said there is replayed in the next
/// scope's turns. Returns how many were marked.
pub(crate) fn leave_sessions(
    sessions: &dyn workforce_common::session::SessionStore,
    agent_id: &str,
    tag: &str,
) -> eyre::Result<usize> {
    let mut marked = 0;
    for shell in sessions.list(None, 100_000)? {
        if shell.agent_id != agent_id || left_behind(&shell) {
            continue;
        }
        let Some(mut s) = sessions.get(&shell.id)? else {
            continue;
        };
        if let Some(meta) = s.metadata.as_object_mut() {
            meta.insert(LEFT_SCOPE.into(), serde_json::json!(tag));
        } else {
            s.metadata = serde_json::json!({ LEFT_SCOPE: tag });
        }
        let open = !s.is_sealed();
        sessions.save(&s)?;
        if open {
            sessions.start_fresh(&s.channel, &s.owner, agent_id)?;
        }
        marked += 1;
    }
    Ok(marked)
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
    use workforce_agent::tools::Tool as _;
    use workforce_common::session::{
        InMemorySessionStore, Session, SessionContextType, SessionStore as _,
    };

    #[tokio::test]
    async fn a_boss_leaving_a_scope_leaves_its_conversations_there_unread() {
        let store = Arc::new(InMemorySessionStore::new());
        let mut dm = store.get_or_create("desk", "person-1", "sarah").unwrap();
        dm.context_type = Some(SessionContextType::Dm);
        dm.add_user_message("the acmecanary pricing", None);
        store.save(&dm).unwrap();
        let mut slack = store
            .get_or_create("slack:C1:T1", "person-1", "sarah")
            .unwrap();
        slack.add_user_message("acmecanary in Slack too", None);
        store.save(&slack).unwrap();
        let mut theirs = store.get_or_create("desk", "person-2", "mia").unwrap();
        theirs.add_user_message("Mia's own", None);
        store.save(&theirs).unwrap();

        assert_eq!(
            leave_sessions(store.as_ref(), "sarah", "desk-group:acme").unwrap(),
            2
        );
        assert_eq!(
            leave_sessions(store.as_ref(), "sarah", "desk-group:acme").unwrap(),
            2,
            "the fresh ones are marked on a second leave, never the old twice"
        );
        let old = store.get(&dm.id).unwrap().unwrap();
        assert!(old.is_sealed() && left_behind(&old));
        let now = store.find("desk", "person-1").unwrap();
        let now = now.filter(|s| s.agent_id == "sarah");
        // The DM goes on in a fresh conversation, with none of Acme's.
        if let Some(fresh) = now {
            assert_ne!(fresh.id, dm.id);
            assert!(fresh
                .messages
                .iter()
                .all(|m| !m.content.contains("acmecanary")));
        }
        assert!(
            !left_behind(&store.get(&theirs.id).unwrap().unwrap()),
            "only Sarah's"
        );
        // Never digested into the next DM, nor found by her own search.
        let target = Session::new("desk", "person-1", "sarah");
        assert!(!crate::session_digest::digest_source_eligible(
            &old,
            &target,
            "sarah",
            chrono::DateTime::<chrono::Utc>::MIN_UTC,
            &Default::default(),
            &Default::default(),
        ));
        let tool = crate::sessions_tool::SessionsTool::new(store.clone());
        let out = workforce_agent::runner::CURRENT_USER_ID
            .scope("person-1".to_string(), async {
                tool.execute(serde_json::json!({ "action": "search", "query": "acmecanary" }))
                    .await
                    .unwrap()
            })
            .await;
        assert_eq!(out.output["count"].as_u64(), Some(0), "{}", out.output);
    }

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
