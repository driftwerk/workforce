//! code.diff - Git diff viewing and change summary.

use async_trait::async_trait;
use eyre::{eyre, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Instant;
use workforce_agent::tools::exec_runner::RunnerEndpoint;
use workforce_agent::tools::{Tool, ToolResult};

use super::utils::{run_command, CODE_TOOL_TIMEOUT_SECS};

/// code.diff tool - View git diffs and changes.
pub struct CodeDiffTool {
    allowed_paths: Vec<PathBuf>,
    /// When set, git commands dispatch to the privilege-separated runner
    /// daemon (audit F7); `None` runs them in-process (dev/local). `git diff`
    /// honours agent-controllable `.gitattributes`/`diff.external` drivers, so
    /// it must run as the agent in agent-writable trees, not as the hub.
    runner: Option<RunnerEndpoint>,
}

impl CodeDiffTool {
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

    /// Run `git` with `args` in `workspace` through the executor (dispatched
    /// when a runner socket is configured, else in-process).
    async fn git(&self, args: &[&str], workspace: &Path) -> Result<String> {
        let outcome = run_command(
            self.runner.as_ref(),
            "git",
            args,
            workspace,
            CODE_TOOL_TIMEOUT_SECS,
        )
        .await?;
        Ok(outcome.stdout)
    }

    /// Get diff based on target type.
    async fn get_diff(
        &self,
        workspace: &Path,
        target: &str,
        ref_name: Option<&str>,
        path: Option<&str>,
        stat_only: bool,
    ) -> Result<DiffOutput> {
        // Check if git is available
        if !workspace.join(".git").exists() {
            return Err(eyre!("Not a git repository"));
        }

        // Build the `git diff` argv shared by the numstat and full-diff passes,
        // preserving the prior `Command` argument order exactly.
        let build_args = |with_numstat: bool| {
            let mut args: Vec<String> = vec!["diff".to_string()];

            match target {
                "working" => {}
                "staged" => args.push("--staged".to_string()),
                "branch" | "commit" => {
                    if let Some(r) = ref_name {
                        args.push(r.to_string());
                    }
                }
                _ => {}
            }

            if let Some(p) = path {
                args.push("--".to_string());
                args.push(p.to_string());
            }

            if with_numstat {
                args.push("--numstat".to_string());
            }

            args
        };

        // Validate ref parameter for branch/commit targets
        match target {
            "branch" | "commit" if ref_name.is_none() => {
                return Err(eyre!("'ref' parameter required for {} diff", target));
            }
            _ => {}
        }

        // Get stats first
        let stat_args = build_args(true);
        let stat_argv: Vec<&str> = stat_args.iter().map(String::as_str).collect();
        let stat_stdout = self.git(&stat_argv, workspace).await?;

        // Parse stats
        let mut total_insertions = 0;
        let mut total_deletions = 0;
        let mut file_stats = Vec::new();

        for line in stat_stdout.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 {
                let insertions: usize = parts[0].parse().unwrap_or(0);
                let deletions: usize = parts[1].parse().unwrap_or(0);
                let file_path = parts[2..].join(" ");

                total_insertions += insertions;
                total_deletions += deletions;

                file_stats.push(FileStat {
                    path: file_path.clone(),
                    status: "modified".to_string(),
                    insertions,
                    deletions,
                    diff: if stat_only {
                        None
                    } else {
                        Some(String::new()) // Will be filled below
                    },
                });
            }
        }

        // Get full diff if not stat_only
        let full_diff = if !stat_only && !file_stats.is_empty() {
            let diff_args = build_args(false);
            let diff_argv: Vec<&str> = diff_args.iter().map(String::as_str).collect();
            Some(self.git(&diff_argv, workspace).await?)
        } else {
            None
        };

        // If we have full diff, split it per file
        if let Some(diff_text) = full_diff {
            // Simple per-file diff extraction (could be improved)
            for file_stat in &mut file_stats {
                // Find diff section for this file
                let file_marker = format!("diff --git a/{} b/{}", file_stat.path, file_stat.path);
                if let Some(start_idx) = diff_text.find(&file_marker) {
                    // Find next file marker or end of string
                    let remaining = &diff_text[start_idx..];
                    let end_idx = remaining.find("\ndiff --git").unwrap_or(remaining.len());
                    file_stat.diff = Some(remaining[..end_idx].to_string());
                }
            }
        }

        Ok(DiffOutput {
            target: target.to_string(),
            files_changed: file_stats.len(),
            insertions: total_insertions,
            deletions: total_deletions,
            files: file_stats,
        })
    }
}

#[derive(Deserialize)]
struct CodeDiffInput {
    #[serde(default = "default_target")]
    target: String,
    #[serde(default, rename = "ref")]
    ref_name: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    stat_only: bool,
    #[serde(default)]
    repo: Option<String>,
}

fn default_target() -> String {
    "working".to_string()
}

#[derive(Serialize, Deserialize, Debug)]
struct FileStat {
    path: String,
    status: String,
    insertions: usize,
    deletions: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    diff: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct DiffOutput {
    #[serde(rename = "type")]
    target: String,
    files_changed: usize,
    insertions: usize,
    deletions: usize,
    files: Vec<FileStat>,
}

#[async_trait]
impl Tool for CodeDiffTool {
    fn name(&self) -> &str {
        "code.diff"
    }

