//! Desk, the desktop app, as a surface of the hub: what an agent is told
//! about a Desk turn and who it is for, delivering to a Desk conversation
//! outside a turn, and the hub link a Desk drains its conversations by.

#[cfg(feature = "postgres")]
mod close;
mod context;
mod deliver;
#[cfg(feature = "postgres")]
pub(crate) mod link;
mod retarget;
mod surface;

use workforce_common::channels::desk::DeskTurn;
use workforce_common::trust::TrustLevel;

#[cfg(feature = "postgres")]
pub(crate) use close::{close, is_room, message_in_chain};
pub(crate) use context::{desk_context, person_name};
pub(crate) use retarget::{retarget, Matches};
pub(crate) use surface::DeskSurface;

/// A message's text as a live channel sends it: secret markers as previews,
/// credentials redacted.
pub(crate) fn shown(content: &str) -> String {
    let rendered = workforce_common::secret_marker::render_markers_as_preview(content);
    match workforce_common::security_scanner::contains_credentials(&rendered) {
        true => workforce_common::security_scanner::redact_credentials(&rendered),
        false => rendered,
    }
}

/// The trust a turn runs at: another agent's hand-off is that agent's
/// output, not the person's, so it is held to [`TrustLevel::Paired`] and
/// scanned as such.
pub(crate) fn hand_off_trust(
    desk: Option<&DeskTurn>,
    trust: Option<TrustLevel>,
) -> Option<TrustLevel> {
    match desk {
        Some(turn) if turn.is_hand_off() => {
            Some(trust.unwrap_or(TrustLevel::Owner).min(TrustLevel::Paired))
        }
        _ => trust,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::channels::desk::{DeskPlace, DeskSpeaker};

    fn from(speaker: DeskSpeaker) -> DeskTurn {
        DeskTurn {
            place: DeskPlace::Dm,
            speaker,
            hop: 1,
            context: Vec::new(),
            message: None,
            spoken: false,
        }
    }

    #[test]
    fn a_hand_off_runs_no_higher_than_paired() {
        let hand_off = from(DeskSpeaker::Agent {
            name: "helper".into(),
        });
        let person = from(DeskSpeaker::Person);
        assert_eq!(
            hand_off_trust(Some(&hand_off), Some(TrustLevel::Owner)),
            Some(TrustLevel::Paired)
        );
        assert_eq!(
            hand_off_trust(Some(&hand_off), None),
            Some(TrustLevel::Paired)
        );
        assert_eq!(
            hand_off_trust(Some(&hand_off), Some(TrustLevel::Untrusted)),
            Some(TrustLevel::Untrusted)
        );
        assert_eq!(
            hand_off_trust(Some(&person), Some(TrustLevel::Owner)),
            Some(TrustLevel::Owner)
        );
        assert_eq!(hand_off_trust(None, None), None);
    }
}
