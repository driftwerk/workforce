//! A follow-up a Desk DM turn took in after its run last looked: the person
//! was told it was taken in, but the run never read it. It is kept after the
//! turn's answer, under the client's id and stamped when it arrived, and the
//! hub answers it itself as the conversation's next turn, whether or not any
//! Desk is there to ask again. A Desk reconciles both by that id.

use std::sync::Arc;
use std::time::Duration;

use workforce_agent::tools::approval::SelfServe;
use workforce_common::channels::desk::{DeskPlace, DeskSpeaker, DeskTurn, DeskTurnContext};

use crate::hub_core::live_turns::{Ending, Heard};
use crate::serve::AgentService;

/// How long the next turn waits for the turns before it to let the session
/// go.
const WAIT: Duration = Duration::from_secs(20 * 60);

/// What a turn does with the follow-ups its run never read: those to keep
/// with the ones it read (before its answer), and those to keep after its
/// answer and answer as the next turn.
#[derive(Debug, Default)]
pub(crate) struct Unread {
    pub now: Vec<Heard>,
    pub after: Vec<Heard>,
}

/// [`Unread`] for a turn asked in `desk`. In the person's DM they are the
/// next turn's, each under an id a Desk can know it by (the client's, else
/// one of the hub's); anywhere else (a room, a chat that isn't Desk's) they
/// are kept as the turn always kept them.
pub(crate) fn unread_plan(desk: Option<&DeskTurnContext>, unread: Vec<Heard>) -> Unread {
    match desk {
        Some(desk) if matches!(desk.turn.place, DeskPlace::Dm { .. }) => Unread {
            now: Vec::new(),
            after: unread
                .into_iter()
                .map(|heard| Heard {
                    external_id: Some(asked_as(heard.external_id.as_deref()).to_string()),
                    ..heard
                })
                .collect(),
        },
        _ => Unread {
            now: unread,
            after: Vec::new(),
        },
    }
}

/// The Desk turn the follow-ups a turn took in are heard in: its own, or,
/// for a turn of the person's DM that came from no Desk (the crew's own),
/// the person's turn there.
pub(crate) fn heard_in(desk: Option<&DeskTurnContext>, channel: &str) -> Option<DeskTurnContext> {
    desk.cloned().or_else(|| {
        (channel == workforce_common::channels::DESK_CHANNEL).then(|| {
            DeskTurnContext::new(
                DeskTurn {
                    place: DeskPlace::Dm { thread: None },
                    speaker: DeskSpeaker::Person,
                    hop: 0,
                    context: Vec::new(),
                    message: None,
                    spoken: false,
                    voice: None,
                    members: Vec::new(),
                    edit: None,
                    starts_thread: false,
                },
                channel,
                None,
                false,
                None,
                uuid::Uuid::new_v4().to_string(),
            )
        })
    })
}

/// The id a follow-up is known by as the next turn's question.
fn asked_as(id: Option<&str>) -> uuid::Uuid {
    id.and_then(|id| uuid::Uuid::parse_str(id).ok())
        .unwrap_or_else(uuid::Uuid::new_v4)
}

/// The next turn of `desk`'s conversation, asked with the follow-up kept
/// last of `after`: its question is that message, already in the session.
pub(crate) fn next_of(
    desk: &DeskTurnContext,
    after: &[Heard],
) -> Option<(DeskTurnContext, String)> {
    let last = after.last()?;
    let asked = asked_as(last.external_id.as_deref());
    Some((
        DeskTurnContext {
            turn: DeskTurn {
                message: Some(asked),
                ..desk.turn.clone()
            },
            target: desk.target.clone(),
            turn_ref: None,
            reply_id: uuid::Uuid::new_v4().to_string(),
        },
        last.text.clone(),
    ))
}

tokio::task_local! {
    /// Set around a turn [`answer_left`] runs: it answers the question the
    /// conversation holds under its id, or nothing.
    static HELD_ONLY: ();
}

/// Whether the turn being set up may answer only a question the
/// conversation already holds, never one it adds.
pub(crate) fn held_only() -> bool {
    HELD_ONLY.try_with(|_| ()).is_ok()
}

/// What a turn [`answer_left`] runs comes to when the question it was to
/// answer is no longer held: answered since (the person asked again), or
/// left behind in a session a rollover sealed.
#[derive(Debug)]
pub(crate) struct NothingHeld;

impl std::fmt::Display for NothingHeld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the follow-up is no longer waiting for an answer")
    }
}

impl std::error::Error for NothingHeld {}

/// Who asks, and where, for [`answer_left`].
pub(crate) struct NextTurn {
    pub session_id: String,
    pub channel: String,
    pub user: String,
    pub run_as: String,
    pub trust_level: Option<workforce_common::trust::TrustLevel>,
    pub desk: DeskTurnContext,
    pub prompt: String,
}

/// Answers the follow-up `next` names as the session's next turn, once the
/// turn before it has let the session go, and tells the person's Desks of
/// the answer. No client is needed: a Desk that is there reads it by its
/// ids, and one that comes later reads it from the conversation.
pub(crate) async fn answer_left(service: Arc<AgentService>, next: NextTurn) {
    let (service, next) = (&service, &next);
    match crate::live_turns::when_free(
        service,
        || Some(next.session_id.clone()),
        WAIT,
        std::future::pending(),
        move || attempt(service, next),
    )
    .await
    {
        Ok(answer) => announce(service, next, &answer).await,
        Err(e) if e.downcast_ref::<NothingHeld>().is_some() => {
            tracing::info!(session = %next.session_id, "a follow-up its turn never read was answered meanwhile; nothing to run");
        }
        Err(e) => {
            tracing::warn!(session = %next.session_id, error = %e, "answering a follow-up its turn never read failed");
        }
    }
}

async fn attempt(service: &Arc<AgentService>, next: &NextTurn) -> eyre::Result<String> {
    let (mut live, inbox) = service
        .begin_live_turn(&next.session_id, SelfServe::Unanswerable, Some(&next.desk))
        .await?;
    let progress = HELD_ONLY
        .scope(
            (),
            crate::serve::chat_turn(service.process_message_with_progress(
                &next.channel,
                &next.user,
                Some(&next.run_as),
                &next.prompt,
                None,
                Some(&next.session_id),
                None,
                None,
                None,
                None,
                Some(inbox),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                next.trust_level,
                Some(live.cancel_token()),
                None,
                Some(&next.desk),
            )),
        )
        .await?;
    let conducted = crate::live_turns::conduct(&mut live, service, progress, |mut rx| async move {
        while rx.recv().await.is_some() {}
    })
    .await;
    match conducted.ending {
        Ending::Finished(result) => Ok(result?.response),
        Ending::Stopped => Err(crate::hub_core::live_turns::TurnStopped.into()),
        Ending::Failed(e) => Err(eyre::eyre!("the next turn did not finish: {e}")),
    }
}

async fn announce(service: &AgentService, next: &NextTurn, answer: &str) {
    let session = match service.sessions.get(&next.session_id) {
        Ok(Some(session)) => session,
        _ => return,
    };
    let owner = session.owner.clone();
    if let Err(e) = crate::desk_surface::resumed(service, &session, &owner, answer).await {
        tracing::warn!(session = %next.session_id, error = %e, "telling Desk of a follow-up's answer failed");
    }
}
