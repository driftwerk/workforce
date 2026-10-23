//! The resolved, format-neutral document the native (editable) backends emit.
//!
//! `generate_report` renders [`ir::Report`] to a Typst PDF; the native backends
//! instead lower the same IR into a [`NativeDoc`] — a flat list of sections of
//! resolved blocks where charts and images are already PNG bytes — and write it
//! as editable OOXML (`.docx`, `.pptx`). Google Docs/Slides are obtained by
//! uploading those bytes to Drive with server-side conversion, so they need no
//! backend of their own.
//!
//! Lowering owns every design decision (theme colours, chart rasterization,
//! image sizing); a backend only walks the blocks and writes its container.

mod docx;
mod ooxml;
mod pptx;

use eyre::Result;

use super::chart;
use super::ir::{AccentTone, Align, Block, Meta, Report, Section};
use super::render::rasterize_svg;
use super::theme::Theme;
use super::typst_gen::{cell_text, chart_fallback, Images};

/// The editable native container an export targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeFormat {
    Docx,
    Pptx,
}

impl NativeFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "docx" => Some(Self::Docx),
            "pptx" => Some(Self::Pptx),
            _ => None,
        }
    }

    pub fn ext(self) -> &'static str {
        match self {
            Self::Docx => "docx",
            Self::Pptx => "pptx",
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            Self::Docx => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            Self::Pptx => {
                "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            }
        }
    }

    fn render(self, doc: &NativeDoc) -> Result<Vec<u8>> {
        match self {
            Self::Docx => docx::write(doc),
            Self::Pptx => pptx::write(doc),
        }
    }
}

/// Lower a report to native-document bytes in the requested format.
pub async fn render(
    report: &Report,
    images: &Images,
    theme: &Theme,
    format: NativeFormat,
) -> Result<Vec<u8>> {
    format.render(&build(report, images, theme).await?)
}

/// A report lowered to resolved, container-agnostic content.
pub struct NativeDoc {
    pub title: String,
    pub subtitle: Option<String>,
    pub meta: Meta,
    pub theme: Theme,
    pub sections: Vec<NativeSection>,
}

pub struct NativeSection {
    pub heading: Option<String>,
    pub blocks: Vec<NativeBlock>,
}

pub enum NativeBlock {
    /// `1` = section heading, `2` = inline heading, `3` = chart title.
    Heading {
        level: u8,
        text: String,
    },
    Paragraph {
        text: String,
    },
    /// Preformatted text: rendered monospaced, line breaks preserved.
    Code {
        text: String,
    },
    Stats {
        items: Vec<NativeStat>,
    },
    Table {
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    List {
        title: Option<String>,
        items: Vec<NativeListItem>,
    },
    Image(NativeImage),
}

pub struct NativeStat {
    pub value: String,
    pub label: String,
    /// Resolved `#RRGGBB` accent colour for the value.
    pub color: String,
}

pub struct NativeListItem {
    pub lead: Option<String>,
    pub text: String,
}

pub struct NativeImage {
    pub bytes: Vec<u8>,
    pub format: ImageFormat,
    pub width_px: u32,
    pub height_px: u32,
    /// Display width as a percentage (1–100) of the content column.
    pub width_pct: u8,
    pub align: Align,
    pub caption: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Gif,
}

impl ImageFormat {
    fn from_ext(ext: &str) -> Option<Self> {
        match ext {
            "png" => Some(Self::Png),
            "jpg" | "jpeg" => Some(Self::Jpeg),
            "gif" => Some(Self::Gif),
            _ => None,
        }
    }

    fn ext(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Gif => "gif",
        }
    }
}

/// Default embed box when an image's intrinsic size can't be sniffed — a 4:3
/// rectangle so the aspect-preserving height stays sane.
const DEFAULT_IMAGE_SIZE: (u32, u32) = (800, 600);

