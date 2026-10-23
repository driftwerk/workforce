//! `export_xlsx` — turn arbitrary tabular data into an `.xlsx` and deliver it.
//!
//! The skill-agnostic sibling of `xero.export`: any agent can hand it rows and
//! get a downloadable spreadsheet in the channel. Rows come either inline as
//! `data` (an array of objects — keys become columns) or by `artifact_id`
//! (a JSON rows artifact another tool staged, so large data never passes back
//! through the model). It reuses the same [`crate::export`] engine (table model,
//! [`Layout`] reshape, xlsx writer) and the same artifact → channel delivery
//! path as `generate_image` / `browser.pdf`, so the bytes never enter the LLM
//! context.

use std::time::Instant;

use async_trait::async_trait;
use eyre::{eyre, Result};
use serde_json::{json, Value};

use super::image_gen::GENERATED_IMAGES_KEY;
use super::{Tool, ToolResult};
use crate::artifacts::download_marker;
use crate::artifacts::resolve::resolve_table_artifact;
use crate::artifacts::store::global_store;
use crate::artifacts::types::{ArtifactBody, ArtifactDescriptor, ArtifactId, ContentType};
use crate::artifacts::ArtifactStore;
use crate::export::{
    apply_layout, build_xlsx, reorder_columns, table_from_objects, xlsx_filename, Layout, Sheet,
    Table, XLSX_MIME,
};
use crate::runner::CURRENT_AGENT_ID;

pub const EXPORT_XLSX_TOOL_NAME: &str = "export_xlsx";

pub const EXPORT_XLSX_DESCRIPTION: &str =
    "Build an .xlsx spreadsheet from tabular data and deliver it to this \
    conversation as a downloadable file. Provide the rows as `data` — an array of objects whose \
    keys become the columns — or, for data another tool already produced, an `artifact_id` of a \
    JSON rows array (so large data needn't pass back through you). Optional `layout` selects, \
    renames and reorders columns and can group + aggregate, sort, and add a totals row. To put \
    several tables in ONE workbook, pass `sheets` — an array of {sheet_name, data|artifact_id, \
    layout} entries — instead of the single-sheet fields. The file is delivered out-of-band: you \
    will NOT see it and the rows do not re-enter your context, so don't try to read, re-upload, or \
    verify it — just tell the user it's ready. For Xero data use `xero.export` instead; it fetches \
    and exports in one step.";

pub struct ExportXlsxTool;

#[async_trait]
impl Tool for ExportXlsxTool {
    fn name(&self) -> &str {
        EXPORT_XLSX_TOOL_NAME
    }

    fn description(&self) -> &str {
        EXPORT_XLSX_DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        export_xlsx_input_schema()
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = |t: Instant| t.elapsed().as_millis() as u64;

        let agent_id = CURRENT_AGENT_ID
            .try_with(|v| v.clone())
            .ok()
            .filter(|s| !s.is_empty());
        let Some(agent_id) = agent_id else {
            return Ok(ToolResult::failure(
                "no agent context in scope; cannot stage the spreadsheet",
                elapsed(start),
            ));
        };
        let Some(store) = global_store() else {
            return Ok(ToolResult::failure(
                "artifact store is not configured; cannot stage the spreadsheet",
                elapsed(start),
            ));
        };

        let (bytes, filename, total_rows, sheet_count) =
            match build_xlsx_from_input(&input, store.as_ref(), &agent_id) {
                Ok(v) => v,
                Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
            };
        let size = bytes.len();

        let descriptor = ArtifactDescriptor {
            row_count: Some(total_rows),
            ..ArtifactDescriptor::from_context(
                EXPORT_XLSX_TOOL_NAME,
                ContentType::OctetStream,
                agent_id.clone(),
            )
            .with_file_meta(filename.clone(), XLSX_MIME)
        };
        let handle = match store.write(&agent_id, descriptor, ArtifactBody::Bytes(bytes)) {
            Ok(h) => h,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("failed to stage the spreadsheet for delivery: {e}"),
                    elapsed(start),
                ))
            }
        };

        let output = json!({
            GENERATED_IMAGES_KEY: [download_marker(&handle.id, XLSX_MIME, &filename, size as u64)],
            "rows": total_rows,
            "sheets": sheet_count,
            "filename": filename,
            "note": "Spreadsheet generated and being delivered to this conversation as a downloadable \
                     .xlsx automatically — it happens outside your context, so you will NOT see the \
                     file. This is expected, NOT a failure: do not try to read, re-upload, or verify \
                     it. Just tell the user the file is ready.",
        });
        Ok(ToolResult::success(output, elapsed(start)))
    }
}

