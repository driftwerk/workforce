//! code.find_replace — Project-wide find and replace using ripgrep + Rust string/regex replacement.

use async_trait::async_trait;
use eyre::{eyre, Result};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Instant;
use tokio::process::Command;
use workforce_agent::tools::config_guard;
use workforce_agent::tools::paths::CanonicalRoots;
use workforce_agent::tools::{Tool, ToolResult};

use super::utils::validate_path;

const DEFAULT_MAX_FILES: usize = 50;
const MAX_PREVIEW_PER_FILE: usize = 5;

/// code.find_replace tool — Project-wide find and replace.
pub struct CodeFindReplaceTool {
    allowed_paths: Vec<PathBuf>,
}

impl CodeFindReplaceTool {
    pub fn new(allowed_paths: Vec<PathBuf>) -> Self {
        Self { allowed_paths }
    }

    /// Use ripgrep to discover files matching the pattern.
    /// Returns a list of absolute file paths that contain at least one match.
    async fn discover_files(
        &self,
        pattern: &str,
        search_path: &std::path::Path,
        file_type: Option<&str>,
        case_sensitive: bool,
        exclude: &[String],
        max_files: usize,
        is_regex: bool,
    ) -> Result<(Vec<PathBuf>, bool)> {
        // Check if ripgrep is installed
        let rg_check = Command::new("rg").arg("--version").output().await;
        if rg_check.is_err() {
            return Err(eyre!(
                "ripgrep (rg) not found. Please install ripgrep: https://github.com/BurntSushi/ripgrep"
            ));
        }

        let mut cmd = Command::new("rg");
        cmd.arg("--files-with-matches").arg("--no-messages");

        if !is_regex {
            cmd.arg("--fixed-strings");
        }

        if !case_sensitive {
            cmd.arg("--ignore-case");
        }

        if let Some(ft) = file_type {
            cmd.arg("--type").arg(ft);
        }

        for pattern_glob in exclude {
            cmd.arg("--glob").arg(format!("!{}", pattern_glob));
        }

        cmd.arg(pattern).arg(search_path);

        let output = cmd.output().await?;
        let stdout = String::from_utf8_lossy(&output.stdout);

        let mut files: Vec<PathBuf> = stdout
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(PathBuf::from)
            .filter(|file| !workforce_agent::tools::paths::is_withheld(file))
            .collect();

        let truncated = files.len() > max_files;
        files.truncate(max_files);

        Ok((files, truncated))
    }

