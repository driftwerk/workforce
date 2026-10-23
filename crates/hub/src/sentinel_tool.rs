//! Sentinel security visibility tools.
//!
//! These tools allow agents to inspect the current Sentinel configuration,
//! query scan history, and manually test content against the security scanner.

use std::collections::VecDeque;
use std::time::Instant;

use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::RwLock;

use workforce_agent::tools::{Tool, ToolResult};

// ─── Sentinel Tool Context ─────────────────────────────────────────────────

/// Maximum number of scan records kept in the in-memory ring buffer.
const MAX_SCAN_HISTORY: usize = 100;

/// A record of a past sentinel scan.
#[derive(Clone, serde::Serialize)]
pub struct ScanRecord {
    pub timestamp: String,
    pub path: String,
    pub total_findings: usize,
    pub critical: u32,
    pub warning: u32,
    pub info: u32,
    pub has_critical: bool,
    pub findings: Vec<serde_json::Value>,
}

/// Shared context for Sentinel tools.
#[derive(Clone)]
pub struct SentinelToolContext {
    pub scan_history: Arc<RwLock<VecDeque<ScanRecord>>>,
    pub identity: Arc<RwLock<workforce_common::identity::AgentIdentity>>,
}

/// Create all Sentinel visibility tools.
pub fn sentinel_tools(ctx: SentinelToolContext) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(SentinelStatusTool::new()),
        Arc::new(SentinelScanTool::new(ctx.clone())),
        Arc::new(SentinelHistoryTool::new(ctx.clone())),
        Arc::new(SentinelIntegrityTool::new(ctx)),
    ]
}

// ─── sentinel.status ───────────────────────────────────────────────────────

pub struct SentinelStatusTool {}

impl Default for SentinelStatusTool {
    fn default() -> Self {
        Self::new()
    }
}

