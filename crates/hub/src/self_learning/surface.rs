//! Pure join + filter + render for the `## Self-Learned Knowledge` context
//! section (design §7). The read path performs NO writes: expired items are
//! dropped here as a pure filter; the actual `Stale` transition + memory
//! forget happens on the runner tick, off the hot path.

use chrono::{DateTime, Utc};

use workforce_common::settings::SelfLearningConfig;
use workforce_memory::types::MemoryId;
use workforce_self_learning::{
    DeepExpertiseStore, DeepNode, ItemStatus, KnowledgeItem, NodeStatus,
};

const SECTION_HEADER: &str = "## Self-Learned Knowledge\n\
    _Agent-curated from connected sources. Lower-confidence than Training; verify before relying on specifics._\n";

const DEEP_SECTION_HEADER: &str = "## Deep Expertise\n\
    _Agent-synthesized sub-domain knowledge, deepest first. Grounded in cited sources; verify before relying on specifics._\n";

/// Most deep nodes the block ever surfaces, regardless of `recall_top_k` — deep
/// bodies are large, so the block is capped tighter than the atomic one.
const DEEP_MAX_NODES: usize = 3;

/// Per-node body budget in characters; longer bodies are truncated with an
/// ellipsis so a single node can never blow the context window.
const DEEP_BODY_BUDGET: usize = 1500;

/// Defensive ceiling on the ancestor walk, independent of the node's reported
/// `depth`, so a cyclic or corrupt `parent_id` chain can never loop forever.
const DEEP_MAX_ANCESTORS: usize = 32;

/// Join recall `hits` (memory id + score) to the SQL `items` by `memory_id`,
/// apply the surfaceability filter, and render the section. Returns `None`
/// when nothing survives (so the caller appends nothing).
///
/// Filter (pure — no writes):
/// - keep only `Provisional` / `Approved`;
/// - drop `high_stakes && Provisional` (never surfaced until approved);
/// - drop items past `expires_at` (the runner does the real expiry);
/// - drop ALL provisional when `cfg.surface_provisional` is false.
///
/// Order: approved before provisional; ties broken by recall score.
pub fn render_self_learned(
    hits: &[(MemoryId, f32)],
    items: &[KnowledgeItem],
    cfg: &SelfLearningConfig,
    now: DateTime<Utc>,
) -> Option<String> {
    let mut rows: Vec<(f32, &KnowledgeItem)> = hits
        .iter()
        .filter_map(|(mid, score)| {
            let item = items
                .iter()
                .find(|i| i.memory_id.as_deref() == Some(mid.0.as_str()))?;
            surfaceable(item, cfg, now).then_some((*score, item))
        })
        .collect();

    if rows.is_empty() {
        return None;
    }

    // Approved before provisional; within a status, higher score first.
    rows.sort_by(|(sa, a), (sb, b)| {
        approved_rank(a)
            .cmp(&approved_rank(b))
            .then(sb.partial_cmp(sa).unwrap_or(std::cmp::Ordering::Equal))
    });

    let body = rows
        .iter()
        .map(|(_, item)| render_line(item, now))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!("{SECTION_HEADER}{body}"))
}

fn approved_rank(item: &KnowledgeItem) -> u8 {
    match item.status {
        ItemStatus::Approved => 0,
        _ => 1,
    }
}

/// Join recall `hits` (memory id + score) to the deep-expertise `nodes` by
/// `memory_id`, keep only surfaceable nodes, and render the `## Deep Expertise`
/// section. Returns `None` when nothing survives.
///
/// Filter (pure — no writes): only `Verified` / `Approved` / `Modelled` surface
/// (`Seeded` stubs have no real body; `Rejected` / `Superseded` / `Stale` are
/// retired). Order: verified/approved before modelled, ties by recall score.
/// At most [`DEEP_MAX_NODES`] (further bounded by `cfg.recall_top_k`) nodes
/// surface, each body truncated to [`DEEP_BODY_BUDGET`] chars. `store` is read
/// to walk each node's ancestor titles into a breadcrumb.
pub fn render_deep_expertise(
    hits: &[(MemoryId, f32)],
    nodes: &[DeepNode],
    store: &dyn DeepExpertiseStore,
    cfg: &SelfLearningConfig,
) -> Option<String> {
    let mut rows: Vec<(f32, &DeepNode)> = hits
        .iter()
        .filter_map(|(mid, score)| {
            let node = nodes
                .iter()
                .find(|n| n.memory_id.as_deref() == Some(mid.0.as_str()))?;
            node_surfaceable(node).then_some((*score, node))
        })
        .collect();

    if rows.is_empty() {
        return None;
    }

    rows.sort_by(|(sa, a), (sb, b)| {
        node_settled_rank(a)
            .cmp(&node_settled_rank(b))
            .then(sb.partial_cmp(sa).unwrap_or(std::cmp::Ordering::Equal))
    });

    let cap = cfg.recall_top_k.min(DEEP_MAX_NODES);
    let body = rows
        .iter()
        .take(cap)
        .map(|(_, node)| render_node(node, store))
        .collect::<Vec<_>>()
        .join("\n\n");
    Some(format!("{DEEP_SECTION_HEADER}{body}"))
}

