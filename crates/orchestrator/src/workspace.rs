//! Shared workspace for collaborative artifacts and decision tracking.
//!
//! Each team session gets a workspace directory containing:
//! - `artifacts/<id>.md` — YAML frontmatter + markdown body
//! - `decisions.jsonl` — append-only decision log

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};
use workforce_core::types::AgentId;

/// A shared workspace directory for team artifacts and decisions.
///
/// Deprecated by Artifacts v2 (DW-546): new code should use
/// [`crate::artifact_store::ArtifactBackend`] (Postgres `KnowledgeStore` or
/// the file fallback). Retained because the disk→DB migration (DW-548)
/// reads legacy `<id>.md` files through it, and decisions still live here.
pub struct SharedWorkspace {
    base_dir: PathBuf,
}

/// A collaborative artifact stored as YAML frontmatter + markdown body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub id: String,
    pub artifact_type: ArtifactType,
    pub title: String,
    pub content: String,
    pub created_by: AgentId,
    pub last_modified_by: AgentId,
    pub status: ArtifactStatus,
    pub version: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The kind of artifact.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactType {
    Spec,
    Ticket,
    Architecture,
    Notes,
    Decision,
    /// A reusable code/text snippet (Artifacts v2, DW-552).
    Snippet,
}

impl std::fmt::Display for ArtifactType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spec => write!(f, "spec"),
            Self::Ticket => write!(f, "ticket"),
            Self::Architecture => write!(f, "architecture"),
            Self::Notes => write!(f, "notes"),
            Self::Decision => write!(f, "decision"),
            Self::Snippet => write!(f, "snippet"),
        }
    }
}

impl std::str::FromStr for ArtifactType {
    type Err = std::convert::Infallible;

    /// Parse a type string, defaulting unknown values to `Notes` so the
    /// agent tool never hard-fails on an unexpected `artifact_type`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "spec" => Self::Spec,
            "ticket" => Self::Ticket,
            "architecture" => Self::Architecture,
            "decision" => Self::Decision,
            "snippet" => Self::Snippet,
            _ => Self::Notes,
        })
    }
}

/// Whether the artifact is still being worked on or is finalised.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactStatus {
    Draft,
    Finalized,
}

impl std::fmt::Display for ArtifactStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Draft => write!(f, "draft"),
            Self::Finalized => write!(f, "finalized"),
        }
    }
}

/// A recorded team decision with optional dissent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub id: String,
    pub summary: String,
    pub rationale: String,
    pub proposed_by: AgentId,
    pub approved_by: Option<AgentId>,
    pub dissent: Vec<Dissent>,
    pub created_at: DateTime<Utc>,
}

/// A dissenting opinion on a decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dissent {
    pub agent_id: AgentId,
    pub position: String,
}

impl SharedWorkspace {
    /// Create (or open) a shared workspace at the given directory.
    pub fn create(base_dir: &Path) -> Result<Self> {
        let artifacts_dir = base_dir.join("artifacts");
        fs::create_dir_all(&artifacts_dir).wrap_err_with(|| {
            format!(
                "Failed to create artifacts dir: {}",
                artifacts_dir.display()
            )
        })?;
        Ok(Self {
            base_dir: base_dir.to_path_buf(),
        })
    }

    /// Get the base directory path.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Create a new artifact on disk.
    pub fn create_artifact(&self, artifact: &Artifact) -> Result<()> {
        let path = self.artifact_path(&artifact.id);
        if path.exists() {
            return Err(eyre::eyre!("Artifact '{}' already exists", artifact.id));
        }
        self.write_artifact(&path, artifact)
    }

    /// Read an artifact by ID.
    pub fn read_artifact(&self, id: &str) -> Result<Option<Artifact>> {
        let path = self.artifact_path(id);
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(&path)
            .wrap_err_with(|| format!("Failed to read artifact: {}", path.display()))?;
        let artifact = parse_artifact(&raw)?;
        Ok(Some(artifact))
    }

    /// Update an existing artifact's content.
    pub fn update_artifact(&self, id: &str, content: &str, modified_by: &AgentId) -> Result<()> {
        let path = self.artifact_path(id);
        let raw =
            fs::read_to_string(&path).wrap_err_with(|| format!("Artifact '{}' not found", id))?;
        let mut artifact = parse_artifact(&raw)?;

        if artifact.status == ArtifactStatus::Finalized {
            return Err(eyre::eyre!(
                "Artifact '{}' is finalized and cannot be modified",
                id
            ));
        }

        artifact.content = content.to_string();
        artifact.last_modified_by = modified_by.clone();
        artifact.version += 1;
        artifact.updated_at = Utc::now();

        self.write_artifact(&path, &artifact)
    }

