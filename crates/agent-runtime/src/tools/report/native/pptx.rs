//! Lower a [`NativeDoc`] to a PowerPoint `.pptx` (PresentationML) package.
//!
//! A title slide plus one or more slides per section. Blocks are packed top to
//! bottom with estimated heights; when a section overflows, it continues on a
//! fresh slide. The mandatory slide-master/layout/theme parts are emitted from
//! fixed, valid templates so PowerPoint never offers to repair the file.

use eyre::Result;

use super::ooxml::{esc, hex6, Package, MONO_FONT};
use super::{NativeBlock, NativeDoc, NativeImage, NativeListItem, NativeSection, NativeStat};
use crate::tools::report::theme::{Palette, Theme};

const SLIDE_W: i64 = 12_192_000;
const SLIDE_H: i64 = 6_858_000;
const MARGIN: i64 = 457_200;
const CONTENT_X: i64 = MARGIN;
const CONTENT_W: i64 = SLIDE_W - 2 * MARGIN;
const TITLE_Y: i64 = 274_320;
const TITLE_H: i64 = 822_960;
const CONTENT_TOP: i64 = TITLE_Y + TITLE_H + 91_440;
const CONTENT_BOTTOM: i64 = SLIDE_H - 320_040;
const BLOCK_GAP: i64 = 91_440;
const LINE_H: i64 = 266_700;
const CHARS_PER_LINE: usize = 100;

// DrawingML run sizes in hundredths of a point.
const SZ_TITLE: u32 = 4000;
const SZ_SUBTITLE: u32 = 2000;
const SZ_META: u32 = 1300;
const SZ_SLIDE_TITLE: u32 = 2600;
const SZ_H2: u32 = 1700;
const SZ_H3: u32 = 1500;
const SZ_BODY: u32 = 1300;
const SZ_CAPTION: u32 = 1050;
const SZ_STAT: u32 = 1500;

pub fn write(doc: &NativeDoc) -> Result<Vec<u8>> {
    let mut deck = Deck::new(&doc.theme);
    deck.title_slide(doc);
    for section in &doc.sections {
        deck.section_slides(section);
    }
    let Deck { plans, media, .. } = deck;

    let mut pkg = Package::new();
    pkg.add_xml("[Content_Types].xml", &content_types(plans.len()))?;
    pkg.add_xml("_rels/.rels", ROOT_RELS)?;
    pkg.add_xml("ppt/presentation.xml", &presentation(plans.len()))?;
    pkg.add_xml(
        "ppt/_rels/presentation.xml.rels",
        &presentation_rels(plans.len()),
    )?;
    pkg.add_xml("ppt/slideMasters/slideMaster1.xml", SLIDE_MASTER)?;
    pkg.add_xml("ppt/slideMasters/_rels/slideMaster1.xml.rels", MASTER_RELS)?;
    pkg.add_xml("ppt/slideLayouts/slideLayout1.xml", SLIDE_LAYOUT)?;
    pkg.add_xml("ppt/slideLayouts/_rels/slideLayout1.xml.rels", LAYOUT_RELS)?;
    pkg.add_xml("ppt/theme/theme1.xml", &theme_xml(&doc.theme))?;
    for (i, plan) in plans.iter().enumerate() {
        let n = i + 1;
        pkg.add_xml(&format!("ppt/slides/slide{n}.xml"), &slide_xml(plan))?;
        pkg.add_xml(
            &format!("ppt/slides/_rels/slide{n}.xml.rels"),
            &slide_rels(plan),
        )?;
    }
    for item in &media {
        pkg.add_media(&format!("ppt/media/{}", item.filename), &item.bytes)?;
    }
    pkg.finish()
}

// ── deck assembly ─────────────────────────────────────────────────────────────

struct Deck<'a> {
    theme: &'a Theme,
    plans: Vec<SlidePlan>,
    media: Vec<Media>,
}

struct SlidePlan {
    shapes: String,
    images: Vec<ImageRef>,
    shape_id: u32,
}

struct ImageRef {
    rid: String,
    filename: String,
}

struct Media {
    filename: String,
    bytes: Vec<u8>,
}

impl<'a> Deck<'a> {
    fn new(theme: &'a Theme) -> Self {
        Self {
            theme,
            plans: Vec::new(),
            media: Vec::new(),
        }
    }

