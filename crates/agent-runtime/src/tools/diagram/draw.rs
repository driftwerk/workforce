//! Geometry → SVG path strings — a port of the widget's `diagramDraw.ts`,
//! clean style only.
//!
//! The hand-drawn skin is deliberately not ported: matching a seeded PRNG and
//! per-segment perturbation byte-for-byte across two languages is the fiddliest
//! parity surface in this port, and a report wants clean lines. A spec asking
//! for `"style":"sketch"` renders clean here — the hand-drawn look is still
//! reachable by downloading the `.excalidraw`, which carries `roughness: 1` and
//! is rendered by the app that owns that aesthetic.

use std::fmt::Write as _;

use super::layout::{LaidOutNode, Point, CYLINDER_CAP};
use super::spec::{Arrow, Shape};

const ARROW: f64 = 7.0;
const CORNER: f64 = 8.0;

/// Two decimals — enough for sub-pixel smoothness, and it keeps the emitted
/// path stable enough to assert on.
fn r(n: f64) -> f64 {
    (n * 100.0).round() / 100.0
}

fn polyline(points: &[Point], close: bool) -> String {
    let mut d = String::new();
    for (i, (x, y)) in points.iter().enumerate() {
        let _ = write!(d, "{}{} {}", if i == 0 { 'M' } else { 'L' }, r(*x), r(*y));
    }
    if close {
        d.push('Z');
    }
    d
}

fn diamond_points(x: f64, y: f64, w: f64, h: f64) -> Vec<Point> {
    vec![
        (x + w / 2.0, y),
        (x + w, y + h / 2.0),
        (x + w / 2.0, y + h),
        (x, y + h / 2.0),
    ]
}

/// The outline of a node box.
pub fn node_path(node: &LaidOutNode) -> String {
    let (x, y, w, h) = (node.x, node.y, node.w, node.h);
    match node.shape {
        Shape::Diamond => polyline(&diamond_points(x, y, w, h), true),
        Shape::Cylinder => {
            let cap = CYLINDER_CAP.min(h / 4.0);
            let rx = w / 2.0;
            format!(
                "M{} {}A{} {} 0 0 1 {} {}A{} {} 0 0 1 {} {}M{} {}L{} {}A{} {} 0 0 0 {} {}L{} {}",
                r(x),
                r(y + cap),
                r(rx),
                r(cap),
                r(x + w),
                r(y + cap),
                r(rx),
                r(cap),
                r(x),
                r(y + cap),
                r(x),
                r(y + cap),
                r(x),
                r(y + h - cap),
                r(rx),
                r(cap),
                r(x + w),
                r(y + h - cap),
                r(x + w),
                r(y + cap),
            )
        }
        _ => {
            let radius = if node.shape == Shape::Round {
                h / 2.0
            } else {
                CORNER
            };
            let rad = radius.min(w / 2.0).min(h / 2.0);
            format!(
                "M{} {}L{} {}A{} {} 0 0 1 {} {}L{} {}A{} {} 0 0 1 {} {}L{} {}A{} {} 0 0 1 {} {}L{} {}A{} {} 0 0 1 {} {}Z",
                r(x + rad), r(y),
                r(x + w - rad), r(y),
                r(rad), r(rad), r(x + w), r(y + rad),
                r(x + w), r(y + h - rad),
                r(rad), r(rad), r(x + w - rad), r(y + h),
                r(x + rad), r(y + h),
                r(rad), r(rad), r(x), r(y + h - rad),
                r(x), r(y + rad),
                r(rad), r(rad), r(x + rad), r(y),
            )
        }
    }
}

/// The connector polyline.
pub fn edge_path(points: &[Point]) -> String {
    polyline(points, false)
}

/// Filled arrow heads: one at the target, plus one at the source for a two-headed
/// edge, none when the edge is undirected.
pub fn arrow_heads(points: &[Point], arrow: Arrow) -> Vec<String> {
    if arrow == Arrow::None || points.len() < 2 {
        return Vec::new();
    }
    let mut heads = vec![head(points[points.len() - 2], points[points.len() - 1])];
    if arrow == Arrow::Both {
        heads.push(head(points[1], points[0]));
    }
    heads
}

fn head(from: Point, tip: Point) -> String {
    let dx = tip.0 - from.0;
    let dy = tip.1 - from.1;
    let length = dx.hypot(dy).max(f64::MIN_POSITIVE);
    let (ux, uy) = (dx / length, dy / length);
    let base = (tip.0 - ux * ARROW, tip.1 - uy * ARROW);
    let wing = ARROW * 0.45;
    polyline(
        &[
            tip,
            (base.0 - uy * wing, base.1 + ux * wing),
            (base.0 + uy * wing, base.1 - ux * wing),
        ],
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::diagram::spec::Tone;

    fn node(shape: Shape) -> LaidOutNode {
        LaidOutNode {
            id: "chk".into(),
            label: "Stock available?".into(),
            lines: vec!["Stock available?".into()],
            shape,
            tone: Tone::Default,
            x: 20.0,
            y: 40.0,
            w: 120.0,
            h: 44.0,
            group: None,
            lane: None,
        }
    }

    #[test]
    fn draws_a_box_as_a_closed_rounded_rect() {
        let d = node_path(&node(Shape::Box));
        assert!(d.starts_with('M') && d.ends_with('Z'));
        assert!(d.contains('A'));
    }

    /// Matches the widget's output for the same box — the drawing half of parity.
    #[test]
    fn draws_a_diamond_exactly_as_the_widget_does() {
        assert_eq!(
            node_path(&node(Shape::Diamond)),
            "M80 40L140 62L80 84L20 62Z"
        );
    }

    #[test]
    fn gives_a_round_shape_a_full_height_radius() {
        assert!(node_path(&node(Shape::Round)).contains("A22 22"));
    }

    #[test]
    fn gives_a_cylinder_a_cap_ellipse_and_a_body() {
        let d = node_path(&node(Shape::Cylinder));
        assert_eq!(d.matches('M').count(), 2);
        assert_eq!(d.matches('A').count(), 3);
    }

    #[test]
    fn draws_a_connector_as_a_polyline() {
        let points = vec![(10.0, 10.0), (10.0, 50.0), (90.0, 50.0)];
        assert_eq!(edge_path(&points), "M10 10L10 50L90 50");
    }

    #[test]
    fn points_the_head_along_the_final_segment() {
        let points = vec![(10.0, 10.0), (10.0, 50.0), (90.0, 50.0)];
        let heads = arrow_heads(&points, Arrow::To);
        assert_eq!(heads.len(), 1);
        assert!(heads[0].starts_with("M90 50"));
    }

    #[test]
    fn adds_a_second_head_only_for_a_two_way_edge() {
        let points = vec![(0.0, 0.0), (10.0, 0.0)];
        assert_eq!(arrow_heads(&points, Arrow::Both).len(), 2);
        assert_eq!(arrow_heads(&points, Arrow::None).len(), 0);
        assert_eq!(arrow_heads(&points[..1], Arrow::To).len(), 0);
    }
}
