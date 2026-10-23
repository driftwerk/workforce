//! PDF loader: one `RawChunk` per page, citation locator `p.<n>`
//! (1-indexed).
//!
//! Two paths share the page-splitter:
//!
//! - [`parse`] — sync, text-only. Uses `pdf-extract`. Image-only PDFs
//!   error out clearly.
//! - [`parse_with_ocr`] — async, hybrid. Tries `pdf-extract` first; on
//!   sparse or empty text it rasterises the pages via the shared
//!   [`workforce_agent::attachments::pdf_render::render_pages`] helper and
//!   transcribes each via the configured `VisionLlm`. The vision
//!   adapter caches by SHA-256 of the PNG payload, so re-loading the
//!   same PDF skips the vision bill.
//!
//! The OCR chunks carry the `ocr` tag in addition to `page:N`, so
//! downstream eval / UI surfaces can tell vision-transcribed text
//! apart from native-text extraction.

use base64::Engine;
use eyre::{eyre, Result, WrapErr};
use workforce_agent::vision::VisionLlm;
use workforce_common::settings::LlmVisionConfig;

use super::RawChunk;

const FORM_FEED: char = '\x0c';
// Namespaced to match the rest of the training tag set (`corpus:`,
// `doc:`, `page:`) so future filters can use a single key:value scheme.
const OCR_TAG: &str = "source:ocr";
const BASE64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

pub fn parse(bytes: &[u8]) -> Result<Vec<RawChunk>> {
    let text = pdf_extract::extract_text_from_mem(bytes).wrap_err("extract text from PDF")?;
    parse_text_or_error(&text)
}

/// Hybrid PDF parse with vision fallback. When `vision` is `None` or
/// `pdf-extract` already returned enough text, this collapses to the
/// text path. Otherwise, each page is rasterised and transcribed by
/// the vision LLM and emitted as one chunk per page.
pub async fn parse_with_ocr(
    bytes: &[u8],
    vision: Option<&dyn VisionLlm>,
    config: &LlmVisionConfig,
) -> Result<Vec<RawChunk>> {
    let text = pdf_extract::extract_text_from_mem(bytes).unwrap_or_default();

    let Some(vision) = vision else {
        return parse_text_or_error(&text);
    };

    let non_ws = text.chars().filter(|c| !c.is_whitespace()).count();
    if non_ws >= config.pdf_text_min_chars {
        return parse_text_or_error(&text);
    }

    tracing::info!(
        chars = non_ws,
        threshold = config.pdf_text_min_chars,
        "training PDF text sparse; falling back to vision OCR",
    );
    let pages = workforce_agent::attachments::pdf_render::render_pages(bytes, config.pdf_max_pages)
        .wrap_err("PDF rasterisation failed (is `poppler-utils` installed on the hub host?)")?;
    parse_pages_via_vision(pages, vision).await
}

/// Apply the vision LLM to each rendered page and emit one
/// `RawChunk` per page that came back with non-empty text. Empty
/// pages (or pages whose vision call errored) are dropped silently
/// with a warn log — a flaky vision call on page 7 doesn't break
/// the whole load. A missing API key fails the load with the sentence
/// that says so, rather than storing the pages read before it went.
pub async fn parse_pages_via_vision(
    pages: Vec<Vec<u8>>,
    vision: &dyn VisionLlm,
) -> Result<Vec<RawChunk>> {
    let total = pages.len();
    let mut chunks = Vec::with_capacity(total);
    for (idx, png) in pages.into_iter().enumerate() {
        let page_no = idx + 1;
        let b64 = BASE64.encode(&png);
        let hint = format!("Training PDF page {page_no} of {total}");
        let body = match vision.describe_image("image/png", &b64, Some(&hint)).await {
            Ok(text) => text.trim().to_string(),
            Err(e) if e.is::<workforce_common::secret::NoKey>() => return Err(e),
            Err(e) => {
                tracing::warn!(
                    page = page_no,
                    error = %e,
                    "training PDF: vision describe failed; skipping page",
                );
                continue;
            }
        };
        if body.is_empty() {
            tracing::warn!(
                page = page_no,
                "training PDF: vision returned empty text; skipping page",
            );
            continue;
        }
        chunks.push(RawChunk {
            title: None,
            body,
            locator: format!("p.{page_no}"),
            tags: vec![format!("page:{page_no}"), OCR_TAG.to_string()],
        });
    }
    if chunks.is_empty() {
        return Err(eyre!(
            "PDF OCR produced no usable text — every vision call returned empty",
        ));
    }
    Ok(chunks)
}

