use serde::{Deserialize, Serialize};

/// A plugin manifest — declares what a NATS-based plugin provides.
///
/// Plugins are standalone processes (in any language) that communicate
/// via NATS. They publish their manifest, then listen for tool invocations
/// on their registered subjects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    /// Plugin name (unique identifier).
    pub name: String,
    /// Version.
    pub version: String,
    /// Description.
    pub description: String,
    /// Tools this plugin provides.
    pub tools: Vec<PluginTool>,
    /// NATS subject prefix for this plugin's tools.
    /// Tools are invoked at: `workforce.tools.{name}.{tool_name}`
    pub subject_prefix: String,
    /// Heartbeat interval in seconds.
    #[serde(default = "default_heartbeat")]
    pub heartbeat_interval_secs: u64,
}

/// A tool exposed by a plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginTool {
    /// Tool name.
    pub name: String,
    /// Description.
    pub description: String,
    /// JSON Schema for input.
    pub input_schema: serde_json::Value,
    /// Expected timeout for this tool in seconds.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

/// A tool invocation request sent to a plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRequest {
    /// Request ID for correlation.
    pub request_id: String,
    /// Tool name being invoked.
    pub tool_name: String,
    /// Input parameters.
    pub input: serde_json::Value,
    /// Who requested this invocation.
    pub caller: String,
}

/// A tool invocation response from a plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginResponse {
    /// Request ID for correlation.
    pub request_id: String,
    /// Whether the invocation succeeded.
    pub success: bool,
    /// Output data.
    pub output: serde_json::Value,
    /// Error message (if failed).
    pub error: Option<String>,
    /// Execution time in milliseconds.
    pub duration_ms: u64,
}

/// Plugin health status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginHeartbeat {
    /// Plugin name.
    pub name: String,
    /// Version.
    pub version: String,
    /// Whether the plugin is healthy.
    pub healthy: bool,
    /// Active tool count.
    pub tools_count: usize,
    /// Uptime in seconds.
    pub uptime_secs: u64,
}

/// Plugin registry — tracks connected plugins and their capabilities.
pub struct PluginRegistry {
    plugins: std::collections::HashMap<String, RegisteredPlugin>,
}

/// A registered plugin with its manifest and health status.
#[derive(Debug, Clone)]
pub struct RegisteredPlugin {
    pub manifest: PluginManifest,
    pub last_heartbeat: chrono::DateTime<chrono::Utc>,
    pub healthy: bool,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self {
            plugins: std::collections::HashMap::new(),
        }
    }

    /// Register a plugin from its manifest.
    pub fn register(&mut self, manifest: PluginManifest) {
        let name = manifest.name.clone();
        tracing::info!(plugin = %name, version = %manifest.version, tools = manifest.tools.len(), "Plugin registered");
        self.plugins.insert(
            name,
            RegisteredPlugin {
                manifest,
                last_heartbeat: chrono::Utc::now(),
                healthy: true,
            },
        );
    }

    /// Update health status from a heartbeat.
    pub fn heartbeat(&mut self, heartbeat: &PluginHeartbeat) {
        if let Some(plugin) = self.plugins.get_mut(&heartbeat.name) {
            plugin.last_heartbeat = chrono::Utc::now();
            plugin.healthy = heartbeat.healthy;
        }
    }

    /// Remove a plugin.
    pub fn deregister(&mut self, name: &str) {
        self.plugins.remove(name);
    }

    /// Get a plugin by name.
    pub fn get(&self, name: &str) -> Option<&RegisteredPlugin> {
        self.plugins.get(name)
    }

    /// Find which plugin provides a given tool.
    pub fn find_tool_provider(&self, tool_name: &str) -> Option<(&str, &PluginTool)> {
        for (name, plugin) in &self.plugins {
            if plugin.healthy {
                if let Some(tool) = plugin.manifest.tools.iter().find(|t| t.name == tool_name) {
                    return Some((name.as_str(), tool));
                }
            }
        }
        None
    }

    /// Get the NATS subject for invoking a plugin tool.
    pub fn tool_subject(&self, plugin_name: &str, tool_name: &str) -> Option<String> {
        self.plugins
            .get(plugin_name)
            .map(|p| format!("{}.{}", p.manifest.subject_prefix, tool_name))
    }

    /// List all healthy plugins.
    pub fn healthy_plugins(&self) -> Vec<&RegisteredPlugin> {
        self.plugins.values().filter(|p| p.healthy).collect()
    }

    /// List all available tools from all healthy plugins.
    pub fn available_tools(&self) -> Vec<(&str, &PluginTool)> {
        self.plugins
            .iter()
            .filter(|(_, p)| p.healthy)
            .flat_map(|(name, p)| p.manifest.tools.iter().map(move |t| (name.as_str(), t)))
            .collect()
    }

    /// Count registered plugins.
    pub fn count(&self) -> usize {
        self.plugins.len()
    }

    /// Mark stale plugins (no heartbeat within 2x interval) as unhealthy.
    pub fn check_health(&mut self) {
        let now = chrono::Utc::now();
        for plugin in self.plugins.values_mut() {
            let max_age =
                chrono::Duration::seconds(plugin.manifest.heartbeat_interval_secs as i64 * 2);
            if now - plugin.last_heartbeat > max_age && plugin.healthy {
                tracing::warn!(plugin = %plugin.manifest.name, "Plugin marked unhealthy (stale heartbeat)");
                plugin.healthy = false;
            }
        }
    }
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginManifest {
    /// Parse from YAML.
    pub fn from_yaml(yaml: &str) -> eyre::Result<Self> {
        serde_yaml::from_str(yaml)
            .map_err(|e| eyre::eyre!("Failed to parse plugin manifest: {}", e))
    }
}