    fn description(&self) -> &str {
        "View git diffs and change summaries"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "enum": ["working", "staged", "branch", "commit"],
                    "description": "What to diff (default: working)",
                    "default": "working"
                },
                "ref": {
                    "type": "string",
                    "description": "Branch or commit ref for comparison (required for"
                },
                "path": {
                    "type": "string",
                    "description": "Specific file/directory to diff (optional)"
                },
                "stat_only": {
                    "type": "boolean",
                    "description": "Show only file stats, not full diff (default: false)",
                    "default": false
                },
                "repo": {
                    "type": "string",
                    "description": super::utils::REPO_PARAM
                }
            }
        ,
            "required": []})
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();

        let input: CodeDiffInput =
            serde_json::from_value(input).map_err(|e| eyre!("Invalid input: {}", e))?;

        let workspace = if let Some(ref repo) = input.repo {
            let base = self
                .allowed_paths
                .first()
                .ok_or_else(|| eyre!("No workspace configured"))?;
            match super::utils::resolve_repo_path(repo, base) {
                Ok(p) => p,
                Err(e) => {
                    return Ok(ToolResult::failure(
                        format!("Diff failed: {}", e),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        } else {
            self.allowed_paths
                .first()
                .ok_or_else(|| eyre!("No workspace configured"))?
                .clone()
        };

        match self
            .get_diff(
                &workspace,
                &input.target,
                input.ref_name.as_deref(),
                input.path.as_deref(),
                input.stat_only,
            )
            .await
        {
            Ok(output) => Ok(ToolResult::success(
                serde_json::to_value(output)?,
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Diff failed: {}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;
    use tokio::process::Command;

    async fn init_git_repo(path: &std::path::Path) {
        Command::new("git")
            .args(["init"])
            .current_dir(path)
            .output()
            .await
            .unwrap();

        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(path)
            .output()
            .await
            .unwrap();

        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(path)
            .output()
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_diff_working_changes() {
        let temp = TempDir::new().unwrap();
        init_git_repo(temp.path()).await;

        // Create initial file and commit
        fs::write(temp.path().join("file.txt"), "line1\nline2\n").unwrap();
        Command::new("git")
            .args(["add", "file.txt"])
            .current_dir(temp.path())
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(temp.path())
            .output()
            .await
            .unwrap();

        // Make changes
        fs::write(temp.path().join("file.txt"), "line1\nline2\nline3\n").unwrap();

        let tool = CodeDiffTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "target": "working"
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: DiffOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.target, "working");
        assert!(output.files_changed > 0);
    }

    #[tokio::test]
    async fn test_diff_staged_changes() {
        let temp = TempDir::new().unwrap();
        init_git_repo(temp.path()).await;

        // Create and commit initial file
        fs::write(temp.path().join("file.txt"), "line1\n").unwrap();
        Command::new("git")
            .args(["add", "file.txt"])
            .current_dir(temp.path())
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(temp.path())
            .output()
            .await
            .unwrap();

        // Make changes and stage
        fs::write(temp.path().join("file.txt"), "line1\nline2\n").unwrap();
        Command::new("git")
            .args(["add", "file.txt"])
            .current_dir(temp.path())
            .output()
            .await
            .unwrap();

        let tool = CodeDiffTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "target": "staged"
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: DiffOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.target, "staged");
        assert!(output.files_changed > 0);
    }

    #[tokio::test]
    async fn test_diff_stat_only() {
        let temp = TempDir::new().unwrap();
        init_git_repo(temp.path()).await;

        // Create and commit file
        fs::write(temp.path().join("file.txt"), "line1\n").unwrap();
        Command::new("git")
            .args(["add", "file.txt"])
            .current_dir(temp.path())
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(temp.path())
            .output()
            .await
            .unwrap();

        // Make changes
        fs::write(temp.path().join("file.txt"), "line1\nline2\n").unwrap();

        let tool = CodeDiffTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "target": "working",
            "stat_only": true
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: DiffOutput = serde_json::from_value(result.output).unwrap();
        // stat_only should not include full diff
        assert!(output.files[0].diff.is_none());
    }

    #[tokio::test]
    async fn test_diff_not_a_repo() {
        let temp = TempDir::new().unwrap();
        // Don't initialize git

        let tool = CodeDiffTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({});

        let result = tool.execute(input).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Not a git repository"));
    }

    /// Wiring test: a configured-but-unreachable runner socket fails closed —
    /// `code.diff` errors and the git execution does NOT fall back in-process
    /// (which would run an agent-planted `diff.external`/textconv driver AS the
    /// hub). With `None` the identical diff runs in-process and parses out the
    /// working-tree change, proving the socket is what gates execution.
    #[tokio::test]
    async fn diff_routes_through_the_runner_and_fails_closed() {
        let temp = TempDir::new().unwrap();
        init_git_repo(temp.path()).await;

        fs::write(temp.path().join("file.txt"), "line1\nline2\n").unwrap();
        Command::new("git")
            .args(["add", "file.txt"])
            .current_dir(temp.path())
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(temp.path())
            .output()
            .await
            .unwrap();
        fs::write(temp.path().join("file.txt"), "line1\nline2\nline3\n").unwrap();

        // Dead socket: dispatch fails closed, no in-process git runs.
        let dead_socket = temp.path().join("absent.sock");
        let gated = CodeDiffTool::new(vec![temp.path().to_path_buf()])
            .with_runner(Some(RunnerEndpoint::Unix(dead_socket)));
        let result = gated
            .execute(serde_json::json!({ "target": "working" }))
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

        // No socket: the identical diff runs in-process and reports the change.
        let local = CodeDiffTool::new(vec![temp.path().to_path_buf()]);
        let result = local
            .execute(serde_json::json!({ "target": "working" }))
            .await
            .unwrap();
        assert!(result.success, "in-process diff should succeed: {result:?}");
        let output: DiffOutput = serde_json::from_value(result.output).unwrap();
        assert!(output.files_changed > 0);
    }
}
