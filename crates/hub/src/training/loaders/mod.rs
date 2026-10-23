//! Format-aware corpus loaders. Each loader returns a `Vec<RawChunk>`;
//! the pipeline takes it from there.
//!
//! Supported formats (DW-539 Phase 1a + DW-541 Phase 3a):
//!
//! | Format   | Extensions                       | Chunk strategy                  |
//! |----------|----------------------------------|---------------------------------|
//! | JSON     | `.json`                          | one chunk per `entries[]`       |
//! | PDF      | `.pdf`                           | one chunk per page              |
//! | XLSX     | `.xlsx .xls .ods .xlsb .xlsm`    | one chunk per sheet (markdown)  |
//! | TXT      | `.txt .text`                     | one chunk per paragraph         |
//! | Markdown | `.md .markdown`                  | one chunk per H1/H2 section     |
//! | DOCX     | `.docx`                          | one chunk per heading section   |
//!
//! Image/scan-only PDFs need a separate OCR loader (deferred).

pub mod docx;
pub mod json;
pub mod markdown;
pub mod pdf;
pub mod text;
pub mod xlsx;

use eyre::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawChunk {
    pub title: Option<String>,
    pub body: String,
    pub locator: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json,
    Pdf,
    Xlsx,
    Text,
    Markdown,
    Docx,
}

impl Format {
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Json => &["json"],
            Self::Pdf => &["pdf"],
            Self::Xlsx => &["xlsx", "xls", "ods", "xlsb", "xlsm"],
            Self::Text => &["txt", "text"],
            Self::Markdown => &["md", "markdown"],
            Self::Docx => &["docx"],
        }
    }
}

const ALL_FORMATS: [Format; 6] = [
    Format::Json,
    Format::Pdf,
    Format::Xlsx,
    Format::Text,
    Format::Markdown,
    Format::Docx,
];

/// Pick a format from the filename's extension. Returns `None` when
/// no loader handles the extension — caller surfaces a clear error
/// listing the supported set.
pub fn detect_format(filename: &str) -> Option<Format> {
    let ext = filename.rsplit_once('.').map(|(_, e)| e.to_lowercase())?;
    ALL_FORMATS
        .into_iter()
        .find(|fmt| fmt.extensions().iter().any(|e| *e == ext))
}

pub fn load(format: Format, bytes: &[u8]) -> Result<Vec<RawChunk>> {
    match format {
        Format::Json => json::parse(bytes),
        Format::Pdf => pdf::parse(bytes),
        Format::Xlsx => xlsx::parse(bytes),
        Format::Text => text::parse(bytes),
        Format::Markdown => markdown::parse(bytes),
        Format::Docx => docx::parse(bytes),
    }
}

/// Comma-separated list of every extension we can load. Used in error
/// messages and CLI help output.
pub fn supported_extensions() -> String {
    let mut exts: Vec<&'static str> = ALL_FORMATS
        .iter()
        .flat_map(|f| f.extensions().iter().copied())
        .collect();
    exts.sort_unstable();
    exts.iter()
        .map(|e| format!(".{e}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_format_matches_known_extensions() {
        assert_eq!(detect_format("x.json"), Some(Format::Json));
        assert_eq!(detect_format("X.JSON"), Some(Format::Json));
        assert_eq!(detect_format("x.pdf"), Some(Format::Pdf));
        assert_eq!(detect_format("x.PDF"), Some(Format::Pdf));
        assert_eq!(detect_format("notes.txt"), Some(Format::Text));
        assert_eq!(detect_format("README.md"), Some(Format::Markdown));
        assert_eq!(detect_format("data.xlsx"), Some(Format::Xlsx));
        assert_eq!(detect_format("data.ods"), Some(Format::Xlsx));
        assert_eq!(detect_format("brief.docx"), Some(Format::Docx));
        assert_eq!(detect_format("no-extension"), None);
        assert_eq!(detect_format("photo.gif"), None);
    }

    #[test]
    fn supported_extensions_lists_every_format() {
        let exts = supported_extensions();
        for required in [".json", ".pdf", ".xlsx", ".txt", ".md", ".docx"] {
            assert!(exts.contains(required), "missing {required} in {exts}");
        }
    }
}
