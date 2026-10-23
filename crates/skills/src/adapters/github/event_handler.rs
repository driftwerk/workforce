//! GitHub webhook event handler.
//!
//! Parses raw GitHub webhook payloads into typed actions for the orchestrator.
//! Decoupled from the agent service: the handler only decides *what* should
//! happen, not *how* to execute it.

use serde::{Deserialize, Serialize};

/// Actions derived from GitHub webhook events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GitHubAction {
    PullRequestOpened {
        repo: String,
        number: u64,
        title: String,
        author: String,
        head_branch: String,
        base_branch: String,
        body: String,
        url: String,
    },
    PullRequestMerged {
        repo: String,
        number: u64,
        title: String,
        author: String,
        merge_commit: String,
    },
    PullRequestClosed {
        repo: String,
        number: u64,
        title: String,
    },
    PullRequestReviewSubmitted {
        repo: String,
        number: u64,
        reviewer: String,
        state: String,
        body: String,
    },
    PushReceived {
        repo: String,
        branch: String,
        commits: usize,
        pusher: String,
        head_message: String,
    },
    IssueOpened {
        repo: String,
        number: u64,
        title: String,
        author: String,
        body: String,
    },
    IssueClosed {
        repo: String,
        number: u64,
        title: String,
    },
    IssueCommented {
        repo: String,
        number: u64,
        author: String,
        body: String,
    },
    CheckRunCompleted {
        repo: String,
        name: String,
        conclusion: String,
        branch: String,
        url: String,
    },
    WorkflowRunCompleted {
        repo: String,
        workflow_name: String,
        conclusion: String,
        branch: String,
        url: String,
    },
    /// Unrecognized or filtered event.
    Ignore,
}

pub struct GitHubEventHandler;

impl GitHubEventHandler {
    /// Parse a GitHub webhook event into a typed action.
    ///
    /// `event_type` comes from the `X-GitHub-Event` header.
    pub fn handle(event_type: &str, payload: &serde_json::Value) -> GitHubAction {
        let action = payload["action"].as_str().unwrap_or("");
        let repo = payload["repository"]["full_name"]
            .as_str()
            .unwrap_or("")
            .to_string();

        match event_type {
            "pull_request" => Self::handle_pull_request(action, &repo, payload),
            "pull_request_review" => Self::handle_pr_review(action, &repo, payload),
            "push" => Self::handle_push(&repo, payload),
            "issues" => Self::handle_issues(action, &repo, payload),
            "issue_comment" => Self::handle_issue_comment(action, &repo, payload),
            "check_run" => Self::handle_check_run(action, &repo, payload),
            "workflow_run" => Self::handle_workflow_run(action, &repo, payload),
            _ => GitHubAction::Ignore,
        }
    }

    fn handle_pull_request(action: &str, repo: &str, payload: &serde_json::Value) -> GitHubAction {
        let pr = &payload["pull_request"];
        let number = pr["number"].as_u64().unwrap_or(0);
        let title = pr["title"].as_str().unwrap_or("").to_string();
        let author = pr["user"]["login"].as_str().unwrap_or("").to_string();

        match action {
            "opened" | "reopened" => GitHubAction::PullRequestOpened {
                repo: repo.to_string(),
                number,
                title,
                author,
                head_branch: pr["head"]["ref"].as_str().unwrap_or("").to_string(),
                base_branch: pr["base"]["ref"].as_str().unwrap_or("").to_string(),
                body: pr["body"].as_str().unwrap_or("").to_string(),
                url: pr["html_url"].as_str().unwrap_or("").to_string(),
            },
            "closed" => {
                let merged = pr["merged"].as_bool().unwrap_or(false);
                if merged {
                    GitHubAction::PullRequestMerged {
                        repo: repo.to_string(),
                        number,
                        title,
                        author,
                        merge_commit: pr["merge_commit_sha"].as_str().unwrap_or("").to_string(),
                    }
                } else {
                    GitHubAction::PullRequestClosed {
                        repo: repo.to_string(),
                        number,
                        title,
                    }
                }
            }
            _ => GitHubAction::Ignore,
        }
    }

    fn handle_pr_review(action: &str, repo: &str, payload: &serde_json::Value) -> GitHubAction {
        if action != "submitted" {
            return GitHubAction::Ignore;
        }

        let review = &payload["review"];
        let pr_number = payload["pull_request"]["number"].as_u64().unwrap_or(0);
        let reviewer = review["user"]["login"].as_str().unwrap_or("").to_string();
        let state = review["state"].as_str().unwrap_or("").to_string();
        let body = review["body"].as_str().unwrap_or("").to_string();

        GitHubAction::PullRequestReviewSubmitted {
            repo: repo.to_string(),
            number: pr_number,
            reviewer,
            state,
            body,
        }
    }

