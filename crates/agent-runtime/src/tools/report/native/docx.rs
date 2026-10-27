//! Lower a [`NativeDoc`] to a Word `.docx` (WordprocessingML) package.
//!
//! Content uses direct run/paragraph formatting rather than named styles, so the
//! package never depends on a style being present — the most robust hedge against
//! a "needs repair" prompt. Charts and images are embedded as inline drawings.

use eyre::Result;

use super::ooxml::{emu_extent, esc, hex6, Package, DOCX_CONTENT_WIDTH_EMU, MONO_FONT};
use super::{NativeBlock, NativeDoc, NativeImage, NativeListItem, NativeStat};
use crate::tools::report::ir::{Align, Meta};
use crate::tools::report::theme::Theme;

/// A4 printable width in twips (210mm − 2×1-inch margins).
const CONTENT_TWIPS: u32 = 9026;

// Run sizes in half-points.
const SZ_TITLE: u32 = 56;
const SZ_SUBTITLE: u32 = 32;
const SZ_META: u32 = 18;
const SZ_H1: u32 = 32;
const SZ_H2: u32 = 26;
const SZ_H3: u32 = 22;
const SZ_BODY: u32 = 21;
const SZ_CAPTION: u32 = 17;
const SZ_STAT_VALUE: u32 = 40;
const SZ_STAT_LABEL: u32 = 18;

pub fn write(doc: &NativeDoc) -> Result<Vec<u8>> {
    let mut media = Media::default();
    let body = render_body(doc, &mut media);

    let document = format!(
        "{XML_DECL}<w:document {DOC_NS}><w:body>{body}{}</w:body></w:document>",
        sect_pr()
    );

    let mut pkg = Package::new();
    pkg.add_xml("[Content_Types].xml", &content_types())?;
    pkg.add_xml("_rels/.rels", ROOT_RELS)?;
    pkg.add_xml("word/document.xml", &document)?;
    pkg.add_xml("word/styles.xml", &styles(&doc.theme))?;
    pkg.add_xml("word/_rels/document.xml.rels", &media.rels())?;
    for image in &media.parts {
        pkg.add_media(&format!("word/media/{}", image.filename), &image.bytes)?;
    }
    pkg.finish()
}

fn render_body(doc: &NativeDoc, media: &mut Media) -> String {
    let p = &doc.theme.palette;
    let sans = &doc.theme.sans;

    let mut out = title_block(doc);
    for section in &doc.sections {
        if let Some(heading) = section.heading.as_deref().filter(|h| !h.is_empty()) {
            out.push_str(&heading_para(heading, 1, p, sans));
        }
        for block in &section.blocks {
            out.push_str(&render_block(block, &doc.theme, media));
        }
    }
    out
}

fn render_block(block: &NativeBlock, theme: &Theme, media: &mut Media) -> String {
    let p = &theme.palette;
    let (sans, serif) = (&theme.sans, &theme.serif);
    match block {
        NativeBlock::Heading { level, text } => heading_para(text, *level, p, sans),
        NativeBlock::Paragraph { text } => para(
            &ppr("left", 60, 60),
            &run(text, &rpr(false, false, SZ_BODY, &hex6(&p.ink), serif)),
        ),
        // `run` already turns each newline into a `<w:br/>`, so verbatim text
        // needs nothing beyond the monospace face.
        NativeBlock::Code { text } => para(
            &ppr("left", 60, 60),
            &run(text, &rpr(false, false, SZ_BODY, &hex6(&p.ink), MONO_FONT)),
        ),
        NativeBlock::Stats { items } => stats_table(items, p, sans),
        NativeBlock::Table { columns, rows } => data_table(columns, rows, p, sans, serif),
        NativeBlock::List { title, items } => list_block(title.as_deref(), items, p, sans, serif),
        NativeBlock::Image(image) => image_block(image, theme, media),
    }
}

