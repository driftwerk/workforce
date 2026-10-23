//! Heartbeat check management tools — let agents list, create, update, delete,
//! and trigger heartbeat checks.
//!
//! Tools:
//! - `heartbeat.list`    — list all heartbeat checks
//! - `heartbeat.read`    — show a single heartbeat check by ID
//! - `heartbeat.create`  — create a new heartbeat check
//! - `heartbeat.update`  — update an existing heartbeat check (partial merge)
//! - `heartbeat.delete`  — delete a heartbeat check
//! - `heartbeat.trigger` — trigger an immediate heartbeat run

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde::Deserialize;
use tracing::info;

use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::heartbeat_check::{self, HeartbeatCheck, HeartbeatCheckMeta};

use crate::heartbeat_signature::{stamp_creator, CheckSigner};
use crate::hub_core::heartbeats::HeartbeatWakeHandle;

// ─── Heartbeat Tool Context ─────────────────────────────────────────────────

/// Shared context for all heartbeat tools.
#[derive(Clone)]
pub struct HeartbeatToolContext {
    pub workspace: PathBuf,
    /// Signs the creator the create and update tools stamp on a check.
    pub signer: Option<Arc<CheckSigner>>,
    pub wake_handle: Arc<parking_lot::RwLock<Option<HeartbeatWakeHandle>>>,
    /// Floor (and default) for a check's `cooldown_secs`
    /// (`[budget] min_recurring_interval_secs`; 0 = no floor).
    pub min_cooldown_secs: u64,
}

/// A check's effective cooldown: defaulted to the floor and never below it.
/// A floor of 0 preserves the caller's value untouched.
fn floored_cooldown(requested: Option<u64>, floor: u64) -> Option<u64> {
    if floor == 0 {
        requested
    } else {
        Some(requested.unwrap_or(floor).max(floor))
    }
}

/// Create all heartbeat management tools.
pub fn heartbeat_tools(ctx: HeartbeatToolContext) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(HeartbeatListTool::new(ctx.clone())),
        Arc::new(HeartbeatReadTool::new(ctx.clone())),
        Arc::new(HeartbeatCreateTool::new(ctx.clone())),
        Arc::new(HeartbeatUpdateTool::new(ctx.clone())),
        Arc::new(HeartbeatDeleteTool::new(ctx.clone())),
        Arc::new(HeartbeatTriggerTool::new(ctx)),
    ]
}

// ─── heartbeat.list ─────────────────────────────────────────────────────────

pub struct HeartbeatListTool {
    ctx: HeartbeatToolContext,
}

