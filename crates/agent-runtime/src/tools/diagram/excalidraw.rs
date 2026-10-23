//! A laid-out diagram as an `.excalidraw` scene — a port of the widget's
//! `excalidraw.ts`, carrying the same decisions:
//!
//! * Every base field is emitted, so the scene never leans on import defaults.
//! * Arrows carry explicit `points` and NULL bindings. The binding shape has
//!   churned across versions (`focus`/`gap` → `fixedPoint`/`mode`), and a
//!   binding would let the app re-route connectors we have already routed.
//! * Fractional `index` is omitted — Excalidraw regenerates it, and a
//!   hand-authored sequence would be a guess.
//! * Colours are literal and always the LIGHT lane set: the file lands on a
//!   white ground regardless of any theme.
//!
//! `roughness: 1` is what makes this the hand-drawn answer — the sketch skin is
//! not ported server-side, so an agent asked for a whiteboard look hands over
//! this file and lets Excalidraw draw it.

use serde_json::{json, Map, Value};

use super::layout::{DiagramLayout, LaidOutNode};
use super::spec::Tone;
use super::svg::LANE_HUES;

const FONT_HAND_DRAWN: u8 = 1;
const LINE_HEIGHT: f64 = 1.25;
const FONT_SIZE: f64 = 16.0;
const GROUP_FONT_SIZE: f64 = 14.0;

const INK: &str = "#1e1e1e";
const MUTED: &str = "#5c5c5c";

fn tone_paint(tone: Tone) -> Option<(&'static str, &'static str)> {
    match tone {
        Tone::Default => None,
        Tone::Accent => Some(("#2a78d6", "#e7f0fc")),
        Tone::Good => Some(("#1baf7a", "#e3f7f0")),
        Tone::Warn => Some(("#eda100", "#fdf3dd")),
        Tone::Bad => Some(("#e34948", "#fbe8e8")),
    }
}

/// FNV-1a — the same stable seed the widget derives, so a diagram exported from
/// either side carries matching element seeds.
fn stable_seed(key: &str) -> u32 {
    let mut h: u32 = 2_166_136_261;
    for b in key.encode_utf16() {
        h ^= b as u32;
        h = h.wrapping_mul(16_777_619);
    }
    h
}

fn round(n: f64) -> f64 {
    (n * 100.0).round() / 100.0
}

/// A 12% tint over white — the export's own ground.
fn tint(hue: &str) -> String {
    let channel = |i: usize| -> u8 {
        let v = u8::from_str_radix(&hue[1 + i * 2..3 + i * 2], 16).unwrap_or(0) as f64;
        (v * 0.12 + 255.0 * 0.88).round() as u8
    };
    format!("#{:02x}{:02x}{:02x}", channel(0), channel(1), channel(2))
}

/// Every field Excalidraw expects on a persisted element.
fn base(id: &str, kind: &str, x: f64, y: f64, w: f64, h: f64) -> Map<String, Value> {
    let seed = stable_seed(id);
    let mut e = Map::new();
    for (k, v) in [
        ("id", json!(id)),
        ("type", json!(kind)),
        ("x", json!(round(x))),
        ("y", json!(round(y))),
        ("width", json!(round(w))),
        ("height", json!(round(h))),
        ("angle", json!(0)),
        ("strokeColor", json!(INK)),
        ("backgroundColor", json!("transparent")),
        ("fillStyle", json!("solid")),
        ("strokeWidth", json!(1)),
        ("strokeStyle", json!("solid")),
        ("roughness", json!(1)),
        ("opacity", json!(100)),
        ("groupIds", json!([])),
        ("frameId", Value::Null),
        ("roundness", Value::Null),
        ("seed", json!(seed)),
        ("version", json!(1)),
        ("versionNonce", json!(seed)),
        ("isDeleted", json!(false)),
        ("boundElements", Value::Null),
        ("updated", json!(1)),
        ("link", Value::Null),
        ("locked", json!(false)),
    ] {
        e.insert(k.to_string(), v);
    }
    e
}

#[allow(clippy::too_many_arguments)]
fn text_element(
    id: &str,
    text: &str,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    container: Option<&str>,
    color: &str,
    size: f64,
) -> Value {
    let mut e = base(id, "text", x, y, w, h);
    e.insert("strokeColor".into(), json!(color));
    e.insert("text".into(), json!(text));
    e.insert("originalText".into(), json!(text));
    e.insert("fontSize".into(), json!(size));
    e.insert("fontFamily".into(), json!(FONT_HAND_DRAWN));
    e.insert(
        "textAlign".into(),
        json!(if container.is_some() {
            "center"
        } else {
            "left"
        }),
    );
    e.insert(
        "verticalAlign".into(),
        json!(if container.is_some() { "middle" } else { "top" }),
    );
    e.insert(
        "containerId".into(),
        container.map_or(Value::Null, |c| json!(c)),
    );
    e.insert("lineHeight".into(), json!(LINE_HEIGHT));
    e.insert("autoResize".into(), json!(true));
    Value::Object(e)
}