    /// Process a single file: find matches, build previews, optionally apply replacements.
    fn process_file(
        &self,
        file_path: &std::path::Path,
        pattern: &str,
        replacement: &str,
        is_regex: bool,
        case_sensitive: bool,
        preview: bool,
        compiled_regex: Option<&Regex>,
    ) -> Result<FileChange> {
        // Validate path is within allowed paths
        let validated = validate_path(file_path, &self.allowed_paths)?;
        let content = std::fs::read_to_string(&validated)?;
        let lines: Vec<&str> = content.lines().collect();

        // Build the regex for matching (used for both regex mode and case-insensitive exact mode)
        let match_regex: Option<Regex>;
        let effective_regex = if let Some(r) = compiled_regex {
            Some(r)
        } else if !is_regex && !case_sensitive {
            // Case-insensitive exact match: escape the pattern and build case-insensitive regex
            match_regex = Some(
                RegexBuilder::new(&regex::escape(pattern))
                    .case_insensitive(true)
                    .build()
                    .map_err(|e| eyre!("Failed to build case-insensitive pattern: {}", e))?,
            );
            match_regex.as_ref()
        } else {
            None
        };

        // Find all matches with line numbers
        let mut match_positions: Vec<(usize, usize, usize)> = Vec::new(); // (byte_start, byte_end, line_number)

        if let Some(re) = effective_regex {
            for mat in re.find_iter(&content) {
                let line_num = content[..mat.start()].matches('\n').count() + 1;
                match_positions.push((mat.start(), mat.end(), line_num));
            }
        } else {
            // Exact match, case-sensitive
            let mut start = 0;
            while let Some(pos) = content[start..].find(pattern) {
                let abs_pos = start + pos;
                let line_num = content[..abs_pos].matches('\n').count() + 1;
                match_positions.push((abs_pos, abs_pos + pattern.len(), line_num));
                start = abs_pos + pattern.len();
            }
        }

        let total_matches = match_positions.len();

        if total_matches == 0 {
            return Ok(FileChange {
                path: file_path.display().to_string(),
                matches: 0,
                replacements: Vec::new(),
                preview_truncated: false,
            });
        }

        // Build previews — collect up to MAX_PREVIEW_PER_FILE unique lines.
        // We iterate over ALL match positions (not just the first N) to ensure
        // we collect enough unique lines even when multiple matches share a line.
        let mut replacements = Vec::new();

        for &(_byte_start, _byte_end, line_num) in &match_positions {
            if replacements.len() >= MAX_PREVIEW_PER_FILE {
                break;
            }

            // Skip lines we've already previewed
            let already_has_line = replacements
                .iter()
                .any(|r: &MatchPreview| r.line == line_num);
            if already_has_line {
                continue;
            }

            let line_idx = line_num.saturating_sub(1);
            let before = lines.get(line_idx).unwrap_or(&"").to_string();

            // Compute the after line by replacing ALL occurrences — this matches
            // what apply mode actually does (replace_all / replace), so the
            // preview accurately represents the final result.
            let after = if let Some(re) = effective_regex {
                let line_content = lines.get(line_idx).unwrap_or(&"");
                re.replace_all(line_content, replacement).to_string()
            } else {
                let line_content = lines.get(line_idx).unwrap_or(&"");
                line_content.replace(pattern, replacement)
            };

            replacements.push(MatchPreview {
                line: line_num,
                before,
                after,
            });
        }

        // Count unique lines with matches for truncation check
        let unique_match_lines: std::collections::HashSet<usize> =
            match_positions.iter().map(|&(_, _, ln)| ln).collect();
        let preview_truncated = unique_match_lines.len() > MAX_PREVIEW_PER_FILE;

        // Apply replacements if not preview mode
        if !preview {
            let new_content = if let Some(re) = effective_regex {
                re.replace_all(&content, replacement).to_string()
            } else {
                content.replace(pattern, replacement)
            };
            let roots = CanonicalRoots::new(&self.allowed_paths);
            config_guard::write(&roots, &validated, new_content.as_bytes())?;
        }

        Ok(FileChange {
            path: file_path.display().to_string(),
            matches: total_matches,
            replacements,
            preview_truncated,
        })
    }
}

#[derive(Deserialize)]
struct FindReplaceInput {
    pattern: String,
    replacement: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    file_type: Option<String>,
    #[serde(default = "default_false")]
    regex: bool,
    #[serde(default = "default_true")]
    preview: bool,
    #[serde(default = "default_true")]
    case_sensitive: bool,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default = "default_max_files")]
    max_files: usize,
    #[serde(default)]
    exclude: Option<Vec<String>>,
}

fn default_false() -> bool {
    false
}

fn default_true() -> bool {
    true
}

fn default_max_files() -> usize {
    DEFAULT_MAX_FILES
}

#[derive(Serialize, Deserialize)]
struct FindReplaceOutput {
    total_matches: usize,
    files_affected: usize,
    files: Vec<FileChange>,
    truncated: bool,
}

#[derive(Serialize, Deserialize)]
struct FileChange {
    path: String,
    matches: usize,
    replacements: Vec<MatchPreview>,
    preview_truncated: bool,
}

#[derive(Serialize, Deserialize)]
struct MatchPreview {
    line: usize,
    before: String,
    after: String,
}

#[async_trait]
impl Tool for CodeFindReplaceTool {
    fn name(&self) -> &str {
        "code.find_replace"
    }

