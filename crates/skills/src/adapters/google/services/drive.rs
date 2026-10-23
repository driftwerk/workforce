//! Google Drive v3 tools.
//! Tool names follow `google.drive.*`.
//!
//! Base: <https://www.googleapis.com/drive/v3/>.
//! Upload endpoint: <https://www.googleapis.com/upload/drive/v3/files>.

use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine as _;
use eyre::Result;
use serde_json::{json, Map, Value};
use workforce_agent::artifacts::{global_store, resolve_bytes_input, ArtifactStore, BytesRequest};
use workforce_agent::runner::CURRENT_AGENT_ID;
use workforce_agent::tools::Tool;

use crate::adapters::google::api::{GoogleApi, GoogleApiContext};

use crate::adapters::google::tool_helpers::{
    handler, object_schema, optional_str, optional_u64, required_str, urlencode, GoogleTool,
};

const BASE: &str = "https://www.googleapis.com/drive/v3";
const UPLOAD: &str = "https://www.googleapis.com/upload/drive/v3/files";

/// Resolve drive upload bytes: inline `content`, base64 `content_base64`,
/// or an artifact handle. Caller-supplied `mime_type` wins; the artifact's
/// mime is used as a fallback so binary uploads from a CSV/JSON artifact
/// arrive with the correct Content-Type header.
fn resolve_upload_bytes(input: &Value) -> Result<(Vec<u8>, String)> {
    let explicit_mime = optional_str(input, "mime_type").map(|s| s.to_string());
    let inline_text = optional_str(input, "content");
    let inline_b64 = optional_str(input, "content_base64");
    let artifact_id = optional_str(input, "artifact_id");

    match (inline_text, inline_b64, artifact_id) {
        (None, None, None) => Err(eyre::eyre!(
            "drive.upload requires one of: content, content_base64, artifact_id"
        )),
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) | (_, Some(_), Some(_)) => Err(eyre::eyre!(
            "drive.upload accepts only one of: content, content_base64, artifact_id"
        )),
        (Some(text), None, None) => Ok((
            text.as_bytes().to_vec(),
            explicit_mime.unwrap_or_else(|| "text/plain; charset=UTF-8".into()),
        )),
        (None, Some(b64), None) => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| eyre::eyre!("invalid content_base64: {e}"))?;
            Ok((
                bytes,
                explicit_mime.unwrap_or_else(|| "application/octet-stream".into()),
            ))
        }
        (None, None, Some(id)) => {
            let store = global_store().ok_or_else(|| {
                eyre::eyre!(
                    "artifact_id supplied but no artifact store is installed in this runtime"
                )
            })?;
            let agent_id = CURRENT_AGENT_ID
                .try_with(|a| a.clone())
                .ok()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| eyre::eyre!("artifact_id supplied outside an agent run"))?;
            let resolved = resolve_bytes_input(
                &*store as &dyn ArtifactStore,
                &agent_id,
                BytesRequest {
                    data: None,
                    data_base64: None,
                    artifact_id: Some(id),
                },
            )?;
            Ok((
                resolved.bytes,
                explicit_mime.unwrap_or_else(|| resolved.mime.to_string()),
            ))
        }
    }
}

/// Build the Drive file metadata for an upload. A `convert_to` Google Apps MIME
/// type on the metadata makes Drive convert the uploaded media server-side
/// (e.g. `.docx` → native Google Doc); without it the file is stored as-is.
fn upload_metadata(name: &str, parent: Option<&str>, convert_to: Option<&str>) -> Value {
    let mut metadata = json!({ "name": name });
    if let Some(p) = parent {
        metadata["parents"] = json!([p]);
    }
    if let Some(target) = convert_to {
        metadata["mimeType"] = json!(target);
    }
    metadata
}

