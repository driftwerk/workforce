//! On-disk role template system.
//!
//! Role templates are stored as individual TOML files under `{data_dir}/roles/`.
//! Each file defines a reusable agent role with a system prompt, capabilities,
//! expertise areas, and optional personality configuration.

use std::fs;
use std::path::{Path, PathBuf};

use eyre::{eyre, Result};
use serde::{Deserialize, Serialize};

use crate::settings::PersonalityConfig;

/// A reusable role template stored on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleTemplate {
    /// Unique role identifier (also the filename stem).
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// System prompt fragment for this role.
    pub system_prompt: String,
    /// Capabilities this role typically needs.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Areas of expertise.
    #[serde(default)]
    pub expertise: Vec<String>,
    /// Communication style descriptor (e.g. "thorough", "concise").
    #[serde(default)]
    pub communication_style: Option<String>,
    /// Personality dimensions.
    #[serde(default)]
    pub personality: Option<PersonalityConfig>,
}

/// Return the roles directory path.
pub fn roles_dir(data_dir: &str) -> PathBuf {
    Path::new(data_dir).join("roles")
}

/// List all role templates from disk.
pub fn list_roles(data_dir: &str) -> Result<Vec<RoleTemplate>> {
    let dir = roles_dir(data_dir);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut roles = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("toml") {
            let content = fs::read_to_string(&path)?;
            let role: RoleTemplate = toml::from_str(&content)
                .map_err(|e| eyre!("Failed to parse {}: {}", path.display(), e))?;
            roles.push(role);
        }
    }

    roles.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(roles)
}

/// The file for `role_id`, refused when the id is not a single name and so
/// would reach outside the roles directory.
fn role_path(data_dir: &str, role_id: &str) -> Result<PathBuf> {
    if !crate::config_files::is_plain_name(role_id) {
        return Err(eyre!("'{role_id}' is not a role id"));
    }
    Ok(roles_dir(data_dir).join(format!("{role_id}.toml")))
}

/// Load a single role template by ID.
pub fn load_role(data_dir: &str, role_id: &str) -> Result<RoleTemplate> {
    let path = role_path(data_dir, role_id)?;
    if !path.exists() {
        return Err(eyre!("Role '{}' not found", role_id));
    }
    let content = fs::read_to_string(&path)?;
    let role: RoleTemplate =
        toml::from_str(&content).map_err(|e| eyre!("Failed to parse role '{}': {}", role_id, e))?;
    Ok(role)
}

/// Save a role template to disk, creating the roles directory if needed.
pub fn save_role(data_dir: &str, role: &RoleTemplate) -> Result<()> {
    let path = role_path(data_dir, &role.id)?;
    fs::create_dir_all(roles_dir(data_dir))?;
    let content = toml::to_string_pretty(role)
        .map_err(|e| eyre!("Failed to serialize role '{}': {}", role.id, e))?;
    fs::write(&path, content)?;
    Ok(())
}

/// Delete a role template from disk.
pub fn delete_role(data_dir: &str, role_id: &str) -> Result<()> {
    let path = role_path(data_dir, role_id)?;
    if !path.exists() {
        return Err(eyre!("Role '{}' not found", role_id));
    }
    fs::remove_file(&path)?;
    Ok(())
}

/// Return the 4 built-in role templates seeded from `role_system_prompt()`.
pub fn builtin_roles() -> Vec<RoleTemplate> {
    vec![
        RoleTemplate {
            id: "architect".to_string(),
            name: "Systems Architect".to_string(),
            system_prompt: "You are a systems architect. Think in data flows, service boundaries, \
                and failure modes. Flag both over-engineering AND under-engineering. \
                Consider scalability, maintainability, and operational complexity. \
                Ask \"how does this fit the whole system?\" and \"what breaks at 10x scale?\". \
                Propose diagrams and architecture decision records for non-trivial choices."
                .to_string(),
            capabilities: vec!["planning".into(), "code".into(), "architecture".into()],
            expertise: vec!["systems".into(), "scalability".into(), "design".into()],
            communication_style: Some("thorough".to_string()),
            personality: Some(PersonalityConfig {
                assertiveness: Some(0.7),
                curiosity: Some(0.8),
                pragmatism: Some(0.6),
                detail_orientation: Some(0.7),
            }),
        },
        RoleTemplate {
            id: "developer".to_string(),
            name: "Senior Developer".to_string(),
            system_prompt: "You are a pragmatic senior developer. Think about implementation \
                complexity, testability, and maintainability. Suggest simpler alternatives \
                when designs are over-engineered. Ask \"how will we test this?\" and \
                \"what's the simplest approach that works?\". Favour proven patterns over \
                novel solutions. Consider debugging and deployment complexity."
                .to_string(),
            capabilities: vec!["code".into(), "testing".into(), "debugging".into()],
            expertise: vec!["implementation".into(), "testing".into(), "patterns".into()],
            communication_style: Some("concise".to_string()),
            personality: Some(PersonalityConfig {
                assertiveness: Some(0.5),
                curiosity: Some(0.6),
                pragmatism: Some(0.9),
                detail_orientation: Some(0.8),
            }),
        },
        RoleTemplate {
            id: "reviewer".to_string(),
            name: "Code Reviewer".to_string(),
            system_prompt: "You are a code reviewer. Focus on code quality, readability, and \
                adherence to established patterns. Flag inconsistencies, missing error \
                handling, and naming issues. Consider performance implications and \
                API design. Ask \"is this consistent with how we do X elsewhere?\" \
                and \"what does this look like from the caller's perspective?\"."
                .to_string(),
            capabilities: vec!["code".into(), "review".into()],
            expertise: vec!["quality".into(), "patterns".into(), "readability".into()],
            communication_style: Some("direct".to_string()),
            personality: Some(PersonalityConfig {
                assertiveness: Some(0.6),
                curiosity: Some(0.5),
                pragmatism: Some(0.7),
                detail_orientation: Some(0.9),
            }),
        },
        RoleTemplate {
            id: "product-manager".to_string(),
            name: "Product Manager".to_string(),
            system_prompt: "You are a product manager. You are user-obsessed: every decision must \
                trace back to real user value. Question assumptions. Push for simplicity \
                over feature creep. Think in user stories and outcomes. Challenge scope \
                creep with \"but why does the user need this?\". When the team proposes \
                solutions, ask who benefits and how we'll know it worked."
                .to_string(),
            capabilities: vec!["planning".into(), "writing".into(), "research".into()],
            expertise: vec!["product".into(), "roadmap".into(), "users".into()],
            communication_style: Some("thorough".to_string()),
            personality: Some(PersonalityConfig {
                assertiveness: Some(0.8),
                curiosity: Some(0.9),
                pragmatism: Some(0.7),
                detail_orientation: Some(0.5),
            }),
        },
    ]
}

