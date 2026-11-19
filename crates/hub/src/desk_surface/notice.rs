//! A notice an agent's own work causes ("I changed my own connectors, …"),
//! told to the person where they are.
//!
//! - **A turn in a conversation off Desk is told there.** When a person's
//!   message to this agent in a Slack thread or DM (Teams, once it runs
//!   turns, the same way) started the turn the notice comes from, it is
//!   posted in that conversation, beside the turn's answer, through the
//!   surface the agent posts there by: a short line. In a shared channel
//!   it is seen by whoever sees the turn's answer, as the answer is.
//! - **It is not also put in the Desk DM.** Desk counts a conversation's
//!   unread by time against the person's one read point
//!   ([`crate::desk_reads::unread_of`]), so no single message can be posted
//!   already read: moving the point past it would also mark read whatever
//!   arrived before it unseen. A copy there would sit unread beside a
//!   conversation the person never had in Desk.
//! - **Everything else goes to the Desk DM, unread, as before:** a Desk
//!   turn's notice, and one with no conversation behind it (a job, a
//!   heartbeat, a workflow, a boss's wake, a crew's background work).
//! - **Nothing is lost.** A conversation that can't take it (its surface
//!   gone, the post refused) leaves the notice in the Desk DM, unread.
//!
//! Only the person whose turn it is is told in their conversation, and only
//! by the agent the turn is with: a notice for anyone else, or of another
//! agent's, goes to its own DM.

use eyre::Result;
use workforce_agent::runner::{
    chat_origin, current_desk, CURRENT_AGENT_ID, CURRENT_CHANNEL_ID, CURRENT_SESSION_KEY,
    CURRENT_THREAD_ID, CURRENT_USER_ID,
};
use workforce_common::channels::desk::{DeliveryKind, DeskPlace, DeskTarget, DESK_CHANNEL};
use workforce_common::channels::{ChannelHint, OutboundMessage};

use crate::serve::AgentService;

/// Where a notice was told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Told {
    /// In the conversation off Desk the turn behind it is in.
    InConversation,
    /// In the person's Desk DM, unread.
    InDeskDm,
}

/// A conversation off Desk that a turn is had in, as its surface posts to
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Conversation {
    pub platform: String,
    pub workspace: Option<String>,
    pub channel: String,
    pub thread: Option<String>,
}

impl Conversation {
    /// The conversation a turn in session `key` answers in: a Slack DM,
    /// channel or thread (`slack:<workspace>:<channel>[:<thread>]`), the
    /// thread its answer goes to (`channel`, `thread`: the turn's own) where
    /// its key names none; a Teams conversation (`teams:<conversation>`).
    /// Only when the turn's own `channel`, which only that surface's
    /// dispatch sets, is the one the key names: a key is what a client may
    /// name (the web app's chat takes any), the channel is where the
    /// message came from. None for any other key: Desk's, the web app's, a
    /// job's, a slash command's.
    pub(crate) fn of_turn(key: &str, channel: Option<&str>, thread: Option<&str>) -> Option<Self> {
        let (platform, rest) = key.split_once(':')?;
        match platform {
            "slack" => {
                let mut parts = rest.splitn(3, ':');
                let workspace = parts.next().filter(|w| !w.is_empty())?;
                let in_key = parts.next().filter(|c| !c.is_empty())?;
                if channel != Some(in_key) {
                    return None;
                }
                let thread = match parts.next() {
                    Some(t) if is_slack_ts(t) => Some(t.to_string()),
                    Some(_) => return None,
                    None => thread.filter(|t| is_slack_ts(t)).map(str::to_string),
                };
                Some(Self {
                    platform: platform.to_string(),
                    workspace: Some(workspace.to_string()),
                    channel: in_key.to_string(),
                    thread,
                })
            }
            "teams" if !rest.is_empty() && (channel == Some(rest) || channel == Some(key)) => {
                Some(Self {
                    platform: platform.to_string(),
                    workspace: None,
                    channel: key.to_string(),
                    thread: None,
                })
            }
            _ => None,
        }
    }
}

