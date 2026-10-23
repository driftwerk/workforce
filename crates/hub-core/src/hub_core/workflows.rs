//! The shapes a workflow run shows the hub: summaries, triggers, stream events, decisions.

use crate::run_steering::SteerRefused;
use uuid::Uuid;
use workforce_core::access::Visibility;

/// A workflow lifecycle event delivered to a streaming HTTP trigger caller.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum WorkflowStreamEvent {
    StepStarted {
        step_id: String,
    },
    StepCompleted {
        step_id: String,
        report: Option<String>,
        outputs: Option<serde_json::Value>,
    },
    Completed {
        result: serde_json::Value,
    },
    Failed {
        reason: String,
    },
}

/// Why an automated run of a definition must not fire now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomatedRunBlock {
    Unknown,
    Tainted,
    Unshared,
    Inactive,
}

impl std::fmt::Display for AutomatedRunBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unknown => "no such workflow",
            Self::Tainted => {
                "last edited by someone other than the identity it runs as; automated runs \
                 resume when that identity re-saves it, or when whoever edited it saves it \
                 with run_as set to themselves"
            }
            Self::Unshared => {
                "its run identity no longer has manage access; the owner or an administrator \
                 must re-save it with run_as set to owner"
            }
            Self::Inactive => "the identity it runs as is not active",
        })
    }
}

/// The public URLs of one HTTP trigger.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TriggerUrls {
    pub trigger_id: String,
    /// The minted address, unchanged by who owns the workflow. Absent on a hub
    /// with no definition store, which mints nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The owner-derived address, present only while it still answers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub legacy_url: Option<String>,
}

/// Summary of a workflow definition for listing. `id` is the addressing key
/// (owner-namespaced for member definitions) the run endpoint expects.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WorkflowSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub step_count: usize,
    pub owner: Option<String>,
    pub visibility: Visibility,
    /// Identity automated runs execute as when it is not the owner.
    pub run_as: Option<String>,
    /// Why automated runs are paused, for a definition with triggers; `None`
    /// when they may fire.
    pub automated_runs_blocked: Option<String>,
}

/// A press asked for by a person on any surface.
pub struct DecisionAsk {
    pub gate: Option<Uuid>,
    pub choice: String,
    pub feedback: Option<String>,
}

/// What a settlement decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionMade {
    pub outcome: String,
    pub label: String,
    pub done: String,
    pub summary: String,
}

/// Why a press settled nothing. The message is what the person sees.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecisionRefused {
    #[error(transparent)]
    Steer(#[from] SteerRefused),
    #[error("This run is not waiting on a decision.")]
    NotWaiting,
    #[error("This run has ended ({status}); nothing was decided.")]
    Ended { status: String },
    #[error("Already decided: {summary}.")]
    Decided { summary: String },
    #[error("That decision is closed; the run has moved on.")]
    Outdated,
    #[error("Choose one of: {choices}.")]
    Choose { choices: String },
    #[error(
        "That choice goes to step '{step}', which the workflow no longer has; choose another, \
         or stop the run."
    )]
    StepGone { step: String },
    #[error("This run's workflow is no longer there, so it can only be stopped.")]
    WorkflowGone,
    #[error("Could not record the decision: {0}")]
    Unavailable(String),
}

impl From<eyre::Report> for DecisionRefused {
    fn from(error: eyre::Report) -> Self {
        Self::Unavailable(format!("{error:#}"))
    }
}

/// A resume asked for on any surface: a press when the run waits on a
/// decision, otherwise today's resume.
pub struct ResumeAsk {
    pub outcome: Option<String>,
    pub gate: Option<Uuid>,
    pub feedback: Option<String>,
    pub channel_id: Option<String>,
    pub thread_id: Option<String>,
    pub originating_agent_id: Option<String>,
}

pub enum Resumed {
    Moved { applied: Option<bool> },
    Decided(DecisionMade),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResumeRefused {
    #[error(transparent)]
    Steer(SteerRefused),
    #[error(transparent)]
    Decision(DecisionRefused),
    #[error("{0}")]
    Failed(String),
}

/// A template in a listing: the bare id, a display name (the JSON's optional
/// `name`, else the id), its owner (`None` = system) and visibility.
#[derive(Clone, serde::Serialize)]
pub struct ConsolidationTemplateSummary {
    pub id: String,
    pub name: String,
    pub owner: Option<String>,
    pub visibility: Visibility,
}
