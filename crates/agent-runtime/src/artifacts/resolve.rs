//! Consumer-side resolvers shared by every tool that may accept either
//! inline payload OR `artifact_id`. The `one-of` JSON-schema lives in
//! each tool's input definition; this module is the single source of
//! truth for the runtime validation and the load-from-store logic.

use eyre::{eyre, Result};
use serde_json::Value;

use super::store::ArtifactStore;
use super::tabular::{derive_schema, rows_to_cells, zip_table};
use super::types::{value_kind, ArtifactBody, ArtifactDescriptor, ArtifactId, ContentType};

/// Resolved 2D row payload, ready for the consumer to push to the API.
pub type Rows = Vec<Vec<Value>>;

/// Inputs for `resolve_rows_input`. Tools build this from their incoming
/// JSON arguments; the resolver enforces the `one-of` rule and any
/// optional slicing.
#[derive(Debug, Default)]
pub struct RowsRequest<'a> {
    pub values: Option<Value>,
    pub artifact_id: Option<&'a str>,
    pub row_range: Option<&'a str>,
}

/// Bytes inputs for `resolve_bytes_input`.
#[derive(Debug, Default)]
pub struct BytesRequest<'a> {
    pub data: Option<&'a [u8]>,
    pub data_base64: Option<&'a str>,
    pub artifact_id: Option<&'a str>,
}

/// Resolved bytes payload + mime, ready for the consumer.
#[derive(Debug)]
pub struct ResolvedBytes {
    pub bytes: Vec<u8>,
    pub mime: ContentType,
}

/// Metadata-first read that enforces same-session AND same-user access for
/// scoped artifacts. Every model-driven reader-by-id resolves through here; an
/// artifact stamped with another session's or another user's id answers with
/// the same wording as a missing one so ids don't leak existence. Legacy
/// descriptors (no `session_id`/`user_id`) and contexts without an ambient
/// session/user (CLI, server-side delivery) are unrestricted. The user check is
/// what isolates a SHARED agent's group channel, where many users share one
/// session so the session check alone wouldn't separate them.
/// The scope reads are checked against: a workflow instance when one is active,
/// otherwise the ambient session. Mirrors the producer side in
/// `ArtifactDescriptor::from_context`, so a stamp and a read never disagree.
fn ambient_artifact_scope() -> Option<String> {
    [
        &crate::runner::CURRENT_ARTIFACT_SCOPE,
        &crate::runner::CURRENT_SESSION_ID,
    ]
    .into_iter()
    .find_map(|local| local.try_with(|v| v.clone()).ok().filter(|v| !v.is_empty()))
}

pub fn read_session_scoped(
    store: &dyn ArtifactStore,
    agent_id: &str,
    id: &ArtifactId,
) -> Result<(ArtifactBody, Option<ArtifactDescriptor>)> {
    let descriptor = store.metadata(agent_id, id)?;
    if let Some(owner) = descriptor.as_ref().and_then(|d| d.session_id.as_deref()) {
        let ambient = ambient_artifact_scope();
        if ambient.is_some_and(|session| session != owner) {
            return Err(eyre!("artifact {id} not found or expired"));
        }
    }
    if let Some(owner) = descriptor.as_ref().and_then(|d| d.user_id.as_deref()) {
        let ambient = crate::runner::CURRENT_USER_ID
            .try_with(|u| u.clone())
            .ok()
            .filter(|u| !u.is_empty());
        if ambient.is_some_and(|user| user != owner) {
            return Err(eyre!("artifact {id} not found or expired"));
        }
    }
    Ok((store.read(agent_id, id)?, descriptor))
}

/// Cap on each object key quoted in a shape description.
const MAX_KEY_CHARS: usize = 40;

/// Human-readable shape of an artifact body, for error messages that say what
/// is actually wrong rather than restating the mime type.
pub(crate) fn describe_shape(body: &ArtifactBody) -> String {
    match body {
        ArtifactBody::Json(Value::Object(map)) => {
            // Keys can be data (`{"someone@example.com": …}`) and a single key can
            // be enormous, so bound BOTH the count and each key's length: this
            // string reaches model context and the logs.
            let mut keys: Vec<String> = map
                .keys()
                .take(4)
                .map(|k| match k.char_indices().nth(MAX_KEY_CHARS) {
                    Some((cut, _)) => format!("{}…", &k[..cut]),
                    None => k.clone(),
                })
                .collect();
            if map.len() > keys.len() {
                keys.push("…".to_string());
            }
            format!("a JSON object (keys: {})", keys.join(", "))
        }
        ArtifactBody::Json(Value::Null) => "JSON null".to_string(),
        ArtifactBody::Json(_) => "a JSON scalar".to_string(),
        other => format!("{} content", other.content_type()),
    }
}

