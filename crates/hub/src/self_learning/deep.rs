//! Deep-expertise curation — the knowledge-TREE counterpart to the flat
//! atomic-item curation in [`super::runner`].
//!
//! Where the atomic path records many independent facts per turn, this deepens
//! ONE node of a hierarchical tree per turn: it picks the thinnest node still
//! needing work ([`select_target`]), runs a single read-only `Curator` turn
//! under the deepening protocol so the model models that node from real source
//! and records it via `deep_expertise.record_node`, then runs the quality gate
//! (`crate::serve::AgentService::deep_expertise_quality_gate`) — claim grounding
//! promotes a clean node to `Verified`, conclusion review turns under-scoping findings
//! into uncovered coverage gaps the next turn deepens.
//!
//! The pure pieces ([`select_target`], the prompt assembly) are unit-tested in
//! isolation; the orchestration (`run_isolated`, grounding, review) is verified
//! by a real run.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use workforce_self_learning::{DeepNode, NodeStatus, SurfaceItem};

/// The node a deepening turn targets, resolved by [`select_target`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetSel {
    /// The sub-domain title the turn must (re-)model. For a seed this is the
    /// plan name; otherwise the existing node's title.
    pub title: String,
    /// The parent node id to attach under, or `None` for a root sub-domain.
    /// Threaded into the prompt so a re-record lands on the same tree position
    /// (`node_path_hash` over parent+title) and UPDATES in place.
    pub parent_id: Option<Uuid>,
    /// The existing node being deepened, if any. `None` ⇒ the tree is empty and
    /// this turn seeds the root.
    pub existing: Option<ExistingTarget>,
}

/// The current state of the node being deepened, surfaced to the prompt so the
/// turn extends rather than restarts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingTarget {
    pub id: Uuid,
    pub body: String,
    /// Surface elements the body does not yet cover — the gaps to close.
    pub uncovered: Vec<String>,
}

impl TargetSel {
    /// The id of the node this turn deepens, if it already exists.
    pub fn existing_id(&self) -> Option<Uuid> {
        self.existing.as_ref().map(|e| e.id)
    }
}

/// Count of a node's enumerated surface elements the body has NOT covered — the
/// "thinness" of a modelled node. A node with no coverage list counts as zero
/// uncovered (it enumerated no surface, so it falls to the `Modelled` tier, not
/// the most-uncovered tier).
fn uncovered_count(node: &DeepNode) -> usize {
    node.coverage.iter().filter(|c| !c.covered).count()
}

/// The uncovered surface references of a node, in order.
fn uncovered_refs(node: &DeepNode) -> Vec<String> {
    node.coverage
        .iter()
        .filter(|c| !c.covered)
        .map(|c| c.reference.clone())
        .collect()
}

/// Pure target selection for a deepening turn. Empty tree ⇒ seed the root
/// (title = `plan_name`, no parent). Otherwise pick the thinnest node by tier:
///
/// 1. any `Seeded` node (a child seed dropped by a prior turn, awaiting its
///    first real modelling);
/// 2. else the node with the MOST uncovered coverage elements (and at least
///    one — the widest known gap);
/// 3. else the shallowest `Modelled` node (re-deepen the closest-to-root node
///    that has no recorded gaps but isn't yet verified).
///
/// Within every tier ties break by oldest `verified_at` (the least recently
/// touched), so the loop fans out across the tree rather than re-hammering one
/// node. Returns `None` only for an empty `plan_name` with an empty tree (the
/// caller guards against that upstream).
pub fn select_target(nodes: &[DeepNode], plan_name: &str) -> Option<TargetSel> {
    if nodes.is_empty() {
        let title = plan_name.trim();
        if title.is_empty() {
            return None;
        }
        return Some(TargetSel {
            title: title.to_string(),
            parent_id: None,
            existing: None,
        });
    }

    let pick = select_seeded(nodes)
        .or_else(|| select_most_uncovered(nodes))
        .or_else(|| select_shallowest_modelled(nodes))
        // Every node is in some terminal/other status (Verified/Approved/…) —
        // re-deepen the oldest-verified node so re-verification still flows.
        .or_else(|| oldest_verified(nodes.iter().collect()));

    pick.map(|node| TargetSel {
        title: node.title.clone(),
        parent_id: node.parent_id,
        existing: Some(ExistingTarget {
            id: node.id,
            body: node.body.clone(),
            uncovered: uncovered_refs(node),
        }),
    })
}

