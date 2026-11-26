//! Conducting a live turn: running it to its end on this instance, settling
//! its stop, and the events that close it for a chat client. The registry the
//! turn lives in is [`crate::hub_core::live_turns`].

use std::future::Future;
use std::time::Duration;

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
    // Follow-ups a stopped turn leaves are saved before its claim goes, so
    // no next turn's question lands ahead of them.
    turn.follow_ups_kept().await;
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

/// How often a session another instance holds is looked at again.
const RECHECK: Duration = Duration::from_millis(500);

/// Runs `attempt`, and again each time another turn held the session
/// `session` names ([`crate::session_turn_claim::held`]), once that turn has
/// let it go ([`let_go`]). Gives up past `within`, or once `stop` resolves.
pub(crate) async fn when_free<T, Fut>(
    service: &AgentService,
    session: impl Fn() -> Option<String>,
    within: Duration,
    stop: impl Future<Output = ()>,
    mut attempt: impl FnMut() -> Fut,
) -> eyre::Result<T>
where
    Fut: Future<Output = eyre::Result<T>>,
{
    let deadline = tokio::time::Instant::now() + within;
    let mut stop = std::pin::pin!(stop);
    loop {
        match attempt().await {
            Err(e) if crate::session_turn_claim::held(&e) => {}
            done => return done,
        }
        let session = session();
        let claimed = || {
            session.as_deref().is_some_and(|s| {
                service
                    .sessions
                    .turn_claim_holder(s)
                    .is_ok_and(|holder| holder.is_some())
            })
        };
        tokio::select! {
            () = &mut stop => eyre::bail!("the hub is shutting down"),
            () = tokio::time::sleep_until(deadline) => eyre::bail!("the conversation stayed busy"),
            () = let_go(&service.live_turns, session.as_deref(), claimed) => {}
        }
    }
}

/// Once the turn holding `session` lets it go: the end of the one live on
/// this instance, then the release of any claim on it (`claimed`), looked at
/// every [`RECHECK`]. With no session to watch, a [`RECHECK`] later.
async fn let_go(live: &LiveTurns, session: Option<&str>, claimed: impl Fn() -> bool) {
    let Some(session) = session else {
        return tokio::time::sleep(RECHECK).await;
    };
    live.ended(session).await;
    while claimed() {
        tokio::time::sleep(RECHECK).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    use workforce_agent::tools::approval::SelfServe;

    use super::*;

    #[tokio::test]
    async fn a_session_is_let_go_once_its_live_turn_ends_and_its_claim_is_released() {
        let live = LiveTurns::new("allan");
        let (turn, _inbox) = live
            .begin("s1", SelfServe::Unanswerable, None, None)
            .await
            .unwrap();
        let claimed = Arc::new(AtomicBool::new(true));
        let waiting = tokio::spawn({
            let (live, claimed) = (Arc::clone(&live), Arc::clone(&claimed));
            async move { let_go(&live, Some("s1"), || claimed.load(Ordering::Acquire)).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(turn);
        tokio::time::sleep(RECHECK + Duration::from_millis(100)).await;
        assert!(!waiting.is_finished(), "its claim still holds it");
        claimed.store(false, Ordering::Release);
        tokio::time::timeout(RECHECK * 3, waiting)
            .await
            .expect("let go once the claim is released")
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_attempt_another_turn_held_is_made_again_once_it_lets_go() {
        let tmp = tempfile::tempdir().unwrap();
        let service = crate::serve::testing::plain_service(tmp.path()).await;
        let (turn, _inbox) = service
            .begin_live_turn("s1", SelfServe::Unanswerable, None)
            .await
            .unwrap();
        let (tries, service) = (&AtomicUsize::new(0), &service);
        let attempt = move || async move {
            match tries.fetch_add(1, Ordering::AcqRel) {
                0 => Err(eyre::Report::new(
                    crate::session_turn_claim::TurnClaimHeld {
                        holder: Some("hub-2".into()),
                        expires_in_secs: Some(30),
                    },
                )),
                _ if service.live_turns.running("s1") => {
                    Err(eyre::Report::new(crate::hub_core::live_turns::TurnBusy))
                }
                n => Ok(n),
            }
        };
        let ran = async {
            when_free(
                service,
                || Some("s1".to_string()),
                Duration::from_secs(5),
                std::future::pending(),
                attempt,
            )
            .await
        };
        let ((), ran) = tokio::join!(
            async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                drop(turn);
            },
            ran
        );
        assert_eq!(ran.unwrap(), 1, "made once more, after the turn ended");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_attempt_gives_up_past_its_wait_or_when_stopped_and_never_retries_a_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let service = crate::serve::testing::plain_service(tmp.path()).await;
        let (_turn, _inbox) = service
            .begin_live_turn("s1", SelfServe::Unanswerable, None)
            .await
            .unwrap();
        let busy =
            || async { Err::<(), _>(eyre::Report::new(crate::hub_core::live_turns::TurnBusy)) };
        let session = || Some("s1".to_string());
        let waited = when_free(
            &service,
            session,
            Duration::from_millis(200),
            std::future::pending(),
            busy,
        )
        .await;
        assert!(waited.unwrap_err().to_string().contains("stayed busy"));
        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();
        let stopped = when_free(
            &service,
            session,
            Duration::from_secs(60),
            stop.cancelled(),
            busy,
        )
        .await;
        assert!(stopped.unwrap_err().to_string().contains("shutting down"));
        let tries = &AtomicUsize::new(0);
        let failed = when_free(
            &service,
            session,
            Duration::from_secs(60),
            std::future::pending(),
            move || async move {
                tries.fetch_add(1, Ordering::AcqRel);
                Err::<(), _>(eyre::eyre!("the model refused"))
            },
        )
        .await;
        assert!(failed.is_err());
        assert_eq!(tries.load(Ordering::Acquire), 1);
    }
}
