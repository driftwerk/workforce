//! Desk, the desktop app, as a surface of the hub: what an agent is told
//! about a Desk turn, and who it is for.

mod context;

use workforce_common::channels::desk::DeskTurn;
use workforce_common::trust::TrustLevel;

pub(crate) use context::{desk_context, person_name};

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
