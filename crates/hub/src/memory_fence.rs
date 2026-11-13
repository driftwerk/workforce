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

/// The turns running now, by their agent's workspace: every admitted turn
/// (a person's, a heartbeat's, a monitor's, a scheduled job's, a check-in),
/// whatever surface started it. A boss is moved only once its count is 0.
static IN_FLIGHT: std::sync::LazyLock<Mutex<std::collections::HashMap<PathBuf, usize>>> =
    std::sync::LazyLock::new(Default::default);

/// One turn of the agent of a workspace, counted while it lives.
pub(crate) struct TurnInFlight(PathBuf);

impl TurnInFlight {
    pub(crate) fn begin(workspace: &Path) -> Self {
        *IN_FLIGHT.lock().entry(workspace.to_path_buf()).or_default() += 1;
        Self(workspace.to_path_buf())
    }
}

impl Drop for TurnInFlight {
    fn drop(&mut self) {
        let mut all = IN_FLIGHT.lock();
        if let Some(n) = all.get_mut(&self.0) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                all.remove(&self.0);
            }
        }
    }
}

/// How many turns the agent of `workspace` is running now.
pub(crate) fn turns_in_flight(workspace: &Path) -> usize {
    IN_FLIGHT.lock().get(workspace).copied().unwrap_or(0)
}

/// The fence a turn of `agent` on `channel` starting now carries.
///
/// In a Desk group's boss conversation (its Desk channel, or the Slack DM
/// thread its link opened), the group's own tag, whatever turn
/// it runs inside: everything remembered there is the group's, and only
/// that is recalled, never what the agent holds from elsewhere. A channel
/// that claims to be one but isn't (`session` says) remembers and recalls
/// nothing scoped. Anywhere else, the one it is inside already (a turn that
/// runs another for the same agent), else its config's; none of those
/// recalls a boss conversation's memories, which no other fence admits.
pub(crate) fn fence_for_turn(
    agent: &AgentId,
    workspace: &Path,
    channel: &str,
    session: impl FnOnce() -> Option<workforce_common::session::Session>,
) -> String {
    let boss_fence = |s: Option<&workforce_common::session::Session>| {
        let fence = s
            .and_then(|s| {
                crate::boss_links::conversation::BossConversation::of(s, agent.as_str(), &s.owner)
            })
            .map_or(Fence::Unknown, |c| Fence::Boss(c.memory_tag()));
        encode(agent, &fence)
    };
    if workforce_common::channels::is_desk_boss_channel(channel) {
        return boss_fence(session().as_ref());
    }
    // A Slack DM thread its link mapped to one: told by its session.
    if crate::boss_links::slack::dm_thread(channel).is_some() {
        if let Some(s) = session().filter(|s| s.claims_desk_boss()) {
            return boss_fence(Some(&s));
        }
    }
    workforce_agent::runner::CURRENT_MEMORY_FENCE
        .try_with(|s| s.clone())
        .ok()
        .filter(|s| decode(s, agent).is_some())
        .unwrap_or_else(|| turn_fence(agent, workspace))
}

/// Whether the turn running now is in a Desk group's boss conversation,
/// by the fence it carries: what it reads outside the memory store (a
/// legacy notes file) is left out.
pub(crate) fn in_boss_conversation() -> bool {
    workforce_agent::runner::CURRENT_MEMORY_FENCE
        .try_with(|s| {
            s.split_once('\t').is_some_and(|(_, f)| {
                f.starts_with(workforce_memory::group_tagged::LINK_BOSS_TAG_PREFIX)
            })
        })
        .unwrap_or(false)
}

/// The session metadata key a boss's conversations carry once it left the
/// scope they were had in: the scope's tag.
pub(crate) const LEFT_SCOPE: &str = workforce_common::session::LEFT_SCOPE_KEY;

