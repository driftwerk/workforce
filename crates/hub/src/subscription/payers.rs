//! Who pays for each kind of model call, by the origin its cost row names.
//! A call's payer is the person whose plan its subscription login is drawn
//! from (`current_payer`): the run-as identity of the turn, job or run, or
//! the person background work is billed to. A person's own route can only
//! follow a call that carries one, so every origin says here where its
//! payer comes from, and the places a call runs with no person at all are
//! listed in [`WITHOUT_A_PERSON`]. A new origin does not compile until it is
//! given a source.

use workforce_common::subscription::Origin;

/// Where a call of one origin finds its payer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayerSource {
    /// The person the work is for, in scope where the call is made: the
    /// turn's run-as identity, a job's or run's principal, or the person
    /// background work is billed to. When the work has no person (see
    /// [`WITHOUT_A_PERSON`]) the call stays on the hub's route.
    ForWhom,
    /// No person by design: the hub's own upkeep, marked with
    /// `unowned_by_design`.
    HubOwn,
    /// An outside caller, never served by a person's own plan.
    Outsider,
}

/// Every origin a cost row can name.
pub const ORIGINS: [Origin; 24] = [
    Origin::Interactive,
    Origin::Voice,
    Origin::Heartbeat,
    Origin::Scheduled,
    Origin::Monitor,
    Origin::Workflow,
    Origin::Consolidation,
    Origin::SelfLearning,
    Origin::Subagent,
    Origin::Games,
    Origin::ApiTask,
    Origin::Coding,
    Origin::Advisor,
    Origin::Compaction,
    Origin::PassiveMemory,
    Origin::Lessons,
    Origin::Vision,
    Origin::Transcription,
    Origin::Knowledge,
    Origin::McpTool,
    Origin::VoiceSummary,
    Origin::Floor,
    Origin::DeskReading,
    Origin::DeskBoss,
];

/// Where a call of `origin` finds its payer.
pub const fn payer_source(origin: Origin) -> PayerSource {
    match origin {
        // In the turn, which runs as its person.
        Origin::Interactive
        | Origin::Voice
        | Origin::DeskBoss
        | Origin::Subagent
        | Origin::Coding
        | Origin::Advisor
        | Origin::Vision
        | Origin::Transcription
        | Origin::McpTool => PayerSource::ForWhom,
        // A heartbeat runs as its signed creator; a job, workflow run or API
        // task as its principal; a monitor as the person it watches for.
        Origin::Heartbeat
        | Origin::Scheduled
        | Origin::Monitor
        | Origin::Workflow
        | Origin::ApiTask => PayerSource::ForWhom,
        // Background work billed to the person whose notes or session it
        // works on, carried across its spawn.
        Origin::Consolidation
        | Origin::SelfLearning
        | Origin::Compaction
        | Origin::PassiveMemory
        | Origin::Lessons
        | Origin::VoiceSummary
        | Origin::Floor => PayerSource::ForWhom,
        // The artifact tagger and know-how settling are billed to their
        // person; the shared knowledge index is the hub's own.
        Origin::Knowledge => PayerSource::ForWhom,
        // Reading a Desk session's reply, for a personal hub's Desk.
        Origin::DeskReading => PayerSource::HubOwn,
        Origin::Games => PayerSource::Outsider,
    }
}

/// A place a model call runs with no person behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoPerson {
    pub origin: Origin,
    pub what: &'static str,
    pub why: &'static str,
}

/// The places a model call runs with no person in scope. Each stays on the
/// hub's route.
pub const WITHOUT_A_PERSON: &[NoPerson] = &[
    NoPerson {
        origin: Origin::Knowledge,
        what: "the shared knowledge index's summaries (a background pass, or one a knowledge tool call starts)",
        why: "the index is the hub's own, whoever asks for it",
    },
    NoPerson {
        origin: Origin::DeskReading,
        what: "the Desk crew's decisions and its reading of a session's reply",
        why: "a personal hub's Desk, run as the hub's own work",
    },
    NoPerson {
        origin: Origin::Floor,
        what: "the floor's gists on a personal hub's Desk",
        why: "a personal hub's Desk, run as the hub's own work",
    },
    NoPerson {
        origin: Origin::Workflow,
        what: "a workflow run started with the static admin token, or from a team channel by a Slack user with no identity",
        why: "no person started it",
    },
    NoPerson {
        origin: Origin::ApiTask,
        what: "a task sent with the static admin token",
        why: "no person sent it",
    },
    NoPerson {
        origin: Origin::Heartbeat,
        what: "a heartbeat check that is unsigned or was edited since it was signed",
        why: "it runs as the heartbeat sentinel, not its creator",
    },
    NoPerson {
        origin: Origin::Scheduled,
        what: "a scheduled job with no person to run as",
        why: "it runs as the scheduler sentinel",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_origin_is_listed_once() {
        let mut names: Vec<&str> = ORIGINS.iter().map(|origin| origin.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ORIGINS.len());
        for origin in ORIGINS {
            let round_trip: Origin =
                serde_json::from_value(serde_json::json!(origin.as_str())).unwrap();
            assert_eq!(round_trip, origin);
        }
    }

    /// The model calls still made with no person in scope, so a person's
    /// own route can't follow them. A new one must be added here, said why,
    /// or given its payer.
    #[test]
    fn model_calls_without_a_payer_are_listed() {
        let without: Vec<(&str, &str)> = WITHOUT_A_PERSON
            .iter()
            .map(|place| (place.origin.as_str(), place.what))
            .collect();
        assert_eq!(
            without
                .iter()
                .map(|(origin, _)| *origin)
                .collect::<Vec<_>>(),
            [
                "knowledge",
                "desk_reading",
                "floor",
                "workflow",
                "api_task",
                "heartbeat",
                "scheduled"
            ],
            "{without:#?}"
        );
        for place in WITHOUT_A_PERSON {
            assert_ne!(
                payer_source(place.origin),
                PayerSource::Outsider,
                "an outside caller has no person by definition: {}",
                place.what
            );
            assert!(!place.why.is_empty());
        }
        let hub_own: Vec<Origin> = ORIGINS
            .into_iter()
            .filter(|origin| payer_source(*origin) == PayerSource::HubOwn)
            .collect();
        assert!(
            hub_own
                .iter()
                .all(|origin| WITHOUT_A_PERSON.iter().any(|p| p.origin == *origin)),
            "every origin with no person by design is listed: {hub_own:?}"
        );
    }
}
