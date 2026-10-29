//! Where output made outside a live turn goes: remembered on a job, a
//! heartbeat check or a workflow run when the turn that set it up came from
//! a surface other than Slack.

use serde::{Deserialize, Serialize};

use crate::channels::desk::{DeskTarget, DESK_PLATFORM};

/// A non-Slack delivery destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryTarget {
    /// Which kind of surface: [`DELIVERY_SURFACE_VOICE`] or
    /// [`DELIVERY_SURFACE_DESK`]. A build that knows neither falls back to
    /// Slack.
    pub surface: String,
    /// Surface-specific channel: for voice, the bridge's gateway channel
    /// name (e.g. `voice-pe`); for Desk, the conversation's channel.
    pub channel: String,
    /// The Desk place and device, on a Desk target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desk: Option<DeskTarget>,
}

/// `DeliveryTarget::surface` for voice-bridge announcement delivery.
pub const DELIVERY_SURFACE_VOICE: &str = "voice";

/// `DeliveryTarget::surface` for delivery to a Desk conversation.
pub const DELIVERY_SURFACE_DESK: &str = DESK_PLATFORM;

impl DeliveryTarget {
    pub fn voice(channel: String) -> Self {
        Self {
            surface: DELIVERY_SURFACE_VOICE.to_string(),
            channel,
            desk: None,
        }
    }

    pub fn desk(target: DeskTarget) -> Self {
        Self {
            surface: DELIVERY_SURFACE_DESK.to_string(),
            channel: target.channel.clone(),
            desk: Some(target),
        }
    }

    pub fn is_voice(&self) -> bool {
        self.surface == DELIVERY_SURFACE_VOICE
    }

    /// The Desk conversation this target names, if it is one.
    pub fn desk_target(&self) -> Option<&DeskTarget> {
        (self.surface == DELIVERY_SURFACE_DESK)
            .then_some(self.desk.as_ref())
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::desk::DeskPlace;
    use serde_json::json;

    #[test]
    fn a_target_written_before_desk_existed_still_reads() {
        let old: DeliveryTarget =
            serde_json::from_value(json!({"surface": "voice", "channel": "voice-pe"})).unwrap();
        assert_eq!(old, DeliveryTarget::voice("voice-pe".into()));
        assert!(old.is_voice() && old.desk_target().is_none());
        assert_eq!(
            serde_json::to_value(&old).unwrap(),
            json!({"surface": "voice", "channel": "voice-pe"})
        );
    }

    #[test]
    fn a_desk_target_round_trips_and_names_its_conversation() {
        let target = DeskTarget {
            place: DeskPlace::Dm,
            channel: "desk".into(),
            device: None,
            spoken: true,
        };
        let stamped = DeliveryTarget::desk(target.clone());
        let wire = serde_json::to_value(&stamped).unwrap();
        assert_eq!(
            wire,
            json!({
                "surface": "desk",
                "channel": "desk",
                "desk": {"place": {"kind": "dm"}, "channel": "desk", "spoken": true},
            })
        );
        let read: DeliveryTarget = serde_json::from_value(wire).unwrap();
        assert_eq!(read.desk_target(), Some(&target));
        assert!(!read.is_voice());
        let unknown = DeliveryTarget {
            surface: "voice".into(),
            ..read
        };
        assert!(unknown.desk_target().is_none());
    }
}
