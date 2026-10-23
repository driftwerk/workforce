//! SKILL.md file format — parse and validate skill definitions from markdown files
//! with YAML frontmatter.
//!
//! Skills are directories containing a SKILL.md with YAML frontmatter that declares
//! metadata, requirements, and tool names. The body of the markdown provides
//! instructions for the agent on how to use the skill's tools.

use serde::{Deserialize, Serialize};

/// Parsed SKILL.md file content — frontmatter plus body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillFile {
    /// Parsed frontmatter metadata.
    pub meta: SkillMeta,
    /// Markdown body (instructions for the agent).
    pub body: String,
}

/// YAML frontmatter of a SKILL.md file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillMeta {
    /// Unique skill name (e.g., "git-operations").
    pub name: String,
    /// Semantic version string.
    pub version: String,
    /// Human-readable description.
    pub description: String,
    /// Author or organisation.
    #[serde(default)]
    pub author: String,
    /// System requirements for this skill.
    #[serde(default)]
    pub requires: SkillRequires,
    /// Tool names this skill provides.
    #[serde(default)]
    pub tools: Vec<String>,
}

/// Requirements that must be satisfied before a skill can be loaded.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillRequires {
    /// Binary names that must exist on PATH.
    #[serde(default)]
    pub bins: Vec<String>,
    /// Environment variables that must be set.
    #[serde(default)]
    pub env: Vec<String>,
    /// Config keys that must be present (reserved for future use).
    #[serde(default)]
    pub config: Vec<String>,
    /// Vault secrets this skill needs (per-agent).
    #[serde(default)]
    pub vault: Vec<VaultRequirement>,
}

/// A vault secret that a skill requires to function.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultRequirement {
    /// Vault path (e.g., "skills/figma/token").
    pub path: String,
    /// Human-readable description of what this secret is.
    pub description: String,
    /// Whether this credential is required (vs. optional).
    #[serde(default = "default_true")]
    pub required: bool,
}

fn default_true() -> bool {
    true
}

impl SkillFile {
    /// Parse a SKILL.md file from its raw content.
    ///
    /// Expects the file to start with `---` YAML frontmatter delimiters.
    pub fn parse(content: &str) -> eyre::Result<Self> {
        let (frontmatter, body) = split_frontmatter(content)?;
        let meta: SkillMeta = serde_yaml::from_str(&frontmatter)
            .map_err(|e| eyre::eyre!("Failed to parse SKILL.md frontmatter: {}", e))?;
        meta.validate()?;
        Ok(Self { meta, body })
    }
}

impl SkillMeta {
    /// Validate the parsed metadata.
    pub fn validate(&self) -> eyre::Result<()> {
        if self.name.is_empty() {
            return Err(eyre::eyre!("Skill name cannot be empty"));
        }
        if self.version.is_empty() {
            return Err(eyre::eyre!("Skill version cannot be empty"));
        }
        if self.description.is_empty() {
            return Err(eyre::eyre!("Skill description cannot be empty"));
        }
        Ok(())
    }
}

