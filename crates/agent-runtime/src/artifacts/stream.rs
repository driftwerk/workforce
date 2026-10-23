//! Streaming export kernel: drive a row producer into a capped CSV file on disk
//! (never materialising the set in memory), move it into the artifact store via
//! a zero-copy rename, and return a delivery handle. Shared by every tool/skill
//! that can emit high volumes of tabular data, so large exports are memory-safe
//! by construction rather than by cap.

use std::path::Path;

use async_trait::async_trait;
use eyre::{eyre, Result};
use serde_json::{json, Value};

use super::store::ATTACHMENT_TTL;
use super::types::{ArtifactDescriptor, ArtifactId, ContentType};
use super::ArtifactStore;

/// Whether a streaming consumer wants more rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Stop,
}

/// Object-safe streaming sink. A producer calls [`RowSink::columns`] once, then
/// [`RowSink::push`] per row until it returns [`Flow::Stop`] or the source is
/// exhausted.
pub trait RowSink: Send {
    fn columns(&mut self, names: &[String]) -> Result<()>;
    fn push(&mut self, cells: Vec<Value>) -> Result<Flow>;
}

/// Row and byte ceilings for a streaming export; the sink stops at whichever is
/// reached first and reports truncation.
#[derive(Debug, Clone, Copy)]
pub struct StreamCaps {
    pub max_rows: usize,
    pub max_bytes: usize,
}

impl Default for StreamCaps {
    fn default() -> Self {
        Self {
            max_rows: 5_000_000,
            max_bytes: 512 * 1024 * 1024,
        }
    }
}

/// A source of rows that drives a [`RowSink`]. Mirrors the database
/// `Backend::export` shape so any streamable producer — a DB cursor, a paged
/// API — adopts the streaming export without buffering.
#[async_trait]
pub trait RowProducer: Send + Sync {
    async fn produce(&self, sink: &mut (dyn RowSink + Send)) -> Result<()>;
}

/// Outcome of a streaming export: the staged artifact plus what was written.
#[derive(Debug)]
pub struct CsvExport {
    pub artifact_id: ArtifactId,
    pub rows: usize,
    pub bytes: u64,
    pub truncated: bool,
}

struct CsvSink {
    writer: csv::Writer<std::fs::File>,
    caps: StreamCaps,
    rows: usize,
    bytes: usize,
    truncated: bool,
}

impl CsvSink {
    fn new(file: std::fs::File, caps: StreamCaps) -> Self {
        Self {
            writer: csv::Writer::from_writer(file),
            caps,
            rows: 0,
            bytes: 0,
            truncated: false,
        }
    }
}

fn cell_to_field(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

impl RowSink for CsvSink {
    fn columns(&mut self, names: &[String]) -> Result<()> {
        self.writer.write_record(names)?;
        Ok(())
    }

    fn push(&mut self, cells: Vec<Value>) -> Result<Flow> {
        if self.rows >= self.caps.max_rows || self.bytes >= self.caps.max_bytes {
            self.truncated = true;
            return Ok(Flow::Stop);
        }
        let record: Vec<String> = cells.iter().map(cell_to_field).collect();
        self.bytes += record.iter().map(|f| f.len() + 1).sum::<usize>();
        self.writer.write_record(&record)?;
        self.rows += 1;
        Ok(Flow::Continue)
    }
}

/// A temp file under the store's staging dir (same filesystem as the store, so
/// the eventual move into it is a rename, not a copy), falling back to the OS
/// temp dir when the store exposes none. A producer fills it, then hands it to
/// [`stage_file`].
pub fn staging_tempfile(
    store: &dyn ArtifactStore,
    agent_id: &str,
    prefix: &str,
    suffix: &str,
) -> Result<tempfile::NamedTempFile> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(prefix).suffix(suffix);
    Ok(match store.staging_dir(agent_id) {
        Some(dir) => builder.tempfile_in(dir)?,
        None => builder.tempfile()?,
    })
}

