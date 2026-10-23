use std::collections::HashMap;
use std::sync::Arc;

use eyre::Result;
use tokio::sync::RwLock;
use tracing::{info, warn};
use workforce_core::types::AgentId;

use crate::task::{Task, TaskId, TaskResult, TaskStatus};

/// The orchestrator manages tasks and agent coordination.
pub struct Orchestrator {
    tasks: RwLock<HashMap<TaskId, Task>>,
    active_assignments: RwLock<HashMap<AgentId, TaskId>>,
}

impl Orchestrator {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            tasks: RwLock::new(HashMap::new()),
            active_assignments: RwLock::new(HashMap::new()),
        })
    }

    pub async fn submit_task(&self, task: Task) -> TaskId {
        let id = task.id.clone();
        info!(task_id = %id, title = %task.title, "Task submitted");
        self.tasks.write().await.insert(id.clone(), task);
        id
    }

    pub async fn get_task(&self, task_id: &TaskId) -> Option<Task> {
        self.tasks.read().await.get(task_id).cloned()
    }

    pub async fn list_tasks(&self, status_filter: Option<TaskStatus>) -> Vec<Task> {
        let tasks = self.tasks.read().await;
        tasks
            .values()
            .filter(|t| status_filter.as_ref().is_none_or(|s| t.status == *s))
            .cloned()
            .collect()
    }

    /// Total number of tasks resident in the in-memory map. Used by memory
    /// observability — tasks accumulate here unboundedly until explicit
    /// retention cleanup runs.
    pub async fn task_count(&self) -> u64 {
        self.tasks.read().await.len() as u64
    }

    /// Number of tasks currently assigned to an agent.
    pub async fn active_assignment_count(&self) -> u64 {
        self.active_assignments.read().await.len() as u64
    }

    pub async fn assign_task_to(&self, task_id: &TaskId, agent_id: AgentId) -> Result<()> {
        let mut tasks = self.tasks.write().await;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| eyre::eyre!("Task {} not found", task_id))?;

        task.assign(agent_id.clone());
        self.active_assignments
            .write()
            .await
            .insert(agent_id.clone(), task_id.clone());

        info!(task_id = %task_id, agent = %agent_id, "Task assigned");
        Ok(())
    }

    pub async fn start_task(&self, task_id: &TaskId) -> Result<()> {
        let mut tasks = self.tasks.write().await;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| eyre::eyre!("Task {} not found", task_id))?;
        task.start();
        info!(task_id = %task_id, "Task started");
        Ok(())
    }

    pub async fn submit_for_review(&self, task_id: &TaskId) -> Result<()> {
        let mut tasks = self.tasks.write().await;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| eyre::eyre!("Task {} not found", task_id))?;
        task.submit_for_review();
        info!(task_id = %task_id, "Task submitted for review");
        Ok(())
    }

    pub async fn complete_task(&self, task_id: &TaskId, result: TaskResult) -> Result<()> {
        let mut tasks = self.tasks.write().await;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| eyre::eyre!("Task {} not found", task_id))?;

        let agent_id = task.assignee.clone();
        task.complete(result);

        if let Some(agent) = agent_id {
            self.active_assignments.write().await.remove(&agent);
        }
        info!(task_id = %task_id, "Task completed");
        Ok(())
    }

    pub async fn fail_task(&self, task_id: &TaskId, reason: &str) -> Result<()> {
        let mut tasks = self.tasks.write().await;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| eyre::eyre!("Task {} not found", task_id))?;

        let agent_id = task.assignee.clone();
        task.fail(reason);

        if let Some(agent) = agent_id {
            self.active_assignments.write().await.remove(&agent);
        }
        warn!(task_id = %task_id, reason = reason, "Task failed");
        Ok(())
    }

    pub async fn cancel_task(&self, task_id: &TaskId) -> Result<()> {
        let mut tasks = self.tasks.write().await;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| eyre::eyre!("Task {} not found", task_id))?;

        let agent_id = task.assignee.clone();
        task.cancel();

        if let Some(agent) = agent_id {
            self.active_assignments.write().await.remove(&agent);
        }
        info!(task_id = %task_id, "Task cancelled");
        Ok(())
    }

    pub async fn decompose_task(
        &self,
        parent_id: &TaskId,
        subtask_specs: Vec<(String, String)>,
    ) -> Result<Vec<TaskId>> {
        let mut tasks = self.tasks.write().await;
        let parent = tasks
            .get_mut(parent_id)
            .ok_or_else(|| eyre::eyre!("Parent task {} not found", parent_id))?;

        let mut subtask_ids = Vec::new();
        let mut subtasks_to_insert = Vec::new();

        for (title, description) in subtask_specs {
            let subtask = parent.create_subtask(title, description);
            subtask_ids.push(subtask.id.clone());
            subtasks_to_insert.push(subtask);
        }

        for subtask in subtasks_to_insert {
            tasks.insert(subtask.id.clone(), subtask);
        }

        info!(parent = %parent_id, subtasks = subtask_ids.len(), "Task decomposed");
        Ok(subtask_ids)
    }

    pub async fn all_subtasks_complete(&self, parent_id: &TaskId) -> Result<bool> {
        let tasks = self.tasks.read().await;
        let parent = tasks
            .get(parent_id)
            .ok_or_else(|| eyre::eyre!("Task {} not found", parent_id))?;

        let all_complete = parent.subtasks.iter().all(|sub_id| {
            tasks
                .get(sub_id)
                .map(|t| t.status == TaskStatus::Complete)
                .unwrap_or(false)
        });
        Ok(all_complete)
    }

    pub async fn agent_task(&self, agent_id: &AgentId) -> Option<TaskId> {
        self.active_assignments.read().await.get(agent_id).cloned()
    }

    pub async fn active_tasks(&self) -> Vec<Task> {
        self.tasks
            .read()
            .await
            .values()
            .filter(|t| t.status.is_active())
            .cloned()
            .collect()
    }

    pub async fn stats(&self) -> OrchestratorStats {
        let tasks = self.tasks.read().await;
        let assignments = self.active_assignments.read().await;

        OrchestratorStats {
            total_tasks: tasks.len(),
            active_tasks: tasks.values().filter(|t| t.status.is_active()).count(),
            completed_tasks: tasks
                .values()
                .filter(|t| t.status == TaskStatus::Complete)
                .count(),
            failed_tasks: tasks
                .values()
                .filter(|t| t.status == TaskStatus::Failed)
                .count(),
            busy_agents: assignments.len(),
        }
    }
}