/// A tabular artifact resolved to canonical row objects plus its column
/// order. `declared_order: false` means the columns were derived from the
/// rows (alphabetical union) rather than recorded by the producer.
#[derive(Debug)]
pub struct ResolvedTable {
    pub rows: Vec<Value>,
    pub columns: Vec<String>,
    pub declared_order: bool,
}

/// Resolve a staged tabular artifact (session-enforced): canonical object
/// rows pass through; legacy array rows zip via the descriptor's schema.
pub fn resolve_table_artifact(
    store: &dyn ArtifactStore,
    agent_id: &str,
    id: &ArtifactId,
) -> Result<ResolvedTable> {
    let (body, descriptor) = read_session_scoped(store, agent_id, id)?;
    let shape = describe_shape(&body);
    let ArtifactBody::Json(Value::Array(rows)) = body else {
        // The old wording quoted the mime twice ("mime application/json,
        // expected application/json"), which read as a content-type problem and
        // sent readers hunting in the wrong place — the complaint is the SHAPE.
        // Deliberately prescribes NO retry: staging is deterministic, so running
        // the same tool again reproduces the same artifact. Telling the model to
        // re-run is what turns one failed read into a round-burning loop — the
        // exact behaviour this artifact plumbing exists to avoid. Nor does it
        // name a specific fallback tool: which one exists depends on the surface
        // (the edge registers no blob-retrieval tool at all).
        return Err(eyre!(
            "artifact {id} holds {shape}, not a table: `data.query` and \
             `data.inspect` read a top-level JSON array of row objects. Do not \
             re-run the tool — the result would stage identically. Work from the \
             preview already in the conversation, or narrow the original request \
             so it returns rows."
        ));
    };
    let schema = descriptor.and_then(|d| d.schema);
    if rows.iter().all(Value::is_object) {
        return Ok(match schema {
            Some(columns) => ResolvedTable {
                rows,
                columns,
                declared_order: true,
            },
            None => {
                let columns = derive_schema(&rows);
                ResolvedTable {
                    rows,
                    columns,
                    declared_order: false,
                }
            }
        });
    }
    if rows.iter().all(Value::is_array) {
        let columns = schema.ok_or_else(|| {
            eyre!(
                "artifact {id} holds array rows but no column schema; \
                 re-stage it as row objects or with a schema"
            )
        })?;
        let cells = rows
            .into_iter()
            .filter_map(|row| match row {
                Value::Array(cells) => Some(cells),
                _ => None,
            })
            .collect();
        return Ok(ResolvedTable {
            rows: zip_table(&columns, cells),
            columns,
            declared_order: true,
        });
    }
    Err(eyre!(
        "artifact {id} rows must be all objects or all arrays"
    ))
}

/// Inline rows, an artifact_id, or neither — never both. Optional
/// `row_range` like `"100-200"` slices the artifact rows.
pub fn resolve_rows_input(
    store: &dyn ArtifactStore,
    agent_id: &str,
    req: RowsRequest<'_>,
) -> Result<Rows> {
    match (req.values, req.artifact_id, req.row_range) {
        (Some(_), Some(_), _) => Err(both_specified_error("values")),
        (None, None, _) => Err(eyre!("must specify either values or artifact_id")),
        (Some(values), None, range) => {
            if range.is_some() {
                return Err(eyre!("row_range is only valid with artifact_id"));
            }
            json_to_rows(values)
        }
        (None, Some(id_str), range) => {
            let id = ArtifactId::parse(id_str)?;
            let (body, descriptor) = read_session_scoped(store, agent_id, &id)?;
            let mime = body.content_type();
            let ArtifactBody::Json(value) = body else {
                return Err(eyre!(
                    "artifact {id} has mime {mime}, expected application/json"
                ));
            };
            let rows = match value {
                Value::Array(arr) if !arr.is_empty() && arr.iter().all(Value::is_object) => {
                    let schema = descriptor
                        .and_then(|d| d.schema)
                        .unwrap_or_else(|| derive_schema(&arr));
                    rows_to_cells(&arr, &schema)
                }
                other => json_to_rows(other)?,
            };
            apply_row_range(rows, range)
        }
    }
}

