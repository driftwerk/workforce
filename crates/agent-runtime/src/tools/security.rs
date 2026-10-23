//! Security audit tool for agent workspaces.
//!
//! Runs configurable security checks on the agent's workspace:
//! - **permissions**: File/directory permission auditing (modes, symlinks)
//! - **code**: Dangerous code pattern scanning (eval, child_process, crypto mining, etc.)
//! - **secrets**: Detection of accidentally committed API keys, tokens, PEM blocks
//!
//! Optionally fixes safe, reversible issues (file permissions only).

use std::path::{Path, PathBuf};
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;

use super::{Tool, ToolResult};

/// Maximum file size to scan for secrets (1 MB).
const MAX_SECRET_SCAN_SIZE: u64 = 1_048_576;

/// Maximum files to walk for secret scanning.
const MAX_SECRET_SCAN_FILES: usize = 500;

/// File extensions to check for accidentally committed secrets.
const SECRET_SCAN_EXTENSIONS: &[&str] = &[
    "env",
    "toml",
    "yaml",
    "yml",
    "json",
    "cfg",
    "ini",
    "conf",
    "properties",
    "js",
    "ts",
    "mjs",
    "cjs",
    "py",
    "sh",
    "bash",
    "rb",
    "txt",
    "md",
];

/// Directories to skip when walking for secret detection.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "__pycache__",
    "target",
    ".venv",
    "vendor",
    "dist",
    "build",
];

/// Valid check names accepted by the tool.
const VALID_CHECKS: &[&str] = &["permissions", "code", "secrets"];

/// Tool for running security audits on the agent workspace.
pub struct SecurityAuditTool {
    allowed_paths: Vec<PathBuf>,
}

impl SecurityAuditTool {
    pub fn new(allowed_paths: Vec<PathBuf>) -> Self {
        Self { allowed_paths }
    }

    /// Return the canonical workspace root. Returns None if the workspace
    /// cannot be resolved (no allowed paths or canonicalization fails).
    fn workspace_canonical(&self) -> Option<PathBuf> {
        let ws = self.allowed_paths.first()?;
        // If the workspace path cannot be canonicalized, deny access rather
        // than falling back to an un-resolved path.
        ws.canonicalize().ok()
    }

    /// Resolve a user-provided sub-path against the workspace root.
    /// Returns the canonical path, or None if it escapes the workspace.
    fn resolve_path(&self, sub_path: Option<&str>) -> Option<PathBuf> {
        let ws_canonical = self.workspace_canonical()?;
        let workspace = self.allowed_paths.first()?;
        match sub_path {
            Some(rel) => {
                let resolved = workspace.join(rel);
                if resolved.exists() {
                    let canonical = resolved.canonicalize().ok()?;
                    if canonical.starts_with(&ws_canonical) {
                        Some(canonical)
                    } else {
                        None
                    }
                } else {
                    // Path doesn't exist — check parent to distinguish
                    // "not found" from "path escape"
                    let parent = resolved.parent()?;
                    let parent_canonical = parent.canonicalize().ok()?;
                    if parent_canonical.starts_with(&ws_canonical) {
                        Some(resolved)
                    } else {
                        None
                    }
                }
            }
            None => Some(ws_canonical),
        }
    }
}

#[derive(Deserialize)]
struct AuditInput {
    #[serde(default)]
    checks: Option<Vec<String>>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    fix: Option<bool>,
}

#[async_trait]
impl Tool for SecurityAuditTool {
    fn name(&self) -> &str {
        "security.audit"
    }

