//! Render a built `Document` to bytes by shelling the `typst` binary in an
//! isolated temp workspace. Reports become multi-page PDFs; infographics become
//! a single PNG (so they preview inline). A hard timeout guarantees one render
//! can never wedge a turn.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use eyre::{eyre, Result, WrapErr};
use tokio::process::Command;

use super::fonts::font_path;
use super::ir::Format;
use super::typst_gen::Document;

const RENDER_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OUTPUT_BYTES: usize = 25 * 1024 * 1024;

/// Pixel density for chart rasters embedded in native documents — high enough
/// that a chart stays crisp when scaled to a Word/Slides content column.
const RASTER_PPI: u16 = 200;

pub fn media_type(format: Format) -> &'static str {
    match format {
        Format::Report => "application/pdf",
        Format::Infographic => "image/png",
    }
}

pub fn extension(format: Format) -> &'static str {
    match format {
        Format::Report => "pdf",
        Format::Infographic => "png",
    }
}

pub async fn render(doc: &Document, format: Format) -> Result<Vec<u8>> {
    let workspace = tempfile::TempDir::new().wrap_err("creating render workspace")?;
    let root = workspace.path();
    for asset in &doc.assets {
        std::fs::write(root.join(&asset.name), &asset.bytes).wrap_err("writing chart asset")?;
    }
    std::fs::write(root.join("main.typ"), &doc.source).wrap_err("writing typst source")?;
    compile(root, extension(format), None).await
}

/// Rasterize a standalone chart SVG to PNG bytes through the *same* `typst`
/// binary the report path uses, so a chart embedded in a native document is
/// pixel-identical to the chart in the PDF. The page is sized to the SVG's
/// intrinsic `viewBox` so the raster is tightly cropped with no margin.
pub async fn rasterize_svg(svg: &str) -> Result<Vec<u8>> {
    let (w, h) = svg_viewbox(svg).unwrap_or((720.0, 720.0));
    let workspace = tempfile::TempDir::new().wrap_err("creating raster workspace")?;
    let root = workspace.path();
    std::fs::write(root.join("chart.svg"), svg).wrap_err("writing chart svg")?;
    let source = format!(
        "#set page(width: {w}pt, height: {h}pt, margin: 0pt)\n#image(\"chart.svg\", width: 100%, height: 100%)\n"
    );
    std::fs::write(root.join("main.typ"), source).wrap_err("writing raster source")?;
    compile(root, "png", Some(RASTER_PPI)).await
}