fn select_seeded(nodes: &[DeepNode]) -> Option<&DeepNode> {
    oldest_verified(
        nodes
            .iter()
            .filter(|n| n.status == NodeStatus::Seeded)
            .collect(),
    )
}

fn select_most_uncovered(nodes: &[DeepNode]) -> Option<&DeepNode> {
    let max = nodes.iter().map(uncovered_count).max().unwrap_or(0);
    if max == 0 {
        return None;
    }
    oldest_verified(nodes.iter().filter(|n| uncovered_count(n) == max).collect())
}

fn select_shallowest_modelled(nodes: &[DeepNode]) -> Option<&DeepNode> {
    let modelled: Vec<&DeepNode> = nodes
        .iter()
        .filter(|n| n.status == NodeStatus::Modelled)
        .collect();
    let min_depth = modelled.iter().map(|n| n.depth).min()?;
    oldest_verified(
        modelled
            .into_iter()
            .filter(|n| n.depth == min_depth)
            .collect(),
    )
}

/// Break a tier's tie by oldest `verified_at`, then by `id` for a total,
/// deterministic order.
fn oldest_verified(candidates: Vec<&DeepNode>) -> Option<&DeepNode> {
    candidates.into_iter().min_by(|a, b| {
        a.verified_at
            .cmp(&b.verified_at)
            .then_with(|| a.id.cmp(&b.id))
    })
}

/// The fixed deepening protocol handed to the curation turn, with the target's
/// title and the per-run node budget substituted. Verbatim from the design.
pub fn deep_system_prompt(target_title: &str, max_items: u32) -> String {
    format!(
        "You are deepening a knowledge tree to become a genuine expert in a domain. This turn \
targets ONE node. Follow this protocol exactly — do not summarize.\n\n\
1. SCOPE: restate, as explicit clauses, what one must understand to be expert in \"{target_title}\", \
independently of any existing summary.\n\
2. ENUMERATE THE SURFACE: use the knowledge/codegraph tools ONLY as an index to list the full \
surface this node must cover — every relevant symbol, caller, callee, config and edge. Codegraph \
LOCATES; it is never your source of truth.\n\
3. MODEL FROM REAL SOURCE: for each surface element, use filesystem.read / filesystem.grep to read \
the ACTUAL files. Write a multi-paragraph body explaining the mechanism, the why, the trade-offs and \
the edge cases — grounded in what you read. Every claim must cite a real file:line you actually read. \
NEVER cite a knowledge/codegraph answer as a source.\n\
4. RECORD: call deep_expertise.record_node once for \"{target_title}\" with the body, the sources \
(each a real file:line you read), and the coverage list — every surface element from step 2, each \
marked covered true/false.\n\
5. SPAWN CHILDREN: for each surface element too large for this node, call deep_expertise.record_node \
for a CHILD (parent_id = the id returned for this node) with a one-paragraph seed body; the next turn \
deepens it.\n\n\
Record at most {max_items} nodes this turn. Stop when the target is modelled."
    )
}

/// A compact one-line view of a node for the tree overview block.
fn tree_line(node: &DeepNode) -> String {
    format!(
        "- {} · {} · depth {}\n",
        node.title,
        node.status.as_str(),
        node.depth
    )
}

/// Hard cap on tree-overview lines in one prompt. The full dump grew linearly
/// with the tree and was re-billed every round of a ~30-round turn — the
/// production cost ramp.
const TREE_CONTEXT_MAX_LINES: usize = 150;