/// Lower a validated [`Report`] into a [`NativeDoc`]: resolve theme accents,
/// rasterize charts, and normalize image artifacts to PNG/JPEG/GIF bytes.
pub async fn build(report: &Report, images: &Images, theme: &Theme) -> Result<NativeDoc> {
    let mut sections = Vec::with_capacity(report.sections.len());
    for section in &report.sections {
        sections.push(build_section(section, images, theme).await?);
    }
    Ok(NativeDoc {
        title: report.title.clone(),
        subtitle: report.subtitle.clone(),
        meta: report.meta.clone(),
        theme: theme.clone(),
        sections,
    })
}

async fn build_section(section: &Section, images: &Images, theme: &Theme) -> Result<NativeSection> {
    let mut blocks = Vec::new();
    for block in &section.blocks {
        lower_block(block, images, theme, &mut blocks).await?;
    }
    Ok(NativeSection {
        heading: section.heading.clone(),
        blocks,
    })
}

async fn lower_block(
    block: &Block,
    images: &Images,
    theme: &Theme,
    out: &mut Vec<NativeBlock>,
) -> Result<()> {
    match block {
        Block::Prose { text } => out.push(NativeBlock::Paragraph { text: text.clone() }),
        Block::Heading { text } => out.push(NativeBlock::Heading {
            level: 2,
            text: text.clone(),
        }),
        Block::Code { text, .. } => out.push(NativeBlock::Code { text: text.clone() }),
        Block::Stats { items } => out.push(NativeBlock::Stats {
            items: items
                .iter()
                .map(|it| NativeStat {
                    value: it.value.clone(),
                    label: it.label.clone(),
                    color: accent_hex(it.accent, theme),
                })
                .collect(),
        }),
        Block::Table {
            columns,
            rows,
            caption,
        } => push_table(
            out,
            columns.clone(),
            stringify_rows(rows),
            caption.as_deref(),
        ),
        Block::List { title, items } => out.push(NativeBlock::List {
            title: title.clone(),
            items: items
                .iter()
                .map(|it| NativeListItem {
                    lead: it.lead.clone(),
                    text: it.text.clone(),
                })
                .collect(),
        }),
        Block::Image {
            artifact_id,
            caption,
            width,
            align,
        } => out.push(resolve_image(images, artifact_id, *width, *align, caption.clone()).await),
        Block::Diagram {
            spec,
            title,
            caption,
        } => {
            if let Some(text) = title {
                out.push(NativeBlock::Heading {
                    level: 3,
                    text: text.clone(),
                });
            }
            match super::diagram_svg(spec, theme) {
                Ok(svg) => {
                    let png = rasterize_svg(&svg).await?;
                    let (width_px, height_px) = ooxml::image_dimensions(&png, ImageFormat::Png)
                        .unwrap_or(DEFAULT_IMAGE_SIZE);
                    out.push(NativeBlock::Image(NativeImage {
                        bytes: png,
                        format: ImageFormat::Png,
                        width_px,
                        height_px,
                        width_pct: 100,
                        align: Align::Center,
                        caption: caption.clone(),
                    }));
                }
                Err(reason) => out.push(NativeBlock::Paragraph {
                    text: format!("Diagram unavailable: {reason}"),
                }),
            }
        }
        Block::Chart {
            intent,
            title,
            caption,
        } => {
            if let Some(text) = title {
                out.push(NativeBlock::Heading {
                    level: 3,
                    text: text.clone(),
                });
            }
            let charted = match chart::render(intent, theme) {
                Ok(svg) => {
                    let png = rasterize_svg(&svg).await?;
                    let (width_px, height_px) = ooxml::image_dimensions(&png, ImageFormat::Png)
                        .unwrap_or(DEFAULT_IMAGE_SIZE);
                    out.push(NativeBlock::Image(NativeImage {
                        bytes: png,
                        format: ImageFormat::Png,
                        width_px,
                        height_px,
                        width_pct: 100,
                        align: Align::Center,
                        caption: caption.clone(),
                    }));
                    true
                }
                Err(_) => false,
            };
            // The image carries the caption when it renders; otherwise the data
            // table — which is all that remains — must keep it.
            let (cols, rows) = chart_fallback(intent);
            let table_caption = (!charted).then_some(caption.as_deref()).flatten();
            push_table(out, cols, stringify_rows(&rows), table_caption);
        }
    }
    Ok(())
}

