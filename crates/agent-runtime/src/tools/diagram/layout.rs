//! Diagram layout, server side — a port of the widget's `diagramLayout.ts`.
//!
//! Pure and deterministic: text metrics arrive as numbers, so the same input
//! always yields the same output and the two implementations can be held to a
//! shared golden corpus (`fixtures/diagrams/`). Invariants alone would not do
//! that job — two layouts can both be internally correct and still draw
//! different pictures.
//!
//! `flow` is a layered (Sugiyama-style) layout: rank, order to reduce
//! crossings, assign coordinates, route orthogonally. `sequence` needs no engine
//! — participants are columns and steps are rows. Everything is computed in an
//! abstract (main, cross) frame so `direction` is one code path and node boxes
//! are never transposed.
//!
//! Parity note: the widget measures label length in UTF-16 units (JS
//! `String.length`); this uses chars. They agree for everything in the Basic
//! Multilingual Plane — which is all report text, since emoji are normalised or
//! stripped upstream — and differ only for astral-plane characters.

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use super::spec::{Arrow, DiagramSpec, Direction, EdgeStyle, Flow, Sequence, Shape, Tone};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    pub char_width: f64,
    pub line_height: f64,
}

pub type Point = (f64, f64);

#[derive(Debug, Clone, Serialize)]
pub struct LaidOutNode {
    pub id: String,
    pub label: String,
    pub lines: Vec<String>,
    pub shape: Shape,
    pub tone: Tone,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// 1-based lane slot for the node's group; absent when it has no group or
    /// its group is past [`MAX_LANES`]. Identity only — `tone` outranks it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lane: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LaidOutEdge {
    pub key: String,
    pub from: String,
    pub to: String,
    pub points: Vec<Point>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(rename = "labelAt", skip_serializing_if = "Option::is_none")]
    pub label_at: Option<Point>,
    pub style: EdgeStyle,
    pub arrow: Arrow,
}