impl SentinelStatusTool {
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl Tool for SentinelStatusTool {
    fn name(&self) -> &str {
        "sentinel.status"
    }

    fn description(&self) -> &str {
        "Expose current Sentinel security configuration and runtime state"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        ,
            "required": []})
    }

    async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();

        // Read workforce.toml to extract [sentinel] and [pairing] config
        let possible_paths = vec![
            std::path::PathBuf::from("/app/workforce.toml"),
            std::path::PathBuf::from("./workforce.toml"),
            std::env::current_dir()
                .ok()
                .map(|p| p.join("workforce.toml"))
                .unwrap_or_else(|| std::path::PathBuf::from("workforce.toml")),
        ];

        let mut config_content = None;
        for path in &possible_paths {
            if let Ok(content) = tokio::fs::read_to_string(&path).await {
                config_content = Some(content);
                break;
            }
        }

        let config_str = config_content
            .ok_or_else(|| eyre::eyre!("Could not find workforce.toml in expected locations"))?;

        let toml_value: toml::Value = toml::from_str(&config_str)?;

        // Extract [sentinel] section
        let sentinel = toml_value.get("sentinel");

        let owner_slack_ids = sentinel
            .and_then(|s| s.get("owner_slack_ids"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let owner_device_ids = sentinel
            .and_then(|s| s.get("owner_device_ids"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        // Extract [pairing] section for policy visibility
        let pairing = toml_value.get("pairing");

        let channel_policy = pairing
            .and_then(|p| p.get("channel_policy"))
            .and_then(|v| v.as_str())
            .unwrap_or("open")
            .to_string();

        let device_pairing_enabled = pairing
            .and_then(|p| p.get("device_pairing_enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let node_pairing_enabled = pairing
            .and_then(|p| p.get("node_pairing_enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Build trust classification rules documentation
        let trust_rules = serde_json::json!({
            "System": "Identity files, policy rules, system prompts",
            "Owner": "Slack users in owner_slack_ids OR devices in owner_device_ids",
            "Paired": "Users/devices on pairing allowlist (not owner-level)",
            "Untrusted": "All other sources (Web, GitHub, Linear, Webhook always untrusted)"
        });

        // Build pairing policy explanation
        let pairing_explanation = serde_json::json!({
            "channel_policy": channel_policy,
            "device_pairing_enabled": device_pairing_enabled,
            "node_pairing_enabled": node_pairing_enabled,
            "policy_meanings": {
                "open": "No restrictions — all users allowed",
                "allowlist": "Only pre-approved users allowed (no pairing codes)",
                "pairing": "Users must request pairing via code; admin approval required",
                "disabled": "No users allowed (channels locked)"
            },
            "note": "Paired users receive 'Paired' trust level. Use CLI 'workforce pairing' commands to manage."
        });

        // Pattern counts (static for now — scanning patterns are not currently implemented)
        let pattern_counts = serde_json::json!({
            "prompt_injection": 0,
            "exfiltration": 0,
            "command_injection": 0,
            "note": "Pattern scanning not yet implemented — wrapping is active for untrusted content"
        });

        let result = serde_json::json!({
            "enabled": true,
            "owner_slack_ids": owner_slack_ids,
            "owner_slack_ids_count": owner_slack_ids.len(),
            "owner_device_ids": owner_device_ids,
            "owner_device_ids_count": owner_device_ids.len(),
            "pairing_policy": pairing_explanation,
            "trust_classification_rules": trust_rules,
            "pattern_counts": pattern_counts,
            "how_to_check_allowlist": "Use CLI: `workforce pairing status` to see paired users/channels/devices",
        });

        Ok(ToolResult::success(
            result,
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ─── sentinel.scan ──────────────────────────────────────────────────────────

pub struct SentinelScanTool {
    ctx: SentinelToolContext,
}

impl SentinelScanTool {
    pub fn new(ctx: SentinelToolContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for SentinelScanTool {
    fn name(&self) -> &str {
        "sentinel.scan"
    }

    fn description(&self) -> &str {
        "Scan a directory for security findings"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory path to scan for security findings"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = std::time::Instant::now();

        let path = input
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre::eyre!("Missing required parameter: path"))?;

        let dir = std::path::Path::new(path);
        if !dir.exists() {
            return Ok(ToolResult::failure(
                format!("Path does not exist: {}", path),
                start.elapsed().as_millis() as u64,
            ));
        }

        let findings = workforce_common::security_scanner::scan_directory(dir);

        let mut critical = 0u32;
        let mut warning = 0u32;
        let mut info_count = 0u32;

        let findings_json: Vec<serde_json::Value> = findings
            .iter()
            .map(|f| {
                match f.severity {
                    workforce_common::security_scanner::Severity::Critical => critical += 1,
                    workforce_common::security_scanner::Severity::Warning => warning += 1,
                    workforce_common::security_scanner::Severity::Info => info_count += 1,
                }
                serde_json::json!({
                    "rule": f.rule,
                    "severity": format!("{:?}", f.severity),
                    "description": f.description,
                    "file": f.file,
                    "line": f.line,
                    "matched_line": workforce_common::text::truncate_str(&f.matched_line, 200),
                })
            })
            .collect();

        let has_critical = workforce_common::security_scanner::has_critical_findings(&findings);

        // Record in scan history ring buffer
        let record = ScanRecord {
            timestamp: chrono::Utc::now().to_rfc3339(),
            path: path.to_string(),
            total_findings: findings.len(),
            critical,
            warning,
            info: info_count,
            has_critical,
            findings: findings_json.clone(),
        };
        {
            let mut history = self.ctx.scan_history.write().await;
            if history.len() >= MAX_SCAN_HISTORY {
                history.pop_front();
            }
            history.push_back(record);
        }

        Ok(ToolResult::success(
            serde_json::json!({
                "path": path,
                "total_findings": findings.len(),
                "summary": {
                    "critical": critical,
                    "warning": warning,
                    "info": info_count,
                },
                "has_critical": has_critical,
                "findings": findings_json,
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ─── sentinel.history ────────────────────────────────────────────────────────

pub struct SentinelHistoryTool {
    ctx: SentinelToolContext,
}

impl SentinelHistoryTool {
    pub fn new(ctx: SentinelToolContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for SentinelHistoryTool {
    fn name(&self) -> &str {
        "sentinel.history"
    }

    fn description(&self) -> &str {
        "View recent sentinel scan results from the in-memory history buffer"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Max number of recent scans to return (default: 10, max: 100)"
                }
            }
        ,
            "required": []})
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();

        let limit = input
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(10)
            .min(100) as usize;

        let history = self.ctx.scan_history.read().await;
        let total = history.len();

        // Return the most recent scans (end of the deque)
        let scans: Vec<serde_json::Value> = history
            .iter()
            .rev()
            .take(limit)
            .map(|r| {
                serde_json::json!({
                    "timestamp": r.timestamp,
                    "path": r.path,
                    "total_findings": r.total_findings,
                    "summary": {
                        "critical": r.critical,
                        "warning": r.warning,
                        "info": r.info,
                    },
                    "has_critical": r.has_critical,
                    "findings": r.findings,
                })
            })
            .collect();

        Ok(ToolResult::success(
            serde_json::json!({
                "total_scans_in_history": total,
                "returned": scans.len(),
                "scans": scans,
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ─── sentinel.integrity ──────────────────────────────────────────────────────

pub struct SentinelIntegrityTool {
    ctx: SentinelToolContext,
}

impl SentinelIntegrityTool {
    pub fn new(ctx: SentinelToolContext) -> Self {
        Self { ctx }
    }
}

/// Critical identity files — modifications to these are Critical severity.
const CRITICAL_IDENTITY_FILES: &[&str] = &["SOUL.md", "AGENTS.md", "IDENTITY.md"];

#[async_trait]
impl Tool for SentinelIntegrityTool {
    fn name(&self) -> &str {
        "sentinel.integrity"
    }

    fn description(&self) -> &str {
        "Verify integrity of identity files (SOUL.md, AGENTS.md, etc.) against hashes"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "refresh": {
                    "type": "boolean",
                    "description": "If true, include full details"
                }
            }
        ,
            "required": []})
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let refresh = input
            .get("refresh")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let violations = {
            let identity = self.ctx.identity.read().await;
            identity.verify_integrity()
        };

        let findings: Vec<serde_json::Value> = violations
            .iter()
            .map(|v| {
                let severity = if CRITICAL_IDENTITY_FILES.iter().any(|f| *f == v.file) {
                    "Critical"
                } else {
                    "Warning"
                };
                serde_json::json!({
                    "file": v.file,
                    "violation_type": v.violation_type,
                    "severity": severity,
                    "expected_hash": &v.expected_hash[..16.min(v.expected_hash.len())],
                    "actual_hash": v.actual_hash.as_ref().map(|h| &h[..16.min(h.len())]),
                })
            })
            .collect();

        let has_critical = violations
            .iter()
            .any(|v| CRITICAL_IDENTITY_FILES.iter().any(|f| *f == v.file));

        if refresh {
            let mut identity = self.ctx.identity.write().await;
            identity.refresh_hashes();
        }

        Ok(ToolResult::success(
            serde_json::json!({
                "total_violations": violations.len(),
                "has_critical": has_critical,
                "refreshed": refresh,
                "findings": findings,
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}
