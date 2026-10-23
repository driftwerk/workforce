//! `export_csv` — stream arbitrary tabular data to a CSV file and deliver it.
//!
//! The CSV sibling of `export_xlsx`: rows come inline as `data` or by
//! `artifact_id`, and are streamed straight to disk through the shared
//! [`crate::artifacts::stream`] kernel — no workbook is built in memory and the
//! output is capped — so it stays memory-safe on high-volume data where
//! `export_xlsx` would buffer the whole workbook. Raw JSON values are written
//! as-is (integers stay integers), unlike the spreadsheet path which coerces
//! every number to a float cell. Use it for large downloads; reach for
//! `export_xlsx` when you need a styled or multi-sheet workbook or a `layout`
//! reshape.

use std::collections::HashSet;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde_json::{json, Value};

use super::export_xlsx::{resolve_rows, ResolvedRows};
use super::image_gen::GENERATED_IMAGES_KEY;
use super::{Tool, ToolResult};
use crate::artifacts::store::global_store;
use crate::artifacts::stream::{
    download_filename, download_marker, export_csv, Flow, RowProducer, RowSink, StreamCaps,
};
use crate::runner::CURRENT_AGENT_ID;

pub const EXPORT_CSV_TOOL_NAME: &str = "export_csv";

const CSV_MIME: &str = "text/csv";

const DESCRIPTION: &str = "Stream tabular data to a .csv file and deliver it to this conversation \
    as a downloadable file. Provide the rows as `data` — an array of objects whose keys become the \
    columns — or, for data another tool already produced, an `artifact_id` of a JSON rows array. \
    Rows are written straight to disk without buffering, so prefer this over export_xlsx for large \
    downloads; use export_xlsx when you need a styled or multi-sheet workbook or a `layout` reshape \
    (export_csv writes raw rows). The file is delivered out-of-band: you will NOT see it and the \
    rows do not re-enter your context, so don't try to read, re-upload, or verify it — just tell \
    the user it's ready.";

pub struct ExportCsvTool;

#[async_trait]
impl Tool for ExportCsvTool {
    fn name(&self) -> &str {
        EXPORT_CSV_TOOL_NAME
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "data": {
                    "type": "array",
                    "items": { "type": "object" },
                    "description": "Rows as objects; each object's keys become columns. Provide this OR artifact_id."
                },
                "artifact_id": {
                    "type": "string",
                    "description": "Alternative to `data`: an artifact id holding a JSON array of row objects."
                },
                "filename": { "type": "string", "description": "Output .csv filename (default export.csv)." }
            }
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = |t: Instant| t.elapsed().as_millis() as u64;

        let Some(agent_id) = CURRENT_AGENT_ID
            .try_with(|v| v.clone())
            .ok()
            .filter(|s| !s.is_empty())
        else {
            return Ok(ToolResult::failure(
                "no agent context in scope; cannot stage the export",
                elapsed(start),
            ));
        };
        let Some(store) = global_store() else {
            return Ok(ToolResult::failure(
                "artifact store is not configured; cannot stage the export",
                elapsed(start),
            ));
        };

        let ResolvedRows { rows, column_order } =
            match resolve_rows(&input, store.as_ref(), &agent_id) {
                Ok(r) => r,
                Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
            };
        if rows.is_empty() {
            return Ok(ToolResult::failure("no rows to export", elapsed(start)));
        }
        let producer = RowsProducer {
            columns: ordered_columns(&rows, column_order.as_deref()),
            rows,
        };
        let export = match export_csv(
            store.as_ref(),
            &agent_id,
            EXPORT_CSV_TOOL_NAME,
            StreamCaps::default(),
            &producer,
        )
        .await
        {
            Ok(e) => e,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("failed to write the CSV: {e}"),
                    elapsed(start),
                ))
            }
        };

        let stem = input["filename"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.trim_end_matches(".csv").to_string())
            .unwrap_or_else(|| "export".to_string());
        let filename = download_filename(&stem, "csv");
        let marker = download_marker(&export.artifact_id, CSV_MIME, &filename, export.bytes);
        let cap_note = if export.truncated {
            " The export hit the row/byte ceiling and was truncated — narrow the data to get the rest."
        } else {
            ""
        };
        Ok(ToolResult::success(
            json!({
                GENERATED_IMAGES_KEY: [marker],
                "rows": export.rows,
                "filename": filename,
                "truncated": export.truncated,
                "note": format!(
                    "Exported {} row(s) to a CSV file, delivered to this conversation as a \
                     downloadable attachment — it happens outside your context, so you will NOT \
                     see the file. Tell the user it is ready; do not try to read or re-upload \
                     it.{cap_note}",
                    export.rows
                ),
            }),
            elapsed(start),
        ))
    }
}