pub fn tools(ctx: Arc<GoogleApiContext>) -> Vec<Arc<dyn Tool>> {
    let mut out: Vec<Arc<dyn Tool>> = Vec::new();

    // ─── list/search ─────────────────────────────────────────────────────────
    let search_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert(
                "q".into(),
                json!({ "type": "string", "description": "Drive search query (e.g. \"name contains 'report'\")." }),
            );
            m.insert("max_results".into(), json!({ "type": "integer" }));
            m.insert(
                "order_by".into(),
                json!({ "type": "string", "description": "e.g. 'modifiedTime desc'" }),
            );
            m.insert("page_token".into(), json!({ "type": "string" }));
            m
        },
        &[],
    );
    let search_handler = handler(|input, api| Box::pin(drive_search(input, api)));
    out.push(
        GoogleTool::new(
            "google.drive.search",
            "Search Google Drive files",
            search_schema.clone(),
            ctx.clone(),
            search_handler.clone(),
        )
        .arc(),
    );
    // ls alias
    out.push(
        GoogleTool::new(
            "google.drive.ls",
            "List Google Drive files (alias for search)",
            search_schema,
            ctx.clone(),
            search_handler,
        )
        .arc(),
    );

    // ─── get ─────────────────────────────────────────────────────────────────
    let get_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("file_id".into(), json!({ "type": "string" }));
            m.insert(
                "fields".into(),
                json!({ "type": "string", "description": "Comma-separated fields; default covers common metadata." }),
            );
            m
        },
        &["file_id"],
    );
    out.push(
        GoogleTool::new(
            "google.drive.get",
            "Get file metadata by ID",
            get_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(drive_get(input, api))),
        )
        .arc(),
    );

    // ─── download ────────────────────────────────────────────────────────────
    let dl_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("file_id".into(), json!({ "type": "string" }));
            m.insert(
                "as_text".into(),
                json!({ "type": "boolean", "description": "If true, return response as UTF-8 text; otherwise base64." }),
            );
            m.insert(
                "export_mime".into(),
                json!({ "type": "string", "description": "For Google-native docs, specify an export MIME type (e.g., 'text/plain')." }),
            );
            m
        },
        &["file_id"],
    );
    out.push(
        GoogleTool::new(
            "google.drive.download",
            "Download a Drive file (binary → base64 or text)",
            dl_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(drive_download(input, api))),
        )
        .arc(),
    );

    // ─── upload ──────────────────────────────────────────────────────────────
    let mut up_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("name".into(), json!({ "type": "string" }));
            m.insert("content".into(), json!({ "type": "string", "description": "Inline file content. Mutually exclusive with content_base64 and artifact_id." }));
            m.insert("content_base64".into(), json!({ "type": "string", "description": "Base64-encoded bytes. Mutually exclusive with content and artifact_id." }));
            m.insert("artifact_id".into(), json!({
                "type": "string",
                "description": "Artifact handle from a producer (e.g. metabase.dataset_query_native). Use this instead of `content` / `content_base64` to upload a large body without holding the bytes in context. Mutually exclusive with content and content_base64."
            }));
            m.insert("mime_type".into(), json!({ "type": "string" }));
            m.insert(
                "parent_id".into(),
                json!({ "type": "string", "description": "Drive folder ID to upload into." }),
            );
            m.insert("convert_to".into(), json!({
                "type": "string",
                "description": "Convert the upload to a native Google editable format on import: \
                    'application/vnd.google-apps.document' (Word/.docx → Docs), \
                    'application/vnd.google-apps.presentation' (PowerPoint/.pptx → Slides), \
                    'application/vnd.google-apps.spreadsheet' (.xlsx/.csv → Sheets). Pair with \
                    export_report to publish a report as a native Google Doc/Slides. Omit to keep the file as-is."
            }));
            m
        },
        &["name"],
    );
    if let Some(obj) = up_schema.as_object_mut() {
        obj.insert(
            "oneOf".to_string(),
            json!([
                {"required": ["content"]},
                {"required": ["content_base64"]},
                {"required": ["artifact_id"]},
            ]),
        );
    }
    out.push(
        GoogleTool::new(
            "google.drive.upload",
            "Upload a new file to Google Drive (optionally converting it to a native Google Doc/Slides/Sheet)",
            up_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(drive_upload(input, api))),
        )
        .arc(),
    );

    // ─── mkdir (create folder) ───────────────────────────────────────────────
    let mkdir_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("name".into(), json!({ "type": "string" }));
            m.insert("parent_id".into(), json!({ "type": "string" }));
            m
        },
        &["name"],
    );
    out.push(
        GoogleTool::new(
            "google.drive.mkdir",
            "Create a new folder in Google Drive",
            mkdir_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(drive_mkdir(input, api))),
        )
        .arc(),
    );

    // ─── rename (patch name) ─────────────────────────────────────────────────
    let rename_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("file_id".into(), json!({ "type": "string" }));
            m.insert("new_name".into(), json!({ "type": "string" }));
            m
        },
        &["file_id", "new_name"],
    );
    out.push(
        GoogleTool::new(
            "google.drive.rename",
            "Rename a Drive file",
            rename_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(drive_rename(input, api))),
        )
        .arc(),
    );

    // ─── move (change parent) ────────────────────────────────────────────────
    let move_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("file_id".into(), json!({ "type": "string" }));
            m.insert("new_parent_id".into(), json!({ "type": "string" }));
            m
        },
        &["file_id", "new_parent_id"],
    );
    out.push(
        GoogleTool::new(
            "google.drive.move",
            "Move a file to a different parent folder",
            move_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(drive_move(input, api))),
        )
        .arc(),
    );

    // ─── copy ────────────────────────────────────────────────────────────────
    let copy_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("file_id".into(), json!({ "type": "string" }));
            m.insert("new_name".into(), json!({ "type": "string" }));
            m.insert("parent_id".into(), json!({ "type": "string" }));
            m
        },
        &["file_id"],
    );
    out.push(
        GoogleTool::new(
            "google.drive.copy",
            "Duplicate a Drive file",
            copy_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(drive_copy(input, api))),
        )
        .arc(),
    );

    // ─── delete ──────────────────────────────────────────────────────────────
    let delete_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("file_id".into(), json!({ "type": "string" }));
            m
        },
        &["file_id"],
    );
    out.push(
        GoogleTool::new(
            "google.drive.delete",
            "Permanently delete a Drive file",
            delete_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(drive_delete(input, api))),
        )
        .arc(),
    );

    // ─── share ───────────────────────────────────────────────────────────────
    let share_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("file_id".into(), json!({ "type": "string" }));
            m.insert(
                "email".into(),
                json!({ "type": "string", "description": "Email of the grantee (required for 'user' or 'group')." }),
            );
            m.insert(
                "role".into(),
                json!({
                    "type": "string",
                    "enum": ["reader", "commenter", "writer", "fileOrganizer", "organizer", "owner"],
                    "description": "Permission role."
                }),
            );
            m.insert(
                "type".into(),
                json!({
                    "type": "string",
                    "enum": ["user", "group", "domain", "anyone"]
                }),
            );
            m.insert("send_notification".into(), json!({ "type": "boolean" }));
            m
        },
        &["file_id", "role"],
    );
    out.push(
        GoogleTool::new(
            "google.drive.share",
            "Grant a permission on a Drive file",
            share_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(drive_share(input, api))),
        )
        .arc(),
    );

    // ─── permissions list ────────────────────────────────────────────────────
    let perms_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("file_id".into(), json!({ "type": "string" }));
            m
        },
        &["file_id"],
    );
    out.push(
        GoogleTool::new(
            "google.drive.permissions",
            "List sharing permissions for a Drive file",
            perms_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(drive_permissions(input, api))),
        )
        .arc(),
    );

    // ─── drives (shared drives) list ─────────────────────────────────────────
    out.push(
        GoogleTool::new(
            "google.drive.drives",
            "List shared drives accessible to the user",
            object_schema(Map::new(), &[]),
            ctx.clone(),
            handler(|input, api| Box::pin(drive_drives(input, api))),
        )
        .arc(),
    );

    // ─── comments list ───────────────────────────────────────────────────────
    let comments_list_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("file_id".into(), json!({ "type": "string" }));
            m
        },
        &["file_id"],
    );
    out.push(
        GoogleTool::new(
            "google.drive.comments_list",
            "List comments on a Drive file",
            comments_list_schema,
            ctx,
            handler(|input, api| Box::pin(drive_comments_list(input, api))),
        )
        .arc(),
    );

    out
}