fn title_block(doc: &NativeDoc) -> String {
    let p = &doc.theme.palette;
    let sans = &doc.theme.sans;
    let mut out = String::new();
    if let Some(kicker) = doc.meta.kicker.as_deref().filter(|s| !s.is_empty()) {
        out.push_str(&para(
            &ppr("left", 0, 40),
            &run(
                &kicker.to_uppercase(),
                &rpr(true, false, SZ_META, &hex6(&p.secondary), sans),
            ),
        ));
    }
    out.push_str(&para(
        &ppr("left", 0, 40),
        &run(
            &doc.title,
            &rpr(true, false, SZ_TITLE, &hex6(&p.primary), sans),
        ),
    ));
    if let Some(subtitle) = doc.subtitle.as_deref().filter(|s| !s.is_empty()) {
        out.push_str(&para(
            &ppr("left", 0, 80),
            &run(
                subtitle,
                &rpr(false, false, SZ_SUBTITLE, &hex6(&p.muted), sans),
            ),
        ));
    }
    if let Some(meta) = meta_line(&doc.meta) {
        out.push_str(&para(
            &ppr("left", 0, 80),
            &run(&meta, &rpr(false, false, SZ_META, &hex6(&p.muted), sans)),
        ));
    }
    out.push_str(&divider(&hex6(&p.hairline)));
    out
}

fn meta_line(meta: &Meta) -> Option<String> {
    let parts: Vec<&str> = [meta.author.as_deref(), meta.date.as_deref()]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join("  •  "))
}

fn heading_para(
    text: &str,
    level: u8,
    p: &crate::tools::report::theme::Palette,
    sans: &str,
) -> String {
    let (sz, before) = match level {
        1 => (SZ_H1, 240),
        2 => (SZ_H2, 180),
        _ => (SZ_H3, 140),
    };
    para(
        &ppr("left", before, 80),
        &run(text, &rpr(true, false, sz, &hex6(&p.primary), sans)),
    )
}

fn list_block(
    title: Option<&str>,
    items: &[NativeListItem],
    p: &crate::tools::report::theme::Palette,
    sans: &str,
    serif: &str,
) -> String {
    let mut out = String::new();
    if let Some(t) = title.filter(|t| !t.is_empty()) {
        out.push_str(&para(
            &ppr("left", 80, 40),
            &run(t, &rpr(true, false, SZ_BODY, &hex6(&p.primary), sans)),
        ));
    }
    for item in items {
        let bullet = run("•  ", &rpr(true, false, SZ_BODY, &hex6(&p.accent), sans));
        let lead = item
            .lead
            .as_deref()
            .filter(|l| !l.is_empty())
            .map(|l| {
                run(
                    &format!("{l} "),
                    &rpr(true, false, SZ_BODY, &hex6(&p.ink), sans),
                )
            })
            .unwrap_or_default();
        let text = run(
            &item.text,
            &rpr(false, false, SZ_BODY, &hex6(&p.ink), serif),
        );
        out.push_str(&para(&bullet_ppr(), &format!("{bullet}{lead}{text}")));
    }
    out
}

fn stats_table(
    items: &[NativeStat],
    p: &crate::tools::report::theme::Palette,
    sans: &str,
) -> String {
    if items.is_empty() {
        return String::new();
    }
    let col = CONTENT_TWIPS / items.len() as u32;
    let grid: String = items
        .iter()
        .map(|_| format!("<w:gridCol w:w=\"{col}\"/>"))
        .collect();
    let cells: String = items
        .iter()
        .map(|s| {
            let value = para(
                &ppr("left", 40, 0),
                &run(
                    &s.value,
                    &rpr(true, false, SZ_STAT_VALUE, &hex6(&s.color), sans),
                ),
            );
            let label = para(
                &ppr("left", 0, 40),
                &run(
                    &s.label,
                    &rpr(false, false, SZ_STAT_LABEL, &hex6(&p.muted), sans),
                ),
            );
            format!(
                "<w:tc><w:tcPr><w:tcW w:w=\"{col}\" w:type=\"dxa\"/></w:tcPr>{value}{label}</w:tc>"
            )
        })
        .collect();
    format!(
        "<w:tbl><w:tblPr><w:tblW w:w=\"{CONTENT_TWIPS}\" w:type=\"dxa\"/><w:tblLook w:val=\"0000\"/></w:tblPr>\
         <w:tblGrid>{grid}</w:tblGrid><w:tr>{cells}</w:tr></w:tbl>{}",
        empty_para()
    )
}