/// `Verified` / `Approved` outrank `Modelled`; nothing else is surfaceable so it
/// never reaches the comparator.
fn node_settled_rank(node: &DeepNode) -> u8 {
    match node.status {
        NodeStatus::Verified | NodeStatus::Approved => 0,
        _ => 1,
    }
}

fn node_surfaceable(node: &DeepNode) -> bool {
    match node.status {
        NodeStatus::Verified | NodeStatus::Approved => true,
        // Drawn from conversation transcripts, a node reaches a prompt only
        // once someone has approved it: modelled, it is still the curator's
        // reading of private conversations.
        NodeStatus::Modelled => !node
            .sources
            .iter()
            .any(|s| s.kind == workforce_self_learning::SourceKind::Experience),
        _ => false,
    }
}

/// Root → … → node titles, walking `parent_id` via the store. Bounded by
/// [`DEEP_MAX_ANCESTORS`] and stops on any error / missing parent, degrading to
/// a partial breadcrumb rather than looping on a cyclic or corrupt chain.
fn breadcrumb(node: &DeepNode, store: &dyn DeepExpertiseStore) -> Vec<String> {
    let mut titles = vec![node.title.clone()];
    let mut parent = node.parent_id;
    for _ in 0..DEEP_MAX_ANCESTORS {
        let Some(pid) = parent else { break };
        match store.get_node(pid) {
            Ok(Some(p)) => {
                titles.push(p.title.clone());
                parent = p.parent_id;
            }
            _ => break,
        }
    }
    titles.reverse();
    titles
}

fn render_node(node: &DeepNode, store: &dyn DeepExpertiseStore) -> String {
    let crumb = breadcrumb(node, store).join(" › ");
    let status = node.status.as_str();
    let body = truncate_chars(node.body.trim(), DEEP_BODY_BUDGET);
    format!("### [{status}] {crumb}\n{body}")
}

/// Truncate to at most `budget` chars on a char boundary, appending an ellipsis
/// when anything was dropped. Char-based so multibyte markdown never panics.
fn truncate_chars(s: &str, budget: usize) -> String {
    if s.chars().count() <= budget {
        return s.to_string();
    }
    let mut out: String = s.chars().take(budget).collect();
    out.push('…');
    out
}

fn surfaceable(item: &KnowledgeItem, cfg: &SelfLearningConfig, now: DateTime<Utc>) -> bool {
    match item.status {
        ItemStatus::Approved => {}
        ItemStatus::Provisional => {
            if item.high_stakes || !cfg.surface_provisional {
                return false;
            }
        }
        ItemStatus::Rejected | ItemStatus::Stale | ItemStatus::Superseded => return false,
    }
    item.expires_at.is_none_or(|exp| exp > now)
}

fn render_line(item: &KnowledgeItem, now: DateTime<Utc>) -> String {
    let status = item.status.as_str();
    let provenance = item
        .source
        .skill
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| item.source.kind.as_str());
    let freshness = humanize_age(now - item.verified_at);
    let marker = if item.status == ItemStatus::Provisional {
        " — pending review"
    } else {
        ""
    };
    format!(
        "- [{status} · {provenance} · {freshness}] {} (src: {}){marker}",
        item.claim, item.source.reference
    )
}

