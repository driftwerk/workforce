use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// An atomic, primary-cited knowledge item curated by an agent from a
/// learning plan. The `claim` is also the body of the backing Memory
/// row referenced by `memory_id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeItem {
    pub id: Uuid,
    pub agent_id: String,
    pub plan_id: String,
    pub topic: String,
    pub claim: String,
    pub content_hash: String,
    pub memory_id: Option<String>,
    pub status: ItemStatus,
    pub confidence: f32,
    pub high_stakes: bool,
    pub source: ItemSource,
    pub created_at: DateTime<Utc>,
    pub created_by: String,
    pub verified_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub reviewed_at: Option<DateTime<Utc>>,
    pub reviewed_by: Option<String>,
    pub review_note: Option<String>,
    pub superseded_by: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    Provisional,
    Approved,
    Rejected,
    Stale,
    Superseded,
}

impl ItemStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Provisional => "provisional",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Stale => "stale",
            Self::Superseded => "superseded",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "approved" => Self::Approved,
            "rejected" => Self::Rejected,
            "stale" => Self::Stale,
            "superseded" => Self::Superseded,
            _ => Self::Provisional,
        }
    }
}

/// Provenance of a knowledge item — every item must cite a primary,
/// resolvable source (invariant: a claim with no `reference` is not a
/// fact and is rejected at record time).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ItemSource {
    pub kind: SourceKind,
    pub skill: Option<String>,
    pub reference: String,
    pub excerpt: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Skill,
    Codebase,
    Knowledge,
    /// A lesson distilled from the agent's own operational history, cited to a
    /// `session_id:turn_index` rather than a researchable primary source.
    Experience,
}

impl SourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Skill => "skill",
            Self::Codebase => "codebase",
            Self::Knowledge => "knowledge",
            Self::Experience => "experience",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "codebase" => Self::Codebase,
            "knowledge" => Self::Knowledge,
            "experience" => Self::Experience,
            _ => Self::Skill,
        }
    }
}

/// Filter for [`crate::store::SelfLearningStore::list_items`]. Every
/// field is optional; `None` means "no constraint on this dimension".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemQuery {
    pub plan_id: Option<String>,
    pub topic: Option<String>,
    pub status: Option<ItemStatus>,
    pub limit: Option<usize>,
}

/// Human review metadata recorded on a status transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewMark {
    pub reviewed_by: String,
    pub note: Option<String>,
}

/// Pure-function identity hash used to detect "this claim already
/// exists" on re-curation. Stable across reruns of the same plan.
/// First 16 bytes of SHA-256 over (agent_id, plan_id, claim), `\0`-separated.
pub fn content_hash(agent_id: &str, plan_id: &str, claim: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(agent_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(plan_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(claim.as_bytes());
    let bytes = hasher.finalize();
    hex_encode(&bytes[..16])
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// A node in an agent's deep-expertise knowledge tree. Unlike a flat
/// [`KnowledgeItem`], a node is a synthesized, source-grounded document for one
/// sub-domain; it spawns children for sub-topics surfaced while deepening, and
/// is updated in place on re-curation (identified by [`node_path_hash`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeepNode {
    pub id: Uuid,
    pub agent_id: String,
    pub plan_id: String,
    pub parent_id: Option<Uuid>,
    pub depth: u32,
    pub title: String,
    pub path_hash: String,
    pub body: String,
    pub sources: Vec<ItemSource>,
    pub coverage: Vec<SurfaceItem>,
    pub memory_id: Option<String>,
    pub status: NodeStatus,
    pub confidence: f32,
    pub created_at: DateTime<Utc>,
    pub created_by: String,
    pub verified_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub reviewed_at: Option<DateTime<Utc>>,
    pub reviewed_by: Option<String>,
    pub review_note: Option<String>,
    pub superseded_by: Option<Uuid>,
}

/// One element of a node's enumerated surface (a symbol, file, or edge the node
/// must cover) and whether the synthesized body covers it. Uncovered elements
/// are the gaps the deepening loop turns into child nodes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SurfaceItem {
    pub reference: String,
    pub covered: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Seeded,
    Modelled,
    Verified,
    Approved,
    Rejected,
    Stale,
    Superseded,
}

impl NodeStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Seeded => "seeded",
            Self::Modelled => "modelled",
            Self::Verified => "verified",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Stale => "stale",
            Self::Superseded => "superseded",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "modelled" => Self::Modelled,
            "verified" => Self::Verified,
            "approved" => Self::Approved,
            "rejected" => Self::Rejected,
            "stale" => Self::Stale,
            "superseded" => Self::Superseded,
            _ => Self::Seeded,
        }
    }
}

