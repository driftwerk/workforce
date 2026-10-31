//! Desk, the desktop app, as a surface of the hub: where a turn came from,
//! where its later output goes, and what a saved message is in a Desk
//! conversation. In `workforce_core` because the gateway's chat frame
//! (`workforce-gateway-client`), the NATS chat request (`workforce-nats`) and
//! the session store (`workforce-common`) all carry these.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The desktop app's channel: a person's direct conversation with an agent.
/// Each of its rooms is a conversation of its own on `desk:<id>` (see
/// [`is_desk_room`]).
pub const DESK_CHANNEL: &str = "desk";

/// The platform name of the Desk surface.
pub const DESK_PLATFORM: &str = DESK_CHANNEL;

/// How many agent-to-agent hand-offs one ask may chain.
pub const MAX_HOPS: u8 = 3;

/// The most thread lines a turn carries, the most bytes they take rendered
/// as `who: text` lines, and the most characters one line's text or one name
/// keeps. The hub's seed formatter renders within the same bounds, so a
/// turn's lines are never cut again.
pub const CONTEXT_LINES: usize = 12;
pub const CONTEXT_BYTES: usize = 4_000;
pub const LINE_CHARS: usize = 500;
pub const NAME_CHARS: usize = 100;

/// The room slug a call's channel uses: `desk:tuple`.
const CALL_SLUG: &str = "tuple";

const DESK_ROOM_SLUG_MAX: usize = 64;

/// True for a desktop app room's channel: `desk:<slug>`, the slug 1–64 of
/// `[a-z0-9-]`.
pub fn is_desk_room(channel: &str) -> bool {
    room_slug(channel).is_some()
}

/// Whether `channel` is one of Desk's: a DM or a room.
pub fn is_desk_conversation(channel: &str) -> bool {
    channel == DESK_CHANNEL || is_desk_room(channel)
}

fn room_slug(channel: &str) -> Option<&str> {
    channel
        .strip_prefix(DESK_CHANNEL)
        .and_then(|rest| rest.strip_prefix(':'))
        .filter(|slug| {
            (1..=DESK_ROOM_SLUG_MAX).contains(&slug.len())
                && slug
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// What a Desk channel is the conversation of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeskAnchor<'a> {
    /// `desk`: the person's DM with the agent.
    Dm,
    /// `desk:tuple`: requests from a call.
    Call,
    /// `desk:<uuid>`: a room created with its channel frozen to its id.
    Room(Uuid),
    /// `desk:<slug>`: a room created before channels were frozen.
    Slug(&'a str),
}

impl<'a> DeskAnchor<'a> {
    /// What `channel` anchors, or `None` when it is not a Desk channel.
    pub fn of(channel: &'a str) -> Option<Self> {
        if channel == DESK_CHANNEL {
            return Some(Self::Dm);
        }
        room_slug(channel).map(|slug| match slug {
            CALL_SLUG => Self::Call,
            slug => Uuid::try_parse(slug)
                .ok()
                .filter(|id| id.hyphenated().to_string() == slug)
                .map_or(Self::Slug(slug), Self::Room),
        })
    }
}

/// Where in Desk a turn was asked, or where output goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeskPlace {
    Dm,
    Room {
        room: Uuid,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<Uuid>,
    },
    Call,
    /// A room of a Desk too old to say which: known only by its slug.
    LegacyRoom {
        slug: String,
    },
}

impl DeskPlace {
    /// The room and thread a message in this place carries.
    pub fn mark(&self) -> DeskMark {
        match self {
            Self::Room { room, thread, .. } => DeskMark {
                room: Some(*room),
                thread: *thread,
                ..DeskMark::default()
            },
            Self::Dm | Self::Call | Self::LegacyRoom { .. } => DeskMark::default(),
        }
    }

    /// Whether a turn in this place may run on `anchor`'s conversation.
    fn fits(&self, anchor: DeskAnchor<'_>) -> bool {
        match (self, anchor) {
            (Self::Dm, DeskAnchor::Dm) | (Self::Call, DeskAnchor::Call) => true,
            (Self::Room { room, .. }, DeskAnchor::Room(id)) => *room == id,
            (Self::Room { .. }, DeskAnchor::Slug(_)) => true,
            (Self::LegacyRoom { slug }, DeskAnchor::Slug(anchor)) => slug == anchor,
            _ => false,
        }
    }

    fn clipped(self) -> Self {
        match self {
            Self::Room { room, name, thread } => Self::Room {
                room,
                name: clip(&name, NAME_CHARS),
                thread,
            },
            place => place,
        }
    }
}

/// Who asked: the person, or another agent handing off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeskSpeaker {
    Person,
    Agent { name: String },
}

/// One earlier line of the thread a turn was asked in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeskLine {
    pub from: String,
    pub text: String,
}