    fn description(&self) -> &str {
        "Run security checks: permissions, code patterns, secrets"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "checks": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["permissions", "code", "secrets"] },
                    "description": "Which checks to run (default: all)"
                },
                "path": {
                    "type": "string",
                    "description": "Subdirectory within workspace to audit (default"
                },
                "fix": {
                    "type": "boolean",
                    "description": "Auto-fix safe issues like file permissions (default: false)"
                }
            }
        ,
            "required": []})
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();

        let input: AuditInput =
            serde_json::from_value(input).map_err(|e| eyre::eyre!("Invalid input: {}", e))?;

        let ws_canonical = match self.workspace_canonical() {
            Some(p) => p,
            None => {
                return Ok(ToolResult::failure(
                    "No workspace configured or workspace path cannot be resolved",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let target = match self.resolve_path(input.path.as_deref()) {
            Some(p) => p,
            None => {
                return Ok(ToolResult::failure(
                    "Path is outside the allowed workspace",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        if !target.exists() {
            return Ok(ToolResult::failure(
                format!("Path does not exist: {}", target.display()),
                start.elapsed().as_millis() as u64,
            ));
        }

        let do_fix = input.fix.unwrap_or(false);
        let checks: Vec<String> = input.checks.unwrap_or_else(|| {
            vec![
                "permissions".to_string(),
                "code".to_string(),
                "secrets".to_string(),
            ]
        });

        // Validate check names
        for check in &checks {
            if !VALID_CHECKS.contains(&check.as_str()) {
                return Ok(ToolResult::failure(
                    format!(
                        "Unknown check '{}'. Valid checks: {}",
                        check,
                        VALID_CHECKS.join(", ")
                    ),
                    start.elapsed().as_millis() as u64,
                ));
            }
        }

        let mut state = AuditState::default();

        // --- Permissions check ---
        // Always check permissions relative to workspace root, not the sub-target
        if checks.iter().any(|c| c == "permissions") {
            run_permissions_check(&ws_canonical, do_fix, &ws_canonical, &mut state);
        }

        // --- Code check ---
        if checks.iter().any(|c| c == "code") {
            run_code_check(&target, &mut state);
        }

        // --- Secrets check ---
        if checks.iter().any(|c| c == "secrets") {
            run_secrets_check(&target, &ws_canonical, &mut state);
        }

        let total = state.critical + state.warning + state.info;
        let output = serde_json::json!({
            "summary": {
                "total": total,
                "critical": state.critical,
                "warning": state.warning,
                "info": state.info,
                "fixed": state.fixed,
            },
            "checks_run": checks,
            "findings": state.findings,
        });

        Ok(ToolResult::success(
            output,
            start.elapsed().as_millis() as u64,
        ))
    }
}

/// Mutable state accumulated during an audit run.
#[derive(Default)]
struct AuditState {
    findings: Vec<serde_json::Value>,
    critical: u32,
    warning: u32,
    info: u32,
    fixed: u32,
}

fn run_permissions_check(
    workspace: &Path,
    do_fix: bool,
    ws_canonical: &Path,
    state: &mut AuditState,
) {
    let perm_checks = workforce_common::permissions::audit_permissions(
        &workspace.join("workforce.toml"),
        workspace,
    );

    for check in perm_checks {
        let severity = match check.severity {
            workforce_common::permissions::PermSeverity::Critical => {
                state.critical += 1;
                "critical"
            }
            workforce_common::permissions::PermSeverity::Warning => {
                state.warning += 1;
                "warning"
            }
            workforce_common::permissions::PermSeverity::Ok => continue,
        };

        let was_fixed = if do_fix {
            try_fix_permissions(&check.path, ws_canonical, &mut state.fixed)
        } else {
            false
        };

        state.findings.push(serde_json::json!({
            "check": "permissions",
            "severity": severity,
            "path": check.path,
            "message": check.message,
            "rule": "file.perms",
            "fixed": was_fixed,
        }));
    }
}

/// Attempt to fix file permissions. Returns true if fixed.
/// Only modifies paths that are verified to be within the workspace.
#[cfg(unix)]
fn try_fix_permissions(path_str: &str, ws_canonical: &Path, fixed: &mut u32) -> bool {
    use std::os::unix::fs::PermissionsExt;

    let path = Path::new(path_str);

    // Re-validate that the path is within the workspace before modifying
    let canonical = match path.canonicalize() {
        Ok(c) => c,
        Err(_) => return false,
    };
    if !canonical.starts_with(ws_canonical) {
        return false;
    }

    // Use symlink_metadata to avoid following symlinks
    let Ok(meta) = std::fs::symlink_metadata(&canonical) else {
        return false;
    };
    if meta.file_type().is_symlink() {
        return false; // Don't chmod through symlinks
    }

    let target_mode = if meta.is_dir() { 0o700 } else { 0o600 };
    let perms = std::fs::Permissions::from_mode(target_mode);
    if std::fs::set_permissions(&canonical, perms).is_ok() {
        *fixed += 1;
        true
    } else {
        false
    }
}

#[cfg(not(unix))]
fn try_fix_permissions(_path_str: &str, _ws_canonical: &Path, _fixed: &mut u32) -> bool {
    false
}

fn run_code_check(target: &Path, state: &mut AuditState) {
    let code_findings = workforce_common::security_scanner::scan_directory(target);

    for finding in code_findings {
        let severity = match finding.severity {
            workforce_common::security_scanner::Severity::Critical => {
                state.critical += 1;
                "critical"
            }
            workforce_common::security_scanner::Severity::Warning => {
                state.warning += 1;
                "warning"
            }
            workforce_common::security_scanner::Severity::Info => {
                state.info += 1;
                "info"
            }
        };

        state.findings.push(serde_json::json!({
            "check": "code",
            "severity": severity,
            "path": finding.file,
            "line": finding.line,
            "message": finding.description,
            "rule": finding.rule,
            "matched_line": finding.matched_line,
            "fixed": false,
        }));
    }
}

fn run_secrets_check(target: &Path, ws_canonical: &Path, state: &mut AuditState) {
    let mut file_count = 0usize;
    walk_for_secrets(target, ws_canonical, state, &mut file_count);
}

fn walk_for_secrets(
    dir: &Path,
    ws_canonical: &Path,
    state: &mut AuditState,
    file_count: &mut usize,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        if *file_count >= MAX_SECRET_SCAN_FILES {
            return;
        }

        let path = entry.path();

        // Use symlink_metadata to avoid following symlinks outside workspace
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };

        if meta.file_type().is_symlink() {
            // Only follow symlinks that resolve within workspace
            if let Ok(canonical) = path.canonicalize() {
                if !canonical.starts_with(ws_canonical) {
                    continue;
                }
            } else {
                continue; // Broken symlink
            }
        }

        if meta.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if SKIP_DIRS.contains(&name) {
                continue;
            }
            walk_for_secrets(&path, ws_canonical, state, file_count);
        } else if meta.is_file() {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if SECRET_SCAN_EXTENSIONS.contains(&ext) || filename.starts_with(".env") {
                *file_count += 1;
                check_file_for_secrets(&path, state);
            }
        }
    }
}

fn check_file_for_secrets(path: &Path, state: &mut AuditState) {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return,
    };
    if meta.len() > MAX_SECRET_SCAN_SIZE {
        return;
    }

    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return, // Skip binary files
    };

    let file_str = path.display().to_string();

    // Check line-by-line for more granular reporting
    for (line_num, line) in content.lines().enumerate() {
        if workforce_common::redact::contains_secret(line) {
            state.critical += 1;
            state.findings.push(serde_json::json!({
                "check": "secrets",
                "severity": "critical",
                "path": file_str,
                "line": line_num + 1,
                "message": "Line contains potential secrets (API keys, tokens, or credentials)",
                "rule": "secret.detected",
                "fixed": false,
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_tool(workspace: &Path) -> SecurityAuditTool {
        SecurityAuditTool::new(vec![workspace.to_path_buf()])
    }

    #[tokio::test]
    async fn clean_workspace_no_critical() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("main.js"), "console.log('hello');").unwrap();

        let tool = make_tool(tmp.path());
        let result = tool.execute(serde_json::json!({})).await.unwrap();

        assert!(result.success);
        assert_eq!(result.output["summary"]["critical"], 0);
    }

    #[tokio::test]
    async fn empty_workspace_succeeds() {
        let tmp = tempfile::tempdir().unwrap();

        let tool = make_tool(tmp.path());
        let result = tool.execute(serde_json::json!({})).await.unwrap();

        assert!(result.success);
        assert_eq!(result.output["summary"]["critical"], 0);
    }

    #[tokio::test]
    async fn detects_eval_in_code() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("bad.js"), "const x = eval(input);").unwrap();

        let tool = make_tool(tmp.path());
        let result = tool
            .execute(serde_json::json!({"checks": ["code"]}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output["summary"]["critical"].as_u64().unwrap() > 0);
        let findings = result.output["findings"].as_array().unwrap();
        assert!(findings.iter().any(|f| f["rule"] == "dynamic-code"));
    }

    #[tokio::test]
    async fn detects_secret_in_file() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join(".env"),
            concat!("API_KEY=sk-ant", "-oat01-abcdefghijklmnop1234567890"),
        )
        .unwrap();

        let tool = make_tool(tmp.path());
        let result = tool
            .execute(serde_json::json!({"checks": ["secrets"]}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output["summary"]["critical"].as_u64().unwrap() > 0);
        let findings = result.output["findings"].as_array().unwrap();
        assert!(findings.iter().any(|f| f["rule"] == "secret.detected"));
        // Should include line number
        assert!(findings.iter().any(|f| f["line"].as_u64().is_some()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn detects_bad_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("workforce.toml");
        fs::write(&config, "[gateway]\nauth_token = \"test\"").unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o644)).unwrap();

        let tool = make_tool(tmp.path());
        let result = tool
            .execute(serde_json::json!({"checks": ["permissions"]}))
            .await
            .unwrap();

        assert!(result.success);
        let findings = result.output["findings"].as_array().unwrap();
        assert!(findings.iter().any(|f| f["check"] == "permissions"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fix_mode_corrects_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("workforce.toml");
        fs::write(&config, "[gateway]").unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o644)).unwrap();

        let tool = make_tool(tmp.path());
        let result = tool
            .execute(serde_json::json!({"checks": ["permissions"], "fix": true}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output["summary"]["fixed"].as_u64().unwrap() > 0);

        // Verify permissions were actually fixed
        let meta = fs::metadata(&config).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[tokio::test]
    async fn path_sandboxing_blocks_escape() {
        let tmp = tempfile::tempdir().unwrap();

        let tool = make_tool(tmp.path());
        let result = tool
            .execute(serde_json::json!({"path": "../../etc"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("outside"));
    }

    #[tokio::test]
    async fn subset_checks_only_runs_selected() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("bad.js"), "eval('x')").unwrap();
        fs::write(
            tmp.path().join(".env"),
            concat!("KEY=sk-ant", "-oat01-abcdefghijklmnop1234567890"),
        )
        .unwrap();

        let tool = make_tool(tmp.path());
        let result = tool
            .execute(serde_json::json!({"checks": ["permissions"]}))
            .await
            .unwrap();

        assert!(result.success);
        let findings = result.output["findings"].as_array().unwrap();
        assert!(findings.iter().all(|f| f["check"] == "permissions"));
        assert_eq!(
            result.output["checks_run"].as_array().unwrap(),
            &[serde_json::json!("permissions")]
        );
    }

    #[tokio::test]
    async fn nonexistent_path_fails_gracefully() {
        let tmp = tempfile::tempdir().unwrap();

        let tool = make_tool(tmp.path());
        let result = tool
            .execute(serde_json::json!({"path": "does_not_exist"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("does not exist"));
    }

    #[tokio::test]
    async fn unknown_check_name_rejected() {
        let tmp = tempfile::tempdir().unwrap();

        let tool = make_tool(tmp.path());
        let result = tool
            .execute(serde_json::json!({"checks": ["cde"]}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown check"));
    }
}
