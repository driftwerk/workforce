//! Desk as a [`ChannelSurface`]: what `messaging.send` on `platform: "desk"`
//! posts through. It posts only to the conversations of the person the run is
//! for with this agent: the current one, their DM, or one of their rooms.

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use eyre::{eyre, Result};
use uuid::Uuid;
use workforce_agent::runner::{current_desk, CURRENT_USER_ID};
use workforce_common::channels::desk::{
    is_desk_room, DeliveryKind, DeskPlace, DeskScope, DeskTarget, DESK_CHANNEL, DESK_PLATFORM,
};
use workforce_common::channels::{
    ChannelCapabilities, ChannelSurface, OutboundMessage, Reaction, SentMessage,
};
use workforce_common::session::SessionStore;

use super::deliver::{deliver, Delivery};
use crate::serve::AgentService;

/// How many of a person's sessions with an agent are searched for a room.
const ROOMS_SEARCHED: usize = 500;

/// The rooms `owner` keeps with the agent of `sessions` that are not gone,
/// as the latest turn in each was asked.
pub(crate) fn rooms_of(sessions: &dyn SessionStore, owner: &str) -> Result<Vec<DeskTarget>> {
    Ok(sessions
        .list_for_owner(owner, ROOMS_SEARCHED)?
        .into_iter()
        .filter(|s| is_desk_room(&s.channel) && !s.is_sealed())
        .filter(|s| s.metadata.get(super::deliver::CLOSED_KEY).is_none())
        .filter_map(|s| s.desk_origin())
        .filter(|target| matches!(target.place, DeskPlace::Room { .. }))
        .collect())
}

/// The Desk conversation `channel` names, read against the run's `scope` and
/// the person's `rooms`: `""` or `"here"` is the current one (`thread`
/// choosing another thread of its room), `"dm"` the DM, `"desk:…"` or
/// `"#<room name>"` one of their rooms.
pub(crate) fn resolve_place(
    scope: Option<&DeskScope>,
    channel: &str,
    thread: Option<&str>,
    rooms: &[DeskTarget],
) -> std::result::Result<DeskTarget, String> {
    let here = || {
        scope
            .map(|scope| scope.target.clone())
            .ok_or_else(|| "this run answers to no Desk conversation: name one".to_string())
    };
    let target = match channel.trim() {
        "" | "here" => here()?,
        "dm" | DESK_CHANNEL => DeskTarget::of_turn(
            &DeskPlace::Dm,
            DESK_CHANNEL,
            None,
            scope.is_some_and(|s| s.target.spoken),
        ),
        named => rooms
            .iter()
            .find(|room| match (named.strip_prefix('#'), &room.place) {
                (
                    Some(name),
                    DeskPlace::Room {
                        name: room_name, ..
                    },
                ) => room_name.eq_ignore_ascii_case(name.trim()),
                (None, _) => room.channel == named,
                _ => false,
            })
            .cloned()
            .ok_or_else(|| {
                format!(
                    "{named:?} is not a Desk conversation of this person's; to post on Slack pass \
                     platform: \"slack\""
                )
            })?,
    };
    in_thread(target, thread)
}

/// `target` moved to `thread` of its room, when one is named.
fn in_thread(target: DeskTarget, thread: Option<&str>) -> std::result::Result<DeskTarget, String> {
    let Some(thread) = thread.map(str::trim).filter(|t| !t.is_empty()) else {
        return Ok(target);
    };
    let thread = Uuid::parse_str(thread).map_err(|_| format!("{thread:?} is not a Desk thread"))?;
    match target.place {
        DeskPlace::Room { room, name, .. } => Ok(DeskTarget {
            place: DeskPlace::Room {
                room,
                name,
                thread: Some(thread),
            },
            ..target
        }),
        _ => Err("only a room has threads".to_string()),
    }
}

/// The Desk surface of one agent.
pub(crate) struct DeskSurface {
    service: Weak<AgentService>,
}

