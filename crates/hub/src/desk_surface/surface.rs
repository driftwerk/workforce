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
    ChannelCapabilities, ChannelInfo, ChannelMessage, ChannelSurface, MemberInfo, OutboundMessage,
    Reaction, SentMessage,
};
use workforce_common::session::SessionStore;

use super::deliver::{deliver, deliver_file, Delivery};
use super::room_view::{self, RoomView};
use crate::desk_attachments::StagedFile;
use crate::serve::AgentService;

/// How many of a person's sessions with an agent are searched for a room.
const ROOMS_SEARCHED: usize = 500;

/// The rooms `owner` keeps with the agent of `sessions` that are not gone,
/// as the latest turn in each was asked.
pub(crate) fn rooms_of(sessions: &dyn SessionStore, owner: &str) -> Result<Vec<DeskTarget>> {
    Ok(sessions
        .list_for_owner_on(owner, DESK_CHANNEL, ROOMS_SEARCHED)?
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

    /// The agent, whose conversations are meant, and the one of theirs
    /// `channel` and `thread` name (see [`resolve_place`]). A run that
    /// answers to a Desk conversation means its owner's; any other, the
    /// person it runs for.
    fn addressed(
        &self,
        channel: &str,
        thread: Option<&str>,
    ) -> Result<(Arc<AgentService>, String, DeskTarget)> {
        let service = self
            .service
            .upgrade()
            .ok_or_else(|| eyre!("the agent is gone"))?;
        let scope = current_desk();
        let owner = scope
            .as_ref()
            .map(|scope| scope.owner.clone())
            .or_else(|| CURRENT_USER_ID.try_with(Clone::clone).ok())
            .filter(|owner| !owner.is_empty())
            .ok_or_else(|| eyre!("a Desk message is someone's: this run is for nobody"))?;
        let rooms = rooms_of(service.sessions.as_ref(), &owner)?;
        let target =
            resolve_place(scope.as_ref(), channel, thread, &rooms).map_err(|e| eyre!(e))?;
        Ok((service, owner, target))
    }
}

impl DeskSurface {
    /// The room this turn is in, as Desk told it: `channel` must name it
    /// (or nothing, for here). A room lives on the person's Desk, so only
    /// what its turn carried can be answered.
    fn room_here(&self, channel: &str) -> Result<(DeskTarget, RoomView)> {
        let (service, _, target) = self.addressed(channel, None)?;
        let here = current_desk().map(|scope| scope.target.channel);
        if here.as_deref() != Some(target.channel.as_str()) {
            return Err(eyre!(
                "only the room this turn is in can be read: its members and recent lines are \
                 in your context"
            ));
        }
        let view = room_view::of(service.agent_id.as_str(), &target.channel).ok_or_else(|| {
            eyre!("this is not a room: the conversation's recent messages are in your context")
        })?;
        Ok((target, view))
    }
}

/// A line of the room with nothing in it, to build a note on.
fn empty_line(target: &DeskTarget, view: &RoomView) -> ChannelMessage {
    ChannelMessage {
        channel: target.channel.clone(),
        message_id: String::new(),
        user: String::new(),
        user_name: None,
        channel_name: Some(format!("room \"{}\"", view.name)),
        text: String::new(),
        timestamp: String::new(),
        thread_id: None,
        files: Vec::new(),
        bot_id: None,
    }
}

/// The room's newest `limit` lines, as messages.
fn lines_of(target: &DeskTarget, view: &RoomView, limit: usize) -> Vec<ChannelMessage> {
    let skip = view.lines.len().saturating_sub(limit);
    view.lines
        .iter()
        .enumerate()
        .skip(skip)
        .map(|(i, line)| ChannelMessage {
            channel: target.channel.clone(),
            message_id: format!("line-{i}"),
            user: line.from.clone(),
            user_name: Some(line.from.clone()),
            channel_name: Some(format!("room \"{}\"", view.name)),
            text: line.text.clone(),
            timestamp: String::new(),
            thread_id: None,
            files: Vec::new(),
            bot_id: None,
        })
        .collect()
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
            file_upload: true,
            ..ChannelCapabilities::default()
        }
    }

    async fn add_reaction(&self, _reaction: &Reaction) -> Result<()> {
        Err(eyre!("Desk has no reactions"))
    }

    async fn remove_reaction(&self, _reaction: &Reaction) -> Result<()> {
        Err(eyre!("Desk has no reactions"))
    }

    async fn read_messages(
        &self,
        channel: &str,
        limit: usize,
        _oldest: Option<&str>,
    ) -> Result<Vec<ChannelMessage>> {
        let (target, view) = self.room_here(channel)?;
        Ok(lines_of(&target, &view, limit))
    }

    /// The lines this turn's room carried, whatever thread is named: Desk
    /// keeps no thread history on the hub, and the first message says so.
    async fn read_thread(
        &self,
        channel: &str,
        _thread_ts: &str,
        limit: usize,
    ) -> Result<Vec<ChannelMessage>> {
        let (target, view) = self.room_here(channel)?;
        let note = ChannelMessage {
            text: "These are the recent lines Desk sent with this turn, for the room or thread \
                   it is in. Desk keeps no other thread history here: thread_ts is not used."
                .to_string(),
            message_id: "note".to_string(),
            user: "Desk".to_string(),
            user_name: Some("Desk".to_string()),
            ..empty_line(&target, &view)
        };
        Ok(std::iter::once(note)
            .chain(lines_of(&target, &view, limit))
            .collect())
    }

    async fn member_info(&self, user_id: &str) -> Result<MemberInfo> {
        let (_, view) = self.room_here("")?;
        let member = view.member(user_id).ok_or_else(|| {
            eyre!(
                "{user_id:?} is not in room \"{}\"; its members are {}",
                view.name,
                view.roster()
            )
        })?;
        Ok(MemberInfo {
            user_id: format!("{}@{}", member.name, member.hub),
            display_name: member.name.clone(),
            real_name: member.name.clone(),
            title: format!(
                "agent on {}{}, in room \"{}\"",
                member.hub,
                if member.lead { ", the room's lead" } else { "" },
                view.name
            ),
            timezone: None,
            status_text: None,
            status_emoji: None,
            avatar_url: None,
        })
    }

    async fn channel_info(&self, channel_id: &str) -> Result<ChannelInfo> {
        let (target, view) = self.room_here(channel_id)?;
        Ok(ChannelInfo {
            channel_id: target.channel,
            display_name: format!("room \"{}\" — members: {}", view.name, view.roster()),
        })
    }

    async fn send_message(&self, message: &OutboundMessage) -> Result<SentMessage> {
        let (service, owner, target) =
            self.addressed(&message.channel, message.thread_id.as_deref())?;
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

    async fn upload_file(
        &self,
        channel: &str,
        filename: &str,
        content: &[u8],
        message: Option<&str>,
        thread_id: Option<&str>,
    ) -> Result<String> {
        let (service, owner, target) = self.addressed(channel, thread_id)?;
        let agent = service.agent_id.as_str();
        let file = StagedFile::stage(agent, filename, content.to_vec(), message)?;
        let link =
            workforce_common::channels::desk::download_path(agent, file.artifact_id.as_str());
        deliver_file(&service, &owner, &target, file).await?;
        Ok(link)
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
            relayed: false,
            owner: "person-1".into(),
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
