//! Google Slides v1 tools.
//! Tool names: `google.slides.*`.
//! Base: <https://slides.googleapis.com/v1/presentations/>.

use std::sync::Arc;

use base64::Engine as _;
use eyre::Result;
use serde_json::{json, Value};
use workforce_agent::tools::Tool;

use crate::adapters::google::api::{GoogleApi, GoogleApiContext};

use crate::adapters::google::tool_helpers::{
    handler, object_schema, optional_str, props, required_str, tool, urlencode,
};

const BASE: &str = "https://slides.googleapis.com/v1/presentations";

pub fn tools(ctx: Arc<GoogleApiContext>) -> Vec<Arc<dyn Tool>> {
    let mut out: Vec<Arc<dyn Tool>> = Vec::new();

    // ── info ─────────────────────────────────────────────────────────────────
    let info_schema = object_schema(
        props(&[("presentation_id", json!({"type":"string"}))]),
        &["presentation_id"],
    );
    out.push(tool(
        "google.slides.info",
        "Get presentation metadata",
        info_schema,
        &ctx,
        handler(|i, a| Box::pin(slides_info(i, a))),
    ));

    // ── create ───────────────────────────────────────────────────────────────
    let create_schema = object_schema(props(&[("title", json!({"type":"string"}))]), &["title"]);
    out.push(tool(
        "google.slides.create",
        "Create a new presentation",
        create_schema,
        &ctx,
        handler(|i, a| Box::pin(slides_create(i, a))),
    ));

    // ── list slides ──────────────────────────────────────────────────────────
    let list_schema = object_schema(
        props(&[("presentation_id", json!({"type":"string"}))]),
        &["presentation_id"],
    );
    out.push(tool(
        "google.slides.list_slides",
        "List all slides in a presentation",
        list_schema,
        &ctx,
        handler(|i, a| Box::pin(slides_list(i, a))),
    ));

    // ── read slide (text content) ────────────────────────────────────────────
    let read_schema = object_schema(
        props(&[
            ("presentation_id", json!({"type":"string"})),
            (
                "slide_index",
                json!({"type":"integer","description":"0-based slide index"}),
            ),
        ]),
        &["presentation_id", "slide_index"],
    );
    out.push(tool(
        "google.slides.read_slide",
        "Read text content of a specific slide",
        read_schema,
        &ctx,
        handler(|i, a| Box::pin(slides_read(i, a))),
    ));

    // ── add slide ────────────────────────────────────────────────────────────
    let add_schema = object_schema(
        props(&[
            ("presentation_id", json!({"type":"string"})),
            (
                "layout",
                json!({"type":"string","description":"Predefined layout: BLANK, TITLE, TITLE_AND_BODY, etc."}),
            ),
            (
                "insert_at",
                json!({"type":"integer","description":"0-based index where the slide is inserted"}),
            ),
        ]),
        &["presentation_id"],
    );
    out.push(tool(
        "google.slides.add_slide",
        "Add a new slide to a presentation",
        add_schema,
        &ctx,
        handler(|i, a| Box::pin(slides_add(i, a))),
    ));

    // ── delete slide ─────────────────────────────────────────────────────────
    let del_schema = object_schema(
        props(&[
            ("presentation_id", json!({"type":"string"})),
            (
                "slide_id",
                json!({"type":"string","description":"Object ID of the slide to delete"}),
            ),
        ]),
        &["presentation_id", "slide_id"],
    );
    out.push(tool(
        "google.slides.delete_slide",
        "Delete a slide from a presentation",
        del_schema,
        &ctx,
        handler(|i, a| Box::pin(slides_delete(i, a))),
    ));

    // ── copy (via Drive) ─────────────────────────────────────────────────────
    let copy_schema = object_schema(
        props(&[
            ("presentation_id", json!({"type":"string"})),
            ("new_title", json!({"type":"string"})),
        ]),
        &["presentation_id"],
    );
    out.push(tool(
        "google.slides.copy",
        "Duplicate a presentation",
        copy_schema,
        &ctx,
        handler(|i, a| Box::pin(slides_copy(i, a))),
    ));

    // ── export / download ────────────────────────────────────────────────────
    let export_schema = object_schema(
        props(&[
            ("presentation_id", json!({"type":"string"})),
            (
                "mime_type",
                json!({"type":"string","description":"Export format: application/pdf, image/png, etc. Default: application/pdf"}),
            ),
        ]),
        &["presentation_id"],
    );
    out.push(tool(
        "google.slides.export",
        "Export a presentation as PDF or other format",
        export_schema,
        &ctx,
        handler(|i, a| Box::pin(slides_export(i, a))),
    ));

    // ── update notes ─────────────────────────────────────────────────────────
    let notes_schema = object_schema(
        props(&[
            ("presentation_id", json!({"type":"string"})),
            ("slide_id", json!({"type":"string"})),
            (
                "text",
                json!({"type":"string","description":"Speaker notes text"}),
            ),
        ]),
        &["presentation_id", "slide_id", "text"],
    );
    out.push(tool(
        "google.slides.update_notes",
        "Set speaker notes on a slide",
        notes_schema,
        &ctx,
        handler(|i, a| Box::pin(slides_update_notes(i, a))),
    ));

    // ── replace slide (insert text into shape) ───────────────────────────────
    let replace_schema = object_schema(
        props(&[
            ("presentation_id", json!({"type":"string"})),
            ("find", json!({"type":"string"})),
            ("replace", json!({"type":"string"})),
            ("match_case", json!({"type":"boolean"})),
        ]),
        &["presentation_id", "find", "replace"],
    );
    out.push(tool(
        "google.slides.replace_text",
        "Find-and-replace text across all slides",
        replace_schema,
        &ctx,
        handler(|i, a| Box::pin(slides_replace_text(i, a))),
    ));

    // ── batch update (escape hatch) ──────────────────────────────────────────
    let batch_schema = object_schema(
        props(&[
            ("presentation_id", json!({"type":"string"})),
            ("requests", json!({"type":"array"})),
        ]),
        &["presentation_id", "requests"],
    );
    out.push(tool(
        "google.slides.batch_update",
        "Run arbitrary Slides batchUpdate requests (advanced)",
        batch_schema,
        &ctx,
        handler(|i, a| Box::pin(slides_batch_update(i, a))),
    ));

    out
}

