//! A Claude Code or Codex step's round, kept on its instance: a hub that
//! picks the run up after a restart collects the round from the worker
//! rather than running it again over a partly changed worktree.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use eyre::{ensure, Result};
use serde::{Deserialize, Serialize};
use workforce_agent::runner::ToolInvocation;
use workforce_agent::tools::exec_runner::JobId;
use workforce_agent::turn_journal::TurnJournal;
use workforce_orchestrator::workflow::instance::{InstanceStatus, WorkflowInstance};

use super::WorkflowRuntime;
use crate::claude_code::{RoundJobs, RoundKey};

const NOTE_KEY: &str = "_coding_round";

/// The round a step is running, and the last job it reached.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct RoundNote {
    step: String,
    #[serde(flatten)]
    key: RoundKey,
    #[serde(default)]
    checkpoint: Option<String>,
}

impl RoundNote {
    /// The job the round runs next: the one it last reached, else its first.
    /// `None` when its checkpoint is not one of the round's jobs.
    fn jobs(&self) -> Option<RoundJobs> {
        match &self.checkpoint {
            Some(mark) => self.key.resume_at(mark).map(|resume| resume.jobs),
            None => Some(self.key.jobs(false)),
        }
    }
}

/// How a coding step's round runs.
#[derive(Debug, PartialEq)]
pub(super) enum StepRound {
    /// On a worker that does not keep jobs: unkeyed, with nothing noted.
    Unkeyed,
    /// Keyed under `note`, which the step saves before the round starts.
    Keyed(RoundNote, RoundJobs),
    /// A round the step noted but can no longer tell the outcome of.
    Unknown,
}

/// The round for `step`: the one its note records, else a new one. A note
/// another step left is the remains of a run that moved on, and is replaced;
/// one nobody can read is a round nobody can tell the outcome of.
pub(super) fn step_round(instance: &WorkflowInstance, step: &str, kept: bool) -> StepRound {
    let Ok(noted) = note(instance) else {
        return StepRound::Unknown;
    };
    match (kept, noted.filter(|note| note.step == step)) {
        (false, None) => StepRound::Unkeyed,
        (false, Some(_)) => StepRound::Unknown,
        (true, Some(note)) => match note.jobs() {
            Some(jobs) => StepRound::Keyed(note, jobs),
            None => StepRound::Unknown,
        },
        (true, None) => {
            let key = RoundKey::new();
            let jobs = key.jobs(false);
            let note = RoundNote {
                step: step.to_string(),
                key,
                checkpoint: None,
            };
            StepRound::Keyed(note, jobs)
        }
    }
}

fn note(instance: &WorkflowInstance) -> Result<Option<RoundNote>> {
    instance
        .variables
        .get(NOTE_KEY)
        .map(|note| serde_json::from_value(note.clone()))
        .transpose()
        .map_err(Into::into)
}

fn note_of(instance: &WorkflowInstance) -> Option<RoundNote> {
    note(instance).ok().flatten()
}

pub(super) fn keep_note(instance: &mut WorkflowInstance, note: &RoundNote) {
    if let Some(vars) = instance.variables.as_object_mut() {
        vars.insert(
            NOTE_KEY.to_string(),
            serde_json::to_value(note).expect("a round note serialises"),
        );
    }
}

/// Drop the step's note; whether there was one.
pub(super) fn clear_note(instance: &mut WorkflowInstance) -> bool {
    instance
        .variables
        .as_object_mut()
        .is_some_and(|vars| vars.remove(NOTE_KEY).is_some())
}

/// The job a noted round last reached, or would start with, for ending it.
pub(super) fn noted_job(instance: &WorkflowInstance) -> Option<JobId> {
    let note = note_of(instance)?;
    Some(match note.checkpoint {
        Some(mark) => JobId::new(mark),
        None => note.key.jobs(false).job(),
    })
}

/// Where a running step's round records the jobs it reaches: on the saved
/// instance, fenced by the run's lease, and on the loop's own copy once the
/// round returns, so the loop's later saves carry the mark.
pub(super) struct StepJournal {
    runtime: Arc<WorkflowRuntime>,
    instance_id: String,
    note: Mutex<RoundNote>,
}

impl StepJournal {
    pub(super) fn new(runtime: Arc<WorkflowRuntime>, instance_id: &str, note: RoundNote) -> Self {
        Self {
            runtime,
            instance_id: instance_id.to_string(),
            note: Mutex::new(note),
        }
    }

