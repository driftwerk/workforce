use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A per-agent named bundle of repos.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub agent_id: String,
    pub name: String,
    #[serde(default)]
    pub defaults: ProjectDefaults,
    #[serde(default)]
    pub repos: Vec<ProjectRepo>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Project {
    pub fn new(agent_id: impl Into<String>, name: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            agent_id: agent_id.into(),
            name: name.into(),
            defaults: ProjectDefaults::default(),
            repos: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    pub fn repo(&self, alias: &str) -> Option<&ProjectRepo> {
        self.repos.iter().find(|r| r.alias == alias)
    }

    /// Per-repo override wins, then project default, then `"main"`.
    pub fn effective_default_branch(&self, alias: &str) -> &str {
        self.repo(alias)
            .and_then(|r| r.default_branch.as_deref())
            .or(self.defaults.default_branch.as_deref())
            .unwrap_or("main")
    }

    /// The shell command that readies a fresh worktree of `alias` (dependency
    /// install, codegen): per-repo override wins, then the project default.
    pub fn effective_setup(&self, alias: &str) -> Option<&str> {
        self.repo(alias)
            .and_then(|r| r.setup.as_deref())
            .or(self.defaults.setup.as_deref())
    }
}

/// Project-wide defaults; each field is overridable per-repo.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<String>,
}

/// `alias` is the project-local short name; `source` is the upstream the hub
/// clones from. `default_branch` and `setup` override the project defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRepo {
    pub alias: String,
    pub source: RepoSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<String>,
}

/// Tagged so v1's GitHub-only shape can grow into git/ssh/path without a
/// breaking schema change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum RepoSource {
    Github { owner: String, repo: String },
}

impl RepoSource {
    /// GitHub owner/user logins are case-insensitive; the canonical form is
    /// lowercase. Repo identity and on-disk layout key on this so a case-variant
    /// checkout (`Owner` vs `owner`) maps to one repo, not two.
    pub fn canonical_owner(&self) -> String {
        match self {
            RepoSource::Github { owner, .. } => owner.to_ascii_lowercase(),
        }
    }

    pub fn canonical_slug(&self) -> String {
        match self {
            RepoSource::Github { repo, .. } => format!("{}/{}", self.canonical_owner(), repo),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_repo() -> ProjectRepo {
        ProjectRepo {
            alias: "rails".to_string(),
            source: RepoSource::Github {
                owner: "harborlabs".to_string(),
                repo: "HarborRails".to_string(),
            },
            default_branch: None,
            setup: None,
        }
    }

    #[test]
    fn effective_setup_prefers_the_repo_override_then_the_project_default() {
        let mut project = Project::new("allan", "harbor");
        project.repos.push(sample_repo());
        let alias = project.repos[0].alias.clone();
        assert_eq!(project.effective_setup(&alias), None);

        project.defaults.setup = Some("pnpm install --frozen-lockfile".into());
        assert_eq!(
            project.effective_setup(&alias),
            Some("pnpm install --frozen-lockfile")
        );

        project.repos[0].setup = Some("bundle install".into());
        assert_eq!(project.effective_setup(&alias), Some("bundle install"));
        assert_eq!(
            project.effective_setup("unknown"),
            Some("pnpm install --frozen-lockfile")
        );
    }

    #[test]
    fn a_repo_without_setup_deserialises_from_the_pre_setup_shape() {
        let repo: ProjectRepo = serde_json::from_str(
            r#"{"alias":"rails","source":{"kind":"github","owner":"acme","repo":"rails"}}"#,
        )
        .unwrap();
        assert_eq!(repo.setup, None);
        assert!(!serde_json::to_string(&repo).unwrap().contains("setup"));
    }

    #[test]
    fn repo_source_serialises_with_kind_tag() {
        let src = RepoSource::Github {
            owner: "acme".to_string(),
            repo: "widgets".to_string(),
        };
        let v = serde_json::to_value(&src).unwrap();
        assert_eq!(
            v,
            json!({ "kind": "github", "owner": "acme", "repo": "widgets" })
        );
    }

    #[test]
    fn repo_source_round_trips() {
        let src = RepoSource::Github {
            owner: "acme".to_string(),
            repo: "widgets".to_string(),
        };
        let s = serde_json::to_string(&src).unwrap();
        let back: RepoSource = serde_json::from_str(&s).unwrap();
        assert_eq!(src, back);
    }

    #[test]
    fn repo_source_canonical_slug_is_owner_slash_repo() {
        let src = RepoSource::Github {
            owner: "acme".to_string(),
            repo: "widgets".to_string(),
        };
        assert_eq!(src.canonical_slug(), "acme/widgets");
    }

    #[test]
    fn canonical_owner_folds_case_and_preserves_repo() {
        let upper = RepoSource::Github {
            owner: "Harborlabs".to_string(),
            repo: "HarborApps".to_string(),
        };
        assert_eq!(upper.canonical_owner(), "harborlabs");
        assert_eq!(upper.canonical_slug(), "harborlabs/HarborApps");
    }

    #[test]
    fn canonical_slug_folds_owner_case_only() {
        let lower = RepoSource::Github {
            owner: "harborlabs".to_string(),
            repo: "HarborApps".to_string(),
        };
        let upper = RepoSource::Github {
            owner: "Harborlabs".to_string(),
            repo: "HarborApps".to_string(),
        };
        assert_eq!(upper.canonical_slug(), lower.canonical_slug());
    }

    #[test]
    fn project_round_trips_through_json() {
        let mut p = Project::new("allan", "harbor");
        p.repos.push(sample_repo());
        let s = serde_json::to_string(&p).unwrap();
        let back: Project = serde_json::from_str(&s).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn project_defaults_omit_when_none() {
        let p = Project::new("allan", "harbor");
        let s = serde_json::to_string(&p).unwrap();
        assert!(!s.contains("default_branch"));
    }

    #[test]
    fn repo_lookup_returns_match_or_none() {
        let mut p = Project::new("allan", "harbor");
        p.repos.push(sample_repo());
        assert!(p.repo("rails").is_some());
        assert!(p.repo("missing").is_none());
    }

    #[test]
    fn effective_default_branch_prefers_repo_override() {
        let mut p = Project::new("allan", "harbor");
        p.defaults.default_branch = Some("main".to_string());
        p.repos.push(ProjectRepo {
            alias: "core-api".to_string(),
            source: RepoSource::Github {
                owner: "harborlabs".to_string(),
                repo: "core-api".to_string(),
            },
            default_branch: Some("develop".to_string()),
            setup: None,
        });
        assert_eq!(p.effective_default_branch("core-api"), "develop");
    }

    #[test]
    fn effective_default_branch_falls_back_to_project_default() {
        let mut p = Project::new("allan", "harbor");
        p.defaults.default_branch = Some("trunk".to_string());
        p.repos.push(sample_repo());
        assert_eq!(p.effective_default_branch("rails"), "trunk");
    }

    #[test]
    fn effective_default_branch_falls_back_to_main_when_unset() {
        let mut p = Project::new("allan", "harbor");
        p.repos.push(sample_repo());
        assert_eq!(p.effective_default_branch("rails"), "main");
    }

    #[test]
    fn unknown_repo_returns_main() {
        let p = Project::new("allan", "harbor");
        assert_eq!(p.effective_default_branch("missing"), "main");
    }
}
