//! code.read - Read file(s) with line numbers, ranges, and glob pattern support.

use async_trait::async_trait;
use eyre::{eyre, Result};
use glob::glob;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Instant;
use workforce_agent::tools::{Tool, ToolResult};

use super::utils::validate_path;

const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024; // 10 MB
const MAX_FILES_PER_GLOB: usize = 50;
const MAX_TOTAL_LINES: usize = 10_000;

/// code.read tool - Read files with line numbers and optional range filtering.
pub struct CodeReadTool {
    allowed_paths: Vec<PathBuf>,
}

impl CodeReadTool {
    pub fn new(allowed_paths: Vec<PathBuf>) -> Self {
        Self { allowed_paths }
    }

    /// Read a single file with optional line range.
    fn read_file(
        &self,
        path: &std::path::Path,
        start_line: Option<usize>,
        end_line: Option<usize>,
        context_lines: usize,
    ) -> Result<FileContent> {
        // Validate path
        let validated_path = validate_path(path, &self.allowed_paths)?;

        // Check file size
        let metadata = std::fs::metadata(&validated_path)?;
        if metadata.len() > MAX_FILE_SIZE {
            return Err(eyre!(
                "File too large: {} bytes (max: {} bytes)",
                metadata.len(),
                MAX_FILE_SIZE
            ));
        }

        // Read file content
        let content = std::fs::read_to_string(&validated_path)?;
        let all_lines: Vec<&str> = content.lines().collect();
        let total_lines = all_lines.len();

        // Determine line range
        let (start, end) = match (start_line, end_line) {
            (Some(s), Some(e)) => {
                // Add context lines
                let start_with_context = s.saturating_sub(context_lines).max(1);
                let end_with_context = (e + context_lines).min(total_lines);
                (start_with_context, end_with_context)
            }
            (Some(s), None) => {
                // Just start line with context
                let start_with_context = s.saturating_sub(context_lines).max(1);
                let end_with_context = (s + context_lines).min(total_lines);
                (start_with_context, end_with_context)
            }
            _ => (1, total_lines), // Read entire file
        };

        // Extract lines (convert from 1-indexed to 0-indexed)
        let selected_lines: Vec<String> = all_lines
            .iter()
            .enumerate()
            .skip(start.saturating_sub(1))
            .take(end.saturating_sub(start) + 1)
            .map(|(idx, line)| format!("{}: {}", idx + 1, line))
            .collect();

        Ok(FileContent {
            path: validated_path.display().to_string(),
            total_lines,
            start_line: start,
            end_line: end,
            content: selected_lines.join("\n"),
        })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct PathSpec {
    path: String,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
}

#[derive(Deserialize)]
struct CodeReadInput {
    // One of path or paths must be provided. paths takes precedence.
    path: Option<String>,
    #[serde(default)]
    paths: Option<Vec<PathSpec>>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    start_line: Option<usize>, // Used if path is provided, ignored for paths
    #[serde(default)]
    end_line: Option<usize>, // Used if path is provided, ignored for paths
    #[serde(default)]
    context_lines: Option<usize>,
}

#[derive(Serialize, Deserialize)]
struct FileContent {
    path: String,
    total_lines: usize,
    start_line: usize,
    end_line: usize,
    content: String,
}

#[derive(Serialize, Deserialize)]
struct CodeReadOutput {
    files: Vec<FileContent>,
    total_files: usize,
    total_lines: usize,
}

#[async_trait]
impl Tool for CodeReadTool {
    fn name(&self) -> &str {
        "code.read"
    }

    fn description(&self) -> &str {
        "Read file(s) with line numbers and optional ranges"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path or glob pattern"
                },
                "paths": {
                    "type": "array",
                    "description": "An array of path specifications to read multiple files or",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "File path or glob pattern."
                            },
                            "start_line": {
                                "type": "integer",
                                "description": "Start line number (1-indexed, optional).",
                                "minimum": 1
                            },
                            "end_line": {
                                "type": "integer",
                                "description": "End line number (inclusive, optional).",
                                "minimum": 1
                            }
                        },
                        "required": ["path"]
                    }
                },
                "start_line": {
                    "type": "integer",
                    "description": "Start line number (1-indexed, optional)",
                    "minimum": 1
                },
                "end_line": {
                    "type": "integer",
                    "description": "End line number (inclusive, optional)",
                    "minimum": 1
                },
                "context_lines": {
                    "type": "integer",
                    "description": "Lines of context before/after range (default: 0)",
                    "minimum": 0,
                    "default": 0
                },
                "repo": {
                    "type": "string",
                    "description": super::utils::REPO_PARAM
                }
            },
            "required": [],
            "description": "Read file(s) with line numbers"
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();

        let input: CodeReadInput =
            serde_json::from_value(input).map_err(|e| eyre!("Invalid input: {}", e))?;

        let context_lines = input.context_lines.unwrap_or(0);

        // Resolve repo base path if provided
        let repo_base = if let Some(ref repo) = input.repo {
            let base = self
                .allowed_paths
                .first()
                .ok_or_else(|| eyre!("No workspace configured"))?;
            match super::utils::resolve_repo_path(repo, base) {
                Ok(p) => Some(p),
                Err(e) => {
                    return Ok(ToolResult::failure(
                        format!("Failed to read file: {}", e),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        } else {
            None
        };

        let path_specs = match (input.paths, input.path) {
            (Some(paths), _) => {
                if paths.is_empty() {
                    return Ok(ToolResult::failure(
                        "The 'paths' array cannot be empty.",
                        start.elapsed().as_millis() as u64,
                    ));
                }
                paths
            }
            (None, Some(path)) => {
                vec![PathSpec {
                    path,
                    start_line: input.start_line,
                    end_line: input.end_line,
                }]
            }
            (None, None) => {
                return Ok(ToolResult::failure(
                    "Either 'path' or 'paths' must be provided.",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let mut file_contents = Vec::new();
        let mut total_lines_read = 0;

        for spec in path_specs {
            if total_lines_read >= MAX_TOTAL_LINES {
                break;
            }

            let has_glob = spec.path.contains('*') || spec.path.contains('?');
            if has_glob {
                let workspace = repo_base.as_ref().unwrap_or_else(|| {
                    self.allowed_paths.first().expect("No workspace configured")
                });

                let pattern_path = workspace.join(&spec.path);
                let pattern = pattern_path
                    .to_str()
                    .ok_or_else(|| eyre!("Invalid path pattern"))?;

                let mut matched_files = 0;
                for entry in glob(pattern).map_err(|e| eyre!("Invalid glob pattern: {}", e))? {
                    if matched_files >= MAX_FILES_PER_GLOB || total_lines_read >= MAX_TOTAL_LINES {
                        break;
                    }
                    match entry {
                        Ok(path) => {
                            if path.is_file() && !workforce_agent::tools::paths::is_withheld(&path)
                            {
                                let remaining_lines = MAX_TOTAL_LINES - total_lines_read;
                                let mut spec_clone = spec.clone();

                                if spec_clone.end_line.is_none()
                                    || spec_clone.end_line.unwrap() > remaining_lines
                                {
                                    spec_clone.end_line = Some(remaining_lines);
                                }

                                match self.read_file(
                                    &path,
                                    spec_clone.start_line,
                                    spec_clone.end_line,
                                    context_lines,
                                ) {
                                    Ok(content) => {
                                        let lines_in_content =
                                            content.end_line.saturating_sub(content.start_line) + 1;
                                        if total_lines_read + lines_in_content > MAX_TOTAL_LINES {
                                            // This should be handled by the end_line adjustment, but as a safeguard
                                            continue;
                                        }
                                        total_lines_read += lines_in_content;
                                        file_contents.push(content);
                                    }
                                    Err(e) => {
                                        tracing::warn!("Failed to read {}: {}", path.display(), e);
                                    }
                                }
                                matched_files += 1;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Glob error: {}", e);
                        }
                    }
                }
            } else {
                let path = if let Some(ref base) = repo_base {
                    base.join(&spec.path)
                } else {
                    PathBuf::from(&spec.path)
                };
                let remaining_lines = MAX_TOTAL_LINES - total_lines_read;
                let mut spec_clone = spec.clone();

                if spec_clone.end_line.is_none() || spec_clone.end_line.unwrap() > remaining_lines {
                    spec_clone.end_line = Some(remaining_lines);
                }
                match self.read_file(
                    &path,
                    spec_clone.start_line,
                    spec_clone.end_line,
                    context_lines,
                ) {
                    Ok(content) => {
                        let lines_in_content =
                            content.end_line.saturating_sub(content.start_line) + 1;
                        if total_lines_read + lines_in_content <= MAX_TOTAL_LINES {
                            total_lines_read += lines_in_content;
                            file_contents.push(content);
                        }
                    }
                    Err(e) => {
                        return Ok(ToolResult::failure(
                            format!("Failed to read file '{}': {}", spec.path, e),
                            start.elapsed().as_millis() as u64,
                        ));
                    }
                }
            }
        }

        if file_contents.is_empty() {
            return Ok(ToolResult::failure(
                "No files could be read.",
                start.elapsed().as_millis() as u64,
            ));
        }

        let output = CodeReadOutput {
            total_files: file_contents.len(),
            total_lines: total_lines_read,
            files: file_contents,
        };

        Ok(ToolResult::success(
            serde_json::to_value(output)?,
            start.elapsed().as_millis() as u64,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_read_single_file() {
        let temp = TempDir::new().unwrap();
        let test_file = temp.path().join("test.txt");
        fs::write(&test_file, "line1\nline2\nline3\n").unwrap();

        let tool = CodeReadTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "path": "test.txt"
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: CodeReadOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.total_files, 1);
        assert_eq!(output.files[0].total_lines, 3);
    }

    #[tokio::test]
    async fn test_read_with_line_range() {
        let temp = TempDir::new().unwrap();
        let test_file = temp.path().join("test.txt");
        fs::write(&test_file, "line1\nline2\nline3\nline4\nline5\n").unwrap();

        let tool = CodeReadTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "path": "test.txt",
            "start_line": 2,
            "end_line": 4
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: CodeReadOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.files[0].start_line, 2);
        assert_eq!(output.files[0].end_line, 4);
        assert!(output.files[0].content.contains("2: line2"));
        assert!(output.files[0].content.contains("3: line3"));
        assert!(output.files[0].content.contains("4: line4"));
    }

    #[tokio::test]
    async fn test_read_with_context() {
        let temp = TempDir::new().unwrap();
        let test_file = temp.path().join("test.txt");
        fs::write(&test_file, "line1\nline2\nline3\nline4\nline5\n").unwrap();

        let tool = CodeReadTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "path": "test.txt",
            "start_line": 3,
            "end_line": 3,
            "context_lines": 1
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: CodeReadOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.files[0].start_line, 2); // 3 - 1 context
        assert_eq!(output.files[0].end_line, 4); // 3 + 1 context
    }

    #[tokio::test]
    async fn test_read_outside_workspace() {
        let temp = TempDir::new().unwrap();
        let allowed = temp.path().join("workspace");
        fs::create_dir(&allowed).unwrap();

        let outside_file = temp.path().join("outside.txt");
        fs::write(&outside_file, "secret").unwrap();

        let tool = CodeReadTool::new(vec![allowed]);

        let input = serde_json::json!({
            "path": "../outside.txt"
        });

        let result = tool.execute(input).await.unwrap();
        assert!(!result.success);
    }
}

// Additional tests for multi-path feature
#[cfg(test)]
mod multi_path_tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_read_multiple_paths() {
        let temp = TempDir::new().unwrap();
        let file1 = temp.path().join("file1.txt");
        let file2 = temp.path().join("file2.txt");
        fs::write(&file1, "content1").unwrap();
        fs::write(&file2, "content2").unwrap();

        let tool = CodeReadTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "paths": [
                { "path": "file1.txt" },
                { "path": "file2.txt" }
            ]
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: CodeReadOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.total_files, 2);
        assert_eq!(output.files[0].content, "1: content1");
        assert_eq!(output.files[1].content, "1: content2");
    }

    #[tokio::test]
    async fn test_read_multiple_paths_with_ranges() {
        let temp = TempDir::new().unwrap();
        let file1 = temp.path().join("file1.txt");
        let file2 = temp.path().join("file2.txt");
        fs::write(&file1, "l1\nl2\nl3").unwrap();
        fs::write(&file2, "a1\na2\na3").unwrap();

        let tool = CodeReadTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "paths": [
                { "path": "file1.txt", "start_line": 2, "end_line": 2 },
                { "path": "file2.txt", "start_line": 3 }
            ]
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: CodeReadOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.total_files, 2);
        assert_eq!(output.files[0].content, "2: l2");
        assert_eq!(output.files[1].content, "3: a3");
    }

    #[tokio::test]
    async fn test_read_paths_respects_limits() {
        let temp = TempDir::new().unwrap();
        for i in 0..10 {
            let file = temp.path().join(format!("file{}.txt", i));
            let content = (0..1500)
                .map(|n| format!("line {}", n))
                .collect::<Vec<_>>()
                .join("\n");
            fs::write(&file, content).unwrap();
        }

        let tool = CodeReadTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({ "paths": [ { "path": "file*.txt" } ] });
        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
        let output: CodeReadOutput = serde_json::from_value(result.output).unwrap();

        // MAX_TOTAL_LINES is 10_000, each file is 1500 lines.
        // Should read at least 6 full files (9000 lines), possibly with a partial 7th.
        assert!(
            output.total_lines >= 9000 && output.total_lines <= 10000,
            "Total lines out of range: {}",
            output.total_lines
        );
        assert!(
            output.total_files >= 6,
            "Expected at least 6 files read, got: {}",
            output.total_files
        );
    }

    #[tokio::test]
    async fn test_read_single_path_still_works() {
        let temp = TempDir::new().unwrap();
        let test_file = temp.path().join("test.txt");
        fs::write(&test_file, "hello").unwrap();

        let tool = CodeReadTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({ "path": "test.txt" });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);
        let output: CodeReadOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.total_files, 1);
        assert_eq!(output.files[0].content, "1: hello");
    }
}