fn parse_text_or_error(text: &str) -> Result<Vec<RawChunk>> {
    if text.trim().is_empty() {
        return Err(eyre!(
            "PDF contained no extractable text — image-only PDFs need OCR. \
             Enable `[llm.vision]` on the hub, or upload via `workforce training load --gateway` \
             (CLI direct mode has no vision LLM wired).",
        ));
    }
    Ok(text
        .split(FORM_FEED)
        .enumerate()
        .filter_map(|(idx, page)| {
            let body = page.trim();
            (!body.is_empty()).then(|| {
                let page_no = idx + 1;
                RawChunk {
                    title: None,
                    body: body.to_string(),
                    locator: format!("p.{page_no}"),
                    tags: vec![format!("page:{page_no}")],
                }
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct FakeVision {
        responses: Vec<String>,
        calls: Arc<AtomicUsize>,
    }

    impl FakeVision {
        fn new(responses: Vec<&str>) -> Self {
            Self {
                responses: responses.into_iter().map(String::from).collect(),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    #[async_trait]
    impl VisionLlm for FakeVision {
        async fn describe_image(
            &self,
            _media_type: &str,
            _base64_data: &str,
            _hint: Option<&str>,
        ) -> Result<String> {
            let idx = self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .responses
                .get(idx)
                .cloned()
                .unwrap_or_else(|| "fallback page".to_string()))
        }
        fn model_name(&self) -> &str {
            "fake-vision"
        }
    }

    fn vision_cfg(min_chars: usize, max_pages: usize) -> LlmVisionConfig {
        LlmVisionConfig {
            model: "fake".to_string(),
            pdf_text_min_chars: min_chars,
            pdf_max_pages: max_pages,
            ..LlmVisionConfig::default()
        }
    }

    fn synth_png(label: u8) -> Vec<u8> {
        vec![0x89, 0x50, 0x4E, 0x47, label]
    }

    #[test]
    fn split_helper_creates_one_chunk_per_non_empty_page() {
        let synthetic = "page one body\x0cpage two body\x0c   \x0cpage four body";
        let chunks = parse_text_or_error(synthetic).unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].locator, "p.1");
        assert_eq!(chunks[1].locator, "p.2");
        assert_eq!(chunks[2].locator, "p.4");
        for c in &chunks {
            assert!(!c.tags.contains(&"source:ocr".to_string()));
        }
    }

    #[test]
    fn empty_pdf_errors() {
        assert!(parse(b"not a pdf").is_err());
        let err = parse_text_or_error("   \x0c   ").unwrap_err().to_string();
        assert!(err.contains("OCR"), "operator-facing hint missing: {err}");
    }

    #[tokio::test]
    async fn parse_pages_via_vision_emits_one_chunk_per_page() {
        let pages = vec![synth_png(1), synth_png(2), synth_png(3)];
        let vision = FakeVision::new(vec!["page one", "page two", "page three"]);
        let chunks = parse_pages_via_vision(pages, &vision).await.unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].body, "page one");
        assert_eq!(chunks[0].locator, "p.1");
        assert!(chunks[0].tags.iter().any(|t| t == "source:ocr"));
        assert!(chunks[0].tags.iter().any(|t| t == "page:1"));
        assert_eq!(chunks[2].locator, "p.3");
        assert_eq!(vision.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn parse_pages_via_vision_skips_empty_responses() {
        let pages = vec![synth_png(1), synth_png(2), synth_png(3)];
        let vision = FakeVision::new(vec!["page one", "   ", "page three"]);
        let chunks = parse_pages_via_vision(pages, &vision).await.unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].locator, "p.1");
        assert_eq!(chunks[1].locator, "p.3");
    }

    #[tokio::test]
    async fn a_missing_key_fails_the_load_saying_so_instead_of_storing_part_of_it() {
        use workforce_common::secret::NoKey;
        struct KeyGone(AtomicUsize);
        #[async_trait]
        impl VisionLlm for KeyGone {
            async fn describe_image(&self, _: &str, _: &str, _: Option<&str>) -> Result<String> {
                match self.0.fetch_add(1, Ordering::SeqCst) {
                    0 => Ok("page one".into()),
                    _ => Err(NoKey("Image description is not available to you.".into()).into()),
                }
            }
            fn model_name(&self) -> &str {
                "key-gone"
            }
        }
        let vision = KeyGone(AtomicUsize::new(0));
        let pages = vec![synth_png(1), synth_png(2), synth_png(3)];
        let err = parse_pages_via_vision(pages, &vision).await.unwrap_err();
        assert_eq!(
            err.downcast_ref::<NoKey>()
                .map(ToString::to_string)
                .as_deref(),
            Some("Image description is not available to you.")
        );
        assert_eq!(vision.0.load(Ordering::SeqCst), 2, "it stops at the first");
    }

    #[tokio::test]
    async fn parse_pages_via_vision_errors_when_no_page_describes() {
        let pages = vec![synth_png(1), synth_png(2)];
        let vision = FakeVision::new(vec!["", ""]);
        let err = parse_pages_via_vision(pages, &vision).await.unwrap_err();
        assert!(err.to_string().to_lowercase().contains("ocr"));
    }

    #[tokio::test]
    async fn parse_with_ocr_collapses_to_parse_when_vision_is_none() {
        let err = parse_with_ocr(b"not a pdf", None, &vision_cfg(50, 5))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("OCR"), "expected OCR hint: {err}");
    }
}
