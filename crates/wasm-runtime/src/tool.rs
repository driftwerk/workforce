//! WasmSkillTool — bridges a WASM skill tool into workforce's `Tool` trait.
//!
//! Each tool declared in a WASM skill's manifest gets its own `WasmSkillTool`
//! instance registered in the agent's `ToolRegistry`. When the agent calls the
//! tool, this bridge serialises the input, dispatches to the WASM engine, and
//! returns a `ToolResult`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;

use workforce_agent::tools::{Tool, ToolResult};

use workforce_common::settings::SecretStore;

use crate::approval::render_summary;
use crate::engine::WasmSkillEngine;
use crate::manifest::SkillManifest;
use crate::types::{SkillRuntimeBindings, WasmToolDefinition, WasmToolRequest};

/// Why a call to a skill-gated tool needs a human. A skill may demand *that*
/// a human decides; which human is the hub's to say, through a
/// `PermissionRule` with `approvers` and `allow_self_approval = false`.
pub const SKILL_APPROVAL_REASON: &str = "Required by the skill's own approval gate";

/// A WASM skill tool registered in the agent's ToolRegistry.
///
/// Implements the `Tool` trait so agents can call it like any built-in tool.
/// Each instance wraps a single tool from a WASM skill, holding references to
/// the shared engine and the skill's manifest.
pub struct WasmSkillTool {
    /// The tool definition (qualified name, description, schema, handler).
    definition: WasmToolDefinition,
    /// The shared WASM engine.
    engine: Arc<WasmSkillEngine>,
    /// The skill's manifest.
    manifest: Arc<SkillManifest>,
    /// Binary version to execute.
    binary_version: u32,
    /// Skill-scoped config.
    config: Arc<HashMap<String, String>>,
    /// Skill-locker vault for the agent this tool is registered under. The
    /// engine is process-shared, so the per-agent store rides on the tool.
    vault: Option<Arc<dyn SecretStore>>,
    /// Late-bound bridges read at execute time: the per-agent tool invoker
    /// and the process-wide use gate.
    bindings: SkillRuntimeBindings,
    /// Whether the manifest holds this tool behind a human decision.
    requires_approval: bool,
    /// The manifest's approval-prompt template, rendered by the host.
    approval_summary: Option<String>,
}

impl WasmSkillTool {
    /// Create all WasmSkillTools for a skill manifest.
    ///
    /// Returns one `WasmSkillTool` per tool declared in the manifest, ready to
    /// be registered in a `ToolRegistry`.
    pub fn from_manifest(
        manifest: &SkillManifest,
        engine: Arc<WasmSkillEngine>,
        binary_version: u32,
        config: HashMap<String, String>,
        vault: Option<Arc<dyn SecretStore>>,
        bindings: SkillRuntimeBindings,
    ) -> Vec<Arc<dyn Tool>> {
        let manifest = Arc::new(manifest.clone());
        let config_arc = Arc::new(config);

        manifest
            .tools
            .iter()
            .map(|tool_def| {
                let qualified_name = format!("{}.{}", manifest.name, tool_def.name);
                let handler = tool_def
                    .handler
                    .clone()
                    .unwrap_or_else(|| tool_def.name.clone());

                let wasm_def = WasmToolDefinition {
                    name: qualified_name,
                    description: tool_def.description.clone(),
                    input_schema: tool_def.input_schema.clone(),
                    handler,
                };

                let tool = WasmSkillTool {
                    definition: wasm_def,
                    engine: Arc::clone(&engine),
                    manifest: Arc::clone(&manifest),
                    binary_version,
                    config: Arc::clone(&config_arc),
                    vault: vault.clone(),
                    bindings: bindings.clone(),
                    requires_approval: tool_def.requires_approval,
                    approval_summary: tool_def.approval_summary.clone(),
                };

                Arc::new(tool) as Arc<dyn Tool>
            })
            .collect()
    }
}

#[async_trait]
impl Tool for WasmSkillTool {
    fn name(&self) -> &str {
        &self.definition.name
    }

    fn description(&self) -> &str {
        &self.definition.description
    }

    fn input_schema(&self) -> serde_json::Value {
        self.definition.input_schema.clone()
    }

    fn approval_requirement(&self) -> Option<String> {
        self.requires_approval
            .then(|| SKILL_APPROVAL_REASON.to_string())
    }

