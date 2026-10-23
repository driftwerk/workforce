use parking_lot::RwLock;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use super::{Tool, ToolDefinition};
use crate::llm::environment::{InstanceSource, SkillVariants};
use crate::llm::tool_schema::sanitize_tool_schema;

/// Registry of available tools.
///
/// Tools are registered by name and can be looked up for execution.
/// Uses interior mutability so tools can be added after construction.
/// Entries are kept in name order because the definitions form a cached
/// prompt prefix: the same tool set must serialise to the same bytes whatever
/// order it was assembled in.
pub struct ToolRegistry {
    tools: RwLock<BTreeMap<String, Arc<dyn Tool>>>,
    instances: RwLock<BTreeMap<String, Arc<dyn InstanceSource>>>,
    generation: AtomicU64,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: RwLock::new(BTreeMap::new()),
            instances: RwLock::new(BTreeMap::new()),
            generation: AtomicU64::new(0),
        }
    }

    /// Register a tool.
    pub fn register(&self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        tracing::info!(tool = %name, "Registered tool");
        self.tools.write().insert(name, tool);
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Advances on every registration and effective removal, so a caller can
    /// tell whether the tool set moved without diffing definitions.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Build a registry from an existing tool set in one shot, logging a single
    /// count rather than a line per tool. For per-request registries assembled
    /// on a hot path — [`register`](Self::register)'s per-tool line is meant for
    /// process startup, where it is read once.
    pub fn from_tools(tools: impl IntoIterator<Item = Arc<dyn Tool>>) -> Self {
        let registry = Self::new();
        {
            let mut map = registry.tools.write();
            for tool in tools {
                map.insert(tool.name().to_string(), tool);
            }
        }
        tracing::debug!(count = registry.len(), "Built tool registry");
        registry
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.read().get(name).cloned()
    }

    /// Check if a tool exists.
    pub fn has(&self, name: &str) -> bool {
        self.tools.read().contains_key(name)
    }

    /// List all registered tool definitions.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.read().values().map(|t| t.definition()).collect()
    }

    /// Tool definitions with `input_schema` sanitized for an MCP `tools/list`
    /// boundary (folds top-level `oneOf` / `allOf` / `anyOf`, which
    /// Anthropic-bound consumers like Claude Code reject). Use this — not
    /// [`definitions`] — when serving tools to an external MCP client.
    pub fn mcp_definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .read()
            .values()
            .map(|t| ToolDefinition {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: sanitize_tool_schema(t.input_schema()),
            })
            .collect()
    }

    /// Remove a tool by name. Returns true if it existed.
    ///
    /// Any in-flight tool calls holding an `Arc<dyn Tool>` from `get()` will
    /// complete safely — this only removes the registry entry.
    pub fn unregister(&self, name: &str) -> bool {
        let removed = self.tools.write().remove(name).is_some();
        if removed {
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
        removed
    }

    /// Return all registered tool names.
    pub fn names(&self) -> Vec<String> {
        self.tools.read().keys().cloned().collect()
    }

    /// Get the number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.read().len()
    }

    /// Check if the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.tools.read().is_empty()
    }

    /// Create a new registry with copies of all tools from this registry.
    ///
    /// Each tool `Arc` is cloned (cheap reference count bump), so the new
    /// registry shares the same tool implementations but can have tools
    /// added or replaced independently.
    pub fn clone_all(&self) -> ToolRegistry {
        Self {
            tools: RwLock::new(self.tools.read().clone()),
            instances: RwLock::new(self.instances.read().clone()),
            generation: AtomicU64::new(0),
        }
    }

    /// Record where `skill`'s instances come from, for the environments of the
    /// agent this registry belongs to.
    pub fn set_instance_source(&self, skill: &str, source: Arc<dyn InstanceSource>) {
        self.instances.write().insert(skill.to_string(), source);
    }

    pub fn clear_instance_source(&self, skill: &str) {
        self.instances.write().remove(skill);
    }

    /// Every multi-instance skill in this registry and its instances now.
    pub fn skill_variants(&self) -> BTreeMap<String, SkillVariants> {
        self.instances
            .read()
            .iter()
            .filter_map(|(skill, source)| source.variants().map(|v| (skill.clone(), v)))
            .collect()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{Tool, ToolResult};
    use async_trait::async_trait;

    struct DummyTool;

    #[async_trait]
    impl Tool for DummyTool {
        fn name(&self) -> &str {
            "test.dummy"
        }

        fn description(&self) -> &str {
            "A dummy tool for testing"
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string" }
                }
            ,
                "required": []})
        }

        async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
            let msg = input
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("no message");
            Ok(ToolResult::success(serde_json::json!({"echo": msg}), 1))
        }
    }

    #[test]
    fn register_and_lookup() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(DummyTool));

        assert!(registry.has("test.dummy"));
        assert!(!registry.has("nonexistent"));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn list_definitions() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(DummyTool));

        let defs = registry.definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "test.dummy");
    }

    /// Guards the workforce → MCP → Claude Code → Anthropic chain: a tool
    /// with root-level `oneOf` must come back from `mcp_definitions()` with
    /// the offending key stripped, even though `definitions()` keeps it.
    #[test]
    fn mcp_definitions_strip_top_level_constraint_keys() {
        struct OneOfTool;

        #[async_trait]
        impl Tool for OneOfTool {
            fn name(&self) -> &str {
                "test.one_of"
            }
            fn description(&self) -> &str {
                "Tool with root-level oneOf (rejected by Anthropic)"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "a": { "type": "string" },
                        "b": { "type": "string" }
                    },
                    "oneOf": [{ "required": ["a"] }, { "required": ["b"] }]
                })
            }
            async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
                Ok(ToolResult::success(serde_json::Value::Null, 1))
            }
        }

        let registry = ToolRegistry::new();
        registry.register(Arc::new(OneOfTool));

        assert!(registry.definitions()[0]
            .input_schema
            .get("oneOf")
            .is_some());
        let mcp = registry.mcp_definitions();
        assert_eq!(mcp.len(), 1);
        assert!(mcp[0].input_schema.get("oneOf").is_none());
        assert!(mcp[0].input_schema.get("allOf").is_none());
        assert!(mcp[0].input_schema.get("anyOf").is_none());
        assert_eq!(mcp[0].input_schema["type"], serde_json::json!("object"));
    }

    #[test]
    fn unregister_existing() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(DummyTool));
        assert!(registry.unregister("test.dummy"));
        assert!(!registry.has("test.dummy"));
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn unregister_nonexistent() {
        let registry = ToolRegistry::new();
        assert!(!registry.unregister("nonexistent"));
    }

    #[test]
    fn names_returns_all() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(DummyTool));
        let names = registry.names();
        assert!(names.contains(&"test.dummy".to_string()));
    }

    fn named(name: &str) -> Arc<dyn Tool> {
        struct Named(String);

        #[async_trait]
        impl Tool for Named {
            fn name(&self) -> &str {
                &self.0
            }
            fn description(&self) -> &str {
                "named"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
                Ok(ToolResult::success(serde_json::Value::Null, 1))
            }
        }

        Arc::new(Named(name.to_string()))
    }

    /// The definitions form a cached prompt prefix, so the order on the wire
    /// must not depend on the order tools happened to be registered in.
    #[test]
    fn tools_list_in_name_order_whatever_the_registration_order() {
        let registry = ToolRegistry::new();
        for name in ["zeta", "alpha", "mid"] {
            registry.register(named(name));
        }
        let expected = ["alpha", "mid", "zeta"];
        assert_eq!(registry.names(), expected);
        let defs: Vec<String> = registry.definitions().into_iter().map(|d| d.name).collect();
        assert_eq!(defs, expected);
        let mcp: Vec<String> = registry
            .mcp_definitions()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(mcp, expected);
        assert_eq!(registry.clone_all().names(), expected);
    }

    #[test]
    fn generation_advances_on_register_and_effective_unregister_only() {
        let registry = ToolRegistry::new();
        assert_eq!(registry.generation(), 0);
        registry.register(Arc::new(DummyTool));
        assert_eq!(registry.generation(), 1);
        registry.register(Arc::new(DummyTool));
        assert_eq!(registry.generation(), 2, "a replacement is a change too");
        assert!(!registry.unregister("missing"));
        assert_eq!(registry.generation(), 2, "removing nothing changes nothing");
        assert!(registry.unregister("test.dummy"));
        assert_eq!(registry.generation(), 3);
        assert_eq!(registry.clone_all().generation(), 0);
    }

    fn variants(instances: &[&str]) -> Arc<dyn InstanceSource> {
        Arc::new(SkillVariants::of(instances.iter().map(|i| i.to_string()), instances[0]).unwrap())
    }

    #[test]
    fn instance_sources_are_per_registry() {
        let minny = ToolRegistry::new();
        let other = ToolRegistry::new();
        minny.set_instance_source("linear", variants(&["driftwerk", "fabrikam-crm"]));
        other.set_instance_source("linear", variants(&["acme", "beta"]));
        assert!(minny.skill_variants()["linear"].has("fabrikam-crm"));
        assert!(!other.skill_variants()["linear"].has("fabrikam-crm"));
    }

    #[test]
    fn instance_sources_survive_clone_all_and_clear() {
        let registry = ToolRegistry::new();
        registry.set_instance_source("linear", variants(&["a", "b"]));
        assert!(registry.clone_all().skill_variants().contains_key("linear"));
        registry.clear_instance_source("linear");
        assert!(registry.skill_variants().is_empty());
    }

    #[test]
    fn instance_sources_are_read_live() {
        struct Live(parking_lot::Mutex<Vec<String>>);
        impl InstanceSource for Live {
            fn variants(&self) -> Option<SkillVariants> {
                SkillVariants::of(self.0.lock().clone(), "a")
            }
        }
        let live = Arc::new(Live(parking_lot::Mutex::new(vec!["a".into()])));
        let registry = ToolRegistry::new();
        registry.set_instance_source("database", live.clone());
        assert!(registry.skill_variants().is_empty());
        live.0.lock().push("b".into());
        assert!(registry.skill_variants()["database"].has("b"));
    }

    #[tokio::test]
    async fn execute_tool() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(DummyTool));

        let tool = registry.get("test.dummy").unwrap();
        let result = tool
            .execute(serde_json::json!({"message": "hello"}))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["echo"], "hello");
    }
}
