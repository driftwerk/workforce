//! A Desk group's boss conversation in Slack (remote bosses, RB6b, design
//! §4.5): a thread in the person's DM with the agent.
//!
//! - **A DM thread is the person's own conversation.** A Slack session's
//!   context is `Dm` whenever its channel is an IM (`D…`), thread or not
//!   ([`crate::turn_loop::prompt::classify_context_type`]: only an IM is
//!   private, and that wins over the thread), so it passes
//!   `is_own_conversation`. A channel, shared or private, never is one.
//! - **Only the link opens one.** [`open_thread`] posts the thread's first
//!   message as the agent ("Acme · Desk") and makes its session, keyed by
//!   the thread, owned by the person, carrying the link's metadata with the
//!   thread's key ([`THREAD_KEY`]). Nothing a person or a model sends writes
//!   that metadata.
//! - **The thread is its own session**, whatever `thread_isolation` says:
//!   [`session_key_for`] routes a message in a DM thread to the thread's
//!   session when, and only when, the agent the DM routes to holds that
//!   session as a real boss conversation of the sender's. Any other thread
//!   is the DM, as before.

use serde_json::{json, Value};
use workforce_common::channels::OutboundMessage;
use workforce_common::session::{Session, SessionContextType};

use super::conversation::{BossConversation, META_KEY};
use crate::serve::AgentService;

/// The key, in a boss conversation's metadata, of the Slack thread it is
/// had in: the thread's session key. On the Desk conversation it names the
/// thread opened for it.
pub const THREAD_KEY: &str = "slackThread";

/// The session metadata key on a group's Desk boss conversation that names
/// its Slack thread, once opened.
pub const OPENED_KEY: &str = "desk_boss_slack_thread";

/// `key` as a Slack DM thread's session key (`slack:<workspace>:<D…>:<ts>`):
/// its workspace, IM channel and thread. None for any other key: a channel's
/// thread, a DM itself, anything else.
pub fn dm_thread(key: &str) -> Option<(&str, &str, &str)> {
    let rest = key.strip_prefix("slack:")?;
    let mut parts = rest.rsplitn(3, ':');
    let thread = parts.next()?;
    let channel = parts.next()?;
    let workspace = parts.next()?;
    let ok = !workspace.is_empty()
        && channel.starts_with('D')
        && channel.len() > 1
        && !thread.is_empty()
        && thread.bytes().all(|b| b.is_ascii_digit() || b == b'.');
    ok.then_some((workspace, channel, thread))
}

/// Whether `session` is a Slack DM thread its link mapped to a boss
/// conversation: the thread its metadata names, a DM.
pub fn is_mapped_thread(session: &Session, meta: &Value) -> bool {
    dm_thread(&session.channel).is_some()
        && meta.get(THREAD_KEY).and_then(Value::as_str) == Some(session.channel.as_str())
        && session.context_type == Some(SessionContextType::Dm)
}

/// The session a Slack message runs in: `default` (the DM, or a thread of
/// it when threads are isolated), unless it is in a DM thread that
/// `service`'s agent holds as a boss conversation of `owner`'s, which is
/// then the thread's own session.
pub fn session_key_for(
    service: &AgentService,
    workspace: &str,
    channel: &str,
    thread: Option<&str>,
    owner: &str,
    default: String,
) -> String {
    let Some(thread) = thread else {
        return default;
    };
    let key = crate::slack_manager::thread_session_key(workspace, channel, thread);
    if dm_thread(&key).is_none() {
        return default;
    }
    let held = service
        .sessions
        .find(&key, owner)
        .ok()
        .flatten()
        .is_some_and(|s| BossConversation::of(&s, service.agent_id.as_str(), owner).is_some());
    if held {
        key
    } else {
        default
    }
}

/// What opening a group's Slack thread came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Thread {
    /// Opened now: its session key.
    Opened(String),
    /// Opened before: its session key.
    Already(String),
    /// Not opened, and why, in words for the person.
    Unavailable(&'static str),
}

