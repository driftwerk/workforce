//! `pdftoppm`-backed PDF page rasterisation, shared between the
//! attachment processing path ([`crate::attachments::pdf`]) and the
//! training PDF loader's OCR fallback
//! ([`crate::training::loaders::pdf::parse_with_ocr`]).
//!
//! Why this is a sibling module rather than living inside either
//! caller: the two share the exact same external dependency
//! (`poppler-utils`), the same 150 DPI render heuristic, and the
//! same per-page PNG output shape. Keeping the impl in one place
//! means a tuning change (DPI bump, JPEG instead of PNG) lands in
//! one site.

use std::process::{Command, Stdio};

use eyre::Result;
use tempfile::TempDir;

/// Render each page of `pdf_bytes` to a PNG via `pdftoppm`, capped at
/// `max_pages`. Pages come back in 1-based order. Returns an empty
/// vec when `pdftoppm` produced no output (e.g. blank PDF).
pub fn render_pages(pdf_bytes: &[u8], max_pages: usize) -> Result<Vec<Vec<u8>>> {
    let dir = TempDir::new()?;
    let pdf_path = dir.path().join("input.pdf");
    std::fs::write(&pdf_path, pdf_bytes)?;
    let prefix = dir.path().join("page");

    let last_page = max_pages.max(1).to_string();
    let status = Command::new("pdftoppm")
        .args([
            "-png",
            "-r",
            // 150 DPI: legible UI screenshots without ballooning the payload.
            "150",
            "-f",
            "1",
            "-l",
            last_page.as_str(),
        ])
        .arg(&pdf_path)
        .arg(&prefix)
        .status()
        .map_err(|e| eyre::eyre!("failed to spawn pdftoppm: {e}"))?;

    if !status.success() {
        return Err(eyre::eyre!(
            "pdftoppm exited with status {status} (PDF may be malformed or encrypted)"
        ));
    }

    let mut pages: Vec<(usize, Vec<u8>)> = std::fs::read_dir(dir.path())?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("png") {
                return None;
            }
            let n = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|stem| stem.rsplit_once('-'))
                .and_then(|(_, n)| n.parse::<usize>().ok())?;
            std::fs::read(&path).ok().map(|bytes| (n, bytes))
        })
        .collect();
    pages.sort_by_key(|(n, _)| *n);
    Ok(pages.into_iter().map(|(_, b)| b).collect())
}

/// Probe total page count via `pdfinfo`. Returns `None` when the
/// tool is missing or the output is unparseable; the caller still
/// renders pages but loses the truncation guarantee.
pub fn probe_page_count(pdf_bytes: &[u8]) -> Option<usize> {
    use std::io::Write;
    let mut child = Command::new("pdfinfo")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    {
        let mut stdin = child.stdin.take()?;
        stdin.write_all(pdf_bytes).ok()?;
    }
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("Pages:")?.trim().parse().ok())
}
