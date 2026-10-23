//! Plugin SDK — lightweight framework for building NATS-based tool plugins.
//!
//! Plugins are standalone processes that subscribe to NATS subjects and
//! handle tool invocation requests.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Plugin manifest — describes what a plugin provides.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    /// Plugin name.
    pub name: String,
    /// Plugin version.
    pub version: String,
    /// Human-readable description.
    pub description: String,
    /// Author.
    #[serde(default)]
    pub author: String,
    /// Tools this plugin provides.
    pub tools: Vec<PluginTool>,
    /// Required credentials.
    #[serde(default)]
    pub credentials: Vec<CredentialRequirement>,
    /// NATS subject prefix (default: workforce.tools.{name}).
    #[serde(default)]
    pub subject_prefix: Option<String>,
}

/// A tool provided by a plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginTool {
    /// Tool name (scoped: plugin_name.tool_name).
    pub name: String,
    /// Description for the LLM.
    pub description: String,
    /// JSON schema for input parameters.
    pub input_schema: serde_json::Value,
    /// Expected output format.
    #[serde(default)]
    pub output_schema: Option<serde_json::Value>,
}

/// A credential required by the plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialRequirement {
    /// Credential name.
    pub name: String,
    /// Credential type (api_key, oauth2, password, etc.).
    pub credential_type: String,
    /// Description.
    pub description: String,
    /// Whether the plugin can function without it.
    #[serde(default)]
    pub required: bool,
}

/// A tool invocation request sent to a plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRequest {
    /// Request ID for correlation.
    pub request_id: String,
    /// Tool name being invoked.
    pub tool_name: String,
    /// Input parameters.
    pub input: serde_json::Value,
    /// Agent making the request.
    pub agent_id: String,
    /// Task context.
    #[serde(default)]
    pub task_id: Option<String>,
}

/// A tool invocation response from a plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResponse {
    /// Correlation ID (matches request_id).
    pub request_id: String,
    /// Whether the invocation succeeded.
    pub success: bool,
    /// Result data.
    pub result: serde_json::Value,
    /// Error message if failed.
    #[serde(default)]
    pub error: Option<String>,
    /// Execution duration in milliseconds.
    #[serde(default)]
    pub duration_ms: u64,
}

impl ToolResponse {
    /// Create a success response.
    pub fn success(request_id: &str, result: serde_json::Value) -> Self {
        Self {
            request_id: request_id.to_string(),
            success: true,
            result,
            error: None,
            duration_ms: 0,
        }
    }

    /// Create an error response.
    pub fn error(request_id: &str, error: &str) -> Self {
        Self {
            request_id: request_id.to_string(),
            success: false,
            result: serde_json::Value::Null,
            error: Some(error.to_string()),
            duration_ms: 0,
        }
    }

    /// Set duration.
    pub fn with_duration(mut self, ms: u64) -> Self {
        self.duration_ms = ms;
        self
    }
}

/// Plugin builder for assembling a manifest.
pub struct PluginBuilder {
    name: String,
    version: String,
    description: String,
    author: String,
    tools: Vec<PluginTool>,
    credentials: Vec<CredentialRequirement>,
}

impl PluginBuilder {
    pub fn new(name: &str, version: &str) -> Self {
        Self {
            name: name.to_string(),
            version: version.to_string(),
            description: String::new(),
            author: String::new(),
            tools: Vec::new(),
            credentials: Vec::new(),
        }
    }

    pub fn description(mut self, desc: &str) -> Self {
        self.description = desc.to_string();
        self
    }

    pub fn author(mut self, author: &str) -> Self {
        self.author = author.to_string();
        self
    }

    pub fn tool(mut self, name: &str, description: &str, schema: serde_json::Value) -> Self {
        self.tools.push(PluginTool {
            name: name.to_string(),
            description: description.to_string(),
            input_schema: schema,
            output_schema: None,
        });
        self
    }

    pub fn credential(
        mut self,
        name: &str,
        cred_type: &str,
        description: &str,
        required: bool,
    ) -> Self {
        self.credentials.push(CredentialRequirement {
            name: name.to_string(),
            credential_type: cred_type.to_string(),
            description: description.to_string(),
            required,
        });
        self
    }

    pub fn build(self) -> PluginManifest {
        PluginManifest {
            name: self.name,
            version: self.version,
            description: self.description,
            author: self.author,
            tools: self.tools,
            credentials: self.credentials,
            subject_prefix: None,
        }
    }
}

