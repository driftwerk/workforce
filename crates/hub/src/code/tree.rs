//! code.tree - Project structure overview with directory tree visualization.

use async_trait::async_trait;
use eyre::{eyre, Result};
use futures::future::{BoxFuture, FutureExt};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;
use workforce_agent::tools::exec_runner::RunnerEndpoint;
use workforce_agent::tools::{Tool, ToolResult};

use super::utils::{detect_project_type, run_command, CODE_TOOL_TIMEOUT_SECS};

const DEFAULT_DEPTH: usize = 3;
const MAX_DEPTH: usize = 10;

/// code.tree tool - Project structure and directory tree.
pub struct CodeTreeTool {
    allowed_paths: Vec<PathBuf>,
    /// When set, the `git status` probe dispatches to the privilege-separated
    /// runner daemon (audit F7); `None` runs it in-process (dev/local). Even a
    /// read-only `git status` honours an agent-planted `core.fsmonitor` hook, so
    /// it must run as the agent in agent-writable trees, not as the hub.
    runner: Option<RunnerEndpoint>,
}

impl CodeTreeTool {
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

    /// Generate directory tree.
    async fn generate_tree(
        &self,
        root: &Path,
        depth: usize,
        show_hidden: bool,
    ) -> Result<TreeOutput> {
        // Detect project type
        let project_type = detect_project_type(root)
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unknown".to_string());

        // Build tree structure
        let mut tree_lines = Vec::new();
        let mut stats = TreeStats {
            total_files: 0,
            total_dirs: 0,
            git_modified: 0,
        };

        self.walk_directory(root, "", depth, 0, show_hidden, &mut tree_lines, &mut stats)
            .await?;

        // Get git status if available
        if root.join(".git").exists() {
            if let Ok(modified_count) = self.get_git_modified_count(root).await {
                stats.git_modified = modified_count;
            }
        }

        Ok(TreeOutput {
            project_type,
            root: root.display().to_string(),
            tree: tree_lines,
            stats,
        })
    }

    /// Recursively walk directory and build tree.
    #[allow(clippy::too_many_arguments, clippy::only_used_in_recursion)]
    fn walk_directory<'a>(
        &'a self,
        path: &'a Path,
        prefix: &'a str,
        max_depth: usize,
        current_depth: usize,
        show_hidden: bool,
        tree_lines: &'a mut Vec<String>,
        stats: &'a mut TreeStats,
    ) -> BoxFuture<'a, Result<()>> {
        async move {
            if current_depth >= max_depth {
                return Ok(());
            }

            // Read directory entries
            let entries: Vec<_> = fs::read_dir(path)?
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let name = e.file_name();
                    let name_str = name.to_string_lossy();

                    // Filter hidden files if needed
                    if !show_hidden && name_str.starts_with('.') {
                        return false;
                    }

                    // Always skip common noise directories
                    !matches!(
                        name_str.as_ref(),
                        "." | ".." | "node_modules" | "target" | "__pycache__" | ".git"
                    ) && !workforce_agent::tools::paths::is_withheld(&e.path())
                })
                .collect();

            // Sort entries: directories first, then files
            let mut sorted_entries = entries;
            sorted_entries.sort_by(|a, b| {
                let a_is_dir = a.path().is_dir();
                let b_is_dir = b.path().is_dir();

                match (a_is_dir, b_is_dir) {
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                    _ => a.file_name().cmp(&b.file_name()),
                }
            });

            let len = sorted_entries.len();
            for (idx, entry) in sorted_entries.iter().enumerate() {
                let is_last = idx == len - 1;
                let connector = if is_last { "└── " } else { "├── " };

                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                let path = entry.path();

                if path.is_dir() {
                    stats.total_dirs += 1;
                    tree_lines.push(format!("{}{}{}/", prefix, connector, name_str));

                    // Recurse into subdirectory
                    let new_prefix = if is_last {
                        format!("{}    ", prefix)
                    } else {
                        format!("{}│   ", prefix)
                    };

                    self.walk_directory(
                        &path,
                        &new_prefix,
                        max_depth,
                        current_depth + 1,
                        show_hidden,
                        tree_lines,
                        stats,
                    )
                    .await?;
                } else {
                    stats.total_files += 1;
                    tree_lines.push(format!("{}{}{}", prefix, connector, name_str));
                }
            }

            Ok(())
        }
        .boxed()
    }

    /// Get count of modified files from git status.
    async fn get_git_modified_count(&self, root: &Path) -> Result<usize> {
        let outcome = run_command(
            self.runner.as_ref(),
            "git",
            &["status", "--short"],
            root,
            CODE_TOOL_TIMEOUT_SECS,
        )
        .await?;

        if !outcome.success() {
            return Ok(0);
        }

        Ok(outcome.stdout.lines().count())
    }
}

