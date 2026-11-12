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
    /// Which vault tiers this agent's connectors read their keys from, kept
    /// by every credential store its adapters load with. Empty: all of them.
    credential_pins: Arc<workforce_common::credential_pin::PinSlot>,
    /// The registry this one was copied from, for a copy that may still take
    /// tools the source gains later ([`follow`](Self::follow)).
    upstream: Option<Upstream>,
    /// What every tool registered here is wrapped in ([`Self::guard_with`]);
    /// kept by every copy, so a tool a copy registers is held alike.
    guard: RwLock<Option<Guard>>,
    /// The names whose tool now is the hub's own, as registered by
    /// [`Self::register_builtin`]: a skill, an MCP server or anything else
    /// registered under the same name since takes the name out.
    builtins: RwLock<std::collections::BTreeSet<String>>,
}

/// A wrapper every tool a registry holds runs through, such as one that
/// refuses a call its agent may not make now.
pub type Guard = Arc<dyn Fn(Arc<dyn Tool>) -> Arc<dyn Tool> + Send + Sync>;

/// Where a [followed](ToolRegistry::follow) copy takes later tools from, and
/// which of them it lets in.
struct Upstream {
    source: Arc<ToolRegistry>,
    admits: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    /// Tools registered on the copy itself after it was made (a turn's own
    /// overrides, such as a node-routed `exec.run`): never replaced from the
    /// source.
    own: RwLock<std::collections::BTreeSet<String>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: RwLock::new(BTreeMap::new()),
            instances: RwLock::new(BTreeMap::new()),
            generation: AtomicU64::new(0),
            credential_pins: Arc::default(),
            upstream: None,
            guard: RwLock::new(None),
            builtins: RwLock::default(),
        }
    }

    /// An empty registry for the same agent as `source`: a skill it loads
    /// later reads its keys as `source`'s agent's are pinned.
    pub fn empty_like(source: &ToolRegistry) -> Self {
        Self {
            credential_pins: source.credential_pins(),
            guard: RwLock::new(source.guard.read().clone()),
            ..Self::new()
        }
    }

    /// Wrap every tool registered here in `guard`: the ones it holds now,
    /// and each one registered later, here or on a copy. Set once; the tools
    /// already held are wrapped once.
    pub fn guard_with(&self, guard: Guard) {
        let mut held = self.guard.write();
        if held.is_some() {
            return;
        }
        for tool in self.tools.write().values_mut() {
            *tool = guard(Arc::clone(tool));
        }
        *held = Some(guard);
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// The slot this agent's credential pins go in.
    pub fn credential_pins(&self) -> Arc<workforce_common::credential_pin::PinSlot> {
        Arc::clone(&self.credential_pins)
    }

    /// Register a tool.
    pub fn register(&self, tool: Arc<dyn Tool>) {
        let tool = match self.guard.read().as_ref() {
            Some(guard) => guard(tool),
            None => tool,
        };
        let name = tool.name().to_string();
        tracing::info!(tool = %name, "Registered tool");
        if let Some(upstream) = &self.upstream {
            upstream.own.write().insert(name.clone());
        }
        self.builtins.write().remove(&name);
        self.tools.write().insert(name, tool);
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Register one of the hub's own tools: [`Self::is_builtin`] holds for
    /// its name until something else is registered under it.
    pub fn register_builtin(&self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        self.register(tool);
        self.builtins.write().insert(name);
    }

    /// Whether the tool registered as `name` now is the hub's own
    /// ([`Self::register_builtin`]), not a skill's, an MCP server's or any
    /// other that took the name.
    pub fn is_builtin(&self, name: &str) -> bool {
        self.has(name) && self.builtins.read().contains(name)
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
        self.builtins.write().remove(name);
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
            credential_pins: Arc::clone(&self.credential_pins),
            upstream: None,
            guard: RwLock::new(self.guard.read().clone()),
            builtins: RwLock::new(self.builtins.read().clone()),
        }
    }

    /// A copy of `source`, like [`clone_all`](Self::clone_all), that can
    /// later [`adopt`](Self::adopt) tools `source` gains, when `admits` lets
    /// them in. A turn runs on such a copy, so a tool its call registers in
    /// the agent's registry mid-turn (`system.enable_skill`, a WASM install)
    /// can still be offered and run in that same turn; `admits` keeps out the
    /// tools the copy deliberately goes without.
    pub fn follow(
        source: &Arc<ToolRegistry>,
        admits: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) -> ToolRegistry {
        Self {
            upstream: Some(Upstream {
                source: Arc::clone(source),
                admits: Arc::new(admits),
                own: RwLock::default(),
            }),
            ..source.clone_all()
        }
    }

    /// Take from the registry this one [follows](Self::follow) the tools in
    /// `names` it admits and lacks, or holds in an older instance than the
    /// source's (a skill re-enabled with new credentials), through every
    /// registry in between; the names it took. A tool registered on the copy
    /// itself is kept. A registry that follows none takes nothing.
    pub fn adopt(&self, names: &[String]) -> Vec<String> {
        let Some(upstream) = &self.upstream else {
            return Vec::new();
        };
        let wanted: Vec<String> = {
            let own = upstream.own.read();
            names
                .iter()
                .filter(|name| (upstream.admits)(name) && !own.contains(name.as_str()))
                .cloned()
                .collect()
        };
        if wanted.is_empty() {
            return Vec::new();
        }
        upstream.source.adopt(&wanted);
        let same = |a: &Arc<dyn Tool>, b: &Arc<dyn Tool>| {
            std::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b))
        };
        let taken: Vec<String> = {
            let mut tools = self.tools.write();
            wanted
                .into_iter()
                .filter_map(|name| {
                    let fresh = upstream.source.get(&name)?;
                    if tools.get(&name).is_some_and(|held| same(held, &fresh)) {
                        return None;
                    }
                    tools.insert(name.clone(), fresh);
                    let mut builtins = self.builtins.write();
                    match upstream.source.is_builtin(&name) {
                        true => builtins.insert(name.clone()),
                        false => builtins.remove(&name),
                    };
                    Some(name)
                })
                .collect()
        };
        if !taken.is_empty() {
            let mut instances = self.instances.write();
            for (skill, source) in upstream.source.instances.read().iter() {
                instances.insert(skill.clone(), Arc::clone(source));
            }
            self.generation.fetch_add(1, Ordering::Relaxed);
            tracing::info!(tools = ?taken, "Turn took tools registered during it");
        }
        taken
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
    fn a_builtin_stays_one_only_until_something_takes_its_name() {
        let source = Arc::new(ToolRegistry::new());
        source.register_builtin(Arc::new(DummyTool));
        assert!(source.is_builtin("test.dummy"));
        let copy = ToolRegistry::follow(&source, |_| true);
        assert!(copy.is_builtin("test.dummy"), "a copy keeps it");
        // A skill or an MCP server registering the same name takes it.
        source.register(Arc::new(DummyTool));
        assert!(!source.is_builtin("test.dummy"));
        assert!(
            copy.is_builtin("test.dummy"),
            "the copy still holds the built-in"
        );
        copy.adopt(&["test.dummy".into()]);
        assert!(
            !copy.is_builtin("test.dummy"),
            "adopting the shadow takes it in the copy too"
        );
        source.register_builtin(Arc::new(DummyTool));
        source.unregister("test.dummy");
        assert!(!source.is_builtin("test.dummy"));
        assert!(
            !ToolRegistry::from_tools([Arc::new(DummyTool) as Arc<dyn Tool>])
                .is_builtin("test.dummy")
        );
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

    /// A followed copy takes, by name, a tool its source gained after the
    /// copy was made, and moves its generation so a turn's plan rebuilds.
    #[test]
    fn a_followed_copy_adopts_tools_its_source_gains_later() {
        let source = Arc::new(ToolRegistry::new());
        source.register(named("alpha"));
        let copy = ToolRegistry::follow(&source, |_| true);
        source.register(named("jira.create_issue"));
        assert!(!copy.has("jira.create_issue"), "nothing arrives unasked");
        let before = copy.generation();
        let taken = copy.adopt(&["jira.create_issue".into(), "alpha".into(), "nope".into()]);
        assert_eq!(taken, ["jira.create_issue"]);
        assert!(copy.has("jira.create_issue"));
        assert!(copy.generation() > before);
        assert!(copy.adopt(&["jira.create_issue".into()]).is_empty());
    }

    /// A skill re-enabled mid-turn (new credentials) re-registers its tools;
    /// the copy swaps in the new instances, but keeps a tool it registered
    /// itself.
    #[test]
    fn a_followed_copy_takes_a_reregistered_tool_but_keeps_its_own() {
        let source = Arc::new(ToolRegistry::new());
        source.register(named("jira.search"));
        source.register(named("exec.run"));
        let copy = ToolRegistry::follow(&source, |_| true);
        let own = named("exec.run");
        copy.register(Arc::clone(&own));
        let reloaded = named("jira.search");
        source.register(Arc::clone(&reloaded));
        source.register(named("exec.run"));
        let taken = copy.adopt(&["jira.search".into(), "exec.run".into()]);
        assert_eq!(taken, ["jira.search"]);
        let held = copy.get("jira.search").unwrap();
        assert!(std::ptr::addr_eq(
            Arc::as_ptr(&held),
            Arc::as_ptr(&reloaded)
        ));
        let kept = copy.get("exec.run").unwrap();
        assert!(std::ptr::addr_eq(Arc::as_ptr(&kept), Arc::as_ptr(&own)));
    }

    /// What a copy goes without stays out, through every copy in between.
    #[test]
    fn adoption_admits_only_what_every_copy_lets_in() {
        let source = Arc::new(ToolRegistry::new());
        let middle = Arc::new(ToolRegistry::follow(&source, |name| name != "hidden.one"));
        let turn = ToolRegistry::follow(&middle, |name| name != "hidden.two");
        for name in ["hidden.one", "hidden.two", "shown"] {
            source.register(named(name));
        }
        let taken = turn.adopt(&["hidden.one".into(), "hidden.two".into(), "shown".into()]);
        assert_eq!(taken, ["shown"]);
        assert!(!turn.has("hidden.one") && !turn.has("hidden.two"));
        assert!(!middle.has("hidden.one"));
        assert!(ToolRegistry::new().adopt(&["shown".into()]).is_empty());
        assert!(source.clone_all().adopt(&["shown".into()]).is_empty());
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

    #[test]
    fn a_guard_wraps_every_tool_held_and_registered_later_and_on_copies() {
        struct Named(&'static str);
        #[async_trait]
        impl Tool for Named {
            fn name(&self) -> &str {
                self.0
            }
            fn description(&self) -> &str {
                "plain"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _: serde_json::Value) -> eyre::Result<ToolResult> {
                Ok(ToolResult::success(serde_json::json!("ran"), 0))
            }
        }
        struct Guarded(Arc<dyn Tool>);
        #[async_trait]
        impl Tool for Guarded {
            fn name(&self) -> &str {
                self.0.name()
            }
            fn description(&self) -> &str {
                "guarded"
            }
            fn input_schema(&self) -> serde_json::Value {
                self.0.input_schema()
            }
            async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
                self.0.execute(input).await
            }
        }
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(Named("before")));
        registry.guard_with(Arc::new(|tool| Arc::new(Guarded(tool)) as Arc<dyn Tool>));
        registry.guard_with(Arc::new(|tool| tool));
        registry.register(Arc::new(Named("after")));
        let copy = ToolRegistry::follow(&registry, |_| true);
        copy.register(Arc::new(Named("on-copy")));
        for (holder, name) in [
            (&*registry, "before"),
            (&*registry, "after"),
            (&copy, "on-copy"),
        ] {
            assert_eq!(holder.get(name).unwrap().description(), "guarded", "{name}");
        }
    }
}
