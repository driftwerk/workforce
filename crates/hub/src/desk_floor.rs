//! The floor as the hub composes it: where an agent's turn happened and
//! whether the person started it.

use workforce_common::channels::{is_platform, DESK_CHANNEL};
use workforce_common::subscription::is_automated_source;
use workforce_desk::floor::Place;

use crate::desk_crew::CHECK_IN;
use crate::slack_manager::RESUME_PROMPT;

/// What an agent's turn came in as.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Origin<'a> {
    pub channel: &'a str,
    pub user: &'a str,
    pub run_as: Option<&'a str>,
    pub message: &'a str,
    pub channel_id: Option<&'a str>,
    pub thread_id: Option<&'a str>,
}

impl Origin<'_> {
    /// Whose work it is: who it runs as, else who sent it.
    pub(crate) fn owner(&self) -> &str {
        self.run_as.unwrap_or(self.user)
    }
}

/// Where a turn, and anything it asks, can be found.
pub(crate) fn place_of(o: &Origin<'_>) -> Place {
    if o.channel == DESK_CHANNEL {
        return Place::Dm;
    }
    if is_platform(o.channel, "slack") {
        let mut key = o.channel.split(':').skip(1);
        let team = key.next().filter(|t| !t.is_empty()).map(str::to_string);
        if let Some(channel) = o.channel_id.or_else(|| key.next()) {
            return Place::Slack {
                team,
                channel: channel.to_string(),
                thread: o.thread_id.map(str::to_string),
            };
        }
    }
    Place::Elsewhere
}

/// Whether the person started the turn themselves, which settles what the
/// agent asked them: not a scheduled job, a workflow step, another agent's
/// hand-off, Desk's own check-in, or a turn resumed after a restart.
pub(crate) fn by_person_of(o: &Origin<'_>) -> bool {
    !is_automated_source(o.user)
        && !o.user.starts_with("agent:")
        && !o.user.starts_with("system")
        && !o.message.starts_with(RESUME_PROMPT)
        && !o.message.starts_with(CHECK_IN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin<'a>(channel: &'a str, user: &'a str, message: &'a str) -> Origin<'a> {
        Origin {
            channel,
            user,
            run_as: None,
            message,
            channel_id: None,
            thread_id: None,
        }
    }

    #[test]
    fn a_turn_is_found_where_it_came_in() {
        assert_eq!(place_of(&origin("desk", "u1", "hi")), Place::Dm);
        let slack = Origin {
            channel_id: Some("C1"),
            thread_id: Some("1700.1"),
            ..origin("slack:T1:C1:1700.1", "u1", "hi")
        };
        assert_eq!(
            place_of(&slack),
            Place::Slack {
                team: Some("T1".into()),
                channel: "C1".into(),
                thread: Some("1700.1".into())
            }
        );
        assert_eq!(
            place_of(&origin("scheduler", "scheduler", "run")),
            Place::Elsewhere
        );
        assert_eq!(place_of(&origin("ios", "u1", "hi")), Place::Elsewhere);
    }

    #[test]
    fn only_the_persons_own_message_is_theirs() {
        assert!(by_person_of(&origin("desk", "u1", "deploy it")));
        assert!(
            by_person_of(&origin("ios", "u1", "deploy it")),
            "a plain /ws message"
        );
        assert!(by_person_of(&origin("slack:T1:C1:1", "u1", "deploy it")));
        assert!(!by_person_of(&origin(
            "scheduler",
            "scheduler",
            "daily report"
        )));
        assert!(!by_person_of(&origin(
            "workflow",
            "workflow-engine",
            "step 2"
        )));
        assert!(!by_person_of(&origin("desk", "agent:scout", "over to you")));
        assert!(!by_person_of(&origin("desk", "u1", RESUME_PROMPT)));
        assert!(!by_person_of(&origin(
            "desk",
            "u1",
            &format!("{CHECK_IN} anything stuck?")
        )));
    }

    #[test]
    fn the_owner_is_who_it_runs_as() {
        let job = Origin {
            run_as: Some("u1"),
            ..origin("scheduler", "scheduler", "run")
        };
        assert_eq!(job.owner(), "u1");
        assert_eq!(origin("desk", "u2", "hi").owner(), "u2");
    }
}
