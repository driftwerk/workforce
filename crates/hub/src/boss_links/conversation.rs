//! A Desk group's boss conversation on this boss hub (remote bosses, RB6a):
//! the one place an agent made a group's boss has that group's boss tools.
//!
//! - It is the person's own session with the agent on the channel
//!   `desk:boss:<group key>` (the hub's key for the group, [`super::group_key`]),
//!   and it carries `desk_boss` metadata naming the group, the person and the
//!   agent. Only [`ensure`] writes that metadata, for a live link the
//!   person's paired Desk holds: a channel alone grants nothing, and no route
//!   or tool that takes a channel from its caller may start one with
//!   [`BOSS_CHANNEL_PREFIX`] ([`refused_channel`]).
//! - It may also be had in Slack, in a thread of the person's DM with the
//!   agent that its link opened ([`super::slack`]).
//! - [`BossConversation::of`] is the only test of one, and it holds only for
//!   a turn of that very agent, as that very person, in a session no one
//!   shares.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde_json::{json, Value};
use uuid::Uuid;
use workforce_common::session::Session;

use super::links;
use crate::gateway::GatewayState;

pub use workforce_common::channels::{
    is_desk_boss_channel, DESK_BOSS_PREFIX as BOSS_CHANNEL_PREFIX,
};
/// The session metadata key only [`ensure`] writes.
pub use workforce_common::session::DESK_BOSS_KEY as META_KEY;

/// The channel of the group `group_key`'s boss conversation.
pub fn boss_channel(group_key: &str) -> String {
    format!("{BOSS_CHANNEL_PREFIX}{group_key}")
}

/// What a caller that names a channel is told when it names a boss
/// conversation's, which only a link opens.
pub const REFUSED: &str = "That is a Desk group's boss conversation: only its link opens it.";

/// Why `channel`, named by a caller, can't be used: it is, or claims to be,
/// a boss conversation's.
pub fn refused_channel(channel: &str) -> Option<&'static str> {
    is_desk_boss_channel(channel).then_some(REFUSED)
}

/// Why the person `owner` can't talk with `service`'s agent on `channel`,
/// a channel they named: one a boss conversation's is theirs only once its
/// link opened it ([`ensure`]). None for every other channel.
pub fn refused_for(
    service: &crate::serve::AgentService,
    channel: &str,
    owner: &str,
) -> Option<&'static str> {
    if !is_desk_boss_channel(channel) {
        return None;
    }
    let opened = service
        .sessions
        .find(channel, owner)
        .ok()
        .flatten()
        .is_some_and(|s| BossConversation::of(&s, service.agent_id.as_str(), owner).is_some());
    (!opened).then_some(REFUSED)
}

