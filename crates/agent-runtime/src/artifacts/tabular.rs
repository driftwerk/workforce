//! Canonical tabular staging and shape conversion for the data plane.
//!
//! Interchange format: a JSON array of row objects, with column order carried
//! by `ArtifactDescriptor::schema` (serde_json maps serialise alphabetically,
//! so object key order cannot carry it). Producers stage via [`stage_table`];
//! consumers convert shapes via [`zip_table`] / [`rows_to_cells`]. Every
//! tabular shape decision lives in this module.

use std::collections::HashSet;
use std::time::Duration;

use eyre::{eyre, Result};
use serde_json::{json, Map, Value};

use super::store::{maybe_store, ArtifactStore, DEFAULT_TTL};
use super::types::{
    value_kind, ArtifactBody, ArtifactDescriptor, ArtifactId, ContentType, MaybeArtifact,
};

pub const TABULAR_ARTIFACT_KIND: &str = "tabular_artifact";

fn staged_note(ttl: Duration) -> String {
    let hours = (ttl.as_secs() / 3600).max(1);
    format!(
        "Full result staged out of context. Transform/filter/join with data.query (SQL over \
         staged tables), peek at shape/stats with data.inspect, export with \
         export_xlsx(artifact_id=...), or pass artifact_id to consumer tools. Never re-fetch or \
         page through the rows. Staged data expires after ~{hours}h - if a handle reports not \
         found, re-run the producing tool (or ask for the file again)."
    )
}

/// Outcome of staging: small tables stay inline, large ones become an
/// artifact plus a model-facing envelope.
#[derive(Debug)]
pub enum StagedTable {
    Inline {
        rows: Vec<Value>,
        columns: Vec<String>,
    },
    Staged {
        envelope: Value,
        artifact_id: ArtifactId,
        columns: Vec<String>,
    },
}

/// Trim names, replace empties with `col_{n}`, and de-duplicate with
/// `_2`/`_3` suffixes so the result is usable as unique object keys.
pub fn normalize_columns(raw: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    raw.into_iter()
        .enumerate()
        .map(|(i, name)| {
            let trimmed = name.trim();
            let base = if trimmed.is_empty() {
                format!("col_{}", i + 1)
            } else {
                trimmed.to_string()
            };
            let unique = std::iter::once(base.clone())
                .chain((2..).map(|n| format!("{base}_{n}")))
                .find(|candidate| !seen.contains(candidate))
                .expect("suffix sequence is unbounded");
            seen.insert(unique.clone());
            unique
        })
        .collect()
}

/// Array rows → canonical row objects in `columns` order. Short rows are
/// null-padded; cells beyond `columns` are dropped (the schema is
/// authoritative).
pub fn zip_table(columns: &[String], rows: Vec<Vec<Value>>) -> Vec<Value> {
    rows.into_iter()
        .map(|row| {
            let mut cells = row.into_iter();
            Value::Object(
                columns
                    .iter()
                    .map(|c| (c.clone(), cells.next().unwrap_or(Value::Null)))
                    .collect::<Map<_, _>>(),
            )
        })
        .collect()
}

/// Ordered union of object keys across rows (first-seen order).
pub fn derive_schema(rows: &[Value]) -> Vec<String> {
    let mut seen: HashSet<&str> = HashSet::new();
    rows.iter()
        .filter_map(Value::as_object)
        .flat_map(|obj| obj.keys())
        .filter(|key| seen.insert(key.as_str()))
        .cloned()
        .collect()
}

/// Canonical row objects → 2D cell rows in `schema` order. Missing keys
/// and non-object rows yield nulls.
pub fn rows_to_cells(rows: &[Value], schema: &[String]) -> Vec<Vec<Value>> {
    rows.iter()
        .map(|row| {
            schema
                .iter()
                .map(|c| row.get(c).cloned().unwrap_or(Value::Null))
                .collect()
        })
        .collect()
}