/// The `export_xlsx` input schema — shared by the channel and edge export tools
/// so a single definition drives both.
pub fn export_xlsx_input_schema() -> Value {
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
            "layout": {
                "type": "object",
                "description": "Optional reshape. Fields: columns:[{field,header?}] to select/rename/reorder, group_by:[field], aggregate:[{field,fn:sum|count|avg|min|max}], sort:[{field,dir:asc|desc}], totals_row:bool. `field` names are the row-object keys."
            },
            "sheet_name": { "type": "string", "description": "Worksheet name (default Sheet1)." },
            "sheets": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "sheet_name": { "type": "string" },
                        "data": { "type": "array", "items": { "type": "object" } },
                        "artifact_id": { "type": "string" },
                        "layout": { "type": "object" }
                    }
                },
                "description": "Multiple worksheets in one workbook: each entry is {sheet_name?, data?|artifact_id?, layout?} (same fields as the single-sheet form). Use this INSTEAD of the top-level data/artifact_id to put several tables in one .xlsx."
            },
            "filename": { "type": "string", "description": "Output .xlsx filename (default export.xlsx)." }
        }
    })
}

/// Resolve the input's worksheets and render the `.xlsx` bytes — the shared,
/// task-local-free core both the channel `ExportXlsxTool` and the edge export
/// tool build on. Returns the bytes, the download filename, the total
/// source-row count, and the worksheet count.
pub fn build_xlsx_from_input(
    input: &Value,
    store: &dyn ArtifactStore,
    agent_id: &str,
) -> Result<(Vec<u8>, String, usize, usize)> {
    let (sheets, total_rows) = resolve_sheets(input, store, agent_id)?;
    let bytes = build_xlsx(&sheets).map_err(|e| eyre!("failed to build the spreadsheet: {e}"))?;
    let filename = xlsx_filename(input["filename"].as_str(), "export");
    Ok((bytes, filename, total_rows, sheets.len()))
}

/// Row objects ready to export, plus the column order their source artifact
/// declared (None for inline `data`, where object keys are all we have).
#[derive(Debug)]
pub(crate) struct ResolvedRows {
    pub(crate) rows: Vec<Value>,
    pub(crate) column_order: Option<Vec<String>>,
}

/// Resolve the row objects from inline `data` or an `artifact_id` (exactly one).
pub(crate) fn resolve_rows(
    input: &Value,
    store: &dyn ArtifactStore,
    agent_id: &str,
) -> Result<ResolvedRows> {
    let inline = input.get("data").filter(|v| !v.is_null());
    let artifact = input["artifact_id"].as_str().filter(|s| !s.is_empty());
    match (inline, artifact) {
        (Some(_), Some(_)) => Err(eyre!("specify `data` or `artifact_id`, not both")),
        (None, None) => Err(eyre!(
            "must specify `data` (an array of row objects) or `artifact_id`"
        )),
        (Some(data), None) => data
            .as_array()
            .cloned()
            .map(|rows| ResolvedRows {
                rows,
                column_order: None,
            })
            .ok_or_else(|| eyre!("`data` must be an array of row objects")),
        (None, Some(id)) => {
            let id = ArtifactId::parse(id)?;
            let table = resolve_table_artifact(store, agent_id, &id)?;
            Ok(ResolvedRows {
                rows: table.rows,
                column_order: table.declared_order.then_some(table.columns),
            })
        }
    }
}

/// Resolve one or more worksheets from the input: a `sheets` array (each entry
/// its own rows + layout + name) or the single-sheet top-level form. Returns the
/// sheets plus the total source-row count. Separated from `execute` so the
/// data-shaping path is testable without the task-locals.
fn resolve_sheets(
    input: &Value,
    store: &dyn ArtifactStore,
    agent_id: &str,
) -> Result<(Vec<Sheet>, usize)> {
    if let Some(specs) = input["sheets"].as_array().filter(|a| !a.is_empty()) {
        let mut sheets = Vec::with_capacity(specs.len());
        let mut total = 0;
        for (i, spec) in specs.iter().enumerate() {
            let (sheet, rows) = build_sheet(spec, store, agent_id, &format!("Sheet{}", i + 1))
                .map_err(|e| eyre!("sheet {}: {e}", i + 1))?;
            total += rows;
            sheets.push(sheet);
        }
        Ok((sheets, total))
    } else {
        let (sheet, rows) = build_sheet(input, store, agent_id, "Sheet1")?;
        Ok((vec![sheet], rows))
    }
}

