//! DOCX loader: extracts paragraph text from `word/document.xml`
//! inside the zip and groups paragraphs by heading level (Heading1,
//! Heading2). Locator format: `<heading>` for sections or
//! `para.<n>` for content before the first heading.
//!
//! Why this is in-tree rather than a third-party DOCX crate: the
//! Rust DOCX ecosystem skews towards *writing* DOCX; the few readers
//! either drag in heavy dependencies or break on real-world files
//! exported from Google Docs. The subset we need — paragraph text +
//! heading style — is small enough to handle with `zip` + `quick-xml`
//! (both already workspace deps).
//!
//! Limitations (acceptable for v1; track follow-ups in DW-541):
//! - tables come through as concatenated cell text (no markdown table)
//! - inline images / drawings are dropped
//! - footnotes / endnotes are not pulled in (separate XML parts)
//! - tracked changes show the *accepted* text only

use std::io::{Cursor, Read};

use eyre::{eyre, Result, WrapErr};
use quick_xml::events::Event;
use quick_xml::reader::Reader;

use super::RawChunk;

/// The main document part inside a DOCX zip. Required by the OOXML
/// spec for any wordprocessing document.
const DOC_PART: &str = "word/document.xml";

pub fn parse(bytes: &[u8]) -> Result<Vec<RawChunk>> {
    let xml = read_document_xml(bytes)?;
    let paragraphs = extract_paragraphs(&xml)?;
    if paragraphs.is_empty() {
        return Err(eyre!("DOCX contained no paragraph text"));
    }
    Ok(group_into_sections(paragraphs))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Para {
    text: String,
    style: ParaStyle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParaStyle {
    Heading1,
    Heading2,
    Body,
}

fn read_document_xml(bytes: &[u8]) -> Result<String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .wrap_err("DOCX archive isn't a valid zip (corrupted or wrong format)")?;
    let mut file = archive
        .by_name(DOC_PART)
        .wrap_err_with(|| format!("DOCX archive missing `{DOC_PART}` — not a Word document"))?;
    let mut xml = String::new();
    file.read_to_string(&mut xml)
        .wrap_err("read word/document.xml")?;
    Ok(xml)
}

/// Walk the OOXML AST, returning one `Para` per `<w:p>` element.
/// Run text (`<w:t>`) is concatenated; non-text runs (images, page
/// breaks) are ignored. Style is read from the optional
/// `<w:pStyle w:val="…">` inside `<w:pPr>`.
fn extract_paragraphs(xml: &str) -> Result<Vec<Para>> {
    // Deliberately **do not** call `reader.trim_text(true)`: Word emits
    // `<w:t xml:space="preserve">Hello </w:t><w:t>world</w:t>` whenever
    // a paragraph is split across styled runs, and trim_text would
    // collapse the trailing space, producing "Helloworld". We trim per
    // paragraph at the `</w:p>` boundary instead.
    let mut reader = Reader::from_str(xml);

    let mut paragraphs: Vec<Para> = Vec::new();
    let mut current_text = String::new();
    let mut current_style = ParaStyle::Body;
    let mut in_paragraph = false;
    let mut in_text = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = e.name();
                match name.as_ref() {
                    b"w:p" => {
                        in_paragraph = true;
                        current_text.clear();
                        current_style = ParaStyle::Body;
                    }
                    b"w:t" => {
                        in_text = in_paragraph;
                    }
                    b"w:pStyle" => {
                        for attr in e.attributes().with_checks(false).flatten() {
                            if attr.key.as_ref() == b"w:val" {
                                let val = String::from_utf8_lossy(attr.value.as_ref()).to_string();
                                current_style = classify_style(&val);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) if e.name().as_ref() == b"w:pStyle" => {
                for attr in e.attributes().with_checks(false).flatten() {
                    if attr.key.as_ref() == b"w:val" {
                        let val = String::from_utf8_lossy(attr.value.as_ref()).to_string();
                        current_style = classify_style(&val);
                    }
                }
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"w:p" => {
                    let trimmed = current_text.trim();
                    if !trimmed.is_empty() {
                        paragraphs.push(Para {
                            text: trimmed.to_string(),
                            style: current_style,
                        });
                    }
                    in_paragraph = false;
                    // Defence-in-depth: a missing `</w:t>` (corrupt file
                    // or a writer that uses self-closing `<w:t/>`) would
                    // otherwise leak `in_text=true` into the next
                    // paragraph and stitch unrelated text together.
                    in_text = false;
                    current_text.clear();
                }
                b"w:t" => in_text = false,
                _ => {}
            },
            Ok(Event::Empty(e)) if e.name().as_ref() == b"w:tab" && in_paragraph => {
                current_text.push('\t');
            }
            Ok(Event::Empty(e)) if e.name().as_ref() == b"w:br" && in_paragraph => {
                current_text.push('\n');
            }
            Ok(Event::Text(t)) if in_text => {
                let s = t.decode().map_err(|e| eyre!("decode DOCX text: {e}"))?;
                current_text.push_str(&s);
            }
            // Entity references (`&amp;`, `&#38;`) arrive as their own events.
            Ok(Event::GeneralRef(r)) if in_text => {
                if let Ok(Some(c)) = r.resolve_char_ref() {
                    current_text.push(c);
                } else if let Some(text) = r
                    .decode()
                    .ok()
                    .and_then(|name| quick_xml::escape::resolve_predefined_entity(&name))
                {
                    current_text.push_str(text);
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(eyre!("DOCX XML parse error: {e}")),
            _ => {}
        }
    }
    Ok(paragraphs)
}

fn classify_style(style_id: &str) -> ParaStyle {
    // Word emits both `Heading1`/`heading 1` style ids; normalise.
    let lc = style_id.to_lowercase().replace(' ', "");
    if lc == "heading1" || lc == "title" {
        ParaStyle::Heading1
    } else if lc == "heading2" {
        ParaStyle::Heading2
    } else {
        ParaStyle::Body
    }
}

/// Group paragraphs into sections under their parent heading. The
/// first non-heading paragraph runs become an `intro` chunk; each
/// H1/H2 starts a new chunk whose locator is the heading text.
fn group_into_sections(paragraphs: Vec<Para>) -> Vec<RawChunk> {
    let mut chunks: Vec<RawChunk> = Vec::new();
    let mut current = Section::new_intro();

    for para in paragraphs {
        match para.style {
            ParaStyle::Heading1 | ParaStyle::Heading2 => {
                if let Some(chunk) = current.into_chunk() {
                    chunks.push(chunk);
                }
                current = Section::new_heading(&para.text, para.style);
            }
            ParaStyle::Body => {
                current.push(&para.text);
            }
        }
    }
    if let Some(chunk) = current.into_chunk() {
        chunks.push(chunk);
    }
    chunks
}

struct Section {
    heading: Option<String>,
    locator: String,
    body: String,
}

impl Section {
    fn new_intro() -> Self {
        Self {
            heading: None,
            locator: "intro".to_string(),
            body: String::new(),
        }
    }
    fn new_heading(text: &str, style: ParaStyle) -> Self {
        let prefix = match style {
            ParaStyle::Heading1 => "#",
            _ => "##",
        };
        Self {
            heading: Some(text.to_string()),
            locator: format!("{prefix} {text}"),
            body: format!("{prefix} {text}\n"),
        }
    }
    fn push(&mut self, text: &str) {
        if !self.body.is_empty() && !self.body.ends_with('\n') {
            self.body.push('\n');
        }
        self.body.push_str(text);
        self.body.push('\n');
    }
    fn into_chunk(self) -> Option<RawChunk> {
        // Leading whitespace can't appear — the heading prefix or intro
        // start at byte 0 — so trim_end is enough and saves one alloc.
        let body = self.body.trim_end().to_owned();
        if body.is_empty() {
            return None;
        }
        let tags = self
            .heading
            .as_ref()
            .map(|h| vec![format!("section:{}", slugify(h))])
            .unwrap_or_default();
        Some(RawChunk {
            title: self.heading,
            body,
            locator: self.locator,
            tags,
        })
    }
}

fn slugify(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xml_doc(body_xml: &str) -> String {
        format!(
            r#"<?xml version="1.0"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
{body_xml}
</w:body>
</w:document>"#,
        )
    }

    fn para(style: Option<&str>, text: &str) -> String {
        let style = style
            .map(|s| format!(r#"<w:pPr><w:pStyle w:val="{s}"/></w:pPr>"#))
            .unwrap_or_default();
        format!(r#"<w:p>{style}<w:r><w:t>{text}</w:t></w:r></w:p>"#)
    }

    #[test]
    fn entity_references_keep_their_characters() {
        // quick-xml reports `&amp;` and `&#38;` as events of their own.
        let xml = xml_doc(&para(None, "Fish &amp; chips &#38; peas &lt;3"));
        let paragraphs = extract_paragraphs(&xml).unwrap();
        assert_eq!(paragraphs[0].text, "Fish & chips & peas <3");
    }

    #[test]
    fn extracts_paragraph_text_in_order() {
        let xml = xml_doc(&format!(
            "{}{}",
            para(None, "First paragraph."),
            para(None, "Second paragraph."),
        ));
        let paragraphs = extract_paragraphs(&xml).unwrap();
        assert_eq!(paragraphs.len(), 2);
        assert_eq!(paragraphs[0].text, "First paragraph.");
        assert_eq!(paragraphs[0].style, ParaStyle::Body);
        assert_eq!(paragraphs[1].text, "Second paragraph.");
    }

    #[test]
    fn detects_heading_styles() {
        let xml = xml_doc(&format!(
            "{}{}{}",
            para(Some("Heading1"), "Top"),
            para(Some("Heading2"), "Sub"),
            para(None, "Body content"),
        ));
        let paragraphs = extract_paragraphs(&xml).unwrap();
        assert_eq!(paragraphs[0].style, ParaStyle::Heading1);
        assert_eq!(paragraphs[1].style, ParaStyle::Heading2);
        assert_eq!(paragraphs[2].style, ParaStyle::Body);
    }

    #[test]
    fn classify_style_normalises_word_variants() {
        assert_eq!(classify_style("Heading1"), ParaStyle::Heading1);
        assert_eq!(classify_style("heading 1"), ParaStyle::Heading1);
        assert_eq!(classify_style("Title"), ParaStyle::Heading1);
        assert_eq!(classify_style("Heading2"), ParaStyle::Heading2);
        assert_eq!(classify_style("Normal"), ParaStyle::Body);
        assert_eq!(classify_style("Heading3"), ParaStyle::Body);
    }

    #[test]
    fn group_into_sections_creates_chunks_per_heading() {
        let paragraphs = vec![
            Para {
                text: "intro line".into(),
                style: ParaStyle::Body,
            },
            Para {
                text: "Top".into(),
                style: ParaStyle::Heading1,
            },
            Para {
                text: "under top".into(),
                style: ParaStyle::Body,
            },
            Para {
                text: "Sub".into(),
                style: ParaStyle::Heading2,
            },
            Para {
                text: "under sub".into(),
                style: ParaStyle::Body,
            },
        ];
        let chunks = group_into_sections(paragraphs);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].locator, "intro");
        assert_eq!(chunks[1].title.as_deref(), Some("Top"));
        assert_eq!(chunks[1].locator, "# Top");
        assert!(chunks[1].body.contains("under top"));
        assert_eq!(chunks[2].title.as_deref(), Some("Sub"));
        assert_eq!(chunks[2].locator, "## Sub");
    }

    #[test]
    fn group_skips_empty_intro_when_first_is_heading() {
        let paragraphs = vec![
            Para {
                text: "Top".into(),
                style: ParaStyle::Heading1,
            },
            Para {
                text: "body".into(),
                style: ParaStyle::Body,
            },
        ];
        let chunks = group_into_sections(paragraphs);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].title.as_deref(), Some("Top"));
    }

    #[test]
    fn non_docx_zip_errors_cleanly() {
        // A valid zip that's missing word/document.xml.
        let mut zip_bytes = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(&mut zip_bytes));
            writer
                .start_file::<_, ()>("hello.txt", zip::write::SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut writer, b"hello").unwrap();
            writer.finish().unwrap();
        }
        assert!(parse(&zip_bytes).is_err());
    }

    #[test]
    fn invalid_zip_errors_cleanly() {
        assert!(parse(b"not a zip").is_err());
    }

    #[test]
    fn handles_xml_space_preserve_runs() {
        // Word emits `xml:space="preserve"` whenever text has
        // leading/trailing whitespace. We must not drop that text or
        // collapse the surrounding markers.
        let xml = xml_doc(
            r#"<w:p><w:r><w:t xml:space="preserve">leading and trailing </w:t></w:r><w:r><w:t xml:space="preserve">stays joined</w:t></w:r></w:p>"#,
        );
        let paragraphs = extract_paragraphs(&xml).unwrap();
        assert_eq!(paragraphs.len(), 1);
        assert_eq!(paragraphs[0].text, "leading and trailing stays joined");
    }

    #[test]
    fn handles_multiple_runs_in_one_paragraph() {
        // Word splits styled text into multiple `<w:r>` (one per
        // formatting change). They must merge into a single `Para`.
        let xml = xml_doc(r#"<w:p><w:r><w:t>Hello </w:t></w:r><w:r><w:t>world</w:t></w:r></w:p>"#);
        let paragraphs = extract_paragraphs(&xml).unwrap();
        assert_eq!(paragraphs.len(), 1);
        assert_eq!(paragraphs[0].text, "Hello world");
    }
}
