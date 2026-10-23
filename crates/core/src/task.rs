use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use strum_macros::{Display, EnumString};

use crate::types::{AgentId, TaskId, TeamId};

/// The lifecycle state of a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Display, EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum TaskStatus {
    /// Task has been created but not yet assigned.
    Created,
    /// Task has been assigned to an agent.
    Assigned,
    /// Agent is actively working on the task.
    InProgress,
    /// Task is in review (e.g., code review).
    InReview,
    /// Task completed successfully.
    Done,
    /// Task failed.
    Failed,
    /// Task was cancelled.
    Cancelled,
}

/// Priority level for a task.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskPriority {
    Low,
    #[default]
    Medium,
    High,
    Urgent,
}

/// A task in the workforce system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// Unique task identifier.
    pub id: TaskId,
    /// Human-readable title.
    pub title: String,
    /// Detailed description of what needs to be done.
    pub description: String,
    /// Current lifecycle status.
    pub status: TaskStatus,
    /// Priority level.
    pub priority: TaskPriority,
    /// The team this task belongs to (if any).
    pub team_id: Option<TeamId>,
    /// The agent assigned to this task (if any).
    pub assigned_to: Option<AgentId>,
    /// Parent task (for subtasks).
    pub parent_id: Option<TaskId>,
    /// When the task was created.
    pub created_at: DateTime<Utc>,
    /// When the task was last updated.
    pub updated_at: DateTime<Utc>,
    /// When the task was completed (if done).
    pub completed_at: Option<DateTime<Utc>>,
    /// Arbitrary metadata.
    pub metadata: serde_json::Value,
}

impl Task {
    /// Create a new task with the given title and description.
    pub fn new(title: impl Into<String>, description: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            id: TaskId::new(),
            title: title.into(),
            description: description.into(),
            status: TaskStatus::Created,
            priority: TaskPriority::default(),
            team_id: None,
            assigned_to: None,
            parent_id: None,
            created_at: now,
            updated_at: now,
            completed_at: None,
            metadata: serde_json::Value::Null,
        }
    }

    /// Assign this task to an agent.
    pub fn assign_to(mut self, agent_id: AgentId) -> Self {
        self.assigned_to = Some(agent_id);
        self.status = TaskStatus::Assigned;
        self.updated_at = Utc::now();
        self
    }

    /// Set the team for this task.
    pub fn with_team(mut self, team_id: TeamId) -> Self {
        self.team_id = Some(team_id);
        self
    }

    /// Set the priority for this task.
    pub fn with_priority(mut self, priority: TaskPriority) -> Self {
        self.priority = priority;
        self
    }

    /// Set this as a subtask of another task.
    pub fn with_parent(mut self, parent_id: TaskId) -> Self {
        self.parent_id = Some(parent_id);
        self
    }

    /// Transition task to in-progress.
    pub fn start(&mut self) {
        self.status = TaskStatus::InProgress;
        self.updated_at = Utc::now();
    }

    /// Transition task to in-review.
    pub fn submit_for_review(&mut self) {
        self.status = TaskStatus::InReview;
        self.updated_at = Utc::now();
    }

    /// Mark task as complete.
    pub fn complete(&mut self) {
        self.status = TaskStatus::Done;
        self.completed_at = Some(Utc::now());
        self.updated_at = Utc::now();
    }

    /// Mark task as failed.
    pub fn fail(&mut self) {
        self.status = TaskStatus::Failed;
        self.updated_at = Utc::now();
    }

    /// Cancel the task.
    pub fn cancel(&mut self) {
        self.status = TaskStatus::Cancelled;
        self.updated_at = Utc::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_lifecycle() {
        let mut task = Task::new("Implement feature", "Build the thing")
            .with_priority(TaskPriority::High)
            .assign_to(AgentId::new("developer"));

        assert_eq!(task.status, TaskStatus::Assigned);

        task.start();
        assert_eq!(task.status, TaskStatus::InProgress);

        task.submit_for_review();
        assert_eq!(task.status, TaskStatus::InReview);

        task.complete();
        assert_eq!(task.status, TaskStatus::Done);
        assert!(task.completed_at.is_some());
    }

    #[test]
    fn task_failure_path() {
        let mut task = Task::new("Failing task", "This will fail");
        task.start();
        task.fail();
        assert_eq!(task.status, TaskStatus::Failed);
        assert!(task.completed_at.is_none());
    }

    #[test]
    fn subtask_relationship() {
        let parent = Task::new("Parent task", "The big task");
        let child = Task::new("Subtask", "A piece of the big task").with_parent(parent.id.clone());
        assert_eq!(child.parent_id, Some(parent.id));
    }
}