/// A shape plus its label, wired the way Excalidraw binds them: the text points
/// at the container, the container lists the text.
#[allow(clippy::too_many_arguments)]
fn labelled(
    out: &mut Vec<Value>,
    id: &str,
    kind: &str,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    label: &str,
    stroke: &str,
    fill: &str,
    rounded: bool,
) {
    let text_id = format!("{id}~label");
    let mut shape = base(id, kind, x, y, w, h);
    shape.insert("strokeColor".into(), json!(stroke));
    shape.insert("backgroundColor".into(), json!(fill));
    shape.insert(
        "roundness".into(),
        if rounded {
            json!({ "type": 3 })
        } else {
            Value::Null
        },
    );
    shape.insert(
        "boundElements".into(),
        json!([{ "id": text_id, "type": "text" }]),
    );
    out.push(Value::Object(shape));
    out.push(text_element(
        &text_id,
        label,
        x,
        y,
        w,
        h,
        Some(id),
        INK,
        FONT_SIZE,
    ));
}

/// Status first (reserved hues never double as identity), then the lane.
fn node_paint(node: &LaidOutNode) -> (String, String) {
    if let Some((stroke, fill)) = tone_paint(node.tone) {
        return (stroke.into(), fill.into());
    }
    match node.lane.and_then(|l| LANE_HUES.get(l - 1)) {
        Some(hue) => (hue.to_string(), tint(hue)),
        None => (INK.into(), "transparent".into()),
    }
}

