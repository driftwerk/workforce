//! code.edit - Surgical file editing with search-and-replace, create, and delete operations.

use async_trait::async_trait;
use eyre::{eyre, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Instant;
use workforce_agent::tools::config_guard;
use workforce_agent::tools::paths::CanonicalRoots;
use workforce_agent::tools::{Tool, ToolResult};

use super::utils::validate_path;

const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024; // 10 MB

/// code.edit tool - Precise surgical file editing.
pub struct CodeEditTool {
    allowed_paths: Vec<PathBuf>,
}

impl CodeEditTool {
    pub fn new(allowed_paths: Vec<PathBuf>) -> Self {
        Self { allowed_paths }
    }

    /// Replace text in a file.
    fn replace_in_file(
        &self,
        path: &std::path::Path,
        old_content: &str,
        new_content: &str,
        replace_all: bool,
        return_modified: bool,
    ) -> Result<EditResult> {
        let validated_path = validate_path(path, &self.allowed_paths)?;
        let roots = CanonicalRoots::new(&self.allowed_paths);
        config_guard::check(&roots, &validated_path)?;

        // Check file size
        let metadata = std::fs::metadata(&validated_path)?;
        if metadata.len() > MAX_FILE_SIZE {
            return Err(eyre!(
                "File too large: {} bytes (max: {} bytes)",
                metadata.len(),
                MAX_FILE_SIZE
            ));
        }

        // Read current content
        let content = std::fs::read_to_string(&validated_path)?;

        // Count matches
        let matches = if replace_all {
            content.matches(old_content).count()
        } else if content.contains(old_content) {
            1
        } else {
            0
        };

        if matches == 0 {
            return Err(eyre!("String not found: '{}'", old_content));
        }

        // Perform replacement
        let new_file_content = if replace_all {
            content.replace(old_content, new_content)
        } else {
            content.replacen(old_content, new_content, 1)
        };

        // Count lines changed (approximate)
        let old_lines: Vec<&str> = content.lines().collect();
        let new_lines: Vec<&str> = new_file_content.lines().collect();
        let lines_changed = (old_lines.len() as i32 - new_lines.len() as i32).unsigned_abs()
            as usize
            + old_lines
                .iter()
                .zip(new_lines.iter())
                .filter(|(a, b)| a != b)
                .count();

        // Write back
        config_guard::write(&roots, &validated_path, new_file_content.as_bytes())?;

        let modified_content = if return_modified {
            // Find the line number of the first change
            let first_change_line = old_lines
                .iter()
                .zip(new_lines.iter())
                .position(|(a, b)| a != b)
                .unwrap_or(0);

            let start_line = first_change_line.saturating_sub(5);
            let end_line = (first_change_line + 5).min(new_lines.len());

            Some(
                new_lines[start_line..end_line]
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<String>>()
                    .join("\n"),
            )
        } else {
            None
        };

        Ok(EditResult {
            action: "replace".to_string(),
            path: validated_path.display().to_string(),
            matches,
            lines_changed,
            success: true,
            modified_content,
        })
    }

    /// Create a new file.
    fn create_file(
        &self,
        path: &std::path::Path,
        content: &str,
        return_modified: bool,
    ) -> Result<EditResult> {
        // The same validation the other actions use: a lexical fold of `..`
        // kept the symlink spelling, so a create through an agent-planted link
        // with one missing directory wrote outside the workspace.
        let normalized_full = validate_path(path, &self.allowed_paths)?;
        let roots = CanonicalRoots::new(&self.allowed_paths);
        config_guard::check(&roots, &normalized_full)?;

        // Check if file already exists
        if normalized_full.exists() {
            return Err(eyre!("File already exists: {}", normalized_full.display()));
        }

        // Create parent directories if needed
        if let Some(parent) = normalized_full.parent() {
            config_guard::create_dirs(&roots, parent)?;
        }

        // Write file
        config_guard::create_new(&roots, &normalized_full, content.as_bytes())?;

        let lines = content.lines().count();
        let modified_content = if return_modified {
            Some(content.to_string())
        } else {
            None
        };

        Ok(EditResult {
            action: "create".to_string(),
            path: normalized_full.display().to_string(),
            matches: 0,
            lines_changed: lines,
            success: true,
            modified_content,
        })
    }

    /// Delete a file.
    fn delete_file(&self, path: &std::path::Path) -> Result<EditResult> {
        let validated_path = validate_path(path, &self.allowed_paths)?;
        let roots = CanonicalRoots::new(&self.allowed_paths);
        config_guard::check(&roots, &validated_path)?;

        if !validated_path.exists() {
            return Err(eyre!("File not found: {}", validated_path.display()));
        }

        if validated_path.is_dir() {
            return Err(eyre!(
                "Cannot delete directory: {}. Use a directory deletion tool instead.",
                validated_path.display()
            ));
        }

        // Count lines before deletion
        let content = std::fs::read_to_string(&validated_path)?;
        let lines = content.lines().count();

        config_guard::remove(&roots, &validated_path)?;

        Ok(EditResult {
            action: "delete".to_string(),
            path: validated_path.display().to_string(),
            matches: 0,
            lines_changed: lines,
            success: true,
            modified_content: None,
        })
    }

    /// Execute a batch of edit operations across multiple files.
    fn execute_batch(
        &self,
        action: &str,
        edits: Vec<EditSpec>,
        replace_all: bool,
        return_modified: bool,
        start: Instant,
    ) -> Result<ToolResult> {
        if edits.is_empty() {
            return Ok(ToolResult::failure(
                "edits array is empty — provide at least one edit operation",
                start.elapsed().as_millis() as u64,
            ));
        }

        let total = edits.len();
        let mut results = Vec::with_capacity(total);
        let mut success_count = 0usize;

        for edit in edits {
            let path = PathBuf::from(&edit.path);
            let entry = match action {
                "replace" => {
                    let old_content = match &edit.old_content {
                        Some(c) => c.as_str(),
                        None => {
                            results.push(BatchEditEntry {
                                status: "error".to_string(),
                                path: edit.path.clone(),
                                result: None,
                                error: Some("old_content required for replace action".to_string()),
                            });
                            continue;
                        }
                    };
                    let new_content = match &edit.new_content {
                        Some(c) => c.as_str(),
                        None => {
                            results.push(BatchEditEntry {
                                status: "error".to_string(),
                                path: edit.path.clone(),
                                result: None,
                                error: Some("new_content required for replace action".to_string()),
                            });
                            continue;
                        }
                    };
                    match self.replace_in_file(
                        &path,
                        old_content,
                        new_content,
                        replace_all,
                        return_modified,
                    ) {
                        Ok(r) => BatchEditEntry {
                            status: "ok".to_string(),
                            path: edit.path.clone(),
                            result: Some(r),
                            error: None,
                        },
                        Err(e) => BatchEditEntry {
                            status: "error".to_string(),
                            path: edit.path.clone(),
                            result: None,
                            error: Some(e.to_string()),
                        },
                    }
                }
                "create" => {
                    let new_content = match &edit.new_content {
                        Some(c) => c.as_str(),
                        None => {
                            results.push(BatchEditEntry {
                                status: "error".to_string(),
                                path: edit.path.clone(),
                                result: None,
                                error: Some("new_content required for create action".to_string()),
                            });
                            continue;
                        }
                    };
                    match self.create_file(&path, new_content, return_modified) {
                        Ok(r) => BatchEditEntry {
                            status: "ok".to_string(),
                            path: edit.path.clone(),
                            result: Some(r),
                            error: None,
                        },
                        Err(e) => BatchEditEntry {
                            status: "error".to_string(),
                            path: edit.path.clone(),
                            result: None,
                            error: Some(e.to_string()),
                        },
                    }
                }
                "delete" => match self.delete_file(&path) {
                    Ok(r) => BatchEditEntry {
                        status: "ok".to_string(),
                        path: edit.path.clone(),
                        result: Some(r),
                        error: None,
                    },
                    Err(e) => BatchEditEntry {
                        status: "error".to_string(),
                        path: edit.path.clone(),
                        result: None,
                        error: Some(e.to_string()),
                    },
                },
                _ => BatchEditEntry {
                    status: "error".to_string(),
                    path: edit.path.clone(),
                    result: None,
                    error: Some(format!("Unknown action: {}", action)),
                },
            };

            if entry.status == "ok" {
                success_count += 1;
            }
            results.push(entry);
        }

        let failed_count = total - success_count;
        let summary = if failed_count == 0 {
            format!("{} of {} files edited successfully", success_count, total)
        } else {
            format!(
                "{} of {} files edited successfully ({} failed)",
                success_count, total, failed_count
            )
        };

        let output = BatchEditOutput { results, summary };

        Ok(ToolResult::success(
            serde_json::to_value(output)?,
            start.elapsed().as_millis() as u64,
        ))
    }
}

/// Per-file edit specification for batch operations.
#[derive(Deserialize)]
struct EditSpec {
    path: String,
    #[serde(default)]
    old_content: Option<String>,
    #[serde(default)]
    new_content: Option<String>,
}

#[derive(Deserialize)]
struct CodeEditInput {
    action: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    old_content: Option<String>,
    #[serde(default)]
    new_content: Option<String>,
    #[serde(default)]
    replace_all: Option<bool>,
    #[serde(default)]
    return_modified: Option<bool>,
    /// Array of edit operations for batch multi-file editing.
    #[serde(default)]
    edits: Option<Vec<EditSpec>>,
}

#[derive(Serialize, Deserialize)]
struct EditResult {
    action: String,
    path: String,
    matches: usize,
    lines_changed: usize,
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    modified_content: Option<String>,
}

/// Per-file result entry in a batch edit response.
#[derive(Serialize)]
struct BatchEditEntry {
    status: String,
    path: String,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    result: Option<EditResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Output for batch edit operations.
#[derive(Serialize)]
struct BatchEditOutput {
    results: Vec<BatchEditEntry>,
    summary: String,
}

#[async_trait]
impl Tool for CodeEditTool {
    fn name(&self) -> &str {
        "code.edit"
    }

    fn description(&self) -> &str {
        "Precise surgical file editing with search-and-replace, create, and delete. Change only \
         the lines the task needs; leave unrelated code and formatting as they are."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["replace", "create", "delete"],
                    "description": "Edit action to perform"
                },
                "path": {
                    "type": "string",
                    "description": "File path (relative to workspace or absolute)"
                },
                "old_content": {
                    "type": "string",
                    "description": "Exact string to find and replace (required for replace"
                },
                "new_content": {
                    "type": "string",
                    "description": "Replacement string (required for replace and create actions)"
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace all occurrences (default: false, only first match)",
                    "default": false
                },
                "return_modified": {
                    "type": "boolean",
                    "description": "If true, returns the modified content in the response",
                    "default": false
                },
                "edits": {
                    "type": "array",
                    "description": "An array of edit specifications to apply across multiple",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "File path (relative to workspace or absolute)"
                            },
                            "old_content": {
                                "type": "string",
                                "description": "Exact string to find and replace (required for replace"
                            },
                            "new_content": {
                                "type": "string",
                                "description": "Replacement string (required for replace and create actions)"
                            }
                        },
                        "required": ["path"]
                    }
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();

        let input: CodeEditInput =
            serde_json::from_value(input).map_err(|e| eyre!("Invalid input: {}", e))?;

        // Batch mode: if edits array is provided, delegate to execute_batch
        if let Some(edits) = input.edits {
            let replace_all = input.replace_all.unwrap_or(false);
            let return_modified = input.return_modified.unwrap_or(false);
            return self.execute_batch(&input.action, edits, replace_all, return_modified, start);
        }

        // Single-file mode: path is required
        let path_str = match input.path {
            Some(p) => p,
            None => {
                return Ok(ToolResult::failure(
                    "Either 'path' or 'edits' must be provided",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };
        let path = PathBuf::from(&path_str);

        let result = match input.action.as_str() {
            "replace" => {
                let old_content = input
                    .old_content
                    .ok_or_else(|| eyre!("old_content required for replace action"))?;
                let new_content = input
                    .new_content
                    .ok_or_else(|| eyre!("new_content required for replace action"))?;
                let replace_all = input.replace_all.unwrap_or(false);
                let return_modified = input.return_modified.unwrap_or(false);

                match self.replace_in_file(
                    &path,
                    &old_content,
                    &new_content,
                    replace_all,
                    return_modified,
                ) {
                    Ok(result) => result,
                    Err(e) => {
                        return Ok(ToolResult::failure(
                            format!("Replace failed: {}", e),
                            start.elapsed().as_millis() as u64,
                        ));
                    }
                }
            }
            "create" => {
                let new_content = input
                    .new_content
                    .ok_or_else(|| eyre!("new_content required for create action"))?;
                let return_modified = input.return_modified.unwrap_or(false);

                match self.create_file(&path, &new_content, return_modified) {
                    Ok(result) => result,
                    Err(e) => {
                        return Ok(ToolResult::failure(
                            format!("Create failed: {}", e),
                            start.elapsed().as_millis() as u64,
                        ));
                    }
                }
            }
            "delete" => match self.delete_file(&path) {
                Ok(result) => result,
                Err(e) => {
                    return Ok(ToolResult::failure(
                        format!("Delete failed: {}", e),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            },
            _ => {
                return Ok(ToolResult::failure(
                    format!("Unknown action: {}", input.action),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        Ok(ToolResult::success(
            serde_json::to_value(result)?,
            start.elapsed().as_millis() as u64,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// A create through an agent-planted symlink with a missing intermediate
    /// directory used to fold `..` lexically, keep the symlink spelling, pass
    /// the prefix check and then write outside the workspace.
    #[tokio::test]
    #[cfg(unix)]
    async fn create_cannot_write_through_a_symlink_out_of_the_workspace() {
        let temp = TempDir::new().unwrap();
        let workspace = temp.path().canonicalize().unwrap();
        let outside = TempDir::new().unwrap();
        let outside = outside.path().canonicalize().unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("link")).unwrap();

        let tool = CodeEditTool::new(vec![workspace]);
        for path in ["link/newdir/escaped.rs", "link/direct.rs"] {
            let result = tool
                .execute(serde_json::json!({
                    "action": "create",
                    "path": path,
                    "new_content": "escaped",
                }))
                .await
                .unwrap();
            assert!(!result.success, "{path} was admitted");
        }
        assert!(!outside.join("newdir/escaped.rs").exists());
        assert!(!outside.join("direct.rs").exists());
    }

    /// Create resolves relative paths the same way every other action does,
    /// so `create` and `replace` never disagree about where a file lives.
    #[tokio::test]
    async fn create_writes_inside_the_workspace() {
        let temp = TempDir::new().unwrap();
        let workspace = temp.path().canonicalize().unwrap();
        let tool = CodeEditTool::new(vec![workspace.clone()]);

        let result = tool
            .execute(serde_json::json!({
                "action": "create",
                "path": "nested/dir/new.rs",
                "new_content": "fn main() {}",
            }))
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);
        let written = workspace.join("nested/dir/new.rs");
        assert_eq!(std::fs::read_to_string(&written).unwrap(), "fn main() {}");
        assert_eq!(result.output["path"], written.display().to_string());
    }

    #[tokio::test]
    async fn the_agents_configuration_is_not_edited_created_or_deleted() {
        let temp = TempDir::new().unwrap();
        let config = temp.path().join("config.toml");
        fs::write(&config, "[access]\nmode = \"allowlist\"\n").unwrap();
        let tool = CodeEditTool::new(vec![temp.path().to_path_buf()]);

        for input in [
            serde_json::json!({
                "action": "replace",
                "path": "config.toml",
                "old_content": "allowlist",
                "new_content": "open",
            }),
            serde_json::json!({ "action": "delete", "path": "config.toml" }),
            serde_json::json!({
                "action": "create",
                "path": "heartbeats/hourly.md",
                "new_content": "every: 1h",
            }),
            serde_json::json!({
                "action": "create",
                "path": "WELCOME.md",
                "new_content": "hi",
            }),
        ] {
            let result = tool.execute(input.clone()).await.unwrap();
            assert!(!result.success, "{input}");
            assert!(
                result
                    .error
                    .as_deref()
                    .is_some_and(|e| e.contains(workforce_common::config_files::WRITE_REFUSED)),
                "{input}: {:?}",
                result.error
            );
        }
        assert!(fs::read_to_string(&config).unwrap().contains("allowlist"));
        assert!(!temp.path().join("heartbeats").exists());
        assert!(!temp.path().join("WELCOME.md").exists());
    }

    #[tokio::test]
    async fn test_replace_single_occurrence() {
        let temp = TempDir::new().unwrap();
        let test_file = temp.path().join("test.txt");
        fs::write(&test_file, "hello world\nhello again").unwrap();

        let tool = CodeEditTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "action": "replace",
            "path": "test.txt",
            "old_content": "hello",
            "new_content": "hi"
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let content = fs::read_to_string(&test_file).unwrap();
        assert_eq!(content, "hi world\nhello again"); // Only first occurrence replaced
    }

    #[tokio::test]
    async fn test_replace_all_occurrences() {
        let temp = TempDir::new().unwrap();
        let test_file = temp.path().join("test.txt");
        fs::write(&test_file, "hello world\nhello again").unwrap();

        let tool = CodeEditTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "action": "replace",
            "path": "test.txt",
            "old_content": "hello",
            "new_content": "hi",
            "replace_all": true
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let content = fs::read_to_string(&test_file).unwrap();
        assert_eq!(content, "hi world\nhi again"); // Both occurrences replaced
    }

    #[tokio::test]
    async fn test_create_file() {
        let temp = TempDir::new().unwrap();
        let tool = CodeEditTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "action": "create",
            "path": "new_file.txt",
            "new_content": "Hello, world!"
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let new_file = temp.path().join("new_file.txt");
        assert!(new_file.exists());
        assert_eq!(fs::read_to_string(new_file).unwrap(), "Hello, world!");
    }

    #[tokio::test]
    async fn test_create_file_with_subdirectory() {
        let temp = TempDir::new().unwrap();
        let tool = CodeEditTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "action": "create",
            "path": "subdir/new_file.txt",
            "new_content": "Test content"
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let new_file = temp.path().join("subdir/new_file.txt");
        assert!(new_file.exists());
    }

    #[tokio::test]
    async fn test_delete_file() {
        let temp = TempDir::new().unwrap();
        let test_file = temp.path().join("delete_me.txt");
        fs::write(&test_file, "temporary content").unwrap();

        let tool = CodeEditTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "action": "delete",
            "path": "delete_me.txt"
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        assert!(!test_file.exists());
    }

    #[tokio::test]
    async fn test_replace_string_not_found() {
        let temp = TempDir::new().unwrap();
        let test_file = temp.path().join("test.txt");
        fs::write(&test_file, "hello world").unwrap();

        let tool = CodeEditTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "action": "replace",
            "path": "test.txt",
            "old_content": "goodbye",
            "new_content": "hi"
        });

        let result = tool.execute(input).await.unwrap();
        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_create_outside_workspace() {
        let temp = TempDir::new().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).unwrap();

        let tool = CodeEditTool::new(vec![workspace]);

        let input = serde_json::json!({
            "action": "create",
            "path": "../outside.txt",
            "new_content": "Should fail"
        });

        let result = tool.execute(input).await.unwrap();
        assert!(!result.success);
    }
}

// Additional tests for return_modified feature
#[cfg(test)]
mod return_modified_tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_replace_with_return_modified() {
        let temp = TempDir::new().unwrap();
        let test_file = temp.path().join("test.txt");
        fs::write(
            &test_file,
            "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10",
        )
        .unwrap();

        let tool = CodeEditTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "action": "replace",
            "path": "test.txt",
            "old_content": "line5",
            "new_content": "MODIFIED",
            "return_modified": true
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: serde_json::Value = result.output;
        assert!(output["modified_content"].is_string());
        let modified = output["modified_content"].as_str().unwrap();
        assert!(modified.contains("MODIFIED"));
    }

    #[tokio::test]
    async fn test_replace_without_return_modified() {
        let temp = TempDir::new().unwrap();
        let test_file = temp.path().join("test.txt");
        fs::write(&test_file, "hello world").unwrap();

        let tool = CodeEditTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "action": "replace",
            "path": "test.txt",
            "old_content": "hello",
            "new_content": "hi"
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: serde_json::Value = result.output;
        assert!(output.get("modified_content").is_none() || output["modified_content"].is_null());
    }

    #[tokio::test]
    async fn test_create_with_return_modified() {
        let temp = TempDir::new().unwrap();
        let tool = CodeEditTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "action": "create",
            "path": "new_file.txt",
            "new_content": "Hello, world!\nSecond line.",
            "return_modified": true
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: serde_json::Value = result.output;
        let modified = output["modified_content"].as_str().unwrap();
        assert_eq!(modified, "Hello, world!\nSecond line.");
    }
}

#[cfg(test)]
mod batch_edit_tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_batch_replace_multiple_files() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("a.txt"), "hello world").unwrap();
        fs::write(temp.path().join("b.txt"), "hello there").unwrap();

        let tool = CodeEditTool::new(vec![temp.path().to_path_buf()]);
        let input = serde_json::json!({
            "action": "replace",
            "edits": [
                { "path": "a.txt", "old_content": "hello", "new_content": "hi" },
                { "path": "b.txt", "old_content": "hello", "new_content": "hi" }
            ]
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: serde_json::Value = result.output;
        assert_eq!(output["summary"], "2 of 2 files edited successfully");
        assert_eq!(output["results"].as_array().unwrap().len(), 2);
        assert_eq!(output["results"][0]["status"], "ok");
        assert_eq!(output["results"][1]["status"], "ok");

        assert_eq!(
            fs::read_to_string(temp.path().join("a.txt")).unwrap(),
            "hi world"
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("b.txt")).unwrap(),
            "hi there"
        );
    }

    #[tokio::test]
    async fn test_batch_replace_partial_failure() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("a.txt"), "hello world").unwrap();
        // b.txt does not exist — will fail for replace

        let tool = CodeEditTool::new(vec![temp.path().to_path_buf()]);
        let input = serde_json::json!({
            "action": "replace",
            "edits": [
                { "path": "a.txt", "old_content": "hello", "new_content": "hi" },
                { "path": "b.txt", "old_content": "foo", "new_content": "bar" }
            ]
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success); // batch itself succeeds even with partial failure

        let output: serde_json::Value = result.output;
        assert_eq!(
            output["summary"],
            "1 of 2 files edited successfully (1 failed)"
        );
        assert_eq!(output["results"][0]["status"], "ok");
        assert_eq!(output["results"][1]["status"], "error");
        assert!(!output["results"][1]["error"].as_str().unwrap().is_empty());

        // First file was still edited
        assert_eq!(
            fs::read_to_string(temp.path().join("a.txt")).unwrap(),
            "hi world"
        );
    }

    #[tokio::test]
    async fn test_batch_replace_with_replace_all() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("a.txt"), "foo bar foo baz foo").unwrap();

        let tool = CodeEditTool::new(vec![temp.path().to_path_buf()]);
        let input = serde_json::json!({
            "action": "replace",
            "replace_all": true,
            "edits": [
                { "path": "a.txt", "old_content": "foo", "new_content": "qux" }
            ]
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: serde_json::Value = result.output;
        assert_eq!(output["results"][0]["status"], "ok");
        assert_eq!(output["results"][0]["matches"], 3);
        assert_eq!(
            fs::read_to_string(temp.path().join("a.txt")).unwrap(),
            "qux bar qux baz qux"
        );
    }

    #[tokio::test]
    async fn test_batch_replace_with_return_modified() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("a.txt"), "line1\nline2\nline3\nold\nline5").unwrap();

        let tool = CodeEditTool::new(vec![temp.path().to_path_buf()]);
        let input = serde_json::json!({
            "action": "replace",
            "return_modified": true,
            "edits": [
                { "path": "a.txt", "old_content": "old", "new_content": "new" }
            ]
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: serde_json::Value = result.output;
        assert_eq!(output["results"][0]["status"], "ok");
        assert!(output["results"][0]["modified_content"].is_string());
        let modified = output["results"][0]["modified_content"].as_str().unwrap();
        assert!(modified.contains("new"));
    }

    #[tokio::test]
    async fn test_batch_create_multiple_files() {
        let temp = TempDir::new().unwrap();
        let tool = CodeEditTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "action": "create",
            "edits": [
                { "path": "file1.txt", "new_content": "content1" },
                { "path": "file2.txt", "new_content": "content2" }
            ]
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: serde_json::Value = result.output;
        assert_eq!(output["summary"], "2 of 2 files edited successfully");

        assert_eq!(
            fs::read_to_string(temp.path().join("file1.txt")).unwrap(),
            "content1"
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("file2.txt")).unwrap(),
            "content2"
        );
    }

    #[tokio::test]
    async fn test_batch_delete_multiple_files() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("a.txt"), "delete me").unwrap();
        fs::write(temp.path().join("b.txt"), "delete me too").unwrap();

        let tool = CodeEditTool::new(vec![temp.path().to_path_buf()]);
        let input = serde_json::json!({
            "action": "delete",
            "edits": [
                { "path": "a.txt" },
                { "path": "b.txt" }
            ]
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: serde_json::Value = result.output;
        assert_eq!(output["summary"], "2 of 2 files edited successfully");
        assert!(!temp.path().join("a.txt").exists());
        assert!(!temp.path().join("b.txt").exists());
    }

    #[tokio::test]
    async fn test_batch_empty_edits_fails() {
        let temp = TempDir::new().unwrap();
        let tool = CodeEditTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "action": "replace",
            "edits": []
        });

        let result = tool.execute(input).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("empty"));
    }

    #[tokio::test]
    async fn test_single_edit_still_works() {
        // Backward compatibility: single-file mode with path works exactly as before
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("test.txt"), "hello world").unwrap();

        let tool = CodeEditTool::new(vec![temp.path().to_path_buf()]);
        let input = serde_json::json!({
            "action": "replace",
            "path": "test.txt",
            "old_content": "hello",
            "new_content": "hi"
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        // Output should be a flat EditResult, not a BatchEditOutput
        let output: serde_json::Value = result.output;
        assert_eq!(output["action"], "replace");
        assert_eq!(output["matches"], 1);
        assert!(output.get("results").is_none()); // no batch wrapper

        assert_eq!(
            fs::read_to_string(temp.path().join("test.txt")).unwrap(),
            "hi world"
        );
    }

    #[tokio::test]
    async fn test_edits_takes_precedence_over_path() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("a.txt"), "hello").unwrap();
        fs::write(temp.path().join("b.txt"), "hello").unwrap();

        let tool = CodeEditTool::new(vec![temp.path().to_path_buf()]);

        // Both path and edits provided — edits should take precedence
        let input = serde_json::json!({
            "action": "replace",
            "path": "a.txt",
            "old_content": "hello",
            "new_content": "SHOULD_NOT_USE_THIS",
            "edits": [
                { "path": "b.txt", "old_content": "hello", "new_content": "hi" }
            ]
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: serde_json::Value = result.output;
        // Should have batch output shape
        assert!(output.get("results").is_some());
        assert_eq!(output["summary"], "1 of 1 files edited successfully");

        // a.txt should be unchanged (single-file path was ignored)
        assert_eq!(
            fs::read_to_string(temp.path().join("a.txt")).unwrap(),
            "hello"
        );
        // b.txt should be edited via batch
        assert_eq!(fs::read_to_string(temp.path().join("b.txt")).unwrap(), "hi");
    }

    #[tokio::test]
    async fn test_no_path_no_edits_fails() {
        let temp = TempDir::new().unwrap();
        let tool = CodeEditTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "action": "replace",
            "old_content": "hello",
            "new_content": "hi"
        });

        let result = tool.execute(input).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("path"));
    }
}