fn data_table(
    columns: &[String],
    rows: &[Vec<String>],
    p: &crate::tools::report::theme::Palette,
    sans: &str,
    serif: &str,
) -> String {
    let n = columns.len().max(1);
    let col = CONTENT_TWIPS / n as u32;
    let grid: String = (0..n)
        .map(|_| format!("<w:gridCol w:w=\"{col}\"/>"))
        .collect();
    let hair = hex6(&p.hairline);

    let header_cells: String = columns
        .iter()
        .map(|c| table_cell(c, col, true, &hex6(&p.primary), sans))
        .collect();
    let header = format!("<w:tr>{header_cells}</w:tr>");

    let body: String = rows
        .iter()
        .map(|row| {
            let cells: String = (0..n)
                .map(|i| {
                    let text = row.get(i).map(String::as_str).unwrap_or("");
                    table_cell(text, col, false, &hex6(&p.ink), serif)
                })
                .collect();
            format!("<w:tr>{cells}</w:tr>")
        })
        .collect();

    format!(
        "<w:tbl><w:tblPr><w:tblW w:w=\"{CONTENT_TWIPS}\" w:type=\"dxa\"/>{borders}\
         <w:tblLook w:val=\"04A0\"/></w:tblPr><w:tblGrid>{grid}</w:tblGrid>{header}{body}</w:tbl>{}",
        empty_para(),
        borders = table_borders(&hair),
    )
}

fn table_cell(text: &str, width: u32, bold: bool, color: &str, font: &str) -> String {
    let cell = para(
        &ppr("left", 30, 30),
        &run(
            text,
            &rpr(bold, false, if bold { SZ_BODY } else { 20 }, color, font),
        ),
    );
    format!("<w:tc><w:tcPr><w:tcW w:w=\"{width}\" w:type=\"dxa\"/></w:tcPr>{cell}</w:tc>")
}

fn image_block(image: &NativeImage, theme: &Theme, media: &mut Media) -> String {
    let p = &theme.palette;
    let (rid, did) = media.add(image);
    let (cx, cy) = emu_extent(
        image.width_px,
        image.height_px,
        DOCX_CONTENT_WIDTH_EMU,
        image.width_pct,
    );
    let jc = align_jc(image.align);
    let drawing = format!(
        "<w:drawing><wp:inline distT=\"0\" distB=\"0\" distL=\"0\" distR=\"0\">\
         <wp:extent cx=\"{cx}\" cy=\"{cy}\"/><wp:docPr id=\"{did}\" name=\"Image {did}\"/>\
         <a:graphic xmlns:a=\"{A_NS}\"><a:graphicData uri=\"{PIC_NS}\">\
         <pic:pic xmlns:pic=\"{PIC_NS}\"><pic:nvPicPr><pic:cNvPr id=\"{did}\" name=\"Image {did}\"/>\
         <pic:cNvPicPr/></pic:nvPicPr><pic:blipFill><a:blip r:embed=\"{rid}\"/>\
         <a:stretch><a:fillRect/></a:stretch></pic:blipFill><pic:spPr><a:xfrm>\
         <a:off x=\"0\" y=\"0\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm>\
         <a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></pic:spPr></pic:pic></a:graphicData></a:graphic>\
         </wp:inline></w:drawing>"
    );
    let mut out = para(&ppr(jc, 120, 40), &format!("<w:r>{drawing}</w:r>"));
    if let Some(caption) = image.caption.as_deref().filter(|c| !c.is_empty()) {
        out.push_str(&para(
            &ppr(jc, 0, 120),
            &run(
                caption,
                &rpr(false, true, SZ_CAPTION, &hex6(&p.muted), &theme.sans),
            ),
        ));
    }
    out
}