async fn slides_info(input: Value, api: &GoogleApi) -> Result<Value> {
    let pid = required_str(&input, "presentation_id")?;
    let url = format!("{BASE}/{pid}", pid = urlencode(pid));
    api.get_json(&url).await
}

async fn slides_create(input: Value, api: &GoogleApi) -> Result<Value> {
    let title = required_str(&input, "title")?;
    api.post_json(BASE, &json!({"title": title})).await
}

async fn slides_list(input: Value, api: &GoogleApi) -> Result<Value> {
    let pid = required_str(&input, "presentation_id")?;
    let pres: Value = api
        .get_json(&format!("{BASE}/{pid}", pid = urlencode(pid)))
        .await?;
    let slides = pres.get("slides").cloned().unwrap_or(json!([]));
    Ok(json!({
        "presentation_id": pid,
        "slide_count": slides.as_array().map(|a| a.len()).unwrap_or(0),
        "slides": slides
    }))
}

async fn slides_read(input: Value, api: &GoogleApi) -> Result<Value> {
    let pid = required_str(&input, "presentation_id")?;
    let idx = input
        .get("slide_index")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    let pres: Value = api
        .get_json(&format!("{BASE}/{pid}", pid = urlencode(pid)))
        .await?;
    let slides = pres.get("slides").and_then(|v| v.as_array());
    let slide = slides
        .and_then(|a| a.get(idx))
        .ok_or_else(|| eyre::eyre!("slide index {idx} out of range"))?;
    let text = extract_slide_text(slide);
    let slide_id = slide.get("objectId").and_then(|v| v.as_str()).unwrap_or("");
    Ok(json!({
        "slide_index": idx,
        "object_id": slide_id,
        "text": text
    }))
}

async fn slides_add(input: Value, api: &GoogleApi) -> Result<Value> {
    let pid = required_str(&input, "presentation_id")?;
    let layout = optional_str(&input, "layout").unwrap_or("BLANK");
    let mut req = json!({
        "createSlide": {
            "slideLayoutReference": { "predefinedLayout": layout }
        }
    });
    if let Some(idx) = input.get("insert_at").and_then(|v| v.as_u64()) {
        req["createSlide"]["insertionIndex"] = json!(idx);
    }
    batch_call(pid, &json!([req]), api).await
}

