//! Team registry — loads, queries, and manages team definitions at runtime.

use parking_lot::RwLock;
use std::collections::HashMap;

use eyre::Result;
use tracing::info;
use workforce_common::settings::{MemberConfig, TeamConfig, WorkforceConfig};

/// Registry of team definitions, loaded from config and queryable at runtime.
pub struct TeamRegistry {
    teams: RwLock<HashMap<String, TeamConfig>>,
}

impl TeamRegistry {
    /// Load all teams from the workforce config.
    pub fn from_config(config: &WorkforceConfig) -> Self {
        let mut teams = HashMap::new();
        for team in &config.teams {
            teams.insert(team.id.clone(), team.clone());
        }
        info!(count = teams.len(), "Team registry loaded");
        Self {
            teams: RwLock::new(teams),
        }
    }

    /// Get a team by ID.
    pub fn get(&self, id: &str) -> Option<TeamConfig> {
        self.teams.read().get(id).cloned()
    }

    /// List all registered teams.
    pub fn list(&self) -> Vec<TeamConfig> {
        self.teams.read().values().cloned().collect()
    }

    /// Number of registered teams.
    pub fn len(&self) -> usize {
        self.teams.read().len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.teams.read().is_empty()
    }

    /// Hot-reload teams from a new config (replaces all entries).
    pub fn reload(&self, config: &WorkforceConfig) {
        let mut teams = self.teams.write();
        teams.clear();
        for team in &config.teams {
            teams.insert(team.id.clone(), team.clone());
        }
        info!(count = teams.len(), "Team registry reloaded");
    }

    /// Insert a team into the registry (in-memory only).
    pub fn insert(&self, team: TeamConfig) -> Result<()> {
        let mut teams = self.teams.write();
        if teams.contains_key(&team.id) {
            return Err(eyre::eyre!("Team '{}' already exists", team.id));
        }
        teams.insert(team.id.clone(), team);
        Ok(())
    }

    /// Update a team in the registry (in-memory only).
    pub fn update(
        &self,
        team_id: &str,
        name: Option<String>,
        lead: Option<String>,
        pattern: Option<String>,
    ) -> Result<TeamConfig> {
        let mut teams = self.teams.write();
        let team = teams
            .get_mut(team_id)
            .ok_or_else(|| eyre::eyre!("Team '{}' not found", team_id))?;
        if let Some(n) = name {
            team.name = n;
        }
        if let Some(l) = lead {
            team.lead = l;
        }
        if let Some(p) = pattern {
            workforce_common::settings::validate_team_pattern(&p)?;
            team.pattern = p;
        }
        Ok(team.clone())
    }

    /// Remove a team from the registry (in-memory only).
    pub fn remove(&self, team_id: &str) -> Result<()> {
        let mut teams = self.teams.write();
        teams
            .remove(team_id)
            .ok_or_else(|| eyre::eyre!("Team '{}' not found", team_id))?;
        Ok(())
    }

    /// Add a member to a team (in-memory only).
    pub fn add_member(&self, team_id: &str, member: MemberConfig) -> Result<()> {
        let mut teams = self.teams.write();
        let team = teams
            .get_mut(team_id)
            .ok_or_else(|| eyre::eyre!("Team '{}' not found", team_id))?;
        if team.members.iter().any(|m| m.id == member.id) {
            return Err(eyre::eyre!(
                "Member '{}' already exists in team '{}'",
                member.id,
                team_id
            ));
        }
        team.members.push(member);
        Ok(())
    }

    /// Remove a member from a team (in-memory only).
    pub fn remove_member(&self, team_id: &str, member_id: &str) -> Result<()> {
        let mut teams = self.teams.write();
        let team = teams
            .get_mut(team_id)
            .ok_or_else(|| eyre::eyre!("Team '{}' not found", team_id))?;
        let before = team.members.len();
        team.members.retain(|m| m.id != member_id);
        if team.members.len() == before {
            return Err(eyre::eyre!(
                "Member '{}' not found in team '{}'",
                member_id,
                team_id
            ));
        }
        Ok(())
    }