    /// Rendered here on the host from the manifest's template and the call's
    /// own arguments — the guest never runs for a summary. An unusable
    /// template falls back to the generic parameter-shape summary.
    fn approval_summary(&self, input: &serde_json::Value) -> Option<String> {
        self.approval_summary
            .as_deref()
            .and_then(|template| render_summary(template, input))
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();

        // Read agent_id and user_id from task-local context set by the runner.
        // user_id is `None` for system-initiated work that hasn't opted into
        // a synthetic identity via `runner::with_system_identity`; the host
        // hard-fails any per-user vault read in that case.
        let agent_id = workforce_agent::runner::CURRENT_AGENT_ID
            .try_with(|id| id.clone())
            .unwrap_or_default();
        let user_id = workforce_agent::runner::CURRENT_USER_ID
            .try_with(|id| id.clone())
            .ok()
            .filter(|id| !id.is_empty());

        if let Err(denial) = self
            .bindings
            .authorize_use(&self.manifest.name, user_id.as_deref())
        {
            return Ok(ToolResult::failure(
                denial,
                start.elapsed().as_millis() as u64,
            ));
        }

        let request = WasmToolRequest {
            tool_name: self.definition.name.clone(),
            handler: self.definition.handler.clone(),
            input,
            agent_id,
            user_id,
            task_id: None,
        };

        let invoker = self.bindings.invoker.read().clone();
        let result = self
            .engine
            .execute_tool(
                &self.manifest.name,
                self.binary_version,
                &self.manifest,
                &request,
                &self.config,
                self.vault.clone(),
                invoker,
            )
            .await;

        let duration_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(response) => Ok(bridge_result(response, duration_ms)),
            Err(e) => Ok(ToolResult::failure(
                format!("WASM execution error: {}", e),
                duration_ms,
            )),
        }
    }
}

/// Convert a guest response into a `ToolResult`, surfacing any host-authored
/// policy notice so the agent reads the remedy even when the guest papered
/// over the denial: a failure leads with the notice; a success carries it
/// inside the result payload.
fn bridge_result(response: crate::types::WasmToolResponse, duration_ms: u64) -> ToolResult {
    let notice = response.policy_notice;
    if response.success {
        let result = match notice {
            Some(n) => match response.result {
                serde_json::Value::Object(mut obj) => {
                    obj.insert("platform_notice".into(), serde_json::Value::String(n));
                    serde_json::Value::Object(obj)
                }
                other => serde_json::json!({ "result": other, "platform_notice": n }),
            },
            None => response.result,
        };
        ToolResult::success(result, duration_ms)
    } else {
        let guest_error = response
            .error
            .unwrap_or_else(|| "WASM tool returned failure".to_string());
        let message = match notice {
            Some(n) => format!("{n} (skill reported: {guest_error})"),
            None => guest_error,
        };
        ToolResult::failure(message, duration_ms)
    }
}