    /// Finalize an artifact (no further modifications allowed).
    pub fn finalize_artifact(&self, id: &str, finalized_by: &AgentId) -> Result<()> {
        let path = self.artifact_path(id);
        let raw =
            fs::read_to_string(&path).wrap_err_with(|| format!("Artifact '{}' not found", id))?;
        let mut artifact = parse_artifact(&raw)?;

        artifact.status = ArtifactStatus::Finalized;
        artifact.last_modified_by = finalized_by.clone();
        artifact.updated_at = Utc::now();

        self.write_artifact(&path, &artifact)
    }

    /// List all artifacts in the workspace.
    pub fn list_artifacts(&self) -> Result<Vec<Artifact>> {
        let dir = self.base_dir.join("artifacts");
        if !dir.exists() {
            return Ok(vec![]);
        }

        let mut artifacts = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "md") {
                let raw = fs::read_to_string(&path)?;
                if let Ok(artifact) = parse_artifact(&raw) {
                    artifacts.push(artifact);
                }
            }
        }

        artifacts.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(artifacts)
    }

    /// Record a decision to the decision log.
    pub fn record_decision(&self, decision: &Decision) -> Result<()> {
        let path = self.base_dir.join("decisions.jsonl");
        let line = serde_json::to_string(decision)?;
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "{line}")?;
        Ok(())
    }

    /// List all recorded decisions.
    pub fn list_decisions(&self) -> Result<Vec<Decision>> {
        let path = self.base_dir.join("decisions.jsonl");
        if !path.exists() {
            return Ok(vec![]);
        }

        let raw = fs::read_to_string(&path)?;
        let mut decisions = Vec::new();
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let decision: Decision = serde_json::from_str(line)?;
            decisions.push(decision);
        }
        Ok(decisions)
    }

    // ── Private helpers ──

    fn artifact_path(&self, id: &str) -> PathBuf {
        self.base_dir.join("artifacts").join(format!("{id}.md"))
    }

    fn write_artifact(&self, path: &Path, artifact: &Artifact) -> Result<()> {
        let frontmatter = serde_yaml::to_string(artifact)?;
        let content = format!("---\n{frontmatter}---\n{}", artifact.content);
        fs::write(path, content)?;
        Ok(())
    }
}

