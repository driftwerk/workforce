use serde::{Deserialize, Serialize};

/// A skill definition — the blueprint for a capability package.
///
/// Skills bundle tools, event sources, default policies, memory schemas,
/// and credential requirements into a single installable unit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDefinition {
    /// Unique skill name (e.g., "email-management").
    pub name: String,
    /// Semantic version.
    pub version: String,
    /// Human-readable description.
    pub description: String,
    /// Author or organisation.
    #[serde(default)]
    pub author: String,
    /// License identifier.
    #[serde(default)]
    pub license: String,
    /// Tools this skill provides.
    #[serde(default)]
    pub tools: Vec<SkillTool>,
    /// Event sources this skill can activate.
    #[serde(default)]
    pub event_sources: Vec<SkillEventSource>,
    /// Suggested default policies for the tools.
    #[serde(default)]
    pub default_policies: Vec<SkillPolicy>,
    /// Memory schemas this skill uses.
    #[serde(default)]
    pub memory_schemas: Vec<SkillMemorySchema>,
    /// Credentials and config this skill needs.
    #[serde(default)]
    pub requirements: SkillRequirements,
    /// Dependencies on other skills.
    #[serde(default)]
    pub dependencies: Vec<String>,
}

/// A tool provided by a skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillTool {
    /// Tool name (e.g., "email.read").
    pub name: String,
    /// What the tool does.
    pub description: String,
    /// JSON Schema for the tool's input.
    pub input_schema: serde_json::Value,
}

/// An event source provided by a skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEventSource {
    /// Adapter type (e.g., "gmail_webhook", "github_webhook").
    pub r#type: String,
    /// Events this source publishes.
    #[serde(default)]
    pub events: Vec<String>,
    /// Config keys required to activate this source.
    #[serde(default)]
    pub config_required: Vec<String>,
}

/// A default policy for a skill's tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillPolicy {
    /// Tool name pattern.
    pub tool: String,
    /// Default action (allow, deny, require_approval).
    pub action: String,
}

/// A memory schema used by the skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMemorySchema {
    pub name: String,
    pub description: String,
}

/// Credentials and config a skill needs to function.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillRequirements {
    /// Required credentials.
    #[serde(default)]
    pub credentials: Vec<CredentialRequirement>,
    /// Configuration values.
    #[serde(default)]
    pub config: Vec<ConfigRequirement>,
}

/// A credential the skill needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialRequirement {
    /// Credential name/key.
    pub name: String,
    /// Type (e.g., "oauth2", "api_key", "token").
    pub r#type: String,
    /// Description of what it's for.
    pub description: String,
}

/// A config value the skill needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigRequirement {
    /// Config key name.
    pub name: String,
    /// Value type (e.g., "string", "int", "bool").
    pub r#type: String,
    /// Description.
    pub description: String,
    /// Whether this config is required.
    #[serde(default)]
    pub required: bool,
}

impl SkillDefinition {
    /// Parse a skill definition from YAML.
    pub fn from_yaml(yaml: &str) -> eyre::Result<Self> {
        serde_yaml::from_str(yaml)
            .map_err(|e| eyre::eyre!("Failed to parse skill definition: {}", e))
    }

    /// Validate the skill definition.
    pub fn validate(&self) -> eyre::Result<()> {
        if self.name.is_empty() {
            return Err(eyre::eyre!("Skill name cannot be empty"));
        }
        if self.version.is_empty() {
            return Err(eyre::eyre!("Skill version cannot be empty"));
        }
        if self.tools.is_empty() && self.event_sources.is_empty() {
            return Err(eyre::eyre!(
                "Skill must provide at least one tool or event source"
            ));
        }
        // Validate tool names
        for tool in &self.tools {
            if tool.name.is_empty() {
                return Err(eyre::eyre!("Tool name cannot be empty"));
            }
        }
        Ok(())
    }

    /// Get all tool names.
    pub fn tool_names(&self) -> Vec<&str> {
        self.tools.iter().map(|t| t.name.as_str()).collect()
    }

    /// Get all event types this skill can produce.
    pub fn event_types(&self) -> Vec<&str> {
        self.event_sources
            .iter()
            .flat_map(|es| es.events.iter().map(|e| e.as_str()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMAIL_SKILL_YAML: &str = r#"
name: email-management
version: "1.2.0"
description: "Read, compose, send, and manage emails via Gmail"
author: workforce-community
license: MIT

tools:
  - name: email.read
    description: "Read emails from inbox"
    input_schema:
      type: object
      properties:
        label: { type: string }
        limit: { type: integer }
  - name: email.send
    description: "Send an email"
    input_schema:
      type: object
      properties:
        to: { type: string }
        subject: { type: string }
        body: { type: string }
      required: [to, subject, body]

event_sources:
  - type: gmail_webhook
    events: [email.incoming, email.label_changed]
    config_required: [gmail_account, gmail_credentials]

default_policies:
  - tool: email.read
    action: allow
  - tool: email.send
    action: require_approval

memory_schemas:
  - name: email_contacts
    description: "Known contacts and context"

requirements:
  credentials:
    - name: gmail_credentials
      type: oauth2
      description: "Gmail OAuth2 token"
  config:
    - name: default_signature
      type: string
      description: "Email signature"
      required: false

dependencies: []
"#;

    #[test]
    fn parse_email_skill() {
        let skill = SkillDefinition::from_yaml(EMAIL_SKILL_YAML).unwrap();
        assert_eq!(skill.name, "email-management");
        assert_eq!(skill.version, "1.2.0");
        assert_eq!(skill.tools.len(), 2);
        assert_eq!(skill.event_sources.len(), 1);
        assert_eq!(skill.default_policies.len(), 2);
        assert_eq!(skill.memory_schemas.len(), 1);
        assert_eq!(skill.requirements.credentials.len(), 1);
        assert_eq!(skill.requirements.config.len(), 1);
        assert!(skill.validate().is_ok());
    }

    #[test]
    fn tool_and_event_names() {
        let skill = SkillDefinition::from_yaml(EMAIL_SKILL_YAML).unwrap();
        assert_eq!(skill.tool_names(), vec!["email.read", "email.send"]);
        assert_eq!(
            skill.event_types(),
            vec!["email.incoming", "email.label_changed"]
        );
    }

    #[test]
    fn validate_empty_name() {
        let yaml = r#"
name: ""
version: "1.0.0"
description: "Bad skill"
tools:
  - name: test
    description: test
    input_schema: {}
"#;
        let skill = SkillDefinition::from_yaml(yaml).unwrap();
        assert!(skill.validate().is_err());
    }

    #[test]
    fn validate_no_tools_or_events() {
        let yaml = r#"
name: empty-skill
version: "1.0.0"
description: "No tools or events"
"#;
        let skill = SkillDefinition::from_yaml(yaml).unwrap();
        assert!(skill.validate().is_err());
    }
}