impl std::fmt::Debug for WasmSkillTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmSkillTool")
            .field("name", &self.definition.name)
            .field("skill", &self.manifest.name)
            .field("handler", &self.definition.handler)
            .field("version", &self.binary_version)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ManifestTool, SkillManifest};
    use std::collections::HashSet;

    fn test_manifest() -> SkillManifest {
        SkillManifest {
            name: "weather".into(),
            version: "1.0.0".into(),
            description: "Weather skill".into(),
            author: String::new(),
            license: String::new(),
            min_workforce_version: None,
            capabilities: HashSet::new(),
            tools: vec![
                ManifestTool {
                    name: "get_forecast".into(),
                    description: "Get weather forecast".into(),
                    requires_approval: false,
                    approval_summary: None,
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {"city": {"type": "string"}},
                        "required": ["city"]
                    }),
                    handler: None,
                },
                ManifestTool {
                    name: "get_alerts".into(),
                    description: "Get weather alerts".into(),
                    requires_approval: false,
                    approval_summary: None,
                    input_schema: serde_json::json!({"type": "object"}),
                    handler: Some("fetch_alerts".into()),
                },
            ],
            credentials: vec![],
            config_schema: vec![],
            allowed_hosts: vec![],
            permissions: vec![],
        }
    }

    #[tokio::test]
    async fn from_manifest_creates_tools() {
        let manifest = test_manifest();
        let engine = Arc::new(WasmSkillEngine::new().unwrap());
        let tools = WasmSkillTool::from_manifest(
            &manifest,
            engine,
            1,
            HashMap::new(),
            None,
            SkillRuntimeBindings::unbound(),
        );

        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name(), "weather.get_forecast");
        assert_eq!(tools[1].name(), "weather.get_alerts");
    }

    #[tokio::test]
    async fn tool_definition() {
        let manifest = test_manifest();
        let engine = Arc::new(WasmSkillEngine::new().unwrap());
        let tools = WasmSkillTool::from_manifest(
            &manifest,
            engine,
            1,
            HashMap::new(),
            None,
            SkillRuntimeBindings::unbound(),
        );

        let def = tools[0].definition();
        assert_eq!(def.name, "weather.get_forecast");
        assert_eq!(def.description, "Get weather forecast");
        assert!(def.input_schema["properties"]["city"]["type"] == "string");
    }

    /// The gate is the authority: a refused call must not reach the engine at
    /// all — which is what this asserts by never loading a module. Reaching the
    /// engine would fail with a compile/lookup error instead of the denial.
    #[tokio::test]
    async fn a_denied_call_never_reaches_the_engine() {
        struct Deny;
        impl crate::types::SkillUseGate for Deny {
            fn authorize_use(&self, skill: &str, user: Option<&str>) -> Result<(), String> {
                Err(format!("`{skill}` is not yours, {user:?}"))
            }
        }
        let bindings = SkillRuntimeBindings::unbound();
        *bindings.use_gate.write() = Some(Arc::new(Deny));

        let tools = WasmSkillTool::from_manifest(
            &test_manifest(),
            Arc::new(WasmSkillEngine::new().unwrap()),
            1,
            HashMap::new(),
            None,
            bindings,
        );
        let result = workforce_agent::runner::CURRENT_USER_ID
            .scope("bob".to_string(), tools[0].execute(serde_json::json!({})))
            .await
            .expect("tool ran");
        assert!(!result.success);
        assert_eq!(
            result.error.as_deref(),
            Some("`weather` is not yours, Some(\"bob\")")
        );
    }

    #[test]
    fn bridge_failure_leads_with_the_policy_notice() {
        let response = crate::types::WasmToolResponse {
            success: false,
            result: serde_json::Value::Null,
            error: Some("Missing password in vault".into()),
            policy_notice: Some("Credential 'pw' has not been granted".into()),
        };
        let result = bridge_result(response, 1);
        assert!(!result.success);
        let msg = result.error.unwrap();
        assert!(msg.starts_with("Credential 'pw' has not been granted"));
        assert!(msg.contains("Missing password in vault"));
    }

    #[test]
    fn bridge_success_carries_the_notice_in_the_payload() {
        let response = crate::types::WasmToolResponse {
            success: true,
            result: serde_json::json!({"enabled": 1}),
            policy_notice: Some("host denied".into()),
            error: None,
        };
        let result = bridge_result(response, 1);
        assert!(result.success);
        assert_eq!(result.output["platform_notice"], "host denied");
        assert_eq!(result.output["enabled"], 1);
    }

    #[test]
    fn bridge_without_notice_is_unchanged() {
        let response = crate::types::WasmToolResponse {
            success: true,
            result: serde_json::json!({"ok": true}),
            policy_notice: None,
            error: None,
        };
        let result = bridge_result(response, 1);
        assert!(result.success);
        assert_eq!(result.output, serde_json::json!({"ok": true}));
    }

    fn gated_manifest() -> SkillManifest {
        let mut manifest = test_manifest();
        manifest.tools[0].requires_approval = true;
        manifest.tools[0].approval_summary =
            Some("forecast for `{{city}}` over {{days|count}} day(s)".into());
        manifest
    }

    fn built(manifest: &SkillManifest) -> Vec<Arc<dyn Tool>> {
        WasmSkillTool::from_manifest(
            manifest,
            Arc::new(WasmSkillEngine::new().unwrap()),
            1,
            HashMap::new(),
            None,
            SkillRuntimeBindings::unbound(),
        )
    }

    #[tokio::test]
    async fn only_a_gated_tool_reports_a_requirement() {
        let tools = built(&gated_manifest());
        assert_eq!(
            tools[0].approval_requirement().as_deref(),
            Some(SKILL_APPROVAL_REASON)
        );
        assert_eq!(tools[1].approval_requirement(), None);
        assert_eq!(built(&test_manifest())[0].approval_requirement(), None);
    }

    /// The summary is asked for on every tool call in every round, so it is
    /// a pure render of the manifest template: no module is loaded here, and
    /// reaching the engine would fail rather than answer.
    #[tokio::test]
    async fn approval_summary_never_touches_the_engine() {
        let tools = built(&gated_manifest());
        assert_eq!(
            tools[0].approval_summary(&serde_json::json!({"city": "Leeds", "days": [1, 2, 3]})),
            Some("forecast for `Leeds` over 3 day(s)".to_string())
        );
        // An ungated tool declares no template, so the generic shape summary
        // still stands.
        assert_eq!(tools[1].approval_summary(&serde_json::json!({})), None);
    }

    #[tokio::test]
    async fn an_unrenderable_template_falls_back_to_the_generic_summary() {
        let mut manifest = gated_manifest();
        manifest.tools[0].approval_summary = Some("{{city|upper}}".into());
        assert_eq!(
            built(&manifest)[0].approval_summary(&serde_json::json!({"city": "Leeds"})),
            None
        );
    }

    #[test]
    fn handler_name_resolution() {
        let manifest = test_manifest();
        // Verify handler resolution through the manifest directly
        assert_eq!(
            manifest.handler_for_tool("get_forecast"),
            Some("get_forecast".into())
        );
        assert_eq!(
            manifest.handler_for_tool("get_alerts"),
            Some("fetch_alerts".into())
        );
    }
}