/// Plugin state tracker — used by the hub to track running plugins.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInfo {
    pub manifest: PluginManifest,
    pub status: PluginStatus,
    pub registered_at: String,
    pub last_heartbeat: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginStatus {
    Running,
    Stopped,
    Error,
    Starting,
}

/// Registry of active plugins.
pub struct PluginRegistry {
    plugins: HashMap<String, PluginInfo>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self {
            plugins: HashMap::new(),
        }
    }

    /// Register a plugin.
    pub fn register(&mut self, manifest: PluginManifest) {
        let name = manifest.name.clone();
        self.plugins.insert(
            name,
            PluginInfo {
                manifest,
                status: PluginStatus::Running,
                registered_at: chrono::Utc::now().to_rfc3339(),
                last_heartbeat: None,
            },
        );
    }

    /// Deregister a plugin.
    pub fn deregister(&mut self, name: &str) -> bool {
        self.plugins.remove(name).is_some()
    }

    /// Get plugin info.
    pub fn get(&self, name: &str) -> Option<&PluginInfo> {
        self.plugins.get(name)
    }

    /// List all plugins.
    pub fn list(&self) -> Vec<&PluginInfo> {
        self.plugins.values().collect()
    }

    /// Find which plugin provides a tool.
    pub fn find_tool_provider(&self, tool_name: &str) -> Option<&PluginInfo> {
        self.plugins
            .values()
            .find(|p| p.manifest.tools.iter().any(|t| t.name == tool_name))
    }

    /// Get the NATS subject for a tool.
    pub fn tool_subject(&self, tool_name: &str) -> Option<String> {
        self.find_tool_provider(tool_name).map(|p| {
            let default_prefix = format!("workforce.tools.{}", p.manifest.name);
            let prefix = p
                .manifest
                .subject_prefix
                .as_deref()
                .unwrap_or(&default_prefix);
            format!("{}.{}", prefix, tool_name)
        })
    }

    /// Count running plugins.
    pub fn running_count(&self) -> usize {
        self.plugins
            .values()
            .filter(|p| p.status == PluginStatus::Running)
            .count()
    }
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jira_plugin() -> PluginManifest {
        PluginBuilder::new("jira", "1.0.0")
            .description("Jira integration")
            .author("workforce-community")
            .tool(
                "create_ticket",
                "Create a Jira ticket",
                serde_json::json!({"type": "object", "properties": {"title": {"type": "string"}}}),
            )
            .tool(
                "search",
                "Search Jira issues",
                serde_json::json!({"type": "object", "properties": {"query": {"type": "string"}}}),
            )
            .credential("jira_api_token", "api_key", "Jira API token", true)
            .build()
    }

    #[test]
    fn build_manifest() {
        let manifest = jira_plugin();
        assert_eq!(manifest.name, "jira");
        assert_eq!(manifest.tools.len(), 2);
        assert_eq!(manifest.credentials.len(), 1);
    }

    #[test]
    fn tool_response_success() {
        let resp = ToolResponse::success("req-1", serde_json::json!({"id": 42})).with_duration(150);
        assert!(resp.success);
        assert_eq!(resp.duration_ms, 150);
        assert!(resp.error.is_none());
    }

    #[test]
    fn tool_response_error() {
        let resp = ToolResponse::error("req-1", "Connection refused");
        assert!(!resp.success);
        assert_eq!(resp.error, Some("Connection refused".to_string()));
    }

    #[test]
    fn plugin_registry() {
        let mut reg = PluginRegistry::new();
        reg.register(jira_plugin());

        assert_eq!(reg.running_count(), 1);
        assert!(reg.get("jira").is_some());
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn find_tool_provider() {
        let mut reg = PluginRegistry::new();
        reg.register(jira_plugin());

        let provider = reg.find_tool_provider("create_ticket").unwrap();
        assert_eq!(provider.manifest.name, "jira");
        assert!(reg.find_tool_provider("unknown_tool").is_none());
    }

    #[test]
    fn tool_subject() {
        let mut reg = PluginRegistry::new();
        reg.register(jira_plugin());

        let subject = reg.tool_subject("create_ticket").unwrap();
        assert_eq!(subject, "workforce.tools.jira.create_ticket");
    }

    #[test]
    fn manifest_serialization() {
        let manifest = jira_plugin();
        let json = serde_json::to_string(&manifest).unwrap();
        let parsed: PluginManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "jira");
        assert_eq!(parsed.tools.len(), 2);
    }
}