#[derive(Debug, Clone, Serialize)]
pub struct LaidOutGroup {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lane: Option<usize>,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LaidOutLifeline {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lane: Option<usize>,
    pub label: String,
    pub lines: Vec<String>,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub bottom: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LaidOutNote {
    pub lines: Vec<String>,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagramLayout {
    pub kind: &'static str,
    pub nodes: Vec<LaidOutNode>,
    pub edges: Vec<LaidOutEdge>,
    pub groups: Vec<LaidOutGroup>,
    pub lifelines: Vec<LaidOutLifeline>,
    pub notes: Vec<LaidOutNote>,
    pub width: f64,
    pub height: f64,
    /// Endpoints that named no declared node, reported rather than swallowed.
    pub dropped: Vec<String>,
}

const PAD: f64 = 14.0;
const NODE_PAD_X: f64 = 12.0;
const NODE_PAD_Y: f64 = 8.0;
const MIN_NODE_W: f64 = 76.0;
const WRAP_CHARS: usize = 22;
const NODE_GAP: f64 = 22.0;
const RANK_GAP: f64 = 46.0;
const DIAMOND_SCALE: f64 = 1.4;
pub const CYLINDER_CAP: f64 = 6.0;
const GROUP_PAD: f64 = 14.0;
const GROUP_LABEL_H: f64 = 18.0;
const DUMMY_SIZE: f64 = 10.0;
const LOOP_OFFSET: f64 = 24.0;
const LOOP_STEP: f64 = 10.0;
const SELF_LOOP: f64 = 22.0;
const ORDER_SWEEPS: usize = 4;
const TRANSPOSE_ROUNDS: usize = 4;
const REFINE_PASSES: usize = 2;

const MIN_COL_W: f64 = 92.0;
const COL_GAP: f64 = 34.0;
const HEAD_PAD_X: f64 = 10.0;
const HEAD_PAD_Y: f64 = 7.0;
const STEP_GAP: f64 = 44.0;
const SELF_STEP_GAP: f64 = 66.0;
const NOTE_GAP: f64 = 34.0;
const NOTE_PAD: f64 = 8.0;
const LIFELINE_TAIL: f64 = 18.0;

/// How many groups get their own hue. Beyond this they go neutral rather than
/// cycling — a repeated hue would claim two different groups are the same one.
pub const MAX_LANES: usize = 5;

/// JS `Math.round` semantics: ties break toward +∞, not away from zero. The
/// goldens come from the TypeScript, so the tie-break has to match it.
fn round_half_up(v: f64) -> f64 {
    (v + 0.5).floor()
}

fn units(s: &str) -> usize {
    s.chars().count()
}

/// Greedy word wrap, hard-breaking any word longer than the line.
pub fn wrap_label(text: &str, max_chars: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && units(&line) + 1 + units(word) <= max_chars {
            line.push(' ');
            line.push_str(word);
        } else {
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            line = word.to_string();
        }
        while units(&line) > max_chars {
            let head: String = line.chars().take(max_chars).collect();
            let rest: String = line.chars().skip(max_chars).collect();
            lines.push(head);
            line = rest;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(text.to_string());
    }
    lines
}

fn text_box(lines: &[String], m: Metrics, pad_x: f64, pad_y: f64, min_w: f64) -> (f64, f64) {
    let longest = lines.iter().map(|l| units(l)).max().unwrap_or(0) as f64;
    (
        min_w.max(round_half_up(longest * m.char_width + 2.0 * pad_x)),
        round_half_up(lines.len() as f64 * m.line_height + 2.0 * pad_y),
    )
}

fn node_box(lines: &[String], shape: Shape, m: Metrics) -> (f64, f64) {
    let (w, h) = text_box(lines, m, NODE_PAD_X, NODE_PAD_Y, MIN_NODE_W);
    match shape {
        Shape::Diamond => (
            round_half_up(w * DIAMOND_SCALE),
            round_half_up(h * DIAMOND_SCALE),
        ),
        Shape::Cylinder => (w, h + 2.0 * CYLINDER_CAP),
        _ => (w, h),
    }
}

fn median(sorted: &[f64]) -> f64 {
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[mid]
    } else {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    }
}

fn sorted_f64(mut values: Vec<f64>) -> Vec<f64> {
    values.sort_by(|a, b| a.partial_cmp(b).expect("layout never produces NaN"));
    values
}

/// Lane slots by first appearance, so adding a step never repaints the lanes
/// around it.
fn assign_lanes<'a>(names: impl Iterator<Item = Option<&'a str>>) -> HashMap<String, usize> {
    let mut lanes: HashMap<String, usize> = HashMap::new();
    for name in names.flatten() {
        if lanes.contains_key(name) || lanes.len() >= MAX_LANES {
            continue;
        }
        let next = lanes.len() + 1;
        lanes.insert(name.to_string(), next);
    }
    lanes
}

/// One laid-out box in the abstract frame — a real node or an edge's waypoint.
#[derive(Debug, Clone)]
struct Cell {
    key: String,
    rank: usize,
    main: f64,
    cross: f64,
    main_size: f64,
    cross_size: f64,
    group: Option<String>,
    /// Index into the layout's node list; `None` for a routing dummy.
    node: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Forward,
    Back,
    SelfLoop,
}

struct Routed {
    edge: usize,
    key: String,
    chain: Vec<String>,
    mode: Mode,
}

pub fn layout(spec: &DiagramSpec, m: Metrics) -> DiagramLayout {
    match spec {
        DiagramSpec::Flow(f) => flow_layout(f, m),
        DiagramSpec::Sequence(s) => sequence_layout(s, m),
    }
}

fn flow_layout(spec: &Flow, m: Metrics) -> DiagramLayout {
    let ids: Vec<&str> = spec.nodes.iter().map(|n| n.id.as_str()).collect();
    let known: HashSet<&str> = ids.iter().copied().collect();
    let mut dropped = Vec::new();
    let mut edges: Vec<usize> = Vec::new();
    for (i, e) in spec.edges.iter().enumerate() {
        // Resolution lives here, not in validation: an endpoint that names no
        // node is dropped and reported, never silently swallowed.
        if !known.contains(e.from.as_str()) || !known.contains(e.to.as_str()) {
            dropped.push(format!("{} → {}", e.from, e.to));
            continue;
        }
        edges.push(i);
    }

    let lanes = assign_lanes(spec.nodes.iter().map(|n| n.group.as_deref()));
    let mut nodes: Vec<LaidOutNode> = Vec::with_capacity(spec.nodes.len());
    let mut cells: Vec<Cell> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for n in &spec.nodes {
        let lines = wrap_label(&n.label, WRAP_CHARS);
        let (w, h) = node_box(&lines, n.shape, m);
        let vertical = spec.direction == Direction::Down;
        index.insert(n.id.clone(), cells.len());
        cells.push(Cell {
            key: n.id.clone(),
            rank: 0,
            main: 0.0,
            cross: 0.0,
            main_size: if vertical { h } else { w },
            cross_size: if vertical { w } else { h },
            group: n.group.clone(),
            node: Some(nodes.len()),
        });
        nodes.push(LaidOutNode {
            id: n.id.clone(),
            label: n.label.clone(),
            lines,
            shape: n.shape,
            tone: n.tone,
            x: 0.0,
            y: 0.0,
            w,
            h,
            group: n.group.clone(),
            lane: n.group.as_deref().and_then(|g| lanes.get(g).copied()),
        });
    }

    let linking: Vec<usize> = edges
        .iter()
        .copied()
        .filter(|&i| spec.edges[i].from != spec.edges[i].to)
        .collect();
    let back = back_edges(spec, &ids, &linking);
    let forward: Vec<usize> = linking
        .iter()
        .copied()
        .filter(|i| !back.contains(i))
        .collect();

    let mut rank = rank_nodes(spec, &ids, &forward);
    apply_hints(spec, &mut rank, &forward);
    compact_ranks(&mut rank);
    for (id, r) in &rank {
        cells[index[id]].rank = *r;
    }

    let mut routed: Vec<Routed> = Vec::new();
    for (i, &e) in edges.iter().enumerate() {
        let edge = &spec.edges[e];
        let key = format!("{}|{}|{}", edge.from, edge.to, i);
        if edge.from == edge.to {
            routed.push(Routed {
                edge: e,
                key,
                chain: vec![edge.from.clone()],
                mode: Mode::SelfLoop,
            });
        } else if back.contains(&e) {
            routed.push(Routed {
                edge: e,
                key,
                chain: vec![edge.from.clone(), edge.to.clone()],
                mode: Mode::Back,
            });
        } else {
            let span = rank[&edge.to] - rank[&edge.from];
            let mut chain = vec![edge.from.clone()];
            for step in 1..span {
                let dummy = format!("~{key}~{step}");
                index.insert(dummy.clone(), cells.len());
                cells.push(Cell {
                    key: dummy.clone(),
                    rank: rank[&edge.from] + step,
                    main: 0.0,
                    cross: 0.0,
                    main_size: DUMMY_SIZE,
                    cross_size: DUMMY_SIZE,
                    group: None,
                    node: None,
                });
                chain.push(dummy);
            }
            chain.push(edge.to.clone());
            routed.push(Routed {
                edge: e,
                key,
                chain,
                mode: Mode::Forward,
            });
        }
    }

    let chains: Vec<Vec<String>> = routed
        .iter()
        .filter(|r| r.mode == Mode::Forward)
        .map(|r| r.chain.clone())
        .collect();
    let ranks = order_ranks(&cells, &chains);
    assign_cross(&ranks, &chains, &mut cells, &index);
    assign_main(&ranks, &mut cells);

    let cross_max = place_cells(&mut cells, &mut nodes, spec.direction);
    let groups = group_boxes(&nodes);
    let laid_out_edges: Vec<LaidOutEdge> = routed
        .iter()
        .enumerate()
        .map(|(i, r)| route_edge(spec, r, &cells, &index, spec.direction, cross_max, i))
        .collect();

    normalize(DiagramLayout {
        kind: "flow",
        nodes,
        edges: laid_out_edges,
        groups,
        lifelines: Vec::new(),
        notes: Vec::new(),
        width: 0.0,
        height: 0.0,
        dropped,
    })
}

/// DFS back edges — the ones that would make ranking cyclic. Excluded from
/// ranking, still drawn with their declared direction.
fn back_edges(spec: &Flow, ids: &[&str], linking: &[usize]) -> HashSet<usize> {
    let mut out: HashMap<&str, Vec<usize>> = HashMap::new();
    for &i in linking {
        out.entry(spec.edges[i].from.as_str()).or_default().push(i);
    }
    let mut state: HashMap<&str, u8> = HashMap::new();
    let mut found = HashSet::new();
    // Iterative DFS: a deep chain must not risk the stack.
    for start in ids {
        if state.get(start).copied().unwrap_or(0) != 0 {
            continue;
        }
        let mut stack: Vec<(&str, usize)> = vec![(start, 0)];
        state.insert(start, 1);
        while let Some((node, cursor)) = stack.pop() {
            let empty = Vec::new();
            let outgoing = out.get(node).unwrap_or(&empty);
            if cursor < outgoing.len() {
                let edge = outgoing[cursor];
                stack.push((node, cursor + 1));
                let to = spec.edges[edge].to.as_str();
                match state.get(to).copied().unwrap_or(0) {
                    1 => {
                        found.insert(edge);
                    }
                    0 => {
                        state.insert(to, 1);
                        stack.push((to, 0));
                    }
                    _ => {}
                }
            } else {
                state.insert(node, 2);
            }
        }
    }
    found
}

/// Longest-path ranking over the acyclic remainder.
fn rank_nodes(spec: &Flow, ids: &[&str], forward: &[usize]) -> HashMap<String, usize> {
    let mut rank: HashMap<String, usize> = ids.iter().map(|i| (i.to_string(), 0)).collect();
    let mut indegree: HashMap<&str, usize> = ids.iter().map(|i| (*i, 0)).collect();
    let mut out: HashMap<&str, Vec<usize>> = HashMap::new();
    for &i in forward {
        out.entry(spec.edges[i].from.as_str()).or_default().push(i);
        *indegree
            .get_mut(spec.edges[i].to.as_str())
            .expect("known id") += 1;
    }
    let mut queue: Vec<&str> = ids.iter().copied().filter(|id| indegree[id] == 0).collect();
    let mut head = 0;
    while head < queue.len() {
        let id = queue[head];
        head += 1;
        let empty = Vec::new();
        for &e in out.get(id).unwrap_or(&empty) {
            let to = spec.edges[e].to.as_str();
            let candidate = rank[id] + 1;
            let entry = rank.get_mut(to).expect("known id");
            *entry = (*entry).max(candidate);
            let left = indegree.get_mut(to).expect("known id");
            *left -= 1;
            if *left == 0 {
                queue.push(to);
            }
        }
    }
    rank
}

/// Honour an `at` row hint only while it keeps every incident edge
/// rank-increasing — a hint must never invert the graph it describes.
fn apply_hints(spec: &Flow, rank: &mut HashMap<String, usize>, forward: &[usize]) {
    for n in &spec.nodes {
        let Some((_, row)) = n.at else { continue };
        let previous = rank[&n.id];
        rank.insert(n.id.clone(), row);
        let consistent = forward.iter().all(|&i| {
            let e = &spec.edges[i];
            (e.from != n.id && e.to != n.id) || rank[&e.from] < rank[&e.to]
        });
        if !consistent {
            rank.insert(n.id.clone(), previous);
        }
    }
}

/// Drop ranks left empty by a hint so the diagram has no blank bands.
fn compact_ranks(rank: &mut HashMap<String, usize>) {
    let mut used: Vec<usize> = rank.values().copied().collect();
    used.sort_unstable();
    used.dedup();
    let dense: HashMap<usize, usize> = used.iter().enumerate().map(|(i, r)| (*r, i)).collect();
    for value in rank.values_mut() {
        *value = dense[value];
    }
}

fn order_ranks(cells: &[Cell], chains: &[Vec<String>]) -> Vec<Vec<usize>> {
    let depth = cells.iter().map(|c| c.rank).max().unwrap_or(0) + 1;
    let mut ranks: Vec<Vec<usize>> = vec![Vec::new(); depth];
    for (i, c) in cells.iter().enumerate() {
        ranks[c.rank].push(i);
    }

    let mut successors: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut predecessors: HashMap<&str, Vec<&str>> = HashMap::new();
    for chain in chains {
        for pair in chain.windows(2) {
            successors
                .entry(pair[0].as_str())
                .or_default()
                .push(pair[1].as_str());
            predecessors
                .entry(pair[1].as_str())
                .or_default()
                .push(pair[0].as_str());
        }
    }

    for sweep in 0..ORDER_SWEEPS {
        let down = sweep % 2 == 0;
        let order: Vec<usize> = if down {
            (1..depth).collect()
        } else {
            (0..depth.saturating_sub(1)).rev().collect()
        };
        for r in order {
            let fixed_rank = if down { r - 1 } else { r + 1 };
            let fixed: HashMap<&str, usize> = ranks[fixed_rank]
                .iter()
                .enumerate()
                .map(|(i, &c)| (cells[c].key.as_str(), i))
                .collect();
            let neighbours = if down { &predecessors } else { &successors };
            ranks[r] = by_median(&ranks[r], cells, &fixed, neighbours);
            ranks[r] = transpose(&ranks[r], cells, &fixed, neighbours);
        }
    }
    ranks
        .into_iter()
        .map(|rank| compact_groups(&rank, cells))
        .collect()
}

fn by_median(
    rank: &[usize],
    cells: &[Cell],
    fixed: &HashMap<&str, usize>,
    neighbours: &HashMap<&str, Vec<&str>>,
) -> Vec<usize> {
    let mut keyed: Vec<(usize, f64)> = rank
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            let positions = sorted_f64(
                neighbours
                    .get(cells[c].key.as_str())
                    .map(|ns| {
                        ns.iter()
                            .filter_map(|k| fixed.get(k).map(|p| *p as f64))
                            .collect()
                    })
                    .unwrap_or_default(),
            );
            // A cell with no neighbour in the fixed rank keeps its current slot.
            let m = if positions.is_empty() {
                i as f64
            } else {
                median(&positions)
            };
            (c, m)
        })
        .collect();
    keyed.sort_by(|a, b| a.1.partial_cmp(&b.1).expect("finite medians"));
    keyed.into_iter().map(|(c, _)| c).collect()
}

