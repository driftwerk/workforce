//! code.find - Find files by name or glob pattern.

use async_trait::async_trait;
use eyre::{eyre, Result};
use glob::glob;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Instant;
use workforce_agent::tools::{Tool, ToolResult};

const DEFAULT_MAX_RESULTS: usize = 50;

/// Directories to skip when listing results.
const IGNORED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "__pycache__",
    ".venv",
    "dist",
    ".next",
    "build",
];

/// code.find tool - Find files by name or glob pattern.
pub struct CodeFindTool {
    allowed_paths: Vec<PathBuf>,
}

impl CodeFindTool {
    pub fn new(allowed_paths: Vec<PathBuf>) -> Self {
        Self { allowed_paths }
    }

    fn find_files(
        &self,
        pattern: &str,
        search_root: &Path,
        max_results: usize,
    ) -> Result<FindOutput> {
        // Build the full glob pattern anchored at search_root.
        // If pattern already starts with **, use it as-is; otherwise prepend **/ so
        // it matches at any depth.
        let full_pattern = if pattern.contains('/') || pattern.starts_with("**") {
            format!("{}/{}", search_root.display(), pattern)
        } else {
            format!("{}/**/{}", search_root.display(), pattern)
        };

        let entries = glob(&full_pattern).map_err(|e| eyre!("Invalid glob pattern: {}", e))?;

        let mut results: Vec<FileEntry> = Vec::new();
        let search_root_canonical = search_root
            .canonicalize()
            .unwrap_or_else(|_| search_root.to_path_buf());

        for entry in entries {
            let path = match entry {
                Ok(p) => p,
                Err(_) => continue,
            };

            // Skip directories themselves (we want files only)
            if path.is_dir() || workforce_agent::tools::paths::is_withheld(&path) {
                continue;
            }

            // Skip ignored directories
            if path.components().any(|c| {
                IGNORED_DIRS
                    .iter()
                    .any(|&d| c.as_os_str() == std::ffi::OsStr::new(d))
            }) {
                continue;
            }

            // Make path relative to search root
            let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
            let relative = canonical
                .strip_prefix(&search_root_canonical)
                .unwrap_or(&canonical)
                .to_string_lossy()
                .to_string();

            // Get metadata
            let (size, modified) = if let Ok(meta) = std::fs::metadata(&path) {
                let size = meta.len();
                let modified = meta.modified().ok().and_then(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .ok()
                        .map(|d| d.as_secs())
                });
                (size, modified)
            } else {
                (0, None)
            };

            results.push(FileEntry {
                path: relative,
                size,
                modified_epoch: modified,
            });

            if results.len() >= max_results {
                break;
            }
        }

        // Sort by path for consistent output
        results.sort_by(|a, b| a.path.cmp(&b.path));

        let total = results.len();
        Ok(FindOutput {
            files: results,
            total,
            truncated: total >= max_results,
        })
    }
}

#[derive(Deserialize)]
struct CodeFindInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default = "default_max_results")]
    max_results: usize,
}

fn default_max_results() -> usize {
    DEFAULT_MAX_RESULTS
}

#[derive(Serialize, Deserialize, Debug)]
struct FileEntry {
    path: String,
    size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    modified_epoch: Option<u64>,
}

#[derive(Serialize, Deserialize)]
struct FindOutput {
    files: Vec<FileEntry>,
    total: usize,
    truncated: bool,
}

#[async_trait]
impl Tool for CodeFindTool {
    fn name(&self) -> &str {
        "code.find"
    }