/// Write built-in role templates that don't already exist on disk.
///
/// Returns the IDs of roles that were created (skips existing ones).
pub fn init_builtin_roles(data_dir: &str) -> Result<Vec<String>> {
    let dir = roles_dir(data_dir);
    fs::create_dir_all(&dir)?;

    let mut created = Vec::new();
    for role in builtin_roles() {
        let path = dir.join(format!("{}.toml", role.id));
        if !path.exists() {
            save_role(data_dir, &role)?;
            created.push(role.id);
        }
    }
    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_roles_count() {
        let roles = builtin_roles();
        assert_eq!(roles.len(), 4);
        let ids: Vec<&str> = roles.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"architect"));
        assert!(ids.contains(&"developer"));
        assert!(ids.contains(&"reviewer"));
        assert!(ids.contains(&"product-manager"));
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();

        let role = RoleTemplate {
            id: "test-role".to_string(),
            name: "Test Role".to_string(),
            system_prompt: "You are a test agent.".to_string(),
            capabilities: vec!["code".into()],
            expertise: vec!["testing".into()],
            communication_style: Some("direct".to_string()),
            personality: None,
        };

        save_role(data_dir, &role).unwrap();
        let loaded = load_role(data_dir, "test-role").unwrap();

        assert_eq!(loaded.id, "test-role");
        assert_eq!(loaded.name, "Test Role");
        assert_eq!(loaded.system_prompt, "You are a test agent.");
        assert_eq!(loaded.capabilities, vec!["code"]);
        assert_eq!(loaded.expertise, vec!["testing"]);
        assert_eq!(loaded.communication_style, Some("direct".to_string()));
    }

    #[test]
    fn a_role_id_that_leaves_the_roles_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        let role = RoleTemplate {
            id: "../agents/scout/config".to_string(),
            name: "Escape".to_string(),
            system_prompt: "[access]".to_string(),
            capabilities: Vec::new(),
            expertise: Vec::new(),
            communication_style: None,
            personality: None,
        };
        assert!(save_role(data_dir, &role).is_err());
        assert!(load_role(data_dir, &role.id).is_err());
        assert!(delete_role(data_dir, &role.id).is_err());
        assert!(!dir.path().join("agents").exists());
    }

    #[test]
    fn list_roles_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();

        let role_b = RoleTemplate {
            id: "beta".to_string(),
            name: "Beta".to_string(),
            system_prompt: "b".to_string(),
            capabilities: vec![],
            expertise: vec![],
            communication_style: None,
            personality: None,
        };
        let role_a = RoleTemplate {
            id: "alpha".to_string(),
            name: "Alpha".to_string(),
            system_prompt: "a".to_string(),
            capabilities: vec![],
            expertise: vec![],
            communication_style: None,
            personality: None,
        };

        save_role(data_dir, &role_b).unwrap();
        save_role(data_dir, &role_a).unwrap();

        let roles = list_roles(data_dir).unwrap();
        assert_eq!(roles.len(), 2);
        assert_eq!(roles[0].id, "alpha");
        assert_eq!(roles[1].id, "beta");
    }

    #[test]
    fn delete_role_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();

        let role = RoleTemplate {
            id: "doomed".to_string(),
            name: "Doomed".to_string(),
            system_prompt: "bye".to_string(),
            capabilities: vec![],
            expertise: vec![],
            communication_style: None,
            personality: None,
        };
        save_role(data_dir, &role).unwrap();
        assert!(load_role(data_dir, "doomed").is_ok());

        delete_role(data_dir, "doomed").unwrap();
        assert!(load_role(data_dir, "doomed").is_err());
    }

    #[test]
    fn init_builtin_roles_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();

        let created1 = init_builtin_roles(data_dir).unwrap();
        assert_eq!(created1.len(), 4);

        let created2 = init_builtin_roles(data_dir).unwrap();
        assert_eq!(created2.len(), 0, "Second init should skip existing roles");

        let all = list_roles(data_dir).unwrap();
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn list_roles_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        let roles = list_roles(data_dir).unwrap();
        assert!(roles.is_empty());
    }

    #[test]
    fn load_nonexistent_role_errors() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        assert!(load_role(data_dir, "nope").is_err());
    }

    #[test]
    fn delete_nonexistent_role_errors() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        assert!(delete_role(data_dir, "nope").is_err());
    }
}