    /// Create a team config from a built-in template.
    ///
    /// Templates: `"product"`, `"dev"`, `"review"`.
    pub fn create_from_template(
        template: &str,
        team_id: &str,
        team_name: &str,
    ) -> Result<TeamConfig> {
        let templates = workforce_common::settings::team_templates();
        let (_key, description, members) =
            templates
                .into_iter()
                .find(|(k, _, _)| *k == template)
                .ok_or_else(|| eyre::eyre!("Unknown template: {}", template))?;

        let lead = members.first().map(|m| m.id.clone()).unwrap_or_default();

        Ok(TeamConfig {
            id: team_id.to_string(),
            name: team_name.to_string(),
            description: Some(description.to_string()),
            team_type: Some(template.to_string()),
            lead,
            members,
            pattern: "hierarchical".to_string(),
            require_review: template == "review",
            shared_workspace: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::settings::MemberConfig;

    fn sample_config() -> WorkforceConfig {
        let mut config = WorkforceConfig::default();
        config.teams = vec![
            TeamConfig {
                id: "backend".into(),
                name: "Backend Team".into(),
                description: Some("Core API development".into()),
                team_type: Some("engineering".into()),
                lead: "tech-lead".into(),
                members: vec![
                    MemberConfig {
                        id: "tech-lead".into(),
                        role: "tech_lead".into(),
                        model: None,
                        system_prompt: None,
                        capabilities: vec!["code-review".into()],
                        expertise: vec!["rust".into(), "api".into()],
                        communication_style: Some("concise".into()),
                        priority_weight: Some(0.9),
                        personality: None,
                        workspace: None,
                    },
                    MemberConfig {
                        id: "dev-1".into(),
                        role: "senior_dev".into(),
                        model: None,
                        system_prompt: None,
                        capabilities: vec!["code-write".into()],
                        expertise: vec!["rust".into(), "databases".into()],
                        communication_style: Some("thorough".into()),
                        priority_weight: Some(0.8),
                        personality: None,
                        workspace: None,
                    },
                ],
                pattern: "hierarchical".into(),
                require_review: true,
                shared_workspace: None,
            },
            TeamConfig {
                id: "review".into(),
                name: "Review Team".into(),
                description: None,
                team_type: None,
                lead: "reviewer".into(),
                members: vec![MemberConfig {
                    id: "reviewer".into(),
                    role: "reviewer".into(),
                    model: None,
                    system_prompt: None,
                    capabilities: vec!["code-review".into()],
                    expertise: vec![],
                    communication_style: None,
                    priority_weight: None,
                    personality: None,
                    workspace: None,
                }],
                pattern: String::new(),
                require_review: false,
                shared_workspace: None,
            },
        ];
        config
    }

    #[test]
    fn load_from_config() {
        let config = sample_config();
        let registry = TeamRegistry::from_config(&config);
        assert_eq!(registry.len(), 2);
        assert!(!registry.is_empty());
    }

    #[test]
    fn get_by_id() {
        let registry = TeamRegistry::from_config(&sample_config());
        let team = registry.get("backend").unwrap();
        assert_eq!(team.name, "Backend Team");
        assert_eq!(team.lead, "tech-lead");
        assert_eq!(team.members.len(), 2);
        assert_eq!(team.members[0].expertise, vec!["rust", "api"]);
    }

    #[test]
    fn get_nonexistent() {
        let registry = TeamRegistry::from_config(&sample_config());
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn list_all() {
        let registry = TeamRegistry::from_config(&sample_config());
        let teams = registry.list();
        assert_eq!(teams.len(), 2);
    }

    #[test]
    fn reload() {
        let registry = TeamRegistry::from_config(&sample_config());
        assert_eq!(registry.len(), 2);

        let empty = WorkforceConfig::default();
        registry.reload(&empty);
        assert_eq!(registry.len(), 0);

        registry.reload(&sample_config());
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn create_from_product_template() {
        let team =
            TeamRegistry::create_from_template("product", "my-product", "My Product Team").unwrap();
        assert_eq!(team.id, "my-product");
        assert_eq!(team.name, "My Product Team");
        assert_eq!(team.team_type.as_deref(), Some("product"));
        assert_eq!(team.lead, "pm");
        assert_eq!(team.members.len(), 3);
        assert!(!team.require_review);
    }

    #[test]
    fn create_from_dev_template() {
        let team = TeamRegistry::create_from_template("dev", "core-dev", "Core Dev").unwrap();
        assert_eq!(team.lead, "tech-lead");
        assert_eq!(team.members.len(), 4);
    }

    #[test]
    fn create_from_review_template() {
        let team =
            TeamRegistry::create_from_template("review", "sec-review", "Security Review").unwrap();
        assert_eq!(team.lead, "architect");
        assert_eq!(team.members.len(), 3);
        assert!(team.require_review);
    }

    #[test]
    fn create_from_unknown_template_fails() {
        assert!(TeamRegistry::create_from_template("unknown", "x", "x").is_err());
    }
}