/// Parse YAML frontmatter + markdown body into an Artifact.
fn parse_artifact(raw: &str) -> Result<Artifact> {
    // Format: ---\n<yaml>\n---\n<body>
    let stripped = raw
        .strip_prefix("---\n")
        .ok_or_else(|| eyre::eyre!("Missing YAML frontmatter delimiter"))?;

    let end = stripped
        .find("---\n")
        .ok_or_else(|| eyre::eyre!("Missing closing frontmatter delimiter"))?;

    let yaml_str = &stripped[..end];
    let body = &stripped[end + 4..]; // skip "---\n"

    let mut artifact: Artifact = serde_yaml::from_str(yaml_str)?;
    // The body in the file is the authoritative content
    artifact.content = body.to_string();
    Ok(artifact)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_workspace() -> (TempDir, SharedWorkspace) {
        let tmp = TempDir::new().unwrap();
        let ws = SharedWorkspace::create(tmp.path()).unwrap();
        (tmp, ws)
    }

    fn sample_artifact(id: &str) -> Artifact {
        Artifact {
            id: id.to_string(),
            artifact_type: ArtifactType::Spec,
            title: "Sample Spec".into(),
            content: "# Overview\n\nThis is the spec content.".into(),
            created_by: AgentId::new("pm"),
            last_modified_by: AgentId::new("pm"),
            status: ArtifactStatus::Draft,
            version: 1,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn create_and_read_artifact() {
        let (_tmp, ws) = make_workspace();
        let artifact = sample_artifact("spec-001");
        ws.create_artifact(&artifact).unwrap();

        let loaded = ws.read_artifact("spec-001").unwrap().unwrap();
        assert_eq!(loaded.id, "spec-001");
        assert_eq!(loaded.title, "Sample Spec");
        assert!(loaded.content.contains("# Overview"));
        assert_eq!(loaded.status, ArtifactStatus::Draft);
        assert_eq!(loaded.version, 1);
    }

    #[test]
    fn read_nonexistent_returns_none() {
        let (_tmp, ws) = make_workspace();
        assert!(ws.read_artifact("does-not-exist").unwrap().is_none());
    }

    #[test]
    fn create_duplicate_fails() {
        let (_tmp, ws) = make_workspace();
        let artifact = sample_artifact("dup");
        ws.create_artifact(&artifact).unwrap();
        assert!(ws.create_artifact(&artifact).is_err());
    }

    #[test]
    fn update_artifact_content() {
        let (_tmp, ws) = make_workspace();
        ws.create_artifact(&sample_artifact("spec-002")).unwrap();

        let modifier = AgentId::new("dev-1");
        ws.update_artifact("spec-002", "# Updated\n\nNew content.", &modifier)
            .unwrap();

        let loaded = ws.read_artifact("spec-002").unwrap().unwrap();
        assert_eq!(loaded.content, "# Updated\n\nNew content.");
        assert_eq!(loaded.last_modified_by, modifier);
        assert_eq!(loaded.version, 2);
    }

    #[test]
    fn update_finalized_fails() {
        let (_tmp, ws) = make_workspace();
        ws.create_artifact(&sample_artifact("spec-003")).unwrap();
        ws.finalize_artifact("spec-003", &AgentId::new("lead"))
            .unwrap();

        let result = ws.update_artifact("spec-003", "new content", &AgentId::new("dev"));
        assert!(result.is_err());
    }

    #[test]
    fn finalize_artifact() {
        let (_tmp, ws) = make_workspace();
        ws.create_artifact(&sample_artifact("spec-004")).unwrap();

        ws.finalize_artifact("spec-004", &AgentId::new("lead"))
            .unwrap();

        let loaded = ws.read_artifact("spec-004").unwrap().unwrap();
        assert_eq!(loaded.status, ArtifactStatus::Finalized);
    }

    #[test]
    fn list_artifacts() {
        let (_tmp, ws) = make_workspace();
        ws.create_artifact(&sample_artifact("a")).unwrap();
        ws.create_artifact(&sample_artifact("b")).unwrap();
        ws.create_artifact(&sample_artifact("c")).unwrap();

        let list = ws.list_artifacts().unwrap();
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn list_empty_workspace() {
        let (_tmp, ws) = make_workspace();
        assert!(ws.list_artifacts().unwrap().is_empty());
    }

    #[test]
    fn record_and_list_decisions() {
        let (_tmp, ws) = make_workspace();

        let d1 = Decision {
            id: "dec-001".into(),
            summary: "Use SQLite for storage".into(),
            rationale: "Simple, embedded, no external deps".into(),
            proposed_by: AgentId::new("architect"),
            approved_by: Some(AgentId::new("tech-lead")),
            dissent: vec![],
            created_at: Utc::now(),
        };
        let d2 = Decision {
            id: "dec-002".into(),
            summary: "Adopt async-first approach".into(),
            rationale: "Better throughput for I/O-bound workloads".into(),
            proposed_by: AgentId::new("tech-lead"),
            approved_by: None,
            dissent: vec![Dissent {
                agent_id: AgentId::new("dev-1"),
                position: "Adds complexity, sync is simpler for our use case".into(),
            }],
            created_at: Utc::now(),
        };

        ws.record_decision(&d1).unwrap();
        ws.record_decision(&d2).unwrap();

        let decisions = ws.list_decisions().unwrap();
        assert_eq!(decisions.len(), 2);
        assert_eq!(decisions[0].id, "dec-001");
        assert_eq!(decisions[1].dissent.len(), 1);
        assert_eq!(decisions[1].dissent[0].agent_id, AgentId::new("dev-1"));
    }

    #[test]
    fn list_decisions_empty() {
        let (_tmp, ws) = make_workspace();
        assert!(ws.list_decisions().unwrap().is_empty());
    }

    #[test]
    fn artifact_type_display() {
        assert_eq!(ArtifactType::Spec.to_string(), "spec");
        assert_eq!(ArtifactType::Architecture.to_string(), "architecture");
    }

    #[test]
    fn artifact_status_display() {
        assert_eq!(ArtifactStatus::Draft.to_string(), "draft");
        assert_eq!(ArtifactStatus::Finalized.to_string(), "finalized");
    }
}