/// Compact age label for the freshness hint: "today" / "3d ago" / "2w ago".
fn humanize_age(age: chrono::Duration) -> String {
    let days = age.num_days().max(0);
    if days == 0 {
        "today".to_string()
    } else if days < 14 {
        format!("{days}d ago")
    } else {
        format!("{}w ago", days / 7)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use workforce_self_learning::{ItemSource, SourceKind};

    fn cfg() -> SelfLearningConfig {
        SelfLearningConfig::default()
    }

    fn item(status: ItemStatus, claim: &str, memory_id: &str) -> KnowledgeItem {
        let now = Utc::now();
        KnowledgeItem {
            id: uuid::Uuid::new_v4(),
            agent_id: "a".into(),
            plan_id: "p".into(),
            topic: "billing".into(),
            claim: claim.into(),
            content_hash: "h".into(),
            memory_id: Some(memory_id.into()),
            status,
            confidence: 0.8,
            high_stakes: false,
            source: ItemSource {
                kind: SourceKind::Skill,
                skill: Some("notion".into()),
                reference: "Billing Overview".into(),
                excerpt: None,
            },
            created_at: now,
            created_by: "id".into(),
            verified_at: now,
            expires_at: Some(now + Duration::days(30)),
            reviewed_at: None,
            reviewed_by: None,
            review_note: None,
            superseded_by: None,
        }
    }

    fn hit(memory_id: &str, score: f32) -> (MemoryId, f32) {
        (MemoryId(memory_id.into()), score)
    }

    #[test]
    fn empty_inputs_render_none() {
        assert!(render_self_learned(&[], &[], &cfg(), Utc::now()).is_none());
    }

    #[test]
    fn approved_sorts_before_provisional() {
        let items = vec![
            item(ItemStatus::Provisional, "prov fact", "m1"),
            item(ItemStatus::Approved, "appr fact", "m2"),
        ];
        let hits = vec![hit("m1", 0.9), hit("m2", 0.5)];
        let out = render_self_learned(&hits, &items, &cfg(), Utc::now()).unwrap();
        let appr = out.find("appr fact").unwrap();
        let prov = out.find("prov fact").unwrap();
        assert!(
            appr < prov,
            "approved must render before provisional: {out}"
        );
        assert!(out.contains("[approved · notion · today]"));
        assert!(out.contains("[provisional · notion · today]"));
        assert!(out.contains("pending review"));
    }

    #[test]
    fn high_stakes_provisional_dropped() {
        let mut it = item(ItemStatus::Provisional, "pricing is X", "m1");
        it.high_stakes = true;
        let out = render_self_learned(&[hit("m1", 0.9)], &[it], &cfg(), Utc::now());
        assert!(out.is_none(), "high-stakes provisional must not surface");
    }

    #[test]
    fn high_stakes_approved_surfaces() {
        let mut it = item(ItemStatus::Approved, "pricing is X", "m1");
        it.high_stakes = true;
        let out = render_self_learned(&[hit("m1", 0.9)], &[it], &cfg(), Utc::now()).unwrap();
        assert!(out.contains("pricing is X"));
    }

    #[test]
    fn expired_dropped() {
        let now = Utc::now();
        let mut it = item(ItemStatus::Approved, "stale fact", "m1");
        it.expires_at = Some(now - Duration::days(1));
        assert!(render_self_learned(&[hit("m1", 0.9)], &[it], &cfg(), now).is_none());
    }

    #[test]
    fn rejected_and_superseded_dropped() {
        let items = vec![
            item(ItemStatus::Rejected, "rej", "m1"),
            item(ItemStatus::Superseded, "sup", "m2"),
            item(ItemStatus::Stale, "stale", "m3"),
        ];
        let hits = vec![hit("m1", 0.9), hit("m2", 0.9), hit("m3", 0.9)];
        assert!(render_self_learned(&hits, &items, &cfg(), Utc::now()).is_none());
    }

    #[test]
    fn surface_provisional_false_drops_all_provisional() {
        let mut c = cfg();
        c.surface_provisional = false;
        let items = vec![
            item(ItemStatus::Provisional, "unreviewed claim", "m1"),
            item(ItemStatus::Approved, "blessed claim", "m2"),
        ];
        let hits = vec![hit("m1", 0.9), hit("m2", 0.5)];
        let out = render_self_learned(&hits, &items, &c, Utc::now()).unwrap();
        assert!(!out.contains("unreviewed claim"));
        assert!(!out.contains("provisional"));
        assert!(out.contains("blessed claim"));
    }

    #[test]
    fn freshness_label_renders_days() {
        let now = Utc::now();
        let mut it = item(ItemStatus::Approved, "old fact", "m1");
        it.verified_at = now - Duration::days(3);
        let out = render_self_learned(&[hit("m1", 0.9)], &[it], &cfg(), now).unwrap();
        assert!(out.contains("3d ago"), "{out}");
    }

    #[test]
    fn hit_with_no_matching_item_is_skipped() {
        let items = vec![item(ItemStatus::Approved, "appr", "m2")];
        // Hit references a memory id with no item row (already forgotten).
        let out = render_self_learned(&[hit("m1", 0.9)], &items, &cfg(), Utc::now());
        assert!(out.is_none());
    }

    mod deep {
        use super::*;
        use uuid::Uuid;
        use workforce_self_learning::{
            sqlite::SqliteSelfLearningStore, DeepNode, NodeStatus, SurfaceItem,
        };

        fn store() -> SqliteSelfLearningStore {
            SqliteSelfLearningStore::in_memory().unwrap()
        }

        fn node(
            id: Uuid,
            parent: Option<Uuid>,
            depth: u32,
            title: &str,
            status: NodeStatus,
            memory_id: &str,
            body: &str,
        ) -> DeepNode {
            let now = Utc::now();
            DeepNode {
                id,
                agent_id: "a".into(),
                plan_id: "p".into(),
                parent_id: parent,
                depth,
                title: title.into(),
                path_hash: format!("h-{title}"),
                body: body.into(),
                sources: vec![ItemSource {
                    kind: SourceKind::Skill,
                    skill: Some("notion".into()),
                    reference: "page-1".into(),
                    excerpt: None,
                }],
                coverage: Vec::<SurfaceItem>::new(),
                memory_id: Some(memory_id.into()),
                status,
                confidence: 0.8,
                created_at: now,
                created_by: "id".into(),
                verified_at: now,
                expires_at: Some(now + Duration::days(30)),
                reviewed_at: None,
                reviewed_by: None,
                review_note: None,
                superseded_by: None,
            }
        }

        #[test]
        fn a_node_learned_from_transcripts_waits_for_approval() {
            let mut learned = node(
                Uuid::new_v4(),
                None,
                0,
                "From chats",
                NodeStatus::Modelled,
                "m1",
                "b",
            );
            learned.sources = vec![ItemSource {
                kind: SourceKind::Experience,
                skill: None,
                reference: "s:0".into(),
                excerpt: None,
            }];
            assert!(
                !node_surfaceable(&learned),
                "modelled from transcripts: not shown"
            );
            learned.status = NodeStatus::Approved;
            assert!(node_surfaceable(&learned), "approved: shown");
            let from_docs = node(
                Uuid::new_v4(),
                None,
                0,
                "Docs",
                NodeStatus::Modelled,
                "m2",
                "b",
            );
            assert!(
                node_surfaceable(&from_docs),
                "modelled from a skill source is unchanged"
            );
        }

        #[test]
        fn empty_recall_renders_no_block() {
            let s = store();
            assert!(render_deep_expertise(&[], &[], &s, &cfg()).is_none());
        }

        #[test]
        fn breadcrumb_walks_root_to_node() {
            let s = store();
            let root = Uuid::new_v4();
            let mid = Uuid::new_v4();
            let leaf = Uuid::new_v4();
            let nodes = vec![
                node(
                    root,
                    None,
                    0,
                    "Discord",
                    NodeStatus::Verified,
                    "mr",
                    "root body",
                ),
                node(
                    mid,
                    Some(root),
                    1,
                    "Commands",
                    NodeStatus::Verified,
                    "mm",
                    "mid body",
                ),
                node(
                    leaf,
                    Some(mid),
                    2,
                    "Slash",
                    NodeStatus::Verified,
                    "ml",
                    "leaf body",
                ),
            ];
            for n in &nodes {
                s.upsert_node(n).unwrap();
            }
            let out =
                render_deep_expertise(&[(MemoryId("ml".into()), 0.9)], &nodes, &s, &cfg()).unwrap();
            assert!(
                out.contains("Discord › Commands › Slash"),
                "breadcrumb root→node: {out}"
            );
            assert!(out.contains("leaf body"));
        }

        #[test]
        fn breadcrumb_degrades_on_missing_parent() {
            let s = store();
            // The orphan's parent is never inserted into the store.
            let orphan = node(
                Uuid::new_v4(),
                Some(Uuid::new_v4()),
                1,
                "Lonely",
                NodeStatus::Verified,
                "mo",
                "orphan body",
            );
            let out = render_deep_expertise(&[(MemoryId("mo".into()), 0.9)], &[orphan], &s, &cfg())
                .unwrap();
            assert!(out.contains("Lonely"), "{out}");
            assert!(!out.contains("›"), "no resolvable ancestor: {out}");
        }

        #[test]
        fn seeded_and_retired_statuses_excluded() {
            let s = store();
            let mk = |status, mid: &str| node(Uuid::new_v4(), None, 0, "T", status, mid, "body");
            let nodes = vec![
                mk(NodeStatus::Seeded, "m1"),
                mk(NodeStatus::Rejected, "m2"),
                mk(NodeStatus::Superseded, "m3"),
                mk(NodeStatus::Stale, "m4"),
            ];
            let hits = vec![
                (MemoryId("m1".into()), 0.9),
                (MemoryId("m2".into()), 0.9),
                (MemoryId("m3".into()), 0.9),
                (MemoryId("m4".into()), 0.9),
            ];
            assert!(render_deep_expertise(&hits, &nodes, &s, &cfg()).is_none());
        }

        #[test]
        fn verified_sorts_before_modelled() {
            let s = store();
            let nodes = vec![
                node(
                    Uuid::new_v4(),
                    None,
                    0,
                    "Modelled one",
                    NodeStatus::Modelled,
                    "m1",
                    "modelled body",
                ),
                node(
                    Uuid::new_v4(),
                    None,
                    0,
                    "Verified one",
                    NodeStatus::Verified,
                    "m2",
                    "verified body",
                ),
            ];
            // Modelled has the higher recall score; status must still win.
            let hits = vec![(MemoryId("m1".into()), 0.95), (MemoryId("m2".into()), 0.5)];
            let out = render_deep_expertise(&hits, &nodes, &s, &cfg()).unwrap();
            let v = out.find("Verified one").unwrap();
            let m = out.find("Modelled one").unwrap();
            assert!(v < m, "verified must render before modelled: {out}");
        }

        #[test]
        fn truncate_chars_caps_to_budget_with_ellipsis() {
            let short = truncate_chars("hello", DEEP_BODY_BUDGET);
            assert_eq!(short, "hello", "under budget is untouched");
            assert!(!short.contains('…'));

            let big = "x".repeat(DEEP_BODY_BUDGET + 500);
            let capped = truncate_chars(&big, DEEP_BODY_BUDGET);
            assert!(capped.ends_with('…'), "truncated ends with ellipsis");
            assert_eq!(
                capped.chars().filter(|c| *c == 'x').count(),
                DEEP_BODY_BUDGET,
                "body capped to budget chars",
            );
        }

        #[test]
        fn body_truncated_in_rendered_block() {
            let s = store();
            let big = "x".repeat(DEEP_BODY_BUDGET + 500);
            let n = node(
                Uuid::new_v4(),
                None,
                0,
                "Big",
                NodeStatus::Verified,
                "m1",
                &big,
            );
            let out =
                render_deep_expertise(&[(MemoryId("m1".into()), 0.9)], &[n], &s, &cfg()).unwrap();
            assert!(out.contains('…'), "truncation ellipsis present: {out}");
            // The original body had BUDGET+500 'x'; the rendered block must carry
            // strictly fewer, proving truncation ran.
            assert!(out.matches('x').count() <= DEEP_BODY_BUDGET + 1);
        }

        #[test]
        fn truncation_is_char_safe_on_multibyte() {
            let s = store();
            let big = "é".repeat(DEEP_BODY_BUDGET + 100);
            let n = node(
                Uuid::new_v4(),
                None,
                0,
                "Accents",
                NodeStatus::Verified,
                "m1",
                &big,
            );
            // Must not panic on a multibyte char boundary.
            let out =
                render_deep_expertise(&[(MemoryId("m1".into()), 0.9)], &[n], &s, &cfg()).unwrap();
            let accents = out.chars().filter(|c| *c == 'é').count();
            assert_eq!(accents, DEEP_BODY_BUDGET);
        }

        #[test]
        fn caps_node_count() {
            let s = store();
            let mut nodes = Vec::new();
            let mut hits = Vec::new();
            for i in 0..6 {
                let mid = format!("m{i}");
                nodes.push(node(
                    Uuid::new_v4(),
                    None,
                    0,
                    &format!("Node {i}"),
                    NodeStatus::Verified,
                    &mid,
                    &format!("body {i}"),
                ));
                hits.push((MemoryId(mid), 0.9 - i as f32 * 0.01));
            }
            let out = render_deep_expertise(&hits, &nodes, &s, &cfg()).unwrap();
            let rendered = (0..6)
                .filter(|i| out.contains(&format!("Node {i}")))
                .count();
            assert_eq!(rendered, DEEP_MAX_NODES, "block capped to DEEP_MAX_NODES");
        }

        #[test]
        fn hit_with_no_matching_node_is_skipped() {
            let s = store();
            let n = node(
                Uuid::new_v4(),
                None,
                0,
                "Present",
                NodeStatus::Verified,
                "m2",
                "body",
            );
            let out = render_deep_expertise(&[(MemoryId("m1".into()), 0.9)], &[n], &s, &cfg());
            assert!(out.is_none());
        }
    }
}
