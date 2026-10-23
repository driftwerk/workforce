//! `impact.*` MCP tools — "what breaks if I change X?" queries over the
//! shared code graph. Works across every repo that has been indexed into the
//! same graph (use `project.index` to pull a whole project's repos in).

use std::collections::BTreeMap;

use rmcp::{handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};
use workforce_codebase_knowledge::graph::entities::EntityId;
use workforce_codebase_knowledge::graph::queries::GraphQueryResult;

use crate::tools::common::GraphHit;
use crate::LocalMcpServer;

const DEFAULT_MAX_DEPTH: usize = 3;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CrossRepoArgs {
    /// Entity ID (from `codegraph.find_symbol`) whose impact to analyse.
    pub entity_id: String,
    /// Maximum BFS depth (default 3).
    #[serde(default)]
    pub max_depth: Option<usize>,
    /// If true, filter results to hits whose repo_id differs from the source
    /// entity's repo_id — "what breaks in *other* repos if I change this?"
    #[serde(default)]
    pub cross_boundary_only: bool,
}

#[derive(Debug, Serialize)]
struct RepoGroup {
    repo_id: String,
    hits: Vec<GraphHit>,
}

#[tool_router(router = impact_router, vis = "pub(crate)")]
impl LocalMcpServer {
    #[tool(
        name = "impact.cross_repo",
        description = "Given an entity_id, BFS across the shared code graph to find every \
                       entity affected by a change to it, grouped by repo. Walks \
                       called_by/imports/depends_on/implements edges. Pass \
                       cross_boundary_only=true to filter out same-repo hits — useful for \
                       'what breaks in *other* repos if I change this?'."
    )]
    async fn impact_cross_repo(
        &self,
        Parameters(args): Parameters<CrossRepoArgs>,
    ) -> Result<String, String> {
        let entity_id = EntityId(args.entity_id.clone());
        let graph = self.state.knowledge.graph();

        let source = graph
            .get_entity(&entity_id)
            .await
            .map_err(|e| format!("get_entity failed: {e}"))?
            .ok_or_else(|| format!("no entity with id '{}'", args.entity_id))?;

        let hits = graph
            .find_affected_entities(&entity_id, args.max_depth.unwrap_or(DEFAULT_MAX_DEPTH))
            .await
            .map_err(|e| format!("find_affected_entities failed: {e}"))?;

        let filtered = if args.cross_boundary_only {
            filter_cross_boundary(&source.repo_id, &hits)
        } else {
            hits.iter().collect()
        };

        Ok(serde_json::json!({
            "entity_id": args.entity_id,
            "source_repo_id": source.repo_id,
            "cross_boundary_only": args.cross_boundary_only,
            "total_affected": filtered.len(),
            "groups": group_by_repo(&filtered),
        })
        .to_string())
    }
}

fn filter_cross_boundary<'a>(
    source_repo_id: &str,
    hits: &'a [GraphQueryResult],
) -> Vec<&'a GraphQueryResult> {
    hits.iter()
        .filter(|h| h.entity.repo_id != source_repo_id)
        .collect()
}

fn group_by_repo(hits: &[&GraphQueryResult]) -> Vec<RepoGroup> {
    let mut buckets: BTreeMap<String, Vec<GraphHit>> = BTreeMap::new();
    for hit in hits {
        buckets
            .entry(hit.entity.repo_id.clone())
            .or_default()
            .push(GraphHit::from(*hit));
    }
    buckets
        .into_iter()
        .map(|(repo_id, hits)| RepoGroup { repo_id, hits })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_codebase_knowledge::graph::entities::{
        CodeEntity, CodeVisibility, EntityKind, Language,
    };
    use workforce_codebase_knowledge::graph::relationships::RelationshipKind;

    fn make_hit(repo_id: &str, id: &str) -> GraphQueryResult {
        GraphQueryResult {
            entity: CodeEntity {
                id: EntityId(id.into()),
                repo_id: repo_id.into(),
                kind: EntityKind::Function,
                name: id.into(),
                qualified_name: id.into(),
                file_path: "f.rs".into(),
                start_line: 1,
                end_line: 1,
                signature: None,
                visibility: CodeVisibility::Public,
                parent_id: None,
                doc_comment: None,
                content_hash: String::new(),
                language: Language::Rust,
            },
            relationship: RelationshipKind::CalledBy,
            distance: 1,
        }
    }

    #[test]
    fn filter_cross_boundary_drops_same_repo() {
        let hits = vec![make_hit("a", "x"), make_hit("b", "y"), make_hit("a", "z")];
        let filtered = filter_cross_boundary("a", &hits);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].entity.id.as_str(), "y");
    }

    #[test]
    fn group_by_repo_sorts_and_buckets() {
        let hits = [
            make_hit("zzz", "1"),
            make_hit("aaa", "2"),
            make_hit("zzz", "3"),
        ];
        let refs: Vec<_> = hits.iter().collect();
        let groups = group_by_repo(&refs);

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].repo_id, "aaa");
        assert_eq!(groups[0].hits.len(), 1);
        assert_eq!(groups[1].repo_id, "zzz");
        assert_eq!(groups[1].hits.len(), 2);
    }
}