/// Stage canonical row objects: inline when the serialised payload fits
/// `inline_threshold`, otherwise write an artifact and return the
/// model-facing envelope. `columns` must already be unique, non-empty
/// keys (see [`normalize_columns`]); omitted columns derive from the rows.
pub fn stage_table(
    store: &dyn ArtifactStore,
    agent_id: &str,
    producer: &str,
    columns: Option<Vec<String>>,
    rows: Vec<Value>,
    inline_threshold: usize,
) -> Result<StagedTable> {
    stage_table_with_ttl(
        store,
        agent_id,
        producer,
        columns,
        rows,
        inline_threshold,
        current_artifact_ttl(),
    )
}

/// The TTL a staged table should carry: the turn's `CURRENT_ARTIFACT_TTL` if a
/// caller (the edge) scoped one so its browsable tables outlive the reasoning
/// window, else the short scratch default.
fn current_artifact_ttl() -> Duration {
    crate::runner::CURRENT_ARTIFACT_TTL
        .try_with(|ttl| *ttl)
        .unwrap_or(DEFAULT_TTL)
}

/// [`stage_table`] with an explicit body TTL. Upload/attachment producers pass
/// [`super::store::ATTACHMENT_TTL`].
#[allow(clippy::too_many_arguments)]
pub fn stage_table_with_ttl(
    store: &dyn ArtifactStore,
    agent_id: &str,
    producer: &str,
    columns: Option<Vec<String>>,
    rows: Vec<Value>,
    inline_threshold: usize,
    ttl: Duration,
) -> Result<StagedTable> {
    let columns = prepared_columns(columns, &rows)?;
    if rows.is_empty() {
        return Ok(StagedTable::Inline { rows, columns });
    }
    let row_count = rows.len();
    let descriptor = ArtifactDescriptor {
        schema: Some(columns.clone()),
        row_count: Some(row_count),
        ..ArtifactDescriptor::from_context(producer, ContentType::ApplicationJson, agent_id)
            .with_ttl(ttl)
    };
    match maybe_store(
        store,
        agent_id,
        descriptor,
        ArtifactBody::Json(Value::Array(rows)),
        inline_threshold,
    )? {
        MaybeArtifact::Inline(ArtifactBody::Json(Value::Array(rows))) => {
            Ok(StagedTable::Inline { rows, columns })
        }
        MaybeArtifact::Inline(_) => Err(eyre!("inline body changed shape during staging")),
        MaybeArtifact::Stored(handle) => {
            let envelope = json!({
                "kind": TABULAR_ARTIFACT_KIND,
                "artifact_id": handle.id.to_string(),
                "rows": row_count,
                "columns": columns,
                "preview": handle.preview.get("rows").cloned().unwrap_or_else(|| json!([])),
                "preview_truncated": handle.preview.get("truncated").and_then(Value::as_bool).unwrap_or(false),
                "ttl_seconds": handle.descriptor.ttl_seconds,
                "note": staged_note(ttl),
            });
            Ok(StagedTable::Staged {
                envelope,
                artifact_id: handle.id,
                columns,
            })
        }
    }
}

/// Re-render a staged envelope's `preview` rows as an injection-wrapped
/// string. Producers whose cell values cross the trust boundary (query
/// results, external APIs) call this before the envelope reaches the model.
pub fn wrap_envelope_preview(envelope: &mut Value, source: &str) {
    if let Some(preview) = envelope.get("preview") {
        let wrapped =
            crate::security::external_content::wrap_external_content(&preview.to_string(), source)
                .wrapped_content;
        envelope["preview"] = Value::String(wrapped);
    }
}