async fn drive_search(input: Value, api: &GoogleApi) -> Result<Value> {
    let mut query: Vec<(String, String)> = Vec::new();
    if let Some(q) = optional_str(&input, "q") {
        query.push(("q".into(), q.into()));
    }
    let max = optional_u64(&input, "max_results").unwrap_or(25).min(1000);
    query.push(("pageSize".into(), max.to_string()));
    if let Some(o) = optional_str(&input, "order_by") {
        query.push(("orderBy".into(), o.into()));
    }
    if let Some(t) = optional_str(&input, "page_token") {
        query.push(("pageToken".into(), t.into()));
    }
    query.push((
        "fields".into(),
        "nextPageToken, files(id, name, mimeType, modifiedTime, size, webViewLink, parents)".into(),
    ));

    let url = format!("{BASE}/files");
    api.get_json_with_query(&url, Some(&query)).await
}

async fn drive_get(input: Value, api: &GoogleApi) -> Result<Value> {
    let file_id = required_str(&input, "file_id")?;
    let fields = optional_str(&input, "fields")
        .unwrap_or("id, name, mimeType, modifiedTime, size, webViewLink, parents, owners");
    let url = format!("{BASE}/files/{id}", id = urlencode(file_id));
    let query = vec![("fields".to_string(), fields.to_string())];
    api.get_json_with_query(&url, Some(&query)).await
}

