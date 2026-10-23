//! code.git - Git operations (status, commit, push, pull, log, branch).

use async_trait::async_trait;
use eyre::{eyre, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Instant;
use workforce_agent::tools::exec_runner::{ExecOutcome, RunnerEndpoint};
use workforce_agent::tools::{Tool, ToolResult};

use super::utils::{run_command, CODE_TOOL_TIMEOUT_SECS};

pub struct CodeGitTool {
    allowed_paths: Vec<PathBuf>,
    /// When set, git commands dispatch to the privilege-separated runner
    /// daemon (audit F7); `None` runs them in-process (dev/local).
    runner: Option<RunnerEndpoint>,
}

/// Check a git command's outcome, returning an error if the command failed.
fn check_git_output(outcome: ExecOutcome) -> Result<String> {
    if !outcome.success() {
        return Err(eyre!("Git command failed: {}", outcome.stderr));
    }
    // Some git commands (e.g. checkout) write to stderr even on success
    if outcome.stdout.is_empty() && !outcome.stderr.is_empty() {
        Ok(outcome.stderr)
    } else {
        Ok(outcome.stdout)
    }
}

impl CodeGitTool {
    pub fn new(allowed_paths: Vec<PathBuf>) -> Self {
        Self {
            allowed_paths,
            runner: None,
        }
    }

    pub fn with_runner(mut self, runner: Option<RunnerEndpoint>) -> Self {
        self.runner = runner;
        self
    }

    /// Spawn `git` (or another program) with `args` in `workspace` through the
    /// executor (dispatched when a runner socket is configured, else in-process).
    async fn git(&self, args: &[&str], workspace: &Path) -> Result<ExecOutcome> {
        run_command(
            self.runner.as_ref(),
            "git",
            args,
            workspace,
            CODE_TOOL_TIMEOUT_SECS,
        )
        .await
    }

    /// Detect the current branch name, falling back to "main" on error.
    async fn current_branch_or_main(&self, workspace: &Path) -> String {
        self.git(["rev-parse", "--abbrev-ref", "HEAD"].as_slice(), workspace)
            .await
            .ok()
            .and_then(|o| o.success().then(|| o.stdout.trim().to_string()))
            .unwrap_or_else(|| "main".to_string())
    }

    async fn run_git(
        &self,
        workspace: &Path,
        action: &str,
        params: &GitParams,
    ) -> Result<GitOutput> {
        if !workspace.join(".git").exists() {
            return Err(eyre!("Not a git repository"));
        }

        let output_text = match action {
            "status" => self.git_status(workspace).await?,
            "commit" => self.git_commit(workspace, params).await?,
            "push" => self.git_push(workspace, params).await?,
            "pull" => self.git_pull(workspace, params).await?,
            "log" => self.git_log(workspace, params).await?,
            "branch" => self.git_branch(workspace, params).await?,
            "stage" => self.git_stage(workspace, params).await?,
            _ => return Err(eyre!("Unknown git action: {}", action)),
        };

        Ok(GitOutput {
            action: action.to_string(),
            output: output_text,
        })
    }

    async fn git_status(&self, workspace: &Path) -> Result<String> {
        let outcome = self
            .git(["status", "--short", "--branch"].as_slice(), workspace)
            .await?;
        check_git_output(outcome)
    }

    async fn git_commit(&self, workspace: &Path, params: &GitParams) -> Result<String> {
        let message = params
            .message
            .as_ref()
            .ok_or_else(|| eyre!("message required"))?;

        // Stage files: specific files if provided and non-empty, otherwise stage all
        if let Some(files) = &params.files {
            if files.is_empty() {
                // Empty files array → stage all
                check_git_output(self.git(["add", "-A"].as_slice(), workspace).await?)?;
            } else {
                for file in files {
                    check_git_output(self.git(["add", file].as_slice(), workspace).await?)?;
                }
            }
        } else {
            // No files specified → stage all changes
            check_git_output(self.git(["add", "-A"].as_slice(), workspace).await?)?;
        }

        // Check if there's anything staged to commit
        let diff_check = self
            .git(["diff", "--cached", "--quiet"].as_slice(), workspace)
            .await?;

        if diff_check.success() {
            // Exit code 0 = no diff = nothing staged
            return Ok("Nothing to commit — working tree clean".to_string());
        }

        // Commit
        check_git_output(
            self.git(["commit", "-m", message].as_slice(), workspace)
                .await?,
        )?;

        // Gather enriched response
        let sha_output = self
            .git(["rev-parse", "HEAD"].as_slice(), workspace)
            .await?;
        let sha = sha_output.stdout.trim().to_string();

        let stat_output = self
            .git(["diff", "--stat", "HEAD~1", "HEAD"].as_slice(), workspace)
            .await?;
        let stat = stat_output.stdout.trim().to_string();

        Ok(format!("Committed: {}\n{}", sha, stat))
    }

    async fn git_push(&self, workspace: &Path, params: &GitParams) -> Result<String> {
        // Pre-push gate: verify code compiles before allowing push
        if workspace.join("Cargo.toml").exists() {
            let check = run_command(
                self.runner.as_ref(),
                "cargo",
                &["check", "--quiet"],
                workspace,
                CODE_TOOL_TIMEOUT_SECS,
            )
            .await;

            if let Ok(output) = check {
                if !output.success() {
                    let stderr = &output.stderr;
                    let truncated = &stderr[..stderr.len().min(2000)];
                    return Err(eyre!(
                        "Push blocked: cargo check failed. Fix compilation errors before pushing.\n{}",
                        truncated
                    ));
                }
            }
        }

        let remote = params.remote.as_deref().unwrap_or("origin");
        let current = self.current_branch_or_main(workspace).await;
        let branch = params.branch.as_deref().unwrap_or(&current);

        let output = self
            .git(["push", "-u", remote, branch].as_slice(), workspace)
            .await?;

        if !output.success() {
            return Err(eyre!("Git push failed: {}", output.stderr));
        }

        Ok(format!("{}\n{}", output.stdout, output.stderr))
    }

    async fn git_pull(&self, workspace: &Path, params: &GitParams) -> Result<String> {
        let remote = params.remote.as_deref().unwrap_or("origin");
        let current = self.current_branch_or_main(workspace).await;
        let branch = params.branch.as_deref().unwrap_or(&current);

        let output = self
            .git(["pull", remote, branch].as_slice(), workspace)
            .await?;
        check_git_output(output)
    }

    async fn git_log(&self, workspace: &Path, params: &GitParams) -> Result<String> {
        let limit = params.limit.unwrap_or(10).to_string();

        let output = self
            .git(["log", "--oneline", "-n", &limit].as_slice(), workspace)
            .await?;
        check_git_output(output)
    }

    async fn git_branch(&self, workspace: &Path, params: &GitParams) -> Result<String> {
        if let Some(branch) = &params.branch {
            // Try checking out existing branch first
            let output = self.git(["checkout", branch].as_slice(), workspace).await?;
            if output.success() {
                return check_git_output(output);
            }

            // Branch doesn't exist locally — try creating it
            let output = self
                .git(["checkout", "-b", branch].as_slice(), workspace)
                .await?;
            if output.success() {
                return check_git_output(output);
            }

            // Maybe it's a remote branch — fetch and retry
            let _ = self.git(["fetch", "origin"].as_slice(), workspace).await;

            let output = self.git(["checkout", branch].as_slice(), workspace).await?;
            if output.success() {
                return check_git_output(output);
            }

            Err(eyre!(
                "Failed to checkout branch '{}': {}",
                branch,
                output.stderr
            ))
        } else {
            // List branches
            let output = self.git(["branch", "-a"].as_slice(), workspace).await?;
            check_git_output(output)
        }
    }

    async fn git_stage(&self, workspace: &Path, params: &GitParams) -> Result<String> {
        if let Some(files) = &params.files {
            if files.is_empty() {
                // Empty files array → stage all
                check_git_output(self.git(["add", "-A"].as_slice(), workspace).await?)?;
            } else {
                for file in files {
                    check_git_output(self.git(["add", file].as_slice(), workspace).await?)?;
                }
            }
        } else {
            // No files specified → stage all changes
            check_git_output(self.git(["add", "-A"].as_slice(), workspace).await?)?;
        }

        // Return what's now staged
        let output = self
            .git(["diff", "--cached", "--stat"].as_slice(), workspace)
            .await?;
        let stat = output.stdout.trim().to_string();

        if stat.is_empty() {
            Ok("Nothing to stage — working tree clean".to_string())
        } else {
            Ok(format!("Staged:\n{}", stat))
        }
    }
}

#[derive(Deserialize, Default)]
struct GitParams {
    branch: Option<String>,
    message: Option<String>,
    files: Option<Vec<String>>,
    remote: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct CodeGitInput {
    action: String,
    #[serde(default)]
    repo: Option<String>,
    #[serde(flatten)]
    params: GitParams,
}

#[derive(Serialize, Deserialize)]
struct GitOutput {
    action: String,
    output: String,
}

#[async_trait]
impl Tool for CodeGitTool {
    fn name(&self) -> &str {
        "code.git"
    }

    fn description(&self) -> &str {
        "Execute git operations: status, commit, push, pull, log, branch, stage."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["status", "commit", "push", "pull", "log", "branch", "stage"],
                    "description": "Git operation to perform"
                },
                "branch": {"type": "string", "description": "Branch name (for branch/push/pull actions)"},
                "message": {"type": "string", "description": "Commit message (for commit action)"},
                "files": {"type": "array", "items": {"type": "string"}, "description": "File paths to stage (for stage action)"},
                "remote": {"type": "string", "description": "Remote name (default: 'origin')"},
                "limit": {"type": "integer", "description": "Number of log entries (integer, default: 10)", "minimum": 1},
                "repo": {
                    "type": "string",
                    "description": super::utils::REPO_PARAM
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: CodeGitInput = serde_json::from_value(input)?;

        let workspace = if let Some(ref repo) = input.repo {
            let base = self
                .allowed_paths
                .first()
                .ok_or_else(|| eyre!("No workspace"))?;
            match super::utils::resolve_repo_path(repo, base) {
                Ok(p) => p,
                Err(e) => {
                    return Ok(ToolResult::failure(
                        format!("Git operation failed: {}", e),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        } else {
            let base = self
                .allowed_paths
                .first()
                .ok_or_else(|| eyre!("No workspace"))?;
            let hint = super::utils::checked_out_note(base);
            return Ok(ToolResult::failure(
                format!(
                    "Missing 'repo' parameter. Specify which repo to operate on. {}",
                    hint
                ),
                start.elapsed().as_millis() as u64,
            ));
        };

        match self.run_git(&workspace, &input.action, &input.params).await {
            Ok(output) => Ok(ToolResult::success(
                serde_json::to_value(output)?,
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Git operation failed: {}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;
    use tempfile::TempDir;

    fn has_git() -> bool {
        StdCommand::new("git").arg("--version").output().is_ok()
    }

    #[tokio::test]
    async fn git_with_repo_param() {
        if !has_git() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let workspace = temp.path().to_path_buf();

        // Create a repo at repos/test-org/test-repo
        let repo_dir = workspace.join("repos").join("test-org").join("test-repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        StdCommand::new("git")
            .args(["init"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();
        std::fs::write(repo_dir.join("README.md"), "# Test").unwrap();
        StdCommand::new("git")
            .args(["add", "."])
            .current_dir(&repo_dir)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();

        let tool = CodeGitTool::new(vec![workspace]);
        let result = tool
            .execute(serde_json::json!({
                "action": "status",
                "repo": "test-org/test-repo"
            }))
            .await
            .unwrap();
        assert!(result.success, "Status should succeed: {:?}", result.output);
    }

    #[tokio::test]
    async fn git_branch_checkout_existing() {
        if !has_git() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let workspace = temp.path().to_path_buf();

        // Create a repo at repos/test-org/test-repo
        let repo_dir = workspace.join("repos").join("test-org").join("test-repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        StdCommand::new("git")
            .args(["init"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();
        std::fs::write(repo_dir.join("README.md"), "# Test").unwrap();
        StdCommand::new("git")
            .args(["add", "."])
            .current_dir(&repo_dir)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();

        // Create a feature branch with a new file
        StdCommand::new("git")
            .args(["checkout", "-b", "feat/test-branch"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();
        std::fs::write(repo_dir.join("new-file.ts"), "export const x = 1;").unwrap();
        StdCommand::new("git")
            .args(["add", "."])
            .current_dir(&repo_dir)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["commit", "-m", "add new file"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();

        // Switch back to main/master
        // (git init may create "master" or "main" depending on config)
        let _ = StdCommand::new("git")
            .args(["checkout", "main"])
            .current_dir(&repo_dir)
            .output();
        let _ = StdCommand::new("git")
            .args(["checkout", "master"])
            .current_dir(&repo_dir)
            .output();

        // Now use the tool to checkout the existing feature branch
        let tool = CodeGitTool::new(vec![workspace]);
        let result = tool
            .execute(serde_json::json!({
                "action": "branch",
                "repo": "test-org/test-repo",
                "branch": "feat/test-branch"
            }))
            .await
            .unwrap();
        assert!(
            result.success,
            "Checking out existing branch should succeed: {:?}",
            result.output
        );

        // Verify the file from the feature branch exists
        assert!(
            repo_dir.join("new-file.ts").exists(),
            "new-file.ts should exist after checking out feature branch"
        );
    }

    #[tokio::test]
    async fn git_repo_path_traversal_blocked() {
        let temp = TempDir::new().unwrap();
        let workspace = temp.path().to_path_buf();

        let tool = CodeGitTool::new(vec![workspace]);
        let result = tool
            .execute(serde_json::json!({
                "action": "status",
                "repo": "../../etc"
            }))
            .await
            .unwrap();
        assert!(!result.success, "Path traversal should be blocked");
    }

    /// Helper: set up a temp git repo with an initial commit and return (temp_dir, workspace, repo_dir).
    fn setup_test_repo() -> (TempDir, PathBuf, PathBuf) {
        let temp = TempDir::new().unwrap();
        let workspace = temp.path().to_path_buf();
        let repo_dir = workspace.join("repos").join("test-org").join("test-repo");
        std::fs::create_dir_all(&repo_dir).unwrap();

        StdCommand::new("git")
            .args(["init"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();
        std::fs::write(repo_dir.join("README.md"), "# Test").unwrap();
        StdCommand::new("git")
            .args(["add", "."])
            .current_dir(&repo_dir)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();

        (temp, workspace, repo_dir)
    }

    #[tokio::test]
    async fn git_commit_auto_stages_specific_files() {
        if !has_git() {
            return;
        }
        let (_temp, workspace, repo_dir) = setup_test_repo();

        // Create a new file (untracked)
        std::fs::write(repo_dir.join("new.txt"), "hello").unwrap();

        let tool = CodeGitTool::new(vec![workspace]);
        let result = tool
            .execute(serde_json::json!({
                "action": "commit",
                "repo": "test-org/test-repo",
                "files": ["new.txt"],
                "message": "add new.txt"
            }))
            .await
            .unwrap();
        assert!(
            result.success,
            "Commit with specific files should succeed: {:?}",
            result.output
        );
        let output = result.output.to_string();
        assert!(
            output.contains("Committed:"),
            "Output should contain 'Committed:': {}",
            output
        );
    }

    #[tokio::test]
    async fn git_commit_stages_all_when_no_files() {
        if !has_git() {
            return;
        }
        let (_temp, workspace, repo_dir) = setup_test_repo();

        // Create two new files (untracked)
        std::fs::write(repo_dir.join("a.txt"), "aaa").unwrap();
        std::fs::write(repo_dir.join("b.txt"), "bbb").unwrap();

        let tool = CodeGitTool::new(vec![workspace]);
        let result = tool
            .execute(serde_json::json!({
                "action": "commit",
                "repo": "test-org/test-repo",
                "message": "add all files"
            }))
            .await
            .unwrap();
        assert!(
            result.success,
            "Commit without files should auto-stage all: {:?}",
            result.output
        );
        let output = result.output.to_string();
        assert!(
            output.contains("a.txt"),
            "Output should mention a.txt: {}",
            output
        );
        assert!(
            output.contains("b.txt"),
            "Output should mention b.txt: {}",
            output
        );
    }

    #[tokio::test]
    async fn git_commit_clean_tree_returns_info() {
        if !has_git() {
            return;
        }
        let (_temp, workspace, _repo_dir) = setup_test_repo();

        let tool = CodeGitTool::new(vec![workspace]);
        let result = tool
            .execute(serde_json::json!({
                "action": "commit",
                "repo": "test-org/test-repo",
                "message": "nothing to commit"
            }))
            .await
            .unwrap();
        assert!(
            result.success,
            "Commit on clean tree should succeed (info), not error: {:?}",
            result.output
        );
        let output = result.output.to_string();
        assert!(
            output.contains("Nothing to commit"),
            "Output should indicate clean tree: {}",
            output
        );
    }

    #[tokio::test]
    async fn git_commit_response_includes_stats() {
        if !has_git() {
            return;
        }
        let (_temp, workspace, repo_dir) = setup_test_repo();

        std::fs::write(repo_dir.join("stats.txt"), "line1\nline2\nline3\n").unwrap();

        let tool = CodeGitTool::new(vec![workspace]);
        let result = tool
            .execute(serde_json::json!({
                "action": "commit",
                "repo": "test-org/test-repo",
                "message": "add stats.txt"
            }))
            .await
            .unwrap();
        assert!(result.success, "Commit should succeed: {:?}", result.output);
        let output = result.output.to_string();
        // SHA should be present
        assert!(
            output.contains("Committed:"),
            "Output should contain commit SHA: {}",
            output
        );
        // Stat output should mention insertions
        assert!(
            output.contains("insertion"),
            "Output should contain insertion stats: {}",
            output
        );
    }

    #[tokio::test]
    async fn git_stage_specific_files() {
        if !has_git() {
            return;
        }
        let (_temp, workspace, repo_dir) = setup_test_repo();

        std::fs::write(repo_dir.join("staged.txt"), "content").unwrap();
        std::fs::write(repo_dir.join("unstaged.txt"), "other").unwrap();

        let tool = CodeGitTool::new(vec![workspace]);
        let result = tool
            .execute(serde_json::json!({
                "action": "stage",
                "repo": "test-org/test-repo",
                "files": ["staged.txt"]
            }))
            .await
            .unwrap();
        assert!(
            result.success,
            "Stage specific files should succeed: {:?}",
            result.output
        );
        let output = result.output.to_string();
        assert!(
            output.contains("staged.txt"),
            "Output should list staged file: {}",
            output
        );
    }

    #[tokio::test]
    async fn git_stage_all_files() {
        if !has_git() {
            return;
        }
        let (_temp, workspace, repo_dir) = setup_test_repo();

        std::fs::write(repo_dir.join("one.txt"), "1").unwrap();
        std::fs::write(repo_dir.join("two.txt"), "2").unwrap();

        let tool = CodeGitTool::new(vec![workspace]);
        let result = tool
            .execute(serde_json::json!({
                "action": "stage",
                "repo": "test-org/test-repo"
            }))
            .await
            .unwrap();
        assert!(
            result.success,
            "Stage all should succeed: {:?}",
            result.output
        );
        let output = result.output.to_string();
        assert!(
            output.contains("Staged:"),
            "Output should contain 'Staged:': {}",
            output
        );
        assert!(
            output.contains("one.txt"),
            "Output should list one.txt: {}",
            output
        );
        assert!(
            output.contains("two.txt"),
            "Output should list two.txt: {}",
            output
        );
    }

    #[tokio::test]
    async fn git_stage_clean_tree() {
        if !has_git() {
            return;
        }
        let (_temp, workspace, _repo_dir) = setup_test_repo();

        let tool = CodeGitTool::new(vec![workspace]);
        let result = tool
            .execute(serde_json::json!({
                "action": "stage",
                "repo": "test-org/test-repo"
            }))
            .await
            .unwrap();
        assert!(
            result.success,
            "Stage on clean tree should succeed (info), not error: {:?}",
            result.output
        );
        let output = result.output.to_string();
        assert!(
            output.contains("Nothing to stage"),
            "Output should indicate clean tree: {}",
            output
        );
    }

    /// Count commits reachable from HEAD (used as the fail-closed sentinel).
    fn commit_count(repo_dir: &Path) -> usize {
        let out = StdCommand::new("git")
            .args(["rev-list", "--count", "HEAD"])
            .current_dir(repo_dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
    }

    /// Wiring test: a configured-but-unreachable runner socket must fail closed
    /// — `code.git commit` errors and the command does NOT run in-process, so no
    /// new commit lands (the observable sentinel). With `None` the same commit
    /// runs in-process and succeeds, proving the socket is what gates execution.
    #[tokio::test]
    async fn git_routes_through_the_runner_and_fails_closed() {
        if !has_git() {
            return;
        }
        let (_temp, workspace, repo_dir) = setup_test_repo();
        std::fs::write(repo_dir.join("sentinel.txt"), "should-not-commit").unwrap();
        let before = commit_count(&repo_dir);

        // Dead socket: dispatch fails closed, the commit must not execute.
        let dead_socket = repo_dir.join("absent.sock");
        let gated = CodeGitTool::new(vec![workspace.clone()])
            .with_runner(Some(RunnerEndpoint::Unix(dead_socket)));
        let result = gated
            .execute(serde_json::json!({
                "action": "commit",
                "repo": "test-org/test-repo",
                "files": ["sentinel.txt"],
                "message": "must not land"
            }))
            .await
            .unwrap();
        assert!(!result.success, "dead socket must fail the tool");
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("secure command runner unavailable"),
            "expected fail-closed error, got: {:?}",
            result.error
        );
        assert_eq!(
            commit_count(&repo_dir),
            before,
            "fail-closed violated: a commit was created in-process"
        );

        // No socket: the identical commit runs in-process and lands.
        let local = CodeGitTool::new(vec![workspace]);
        let result = local
            .execute(serde_json::json!({
                "action": "commit",
                "repo": "test-org/test-repo",
                "files": ["sentinel.txt"],
                "message": "lands in-process"
            }))
            .await
            .unwrap();
        assert!(
            result.success,
            "in-process commit should succeed: {:?}",
            result
        );
        assert_eq!(
            commit_count(&repo_dir),
            before + 1,
            "in-process commit should add exactly one commit"
        );
    }
}