    fn title_slide(&mut self, doc: &NativeDoc) {
        let p = &self.theme.palette;
        let mut plan = SlidePlan::new();
        let mut y = SLIDE_H / 3;
        if let Some(kicker) = doc.meta.kicker.as_deref().filter(|s| !s.is_empty()) {
            y = plan.text_box(
                CONTENT_X,
                y,
                CONTENT_W,
                430_000,
                "ctr",
                &[line(
                    &kicker.to_uppercase(),
                    SZ_META,
                    true,
                    &p.secondary,
                    &self.theme.sans,
                )],
            );
        }
        y = plan.text_box(
            CONTENT_X,
            y,
            CONTENT_W,
            900_000,
            "ctr",
            &[line(
                &doc.title,
                SZ_TITLE,
                true,
                &p.primary,
                &self.theme.sans,
            )],
        );
        if let Some(subtitle) = doc.subtitle.as_deref().filter(|s| !s.is_empty()) {
            y = plan.text_box(
                CONTENT_X,
                y,
                CONTENT_W,
                520_000,
                "ctr",
                &[line(
                    subtitle,
                    SZ_SUBTITLE,
                    false,
                    &p.muted,
                    &self.theme.sans,
                )],
            );
        }
        if let Some(meta) = meta_line(doc) {
            plan.text_box(
                CONTENT_X,
                y,
                CONTENT_W,
                430_000,
                "ctr",
                &[line(&meta, SZ_META, false, &p.muted, &self.theme.sans)],
            );
        }
        self.plans.push(plan);
    }

    fn section_slides(&mut self, section: &NativeSection) {
        let title = section.heading.clone();
        let mut plan = self.open_slide(&title);
        let mut y = CONTENT_TOP;
        for block in &section.blocks {
            let height = self.block_height(block);
            if y + height > CONTENT_BOTTOM && y > CONTENT_TOP {
                self.plans.push(plan);
                plan = self.open_slide(&continuation(&title));
                y = CONTENT_TOP;
            }
            self.place_block(&mut plan, block, y, height);
            y += height + BLOCK_GAP;
        }
        self.plans.push(plan);
    }

    fn open_slide(&self, title: &Option<String>) -> SlidePlan {
        let mut plan = SlidePlan::new();
        if let Some(t) = title.as_deref().filter(|t| !t.is_empty()) {
            let p = &self.theme.palette;
            plan.text_box(
                CONTENT_X,
                TITLE_Y,
                CONTENT_W,
                TITLE_H,
                "l",
                &[line(t, SZ_SLIDE_TITLE, true, &p.primary, &self.theme.sans)],
            );
        }
        plan
    }

    fn place_block(&mut self, plan: &mut SlidePlan, block: &NativeBlock, y: i64, height: i64) {
        let p = &self.theme.palette;
        let (sans, serif) = (&self.theme.sans, &self.theme.serif);
        match block {
            NativeBlock::Heading { level, text } => {
                let sz = if *level <= 2 { SZ_H2 } else { SZ_H3 };
                plan.text_box(
                    CONTENT_X,
                    y,
                    CONTENT_W,
                    height,
                    "l",
                    &[line(text, sz, true, &p.primary, sans)],
                );
            }
            NativeBlock::Paragraph { text } => {
                plan.text_box(
                    CONTENT_X,
                    y,
                    CONTENT_W,
                    height,
                    "l",
                    &[line(text, SZ_BODY, false, &p.ink, serif)],
                );
            }
            // `line` already turns each newline into an `<a:br/>`, so verbatim
            // text needs nothing beyond the monospace face.
            NativeBlock::Code { text } => {
                plan.text_box(
                    CONTENT_X,
                    y,
                    CONTENT_W,
                    height,
                    "l",
                    &[line(text, SZ_BODY, false, &p.ink, MONO_FONT)],
                );
            }
            NativeBlock::Stats { items } => {
                let lines: Vec<String> = items.iter().map(|s| stat_line(s, p, sans)).collect();
                plan.text_box(CONTENT_X, y, CONTENT_W, height, "l", &lines);
            }
            NativeBlock::List { title, items } => {
                let mut lines = Vec::new();
                if let Some(t) = title.as_deref().filter(|t| !t.is_empty()) {
                    lines.push(line(t, SZ_BODY, true, &p.primary, sans));
                }
                lines.extend(items.iter().map(|it| bullet_line(it, p, sans, serif)));
                plan.text_box(CONTENT_X, y, CONTENT_W, height, "l", &lines);
            }
            NativeBlock::Table { columns, rows } => {
                plan.table(
                    CONTENT_X, y, CONTENT_W, height, columns, rows, p, sans, serif,
                );
            }
            NativeBlock::Image(image) => {
                let media_n = self.media.len() + 1;
                let filename = format!("image{media_n}.{}", image.format.ext());
                let rid = plan.add_image(&filename);
                self.media.push(Media {
                    filename,
                    bytes: image.bytes.clone(),
                });
                let (w, h) = fit_image(image);
                let x = CONTENT_X + (CONTENT_W - w) / 2;
                plan.picture(&rid, x, y, w, h);
                if let Some(caption) = image.caption.as_deref().filter(|c| !c.is_empty()) {
                    plan.text_box(
                        CONTENT_X,
                        y + h,
                        CONTENT_W,
                        300_000,
                        "ctr",
                        &[line(caption, SZ_CAPTION, false, &p.muted, sans)],
                    );
                }
            }
        }
    }