/// Compile the `main.typ` already staged under `root` to `out.<ext>` and return
/// the bytes. Shared by the report renderer and the chart rasterizer.
async fn compile(root: &Path, ext: &str, ppi: Option<u16>) -> Result<Vec<u8>> {
    let fonts = font_path()?;
    let main = root.join("main.typ");
    let out = root.join(format!("out.{ext}"));

    let mut command = Command::new("typst");
    command
        .arg("compile")
        .arg("--root")
        .arg(root)
        .arg("--font-path")
        .arg(fonts)
        .arg("--format")
        .arg(ext);
    if let Some(ppi) = ppi {
        command.arg("--ppi").arg(ppi.to_string());
    }
    command
        .arg(&main)
        .arg(&out)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output = tokio::time::timeout(RENDER_TIMEOUT, command.output())
        .await
        .map_err(|_| eyre!("typst render timed out after {}s", RENDER_TIMEOUT.as_secs()))?
        .map_err(|e| eyre!("could not run the `typst` renderer (is it installed?): {e}"))?;

    if !output.status.success() {
        return Err(eyre!(
            "typst compile failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let bytes = std::fs::read(&out).wrap_err("reading rendered document")?;
    if bytes.len() > MAX_OUTPUT_BYTES {
        return Err(eyre!(
            "rendered output is {:.1} MB, over the {} MB limit; use fewer sections",
            bytes.len() as f64 / 1_048_576.0,
            MAX_OUTPUT_BYTES / 1_048_576
        ));
    }
    Ok(bytes)
}

/// Parse `width`/`height` (in user units = points) from an SVG `viewBox`.
fn svg_viewbox(svg: &str) -> Option<(f64, f64)> {
    let start = svg.find("viewBox")?;
    let open = svg[start..].find(['\'', '"'])? + start + 1;
    let rest = &svg[open..];
    let close = rest.find(['\'', '"'])?;
    let mut nums = rest[..close]
        .split_whitespace()
        .filter_map(|n| n.parse::<f64>().ok());
    let (_, _, w, h) = (nums.next()?, nums.next()?, nums.next()?, nums.next()?);
    (w > 0.0 && h > 0.0).then_some((w, h))
}

/// Whether the `typst` binary is on PATH.
///
/// Rendering shells out to it, so a test that actually rasterizes only works
/// where it is installed — it is on a dev laptop and NOT on the CI runner. Every
/// such test must skip when this is false; one that forgot took the hub's build
/// down for a day and a half, which meant no hub deploy at all, not merely a red
/// tick. Crate-visible so a new render test finds it instead of re-deriving it.
#[cfg(test)]
pub(crate) fn typst_available() -> bool {
    std::process::Command::new("typst")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::report::{theme, typst_gen};
    use serde_json::json;

    fn render_blocking(value: serde_json::Value) -> Result<Vec<u8>> {
        let report: crate::tools::report::ir::Report = serde_json::from_value(value).unwrap();
        let doc = typst_gen::build(
            &report,
            theme::resolve(report.theme.as_deref()),
            &Default::default(),
            None,
        );
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(render(&doc, report.format))
    }

    #[test]
    fn renders_a_report_pdf_and_an_infographic_png() {
        if !typst_available() {
            eprintln!("skipping: `typst` binary not on PATH");
            return;
        }
        let spec = |format: &str| {
            json!({
                "title": "Coffee", "subtitle": "Sub", "format": format,
                "meta": { "kicker": "Industry", "author": "Me", "date": "2026" },
                "sections": [{ "heading": "Overview", "blocks": [
                    { "type": "prose", "text": "Brazil leads & Vietnam follows." },
                    { "type": "stats", "items": [
                        { "value": "$138B", "label": "Market" },
                        { "value": "Brazil", "label": "Top", "accent": "secondary" }
                    ] },
                    { "type": "chart", "title": "Producers", "caption": "m bags",
                      "intent": { "kind": "bar", "x": "c", "y": "v",
                                  "data": [{ "c": "Brazil", "v": 66.4 }, { "c": "Vietnam", "v": 29 }] } },
                    { "type": "chart", "title": "Share",
                      "intent": { "kind": "donut", "x": "r", "y": "p",
                                  "data": [{ "r": "EU", "p": 32 }, { "r": "APAC", "p": 23 }] } },
                    { "type": "list", "title": "Outlook", "items": [
                        { "lead": "Climate.", "text": "Tight supply." }
                    ] },
                    // A `lang` typst may not know, quotes, and indentation — the
                    // shapes that would break the raw-block lowering.
                    { "type": "code", "language": "notalanguage",
                      "text": "fn main() {\n    say(\"hi\");\n}" }
                ] }]
            })
        };
        let pdf = render_blocking(spec("report")).unwrap();
        assert_eq!(&pdf[0..4], b"%PDF", "report is a PDF");
        let png = render_blocking(spec("infographic")).unwrap();
        assert_eq!(&png[0..4], b"\x89PNG", "infographic is a PNG");
    }

    #[test]
    fn renders_a_report_with_an_embedded_image_and_a_dark_theme() {
        if !typst_available() {
            eprintln!("skipping: `typst` binary not on PATH");
            return;
        }
        let report: crate::tools::report::ir::Report = serde_json::from_value(json!({
            "title": "Embedded", "theme": "noir",
            "sections": [{ "heading": "Logo", "blocks": [
                { "type": "image", "artifact_id": "art_x", "width": 40, "caption": "mark" },
                { "type": "chart", "title": "Trend",
                  "intent": { "kind": "line", "x": "y", "y": "v",
                              "data": [{ "y": "Q1", "v": 3 }, { "y": "Q2", "v": 8 }] } }
            ] }]
        }))
        .unwrap();
        let mut images = typst_gen::Images::new();
        images.insert(
            "art_x".into(),
            typst_gen::ResolvedImage {
                ext: "svg",
                bytes: b"<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 20 20'><circle cx='10' cy='10' r='9' fill='#888'/></svg>".to_vec(),
            },
        );
        let doc = typst_gen::build(&report, theme::resolve(Some("noir")), &images, None);
        let pdf = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(render(&doc, report.format))
            .unwrap();
        assert_eq!(&pdf[0..4], b"%PDF", "dark-theme report with image renders");
    }

    #[test]
    fn svg_viewbox_reads_width_and_height() {
        assert_eq!(
            svg_viewbox("<svg viewBox='0 0 720 360'></svg>"),
            Some((720.0, 360.0))
        );
        assert_eq!(
            svg_viewbox("<svg xmlns='x' viewBox=\"0 0 100 50\">"),
            Some((100.0, 50.0))
        );
        assert_eq!(svg_viewbox("<svg></svg>"), None);
        assert_eq!(svg_viewbox("<svg viewBox='0 0 0 0'></svg>"), None);
    }

    #[test]
    fn rasterizes_a_chart_svg_to_png() {
        if !typst_available() {
            eprintln!("skipping: `typst` binary not on PATH");
            return;
        }
        let svg = crate::tools::report::chart::render(
            &serde_json::from_value(json!({
                "kind": "bar", "x": "k", "y": "v",
                "data": [{ "k": "A", "v": 10 }, { "k": "B", "v": 20 }]
            }))
            .unwrap(),
            theme::resolve(None),
        )
        .unwrap();
        let png = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(rasterize_svg(&svg))
            .unwrap();
        assert_eq!(&png[0..4], b"\x89PNG", "chart rasterizes to a PNG");
    }
}
