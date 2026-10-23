//! `ChartSpec` → themed SVG, generated directly in Rust (no JS runtime). The
//! SVG references the theme's bundled font family, so its `<text>` resolves
//! through the same `--font-path` Typst uses — identical on every platform.
//!
//! Never fails: validation already guarantees every series has numeric data,
//! so every spec draws. A pie with no positive slice draws as bars.

use std::fmt::Write;

use super::spec::{ChartSpec, Kind, Series};
use crate::tools::report::theme::Theme;

pub const W: f64 = 720.0;
const PLOT_H: f64 = 360.0;
/// Slices beyond this fold into "Other" — the widget's rule, and the number of
/// series colours a theme carries.
const MAX_PIE_SLICES: usize = 8;
/// A line with more points than this draws no markers, as the widget hides them.
const MARKER_LIMIT: usize = 30;
/// Horizontal bars take a row per category with no stride, so past this many
/// the chart draws vertically instead (the widget starts zooming at 50).
const MAX_HORIZONTAL_ROWS: usize = 50;
/// Currency marks lead the value (`£12`, `US$12`); every other unit follows it.
const CURRENCY: &[char] = &['$', '£', '€', '¥', '₹', '₽', '₩', '₪', '₫', '₴', '₦', '¢'];

pub fn render(spec: &ChartSpec, theme: &Theme) -> String {
    let unit = Unit(spec.unit.as_deref().unwrap_or(""));
    let header = header(spec, theme);
    let (body, height) = match spec.kind {
        Kind::Pie => pie(spec, theme, &unit),
        Kind::Bar => bars(spec, theme, &unit),
        Kind::Line | Kind::Area => line(spec, theme, &unit, spec.kind == Kind::Area),
        Kind::Scatter => scatter(spec, theme, &unit),
    };
    svg(
        header.height + height,
        &defs(theme),
        format!(
            "{}<g transform='translate(0 {:.1})'>{body}</g>",
            header.svg, header.height
        ),
    )
}

/// The widget's unit rule — `%` is a suffix, a symbol such as `£` a prefix,
/// the sign always leads — extended for a word unit (`bags`, `ms`), which
/// follows the value with a space, as a report's `value_suffix` always did.
/// Where the unit goes: a currency mark leads (`£12`, `US$12`), a word
/// follows with a space (`12 bags`), anything else follows directly (`12%`,
/// `12°C`, `12/mo`). The sign always leads. Agrees with the widget on the
/// units it documents (`£`, `%`) and reads naturally for the rest.
struct Unit<'a>(&'a str);

impl Unit<'_> {
    fn apply(&self, digits: String, negative: bool) -> String {
        let sign = if negative { "-" } else { "" };
        match self.0 {
            "" => format!("{sign}{digits}"),
            unit if unit.ends_with(CURRENCY) => format!("{sign}{unit}{digits}"),
            unit if unit.starts_with(char::is_alphabetic) => format!("{sign}{digits} {unit}"),
            unit => format!("{sign}{digits}{unit}"),
        }
    }

    fn full(&self, v: f64) -> String {
        self.apply(fmt_num(v.abs()), v < 0.0)
    }

    fn compact(&self, v: f64) -> String {
        self.apply(fmt_compact(v.abs()), v < 0.0)
    }
}

struct Header {
    svg: String,
    height: f64,
}

/// Title and, for more than one series, a wrapping legend row.
fn header(spec: &ChartSpec, theme: &Theme) -> Header {
    let p = &theme.palette;
    let mut out = String::new();
    let mut y = 0.0;
    if let Some(title) = &spec.title {
        y = 32.0;
        out.push_str(&text(
            16.0,
            24.0,
            "start",
            18.0,
            700,
            &p.ink,
            &theme.sans,
            title,
        ));
    }
    if spec.series.len() > 1 {
        let mut x = 16.0;
        let mut row = y + 18.0;
        for (i, s) in spec.series.iter().enumerate() {
            let name = series_name(s, i);
            let width = 20.0 + text_width(&name, 13.0) + 20.0;
            if x > 16.0 && x + width > W - 16.0 {
                x = 16.0;
                row += 22.0;
            }
            let _ = write!(
                out,
                "<circle cx='{:.1}' cy='{:.1}' r='6' fill='{}'/>{}",
                x + 6.0,
                row - 4.5,
                series_color(theme, i),
                text(
                    x + 20.0,
                    row,
                    "start",
                    13.0,
                    500,
                    &p.ink,
                    &theme.sans,
                    &name
                )
            );
            x += width;
        }
        y = row + 14.0;
    }
    Header {
        svg: out,
        height: y,
    }
}

fn series_name(s: &Series, i: usize) -> String {
    s.name
        .clone()
        .unwrap_or_else(|| format!("Series {}", i + 1))
}

fn text_width(s: &str, size: f64) -> f64 {
    s.chars().count() as f64 * size * 0.56
}

/// `label` cut to `max_width` with an ellipsis, so a run of long category
/// names reads as a row of labels rather than an overlapping band.
fn fit(label: &str, max_width: f64, size: f64) -> String {
    if text_width(label, size) <= max_width {
        return label.to_string();
    }
    let keep = ((max_width / (size * 0.56)) as usize).saturating_sub(1);
    format!(
        "{}…",
        label.chars().take(keep).collect::<String>().trim_end()
    )
}

/// The plot rectangle inside the canvas.
#[derive(Clone, Copy)]
struct Frame {
    left: f64,
    right: f64,
    top: f64,
    bottom: f64,
    height: f64,
}

impl Frame {
    fn pw(&self) -> f64 {
        W - self.left - self.right
    }

    fn ph(&self) -> f64 {
        self.height - self.top - self.bottom
    }

    /// Room for the axis titles the spec declares.
    fn with_axis_titles(mut self, spec: &ChartSpec) -> Self {
        if spec.y_label.is_some() {
            self.left += 22.0;
        }
        if spec.x_label.is_some() {
            self.bottom += 20.0;
            self.height += 20.0;
        }
        self
    }
}