impl DeskLine {
    fn clipped(self) -> Self {
        Self {
            from: clip(&self.from, NAME_CHARS),
            text: clip(self.text.trim(), LINE_CHARS),
        }
    }

    /// The bytes the line takes rendered as `from: text`.
    pub fn rendered_len(&self) -> usize {
        self.from.len() + 2 + self.text.len()
    }
}

/// One turn as Desk sends it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeskTurn {
    pub place: DeskPlace,
    pub speaker: DeskSpeaker,
    #[serde(default)]
    pub hop: u8,
    #[serde(default)]
    pub context: Vec<DeskLine>,
    /// The Desk room message this turn answers.
    #[serde(default)]
    pub message: Option<Uuid>,
    /// Spoken rather than typed: the agent's voice model and thinking budget
    /// apply to this message alone.
    #[serde(default)]
    pub spoken: bool,
}

/// Why a turn Desk sent cannot run on the conversation it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeskTurnError {
    NotDesk,
    WrongPlace,
    TooManyHops,
}

impl std::fmt::Display for DeskTurnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotDesk => "a Desk turn must be sent on a Desk channel",
            Self::WrongPlace => "the turn's place is not this conversation's",
            Self::TooManyHops => "agents have handed this on too many times already",
        })
    }
}

impl std::error::Error for DeskTurnError {}

impl DeskTurn {
    /// The turn, checked against the channel it was sent on and with its
    /// thread lines held to [`CONTEXT_LINES`] and [`CONTEXT_BYTES`], newest
    /// kept and blank ones dropped.
    pub fn validated(self, anchor: &str) -> Result<Self, DeskTurnError> {
        let anchor = DeskAnchor::of(anchor).ok_or(DeskTurnError::NotDesk)?;
        if !self.place.fits(anchor) {
            return Err(DeskTurnError::WrongPlace);
        }
        if self.hop > MAX_HOPS {
            return Err(DeskTurnError::TooManyHops);
        }
        Ok(Self {
            place: self.place.clipped(),
            speaker: match self.speaker {
                DeskSpeaker::Agent { name } => DeskSpeaker::Agent {
                    name: clip(&name, NAME_CHARS),
                },
                person => person,
            },
            context: clamp(self.context),
            ..self
        })
    }

    /// The turn a Desk too old to describe it sent on `anchor`: the person,
    /// in the DM, on the call, or in a room known by its channel alone.
    pub fn legacy(anchor: &str) -> Option<Self> {
        let place = match DeskAnchor::of(anchor)? {
            DeskAnchor::Dm => DeskPlace::Dm,
            DeskAnchor::Call => DeskPlace::Call,
            DeskAnchor::Room(_) | DeskAnchor::Slug(_) => DeskPlace::LegacyRoom {
                slug: anchor[DESK_CHANNEL.len() + 1..].to_string(),
            },
        };
        Some(Self {
            place,
            speaker: DeskSpeaker::Person,
            hop: 0,
            context: Vec::new(),
            message: None,
            spoken: false,
        })
    }

    /// What the message this turn carries is in its conversation: its room
    /// and thread, and who said it.
    pub fn mark(&self) -> DeskMark {
        DeskMark {
            speaker: Some(self.speaker.clone()),
            ..self.place.mark()
        }
    }

    /// Whether another agent, not the person, said this.
    pub fn is_hand_off(&self) -> bool {
        matches!(self.speaker, DeskSpeaker::Agent { .. })
    }
}

fn clamp(context: Vec<DeskLine>) -> Vec<DeskLine> {
    let mut kept: Vec<DeskLine> = context
        .into_iter()
        .rev()
        .map(DeskLine::clipped)
        .filter(|line| !line.text.is_empty())
        .take(CONTEXT_LINES)
        .scan(0usize, |total, line| {
            *total += line.rendered_len();
            (*total <= CONTEXT_BYTES).then_some(line)
        })
        .collect();
    kept.reverse();
    kept
}

fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// Where a conversation's output goes, remembered on whatever outlives the
/// turn: a reminder, a job, a workflow run, a heartbeat check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeskTarget {
    pub place: DeskPlace,
    pub channel: String,
    /// The device whose Desk holds the room; a DM is any of the person's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// Read aloud when it arrives.
    #[serde(default)]
    pub spoken: bool,
}

impl DeskTarget {
    /// Where output of a turn in `place` on `channel` goes. A call is over
    /// by the time anything arrives later, and a room too old to name its id
    /// cannot be posted to, so both go to the DM.
    pub fn of_turn(place: &DeskPlace, channel: &str, device: Option<&str>, spoken: bool) -> Self {
        match place {
            DeskPlace::Room { .. } => Self {
                place: place.clone(),
                channel: channel.to_string(),
                device: device.map(str::to_string),
                spoken,
            },
            DeskPlace::LegacyRoom { .. } => Self {
                place: place.clone(),
                channel: DESK_CHANNEL.to_string(),
                device: None,
                spoken,
            },
            DeskPlace::Dm | DeskPlace::Call => Self::dm_of(spoken),
        }
    }

    /// The person's DM, where everything falls back to: a deleted room, an
    /// unpaired device or a finished call.
    pub fn dm(&self) -> Self {
        Self::dm_of(self.spoken)
    }

    fn dm_of(spoken: bool) -> Self {
        Self {
            place: DeskPlace::Dm,
            channel: DESK_CHANNEL.to_string(),
            device: None,
            spoken,
        }
    }
}

/// Why a message was delivered to a Desk conversation outside a live turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryKind {
    Reminder,
    Job,
    Workflow,
    CheckIn,
    Approval,
    File,
    Resumed,
    Notice,
    /// One a newer hub or Desk names, read by one that doesn't know it.
    #[serde(other)]
    Other,
}

/// What a session message is in a Desk conversation, beyond its text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeskMark {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<DeskSpeaker>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<DeliveryKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// The files the message carries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<DeskFile>,
    /// The Desk room message the turn that said it answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<Uuid>,
    /// The tools that turn ran, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<DeskStep>,
}

/// How far a person has read a place: through `at`, the item seen last
/// when there is one (a message's id, an event's, a version).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadPoint {
    pub at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub message: Option<String>,
}

/// How a write moves a read point: never back, or exactly here (marking
/// something unread).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mark {
    Advance,
    Set,
}

/// One tool an agent's turn ran: what it was, whether it worked, how long.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeskStep {
    pub tool: String,
    pub ok: bool,
    pub ms: u64,
}

/// Steps kept on one message, the first ones.
pub const MAX_STEPS: usize = 100;
/// The longest tool name a step keeps.
pub const STEP_TOOL_CHARS: usize = 120;

impl DeskStep {
    /// The step a tool's run makes, its name clipped to [`STEP_TOOL_CHARS`].
    pub fn of(tool: &str, ok: bool, ms: u64) -> Self {
        Self {
            tool: clip(tool, STEP_TOOL_CHARS),
            ok,
            ms,
        }
    }
}

/// A file delivered into a Desk conversation, by reference: the artifact
/// holding its bytes in the store of the agent that delivered it, and what
/// is cheap to know about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeskFile {
    pub artifact_id: String,
    pub agent_id: String,
    pub filename: String,
    pub media_type: String,
    /// Bytes.
    #[serde(default)]
    pub size: u64,
    /// Pixels, for an image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// The line its producer gave it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
}

/// The download path of `artifact_id` in `agent`'s store, on its hub.
pub fn download_path(agent: &str, artifact_id: &str) -> String {
    format!("/api/agents/{agent}/attachments/{artifact_id}")
}

/// A Desk turn as the turn loop runs it: what Desk sent, where its later
/// output goes, the client's id for the turn, and the id its answer is saved
/// under. It rides the NATS chat request to whichever instance runs the turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeskTurnContext {
    pub turn: DeskTurn,
    pub target: DeskTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_ref: Option<String>,
    pub reply_id: String,
}

