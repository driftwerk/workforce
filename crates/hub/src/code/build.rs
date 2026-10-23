//! code.build - Build projects with framework auto-detection.

use async_trait::async_trait;
use eyre::{eyre, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Instant;
use workforce_agent::tools::exec_runner::RunnerEndpoint;
use workforce_agent::tools::{Tool, ToolResult};

use super::utils::{detect_project_type, run_command, CODE_TOOL_TIMEOUT_SECS};

pub struct CodeBuildTool {
    allowed_paths: Vec<PathBuf>,
    /// When set, build commands dispatch to the privilege-separated runner
    /// daemon (audit F7); `None` runs them in-process (dev/local).
    runner: Option<RunnerEndpoint>,
}

impl CodeBuildTool {
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

    async fn run_build(
        &self,
        workspace: &Path,
        system: &str,
        release: bool,
        clean: bool,
    ) -> Result<BuildOutput> {
        let start = Instant::now();

        let detected_system = if system == "auto" {
            detect_project_type(workspace)
                .map(|s| match s {
                    "rust" => "cargo",
                    "node" => "npm",
                    _ => "unknown",
                })
                .ok_or_else(|| eyre!("Could not detect build system"))?
                .to_string()
        } else {
            system.to_string()
        };

        let (success, errors, warnings, raw_output) = match detected_system.as_str() {
            "cargo" => self.build_cargo(workspace, release, clean).await?,
            "npm" => self.build_npm(workspace).await?,
            _ => return Err(eyre!("Unknown build system: {}", detected_system)),
        };

        Ok(BuildOutput {
            success,
            build_system: detected_system,
            errors,
            warnings,
            duration_ms: start.elapsed().as_millis() as u64,
            raw_output,
        })
    }

    async fn build_cargo(
        &self,
        workspace: &Path,
        release: bool,
        clean: bool,
    ) -> Result<(bool, usize, usize, String)> {
        let runner = self.runner.as_ref();

        if clean {
            run_command(
                runner,
                "cargo",
                &["clean"],
                workspace,
                CODE_TOOL_TIMEOUT_SECS,
            )
            .await?;
        }

        let mut args = vec!["build"];
        if release {
            args.push("--release");
        }

        let outcome =
            run_command(runner, "cargo", &args, workspace, CODE_TOOL_TIMEOUT_SECS).await?;
        let combined = format!("{}\n{}", outcome.stdout, outcome.stderr);

        let success = outcome.success();
        let errors = combined.matches("error:").count();
        let warnings = combined.matches("warning:").count();

        Ok((success, errors, warnings, combined))
    }

    async fn build_npm(&self, workspace: &Path) -> Result<(bool, usize, usize, String)> {
        let outcome = run_command(
            self.runner.as_ref(),
            "npm",
            &["run", "build"],
            workspace,
            CODE_TOOL_TIMEOUT_SECS,
        )
        .await?;

        Ok((outcome.success(), 0, 0, outcome.stdout))
    }
}

#[derive(Deserialize)]
struct CodeBuildInput {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default = "default_system")]
    system: String,
    #[serde(default)]
    release: bool,
    #[serde(default)]
    clean: bool,
}

fn default_system() -> String {
    "auto".to_string()
}

#[derive(Serialize, Deserialize)]
struct BuildOutput {
    success: bool,
    build_system: String,
    errors: usize,
    warnings: usize,
    duration_ms: u64,
    raw_output: String,
}

#[async_trait]
impl Tool for CodeBuildTool {
    fn name(&self) -> &str {
        "code.build"
    }

    fn description(&self) -> &str {
        "Build projects with auto-detected build system. Supports Cargo and NPM."
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
                "system": {
                    "type": "string",
                    "enum": ["auto", "cargo", "npm"],
                    "default": "auto"
                },
                "release": {"type": "boolean", "default": false},
                "clean": {"type": "boolean", "default": false}
            }
        ,
            "required": []})
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: CodeBuildInput = serde_json::from_value(input)?;

        // Resolve workspace: prefer repo param, fall back to path, then error
        let workspace = if let Some(ref repo) = input.repo {
            let base = self
                .allowed_paths
                .first()
                .ok_or_else(|| eyre!("No workspace"))?;
            match super::utils::resolve_repo_path(repo, base) {
                Ok(p) => p,
                Err(e) => {
                    return Ok(ToolResult::failure(
                        format!("Build failed: {}", e),
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
                .ok_or_else(|| eyre!("No workspace"))?;
            let hint = super::utils::checked_out_note(base);
            return Ok(ToolResult::failure(
                format!(
                    "Missing 'repo' parameter. Specify which repo to build. {}",
                    hint
                ),
                start.elapsed().as_millis() as u64,
            ));
        };

        match self
            .run_build(&workspace, &input.system, input.release, input.clean)
            .await
        {
            Ok(output) => Ok(ToolResult::success(
                serde_json::to_value(output)?,
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Build failed: {}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}