/// The bounded tree block for a deepening prompt: the target's neighbourhood
/// (ancestor chain, siblings, children) plus a root overview (depth ≤ 1),
/// with Rejected/Superseded/Stale nodes excluded and everything beyond
/// [`TREE_CONTEXT_MAX_LINES`] elided with a count. Deterministic order:
/// depth, then title, then id.
fn tree_context(nodes: &[DeepNode], target: &TargetSel) -> String {
    use std::collections::{HashMap, HashSet};

    let live: Vec<&DeepNode> = nodes
        .iter()
        .filter(|n| {
            !matches!(
                n.status,
                NodeStatus::Rejected | NodeStatus::Superseded | NodeStatus::Stale
            )
        })
        .collect();
    let by_id: HashMap<Uuid, &DeepNode> = live.iter().map(|n| (n.id, *n)).collect();

    // The target's own chain (ancestors, target, siblings, children) ranks
    // ahead of the root overview so the line cap can never truncate the
    // context the turn actually deepens against.
    let mut chain: HashSet<Uuid> = HashSet::new();
    let mut cursor = target.parent_id;
    while let Some(pid) = cursor {
        if !chain.insert(pid) {
            break;
        }
        cursor = by_id.get(&pid).and_then(|n| n.parent_id);
    }
    let target_id = target.existing_id();
    for n in &live {
        let in_chain = n.parent_id == target.parent_id
            || (target_id.is_some() && n.parent_id == target_id)
            || Some(n.id) == target_id;
        if in_chain {
            chain.insert(n.id);
        }
    }

    let mut selected: Vec<&DeepNode> = live
        .iter()
        .filter(|n| chain.contains(&n.id) || n.depth <= 1)
        .copied()
        .collect();
    selected.sort_by(|a, b| {
        chain
            .contains(&b.id)
            .cmp(&chain.contains(&a.id))
            .then_with(|| a.depth.cmp(&b.depth))
            .then_with(|| a.title.cmp(&b.title))
            .then_with(|| a.id.cmp(&b.id))
    });
    selected.truncate(TREE_CONTEXT_MAX_LINES);

    let mut out = String::new();
    for n in &selected {
        out.push_str(&tree_line(n));
    }
    let hidden = live.len() - selected.len();
    if hidden > 0 {
        out.push_str(&format!("… and {hidden} more nodes (elided)\n"));
    }
    out
}

/// Assemble the deepening turn's user prompt: the plan objectives, a compact
/// view of the current tree, and — for an existing target — its current body and
/// the uncovered surface to close. Pure over `(plan_name, plan_body, nodes,
/// target, now)` so it is unit-testable.
pub fn build_deep_prompt(
    plan_name: &str,
    plan_body: &str,
    nodes: &[DeepNode],
    target: &TargetSel,
    now: DateTime<Utc>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Learning objectives: {plan_name}\n\n"));
    out.push_str(plan_body.trim());
    out.push_str("\n\n");

    if nodes.is_empty() {
        out.push_str(
            "## Current tree\n(empty — this turn SEEDS the root node for the domain above)\n\n",
        );
    } else {
        out.push_str("## Current tree — the target's neighbourhood (title · status · depth)\n");
        out.push_str(&tree_context(nodes, target));
        out.push('\n');
    }

    out.push_str(&format!("## This turn's target: {}\n", target.title));
    match (&target.parent_id, &target.existing) {
        (None, None) => {
            out.push_str(
                "This is a NEW ROOT node (omit parent_id). Model the domain's top-level surface, \
                 then seed children for its major sub-domains.\n\n",
            );
        }
        (parent, existing) => {
            if let Some(pid) = parent {
                out.push_str(&format!(
                    "Attach to parent_id = {pid} (pass it on the record_node call so this UPDATES \
                     the existing node in place rather than creating a duplicate).\n",
                ));
            } else {
                out.push_str(
                    "This is a ROOT node — omit parent_id so the record_node call UPDATES the \
                     existing node in place rather than creating a duplicate.\n",
                );
            }
            if let Some(e) = existing {
                if !e.body.trim().is_empty() {
                    out.push_str("\n### Current body (extend and deepen it)\n");
                    out.push_str(e.body.trim());
                    out.push('\n');
                }
                if e.uncovered.is_empty() {
                    out.push_str(
                        "\nNo uncovered surface is recorded yet — enumerate the full surface, then \
                         close every gap.\n\n",
                    );
                } else {
                    out.push_str("\n### Surface still to cover (close these gaps)\n");
                    for reference in &e.uncovered {
                        out.push_str(&format!("- {reference}\n"));
                    }
                    out.push('\n');
                }
            }
        }
    }

    out.push_str(&format!(
        "Current time (UTC): {}\n",
        now.format("%Y-%m-%d %H:%M:%S")
    ));
    out
}