/// Tree-position constraint for [`NodeQuery`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum NodeParent {
    #[default]
    Any,
    Roots,
    Of(Uuid),
}

/// Filter for [`crate::store::DeepExpertiseStore::list_nodes`]. `None`/`Any`
/// means "no constraint on this dimension".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeQuery {
    pub plan_id: Option<String>,
    pub parent: NodeParent,
    pub status: Option<NodeStatus>,
    pub limit: Option<usize>,
}

/// Stable identity of a node by its position in the tree, so re-curating the
/// same sub-domain updates the node rather than appending a duplicate. First 16
/// bytes of SHA-256 over (agent_id, plan_id, parent_id, title).
pub fn node_path_hash(
    agent_id: &str,
    plan_id: &str,
    parent_id: Option<Uuid>,
    title: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(agent_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(plan_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(
        parent_id
            .map(|p| p.to_string())
            .unwrap_or_default()
            .as_bytes(),
    );
    hasher.update(b"\0");
    hasher.update(title.as_bytes());
    let bytes = hasher.finalize();
    hex_encode(&bytes[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_is_deterministic() {
        let a = content_hash("agent", "plan", "billing is monthly");
        let b = content_hash("agent", "plan", "billing is monthly");
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn content_hash_differs_on_claim() {
        assert_ne!(
            content_hash("agent", "plan", "billing is monthly"),
            content_hash("agent", "plan", "billing is weekly"),
        );
    }

    #[test]
    fn content_hash_differs_per_agent_and_plan() {
        assert_ne!(
            content_hash("a1", "plan", "x"),
            content_hash("a2", "plan", "x"),
        );
        assert_ne!(
            content_hash("agent", "p1", "x"),
            content_hash("agent", "p2", "x"),
        );
    }

    #[test]
    fn status_round_trip() {
        for s in [
            ItemStatus::Provisional,
            ItemStatus::Approved,
            ItemStatus::Rejected,
            ItemStatus::Stale,
            ItemStatus::Superseded,
        ] {
            assert_eq!(ItemStatus::parse(s.as_str()), s);
        }
    }

    #[test]
    fn source_kind_round_trip() {
        for k in [
            SourceKind::Skill,
            SourceKind::Codebase,
            SourceKind::Knowledge,
            SourceKind::Experience,
        ] {
            assert_eq!(SourceKind::parse(k.as_str()), k);
        }
    }

    #[test]
    fn node_status_round_trip() {
        for s in [
            NodeStatus::Seeded,
            NodeStatus::Modelled,
            NodeStatus::Verified,
            NodeStatus::Approved,
            NodeStatus::Rejected,
            NodeStatus::Stale,
            NodeStatus::Superseded,
        ] {
            assert_eq!(NodeStatus::parse(s.as_str()), s);
        }
    }

    #[test]
    fn node_path_hash_identifies_tree_position() {
        let parent = Uuid::new_v4();
        let a = node_path_hash("agent", "plan", Some(parent), "Discord surface");
        assert_eq!(
            a,
            node_path_hash("agent", "plan", Some(parent), "Discord surface")
        );
        assert_eq!(a.len(), 32);
        assert_ne!(a, node_path_hash("agent", "plan", None, "Discord surface"));
        assert_ne!(
            a,
            node_path_hash("agent", "plan", Some(parent), "Slack surface")
        );
        assert_ne!(
            a,
            node_path_hash("agent", "other", Some(parent), "Discord surface")
        );
    }
}
