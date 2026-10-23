use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use workforce_core::types::AgentId;

/// A unit of work within the system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// Unique task identifier.
    pub id: TaskId,
    /// Parent task (if this is a subtask).
    pub parent_id: Option<TaskId>,
    /// Human-readable title.
    pub title: String,
    /// Detailed description/instructions.
    pub description: String,
    /// Current status.
    pub status: TaskStatus,
    /// Priority (higher = more urgent).
    pub priority: TaskPriority,
    /// Assigned agent (if any).
    pub assignee: Option<AgentId>,
    /// Team this task belongs to.
    pub team_id: Option<String>,
    /// Task metadata/labels.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Timestamps.
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    /// Result of the task (set on completion).
    pub result: Option<TaskResult>,
    /// Subtask IDs.
    #[serde(default)]
    pub subtasks: Vec<TaskId>,
}

/// Strongly-typed task identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub String);

impl TaskId {
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub fn from(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Task lifecycle states.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Task created but not yet assigned.
    Created,
    /// Task assigned to an agent, waiting to start.
    Assigned,
    /// Agent is actively working on the task.
    InProgress,
    /// Work done, awaiting review.
    InReview,
    /// Task completed successfully.
    Complete,
    /// Task failed.
    Failed,
    /// Task cancelled.
    Cancelled,
    /// Task blocked by another task or external factor.
    Blocked,
}

impl TaskStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskStatus::Complete | TaskStatus::Failed | TaskStatus::Cancelled
        )
    }

    pub fn is_active(&self) -> bool {
        matches!(
            self,
            TaskStatus::Assigned | TaskStatus::InProgress | TaskStatus::InReview
        )
    }
}

/// Task priority levels.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskPriority {
    Low,
    #[default]
    Normal,
    High,
    Urgent,
}

/// The outcome of a completed task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    /// Whether the task succeeded.
    pub success: bool,
    /// Summary of what was done.
    pub summary: String,
    /// Structured output data (if any).
    #[serde(default)]
    pub output: serde_json::Value,
    /// Token usage for the task.
    #[serde(default)]
    pub total_tokens: u64,
}

impl Task {
    /// Create a new task.
    pub fn new(title: impl Into<String>, description: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            id: TaskId::new(),
            parent_id: None,
            title: title.into(),
            description: description.into(),
            status: TaskStatus::Created,
            priority: TaskPriority::default(),
            assignee: None,
            team_id: None,
            labels: Vec::new(),
            created_at: now,
            updated_at: now,
            started_at: None,
            completed_at: None,
            result: None,
            subtasks: Vec::new(),
        }
    }

    /// Create a subtask under this task.
    pub fn create_subtask(
        &mut self,
        title: impl Into<String>,
        description: impl Into<String>,
    ) -> Task {
        let mut subtask = Task::new(title, description);
        subtask.parent_id = Some(self.id.clone());
        subtask.team_id = self.team_id.clone();
        self.subtasks.push(subtask.id.clone());
        self.updated_at = Utc::now();
        subtask
    }

    /// Assign the task to an agent.
    pub fn assign(&mut self, agent_id: AgentId) {
        self.assignee = Some(agent_id);
        self.status = TaskStatus::Assigned;
        self.updated_at = Utc::now();
    }

    /// Mark the task as in progress.
    pub fn start(&mut self) {
        self.status = TaskStatus::InProgress;
        self.started_at = Some(Utc::now());
        self.updated_at = Utc::now();
    }

    /// Move to review.
    pub fn submit_for_review(&mut self) {
        self.status = TaskStatus::InReview;
        self.updated_at = Utc::now();
    }

    /// Complete the task with a result.
    pub fn complete(&mut self, result: TaskResult) {
        self.status = TaskStatus::Complete;
        self.completed_at = Some(Utc::now());
        self.updated_at = Utc::now();
        self.result = Some(result);
    }

    /// Fail the task.
    pub fn fail(&mut self, reason: impl Into<String>) {
        self.status = TaskStatus::Failed;
        self.completed_at = Some(Utc::now());
        self.updated_at = Utc::now();
        self.result = Some(TaskResult {
            success: false,
            summary: reason.into(),
            output: serde_json::Value::Null,
            total_tokens: 0,
        });
    }

    /// Cancel the task.
    pub fn cancel(&mut self) {
        self.status = TaskStatus::Cancelled;
        self.completed_at = Some(Utc::now());
        self.updated_at = Utc::now();
    }

    /// Check if the task can be assigned.
    pub fn can_assign(&self) -> bool {
        matches!(self.status, TaskStatus::Created)
    }

    /// Check if all subtasks are complete.
    pub fn all_subtasks_ids(&self) -> &[TaskId] {
        &self.subtasks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_lifecycle() {
        let mut task = Task::new("Build feature", "Implement the login page");
        assert_eq!(task.status, TaskStatus::Created);
        assert!(task.can_assign());

        task.assign(AgentId::new("developer"));
        assert_eq!(task.status, TaskStatus::Assigned);
        assert!(!task.can_assign());
        assert!(task.status.is_active());

        task.start();
        assert_eq!(task.status, TaskStatus::InProgress);
        assert!(task.started_at.is_some());

        task.submit_for_review();
        assert_eq!(task.status, TaskStatus::InReview);

        task.complete(TaskResult {
            success: true,
            summary: "Login page implemented".to_string(),
            output: serde_json::json!({"files_changed": 3}),
            total_tokens: 5000,
        });
        assert_eq!(task.status, TaskStatus::Complete);
        assert!(task.status.is_terminal());
        assert!(task.completed_at.is_some());
        assert!(task.result.as_ref().unwrap().success);
    }

    #[test]
    fn task_failure() {
        let mut task = Task::new("Deploy", "Deploy to prod");
        task.assign(AgentId::new("deployer"));
        task.start();
        task.fail("Connection timeout");
        assert_eq!(task.status, TaskStatus::Failed);
        assert!(!task.result.as_ref().unwrap().success);
    }

    #[test]
    fn subtasks() {
        let mut parent = Task::new("Build feature", "Big feature");
        let sub1 = parent.create_subtask("Backend", "API routes");
        let sub2 = parent.create_subtask("Frontend", "React components");

        assert_eq!(parent.subtasks.len(), 2);
        assert_eq!(sub1.parent_id, Some(parent.id.clone()));
        assert_eq!(sub2.parent_id, Some(parent.id.clone()));
    }

    #[test]
    fn task_priority_ordering() {
        assert!(TaskPriority::Urgent > TaskPriority::High);
        assert!(TaskPriority::High > TaskPriority::Normal);
        assert!(TaskPriority::Normal > TaskPriority::Low);
    }

    #[test]
    fn task_serialization_roundtrip() {
        let mut task = Task::new("Test task", "A description");
        task.assign(AgentId::new("dev"));
        task.labels = vec!["backend".to_string(), "urgent".to_string()];

        let json = serde_json::to_string(&task).unwrap();
        let deserialized: Task = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.title, "Test task");
        assert_eq!(deserialized.assignee, Some(AgentId::new("dev")));
        assert_eq!(deserialized.labels.len(), 2);
    }
}