fn axis_titles(spec: &ChartSpec, f: &Frame, theme: &Theme) -> String {
    let p = &theme.palette;
    let y_title = spec.y_label.as_deref().map_or(String::new(), |label| {
        let cy = f.top + f.ph() / 2.0;
        format!(
            "<text transform='rotate(-90 14 {cy:.1})' x='14' y='{cy:.1}' text-anchor='middle' \
             font-family='{}, \"Noto Emoji\"' font-size='13' font-weight='600' fill='{}'>{}</text>",
            theme.sans,
            p.muted,
            xml_escape(label)
        )
    });
    let x_title = spec.x_label.as_deref().map_or(String::new(), |label| {
        text(
            f.left + f.pw() / 2.0,
            f.height - 8.0,
            "middle",
            13.0,
            600,
            &p.muted,
            &theme.sans,
            label,
        )
    });
    format!("{y_title}{x_title}")
}

fn value_at(s: &Series, i: usize) -> Option<f64> {
    s.data.values().get(i).copied()
}

/// Value-axis domain over the raw values of every series.
fn value_domain(spec: &ChartSpec) -> (f64, f64, f64) {
    let values = spec
        .series
        .iter()
        .flat_map(|s| s.data.values().iter().copied());
    let (min, max) = values.fold((f64::MAX, f64::MIN), |(lo, hi), v| (lo.min(v), hi.max(v)));
    nice_bounds(min, max, 5)
}

/// Value-axis domain over the per-category stacks: positives stack up from
/// zero, negatives stack down.
fn stacked_domain(spec: &ChartSpec, n: usize) -> (f64, f64, f64) {
    let (min, max) = (0..n).fold((0.0_f64, 0.0_f64), |(lo, hi), i| {
        let (down, up) = spec.series.iter().filter_map(|s| value_at(s, i)).fold(
            (0.0_f64, 0.0_f64),
            |(d, u), v| {
                if v < 0.0 {
                    (d + v, u)
                } else {
                    (d, u + v)
                }
            },
        );
        (lo.min(down), hi.max(up))
    });
    nice_bounds(min, max, 5)
}

fn domain(spec: &ChartSpec, n: usize) -> (f64, f64, f64) {
    if spec.stacked {
        stacked_domain(spec, n)
    } else {
        value_domain(spec)
    }
}

/// Horizontal grid lines with value labels down the left edge.
fn value_grid(f: &Frame, lo: f64, hi: f64, step: f64, unit: &Unit, theme: &Theme) -> String {
    let p = &theme.palette;
    ticks(lo, hi, step)
        .map(|t| {
            let y = f.top + f.ph() * (1.0 - (t - lo) / (hi - lo));
            format!(
                "<line x1='{:.1}' y1='{y:.1}' x2='{:.1}' y2='{y:.1}' stroke='{}' stroke-width='1'/>{}",
                f.left,
                W - f.right,
                p.hairline,
                text(f.left - 10.0, y + 4.0, "end", 13.0, 400, &p.muted, &theme.sans, &unit.compact(t)),
            )
        })
        .collect()
}

/// Fill for series `i`: the theme gradient when it is the only one, else its
/// solid series colour.
fn bar_fill(theme: &Theme, i: usize, single: bool) -> String {
    if single {
        "url(#barfill)".into()
    } else {
        series_color(theme, i).to_string()
    }
}

/// Stacks accumulate separately above and below zero, so a mixed-sign stack
/// never draws a segment across the baseline.
#[derive(Default)]
struct Stack {
    up: f64,
    down: f64,
}

impl Stack {
    fn push(&mut self, v: f64) -> (f64, f64) {
        let base = if v < 0.0 {
            &mut self.down
        } else {
            &mut self.up
        };
        let from = *base;
        *base += v;
        (from, *base)
    }
}

fn bar_vertical(spec: &ChartSpec, theme: &Theme, unit: &Unit) -> (String, f64) {
    let f = Frame {
        left: 52.0,
        right: 20.0,
        top: 16.0,
        bottom: 54.0,
        height: PLOT_H,
    }
    .with_axis_titles(spec);
    let p = &theme.palette;
    let labels = spec.categories();
    let (n, k) = (labels.len(), spec.series.len());
    let single = k == 1;
    let (lo, hi, step) = domain(spec, n);
    let yof = |v: f64| f.top + f.ph() * (1.0 - (v - lo) / (hi - lo));
    let slot = f.pw() / n as f64;
    let bw = if spec.stacked {
        (slot * 0.6).min(64.0)
    } else {
        (slot * 0.72 / k as f64).min(64.0)
    };
    let group_w = if spec.stacked { bw } else { bw * k as f64 };
    let stride = label_stride(n, 14);
    let zero = yof(0.0);
    let rx = if spec.stacked { 0.0 } else { 4.0 };
    let stroke = if spec.stacked {
        format!(" stroke='{}' stroke-width='1'", p.paper)
    } else {
        String::new()
    };

    let mut bars = String::new();
    for (i, label) in labels.iter().enumerate() {
        let cx = f.left + slot * (i as f64 + 0.5);
        let x0 = cx - group_w / 2.0;
        let mut stack = Stack::default();
        for (s, series) in spec.series.iter().enumerate() {
            let Some(v) = value_at(series, i) else {
                continue;
            };
            let (x, from, to) = if spec.stacked {
                let (from, to) = stack.push(v);
                (x0, yof(from), yof(to))
            } else {
                (x0 + bw * s as f64, zero, yof(v))
            };
            let (by, bh) = (from.min(to), (from - to).abs());
            let _ = write!(
                bars,
                "<rect x='{x:.1}' y='{by:.1}' width='{bw:.1}' height='{bh:.1}' rx='{rx}' fill='{}'{stroke}/>",
                bar_fill(theme, s, single)
            );
            if single && i % stride == 0 {
                let ly = if v < 0.0 { by + bh + 16.0 } else { by - 7.0 };
                bars.push_str(&text(
                    x + bw / 2.0,
                    ly,
                    "middle",
                    13.0,
                    700,
                    &p.secondary,
                    &theme.sans,
                    &unit.full(v),
                ));
            }
        }
        if i % stride == 0 {
            bars.push_str(&text(
                cx,
                f.height - f.bottom + 22.0,
                "middle",
                13.0,
                400,
                &p.muted,
                &theme.sans,
                &fit(label, slot * stride as f64 - 6.0, 13.0),
            ));
        }
    }
    (
        format!(
            "{}{bars}{}",
            value_grid(&f, lo, hi, step, unit, theme),
            axis_titles(spec, &f, theme)
        ),
        f.height,
    )
}

