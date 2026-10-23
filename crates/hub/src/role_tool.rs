//! Role management tools — let agents list, create, update, and delete role templates.
//!
//! Tools:
//! - `role.list`   — list all role templates
//! - `role.show`   — show a single role template by ID
//! - `role.create` — create a new role template
//! - `role.update` — update an existing role template (partial merge)
//! - `role.delete` — delete a role template

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde::Deserialize;
use tracing::info;

use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::roles;

// ─── Role Tool Context ──────────────────────────────────────────────────────

/// Shared context for all role tools.
#[derive(Clone)]
pub struct RoleToolContext {
    pub data_dir: String,
}

/// Create all role management tools.
pub fn role_tools(ctx: RoleToolContext) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(RoleListTool::new(ctx.clone())),
        Arc::new(RoleShowTool::new(ctx.clone())),
        Arc::new(RoleCreateTool::new(ctx.clone())),
        Arc::new(RoleUpdateTool::new(ctx.clone())),
        Arc::new(RoleDeleteTool::new(ctx)),
    ]
}

// ─── role.list ──────────────────────────────────────────────────────────────

pub struct RoleListTool {
    ctx: RoleToolContext,
}

impl RoleListTool {
    pub fn new(ctx: RoleToolContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for RoleListTool {
    fn name(&self) -> &str {
        "role.list"
    }

    fn description(&self) -> &str {
        "List all available role templates"
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

        match roles::list_roles(&self.ctx.data_dir) {
            Ok(role_list) => {
                let roles_json: Vec<serde_json::Value> = role_list
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "id": r.id,
                            "name": r.name,
                            "system_prompt": r.system_prompt,
                            "capabilities": r.capabilities,
                            "expertise": r.expertise,
                            "communication_style": r.communication_style,
                        })
                    })
                    .collect();

                Ok(ToolResult::success(
                    serde_json::json!({ "roles": roles_json }),
                    start.elapsed().as_millis() as u64,
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to list roles: {}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

// ─── role.show ──────────────────────────────────────────────────────────────

pub struct RoleShowTool {
    ctx: RoleToolContext,
}

impl RoleShowTool {
    pub fn new(ctx: RoleToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct ShowInput {
    role_id: String,
}

#[async_trait]
impl Tool for RoleShowTool {
    fn name(&self) -> &str {
        "role.show"
    }

    fn description(&self) -> &str {
        "Show the details of a single role template by its ID."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "role_id": {
                    "type": "string",
                    "description": "The unique role identifier"
                }
            },
            "required": ["role_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: ShowInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        match roles::load_role(&self.ctx.data_dir, &params.role_id) {
            Ok(role) => Ok(ToolResult::success(
                serde_json::json!({
                    "id": role.id,
                    "name": role.name,
                    "system_prompt": role.system_prompt,
                    "capabilities": role.capabilities,
                    "expertise": role.expertise,
                    "communication_style": role.communication_style,
                    "personality": role.personality,
                }),
                duration(),
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to load role '{}': {}", params.role_id, e),
                duration(),
            )),
        }
    }
}

// ─── role.create ────────────────────────────────────────────────────────────

pub struct RoleCreateTool {
    ctx: RoleToolContext,
}

impl RoleCreateTool {
    pub fn new(ctx: RoleToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct CreateInput {
    id: String,
    name: String,
    system_prompt: String,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    expertise: Vec<String>,
    #[serde(default)]
    communication_style: Option<String>,
}

#[async_trait]
impl Tool for RoleCreateTool {
    fn name(&self) -> &str {
        "role.create"
    }

    fn description(&self) -> &str {
        "Create a new role template. Fails if a role with the same ID already exists."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Unique role identifier (used as filename stem)"
                },
                "name": {
                    "type": "string",
                    "description": "Human-readable role name"
                },
                "system_prompt": {
                    "type": "string",
                    "description": "System prompt fragment for this role"
                },
                "capabilities": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Capabilities this role typically needs"
                },
                "expertise": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Areas of expertise"
                },
                "communication_style": {
                    "type": "string",
                    "description": "Communication style descriptor (e.g. 'thorough', 'concise')"
                }
            },
            "required": ["id", "name", "system_prompt"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: CreateInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        // Check uniqueness — load_role should fail for a new role
        if roles::load_role(&self.ctx.data_dir, &params.id).is_ok() {
            return Ok(ToolResult::failure(
                format!("Role '{}' already exists", params.id),
                duration(),
            ));
        }

        let role = roles::RoleTemplate {
            id: params.id.clone(),
            name: params.name,
            system_prompt: params.system_prompt,
            capabilities: params.capabilities,
            expertise: params.expertise,
            communication_style: params.communication_style,
            personality: None,
        };

        match roles::save_role(&self.ctx.data_dir, &role) {
            Ok(()) => {
                info!(role_id = %params.id, "Role created via role.create");
                Ok(ToolResult::success(
                    serde_json::json!({
                        "created": true,
                        "id": role.id,
                        "name": role.name,
                    }),
                    duration(),
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to create role '{}': {}", params.id, e),
                duration(),
            )),
        }
    }
}

// ─── role.update ────────────────────────────────────────────────────────────

pub struct RoleUpdateTool {
    ctx: RoleToolContext,
}

impl RoleUpdateTool {
    pub fn new(ctx: RoleToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct UpdateInput {
    role_id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    system_prompt: Option<String>,
    #[serde(default)]
    capabilities: Option<Vec<String>>,
    #[serde(default)]
    expertise: Option<Vec<String>>,
    #[serde(default)]
    communication_style: Option<String>,
}

#[async_trait]
impl Tool for RoleUpdateTool {
    fn name(&self) -> &str {
        "role.update"
    }

    fn description(&self) -> &str {
        "Update a role definition"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "role_id": {
                    "type": "string",
                    "description": "The ID of the role to update"
                },
                "name": {
                    "type": "string",
                    "description": "New human-readable name"
                },
                "system_prompt": {
                    "type": "string",
                    "description": "New system prompt fragment"
                },
                "capabilities": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "New capabilities list"
                },
                "expertise": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "New expertise areas"
                },
                "communication_style": {
                    "type": "string",
                    "description": "New communication style descriptor"
                }
            },
            "required": ["role_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: UpdateInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        // Load existing role
        let mut role = match roles::load_role(&self.ctx.data_dir, &params.role_id) {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Failed to load role '{}': {}", params.role_id, e),
                    duration(),
                ));
            }
        };

        // Merge only provided fields
        if let Some(name) = params.name {
            role.name = name;
        }
        if let Some(system_prompt) = params.system_prompt {
            role.system_prompt = system_prompt;
        }
        if let Some(capabilities) = params.capabilities {
            role.capabilities = capabilities;
        }
        if let Some(expertise) = params.expertise {
            role.expertise = expertise;
        }
        if let Some(communication_style) = params.communication_style {
            role.communication_style = Some(communication_style);
        }

        match roles::save_role(&self.ctx.data_dir, &role) {
            Ok(()) => {
                info!(role_id = %params.role_id, "Role updated via role.update");
                Ok(ToolResult::success(
                    serde_json::json!({
                        "updated": true,
                        "id": role.id,
                        "name": role.name,
                    }),
                    duration(),
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to save role '{}': {}", params.role_id, e),
                duration(),
            )),
        }
    }
}

// ─── role.delete ────────────────────────────────────────────────────────────

pub struct RoleDeleteTool {
    ctx: RoleToolContext,
}

impl RoleDeleteTool {
    pub fn new(ctx: RoleToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct DeleteInput {
    role_id: String,
}

#[async_trait]
impl Tool for RoleDeleteTool {
    fn name(&self) -> &str {
        "role.delete"
    }

    fn description(&self) -> &str {
        "Delete a role template by its ID. This permanently removes the role from disk."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "role_id": {
                    "type": "string",
                    "description": "The ID of the role to delete"
                }
            },
            "required": ["role_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: DeleteInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        match roles::delete_role(&self.ctx.data_dir, &params.role_id) {
            Ok(()) => {
                info!(role_id = %params.role_id, "Role deleted via role.delete");
                Ok(ToolResult::success(
                    serde_json::json!({
                        "deleted": true,
                        "id": params.role_id,
                    }),
                    duration(),
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to delete role '{}': {}", params.role_id, e),
                duration(),
            )),
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> RoleToolContext {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap().to_string();
        // Keep the tempdir alive by leaking it so the directory persists for the test.
        std::mem::forget(dir);
        RoleToolContext { data_dir: path }
    }

    #[tokio::test]
    async fn list_empty_returns_empty_array() {
        let ctx = test_ctx();
        let tool = RoleListTool::new(ctx);
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(result.success);
        let roles = result.output["roles"].as_array().unwrap();
        assert!(roles.is_empty());
    }

    #[tokio::test]
    async fn create_and_show_roundtrip() {
        let ctx = test_ctx();
        let create_tool = RoleCreateTool::new(ctx.clone());
        let show_tool = RoleShowTool::new(ctx);

        // Create a role
        let result = create_tool
            .execute(serde_json::json!({
                "id": "test-dev",
                "name": "Test Developer",
                "system_prompt": "You are a test developer.",
                "capabilities": ["code", "testing"],
                "expertise": ["rust"],
                "communication_style": "concise"
            }))
            .await
            .unwrap();
        assert!(result.success, "create failed: {:?}", result.error);
        assert_eq!(result.output["created"], true);

        // Show it back
        let result = show_tool
            .execute(serde_json::json!({ "role_id": "test-dev" }))
            .await
            .unwrap();
        assert!(result.success, "show failed: {:?}", result.error);
        assert_eq!(result.output["id"], "test-dev");
        assert_eq!(result.output["name"], "Test Developer");
        assert_eq!(result.output["system_prompt"], "You are a test developer.");
        assert_eq!(
            result.output["capabilities"],
            serde_json::json!(["code", "testing"])
        );
        assert_eq!(result.output["expertise"], serde_json::json!(["rust"]));
        assert_eq!(result.output["communication_style"], "concise");
    }

    #[tokio::test]
    async fn update_partial_merge() {
        let ctx = test_ctx();
        let create_tool = RoleCreateTool::new(ctx.clone());
        let update_tool = RoleUpdateTool::new(ctx.clone());
        let show_tool = RoleShowTool::new(ctx);

        // Create a role
        create_tool
            .execute(serde_json::json!({
                "id": "merge-test",
                "name": "Original Name",
                "system_prompt": "Original prompt.",
                "capabilities": ["code"],
                "expertise": ["rust"],
                "communication_style": "verbose"
            }))
            .await
            .unwrap();

        // Update only the name — other fields should remain unchanged
        let result = update_tool
            .execute(serde_json::json!({
                "role_id": "merge-test",
                "name": "Updated Name"
            }))
            .await
            .unwrap();
        assert!(result.success, "update failed: {:?}", result.error);

        // Verify merge
        let result = show_tool
            .execute(serde_json::json!({ "role_id": "merge-test" }))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["name"], "Updated Name");
        assert_eq!(result.output["system_prompt"], "Original prompt.");
        assert_eq!(result.output["capabilities"], serde_json::json!(["code"]));
        assert_eq!(result.output["expertise"], serde_json::json!(["rust"]));
        assert_eq!(result.output["communication_style"], "verbose");
    }

    #[tokio::test]
    async fn delete_removes_role() {
        let ctx = test_ctx();
        let create_tool = RoleCreateTool::new(ctx.clone());
        let delete_tool = RoleDeleteTool::new(ctx.clone());
        let show_tool = RoleShowTool::new(ctx);

        // Create then delete
        create_tool
            .execute(serde_json::json!({
                "id": "doomed",
                "name": "Doomed Role",
                "system_prompt": "Goodbye."
            }))
            .await
            .unwrap();

        let result = delete_tool
            .execute(serde_json::json!({ "role_id": "doomed" }))
            .await
            .unwrap();
        assert!(result.success, "delete failed: {:?}", result.error);
        assert_eq!(result.output["deleted"], true);

        // Verify it is gone
        let result = show_tool
            .execute(serde_json::json!({ "role_id": "doomed" }))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn create_duplicate_fails() {
        let ctx = test_ctx();
        let create_tool = RoleCreateTool::new(ctx);

        // Create first time — should succeed
        let result = create_tool
            .execute(serde_json::json!({
                "id": "unique-role",
                "name": "First",
                "system_prompt": "First prompt."
            }))
            .await
            .unwrap();
        assert!(result.success);

        // Create again with same ID — should fail
        let result = create_tool
            .execute(serde_json::json!({
                "id": "unique-role",
                "name": "Second",
                "system_prompt": "Second prompt."
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

    // Admin gating for `role.{create,update,delete}` is now enforced
    // pre-dispatch by the `HubCapabilityGate` (`role.* → PersonasManage`) off
    // the `tool_capability` registry, not by an inline check here — the
    // member-denied / admin-allowed adversarial coverage lives in
    // `crate::authz` and `crate::cap_gate`. The functional create/show/delete
    // round-trips above run with no admin check of their own.
}