// ── run / paragraph primitives ───────────────────────────────────────────────

fn rpr(bold: bool, italic: bool, sz: u32, color: &str, font: &str) -> String {
    format!(
        "<w:rPr><w:rFonts w:ascii=\"{font}\" w:hAnsi=\"{font}\"/>{b}{i}\
         <w:color w:val=\"{color}\"/><w:sz w:val=\"{sz}\"/><w:szCs w:val=\"{sz}\"/></w:rPr>",
        b = if bold { "<w:b/>" } else { "" },
        i = if italic { "<w:i/>" } else { "" },
    )
}

fn run(text: &str, rpr: &str) -> String {
    let content = text
        .split('\n')
        .map(|seg| format!("<w:t xml:space=\"preserve\">{}</w:t>", esc(seg)))
        .collect::<Vec<_>>()
        .join("<w:br/>");
    format!("<w:r>{rpr}{content}</w:r>")
}

fn para(ppr: &str, runs: &str) -> String {
    format!("<w:p>{ppr}{runs}</w:p>")
}

fn ppr(jc: &str, before: u32, after: u32) -> String {
    format!("<w:pPr><w:spacing w:before=\"{before}\" w:after=\"{after}\"/><w:jc w:val=\"{jc}\"/></w:pPr>")
}

fn bullet_ppr() -> String {
    "<w:pPr><w:spacing w:before=\"30\" w:after=\"30\"/><w:ind w:left=\"360\" w:hanging=\"360\"/></w:pPr>"
        .to_string()
}

fn empty_para() -> String {
    "<w:p/>".to_string()
}

fn divider(hair: &str) -> String {
    format!(
        "<w:p><w:pPr><w:pBdr><w:bottom w:val=\"single\" w:sz=\"6\" w:space=\"1\" w:color=\"{hair}\"/></w:pBdr>\
         <w:spacing w:after=\"200\"/></w:pPr></w:p>"
    )
}

fn table_borders(hair: &str) -> String {
    let edge = |which: &str| {
        format!("<w:{which} w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"{hair}\"/>")
    };
    format!(
        "<w:tblBorders>{}{}{}{}{}{}</w:tblBorders>",
        edge("top"),
        edge("left"),
        edge("bottom"),
        edge("right"),
        edge("insideH"),
        edge("insideV"),
    )
}

fn align_jc(align: Align) -> &'static str {
    match align {
        Align::Left => "left",
        Align::Center => "center",
        Align::Right => "right",
    }
}

fn sect_pr() -> String {
    "<w:sectPr><w:pgSz w:w=\"11906\" w:h=\"16838\"/>\
     <w:pgMar w:top=\"1440\" w:right=\"1440\" w:bottom=\"1440\" w:left=\"1440\" \
     w:header=\"708\" w:footer=\"708\" w:gutter=\"0\"/></w:sectPr>"
        .to_string()
}

// ── package parts ─────────────────────────────────────────────────────────────

#[derive(Default)]
struct Media {
    parts: Vec<MediaPart>,
}

struct MediaPart {
    rid: String,
    filename: String,
    bytes: Vec<u8>,
}

impl Media {
    /// Register an image, returning its relationship id and a unique drawing id.
    fn add(&mut self, image: &NativeImage) -> (String, u32) {
        let n = self.parts.len() as u32 + 1;
        let rid = format!("rId{}", 100 + n);
        self.parts.push(MediaPart {
            rid: rid.clone(),
            filename: format!("image{n}.{}", image.format.ext()),
            bytes: image.bytes.clone(),
        });
        (rid, n)
    }