    fn description(&self) -> &str {
        "Project-wide find and replace"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Search pattern (exact string or regex)"
                },
                "replacement": {
                    "type": "string",
                    "description": "Replacement string"
                },
                "path": {
                    "type": "string",
                    "description": "Directory scope (default: workspace root)"
                },
                "file_type": {
                    "type": "string",
                    "description": "File type filter (e.g., 'rust', 'ts', 'py', 'js', 'go')"
                },
                "regex": {
                    "type": "boolean",
                    "description": "Treat pattern as regex (default: false)",
                    "default": false
                },
                "preview": {
                    "type": "boolean",
                    "description": "Dry-run mode showing what would change (default: true)",
                    "default": true
                },
                "case_sensitive": {
                    "type": "boolean",
                    "description": "Case-sensitive matching (default: true)",
                    "default": true
                },
                "repo": {
                    "type": "string",
                    "description": super::utils::REPO_PARAM
                },
                "max_files": {
                    "type": "integer",
                    "description": "Maximum number of files to process (default",
                    "default": DEFAULT_MAX_FILES,
                    "minimum": 1
                },
                "exclude": {
                    "type": "array",
                    "description": "Glob patterns to exclude",
                    "items": { "type": "string" }
                }
            },
            "required": ["pattern", "replacement"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();

        let input: FindReplaceInput =
            serde_json::from_value(input).map_err(|e| eyre!("Invalid input: {}", e))?;

        // Validate regex pattern if regex mode is enabled
        let compiled_regex = if input.regex {
            let re = if input.case_sensitive {
                Regex::new(&input.pattern)
            } else {
                RegexBuilder::new(&input.pattern)
                    .case_insensitive(true)
                    .build()
            };
            match re {
                Ok(r) => Some(r),
                Err(e) => {
                    return Ok(ToolResult::failure(
                        format!("Invalid regex pattern '{}': {}", input.pattern, e),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        } else {
            None
        };

        // Resolve search path
        let search_path = if let Some(ref repo) = input.repo {
            let base = self
                .allowed_paths
                .first()
                .ok_or_else(|| eyre!("No workspace configured"))?;
            match super::utils::resolve_repo_path(repo, base) {
                Ok(p) => p,
                Err(e) => {
                    return Ok(ToolResult::failure(
                        format!("Find-replace failed: {}", e),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        } else if let Some(ref path) = input.path {
            let p = PathBuf::from(path);
            match validate_path(&p, &self.allowed_paths) {
                Ok(vp) => vp,
                Err(e) => {
                    return Ok(ToolResult::failure(
                        format!("Find-replace failed: {}", e),
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

        // Empty pattern check
        if input.pattern.is_empty() {
            return Ok(ToolResult::failure(
                "Pattern cannot be empty",
                start.elapsed().as_millis() as u64,
            ));
        }

        // Discover files using ripgrep
        let exclude_patterns = input.exclude.as_deref().unwrap_or(&[]);
        let (files, truncated) = match self
            .discover_files(
                &input.pattern,
                &search_path,
                input.file_type.as_deref(),
                input.case_sensitive,
                exclude_patterns,
                input.max_files,
                input.regex,
            )
            .await
        {
            Ok(result) => result,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Find-replace failed during file discovery: {}", e),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        // Process each file
        let mut file_changes = Vec::new();
        let mut total_matches = 0usize;

        for file_path in &files {
            match self.process_file(
                file_path,
                &input.pattern,
                &input.replacement,
                input.regex,
                input.case_sensitive,
                input.preview,
                compiled_regex.as_ref(),
            ) {
                Ok(change) => {
                    total_matches += change.matches;
                    if change.matches > 0 {
                        file_changes.push(change);
                    }
                }
                Err(e) => {
                    // Log the error but continue processing other files
                    tracing::warn!("Failed to process {}: {}", file_path.display(), e);
                }
            }
        }

        let output = FindReplaceOutput {
            total_matches,
            files_affected: file_changes.len(),
            files: file_changes,
            truncated,
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
    use std::process::Command as StdCommand;
    use tempfile::TempDir;

    fn has_ripgrep() -> bool {
        StdCommand::new("rg").arg("--version").output().is_ok()
    }

    /// Helper: create a temp dir with some test files and return the tool + dir.
    fn setup_test_dir() -> (TempDir, CodeFindReplaceTool) {
        let temp = TempDir::new().unwrap();
        let tool = CodeFindReplaceTool::new(vec![temp.path().to_path_buf()]);
        (temp, tool)
    }

    #[tokio::test]
    async fn test_preview_exact_match() {
        if !has_ripgrep() {
            eprintln!("Skipping test: ripgrep not installed");
            return;
        }
        let (temp, tool) = setup_test_dir();
        fs::write(
            temp.path().join("foo.rs"),
            "fn UserStatus() {}\nlet x = UserStatus;\n",
        )
        .unwrap();

        let input = serde_json::json!({
            "pattern": "UserStatus",
            "replacement": "AccountStatus",
            "preview": true
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: FindReplaceOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.total_matches, 2);
        assert_eq!(output.files_affected, 1);
        assert!(!output.truncated);

        // File should NOT be modified in preview mode
        let content = fs::read_to_string(temp.path().join("foo.rs")).unwrap();
        assert!(content.contains("UserStatus"));
    }

    #[tokio::test]
    async fn test_apply_exact_match() {
        if !has_ripgrep() {
            eprintln!("Skipping test: ripgrep not installed");
            return;
        }
        let (temp, tool) = setup_test_dir();
        fs::write(
            temp.path().join("foo.rs"),
            "fn UserStatus() {}\nlet x = UserStatus;\n",
        )
        .unwrap();

        let input = serde_json::json!({
            "pattern": "UserStatus",
            "replacement": "AccountStatus",
            "preview": false
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: FindReplaceOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.total_matches, 2);
        assert_eq!(output.files_affected, 1);

        // File SHOULD be modified
        let content = fs::read_to_string(temp.path().join("foo.rs")).unwrap();
        assert!(!content.contains("UserStatus"));
        assert!(content.contains("AccountStatus"));
        assert_eq!(content.matches("AccountStatus").count(), 2);
    }

    #[tokio::test]
    async fn test_preview_regex() {
        if !has_ripgrep() {
            eprintln!("Skipping test: ripgrep not installed");
            return;
        }
        let (temp, tool) = setup_test_dir();
        fs::write(
            temp.path().join("test.txt"),
            "foo_bar = 1\nfoo_baz = 2\nhello = 3\n",
        )
        .unwrap();

        let input = serde_json::json!({
            "pattern": "foo_(\\w+)",
            "replacement": "bar_$1",
            "regex": true,
            "preview": true
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: FindReplaceOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.total_matches, 2);
        assert_eq!(output.files_affected, 1);

        // Check preview shows correct after
        let file = &output.files[0];
        let first_replace = &file.replacements[0];
        assert!(first_replace.after.contains("bar_bar") || first_replace.after.contains("bar_baz"));

        // File should NOT be modified in preview
        let content = fs::read_to_string(temp.path().join("test.txt")).unwrap();
        assert!(content.contains("foo_bar"));
    }

    #[tokio::test]
    async fn test_apply_regex_capture_groups() {
        if !has_ripgrep() {
            eprintln!("Skipping test: ripgrep not installed");
            return;
        }
        let (temp, tool) = setup_test_dir();
        fs::write(
            temp.path().join("test.txt"),
            "fn get_user(id: u32) {}\nfn get_post(id: u32) {}\n",
        )
        .unwrap();

        let input = serde_json::json!({
            "pattern": "get_(\\w+)",
            "replacement": "fetch_$1",
            "regex": true,
            "preview": false
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let content = fs::read_to_string(temp.path().join("test.txt")).unwrap();
        assert!(content.contains("fetch_user"));
        assert!(content.contains("fetch_post"));
        assert!(!content.contains("get_user"));
        assert!(!content.contains("get_post"));
    }

    #[tokio::test]
    async fn test_case_insensitive() {
        if !has_ripgrep() {
            eprintln!("Skipping test: ripgrep not installed");
            return;
        }
        let (temp, tool) = setup_test_dir();
        fs::write(
            temp.path().join("test.txt"),
            "Hello World\nhello world\nHELLO WORLD\n",
        )
        .unwrap();

        let input = serde_json::json!({
            "pattern": "hello",
            "replacement": "Hi",
            "case_sensitive": false,
            "preview": false
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let content = fs::read_to_string(temp.path().join("test.txt")).unwrap();
        assert_eq!(content.matches("Hi").count(), 3);
        assert!(!content.contains("Hello"));
        assert!(!content.contains("hello"));
        assert!(!content.contains("HELLO"));
    }

    #[tokio::test]
    async fn test_file_type_filter() {
        if !has_ripgrep() {
            eprintln!("Skipping test: ripgrep not installed");
            return;
        }
        let (temp, tool) = setup_test_dir();
        fs::write(temp.path().join("code.rs"), "let x = foo;\n").unwrap();
        fs::write(temp.path().join("script.py"), "x = foo\n").unwrap();

        let input = serde_json::json!({
            "pattern": "foo",
            "replacement": "bar",
            "file_type": "rust",
            "preview": false
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: FindReplaceOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.files_affected, 1);

        // Only .rs file should be modified
        let rs_content = fs::read_to_string(temp.path().join("code.rs")).unwrap();
        assert!(rs_content.contains("bar"));

        let py_content = fs::read_to_string(temp.path().join("script.py")).unwrap();
        assert!(py_content.contains("foo")); // unchanged
    }

    #[tokio::test]
    async fn test_directory_scoping() {
        if !has_ripgrep() {
            eprintln!("Skipping test: ripgrep not installed");
            return;
        }
        let (temp, tool) = setup_test_dir();
        let subdir = temp.path().join("src");
        fs::create_dir(&subdir).unwrap();
        fs::write(subdir.join("inner.txt"), "hello\n").unwrap();
        fs::write(temp.path().join("outer.txt"), "hello\n").unwrap();

        let input = serde_json::json!({
            "pattern": "hello",
            "replacement": "hi",
            "path": subdir.display().to_string(),
            "preview": false
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: FindReplaceOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.files_affected, 1);

        // Only inner file modified
        let inner = fs::read_to_string(subdir.join("inner.txt")).unwrap();
        assert!(inner.contains("hi"));

        let outer = fs::read_to_string(temp.path().join("outer.txt")).unwrap();
        assert!(outer.contains("hello"));
    }

    #[tokio::test]
    async fn test_exclude_patterns() {
        if !has_ripgrep() {
            eprintln!("Skipping test: ripgrep not installed");
            return;
        }
        let (temp, tool) = setup_test_dir();
        fs::write(temp.path().join("code.rs"), "let foo = 1;\n").unwrap();
        fs::write(temp.path().join("code.test.rs"), "let foo = 2;\n").unwrap();

        let input = serde_json::json!({
            "pattern": "foo",
            "replacement": "bar",
            "exclude": ["*.test.rs"],
            "preview": false
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: FindReplaceOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.files_affected, 1);

        let code = fs::read_to_string(temp.path().join("code.rs")).unwrap();
        assert!(code.contains("bar"));

        let test_code = fs::read_to_string(temp.path().join("code.test.rs")).unwrap();
        assert!(test_code.contains("foo")); // excluded, unchanged
    }

    #[tokio::test]
    async fn test_max_files_limit() {
        if !has_ripgrep() {
            eprintln!("Skipping test: ripgrep not installed");
            return;
        }
        let (temp, tool) = setup_test_dir();

        // Create 5 files with matching content
        for i in 0..5 {
            fs::write(temp.path().join(format!("file{}.txt", i)), "match_me\n").unwrap();
        }

        let input = serde_json::json!({
            "pattern": "match_me",
            "replacement": "matched",
            "max_files": 3,
            "preview": true
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: FindReplaceOutput = serde_json::from_value(result.output).unwrap();
        assert!(output.truncated);
        assert!(output.files_affected <= 3);
    }

    #[tokio::test]
    async fn test_preview_truncation() {
        if !has_ripgrep() {
            eprintln!("Skipping test: ripgrep not installed");
            return;
        }
        let (temp, tool) = setup_test_dir();

        // Create file with more than 5 matches
        let content: String = (0..10)
            .map(|i| format!("line{} has target here\n", i))
            .collect();
        fs::write(temp.path().join("many.txt"), &content).unwrap();

        let input = serde_json::json!({
            "pattern": "target",
            "replacement": "replaced",
            "preview": true
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: FindReplaceOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.total_matches, 10);
        assert!(output.files[0].preview_truncated);
        assert!(output.files[0].replacements.len() <= MAX_PREVIEW_PER_FILE);
    }

    #[tokio::test]
    async fn test_invalid_regex() {
        if !has_ripgrep() {
            eprintln!("Skipping test: ripgrep not installed");
            return;
        }
        let (temp, tool) = setup_test_dir();
        fs::write(temp.path().join("test.txt"), "hello\n").unwrap();

        let input = serde_json::json!({
            "pattern": "[invalid(regex",
            "replacement": "x",
            "regex": true
        });

        let result = tool.execute(input).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Invalid regex"));
    }

    #[tokio::test]
    async fn test_no_matches() {
        if !has_ripgrep() {
            eprintln!("Skipping test: ripgrep not installed");
            return;
        }
        let (temp, tool) = setup_test_dir();
        fs::write(temp.path().join("test.txt"), "hello world\n").unwrap();

        let input = serde_json::json!({
            "pattern": "nonexistent_xyz_pattern",
            "replacement": "whatever"
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: FindReplaceOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.total_matches, 0);
        assert_eq!(output.files_affected, 0);
        assert!(output.files.is_empty());
    }

    #[tokio::test]
    async fn test_gitignore_respect() {
        if !has_ripgrep() {
            eprintln!("Skipping test: ripgrep not installed");
            return;
        }
        let (temp, tool) = setup_test_dir();

        // Initialize a git repo so ripgrep respects .gitignore
        StdCommand::new("git")
            .args(["init"])
            .current_dir(temp.path())
            .output()
            .unwrap();
        fs::write(temp.path().join(".gitignore"), "ignored_dir/\n").unwrap();
        let ignored_dir = temp.path().join("ignored_dir");
        fs::create_dir(&ignored_dir).unwrap();
        fs::write(ignored_dir.join("hidden.txt"), "target_text\n").unwrap();
        fs::write(temp.path().join("visible.txt"), "target_text\n").unwrap();

        let input = serde_json::json!({
            "pattern": "target_text",
            "replacement": "replaced",
            "preview": false
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: FindReplaceOutput = serde_json::from_value(result.output).unwrap();
        // Only visible.txt should be matched
        assert_eq!(output.files_affected, 1);

        let visible = fs::read_to_string(temp.path().join("visible.txt")).unwrap();
        assert!(visible.contains("replaced"));

        let hidden = fs::read_to_string(ignored_dir.join("hidden.txt")).unwrap();
        assert!(hidden.contains("target_text")); // unchanged, was ignored
    }

    #[tokio::test]
    async fn test_empty_pattern_rejected() {
        let (_, tool) = setup_test_dir();

        let input = serde_json::json!({
            "pattern": "",
            "replacement": "x"
        });

        let result = tool.execute(input).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("empty"));
    }

    #[tokio::test]
    async fn test_multiple_files_exact() {
        if !has_ripgrep() {
            eprintln!("Skipping test: ripgrep not installed");
            return;
        }
        let (temp, tool) = setup_test_dir();
        fs::write(temp.path().join("a.txt"), "old_value = 1\n").unwrap();
        fs::write(temp.path().join("b.txt"), "old_value = 2\n").unwrap();
        fs::write(temp.path().join("c.txt"), "unrelated\n").unwrap();

        let input = serde_json::json!({
            "pattern": "old_value",
            "replacement": "new_value",
            "preview": false
        });

        let result = tool.execute(input).await.unwrap();
        assert!(result.success);

        let output: FindReplaceOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.files_affected, 2);
        assert_eq!(output.total_matches, 2);

        let a = fs::read_to_string(temp.path().join("a.txt")).unwrap();
        assert!(a.contains("new_value"));

        let b = fs::read_to_string(temp.path().join("b.txt")).unwrap();
        assert!(b.contains("new_value"));

        let c = fs::read_to_string(temp.path().join("c.txt")).unwrap();
        assert!(c.contains("unrelated"));
    }
}