#[derive(Debug)]
pub struct OrchestratorStats {
    pub total_tasks: usize,
    pub active_tasks: usize,
    pub completed_tasks: usize,
    pub failed_tasks: usize,
    pub busy_agents: usize,
}

impl Default for Orchestrator {
    fn default() -> Self {
        Self {
            tasks: RwLock::new(HashMap::new()),
            active_assignments: RwLock::new(HashMap::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn submit_and_complete_task() {
        let orch = Orchestrator::new();
        let task = Task::new("Test task", "Do something");
        let id = orch.submit_task(task).await;

        let agent = AgentId::new("agent-1");
        orch.assign_task_to(&id, agent.clone()).await.unwrap();
        orch.start_task(&id).await.unwrap();
        orch.complete_task(
            &id,
            TaskResult {
                success: true,
                summary: "done".into(),
                output: serde_json::Value::Null,
                total_tokens: 0,
            },
        )
        .await
        .unwrap();

        let t = orch.get_task(&id).await.unwrap();
        assert_eq!(t.status, TaskStatus::Complete);
        assert!(orch.agent_task(&agent).await.is_none());
    }

    #[tokio::test]
    async fn decompose_task() {
        let orch = Orchestrator::new();
        let task = Task::new("Parent", "Big task");
        let parent_id = orch.submit_task(task).await;

        let subs = orch
            .decompose_task(
                &parent_id,
                vec![
                    ("Sub 1".into(), "First subtask".into()),
                    ("Sub 2".into(), "Second subtask".into()),
                ],
            )
            .await
            .unwrap();

        assert_eq!(subs.len(), 2);
        assert!(!orch.all_subtasks_complete(&parent_id).await.unwrap());
    }
}