    fn block_height(&self, block: &NativeBlock) -> i64 {
        match block {
            NativeBlock::Heading { level, .. } => {
                if *level <= 2 {
                    430_000
                } else {
                    380_000
                }
            }
            NativeBlock::Paragraph { text } | NativeBlock::Code { text } => text_height(text),
            NativeBlock::Stats { items } => items.len() as i64 * 360_000 + BLOCK_GAP,
            NativeBlock::List { title, items } => {
                let title_h = i64::from(title.as_deref().is_some_and(|t| !t.is_empty())) * 320_000;
                title_h + items.len() as i64 * 330_000 + BLOCK_GAP
            }
            NativeBlock::Table { rows, .. } => {
                ((rows.len() + 1) as i64 * 360_000 + BLOCK_GAP).min(CONTENT_BOTTOM - CONTENT_TOP)
            }
            NativeBlock::Image(image) => {
                let (_, h) = fit_image(image);
                let caption =
                    i64::from(image.caption.as_deref().is_some_and(|c| !c.is_empty())) * 300_000;
                h + caption
            }
        }
    }
}

impl SlidePlan {
    fn new() -> Self {
        Self {
            shapes: String::new(),
            images: Vec::new(),
            shape_id: 1,
        }
    }

    fn next_id(&mut self) -> u32 {
        self.shape_id += 1;
        self.shape_id
    }

    fn add_image(&mut self, filename: &str) -> String {
        let rid = format!("rId{}", self.images.len() + 2);
        self.images.push(ImageRef {
            rid: rid.clone(),
            filename: filename.to_string(),
        });
        rid
    }

    /// Append a wrapped text box and return the y past its bottom.
    fn text_box(&mut self, x: i64, y: i64, cx: i64, cy: i64, align: &str, lines: &[String]) -> i64 {
        let id = self.next_id();
        let body = if lines.is_empty() {
            "<a:p/>".to_string()
        } else {
            lines
                .iter()
                .map(|l| format!("<a:p><a:pPr algn=\"{align}\"/>{l}</a:p>"))
                .collect()
        };
        self.shapes.push_str(&format!(
            "<p:sp><p:nvSpPr><p:cNvPr id=\"{id}\" name=\"Text {id}\"/><p:cNvSpPr txBox=\"1\"/><p:nvPr/></p:nvSpPr>\
             <p:spPr><a:xfrm><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm>\
             <a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></p:spPr>\
             <p:txBody><a:bodyPr wrap=\"square\" rtlCol=\"0\"><a:normAutofit/></a:bodyPr><a:lstStyle/>{body}</p:txBody></p:sp>"
        ));
        y + cy
    }

    fn picture(&mut self, rid: &str, x: i64, y: i64, cx: i64, cy: i64) {
        let id = self.next_id();
        self.shapes.push_str(&format!(
            "<p:pic><p:nvPicPr><p:cNvPr id=\"{id}\" name=\"Image {id}\"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr>\
             <p:blipFill><a:blip r:embed=\"{rid}\"/><a:stretch><a:fillRect/></a:stretch></p:blipFill>\
             <p:spPr><a:xfrm><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm>\
             <a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></p:spPr></p:pic>"
        ));
    }

