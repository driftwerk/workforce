//! Which skills an agent has loaded, and the tools each brought.

use std::collections::HashMap;

use parking_lot::RwLock;
use serde::Serialize;

/// Tracks which skills are loaded at runtime and their tool names.
#[derive(Debug)]
pub struct SkillStateTracker {
    loaded: RwLock<HashMap<String, LoadedSkillState>>,
}

/// State of a loaded skill.
#[derive(Debug, Clone, Serialize)]
pub struct LoadedSkillState {
    pub name: String,
    pub tool_names: Vec<String>,
    /// How the skill was loaded: "vault", "config", or "env".
    pub source: String,
    /// Human-readable context about the loaded configuration (e.g. account/workspace info).
    /// Injected into the agent's system prompt so it knows which accounts are available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_hint: Option<String>,
}

impl Default for SkillStateTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillStateTracker {
    pub fn new() -> Self {
        Self {
            loaded: RwLock::new(HashMap::new()),
        }
    }

    pub fn mark_loaded(
        &self,
        name: &str,
        tool_names: Vec<String>,
        source: &str,
        context_hint: Option<String>,
    ) {
        self.loaded.write().insert(
            name.to_string(),
            LoadedSkillState {
                name: name.to_string(),
                tool_names,
                source: source.to_string(),
                context_hint,
            },
        );
    }

    pub fn mark_unloaded(&self, name: &str) {
        self.loaded.write().remove(name);
    }

    pub fn is_loaded(&self, name: &str) -> bool {
        self.loaded.read().contains_key(name)
    }

    pub fn get(&self, name: &str) -> Option<LoadedSkillState> {
        self.loaded.read().get(name).cloned()
    }

    pub fn all_loaded(&self) -> Vec<LoadedSkillState> {
        self.loaded.read().values().cloned().collect()
    }
}