fn crossings(
    rank: &[usize],
    cells: &[Cell],
    fixed: &HashMap<&str, usize>,
    neighbours: &HashMap<&str, Vec<&str>>,
) -> usize {
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for (i, &c) in rank.iter().enumerate() {
        if let Some(ns) = neighbours.get(cells[c].key.as_str()) {
            for k in ns {
                if let Some(&p) = fixed.get(k) {
                    pairs.push((i, p));
                }
            }
        }
    }
    let mut total = 0;
    for i in 0..pairs.len() {
        for j in i + 1..pairs.len() {
            let ((ai, ap), (bi, bp)) = (pairs[i], pairs[j]);
            if (ai < bi && ap > bp) || (ai > bi && ap < bp) {
                total += 1;
            }
        }
    }
    total
}

/// Adjacent-swap improvement — breaks the ties the median heuristic leaves.
fn transpose(
    rank: &[usize],
    cells: &[Cell],
    fixed: &HashMap<&str, usize>,
    neighbours: &HashMap<&str, Vec<&str>>,
) -> Vec<usize> {
    let mut best = rank.to_vec();
    let mut best_score = crossings(&best, cells, fixed, neighbours);
    for _ in 0..TRANSPOSE_ROUNDS {
        if best_score == 0 {
            break;
        }
        let mut improved = false;
        for i in 0..best.len().saturating_sub(1) {
            let mut swapped = best.clone();
            swapped.swap(i, i + 1);
            let score = crossings(&swapped, cells, fixed, neighbours);
            if score < best_score {
                best = swapped;
                best_score = score;
                improved = true;
            }
        }
        if !improved {
            break;
        }
    }
    best
}