    fn handle_push(repo: &str, payload: &serde_json::Value) -> GitHubAction {
        let git_ref = payload["ref"].as_str().unwrap_or("");
        // Extract branch name from refs/heads/<branch>
        let branch = git_ref.strip_prefix("refs/heads/").unwrap_or(git_ref);
        let commits = payload["commits"].as_array().map(|c| c.len()).unwrap_or(0);
        let pusher = payload["pusher"]["name"]
            .as_str()
            .unwrap_or_else(|| payload["sender"]["login"].as_str().unwrap_or(""))
            .to_string();
        let head_message = payload["head_commit"]["message"]
            .as_str()
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("")
            .to_string();

        GitHubAction::PushReceived {
            repo: repo.to_string(),
            branch: branch.to_string(),
            commits,
            pusher,
            head_message,
        }
    }

    fn handle_issues(action: &str, repo: &str, payload: &serde_json::Value) -> GitHubAction {
        let issue = &payload["issue"];
        let number = issue["number"].as_u64().unwrap_or(0);
        let title = issue["title"].as_str().unwrap_or("").to_string();

        match action {
            "opened" => {
                let author = issue["user"]["login"].as_str().unwrap_or("").to_string();
                let body = issue["body"].as_str().unwrap_or("").to_string();
                GitHubAction::IssueOpened {
                    repo: repo.to_string(),
                    number,
                    title,
                    author,
                    body,
                }
            }
            "closed" => GitHubAction::IssueClosed {
                repo: repo.to_string(),
                number,
                title,
            },
            _ => GitHubAction::Ignore,
        }
    }