/// The standard model-facing rendering of a staging outcome for external
/// data: inline rows render as one injection-wrapped string, staged
/// envelopes get their preview wrapped. Shared by every producer that
/// stages query/API results (data.query, xero.export, datadog exports).
pub fn staged_to_output(staged: StagedTable, source: &str) -> Value {
    match staged {
        StagedTable::Inline { rows, columns } => {
            let rendered = serde_json::to_string(&rows).unwrap_or_default();
            let wrapped =
                crate::security::external_content::wrap_external_content(&rendered, source)
                    .wrapped_content;
            json!({
                "kind": "rows",
                "columns": columns,
                "row_count": rows.len(),
                "rows_wrapped": wrapped,
            })
        }
        StagedTable::Staged { mut envelope, .. } => {
            wrap_envelope_preview(&mut envelope, source);
            envelope
        }
    }
}

fn prepared_columns(columns: Option<Vec<String>>, rows: &[Value]) -> Result<Vec<String>> {
    if let Some((i, row)) = rows.iter().enumerate().find(|(_, r)| !r.is_object()) {
        return Err(eyre!(
            "row {i} is not an object ({}); zip_table array rows before staging",
            value_kind(row)
        ));
    }
    match columns {
        Some(cols) => {
            let unique: HashSet<&str> = cols.iter().map(String::as_str).collect();
            if cols.is_empty()
                || unique.len() != cols.len()
                || cols.iter().any(|c| c.trim().is_empty())
            {
                return Err(eyre!(
                    "columns must be non-empty and unique; pass them through normalize_columns"
                ));
            }
            Ok(cols)
        }
        None => Ok(derive_schema(rows)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::store::{FsArtifactStore, DEFAULT_TTL};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn fresh_store() -> (Arc<FsArtifactStore>, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        (store, dir)
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn normalize_handles_trim_empty_and_duplicates() {
        let cols = normalize_columns(strings(&[" a ", "", "a", "a", "  "]));
        assert_eq!(cols, strings(&["a", "col_2", "a_2", "a_3", "col_5"]));
    }

    #[test]
    fn normalize_avoids_collision_with_existing_suffix() {
        let cols = normalize_columns(strings(&["a", "a_2", "a"]));
        assert_eq!(cols, strings(&["a", "a_2", "a_3"]));
    }

    #[test]
    fn zip_pads_short_rows_and_drops_extra_cells() {
        let cols = strings(&["a", "b"]);
        let rows = vec![vec![json!(1)], vec![json!(2), json!(3), json!("dropped")]];
        let objects = zip_table(&cols, rows);
        assert_eq!(objects[0], json!({"a": 1, "b": null}));
        assert_eq!(objects[1], json!({"a": 2, "b": 3}));
    }

    #[test]
    fn derive_schema_unions_in_first_seen_order() {
        let rows = vec![json!({"b": 1, "a": 2}), json!({"c": 3, "a": 4})];
        assert_eq!(derive_schema(&rows), strings(&["a", "b", "c"]));
    }

    #[test]
    fn rows_to_cells_fills_missing_with_null_in_schema_order() {
        let rows = vec![json!({"a": 1}), json!({"b": 2, "a": 3})];
        let cells = rows_to_cells(&rows, &strings(&["b", "a"]));
        assert_eq!(
            cells,
            vec![vec![json!(null), json!(1)], vec![json!(2), json!(3)]]
        );
    }

    #[test]
    fn staged_table_ttl_defaults_to_scratch_and_follows_the_turn_scope() {
        let (store, _g) = fresh_store();
        let rows: Vec<Value> = (0..200).map(|i| json!({ "id": i })).collect();
        let ttl_of = |staged: StagedTable| match staged {
            StagedTable::Staged { envelope, .. } => envelope["ttl_seconds"].as_u64().unwrap(),
            other => panic!("expected staged, got {other:?}"),
        };
        let default = stage_table(&*store, "agent-a", "t", None, rows.clone(), 64).unwrap();
        assert_eq!(ttl_of(default), DEFAULT_TTL.as_secs());
        let scoped = crate::runner::CURRENT_ARTIFACT_TTL
            .sync_scope(Duration::from_secs(86_400), || {
                stage_table(&*store, "agent-a", "t", None, rows, 64).unwrap()
            });
        assert_eq!(ttl_of(scoped), 86_400);
    }

    #[test]
    fn stage_table_inlines_under_threshold() {
        let (store, _g) = fresh_store();
        let rows = vec![json!({"a": 1}), json!({"a": 2})];
        match stage_table(&*store, "agent-a", "test.rows", None, rows.clone(), 10_000).unwrap() {
            StagedTable::Inline { rows: out, columns } => {
                assert_eq!(out, rows);
                assert_eq!(columns, strings(&["a"]));
            }
            other => panic!("expected inline, got {other:?}"),
        }
    }

    #[test]
    fn stage_table_stages_over_threshold_and_roundtrips() {
        let (store, _g) = fresh_store();
        let rows: Vec<Value> = (0..200)
            .map(|i| json!({"id": i, "name": format!("name-{i}")}))
            .collect();
        let staged = stage_table(&*store, "agent-a", "test.rows", None, rows.clone(), 64).unwrap();
        let StagedTable::Staged {
            envelope,
            artifact_id,
            columns,
        } = staged
        else {
            panic!("expected staged");
        };
        assert_eq!(columns, strings(&["id", "name"]));
        assert_eq!(envelope["kind"], TABULAR_ARTIFACT_KIND);
        assert_eq!(envelope["rows"], 200);
        assert_eq!(envelope["columns"], json!(["id", "name"]));
        assert_eq!(envelope["preview"].as_array().unwrap().len(), 5);
        assert_eq!(envelope["preview_truncated"], true);
        assert_eq!(envelope["ttl_seconds"], DEFAULT_TTL.as_secs());
        assert!(envelope["note"].as_str().unwrap().contains("export_xlsx"));
        assert!(envelope["artifact_id"]
            .as_str()
            .unwrap()
            .starts_with("art_"));
        match store.read("agent-a", &artifact_id).unwrap() {
            ArtifactBody::Json(Value::Array(stored)) => assert_eq!(stored, rows),
            other => panic!("expected json rows, got {other:?}"),
        }
    }

    #[test]
    fn stage_table_records_schema_and_row_count_in_descriptor() {
        let (store, _g) = fresh_store();
        let rows: Vec<Value> = (0..50).map(|i| json!({"x": i})).collect();
        let StagedTable::Staged { artifact_id, .. } =
            stage_table(&*store, "agent-a", "test.rows", None, rows, 16).unwrap()
        else {
            panic!("expected staged");
        };
        let descriptor = store.metadata("agent-a", &artifact_id).unwrap().unwrap();
        assert_eq!(descriptor.schema, Some(strings(&["x"])));
        assert_eq!(descriptor.row_count, Some(50));
        assert_eq!(descriptor.producer, "test.rows");
    }

    #[test]
    fn stage_table_rejects_non_object_rows() {
        let (store, _g) = fresh_store();
        let rows = vec![json!({"a": 1}), json!([1, 2])];
        let err = stage_table(&*store, "agent-a", "test.rows", None, rows, 16)
            .unwrap_err()
            .to_string();
        assert!(err.contains("row 1"));
        assert!(err.contains("array"));
    }

    #[test]
    fn stage_table_rejects_unnormalized_columns() {
        let (store, _g) = fresh_store();
        let rows = vec![json!({"a": 1})];
        for cols in [strings(&["a", "a"]), strings(&[""]), Vec::new()] {
            let err = stage_table(
                &*store,
                "agent-a",
                "test.rows",
                Some(cols),
                rows.clone(),
                16,
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("normalize_columns"));
        }
    }

    #[test]
    fn stage_table_empty_rows_stay_inline() {
        let (store, _g) = fresh_store();
        match stage_table(
            &*store,
            "agent-a",
            "test.rows",
            Some(strings(&["a"])),
            Vec::new(),
            0,
        )
        .unwrap()
        {
            StagedTable::Inline { rows, columns } => {
                assert!(rows.is_empty());
                assert_eq!(columns, strings(&["a"]));
            }
            other => panic!("expected inline, got {other:?}"),
        }
    }
}