/// One worksheet spec (`{sheet_name?, data?|artifact_id?, layout?}`) → a `Sheet`
/// plus its source-row count.
fn build_sheet(
    spec: &Value,
    store: &dyn ArtifactStore,
    agent_id: &str,
    default_name: &str,
) -> Result<(Sheet, usize)> {
    let resolved = resolve_rows(spec, store, agent_id)?;
    if resolved.rows.is_empty() {
        return Err(eyre!("no rows to export"));
    }
    let source_rows = resolved.rows.len();
    let name = spec["sheet_name"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(default_name);
    let layout = Layout::from_json(&spec["layout"]);
    let table = assemble_table(&resolved.rows, &layout, resolved.column_order.as_deref());
    Ok((Sheet::new(name, table), source_rows))
}

fn assemble_table(rows: &[Value], layout: &Layout, column_order: Option<&[String]>) -> Table {
    let table = table_from_objects(rows);
    let table = match column_order {
        Some(order) => reorder_columns(table, order),
        None => table,
    };
    apply_layout(&table, layout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::store::{FsArtifactStore, DEFAULT_TTL};
    use tempfile::TempDir;

    #[test]
    fn build_sheet_resolves_name_and_rows() {
        let dir = TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let spec = json!({ "sheet_name": "Staff", "data": [
            { "Name": "Ann", "Salary": 100 }, { "Name": "Bo", "Salary": 50 }
        ]});
        let (sheet, rows) = build_sheet(&spec, store.as_ref(), "a", "Sheet1").unwrap();
        assert_eq!(sheet.name, "Staff");
        assert_eq!(rows, 2);
        let bytes = build_xlsx(&[sheet]).unwrap();
        assert_eq!(&bytes[0..2], b"PK", "xlsx is a zip");
        assert!(bytes.len() > 500);
    }

    #[test]
    fn build_sheet_applies_layout_and_reports_source_rows() {
        let dir = TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let spec = json!({ "data": [ { "dept": "Eng", "n": 1 }, { "dept": "Eng", "n": 2 } ],
            "layout": { "group_by": ["dept"], "aggregate": [{ "field": "n", "fn": "sum" }] } });
        let (sheet, rows) = build_sheet(&spec, store.as_ref(), "a", "Sheet1").unwrap();
        assert_eq!(rows, 2, "row count is the pre-aggregation source count");
        assert_eq!(sheet.table.rows.len(), 1, "group_by collapsed to one row");
    }

    #[test]
    fn resolve_sheets_builds_a_multi_sheet_workbook() {
        let dir = TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let input = json!({ "sheets": [
            { "sheet_name": "A", "data": [ { "x": 1 }, { "x": 2 } ] },
            { "sheet_name": "B", "data": [ { "y": 9 } ] }
        ]});
        let (sheets, total) = resolve_sheets(&input, store.as_ref(), "a").unwrap();
        assert_eq!(sheets.len(), 2);
        assert_eq!(sheets[0].name, "A");
        assert_eq!(sheets[1].name, "B");
        assert_eq!(total, 3);
        let bytes = build_xlsx(&sheets).unwrap();
        assert_eq!(&bytes[0..2], b"PK");
    }

    #[test]
    fn resolve_sheets_defaults_to_single_sheet() {
        let dir = TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let input = json!({ "data": [ { "x": 1 } ] });
        let (sheets, total) = resolve_sheets(&input, store.as_ref(), "a").unwrap();
        assert_eq!(sheets.len(), 1);
        assert_eq!(sheets[0].name, "Sheet1");
        assert_eq!(total, 1);
    }

    #[test]
    fn resolve_sheets_empty_array_falls_back_and_sheets_win() {
        let dir = TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let (one, n) = resolve_sheets(
            &json!({ "sheets": [], "data": [ { "x": 1 } ] }),
            store.as_ref(),
            "a",
        )
        .unwrap();
        assert_eq!(one.len(), 1, "empty sheets falls back to top-level data");
        assert_eq!(n, 1);

        let (two, _) = resolve_sheets(
            &json!({
                "sheets": [ { "sheet_name": "S", "data": [ { "a": 1 }, { "a": 2 } ] } ],
                "data": [ { "ignored": 9 } ]
            }),
            store.as_ref(),
            "a",
        )
        .unwrap();
        assert_eq!(two.len(), 1);
        assert_eq!(two[0].name, "S");
        assert_eq!(two[0].table.rows.len(), 2, "sheets win over top-level data");
    }

    #[test]
    fn assemble_table_honors_artifact_column_order() {
        let rows = vec![json!({ "id": 1, "status": "Unfilled" })];
        let order = vec!["status".to_string(), "id".to_string()];
        let table = assemble_table(&rows, &Layout::default(), Some(&order));
        let keys: Vec<&str> = table.columns.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, ["status", "id"], "schema order beats alphabetical");
    }

    #[test]
    fn resolve_rows_one_of_validation() {
        let dir = TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();

        let both = json!({ "data": [], "artifact_id": "art_00000000000000000000000000000000" });
        assert!(resolve_rows(&both, store.as_ref(), "a")
            .unwrap_err()
            .to_string()
            .contains("not both"));

        let neither = json!({});
        assert!(resolve_rows(&neither, store.as_ref(), "a")
            .unwrap_err()
            .to_string()
            .contains("must specify"));

        let inline = json!({ "data": [ { "x": 1 } ] });
        let resolved = resolve_rows(&inline, store.as_ref(), "a").unwrap();
        assert_eq!(resolved.rows.len(), 1);
        assert!(resolved.column_order.is_none());
    }

    #[test]
    fn resolve_rows_reads_from_artifact() {
        let dir = TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let id = store
            .write(
                "agent-a",
                ArtifactDescriptor {
                    producer: "test".into(),
                    mime: ContentType::ApplicationJson,
                    schema: None,
                    row_count: Some(1),
                    ttl_seconds: DEFAULT_TTL.as_secs(),
                    agent_id: "agent-a".into(),
                    session_id: None,
                    user_id: None,
                    filename: None,
                    media_type: None,
                },
                ArtifactBody::Json(json!([{ "x": 1 }, { "x": 2 }])),
            )
            .unwrap()
            .id;
        let input = json!({ "artifact_id": id.as_str() });
        let resolved = resolve_rows(&input, store.as_ref(), "agent-a").unwrap();
        assert_eq!(resolved.rows.len(), 2);
    }

    fn write_array_rows_artifact(
        store: &FsArtifactStore,
        schema: Option<Vec<String>>,
    ) -> ArtifactId {
        store
            .write(
                "agent-a",
                ArtifactDescriptor {
                    producer: "metabase_query".into(),
                    mime: ContentType::ApplicationJson,
                    schema,
                    row_count: Some(2),
                    ttl_seconds: DEFAULT_TTL.as_secs(),
                    agent_id: "agent-a".into(),
                    session_id: None,
                    user_id: None,
                    filename: None,
                    media_type: None,
                },
                ArtifactBody::Json(json!([["Unfilled", 1], ["Filled", 2]])),
            )
            .unwrap()
            .id
    }

    #[test]
    fn resolve_rows_zips_array_rows_via_schema_preserving_order() {
        let dir = TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let id = write_array_rows_artifact(&store, Some(vec!["status".into(), "id".into()]));
        let input = json!({ "artifact_id": id.as_str() });
        let resolved = resolve_rows(&input, store.as_ref(), "agent-a").unwrap();
        assert_eq!(
            resolved.rows,
            vec![
                json!({"status": "Unfilled", "id": 1}),
                json!({"status": "Filled", "id": 2}),
            ]
        );
        assert_eq!(
            resolved.column_order,
            Some(vec!["status".to_string(), "id".to_string()])
        );
        let table = assemble_table(
            &resolved.rows,
            &Layout::default(),
            resolved.column_order.as_deref(),
        );
        let keys: Vec<&str> = table.columns.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, ["status", "id"]);
        let bytes = build_xlsx(&[Sheet::new("Sheet1", table)]).unwrap();
        assert_eq!(&bytes[0..2], b"PK");
    }

    #[test]
    fn resolve_rows_object_artifact_carries_schema_order() {
        let dir = TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let id = store
            .write(
                "agent-a",
                ArtifactDescriptor {
                    producer: "test".into(),
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
        let input = json!({ "artifact_id": id.as_str() });
        let resolved = resolve_rows(&input, store.as_ref(), "agent-a").unwrap();
        assert_eq!(
            resolved.column_order,
            Some(vec!["status".to_string(), "id".to_string()])
        );
    }

    #[test]
    fn resolve_rows_rejects_array_rows_without_schema() {
        let dir = TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let id = write_array_rows_artifact(&store, None);
        let input = json!({ "artifact_id": id.as_str() });
        let err = resolve_rows(&input, store.as_ref(), "agent-a")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no column schema"));
    }

    #[test]
    fn resolve_rows_rejects_mixed_row_shapes() {
        let dir = TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let id = store
            .write(
                "agent-a",
                ArtifactDescriptor {
                    producer: "test".into(),
                    mime: ContentType::ApplicationJson,
                    schema: None,
                    row_count: Some(2),
                    ttl_seconds: DEFAULT_TTL.as_secs(),
                    agent_id: "agent-a".into(),
                    session_id: None,
                    user_id: None,
                    filename: None,
                    media_type: None,
                },
                ArtifactBody::Json(json!([{ "x": 1 }, [2]])),
            )
            .unwrap()
            .id;
        let input = json!({ "artifact_id": id.as_str() });
        let err = resolve_rows(&input, store.as_ref(), "agent-a")
            .unwrap_err()
            .to_string();
        assert!(err.contains("all objects or all arrays"));
    }
}
