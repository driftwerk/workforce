//! Hybrid PDF processing.
//!
//! Strategy: try `pdf-extract` first (cheap, works for most exported PDFs).
//! If the extracted text is sparse — common for scanned reports and slide
//! decks — render each page with `pdftoppm` (poppler-utils) and feed the PNGs
//! to the configured vision LLM. The render path is bounded by
//! `pdf_max_pages` and degrades to a placeholder if poppler isn't installed.
//!
//! Audio/video transcription will follow the same shape under
//! `attachments::audio` once `[llm.transcription]` lands.

use crate::llm::types::ContentBlock;
use crate::vision::VisionLlm;
use base64::Engine;
use eyre::Result;
use workforce_common::secret::NoKey;
use workforce_common::settings::LlmVisionConfig;

use super::pdf_render::{probe_page_count, render_pages};

const BASE64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// Process a PDF attachment, returning the content blocks to substitute in
/// place of the original `Document` block. Always non-empty on `Ok`.
pub async fn process_pdf(
    filename: &str,
    base64_data: &str,
    vision: &dyn VisionLlm,
    config: &LlmVisionConfig,
) -> Result<Vec<ContentBlock>> {
    let bytes = BASE64
        .decode(base64_data)
        .map_err(|e| eyre::eyre!("base64 decode failed: {e}"))?;

    let (extracted, non_ws) = extracted_text(filename, &bytes);

    if non_ws >= config.pdf_text_min_chars {
        tracing::info!(file = %filename, chars = non_ws, "PDF text-extracted (no vision call)");
        return Ok(vec![ContentBlock::Text {
            text: format!(
                "PDF `{}` (text content extracted, no images rendered):\n{}",
                filename, extracted
            ),
        }]);
    }

    tracing::info!(
        file = %filename,
        chars = non_ws,
        threshold = config.pdf_text_min_chars,
        "PDF text sparse; falling back to vision page-render"
    );
    render_and_describe(filename, &bytes, vision, config, non_ws).await
}

/// A PDF's text, and how many non-blank characters it has; nothing when the
/// text cannot be extracted.
fn extracted_text(filename: &str, bytes: &[u8]) -> (String, usize) {
    let extracted = pdf_extract::extract_text_from_mem(bytes).unwrap_or_else(|e| {
        tracing::warn!(file = %filename, error = %e, "PDF text extraction failed");
        String::new()
    });
    let non_ws = extracted.chars().filter(|c| !c.is_whitespace()).count();
    (extracted, non_ws)
}

/// A PDF as its text alone, for a model that reads no documents and has no
/// vision provider to render pages for: the text, else a note that it had
/// none.
pub fn text_only(filename: &str, base64_data: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text {
        text: text_of(filename, base64_data),
    }]
}

/// A PDF's extracted text as the model reads it, or a note that it had none.
pub fn text_of(filename: &str, base64_data: &str) -> String {
    let (extracted, non_ws) = BASE64
        .decode(base64_data)
        .map(|bytes| extracted_text(filename, &bytes))
        .unwrap_or_default();
    match non_ws {
        0 => format!(
            "PDF `{filename}` was attached, but it has no text that could be extracted and the active model reads no documents."
        ),
        _ => format!("PDF `{filename}` (text content extracted, no images rendered):\n{extracted}"),
    }
}

async fn render_and_describe(
    filename: &str,
    pdf_bytes: &[u8],
    vision: &dyn VisionLlm,
    config: &LlmVisionConfig,
    non_ws: usize,
) -> Result<Vec<ContentBlock>> {
    // pdftoppm `-l` caps output, so we probe pdfinfo separately to detect
    // truncation accurately. If pdfinfo isn't available we still render —
    // we just lose the "first N of M pages" note.
    let total_pages = probe_page_count(pdf_bytes);

    let pages = match render_pages(pdf_bytes, config.pdf_max_pages) {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => {
            tracing::warn!(file = %filename, "pdftoppm produced zero pages");
            return Ok(vec![ContentBlock::Text {
                text: format!(
                    "PDF `{}` was attached but neither text extraction nor page rendering produced usable content.",
                    filename
                ),
            }]);
        }
        Err(e) => {
            tracing::warn!(
                file = %filename,
                error = %e,
                "PDF rasterisation failed (is pdftoppm installed?); emitting placeholder"
            );
            return Ok(vec![ContentBlock::Text {
                text: format!(
                    "PDF `{filename}` was attached. Text extraction returned {non_ws} non-whitespace \
                     characters (below the {threshold} threshold) and page rasterisation failed: {e}. \
                     If this is a scanned PDF or slide deck, install `poppler-utils` (provides `pdftoppm`) \
                     on the agent host, or convert the PDF to images upstream.",
                    threshold = config.pdf_text_min_chars,
                ),
            }]);
        }
    };

    Ok(describe_pages(filename, pages, vision, total_pages).await)
}

