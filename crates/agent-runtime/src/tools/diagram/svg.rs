//! A laid-out diagram as a standalone SVG.
//!
//! The widget paints from CSS variables; there is no stylesheet here, so every
//! value is literal. Chrome (ink, muted, surface, hairline) comes from the
//! report's `Theme` — a diagram in an espresso report should not be wearing
//! slate's greys — while the lane hues do not, because they encode identity and
//! were validated as a set: a theme must not restyle them.
//!
//! Labels are agent-authored text going straight into markup, so everything is
//! XML-escaped here. The widget got that for free from React; this does not.

use std::fmt::Write as _;

use super::draw::{arrow_heads, edge_path, node_path};
use super::layout::{DiagramLayout, LaidOutNode, Metrics};
use super::spec::{DiagramSpec, EdgeStyle, Tone};
use crate::tools::report::theme::Theme;

/// The validated lane palette, light mode — kept in step with the widget's
/// `LANE_HUES`. Five, then neutral.
pub const LANE_HUES: [&str; 5] = ["#2a78d6", "#1baf7a", "#eda100", "#4a3aa7", "#e34948"];

const TONE_ACCENT: &str = "#2a78d6";
const TONE_GOOD: &str = "#1baf7a";
const TONE_WARN: &str = "#eda100";
const TONE_BAD: &str = "#e34948";

/// The metrics the server renders at. Fixed rather than measured: there is no
/// browser here, and the goldens pin the widget to the same numbers.
pub const SERVER_METRICS: Metrics = Metrics {
    char_width: 7.0,
    line_height: 16.0,
};

const FONT_SIZE: f64 = 13.0;
const EDGE_FONT_SIZE: f64 = 12.0;
const CHIP_PAD: f64 = 5.0;

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// A 12% tint of `hue` over the paper — the fill behind a lane or tone, mixed
/// here because there is no `color-mix` outside a stylesheet.
fn tint(hue: &str, paper: &str) -> String {
    let channel = |s: &str, i: usize| -> f64 {
        u8::from_str_radix(&s[1 + i * 2..3 + i * 2], 16).unwrap_or(0) as f64
    };
    if hue.len() != 7 || paper.len() != 7 {
        return paper.to_string();
    }
    let mix = |i: usize| (channel(hue, i) * 0.12 + channel(paper, i) * 0.88).round() as u8;
    format!("#{:02x}{:02x}{:02x}", mix(0), mix(1), mix(2))
}

fn tone_hue(tone: Tone) -> Option<&'static str> {
    match tone {
        Tone::Default => None,
        Tone::Accent => Some(TONE_ACCENT),
        Tone::Good => Some(TONE_GOOD),
        Tone::Warn => Some(TONE_WARN),
        Tone::Bad => Some(TONE_BAD),
    }
}

/// Status first (reserved hues never double as identity), then the lane, then
/// plain chrome — the same precedence the widget's stylesheet encodes.
fn node_paint(node: &LaidOutNode, theme: &Theme) -> (String, String) {
    let hue = tone_hue(node.tone).or_else(|| node.lane.and_then(|l| LANE_HUES.get(l - 1).copied()));
    match hue {
        Some(h) => (h.to_string(), tint(h, &theme.palette.paper)),
        None => (theme.palette.hairline.clone(), theme.palette.paper.clone()),
    }
}

fn text_lines(
    out: &mut String,
    lines: &[String],
    cx: f64,
    cy: f64,
    size: f64,
    fill: &str,
    font: &str,
) {
    let top = cy - ((lines.len() as f64 - 1.0) * SERVER_METRICS.line_height) / 2.0;
    let _ = write!(
        out,
        "<text text-anchor='middle' dominant-baseline='middle' font-family='{}' font-size='{size}' fill='{fill}'>",
        escape(font)
    );
    for (i, line) in lines.iter().enumerate() {
        let _ = write!(
            out,
            "<tspan x='{cx}' y='{}'>{}</tspan>",
            top + i as f64 * SERVER_METRICS.line_height,
            escape(line)
        );
    }
    out.push_str("</text>");
}