/// Whether `s` was had while its agent was the boss of a scope it has left
/// since, or is a Desk group's boss conversation: never read again in
/// another conversation, nor recalled in a digest.
pub(crate) fn left_behind(s: &workforce_common::session::Session) -> bool {
    s.kept_apart()
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
        // Only the mark is written: a turn's own save of the session never
        // races a whole-session save of ours.
        let mut mark = serde_json::Map::new();
        mark.insert(LEFT_SCOPE.into(), serde_json::json!(tag));
        sessions.merge_metadata(&shell.id, mark, &[])?;
        if !shell.is_sealed() {
            sessions.start_fresh(&shell.channel, &shell.owner, agent_id)?;
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
        Arc::new(ConfigFence::new(agent.clone(), workspace.to_path_buf())),
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

    /// A Desk group's boss conversation of Allan's with Sam, as its link
    /// makes it.
    fn boss_conversation(key: &str) -> Session {
        let mut s = Session::new(&format!("desk:boss:{key}"), "sam", "allan");
        s.metadata = serde_json::json!({ "desk_boss": {
            "v": 1, "groupKey": key, "groupName": "Acme", "person": "sam", "agent": "allan",
        }});
        s
    }

    #[tokio::test]
    async fn a_boss_conversation_keeps_its_memory_and_none_other_both_ways() {
        use workforce_memory::types::{Memory, MemoryFilter, MemoryLayer};
        let dir = tempfile::tempdir().unwrap();
        let allan = AgentId::new("allan");
        let inner: Arc<dyn MemoryStore> =
            Arc::new(workforce_memory::in_memory::InMemoryStore::new());
        let store = fenced(Arc::clone(&inner), &allan, dir.path());
        let mem = |content: &str| {
            let mut m = Memory::new(content, MemoryLayer::Episodic, allan.clone());
            m.identity_id = Some("sam".into());
            m
        };
        let filter = || MemoryFilter {
            identity_id: Some("sam".into()),
            limit: Some(10),
            ..MemoryFilter::default()
        };
        let all = |store: Arc<dyn MemoryStore>| async move {
            let mut c: Vec<String> = store
                .list(filter())
                .await
                .unwrap()
                .into_iter()
                .map(|m| m.content)
                .collect();
            c.sort();
            c
        };
        let acme = boss_conversation("k-acme");
        let in_acme = fence_for_turn(&allan, dir.path(), &acme.channel, || Some(acme.clone()));
        let in_dm = fence_for_turn(&allan, dir.path(), "desk", || None);
        assert_eq!(in_dm, encode(&allan, &Fence::Open));
        let fence = &workforce_agent::runner::CURRENT_MEMORY_FENCE;

        // Remembered in a plain DM: untagged. In Acme's conversation: Acme's.
        fence
            .scope(in_dm.clone(), store.remember(mem("dm canary")))
            .await
            .unwrap();
        let acmes = fence
            .scope(in_acme.clone(), store.remember(mem("acme canary")))
            .await
            .unwrap();
        assert_eq!(
            inner.get(&acmes).await.unwrap().unwrap().tags,
            ["desk-group:boss/k-acme"]
        );
        // Each recalls its own alone.
        assert_eq!(
            fence.scope(in_acme.clone(), all(store.clone())).await,
            ["acme canary"]
        );
        assert_eq!(
            fence.scope(in_dm.clone(), all(store.clone())).await,
            ["dm canary"]
        );
        // A turn inside Acme's keeps Acme's fence; Acme's never inherits a
        // DM's.
        let nested = fence
            .scope(in_acme.clone(), async {
                fence_for_turn(&allan, dir.path(), "subagent:x", || None)
            })
            .await;
        assert_eq!(nested, in_acme);
        let entered = fence
            .scope(in_dm.clone(), async {
                fence_for_turn(&allan, dir.path(), &acme.channel, || Some(acme.clone()))
            })
            .await;
        assert_eq!(entered, in_acme);
        // Another group's conversation reads none of Acme's.
        let globex = boss_conversation("k-globex");
        let in_globex =
            fence_for_turn(&allan, dir.path(), &globex.channel, || Some(globex.clone()));
        assert!(fence.scope(in_globex, all(store.clone())).await.is_empty());

        // A channel that only claims to be one remembers nothing and recalls
        // none of the group's (a turn can't be had there: nothing opens it).
        let mut forged = Session::new("desk:boss:k-acme", "sam", "allan");
        for s in [forged.clone(), {
            forged.metadata = serde_json::json!({ "desk_boss": { "groupKey": "k-acme", "person": "mia", "agent": "allan" }});
            forged.clone()
        }] {
            let f = fence_for_turn(&allan, dir.path(), &s.channel, || Some(s.clone()));
            assert_eq!(f, encode(&allan, &Fence::Unknown));
            assert_eq!(
                fence.scope(f.clone(), all(store.clone())).await,
                ["dm canary"],
                "never the group's"
            );
            assert!(fence
                .scope(f, store.remember(mem("planted")))
                .await
                .is_err());
        }
        assert_eq!(
            fence_for_turn(&allan, dir.path(), "DESK:BOSS:k-acme", || None),
            encode(&allan, &Fence::Unknown)
        );
    }

    #[test]
    fn a_boss_conversation_is_never_read_in_another() {
        let acme = boss_conversation("k-acme");
        assert!(left_behind(&acme));
        let mut dm = Session::new("desk", "sam", "allan");
        dm.context_type = Some(SessionContextType::Dm);
        let mut talked = acme.clone();
        talked.context_type = Some(SessionContextType::Dm);
        talked.add_user_message("acme canary", None);
        assert!(!crate::session_digest::digest_source_eligible(
            &talked,
            &dm,
            "allan",
            chrono::DateTime::<chrono::Utc>::MIN_UTC,
            &Default::default(),
            &Default::default(),
        ));
        // Its successor stays one.
        let next = {
            let mut n = acme.successor();
            n.adopt_conversation_mode(&acme);
            n
        };
        assert!(
            crate::boss_links::conversation::BossConversation::of(&next, "allan", "sam").is_some()
        );
    }

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
        // A turn begun inside another of the same agent's keeps the outer
        // one's fence; one begun fresh reads the config.
        let inner = workforce_agent::runner::CURRENT_MEMORY_FENCE
            .scope(began.clone(), async {
                fence_for_turn(&sarah, dir.path(), "desk", || None)
            })
            .await;
        assert_eq!(inner, began);
        assert_eq!(
            fence_for_turn(&sarah, dir.path(), "desk", || None),
            encode(&sarah, &Fence::Boss("desk-group:g-atlas".into()))
        );
        // A turn counts while it lives.
        {
            let _one = TurnInFlight::begin(dir.path());
            let _two = TurnInFlight::begin(dir.path());
            assert_eq!(turns_in_flight(dir.path()), 2);
        }
        assert_eq!(turns_in_flight(dir.path()), 0);
        // Another agent's turn says nothing of this one's.
        let mias = encode(&AgentId::new("mia"), &Fence::Open);
        let seen = workforce_agent::runner::CURRENT_MEMORY_FENCE
            .scope(mias, async { fence.fence() })
            .await;
        assert_eq!(seen, Fence::Boss("desk-group:g-atlas".into()));
    }
}