    #[allow(clippy::too_many_arguments)]
    fn table(
        &mut self,
        x: i64,
        y: i64,
        cx: i64,
        cy: i64,
        columns: &[String],
        rows: &[Vec<String>],
        p: &Palette,
        sans: &str,
        serif: &str,
    ) {
        let id = self.next_id();
        let n = columns.len().max(1) as i64;
        let col_w = cx / n;
        let grid: String = (0..n)
            .map(|_| format!("<a:gridCol w=\"{col_w}\"/>"))
            .collect();
        let header = table_row(
            columns.iter().map(String::as_str),
            columns.len(),
            true,
            &p.primary,
            sans,
        );
        let body: String = rows
            .iter()
            .map(|row| {
                table_row(
                    row.iter().map(String::as_str),
                    columns.len(),
                    false,
                    &p.ink,
                    serif,
                )
            })
            .collect();
        self.shapes.push_str(&format!(
            "<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id=\"{id}\" name=\"Table {id}\"/>\
             <p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr>\
             <p:xfrm><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></p:xfrm>\
             <a:graphic><a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/table\">\
             <a:tbl><a:tblPr firstRow=\"1\" bandRow=\"1\"/><a:tblGrid>{grid}</a:tblGrid>{header}{body}</a:tbl>\
             </a:graphicData></a:graphic></p:graphicFrame>"
        ));
    }
}

// ── runs, rows, geometry ──────────────────────────────────────────────────────

fn line(text: &str, sz: u32, bold: bool, color: &str, font: &str) -> String {
    let runs = text
        .split('\n')
        .map(|seg| {
            format!(
                "<a:r><a:rPr lang=\"en-US\" sz=\"{sz}\" b=\"{b}\" dirty=\"0\">\
                 <a:solidFill><a:srgbClr val=\"{}\"/></a:solidFill><a:latin typeface=\"{font}\"/></a:rPr>\
                 <a:t>{}</a:t></a:r>",
                hex6(color),
                esc(seg),
                b = u8::from(bold),
            )
        })
        .collect::<Vec<_>>()
        .join("<a:br/>");
    runs
}

fn stat_line(s: &NativeStat, p: &Palette, sans: &str) -> String {
    let value = format!(
        "<a:r><a:rPr lang=\"en-US\" sz=\"{SZ_STAT}\" b=\"1\" dirty=\"0\">\
         <a:solidFill><a:srgbClr val=\"{}\"/></a:solidFill><a:latin typeface=\"{sans}\"/></a:rPr><a:t>{}</a:t></a:r>",
        hex6(&s.color),
        esc(&s.value),
    );
    let label = format!(
        "<a:r><a:rPr lang=\"en-US\" sz=\"{SZ_BODY}\" dirty=\"0\">\
         <a:solidFill><a:srgbClr val=\"{}\"/></a:solidFill><a:latin typeface=\"{sans}\"/></a:rPr><a:t>{}</a:t></a:r>",
        hex6(&p.muted),
        esc(&format!("  —  {}", s.label)),
    );
    format!("{value}{label}")
}

fn bullet_line(it: &NativeListItem, p: &Palette, sans: &str, serif: &str) -> String {
    let bullet = line("•  ", SZ_BODY, true, &p.accent, sans);
    let lead = it
        .lead
        .as_deref()
        .filter(|l| !l.is_empty())
        .map(|l| line(&format!("{l} "), SZ_BODY, true, &p.ink, sans))
        .unwrap_or_default();
    let body = line(&it.text, SZ_BODY, false, &p.ink, serif);
    format!("{bullet}{lead}{body}")
}

fn table_row<'a>(
    cells: impl Iterator<Item = &'a str>,
    cols: usize,
    header: bool,
    color: &str,
    font: &str,
) -> String {
    let n = cols.max(1);
    let mut values: Vec<&str> = cells.collect();
    values.resize(n, "");
    let tcs: String = values
        .iter()
        .map(|c| {
            format!(
                "<a:tc><a:txBody><a:bodyPr/><a:lstStyle/><a:p>{}</a:p></a:txBody><a:tcPr/></a:tc>",
                line(c, SZ_BODY, header, color, font)
            )
        })
        .collect();
    format!("<a:tr h=\"320000\">{tcs}</a:tr>")
}

