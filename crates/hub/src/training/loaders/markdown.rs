//! Markdown loader: splits on H1/H2 headings into one chunk per
//! section. Locator format: `##<heading>` (the literal markdown).
//!
//! Content before the first heading becomes a single chunk with a
//! synthetic locator `intro`. Headings deeper than H2 stay inside
//! their parent section — they're part of the chunk body so the
//! agent sees their context.
//!
//! Why H1/H2 only: most curated reference material (handbooks,
//! runbooks, API guides) treats H1/H2 as the natural recall unit,
//! while H3+ are subtopics that get pulled in via the parent.

use eyre::{eyre, Result};
use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};

use super::RawChunk;

pub fn parse(bytes: &[u8]) -> Result<Vec<RawChunk>> {
    let text =
        std::str::from_utf8(bytes).map_err(|e| eyre!("markdown file is not valid UTF-8: {e}"))?;
    let cuts = section_offsets(text);
    let mut chunks = Vec::with_capacity(cuts.len());

    for (idx, cut) in cuts.iter().enumerate() {
        let end = cuts.get(idx + 1).map(|c| c.start).unwrap_or(text.len());
        let body = text[cut.start..end].trim();
        if body.is_empty() {
            continue;
        }
        chunks.push(RawChunk {
            title: cut.heading.clone(),
            body: body.to_string(),
            locator: cut.locator.clone(),
            tags: cut
                .heading
                .as_ref()
                .map(|h| vec![format!("section:{}", slugify(h))])
                .unwrap_or_default(),
        });
    }

    if chunks.is_empty() {
        return Err(eyre!("markdown file is empty"));
    }
    Ok(chunks)
}

struct Cut {
    start: usize,
    heading: Option<String>,
    locator: String,
}

/// Walk the markdown AST and record byte offsets where a new section
/// starts. The first cut covers any pre-heading content (locator
/// `intro`); subsequent cuts start at each H1/H2 boundary.
fn section_offsets(text: &str) -> Vec<Cut> {
    let parser = Parser::new(text).into_offset_iter();
    let mut cuts: Vec<Cut> = vec![Cut {
        start: 0,
        heading: None,
        locator: "intro".to_string(),
    }];
    let mut current_heading: Option<(HeadingLevel, usize)> = None;
    let mut heading_text = String::new();

    for (event, range) in parser {
        match event {
            Event::Start(Tag::Heading { level, .. })
                if matches!(level, HeadingLevel::H1 | HeadingLevel::H2) =>
            {
                current_heading = Some((level, range.start));
                heading_text.clear();
            }
            Event::Text(ref s) | Event::Code(ref s) if current_heading.is_some() => {
                heading_text.push_str(s);
            }
            Event::End(TagEnd::Heading(level))
                if matches!(level, HeadingLevel::H1 | HeadingLevel::H2) =>
            {
                if let Some((_, start)) = current_heading.take() {
                    let heading = heading_text.trim().to_string();
                    let prefix = match level {
                        HeadingLevel::H1 => "#",
                        _ => "##",
                    };
                    let locator = format!("{prefix} {heading}");
                    cuts.push(Cut {
                        start,
                        heading: Some(heading),
                        locator,
                    });
                }
            }
            _ => {}
        }
    }
    cuts
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

    #[test]
    fn splits_on_h1_and_h2_headings() {
        let src = b"intro text\n\n# First\n\nbody one\n\n## Second\n\nbody two\n";
        let chunks = parse(src).unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].locator, "intro");
        assert!(chunks[0].body.contains("intro text"));
        assert_eq!(chunks[1].title.as_deref(), Some("First"));
        assert_eq!(chunks[1].locator, "# First");
        assert!(chunks[1].body.contains("body one"));
        assert_eq!(chunks[2].title.as_deref(), Some("Second"));
        assert_eq!(chunks[2].locator, "## Second");
    }

    #[test]
    fn h3_stays_inside_parent() {
        let src = b"# Top\n\n## Sub\n\n### Deep\n\nbody";
        let chunks = parse(src).unwrap();
        assert_eq!(chunks.len(), 2);
        assert!(chunks[1].body.contains("### Deep"));
    }

    #[test]
    fn no_intro_when_first_line_is_heading() {
        let src = b"# Top\n\nbody";
        let chunks = parse(src).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].title.as_deref(), Some("Top"));
    }

    #[test]
    fn empty_input_errors() {
        assert!(parse(b"").is_err());
        assert!(parse(b"   \n   \n").is_err());
    }

    #[test]
    fn slugify_lowercases_and_dashes() {
        assert_eq!(slugify("Hello World"), "hello-world");
        assert_eq!(slugify("API/Reference"), "api-reference");
        assert_eq!(slugify("123 GO!"), "123-go");
    }

    #[test]
    fn ignores_headings_inside_fenced_code_blocks() {
        let src = b"# Real Heading\n\nbefore fence\n\n```\n# fake heading inside code\n```\n\nafter fence\n";
        let chunks = parse(src).unwrap();
        assert_eq!(chunks.len(), 1, "fence content must stay in one chunk");
        assert_eq!(chunks[0].title.as_deref(), Some("Real Heading"));
        assert!(chunks[0].body.contains("# fake heading inside code"));
        assert!(chunks[0].body.contains("after fence"));
    }
}