fn bars(spec: &ChartSpec, theme: &Theme, unit: &Unit) -> (String, f64) {
    if spec.horizontal && spec.categories().len() <= MAX_HORIZONTAL_ROWS {
        bar_horizontal(spec, theme, unit)
    } else {
        bar_vertical(spec, theme, unit)
    }
}

fn bar_horizontal(spec: &ChartSpec, theme: &Theme, unit: &Unit) -> (String, f64) {
    let p = &theme.palette;
    let labels = spec.categories();
    let (n, k) = (labels.len(), spec.series.len());
    let single = k == 1;
    let (lo, hi, _) = domain(spec, n);
    let (top, gap, gutter, right_pad) = (12.0, 14.0, 138.0, 70.0);
    let bh = if single || spec.stacked {
        30.0
    } else {
        (30.0 / k as f64).max(10.0)
    };
    let row_h = if spec.stacked { bh } else { bh * k as f64 };
    let (x0, track_w) = (gutter, W - right_pad - gutter);
    let pos = |v: f64| x0 + track_w * ((v - lo) / (hi - lo));
    let zero = pos(0.0);
    let label_size = if single { 16.0 } else { 12.0 };

    let mut rows = String::new();
    for (i, label) in labels.iter().enumerate() {
        let y = top + i as f64 * (row_h + gap);
        rows.push_str(&text(
            gutter - 12.0,
            y + row_h / 2.0 + 5.0,
            "end",
            16.0,
            600,
            &p.ink,
            &theme.sans,
            label,
        ));
        if single {
            let _ = write!(
                rows,
                "<rect x='{x0:.1}' y='{y:.1}' width='{track_w:.1}' height='{bh}' rx='6' fill='{}'/>",
                p.paper
            );
        }
        let mut stack = Stack::default();
        for (s, series) in spec.series.iter().enumerate() {
            let Some(v) = value_at(series, i) else {
                continue;
            };
            let (by, from, to) = if spec.stacked {
                let (from, to) = stack.push(v);
                (y, pos(from), pos(to))
            } else {
                (y + bh * s as f64, zero, pos(v))
            };
            let (bx, bw) = (from.min(to), (from - to).abs());
            let _ = write!(
                rows,
                "<rect x='{bx:.1}' y='{by:.1}' width='{bw:.1}' height='{bh}' rx='{}' fill='{}'/>",
                if spec.stacked { 0.0 } else { 6.0 },
                bar_fill(theme, s, single)
            );
            if !spec.stacked {
                rows.push_str(&end_label(
                    v,
                    bx,
                    bw,
                    by + bh / 2.0,
                    label_size,
                    unit,
                    theme,
                ));
            }
        }
        if spec.stacked {
            let cy = y + bh / 2.0;
            if stack.up > 0.0 {
                rows.push_str(&end_label(
                    stack.up,
                    zero,
                    pos(stack.up) - zero,
                    cy,
                    label_size,
                    unit,
                    theme,
                ));
            }
            if stack.down < 0.0 {
                rows.push_str(&end_label(
                    stack.down,
                    pos(stack.down),
                    zero - pos(stack.down),
                    cy,
                    label_size,
                    unit,
                    theme,
                ));
            }
        }
    }
    (rows, top + n as f64 * (row_h + gap) + 6.0)
}

/// A value label just past the end of a horizontal bar, on the side it grows.
fn end_label(v: f64, bx: f64, bw: f64, cy: f64, size: f64, unit: &Unit, theme: &Theme) -> String {
    let (x, anchor) = if v < 0.0 {
        (bx - 8.0, "end")
    } else {
        (bx + bw + 10.0, "start")
    };
    text(
        x,
        cy + size * 0.35,
        anchor,
        size,
        700,
        &theme.palette.secondary,
        &theme.sans,
        &unit.full(v),
    )
}