/// Make same-group cells contiguous, anchored at the group's first member so
/// the crossing-minimised order survives as far as possible.
fn compact_groups(rank: &[usize], cells: &[Cell]) -> Vec<usize> {
    let mut out = Vec::with_capacity(rank.len());
    let mut taken = vec![false; rank.len()];
    for i in 0..rank.len() {
        if taken[i] {
            continue;
        }
        out.push(rank[i]);
        taken[i] = true;
        let Some(group) = cells[rank[i]].group.clone() else {
            continue;
        };
        for j in i + 1..rank.len() {
            if !taken[j] && cells[rank[j]].group.as_deref() == Some(group.as_str()) {
                out.push(rank[j]);
                taken[j] = true;
            }
        }
    }
    out
}

fn gap_between(a: &Cell, b: &Cell) -> f64 {
    let differs = a.group != b.group && (a.group.is_some() || b.group.is_some());
    NODE_GAP + if differs { 2.0 * GROUP_PAD } else { 0.0 }
}

fn assign_cross(
    ranks: &[Vec<usize>],
    chains: &[Vec<String>],
    cells: &mut [Cell],
    index: &HashMap<String, usize>,
) {
    for rank in ranks {
        let mut cursor = 0.0;
        for (i, &c) in rank.iter().enumerate() {
            if i > 0 {
                cursor += gap_between(&cells[rank[i - 1]], &cells[c]);
            }
            cells[c].cross = cursor + cells[c].cross_size / 2.0;
            cursor += cells[c].cross_size;
        }
    }

    let mut linked: HashMap<String, Vec<String>> = HashMap::new();
    for chain in chains {
        for pair in chain.windows(2) {
            linked
                .entry(pair[0].clone())
                .or_default()
                .push(pair[1].clone());
            linked
                .entry(pair[1].clone())
                .or_default()
                .push(pair[0].clone());
        }
    }

    // Pull each cell toward its neighbours' median, clamped so the rank's order
    // and gaps still hold. Straightens long chains without reshuffling anything.
    for _ in 0..REFINE_PASSES {
        for rank in ranks {
            for (i, &c) in rank.iter().enumerate() {
                let xs = sorted_f64(
                    linked
                        .get(&cells[c].key)
                        .map(|ns| {
                            ns.iter()
                                .filter_map(|k| index.get(k).map(|&j| cells[j].cross))
                                .collect()
                        })
                        .unwrap_or_default(),
                );
                if xs.is_empty() {
                    continue;
                }
                let low = if i > 0 {
                    let p = &cells[rank[i - 1]];
                    p.cross
                        + p.cross_size / 2.0
                        + gap_between(p, &cells[c])
                        + cells[c].cross_size / 2.0
                } else {
                    f64::NEG_INFINITY
                };
                let high = if i + 1 < rank.len() {
                    let n = &cells[rank[i + 1]];
                    n.cross
                        - n.cross_size / 2.0
                        - gap_between(&cells[c], n)
                        - cells[c].cross_size / 2.0
                } else {
                    f64::INFINITY
                };
                if low > high {
                    continue;
                }
                cells[c].cross = median(&xs).max(low).min(high);
            }
        }
    }
}