impl DeskSurface {
    pub(crate) fn new(service: &Arc<AgentService>) -> Self {
        Self {
            service: Arc::downgrade(service),
        }
    }
}

#[async_trait]
impl ChannelSurface for DeskSurface {
    fn platform(&self) -> &str {
        DESK_PLATFORM
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            send_message: true,
            threads: true,
            ..ChannelCapabilities::default()
        }
    }

    async fn add_reaction(&self, _reaction: &Reaction) -> Result<()> {
        Err(eyre!("Desk has no reactions"))
    }

    async fn remove_reaction(&self, _reaction: &Reaction) -> Result<()> {
        Err(eyre!("Desk has no reactions"))
    }

    async fn send_message(&self, message: &OutboundMessage) -> Result<SentMessage> {
        let service = self
            .service
            .upgrade()
            .ok_or_else(|| eyre!("the agent is gone"))?;
        let owner = CURRENT_USER_ID
            .try_with(Clone::clone)
            .ok()
            .filter(|owner| !owner.is_empty())
            .ok_or_else(|| eyre!("a Desk message is someone's: this run is for nobody"))?;
        let rooms = rooms_of(service.sessions.as_ref(), &owner)?;
        let target = resolve_place(
            current_desk().as_ref(),
            &message.channel,
            message.thread_id.as_deref(),
            &rooms,
        )
        .map_err(|e| eyre!(e))?;
        let delivered = deliver(
            &service,
            &owner,
            &target,
            Delivery::text(DeliveryKind::Notice, message.text.clone()),
        )
        .await?;
        Ok(SentMessage {
            channel: target.channel,
            message_id: delivered.message_id,
            thread_id: message.thread_id.clone(),
            platform: DESK_PLATFORM.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room(n: u128, name: &str) -> DeskTarget {
        DeskTarget {
            place: DeskPlace::Room {
                room: Uuid::from_u128(n),
                name: name.into(),
                thread: None,
            },
            channel: format!("desk:{}", Uuid::from_u128(n)),
            device: Some("device-1".into()),
            spoken: false,
        }
    }

    fn scope(target: DeskTarget) -> DeskScope {
        DeskScope {
            target,
            attended: true,
        }
    }

    #[test]
    fn a_message_goes_where_it_names_or_where_the_run_is() {
        let rooms = [room(1, "Planning"), room(2, "Release")];
        let here = scope(room(1, "Planning"));
        assert_eq!(
            resolve_place(Some(&here), "", None, &rooms),
            Ok(room(1, "Planning"))
        );
        assert_eq!(
            resolve_place(Some(&here), "here", None, &rooms),
            Ok(room(1, "Planning"))
        );
        assert_eq!(
            resolve_place(Some(&here), "dm", None, &rooms).map(|t| t.place),
            Ok(DeskPlace::Dm)
        );
        assert_eq!(
            resolve_place(Some(&here), "#release", None, &rooms),
            Ok(room(2, "Release"))
        );
        assert_eq!(
            resolve_place(None, &room(2, "Release").channel, None, &rooms),
            Ok(room(2, "Release"))
        );
        let thread = Uuid::from_u128(9);
        let threaded = resolve_place(Some(&here), "", Some(&thread.to_string()), &rooms).unwrap();
        assert!(matches!(threaded.place, DeskPlace::Room { thread: Some(t), .. } if t == thread));
    }

    #[test]
    fn a_slack_channel_or_no_conversation_is_refused() {
        let rooms = [room(1, "Planning")];
        let refused = resolve_place(None, "C0123ABC", None, &rooms).unwrap_err();
        assert!(refused.contains("platform: \"slack\""), "{refused}");
        assert!(resolve_place(None, "", None, &rooms).is_err());
        assert!(resolve_place(None, "#elsewhere", None, &rooms).is_err());
        let dm = scope(room(1, "Planning").dm());
        assert_eq!(
            resolve_place(Some(&dm), "", Some(&Uuid::from_u128(3).to_string()), &rooms),
            Err("only a room has threads".to_string())
        );
    }
}
