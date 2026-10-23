//! code.test - Run tests with framework auto-detection and result parsing.

use async_trait::async_trait;
use eyre::{eyre, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Instant;
use workforce_agent::tools::exec_runner::RunnerEndpoint;
use workforce_agent::tools::{Tool, ToolResult};

use super::utils::{detect_project_type, run_command};

const DEFAULT_TIMEOUT: u64 = 300; // 5 minutes

/// code.test tool - Run tests with framework detection.
pub struct CodeTestTool {
    allowed_paths: Vec<PathBuf>,
    /// When set, test commands dispatch to the privilege-separated runner
    /// daemon (audit F7); `None` runs them in-process (dev/local).
    runner: Option<RunnerEndpoint>,
}

impl CodeTestTool {
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

    /// Run tests with specified or auto-detected framework.
    async fn run_tests(
        &self,
        workspace: &Path,
        framework: &str,
        filter: Option<&str>,
        _failed_only: bool,
        timeout_secs: u64,
    ) -> Result<TestOutput> {
        let start = Instant::now();

        // Auto-detect framework if set to "auto"
        let detected_framework = if framework == "auto" {
            self.detect_test_framework(workspace)?
        } else {
            framework.to_string()
        };

        // Build and run test command
        let raw_output = match detected_framework.as_str() {
            "cargo" => {
                self.run_cargo_tests(workspace, filter, timeout_secs)
                    .await?
            }
            "npm" => self.run_npm_tests(workspace, filter, timeout_secs).await?,
            "pytest" => {
                self.run_pytest_tests(workspace, filter, timeout_secs)
                    .await?
            }
            "rspec" => {
                self.run_rspec_tests(workspace, filter, timeout_secs)
                    .await?
            }
            "playwright" => {
                self.run_playwright_tests(workspace, filter, timeout_secs)
                    .await?
            }
            "cypress" => {
                self.run_cypress_tests(workspace, filter, timeout_secs)
                    .await?
            }
            _ => {
                return Err(eyre!(
                    "Unknown or unsupported test framework: {}",
                    detected_framework
                ))
            }
        };

        // Parse output based on framework
        let (total, passed, failed, failed_tests) = match detected_framework.as_str() {
            "cargo" => self.parse_cargo_output(&raw_output)?,
            "npm" => self.parse_npm_output(&raw_output)?,
            "pytest" => self.parse_pytest_output(&raw_output)?,
            "rspec" => self.parse_rspec_output(&raw_output)?,
            "playwright" => self.parse_playwright_output(&raw_output)?,
            "cypress" => self.parse_npm_output(&raw_output)?, // Cypress uses similar output to Jest
            _ => (0, 0, 0, Vec::new()),
        };

        Ok(TestOutput {
            framework: detected_framework,
            total,
            passed,
            failed,
            duration_ms: start.elapsed().as_millis() as u64,
            failed_tests,
            raw_output,
        })
    }

    /// Auto-detect test framework from project type.
    fn detect_test_framework(&self, workspace: &std::path::Path) -> Result<String> {
        let project_type = detect_project_type(workspace);

        match project_type {
            Some("rust") => Ok("cargo".to_string()),
            Some("node") => {
                // Check for E2E frameworks first, then fall back to npm test
                if workspace.join("playwright.config.ts").exists()
                    || workspace.join("playwright.config.js").exists()
                {
                    Ok("playwright".to_string())
                } else if workspace.join("cypress.config.ts").exists()
                    || workspace.join("cypress.config.js").exists()
                    || workspace.join("cypress.json").exists()
                {
                    Ok("cypress".to_string())
                } else if workspace.join("package.json").exists() {
                    Ok("npm".to_string())
                } else {
                    Err(eyre!("No test script found in package.json"))
                }
            }
            Some("python") => Ok("pytest".to_string()),
            Some("ruby") => Ok("rspec".to_string()),
            _ => Err(eyre!("Could not auto-detect test framework")),
        }
    }

    /// Run cargo tests.
    async fn run_cargo_tests(
        &self,
        workspace: &Path,
        filter: Option<&str>,
        timeout_secs: u64,
    ) -> Result<String> {
        let mut args = vec!["test"];
        if let Some(f) = filter {
            args.push(f);
        }

        let outcome = self.run("cargo", &args, workspace, timeout_secs).await?;
        Ok(format!("{}\n{}", outcome.stdout, outcome.stderr))
    }

    /// Run npm tests.
    async fn run_npm_tests(
        &self,
        workspace: &Path,
        filter: Option<&str>,
        timeout_secs: u64,
    ) -> Result<String> {
        let mut args = vec!["test"];
        if let Some(f) = filter {
            args.extend(["--", f]);
        }

        let outcome = self.run("npm", &args, workspace, timeout_secs).await?;
        Ok(outcome.stdout)
    }

    /// Run pytest tests.
    async fn run_pytest_tests(
        &self,
        workspace: &Path,
        filter: Option<&str>,
        timeout_secs: u64,
    ) -> Result<String> {
        let mut args = vec!["-v"];
        if let Some(f) = filter {
            args.extend(["-k", f]);
        }

        let outcome = self.run("pytest", &args, workspace, timeout_secs).await?;
        Ok(outcome.stdout)
    }

    /// Run rspec tests.
    async fn run_rspec_tests(
        &self,
        workspace: &Path,
        filter: Option<&str>,
        timeout_secs: u64,
    ) -> Result<String> {
        let mut args: Vec<&str> = Vec::new();
        if let Some(f) = filter {
            args.push(f);
        }

        let outcome = self.run("rspec", &args, workspace, timeout_secs).await?;
        Ok(outcome.stdout)
    }

    /// Run Playwright E2E tests.
    async fn run_playwright_tests(
        &self,
        workspace: &Path,
        filter: Option<&str>,
        timeout_secs: u64,
    ) -> Result<String> {
        let mut args = vec!["playwright", "test"];
        if let Some(f) = filter {
            args.push(f);
        }

        let outcome = self.run("npx", &args, workspace, timeout_secs).await?;
        Ok(format!("{}\n{}", outcome.stdout, outcome.stderr))
    }

    /// Run Cypress E2E tests.
    async fn run_cypress_tests(
        &self,
        workspace: &Path,
        filter: Option<&str>,
        timeout_secs: u64,
    ) -> Result<String> {
        let mut args = vec!["cypress", "run"];
        if let Some(f) = filter {
            args.extend(["--spec", f]);
        }

        let outcome = self.run("npx", &args, workspace, timeout_secs).await?;
        Ok(outcome.stdout)
    }

    /// Parse Playwright test output.
    fn parse_playwright_output(&self, output: &str) -> Result<(usize, usize, usize, Vec<String>)> {
        // Playwright: "X passed", "X failed", "X skipped"
        let passed_re = Regex::new(r"(\d+) passed")?;
        let failed_re = Regex::new(r"(\d+) failed")?;

        let passed = passed_re
            .captures(output)
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        let failed = failed_re
            .captures(output)
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        let total = passed + failed;

        // Extract failed test names: lines with "✘" or "×" or "FAILED"
        let mut failed_tests = Vec::new();
        let fail_line_re = Regex::new(r"[✘×]\s+(.+)")?;
        for caps in fail_line_re.captures_iter(output) {
            if let Some(name) = caps.get(1) {
                failed_tests.push(name.as_str().trim().to_string());
            }
        }

        Ok((total, passed, failed, failed_tests))
    }

    /// Run a test command through the executor (or in-process), honouring the
    /// caller's timeout — which the previous in-process path silently ignored,
    /// so this newly enforces the `timeout` parameter the tool already accepted.
    async fn run(
        &self,
        program: &str,
        args: &[&str],
        workspace: &Path,
        timeout_secs: u64,
    ) -> Result<workforce_agent::tools::exec_runner::ExecOutcome> {
        run_command(self.runner.as_ref(), program, args, workspace, timeout_secs).await
    }

    /// Parse cargo test output.
    fn parse_cargo_output(&self, output: &str) -> Result<(usize, usize, usize, Vec<String>)> {
        // Look for "test result: ok. X passed; Y failed"
        let result_regex = Regex::new(r"test result: \w+\. (\d+) passed; (\d+) failed")?;

        let mut total = 0;
        let mut passed = 0;
        let mut failed = 0;
        let mut failed_tests = Vec::new();

        if let Some(caps) = result_regex.captures(output) {
            passed = caps
                .get(1)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            failed = caps
                .get(2)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            total = passed + failed;
        }

        // Extract failed test names: "test test_name ... FAILED"
        let failed_regex = Regex::new(r"test ([\w:]+) \.\.\. FAILED")?;
        for caps in failed_regex.captures_iter(output) {
            if let Some(name) = caps.get(1) {
                failed_tests.push(name.as_str().to_string());
            }
        }

        Ok((total, passed, failed, failed_tests))
    }

    /// Parse npm test output (Jest format).
    fn parse_npm_output(&self, output: &str) -> Result<(usize, usize, usize, Vec<String>)> {
        // Jest: "Tests: X failed, Y passed, Z total"
        let result_regex =
            Regex::new(r"Tests:\s+(?:(\d+) failed,\s+)?(\d+) passed,\s+(\d+) total")?;

        let mut total = 0;
        let mut passed = 0;
        let mut failed = 0;
        let mut failed_tests = Vec::new();

        if let Some(caps) = result_regex.captures(output) {
            failed = caps
                .get(1)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            passed = caps
                .get(2)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            total = caps
                .get(3)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
        }

        // Extract failed test names: "● Suite Name › test name" or "● test name"
        let failed_regex = Regex::new(r"●\s+(.+)")?;
        for caps in failed_regex.captures_iter(output) {
            if let Some(name) = caps.get(1) {
                let name = name.as_str().trim();
                if !name.is_empty() {
                    failed_tests.push(name.to_string());
                }
            }
        }

        Ok((total, passed, failed, failed_tests))
    }

    /// Parse pytest output.
    fn parse_pytest_output(&self, output: &str) -> Result<(usize, usize, usize, Vec<String>)> {
        // pytest: "X passed, Y failed in Z.ZZs"
        let result_regex = Regex::new(r"(\d+) passed(?:, (\d+) failed)?")?;

        let mut total = 0;
        let mut passed = 0;
        let mut failed = 0;
        let mut failed_tests = Vec::new();

        if let Some(caps) = result_regex.captures(output) {
            passed = caps
                .get(1)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            failed = caps
                .get(2)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            total = passed + failed;
        }

        // Extract failed test names: "FAILED test_file.py::test_name"
        let failed_regex = Regex::new(r"FAILED ([\w/.]+::[\w_]+)")?;
        for caps in failed_regex.captures_iter(output) {
            if let Some(name) = caps.get(1) {
                failed_tests.push(name.as_str().to_string());
            }
        }

        Ok((total, passed, failed, failed_tests))
    }

    /// Parse rspec output.
    fn parse_rspec_output(&self, output: &str) -> Result<(usize, usize, usize, Vec<String>)> {
        // rspec: "X examples, Y failures"
        let result_regex = Regex::new(r"(\d+) examples?, (\d+) failures?")?;

        let mut total = 0;
        let mut passed = 0;
        let mut failed = 0;
        let failed_tests = Vec::new();

        if let Some(caps) = result_regex.captures(output) {
            total = caps
                .get(1)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            failed = caps
                .get(2)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            passed = total - failed;
        }

        Ok((total, passed, failed, failed_tests))
    }
}

#[derive(Deserialize)]
struct CodeTestInput {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default = "default_framework")]
    framework: String,
    #[serde(default)]
    filter: Option<String>,
    #[serde(default)]
    failed_only: bool,
    #[serde(default = "default_timeout")]
    timeout: u64,
}

