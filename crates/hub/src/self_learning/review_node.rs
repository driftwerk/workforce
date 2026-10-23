//! Human review transitions for deep-expertise tree nodes: approve / reject /
//! delete-subtree. Factored out of the REST handlers so the agent-scoping and
//! status ordering are unit-testable against an in-memory store, mirroring
//! [`super::review`] for atomic items.
//!
//! Unlike atomic items, a node's status transition is a pure store update: the
//! recall surface ([`super::surface::render_deep_expertise`]) joins live nodes
//! to recall hits and retires hits whose node is gone, so a delete removes a
//! node from recall without a Memory forget. Delete cascades the whole subtree.

use uuid::Uuid;

use workforce_self_learning::{DeepExpertiseStore, DeepNode, NodeStatus, ReviewMark};

use super::review::ReviewError;

type ReviewResult<T> = Result<T, ReviewError>;

fn store_err(e: impl Into<eyre::Report>) -> ReviewError {
    ReviewError::Store(e.into())
}

/// Fetch a node, enforcing it belongs to `agent_id` — the URL scope is the auth
/// scope, so review can never reach across agents (a cross-agent node is
/// reported as `NotFound`, not "belongs to someone else").
fn owned_node(
    store: &dyn DeepExpertiseStore,
    agent_id: &str,
    node_id: Uuid,
) -> ReviewResult<DeepNode> {
    store
        .get_node(node_id)
        .map_err(store_err)?
        .filter(|node| node.agent_id == agent_id)
        .ok_or(ReviewError::NotFound)
}

/// The node plus its direct children, both agent-scoped, for the detail view.
pub fn node_with_children(
    store: &dyn DeepExpertiseStore,
    agent_id: &str,
    node_id: Uuid,
) -> ReviewResult<(DeepNode, Vec<DeepNode>)> {
    let node = owned_node(store, agent_id, node_id)?;
    let children = store
        .list_nodes(
            agent_id,
            &workforce_self_learning::NodeQuery {
                parent: workforce_self_learning::NodeParent::Of(node_id),
                ..Default::default()
            },
        )
        .map_err(store_err)?;
    Ok((node, children))
}

/// Apply a review status transition (`Approved` / `Rejected`), recording the
/// reviewer and an optional note.
pub fn set_status(
    store: &dyn DeepExpertiseStore,
    agent_id: &str,
    node_id: Uuid,
    status: NodeStatus,
    reviewed_by: &str,
    note: Option<String>,
) -> ReviewResult<DeepNode> {
    owned_node(store, agent_id, node_id)?;
    store
        .set_status(
            node_id,
            status,
            Some(&ReviewMark {
                reviewed_by: reviewed_by.to_string(),
                note,
            }),
        )
        .map_err(store_err)?;
    owned_node(store, agent_id, node_id)
}

