//! code.lint - Run linters and formatters with auto-detection.

use async_trait::async_trait;
use eyre::{eyre, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Instant;
use workforce_agent::tools::exec_runner::RunnerEndpoint;
use workforce_agent::tools::{Tool, ToolResult};

use super::utils::{detect_project_type, run_command, CODE_TOOL_TIMEOUT_SECS};

/// code.lint tool - Run linters and formatters.
pub struct CodeLintTool {
    allowed_paths: Vec<PathBuf>,
    /// When set, lint commands dispatch to the privilege-separated runner
    /// daemon (audit F7); `None` runs them in-process (dev/local).
    runner: Option<RunnerEndpoint>,
}

impl CodeLintTool {
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

    async fn run_lint(
        &self,
        workspace: &Path,
        tool: &str,
        fix: bool,
        path: Option<&str>,
    ) -> Result<LintOutput> {
        let detected_tool = if tool == "auto" {
            self.detect_lint_tool(workspace)?
        } else {
            tool.to_string()
        };

        let (warnings, errors, issues, raw_output) = match detected_tool.as_str() {
            "clippy" => self.run_clippy(workspace, fix, path).await?,
            "eslint" => self.run_eslint(workspace, fix, path).await?,
            "pylint" => self.run_pylint(workspace, path).await?,
            "rubocop" => self.run_rubocop(workspace, fix, path).await?,
            _ => return Err(eyre!("Unknown lint tool: {}", detected_tool)),
        };

        Ok(LintOutput {
            tool: detected_tool,
            warnings,
            errors,
            issues,
            raw_output,
        })
    }

    fn detect_lint_tool(&self, workspace: &std::path::Path) -> Result<String> {
        match detect_project_type(workspace) {
            Some("rust") => Ok("clippy".to_string()),
            Some("node") => Ok("eslint".to_string()),
            Some("python") => Ok("pylint".to_string()),
            Some("ruby") => Ok("rubocop".to_string()),
            _ => Err(eyre!("Could not detect lint tool")),
        }
    }

    async fn run_clippy(
        &self,
        workspace: &Path,
        _fix: bool,
        _path: Option<&str>,
    ) -> Result<(usize, usize, Vec<LintIssue>, String)> {
        let outcome = run_command(
            self.runner.as_ref(),
            "cargo",
            &["clippy", "--", "-D", "warnings"],
            workspace,
            CODE_TOOL_TIMEOUT_SECS,
        )
        .await?;

        let combined = format!("{}\n{}", outcome.stdout, outcome.stderr);

        // Simple parsing - count "warning:" and "error:"
        let warnings = combined.matches("warning:").count();
        let errors = combined.matches("error:").count();

        Ok((warnings, errors, Vec::new(), combined))
    }

    async fn run_eslint(
        &self,
        workspace: &Path,
        fix: bool,
        path: Option<&str>,
    ) -> Result<(usize, usize, Vec<LintIssue>, String)> {
        let mut args: Vec<&str> = Vec::new();
        if fix {
            args.push("--fix");
        }
        args.push(path.unwrap_or("."));

        let outcome = run_command(
            self.runner.as_ref(),
            "eslint",
            &args,
            workspace,
            CODE_TOOL_TIMEOUT_SECS,
        )
        .await?;
        let stdout = outcome.stdout;

        let warnings = stdout.matches("warning").count();
        let errors = stdout.matches("error").count();

        Ok((warnings, errors, Vec::new(), stdout))
    }

    async fn run_pylint(
        &self,
        workspace: &Path,
        path: Option<&str>,
    ) -> Result<(usize, usize, Vec<LintIssue>, String)> {
        let outcome = run_command(
            self.runner.as_ref(),
            "pylint",
            &[path.unwrap_or(".")],
            workspace,
            CODE_TOOL_TIMEOUT_SECS,
        )
        .await?;

        Ok((0, 0, Vec::new(), outcome.stdout))
    }

    async fn run_rubocop(
        &self,
        workspace: &Path,
        fix: bool,
        path: Option<&str>,
    ) -> Result<(usize, usize, Vec<LintIssue>, String)> {
        let mut args: Vec<&str> = Vec::new();
        if fix {
            args.push("-a");
        }
        if let Some(p) = path {
            args.push(p);
        }

        let outcome = run_command(
            self.runner.as_ref(),
            "rubocop",
            &args,
            workspace,
            CODE_TOOL_TIMEOUT_SECS,
        )
        .await?;

        Ok((0, 0, Vec::new(), outcome.stdout))
    }
}

#[derive(Deserialize)]
struct CodeLintInput {
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default = "default_tool")]
    tool: String,
    #[serde(default)]
    fix: bool,
    #[serde(default)]
    path: Option<String>,
}

fn default_tool() -> String {
    "auto".to_string()
}

#[derive(Serialize, Deserialize, Debug)]
struct LintIssue {
    file: String,
    line: usize,
    severity: String,
    code: String,
    message: String,
}

#[derive(Serialize, Deserialize)]
struct LintOutput {
    tool: String,
    warnings: usize,
    errors: usize,
    issues: Vec<LintIssue>,
    raw_output: String,
}

#[async_trait]
impl Tool for CodeLintTool {
    fn name(&self) -> &str {
        "code.lint"
    }

    fn description(&self) -> &str {
        "Run linters and formatters with auto-detection"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "workdir": {
                    "type": "string",
                    "description": "Absolute project directory path"
                },
                "repo": {
                    "type": "string",
                    "description": super::utils::REPO_PARAM
                },
                "tool": {
                    "type": "string",
                    "enum": ["auto", "clippy", "eslint", "pylint", "rubocop"],
                    "default": "auto"
                },
                "fix": {
                    "type": "boolean",
                    "default": false
                },
                "path": {
                    "type": "string",
                    "description": "Specific file/directory to lint within workdir"
                }
            }
        ,
            "required": []})
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: CodeLintInput = serde_json::from_value(input)?;

        // Resolve workspace: prefer repo param, fall back to workdir, then error
        let workspace = if let Some(ref repo) = input.repo {
            let base = self
                .allowed_paths
                .first()
                .ok_or_else(|| eyre!("No workspace"))?;
            match super::utils::resolve_repo_path(repo, base) {
                Ok(p) => p,
                Err(e) => {
                    return Ok(ToolResult::failure(
                        format!("Lint failed: {}", e),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        } else if let Some(w) = input.workdir {
            PathBuf::from(w)
        } else {
            let base = self
                .allowed_paths
                .first()
                .ok_or_else(|| eyre!("No workspace"))?;
            let hint = super::utils::checked_out_note(base);
            return Ok(ToolResult::failure(
                format!(
                    "Missing 'repo' parameter. Specify which repo to lint. {}",
                    hint
                ),
                start.elapsed().as_millis() as u64,
            ));
        };

        match self
            .run_lint(&workspace, &input.tool, input.fix, input.path.as_deref())
            .await
        {
            Ok(output) => Ok(ToolResult::success(
                serde_json::to_value(output)?,
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Lint failed: {}", e),
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
    async fn test_detect_clippy() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("Cargo.toml"), "[package]").unwrap();

        let tool = CodeLintTool::new(vec![temp.path().to_path_buf()]);
        assert_eq!(tool.detect_lint_tool(temp.path()).unwrap(), "clippy");
    }

    #[tokio::test]
    async fn test_detect_eslint() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("package.json"), "{}").unwrap();

        let tool = CodeLintTool::new(vec![temp.path().to_path_buf()]);
        assert_eq!(tool.detect_lint_tool(temp.path()).unwrap(), "eslint");
    }
}
