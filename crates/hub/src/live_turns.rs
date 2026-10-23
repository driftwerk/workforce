//! Conducting a live turn: running it to its end on this instance, settling
//! its stop, and the events that close it for a chat client. The registry the
//! turn lives in is [`crate::hub_core::live_turns`].

use std::future::Future;

use tokio::sync::mpsc;
use tracing::warn;
use workforce_agent::runner::{ProgressEvent, ServedLog};
use workforce_core::code_citation::CodeCitation;
use workforce_nats::chat::ChatProgress;

use crate::hub_core::live_turns::{relay, FORWARD_DRAIN, RELAY_BUFFER};
pub(crate) use crate::hub_core::live_turns::{Ending, LiveTurn, LiveTurns};
use crate::serve::{AgentService, MessageProgress};

/// What a conducted turn came to.
pub(crate) struct Conducted {
    pub session_id: String,
    pub ending: Ending,
    code_citations: Vec<CodeCitation>,
    served: ServedLog,
}

impl Conducted {
    /// The events that close the turn for a chat client. A stopped turn closes
    /// silently: the client already finalized it, and a late `stream_end`
    /// would land on the reply to its next message.
    pub(crate) fn closing(self, svc: &AgentService) -> Vec<ChatProgress> {
        match self.ending {
            Ending::Finished(Ok(result)) => crate::chat_worker::after_answer(
                self.code_citations,
                crate::subscription::route_line(svc, &result.model, self.served.served().as_ref()),
            )
            .collect(),
            Ending::Finished(Err(e)) => vec![ChatProgress::Error {
                message: format!("Agent error: {e}"),
            }],
            Ending::Stopped => Vec::new(),
            Ending::Failed(e) => vec![ChatProgress::Error {
                message: format!("Agent task failed: {e}"),
            }],
        }
    }
}

/// Run a started turn to its end and settle it: its progress goes to
/// `forward` until a stop silences it, a stop before the answer aborts
/// the run, and the claim is released before this returns. The caller
/// sends the closing events, then drops the turn — which is what
/// acknowledges a stop.
pub(crate) async fn conduct<Fut>(
    turn: &mut LiveTurn,
    svc: &AgentService,
    progress: MessageProgress,
    forward: impl FnOnce(mpsc::Receiver<ProgressEvent>) -> Fut,
) -> Conducted
where
    Fut: Future<Output = ()> + Send + 'static,
{
    let MessageProgress {
        progress_rx,
        handle,
        session_id,
        turn_token,
        turn_claim_id,
        code_citations,
        served,
        ..
    } = progress;
    turn.alias(&session_id).await;
    let (relay_tx, relay_rx) = mpsc::channel(RELAY_BUFFER);
    let relay = tokio::spawn(relay(progress_rx, relay_tx, turn.watch()));
    let forwarder = tokio::spawn(forward(relay_rx));
    let ending = turn.drive(handle).await;
    if matches!(ending, Ending::Stopped) {
        forwarder.abort();
    } else if tokio::time::timeout(FORWARD_DRAIN, forwarder)
        .await
        .is_err()
    {
        warn!(session_id = %session_id, "Progress forwarder outlived its turn");
    }
    relay.abort();
    if matches!(ending, Ending::Stopped) {
        crate::slack_manager::annotate_stopped_session(
            svc,
            &session_id,
            Some(&turn_token),
            crate::slack_manager::INTERRUPTED_DIRECTIVE,
            &crate::serve::TurnSteps::default(),
        )
        .await;
    }
    if let Some(claim_id) = turn_claim_id {
        crate::session_turn_claim::release_claim(&svc.sessions, &session_id, &claim_id).await;
    }
    Conducted {
        session_id,
        ending,
        code_citations,
        served,
    }
}