    fn rels(&self) -> String {
        let images: String = self
            .parts
            .iter()
            .map(|m| {
                format!(
                    "<Relationship Id=\"{}\" Type=\"{REL_IMAGE}\" Target=\"media/{}\"/>",
                    m.rid, m.filename
                )
            })
            .collect();
        format!(
            "{XML_DECL}<Relationships xmlns=\"{PKG_REL_NS}\">\
             <Relationship Id=\"rId1\" Type=\"{REL_STYLES}\" Target=\"styles.xml\"/>{images}</Relationships>"
        )
    }
}

fn content_types() -> String {
    format!(
        "{XML_DECL}<Types xmlns=\"{CT_NS}\">\
         <Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
         <Default Extension=\"xml\" ContentType=\"application/xml\"/>\
         <Default Extension=\"png\" ContentType=\"image/png\"/>\
         <Default Extension=\"jpeg\" ContentType=\"image/jpeg\"/>\
         <Default Extension=\"gif\" ContentType=\"image/gif\"/>\
         <Override PartName=\"/word/document.xml\" ContentType=\"{CT_DOCUMENT}\"/>\
         <Override PartName=\"/word/styles.xml\" ContentType=\"{CT_STYLES}\"/></Types>"
    )
}

fn styles(theme: &Theme) -> String {
    let ink = hex6(&theme.palette.ink);
    let serif = &theme.serif;
    format!(
        "{XML_DECL}<w:styles {STYLES_NS}><w:docDefaults><w:rPrDefault><w:rPr>\
         <w:rFonts w:ascii=\"{serif}\" w:hAnsi=\"{serif}\"/><w:color w:val=\"{ink}\"/>\
         <w:sz w:val=\"{SZ_BODY}\"/><w:szCs w:val=\"{SZ_BODY}\"/></w:rPr></w:rPrDefault></w:docDefaults>\
         <w:style w:type=\"paragraph\" w:default=\"1\" w:styleId=\"Normal\"><w:name w:val=\"Normal\"/></w:style>\
         </w:styles>"
    )
}

const XML_DECL: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>";
const DOC_NS: &str = "xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" \
     xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
     xmlns:wp=\"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing\" \
     xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
     xmlns:pic=\"http://schemas.openxmlformats.org/drawingml/2006/picture\"";
const STYLES_NS: &str = "xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"";
const A_NS: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const PIC_NS: &str = "http://schemas.openxmlformats.org/drawingml/2006/picture";
const CT_NS: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
const PKG_REL_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const REL_STYLES: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles";
const REL_IMAGE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image";
const CT_DOCUMENT: &str =
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml";
const CT_STYLES: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml";
const ROOT_RELS: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
     <Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
     <Relationship Id=\"rId1\" \
     Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" \
     Target=\"word/document.xml\"/></Relationships>";

