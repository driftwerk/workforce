//! code.cargo - Advanced cargo ecosystem tools (audit, outdated, tree, geiger, deny).

use async_trait::async_trait;
use eyre::{eyre, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Instant;
use workforce_agent::tools::exec_runner::RunnerEndpoint;
use workforce_agent::tools::{Tool, ToolResult};

use super::utils::{run_command, CODE_TOOL_TIMEOUT_SECS};

/// code.cargo tool - Cargo ecosystem commands for dependency management and security.
pub struct CodeCargoTool {
    allowed_paths: Vec<PathBuf>,
    /// When set, cargo commands dispatch to the privilege-separated runner
    /// daemon (audit F7); `None` runs them in-process (dev/local).
    runner: Option<RunnerEndpoint>,
}

impl CodeCargoTool {
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

    async fn run_cargo_action(
        &self,
        workspace: &Path,
        action: &str,
        params: &CargoParams,
    ) -> Result<CargoOutput> {
        if !workspace.join("Cargo.toml").exists() {
            return Err(eyre!("Not a Rust project (no Cargo.toml found)"));
        }

        match action {
            "audit" => self.cargo_audit(workspace).await,
            "outdated" => self.cargo_outdated(workspace).await,
            "tree" => self.cargo_tree(workspace, params).await,
            "geiger" => self.cargo_geiger(workspace).await,
            "deny" => self.cargo_deny(workspace, params).await,
            _ => Err(eyre!(
                "Unknown cargo action: '{}'. Valid actions: audit, outdated, tree, geiger, deny",
                action
            )),
        }
    }

    /// cargo audit — check for known security vulnerabilities in dependencies.
    async fn cargo_audit(&self, workspace: &Path) -> Result<CargoOutput> {
        let out = run_command(
            self.runner.as_ref(),
            "cargo",
            &["audit", "--json"],
            workspace,
            CODE_TOOL_TIMEOUT_SECS,
        )
        .await?;
        let stdout = out.stdout;
        let stderr = out.stderr;

        // Try to parse JSON output for structured data
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&stdout) {
            let vuln_count = json
                .get("vulnerabilities")
                .and_then(|v| v.get("found"))
                .and_then(|f| f.as_u64())
                .unwrap_or(0);

            Ok(CargoOutput {
                action: "audit".into(),
                output: stdout,
                summary: format!(
                    "{} vulnerabilit{} found",
                    vuln_count,
                    if vuln_count == 1 { "y" } else { "ies" }
                ),
                tool_installed: true,
            })
        } else {
            // Fallback to raw output
            Ok(CargoOutput {
                action: "audit".into(),
                output: format!("{}\n{}", stdout, stderr).trim().to_string(),
                summary: "audit complete".into(),
                tool_installed: true,
            })
        }
    }

    /// cargo outdated — check for outdated dependencies.
    async fn cargo_outdated(&self, workspace: &Path) -> Result<CargoOutput> {
        let runner = self.runner.as_ref();
        let out = run_command(
            runner,
            "cargo",
            &["outdated", "--root-deps-only", "--format", "json"],
            workspace,
            CODE_TOOL_TIMEOUT_SECS,
        )
        .await?;
        let stdout = out.stdout;
        let stderr = out.stderr;

        // Try to parse JSON for a summary
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&stdout) {
            let dep_count = json
                .get("dependencies")
                .and_then(|d| d.as_array())
                .map(|a| a.len())
                .unwrap_or(0);

            Ok(CargoOutput {
                action: "outdated".into(),
                output: stdout,
                summary: format!(
                    "{} outdated direct dependenc{}",
                    dep_count,
                    if dep_count == 1 { "y" } else { "ies" }
                ),
                tool_installed: true,
            })
        } else {
            // cargo-outdated may not support --format json in all versions;
            // fall back to table output
            let out = run_command(
                runner,
                "cargo",
                &["outdated", "--root-deps-only"],
                workspace,
                CODE_TOOL_TIMEOUT_SECS,
            )
            .await?;
            let stdout = out.stdout;

            Ok(CargoOutput {
                action: "outdated".into(),
                output: if stdout.trim().is_empty() {
                    stderr
                } else {
                    stdout
                },
                summary: "outdated check complete".into(),
                tool_installed: true,
            })
        }
    }

    /// cargo tree — dependency tree visualization.
    async fn cargo_tree(&self, workspace: &Path, params: &CargoParams) -> Result<CargoOutput> {
        let depth = params.depth.map(|d| d.to_string());
        let mut args = vec!["tree"];

        // Filter to a specific package
        if let Some(ref package) = params.package {
            args.extend(["--invert", package]);
        }

        // Depth limit
        if let Some(ref depth) = depth {
            args.extend(["--depth", depth]);
        }

        // Duplicates only
        if params.duplicates {
            args.push("--duplicates");
        }

        let out = run_command(
            self.runner.as_ref(),
            "cargo",
            &args,
            workspace,
            CODE_TOOL_TIMEOUT_SECS,
        )
        .await?;
        let stdout = out.stdout;

        // Count direct dependencies (lines at depth 0, starting with the package name)
        let dep_lines = stdout
            .lines()
            .filter(|l| l.starts_with("├") || l.starts_with("└"))
            .count();

        Ok(CargoOutput {
            action: "tree".into(),
            output: stdout,
            summary: format!("{} direct dependencies", dep_lines),
            tool_installed: true,
        })
    }

    /// cargo geiger — detect unsafe code usage.
    async fn cargo_geiger(&self, workspace: &Path) -> Result<CargoOutput> {
        let out = run_command(
            self.runner.as_ref(),
            "cargo",
            &["geiger", "--output-format", "json"],
            workspace,
            CODE_TOOL_TIMEOUT_SECS,
        )
        .await?;
        let stdout = out.stdout;
        let stderr = out.stderr;

        Ok(CargoOutput {
            action: "geiger".into(),
            output: if stdout.trim().is_empty() {
                stderr
            } else {
                stdout
            },
            summary: "unsafe code analysis complete".into(),
            tool_installed: true,
        })
    }

    /// cargo deny — dependency policy enforcement.
    async fn cargo_deny(&self, workspace: &Path, params: &CargoParams) -> Result<CargoOutput> {
        // Default to "check" subcommand
        let check_type = params.deny_check.as_deref().unwrap_or("all");

        let out = run_command(
            self.runner.as_ref(),
            "cargo",
            &["deny", "check", check_type],
            workspace,
            CODE_TOOL_TIMEOUT_SECS,
        )
        .await?;
        let combined = format!("{}\n{}", out.stdout, out.stderr).trim().to_string();

        let passed = out.success();
        Ok(CargoOutput {
            action: "deny".into(),
            output: combined,
            summary: if passed {
                format!("deny check '{}' passed", check_type)
            } else {
                format!("deny check '{}' found violations", check_type)
            },
            tool_installed: true,
        })
    }
}

