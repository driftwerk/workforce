//! Device pairing management tools — let agents list, approve, reject pending pairing
//! requests and manage paired devices.
//!
//! Tools:
//! - `pairing.pending`  — list pending device pairing requests
//! - `pairing.approve`  — approve a pending pairing request by code
//! - `pairing.reject`   — reject a pending pairing request by code
//! - `pairing.devices`  — list all paired (approved) devices
//! - `pairing.revoke`   — revoke an approved device's access

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde::Deserialize;
use tracing::info;

use workforce_agent::tools::{Tool, ToolResult};
use workforce_pairing::DevicePairingService;

// ─── Pairing Tool Context ────────────────────────────────────────────────────

/// Shared context for all pairing tools.
#[derive(Clone)]
pub struct PairingToolContext {
    pub device_pairing: Arc<DevicePairingService>,
}

/// Create all device pairing management tools.
pub fn pairing_tools(ctx: PairingToolContext) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(PairingPendingTool::new(ctx.clone())),
        Arc::new(PairingApproveTool::new(ctx.clone())),
        Arc::new(PairingRejectTool::new(ctx.clone())),
        Arc::new(PairingDevicesTool::new(ctx.clone())),
        Arc::new(PairingRevokeTool::new(ctx)),
    ]
}

// ─── pairing.pending ─────────────────────────────────────────────────────────

pub struct PairingPendingTool {
    ctx: PairingToolContext,
}