fn assign_main(ranks: &[Vec<usize>], cells: &mut [Cell]) {
    let mut cursor = 0.0;
    for rank in ranks {
        let band = rank.iter().fold(0.0_f64, |a, &c| a.max(cells[c].main_size));
        for &c in rank {
            cells[c].main = cursor + band / 2.0;
        }
        cursor += band + RANK_GAP;
    }
}

fn to_xy(main: f64, cross: f64, direction: Direction) -> Point {
    match direction {
        Direction::Down => (cross, main),
        Direction::Right => (main, cross),
    }
}

/// Project the abstract frame onto x/y and write it back into the node boxes.
fn place_cells(cells: &mut [Cell], nodes: &mut [LaidOutNode], direction: Direction) -> f64 {
    let mut cross_max = 0.0_f64;
    for cell in cells.iter() {
        cross_max = cross_max.max(cell.cross + cell.cross_size / 2.0);
        let Some(n) = cell.node else { continue };
        let (x, y) = to_xy(cell.main, cell.cross, direction);
        nodes[n].x = x - nodes[n].w / 2.0;
        nodes[n].y = y - nodes[n].h / 2.0;
    }
    cross_max
}

fn group_boxes(nodes: &[LaidOutNode]) -> Vec<LaidOutGroup> {
    let mut names: Vec<&str> = Vec::new();
    for n in nodes {
        if let Some(g) = n.group.as_deref() {
            if !names.contains(&g) {
                names.push(g);
            }
        }
    }
    let boxes: Vec<LaidOutGroup> = names
        .into_iter()
        .map(|label| {
            let members: Vec<&LaidOutNode> = nodes
                .iter()
                .filter(|n| n.group.as_deref() == Some(label))
                .collect();
            let x = members.iter().fold(f64::INFINITY, |a, n| a.min(n.x)) - GROUP_PAD;
            let y =
                members.iter().fold(f64::INFINITY, |a, n| a.min(n.y)) - GROUP_PAD - GROUP_LABEL_H;
            let right = members
                .iter()
                .fold(f64::NEG_INFINITY, |a, n| a.max(n.x + n.w))
                + GROUP_PAD;
            let bottom = members
                .iter()
                .fold(f64::NEG_INFINITY, |a, n| a.max(n.y + n.h))
                + GROUP_PAD;
            LaidOutGroup {
                label: label.to_string(),
                lane: members[0].lane,
                x,
                y,
                w: right - x,
                h: bottom - y,
            }
        })
        .collect();
    // An overlapping cluster box is worse than none, so they go all or nothing.
    let overlap = boxes.iter().enumerate().any(|(i, a)| {
        boxes
            .iter()
            .skip(i + 1)
            .any(|b| a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h)
    });
    if overlap {
        Vec::new()
    } else {
        boxes
    }
}