async fn slides_delete(input: Value, api: &GoogleApi) -> Result<Value> {
    let pid = required_str(&input, "presentation_id")?;
    let sid = required_str(&input, "slide_id")?;
    let req = json!([{"deleteObject": {"objectId": sid}}]);
    batch_call(pid, &req, api).await
}

async fn slides_copy(input: Value, api: &GoogleApi) -> Result<Value> {
    let pid = required_str(&input, "presentation_id")?;
    let new_title = optional_str(&input, "new_title");
    let url = format!(
        "https://www.googleapis.com/drive/v3/files/{id}/copy",
        id = urlencode(pid)
    );
    let body = if let Some(t) = new_title {
        json!({"name": t})
    } else {
        json!({})
    };
    api.post_json(&url, &body).await
}

async fn slides_export(input: Value, api: &GoogleApi) -> Result<Value> {
    let pid = required_str(&input, "presentation_id")?;
    let mime = optional_str(&input, "mime_type").unwrap_or("application/pdf");
    let url = format!(
        "https://www.googleapis.com/drive/v3/files/{id}/export?mimeType={mime}",
        id = urlencode(pid),
        mime = urlencode(mime)
    );
    let bytes = api.download_bytes(&url).await?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(json!({
        "presentation_id": pid,
        "mime_type": mime,
        "content_base64": b64,
        "bytes": bytes.len()
    }))
}

async fn slides_update_notes(input: Value, api: &GoogleApi) -> Result<Value> {
    let pid = required_str(&input, "presentation_id")?;
    let sid = required_str(&input, "slide_id")?;
    let text = required_str(&input, "text")?;

    // Fetch slide to get the notes page object ID.
    let pres: Value = api
        .get_json(&format!("{BASE}/{pid}", pid = urlencode(pid)))
        .await?;
    let notes_id = pres
        .get("slides")
        .and_then(|v| v.as_array())
        .and_then(|a| {
            a.iter()
                .find(|s| s.get("objectId").and_then(|v| v.as_str()) == Some(sid))
        })
        .and_then(|s| s.get("slideProperties"))
        .and_then(|p| p.get("notesPage"))
        .and_then(|n| n.get("notesProperties"))
        .and_then(|p| p.get("speakerNotesObjectId"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("could not find speaker notes object ID for slide {sid}"))?;

    let reqs = json!([
        {"deleteText": {"objectId": notes_id, "textRange": {"type": "ALL"}}},
        {"insertText": {"objectId": notes_id, "text": text, "insertionIndex": 0}}
    ]);
    batch_call(pid, &reqs, api).await
}

async fn slides_replace_text(input: Value, api: &GoogleApi) -> Result<Value> {
    let pid = required_str(&input, "presentation_id")?;
    let find = required_str(&input, "find")?;
    let replace = required_str(&input, "replace")?;
    let match_case = input
        .get("match_case")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let req = json!([{
        "replaceAllText": {
            "containsText": {"text": find, "matchCase": match_case},
            "replaceText": replace
        }
    }]);
    batch_call(pid, &req, api).await
}

async fn slides_batch_update(input: Value, api: &GoogleApi) -> Result<Value> {
    let pid = required_str(&input, "presentation_id")?;
    let requests = input
        .get("requests")
        .cloned()
        .ok_or_else(|| eyre::eyre!("requests is required"))?;
    batch_call(pid, &requests, api).await
}

async fn batch_call(pid: &str, requests: &Value, api: &GoogleApi) -> Result<Value> {
    let url = format!("{BASE}/{pid}:batchUpdate", pid = urlencode(pid));
    api.post_json(&url, &json!({"requests": requests})).await
}

/// Extract plain text from a slide's page elements.
fn extract_slide_text(slide: &Value) -> String {
    let mut out = String::new();
    let elements = slide.get("pageElements").and_then(|v| v.as_array());
    if let Some(elems) = elements {
        for elem in elems {
            if let Some(shape) = elem.get("shape") {
                if let Some(text) = shape.get("text") {
                    if let Some(elements) = text.get("textElements").and_then(|v| v.as_array()) {
                        for te in elements {
                            if let Some(tr) = te.get("textRun") {
                                if let Some(c) = tr.get("content").and_then(|v| v.as_str()) {
                                    out.push_str(c);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    out
}