impl PairingPendingTool {
    pub fn new(ctx: PairingToolContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for PairingPendingTool {
    fn name(&self) -> &str {
        "pairing.pending"
    }

    fn description(&self) -> &str {
        "List all pending device pairing requests"
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

        match self.ctx.device_pairing.list_pending_async().await {
            Ok(requests) => {
                let items: Vec<serde_json::Value> = requests
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "code": r.code,
                            "device_name": r.device_name,
                            "status": format!("{:?}", r.status),
                            "created_at": r.created_at.to_rfc3339(),
                            "expires_at": r.expires_at.to_rfc3339(),
                        })
                    })
                    .collect();

                Ok(ToolResult::success(
                    serde_json::json!({ "pending": items, "count": items.len() }),
                    start.elapsed().as_millis() as u64,
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to list pending pairing requests: {}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

// ─── pairing.approve ─────────────────────────────────────────────────────────

pub struct PairingApproveTool {
    ctx: PairingToolContext,
}

impl PairingApproveTool {
    pub fn new(ctx: PairingToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct ApproveInput {
    code: String,
}

#[async_trait]
impl Tool for PairingApproveTool {
    fn name(&self) -> &str {
        "pairing.approve"
    }

    fn description(&self) -> &str {
        "Approve a pending device pairing request by code"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "The 8-character pairing code to approve"
                }
            },
            "required": ["code"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: ApproveInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        match self
            .ctx
            .device_pairing
            .approve_async(params.code.clone(), "agent".to_string())
            .await
        {
            Ok((device, _token)) => {
                info!(code = %params.code, device = %device.device_name, "Pairing approved via pairing.approve tool");
                Ok(ToolResult::success(
                    serde_json::json!({
                        "approved": true,
                        "code": params.code,
                        "device_name": device.device_name,
                        "device_id": device.id,
                    }),
                    duration(),
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to approve pairing code '{}': {}", params.code, e),
                duration(),
            )),
        }
    }
}

// ─── pairing.reject ──────────────────────────────────────────────────────────

pub struct PairingRejectTool {
    ctx: PairingToolContext,
}

impl PairingRejectTool {
    pub fn new(ctx: PairingToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct RejectInput {
    code: String,
}

#[async_trait]
impl Tool for PairingRejectTool {
    fn name(&self) -> &str {
        "pairing.reject"
    }

    fn description(&self) -> &str {
        "Reject a pending device pairing request by its 8-character code"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "The 8-character pairing code to reject"
                }
            },
            "required": ["code"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: RejectInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        match self
            .ctx
            .device_pairing
            .reject_async(params.code.clone())
            .await
        {
            Ok(()) => {
                info!(code = %params.code, "Pairing rejected via pairing.reject tool");
                Ok(ToolResult::success(
                    serde_json::json!({
                        "rejected": true,
                        "code": params.code,
                    }),
                    duration(),
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to reject pairing code '{}': {}", params.code, e),
                duration(),
            )),
        }
    }
}

// ─── pairing.devices ─────────────────────────────────────────────────────────

pub struct PairingDevicesTool {
    ctx: PairingToolContext,
}

impl PairingDevicesTool {
    pub fn new(ctx: PairingToolContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for PairingDevicesTool {
    fn name(&self) -> &str {
        "pairing.devices"
    }

    fn description(&self) -> &str {
        "List all approved (paired) devices"
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

        match self.ctx.device_pairing.list_devices_async().await {
            Ok(devices) => {
                let items: Vec<serde_json::Value> = devices
                    .iter()
                    .map(|d| {
                        serde_json::json!({
                            "id": d.id,
                            "device_name": d.device_name,
                            "scopes": d.scopes,
                            "created_at": d.created_at.to_rfc3339(),
                            "last_used": d.last_used.map(|t| t.to_rfc3339()),
                            "revoked": d.revoked,
                        })
                    })
                    .collect();

                Ok(ToolResult::success(
                    serde_json::json!({ "devices": items, "count": items.len() }),
                    start.elapsed().as_millis() as u64,
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to list devices: {}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

// ─── pairing.revoke ──────────────────────────────────────────────────────────

pub struct PairingRevokeTool {
    ctx: PairingToolContext,
}

impl PairingRevokeTool {
    pub fn new(ctx: PairingToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct RevokeInput {
    device_id: String,
}

#[async_trait]
impl Tool for PairingRevokeTool {
    fn name(&self) -> &str {
        "pairing.revoke"
    }

    fn description(&self) -> &str {
        "Revoke a device pairing"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "device_id": {
                    "type": "string",
                    "description": "The device ID to revoke (from pairing.devices)"
                }
            },
            "required": ["device_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: RevokeInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        match self
            .ctx
            .device_pairing
            .revoke_async(params.device_id.clone())
            .await
        {
            Ok(()) => {
                info!(device_id = %params.device_id, "Device revoked via pairing.revoke tool");
                Ok(ToolResult::success(
                    serde_json::json!({
                        "revoked": true,
                        "device_id": params.device_id,
                    }),
                    duration(),
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to revoke device '{}': {}", params.device_id, e),
                duration(),
            )),
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_pairing::{store::SqlitePairingStore, PairingConfig};

    fn test_ctx() -> PairingToolContext {
        let store = Arc::new(SqlitePairingStore::in_memory().unwrap());
        let config = PairingConfig {
            device_pairing_enabled: true,
            ..Default::default()
        };
        PairingToolContext {
            device_pairing: Arc::new(DevicePairingService::new(store, config)),
        }
    }

    #[tokio::test]
    async fn pending_empty_returns_empty() {
        let ctx = test_ctx();
        let tool = PairingPendingTool::new(ctx);
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["count"], 0);
        assert!(result.output["pending"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn approve_and_list_devices() {
        let ctx = test_ctx();

        // Initiate a pairing request
        let req = ctx
            .device_pairing
            .initiate("test-laptop", vec!["chat".into()], None, None, None, None)
            .unwrap();

        // Approve via tool
        let approve_tool = PairingApproveTool::new(ctx.clone());
        let result = approve_tool
            .execute(serde_json::json!({ "code": req.code }))
            .await
            .unwrap();
        assert!(result.success, "approve failed: {:?}", result.error);
        assert_eq!(result.output["approved"], true);
        assert_eq!(result.output["device_name"], "test-laptop");

        // List devices
        let devices_tool = PairingDevicesTool::new(ctx);
        let result = devices_tool.execute(serde_json::json!({})).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["count"], 1);
        let devices = result.output["devices"].as_array().unwrap();
        assert_eq!(devices[0]["device_name"], "test-laptop");
    }

    #[tokio::test]
    async fn reject_pairing() {
        let ctx = test_ctx();

        let req = ctx
            .device_pairing
            .initiate("rejected-device", vec![], None, None, None, None)
            .unwrap();

        let reject_tool = PairingRejectTool::new(ctx.clone());
        let result = reject_tool
            .execute(serde_json::json!({ "code": req.code }))
            .await
            .unwrap();
        assert!(result.success, "reject failed: {:?}", result.error);
        assert_eq!(result.output["rejected"], true);

        // Pending should be empty now
        let pending_tool = PairingPendingTool::new(ctx);
        let result = pending_tool.execute(serde_json::json!({})).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["count"], 0);
    }

    #[tokio::test]
    async fn revoke_device() {
        let ctx = test_ctx();

        // Pair a device
        let req = ctx
            .device_pairing
            .initiate("revoke-me", vec![], None, None, None, None)
            .unwrap();
        let (device, _token) = ctx.device_pairing.approve(&req.code, "test").unwrap();

        // Revoke via tool
        let revoke_tool = PairingRevokeTool::new(ctx.clone());
        let result = revoke_tool
            .execute(serde_json::json!({ "device_id": device.id }))
            .await
            .unwrap();
        assert!(result.success, "revoke failed: {:?}", result.error);
        assert_eq!(result.output["revoked"], true);

        // Device should show as revoked
        let devices_tool = PairingDevicesTool::new(ctx);
        let result = devices_tool.execute(serde_json::json!({})).await.unwrap();
        let devices = result.output["devices"].as_array().unwrap();
        assert_eq!(devices[0]["revoked"], true);
    }
}