fn fit_image(image: &NativeImage) -> (i64, i64) {
    let max_w = CONTENT_W * i64::from(image.width_pct.clamp(1, 100)) / 100;
    let max_h = (CONTENT_BOTTOM - CONTENT_TOP) * 62 / 100;
    let (w_px, h_px) = (image.width_px.max(1) as i64, image.height_px.max(1) as i64);
    let mut w = max_w;
    let mut h = w * h_px / w_px;
    if h > max_h {
        h = max_h;
        w = h * w_px / h_px;
    }
    (w.max(1), h.max(1))
}

fn text_height(text: &str) -> i64 {
    let lines: i64 = text
        .split('\n')
        .map(|seg| (seg.chars().count() / CHARS_PER_LINE + 1) as i64)
        .sum();
    lines * LINE_H + BLOCK_GAP
}

fn meta_line(doc: &NativeDoc) -> Option<String> {
    let parts: Vec<&str> = [doc.meta.author.as_deref(), doc.meta.date.as_deref()]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join("  •  "))
}

fn continuation(title: &Option<String>) -> Option<String> {
    title
        .as_deref()
        .filter(|t| !t.is_empty())
        .map(|t| format!("{t} (cont.)"))
}

// ── package parts ─────────────────────────────────────────────────────────────

fn slide_xml(plan: &SlidePlan) -> String {
    format!(
        "{XML_DECL}<p:sld {SLIDE_NS}><p:cSld><p:spTree>\
         <p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>\
         <p:grpSpPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/>\
         <a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"0\" cy=\"0\"/></a:xfrm></p:grpSpPr>\
         {}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>",
        plan.shapes
    )
}

fn slide_rels(plan: &SlidePlan) -> String {
    let images: String = plan
        .images
        .iter()
        .map(|i| {
            format!(
                "<Relationship Id=\"{}\" Type=\"{REL_IMAGE}\" Target=\"../media/{}\"/>",
                i.rid, i.filename
            )
        })
        .collect();
    format!(
        "{XML_DECL}<Relationships xmlns=\"{PKG_REL_NS}\">\
         <Relationship Id=\"rId1\" Type=\"{REL_LAYOUT}\" Target=\"../slideLayouts/slideLayout1.xml\"/>{images}</Relationships>"
    )
}

fn presentation(slides: usize) -> String {
    let ids: String = (0..slides)
        .map(|i| format!("<p:sldId id=\"{}\" r:id=\"rId{}\"/>", 256 + i, 2 + i))
        .collect();
    format!(
        "{XML_DECL}<p:presentation {PRES_NS}>\
         <p:sldMasterIdLst><p:sldMasterId id=\"2147483648\" r:id=\"rId1\"/></p:sldMasterIdLst>\
         <p:sldIdLst>{ids}</p:sldIdLst>\
         <p:sldSz cx=\"{SLIDE_W}\" cy=\"{SLIDE_H}\" type=\"screen16x9\"/>\
         <p:notesSz cx=\"6858000\" cy=\"9144000\"/></p:presentation>"
    )
}

fn presentation_rels(slides: usize) -> String {
    let slide_rels: String = (0..slides)
        .map(|i| {
            format!(
                "<Relationship Id=\"rId{}\" Type=\"{REL_SLIDE}\" Target=\"slides/slide{}.xml\"/>",
                2 + i,
                1 + i
            )
        })
        .collect();
    format!(
        "{XML_DECL}<Relationships xmlns=\"{PKG_REL_NS}\">\
         <Relationship Id=\"rId1\" Type=\"{REL_MASTER}\" Target=\"slideMasters/slideMaster1.xml\"/>\
         {slide_rels}</Relationships>"
    )
}

