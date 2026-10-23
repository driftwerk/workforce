//! IR + theme → Typst source plus the chart SVGs it references. The agent never
//! authors Typst: its strings are escaped and interpolated as Typst string
//! literals, so content cannot inject markup or code. A chart that can't render
//! degrades to a table here, never failing the document.

use serde_json::Value;

use super::chart;
use super::ir::{AccentTone, Align, Block, Format, Report, Section};
use super::theme::Theme;

pub struct Asset {
    pub name: String,
    pub bytes: Vec<u8>,
}

pub struct Document {
    pub source: String,
    pub assets: Vec<Asset>,
}

/// A staged image resolved to bytes + file extension, keyed by artifact id.
/// Resolved in `execute` (which has store access) so `build` stays pure.
pub struct ResolvedImage {
    pub ext: &'static str,
    pub bytes: Vec<u8>,
}

pub type Images = std::collections::HashMap<String, ResolvedImage>;

pub fn build(
    report: &Report,
    theme: &Theme,
    images: &Images,
    logo: Option<&ResolvedImage>,
) -> Document {
    let mut assets = Vec::new();
    let body = report
        .sections
        .iter()
        .enumerate()
        .map(|(si, sec)| render_section(sec, si, theme, images, &mut assets))
        .collect::<Vec<_>>()
        .join("\n");
    let logo_markup = logo
        .map(|img| {
            let name = format!("logo.{}", img.ext);
            assets.push(Asset {
                name: name.clone(),
                bytes: img.bytes.clone(),
            });
            format!("#image(\"{name}\", height: 30pt)\n")
        })
        .unwrap_or_default();
    let source = match report.format {
        Format::Report => report_doc(report, theme, &body, &logo_markup),
        Format::Infographic => infographic_doc(report, theme, &body, &logo_markup),
    };
    Document { source, assets }
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace(['\n', '\r', '\t'], " ")
}

/// Like [`esc`] but keeps line breaks and tabs as Typst string escapes instead of
/// flattening them to spaces — for `code` blocks, whose whitespace is the content.
fn esc_verbatim(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\r', "")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}

/// A styled run of literal text. `font`/`fill` are Typst bindings (barewords);
/// the user string is escaped and interpolated, never re-parsed as markup.
fn txt(font: &str, size: f64, weight: u32, fill: &str, s: &str) -> String {
    format!(
        "text(font: {font}, size: {size}pt, weight: {weight}, fill: {fill})[#\"{}\"]",
        esc(s)
    )
}

/// Like [`txt`] but the run is parsed for inline Markdown (see [`inline`])
/// instead of shown literally — for body text and captions the model authors.
fn styled(font: &str, size: f64, weight: u32, fill: &str, s: &str) -> String {
    format!(
        "text(font: {font}, size: {size}pt, weight: {weight}, fill: {fill})[{}]",
        inline(s)
    )
}

/// Lower a text run to Typst *markup*, recognising a safe inline-Markdown subset
/// so the model can write `**bold**`, `*italic*` and `` `code` `` and have them
/// styled rather than shown literally. Every other character — and the inner
/// text of each span — is escaped and emitted as a Typst string literal, so user
/// content can never inject Typst markup or code (only the `strong`/`emph`/`raw`
/// wrappers are literal Typst). `_` is deliberately NOT emphasis, so `snake_case`
/// identifiers survive; `*`/`**` require a non-space inner edge (CommonMark-style
/// flanking) so arithmetic like `3 * 4` stays literal.
fn inline(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut out = String::new();
    let mut plain = String::new();
    let mut i = 0;
    while i < n {
        let c = chars[i];
        // `code` — verbatim inner, highest precedence.
        if c == '`' {
            if let Some(close) = find_char(&chars, i + 1, '`') {
                if close > i + 1 {
                    flush_plain(&mut out, &mut plain);
                    out.push_str(&format!(
                        "#raw(\"{}\")",
                        esc(&collect(&chars, i + 1, close))
                    ));
                    i = close + 1;
                    continue;
                }
            }
        }
        // **bold** — opener and closer both flanked by a non-space inner edge.
        if c == '*'
            && i + 1 < n
            && chars[i + 1] == '*'
            && i + 2 < n
            && !chars[i + 2].is_whitespace()
        {
            if let Some(close) = find_emphasis(&chars, i + 2, true) {
                flush_plain(&mut out, &mut plain);
                out.push_str(&format!(
                    "#strong[#\"{}\"]",
                    esc(&collect(&chars, i + 2, close))
                ));
                i = close + 2;
                continue;
            }
        }
        // *italic* — single star, not the start of a `**` run.
        if c == '*' && i + 1 < n && chars[i + 1] != '*' && !chars[i + 1].is_whitespace() {
            if let Some(close) = find_emphasis(&chars, i + 1, false) {
                flush_plain(&mut out, &mut plain);
                out.push_str(&format!(
                    "#emph[#\"{}\"]",
                    esc(&collect(&chars, i + 1, close))
                ));
                i = close + 1;
                continue;
            }
        }
        plain.push(c);
        i += 1;
    }
    flush_plain(&mut out, &mut plain);
    if out.is_empty() {
        out.push_str("#\"\"");
    }
    out
}

/// Emit any pending literal run as an escaped Typst string literal.
fn flush_plain(out: &mut String, plain: &mut String) {
    if !plain.is_empty() {
        out.push_str(&format!("#\"{}\"", esc(plain)));
        plain.clear();
    }
}

fn collect(chars: &[char], from: usize, to: usize) -> String {
    chars[from..to].iter().collect()
}

fn find_char(chars: &[char], from: usize, target: char) -> Option<usize> {
    (from..chars.len()).find(|&k| chars[k] == target)
}

/// Find the closing index of an emphasis span opened at the first inner char
/// `from`: the first `*` (for `double`, a `**` pair) preceded by a non-space char
/// and enclosing a non-empty inner run. Returns the index of that closing `*`.
fn find_emphasis(chars: &[char], from: usize, double: bool) -> Option<usize> {
    let n = chars.len();
    let mut k = from;
    while k < n {
        let is_close = chars[k] == '*' && (!double || (k + 1 < n && chars[k + 1] == '*'));
        if is_close && k > from && !chars[k - 1].is_whitespace() {
            return Some(k);
        }
        k += 1;
    }
    None
}

fn accent_var(accent: AccentTone) -> &'static str {
    match accent {
        AccentTone::Accent => "accent",
        AccentTone::Primary => "primary",
        AccentTone::Secondary => "secondary",
        AccentTone::Ink => "ink",
    }
}

