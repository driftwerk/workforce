use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const BUNDLE_VERSION: u32 = 1;
pub const MANIFEST_FILE: &str = "manifest.json";
pub const IDENTITIES_FILE: &str = "identities.ndjson";

/// Per-agent memory page size. Caps a single `MemoryStore::list` — without it
/// the Weaviate backend defaults to 10, silently truncating the export. Sized
/// to Weaviate's default `QUERY_MAXIMUM_RESULTS` so a single query stays valid.
pub const MEMORY_QUERY_LIMIT: usize = 10_000;

pub fn agent_config_entry(id: &str) -> String {
    format!("agents/{id}/agent.json")
}

pub fn agent_workspace_entry(id: &str) -> String {
    format!("agents/{id}/workspace.toml")
}

pub fn agent_identity_entry(id: &str, file: &str) -> String {
    format!("agents/{id}/identity/{file}")
}

pub fn agent_sessions_prefix(id: &str) -> String {
    format!("agents/{id}/sessions/")
}

pub fn agent_memory_entry(id: &str) -> String {
    format!("agents/{id}/memory.ndjson")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaVersions {
    pub agent: u32,
    pub session: u32,
    pub memory: u32,
    pub identity: u32,
}

impl Default for SchemaVersions {
    fn default() -> Self {
        Self {
            agent: 1,
            session: 1,
            memory: 1,
            identity: 1,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub agents: usize,
    pub sessions: usize,
    pub memories: usize,
    pub identities: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub bundle_version: u32,
    pub source_install_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_slack_team_id: Option<String>,
    pub exported_at: DateTime<Utc>,
    pub agents: Vec<String>,
    #[serde(default)]
    pub schema_versions: SchemaVersions,
    #[serde(default)]
    pub counts: Counts,
    #[serde(default)]
    pub checksums: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportOptions {
    /// Source agent id → target agent id. Overrides the default collision rename.
    #[serde(default)]
    pub renames: BTreeMap<String, String>,
    /// Source identity uuid → target identity uuid. Overrides email reconcile.
    #[serde(default)]
    pub identity_overrides: BTreeMap<String, String>,
    /// Must be set for `apply_import` to write. `plan_import` ignores it.
    #[serde(default)]
    pub confirm: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IdentityResolutionKind {
    /// Matched an existing target identity (by override, email, or Slack id).
    Matched { target_id: String },
    /// No match — a new identity will be created with this id.
    Create { target_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityResolution {
    pub source_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub resolution: IdentityResolutionKind,
}

impl IdentityResolution {
    pub fn target_id(&self) -> &str {
        match &self.resolution {
            IdentityResolutionKind::Matched { target_id }
            | IdentityResolutionKind::Create { target_id } => target_id,
        }
    }

    pub fn is_create(&self) -> bool {
        matches!(self.resolution, IdentityResolutionKind::Create { .. })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentPlan {
    pub source_id: String,
    pub target_id: String,
    pub collides: bool,
    pub sessions: usize,
    pub memories: usize,
    /// Advisory: credentials the operator must provision (vault is not transferred).
    pub expected_credentials: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergePlan {
    pub source_install_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_slack_team_id: Option<String>,
    pub agents: Vec<AgentPlan>,
    pub identities: Vec<IdentityResolution>,
    pub counts: Counts,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportOutcome {
    pub agents_imported: Vec<String>,
    pub renames_applied: BTreeMap<String, String>,
    pub identities_created: usize,
    pub identities_matched: usize,
    pub sessions_imported: usize,
    pub memories_imported: usize,
    pub memories_skipped: usize,
    pub notes: Vec<String>,
}

/// The id maps every rewrite derives from. A lookup falls back to the original
/// id, so platform-scoped values (e.g. a Slack channel owner, an unbundled
/// agent) pass through untouched. Session ids are UUIDs imported skip-if-exists,
/// so they need no map — their `source_session_id` references stay verbatim.
#[derive(Debug, Clone, Default)]
pub struct RemapPlan {
    pub agents: HashMap<String, String>,
    pub identities: HashMap<String, String>,
}

impl RemapPlan {
    pub fn agent(&self, id: &str) -> String {
        self.agents
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_string())
    }

    pub fn identity(&self, id: &str) -> String {
        self.identities
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_string())
    }
}