fn content_types(slides: usize) -> String {
    let slide_overrides: String = (1..=slides)
        .map(|n| {
            format!("<Override PartName=\"/ppt/slides/slide{n}.xml\" ContentType=\"{CT_SLIDE}\"/>")
        })
        .collect();
    format!(
        "{XML_DECL}<Types xmlns=\"{CT_NS}\">\
         <Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
         <Default Extension=\"xml\" ContentType=\"application/xml\"/>\
         <Default Extension=\"png\" ContentType=\"image/png\"/>\
         <Default Extension=\"jpeg\" ContentType=\"image/jpeg\"/>\
         <Default Extension=\"gif\" ContentType=\"image/gif\"/>\
         <Override PartName=\"/ppt/presentation.xml\" ContentType=\"{CT_PRESENTATION}\"/>\
         <Override PartName=\"/ppt/slideMasters/slideMaster1.xml\" ContentType=\"{CT_MASTER}\"/>\
         <Override PartName=\"/ppt/slideLayouts/slideLayout1.xml\" ContentType=\"{CT_LAYOUT}\"/>\
         <Override PartName=\"/ppt/theme/theme1.xml\" ContentType=\"{CT_THEME}\"/>\
         {slide_overrides}</Types>"
    )
}

fn theme_xml(theme: &Theme) -> String {
    let p = &theme.palette;
    let scheme = |c: &str| hex6(c);
    format!(
        "{XML_DECL}<a:theme xmlns:a=\"{A_NS}\" name=\"Workforce\"><a:themeElements>\
         <a:clrScheme name=\"Workforce\">\
         <a:dk1><a:srgbClr val=\"{ink}\"/></a:dk1><a:lt1><a:srgbClr val=\"{paper}\"/></a:lt1>\
         <a:dk2><a:srgbClr val=\"{primary}\"/></a:dk2><a:lt2><a:srgbClr val=\"{panel}\"/></a:lt2>\
         <a:accent1><a:srgbClr val=\"{accent}\"/></a:accent1><a:accent2><a:srgbClr val=\"{secondary}\"/></a:accent2>\
         <a:accent3><a:srgbClr val=\"{muted}\"/></a:accent3><a:accent4><a:srgbClr val=\"{accent}\"/></a:accent4>\
         <a:accent5><a:srgbClr val=\"{secondary}\"/></a:accent5><a:accent6><a:srgbClr val=\"{primary}\"/></a:accent6>\
         <a:hlink><a:srgbClr val=\"{accent}\"/></a:hlink><a:folHlink><a:srgbClr val=\"{muted}\"/></a:folHlink></a:clrScheme>\
         <a:fontScheme name=\"Workforce\"><a:majorFont><a:latin typeface=\"{sans}\"/><a:ea typeface=\"\"/><a:cs typeface=\"\"/></a:majorFont>\
         <a:minorFont><a:latin typeface=\"{serif}\"/><a:ea typeface=\"\"/><a:cs typeface=\"\"/></a:minorFont></a:fontScheme>\
         {FMT_SCHEME}</a:themeElements><a:objectDefaults/><a:extraClrSchemeLst/></a:theme>",
        ink = scheme(&p.ink),
        paper = scheme(&p.paper),
        primary = scheme(&p.primary),
        panel = scheme(&p.panel),
        accent = scheme(&p.accent),
        secondary = scheme(&p.secondary),
        muted = scheme(&p.muted),
        sans = theme.sans,
        serif = theme.serif,
    )
}

const XML_DECL: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>";
const A_NS: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const CT_NS: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
const PKG_REL_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const SLIDE_NS: &str = "xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
     xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
     xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\"";
const PRES_NS: &str = SLIDE_NS;
const REL_SLIDE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide";
const REL_MASTER: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster";
const REL_LAYOUT: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout";
const REL_IMAGE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image";
const CT_PRESENTATION: &str =
    "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml";
const CT_SLIDE: &str = "application/vnd.openxmlformats-officedocument.presentationml.slide+xml";
const CT_MASTER: &str =
    "application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml";
const CT_LAYOUT: &str =
    "application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml";
const CT_THEME: &str = "application/vnd.openxmlformats-officedocument.theme+xml";

const ROOT_RELS: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
     <Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
     <Relationship Id=\"rId1\" \
     Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" \
     Target=\"ppt/presentation.xml\"/></Relationships>";

const MASTER_RELS: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
     <Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
     <Relationship Id=\"rId1\" \
     Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout\" \
     Target=\"../slideLayouts/slideLayout1.xml\"/>\
     <Relationship Id=\"rId2\" \
     Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme\" \
     Target=\"../theme/theme1.xml\"/></Relationships>";

