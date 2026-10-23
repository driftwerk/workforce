//! Linear API response types.

use serde::{Deserialize, Serialize};

/// Linear issue representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearIssue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub priority: Option<i32>,
    #[serde(default)]
    pub priority_label: Option<String>,
    #[serde(default)]
    pub state: Option<LinearWorkflowState>,
    #[serde(default)]
    pub assignee: Option<LinearUser>,
    #[serde(default)]
    pub team: Option<LinearTeam>,
    #[serde(default)]
    pub project: Option<LinearProject>,
    #[serde(default)]
    pub labels: Option<LinearLabelConnection>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

/// Linear comment representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearComment {
    pub id: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub user: Option<LinearUser>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// Linear project status representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearProjectStatus {
    pub id: String,
    pub name: String,
}

/// Linear project representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearProject {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub slug_id: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub status: Option<LinearProjectStatus>,
    #[serde(default)]
    pub lead: Option<LinearUser>,
    #[serde(default)]
    pub start_date: Option<String>,
    #[serde(default)]
    pub target_date: Option<String>,
    #[serde(default)]
    pub progress: Option<f64>,
    #[serde(default)]
    pub completed_at: Option<String>,
    #[serde(default)]
    pub canceled_at: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// Linear team representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearTeam {
    pub id: String,
    pub name: String,
    pub key: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// Linear workflow state representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearWorkflowState {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
}

/// Linear user representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearUser {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub active: Option<bool>,
}

/// Linear label representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearLabel {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub color: Option<String>,
}

/// Connection wrapper for labels (Linear uses Relay-style pagination).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinearLabelConnection {
    pub nodes: Vec<LinearLabel>,
}

/// Connection wrapper for projects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinearProjectConnection {
    pub nodes: Vec<LinearProject>,
}

/// Connection wrapper for users.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinearUserConnection {
    pub nodes: Vec<LinearUser>,
}

/// Connection wrapper for issues.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinearIssueConnection {
    pub nodes: Vec<LinearIssue>,
}

/// Connection wrapper for comments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinearCommentConnection {
    pub nodes: Vec<LinearComment>,
}

/// Connection wrapper for teams.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinearTeamConnection {
    pub nodes: Vec<LinearTeam>,
}

/// Connection wrapper for workflow states.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinearWorkflowStateConnection {
    pub nodes: Vec<LinearWorkflowState>,
}

/// Payload returned from issue mutations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearIssuePayload {
    pub success: bool,
    #[serde(default)]
    pub issue: Option<LinearIssue>,
}

/// Payload returned from comment mutations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearCommentPayload {
    pub success: bool,
    #[serde(default)]
    pub comment: Option<LinearComment>,
}

/// A lightweight issue reference (used in relations and bulk results).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearIssueRef {
    pub id: String,
    #[serde(default)]
    pub identifier: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
}

/// An issue relation (blocks, related, duplicate).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearIssueRelation {
    pub id: String,
    #[serde(rename = "type")]
    pub relation_type: String,
    pub issue: LinearIssueRef,
    pub related_issue: LinearIssueRef,
}

/// Linear document representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearDocument {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub project: Option<LinearProject>,
    #[serde(default)]
    pub creator: Option<LinearUser>,
    #[serde(default)]
    pub updated_by: Option<LinearUser>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

/// Connection wrapper for documents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinearDocumentConnection {
    pub nodes: Vec<LinearDocument>,
}

/// Payload returned from document mutations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearDocumentPayload {
    pub success: bool,
    #[serde(default)]
    pub document: Option<LinearDocument>,
}

/// Payload returned from document delete mutation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearDocumentDeletePayload {
    pub success: bool,
}
