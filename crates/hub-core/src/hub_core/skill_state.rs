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

    /// Record a load that only added to the skill, under one lock: its
    /// tools join those already listed, and its hint replaces the standing
    /// one only when it added any. A skill not yet listed is listed as
    /// `source` with this load's tools and hint.
    pub fn merge_loaded(
        &self,
        name: &str,
        tool_names: &[String],
        source: &str,
        context_hint: Option<String>,
    ) {
        let mut loaded = self.loaded.write();
        let entry = loaded
            .entry(name.to_string())
            .or_insert_with(|| LoadedSkillState {
                name: name.to_string(),
                tool_names: Vec::new(),
                source: source.to_string(),
                context_hint: context_hint.clone(),
            });
        let mut added = false;
        for tool in tool_names {
            if !entry.tool_names.contains(tool) {
                entry.tool_names.push(tool.clone());
                added = true;
            }
        }
        if added {
            entry.context_hint = context_hint;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_merged_load_adds_its_tools_and_keeps_the_hint_unless_it_added_any() {
        let state = SkillStateTracker::new();
        state.mark_loaded(
            "email",
            vec!["email.inbox".into()],
            "vault",
            Some("shared".into()),
        );
        state.merge_loaded(
            "email",
            &["email.setup".into()],
            "monitor",
            Some("none".into()),
        );
        let email = state.get("email").unwrap();
        assert_eq!(email.tool_names, vec!["email.inbox", "email.setup"]);
        assert_eq!(email.context_hint.as_deref(), Some("none"), "it added one");
        state.merge_loaded(
            "email",
            &["email.inbox".into()],
            "monitor",
            Some("mine".into()),
        );
        let email = state.get("email").unwrap();
        assert_eq!(email.context_hint.as_deref(), Some("none"), "it added none");
        assert_eq!(email.source, "vault");
    }
}