async fn drive_download(input: Value, api: &GoogleApi) -> Result<Value> {
    let file_id = required_str(&input, "file_id")?;
    let as_text = input
        .get("as_text")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let url = if let Some(mime) = optional_str(&input, "export_mime") {
        format!(
            "{BASE}/files/{id}/export?mimeType={mime}",
            id = urlencode(file_id),
            mime = urlencode(mime)
        )
    } else {
        format!("{BASE}/files/{id}?alt=media", id = urlencode(file_id))
    };

    let bytes = api.download_bytes(&url).await?;

    if as_text {
        let text = String::from_utf8_lossy(&bytes).to_string();
        Ok(json!({ "file_id": file_id, "content": text, "bytes": bytes.len() }))
    } else {
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Ok(json!({ "file_id": file_id, "content_base64": b64, "bytes": bytes.len() }))
    }
}

async fn drive_upload(input: Value, api: &GoogleApi) -> Result<Value> {
    let name = required_str(&input, "name")?;
    let parent = optional_str(&input, "parent_id");
    let convert_to = optional_str(&input, "convert_to");

    let (bytes, mime) = resolve_upload_bytes(&input)?;

    // Multipart upload: part 1 = JSON metadata, part 2 = file bytes.
    let boundary = format!("wf-{}", uuid::Uuid::new_v4().simple());
    let metadata = upload_metadata(name, parent, convert_to);

    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Type: application/json; charset=UTF-8\r\n\r\n");
    body.extend_from_slice(serde_json::to_vec(&metadata)?.as_slice());
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(format!("Content-Type: {mime}\r\n\r\n").as_bytes());
    body.extend_from_slice(&bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let content_type = format!("multipart/related; boundary={boundary}");
    let mut query = HashMap::new();
    query.insert("uploadType", "multipart".to_string());
    api.upload_bytes(UPLOAD, &query, &content_type, body).await
}

async fn drive_mkdir(input: Value, api: &GoogleApi) -> Result<Value> {
    let name = required_str(&input, "name")?;
    let mut body = json!({
        "name": name,
        "mimeType": "application/vnd.google-apps.folder"
    });
    if let Some(p) = optional_str(&input, "parent_id") {
        body["parents"] = json!([p]);
    }
    let url = format!("{BASE}/files");
    api.post_json(&url, &body).await
}

async fn drive_rename(input: Value, api: &GoogleApi) -> Result<Value> {
    let file_id = required_str(&input, "file_id")?;
    let new_name = required_str(&input, "new_name")?;
    let url = format!("{BASE}/files/{id}", id = urlencode(file_id));
    api.patch_json(&url, &json!({ "name": new_name })).await
}

async fn drive_move(input: Value, api: &GoogleApi) -> Result<Value> {
    let file_id = required_str(&input, "file_id")?;
    let new_parent = required_str(&input, "new_parent_id")?;

    // First fetch existing parents so we can remove them.
    let meta: Value = {
        let url = format!("{BASE}/files/{id}?fields=parents", id = urlencode(file_id));
        api.get_json(&url).await?
    };
    let current_parents: Vec<String> = meta
        .get("parents")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let url = format!("{BASE}/files/{id}", id = urlencode(file_id));
    let query = vec![
        ("addParents".to_string(), new_parent.to_string()),
        ("removeParents".to_string(), current_parents.join(",")),
    ];
    api.patch_json_with_query(&url, Some(&query), &json!({}))
        .await
}

async fn drive_copy(input: Value, api: &GoogleApi) -> Result<Value> {
    let file_id = required_str(&input, "file_id")?;
    let mut body = Map::new();
    if let Some(n) = optional_str(&input, "new_name") {
        body.insert("name".into(), json!(n));
    }
    if let Some(p) = optional_str(&input, "parent_id") {
        body.insert("parents".into(), json!([p]));
    }
    let url = format!("{BASE}/files/{id}/copy", id = urlencode(file_id));
    api.post_json(&url, &Value::Object(body)).await
}

async fn drive_delete(input: Value, api: &GoogleApi) -> Result<Value> {
    let file_id = required_str(&input, "file_id")?;
    let url = format!("{BASE}/files/{id}", id = urlencode(file_id));
    api.delete(&url).await?;
    Ok(json!({ "deleted": true, "file_id": file_id }))
}

async fn drive_share(input: Value, api: &GoogleApi) -> Result<Value> {
    let file_id = required_str(&input, "file_id")?;
    let role = required_str(&input, "role")?;
    let perm_type = optional_str(&input, "type").unwrap_or("user");
    let send_notification = input
        .get("send_notification")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut body = json!({
        "role": role,
        "type": perm_type,
    });
    if let Some(email) = optional_str(&input, "email") {
        body["emailAddress"] = json!(email);
    }

    let url = format!("{BASE}/files/{id}/permissions", id = urlencode(file_id));
    let query = vec![(
        "sendNotificationEmail".to_string(),
        send_notification.to_string(),
    )];
    api.post_json_with_query(&url, Some(&query), Some(&body))
        .await
}

async fn drive_permissions(input: Value, api: &GoogleApi) -> Result<Value> {
    let file_id = required_str(&input, "file_id")?;
    let url = format!("{BASE}/files/{id}/permissions", id = urlencode(file_id));
    api.get_json(&url).await
}

async fn drive_drives(_input: Value, api: &GoogleApi) -> Result<Value> {
    let url = format!("{BASE}/drives");
    api.get_json(&url).await
}

async fn drive_comments_list(input: Value, api: &GoogleApi) -> Result<Value> {
    let file_id = required_str(&input, "file_id")?;
    let url = format!(
        "{BASE}/files/{id}/comments?fields=*",
        id = urlencode(file_id)
    );
    api.get_json(&url).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_support::fresh_agent_id as mint_agent_id;
    use workforce_agent::artifacts::{ArtifactBody, ArtifactDescriptor, ContentType, DEFAULT_TTL};

    fn fresh_agent_id() -> String {
        mint_agent_id("drive-test-agent")
    }

    fn bytes_artifact(bytes: Vec<u8>, agent: &str) -> String {
        let store = global_store().unwrap();
        store
            .write(
                agent,
                ArtifactDescriptor {
                    producer: "test".into(),
                    mime: ContentType::OctetStream,
                    schema: None,
                    row_count: None,
                    ttl_seconds: DEFAULT_TTL.as_secs(),
                    agent_id: agent.into(),
                    session_id: None,
                    user_id: None,
                    filename: None,
                    media_type: None,
                },
                ArtifactBody::Bytes(bytes),
            )
            .unwrap()
            .id
            .to_string()
    }

    fn run_scoped<F, T>(agent: String, f: F) -> T
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let runner = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runner.block_on(CURRENT_AGENT_ID.scope(agent, async move { f() }))
    }

    #[test]
    fn upload_inline_content_text_uses_text_plain_default() {
        let input = json!({ "content": "hello" });
        let (bytes, mime) = resolve_upload_bytes(&input).unwrap();
        assert_eq!(bytes, b"hello".to_vec());
        assert!(mime.starts_with("text/plain"));
    }

    #[test]
    fn upload_inline_base64_decodes_and_defaults_octet_stream() {
        let input = json!({ "content_base64": "aGVsbG8=" });
        let (bytes, mime) = resolve_upload_bytes(&input).unwrap();
        assert_eq!(bytes, b"hello".to_vec());
        assert_eq!(mime, "application/octet-stream");
    }

    #[test]
    fn upload_explicit_mime_overrides_default() {
        let input = json!({ "content": "x,y\n1,2", "mime_type": "text/csv" });
        let (_, mime) = resolve_upload_bytes(&input).unwrap();
        assert_eq!(mime, "text/csv");
    }

    #[test]
    fn upload_via_artifact_loads_bytes_from_store() {
        let agent = fresh_agent_id();
        let id = bytes_artifact(vec![1, 2, 3], &agent);
        let input = json!({ "artifact_id": id });
        let (bytes, mime) = run_scoped(agent, move || resolve_upload_bytes(&input).unwrap());
        assert_eq!(bytes, vec![1, 2, 3]);
        assert_eq!(mime, "application/octet-stream");
    }

    #[test]
    fn upload_rejects_no_source() {
        let input = json!({});
        let err = resolve_upload_bytes(&input).unwrap_err();
        assert!(err.to_string().contains("requires"));
    }

    #[test]
    fn upload_rejects_multiple_sources() {
        let input = json!({ "content": "x", "content_base64": "aGk=" });
        let err = resolve_upload_bytes(&input).unwrap_err();
        assert!(err.to_string().contains("only one"));
    }

    #[test]
    fn upload_rejects_artifact_without_agent_scope() {
        let agent = fresh_agent_id();
        let id = bytes_artifact(vec![0], &agent);
        let input = json!({ "artifact_id": id });
        let err = resolve_upload_bytes(&input).unwrap_err();
        assert!(err.to_string().contains("agent run"));
    }

    #[test]
    fn upload_metadata_sets_name_parent_and_conversion_target() {
        let plain = upload_metadata("Report.docx", None, None);
        assert_eq!(plain, json!({ "name": "Report.docx" }));

        let converted = upload_metadata(
            "Report.docx",
            Some("folder123"),
            Some("application/vnd.google-apps.document"),
        );
        assert_eq!(
            converted,
            json!({
                "name": "Report.docx",
                "parents": ["folder123"],
                "mimeType": "application/vnd.google-apps.document"
            })
        );
    }
}