const LAYOUT_RELS: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
     <Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
     <Relationship Id=\"rId1\" \
     Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster\" \
     Target=\"../slideMasters/slideMaster1.xml\"/></Relationships>";

const SLIDE_MASTER: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<p:sldMaster xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\">\
<p:cSld><p:bg><p:bgRef idx=\"1001\"><a:schemeClr val=\"bg1\"/></p:bgRef></p:bg>\
<p:spTree><p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>\
<p:grpSpPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/><a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"0\" cy=\"0\"/></a:xfrm></p:grpSpPr>\
</p:spTree></p:cSld><p:clrMap bg1=\"lt1\" tx1=\"dk1\" bg2=\"lt2\" tx2=\"dk2\" accent1=\"accent1\" accent2=\"accent2\" accent3=\"accent3\" accent4=\"accent4\" accent5=\"accent5\" accent6=\"accent6\" hlink=\"hlink\" folHlink=\"folHlink\"/>\
<p:sldLayoutIdLst><p:sldLayoutId id=\"2147483649\" r:id=\"rId1\"/></p:sldLayoutIdLst>\
<p:txStyles><p:titleStyle><a:lvl1pPr><a:defRPr sz=\"2600\"/></a:lvl1pPr></p:titleStyle>\
<p:bodyStyle><a:lvl1pPr><a:defRPr sz=\"1300\"/></a:lvl1pPr></p:bodyStyle><p:otherStyle/></p:txStyles></p:sldMaster>";

const SLIDE_LAYOUT: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<p:sldLayout xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\" type=\"blank\" preserve=\"1\">\
<p:cSld name=\"Blank\"><p:spTree><p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>\
<p:grpSpPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/><a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"0\" cy=\"0\"/></a:xfrm></p:grpSpPr>\
</p:spTree></p:cSld><p:clrMapOvr><a:overrideClrMapping bg1=\"lt1\" tx1=\"dk1\" bg2=\"lt2\" tx2=\"dk2\" accent1=\"accent1\" accent2=\"accent2\" accent3=\"accent3\" accent4=\"accent4\" accent5=\"accent5\" accent6=\"accent6\" hlink=\"hlink\" folHlink=\"folHlink\"/></p:clrMapOvr></p:sldLayout>";