/// Resolve an `image` block's artifact to a native image, rasterizing SVG
/// charts/marks to PNG. An unresolvable artifact degrades to a placeholder, as
/// in the Typst path.
async fn resolve_image(
    images: &Images,
    artifact_id: &str,
    width: Option<u8>,
    align: Align,
    caption: Option<String>,
) -> NativeBlock {
    let Some(img) = images.get(artifact_id) else {
        return NativeBlock::Paragraph {
            text: "[image unavailable]".into(),
        };
    };
    let resolved = if img.ext == "svg" {
        rasterize_svg(&String::from_utf8_lossy(&img.bytes))
            .await
            .ok()
            .map(|png| (png, ImageFormat::Png))
    } else {
        ImageFormat::from_ext(img.ext).map(|fmt| (img.bytes.clone(), fmt))
    };
    let Some((bytes, format)) = resolved else {
        return NativeBlock::Paragraph {
            text: "[image unavailable]".into(),
        };
    };
    let (width_px, height_px) =
        ooxml::image_dimensions(&bytes, format).unwrap_or(DEFAULT_IMAGE_SIZE);
    NativeBlock::Image(NativeImage {
        bytes,
        format,
        width_px,
        height_px,
        width_pct: width.unwrap_or(100).clamp(1, 100),
        align,
        caption,
    })
}

/// Push a table, synthesizing headers when columns are omitted and dropping an
/// entirely empty table (mirrors the Typst table block).
fn push_table(
    out: &mut Vec<NativeBlock>,
    columns: Vec<String>,
    rows: Vec<Vec<String>>,
    caption: Option<&str>,
) {
    let columns = if !columns.is_empty() {
        columns
    } else {
        match rows.iter().map(Vec::len).max() {
            Some(n) if n > 0 => (1..=n).map(|i| format!("Column {i}")).collect(),
            _ => return,
        }
    };
    out.push(NativeBlock::Table { columns, rows });
    if let Some(c) = caption.filter(|c| !c.is_empty()) {
        out.push(NativeBlock::Paragraph {
            text: c.to_string(),
        });
    }
}

fn stringify_rows(rows: &[Vec<serde_json::Value>]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| row.iter().map(cell_text).collect())
        .collect()
}