/// Whether `ts` is a Slack message timestamp (`1700000000.000100`).
fn is_slack_ts(ts: &str) -> bool {
    !ts.is_empty() && ts.bytes().all(|b| b.is_ascii_digit() || b == b'.')
}

/// The conversation off Desk the running turn is in, when a person's
/// message there started it, that person is `owner`, and its agent is
/// `agent`. None for a Desk turn, for automated work (a job, a heartbeat, a
/// workflow, a boss's wake, a turn a tool started), and outside a turn.
fn turn_conversation(agent: &str, owner: &str) -> Option<Conversation> {
    let local = |key: &'static tokio::task::LocalKey<String>| {
        key.try_with(String::clone).ok().filter(|v| !v.is_empty())
    };
    if current_desk().is_some()
        || !chat_origin()
        || crate::hub_core::origins::automated_origin().is_some()
        || local(&CURRENT_USER_ID).as_deref() != Some(owner)
        || local(&CURRENT_AGENT_ID).as_deref() != Some(agent)
    {
        return None;
    }
    Conversation::of_turn(
        &local(&CURRENT_SESSION_KEY)?,
        local(&CURRENT_CHANNEL_ID).as_deref(),
        local(&CURRENT_THREAD_ID).as_deref(),
    )
}

/// Post `text` in `conversation` through the surface `service` posts there
/// by.
async fn post(service: &AgentService, conversation: &Conversation, text: &str) -> Result<()> {
    let hint = ChannelHint::from_parts(conversation.workspace.clone(), None, None);
    let surface = service
        .surface_registry
        .read()
        .await
        .resolve(&conversation.platform, &conversation.channel, hint)
        .await?;
    surface
        .send_message(&OutboundMessage {
            thread_id: conversation.thread.clone(),
            ..OutboundMessage::text(conversation.channel.clone(), text)
        })
        .await?;
    Ok(())
}

/// Tell `owner` `text`, a notice from `service`'s agent, in the
/// conversation off Desk the running turn is in, when it is theirs with
/// this agent. Whether it was told there; `false` leaves it to the caller.
pub(crate) async fn in_conversation(service: &AgentService, owner: &str, text: &str) -> bool {
    let Some(conversation) = turn_conversation(service.agent_id.as_str(), owner) else {
        return false;
    };
    let text = workforce_common::redact::redact_secrets(text);
    match post(service, &conversation, &text).await {
        Ok(()) => {
            tracing::info!(
                agent = %service.agent_id,
                platform = %conversation.platform,
                channel = %conversation.channel,
                "a notice was told in the conversation its turn is in"
            );
            true
        }
        Err(e) => {
            tracing::warn!(
                agent = %service.agent_id,
                platform = %conversation.platform,
                channel = %conversation.channel,
                error = %format!("{e:#}"),
                "a notice couldn't be told in its turn's conversation"
            );
            false
        }
    }
}

/// Tell `owner` `text`, a notice from `service`'s agent: in the conversation
/// off Desk the running turn is in ([`in_conversation`]), else in their Desk
/// DM, unread. An error is a notice told nowhere: the Desk DM couldn't be
/// written.
pub(crate) async fn notice(service: &AgentService, owner: &str, text: String) -> Result<Told> {
    if in_conversation(service, owner, &text).await {
        return Ok(Told::InConversation);
    }
    let dm = DeskTarget::of_turn(&DeskPlace::Dm { thread: None }, DESK_CHANNEL, None, false);
    super::deliver(
        service,
        owner,
        &dm,
        super::Delivery::text(DeliveryKind::Notice, text),
    )
    .await?;
    Ok(Told::InDeskDm)
}

#[cfg(test)]
#[path = "notice_tests.rs"]
mod tests;
