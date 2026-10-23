//! Ticket tracker — per-ticket state machine for the dev team workflow.
//!
//! Tracks tickets through: Queued → Assigned → Implementing → PrOpen →
//! InReview → Approved → Merged → Done.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use eyre::Result;
use serde::{Deserialize, Serialize};
use workforce_core::types::AgentId;

/// Tracks the lifecycle of multiple tickets through the dev workflow.
pub struct TicketTracker {
    tickets: HashMap<String, TicketState>,
}

/// The state of a single ticket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TicketState {
    pub ticket_id: String,
    pub title: String,
    pub assignee: Option<AgentId>,
    pub status: TicketStatus,
    pub branch: Option<String>,
    pub pr_number: Option<u64>,
    pub review_status: Option<ReviewStatus>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Ticket lifecycle status.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketStatus {
    Queued,
    Assigned,
    Implementing,
    PrOpen,
    InReview,
    Approved,
    Merged,
    Done,
}

impl std::fmt::Display for TicketStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Queued => write!(f, "queued"),
            Self::Assigned => write!(f, "assigned"),
            Self::Implementing => write!(f, "implementing"),
            Self::PrOpen => write!(f, "pr_open"),
            Self::InReview => write!(f, "in_review"),
            Self::Approved => write!(f, "approved"),
            Self::Merged => write!(f, "merged"),
            Self::Done => write!(f, "done"),
        }
    }
}

/// Code review status for a PR.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    Pending,
    ChangesRequested,
    Approved,
}

impl std::fmt::Display for ReviewStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::ChangesRequested => write!(f, "changes_requested"),
            Self::Approved => write!(f, "approved"),
        }
    }
}

/// Summary of ticket counts by status.
#[derive(Debug, Default)]
pub struct TrackerSummary {
    pub queued: usize,
    pub assigned: usize,
    pub implementing: usize,
    pub pr_open: usize,
    pub in_review: usize,
    pub approved: usize,
    pub merged: usize,
    pub done: usize,
}

impl TicketTracker {
    /// Create a new empty tracker.
    pub fn new() -> Self {
        Self {
            tickets: HashMap::new(),
        }
    }

    /// Add a new ticket to the tracker in Queued status.
    pub fn add_ticket(&mut self, id: &str, title: &str) -> &TicketState {
        let now = Utc::now();
        self.tickets.insert(
            id.to_string(),
            TicketState {
                ticket_id: id.to_string(),
                title: title.to_string(),
                assignee: None,
                status: TicketStatus::Queued,
                branch: None,
                pr_number: None,
                review_status: None,
                created_at: now,
                updated_at: now,
            },
        );
        self.tickets.get(id).unwrap()
    }

    /// Assign a ticket to an agent. Transitions from Queued → Assigned.
    pub fn assign(&mut self, id: &str, assignee: AgentId) -> Result<()> {
        let ticket = self
            .tickets
            .get_mut(id)
            .ok_or_else(|| eyre::eyre!("Ticket '{}' not found", id))?;

        if ticket.status != TicketStatus::Queued {
            return Err(eyre::eyre!(
                "Cannot assign ticket '{}' — status is {} (expected queued)",
                id,
                ticket.status
            ));
        }

        ticket.assignee = Some(assignee);
        ticket.status = TicketStatus::Assigned;
        ticket.updated_at = Utc::now();
        Ok(())
    }

    /// Transition a ticket to a new status.
    ///
    /// Validates that the transition is legal:
    /// - Queued → Assigned (use `assign()` instead)
    /// - Assigned → Implementing
    /// - Implementing → PrOpen
    /// - PrOpen → InReview
    /// - InReview → Approved | Implementing (revision)
    /// - Approved → Merged
    /// - Merged → Done
    pub fn transition(&mut self, id: &str, new_status: TicketStatus) -> Result<()> {
        let ticket = self
            .tickets
            .get_mut(id)
            .ok_or_else(|| eyre::eyre!("Ticket '{}' not found", id))?;

        if !is_valid_transition(&ticket.status, &new_status) {
            return Err(eyre::eyre!(
                "Invalid transition for ticket '{}': {} → {}",
                id,
                ticket.status,
                new_status
            ));
        }

        ticket.status = new_status;
        ticket.updated_at = Utc::now();
        Ok(())
    }

    /// Set the PR details for a ticket.
    pub fn set_pr(&mut self, id: &str, pr_number: u64, branch: &str) -> Result<()> {
        let ticket = self
            .tickets
            .get_mut(id)
            .ok_or_else(|| eyre::eyre!("Ticket '{}' not found", id))?;

        ticket.pr_number = Some(pr_number);
        ticket.branch = Some(branch.to_string());
        ticket.updated_at = Utc::now();
        Ok(())
    }