/// The first LibreOffice binary on `PATH` that answers `--version`, if any.
/// Shared with the pptx round-trip test.
#[cfg(test)]
pub(super) fn soffice_bin() -> Option<&'static str> {
    ["soffice", "libreoffice"].into_iter().find(|bin| {
        std::process::Command::new(bin)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// True when LibreOffice converts the given OOXML bytes to a non-empty PDF — the
/// real test that the package opens without repair.
#[cfg(test)]
pub(super) fn soffice_converts(bin: &str, bytes: &[u8], ext: &str) -> bool {
    let dir = tempfile::TempDir::new().unwrap();
    let input = dir.path().join(format!("in.{ext}"));
    std::fs::write(&input, bytes).unwrap();
    let ok = std::process::Command::new(bin)
        .args(["--headless", "--convert-to", "pdf", "--outdir"])
        .arg(dir.path())
        .arg(&input)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    ok && std::fs::read(dir.path().join("in.pdf"))
        .map(|p| !p.is_empty())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::report::native::{ImageFormat, NativeSection};
    use crate::tools::report::theme;
    use std::io::{Cursor, Read};

    fn sample() -> NativeDoc {
        NativeDoc {
            title: "Q3 & Beyond".into(),
            subtitle: Some("Results".into()),
            meta: Meta {
                kicker: Some("Finance".into()),
                author: Some("Me".into()),
                date: Some("2026".into()),
            },
            theme: theme::resolve(Some("slate")).clone(),
            sections: vec![NativeSection {
                heading: Some("Overview".into()),
                blocks: vec![
                    NativeBlock::Paragraph {
                        text: "Line one\nLine two".into(),
                    },
                    NativeBlock::Stats {
                        items: vec![NativeStat {
                            value: "42".into(),
                            label: "Count".into(),
                            color: "#2F7FB5".into(),
                        }],
                    },
                    NativeBlock::Table {
                        columns: vec!["A".into(), "B".into()],
                        rows: vec![vec!["1".into(), "x".into()]],
                    },
                    NativeBlock::List {
                        title: Some("Notes".into()),
                        items: vec![NativeListItem {
                            lead: Some("Lead.".into()),
                            text: "body".into(),
                        }],
                    },
                    NativeBlock::Image(NativeImage {
                        bytes: b"\x89PNG\r\n\x1a\n".to_vec(),
                        format: ImageFormat::Png,
                        width_px: 800,
                        height_px: 400,
                        width_pct: 100,
                        align: Align::Center,
                        caption: Some("Fig 1".into()),
                    }),
                ],
            }],
        }
    }

    fn parts(bytes: &[u8]) -> Vec<String> {
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes.to_vec())).unwrap();
        (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().unwrap().to_string())
            .collect()
    }

    fn part_text(bytes: &[u8], name: &str) -> String {
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes.to_vec())).unwrap();
        let mut s = String::new();
        zip.by_name(name).unwrap().read_to_string(&mut s).unwrap();
        s
    }

    fn assert_well_formed(xml: &str) {
        let mut reader = quick_xml::Reader::from_str(xml);
        loop {
            match reader.read_event() {
                Ok(quick_xml::events::Event::Eof) => break,
                Ok(_) => {}
                Err(e) => panic!("malformed XML: {e}\n{xml}"),
            }
        }
    }

    #[test]
    fn writes_a_complete_package_with_required_parts() {
        let bytes = write(&sample()).unwrap();
        let names = parts(&bytes);
        for required in [
            "[Content_Types].xml",
            "_rels/.rels",
            "word/document.xml",
            "word/styles.xml",
            "word/_rels/document.xml.rels",
            "word/media/image1.png",
        ] {
            assert!(
                names.contains(&required.to_string()),
                "missing {required} in {names:?}"
            );
        }
    }

    #[test]
    fn every_xml_part_is_well_formed_and_escaped() {
        let bytes = write(&sample()).unwrap();
        for name in parts(&bytes)
            .into_iter()
            .filter(|n| n.ends_with(".xml") || n.ends_with(".rels"))
        {
            assert_well_formed(&part_text(&bytes, &name));
        }
        let doc = part_text(&bytes, "word/document.xml");
        assert!(doc.contains("Q3 &amp; Beyond"), "title must be xml-escaped");
        assert!(doc.contains("<w:br/>"), "newlines become line breaks");
    }

    #[test]
    fn image_relationship_is_wired() {
        let bytes = write(&sample()).unwrap();
        let doc = part_text(&bytes, "word/document.xml");
        let rels = part_text(&bytes, "word/_rels/document.xml.rels");
        assert!(doc.contains("r:embed=\"rId101\""));
        assert!(rels.contains("Id=\"rId101\""));
        assert!(rels.contains("Target=\"media/image1.png\""));
    }

    #[test]
    fn round_trips_through_libreoffice_when_available() {
        let Some(bin) = soffice_bin() else {
            eprintln!("skipping: `soffice` not on PATH");
            return;
        };
        let bytes = write(&sample()).unwrap();
        assert!(
            soffice_converts(bin, &bytes, "docx"),
            "LibreOffice must open the .docx cleanly"
        );
    }
}
