//! Which surface a turn came from, and the block its message is prefixed
//! with so the agent knows where it is and where its messaging tools post.

use workforce_common::channels::desk::DeskTurnContext;

/// Where a turn was asked.
#[derive(Debug, Clone, Copy)]
pub(crate) enum TurnSurface<'a> {
    Slack {
        channel_id: &'a str,
        thread_id: Option<&'a str>,
    },
    Desk(&'a DeskTurnContext),
    Plain,
}

impl<'a> TurnSurface<'a> {
    /// A Slack channel id makes it Slack's; else a Desk turn makes it Desk's.
    pub(crate) fn of(
        channel_id: Option<&'a str>,
        thread_id: Option<&'a str>,
        desk: Option<&'a DeskTurnContext>,
    ) -> Self {
        match (channel_id, desk) {
            (Some(channel_id), _) => Self::Slack {
                channel_id,
                thread_id,
            },
            (None, Some(desk)) => Self::Desk(desk),
            (None, None) => Self::Plain,
        }
    }
}

/// The `[MESSAGING CONTEXT]` block for a turn on `surface`, or `None` on a
/// surface with nothing to say. `person` names whom a Desk turn is for.
pub(crate) fn surface_context(
    surface: &TurnSurface<'_>,
    session_channel: &str,
    session_id: &str,
    user: &str,
    person: Option<&str>,
) -> Option<String> {
    match surface {
        TurnSurface::Slack {
            channel_id,
            thread_id,
        } => {
            let thread_info = thread_id
                .map(|t| format!("\nthread_id: {t}"))
                .unwrap_or_default();
            Some(format!(
                "[MESSAGING CONTEXT — use these values when calling messaging tools]\n\
                 channel: {channel_id}\n\
                 session_channel: {session_channel}{thread_info}\n\
                 session_id: {session_id}\n\
                 user: {user}"
            ))
        }
        TurnSurface::Desk(desk) => Some(crate::desk_surface::desk_context(
            desk,
            session_channel,
            session_id,
            person,
        )),
        TurnSurface::Plain => None,
    }
}