/// The name of the group `session` is the boss conversation of, as its
/// link registered it, plain and short: what the person reads in its
/// prompts. "this group" when it names none.
pub fn group_name(session: &Session) -> String {
    let named: String = session
        .metadata
        .get(META_KEY)
        .and_then(|m| m.get("groupName"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(80)
        .collect();
    match named.trim() {
        "" => "this group".to_string(),
        name => name.to_string(),
    }
}

/// Whether a WebSocket of `owner`'s, with `agent` (the hub's default when
/// none), may anchor on `channel`, a boss conversation's: only once its
/// link opened it for that very person and agent.
pub fn anchors(
    state: &GatewayState,
    owner: Option<&str>,
    agent: Option<&str>,
    channel: &str,
) -> bool {
    let service = state
        .registry
        .get(agent.unwrap_or(state.registry.default_id()));
    owner
        .zip(service)
        .is_some_and(|(owner, service)| refused_for(&service, channel, owner).is_none())
}

/// One group's boss conversation, as a turn is in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BossConversation {
    pub group_key: String,
    pub person: String,
    pub agent: String,
}

impl BossConversation {
    /// The boss conversation `session` is, for a turn of `agent` as
    /// `person`: its channel, its metadata, its owner and agent all saying
    /// the same, and no one else in it. None for any other session.
    pub fn of(session: &Session, agent: &str, person: &str) -> Option<Self> {
        let meta = session.metadata.get(META_KEY)?;
        let said = |k: &str| meta.get(k).and_then(Value::as_str);
        let group_key = said("groupKey").filter(|k| !k.is_empty())?;
        // Where it is had: its Desk channel, or the Slack thread its link
        // opened for it.
        let here = match session.channel.strip_prefix(BOSS_CHANNEL_PREFIX) {
            Some(key) => key == group_key && meta.get(super::slack::THREAD_KEY).is_none(),
            None => super::slack::is_mapped_thread(session, meta),
        };
        let ok = here
            && said("person") == Some(person)
            && said("agent") == Some(agent)
            && session.owner == person
            && session.agent_id == agent
            && !session.is_shared_conversation();
        ok.then(|| Self {
            group_key: group_key.to_string(),
            person: person.to_string(),
            agent: agent.to_string(),
        })
    }

    /// The memory tag of the group's work: what this agent remembers in the
    /// conversation, and all it recalls there.
    pub fn memory_tag(&self) -> String {
        workforce_memory::group_tagged::link_boss_tag(&self.group_key)
    }
}

/// The group's boss conversation, as [`ensure`] opened it.
#[derive(Debug)]
pub struct Opened {
    pub session_id: String,
    pub channel: String,
    /// Its Slack thread, when asked for.
    pub slack: Option<super::slack::Thread>,
}

/// Makes sure the boss conversation of `person`'s link `token_id`, which
/// `device_id` minted, exists with its metadata, and, with `slack`, its
/// thread in the person's Slack DM with the agent.
pub async fn ensure(
    state: &GatewayState,
    person: Uuid,
    device_id: &str,
    token_id: Uuid,
    slack: bool,
) -> Result<Opened, (StatusCode, &'static str)> {
    let not_found = (StatusCode::NOT_FOUND, "Link not found");
    let token = links::list(state, person, device_id)
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "The links couldn't be read.",
            )
        })?
        .into_iter()
        .find(|t| t.id == token_id)
        .ok_or(not_found)?;
    let link = token.desk_link.ok_or(not_found)?;
    let service = state.registry.get(&link.agent_id).ok_or(not_found)?;
    let channel = boss_channel(&link.group_key);
    let owner = person.to_string();
    let mut session = service
        .sessions
        .get_or_create(&channel, &owner, &link.agent_id)
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "The conversation couldn't be made.",
            )
        })?;
    let meta = json!({
        "v": 1,
        "groupKey": link.group_key,
        "groupName": link.group_name,
        "person": owner,
        "agent": link.agent_id,
    });
    if session.metadata.get(META_KEY) != Some(&meta) {
        session
            .metadata_object_mut()
            .insert(META_KEY.into(), meta.clone());
        service.sessions.save(&session).map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "The conversation couldn't be kept.",
            )
        })?;
    }
    let slack = match slack {
        true => Some(super::slack::open_thread(&service, &mut session, &meta).await),
        false => None,
    };
    Ok(Opened {
        session_id: session.id,
        channel,
        slack,
    })
}

/// What a Desk asks of [`open`].
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct OpenBody {
    /// Open its thread in the person's Slack DM with the agent too.
    pub slack: bool,
}

/// `POST /api/me/desk-boss-links/{id}/conversation`: the paired Desk opens
/// its link's boss conversation. Device-only, for its own link only.
pub async fn open(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Option<Json<OpenBody>>,
) -> Result<Json<Value>, crate::api_handlers::desk_boss_links::Rejection> {
    use crate::api_handlers::desk_boss_links as api;
    links::require_enabled(&state).map_err(api::rejection)?;
    let (device, person) =
        crate::api_handlers::remote_mcp_tokens::paired_device(&state, &headers).await?;
    crate::api_handlers::my_settings::enforce_action_rate_limit(&state, &person.to_string())?;
    let id = Uuid::parse_str(&id).map_err(|_| api::err(StatusCode::NOT_FOUND, "Link not found"))?;
    let slack = body.is_some_and(|Json(b)| b.slack);
    let opened = ensure(&state, person, &device.id, id, slack)
        .await
        .map_err(|(s, why)| api::err(s, why))?;
    let mut view = json!({ "sessionId": opened.session_id, "channel": opened.channel });
    if let Some(thread) = &opened.slack {
        view["slack"] = thread.view();
    }
    Ok(Json(view))
}