impl DeskTurnContext {
    pub fn new(
        turn: DeskTurn,
        anchor_channel: &str,
        device: Option<&str>,
        spoken: bool,
        turn_ref: Option<String>,
        reply_id: String,
    ) -> Self {
        let target = DeskTarget::of_turn(&turn.place, anchor_channel, device, spoken);
        Self {
            turn,
            target,
            turn_ref,
            reply_id,
        }
    }
}

/// The Desk conversation a run's output reaches, whose it is, and whether a
/// person is there to answer: a live Desk turn is attended, a job or
/// workflow it set up is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeskScope {
    pub target: DeskTarget,
    /// The person (or paired device) whose conversation `target` is.
    pub owner: String,
    pub attended: bool,
}

/// What the hub link tells a Desk when a message lands in one of its
/// person's conversations outside a live turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeskDelivered {
    pub agent_id: String,
    pub channel: String,
    pub session_id: String,
    pub message_id: String,
    pub kind: DeliveryKind,
    pub place: DeskPlace,
    #[serde(default)]
    pub spoken: bool,
}

/// What else the hub link tells a Desk changed of its person's: what needs
/// them and what runs (the floor), or their plans' usage. A signal only:
/// the Desk reads the route again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeskSignal {
    Floor,
    Usage,
    /// How far the person has read one of their conversations moved; its
    /// frame names the conversation.
    Read,
}

impl DeskSignal {
    /// The frame the link sends it as.
    pub fn frame(self) -> &'static str {
        match self {
            Self::Floor => "floor_changed",
            Self::Usage => "usage_changed",
            Self::Read => "read_changed",
        }
    }

    /// The feature a hub that sends it declares.
    pub fn feature(self) -> &'static str {
        match self {
            Self::Floor => "desk.floor",
            Self::Usage => "desk.usage",
            Self::Read => "desk.reads",
        }
    }

    /// The signal a frame is, if it is one.
    pub fn of_frame(frame: &str) -> Option<Self> {
        [Self::Floor, Self::Usage, Self::Read]
            .into_iter()
            .find(|s| s.frame() == frame)
    }
}

/// The ids a live Desk turn's frames carry: the client's own for the turn,
/// and the id its answer is saved under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnStamp {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_ref: Option<String>,
    pub reply_id: String,
}

impl From<&DeskTurnContext> for TurnStamp {
    fn from(desk: &DeskTurnContext) -> Self {
        Self {
            turn_ref: desk.turn_ref.clone(),
            reply_id: desk.reply_id.clone(),
        }
    }
}

/// What a live turn is doing, as a reconnecting Desk shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    /// Waiting on the model.
    Thinking,
    /// Streaming its answer.
    Writing,
    /// Running a tool.
    Working,
    /// Compacting its context.
    Tidying,
    /// Held by a rate limit or switching model.
    Waiting,
}

/// A live turn as it stands: what a Desk that connects mid-turn replaces
/// its draft with, so the deltas after it carry on from the same text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnSnapshot {
    #[serde(flatten)]
    pub stamp: TurnStamp,
    pub since: chrono::DateTime<chrono::Utc>,
    pub status: TurnPhase,
    pub text: String,
}

