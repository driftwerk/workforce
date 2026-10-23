//! Opaque non-tabular blob staging for the data plane.
//!
//! The tabular path ([`super::tabular::stage_table`]) stages array-of-objects
//! results so `data.query` can transform them. Everything else over the spill
//! threshold — single objects, nested aggregations, large free text — stages
//! here as an opaque JSON body and reaches the model as a bounded digest
//! carrying a `tool.retrieve` handle. The full body never enters context.

use eyre::Result;
use serde_json::{json, Value};
use workforce_common::text::truncate_str;

use super::store::ArtifactStore;
use super::types::{value_kind, ArtifactBody, ArtifactDescriptor, ContentType};

pub const CLIPPED_ARTIFACT_KIND: &str = "clipped_artifact";

/// Bytes of the rendered body kept inline as a preview head. The rest lives in
/// the store; the agent recovers it with `tool.retrieve`.
const PREVIEW_MAX_BYTES: usize = 2_048;

/// Flatten a stored value to text for the preview and for `tool.retrieve`. A
/// `String` renders as its inner text; everything else as pretty (multi-line)
/// JSON so a line-based query can narrow it and the preview stays readable.
pub fn value_to_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

/// Stage an oversized non-tabular tool output as an opaque JSON artifact and
/// return the model-facing digest envelope. Callers stage only past the spill
/// threshold, so this always writes; `artifact_id` sorts first in the
/// serialised envelope so a later prune-clip preserves the handle.
pub fn stage_blob(
    store: &dyn ArtifactStore,
    agent_id: &str,
    producer: &str,
    value: Value,
) -> Result<Value> {
    let shape = value_kind(&value);
    let rendered = value_to_text(&value);
    let preview_truncated = rendered.len() > PREVIEW_MAX_BYTES;
    let preview = crate::security::external_content::wrap_external_content(
        truncate_str(&rendered, PREVIEW_MAX_BYTES),
        producer,
    )
    .wrapped_content;
    // The in-context size being staged out: the compact serialised form the
    // spill backstop measured, so the envelope and the trace agree.
    let bytes = serde_json::to_string(&value).map_or(rendered.len(), |s| s.len());

    let descriptor =
        ArtifactDescriptor::from_context(producer, ContentType::ApplicationJson, agent_id);
    let handle = store.write(agent_id, descriptor, ArtifactBody::Json(value))?;
    let id = handle.id.to_string();
    // The tool name is interpolated, not spelled: a blob is only ever staged
    // where this tool is registered, so the note must never instruct the model
    // to call something that has been renamed out from under it.
    let retrieve = crate::spill::RETRIEVE_TOOL_NAME;
    let note = format!(
        "Full output staged out of context ({bytes} bytes, {shape}). The preview is the head; \
         recover the rest with {retrieve}(handle=\"{id}\", query=\"…\") — pass a query to \
         keep only matching lines. Do not re-run the tool. Staged data expires after ~1 hour."
    );
    Ok(json!({
        "kind": CLIPPED_ARTIFACT_KIND,
        "artifact_id": id,
        "bytes": bytes,
        "shape": shape,
        "preview": preview,
        "preview_truncated": preview_truncated,
        "ttl_seconds": handle.descriptor.ttl_seconds,
        "note": note,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::store::FsArtifactStore;
    use crate::artifacts::types::ArtifactId;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn fresh_store() -> (Arc<FsArtifactStore>, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        (store, dir)
    }

    #[test]
    fn value_to_text_renders_string_inner_and_others_pretty() {
        assert_eq!(value_to_text(&json!("plain text")), "plain text");
        assert_eq!(value_to_text(&json!(123)), "123");
        let pretty = value_to_text(&json!({ "a": 1 }));
        assert!(pretty.contains('\n'), "objects render multi-line: {pretty}");
        assert!(pretty.contains("\"a\": 1"));
    }

    #[test]
    fn stage_blob_object_roundtrips_and_describes() {
        let (store, _g) = fresh_store();
        let value = json!({ "data": { "rows": (0..2000).collect::<Vec<_>>() } });
        let envelope =
            stage_blob(&*store, "agent-a", "datadog.logs_aggregate", value.clone()).expect("stage");

        assert_eq!(envelope["kind"], CLIPPED_ARTIFACT_KIND);
        assert_eq!(envelope["shape"], "object");
        assert_eq!(envelope["preview_truncated"], true);
        assert!(envelope["bytes"].as_u64().unwrap() > PREVIEW_MAX_BYTES as u64);
        assert!(envelope["note"].as_str().unwrap().contains("tool.retrieve"));
        let id = ArtifactId::parse(envelope["artifact_id"].as_str().unwrap()).unwrap();
        match store.read("agent-a", &id).unwrap() {
            ArtifactBody::Json(stored) => assert_eq!(stored, value),
            other => panic!("expected json blob, got {other:?}"),
        }
    }

    #[test]
    fn stage_blob_string_preview_is_inner_text() {
        let (store, _g) = fresh_store();
        let text = format!("error trace head\n{}", "frame line\n".repeat(2000));
        let envelope = stage_blob(&*store, "agent-a", "datadog.api", json!(text)).expect("stage");

        assert_eq!(envelope["shape"], "string");
        let preview = envelope["preview"].as_str().unwrap();
        assert!(
            preview.contains("error trace head"),
            "preview head: {preview}"
        );
        assert!(
            !preview.contains("\\n"),
            "string body is inner text, not escaped json: {preview}"
        );
        let id = ArtifactId::parse(envelope["artifact_id"].as_str().unwrap()).unwrap();
        match store.read("agent-a", &id).unwrap() {
            ArtifactBody::Json(Value::String(stored)) => assert_eq!(stored, text),
            other => panic!("expected json string blob, got {other:?}"),
        }
    }

    #[test]
    fn stage_blob_small_value_is_not_marked_truncated() {
        let (store, _g) = fresh_store();
        let envelope = stage_blob(&*store, "agent-a", "t", json!({ "k": "small" })).expect("stage");
        assert_eq!(envelope["preview_truncated"], false);
    }
}