    fn handle_issue_comment(action: &str, repo: &str, payload: &serde_json::Value) -> GitHubAction {
        if action != "created" {
            return GitHubAction::Ignore;
        }

        let number = payload["issue"]["number"].as_u64().unwrap_or(0);
        let author = payload["comment"]["user"]["login"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let body = payload["comment"]["body"]
            .as_str()
            .unwrap_or("")
            .to_string();

        GitHubAction::IssueCommented {
            repo: repo.to_string(),
            number,
            author,
            body,
        }
    }

    fn handle_check_run(action: &str, repo: &str, payload: &serde_json::Value) -> GitHubAction {
        if action != "completed" {
            return GitHubAction::Ignore;
        }

        let check = &payload["check_run"];
        let name = check["name"].as_str().unwrap_or("").to_string();
        let conclusion = check["conclusion"].as_str().unwrap_or("").to_string();
        let branch = check["check_suite"]["head_branch"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let url = check["html_url"].as_str().unwrap_or("").to_string();

        GitHubAction::CheckRunCompleted {
            repo: repo.to_string(),
            name,
            conclusion,
            branch,
            url,
        }
    }

    fn handle_workflow_run(action: &str, repo: &str, payload: &serde_json::Value) -> GitHubAction {
        if action != "completed" {
            return GitHubAction::Ignore;
        }

        let run = &payload["workflow_run"];
        let workflow_name = run["name"].as_str().unwrap_or("").to_string();
        let conclusion = run["conclusion"].as_str().unwrap_or("").to_string();
        let branch = run["head_branch"].as_str().unwrap_or("").to_string();
        let url = run["html_url"].as_str().unwrap_or("").to_string();

        GitHubAction::WorkflowRunCompleted {
            repo: repo.to_string(),
            workflow_name,
            conclusion,
            branch,
            url,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Pull request events ─────────────────────────────────────────

    #[test]
    fn pr_opened() {
        let payload = serde_json::json!({
            "action": "opened",
            "pull_request": {
                "number": 42,
                "title": "Add feature X",
                "user": { "login": "alice" },
                "head": { "ref": "feature-x" },
                "base": { "ref": "main" },
                "body": "This adds feature X.",
                "html_url": "https://github.com/acme/repo/pull/42"
            },
            "repository": { "full_name": "acme/repo" }
        });

        match GitHubEventHandler::handle("pull_request", &payload) {
            GitHubAction::PullRequestOpened {
                repo,
                number,
                title,
                author,
                head_branch,
                base_branch,
                body,
                url,
            } => {
                assert_eq!(repo, "acme/repo");
                assert_eq!(number, 42);
                assert_eq!(title, "Add feature X");
                assert_eq!(author, "alice");
                assert_eq!(head_branch, "feature-x");
                assert_eq!(base_branch, "main");
                assert_eq!(body, "This adds feature X.");
                assert_eq!(url, "https://github.com/acme/repo/pull/42");
            }
            other => panic!("Expected PullRequestOpened, got {:?}", other),
        }
    }

    #[test]
    fn pr_reopened() {
        let payload = serde_json::json!({
            "action": "reopened",
            "pull_request": {
                "number": 7,
                "title": "Reopened PR",
                "user": { "login": "bob" },
                "head": { "ref": "fix-bug" },
                "base": { "ref": "main" },
                "body": "",
                "html_url": "https://github.com/acme/repo/pull/7"
            },
            "repository": { "full_name": "acme/repo" }
        });

        assert!(matches!(
            GitHubEventHandler::handle("pull_request", &payload),
            GitHubAction::PullRequestOpened { number: 7, .. }
        ));
    }

    #[test]
    fn pr_merged() {
        let payload = serde_json::json!({
            "action": "closed",
            "pull_request": {
                "number": 10,
                "title": "Merged PR",
                "user": { "login": "carol" },
                "merged": true,
                "merge_commit_sha": "abc123def456"
            },
            "repository": { "full_name": "acme/repo" }
        });

        match GitHubEventHandler::handle("pull_request", &payload) {
            GitHubAction::PullRequestMerged {
                repo,
                number,
                title,
                author,
                merge_commit,
            } => {
                assert_eq!(repo, "acme/repo");
                assert_eq!(number, 10);
                assert_eq!(title, "Merged PR");
                assert_eq!(author, "carol");
                assert_eq!(merge_commit, "abc123def456");
            }
            other => panic!("Expected PullRequestMerged, got {:?}", other),
        }
    }

    #[test]
    fn pr_closed_without_merge() {
        let payload = serde_json::json!({
            "action": "closed",
            "pull_request": {
                "number": 11,
                "title": "Closed PR",
                "user": { "login": "dave" },
                "merged": false
            },
            "repository": { "full_name": "acme/repo" }
        });

        match GitHubEventHandler::handle("pull_request", &payload) {
            GitHubAction::PullRequestClosed {
                repo,
                number,
                title,
            } => {
                assert_eq!(repo, "acme/repo");
                assert_eq!(number, 11);
                assert_eq!(title, "Closed PR");
            }
            other => panic!("Expected PullRequestClosed, got {:?}", other),
        }
    }

    #[test]
    fn pr_unknown_action_ignored() {
        let payload = serde_json::json!({
            "action": "labeled",
            "pull_request": { "number": 1, "title": "t", "user": { "login": "u" } },
            "repository": { "full_name": "acme/repo" }
        });

        assert!(matches!(
            GitHubEventHandler::handle("pull_request", &payload),
            GitHubAction::Ignore
        ));
    }

    // ── Pull request review events ──────────────────────────────────

    #[test]
    fn pr_review_submitted() {
        let payload = serde_json::json!({
            "action": "submitted",
            "review": {
                "user": { "login": "reviewer1" },
                "state": "approved",
                "body": "LGTM!"
            },
            "pull_request": { "number": 42 },
            "repository": { "full_name": "acme/repo" }
        });

        match GitHubEventHandler::handle("pull_request_review", &payload) {
            GitHubAction::PullRequestReviewSubmitted {
                repo,
                number,
                reviewer,
                state,
                body,
            } => {
                assert_eq!(repo, "acme/repo");
                assert_eq!(number, 42);
                assert_eq!(reviewer, "reviewer1");
                assert_eq!(state, "approved");
                assert_eq!(body, "LGTM!");
            }
            other => panic!("Expected PullRequestReviewSubmitted, got {:?}", other),
        }
    }

    #[test]
    fn pr_review_non_submitted_ignored() {
        let payload = serde_json::json!({
            "action": "dismissed",
            "review": { "user": { "login": "r" }, "state": "dismissed", "body": "" },
            "pull_request": { "number": 1 },
            "repository": { "full_name": "acme/repo" }
        });

        assert!(matches!(
            GitHubEventHandler::handle("pull_request_review", &payload),
            GitHubAction::Ignore
        ));
    }

    // ── Push events ─────────────────────────────────────────────────

    #[test]
    fn push_event() {
        let payload = serde_json::json!({
            "ref": "refs/heads/main",
            "commits": [{"id": "aaa"}, {"id": "bbb"}, {"id": "ccc"}],
            "pusher": { "name": "deployer" },
            "head_commit": { "message": "Deploy v2.0\n\nRelease notes here" },
            "repository": { "full_name": "acme/repo" },
            "sender": { "login": "deployer" }
        });

        match GitHubEventHandler::handle("push", &payload) {
            GitHubAction::PushReceived {
                repo,
                branch,
                commits,
                pusher,
                head_message,
            } => {
                assert_eq!(repo, "acme/repo");
                assert_eq!(branch, "main");
                assert_eq!(commits, 3);
                assert_eq!(pusher, "deployer");
                assert_eq!(head_message, "Deploy v2.0");
            }
            other => panic!("Expected PushReceived, got {:?}", other),
        }
    }

    #[test]
    fn push_event_tag_ref() {
        let payload = serde_json::json!({
            "ref": "refs/tags/v1.0",
            "commits": [],
            "pusher": { "name": "tagger" },
            "head_commit": { "message": "Tag v1.0" },
            "repository": { "full_name": "acme/repo" }
        });

        match GitHubEventHandler::handle("push", &payload) {
            GitHubAction::PushReceived { branch, .. } => {
                // Tags don't strip "refs/heads/", so full ref kept
                assert_eq!(branch, "refs/tags/v1.0");
            }
            other => panic!("Expected PushReceived, got {:?}", other),
        }
    }

    // ── Issue events ────────────────────────────────────────────────

    #[test]
    fn issue_opened() {
        let payload = serde_json::json!({
            "action": "opened",
            "issue": {
                "number": 99,
                "title": "Bug report",
                "user": { "login": "reporter" },
                "body": "Something is broken."
            },
            "repository": { "full_name": "acme/repo" }
        });

        match GitHubEventHandler::handle("issues", &payload) {
            GitHubAction::IssueOpened {
                repo,
                number,
                title,
                author,
                body,
            } => {
                assert_eq!(repo, "acme/repo");
                assert_eq!(number, 99);
                assert_eq!(title, "Bug report");
                assert_eq!(author, "reporter");
                assert_eq!(body, "Something is broken.");
            }
            other => panic!("Expected IssueOpened, got {:?}", other),
        }
    }

    #[test]
    fn issue_closed() {
        let payload = serde_json::json!({
            "action": "closed",
            "issue": {
                "number": 99,
                "title": "Bug report"
            },
            "repository": { "full_name": "acme/repo" }
        });

        match GitHubEventHandler::handle("issues", &payload) {
            GitHubAction::IssueClosed {
                repo,
                number,
                title,
            } => {
                assert_eq!(repo, "acme/repo");
                assert_eq!(number, 99);
                assert_eq!(title, "Bug report");
            }
            other => panic!("Expected IssueClosed, got {:?}", other),
        }
    }

    #[test]
    fn issue_unknown_action_ignored() {
        let payload = serde_json::json!({
            "action": "labeled",
            "issue": { "number": 1, "title": "t" },
            "repository": { "full_name": "acme/repo" }
        });

        assert!(matches!(
            GitHubEventHandler::handle("issues", &payload),
            GitHubAction::Ignore
        ));
    }

    // ── Issue comment events ────────────────────────────────────────

    #[test]
    fn issue_comment_created() {
        let payload = serde_json::json!({
            "action": "created",
            "issue": { "number": 42 },
            "comment": {
                "user": { "login": "commenter" },
                "body": "I can reproduce this."
            },
            "repository": { "full_name": "acme/repo" }
        });

        match GitHubEventHandler::handle("issue_comment", &payload) {
            GitHubAction::IssueCommented {
                repo,
                number,
                author,
                body,
            } => {
                assert_eq!(repo, "acme/repo");
                assert_eq!(number, 42);
                assert_eq!(author, "commenter");
                assert_eq!(body, "I can reproduce this.");
            }
            other => panic!("Expected IssueCommented, got {:?}", other),
        }
    }

    #[test]
    fn issue_comment_edited_ignored() {
        let payload = serde_json::json!({
            "action": "edited",
            "issue": { "number": 42 },
            "comment": { "user": { "login": "c" }, "body": "edited" },
            "repository": { "full_name": "acme/repo" }
        });

        assert!(matches!(
            GitHubEventHandler::handle("issue_comment", &payload),
            GitHubAction::Ignore
        ));
    }

    // ── Check run events ────────────────────────────────────────────

    #[test]
    fn check_run_completed() {
        let payload = serde_json::json!({
            "action": "completed",
            "check_run": {
                "name": "CI / Build",
                "conclusion": "success",
                "check_suite": { "head_branch": "main" },
                "html_url": "https://github.com/acme/repo/runs/123"
            },
            "repository": { "full_name": "acme/repo" }
        });

        match GitHubEventHandler::handle("check_run", &payload) {
            GitHubAction::CheckRunCompleted {
                repo,
                name,
                conclusion,
                branch,
                url,
            } => {
                assert_eq!(repo, "acme/repo");
                assert_eq!(name, "CI / Build");
                assert_eq!(conclusion, "success");
                assert_eq!(branch, "main");
                assert_eq!(url, "https://github.com/acme/repo/runs/123");
            }
            other => panic!("Expected CheckRunCompleted, got {:?}", other),
        }
    }

    #[test]
    fn check_run_non_completed_ignored() {
        let payload = serde_json::json!({
            "action": "created",
            "check_run": { "name": "CI", "conclusion": null },
            "repository": { "full_name": "acme/repo" }
        });

        assert!(matches!(
            GitHubEventHandler::handle("check_run", &payload),
            GitHubAction::Ignore
        ));
    }

    // ── Workflow run events ─────────────────────────────────────────

    #[test]
    fn workflow_run_completed() {
        let payload = serde_json::json!({
            "action": "completed",
            "workflow_run": {
                "name": "Deploy Production",
                "conclusion": "failure",
                "head_branch": "release/v2",
                "html_url": "https://github.com/acme/repo/actions/runs/456"
            },
            "repository": { "full_name": "acme/repo" }
        });

        match GitHubEventHandler::handle("workflow_run", &payload) {
            GitHubAction::WorkflowRunCompleted {
                repo,
                workflow_name,
                conclusion,
                branch,
                url,
            } => {
                assert_eq!(repo, "acme/repo");
                assert_eq!(workflow_name, "Deploy Production");
                assert_eq!(conclusion, "failure");
                assert_eq!(branch, "release/v2");
                assert_eq!(url, "https://github.com/acme/repo/actions/runs/456");
            }
            other => panic!("Expected WorkflowRunCompleted, got {:?}", other),
        }
    }

    #[test]
    fn workflow_run_non_completed_ignored() {
        let payload = serde_json::json!({
            "action": "requested",
            "workflow_run": { "name": "CI", "conclusion": null },
            "repository": { "full_name": "acme/repo" }
        });

        assert!(matches!(
            GitHubEventHandler::handle("workflow_run", &payload),
            GitHubAction::Ignore
        ));
    }

    // ── Unknown event type ──────────────────────────────────────────

    #[test]
    fn unknown_event_type_ignored() {
        let payload = serde_json::json!({
            "action": "created",
            "repository": { "full_name": "acme/repo" }
        });

        assert!(matches!(
            GitHubEventHandler::handle("star", &payload),
            GitHubAction::Ignore
        ));
    }

    // ── Missing fields handled gracefully ───────────────────────────

    #[test]
    fn pr_opened_with_missing_fields() {
        let payload = serde_json::json!({
            "action": "opened",
            "pull_request": { "number": 1 },
            "repository": {}
        });

        match GitHubEventHandler::handle("pull_request", &payload) {
            GitHubAction::PullRequestOpened {
                repo,
                number,
                title,
                author,
                ..
            } => {
                assert_eq!(repo, "");
                assert_eq!(number, 1);
                assert_eq!(title, "");
                assert_eq!(author, "");
            }
            other => panic!("Expected PullRequestOpened, got {:?}", other),
        }
    }

    #[test]
    fn push_with_no_commits() {
        let payload = serde_json::json!({
            "ref": "refs/heads/empty",
            "repository": { "full_name": "acme/repo" },
            "sender": { "login": "bot" }
        });

        match GitHubEventHandler::handle("push", &payload) {
            GitHubAction::PushReceived { commits, .. } => {
                assert_eq!(commits, 0);
            }
            other => panic!("Expected PushReceived, got {:?}", other),
        }
    }

    // ── Serialization round-trip ────────────────────────────────────

    #[test]
    fn action_serialization_round_trip() {
        let action = GitHubAction::PullRequestOpened {
            repo: "acme/repo".to_string(),
            number: 42,
            title: "Test".to_string(),
            author: "alice".to_string(),
            head_branch: "feature".to_string(),
            base_branch: "main".to_string(),
            body: "desc".to_string(),
            url: "https://github.com/acme/repo/pull/42".to_string(),
        };

        let json = serde_json::to_string(&action).unwrap();
        let parsed: GitHubAction = serde_json::from_str(&json).unwrap();
        match parsed {
            GitHubAction::PullRequestOpened { repo, number, .. } => {
                assert_eq!(repo, "acme/repo");
                assert_eq!(number, 42);
            }
            other => panic!("Expected PullRequestOpened, got {:?}", other),
        }
    }
}