fn line(spec: &ChartSpec, theme: &Theme, unit: &Unit, area: bool) -> (String, f64) {
    let f = Frame {
        left: 52.0,
        right: 24.0,
        top: 18.0,
        bottom: 52.0,
        height: PLOT_H,
    }
    .with_axis_titles(spec);
    let p = &theme.palette;
    let labels = spec.categories();
    let (n, k) = (labels.len(), spec.series.len());
    let single = k == 1;
    let plotted = plotted_values(spec, n);
    let (min, max) = plotted
        .iter()
        .flatten()
        .flatten()
        .fold((0.0_f64, 0.0_f64), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
    let (lo, hi, step) = nice_bounds(min, max, 5);
    let xof = |i: usize| {
        if n == 1 {
            f.left + f.pw() / 2.0
        } else {
            f.left + f.pw() * i as f64 / (n - 1) as f64
        }
    };
    let yof = |v: f64| f.top + f.ph() * (1.0 - (v - lo) / (hi - lo));
    let coords = |values: &[Option<f64>]| -> Vec<(f64, f64)> {
        values
            .iter()
            .enumerate()
            .filter_map(|(i, v)| v.map(|v| (xof(i), yof(v))))
            .collect()
    };
    let poly = |pts: &[(f64, f64)]| {
        pts.iter()
            .map(|(x, y)| format!("{x:.1},{y:.1}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let base = yof(0.0);

    let mut fills = String::new();
    let mut strokes = String::new();
    for (s, values) in plotted.iter().enumerate() {
        let pts = coords(values);
        if pts.is_empty() {
            continue;
        }
        let color = if single {
            p.ink.as_str()
        } else {
            series_color(theme, s)
        };
        if area {
            let lower = match (spec.stacked && s > 0).then(|| coords(&plotted[s - 1])) {
                Some(prev) => poly(&prev.iter().rev().copied().collect::<Vec<_>>()),
                None => format!(
                    "{:.1},{base:.1} {:.1},{base:.1}",
                    pts[pts.len() - 1].0,
                    pts[0].0
                ),
            };
            let fill = if single && !spec.stacked {
                "fill='url(#areafill)'".to_string()
            } else {
                format!("fill='{color}' fill-opacity='0.18'")
            };
            let _ = write!(fills, "<polygon points='{} {lower}' {fill}/>", poly(&pts));
        }
        let _ = write!(
            strokes,
            "<polyline points='{}' fill='none' stroke='{color}' stroke-width='3' \
             stroke-linejoin='round' stroke-linecap='round'/>",
            poly(&pts)
        );
        if n <= MARKER_LIMIT {
            for (x, y) in &pts {
                let _ = write!(
                    strokes,
                    "<circle cx='{x:.1}' cy='{y:.1}' r='4.5' fill='{}' stroke='{color}' stroke-width='2.5'/>",
                    p.panel
                );
            }
        }
        if single {
            let last = pts[pts.len() - 1];
            let value = values
                .iter()
                .rev()
                .flatten()
                .next()
                .copied()
                .unwrap_or_default();
            strokes.push_str(&text(
                last.0,
                last.1 - 14.0,
                "middle",
                14.0,
                700,
                &p.ink,
                &theme.sans,
                &unit.full(value),
            ));
        }
    }
    let stride = label_stride(n, 12);
    let slot = f.pw() * stride as f64 / n.max(2) as f64;
    let xlabels = labels
        .iter()
        .enumerate()
        .filter(|(i, _)| i % stride == 0)
        .map(|(i, label)| {
            text(
                xof(i),
                f.height - f.bottom + 20.0,
                "middle",
                13.0,
                400,
                &p.muted,
                &theme.sans,
                &fit(label, slot - 6.0, 13.0),
            )
        })
        .collect::<String>();
    (
        format!(
            "{}{fills}{strokes}{xlabels}{}",
            value_grid(&f, lo, hi, step, unit, theme),
            axis_titles(spec, &f, theme)
        ),
        f.height,
    )
}

/// The y each series plots at every category: its own value, or the running
/// stack when stacked. A category a series has no value for plots nothing
/// and adds nothing to the stack.
fn plotted_values(spec: &ChartSpec, n: usize) -> Vec<Vec<Option<f64>>> {
    if !spec.stacked {
        return spec
            .series
            .iter()
            .map(|s| (0..n).map(|i| value_at(s, i)).collect())
            .collect();
    }
    spec.series
        .iter()
        .scan(vec![0.0; n], |running, s| {
            Some(
                (0..n)
                    .map(|i| {
                        value_at(s, i).map(|v| {
                            running[i] += v;
                            running[i]
                        })
                    })
                    .collect(),
            )
        })
        .collect()
}

fn pie(spec: &ChartSpec, theme: &Theme, unit: &Unit) -> (String, f64) {
    let labels = spec.categories();
    let values = spec.series[0].data.values();
    let slices = fold_slices(labels.iter().cloned().zip(values.iter().copied()).collect());
    let positive: Vec<&(String, f64)> = slices.iter().filter(|(_, v)| *v > 0.0).collect();
    if positive.is_empty() {
        let fallback = ChartSpec {
            kind: Kind::Bar,
            horizontal: true,
            stacked: false,
            ..spec.clone()
        };
        return bars(&fallback, theme, unit);
    }
    let (cx, cy, r, rin) = (180.0, 180.0, 132.0, 78.0);
    let total: f64 = positive.iter().map(|(_, v)| *v).sum();
    let p = &theme.palette;
    let mut a0 = -90.0_f64;
    let mut segs = String::new();
    let mut legend = String::new();
    for (i, (label, v)) in positive.iter().enumerate() {
        let sweep = 360.0 * v / total;
        let a1 = a0 + sweep;
        let color = series_color(theme, i);
        // A single 360° slice would be a degenerate (zero-length) arc that draws
        // nothing, so split a full ring into two halves.
        if sweep >= 359.999 {
            let mid = a0 + 180.0;
            segs.push_str(&annular_path(cx, cy, r, rin, a0, mid, color));
            segs.push_str(&annular_path(cx, cy, r, rin, mid, a0 + 360.0, color));
        } else {
            segs.push_str(&annular_path(cx, cy, r, rin, a0, a1, color));
        }
        let ly = 64.0 + i as f64 * 44.0;
        let pct = (v / total * 100.0).round();
        let _ = write!(
            legend,
            "<rect x='400' y='{ly:.1}' width='18' height='18' rx='4' fill='{color}'/>{}{}",
            text(
                428.0,
                ly + 14.0,
                "start",
                17.0,
                600,
                &p.ink,
                &theme.sans,
                label
            ),
            text(
                W - 30.0,
                ly + 14.0,
                "end",
                17.0,
                700,
                &p.secondary,
                &theme.sans,
                &format!("{pct:.0}%")
            ),
        );
        a0 = a1;
    }
    let height = (64.0 + positive.len() as f64 * 44.0).max(2.0 * cy) + 20.0;
    (format!("{segs}{legend}"), height)
}

/// Slices beyond [`MAX_PIE_SLICES`] fold into one "Other", as the widget does.
fn fold_slices(slices: Vec<(String, f64)>) -> Vec<(String, f64)> {
    if slices.len() <= MAX_PIE_SLICES {
        return slices;
    }
    let (kept, rest) = slices.split_at(MAX_PIE_SLICES - 1);
    let other = rest.iter().map(|(_, v)| v).sum();
    kept.iter()
        .cloned()
        .chain(std::iter::once(("Other".to_string(), other)))
        .collect()
}

fn scatter(spec: &ChartSpec, theme: &Theme, unit: &Unit) -> (String, f64) {
    let f = Frame {
        left: 60.0,
        right: 24.0,
        top: 18.0,
        bottom: 52.0,
        height: PLOT_H,
    }
    .with_axis_titles(spec);
    let p = &theme.palette;
    let points: Vec<(usize, (f64, f64))> = spec
        .series
        .iter()
        .enumerate()
        .flat_map(|(s, series)| series.data.pairs().iter().map(move |pt| (s, *pt)))
        .collect();
    let (xmin, xmax, ymin, ymax) = points.iter().fold(
        (f64::MAX, f64::MIN, f64::MAX, f64::MIN),
        |(xl, xh, yl, yh), (_, (x, y))| (xl.min(*x), xh.max(*x), yl.min(*y), yh.max(*y)),
    );
    let years = looks_like_years(points.iter().map(|(_, (x, _))| *x));
    let (xlo, xhi, xstep) = if years {
        year_range(xmin, xmax)
    } else {
        nice_range(xmin, xmax, 6)
    };
    let (ylo, yhi, ystep) = nice_range(ymin, ymax, 5);
    let xof = |x: f64| f.left + f.pw() * (x - xlo) / (xhi - xlo);
    let yof = |y: f64| f.top + f.ph() * (1.0 - (y - ylo) / (yhi - ylo));
    let x_tick = |t: f64| {
        if years {
            format!("{t:.0}")
        } else {
            fmt_compact(t)
        }
    };

    let grid_y = ticks(ylo, yhi, ystep)
        .map(|t| {
            let y = yof(t);
            format!(
                "<line x1='{:.1}' y1='{y:.1}' x2='{:.1}' y2='{y:.1}' stroke='{}' stroke-width='1'/>{}",
                f.left,
                W - f.right,
                p.hairline,
                text(f.left - 10.0, y + 4.0, "end", 13.0, 400, &p.muted, &theme.sans, &unit.compact(t)),
            )
        })
        .collect::<String>();
    let grid_x = ticks(xlo, xhi, xstep)
        .map(|t| {
            let x = xof(t);
            format!(
                "<line x1='{x:.1}' y1='{:.1}' x2='{x:.1}' y2='{:.1}' stroke='{}' stroke-width='1'/>{}",
                f.top,
                f.height - f.bottom,
                p.hairline,
                text(x, f.height - f.bottom + 20.0, "middle", 13.0, 400, &p.muted, &theme.sans, &x_tick(t)),
            )
        })
        .collect::<String>();
    let marks = points
        .iter()
        .map(|(s, (x, y))| {
            format!(
                "<circle cx='{:.1}' cy='{:.1}' r='5' fill='{}' fill-opacity='0.85' stroke='{}' stroke-width='1'/>",
                xof(*x),
                yof(*y),
                series_color(theme, *s),
                p.panel
            )
        })
        .collect::<String>();
    (
        format!("{grid_y}{grid_x}{marks}{}", axis_titles(spec, &f, theme)),
        f.height,
    )
}

/// Whole-year ticks over `[min, max]`: never a half-year step, which would
/// print the same year twice.
fn year_range(min: f64, max: f64) -> (f64, f64, f64) {
    let (lo, hi, step) = nice_range(min, max, 6);
    if step >= 1.0 {
        (lo, hi, step)
    } else {
        (min, max.max(min + 1.0), 1.0)
    }
}

/// Every x is a whole number in a calendar-year range — the one scatter input
/// that must not read as `2K`.
fn looks_like_years(xs: impl Iterator<Item = f64>) -> bool {
    let mut any = false;
    for x in xs {
        any = true;
        if x.fract() != 0.0 || !(1900.0..=2100.0).contains(&x) {
            return false;
        }
    }
    any
}

// ── number formatting ───────────────────────────────────────────────────────

/// Integer when whole, else one decimal — with negative zero normalised so a
/// rounding-to-zero value never shows as `-0`.
pub(crate) fn fmt_decimal(n: f64) -> String {
    let n = if n == 0.0 { 0.0 } else { n };
    if (n - n.round()).abs() < 1e-9 {
        let r = n.round();
        format!("{:.0}", if r == 0.0 { 0.0 } else { r })
    } else {
        match format!("{n:.1}").as_str() {
            "-0.0" => "0.0".into(),
            s => s.to_string(),
        }
    }
}

pub(crate) fn fmt_num(v: f64) -> String {
    if !v.is_finite() {
        return "0".into();
    }
    with_thousands(&fmt_decimal(v))
}

pub(crate) fn fmt_compact(v: f64) -> String {
    let a = v.abs();
    let (n, suffix) = if a >= 1e9 {
        (v / 1e9, "B")
    } else if a >= 1e6 {
        (v / 1e6, "M")
    } else if a >= 1e3 {
        (v / 1e3, "K")
    } else {
        return fmt_num(v);
    };
    format!("{}{suffix}", fmt_decimal(n))
}

fn with_thousands(body: &str) -> String {
    let (sign, rest) = body.strip_prefix('-').map_or(("", body), |r| ("-", r));
    let (int, frac) = rest.split_once('.').map_or((rest, ""), |(i, f)| (i, f));
    let grouped = int
        .as_bytes()
        .rchunks(3)
        .rev()
        .map(|c| std::str::from_utf8(c).unwrap_or_default())
        .collect::<Vec<_>>()
        .join(",");
    match frac {
        "" => format!("{sign}{grouped}"),
        f => format!("{sign}{grouped}.{f}"),
    }
}

// ── axis scaling (Heckbert nice numbers) ─────────────────────────────────────

fn nice_num(range: f64, round: bool) -> f64 {
    if range <= 0.0 {
        return 1.0;
    }
    let exp = range.log10().floor();
    let frac = range / 10f64.powf(exp);
    let nice = if round {
        match frac {
            f if f < 1.5 => 1.0,
            f if f < 3.0 => 2.0,
            f if f < 7.0 => 5.0,
            _ => 10.0,
        }
    } else {
        match frac {
            f if f <= 1.0 => 1.0,
            f if f <= 2.0 => 2.0,
            f if f <= 5.0 => 5.0,
            _ => 10.0,
        }
    };
    nice * 10f64.powf(exp)
}

/// Value-axis domain `[lo, hi]` (with a tick step) that includes the zero
/// baseline and gives nice round bounds. `lo <= 0 <= hi` always holds.
pub(crate) fn nice_bounds(min: f64, max: f64, ticks: usize) -> (f64, f64, f64) {
    nice_range(min.min(0.0), max.max(0.0), ticks)
}

/// Nice round bounds around `[min, max]` without forcing a zero baseline —
/// a scatter of years must not start at year 0.
fn nice_range(min: f64, max: f64, ticks: usize) -> (f64, f64, f64) {
    let max = if max <= min { min + 1.0 } else { max };
    let step = nice_num(nice_num(max - min, false) / ticks.max(1) as f64, true);
    let lo = (min / step).floor() * step;
    let hi = (max / step).ceil() * step;
    let hi = if hi <= lo { lo + step } else { hi };
    (lo, hi, step)
}

fn ticks(lo: f64, hi: f64, step: f64) -> impl Iterator<Item = f64> {
    let count = ((hi - lo) / step).round() as i64;
    (0..=count).map(move |i| lo + i as f64 * step)
}

// ── svg primitives ───────────────────────────────────────────────────────────

pub(crate) fn xml_escape(s: &str) -> String {
    s.chars()
        .filter_map(|c| match c {
            '&' => Some("&amp;".to_string()),
            '<' => Some("&lt;".to_string()),
            '>' => Some("&gt;".to_string()),
            '"' => Some("&quot;".to_string()),
            '\'' => Some("&apos;".to_string()),
            c if c.is_control() => None,
            c => Some(c.to_string()),
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn text(
    x: f64,
    y: f64,
    anchor: &str,
    size: f64,
    weight: u32,
    fill: &str,
    font: &str,
    s: &str,
) -> String {
    // Chart SVGs are rendered standalone, so they need their own emoji fallback
    // (the document font chain in `typst_gen` doesn't reach them).
    format!(
        "<text x='{x:.1}' y='{y:.1}' text-anchor='{anchor}' font-family='{font}, \"Noto Emoji\"' \
         font-size='{size}' font-weight='{weight}' fill='{fill}'>{}</text>",
        xml_escape(s)
    )
}

fn svg(view_h: f64, defs: &str, body: String) -> String {
    format!(
        "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 {W} {view_h:.0}'>{defs}{body}</svg>"
    )
}

fn label_stride(n: usize, max_labels: usize) -> usize {
    n.div_ceil(max_labels.max(1)).max(1)
}

fn series_color(theme: &Theme, i: usize) -> &str {
    let s = &theme.palette.series;
    s[i % s.len()].as_str()
}

fn annular_path(cx: f64, cy: f64, r: f64, rin: f64, a0: f64, a1: f64, color: &str) -> String {
    let large = u8::from((a1 - a0) > 180.0);
    let pol = |ang: f64, rad: f64| {
        (
            cx + rad * ang.to_radians().cos(),
            cy + rad * ang.to_radians().sin(),
        )
    };
    let (x0, y0) = pol(a0, r);
    let (x1, y1) = pol(a1, r);
    let (xi1, yi1) = pol(a1, rin);
    let (xi0, yi0) = pol(a0, rin);
    format!(
        "<path d='M{x0:.1},{y0:.1} A{r},{r} 0 {large} 1 {x1:.1},{y1:.1} \
         L{xi1:.1},{yi1:.1} A{rin},{rin} 0 {large} 0 {xi0:.1},{yi0:.1} Z' fill='{color}'/>"
    )
}

fn defs(theme: &Theme) -> String {
    let p = &theme.palette;
    format!(
        "<defs><linearGradient id='barfill' x1='0' y1='0' x2='1' y2='0'>\
         <stop offset='0' stop-color='{}'/><stop offset='1' stop-color='{}'/></linearGradient>\
         <linearGradient id='areafill' x1='0' y1='0' x2='0' y2='1'>\
         <stop offset='0' stop-color='{}' stop-opacity='0.32'/>\
         <stop offset='1' stop-color='{}' stop-opacity='0'/></linearGradient></defs>",
        p.secondary, p.accent, p.accent, p.accent
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::chart::spec::validate;
    use crate::tools::report::theme;
    use serde_json::{json, Value};

    fn draw(v: Value) -> String {
        render(&validate(&v).expect("valid spec"), theme::resolve(None))
    }

    fn count(svg: &str, tag: &str) -> usize {
        svg.matches(&format!("<{tag}")).count()
    }

    /// The `(x, width)` of every `<rect>` in draw order.
    fn rects(svg: &str) -> Vec<(f64, f64, f64, f64)> {
        let attr = |frag: &str, name: &str| {
            let pat = format!("{name}='");
            let start = frag.find(&pat)? + pat.len();
            frag[start..]
                .split_once('\'')
                .and_then(|(v, _)| v.parse::<f64>().ok())
        };
        svg.split("<rect")
            .skip(1)
            .filter_map(|frag| {
                Some((
                    attr(frag, "x")?,
                    attr(frag, "y")?,
                    attr(frag, "width")?,
                    attr(frag, "height")?,
                ))
            })
            .collect()
    }

    #[test]
    fn with_thousands_groups_and_keeps_sign_and_fraction() {
        assert_eq!(with_thousands("1234567"), "1,234,567");
        assert_eq!(with_thousands("-1234.5"), "-1,234.5");
        assert_eq!(with_thousands("99"), "99");
    }

    #[test]
    fn fmt_compact_scales_magnitudes() {
        assert_eq!(fmt_compact(950.0), "950");
        assert_eq!(fmt_compact(1500.0), "1.5K");
        assert_eq!(fmt_compact(2_000_000.0), "2M");
        assert_eq!(fmt_compact(3_400_000_000.0), "3.4B");
    }

    #[test]
    fn fmt_decimal_normalises_negative_zero() {
        assert_eq!(fmt_decimal(-0.0), "0");
        assert_eq!(fmt_decimal(-0.04), "0.0");
        assert!(!fmt_num(-0.0).starts_with('-'));
    }

    #[test]
    fn nice_bounds_includes_zero_and_is_ordered() {
        let (lo, hi, step) = nice_bounds(3.0, 66.4, 5);
        assert!(lo <= 0.0 && hi >= 66.4 && step > 0.0);
        let (lo, hi, _) = nice_bounds(-5.0, 20.0, 5);
        assert!(lo <= -5.0 && hi >= 20.0);
        let (lo, hi, step) = nice_bounds(0.0, 0.0, 5);
        assert!(hi > lo && step > 0.0);
        let (lo, hi, _) = nice_bounds(-9.0, -4.0, 5);
        assert!(lo <= -9.0 && hi >= 0.0, "lo={lo} hi={hi}");
    }

    #[test]
    fn nice_range_does_not_force_a_zero_baseline() {
        let (lo, hi, _) = nice_range(2015.0, 2024.0, 6);
        assert!(
            (2000.0..=2015.0).contains(&lo) && hi >= 2024.0,
            "lo={lo} hi={hi}"
        );
    }

    #[test]
    fn currency_leads_and_every_other_unit_follows() {
        assert_eq!(Unit("£").full(-1234.5), "-£1,234.5");
        assert_eq!(Unit("US$").full(12.0), "US$12");
        assert_eq!(Unit("%").full(42.0), "42%");
        assert_eq!(Unit("°C").full(12.0), "12°C");
        assert_eq!(Unit("/mo").full(12.0), "12/mo");
        assert_eq!(Unit("bags").full(12.0), "12 bags");
        assert_eq!(Unit("").compact(1500.0), "1.5K");
        assert_eq!(Unit("$").compact(-2_000_000.0), "-$2M");
    }

    #[test]
    fn long_category_labels_are_ellipsised_not_overlapped() {
        let labels: Vec<String> = (1..=12)
            .map(|i| format!("September the {i}th 2025"))
            .collect();
        let data: Vec<u32> = (1..=12).collect();
        let svg = draw(json!({ "kind": "bar", "labels": labels, "series": [{ "data": data }] }));
        assert!(svg.contains("…</text>"), "{svg}");
        assert!(!svg.contains("September the 1th 2025<"));
        let short = draw(
            json!({ "kind": "line", "labels": ["Jan", "Feb"], "series": [{ "data": [1, 2] }] }),
        );
        assert!(short.contains(">Jan<") && !short.contains("…"));
    }

    #[test]
    fn too_many_horizontal_rows_draw_vertically_instead() {
        let labels: Vec<String> = (0..MAX_HORIZONTAL_ROWS + 1)
            .map(|i| i.to_string())
            .collect();
        let data: Vec<usize> = (0..MAX_HORIZONTAL_ROWS + 1).collect();
        let svg = draw(
            json!({ "kind": "bar", "horizontal": true, "labels": labels, "series": [{ "data": data }] }),
        );
        let height: f64 = svg
            .split("viewBox='0 0 720 ")
            .nth(1)
            .unwrap()
            .split('\'')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(height < 500.0, "a vertical chart, not {height} px of rows");
        let fallback = draw(
            json!({ "kind": "pie", "labels": (0..60).map(|i| i.to_string()).collect::<Vec<_>>(), "series": [{ "data": vec![0; 60] }] }),
        );
        assert!(
            fallback.contains("viewBox='0 0 720 3"),
            "pie fallback obeys the same cap"
        );
    }

    #[test]
    fn scatter_year_ticks_never_repeat() {
        let svg = draw(
            json!({ "kind": "scatter", "series": [{ "data": [[2019, 5], [2020, 7], [2021, 2]] }] }),
        );
        let ticks: Vec<&str> = svg
            .split("<text")
            .skip(1)
            .filter_map(|t| t.split('>').nth(1)?.split('<').next())
            .filter(|t| t.starts_with("20"))
            .collect();
        let mut unique = ticks.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(ticks.len(), unique.len(), "{ticks:?}");
    }

    #[test]
    fn a_ragged_stacked_series_plots_only_where_it_has_data() {
        let svg = draw(json!({
            "kind": "area", "stacked": true,
            "series": [{ "data": [1, 2, 3] }, { "data": [1] }]
        }));
        assert_eq!(
            count(&svg, "circle"),
            2 + 3 + 1,
            "legend swatches plus one marker per plotted point"
        );
    }

    #[test]
    fn every_kind_renders_well_formed_svg() {
        let t = theme::resolve(None);
        let specs = [
            json!({ "kind": "bar", "labels": ["A", "B"], "series": [{ "data": [1, 2] }] }),
            json!({ "kind": "bar", "horizontal": true, "labels": ["A", "B"], "series": [{ "data": [1, 2] }] }),
            json!({ "kind": "line", "labels": ["A", "B"], "series": [{ "data": [1, 2] }] }),
            json!({ "kind": "area", "labels": ["A", "B"], "series": [{ "data": [1, 2] }] }),
            json!({ "kind": "pie", "labels": ["A", "B"], "series": [{ "data": [1, 2] }] }),
            json!({ "kind": "scatter", "series": [{ "data": [[1, 2], [3, 4]] }] }),
        ];
        for v in specs {
            let kind = v["kind"].clone();
            let out = render(&validate(&v).unwrap(), t);
            assert!(out.starts_with("<svg") && out.ends_with("</svg>"), "{kind}");
            assert!(out.contains("viewBox='0 0 720 "), "{kind}");
            assert!(
                out.contains("font-family='Lato, \"Noto Emoji\"'"),
                "{kind} should use the theme font with an emoji fallback"
            );
        }
    }

    #[test]
    fn grouped_bars_draw_one_rect_per_series_per_label() {
        let svg = draw(json!({
            "kind": "bar", "labels": ["Q1", "Q2", "Q3"],
            "series": [{ "name": "North", "data": [1, 2, 3] }, { "name": "South", "data": [2, 3, 4] }]
        }));
        assert_eq!(count(&svg, "rect"), 6);
        assert!(
            svg.contains(">North<") && svg.contains(">South<"),
            "legend names"
        );
        assert_eq!(count(&svg, "circle"), 2, "one legend swatch per series");
        assert!(
            !svg.contains("url(#barfill)"),
            "multi-series uses solid colours"
        );
    }

    #[test]
    fn stacked_bars_sum_to_the_total_height() {
        let svg = draw(json!({
            "kind": "bar", "stacked": true, "labels": ["A"],
            "series": [{ "data": [10] }, { "data": [30] }]
        }));
        let r = rects(&svg);
        assert_eq!(r.len(), 2);
        let (a, b) = (r[0], r[1]);
        assert_eq!(a.0, b.0, "stacked segments share an x");
        assert!(
            (a.1 - (b.1 + b.3)).abs() < 0.2,
            "the second sits on top of the first: {a:?} {b:?}"
        );
        assert!((b.3 / a.3 - 3.0).abs() < 0.1, "heights are proportional");
    }

    #[test]
    fn mixed_sign_stacks_never_cross_the_baseline() {
        let svg = draw(json!({
            "kind": "bar", "stacked": true, "labels": ["A"],
            "series": [{ "data": [10] }, { "data": [-10] }]
        }));
        let r = rects(&svg);
        assert!(
            (r[0].1 + r[0].3 - r[1].1).abs() < 0.2,
            "both segments meet at zero: {r:?}"
        );
    }

    #[test]
    fn horizontal_bars_stay_within_canvas() {
        let svg = draw(json!({
            "kind": "bar", "horizontal": true, "labels": ["A", "B"],
            "series": [{ "data": [-4, -9] }, { "data": [3, 1] }]
        }));
        for (x, _, w, _) in rects(&svg) {
            assert!(
                x >= 0.0 && x + w <= W + 1.0,
                "rect x={x} w={w} overflows {W}"
            );
        }
    }

    #[test]
    fn stacked_horizontal_bars_label_the_total() {
        let svg = draw(json!({
            "kind": "bar", "horizontal": true, "stacked": true, "labels": ["A"],
            "series": [{ "data": [40] }, { "data": [60] }]
        }));
        assert!(svg.contains(">100<"));
        assert!(!svg.contains(">40<") && !svg.contains(">60<"));
    }

    #[test]
    fn multi_line_draws_one_polyline_per_series() {
        let svg = draw(json!({
            "kind": "line", "labels": ["a", "b", "c"],
            "series": [{ "data": [1, 2, 3] }, { "data": [3, 2, 1] }, { "data": [2, 2, 2] }]
        }));
        assert_eq!(count(&svg, "polyline"), 3);
        assert_eq!(count(&svg, "polygon"), 0);
        assert_eq!(count(&svg, "circle"), 3 + 9, "legend swatches plus markers");
    }

    #[test]
    fn stacked_area_fills_between_series() {
        let svg = draw(json!({
            "kind": "area", "stacked": true, "labels": ["a", "b"],
            "series": [{ "data": [1, 1] }, { "data": [1, 1] }]
        }));
        assert_eq!(count(&svg, "polygon"), 2);
        assert!(svg.contains("fill-opacity='0.18'"));
        let second = svg.split("<polygon").nth(2).unwrap();
        assert_eq!(
            second
                .split("points='")
                .nth(1)
                .unwrap()
                .split('\'')
                .next()
                .unwrap()
                .split(' ')
                .count(),
            4,
            "the upper area closes along the lower series, not the baseline"
        );
    }

    #[test]
    fn single_series_keeps_the_report_look() {
        let bar =
            draw(json!({ "kind": "bar", "labels": ["A", "B"], "series": [{ "data": [1, 2] }] }));
        assert!(bar.contains("url(#barfill)"));
        assert!(bar.contains(">1<") && bar.contains(">2<"), "value labels");
        let area =
            draw(json!({ "kind": "area", "labels": ["A", "B"], "series": [{ "data": [1, 2] }] }));
        assert!(area.contains("url(#areafill)"));
        assert!(!bar.contains("<circle"), "no legend for one series");
    }

    #[test]
    fn long_lines_draw_no_markers() {
        let data: Vec<u32> = (0..40).collect();
        let svg = draw(json!({ "kind": "line", "series": [{ "data": data }] }));
        assert_eq!(count(&svg, "circle"), 0);
    }

    #[test]
    fn pie_folds_slices_beyond_eight_into_other() {
        let labels: Vec<String> = (0..10).map(|i| format!("s{i}")).collect();
        let data: Vec<u32> = (1..=10).collect();
        let svg = draw(json!({ "kind": "pie", "labels": labels, "series": [{ "data": data }] }));
        assert_eq!(count(&svg, "path"), 8);
        assert!(svg.contains(">Other<"));
        assert!(!svg.contains(">s8<"));
    }

    #[test]
    fn pie_with_no_positive_values_falls_back_to_bars() {
        let svg =
            draw(json!({ "kind": "pie", "labels": ["A", "B"], "series": [{ "data": [0, -2] }] }));
        assert!(svg.contains("barfill") && count(&svg, "path") == 0);
    }

    #[test]
    fn pie_single_full_slice_draws_a_ring_not_a_blank() {
        let svg = draw(json!({ "kind": "pie", "labels": ["All"], "series": [{ "data": [100] }] }));
        assert_eq!(count(&svg, "path"), 2);
    }

    #[test]
    fn scatter_places_every_point_and_keeps_years_whole() {
        let svg = draw(json!({
            "kind": "scatter", "x_label": "Year", "y_label": "Sales",
            "series": [{ "data": [[2019, 5], [2020, 7]] }, { "data": [[2021, 2]] }]
        }));
        assert_eq!(count(&svg, "circle"), 2 + 3, "legend swatches plus points");
        assert!(svg.contains(">2020<") && !svg.contains("2K"));
        assert!(svg.contains(">Year<") && svg.contains("rotate(-90"));
    }

    #[test]
    fn title_and_axis_titles_are_drawn() {
        let svg = draw(json!({
            "kind": "bar", "title": "Signups", "x_label": "Plan", "y_label": "Count",
            "labels": ["A"], "series": [{ "data": [1] }]
        }));
        assert!(svg.contains(">Signups<") && svg.contains(">Plan<") && svg.contains(">Count<"));
    }

    #[test]
    fn unit_reaches_axis_and_value_labels() {
        let svg = draw(
            json!({ "kind": "bar", "unit": "£", "labels": ["A"], "series": [{ "data": [1500] }] }),
        );
        assert!(svg.contains(">£1,500<"), "value label");
        assert!(svg.contains(">£1K<") || svg.contains(">£2K<"), "axis tick");
    }

    #[test]
    fn control_chars_are_stripped_and_markup_escaped() {
        let svg = draw(
            json!({ "kind": "bar", "labels": ["A\u{0007}B & <C>"], "series": [{ "data": [5] }] }),
        );
        assert!(!svg.chars().any(char::is_control));
        assert!(svg.contains("AB &amp; &lt;C&gt;"));
    }

    #[test]
    fn single_point_and_ragged_series_do_not_panic() {
        let t = theme::resolve(None);
        for v in [
            json!({ "kind": "line", "series": [{ "data": [7] }] }),
            json!({ "kind": "area", "stacked": true, "series": [{ "data": [1, 2, 3] }, { "data": [1] }] }),
            json!({ "kind": "bar", "series": [{ "data": [1, 2] }, { "data": [1] }] }),
            json!({ "kind": "bar", "horizontal": true, "stacked": true, "series": [{ "data": [-1] }, { "data": [2] }] }),
            json!({ "kind": "scatter", "series": [{ "data": [[1, 1]] }] }),
            json!({ "kind": "bar", "labels": ["x"], "series": [{ "data": [0] }] }),
        ] {
            let out = render(&validate(&v).unwrap(), t);
            assert!(out.contains("<svg"), "{v}");
        }
    }
}
