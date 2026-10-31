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
pub(crate) use deliver::{deliver, deliver_file, resumed, Delivery, Outbox};
pub(crate) use retarget::retarget;
pub(crate) use surface::DeskSurface;

/// The hub's surface allow list, where it keeps one: what each person
/// always allows an agent to write to from their Desk turns.
pub(crate) fn allows() -> Option<std::sync::Arc<dyn workforce_agent::tools::dispatch::SurfaceAllows>>
{
    #[cfg(feature = "postgres")]
    {
        crate::surface_allows::installed()
    }
    #[cfg(not(feature = "postgres"))]
    {
        None
    }
}

/// What a call that sets up delivery to a Slack channel it names (a job's
/// result, a workflow's status) does to its surface: writes there, so an
/// attended Desk turn asks before it is set up, as it asks before a post.
pub(crate) fn delivers_to_slack(
    input: &serde_json::Value,
) -> Option<workforce_agent::tools::SurfaceEffect> {
    input["channel_id"]
        .as_str()
        .filter(|channel| !channel.trim().is_empty())
        .map(|channel| workforce_agent::tools::SurfaceEffect::Writes {
            platform: "slack".to_string(),
            destination: channel.to_string(),
        })
}

/// A message's text as a live channel sends it: secret markers as previews,
/// credentials redacted.
pub(crate) fn shown(content: &str) -> String {
    let rendered = workforce_common::secret_marker::render_markers_as_preview(content);
    match workforce_common::security_scanner::contains_credentials(&rendered) {
        true => workforce_common::security_scanner::redact_credentials(&rendered),
        false => rendered,
    }
}

/// Where output a run sets up to come later goes, stamped when the job,
/// heartbeat check or workflow run is made: the voice bridge a spoken turn
/// came from, else the Desk conversation the run answers to. `None` leaves
/// it to Slack, as before either existed.
///
/// Reads the per-turn task-locals, never the session: one session is shared
/// by every surface a person uses with an agent, so its flags say what one
/// of the person's turns was, not what this one is. The stamp is permanent
/// and invisible, so it has to be right; whether the surface is reachable
/// is decided when the output is delivered.
pub(crate) fn origin() -> Option<workforce_common::delivery::DeliveryTarget> {
    workforce_agent::runner::CURRENT_VOICE_CHANNEL
        .try_with(Clone::clone)
        .ok()
        .filter(|channel| !channel.is_empty())
        .map(workforce_common::delivery::DeliveryTarget::voice)
        .or_else(|| {
            workforce_agent::runner::current_desk()
                .map(|scope| workforce_common::delivery::DeliveryTarget::desk(scope.target))
        })
}

/// How the Desk turn `desk` in `session_id` ended: its client's id, and
/// its reply id when an answer was saved under it. A turn answered by a slash
/// command, refused by the budget or stopped saved none.
pub(crate) fn turn_end(
    service: &crate::serve::AgentService,
    session_id: &str,
    desk: &workforce_common::channels::desk::DeskTurnContext,
) -> workforce_common::channels::desk::TurnEnd {
    let saved = service
        .sessions
        .get(session_id)
        .ok()
        .flatten()
        .is_some_and(|session| session.messages.iter().any(|m| m.id == desk.reply_id));
    workforce_common::channels::desk::TurnEnd {
        turn_ref: desk.turn_ref.clone(),
        reply_id: saved.then(|| desk.reply_id.clone()),
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

    #[test]
    fn setting_up_delivery_to_a_named_slack_channel_writes_there() {
        assert_eq!(
            delivers_to_slack(&serde_json::json!({"action": "create", "channel_id": "C_OPS"}))
                .and_then(|effect| effect.destination())
                .map(|to| (to.platform, to.target)),
            Some(("slack".to_string(), "c_ops".to_string()))
        );
        for named_none in [
            serde_json::json!({"action": "create"}),
            serde_json::json!({"action": "create", "channel_id": " "}),
        ] {
            assert_eq!(delivers_to_slack(&named_none), None);
        }
    }

    fn from(speaker: DeskSpeaker) -> DeskTurn {
        DeskTurn {
            place: DeskPlace::Dm,
            speaker,
            hop: 1,
            context: Vec::new(),
            message: None,
            spoken: false,
            voice: None,
        }
    }

    #[tokio::test]
    async fn what_a_run_sets_up_goes_to_the_voice_bridge_then_desk_then_slack() {
        use workforce_agent::runner::{with_desk, CURRENT_VOICE_CHANNEL};
        use workforce_common::channels::desk::{DeskScope, DeskTarget};
        use workforce_common::delivery::DeliveryTarget;
        let desk = || {
            Some(DeskScope {
                target: DeskTarget::of_turn(&DeskPlace::Dm, "desk", None, false),
                attended: true,
                owner: "person-1".into(),
            })
        };
        assert_eq!(origin(), None);
        assert_eq!(
            with_desk(desk(), async { origin() }).await,
            Some(DeliveryTarget::desk(DeskTarget::of_turn(
                &DeskPlace::Dm,
                "desk",
                None,
                false
            )))
        );
        let spoken = CURRENT_VOICE_CHANNEL
            .scope("voice-pe".into(), with_desk(desk(), async { origin() }))
            .await;
        assert_eq!(spoken, Some(DeliveryTarget::voice("voice-pe".into())));
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
