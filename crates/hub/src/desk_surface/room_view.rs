//! What a Desk room turn told an agent of its room: who is in it and the
//! room's newest lines. A room lives on the person's Desk, not on the hub,
//! so a lookup the agent makes in the turn (`messaging.member_info`,
//! `channel_info`, `read`) is answered from these, never a channel the hub
//! cannot see.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use workforce_common::channels::desk::{DeskLine, DeskMember, DeskPlace, DeskTurnContext};

/// How many rooms' views are held, across every agent of the hub.
const VIEWS_KEPT: usize = 512;

/// One room as its latest turn to an agent told it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoomView {
    pub(crate) name: String,
    pub(crate) members: Vec<DeskMember>,
    pub(crate) lines: Vec<DeskLine>,
}

#[derive(Default)]
struct Views {
    by_room: HashMap<(String, String), (u64, RoomView)>,
    told: u64,
}

static VIEWS: LazyLock<Mutex<Views>> = LazyLock::new(Mutex::default);

/// Keeps what room turn `desk` told `agent`; a turn anywhere else changes
/// nothing.
pub(crate) fn remember(agent: &str, desk: &DeskTurnContext) {
    let DeskPlace::Room { name, .. } = &desk.turn.place else {
        return;
    };
    let view = RoomView {
        name: name.clone(),
        members: desk.turn.members.clone(),
        lines: desk.turn.context.clone(),
    };
    let mut views = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
    views.told += 1;
    let told = views.told;
    views.by_room.insert(
        (agent.to_string(), desk.target.channel.clone()),
        (told, view),
    );
    if views.by_room.len() > VIEWS_KEPT {
        let oldest = views
            .by_room
            .iter()
            .min_by_key(|(_, (told, _))| *told)
            .map(|(key, _)| key.clone());
        if let Some(key) = oldest {
            views.by_room.remove(&key);
        }
    }
}

/// What `agent` was last told of the room on `channel`.
pub(crate) fn of(agent: &str, channel: &str) -> Option<RoomView> {
    VIEWS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .by_room
        .get(&(agent.to_string(), channel.to_string()))
        .map(|(_, view)| view.clone())
}

impl RoomView {
    /// The member `who` names: by name, `@name`, or `name@hub`, any case.
    pub(crate) fn member(&self, who: &str) -> Option<&DeskMember> {
        let who = who.trim().trim_start_matches('@');
        let (name, hub) = match who.rsplit_once('@') {
            Some((name, hub)) => (name, Some(hub)),
            None => (who, None),
        };
        self.members.iter().find(|m| {
            m.name.eq_ignore_ascii_case(name) && hub.is_none_or(|h| m.hub.eq_ignore_ascii_case(h))
        })
    }

    /// The members, as a line: `Ada (Acme, lead), Bo (Globex)`.
    pub(crate) fn roster(&self) -> String {
        self.members
            .iter()
            .map(|m| {
                format!(
                    "{} ({}{})",
                    m.name,
                    m.hub,
                    if m.lead { ", lead" } else { "" }
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;
    use workforce_common::channels::desk::{DeskSpeaker, DeskTarget, DeskTurn};

    pub(crate) fn room_turn(channel: &str, members: &[(&str, &str, bool)]) -> DeskTurnContext {
        let place = DeskPlace::Room {
            room: Uuid::from_u128(9),
            name: "Planning".into(),
            thread: None,
        };
        DeskTurnContext {
            turn: DeskTurn {
                place: place.clone(),
                speaker: DeskSpeaker::Person,
                hop: 0,
                context: vec![DeskLine {
                    from: "Ada (agent)".into(),
                    text: "on the release".into(),
                }],
                message: None,
                spoken: false,
                voice: None,
                members: members
                    .iter()
                    .map(|(name, hub, lead)| DeskMember {
                        name: name.to_string(),
                        hub: hub.to_string(),
                        lead: *lead,
                        asked: false,
                    })
                    .collect(),
                edit: None,
            },
            target: DeskTarget {
                place,
                channel: channel.into(),
                device: None,
                spoken: false,
            },
            turn_ref: None,
            reply_id: "r".into(),
        }
    }

    #[test]
    fn a_room_turn_is_kept_per_agent_and_room_and_its_members_found_by_name() {
        let channel = "desk:room-view-test";
        remember(
            "agent-a",
            &room_turn(channel, &[("Ada", "Acme", true), ("Bo", "Globex", false)]),
        );
        let view = of("agent-a", channel).expect("kept");
        assert_eq!(view.roster(), "Ada (Acme, lead), Bo (Globex)");
        assert_eq!(view.member("@ada").map(|m| m.hub.as_str()), Some("Acme"));
        assert_eq!(
            view.member("bo@globex").map(|m| m.name.as_str()),
            Some("Bo")
        );
        assert!(view.member("bo@acme").is_none());
        assert!(
            of("agent-b", channel).is_none(),
            "another agent's turn told it nothing"
        );
    }
}
