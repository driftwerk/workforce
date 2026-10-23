//! Streaming bulk export: pipe a read-only result straight to a CSV file via
//! the shared streaming kernel and emit the delivery marker so it reaches the
//! conversation as a downloadable file. The bytes never enter the agent's
//! context — it gets row count and a download handle.

use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use serde_json::{json, Value};

use crate::artifacts::stream::{
    download_filename, download_marker, export_csv, RowProducer, RowSink, StreamCaps,
};
use crate::artifacts::ArtifactStore;
use crate::tools::image_gen::GENERATED_IMAGES_KEY;

use super::backend::Backend;

const CSV_MIME: &str = "text/csv";

/// Drives the streaming export from a read-only statement: the backend's
/// server-side cursor pushes rows straight into the kernel's CSV sink.
struct SqlProducer<'a> {
    backend: &'a dyn Backend,
    sql: &'a str,
    timeout: Duration,
}

#[async_trait]
impl RowProducer for SqlProducer<'_> {
    async fn produce(&self, sink: &mut (dyn RowSink + Send)) -> Result<()> {
        self.backend.export(self.sql, self.timeout, sink).await?;
        Ok(())
    }
}

/// Stream `sql` to a CSV file and return the delivery output (a
/// `generated_images` marker the runner uploads).
pub async fn run(
    backend: &dyn Backend,
    sql: &str,
    timeout: Duration,
    instance: &str,
    filename: &str,
    agent_id: &str,
    store: &dyn ArtifactStore,
) -> Result<Value> {
    let caps = StreamCaps::default();
    let producer = SqlProducer {
        backend,
        sql,
        timeout,
    };
    let export = export_csv(store, agent_id, "database.export", caps, &producer).await?;
    let marker = download_marker(
        &export.artifact_id,
        CSV_MIME,
        &csv_filename(instance, filename),
        export.bytes,
    );
    Ok(json!({
        GENERATED_IMAGES_KEY: [marker],
        "instance": instance,
        "rows": export.rows,
        "truncated": export.truncated,
        "note": export_note(export.rows, export.truncated, caps),
    }))
}

fn export_note(rows: usize, truncated: bool, caps: StreamCaps) -> String {
    let ceiling = if truncated {
        format!(
            " (truncated at the {}-row / {}MB export ceiling — narrow the query for the full set)",
            caps.max_rows,
            caps.max_bytes / (1024 * 1024)
        )
    } else {
        String::new()
    };
    format!(
        "Exported {rows} row(s){ceiling} to a CSV file, delivered to this conversation as a \
         downloadable attachment — it happens outside your context, so you will NOT see the file. \
         Tell the user it is ready; do not try to read or re-upload it."
    )
}

fn csv_filename(instance: &str, requested: &str) -> String {
    let trimmed = requested.trim();
    let stem = if trimmed.is_empty() {
        format!("{instance}-export")
    } else {
        trimmed.trim_end_matches(".csv").to_string()
    };
    download_filename(&stem, "csv")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_is_sanitized_and_suffixed() {
        assert_eq!(csv_filename("pw-prod", ""), "pw-prod-export.csv");
        assert_eq!(csv_filename("pw", "my report"), "my-report.csv");
        assert_eq!(csv_filename("pw", "data.csv"), "data.csv");
        // Path separators are neutralized (the name only labels the download).
        assert_eq!(csv_filename("pw", "../etc/passwd"), "..-etc-passwd.csv");
    }
}