fn accent_hex(tone: AccentTone, theme: &Theme) -> String {
    let p = &theme.palette;
    match tone {
        AccentTone::Accent => &p.accent,
        AccentTone::Primary => &p.primary,
        AccentTone::Secondary => &p.secondary,
        AccentTone::Ink => &p.ink,
    }
    .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::report::theme;
    use serde_json::json;

    fn lower(value: serde_json::Value) -> NativeDoc {
        let report: Report = serde_json::from_value(value).unwrap();
        let images = Images::new();
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(build(
                &report,
                &images,
                theme::resolve(report.theme.as_deref()),
            ))
            .unwrap()
    }

    #[test]
    fn lowers_text_blocks_without_a_renderer() {
        let doc = lower(json!({
            "title": "Q3", "subtitle": "Sub", "theme": "slate",
            "meta": { "author": "Me" },
            "sections": [{ "heading": "Overview", "blocks": [
                { "type": "prose", "text": "Body." },
                { "type": "heading", "text": "Detail" },
                { "type": "stats", "items": [{ "value": "42", "label": "Count", "accent": "secondary" }] },
                { "type": "table", "columns": ["A", "B"], "rows": [[1, "x"], [2, "y"]] },
                { "type": "list", "items": [{ "lead": "One.", "text": "first" }] }
            ] }]
        }));
        assert_eq!(doc.title, "Q3");
        assert_eq!(doc.subtitle.as_deref(), Some("Sub"));
        assert_eq!(doc.theme.name, "slate");
        let blocks = &doc.sections[0].blocks;
        assert!(matches!(&blocks[0], NativeBlock::Paragraph { text } if text == "Body."));
        assert!(matches!(&blocks[1], NativeBlock::Heading { level: 2, .. }));
        let NativeBlock::Stats { items } = &blocks[2] else {
            panic!("stats");
        };
        // slate secondary accent.
        assert_eq!(items[0].color, "#3B5266");
        let NativeBlock::Table { columns, rows } = &blocks[3] else {
            panic!("table");
        };
        assert_eq!(columns, &["A", "B"]);
        assert_eq!(rows, &vec![vec!["1", "x"], vec!["2", "y"]]);
        assert!(matches!(&blocks[4], NativeBlock::List { .. }));
    }

    #[test]
    fn code_lowers_verbatim_for_the_native_containers() {
        let doc = lower(json!({
            "title": "T",
            "sections": [{ "blocks": [
                { "type": "code", "text": "line one\n    line two", "language": "rust" }
            ] }]
        }));
        let NativeBlock::Code { text } = &doc.sections[0].blocks[0] else {
            panic!("expected a code block");
        };
        assert_eq!(
            text, "line one\n    line two",
            "docx/pptx split on newlines themselves, so the text stays whole"
        );
    }

    #[test]
    fn table_without_columns_synthesizes_headers_and_empty_is_dropped() {
        let doc = lower(json!({
            "title": "T",
            "sections": [{ "blocks": [
                { "type": "table", "rows": [["a", "b"]] },
                { "type": "table", "rows": [] }
            ] }]
        }));
        let blocks = &doc.sections[0].blocks;
        assert_eq!(blocks.len(), 1, "the empty table is dropped");
        let NativeBlock::Table { columns, .. } = &blocks[0] else {
            panic!("table");
        };
        assert_eq!(columns, &["Column 1", "Column 2"]);
    }

    #[test]
    fn chart_lowers_to_image_plus_data_table_when_renderer_present() {
        if std::process::Command::new("typst")
            .arg("--version")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            eprintln!("skipping: `typst` binary not on PATH");
            return;
        }
        let doc = lower(json!({
            "title": "T",
            "sections": [{ "blocks": [
                { "type": "chart", "title": "Producers", "caption": "m bags",
                  "intent": { "kind": "bar", "x": "c", "y": "v",
                              "data": [{ "c": "Brazil", "v": 66 }, { "c": "Vietnam", "v": 29 }] } }
            ] }]
        }));
        let blocks = &doc.sections[0].blocks;
        // title heading, chart image, then the editable data table.
        assert!(matches!(&blocks[0], NativeBlock::Heading { level: 3, .. }));
        let NativeBlock::Image(img) = &blocks[1] else {
            panic!("chart image");
        };
        assert_eq!(img.format, ImageFormat::Png);
        assert!(img.width_px > 0 && img.height_px > 0);
        assert_eq!(img.caption.as_deref(), Some("m bags"));
        let NativeBlock::Table { columns, rows } = &blocks[2] else {
            panic!("data table");
        };
        assert_eq!(columns, &["c", "v"]);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn chart_without_numeric_data_degrades_to_a_captioned_table() {
        // No renderer needed: a non-numeric chart never reaches rasterization.
        let doc = lower(json!({
            "title": "T",
            "sections": [{ "blocks": [
                { "type": "chart", "title": "Trend", "caption": "context",
                  "intent": { "kind": "bar", "x": "k", "y": "v",
                              "data": [{ "k": "A", "v": "n/a" }] } }
            ] }]
        }));
        let blocks = &doc.sections[0].blocks;
        assert!(matches!(&blocks[0], NativeBlock::Heading { level: 3, .. }));
        assert!(
            !blocks.iter().any(|b| matches!(b, NativeBlock::Image(_))),
            "a chart with no numeric data emits no image"
        );
        assert!(matches!(&blocks[1], NativeBlock::Table { .. }));
        // The caption survives on the fallback table rather than being dropped.
        assert!(
            matches!(blocks.last(), Some(NativeBlock::Paragraph { text }) if text == "context"),
            "caption is preserved on the data table"
        );
    }

    #[test]
    fn unresolved_image_degrades_to_placeholder() {
        let doc = lower(json!({
            "title": "T",
            "sections": [{ "blocks": [
                { "type": "image", "artifact_id": "art_missing" }
            ] }]
        }));
        assert!(matches!(
            &doc.sections[0].blocks[0],
            NativeBlock::Paragraph { text } if text == "[image unavailable]"
        ));
    }
}