/// How a live Desk turn ended: its client's id, and the id its answer was
/// saved under when one was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnEnd {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ROOM: &str = "6f1c2a4e-0b7d-4c55-9a51-3d2e8f9b1c07";

    fn room_id() -> Uuid {
        Uuid::parse_str(ROOM).unwrap()
    }

    fn in_room(thread: Option<Uuid>) -> DeskPlace {
        DeskPlace::Room {
            room: room_id(),
            name: "Planning".into(),
            thread,
        }
    }

    fn turn(place: DeskPlace) -> DeskTurn {
        DeskTurn {
            place,
            speaker: DeskSpeaker::Person,
            hop: 0,
            context: Vec::new(),
            message: None,
            spoken: false,
        }
    }

    fn line(n: usize, chars: usize) -> DeskLine {
        DeskLine {
            from: format!("p{n}"),
            text: "x".repeat(chars),
        }
    }

    #[test]
    fn a_channel_anchors_the_dm_the_call_a_room_or_an_old_room() {
        let room = format!("desk:{ROOM}");
        assert_eq!(DeskAnchor::of("desk"), Some(DeskAnchor::Dm));
        assert_eq!(DeskAnchor::of("desk:tuple"), Some(DeskAnchor::Call));
        assert_eq!(DeskAnchor::of(&room), Some(DeskAnchor::Room(room_id())));
        assert_eq!(
            DeskAnchor::of("desk:planning"),
            Some(DeskAnchor::Slug("planning"))
        );
        for channel in ["ios", "slack:C1", "desk:", "desk:Planning", "desks"] {
            assert_eq!(DeskAnchor::of(channel), None, "{channel}");
        }
        assert!(is_desk_conversation("desk") && is_desk_conversation(&room));
        assert!(!is_desk_conversation("web"));
    }

    #[test]
    fn a_turn_must_be_sent_on_its_places_conversation() {
        let room = format!("desk:{ROOM}");
        let other = format!("desk:{}", Uuid::new_v4());
        let cases = [
            (DeskPlace::Dm, "desk", Ok(())),
            (DeskPlace::Dm, room.as_str(), Err(DeskTurnError::WrongPlace)),
            (DeskPlace::Call, "desk:tuple", Ok(())),
            (DeskPlace::Call, "desk", Err(DeskTurnError::WrongPlace)),
            (DeskPlace::Dm, "desk:tuple", Err(DeskTurnError::WrongPlace)),
            (in_room(None), room.as_str(), Ok(())),
            (
                in_room(None),
                other.as_str(),
                Err(DeskTurnError::WrongPlace),
            ),
            (in_room(None), "desk:planning", Ok(())),
            (in_room(None), "desk", Err(DeskTurnError::WrongPlace)),
            (in_room(None), "desk:tuple", Err(DeskTurnError::WrongPlace)),
            (
                DeskPlace::LegacyRoom {
                    slug: "planning".into(),
                },
                "desk:planning",
                Ok(()),
            ),
            (
                DeskPlace::LegacyRoom {
                    slug: "other".into(),
                },
                "desk:planning",
                Err(DeskTurnError::WrongPlace),
            ),
            (DeskPlace::Dm, "ios", Err(DeskTurnError::NotDesk)),
        ];
        for (place, anchor, expected) in cases {
            let got = turn(place.clone()).validated(anchor).map(|_| ());
            assert_eq!(got, expected, "{place:?} on {anchor}");
        }
    }

    #[test]
    fn hand_offs_stop_at_the_hop_bound() {
        let hop = |hop| DeskTurn {
            hop,
            speaker: DeskSpeaker::Agent {
                name: "helper".into(),
            },
            ..turn(DeskPlace::Dm)
        };
        assert!(hop(MAX_HOPS).validated("desk").unwrap().is_hand_off());
        assert_eq!(
            hop(MAX_HOPS + 1).validated("desk"),
            Err(DeskTurnError::TooManyHops)
        );
    }

    #[test]
    fn the_thread_lines_keep_the_newest_within_the_caps() {
        let many = DeskTurn {
            context: (0..30)
                .map(|n| line(n, 10))
                .chain([DeskLine {
                    from: "p30".into(),
                    text: "   ".into(),
                }])
                .collect(),
            ..turn(DeskPlace::Dm)
        }
        .validated("desk")
        .unwrap();
        assert_eq!(many.context.len(), CONTEXT_LINES);
        assert_eq!(many.context.first().unwrap().from, "p18");
        assert_eq!(many.context.last().unwrap().from, "p29");

        let long = DeskTurn {
            context: (10..25).map(|n| line(n, 900)).collect(),
            ..turn(DeskPlace::Dm)
        }
        .validated("desk")
        .unwrap();
        assert!(long
            .context
            .iter()
            .all(|l| l.text.chars().count() == LINE_CHARS));
        assert_eq!(long.context.last().unwrap().from, "p24");
        assert_eq!(long.context.len(), CONTEXT_BYTES / (3 + 2 + LINE_CHARS));
    }

    #[test]
    fn an_old_desk_s_turn_is_the_person_in_the_conversation_it_was_sent_on() {
        assert_eq!(DeskTurn::legacy("desk").unwrap().place, DeskPlace::Dm);
        assert_eq!(
            DeskTurn::legacy("desk:tuple").unwrap().place,
            DeskPlace::Call
        );
        assert_eq!(
            DeskTurn::legacy("desk:planning").unwrap(),
            turn(DeskPlace::LegacyRoom {
                slug: "planning".into()
            })
        );
        assert!(DeskTurn::legacy("web").is_none());
    }

    #[test]
    fn a_place_marks_its_room_and_thread() {
        let thread = Uuid::new_v4();
        assert_eq!(
            in_room(Some(thread)).mark(),
            DeskMark {
                room: Some(room_id()),
                thread: Some(thread),
                ..DeskMark::default()
            }
        );
        assert_eq!(DeskPlace::Dm.mark(), DeskMark::default());
        assert_eq!(
            turn(in_room(None)).mark(),
            DeskMark {
                room: Some(room_id()),
                speaker: Some(DeskSpeaker::Person),
                ..DeskMark::default()
            }
        );
        assert_eq!(DeskPlace::Call.mark(), DeskMark::default());
    }

    #[test]
    fn a_target_falls_back_to_the_dm() {
        let room = format!("desk:{ROOM}");
        let target = DeskTarget::of_turn(&in_room(None), &room, Some("dev-1"), true);
        assert_eq!(target.channel, room);
        assert_eq!(target.device.as_deref(), Some("dev-1"));
        let dm = target.dm();
        assert_eq!(
            dm,
            DeskTarget {
                place: DeskPlace::Dm,
                channel: DESK_CHANNEL.into(),
                device: None,
                spoken: true,
            }
        );
        assert_eq!(
            DeskTarget::of_turn(&DeskPlace::Call, "desk:tuple", Some("dev-1"), false),
            DeskTarget::dm_of(false)
        );
        let legacy = DeskTarget::of_turn(
            &DeskPlace::LegacyRoom {
                slug: "planning".into(),
            },
            "desk:planning",
            Some("dev-1"),
            false,
        );
        assert_eq!(
            (legacy.channel.as_str(), legacy.device),
            (DESK_CHANNEL, None)
        );
    }

    #[test]
    fn the_wire_shapes_are_stable() {
        let thread = Uuid::parse_str("0d9a7d43-5c1e-4d8f-8e3b-2b8f6f0e4a11").unwrap();
        let sent = json!({
            "place": {"kind": "room", "room": ROOM, "name": "Planning", "thread": thread},
            "speaker": {"kind": "agent", "name": "helper"},
            "hop": 2,
            "context": [{"from": "p1", "text": "hello"}],
        });
        let turn: DeskTurn = serde_json::from_value(sent).unwrap();
        assert_eq!(turn.place, in_room(Some(thread)));
        assert_eq!((turn.hop, turn.message, turn.spoken), (2, None, false));
        assert_eq!(
            serde_json::to_value(DeskMark {
                delivery: Some(DeliveryKind::CheckIn),
                ..in_room(None).mark()
            })
            .unwrap(),
            json!({"room": ROOM, "delivery": "check_in"})
        );
        let target: DeskTarget =
            serde_json::from_value(json!({"place": {"kind": "dm"}, "channel": "desk"})).unwrap();
        assert_eq!(target, DeskTarget::dm_of(false));
        let answer = DeskMark {
            in_reply_to: Some(thread),
            steps: vec![DeskStep::of("web.search", true, 840)],
            ..in_room(None).mark()
        };
        let written = json!({
            "room": ROOM,
            "in_reply_to": thread,
            "steps": [{"tool": "web.search", "ok": true, "ms": 840}],
        });
        assert_eq!(serde_json::to_value(&answer).unwrap(), written);
        assert_eq!(serde_json::from_value::<DeskMark>(written).unwrap(), answer);
        let before: DeskMark = serde_json::from_value(json!({"room": ROOM})).unwrap();
        assert_eq!(
            before,
            in_room(None).mark(),
            "a mark kept before answers reads as it was"
        );
        assert_eq!(
            DeskStep::of(&"t".repeat(200), false, 1).tool.len(),
            STEP_TOOL_CHARS
        );
        let newer: DeskMark = serde_json::from_value(json!({"delivery": "digest"})).unwrap();
        assert_eq!(
            newer.delivery,
            Some(DeliveryKind::Other),
            "a kind named later is still read"
        );
    }
}