fn default_heartbeat() -> u64 {
    30
}

fn default_timeout() -> u64 {
    30
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_manifest() -> PluginManifest {
        PluginManifest::from_yaml(
            r#"
name: python-data-tools
version: "1.0.0"
description: "Data processing tools in Python"
subject_prefix: "workforce.tools.python-data-tools"
tools:
  - name: csv.parse
    description: "Parse a CSV file"
    input_schema:
      type: object
      properties:
        path: { type: string }
    timeout_secs: 60
  - name: csv.transform
    description: "Transform CSV data"
    input_schema:
      type: object
      properties:
        data: { type: array }
        operations: { type: array }
"#,
        )
        .unwrap()
    }

    #[test]
    fn register_and_find_tool() {
        let mut reg = PluginRegistry::new();
        reg.register(test_manifest());

        assert_eq!(reg.count(), 1);

        let (plugin, tool) = reg.find_tool_provider("csv.parse").unwrap();
        assert_eq!(plugin, "python-data-tools");
        assert_eq!(tool.name, "csv.parse");
        assert_eq!(tool.timeout_secs, 60);
    }

    #[test]
    fn tool_subject_generation() {
        let mut reg = PluginRegistry::new();
        reg.register(test_manifest());

        let subject = reg.tool_subject("python-data-tools", "csv.parse").unwrap();
        assert_eq!(subject, "workforce.tools.python-data-tools.csv.parse");
    }

    #[test]
    fn available_tools_from_healthy_plugins() {
        let mut reg = PluginRegistry::new();
        reg.register(test_manifest());

        let tools = reg.available_tools();
        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn unhealthy_plugin_excluded() {
        let mut reg = PluginRegistry::new();
        reg.register(test_manifest());

        // Mark unhealthy
        reg.plugins.get_mut("python-data-tools").unwrap().healthy = false;

        assert!(reg.find_tool_provider("csv.parse").is_none());
        assert!(reg.available_tools().is_empty());
        assert!(reg.healthy_plugins().is_empty());
    }

    #[test]
    fn heartbeat_updates_status() {
        let mut reg = PluginRegistry::new();
        reg.register(test_manifest());

        reg.plugins.get_mut("python-data-tools").unwrap().healthy = false;

        reg.heartbeat(&PluginHeartbeat {
            name: "python-data-tools".to_string(),
            version: "1.0.0".to_string(),
            healthy: true,
            tools_count: 2,
            uptime_secs: 3600,
        });

        assert!(reg.get("python-data-tools").unwrap().healthy);
    }

    #[test]
    fn request_response_roundtrip() {
        let request = PluginRequest {
            request_id: "req-1".to_string(),
            tool_name: "csv.parse".to_string(),
            input: serde_json::json!({"path": "/data/input.csv"}),
            caller: "dev-agent".to_string(),
        };

        let json = serde_json::to_string(&request).unwrap();
        let parsed: PluginRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.request_id, "req-1");

        let response = PluginResponse {
            request_id: "req-1".to_string(),
            success: true,
            output: serde_json::json!({"rows": 42, "columns": 5}),
            error: None,
            duration_ms: 150,
        };

        let json = serde_json::to_string(&response).unwrap();
        let parsed: PluginResponse = serde_json::from_str(&json).unwrap();
        assert!(parsed.success);
    }

    #[test]
    fn deregister() {
        let mut reg = PluginRegistry::new();
        reg.register(test_manifest());
        assert_eq!(reg.count(), 1);

        reg.deregister("python-data-tools");
        assert_eq!(reg.count(), 0);
    }
}