    /// The note as the round left it.
    pub(super) fn note(&self) -> RoundNote {
        self.note
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl TurnJournal for StepJournal {
    async fn pending(
        &self,
        _call: &str,
        _tool: &str,
        _input: &serde_json::Value,
        _plan: serde_json::Value,
    ) -> Result<()> {
        Ok(())
    }

    /// Record `mark` unless the run has moved on from this round, stopped,
    /// paused, skipped or rewound, so that no further job of it starts.
    async fn checkpoint(&self, _call: &str, mark: &str) -> Result<()> {
        let mut note = self.note();
        let mut saved = self.runtime.load_instance(&self.instance_id)?;
        let current = note_of(&saved).map(|noted| noted.key.round);
        ensure!(
            saved.status == InstanceStatus::Running
                && saved.current_step_id.as_deref() == Some(note.step.as_str())
                && current == Some(note.key.round),
            "workflow {} has moved on from step '{}'",
            self.instance_id,
            note.step
        );
        note.checkpoint = Some(mark.to_string());
        keep_note(&mut saved, &note);
        self.runtime.save_instance(&saved)?;
        *self
            .note
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = note;
        Ok(())
    }

    async fn finished(&self, _call: &str, _invocation: &ToolInvocation) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_orchestrator::workflow::definition::WorkflowDefinition;
    use workforce_orchestrator::workflow::engine::WorkflowExecutor;
    use workforce_orchestrator::workflow::instance::{TriggerInfo, TriggerSource};

    fn instance() -> WorkflowInstance {
        let def = WorkflowDefinition::from_toml(
            "[workflow]\nid = \"w\"\nname = \"W\"\n\n\
             [[step]]\nid = \"build\"\nname = \"B\"\ntask = \"go\"\nexit = true\n",
        )
        .unwrap();
        let trigger = TriggerInfo {
            source: TriggerSource::Api,
            payload: serde_json::json!({}),
        };
        WorkflowExecutor::start(&def, trigger, None).unwrap()
    }

    fn noted(step: &str, checkpoint: Option<&str>) -> (WorkflowInstance, RoundNote) {
        let mut instance = instance();
        let note = RoundNote {
            step: step.into(),
            key: RoundKey::new(),
            checkpoint: checkpoint.map(String::from),
        };
        keep_note(&mut instance, &note);
        (instance, note)
    }

    #[test]
    fn a_step_starts_a_new_round_on_a_worker_that_keeps_jobs() {
        let StepRound::Keyed(note, jobs) = step_round(&instance(), "build", true) else {
            panic!("a keyed round");
        };
        assert_eq!((note.step.as_str(), note.checkpoint), ("build", None));
        assert_eq!(jobs.job(), note.key.jobs(false).job());
        assert!(!jobs.pinned());
    }

    #[test]
    fn a_step_resumes_its_noted_round_at_the_job_it_reached() {
        let key = RoundKey::new();
        let reached = key.jobs(false).for_engine(1).job().to_string();
        let mut instance = instance();
        let note = RoundNote {
            step: "build".into(),
            key,
            checkpoint: Some(reached.clone()),
        };
        keep_note(&mut instance, &note);

        let StepRound::Keyed(resumed, jobs) = step_round(&instance, "build", true) else {
            panic!("a keyed round");
        };
        assert_eq!(resumed, note);
        assert_eq!((jobs.job().to_string(), jobs.pinned()), (reached, true));
    }

    #[test]
    fn a_noted_round_whose_outcome_nobody_can_tell_is_not_run_again() {
        let (foreign, _) = noted("build", Some("another-round.0.0"));
        assert_eq!(step_round(&foreign, "build", true), StepRound::Unknown);
        assert_eq!(noted_job(&foreign), Some(JobId::new("another-round.0.0")));
        let mut unreadable = instance();
        unreadable.variables[NOTE_KEY] = serde_json::json!({"step": 3});
        assert_eq!(step_round(&unreadable, "build", true), StepRound::Unknown);
        let (unkept, _) = noted("build", None);
        assert_eq!(step_round(&unkept, "build", false), StepRound::Unknown);
    }

    #[test]
    fn a_note_another_step_left_is_replaced_by_a_new_round() {
        let (instance, left) = noted("plan", Some("x.0.0"));
        let StepRound::Keyed(note, _) = step_round(&instance, "build", true) else {
            panic!("a keyed round");
        };
        assert_eq!(note.step, "build");
        assert_ne!(note.key.round, left.key.round);
        assert_eq!(step_round(&instance, "build", false), StepRound::Unkeyed);
    }

    #[test]
    fn a_cleared_note_leaves_no_job_to_end() {
        let (mut instance, note) = noted("build", None);
        assert_eq!(noted_job(&instance), Some(note.key.jobs(false).job()));
        assert!(clear_note(&mut instance));
        assert!(noted_job(&instance).is_none());
        assert!(!clear_note(&mut instance));
    }
}