fn align_typst(align: Align) -> &'static str {
    match align {
        Align::Left => "left",
        Align::Center => "center",
        Align::Right => "right",
    }
}

pub(crate) fn cell_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

fn render_section(
    sec: &Section,
    si: usize,
    theme: &Theme,
    images: &Images,
    assets: &mut Vec<Asset>,
) -> String {
    let head = sec
        .heading
        .as_deref()
        .map(|h| format!("#sectionhead(\"{}\")", esc(h)))
        .unwrap_or_default();
    let blocks = sec
        .blocks
        .iter()
        .enumerate()
        .map(|(bi, b)| render_block(b, si, bi, theme, images, assets))
        .collect::<Vec<_>>()
        .join("\n#v(10pt)\n");
    format!("{head}\n{blocks}")
}

fn render_block(
    block: &Block,
    si: usize,
    bi: usize,
    theme: &Theme,
    images: &Images,
    assets: &mut Vec<Asset>,
) -> String {
    match block {
        Block::Prose { text } => format!("#prose[{}]", inline(text)),
        Block::Heading { text } => format!("#{}", styled("sansf", 13.0, 700, "primary", text)),
        // `raw` is already the lowering for inline `` `code` ``, so the block form
        // resolves its font the same way and needs no extra bundled face.
        Block::Code { text, language } => {
            let lang = language
                .as_deref()
                .filter(|l| !l.is_empty())
                .map(|l| format!(", lang: \"{}\"", esc(l)))
                .unwrap_or_default();
            format!(
                "#block(width: 100%, fill: panel, inset: 9pt, radius: 3pt)[#raw(block: true{lang}, \"{}\")]",
                esc_verbatim(text)
            )
        }
        Block::Stats { items } => {
            let cols = items.len().clamp(1, 4);
            let cards = items
                .iter()
                .map(|it| {
                    format!(
                        "  statcard(\"{}\", \"{}\", {}),",
                        esc(&it.value),
                        esc(&it.label),
                        accent_var(it.accent)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!("#grid(columns: (1fr,) * {cols}, column-gutter: 11pt, row-gutter: 11pt,\n{cards}\n)")
        }
        Block::Diagram {
            spec,
            title,
            caption,
        } => match super::diagram_svg(spec, theme) {
            Ok(svg) => {
                let name = format!("diagram_{si}_{bi}.svg");
                assets.push(Asset {
                    name: name.clone(),
                    bytes: Vec::from(svg),
                });
                chart_card(title.as_deref(), &name, caption.as_deref())
            }
            // An unrenderable diagram degrades to its own description rather
            // than vanishing, the same posture the chart fallback takes.
            Err(reason) => format!(
                "#prose[{}]",
                inline(&format!("Diagram unavailable: {reason}"))
            ),
        },
        Block::Chart {
            intent,
            title,
            caption,
        } => match chart::render(intent, theme) {
            Ok(svg) => {
                let name = format!("chart_{si}_{bi}.svg");
                assets.push(Asset {
                    name: name.clone(),
                    bytes: svg.into_bytes(),
                });
                chart_card(title.as_deref(), &name, caption.as_deref())
            }
            Err(_) => {
                let (columns, rows) = chart_fallback(intent);
                table_block(&columns, &rows, caption.as_deref())
            }
        },
        Block::Table {
            columns,
            rows,
            caption,
        } => table_block(columns, rows, caption.as_deref()),
        Block::List { title, items } => {
            let title_line = title
                .as_deref()
                .map(|t| format!("#{}\n#v(6pt)\n", txt("sansf", 12.0, 700, "primary", t)))
                .unwrap_or_default();
            let entries = items
                .iter()
                .map(|it| {
                    let body = match &it.lead {
                        Some(lead) => format!(
                            "#text(font: sansf, weight: 700, fill: ink)[{}] #text(font: seriff, fill: ink)[#\" \"{}]",
                            inline(lead),
                            inline(&it.text)
                        ),
                        None => format!("#text(font: seriff, fill: ink)[{}]", inline(&it.text)),
                    };
                    format!("  [{body}],")
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "{title_line}#list(marker: text(fill: accent)[•], spacing: 0.9em,\n{entries}\n)"
            )
        }
        Block::Image {
            artifact_id,
            caption,
            width,
            align,
        } => match images.get(artifact_id) {
            Some(img) => {
                let name = format!("img_{si}_{bi}.{}", img.ext);
                assets.push(Asset {
                    name: name.clone(),
                    bytes: img.bytes.clone(),
                });
                let w = width.unwrap_or(100).clamp(1, 100);
                let body = format!(
                    "#align({})[#image(\"{name}\", width: {w}%)]",
                    align_typst(*align)
                );
                match caption.as_deref() {
                    Some(c) => format!(
                        "{body}\n#align(center)[#{}]",
                        styled("sansf", 8.5, 400, "muted", c)
                    ),
                    None => body,
                }
            }
            None => format!(
                "#{}",
                txt("sansf", 9.0, 400, "muted", "[image unavailable]")
            ),
        },
    }
}

fn chart_card(title: Option<&str>, asset: &str, caption: Option<&str>) -> String {
    let title_arg = title.map_or("none".to_string(), |t| format!("\"{}\"", esc(t)));
    let cap = caption
        .map(|c| format!("\n  #v(7pt)\n  #{}", styled("sansf", 8.5, 400, "muted", c)))
        .unwrap_or_default();
    format!("#card({title_arg})[\n  #image(\"{asset}\", width: 100%){cap}\n]")
}

fn table_block(columns: &[String], rows: &[Vec<Value>], caption: Option<&str>) -> String {
    let cols: Vec<String> = if !columns.is_empty() {
        columns.to_vec()
    } else {
        match rows.iter().map(Vec::len).max() {
            Some(n) if n > 0 => (1..=n).map(|i| format!("Column {i}")).collect(),
            _ => return format!("#{}", txt("sansf", 9.0, 400, "muted", "No data")),
        }
    };
    let n = cols.len();
    let header = cols
        .iter()
        .map(|c| {
            format!(
                "text(font: sansf, weight: 700, fill: primary)[#\"{}\"]",
                esc(c)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let cells = rows
        .iter()
        .map(|row| {
            (0..n)
                .map(|i| {
                    let s = row.get(i).map(cell_text).unwrap_or_default();
                    format!("text(font: seriff, fill: ink)[{}]", inline(&s))
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .collect::<Vec<_>>()
        .join(",\n  ");
    let cap = caption
        .map(|c| format!("\n#v(6pt)\n#{}", styled("sansf", 8.5, 400, "muted", c)))
        .unwrap_or_default();
    format!(
        "#table(columns: {n}, inset: (x: 8pt, y: 6pt), stroke: 0.5pt + hair, align: left,\n  \
         table.header({header}),\n  {cells}\n){cap}"
    )
}

pub(crate) fn chart_fallback(intent: &super::ir::ChartIntent) -> (Vec<String>, Vec<Vec<Value>>) {
    let xcol = intent.x.clone().unwrap_or_else(|| "Category".into());
    let ycol = intent.y.clone().unwrap_or_else(|| "Value".into());
    let rows = intent
        .data
        .iter()
        .filter_map(|r| {
            let obj = r.as_object()?;
            Some(vec![
                obj.get(&xcol).cloned().unwrap_or(Value::Null),
                obj.get(&ycol).cloned().unwrap_or(Value::Null),
            ])
        })
        .collect();
    (vec![xcol, ycol], rows)
}

fn preamble(theme: &Theme) -> String {
    let p = &theme.palette;
    // Each text family is a fallback chain ending in "Noto Emoji" (bundled,
    // monochrome) so emoji a model emits render as glyphs instead of tofu boxes.
    let lets = format!(
        "#let sansf = (\"{}\", \"Noto Emoji\")\n#let seriff = (\"{}\", \"Noto Emoji\")\n\
         #let ink = rgb(\"{}\")\n#let paper = rgb(\"{}\")\n#let panel = rgb(\"{}\")\n\
         #let muted = rgb(\"{}\")\n#let hair = rgb(\"{}\")\n#let primary = rgb(\"{}\")\n\
         #let coverbg = rgb(\"{}\")\n\
         #let secondary = rgb(\"{}\")\n#let accent = rgb(\"{}\")\n#let ondark = rgb(\"{}\")\n\
         #set text(font: seriff, size: 10.5pt, fill: ink, hyphenate: false)\n\
         #set par(justify: false, leading: 0.72em, spacing: 0.9em)\n",
        theme.sans,
        theme.serif,
        p.ink,
        p.paper,
        p.panel,
        p.muted,
        p.hairline,
        p.primary,
        p.cover_bg,
        p.secondary,
        p.accent,
        p.on_dark,
    );
    let helpers = r##"#let statcard(value, label, col) = block(
  width: 100%, fill: panel, radius: 9pt, inset: (x: 13pt, y: 15pt), stroke: 0.6pt + hair,
)[
  #text(font: sansf, size: 21pt, weight: 800, fill: col)[#value]
  #v(-2pt)
  #text(font: sansf, size: 8pt, weight: 600, fill: muted)[#upper(label)]
]
#let sectionhead(title) = {
  v(10pt)
  text(font: sansf, size: 15pt, weight: 700, fill: primary)[#title]
  v(3pt)
  line(length: 100%, stroke: 0.8pt + hair)
  v(6pt)
}
#let card(title, body) = block(
  width: 100%, fill: panel, radius: 9pt, inset: 14pt, stroke: 0.6pt + hair,
)[
  #if title != none {
    text(font: sansf, size: 13pt, weight: 700, fill: primary)[#title]
    v(7pt)
  }
  #body
]
#let prose(s) = text(font: seriff, size: 10.5pt, fill: ink)[#s]
"##;
    format!("{lets}{helpers}")
}

fn dark_kicker(meta_kicker: Option<&str>) -> String {
    meta_kicker
        .map(|k| {
            format!(
                "#{}\n",
                txt("sansf", 10.0, 700, "accent", &k.to_uppercase())
            )
        })
        .unwrap_or_default()
}

fn report_doc(report: &Report, theme: &Theme, body: &str, logo: &str) -> String {
    let page = format!(
        "#set page(paper: \"a4\", margin: (x: 2.3cm, top: 2.4cm, bottom: 1.9cm), fill: paper, footer: context {{\n  \
         set text(font: sansf, size: 8pt, fill: muted)\n  \
         grid(columns: (1fr, auto), upper(\"{}\"), counter(page).display())\n}})\n",
        esc(&report.title)
    );
    let subtitle = report
        .subtitle
        .as_deref()
        .map(|s| format!("#v(16pt)\n#{}\n", txt("seriff", 14.0, 400, "ondark", s)))
        .unwrap_or_default();
    let footer = byline(report, "1.8fr");
    let cover = format!(
        "#page(fill: coverbg, margin: (x: 2.6cm, top: 3.0cm, bottom: 2.6cm), footer: none)[\n\
         {logo}{kicker}#v(1.5fr)\n#par(justify: false)[#{title}]\n#v(12pt)\n\
         #line(length: 80pt, stroke: 3pt + accent)\n{subtitle}{footer}\n]\n",
        kicker = dark_kicker(report.meta.kicker.as_deref()),
        title = txt("sansf", 40.0, 800, "ondark", &report.title),
    );
    format!("{}{page}{cover}{body}\n", preamble(theme))
}

fn infographic_doc(report: &Report, theme: &Theme, body: &str, logo: &str) -> String {
    let page = "#set page(width: 22cm, height: auto, margin: 0pt, fill: paper)\n";
    let subtitle = report
        .subtitle
        .as_deref()
        .map(|s| format!("\n  #v(7pt)\n  #{}", txt("seriff", 12.5, 400, "ondark", s)))
        .unwrap_or_default();
    let header = format!(
        "#block(width: 100%, fill: coverbg, inset: (x: 1.7cm, top: 1.5cm, bottom: 1.5cm))[\n  \
         {logo}{kicker}#par(justify: false)[#{title}]{subtitle}\n]\n",
        kicker = dark_kicker(report.meta.kicker.as_deref()),
        title = txt("sansf", 33.0, 800, "ondark", &report.title),
    );
    format!(
        "{}{page}{header}#block(inset: (x: 1.7cm, top: 1.2cm, bottom: 1.3cm))[\n{body}\n]\n",
        preamble(theme)
    )
}

fn byline(report: &Report, gap: &str) -> String {
    let author = report.meta.author.as_deref();
    let date = report.meta.date.as_deref();
    if author.is_none() && date.is_none() {
        return String::new();
    }
    let cell =
        |s: Option<&str>| s.map_or("[]".to_string(), |v| txt("sansf", 10.5, 400, "ondark", v));
    format!(
        "#v({gap})\n#grid(columns: (1fr, auto), {}, {})\n",
        cell(author),
        cell(date)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::report::theme;
    use serde_json::json;

    fn report(v: Value) -> Report {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn builds_report_with_cover_and_blocks() {
        let r = report(json!({
            "title": "Coffee", "subtitle": "Sub", "meta": { "kicker": "Industry", "author": "Me", "date": "2026" },
            "sections": [{ "heading": "Grown", "blocks": [
                { "type": "prose", "text": "Brazil leads." },
                { "type": "stats", "items": [{ "value": "$1B", "label": "Market" }] }
            ] }]
        }));
        let doc = build(&r, theme::resolve(None), &Default::default(), None);
        assert!(doc.source.contains("#set page(paper: \"a4\""));
        assert!(doc.source.contains("counter(page).display()"));
        assert!(doc.source.contains("#sectionhead(\"Grown\")"));
        assert!(doc.source.contains("#prose[#\"Brazil leads.\"]"));
        assert!(doc.source.contains("statcard(\"$1B\", \"Market\", accent)"));
        assert!(doc.source.contains("#upper(\"INDUSTRY\")") || doc.source.contains("INDUSTRY"));
    }

    #[test]
    fn code_block_keeps_its_newlines_and_carries_the_language() {
        let r = report(json!({
            "title": "T", "sections": [{ "blocks": [
                { "type": "code", "text": "fn main() {\n    ok(\"x\");\n}", "language": "rust" },
                { "type": "code", "text": "no lang" }
            ] }]
        }));
        let doc = build(&r, theme::resolve(None), &Default::default(), None);
        assert!(doc.source.contains("lang: \"rust\""));
        assert!(
            doc.source.contains(
                "#raw(block: true, lang: \"rust\", \"fn main() {\\n    ok(\\\"x\\\");\\n}\")"
            ),
            "newlines survive as escapes and quotes are escaped: {}",
            doc.source
        );
        assert!(
            doc.source.contains("#raw(block: true, \"no lang\")"),
            "no lang argument when unset"
        );
    }

    #[test]
    fn esc_verbatim_preserves_layout_where_esc_flattens_it() {
        assert_eq!(esc("a\nb\tc"), "a b c");
        assert_eq!(esc_verbatim("a\nb\tc"), "a\\nb\\tc");
        assert_eq!(esc_verbatim("q\"\\p\r\n"), "q\\\"\\\\p\\n");
    }

    #[test]
    fn text_families_fall_back_to_noto_emoji() {
        let r = report(json!({
            "title": "T", "sections": [{ "blocks": [{ "type": "prose", "text": "x" }] }]
        }));
        let doc = build(&r, theme::resolve(None), &Default::default(), None);
        assert!(
            doc.source
                .contains("#let sansf = (\"Lato\", \"Noto Emoji\")"),
            "sans family chains to the emoji fallback"
        );
        assert!(
            doc.source
                .contains("#let seriff = (\"IBM Plex Serif\", \"Noto Emoji\")"),
            "serif family chains to the emoji fallback"
        );
    }

    #[test]
    fn infographic_uses_auto_height_single_page() {
        let r = report(json!({
            "title": "T", "format": "infographic",
            "sections": [{ "blocks": [{ "type": "prose", "text": "x" }] }]
        }));
        let doc = build(&r, theme::resolve(None), &Default::default(), None);
        assert!(doc.source.contains("height: auto"));
        assert!(!doc.source.contains("counter(page)"));
    }

    #[test]
    fn diagram_block_emits_an_svg_asset_and_image_ref() {
        let r = report(json!({
            "title": "T",
            "sections": [{ "blocks": [{ "type": "diagram", "title": "Pipeline", "caption": "How it flows",
                "spec": { "kind": "flow",
                    "nodes": [{ "id": "a", "label": "Start", "group": "Ops" },
                              { "id": "b", "label": "Finish" }],
                    "edges": [{ "from": "a", "to": "b", "label": "then" }] } }] }]
        }));
        let doc = build(&r, theme::resolve(None), &Default::default(), None);
        assert_eq!(doc.assets.len(), 1);
        assert_eq!(doc.assets[0].name, "diagram_0_0.svg");
        assert!(doc.source.contains("#image(\"diagram_0_0.svg\""));
        let svg = String::from_utf8(doc.assets[0].bytes.clone()).expect("utf8 svg");
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains("Start") && svg.contains("Finish"));
        // A grouped node takes its lane hue even inside a report.
        assert!(svg.contains("#2a78d6"));
    }

    /// A malformed spec must degrade to legible prose, never vanish.
    #[test]
    fn an_unrenderable_diagram_degrades_to_prose() {
        let r = report(json!({
            "title": "T",
            "sections": [{ "blocks": [{ "type": "diagram", "spec": { "kind": "mindmap" } }] }]
        }));
        let doc = build(&r, theme::resolve(None), &Default::default(), None);
        assert!(doc.assets.is_empty());
        assert!(doc.source.contains("Diagram unavailable"));
    }

    #[test]
    fn chart_block_emits_an_svg_asset_and_image_ref() {
        let r = report(json!({
            "title": "T",
            "sections": [{ "blocks": [{ "type": "chart", "title": "Producers",
                "intent": { "kind": "bar", "x": "k", "y": "v", "data": [{ "k": "A", "v": 1 }] } }] }]
        }));
        let doc = build(&r, theme::resolve(None), &Default::default(), None);
        assert_eq!(doc.assets.len(), 1);
        assert_eq!(doc.assets[0].name, "chart_0_0.svg");
        assert!(doc.source.contains("#image(\"chart_0_0.svg\""));
    }

    #[test]
    fn empty_chart_degrades_to_a_table_with_no_asset() {
        let r = report(json!({
            "title": "T",
            "sections": [{ "blocks": [{ "type": "chart", "caption": "cap",
                "intent": { "kind": "bar", "x": "k", "y": "v", "data": [] } }] }]
        }));
        let doc = build(&r, theme::resolve(None), &Default::default(), None);
        assert!(doc.assets.is_empty(), "no chart asset for empty data");
        assert!(doc.source.contains("#table(columns: 2"));
    }

    #[test]
    fn user_strings_are_escaped_against_markup_and_quote_injection() {
        let r = report(json!({
            "title": "Say \"hi\" * _x_ #y",
            "sections": [{ "blocks": [{ "type": "prose", "text": "danger \" ] \\ backslash" }] }]
        }));
        let doc = build(&r, theme::resolve(None), &Default::default(), None);
        assert!(doc.source.contains(r#"\"hi\""#), "quotes escaped in title");
        assert!(doc.source.contains(r#"danger \" ] \\ backslash"#));
        assert!(
            !doc.source.contains("danger \" ]"),
            "raw quote must not leak"
        );
    }

    #[test]
    fn inline_parses_a_safe_markdown_subset() {
        assert_eq!(inline("**bold**"), "#strong[#\"bold\"]");
        assert_eq!(inline("*italic*"), "#emph[#\"italic\"]");
        assert_eq!(inline("a `code` b"), "#\"a \"#raw(\"code\")#\" b\"");
        assert_eq!(inline("**A:** body"), "#strong[#\"A:\"]#\" body\"");
        assert_eq!(inline("plain text"), "#\"plain text\"");
        assert_eq!(inline(""), "#\"\"");
        // Inner text of a span is still escaped — no markup/quote injection.
        assert_eq!(inline("**say \"hi\"**"), "#strong[#\"say \\\"hi\\\"\"]");
    }

    #[test]
    fn inline_leaves_identifiers_and_arithmetic_literal() {
        // `_` is never emphasis, so snake_case survives intact.
        assert_eq!(
            inline("call worker_read_sources now"),
            "#\"call worker_read_sources now\""
        );
        // `*` with a space on the inner edge is literal (no flanking match).
        assert_eq!(inline("3 * 4 = 12"), "#\"3 * 4 = 12\"");
        // Unmatched delimiters stay literal.
        assert_eq!(inline("**unclosed"), "#\"**unclosed\"");
    }

    #[test]
    fn preamble_disables_hyphenation_and_justification() {
        let r = report(json!({
            "title": "T", "sections": [{ "blocks": [{ "type": "prose", "text": "x" }] }]
        }));
        let doc = build(&r, theme::resolve(None), &Default::default(), None);
        assert!(
            doc.source.contains("hyphenate: false"),
            "auto-hyphenation off so identifiers never break mid-word"
        );
        assert!(
            doc.source.contains("#set par(justify: false"),
            "ragged-right body avoids loose-spacing rivers"
        );
    }

    #[test]
    fn image_block_emits_aligned_image_with_caption_or_placeholder() {
        let r = report(json!({
            "title": "T",
            "sections": [{ "blocks": [
                { "type": "image", "artifact_id": "art_x", "width": 60, "align": "left", "caption": "c" },
                { "type": "image", "artifact_id": "missing" }
            ] }]
        }));
        let mut images = Images::new();
        images.insert(
            "art_x".into(),
            ResolvedImage {
                ext: "png",
                bytes: vec![1, 2, 3],
            },
        );
        let doc = build(&r, theme::resolve(None), &images, None);
        assert!(doc
            .source
            .contains("#align(left)[#image(\"img_0_0.png\", width: 60%)]"));
        assert_eq!(
            doc.assets
                .iter()
                .filter(|a| a.name.starts_with("img_"))
                .count(),
            1,
            "only the resolved image becomes an asset"
        );
        assert!(
            doc.source.contains("[image unavailable]"),
            "missing image degrades"
        );
    }

    #[test]
    fn logo_is_staged_and_placed_on_the_cover() {
        let r = report(json!({
            "title": "T", "sections": [{ "blocks": [{ "type": "prose", "text": "x" }] }]
        }));
        let logo = ResolvedImage {
            ext: "svg",
            bytes: b"<svg/>".to_vec(),
        };
        let doc = build(&r, theme::resolve(None), &Default::default(), Some(&logo));
        assert!(
            doc.assets.iter().any(|a| a.name == "logo.svg"),
            "logo staged as asset"
        );
        assert!(
            doc.source.contains("#image(\"logo.svg\", height: 30pt)"),
            "logo placed"
        );
    }

    #[test]
    fn cover_uses_cover_bg_binding_distinct_from_primary_on_dark_theme() {
        let r = report(json!({
            "title": "T", "sections": [{ "blocks": [{ "type": "prose", "text": "x" }] }]
        }));
        let doc = build(&r, theme::resolve(Some("noir")), &Default::default(), None);
        assert!(doc.source.contains("#let coverbg = rgb(\"#12161C\")"));
        assert!(doc.source.contains("#let primary = rgb(\"#EDEFF2\")"));
        assert!(doc.source.contains("#page(fill: coverbg"));
    }

    #[test]
    fn table_pads_short_rows_and_handles_no_data() {
        assert!(table_block(&[], &[], None).contains("No data"));
        let out = table_block(
            &["A".into(), "B".into()],
            &[vec![json!(1)], vec![json!(2), json!(3)]],
            Some("c"),
        );
        assert!(out.contains("#table(columns: 2"));
        assert!(out.contains("table.header"));
    }
}
