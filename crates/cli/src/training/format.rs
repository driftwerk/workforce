//! Shared formatting for `workforce training docs`.
//!
//! Functional `String`-building: every renderer collects an iterator
//! of `format_*` outputs into a `Vec<String>`-joined `String`. Both
//! the direct and remote modes adapt their per-doc input into
//! `Vec<DocRow>` then call the same renderer, so the table is
//! byte-identical between modes.

use chrono::{DateTime, Utc};
use uuid::Uuid;

const FILENAME_MAX: usize = 48;
const KIB: f64 = 1024.0;

/// One row in the documents table.
///
/// `chunk_count: None` means the count couldn't be fetched (e.g.
/// Weaviate-side error) — the row is shown with `?` rather than
/// silently falling back to `0`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocRow {
    pub id: Uuid,
    pub short_id: String,
    pub filename: String,
    pub size_bytes: i64,
    pub chunk_count: Option<usize>,
    pub status: String,
    pub uploaded_at: DateTime<Utc>,
}

pub fn render_doc_table(agent: &str, corpus: &str, rows: &[DocRow]) -> String {
    if rows.is_empty() {
        return format!("no documents in corpus `{corpus}` for agent `{agent}`");
    }
    let header = format!(
        "{:<8}  {:<48}  {:>10}  {:>6}  {:<10}  {}",
        "SHORT ID", "FILENAME", "SIZE", "CHUNKS", "STATUS", "UPLOADED",
    );
    let body = rows.iter().map(format_row).collect::<Vec<_>>().join("\n");
    format!("{header}\n{body}")
}

fn format_row(row: &DocRow) -> String {
    format!(
        "{:<8}  {:<48}  {:>10}  {:>6}  {:<10}  {}",
        row.short_id,
        truncate(&row.filename, FILENAME_MAX),
        human_size(row.size_bytes),
        chunk_display(row.chunk_count),
        row.status,
        row.uploaded_at.to_rfc3339(),
    )
}

fn chunk_display(count: Option<usize>) -> String {
    count
        .map(|n| n.to_string())
        .unwrap_or_else(|| "?".to_string())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// Format a byte count with binary (KiB/MiB/GiB) prefixes. Negative
/// inputs are clamped to zero — `size_bytes` is signed in the schema
/// only because `tokio_postgres` maps `BIGINT` to `i64`.
pub fn human_size(bytes: i64) -> String {
    let n = bytes.max(0) as f64;
    if n < KIB {
        return format!("{} B", bytes.max(0));
    }
    let (value, unit) = if n < KIB.powi(2) {
        (n / KIB, "KiB")
    } else if n < KIB.powi(3) {
        (n / KIB.powi(2), "MiB")
    } else if n < KIB.powi(4) {
        (n / KIB.powi(3), "GiB")
    } else {
        (n / KIB.powi(4), "TiB")
    };
    format!("{value:.1} {unit}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(short: &str, name: &str, bytes: i64, chunks: Option<usize>) -> DocRow {
        DocRow {
            id: Uuid::nil(),
            short_id: short.into(),
            filename: name.into(),
            size_bytes: bytes,
            chunk_count: chunks,
            status: "ready".into(),
            uploaded_at: DateTime::parse_from_rfc3339("2026-06-04T09:12:33Z")
                .unwrap()
                .with_timezone(&Utc),
        }
    }

    #[test]
    fn human_size_buckets_correctly() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(2_200_000), "2.1 MiB");
        assert_eq!(human_size(5_000_000_000), "4.7 GiB");
    }

    #[test]
    fn human_size_uses_binary_prefix() {
        assert!(human_size(1024).contains("KiB"));
        assert!(!human_size(1024).contains("kB"));
        assert!(!human_size(1024).contains("KB"));
    }

    #[test]
    fn human_size_clamps_negative_input() {
        assert_eq!(human_size(-5), "0 B");
    }

    #[test]
    fn render_doc_table_empty_input_prints_user_friendly_message() {
        let out = render_doc_table("mark", "handbook", &[]);
        assert_eq!(out, "no documents in corpus `handbook` for agent `mark`");
    }

    #[test]
    fn render_doc_table_includes_header_and_one_row_per_doc() {
        let rows = vec![
            row("abc12345", "handbook.pdf", 2_200_000, Some(47)),
            row("def67890", "policies.docx", 148_480, Some(8)),
        ];
        let out = render_doc_table("mark", "handbook", &rows);
        assert!(out.starts_with("SHORT ID"));
        assert_eq!(out.lines().count(), 3);
        assert!(out.contains("abc12345"));
        assert!(out.contains("handbook.pdf"));
        assert!(out.contains("2.1 MiB"));
        assert!(out.contains("145.0 KiB"));
    }

    #[test]
    fn render_doc_table_marks_unknown_chunk_count() {
        let rows = vec![row("abc12345", "x.pdf", 100, None)];
        let out = render_doc_table("mark", "handbook", &rows);
        let last = out.lines().last().unwrap();
        assert!(
            last.contains(" ?  "),
            "chunk column must show `?` for None: {last:?}",
        );
        assert!(!last.contains(" 0  "), "must not silently fall back to 0");
    }

    #[test]
    fn render_doc_table_truncates_long_filename() {
        let long = "a".repeat(120);
        let rows = vec![row("abc12345", &long, 100, Some(1))];
        let out = render_doc_table("mark", "handbook", &rows);
        assert!(out.contains('…'), "long names get an ellipsis: {out}");
        for line in out.lines().skip(1) {
            assert!(!line.contains(&"a".repeat(120)), "untruncated: {line}");
        }
    }

    #[test]
    fn truncate_keeps_short_strings_intact() {
        assert_eq!(truncate("short", 48), "short");
    }

    #[test]
    fn truncate_handles_multibyte_characters() {
        let mixed = "héllo wörld ☃️ ".repeat(10);
        let out = truncate(&mixed, 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with('…'));
    }
}