#[allow(clippy::too_many_arguments)]
fn route_edge(
    spec: &Flow,
    routed: &Routed,
    cells: &[Cell],
    index: &HashMap<String, usize>,
    direction: Direction,
    cross_max: f64,
    position: usize,
) -> LaidOutEdge {
    let edge = &spec.edges[routed.edge];
    let chain: Vec<&Cell> = routed.chain.iter().map(|k| &cells[index[k]]).collect();
    let mut points: Vec<Point> = Vec::new();

    match routed.mode {
        Mode::SelfLoop => {
            let c = chain[0];
            let far = c.cross + c.cross_size / 2.0;
            let back = c.main - c.main_size / 4.0;
            let front = c.main + c.main_size / 4.0;
            points.push(to_xy(back, far, direction));
            points.push(to_xy(back, far + SELF_LOOP, direction));
            points.push(to_xy(front, far + SELF_LOOP, direction));
            points.push(to_xy(front, far, direction));
        }
        Mode::Back => {
            // Route around the outside so a loop never crosses the boxes it links.
            let (a, b) = (chain[0], chain[1]);
            let lane = cross_max + LOOP_OFFSET + position as f64 * LOOP_STEP;
            points.push(to_xy(a.main, a.cross + a.cross_size / 2.0, direction));
            points.push(to_xy(a.main, lane, direction));
            points.push(to_xy(b.main, lane, direction));
            points.push(to_xy(b.main, b.cross + b.cross_size / 2.0, direction));
        }
        Mode::Forward => {
            for i in 0..chain.len() - 1 {
                let (a, b) = (chain[i], chain[i + 1]);
                let exit = a.main + a.main_size / 2.0;
                let enter = b.main - b.main_size / 2.0;
                let bend = (exit + enter) / 2.0;
                if i == 0 {
                    points.push(to_xy(exit, a.cross, direction));
                }
                points.push(to_xy(bend, a.cross, direction));
                points.push(to_xy(bend, b.cross, direction));
                points.push(to_xy(enter, b.cross, direction));
            }
        }
    }

    let simplified = simplify(points);
    LaidOutEdge {
        key: routed.key.clone(),
        from: edge.from.clone(),
        to: edge.to.clone(),
        label_at: edge
            .label
            .as_ref()
            .and_then(|_| longest_midpoint(&simplified)),
        points: simplified,
        label: edge.label.clone(),
        style: edge.style,
        arrow: edge.arrow,
    }
}