fn default_framework() -> String {
    "auto".to_string()
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT
}

#[derive(Serialize, Deserialize)]
struct TestOutput {
    framework: String,
    total: usize,
    passed: usize,
    failed: usize,
    duration_ms: u64,
    failed_tests: Vec<String>,
    raw_output: String,
}

#[async_trait]
impl Tool for CodeTestTool {
    fn name(&self) -> &str {
        "code.test"
    }

    fn description(&self) -> &str {
        "Run tests with framework auto-detection. IMPORTANT: Never run tests for the entire workspace — only test the specific crate/package you changed (e.g. use filter parameter or specify the crate path). For non-code changes (README, docs, config) skip tests entirely — CI handles full test runs."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute project directory path"
                },
                "repo": {
                    "type": "string",
                    "description": super::utils::REPO_PARAM
                },
                "framework": {
                    "type": "string",
                    "enum": ["auto", "cargo", "npm", "pytest", "rspec", "playwright", "cypress"],
                    "description": "Test framework (default: auto-detect)",
                    "default": "auto"
                },
                "filter": {
                    "type": "string",
                    "description": "Test name filter (e.g., 'test_login')"
                },
                "failed_only": {
                    "type": "boolean",
                    "description": "Re-run only failed tests (default: false)",
                    "default": false
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds (default: 300)",
                    "default": DEFAULT_TIMEOUT
                }
            }
        ,
            "required": []})
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();

        let input: CodeTestInput =
            serde_json::from_value(input).map_err(|e| eyre!("Invalid input: {}", e))?;

        // Resolve workspace: prefer repo param, fall back to path, then error
        let workspace = if let Some(ref repo) = input.repo {
            let base = self
                .allowed_paths
                .first()
                .ok_or_else(|| eyre!("No workspace configured"))?;
            match super::utils::resolve_repo_path(repo, base) {
                Ok(p) => p,
                Err(e) => {
                    return Ok(ToolResult::failure(
                        format!("Test failed: {}", e),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        } else if let Some(p) = input.path {
            PathBuf::from(p)
        } else {
            let base = self
                .allowed_paths
                .first()
                .ok_or_else(|| eyre!("No workspace configured"))?;
            let hint = super::utils::checked_out_note(base);
            return Ok(ToolResult::failure(
                format!(
                    "Missing 'repo' parameter. Specify which repo to test. {}",
                    hint
                ),
                start.elapsed().as_millis() as u64,
            ));
        };

        match self
            .run_tests(
                &workspace,
                &input.framework,
                input.filter.as_deref(),
                input.failed_only,
                input.timeout,
            )
            .await
        {
            Ok(output) => Ok(ToolResult::success(
                serde_json::to_value(output)?,
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Test execution failed: {}", e),
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
    async fn test_detect_rust_project() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("Cargo.toml"), "[package]\nname = \"test\"").unwrap();

        let tool = CodeTestTool::new(vec![temp.path().to_path_buf()]);
        let framework = tool.detect_test_framework(temp.path()).unwrap();

        assert_eq!(framework, "cargo");
    }

    #[tokio::test]
    async fn test_detect_node_project() {
        let temp = TempDir::new().unwrap();
        fs::write(
            temp.path().join("package.json"),
            r#"{"scripts":{"test":"jest"}}"#,
        )
        .unwrap();

        let tool = CodeTestTool::new(vec![temp.path().to_path_buf()]);
        let framework = tool.detect_test_framework(temp.path()).unwrap();

        assert_eq!(framework, "npm");
    }

    #[tokio::test]
    async fn test_parse_cargo_output() {
        let tool = CodeTestTool::new(vec![]);

        let output = "running 10 tests\ntest test_one ... ok\ntest test_two ... FAILED\n\ntest result: FAILED. 9 passed; 1 failed";

        let (total, passed, failed, failed_tests) = tool.parse_cargo_output(output).unwrap();

        assert_eq!(total, 10);
        assert_eq!(passed, 9);
        assert_eq!(failed, 1);
        assert_eq!(failed_tests.len(), 1);
        assert_eq!(failed_tests[0], "test_two");
    }

    #[tokio::test]
    async fn test_parse_pytest_output() {
        let tool = CodeTestTool::new(vec![]);

        let output = "FAILED test_file.py::test_login\nFAILED test_file.py::test_logout\n5 passed, 2 failed in 1.23s";

        let (total, passed, failed, failed_tests) = tool.parse_pytest_output(output).unwrap();

        assert_eq!(total, 7);
        assert_eq!(passed, 5);
        assert_eq!(failed, 2);
        assert_eq!(failed_tests.len(), 2);
    }

    #[tokio::test]
    async fn test_detect_playwright_project() {
        let temp = TempDir::new().unwrap();
        fs::write(
            temp.path().join("package.json"),
            r#"{"scripts":{"test":"vitest"}}"#,
        )
        .unwrap();
        fs::write(
            temp.path().join("playwright.config.ts"),
            "export default {}",
        )
        .unwrap();

        let tool = CodeTestTool::new(vec![temp.path().to_path_buf()]);
        let framework = tool.detect_test_framework(temp.path()).unwrap();

        assert_eq!(framework, "playwright");
    }

    #[tokio::test]
    async fn test_detect_cypress_project() {
        let temp = TempDir::new().unwrap();
        fs::write(
            temp.path().join("package.json"),
            r#"{"scripts":{"test":"jest"}}"#,
        )
        .unwrap();
        fs::write(temp.path().join("cypress.config.ts"), "export default {}").unwrap();

        let tool = CodeTestTool::new(vec![temp.path().to_path_buf()]);
        let framework = tool.detect_test_framework(temp.path()).unwrap();

        assert_eq!(framework, "cypress");
    }

    #[tokio::test]
    async fn test_parse_playwright_output() {
        let tool = CodeTestTool::new(vec![]);

        let output = "Running 5 tests\n  ✓ test login\n  ✓ test signup\n  ✘ test checkout\n\n  3 passed\n  1 failed";
        let (total, passed, failed, failed_tests) = tool.parse_playwright_output(output).unwrap();

        assert_eq!(total, 4);
        assert_eq!(passed, 3);
        assert_eq!(failed, 1);
        assert_eq!(failed_tests.len(), 1);
    }

    #[tokio::test]
    async fn test_unknown_framework_error() {
        let temp = TempDir::new().unwrap();
        // No marker files

        let tool = CodeTestTool::new(vec![temp.path().to_path_buf()]);

        let input = serde_json::json!({
            "framework": "auto"
        });

        let result = tool.execute(input).await.unwrap();
        assert!(!result.success);
    }
}