/// The laid-out diagram as an Excalidraw scene document.
pub fn to_excalidraw(layout: &DiagramLayout, title: Option<&str>) -> Value {
    let mut elements: Vec<Value> = Vec::new();

    for group in &layout.groups {
        let hue = group
            .lane
            .and_then(|l| LANE_HUES.get(l - 1).copied())
            .unwrap_or(MUTED);
        let mut e = base(
            &format!("group~{}", group.label),
            "rectangle",
            group.x,
            group.y,
            group.w,
            group.h,
        );
        e.insert("strokeColor".into(), json!(hue));
        e.insert("strokeStyle".into(), json!("dashed"));
        e.insert("roundness".into(), json!({ "type": 3 }));
        elements.push(Value::Object(e));
        elements.push(text_element(
            &format!("group~{}~title", group.label),
            &group.label,
            group.x + 8.0,
            group.y + 4.0,
            group.w - 16.0,
            GROUP_FONT_SIZE * LINE_HEIGHT,
            None,
            hue,
            GROUP_FONT_SIZE,
        ));
    }

    for node in &layout.nodes {
        let (stroke, fill) = node_paint(node);
        let kind = if node.shape == super::spec::Shape::Diamond {
            "diamond"
        } else {
            "rectangle"
        };
        labelled(
            &mut elements,
            &node.id,
            kind,
            node.x,
            node.y,
            node.w,
            node.h,
            &node.label,
            &stroke,
            &fill,
            node.shape == super::spec::Shape::Round,
        );
    }

    for lifeline in &layout.lifelines {
        let hue = lifeline.lane.and_then(|l| LANE_HUES.get(l - 1).copied());
        let (stroke, fill) = match hue {
            Some(h) => (h.to_string(), tint(h)),
            None => (INK.to_string(), "transparent".to_string()),
        };
        labelled(
            &mut elements,
            &lifeline.id,
            "rectangle",
            lifeline.x,
            lifeline.y,
            lifeline.w,
            lifeline.h,
            &lifeline.label,
            &stroke,
            &fill,
            false,
        );
        let x = lifeline.x + lifeline.w / 2.0;
        let height = lifeline.bottom - lifeline.y - lifeline.h;
        let mut line = base(
            &format!("{}~life", lifeline.id),
            "line",
            x,
            lifeline.y + lifeline.h,
            0.0,
            height,
        );
        line.insert("strokeColor".into(), json!(MUTED));
        line.insert("strokeStyle".into(), json!("dashed"));
        line.insert("points".into(), json!([[0, 0], [0, round(height)]]));
        line.insert("lastCommittedPoint".into(), Value::Null);
        line.insert("startBinding".into(), Value::Null);
        line.insert("endBinding".into(), Value::Null);
        line.insert("startArrowhead".into(), Value::Null);
        line.insert("endArrowhead".into(), Value::Null);
        elements.push(Value::Object(line));
    }

    for (i, note) in layout.notes.iter().enumerate() {
        labelled(
            &mut elements,
            &format!("note~{i}"),
            "rectangle",
            note.x,
            note.y,
            note.w,
            note.h,
            &note.lines.join(" "),
            MUTED,
            "transparent",
            false,
        );
    }

    for edge in &layout.edges {
        let id = format!("edge~{}", edge.key);
        let (ox, oy) = edge.points[0];
        let xs: Vec<f64> = edge.points.iter().map(|p| p.0).collect();
        let ys: Vec<f64> = edge.points.iter().map(|p| p.1).collect();
        let span = |v: &[f64]| {
            v.iter().copied().fold(f64::NEG_INFINITY, f64::max)
                - v.iter().copied().fold(f64::INFINITY, f64::min)
        };
        let mut e = base(&id, "arrow", ox, oy, span(&xs), span(&ys));
        e.insert("strokeColor".into(), json!(MUTED));
        e.insert(
            "strokeStyle".into(),
            json!(if edge.style == super::spec::EdgeStyle::Dashed {
                "dashed"
            } else {
                "solid"
            }),
        );
        e.insert(
            "points".into(),
            Value::Array(
                edge.points
                    .iter()
                    .map(|(x, y)| json!([round(x - ox), round(y - oy)]))
                    .collect(),
            ),
        );
        e.insert("lastCommittedPoint".into(), Value::Null);
        e.insert("startBinding".into(), Value::Null);
        e.insert("endBinding".into(), Value::Null);
        e.insert(
            "startArrowhead".into(),
            match edge.arrow {
                super::spec::Arrow::Both => json!("arrow"),
                _ => Value::Null,
            },
        );
        e.insert(
            "endArrowhead".into(),
            match edge.arrow {
                super::spec::Arrow::None => Value::Null,
                _ => json!("arrow"),
            },
        );
        e.insert("elbowed".into(), json!(false));

        match &edge.label {
            Some(label) => {
                let text_id = format!("{id}~label");
                e.insert(
                    "boundElements".into(),
                    json!([{ "id": text_id, "type": "text" }]),
                );
                elements.push(Value::Object(e));
                elements.push(text_element(
                    &text_id,
                    label,
                    ox,
                    oy,
                    0.0,
                    0.0,
                    Some(&id),
                    MUTED,
                    GROUP_FONT_SIZE,
                ));
            }
            None => elements.push(Value::Object(e)),
        }
    }

    let mut app_state = Map::new();
    app_state.insert("gridSize".into(), Value::Null);
    app_state.insert("viewBackgroundColor".into(), json!("#ffffff"));
    if let Some(name) = title {
        app_state.insert("name".into(), json!(name));
    }

    json!({
        "type": "excalidraw",
        "version": 2,
        "source": "https://github.com/driftwerk/workforce",
        "elements": elements,
        "appState": Value::Object(app_state),
        "files": {},
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::diagram::layout::layout;
    use crate::tools::diagram::spec::validate;
    use crate::tools::diagram::svg::SERVER_METRICS;
    use serde_json::json;

    const BASE_FIELDS: &[&str] = &[
        "id",
        "type",
        "x",
        "y",
        "width",
        "height",
        "angle",
        "strokeColor",
        "backgroundColor",
        "fillStyle",
        "strokeWidth",
        "strokeStyle",
        "roughness",
        "opacity",
        "groupIds",
        "frameId",
        "roundness",
        "seed",
        "version",
        "versionNonce",
        "isDeleted",
        "boundElements",
        "updated",
        "link",
        "locked",
    ];

    fn scene(spec: Value) -> Value {
        let parsed = validate(&spec).expect("valid");
        let laid_out = layout(&parsed, SERVER_METRICS);
        to_excalidraw(&laid_out, parsed.title())
    }

    fn pipeline() -> Value {
        scene(json!({
            "kind": "flow",
            "title": "Order pipeline",
            "nodes": [
                { "id": "in", "label": "Order received", "shape": "round" },
                { "id": "chk", "label": "Stock available?", "shape": "diamond" },
                { "id": "ship", "label": "Pick and ship", "group": "Warehouse" },
                { "id": "bo", "label": "Backorder", "group": "Warehouse", "tone": "warn" },
            ],
            "edges": [
                { "from": "in", "to": "chk" },
                { "from": "chk", "to": "ship", "label": "yes" },
                { "from": "chk", "to": "bo", "label": "no", "style": "dashed" },
            ],
        }))
    }

    fn by_id(scene: &Value, id: &str) -> Value {
        scene["elements"]
            .as_array()
            .expect("elements")
            .iter()
            .find(|e| e["id"] == json!(id))
            .unwrap_or_else(|| panic!("no element {id}"))
            .clone()
    }

    #[test]
    fn produces_a_well_formed_scene_document() {
        let scene = pipeline();
        assert_eq!(scene["type"], json!("excalidraw"));
        assert_eq!(scene["version"], json!(2));
        assert_eq!(scene["appState"]["name"], json!("Order pipeline"));
        assert_eq!(scene["files"], json!({}));
    }

    #[test]
    fn gives_every_element_the_full_base_field_set_and_a_unique_id() {
        let scene = pipeline();
        let elements = scene["elements"].as_array().expect("elements");
        for element in elements {
            for field in BASE_FIELDS {
                assert!(
                    element.get(field).is_some(),
                    "{} lacks {field}",
                    element["id"]
                );
            }
        }
        let mut ids: Vec<&str> = elements.iter().map(|e| e["id"].as_str().unwrap()).collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total, "ids are unique");
    }

    #[test]
    fn maps_shapes_lanes_and_tones_onto_literal_colours() {
        let scene = pipeline();
        assert_eq!(by_id(&scene, "chk")["type"], json!("diamond"));
        assert_eq!(by_id(&scene, "in")["roundness"], json!({ "type": 3 }));
        assert_eq!(by_id(&scene, "ship")["strokeColor"], json!(LANE_HUES[0]));
        // Status outranks the lane it shares with `ship`.
        assert_eq!(by_id(&scene, "bo")["strokeColor"], json!("#eda100"));
        assert_eq!(by_id(&scene, "chk")["strokeColor"], json!(INK));
    }

    #[test]
    fn binds_each_label_to_its_container_both_ways() {
        let scene = pipeline();
        assert_eq!(
            by_id(&scene, "chk")["boundElements"],
            json!([{ "id": "chk~label", "type": "text" }])
        );
        let text = by_id(&scene, "chk~label");
        assert_eq!(text["containerId"], json!("chk"));
        assert_eq!(text["text"], json!("Stock available?"));
    }

    #[test]
    fn exports_arrows_with_relative_points_and_null_bindings() {
        let scene = pipeline();
        let arrows: Vec<&Value> = scene["elements"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == json!("arrow"))
            .collect();
        assert_eq!(arrows.len(), 3);
        for arrow in &arrows {
            assert_eq!(arrow["startBinding"], Value::Null);
            assert_eq!(arrow["endBinding"], Value::Null);
            assert_eq!(arrow["endArrowhead"], json!("arrow"));
            assert_eq!(arrow["points"][0], json!([0.0, 0.0]));
        }
        assert!(arrows.iter().any(|a| a["strokeStyle"] == json!("dashed")));
    }

    /// The seed is derived the same way on both sides, so the same diagram
    /// exported from the widget or the hub carries matching element seeds.
    #[test]
    fn seeds_match_the_widget_derivation() {
        assert_eq!(stable_seed("a"), stable_seed("a"));
        assert_ne!(stable_seed("a"), stable_seed("b"));
        // Locked against values computed from the widget's own implementation,
        // including a non-ASCII id — the hash reads whole UTF-16 units, so a
        // byte-masked port would agree on ASCII and quietly diverge here.
        assert_eq!(stable_seed("chk"), 4_085_614_399);
        assert_eq!(stable_seed("Warehouse"), 3_780_914_116);
        assert_eq!(stable_seed("café"), 856_211_068);
    }

    #[test]
    fn exports_a_sequence_as_heads_lifelines_and_messages() {
        let scene = scene(json!({
            "kind": "sequence",
            "participants": [{ "id": "c", "label": "Customer" }, { "id": "a", "label": "Agent" }],
            "steps": [{ "from": "c", "to": "a", "label": "Asks", "note": "Logged in the CRM" }],
        }));
        let kinds: Vec<&str> = scene["elements"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds.iter().filter(|k| **k == "line").count(), 2);
        assert_eq!(kinds.iter().filter(|k| **k == "arrow").count(), 1);
        assert_eq!(by_id(&scene, "c~label")["text"], json!("Customer"));
        assert_eq!(
            by_id(&scene, "note~0~label")["text"],
            json!("Logged in the CRM")
        );
    }
}