/// Inline bytes (raw or base64), or an artifact_id pointing at any
/// blob. Returns the bytes + the content type so the consumer can
/// forward an accurate header.
pub fn resolve_bytes_input(
    store: &dyn ArtifactStore,
    agent_id: &str,
    req: BytesRequest<'_>,
) -> Result<ResolvedBytes> {
    let count = [
        req.data.is_some(),
        req.data_base64.is_some(),
        req.artifact_id.is_some(),
    ]
    .iter()
    .filter(|x| **x)
    .count();
    if count == 0 {
        return Err(eyre!(
            "must specify one of data, data_base64, or artifact_id"
        ));
    }
    if count > 1 {
        return Err(eyre!(
            "specify only one of data, data_base64, or artifact_id"
        ));
    }
    if let Some(bytes) = req.data {
        return Ok(ResolvedBytes {
            bytes: bytes.to_vec(),
            mime: ContentType::OctetStream,
        });
    }
    if let Some(b64) = req.data_base64 {
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| eyre!("invalid base64 data: {e}"))?;
        return Ok(ResolvedBytes {
            bytes: decoded,
            mime: ContentType::OctetStream,
        });
    }
    let id_str = req.artifact_id.expect("count enforced above");
    let id = ArtifactId::parse(id_str)?;
    let (body, _) = read_session_scoped(store, agent_id, &id)?;
    Ok(match body {
        ArtifactBody::Bytes(b) => ResolvedBytes {
            bytes: b,
            mime: ContentType::OctetStream,
        },
        ArtifactBody::Csv(s) => ResolvedBytes {
            bytes: s.into_bytes(),
            mime: ContentType::TextCsv,
        },
        ArtifactBody::Json(v) => {
            let bytes = serde_json::to_vec(&v)?;
            ResolvedBytes {
                bytes,
                mime: ContentType::ApplicationJson,
            }
        }
    })
}

fn both_specified_error(field: &str) -> eyre::Report {
    eyre!("specify {field} or artifact_id, not both")
}

fn json_to_rows(value: Value) -> Result<Rows> {
    let arr = value
        .as_array()
        .ok_or_else(|| eyre!("expected an array of rows, got {}", value_kind(&value)))?;
    arr.iter()
        .enumerate()
        .map(|(i, row)| {
            row.as_array()
                .cloned()
                .ok_or_else(|| eyre!("row {i} is not an array"))
        })
        .collect()
}

fn apply_row_range(rows: Rows, range: Option<&str>) -> Result<Rows> {
    let Some(range) = range else {
        return Ok(rows);
    };
    let (start, end) = parse_range(range, rows.len())?;
    Ok(rows[start..end].to_vec())
}