/// Move a file already written under the staging dir into the store as a
/// delivery artifact, returning its id and on-disk size (the bytes are moved,
/// never re-read). On any error the source file is removed so a failed stage
/// leaves no orphan.
pub fn stage_file(
    store: &dyn ArtifactStore,
    agent_id: &str,
    producer_label: &str,
    path: &Path,
    row_count: Option<usize>,
) -> Result<(ArtifactId, u64)> {
    let staged = (|| -> Result<(ArtifactId, u64)> {
        let bytes = std::fs::metadata(path)?.len();
        let descriptor = ArtifactDescriptor {
            row_count,
            ..ArtifactDescriptor::from_context(producer_label, ContentType::OctetStream, agent_id)
                .with_ttl(ATTACHMENT_TTL)
        };
        let handle = store.write_from_path(agent_id, descriptor, path)?;
        Ok((handle.id, bytes))
    })();
    if staged.is_err() {
        let _ = std::fs::remove_file(path);
    }
    staged
}

/// Stream `producer`'s rows to a CSV file under the store's staging dir, then
/// register the file as an artifact. The bytes never enter the agent context.
pub async fn export_csv(
    store: &dyn ArtifactStore,
    agent_id: &str,
    producer_label: &str,
    caps: StreamCaps,
    producer: &dyn RowProducer,
) -> Result<CsvExport> {
    let temp = staging_tempfile(store, agent_id, "wf-export-", ".csv")?;
    let file = temp.as_file().try_clone()?;
    let mut sink = CsvSink::new(file, caps);
    producer.produce(&mut sink).await?;
    sink.writer.flush()?;
    let rows = sink.rows;
    let truncated = sink.truncated;

    let (_file, path) = temp
        .keep()
        .map_err(|e| eyre!("failed to retain export file: {e}"))?;
    let (artifact_id, bytes) = stage_file(store, agent_id, producer_label, &path, Some(rows))?;
    Ok(CsvExport {
        artifact_id,
        rows,
        bytes,
        truncated,
    })
}

/// The inner `generated_images` delivery object every out-of-band file marker
/// shares: an artifact id the runner uploads, its media type, download filename,
/// and size.
pub fn download_marker(id: &ArtifactId, media_type: &str, filename: &str, bytes: u64) -> Value {
    json!({
        "artifact_id": id.as_str(),
        "media_type": media_type,
        "filename": filename,
        "bytes": bytes,
    })
}