/// Merge reviewer findings into a node's coverage as uncovered gaps (an upsert
/// keyed by reference): an existing reference keeps its `covered` flag, a new
/// finding is appended as `covered: false`. Pure so the merge is unit-testable;
/// the caller persists the result. Returns the merged coverage.
pub fn merge_findings_into_coverage(
    coverage: &[SurfaceItem],
    findings: &[String],
) -> Vec<SurfaceItem> {
    let mut merged = coverage.to_vec();
    for finding in findings {
        let reference = finding.trim();
        if reference.is_empty() {
            continue;
        }
        if merged.iter().any(|c| c.reference == reference) {
            continue;
        }
        merged.push(SurfaceItem {
            reference: reference.to_string(),
            covered: false,
        });
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_self_learning::{ItemSource, SourceKind};

    fn node(
        title: &str,
        depth: u32,
        status: NodeStatus,
        coverage: Vec<(&str, bool)>,
        verified_at: DateTime<Utc>,
    ) -> DeepNode {
        DeepNode {
            id: Uuid::new_v4(),
            agent_id: "a".into(),
            plan_id: "p".into(),
            parent_id: None,
            depth,
            title: title.into(),
            path_hash: "h".into(),
            body: format!("body of {title}"),
            sources: vec![ItemSource {
                kind: SourceKind::Codebase,
                skill: None,
                reference: "src.rs:1".into(),
                excerpt: None,
            }],
            coverage: coverage
                .into_iter()
                .map(|(r, c)| SurfaceItem {
                    reference: r.into(),
                    covered: c,
                })
                .collect(),
            memory_id: None,
            status,
            confidence: 0.5,
            created_at: verified_at,
            created_by: "id".into(),
            verified_at,
            expires_at: None,
            reviewed_at: None,
            reviewed_by: None,
            review_note: None,
            superseded_by: None,
        }
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).unwrap()
    }

    #[test]
    fn empty_tree_seeds_the_root() {
        let sel = select_target(&[], "Discord platform").unwrap();
        assert_eq!(sel.title, "Discord platform");
        assert!(sel.parent_id.is_none());
        assert!(sel.existing.is_none());
    }

    #[test]
    fn empty_tree_with_blank_name_is_none() {
        assert!(select_target(&[], "   ").is_none());
    }

    #[test]
    fn seeded_node_wins_over_uncovered_and_modelled() {
        let seeded = node("Seed", 2, NodeStatus::Seeded, vec![], at(100));
        let uncovered = node(
            "Wide",
            1,
            NodeStatus::Modelled,
            vec![("a", false), ("b", false), ("c", false)],
            at(50),
        );
        let modelled = node("Shallow", 0, NodeStatus::Modelled, vec![], at(10));
        let nodes = vec![uncovered, modelled, seeded.clone()];
        let sel = select_target(&nodes, "plan").unwrap();
        assert_eq!(sel.title, "Seed");
        assert_eq!(sel.existing_id(), Some(seeded.id));
    }

    #[test]
    fn most_uncovered_wins_when_no_seed() {
        let wide = node(
            "Wide",
            1,
            NodeStatus::Modelled,
            vec![("a", false), ("b", false), ("c", false)],
            at(100),
        );
        let narrow = node(
            "Narrow",
            0,
            NodeStatus::Modelled,
            vec![("x", false)],
            at(10),
        );
        let sel = select_target(&[narrow, wide.clone()], "plan").unwrap();
        assert_eq!(sel.title, "Wide");
        // The uncovered refs are threaded through for the prompt.
        assert_eq!(
            sel.existing.unwrap().uncovered,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn shallowest_modelled_wins_when_all_covered() {
        // No seed, every coverage element covered (so uncovered tier is empty):
        // fall to the shallowest Modelled node.
        let deep = node("Deep", 3, NodeStatus::Modelled, vec![("a", true)], at(10));
        let shallow = node(
            "Shallow",
            1,
            NodeStatus::Modelled,
            vec![("b", true)],
            at(100),
        );
        let sel = select_target(&[deep, shallow.clone()], "plan").unwrap();
        assert_eq!(sel.title, "Shallow");
        assert_eq!(sel.existing_id(), Some(shallow.id));
    }

    #[test]
    fn ties_break_by_oldest_verified() {
        // Two equally-wide uncovered nodes: the older `verified_at` wins.
        let older = node(
            "Older",
            1,
            NodeStatus::Modelled,
            vec![("a", false), ("b", false)],
            at(10),
        );
        let newer = node(
            "Newer",
            1,
            NodeStatus::Modelled,
            vec![("c", false), ("d", false)],
            at(900),
        );
        let sel = select_target(&[newer, older.clone()], "plan").unwrap();
        assert_eq!(sel.title, "Older");
        assert_eq!(sel.existing_id(), Some(older.id));
    }

    #[test]
    fn child_target_carries_parent_id() {
        let parent = Uuid::new_v4();
        let mut child = node("Child", 1, NodeStatus::Seeded, vec![], at(10));
        child.parent_id = Some(parent);
        let sel = select_target(std::slice::from_ref(&child), "plan").unwrap();
        assert_eq!(sel.parent_id, Some(parent));
        assert_eq!(sel.title, "Child");
    }

    #[test]
    fn prompt_for_seed_says_new_root_and_lists_no_tree() {
        let target = select_target(&[], "Billing domain").unwrap();
        let out = build_deep_prompt("Billing domain", "Learn billing.", &[], &target, at(0));
        assert!(out.contains("Learning objectives: Billing domain"));
        assert!(out.contains("empty — this turn SEEDS the root"));
        assert!(out.contains("NEW ROOT node"));
    }

    #[test]
    fn prompt_for_existing_target_shows_body_uncovered_and_parent() {
        let parent = Uuid::new_v4();
        let mut target_node = node(
            "Slash commands",
            1,
            NodeStatus::Modelled,
            vec![("register_command", false), ("dispatch", true)],
            at(10),
        );
        target_node.parent_id = Some(parent);
        let nodes = vec![target_node.clone()];
        let target = select_target(&nodes, "plan").unwrap();
        let out = build_deep_prompt("plan", "objectives", &nodes, &target, at(0));
        assert!(out.contains("This turn's target: Slash commands"));
        assert!(out.contains(&format!("parent_id = {parent}")));
        assert!(out.contains("Current body"));
        assert!(out.contains("body of Slash commands"));
        assert!(out.contains("Surface still to cover"));
        assert!(out.contains("register_command"));
        // A covered element is NOT listed as a gap.
        assert!(!out.contains("- dispatch\n"));
        // The tree overview lists the node line.
        assert!(out.contains("Slash commands · modelled · depth 1"));
    }

    #[test]
    fn deep_system_prompt_substitutes_title_and_budget() {
        let p = deep_system_prompt("Discord surface", 7);
        assert!(p.contains("expert in \"Discord surface\""));
        assert!(p.contains("Record at most 7 nodes this turn"));
        assert!(p.contains("Codegraph LOCATES"));
    }

    #[test]
    fn tree_context_keeps_neighbourhood_and_elides_the_rest() {
        // Target at depth 2 under parent P; a large unrelated subtree at
        // depth 2-3 must be elided, roots stay as overview.
        let mut parent = node("Parent", 1, NodeStatus::Modelled, vec![], at(10));
        let parent_id = parent.id;
        parent.parent_id = None;
        let mut target_node = node("Target", 2, NodeStatus::Seeded, vec![], at(5));
        target_node.parent_id = Some(parent_id);
        let mut sibling = node("Sibling", 2, NodeStatus::Modelled, vec![], at(20));
        sibling.parent_id = Some(parent_id);
        let mut child = node("Child", 3, NodeStatus::Seeded, vec![], at(30));
        child.parent_id = Some(target_node.id);
        let other_root = node("OtherRoot", 1, NodeStatus::Verified, vec![], at(40));
        let mut nodes = vec![
            parent,
            target_node.clone(),
            sibling,
            child,
            other_root.clone(),
        ];
        for i in 0..20 {
            let mut stray = node(
                &format!("Stray{i}"),
                2,
                NodeStatus::Modelled,
                vec![],
                at(50),
            );
            stray.parent_id = Some(other_root.id);
            nodes.push(stray);
        }
        let rejected = {
            let mut n = node("Dead", 1, NodeStatus::Rejected, vec![], at(60));
            n.parent_id = None;
            n
        };
        nodes.push(rejected);

        let target = TargetSel {
            title: "Target".into(),
            parent_id: Some(parent_id),
            existing: Some(ExistingTarget {
                id: target_node.id,
                body: String::new(),
                uncovered: vec![],
            }),
        };
        let out = tree_context(&nodes, &target);
        assert!(out.contains("- Parent"));
        assert!(out.contains("- Target"));
        assert!(out.contains("- Sibling"));
        assert!(out.contains("- Child"));
        assert!(out.contains("- OtherRoot"));
        assert!(!out.contains("Stray"));
        assert!(!out.contains("Dead"));
        assert!(out.contains("and 20 more nodes (elided)"));
    }

    #[test]
    fn tree_context_caps_total_lines() {
        let target = TargetSel {
            title: "plan".into(),
            parent_id: None,
            existing: None,
        };
        // 400 root-overview nodes: everything qualifies, the cap truncates.
        let nodes: Vec<DeepNode> = (0..400)
            .map(|i| node(&format!("N{i:03}"), 1, NodeStatus::Modelled, vec![], at(i)))
            .collect();
        let out = tree_context(&nodes, &target);
        assert_eq!(out.lines().filter(|l| l.starts_with("- ")).count(), 150);
        assert!(out.contains("and 250 more nodes (elided)"));
    }

    #[test]
    fn merge_findings_appends_new_gaps_and_keeps_existing_flags() {
        let coverage = vec![
            SurfaceItem {
                reference: "covered_one".into(),
                covered: true,
            },
            SurfaceItem {
                reference: "gap_one".into(),
                covered: false,
            },
        ];
        let findings = vec![
            "covered_one".to_string(), // already present: untouched, stays covered
            "new_gap".to_string(),     // appended as uncovered
            "  ".to_string(),          // blank: skipped
        ];
        let merged = merge_findings_into_coverage(&coverage, &findings);
        assert_eq!(merged.len(), 3);
        // The pre-existing covered element keeps its flag.
        assert!(merged
            .iter()
            .any(|c| c.reference == "covered_one" && c.covered));
        // The new finding is an uncovered gap.
        assert!(merged
            .iter()
            .any(|c| c.reference == "new_gap" && !c.covered));
    }
}