/// Render a laid-out diagram to a standalone SVG document.
pub fn render(spec: &DiagramSpec, layout: &DiagramLayout, theme: &Theme) -> String {
    let p = &theme.palette;
    let font = &theme.sans;
    let mut out = String::with_capacity(4096);
    let _ = write!(
        out,
        "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 {} {}' width='{}' height='{}' role='img' aria-label='{}'>",
        layout.width,
        layout.height,
        layout.width,
        layout.height,
        escape(&super::layout::aria_label(spec, layout)),
    );
    let _ = write!(
        out,
        "<rect x='0' y='0' width='100%' height='100%' fill='{}'/>",
        p.paper
    );

    for group in &layout.groups {
        let hue = group
            .lane
            .and_then(|l| LANE_HUES.get(l - 1).copied())
            .unwrap_or(&p.hairline);
        let _ = write!(
            out,
            "<rect x='{}' y='{}' width='{}' height='{}' rx='10' fill='none' stroke='{hue}' stroke-dasharray='4 4'/>",
            group.x, group.y, group.w, group.h
        );
        let _ = write!(
            out,
            "<text x='{}' y='{}' font-family='{}' font-size='{EDGE_FONT_SIZE}' fill='{hue}'>{}</text>",
            group.x + 10.0,
            group.y + 13.0,
            escape(font),
            escape(&group.label)
        );
    }

    for lifeline in &layout.lifelines {
        let hue = lifeline
            .lane
            .and_then(|l| LANE_HUES.get(l - 1).copied())
            .unwrap_or(&p.hairline);
        let cx = lifeline.x + lifeline.w / 2.0;
        let _ = write!(
            out,
            "<line x1='{cx}' y1='{}' x2='{cx}' y2='{}' stroke='{hue}' stroke-dasharray='4 4'/>",
            lifeline.y + lifeline.h,
            lifeline.bottom
        );
        let head = LaidOutNode {
            id: lifeline.id.clone(),
            label: lifeline.label.clone(),
            lines: lifeline.lines.clone(),
            shape: super::spec::Shape::Box,
            tone: Tone::Default,
            x: lifeline.x,
            y: lifeline.y,
            w: lifeline.w,
            h: lifeline.h,
            group: None,
            lane: None,
        };
        let _ = write!(
            out,
            "<path d='{}' fill='{}' stroke='{hue}' stroke-width='1.4' stroke-linejoin='round'/>",
            node_path(&head),
            tint(hue, &p.paper)
        );
        text_lines(
            &mut out,
            &lifeline.lines,
            cx,
            lifeline.y + lifeline.h / 2.0,
            FONT_SIZE,
            &p.ink,
            font,
        );
    }

    for (i, note) in layout.notes.iter().enumerate() {
        let head = LaidOutNode {
            id: format!("note~{i}"),
            label: String::new(),
            lines: note.lines.clone(),
            shape: super::spec::Shape::Box,
            tone: Tone::Default,
            x: note.x,
            y: note.y,
            w: note.w,
            h: note.h,
            group: None,
            lane: None,
        };
        let _ = write!(
            out,
            "<path d='{}' fill='{}' stroke='{}' stroke-width='1.4'/>",
            node_path(&head),
            p.panel,
            p.hairline
        );
        text_lines(
            &mut out,
            &note.lines,
            note.x + note.w / 2.0,
            note.y + note.h / 2.0,
            FONT_SIZE,
            &p.ink,
            font,
        );
    }

    for edge in &layout.edges {
        let dash = if edge.style == EdgeStyle::Dashed {
            " stroke-dasharray='5 4'"
        } else {
            ""
        };
        let _ = write!(
            out,
            "<path d='{}' fill='none' stroke='{}' stroke-width='1.4' stroke-linecap='round'{dash}/>",
            edge_path(&edge.points),
            p.muted
        );
        for head in arrow_heads(&edge.points, edge.arrow) {
            let _ = write!(out, "<path d='{head}' fill='{}'/>", p.muted);
        }
        if let (Some(label), Some((lx, ly))) = (&edge.label, edge.label_at) {
            let w = label.chars().count() as f64 * SERVER_METRICS.char_width;
            let _ = write!(
                out,
                "<rect x='{}' y='{}' width='{}' height='{}' rx='4' fill='{}'/>",
                lx - w / 2.0 - CHIP_PAD,
                ly - SERVER_METRICS.line_height / 2.0,
                w + 2.0 * CHIP_PAD,
                SERVER_METRICS.line_height,
                p.paper
            );
            text_lines(
                &mut out,
                std::slice::from_ref(label),
                lx,
                ly,
                EDGE_FONT_SIZE,
                &p.muted,
                font,
            );
        }
    }

    for node in &layout.nodes {
        let (stroke, fill) = node_paint(node, theme);
        let _ = write!(
            out,
            "<path d='{}' fill='{fill}' stroke='{stroke}' stroke-width='1.4' stroke-linejoin='round'/>",
            node_path(node)
        );
        text_lines(
            &mut out,
            &node.lines,
            node.x + node.w / 2.0,
            node.y + node.h / 2.0,
            FONT_SIZE,
            &p.ink,
            font,
        );
    }

    out.push_str("</svg>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::diagram::layout::layout;
    use crate::tools::diagram::spec::validate;
    use crate::tools::report::theme::resolve;
    use serde_json::json;

    fn svg_of(spec: serde_json::Value) -> String {
        let spec = validate(&spec).expect("valid spec");
        let laid_out = layout(&spec, SERVER_METRICS);
        render(&spec, &laid_out, resolve(None))
    }

    #[test]
    fn renders_a_well_formed_document_with_the_theme_ground() {
        let svg = svg_of(json!({
            "kind": "flow",
            "title": "Order pipeline",
            "nodes": [{ "id": "a", "label": "Start" }, { "id": "b", "label": "Finish" }],
            "edges": [{ "from": "a", "to": "b", "label": "next" }],
        }));
        assert!(svg.starts_with("<svg xmlns="));
        assert!(svg.ends_with("</svg>"));
        assert!(svg.contains("aria-label='Flow diagram: Order pipeline; 2 steps'"));
        assert!(svg.contains(&resolve(None).palette.paper));
        assert_eq!(
            svg.matches("<path").count(),
            4,
            "two nodes, an edge, a head"
        );
    }

    #[test]
    fn paints_a_lane_hue_and_lets_a_status_tone_outrank_it() {
        let svg = svg_of(json!({
            "kind": "flow",
            "nodes": [
                { "id": "a", "label": "Pick", "group": "Warehouse" },
                { "id": "b", "label": "Failed", "group": "Warehouse", "tone": "bad" },
            ],
            "edges": [{ "from": "a", "to": "b" }],
        }));
        assert!(svg.contains(LANE_HUES[0]), "lane hue present");
        assert!(svg.contains(TONE_BAD), "status hue present");
    }

    /// Agent text reaches the markup directly here; the widget got escaping free
    /// from React and this does not.
    #[test]
    fn escapes_agent_text_into_the_markup() {
        let svg = svg_of(json!({
            "kind": "flow",
            "nodes": [{ "id": "a", "label": "A <script> & \"quotes\"" }],
        }));
        assert!(!svg.contains("<script>"));
        assert!(svg.contains("&lt;script&gt;"));
        assert!(svg.contains("&amp;"));
    }

    #[test]
    fn renders_a_sequence_with_lifelines_and_a_note() {
        let svg = svg_of(json!({
            "kind": "sequence",
            "participants": [{ "id": "c", "label": "Customer" }, { "id": "a", "label": "Agent" }],
            "steps": [{ "from": "c", "to": "a", "label": "Asks", "note": "Logged in the CRM" }],
        }));
        assert_eq!(svg.matches("<line").count(), 2, "one lifeline each");
        assert!(svg.contains("Logged in the CRM"));
        assert!(svg.contains("stroke-dasharray='4 4'"));
    }
}