    /// Set the review status for a ticket.
    pub fn set_review_status(&mut self, id: &str, status: ReviewStatus) -> Result<()> {
        let ticket = self
            .tickets
            .get_mut(id)
            .ok_or_else(|| eyre::eyre!("Ticket '{}' not found", id))?;

        ticket.review_status = Some(status);
        ticket.updated_at = Utc::now();
        Ok(())
    }

    /// Get a ticket by ID.
    pub fn get(&self, id: &str) -> Option<&TicketState> {
        self.tickets.get(id)
    }

    /// List all tickets with a given status.
    pub fn list_by_status(&self, status: &TicketStatus) -> Vec<&TicketState> {
        self.tickets
            .values()
            .filter(|t| t.status == *status)
            .collect()
    }

    /// Count of tickets that are actively being worked on (not queued, not done).
    pub fn active_count(&self) -> usize {
        self.tickets
            .values()
            .filter(|t| {
                !matches!(
                    t.status,
                    TicketStatus::Queued | TicketStatus::Done | TicketStatus::Merged
                )
            })
            .count()
    }

    /// Get a summary of ticket counts by status.
    pub fn summary(&self) -> TrackerSummary {
        let mut s = TrackerSummary::default();
        for ticket in self.tickets.values() {
            match ticket.status {
                TicketStatus::Queued => s.queued += 1,
                TicketStatus::Assigned => s.assigned += 1,
                TicketStatus::Implementing => s.implementing += 1,
                TicketStatus::PrOpen => s.pr_open += 1,
                TicketStatus::InReview => s.in_review += 1,
                TicketStatus::Approved => s.approved += 1,
                TicketStatus::Merged => s.merged += 1,
                TicketStatus::Done => s.done += 1,
            }
        }
        s
    }

    /// Total number of tracked tickets.
    pub fn len(&self) -> usize {
        self.tickets.len()
    }

    /// Whether the tracker has no tickets.
    pub fn is_empty(&self) -> bool {
        self.tickets.is_empty()
    }
}