/// Drop duplicate and collinear waypoints so the path is the shortest polyline
/// that still describes the same route.
fn simplify(points: Vec<Point>) -> Vec<Point> {
    let mut out: Vec<Point> = Vec::with_capacity(points.len());
    for p in points {
        if let Some(&last) = out.last() {
            if last == p {
                continue;
            }
            if out.len() >= 2 {
                let prior = out[out.len() - 2];
                if (prior.0 == last.0 && last.0 == p.0) || (prior.1 == last.1 && last.1 == p.1) {
                    let end = out.len() - 1;
                    out[end] = p;
                    continue;
                }
            }
        }
        out.push(p);
    }
    out
}

fn longest_midpoint(points: &[Point]) -> Option<Point> {
    let mut best = None;
    let mut best_length = -1.0;
    for pair in points.windows(2) {
        let ((ax, ay), (bx, by)) = (pair[0], pair[1]);
        let length = (bx - ax).abs() + (by - ay).abs();
        if length > best_length {
            best_length = length;
            best = Some(((ax + bx) / 2.0, (ay + by) / 2.0));
        }
    }
    best
}

fn sequence_layout(spec: &Sequence, m: Metrics) -> DiagramLayout {
    let known: HashSet<&str> = spec.participants.iter().map(|p| p.id.as_str()).collect();
    let mut dropped = Vec::new();

    let lanes = assign_lanes(spec.participants.iter().map(|p| Some(p.id.as_str())));
    let mut lifelines: Vec<LaidOutLifeline> = Vec::new();
    let mut cursor = 0.0;
    for p in &spec.participants {
        let lines = wrap_label(&p.label, WRAP_CHARS);
        let (w, h) = text_box(&lines, m, HEAD_PAD_X, HEAD_PAD_Y, MIN_COL_W);
        lifelines.push(LaidOutLifeline {
            id: p.id.clone(),
            lane: lanes.get(&p.id).copied(),
            label: p.label.clone(),
            lines,
            x: cursor,
            y: 0.0,
            w,
            h,
            bottom: 0.0,
        });
        cursor += w + COL_GAP;
    }
    let centre: HashMap<&str, f64> = lifelines
        .iter()
        .map(|l| (l.id.as_str(), l.x + l.w / 2.0))
        .collect();
    let head_bottom = lifelines.iter().fold(0.0_f64, |a, l| a.max(l.h));

    let mut edges: Vec<LaidOutEdge> = Vec::new();
    let mut notes: Vec<LaidOutNote> = Vec::new();
    let mut y = head_bottom;
    for (i, step) in spec.steps.iter().enumerate() {
        if !known.contains(step.from.as_str()) || !known.contains(step.to.as_str()) {
            dropped.push(format!("{} → {}", step.from, step.to));
            continue;
        }
        let is_self = step.from == step.to;
        y += if is_self { SELF_STEP_GAP } else { STEP_GAP };
        let from = centre[step.from.as_str()];
        let to = centre[step.to.as_str()];
        let points: Vec<Point> = if is_self {
            vec![
                (from, y - SELF_STEP_GAP / 3.0),
                (from + SELF_LOOP, y - SELF_STEP_GAP / 3.0),
                (from + SELF_LOOP, y),
                (from, y),
            ]
        } else {
            vec![(from, y), (to, y)]
        };
        edges.push(LaidOutEdge {
            key: format!("{}|{}|{}", step.from, step.to, i),
            from: step.from.clone(),
            to: step.to.clone(),
            label_at: step.label.as_ref().and_then(|_| longest_midpoint(&points)),
            points,
            label: step.label.clone(),
            style: step.style,
            arrow: Arrow::To,
        });
        if let Some(note) = &step.note {
            let lines = wrap_label(note, WRAP_CHARS * 2);
            let (w, h) = text_box(&lines, m, NOTE_PAD, NOTE_PAD, MIN_COL_W);
            let top = y + NOTE_GAP / 3.0;
            notes.push(LaidOutNote {
                lines,
                x: from.min(to),
                y: top,
                w,
                h,
            });
            // Reserve the note's real height — a fixed gap would let a tall note
            // collide with the next message.
            y = top + h;
        }
    }

    let bottom = y + LIFELINE_TAIL;
    for l in &mut lifelines {
        l.bottom = bottom;
    }

    normalize(DiagramLayout {
        kind: "sequence",
        nodes: Vec::new(),
        edges,
        groups: Vec::new(),
        lifelines,
        notes,
        width: 0.0,
        height: 0.0,
        dropped,
    })
}