    fn description(&self) -> &str {
        "Find files by name or glob pattern"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern for file names"
                },
                "path": {
                    "type": "string",
                    "description": "Subdirectory to search within (default: workspace root)"
                },
                "repo": {
                    "type": "string",
                    "description": super::utils::REPO_PARAM
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum number of results (default: 50)",
                    "minimum": 1,
                    "default": DEFAULT_MAX_RESULTS
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();

        let input: CodeFindInput =
            serde_json::from_value(input).map_err(|e| eyre!("Invalid input: {}", e))?;

        // Determine search root
        let search_root = if let Some(ref repo) = input.repo {
            let base = self
                .allowed_paths
                .first()
                .ok_or_else(|| eyre!("No workspace configured"))?;
            match super::utils::resolve_repo_path(repo, base) {
                Ok(p) => p,
                Err(e) => {
                    return Ok(ToolResult::failure(
                        format!("Find failed: {}", e),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        } else if let Some(ref subpath) = input.path {
            let base = self
                .allowed_paths
                .first()
                .ok_or_else(|| eyre!("No workspace configured"))?;
            let full = base.join(subpath);
            if !full.exists() {
                return Ok(ToolResult::failure(
                    format!("Path not found: {}", subpath),
                    start.elapsed().as_millis() as u64,
                ));
            }
            full
        } else {
            self.allowed_paths
                .first()
                .ok_or_else(|| eyre!("No workspace configured"))?
                .clone()
        };

        match self.find_files(&input.pattern, &search_root, input.max_results) {
            Ok(output) => Ok(ToolResult::success(
                serde_json::to_value(output)?,
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Find failed: {}", e),
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
    async fn test_find_by_extension() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("main.rs"), "fn main() {}").unwrap();
        fs::write(temp.path().join("lib.rs"), "pub mod lib;").unwrap();
        fs::write(temp.path().join("readme.md"), "# Hello").unwrap();

        let tool = CodeFindTool::new(vec![temp.path().to_path_buf()]);
        let result = tool
            .execute(serde_json::json!({ "pattern": "*.rs" }))
            .await
            .unwrap();

        assert!(result.success);
        let output: FindOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.total, 2);
        assert!(output.files.iter().all(|f| f.path.ends_with(".rs")));
    }

    #[tokio::test]
    async fn test_find_by_exact_name() {
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join("sub")).unwrap();
        fs::write(temp.path().join("Cargo.toml"), "[package]").unwrap();
        fs::write(temp.path().join("sub/Cargo.toml"), "[package]").unwrap();

        let tool = CodeFindTool::new(vec![temp.path().to_path_buf()]);
        let result = tool
            .execute(serde_json::json!({ "pattern": "Cargo.toml" }))
            .await
            .unwrap();

        assert!(result.success);
        let output: FindOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.total, 2);
    }

    #[tokio::test]
    async fn test_find_skips_ignored_dirs() {
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join("src")).unwrap();
        fs::create_dir_all(temp.path().join("node_modules/dep")).unwrap();
        fs::write(temp.path().join("src/app.js"), "const x = 1;").unwrap();
        fs::write(
            temp.path().join("node_modules/dep/index.js"),
            "module.exports = {};",
        )
        .unwrap();

        let tool = CodeFindTool::new(vec![temp.path().to_path_buf()]);
        let result = tool
            .execute(serde_json::json!({ "pattern": "*.js" }))
            .await
            .unwrap();

        assert!(result.success);
        let output: FindOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.total, 1);
        assert!(output.files[0].path.contains("app.js"));
    }

    #[tokio::test]
    async fn test_find_no_matches() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("test.txt"), "hello").unwrap();

        let tool = CodeFindTool::new(vec![temp.path().to_path_buf()]);
        let result = tool
            .execute(serde_json::json!({ "pattern": "*.xyz" }))
            .await
            .unwrap();

        assert!(result.success);
        let output: FindOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.total, 0);
        assert!(!output.truncated);
    }

    #[tokio::test]
    async fn test_find_max_results() {
        let temp = TempDir::new().unwrap();
        for i in 0..10 {
            fs::write(temp.path().join(format!("file_{}.txt", i)), "content").unwrap();
        }

        let tool = CodeFindTool::new(vec![temp.path().to_path_buf()]);
        let result = tool
            .execute(serde_json::json!({ "pattern": "*.txt", "max_results": 3 }))
            .await
            .unwrap();

        assert!(result.success);
        let output: FindOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.total, 3);
        assert!(output.truncated);
    }

    #[tokio::test]
    async fn test_find_in_subdirectory() {
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join("src/models")).unwrap();
        fs::write(temp.path().join("src/models/user.rs"), "struct User {}").unwrap();
        fs::write(temp.path().join("src/main.rs"), "fn main() {}").unwrap();

        let tool = CodeFindTool::new(vec![temp.path().to_path_buf()]);
        let result = tool
            .execute(serde_json::json!({ "pattern": "*.rs", "path": "src/models" }))
            .await
            .unwrap();

        assert!(result.success);
        let output: FindOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.total, 1);
        assert!(output.files[0].path.contains("user.rs"));
    }
}