impl Default for TicketTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Check if a status transition is valid.
fn is_valid_transition(from: &TicketStatus, to: &TicketStatus) -> bool {
    matches!(
        (from, to),
        (TicketStatus::Queued, TicketStatus::Assigned)
            | (TicketStatus::Assigned, TicketStatus::Implementing)
            | (TicketStatus::Implementing, TicketStatus::PrOpen)
            | (TicketStatus::PrOpen, TicketStatus::InReview)
            | (TicketStatus::InReview, TicketStatus::Approved)
            | (TicketStatus::InReview, TicketStatus::Implementing) // revision
            | (TicketStatus::Approved, TicketStatus::Merged)
            | (TicketStatus::Merged, TicketStatus::Done)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_and_get_ticket() {
        let mut tracker = TicketTracker::new();
        tracker.add_ticket("DW-100", "Implement login");

        let ticket = tracker.get("DW-100").unwrap();
        assert_eq!(ticket.title, "Implement login");
        assert_eq!(ticket.status, TicketStatus::Queued);
        assert!(ticket.assignee.is_none());
    }

    #[test]
    fn assign_ticket() {
        let mut tracker = TicketTracker::new();
        tracker.add_ticket("DW-101", "Add tests");

        tracker.assign("DW-101", AgentId::new("dev-1")).unwrap();

        let ticket = tracker.get("DW-101").unwrap();
        assert_eq!(ticket.status, TicketStatus::Assigned);
        assert_eq!(ticket.assignee.as_ref().unwrap(), &AgentId::new("dev-1"));
    }

    #[test]
    fn assign_non_queued_fails() {
        let mut tracker = TicketTracker::new();
        tracker.add_ticket("DW-102", "Fix bug");
        tracker.assign("DW-102", AgentId::new("dev-1")).unwrap();

        // Already assigned, can't re-assign
        assert!(tracker.assign("DW-102", AgentId::new("dev-2")).is_err());
    }

    #[test]
    fn full_lifecycle() {
        let mut tracker = TicketTracker::new();
        tracker.add_ticket("DW-103", "Feature X");

        tracker.assign("DW-103", AgentId::new("dev-1")).unwrap();
        tracker
            .transition("DW-103", TicketStatus::Implementing)
            .unwrap();
        tracker.transition("DW-103", TicketStatus::PrOpen).unwrap();
        tracker.set_pr("DW-103", 42, "feature/x").unwrap();
        tracker
            .transition("DW-103", TicketStatus::InReview)
            .unwrap();
        tracker
            .set_review_status("DW-103", ReviewStatus::Approved)
            .unwrap();
        tracker
            .transition("DW-103", TicketStatus::Approved)
            .unwrap();
        tracker.transition("DW-103", TicketStatus::Merged).unwrap();
        tracker.transition("DW-103", TicketStatus::Done).unwrap();

        let ticket = tracker.get("DW-103").unwrap();
        assert_eq!(ticket.status, TicketStatus::Done);
        assert_eq!(ticket.pr_number, Some(42));
        assert_eq!(ticket.branch.as_deref(), Some("feature/x"));
    }

    #[test]
    fn revision_loop() {
        let mut tracker = TicketTracker::new();
        tracker.add_ticket("DW-104", "Needs revision");
        tracker.assign("DW-104", AgentId::new("dev-1")).unwrap();
        tracker
            .transition("DW-104", TicketStatus::Implementing)
            .unwrap();
        tracker.transition("DW-104", TicketStatus::PrOpen).unwrap();
        tracker
            .transition("DW-104", TicketStatus::InReview)
            .unwrap();

        // Reviewer requests changes → back to implementing
        tracker
            .set_review_status("DW-104", ReviewStatus::ChangesRequested)
            .unwrap();
        tracker
            .transition("DW-104", TicketStatus::Implementing)
            .unwrap();

        assert_eq!(
            tracker.get("DW-104").unwrap().status,
            TicketStatus::Implementing
        );
    }

    #[test]
    fn invalid_transition_fails() {
        let mut tracker = TicketTracker::new();
        tracker.add_ticket("DW-105", "Bad transition");
        tracker.assign("DW-105", AgentId::new("dev-1")).unwrap();

        // Can't jump from Assigned to Merged
        assert!(tracker.transition("DW-105", TicketStatus::Merged).is_err());
    }

    #[test]
    fn list_by_status() {
        let mut tracker = TicketTracker::new();
        tracker.add_ticket("A", "First");
        tracker.add_ticket("B", "Second");
        tracker.add_ticket("C", "Third");
        tracker.assign("B", AgentId::new("dev-1")).unwrap();

        assert_eq!(tracker.list_by_status(&TicketStatus::Queued).len(), 2);
        assert_eq!(tracker.list_by_status(&TicketStatus::Assigned).len(), 1);
        assert_eq!(tracker.list_by_status(&TicketStatus::Implementing).len(), 0);
    }

    #[test]
    fn active_count() {
        let mut tracker = TicketTracker::new();
        tracker.add_ticket("A", "Queued"); // not active
        tracker.add_ticket("B", "Working");
        tracker.assign("B", AgentId::new("dev-1")).unwrap();
        tracker.transition("B", TicketStatus::Implementing).unwrap();

        assert_eq!(tracker.active_count(), 1);
    }

    #[test]
    fn summary_counts() {
        let mut tracker = TicketTracker::new();
        tracker.add_ticket("A", "One");
        tracker.add_ticket("B", "Two");
        tracker.add_ticket("C", "Three");
        tracker.assign("B", AgentId::new("dev")).unwrap();

        let s = tracker.summary();
        assert_eq!(s.queued, 2);
        assert_eq!(s.assigned, 1);
        assert_eq!(s.implementing, 0);
    }

    #[test]
    fn nonexistent_ticket_fails() {
        let mut tracker = TicketTracker::new();
        assert!(tracker.assign("nope", AgentId::new("dev")).is_err());
        assert!(tracker.transition("nope", TicketStatus::Done).is_err());
    }

    #[test]
    fn status_display() {
        assert_eq!(TicketStatus::Queued.to_string(), "queued");
        assert_eq!(TicketStatus::InReview.to_string(), "in_review");
        assert_eq!(TicketStatus::Done.to_string(), "done");
    }

    #[test]
    fn review_status_display() {
        assert_eq!(ReviewStatus::Pending.to_string(), "pending");
        assert_eq!(
            ReviewStatus::ChangesRequested.to_string(),
            "changes_requested"
        );
        assert_eq!(ReviewStatus::Approved.to_string(), "approved");
    }

    #[test]
    fn len_and_is_empty() {
        let mut tracker = TicketTracker::new();
        assert!(tracker.is_empty());
        assert_eq!(tracker.len(), 0);

        tracker.add_ticket("A", "One");
        assert!(!tracker.is_empty());
        assert_eq!(tracker.len(), 1);
    }
}