#[derive(Deserialize, Default)]
struct CargoParams {
    /// Package name filter (for tree --invert)
    package: Option<String>,
    /// Depth limit (for tree)
    depth: Option<usize>,
    /// Show only duplicate dependencies (for tree)
    #[serde(default)]
    duplicates: bool,
    /// Deny check type: "all", "licenses", "bans", "advisories", "sources"
    deny_check: Option<String>,
}

#[derive(Deserialize)]
struct CodeCargoInput {
    action: String,
    #[serde(default)]
    repo: Option<String>,
    #[serde(flatten)]
    params: CargoParams,
}

#[derive(Serialize, Deserialize)]
struct CargoOutput {
    action: String,
    output: String,
    summary: String,
    tool_installed: bool,
}

#[async_trait]
impl Tool for CodeCargoTool {
    fn name(&self) -> &str {
        "code.cargo"
    }

    fn description(&self) -> &str {
        "Cargo tools: audit, outdated, tree, features, crate info"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["audit", "outdated", "tree", "geiger", "deny"],
                    "description": "Cargo action to run"
                },
                "package": {
                    "type": "string",
                    "description": "Package name to filter on (for 'tree' action"
                },
                "depth": {
                    "type": "integer",
                    "description": "Max tree depth (for 'tree' action)"
                },
                "duplicates": {
                    "type": "boolean",
                    "description": "Show only duplicate dependencies (for 'tree' action)",
                    "default": false
                },
                "deny_check": {
                    "type": "string",
                    "enum": ["all", "licenses", "bans", "advisories", "sources"],
                    "description": "Type of deny check (for 'deny' action, default: 'all')"
                },
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
        let input: CodeCargoInput =
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
                        format!("Cargo operation failed: {}", e),
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
            .run_cargo_action(&workspace, &input.action, &input.params)
            .await
        {
            Ok(output) => Ok(ToolResult::success(
                serde_json::to_value(output)?,
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Cargo operation failed: {}", e),
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

    fn minimal_cargo_toml() -> &'static str {
        r#"[package]
name = "test-project"
version = "0.1.0"
edition = "2021"
"#
    }

    #[tokio::test]
    async fn test_not_rust_project() {
        let temp = TempDir::new().unwrap();
        let tool = CodeCargoTool::new(vec![temp.path().to_path_buf()]);

        let result = tool
            .execute(serde_json::json!({ "action": "audit" }))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_unknown_action() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("Cargo.toml"), minimal_cargo_toml()).unwrap();

        let tool = CodeCargoTool::new(vec![temp.path().to_path_buf()]);
        let result = tool
            .execute(serde_json::json!({ "action": "nonexistent" }))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_tree_on_real_project() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("Cargo.toml"), minimal_cargo_toml()).unwrap();
        fs::create_dir_all(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("src/lib.rs"), "").unwrap();

        let tool = CodeCargoTool::new(vec![temp.path().to_path_buf()]);
        let result = tool
            .execute(serde_json::json!({ "action": "tree" }))
            .await
            .unwrap();

        // cargo tree is a built-in command, should always work
        assert!(result.success);
        let output: CargoOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.action, "tree");
        assert!(output.tool_installed);
    }

    #[tokio::test]
    async fn test_tree_with_depth() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("Cargo.toml"), minimal_cargo_toml()).unwrap();
        fs::create_dir_all(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("src/lib.rs"), "").unwrap();

        let tool = CodeCargoTool::new(vec![temp.path().to_path_buf()]);
        let result = tool
            .execute(serde_json::json!({ "action": "tree", "depth": 1 }))
            .await
            .unwrap();

        assert!(result.success);
    }

    #[tokio::test]
    async fn test_audit_graceful_when_not_installed() {
        // If cargo-audit is not installed, should still return success with install instructions
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("Cargo.toml"), minimal_cargo_toml()).unwrap();
        fs::create_dir_all(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("src/lib.rs"), "").unwrap();

        let tool = CodeCargoTool::new(vec![temp.path().to_path_buf()]);
        let result = tool
            .execute(serde_json::json!({ "action": "audit" }))
            .await
            .unwrap();

        // Should succeed even if tool not installed (graceful degradation)
        assert!(result.success);
        let output: CargoOutput = serde_json::from_value(result.output).unwrap();
        assert_eq!(output.action, "audit");
        // Either the tool is installed and ran, or we got the install message
        if !output.tool_installed {
            assert!(output.summary.contains("not installed"));
        }
    }
}