/// Delete the node and its whole subtree. Returns the number of nodes removed.
pub fn delete_subtree(
    store: &dyn DeepExpertiseStore,
    agent_id: &str,
    node_id: Uuid,
) -> ReviewResult<u64> {
    owned_node(store, agent_id, node_id)?;
    store.delete_subtree(node_id).map_err(store_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use workforce_self_learning::sqlite::SqliteSelfLearningStore;
    use workforce_self_learning::{
        node_path_hash, ItemSource, NodeParent, NodeQuery, SourceKind, SurfaceItem,
    };

    fn node(agent: &str, parent: Option<Uuid>, title: &str) -> DeepNode {
        let now = Utc::now();
        DeepNode {
            id: Uuid::new_v4(),
            agent_id: agent.into(),
            plan_id: "p".into(),
            parent_id: parent,
            depth: parent.map(|_| 1).unwrap_or(0),
            title: title.into(),
            path_hash: node_path_hash(agent, "p", parent, title),
            body: format!("synthesized body for {title}"),
            sources: vec![ItemSource {
                kind: SourceKind::Codebase,
                skill: None,
                reference: "src/lib.rs#L1".into(),
                excerpt: Some("fn main".into()),
            }],
            coverage: vec![SurfaceItem {
                reference: "symbol::a".into(),
                covered: true,
            }],
            memory_id: Some(Uuid::new_v4().to_string()),
            status: NodeStatus::Modelled,
            confidence: 0.7,
            created_at: now,
            created_by: "identity-1".into(),
            verified_at: now,
            expires_at: Some(now + Duration::days(30)),
            reviewed_at: None,
            reviewed_by: None,
            review_note: None,
            superseded_by: None,
        }
    }

    fn store() -> SqliteSelfLearningStore {
        SqliteSelfLearningStore::in_memory().unwrap()
    }

    #[test]
    fn approve_marks_approved_and_records_reviewer() {
        let s = store();
        let n = node("a", None, "Discord gateway");
        s.upsert_node(&n).unwrap();

        let out = set_status(&s, "a", n.id, NodeStatus::Approved, "boss", None).unwrap();
        assert_eq!(out.status, NodeStatus::Approved);
        assert_eq!(out.reviewed_by.as_deref(), Some("boss"));
        assert!(out.reviewed_at.is_some());
    }

    #[test]
    fn reject_records_note() {
        let s = store();
        let n = node("a", None, "Discord gateway");
        s.upsert_node(&n).unwrap();

        let out = set_status(
            &s,
            "a",
            n.id,
            NodeStatus::Rejected,
            "boss",
            Some("hallucinated".into()),
        )
        .unwrap();
        assert_eq!(out.status, NodeStatus::Rejected);
        assert_eq!(out.review_note.as_deref(), Some("hallucinated"));
    }

    #[test]
    fn delete_cascades_subtree() {
        let s = store();
        let root = node("a", None, "Discord gateway");
        let child = node("a", Some(root.id), "Heartbeat");
        let grandchild = node("a", Some(child.id), "Ack timing");
        let sibling = node("a", None, "Slack events");
        for n in [&root, &child, &grandchild, &sibling] {
            s.upsert_node(n).unwrap();
        }

        let removed = delete_subtree(&s, "a", root.id).unwrap();
        assert_eq!(removed, 3);
        assert!(s.get_node(root.id).unwrap().is_none());
        assert!(s.get_node(sibling.id).unwrap().is_some());
    }

    #[test]
    fn node_with_children_returns_direct_children() {
        let s = store();
        let root = node("a", None, "Discord gateway");
        let child = node("a", Some(root.id), "Heartbeat");
        let grandchild = node("a", Some(child.id), "Ack timing");
        for n in [&root, &child, &grandchild] {
            s.upsert_node(n).unwrap();
        }

        let (got, children) = node_with_children(&s, "a", root.id).unwrap();
        assert_eq!(got.id, root.id);
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].id, child.id);
    }

    #[test]
    fn cross_agent_review_is_refused() {
        let s = store();
        let n = node("a", None, "Discord gateway");
        s.upsert_node(&n).unwrap();

        assert!(set_status(&s, "b", n.id, NodeStatus::Approved, "boss", None).is_err());
        assert!(delete_subtree(&s, "b", n.id).is_err());
        assert!(node_with_children(&s, "b", n.id).is_err());
        // Untouched: still Modelled, still present.
        assert_eq!(
            s.get_node(n.id).unwrap().unwrap().status,
            NodeStatus::Modelled
        );
    }

    #[test]
    fn missing_node_is_not_found() {
        let s = store();
        assert!(matches!(
            set_status(&s, "a", Uuid::new_v4(), NodeStatus::Approved, "boss", None),
            Err(ReviewError::NotFound)
        ));
    }

    #[test]
    fn list_roots_excludes_children() {
        let s = store();
        let root = node("a", None, "Discord gateway");
        let child = node("a", Some(root.id), "Heartbeat");
        s.upsert_node(&root).unwrap();
        s.upsert_node(&child).unwrap();

        let roots = s
            .list_nodes(
                "a",
                &NodeQuery {
                    parent: NodeParent::Roots,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].id, root.id);
    }
}
