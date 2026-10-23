use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::info;
use workforce_core::types::AgentId;

use crate::definition::SkillDefinition;

/// Installed skill with runtime state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledSkill {
    /// The skill definition.
    pub definition: SkillDefinition,
    /// Current lifecycle state.
    pub state: SkillState,
    /// When the skill was installed.
    pub installed_at: DateTime<Utc>,
    /// Agents this skill is assigned to.
    #[serde(default)]
    pub assigned_to: Vec<AgentId>,
    /// Credential configuration status.
    #[serde(default)]
    pub credentials_configured: bool,
    /// Config values (non-secret).
    #[serde(default)]
    pub config: HashMap<String, String>,
}

/// Skill lifecycle states.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillState {
    /// Downloaded/registered but not yet configured.
    Installed,
    /// Credentials and config provided.
    Configured,
    /// Assigned to agents and ready to use.
    Active,
    /// Temporarily disabled.
    Disabled,
    /// Error state — skill failed to activate.
    Error,
}

/// The skill registry — manages installed skills and their assignments.
pub struct SkillRegistry {
    skills: HashMap<String, InstalledSkill>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            skills: HashMap::new(),
        }
    }

    /// Install a skill from its definition.
    pub fn install(&mut self, definition: SkillDefinition) -> eyre::Result<()> {
        definition.validate()?;

        let name = definition.name.clone();
        info!(skill = %name, version = %definition.version, "Installing skill");

        let installed = InstalledSkill {
            definition,
            state: SkillState::Installed,
            installed_at: Utc::now(),
            assigned_to: Vec::new(),
            credentials_configured: false,
            config: HashMap::new(),
        };

        self.skills.insert(name, installed);
        Ok(())
    }

    /// Remove a skill.
    pub fn uninstall(&mut self, name: &str) -> eyre::Result<()> {
        self.skills
            .remove(name)
            .ok_or_else(|| eyre::eyre!("Skill '{}' not installed", name))?;
        info!(skill = %name, "Skill uninstalled");
        Ok(())
    }

    /// Configure a skill's credentials and config.
    pub fn configure(&mut self, name: &str, config: HashMap<String, String>) -> eyre::Result<()> {
        let skill = self
            .skills
            .get_mut(name)
            .ok_or_else(|| eyre::eyre!("Skill '{}' not installed", name))?;

        // Check required config
        for req in &skill.definition.requirements.config {
            if req.required && !config.contains_key(&req.name) {
                return Err(eyre::eyre!(
                    "Missing required config '{}' for skill '{}'",
                    req.name,
                    name
                ));
            }
        }

        skill.config = config;
        skill.credentials_configured = true;
        skill.state = SkillState::Configured;
        info!(skill = %name, "Skill configured");
        Ok(())
    }

    /// Assign a skill to an agent.
    pub fn assign(&mut self, skill_name: &str, agent_id: AgentId) -> eyre::Result<()> {
        let skill = self
            .skills
            .get_mut(skill_name)
            .ok_or_else(|| eyre::eyre!("Skill '{}' not installed", skill_name))?;

        if !skill.assigned_to.contains(&agent_id) {
            skill.assigned_to.push(agent_id.clone());
        }

        if skill.state == SkillState::Configured || skill.state == SkillState::Active {
            skill.state = SkillState::Active;
        }

        info!(skill = %skill_name, agent = %agent_id, "Skill assigned to agent");
        Ok(())
    }

    /// Unassign a skill from an agent.
    pub fn unassign(&mut self, skill_name: &str, agent_id: &AgentId) -> eyre::Result<()> {
        let skill = self
            .skills
            .get_mut(skill_name)
            .ok_or_else(|| eyre::eyre!("Skill '{}' not installed", skill_name))?;

        skill.assigned_to.retain(|a| a != agent_id);

        if skill.assigned_to.is_empty() && skill.state == SkillState::Active {
            skill.state = SkillState::Configured;
        }

        info!(skill = %skill_name, agent = %agent_id, "Skill unassigned from agent");
        Ok(())
    }

    /// Get a skill by name.
    pub fn get(&self, name: &str) -> Option<&InstalledSkill> {
        self.skills.get(name)
    }

    /// List all installed skills.
    pub fn list(&self) -> Vec<&InstalledSkill> {
        self.skills.values().collect()
    }

    /// Get all skills assigned to a specific agent.
    pub fn agent_skills(&self, agent_id: &AgentId) -> Vec<&InstalledSkill> {
        self.skills
            .values()
            .filter(|s| s.assigned_to.contains(agent_id))
            .collect()
    }

    /// Get all tool names available to an agent (from their assigned skills).
    pub fn agent_tool_names(&self, agent_id: &AgentId) -> Vec<String> {
        self.agent_skills(agent_id)
            .iter()
            .flat_map(|s| s.definition.tool_names())
            .map(|s| s.to_string())
            .collect()
    }

    /// Get all event types an agent subscribes to (from their assigned skills).
    pub fn agent_event_types(&self, agent_id: &AgentId) -> Vec<String> {
        self.agent_skills(agent_id)
            .iter()
            .flat_map(|s| s.definition.event_types())
            .map(|s| s.to_string())
            .collect()
    }

    /// Enable a disabled skill.
    pub fn enable(&mut self, name: &str) -> eyre::Result<()> {
        let skill = self
            .skills
            .get_mut(name)
            .ok_or_else(|| eyre::eyre!("Skill '{}' not installed", name))?;

        if skill.state == SkillState::Disabled {
            skill.state = if skill.assigned_to.is_empty() {
                SkillState::Configured
            } else {
                SkillState::Active
            };
        }
        Ok(())
    }

    /// Disable a skill without removing it.
    pub fn disable(&mut self, name: &str) -> eyre::Result<()> {
        let skill = self
            .skills
            .get_mut(name)
            .ok_or_else(|| eyre::eyre!("Skill '{}' not installed", name))?;
        skill.state = SkillState::Disabled;
        Ok(())
    }

    /// Count installed skills.
    pub fn count(&self) -> usize {
        self.skills.len()
    }
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::SkillDefinition;

    fn email_skill() -> SkillDefinition {
        SkillDefinition::from_yaml(
            r#"
name: email-management
version: "1.0.0"
description: "Email tools"
tools:
  - name: email.read
    description: "Read emails"
    input_schema: {}
  - name: email.send
    description: "Send emails"
    input_schema: {}
event_sources:
  - type: gmail_webhook
    events: [email.incoming]
"#,
        )
        .unwrap()
    }

    fn git_skill() -> SkillDefinition {
        SkillDefinition::from_yaml(
            r#"
name: git-integration
version: "1.0.0"
description: "Git tools"
tools:
  - name: git.status
    description: "Check git status"
    input_schema: {}
"#,
        )
        .unwrap()
    }

    #[test]
    fn install_and_list() {
        let mut reg = SkillRegistry::new();
        reg.install(email_skill()).unwrap();
        reg.install(git_skill()).unwrap();

        assert_eq!(reg.count(), 2);
        assert!(reg.get("email-management").is_some());
        assert!(reg.get("git-integration").is_some());
    }

    #[test]
    fn full_lifecycle() {
        let mut reg = SkillRegistry::new();
        reg.install(email_skill()).unwrap();

        let skill = reg.get("email-management").unwrap();
        assert_eq!(skill.state, SkillState::Installed);

        // Configure
        reg.configure("email-management", HashMap::new()).unwrap();
        let skill = reg.get("email-management").unwrap();
        assert_eq!(skill.state, SkillState::Configured);

        // Assign
        reg.assign("email-management", AgentId::new("assistant"))
            .unwrap();
        let skill = reg.get("email-management").unwrap();
        assert_eq!(skill.state, SkillState::Active);

        // Unassign
        reg.unassign("email-management", &AgentId::new("assistant"))
            .unwrap();
        let skill = reg.get("email-management").unwrap();
        assert_eq!(skill.state, SkillState::Configured);

        // Uninstall
        reg.uninstall("email-management").unwrap();
        assert_eq!(reg.count(), 0);
    }

    #[test]
    fn agent_skills_and_tools() {
        let mut reg = SkillRegistry::new();
        reg.install(email_skill()).unwrap();
        reg.install(git_skill()).unwrap();
        reg.configure("email-management", HashMap::new()).unwrap();
        reg.configure("git-integration", HashMap::new()).unwrap();

        let agent = AgentId::new("dev");
        reg.assign("email-management", agent.clone()).unwrap();
        reg.assign("git-integration", agent.clone()).unwrap();

        let skills = reg.agent_skills(&agent);
        assert_eq!(skills.len(), 2);

        let tools = reg.agent_tool_names(&agent);
        assert_eq!(tools.len(), 3); // email.read, email.send, git.status

        let events = reg.agent_event_types(&agent);
        assert_eq!(events.len(), 1); // email.incoming
    }

    #[test]
    fn disable_enable() {
        let mut reg = SkillRegistry::new();
        reg.install(email_skill()).unwrap();
        reg.configure("email-management", HashMap::new()).unwrap();
        reg.assign("email-management", AgentId::new("a")).unwrap();

        assert_eq!(
            reg.get("email-management").unwrap().state,
            SkillState::Active
        );

        reg.disable("email-management").unwrap();
        assert_eq!(
            reg.get("email-management").unwrap().state,
            SkillState::Disabled
        );

        reg.enable("email-management").unwrap();
        assert_eq!(
            reg.get("email-management").unwrap().state,
            SkillState::Active
        );
    }
}
