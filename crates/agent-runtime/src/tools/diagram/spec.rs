//! The `wf-diagram` wire contract, server side.
//!
//! A deliberate line-for-line port of the widget's `spec.ts`: the same shapes,
//! the same caps, the same lenient-parse/strict-validate discipline, and the
//! same split where validation checks the SHAPE of an id and layout resolves it
//! (reporting what it could not place). A spec that renders in the widget must
//! render in a report, so the two are held together by a golden corpus rather
//! than by good intentions — see `fixtures/diagrams/`.
//!
//! Validation walks `serde_json::Value` rather than deriving `Deserialize`
//! because the contract needs per-field fallbacks (an unknown `shape` becomes a
//! box, it does not fail the spec) that serde would fight.

use serde::Serialize;
use serde_json::Value;

pub const MAX_NODES: usize = 60;
pub const MAX_EDGES: usize = 120;
pub const MAX_PARTICIPANTS: usize = 12;
pub const MAX_STEPS: usize = 60;
pub const MAX_LABEL: usize = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Shape {
    Box,
    Round,
    Diamond,
    Cylinder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    Default,
    Accent,
    Good,
    Warn,
    Bad,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Style {
    Clean,
    Sketch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Down,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeStyle {
    Solid,
    Dashed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Arrow {
    To,
    Both,
    None,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub id: String,
    pub label: String,
    pub shape: Shape,
    pub tone: Tone,
    pub group: Option<String>,
    /// `[col, row]` placement hint, honoured only while it keeps every incident
    /// edge rank-increasing. Never pixels.
    pub at: Option<(usize, usize)>,
}

#[derive(Debug, Clone)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub label: Option<String>,
    pub style: EdgeStyle,
    pub arrow: Arrow,
}

#[derive(Debug, Clone)]
pub struct Flow {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub title: Option<String>,
    pub direction: Direction,
    pub style: Style,
}

#[derive(Debug, Clone)]
pub struct Participant {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct Step {
    pub from: String,
    pub to: String,
    pub label: Option<String>,
    pub style: EdgeStyle,
    pub note: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Sequence {
    pub participants: Vec<Participant>,
    pub steps: Vec<Step>,
    pub title: Option<String>,
    pub style: Style,
}

#[derive(Debug, Clone)]
pub enum DiagramSpec {
    Flow(Flow),
    Sequence(Sequence),
}

impl DiagramSpec {
    pub fn title(&self) -> Option<&str> {
        match self {
            Self::Flow(f) => f.title.as_deref(),
            Self::Sequence(s) => s.title.as_deref(),
        }
    }
}

/// A trimmed, length-capped label; `None` when there is no usable text. An
/// over-long label is truncated rather than rejected — the diagram is still
/// worth drawing.
fn label(v: Option<&Value>) -> Option<String> {
    let text = v?.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    Some(match text.chars().count() > MAX_LABEL {
        true => text.chars().take(MAX_LABEL - 1).collect::<String>() + "…",
        false => text.to_string(),
    })
}

fn id(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// Map a string field onto a known value, falling back rather than failing —
/// the widget does the same, so an unrecognised style never costs the diagram.
fn one_of<T: Copy>(v: Option<&Value>, allowed: &[(&str, T)], fallback: T) -> T {
    v.and_then(Value::as_str)
        .and_then(|s| allowed.iter().find(|(name, _)| *name == s))
        .map_or(fallback, |(_, value)| *value)
}

fn grid_hint(v: Option<&Value>) -> Option<(usize, usize)> {
    let pair = v?.as_array()?;
    if pair.len() != 2 {
        return None;
    }
    let cell = |i: usize| -> Option<usize> {
        let n = pair[i].as_f64()?;
        (n >= 0.0 && n.fract() == 0.0).then_some(n as usize)
    };
    Some((cell(0)?, cell(1)?))
}

const SHAPES: &[(&str, Shape)] = &[
    ("box", Shape::Box),
    ("round", Shape::Round),
    ("diamond", Shape::Diamond),
    ("cylinder", Shape::Cylinder),
];
const TONES: &[(&str, Tone)] = &[
    ("default", Tone::Default),
    ("accent", Tone::Accent),
    ("good", Tone::Good),
    ("warn", Tone::Warn),
    ("bad", Tone::Bad),
];
const STYLES: &[(&str, Style)] = &[("clean", Style::Clean), ("sketch", Style::Sketch)];
const DIRECTIONS: &[(&str, Direction)] = &[("down", Direction::Down), ("right", Direction::Right)];
const EDGE_STYLES: &[(&str, EdgeStyle)] =
    &[("solid", EdgeStyle::Solid), ("dashed", EdgeStyle::Dashed)];
const ARROWS: &[(&str, Arrow)] = &[
    ("to", Arrow::To),
    ("both", Arrow::Both),
    ("none", Arrow::None),
];

/// Parse and validate a diagram spec. `Err` carries the same messages the widget
/// produces, so a spec rejected here is rejected there for the same reason.
pub fn validate(value: &Value) -> Result<DiagramSpec, String> {
    let object = value
        .as_object()
        .ok_or("payload must be a JSON object".to_string())?;
    match object.get("kind").and_then(Value::as_str) {
        Some("flow") => validate_flow(value).map(DiagramSpec::Flow),
        Some("sequence") => validate_sequence(value).map(DiagramSpec::Sequence),
        _ => Err(r#"kind must be "flow" or "sequence""#.into()),
    }
}

fn validate_flow(value: &Value) -> Result<Flow, String> {
    let raw_nodes = value
        .get("nodes")
        .and_then(Value::as_array)
        .filter(|n| !n.is_empty())
        .ok_or("nodes must be a non-empty array")?;
    if raw_nodes.len() > MAX_NODES {
        return Err(format!("too many nodes (max {MAX_NODES})"));
    }
    let mut nodes = Vec::with_capacity(raw_nodes.len());
    let mut seen: Vec<String> = Vec::new();
    for (i, raw) in raw_nodes.iter().enumerate() {
        if !raw.is_object() {
            return Err(format!("node {} must be an object", i + 1));
        }
        let node_id = id(raw.get("id"));
        if node_id.is_empty() {
            return Err(format!("node {} needs an id", i + 1));
        }
        let node_label =
            label(raw.get("label")).ok_or(format!("node \"{node_id}\" needs a label"))?;
        // Merging a duplicate would silently draw a different process than the
        // one described, so it fails the whole spec.
        if seen.contains(&node_id) {
            return Err(format!("duplicate node id \"{node_id}\""));
        }
        seen.push(node_id.clone());
        nodes.push(Node {
            id: node_id,
            label: node_label,
            shape: one_of(raw.get("shape"), SHAPES, Shape::Box),
            tone: one_of(raw.get("tone"), TONES, Tone::Default),
            group: label(raw.get("group")),
            at: grid_hint(raw.get("at")),
        });
    }

    let raw_edges = match value.get("edges") {
        None | Some(Value::Null) => &Vec::new(),
        Some(Value::Array(a)) => a,
        Some(_) => return Err("edges must be an array".into()),
    };
    if raw_edges.len() > MAX_EDGES {
        return Err(format!("too many edges (max {MAX_EDGES})"));
    }
    let mut edges = Vec::with_capacity(raw_edges.len());
    for (i, raw) in raw_edges.iter().enumerate() {
        if !raw.is_object() {
            return Err(format!("edge {} must be an object", i + 1));
        }
        let (from, to) = (id(raw.get("from")), id(raw.get("to")));
        if from.is_empty() || to.is_empty() {
            return Err(format!("edge {} needs from and to", i + 1));
        }
        edges.push(Edge {
            from,
            to,
            label: label(raw.get("label")),
            style: one_of(raw.get("style"), EDGE_STYLES, EdgeStyle::Solid),
            arrow: one_of(raw.get("arrow"), ARROWS, Arrow::To),
        });
    }

    Ok(Flow {
        nodes,
        edges,
        title: label(value.get("title")),
        direction: one_of(value.get("direction"), DIRECTIONS, Direction::Down),
        style: one_of(value.get("style"), STYLES, Style::Clean),
    })
}

fn validate_sequence(value: &Value) -> Result<Sequence, String> {
    let raw_people = value
        .get("participants")
        .and_then(Value::as_array)
        .filter(|p| p.len() >= 2)
        .ok_or("a sequence needs at least 2 participants")?;
    if raw_people.len() > MAX_PARTICIPANTS {
        return Err(format!("too many participants (max {MAX_PARTICIPANTS})"));
    }
    let mut participants = Vec::with_capacity(raw_people.len());
    let mut seen: Vec<String> = Vec::new();
    for (i, raw) in raw_people.iter().enumerate() {
        if !raw.is_object() {
            return Err(format!("participant {} must be an object", i + 1));
        }
        let person = id(raw.get("id"));
        if person.is_empty() {
            return Err(format!("participant {} needs an id", i + 1));
        }
        if seen.contains(&person) {
            return Err(format!("duplicate participant id \"{person}\""));
        }
        seen.push(person.clone());
        participants.push(Participant {
            label: label(raw.get("label")).unwrap_or_else(|| person.clone()),
            id: person,
        });
    }

    let raw_steps = value
        .get("steps")
        .and_then(Value::as_array)
        .filter(|s| !s.is_empty())
        .ok_or("steps must be a non-empty array")?;
    if raw_steps.len() > MAX_STEPS {
        return Err(format!("too many steps (max {MAX_STEPS})"));
    }
    let mut steps = Vec::with_capacity(raw_steps.len());
    for (i, raw) in raw_steps.iter().enumerate() {
        if !raw.is_object() {
            return Err(format!("step {} must be an object", i + 1));
        }
        let (from, to) = (id(raw.get("from")), id(raw.get("to")));
        if from.is_empty() || to.is_empty() {
            return Err(format!("step {} needs from and to", i + 1));
        }
        steps.push(Step {
            from,
            to,
            label: label(raw.get("label")),
            style: one_of(raw.get("style"), EDGE_STYLES, EdgeStyle::Solid),
            note: label(raw.get("note")),
        });
    }

    Ok(Sequence {
        participants,
        steps,
        title: label(value.get("title")),
        style: one_of(value.get("style"), STYLES, Style::Clean),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn flow(payload: Value) -> Result<Flow, String> {
        let mut object = payload;
        object["kind"] = json!("flow");
        match validate(&object)? {
            DiagramSpec::Flow(f) => Ok(f),
            DiagramSpec::Sequence(_) => unreachable!(),
        }
    }

    #[test]
    fn normalizes_every_optional_to_a_concrete_value() {
        let spec = flow(json!({
            "nodes": [{ "id": "a", "label": "Start" }],
            "edges": [{ "from": "a", "to": "b" }],
        }))
        .unwrap();
        assert_eq!(spec.direction, Direction::Down);
        assert_eq!(spec.style, Style::Clean);
        assert_eq!(spec.nodes[0].shape, Shape::Box);
        assert_eq!(spec.nodes[0].tone, Tone::Default);
        assert_eq!(spec.edges[0].style, EdgeStyle::Solid);
        assert_eq!(spec.edges[0].arrow, Arrow::To);
    }

    #[test]
    fn rejects_a_payload_that_is_not_a_known_diagram() {
        assert!(validate(&json!("nope")).is_err());
        assert!(validate(&json!({ "kind": "mindmap" })).is_err());
        assert!(flow(json!({ "nodes": [] })).is_err());
        assert!(flow(json!({ "nodes": [{ "id": "a" }] })).is_err());
        assert!(flow(json!({ "nodes": [{ "label": "no id" }] })).is_err());
    }

    #[test]
    fn fails_a_duplicate_node_id_rather_than_merging_it() {
        let err = flow(json!({
            "nodes": [{ "id": "a", "label": "One" }, { "id": "a", "label": "Two" }],
        }))
        .unwrap_err();
        assert!(err.contains("duplicate"), "{err}");
    }

    /// Resolution belongs to layout, which reports what it dropped.
    #[test]
    fn accepts_an_edge_naming_no_declared_node() {
        let spec = flow(json!({
            "nodes": [{ "id": "a", "label": "A" }],
            "edges": [{ "from": "a", "to": "ghost" }],
        }))
        .unwrap();
        assert_eq!(spec.edges.len(), 1);
    }

    #[test]
    fn falls_back_on_unknown_enum_values_instead_of_failing() {
        let spec = flow(json!({
            "nodes": [{ "id": "a", "label": "A", "shape": "hexagon", "tone": "neon" }],
            "edges": [{ "from": "a", "to": "a", "style": "wiggly", "arrow": "sideways" }],
            "direction": "sideways",
            "style": "watercolour",
        }))
        .unwrap();
        assert_eq!(spec.nodes[0].shape, Shape::Box);
        assert_eq!(spec.nodes[0].tone, Tone::Default);
        assert_eq!(spec.edges[0].style, EdgeStyle::Solid);
        assert_eq!(spec.edges[0].arrow, Arrow::To);
        assert_eq!(spec.direction, Direction::Down);
        assert_eq!(spec.style, Style::Clean);
    }

    #[test]
    fn keeps_only_a_well_formed_grid_hint() {
        let spec = flow(json!({
            "nodes": [
                { "id": "a", "label": "A", "at": [1, 2] },
                { "id": "b", "label": "B", "at": [0.5, 2] },
                { "id": "c", "label": "C", "at": [-1, 0] },
                { "id": "d", "label": "D", "at": "middle" },
            ],
        }))
        .unwrap();
        let hints: Vec<_> = spec.nodes.iter().map(|n| n.at).collect();
        assert_eq!(hints, vec![Some((1, 2)), None, None, None]);
    }

    #[test]
    fn truncates_an_over_long_label_rather_than_rejecting_it() {
        let spec = flow(json!({
            "nodes": [{ "id": "a", "label": "x".repeat(MAX_LABEL + 40) }],
        }))
        .unwrap();
        assert_eq!(spec.nodes[0].label.chars().count(), MAX_LABEL);
        assert!(spec.nodes[0].label.ends_with('…'));
    }

    #[test]
    fn refuses_input_past_the_size_caps() {
        let many = |n: usize| -> Value {
            (0..n)
                .map(|i| json!({ "id": format!("n{i}"), "label": format!("N{i}") }))
                .collect()
        };
        assert!(flow(json!({ "nodes": many(MAX_NODES + 1) })).is_err());
        assert!(flow(json!({
            "nodes": many(2),
            "edges": (0..MAX_EDGES + 1).map(|_| json!({ "from": "n0", "to": "n1" })).collect::<Value>(),
        }))
        .is_err());
        assert!(validate(&json!({
            "kind": "sequence",
            "participants": [{ "id": "a", "label": "A" }],
            "steps": [{ "from": "a", "to": "a" }],
        }))
        .is_err());
        assert!(validate(&json!({
            "kind": "sequence",
            "participants": many(MAX_PARTICIPANTS + 1),
            "steps": [{ "from": "n0", "to": "n1" }],
        }))
        .is_err());
    }

    #[test]
    fn accepts_a_sequence() {
        let DiagramSpec::Sequence(spec) = validate(&json!({
            "kind": "sequence",
            "participants": [{ "id": "c", "label": "Customer" }, { "id": "a", "label": "Agent" }],
            "steps": [{ "from": "c", "to": "a", "label": "Asks", "style": "dashed" }],
        }))
        .unwrap() else {
            panic!("expected a sequence");
        };
        assert_eq!(spec.participants.len(), 2);
        assert_eq!(spec.steps[0].style, EdgeStyle::Dashed);
    }
}