/// Column order for the export: any artifact-declared schema first (honored
/// verbatim), then any remaining row keys in first-seen order. Derived without
/// materialising a cell matrix so a large export stays streamed.
fn ordered_columns(rows: &[Value], declared: Option<&[String]>) -> Vec<String> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut columns: Vec<String> = Vec::new();
    for key in declared.unwrap_or(&[]) {
        if seen.insert(key.as_str()) {
            columns.push(key.clone());
        }
    }
    for obj in rows.iter().filter_map(Value::as_object) {
        for key in obj.keys() {
            if seen.insert(key.as_str()) {
                columns.push(key.clone());
            }
        }
    }
    columns
}

/// Streams row objects into the CSV sink: header once, then each row's values
/// in column order. Raw JSON values are pushed (the kernel renders them), so
/// types are preserved and no per-row cell matrix is held.
struct RowsProducer {
    columns: Vec<String>,
    rows: Vec<Value>,
}

#[async_trait]
impl RowProducer for RowsProducer {
    async fn produce(&self, sink: &mut (dyn RowSink + Send)) -> Result<()> {
        sink.columns(&self.columns)?;
        for row in &self.rows {
            let cells: Vec<Value> = self
                .columns
                .iter()
                .map(|key| row.get(key).cloned().unwrap_or(Value::Null))
                .collect();
            if sink.push(cells)? == Flow::Stop {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::store::{FsArtifactStore, DEFAULT_TTL};
    use crate::artifacts::types::{ArtifactBody, ArtifactDescriptor, ContentType};
    use crate::artifacts::{ArtifactId, ArtifactStore};
    use tempfile::TempDir;

    fn read_csv(store: &FsArtifactStore, id: &ArtifactId) -> String {
        match store.read("agent-a", id).unwrap() {
            ArtifactBody::Bytes(b) => String::from_utf8(b).unwrap(),
            other => panic!("expected bytes, got {other:?}"),
        }
    }

    async fn run(store: &FsArtifactStore, input: Value) -> crate::artifacts::CsvExport {
        let ResolvedRows { rows, column_order } = resolve_rows(&input, store, "agent-a").unwrap();
        let producer = RowsProducer {
            columns: ordered_columns(&rows, column_order.as_deref()),
            rows,
        };
        export_csv(
            store,
            "agent-a",
            EXPORT_CSV_TOOL_NAME,
            StreamCaps::default(),
            &producer,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn streams_inline_data_preserving_integer_values() {
        let dir = TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let export = run(
            &store,
            json!({ "data": [ { "name": "Ann", "n": 1 }, { "name": "Bo", "n": 2 } ] }),
        )
        .await;
        assert_eq!(export.rows, 2);
        assert!(!export.truncated);
        // Columns are serde's map order (alphabetical here); integers stay integers.
        assert_eq!(
            read_csv(&store, &export.artifact_id),
            "n,name\n1,Ann\n2,Bo\n"
        );
    }

    #[tokio::test]
    async fn streams_artifact_rows_preserving_declared_order() {
        let dir = TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let id = store
            .write(
                "agent-a",
                ArtifactDescriptor {
                    producer: "metabase_query".into(),
                    mime: ContentType::ApplicationJson,
                    schema: Some(vec!["status".into(), "id".into()]),
                    row_count: Some(1),
                    ttl_seconds: DEFAULT_TTL.as_secs(),
                    agent_id: "agent-a".into(),
                    session_id: None,
                    user_id: None,
                    filename: None,
                    media_type: None,
                },
                ArtifactBody::Json(json!([{ "id": 1, "status": "Unfilled" }])),
            )
            .unwrap()
            .id;
        let export = run(&store, json!({ "artifact_id": id.as_str() })).await;
        assert_eq!(
            read_csv(&store, &export.artifact_id),
            "status,id\nUnfilled,1\n",
            "schema order beats alphabetical; id stays an integer"
        );
    }

    #[test]
    fn ordered_columns_unions_keys_with_declared_first() {
        let rows = vec![json!({ "b": 1, "a": 2 }), json!({ "c": 3, "a": 4 })];
        assert_eq!(ordered_columns(&rows, Some(&["c".into()])), ["c", "a", "b"]);
        // No declared order → serde map order (alphabetical) across the union.
        assert_eq!(ordered_columns(&rows, None), ["a", "b", "c"]);
    }

    #[tokio::test]
    async fn execute_rejects_empty_data() {
        crate::artifacts::store::install_test_global_store();
        let res = CURRENT_AGENT_ID
            .scope(
                "export-csv-empty".to_string(),
                ExportCsvTool.execute(json!({ "data": [] })),
            )
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("no rows"));
    }
}
