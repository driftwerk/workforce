//! Answering an agent's prompt in a live turn, however the person answers:
//! the card in their conversation (`permission_response` on `/ws`) or a
//! quick action on Desk's floor (`POST /api/desk/needs/decide`). Both reach
//! the turn the same way, on whichever replica runs it.

use workforce_core::policy::PermissionChoice;
use workforce_nats::chat::TurnControl;

use crate::gateway::{owns_session, steer};
use crate::serve::AgentService;

/// Why an answer reached no prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unanswered {
    /// The session is not the answerer's.
    NotYours,
    /// No live turn took the answer: it ended, or the prompt went.
    NotLive,
}

/// Answer `request_id` in `session_id`'s live turn with `choice`, as
/// `owner`, the person the session's turns run as.
pub(crate) async fn answer(
    nats: Option<&async_nats::Client>,
    service: &AgentService,
    owner: &str,
    session_id: &str,
    request_id: &str,
    choice: PermissionChoice,
) -> Result<(), Unanswered> {
    if !owns_session(service.sessions.as_ref(), session_id, owner) {
        return Err(Unanswered::NotYours);
    }
    let control = TurnControl::Approval {
        request_id: request_id.to_string(),
        approved: choice.is_allow(),
        user: owner.to_string(),
        choice: Some(choice),
    };
    steer(nats, service, session_id, control)
        .await
        .applied()
        .then_some(())
        .ok_or(Unanswered::NotLive)
}