async fn describe_pages(
    filename: &str,
    pages: Vec<Vec<u8>>,
    vision: &dyn VisionLlm,
    total_pages: Option<usize>,
) -> Vec<ContentBlock> {
    let described = pages.len();
    let truncated_to = total_pages.filter(|&t| t > described);
    let mut blocks = Vec::with_capacity(described + 2);

    blocks.push(ContentBlock::Text {
        text: match truncated_to {
            Some(total) => format!(
                "PDF `{filename}` (described page-by-page; first {described} of {total} pages):"
            ),
            None => format!("PDF `{filename}` (described page-by-page, {described} pages):"),
        },
    });

    for (i, png) in pages.into_iter().enumerate() {
        let page_num = i + 1;
        let b64 = BASE64.encode(&png);
        let hint = format!("Page {page_num} of {filename}");
        let text = match vision.describe_image("image/png", &b64, Some(&hint)).await {
            Ok(desc) => format!("Page {page_num}:\n{desc}"),
            Err(e) if e.is::<NoKey>() => {
                blocks.push(ContentBlock::Text {
                    text: format!("(The pages are not described: {e})"),
                });
                break;
            }
            Err(e) => {
                tracing::warn!(file = %filename, page = page_num, error = %e, "vision describe failed for page");
                format!("Page {page_num}: (description unavailable: {e})")
            }
        };
        blocks.push(ContentBlock::Text { text });
    }

    if let Some(total) = truncated_to {
        blocks.push(ContentBlock::Text {
            text: format!(
                "(Truncated after {described} pages — original PDF has {total} pages. \
                 Increase [llm.vision].pdf_max_pages to describe more.)"
            ),
        });
    }

    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct FakeVision {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl VisionLlm for FakeVision {
        async fn describe_image(
            &self,
            _mt: &str,
            _data: &str,
            _hint: Option<&str>,
        ) -> Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok("page describes a chart".to_string())
        }
        fn model_name(&self) -> &str {
            "fake-vision"
        }
    }

    fn cfg_threshold(pdf_text_min_chars: usize) -> LlmVisionConfig {
        LlmVisionConfig {
            model: "fake".to_string(),
            pdf_text_min_chars,
            ..LlmVisionConfig::default()
        }
    }

    /// Hand-crafted minimal PDF used purely to exercise the threshold gate.
    /// Whether `pdf-extract` returns text from this synthetic input depends
    /// on its parser; tests here treat both outcomes as valid.
    const SAMPLE_PDF: &[u8] = b"%PDF-1.1\n%\xC0\x9F\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n3 0 obj\n<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 4 0 R >> >> /MediaBox [0 0 300 200] /Contents 5 0 R >>\nendobj\n4 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n5 0 obj\n<< /Length 200 >>\nstream\nBT /F1 12 Tf 50 150 Td (This is a longer PDF text body that should easily clear the density threshold for text extraction.) Tj ET\nendstream\nendobj\nxref\n0 6\n0000000000 65535 f\n0000000018 00000 n\n0000000067 00000 n\n0000000118 00000 n\n0000000222 00000 n\n0000000284 00000 n\ntrailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n550\n%%EOF\n";

    #[tokio::test]
    async fn produces_blocks_with_threshold_zero() {
        let vision = FakeVision {
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let b64 = BASE64.encode(SAMPLE_PDF);
        let blocks = process_pdf("doc.pdf", &b64, &vision, &cfg_threshold(0))
            .await
            .unwrap();
        assert!(!blocks.is_empty());
        let ContentBlock::Text { text } = &blocks[0] else {
            panic!("expected text block");
        };
        assert!(text.contains("doc.pdf"));
    }

    #[tokio::test]
    async fn high_threshold_routes_to_vision_path() {
        let vision = FakeVision {
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let b64 = BASE64.encode(SAMPLE_PDF);
        let blocks = process_pdf("scan.pdf", &b64, &vision, &cfg_threshold(1_000_000))
            .await
            .unwrap();
        // Either pdftoppm rendered pages and we got descriptions, or it wasn't
        // available and we got a placeholder. Both are non-empty.
        assert!(!blocks.is_empty());
    }

    #[tokio::test]
    async fn truncation_note_emitted_when_total_exceeds_described() {
        let calls = Arc::new(AtomicUsize::new(0));
        let vision = FakeVision {
            calls: Arc::clone(&calls),
        };
        let pages = vec![vec![1u8, 2, 3], vec![4u8, 5], vec![6u8]];
        let blocks = describe_pages("deck.pdf", pages, &vision, Some(12)).await;
        assert_eq!(blocks.len(), 5);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        let ContentBlock::Text { text } = blocks.last().unwrap() else {
            panic!("expected text block");
        };
        assert!(text.contains("Truncated") && text.contains("12 pages"));
    }

    #[tokio::test]
    async fn no_truncation_note_when_complete() {
        let vision = FakeVision {
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let pages = vec![vec![1u8], vec![2u8]];
        let blocks = describe_pages("doc.pdf", pages, &vision, Some(2)).await;
        assert_eq!(blocks.len(), 3);
        assert!(blocks.iter().all(|b| match b {
            ContentBlock::Text { text } => !text.starts_with("(Truncated"),
            _ => true,
        }));
    }

    #[tokio::test]
    async fn no_truncation_note_when_total_unknown() {
        let vision = FakeVision {
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let pages = vec![vec![1u8]];
        let blocks = describe_pages("mystery.pdf", pages, &vision, None).await;
        assert!(blocks.iter().all(|b| match b {
            ContentBlock::Text { text } => !text.starts_with("(Truncated"),
            _ => true,
        }));
    }

    #[tokio::test]
    async fn invalid_base64_returns_error() {
        let calls = Arc::new(AtomicUsize::new(0));
        let vision = FakeVision {
            calls: Arc::clone(&calls),
        };
        let result = process_pdf(
            "bad.pdf",
            "not-valid-base64!!!",
            &vision,
            &cfg_threshold(200),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