/// Split a markdown file into YAML frontmatter and body.
///
/// The frontmatter must be delimited by `---` lines at the start of the file.
fn split_frontmatter(content: &str) -> eyre::Result<(String, String)> {
    let trimmed = content.trim_start();

    if !trimmed.starts_with("---") {
        return Err(eyre::eyre!(
            "SKILL.md must start with YAML frontmatter delimited by ---"
        ));
    }

    // Find the closing `---` (skip the first one)
    let after_first = &trimmed[3..];
    let closing_pos = after_first
        .find("\n---")
        .ok_or_else(|| eyre::eyre!("Missing closing --- delimiter in SKILL.md frontmatter"))?;

    let frontmatter = after_first[..closing_pos].trim().to_string();
    let body = after_first[closing_pos + 4..].trim().to_string();

    Ok((frontmatter, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIT_SKILL_MD: &str = r#"---
name: git-operations
version: 1.0.0
description: Git operations for agents
author: contoso
requires:
  bins: [git]
  env: [GITHUB_TOKEN]
tools:
  - git.clone
  - git.commit
  - git.push
  - git.branch
  - git.diff
---
# Git Operations Skill

Instructions for the agent on how to use git tools.

## Usage

Use `git.clone` to clone a repository, `git.commit` to commit changes, etc.
"#;

    const MINIMAL_SKILL_MD: &str = r#"---
name: minimal
version: 0.1.0
description: A minimal skill
---
Just a body.
"#;

    #[test]
    fn parse_full_skill_md() {
        let skill = SkillFile::parse(GIT_SKILL_MD).expect("should parse");
        assert_eq!(skill.meta.name, "git-operations");
        assert_eq!(skill.meta.version, "1.0.0");
        assert_eq!(skill.meta.description, "Git operations for agents");
        assert_eq!(skill.meta.author, "contoso");
        assert_eq!(skill.meta.requires.bins, vec!["git"]);
        assert_eq!(skill.meta.requires.env, vec!["GITHUB_TOKEN"]);
        assert_eq!(skill.meta.tools.len(), 5);
        assert_eq!(skill.meta.tools[0], "git.clone");
        assert!(skill.body.contains("# Git Operations Skill"));
        assert!(skill.body.contains("Use `git.clone`"));
    }

    #[test]
    fn parse_minimal_skill_md() {
        let skill = SkillFile::parse(MINIMAL_SKILL_MD).expect("should parse");
        assert_eq!(skill.meta.name, "minimal");
        assert_eq!(skill.meta.version, "0.1.0");
        assert!(skill.meta.requires.bins.is_empty());
        assert!(skill.meta.requires.env.is_empty());
        assert!(skill.meta.tools.is_empty());
        assert_eq!(skill.body, "Just a body.");
    }

    #[test]
    fn missing_frontmatter_delimiter() {
        let content = "name: bad\nversion: 1.0\n";
        let result = SkillFile::parse(content);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("must start with YAML frontmatter"));
    }

    #[test]
    fn missing_closing_delimiter() {
        let content = "---\nname: bad\nversion: 1.0\n";
        let result = SkillFile::parse(content);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Missing closing ---"));
    }

    #[test]
    fn empty_name_rejected() {
        let content = "---\nname: \"\"\nversion: 1.0.0\ndescription: test\n---\nbody\n";
        let result = SkillFile::parse(content);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("name cannot be empty"));
    }

    #[test]
    fn empty_version_rejected() {
        let content = "---\nname: test\nversion: \"\"\ndescription: test\n---\nbody\n";
        let result = SkillFile::parse(content);
        assert!(result.is_err());
    }

    #[test]
    fn empty_description_rejected() {
        let content = "---\nname: test\nversion: 1.0.0\ndescription: \"\"\n---\nbody\n";
        let result = SkillFile::parse(content);
        assert!(result.is_err());
    }

    #[test]
    fn requires_config_keys() {
        let content = r#"---
name: test-config
version: 1.0.0
description: Tests config keys
requires:
  config: [database_url, api_endpoint]
---
Body.
"#;
        let skill = SkillFile::parse(content).expect("should parse");
        assert_eq!(
            skill.meta.requires.config,
            vec!["database_url", "api_endpoint"]
        );
    }

    #[test]
    fn requires_vault_secrets() {
        let content = r#"---
name: test-vault
version: 1.0.0
description: Tests vault requirements
requires:
  vault:
    - path: skills/test/token
      description: API token
      required: true
    - path: skills/test/webhook_secret
      description: Webhook signing secret
      required: false
---
Body.
"#;
        let skill = SkillFile::parse(content).expect("should parse");
        assert_eq!(skill.meta.requires.vault.len(), 2);
        assert_eq!(skill.meta.requires.vault[0].path, "skills/test/token");
        assert!(skill.meta.requires.vault[0].required);
        assert_eq!(
            skill.meta.requires.vault[1].path,
            "skills/test/webhook_secret"
        );
        assert!(!skill.meta.requires.vault[1].required);
    }

    #[test]
    fn split_frontmatter_basic() {
        let (fm, body) = split_frontmatter("---\nfoo: bar\n---\nBody text").unwrap();
        assert_eq!(fm, "foo: bar");
        assert_eq!(body, "Body text");
    }

    #[test]
    fn split_frontmatter_multiline_body() {
        let content = "---\nkey: val\n---\nLine 1\n\nLine 2\n";
        let (fm, body) = split_frontmatter(content).unwrap();
        assert_eq!(fm, "key: val");
        assert!(body.contains("Line 1"));
        assert!(body.contains("Line 2"));
    }
}