/// Translate everything to a `PAD` margin and record the extents.
fn normalize(mut layout: DiagramLayout) -> DiagramLayout {
    let mut xs: Vec<f64> = Vec::new();
    let mut ys: Vec<f64> = Vec::new();
    let mut track = |x: f64, y: f64, w: f64, h: f64| {
        xs.push(x);
        xs.push(x + w);
        ys.push(y);
        ys.push(y + h);
    };
    for n in &layout.nodes {
        track(n.x, n.y, n.w, n.h);
    }
    for g in &layout.groups {
        track(g.x, g.y, g.w, g.h);
    }
    for l in &layout.lifelines {
        track(l.x, l.y, l.w, l.h);
        track(l.x, l.bottom, 0.0, 0.0);
    }
    for n in &layout.notes {
        track(n.x, n.y, n.w, n.h);
    }
    for e in &layout.edges {
        for &(x, y) in &e.points {
            track(x, y, 0.0, 0.0);
        }
    }
    if xs.is_empty() {
        layout.width = 2.0 * PAD;
        layout.height = 2.0 * PAD;
        return layout;
    }

    let min_x = xs.iter().copied().fold(f64::INFINITY, f64::min);
    let max_x = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let min_y = ys.iter().copied().fold(f64::INFINITY, f64::min);
    let max_y = ys.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let dx = PAD - min_x;
    let dy = PAD - min_y;
    for n in &mut layout.nodes {
        n.x += dx;
        n.y += dy;
    }
    for g in &mut layout.groups {
        g.x += dx;
        g.y += dy;
    }
    for n in &mut layout.notes {
        n.x += dx;
        n.y += dy;
    }
    for l in &mut layout.lifelines {
        l.x += dx;
        l.y += dy;
        l.bottom += dy;
    }
    for e in &mut layout.edges {
        for p in &mut e.points {
            *p = (p.0 + dx, p.1 + dy);
        }
        if let Some(p) = &mut e.label_at {
            *p = (p.0 + dx, p.1 + dy);
        }
    }
    layout.width = round_half_up(max_x - min_x + 2.0 * PAD);
    layout.height = round_half_up(max_y - min_y + 2.0 * PAD);
    layout
}

/// A one-line description for `aria-label` and alt text — what the diagram is
/// and its spine.
pub fn aria_label(spec: &DiagramSpec, layout: &DiagramLayout) -> String {
    let what = match spec {
        DiagramSpec::Flow(_) => "Flow diagram",
        DiagramSpec::Sequence(_) => "Sequence diagram",
    };
    let title = spec.title().map(|t| format!(": {t}")).unwrap_or_default();
    let size = match spec {
        DiagramSpec::Flow(_) => format!(
            "{} {}",
            layout.nodes.len(),
            if layout.nodes.len() == 1 {
                "step"
            } else {
                "steps"
            }
        ),
        DiagramSpec::Sequence(_) => format!(
            "{} participants, {} messages",
            layout.lifelines.len(),
            layout.edges.len()
        ),
    };
    format!("{what}{title}; {size}")
}

/// The diagram as sentences — the exact, legible view a picture cannot give.
pub fn outline(layout: &DiagramLayout) -> Vec<String> {
    let mut label: HashMap<&str, &str> = HashMap::new();
    for n in &layout.nodes {
        label.insert(&n.id, &n.label);
    }
    for l in &layout.lifelines {
        label.insert(&l.id, &l.label);
    }
    layout
        .edges
        .iter()
        .map(|e| {
            let named = |id: &str| label.get(id).copied().unwrap_or(id).to_string();
            let arrow = if e.arrow == Arrow::Both { "↔" } else { "→" };
            let suffix = e
                .label
                .as_ref()
                .map(|l| format!(" ({l})"))
                .unwrap_or_default();
            format!("{} {arrow} {}{suffix}", named(&e.from), named(&e.to))
        })
        .collect()
}
