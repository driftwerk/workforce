//! Plain-text loader. Splits on blank lines (one or more `\n\n`) so a
//! paragraph becomes one chunk. Locator format: `para.<n>` (1-indexed).
//!
//! UTF-8 only. BOM is stripped if present. Mixed line endings (CRLF
//! and LF) are normalised so the splitter doesn't produce per-line
//! chunks on Windows-encoded files.

use eyre::{eyre, Result};

use super::RawChunk;

pub fn parse(bytes: &[u8]) -> Result<Vec<RawChunk>> {
    let text =
        std::str::from_utf8(bytes).map_err(|e| eyre!("plain-text file is not valid UTF-8: {e}"))?;
    let normalised = text
        .strip_prefix('\u{feff}')
        .unwrap_or(text)
        .replace("\r\n", "\n")
        .replace('\r', "\n");

    let mut paragraphs = Vec::new();
    let mut current = String::new();
    for line in normalised.lines() {
        if line.trim().is_empty() {
            if !current.is_empty() {
                paragraphs.push(std::mem::take(&mut current));
            }
        } else {
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(line);
        }
    }
    if !current.is_empty() {
        paragraphs.push(current);
    }

    if paragraphs.is_empty() {
        return Err(eyre!("text file is empty"));
    }

    Ok(paragraphs
        .into_iter()
        .enumerate()
        .map(|(idx, body)| RawChunk {
            title: None,
            body,
            locator: format!("para.{}", idx + 1),
            tags: Vec::new(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_blank_lines() {
        let src = b"first paragraph\nspans two lines\n\nsecond\n\nthird";
        let chunks = parse(src).unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].body, "first paragraph\nspans two lines");
        assert_eq!(chunks[0].locator, "para.1");
        assert_eq!(chunks[1].body, "second");
        assert_eq!(chunks[2].body, "third");
    }

    #[test]
    fn handles_crlf_line_endings() {
        let src = b"para one\r\n\r\npara two";
        let chunks = parse(src).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[1].body, "para two");
    }

    #[test]
    fn strips_bom() {
        let src = "\u{feff}hello".as_bytes();
        let chunks = parse(src).unwrap();
        assert_eq!(chunks[0].body, "hello");
    }

    #[test]
    fn collapses_multiple_blank_lines() {
        let src = b"one\n\n\n\n\ntwo";
        let chunks = parse(src).unwrap();
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn empty_input_errors() {
        assert!(parse(b"").is_err());
        assert!(parse(b"   \n   \n").is_err());
    }

    #[test]
    fn rejects_invalid_utf8() {
        let bad = [0xFF, 0xFE, 0xFD];
        assert!(parse(&bad).is_err());
    }
}