impl HeartbeatListTool {
    pub fn new(ctx: HeartbeatToolContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for HeartbeatListTool {
    fn name(&self) -> &str {
        "heartbeat.list"
    }

    fn description(&self) -> &str {
        "List all heartbeat checks"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();

        match heartbeat_check::list_heartbeat_checks(&self.ctx.workspace) {
            Ok(checks) => {
                let checks_json: Vec<serde_json::Value> = checks
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "id": c.id,
                            "name": c.meta.name,
                            "enabled": c.meta.enabled,
                            "schedule": c.meta.schedule,
                            "priority": c.meta.priority,
                            "tags": c.meta.tags,
                        })
                    })
                    .collect();

                Ok(ToolResult::success(
                    serde_json::json!({ "checks": checks_json }),
                    start.elapsed().as_millis() as u64,
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to list heartbeat checks: {}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

// ─── heartbeat.read ─────────────────────────────────────────────────────────

pub struct HeartbeatReadTool {
    ctx: HeartbeatToolContext,
}

impl HeartbeatReadTool {
    pub fn new(ctx: HeartbeatToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct ReadInput {
    check_id: String,
}

#[async_trait]
impl Tool for HeartbeatReadTool {
    fn name(&self) -> &str {
        "heartbeat.read"
    }

    fn description(&self) -> &str {
        "Show the full details of a single heartbeat check by its ID, including the body."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "check_id": {
                    "type": "string",
                    "description": "The unique heartbeat check identifier (file stem)"
                }
            },
            "required": ["check_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: ReadInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        match heartbeat_check::load_heartbeat_check(&self.ctx.workspace, &params.check_id) {
            Ok(check) => Ok(ToolResult::success(
                serde_json::json!({
                    "id": check.id,
                    "name": check.meta.name,
                    "enabled": check.meta.enabled,
                    "schedule": check.meta.schedule,
                    "priority": check.meta.priority,
                    "tags": check.meta.tags,
                    "timezone": check.meta.timezone,
                    "cooldown_secs": check.meta.cooldown_secs,
                    "body": check.body,
                }),
                duration(),
            )),
            Err(e) => Ok(ToolResult::failure(
                format!(
                    "Failed to load heartbeat check '{}': {}",
                    params.check_id, e
                ),
                duration(),
            )),
        }
    }
}

// ─── heartbeat.create ───────────────────────────────────────────────────────

pub struct HeartbeatCreateTool {
    ctx: HeartbeatToolContext,
}

impl HeartbeatCreateTool {
    pub fn new(ctx: HeartbeatToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct CreateInput {
    id: String,
    name: String,
    body: String,
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default = "default_priority")]
    priority: u32,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    cooldown_secs: Option<u64>,
}

fn default_priority() -> u32 {
    5
}

#[async_trait]
impl Tool for HeartbeatCreateTool {
    fn name(&self) -> &str {
        "heartbeat.create"
    }

    fn description(&self) -> &str {
        "Create a new heartbeat check. Fails if a check with the same ID already exists."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Unique check identifier (used as filename stem)"
                },
                "name": {
                    "type": "string",
                    "description": "Human-readable check name"
                },
                "body": {
                    "type": "string",
                    "description": "Markdown body — the prompt/instructions for the agent"
                },
                "schedule": {
                    "type": "string",
                    "description": "Cron expression for scheduling (e.g. \"0 9,13,17 * * 1-5\")"
                },
                "priority": {
                    "type": "integer",
                    "description": "Priority: lower number = higher priority. Default 5."
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Tags for grouping/filtering"
                },
                "timezone": {
                    "type": "string",
                    "description": "Timezone override for the cron expression (IANA name)"
                },
                "cooldown_secs": {
                    "type": "integer",
                    "description": "Minimum seconds between two runs of this check. Defaults to (and is floored at) the configured minimum recurring interval."
                }
            },
            "required": ["id", "name", "body"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: CreateInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        // Check uniqueness — load should fail for a new check
        if heartbeat_check::load_heartbeat_check(&self.ctx.workspace, &params.id).is_ok() {
            return Ok(ToolResult::failure(
                format!("Heartbeat check '{}' already exists", params.id),
                duration(),
            ));
        }

        let mut check = HeartbeatCheck {
            id: params.id.clone(),
            meta: HeartbeatCheckMeta {
                name: params.name,
                enabled: true,
                schedule: params.schedule,
                priority: params.priority,
                tags: params.tags,
                timezone: params.timezone,
                cooldown_secs: floored_cooldown(params.cooldown_secs, self.ctx.min_cooldown_secs),
                take_action: false,
                tools: None,
                creator: None,
                creator_sig: None,
            },
            body: params.body,
        };
        stamp_creator(
            self.ctx.signer.as_deref(),
            &mut check,
            crate::user_config::current_identity_id().as_deref(),
        );

        match heartbeat_check::save_heartbeat_check(&self.ctx.workspace, &check) {
            Ok(()) => {
                info!(check_id = %params.id, "Heartbeat check created via heartbeat.create");
                Ok(ToolResult::success(
                    serde_json::json!({
                        "created": true,
                        "id": check.id,
                        "name": check.meta.name,
                    }),
                    duration(),
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to create heartbeat check '{}': {}", params.id, e),
                duration(),
            )),
        }
    }
}

// ─── heartbeat.update ───────────────────────────────────────────────────────

pub struct HeartbeatUpdateTool {
    ctx: HeartbeatToolContext,
}

impl HeartbeatUpdateTool {
    pub fn new(ctx: HeartbeatToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct UpdateInput {
    check_id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    priority: Option<u32>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

#[async_trait]
impl Tool for HeartbeatUpdateTool {
    fn name(&self) -> &str {
        "heartbeat.update"
    }

    fn description(&self) -> &str {
        "Update an existing heartbeat check"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "check_id": {
                    "type": "string",
                    "description": "The ID of the heartbeat check to update"
                },
                "name": {
                    "type": "string",
                    "description": "New human-readable name"
                },
                "body": {
                    "type": "string",
                    "description": "New markdown body"
                },
                "schedule": {
                    "type": "string",
                    "description": "New cron schedule expression"
                },
                "priority": {
                    "type": "integer",
                    "description": "New priority (lower = higher priority)"
                },
                "enabled": {
                    "type": "boolean",
                    "description": "Enable or disable the check"
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "New tags list"
                }
            },
            "required": ["check_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: UpdateInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        // Load existing check
        let mut check =
            match heartbeat_check::load_heartbeat_check(&self.ctx.workspace, &params.check_id) {
                Ok(c) => c,
                Err(e) => {
                    return Ok(ToolResult::failure(
                        format!(
                            "Failed to load heartbeat check '{}': {}",
                            params.check_id, e
                        ),
                        duration(),
                    ));
                }
            };

        // Merge only provided fields
        if let Some(name) = params.name {
            check.meta.name = name;
        }
        if let Some(body) = params.body {
            check.body = body;
        }
        if let Some(schedule) = params.schedule {
            check.meta.schedule = Some(schedule);
        }
        if let Some(priority) = params.priority {
            check.meta.priority = priority;
        }
        if let Some(enabled) = params.enabled {
            check.meta.enabled = enabled;
        }
        if let Some(tags) = params.tags {
            check.meta.tags = tags;
        }
        stamp_creator(
            self.ctx.signer.as_deref(),
            &mut check,
            crate::user_config::current_identity_id().as_deref(),
        );

        match heartbeat_check::save_heartbeat_check(&self.ctx.workspace, &check) {
            Ok(()) => {
                info!(check_id = %params.check_id, "Heartbeat check updated via heartbeat.update");
                Ok(ToolResult::success(
                    serde_json::json!({
                        "updated": true,
                        "id": check.id,
                        "name": check.meta.name,
                    }),
                    duration(),
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!(
                    "Failed to save heartbeat check '{}': {}",
                    params.check_id, e
                ),
                duration(),
            )),
        }
    }
}

// ─── heartbeat.delete ───────────────────────────────────────────────────────

pub struct HeartbeatDeleteTool {
    ctx: HeartbeatToolContext,
}

impl HeartbeatDeleteTool {
    pub fn new(ctx: HeartbeatToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct DeleteInput {
    check_id: String,
}

#[async_trait]
impl Tool for HeartbeatDeleteTool {
    fn name(&self) -> &str {
        "heartbeat.delete"
    }

    fn description(&self) -> &str {
        "Delete a heartbeat check by its ID"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "check_id": {
                    "type": "string",
                    "description": "The ID of the heartbeat check to delete"
                }
            },
            "required": ["check_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: DeleteInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        match heartbeat_check::delete_heartbeat_check(&self.ctx.workspace, &params.check_id) {
            Ok(()) => {
                info!(check_id = %params.check_id, "Heartbeat check deleted via heartbeat.delete");
                Ok(ToolResult::success(
                    serde_json::json!({
                        "deleted": true,
                        "id": params.check_id,
                    }),
                    duration(),
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!(
                    "Failed to delete heartbeat check '{}': {}",
                    params.check_id, e
                ),
                duration(),
            )),
        }
    }
}

// ─── heartbeat.trigger ──────────────────────────────────────────────────────

pub struct HeartbeatTriggerTool {
    ctx: HeartbeatToolContext,
}

impl HeartbeatTriggerTool {
    pub fn new(ctx: HeartbeatToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct TriggerInput {
    #[serde(default)]
    check_id: Option<String>,
}

#[async_trait]
impl Tool for HeartbeatTriggerTool {
    fn name(&self) -> &str {
        "heartbeat.trigger"
    }

    fn description(&self) -> &str {
        "Trigger an immediate heartbeat run"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "check_id": {
                    "type": "string",
                    "description": "Optional: trigger only this specific check"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: TriggerInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        let handle = {
            let guard = self.ctx.wake_handle.read();
            match guard.as_ref() {
                Some(h) => h.clone(),
                None => {
                    return Ok(ToolResult::failure(
                        "Heartbeat runner is not active. The heartbeat system must be running to trigger checks.",
                        duration(),
                    ));
                }
            }
        };

        let ok = match &params.check_id {
            Some(id) => handle.wake_check(id.clone()).await,
            None => handle.wake().await,
        };

        if ok {
            let msg = match &params.check_id {
                Some(id) => format!("Triggered heartbeat check '{}'", id),
                None => "Triggered all heartbeat checks".to_string(),
            };
            info!("{}", msg);
            Ok(ToolResult::success(
                serde_json::json!({ "triggered": true, "message": msg }),
                duration(),
            ))
        } else {
            Ok(ToolResult::failure(
                "Failed to send wake command — heartbeat runner may have stopped",
                duration(),
            ))
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> HeartbeatToolContext {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        // Keep the tempdir alive by leaking it so the directory persists for the test.
        std::mem::forget(dir);
        HeartbeatToolContext {
            workspace: path,
            signer: None,
            wake_handle: Arc::new(parking_lot::RwLock::new(None)),
            min_cooldown_secs: 300,
        }
    }

    #[tokio::test]
    async fn list_empty_returns_empty_array() {
        let ctx = test_ctx();
        let tool = HeartbeatListTool::new(ctx);
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(result.success);
        let checks = result.output["checks"].as_array().unwrap();
        assert!(checks.is_empty());
    }

    #[tokio::test]
    async fn create_and_read_roundtrip() {
        let ctx = test_ctx();
        let create_tool = HeartbeatCreateTool::new(ctx.clone());
        let read_tool = HeartbeatReadTool::new(ctx);

        // Create a check
        let result = create_tool
            .execute(serde_json::json!({
                "id": "test-check",
                "name": "Test Check",
                "body": "# Test\n\nCheck something.",
                "schedule": "0 9 * * *",
                "priority": 3,
                "tags": ["test", "ci"]
            }))
            .await
            .unwrap();
        assert!(result.success, "create failed: {:?}", result.error);
        assert_eq!(result.output["created"], true);

        // Read it back
        let result = read_tool
            .execute(serde_json::json!({ "check_id": "test-check" }))
            .await
            .unwrap();
        assert!(result.success, "read failed: {:?}", result.error);
        assert_eq!(result.output["id"], "test-check");
        assert_eq!(result.output["name"], "Test Check");
        assert_eq!(result.output["schedule"], "0 9 * * *");
        assert_eq!(result.output["priority"], 3);
        assert_eq!(result.output["tags"], serde_json::json!(["test", "ci"]));
        assert!(result.output["body"]
            .as_str()
            .unwrap()
            .contains("Check something"));
    }

    #[tokio::test]
    async fn update_partial_merge() {
        let ctx = test_ctx();
        let create_tool = HeartbeatCreateTool::new(ctx.clone());
        let update_tool = HeartbeatUpdateTool::new(ctx.clone());
        let read_tool = HeartbeatReadTool::new(ctx);

        // Create a check
        create_tool
            .execute(serde_json::json!({
                "id": "merge-test",
                "name": "Original Name",
                "body": "Original body.",
                "schedule": "0 9 * * *",
                "priority": 2,
                "tags": ["alpha"]
            }))
            .await
            .unwrap();

        // Update only the name — other fields should remain unchanged
        let result = update_tool
            .execute(serde_json::json!({
                "check_id": "merge-test",
                "name": "Updated Name"
            }))
            .await
            .unwrap();
        assert!(result.success, "update failed: {:?}", result.error);

        // Verify merge
        let result = read_tool
            .execute(serde_json::json!({ "check_id": "merge-test" }))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["name"], "Updated Name");
        assert_eq!(result.output["schedule"], "0 9 * * *");
        assert_eq!(result.output["priority"], 2);
        assert_eq!(result.output["tags"], serde_json::json!(["alpha"]));
        assert!(result.output["body"]
            .as_str()
            .unwrap()
            .contains("Original body"));
    }

    #[tokio::test]
    async fn delete_removes_check() {
        let ctx = test_ctx();
        let create_tool = HeartbeatCreateTool::new(ctx.clone());
        let delete_tool = HeartbeatDeleteTool::new(ctx.clone());
        let read_tool = HeartbeatReadTool::new(ctx);

        // Create then delete
        create_tool
            .execute(serde_json::json!({
                "id": "doomed",
                "name": "Doomed Check",
                "body": "Goodbye."
            }))
            .await
            .unwrap();

        let result = delete_tool
            .execute(serde_json::json!({ "check_id": "doomed" }))
            .await
            .unwrap();
        assert!(result.success, "delete failed: {:?}", result.error);
        assert_eq!(result.output["deleted"], true);

        // Verify it is gone
        let result = read_tool
            .execute(serde_json::json!({ "check_id": "doomed" }))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn create_duplicate_fails() {
        let ctx = test_ctx();
        let create_tool = HeartbeatCreateTool::new(ctx);

        // Create first time — should succeed
        let result = create_tool
            .execute(serde_json::json!({
                "id": "unique-check",
                "name": "First",
                "body": "First body."
            }))
            .await
            .unwrap();
        assert!(result.success);

        // Create again with same ID — should fail
        let result = create_tool
            .execute(serde_json::json!({
                "id": "unique-check",
                "name": "Second",
                "body": "Second body."
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("already exists"),
            "Expected 'already exists' error, got: {:?}",
            result.error
        );
    }

    #[tokio::test]
    async fn trigger_without_handle_reports_not_running() {
        let ctx = test_ctx();
        let tool = HeartbeatTriggerTool::new(ctx);
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(!result.success);
        assert!(
            result.error.as_deref().unwrap_or("").contains("not active"),
            "Expected 'not active' error, got: {:?}",
            result.error
        );
    }
}