#[derive(Deserialize)]
struct CodeTreeInput {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default = "default_depth")]
    depth: usize,
    #[serde(default)]
    show_hidden: bool,
}

fn default_depth() -> usize {
    DEFAULT_DEPTH
}

#[derive(Serialize, Deserialize)]
struct TreeStats {
    total_files: usize,
    total_dirs: usize,
    git_modified: usize,
}

#[derive(Serialize, Deserialize)]
struct TreeOutput {
    project_type: String,
    root: String,
    tree: Vec<String>,
    stats: TreeStats,
}

#[async_trait]
impl Tool for CodeTreeTool {
    fn name(&self) -> &str {
        "code.tree"
    }

    fn description(&self) -> &str {
        "Show directory tree structure"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Root path (default: workspace root)"
                },
                "depth": {
                    "type": "integer",
                    "description": "Maximum depth to traverse (default: 3, max: 10)",
                    "minimum": 1,
                    "maximum": MAX_DEPTH,
                    "default": DEFAULT_DEPTH
                },
                "show_hidden": {
                    "type": "boolean",
                    "description": "Show hidden files/directories (default: false)",
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

        let input: CodeTreeInput =
            serde_json::from_value(input).map_err(|e| eyre!("Invalid input: {}", e))?;

        // Determine root path
        let root = if let Some(ref repo) = input.repo {
            let base = self
                .allowed_paths
                .first()
                .ok_or_else(|| eyre!("No workspace configured"))?;
            match super::utils::resolve_repo_path(repo, base) {
                Ok(p) => p,
                Err(e) => {
                    return Ok(ToolResult::failure(
                        format!("Tree generation failed: {}", e),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        } else if let Some(p) = input.path {
            PathBuf::from(p)
        } else {
            self.allowed_paths
                .first()
                .ok_or_else(|| eyre!("No workspace configured"))?
                .clone()
        };

        // Validate depth
        let depth = input.depth.min(MAX_DEPTH);

        match self.generate_tree(&root, depth, input.show_hidden).await {
            Ok(output) => Ok(ToolResult::success(
                serde_json::to_value(output)?,
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Tree generation failed: {}", e),
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

    #[tokio::test]
    async fn test_tree_basic_structure() {
        let temp = TempDir::new().unwrap();

        // Create simple structure
        fs::create_dir(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(temp.path().join("Cargo.toml"), "[package]").unwrap();

        let tool = CodeTreeTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({});

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: TreeOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.project_type, "rust");
        assert!(!output.tree.is_empty());
        assert!(output.stats.total_files > 0);
    }

    #[tokio::test]
    async fn test_tree_depth_limit() {
        let temp = TempDir::new().unwrap();

        // Create nested structure
        fs::create_dir_all(temp.path().join("a/b/c")).unwrap();
        fs::write(temp.path().join("a/b/c/deep.txt"), "content").unwrap();

        let tool = CodeTreeTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "depth": 2
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: TreeOutput = serde_json::from_value(result.output).unwrap();
        // Should not include deep.txt because it's at depth 3
        let tree_str = output.tree.join("\n");
        assert!(!tree_str.contains("deep.txt"));
    }

    #[tokio::test]
    async fn test_tree_hidden_files() {
        let temp = TempDir::new().unwrap();

        fs::write(temp.path().join("visible.txt"), "content").unwrap();
        fs::write(temp.path().join(".hidden"), "content").unwrap();

        let tool = CodeTreeTool::new(vec![temp.path().to_path_buf()]);

        // Without show_hidden
        let input = serde_json::json!({});
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: TreeOutput = serde_json::from_value(result.output).unwrap();
        let tree_str = output.tree.join("\n");
        assert!(tree_str.contains("visible.txt"));
        assert!(!tree_str.contains(".hidden"));

        // With show_hidden
        let input = serde_json::json!({
            "show_hidden": true
        });
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: TreeOutput = serde_json::from_value(result.output).unwrap();
        let tree_str = output.tree.join("\n");
        assert!(tree_str.contains("visible.txt"));
        assert!(tree_str.contains(".hidden"));
    }

    #[tokio::test]
    async fn test_tree_project_type_detection() {
        // Test Node project
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("package.json"), "{}").unwrap();

        let tool = CodeTreeTool::new(vec![temp.path().to_path_buf()]);
        let result = tool.execute(serde_json::json!({})).await.unwrap();

        assert!(result.success);
        let output: TreeOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.project_type, "node");
    }

    /// Wiring test: the `git status` enrichment probe routes through the runner.
    /// With a configured-but-unreachable socket the git probe fails closed and
    /// does NOT run in-process (which would honour an agent-planted
    /// `core.fsmonitor` hook AS the hub) — the observable proof is `git_modified`
    /// stays 0 despite a real uncommitted change. With `None` the identical probe
    /// runs in-process and counts the change. The tree listing itself is pure
    /// `std::fs` and succeeds either way.
    #[tokio::test]
    async fn tree_git_probe_routes_through_the_runner_and_fails_closed() {
        use std::process::Command as StdCommand;
        if StdCommand::new("git").arg("--version").output().is_err() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        for args in [
            vec!["init"],
            vec!["config", "user.email", "t@t.com"],
            vec!["config", "user.name", "T"],
        ] {
            StdCommand::new("git")
                .args(&args)
                .current_dir(root)
                .output()
                .unwrap();
        }
        fs::write(root.join("tracked.txt"), "v1").unwrap();
        StdCommand::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();
        // An uncommitted change the git probe would count.
        fs::write(root.join("tracked.txt"), "v2").unwrap();

        // Dead socket: the git probe fails closed, so it never runs in-process
        // and git_modified stays 0. The tree (pure fs) still lists.
        let dead_socket = root.join("absent.sock");
        let gated = CodeTreeTool::new(vec![root.to_path_buf()])
            .with_runner(Some(RunnerEndpoint::Unix(dead_socket)));
        let result = gated.execute(serde_json::json!({})).await.unwrap();
        assert!(
            result.success,
            "tree listing should still succeed: {result:?}"
        );
        let output: TreeOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(
            output.stats.git_modified, 0,
            "fail-closed violated: git probe ran in-process"
        );

        // No socket: the identical probe runs in-process and counts the change.
        let local = CodeTreeTool::new(vec![root.to_path_buf()]);
        let result = local.execute(serde_json::json!({})).await.unwrap();
        assert!(result.success);
        let output: TreeOutput = serde_json::from_value(result.output).unwrap();
        assert!(
            output.stats.git_modified >= 1,
            "in-process probe should count the uncommitted change"
        );
    }

    #[tokio::test]
    async fn test_tree_file_count() {
        let temp = TempDir::new().unwrap();

        fs::create_dir(temp.path().join("dir1")).unwrap();
        fs::create_dir(temp.path().join("dir2")).unwrap();
        fs::write(temp.path().join("file1.txt"), "content").unwrap();
        fs::write(temp.path().join("file2.txt"), "content").unwrap();
        fs::write(temp.path().join("dir1/file3.txt"), "content").unwrap();

        let tool = CodeTreeTool::new(vec![temp.path().to_path_buf()]);
        let result = tool.execute(serde_json::json!({})).await.unwrap();

        assert!(result.success);
        let output: TreeOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.stats.total_files, 3);
        assert_eq!(output.stats.total_dirs, 2);
    }
}