/// Sanitise `stem` and `ext` into a safe download filename ending in `.{ext}`:
/// alphanumerics and `-_.` survive; anything else (path separators, spaces)
/// becomes `-`. The name only labels the download — it is never a path. `ext`
/// is sanitised too, since callers like the WASM artifact bridge derive it from
/// an untrusted, guest-supplied filename.
pub fn download_filename(stem: &str, ext: &str) -> String {
    let sanitise = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '-'
                }
            })
            .collect()
    };
    let safe = sanitise(stem);
    let suffix = format!(".{}", sanitise(ext));
    if safe.ends_with(&suffix) {
        safe
    } else {
        format!("{safe}{suffix}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::store::FsArtifactStore;
    use crate::artifacts::types::ArtifactBody;

    struct VecProducer {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
    }

    #[async_trait]
    impl RowProducer for VecProducer {
        async fn produce(&self, sink: &mut (dyn RowSink + Send)) -> Result<()> {
            sink.columns(&self.columns)?;
            for row in &self.rows {
                if sink.push(row.clone())? == Flow::Stop {
                    break;
                }
            }
            Ok(())
        }
    }

    struct FailingProducer;

    #[async_trait]
    impl RowProducer for FailingProducer {
        async fn produce(&self, sink: &mut (dyn RowSink + Send)) -> Result<()> {
            sink.columns(&["a".to_string()])?;
            Err(eyre!("source exploded"))
        }
    }

    fn store() -> (std::sync::Arc<FsArtifactStore>, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        (store, dir)
    }

    fn read_csv(store: &FsArtifactStore, agent: &str, export: &CsvExport) -> String {
        match store.read(agent, &export.artifact_id).unwrap() {
            ArtifactBody::Bytes(b) => String::from_utf8(b).unwrap(),
            other => panic!("expected bytes, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exports_rows_to_a_csv_artifact() {
        let (store, _g) = store();
        let producer = VecProducer {
            columns: vec!["name".into(), "n".into()],
            rows: vec![vec![json!("ann"), json!(1)], vec![json!("bo"), json!(2)]],
        };
        let export = export_csv(
            store.as_ref(),
            "agent-a",
            "test.export",
            StreamCaps::default(),
            &producer,
        )
        .await
        .unwrap();
        assert_eq!(export.rows, 2);
        assert!(!export.truncated);
        assert!(export.bytes > 0);
        assert_eq!(
            read_csv(&store, "agent-a", &export),
            "name,n\nann,1\nbo,2\n"
        );
    }

    #[tokio::test]
    async fn stops_at_row_ceiling_and_reports_truncation() {
        let (store, _g) = store();
        let producer = VecProducer {
            columns: vec!["n".into()],
            rows: (0..5).map(|i| vec![json!(i)]).collect(),
        };
        let caps = StreamCaps {
            max_rows: 2,
            ..StreamCaps::default()
        };
        let export = export_csv(store.as_ref(), "agent-a", "test.export", caps, &producer)
            .await
            .unwrap();
        assert_eq!(export.rows, 2);
        assert!(export.truncated);
        assert_eq!(read_csv(&store, "agent-a", &export), "n\n0\n1\n");
    }

    #[tokio::test]
    async fn stops_at_byte_ceiling() {
        let (store, _g) = store();
        let producer = VecProducer {
            columns: vec!["v".into()],
            rows: (0..100).map(|_| vec![json!("x".repeat(50))]).collect(),
        };
        let caps = StreamCaps {
            max_bytes: 120,
            ..StreamCaps::default()
        };
        let export = export_csv(store.as_ref(), "agent-a", "test.export", caps, &producer)
            .await
            .unwrap();
        assert!(export.truncated);
        assert!(export.rows < 100);
    }

    #[tokio::test]
    async fn producer_error_propagates_without_staging() {
        let (store, _g) = store();
        let err = export_csv(
            store.as_ref(),
            "agent-a",
            "test.export",
            StreamCaps::default(),
            &FailingProducer,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("source exploded"));
    }

    #[test]
    fn cell_rendering_matches_csv_expectations() {
        assert_eq!(cell_to_field(&Value::Null), "");
        assert_eq!(cell_to_field(&json!("hi")), "hi");
        assert_eq!(cell_to_field(&json!(42)), "42");
        assert_eq!(cell_to_field(&json!(true)), "true");
        assert_eq!(cell_to_field(&json!({"a": 1})), "{\"a\":1}");
    }

    #[test]
    fn download_marker_carries_handle_fields() {
        let marker = download_marker(&ArtifactId::fresh(), "text/csv", "report.csv", 42);
        assert_eq!(marker["media_type"], "text/csv");
        assert_eq!(marker["filename"], "report.csv");
        assert_eq!(marker["bytes"], 42);
        assert!(marker["artifact_id"].as_str().unwrap().starts_with("art_"));
    }

    #[test]
    fn download_filename_sanitizes_and_suffixes() {
        assert_eq!(download_filename("my report", "csv"), "my-report.csv");
        assert_eq!(download_filename("data.csv", "csv"), "data.csv");
        assert_eq!(
            download_filename("../etc/passwd", "csv"),
            "..-etc-passwd.csv"
        );
        // An untrusted, guest-supplied extension is sanitised too.
        assert_eq!(download_filename("out", "c/d"), "out.c-d");
    }
}