/// A complete, valid `fmtScheme` (fills, lines, effects, backgrounds) — required
/// for PowerPoint to accept the theme without offering a repair.
const FMT_SCHEME: &str = "<a:fmtScheme name=\"Office\">\
<a:fillStyleLst>\
<a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill>\
<a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill>\
<a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill></a:fillStyleLst>\
<a:lnStyleLst>\
<a:ln w=\"6350\" cap=\"flat\" cmpd=\"sng\" algn=\"ctr\"><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill><a:prstDash val=\"solid\"/></a:ln>\
<a:ln w=\"12700\" cap=\"flat\" cmpd=\"sng\" algn=\"ctr\"><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill><a:prstDash val=\"solid\"/></a:ln>\
<a:ln w=\"19050\" cap=\"flat\" cmpd=\"sng\" algn=\"ctr\"><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill><a:prstDash val=\"solid\"/></a:ln></a:lnStyleLst>\
<a:effectStyleLst>\
<a:effectStyle><a:effectLst/></a:effectStyle>\
<a:effectStyle><a:effectLst/></a:effectStyle>\
<a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst>\
<a:bgFillStyleLst>\
<a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill>\
<a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill>\
<a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill></a:bgFillStyleLst></a:fmtScheme>";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::report::ir::Align;
    use crate::tools::report::native::docx::{soffice_bin, soffice_converts};
    use crate::tools::report::native::{ImageFormat, NativeStat};
    use crate::tools::report::theme;
    use std::io::{Cursor, Read};

    fn sample() -> NativeDoc {
        NativeDoc {
            title: "Coffee & Tea".into(),
            subtitle: Some("Outlook".into()),
            meta: crate::tools::report::ir::Meta {
                kicker: Some("Industry".into()),
                author: Some("Me".into()),
                date: Some("2026".into()),
            },
            theme: theme::resolve(Some("noir")).clone(),
            sections: vec![NativeSection {
                heading: Some("Overview".into()),
                blocks: vec![
                    NativeBlock::Paragraph {
                        text: "Body line one.\nBody line two.".into(),
                    },
                    NativeBlock::Heading {
                        level: 3,
                        text: "Producers".into(),
                    },
                    NativeBlock::Stats {
                        items: vec![NativeStat {
                            value: "66".into(),
                            label: "Brazil".into(),
                            color: "#E2B341".into(),
                        }],
                    },
                    NativeBlock::Table {
                        columns: vec!["Region".into(), "Share".into()],
                        rows: vec![vec!["EU".into(), "32%".into()]],
                    },
                    NativeBlock::List {
                        title: Some("Outlook".into()),
                        items: vec![NativeListItem {
                            lead: Some("Climate.".into()),
                            text: "Tight.".into(),
                        }],
                    },
                    NativeBlock::Image(NativeImage {
                        bytes: b"\x89PNG\r\n\x1a\n".to_vec(),
                        format: ImageFormat::Png,
                        width_px: 720,
                        height_px: 360,
                        width_pct: 100,
                        align: Align::Center,
                        caption: Some("m bags".into()),
                    }),
                ],
            }],
        }
    }

    fn names(bytes: &[u8]) -> Vec<String> {
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes.to_vec())).unwrap();
        (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
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
    fn writes_required_parts_and_one_slide_per_section_plus_title() {
        let bytes = write(&sample()).unwrap();
        let parts = names(&bytes);
        for required in [
            "[Content_Types].xml",
            "_rels/.rels",
            "ppt/presentation.xml",
            "ppt/_rels/presentation.xml.rels",
            "ppt/slideMasters/slideMaster1.xml",
            "ppt/slideLayouts/slideLayout1.xml",
            "ppt/theme/theme1.xml",
            "ppt/slides/slide1.xml",
            "ppt/slides/slide2.xml",
            "ppt/media/image1.png",
        ] {
            assert!(
                parts.contains(&required.to_string()),
                "missing {required} in {parts:?}"
            );
        }
    }

    #[test]
    fn every_xml_part_is_well_formed_and_image_is_wired() {
        let bytes = write(&sample()).unwrap();
        let all = names(&bytes);
        for name in all
            .iter()
            .filter(|n| n.ends_with(".xml") || n.ends_with(".rels"))
        {
            assert_well_formed(&part_text(&bytes, name));
        }
        // Every slide references the layout, and the chart lands on exactly one.
        let slides: Vec<&String> = all
            .iter()
            .filter(|n| n.starts_with("ppt/slides/slide") && n.ends_with(".xml"))
            .collect();
        let with_image: Vec<&&String> = slides
            .iter()
            .filter(|n| part_text(&bytes, n).contains("r:embed=\"rId2\""))
            .collect();
        assert_eq!(
            with_image.len(),
            1,
            "the chart image is embedded on exactly one slide"
        );
        let slide = with_image[0];
        let (dir, file) = slide.rsplit_once('/').unwrap();
        let rels = part_text(&bytes, &format!("{dir}/_rels/{file}.rels"));
        assert!(rels.contains("Target=\"../media/image1.png\""));
        assert!(rels.contains("slideLayout1.xml"));
        // Slides inherit the master's colour map via the *empty* override form;
        // a bare <a:overrideClrMapping/> is schema-invalid (12 required attrs).
        let slide1 = part_text(&bytes, "ppt/slides/slide1.xml");
        assert!(slide1.contains("<a:masterClrMapping/>"));
        assert!(!slide1.contains("<a:overrideClrMapping/>"));
    }

    #[test]
    fn title_is_escaped_on_the_title_slide() {
        let bytes = write(&sample()).unwrap();
        let slide1 = part_text(&bytes, "ppt/slides/slide1.xml");
        assert!(slide1.contains("Coffee &amp; Tea"));
        assert!(!slide1.contains("Coffee & Tea"));
    }

    #[test]
    fn round_trips_through_libreoffice_when_available() {
        let Some(bin) = soffice_bin() else {
            eprintln!("skipping: `soffice` not on PATH");
            return;
        };
        let bytes = write(&sample()).unwrap();
        assert!(
            soffice_converts(bin, &bytes, "pptx"),
            "LibreOffice must open the .pptx cleanly"
        );
    }
}