fn parse_range(range: &str, total: usize) -> Result<(usize, usize)> {
    let (a, b) = range
        .split_once('-')
        .ok_or_else(|| eyre!("row_range must be `start-end`, got `{range}`"))?;
    let start: usize = a
        .trim()
        .parse()
        .map_err(|_| eyre!("row_range start `{a}` is not a non-negative integer"))?;
    let end: usize = b
        .trim()
        .parse()
        .map_err(|_| eyre!("row_range end `{b}` is not a non-negative integer"))?;
    if start > end {
        return Err(eyre!("row_range start {start} exceeds end {end}"));
    }
    if end > total {
        return Err(eyre!(
            "row_range {start}-{end} exceeds artifact row_count of {total}"
        ));
    }
    Ok((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::store::{FsArtifactStore, DEFAULT_TTL};
    use crate::artifacts::types::ArtifactDescriptor;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::TempDir;

    fn fresh_store() -> (Arc<FsArtifactStore>, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        (store, dir)
    }

    fn rows_descriptor(agent: &str, ttl: Duration) -> ArtifactDescriptor {
        ArtifactDescriptor {
            producer: "test.rows".into(),
            mime: ContentType::ApplicationJson,
            schema: Some(vec!["a".into(), "b".into()]),
            row_count: Some(2),
            ttl_seconds: ttl.as_secs(),
            agent_id: agent.into(),
            session_id: None,
            user_id: None,
            filename: None,
            media_type: None,
        }
    }

    fn write_rows(
        store: &Arc<FsArtifactStore>,
        agent: &str,
        rows: serde_json::Value,
    ) -> ArtifactId {
        let body = ArtifactBody::Json(rows);
        store
            .write(agent, rows_descriptor(agent, DEFAULT_TTL), body)
            .unwrap()
            .id
    }

    #[test]
    fn resolve_rows_inline_passthrough() {
        let (store, _g) = fresh_store();
        let req = RowsRequest {
            values: Some(serde_json::json!([[1, 2], [3, 4]])),
            artifact_id: None,
            row_range: None,
        };
        let rows = resolve_rows_input(&*store, "agent-a", req).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], serde_json::json!(1));
    }

    #[test]
    fn resolve_rows_via_artifact_loads_from_store() {
        let (store, _g) = fresh_store();
        let id = write_rows(&store, "agent-a", serde_json::json!([[1, 2], [3, 4]]));
        let req = RowsRequest {
            values: None,
            artifact_id: Some(id.as_str()),
            row_range: None,
        };
        let rows = resolve_rows_input(&*store, "agent-a", req).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1][1], serde_json::json!(4));
    }

    #[test]
    fn resolve_rows_with_row_range_slices() {
        let (store, _g) = fresh_store();
        let id = write_rows(
            &store,
            "agent-a",
            serde_json::json!([[0], [1], [2], [3], [4]]),
        );
        let req = RowsRequest {
            values: None,
            artifact_id: Some(id.as_str()),
            row_range: Some("1-4"),
        };
        let rows = resolve_rows_input(&*store, "agent-a", req).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][0], serde_json::json!(1));
        assert_eq!(rows[2][0], serde_json::json!(3));
    }

    #[test]
    fn session_scoping_allows_same_denies_cross_and_passes_legacy() {
        let (store, _g) = fresh_store();
        let scoped = ArtifactDescriptor {
            session_id: Some("session-a".into()),
            ..rows_descriptor("agent-a", DEFAULT_TTL)
        };
        let id = store
            .write(
                "agent-a",
                scoped,
                ArtifactBody::Json(serde_json::json!([[1]])),
            )
            .unwrap()
            .id;

        let same_session = crate::runner::CURRENT_SESSION_ID
            .sync_scope("session-a".to_string(), || {
                read_session_scoped(&*store, "agent-a", &id).is_ok()
            });
        assert!(same_session, "owning session must read its artifact");

        // The discriminating case: a second session on the SAME shared agent.
        let cross_session =
            crate::runner::CURRENT_SESSION_ID.sync_scope("session-b".to_string(), || {
                read_session_scoped(&*store, "agent-a", &id)
                    .unwrap_err()
                    .to_string()
            });
        assert!(
            cross_session.contains("not found or expired"),
            "cross-session read must deny with not-found wording, got: {cross_session}"
        );

        assert!(
            read_session_scoped(&*store, "agent-a", &id).is_ok(),
            "no ambient session (server-side) must pass"
        );

        let legacy = store
            .write(
                "agent-a",
                rows_descriptor("agent-a", DEFAULT_TTL),
                ArtifactBody::Json(serde_json::json!([[1]])),
            )
            .unwrap()
            .id;
        let legacy_ok = crate::runner::CURRENT_SESSION_ID
            .sync_scope("session-b".to_string(), || {
                read_session_scoped(&*store, "agent-a", &legacy).is_ok()
            });
        assert!(legacy_ok, "legacy unscoped artifacts stay readable");
    }

    #[test]
    fn user_scoping_allows_owner_denies_other_and_passes_legacy() {
        let (store, _g) = fresh_store();
        let owned = ArtifactDescriptor {
            user_id: Some("user-a".into()),
            ..rows_descriptor("agent-a", DEFAULT_TTL)
        };
        let id = store
            .write(
                "agent-a",
                owned,
                ArtifactBody::Json(serde_json::json!([[1]])),
            )
            .unwrap()
            .id;

        let owner_reads = crate::runner::CURRENT_USER_ID.sync_scope("user-a".to_string(), || {
            read_session_scoped(&*store, "agent-a", &id).is_ok()
        });
        assert!(owner_reads, "the owning user must read its artifact");

        // The discriminating case: a DIFFERENT user on the SAME shared agent —
        // a shared group session wouldn't separate them, the user check does.
        let other = crate::runner::CURRENT_USER_ID.sync_scope("user-b".to_string(), || {
            read_session_scoped(&*store, "agent-a", &id)
                .unwrap_err()
                .to_string()
        });
        assert!(
            other.contains("not found or expired"),
            "cross-user read must deny with not-found wording, got: {other}"
        );

        assert!(
            read_session_scoped(&*store, "agent-a", &id).is_ok(),
            "no ambient user (server-side delivery) must pass"
        );

        // Owner-less (legacy / system) artifact stays readable by anyone.
        let legacy = store
            .write(
                "agent-a",
                rows_descriptor("agent-a", DEFAULT_TTL),
                ArtifactBody::Json(serde_json::json!([[1]])),
            )
            .unwrap()
            .id;
        let legacy_ok = crate::runner::CURRENT_USER_ID.sync_scope("user-b".to_string(), || {
            read_session_scoped(&*store, "agent-a", &legacy).is_ok()
        });
        assert!(legacy_ok, "legacy owner-less artifacts stay readable");
    }

    #[test]
    fn resolve_rows_input_enforces_session_scope() {
        let (store, _g) = fresh_store();
        let scoped = ArtifactDescriptor {
            session_id: Some("session-a".into()),
            ..rows_descriptor("agent-a", DEFAULT_TTL)
        };
        let id = store
            .write(
                "agent-a",
                scoped,
                ArtifactBody::Json(serde_json::json!([[1, 2]])),
            )
            .unwrap()
            .id;
        let err = crate::runner::CURRENT_SESSION_ID.sync_scope("session-b".to_string(), || {
            resolve_rows_input(
                &*store,
                "agent-a",
                RowsRequest {
                    values: None,
                    artifact_id: Some(id.as_str()),
                    row_range: None,
                },
            )
            .unwrap_err()
            .to_string()
        });
        assert!(err.contains("not found or expired"));
    }

    #[test]
    fn resolve_rows_object_artifact_converts_in_schema_order() {
        let (store, _g) = fresh_store();
        let body = ArtifactBody::Json(serde_json::json!([
            {"a": 1, "b": "x"},
            {"b": "y"}
        ]));
        let descriptor = ArtifactDescriptor {
            schema: Some(vec!["b".into(), "a".into()]),
            row_count: Some(2),
            ..rows_descriptor("agent-a", DEFAULT_TTL)
        };
        let id = store.write("agent-a", descriptor, body).unwrap().id;
        let req = RowsRequest {
            values: None,
            artifact_id: Some(id.as_str()),
            row_range: None,
        };
        let rows = resolve_rows_input(&*store, "agent-a", req).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![serde_json::json!("x"), serde_json::json!(1)],
                vec![serde_json::json!("y"), serde_json::Value::Null],
            ]
        );
    }

    #[test]
    fn resolve_rows_object_artifact_without_schema_derives_order() {
        let (store, _g) = fresh_store();
        let body = ArtifactBody::Json(serde_json::json!([{"a": 1, "b": 2}]));
        let descriptor = ArtifactDescriptor {
            schema: None,
            ..rows_descriptor("agent-a", DEFAULT_TTL)
        };
        let id = store.write("agent-a", descriptor, body).unwrap().id;
        let req = RowsRequest {
            values: None,
            artifact_id: Some(id.as_str()),
            row_range: None,
        };
        let rows = resolve_rows_input(&*store, "agent-a", req).unwrap();
        assert_eq!(rows, vec![vec![serde_json::json!(1), serde_json::json!(2)]]);
    }

    #[test]
    fn resolve_rows_object_artifact_respects_row_range() {
        let (store, _g) = fresh_store();
        let body = ArtifactBody::Json(serde_json::json!([{"a": 0}, {"a": 1}, {"a": 2}, {"a": 3}]));
        let descriptor = ArtifactDescriptor {
            schema: Some(vec!["a".into()]),
            row_count: Some(4),
            ..rows_descriptor("agent-a", DEFAULT_TTL)
        };
        let id = store.write("agent-a", descriptor, body).unwrap().id;
        let req = RowsRequest {
            values: None,
            artifact_id: Some(id.as_str()),
            row_range: Some("1-3"),
        };
        let rows = resolve_rows_input(&*store, "agent-a", req).unwrap();
        assert_eq!(
            rows,
            vec![vec![serde_json::json!(1)], vec![serde_json::json!(2)]]
        );
    }

    #[test]
    fn resolve_rows_inline_object_rows_still_rejected() {
        let (store, _g) = fresh_store();
        let req = RowsRequest {
            values: Some(serde_json::json!([{"a": 1}])),
            artifact_id: None,
            row_range: None,
        };
        let err = resolve_rows_input(&*store, "agent-a", req)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not an array"));
    }

    #[test]
    fn resolve_rows_rejects_both_values_and_artifact_id() {
        let (store, _g) = fresh_store();
        let req = RowsRequest {
            values: Some(serde_json::json!([[1]])),
            artifact_id: Some("art_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            row_range: None,
        };
        let err = resolve_rows_input(&*store, "agent-a", req)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not both"));
    }

    #[test]
    fn resolve_rows_rejects_neither_values_nor_artifact_id() {
        let (store, _g) = fresh_store();
        let err = resolve_rows_input(&*store, "agent-a", RowsRequest::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("either"));
    }

    #[test]
    fn resolve_rows_rejects_expired_artifact_id() {
        let (store, _g) = fresh_store();
        let body = ArtifactBody::Json(serde_json::json!([[1]]));
        let id = store
            .write(
                "agent-a",
                rows_descriptor("agent-a", Duration::from_secs(0)),
                body,
            )
            .unwrap()
            .id;
        std::thread::sleep(Duration::from_millis(20));
        let req = RowsRequest {
            values: None,
            artifact_id: Some(id.as_str()),
            row_range: None,
        };
        let err = resolve_rows_input(&*store, "agent-a", req)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"));
    }

    #[test]
    fn resolve_rows_rejects_mime_mismatch() {
        let (store, _g) = fresh_store();
        let descriptor = ArtifactDescriptor {
            producer: "test.csv".into(),
            mime: ContentType::TextCsv,
            schema: None,
            row_count: None,
            ttl_seconds: DEFAULT_TTL.as_secs(),
            agent_id: "agent-a".into(),
            session_id: None,
            user_id: None,
            filename: None,
            media_type: None,
        };
        let id = store
            .write(
                "agent-a",
                descriptor,
                ArtifactBody::Csv("a,b\n1,2\n".into()),
            )
            .unwrap()
            .id;
        let req = RowsRequest {
            values: None,
            artifact_id: Some(id.as_str()),
            row_range: None,
        };
        let err = resolve_rows_input(&*store, "agent-a", req)
            .unwrap_err()
            .to_string();
        assert!(err.contains("application/json"));
    }

    #[test]
    fn resolve_rows_rejects_out_of_bounds_row_range() {
        let (store, _g) = fresh_store();
        let id = write_rows(&store, "agent-a", serde_json::json!([[0], [1]]));
        let req = RowsRequest {
            values: None,
            artifact_id: Some(id.as_str()),
            row_range: Some("0-5"),
        };
        let err = resolve_rows_input(&*store, "agent-a", req)
            .unwrap_err()
            .to_string();
        assert!(err.contains("exceeds artifact row_count"));
    }

    #[test]
    fn resolve_rows_rejects_invalid_row_range_format() {
        let (store, _g) = fresh_store();
        let id = write_rows(&store, "agent-a", serde_json::json!([[0]]));
        let req = RowsRequest {
            values: None,
            artifact_id: Some(id.as_str()),
            row_range: Some("not-a-range"),
        };
        let err = resolve_rows_input(&*store, "agent-a", req)
            .unwrap_err()
            .to_string();
        assert!(err.contains("non-negative integer"));
    }

    #[test]
    fn resolve_rows_rejects_row_range_without_artifact_id() {
        let (store, _g) = fresh_store();
        let req = RowsRequest {
            values: Some(serde_json::json!([[1]])),
            artifact_id: None,
            row_range: Some("0-1"),
        };
        let err = resolve_rows_input(&*store, "agent-a", req)
            .unwrap_err()
            .to_string();
        assert!(err.contains("only valid with artifact_id"));
    }

    #[test]
    fn resolve_bytes_inline_passthrough() {
        let (store, _g) = fresh_store();
        let req = BytesRequest {
            data: Some(&[1, 2, 3]),
            data_base64: None,
            artifact_id: None,
        };
        let resolved = resolve_bytes_input(&*store, "agent-a", req).unwrap();
        assert_eq!(resolved.bytes, vec![1, 2, 3]);
    }

    #[test]
    fn resolve_bytes_base64_decoded() {
        let (store, _g) = fresh_store();
        let req = BytesRequest {
            data: None,
            data_base64: Some("aGVsbG8="),
            artifact_id: None,
        };
        let resolved = resolve_bytes_input(&*store, "agent-a", req).unwrap();
        assert_eq!(resolved.bytes, b"hello".to_vec());
    }

    #[test]
    fn resolve_bytes_via_artifact_loads_from_store() {
        let (store, _g) = fresh_store();
        let descriptor = ArtifactDescriptor {
            producer: "test.bytes".into(),
            mime: ContentType::OctetStream,
            schema: None,
            row_count: None,
            ttl_seconds: DEFAULT_TTL.as_secs(),
            agent_id: "agent-a".into(),
            session_id: None,
            user_id: None,
            filename: None,
            media_type: None,
        };
        let id = store
            .write("agent-a", descriptor, ArtifactBody::Bytes(vec![9, 9, 9]))
            .unwrap()
            .id;
        let req = BytesRequest {
            data: None,
            data_base64: None,
            artifact_id: Some(id.as_str()),
        };
        let resolved = resolve_bytes_input(&*store, "agent-a", req).unwrap();
        assert_eq!(resolved.bytes, vec![9, 9, 9]);
        assert_eq!(resolved.mime, ContentType::OctetStream);
    }

    #[test]
    fn resolve_bytes_rejects_multiple_sources() {
        let (store, _g) = fresh_store();
        let req = BytesRequest {
            data: Some(&[1]),
            data_base64: Some("aGk="),
            artifact_id: None,
        };
        let err = resolve_bytes_input(&*store, "agent-a", req)
            .unwrap_err()
            .to_string();
        assert!(err.contains("only one"));
    }

    #[test]
    fn resolve_bytes_rejects_no_source() {
        let (store, _g) = fresh_store();
        let err = resolve_bytes_input(&*store, "agent-a", BytesRequest::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("one of"));
    }

    #[test]
    fn workflow_scope_is_readable_from_every_step_session_of_one_instance() {
        let (store, _dir) = fresh_store();
        let staged = ArtifactDescriptor {
            session_id: Some("workflow:inst-1".to_string()),
            ..rows_descriptor("agent-a", DEFAULT_TTL)
        };
        let id = store
            .write(
                "agent-a",
                staged,
                ArtifactBody::Json(serde_json::json!([[1]])),
            )
            .unwrap()
            .id;

        let scope = |instance: &str, session: &str, id: &ArtifactId| {
            crate::runner::CURRENT_ARTIFACT_SCOPE.sync_scope(instance.to_string(), || {
                crate::runner::CURRENT_SESSION_ID.sync_scope(session.to_string(), || {
                    read_session_scoped(&*store, "agent-a", id).is_ok()
                })
            })
        };

        assert!(
            scope("workflow:inst-1", "workflow:inst-1:fetch", &id),
            "the staging step must read its own artifact"
        );
        assert!(
            scope("workflow:inst-1", "workflow:inst-1:report", &id),
            "a later step of the same instance must read what an earlier one staged"
        );
        assert!(
            !scope("workflow:inst-2", "workflow:inst-2:fetch", &id),
            "another instance must not read it"
        );

        let plain_chat = crate::runner::CURRENT_SESSION_ID
            .sync_scope("chat-session".to_string(), || {
                read_session_scoped(&*store, "agent-a", &id).is_ok()
            });
        assert!(!plain_chat, "an ordinary chat session must not read it");
    }
}