impl Thread {
    pub fn view(&self) -> Value {
        match self {
            Self::Opened(key) | Self::Already(key) => json!({ "state": "open", "key": key }),
            Self::Unavailable(why) => json!({ "state": "unavailable", "why": why }),
        }
    }
}

/// One opening at a time, so two calls never post two threads.
static OPENING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Opens the Slack thread of `boss`, the group's Desk boss conversation of
/// `service`'s agent with `person` (whose link metadata is `meta`), in the
/// person's DM with the agent: once. Unavailable when the agent has no
/// Slack, the person no Slack account linked to them, or Slack refuses.
pub async fn open_thread(service: &AgentService, boss: &mut Session, meta: &Value) -> Thread {
    let _one = OPENING.lock().await;
    if let Ok(Some(fresh)) = service.sessions.get(&boss.id) {
        *boss = fresh;
    }
    if let Some(key) = boss.metadata.get(OPENED_KEY).and_then(Value::as_str) {
        let still = service
            .sessions
            .find(key, &boss.owner)
            .ok()
            .flatten()
            .is_some_and(|s| {
                BossConversation::of(&s, service.agent_id.as_str(), &boss.owner).is_some()
            });
        if still {
            return Thread::Already(key.to_string());
        }
    }
    let slack_user = service.resolve_platform_user_id(&boss.owner);
    if slack_user.is_empty() {
        return Thread::Unavailable("Your Slack account isn't linked on this hub.");
    }
    let registry = service.surface_registry.read().await;
    let Ok((surface, dm)) = registry
        .resolve_user_dm(
            "slack",
            &slack_user,
            workforce_common::channels::ChannelHint::default(),
        )
        .await
    else {
        return Thread::Unavailable("The agent can't open a Slack DM with you.");
    };
    let Some(workspace) = registry.workspace_of("slack", &dm).await else {
        return Thread::Unavailable("The agent's Slack workspace can't be told.");
    };
    drop(registry);
    let group: String = meta["groupName"]
        .as_str()
        .unwrap_or("Your group")
        .chars()
        .filter(|c| !c.is_control())
        .take(80)
        .collect();
    let first = format!(
        "*{group} · Desk*\nTalk to me about {group} in this thread: here I can see and run its sessions, as your Desk lets me. Anywhere else, I can't."
    );
    let Ok(sent) = surface
        .send_message(&OutboundMessage::text(&dm, first))
        .await
    else {
        return Thread::Unavailable("Slack didn't take the thread's first message.");
    };
    let key = crate::slack_manager::thread_session_key(&workspace, &dm, &sent.message_id);
    if dm_thread(&key).is_none() {
        return Thread::Unavailable("Slack answered with a thread that isn't a DM's.");
    }
    let Ok(mut thread) =
        service
            .sessions
            .get_or_create(&key, &boss.owner, service.agent_id.as_str())
    else {
        return Thread::Unavailable("The thread's conversation couldn't be made.");
    };
    let mut thread_meta = meta.clone();
    thread_meta[THREAD_KEY] = json!(key);
    thread.context_type = Some(SessionContextType::Dm);
    thread
        .metadata_object_mut()
        .insert(META_KEY.into(), thread_meta);
    boss.metadata_object_mut()
        .insert(OPENED_KEY.into(), json!(key));
    if service.sessions.save(&thread).is_err() || service.sessions.save(boss).is_err() {
        return Thread::Unavailable("The thread's conversation couldn't be kept.");
    }
    Thread::Opened(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_ims_thread_is_a_dm_thread() {
        assert_eq!(
            dm_thread("slack:T1:D0ABC:1700000000.0001"),
            Some(("T1", "D0ABC", "1700000000.0001"))
        );
        for not in [
            "slack:T1:C0ABC:1700000000.0001",
            "slack:T1:G0ABC:1700000000.0001",
            "slack:T1:D0ABC",
            "slack:T1:D:1.2",
            "slack::D0ABC:1.2",
            "slack:T1:D0ABC:slash",
            "desk:boss:k",
            "D0ABC:1.2",
        ] {
            assert_eq!(dm_thread(not), None, "{not}");
        }
    }
}
