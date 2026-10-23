//! Google Docs v1 tools.
//! Tool names: `google.docs.*`.
//! Base: <https://docs.googleapis.com/v1/>.
//!
//! The Docs API's document model is a tree of structural elements. This module
//! implements a minimal plain-text flattener for `cat`/`info` and passes
//! batchUpdate requests through for everything else.

use std::sync::Arc;

use eyre::Result;
use serde_json::{json, Map, Value};
use workforce_agent::tools::Tool;

use crate::adapters::google::api::{GoogleApi, GoogleApiContext};

use crate::adapters::google::tool_helpers::{
    handler, object_schema, optional_str, required_str, urlencode, GoogleTool,
};

const BASE: &str = "https://docs.googleapis.com/v1/documents";

pub fn tools(ctx: Arc<GoogleApiContext>) -> Vec<Arc<dyn Tool>> {
    let mut out: Vec<Arc<dyn Tool>> = Vec::new();

    // ─── cat (plain text) ────────────────────────────────────────────────────
    let cat_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("doc_id".into(), json!({ "type": "string" }));
            m
        },
        &["doc_id"],
    );
    out.push(
        GoogleTool::new(
            "google.docs.cat",
            "Read a Google Doc as plain text",
            cat_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(docs_cat(input, api))),
        )
        .arc(),
    );

    // ─── info (full metadata including title + revisionId) ─────────────────-
    let info_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("doc_id".into(), json!({ "type": "string" }));
            m
        },
        &["doc_id"],
    );
    out.push(
        GoogleTool::new(
            "google.docs.info",
            "Get a Google Doc's full metadata (title, revisionId, tabs)",
            info_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(docs_info(input, api))),
        )
        .arc(),
    );

    // ─── create ──────────────────────────────────────────────────────────────
    let create_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("title".into(), json!({ "type": "string" }));
            m
        },
        &["title"],
    );
    out.push(
        GoogleTool::new(
            "google.docs.create",
            "Create a new Google Doc with the given title",
            create_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(docs_create(input, api))),
        )
        .arc(),
    );

    // ─── append (insertText at end) ──────────────────────────────────────────
    let append_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("doc_id".into(), json!({ "type": "string" }));
            m.insert("text".into(), json!({ "type": "string" }));
            m
        },
        &["doc_id", "text"],
    );
    out.push(
        GoogleTool::new(
            "google.docs.append",
            "Append text at the end of a Google Doc",
            append_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(docs_append(input, api))),
        )
        .arc(),
    );

    // ─── insert at offset ────────────────────────────────────────────────────
    let insert_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("doc_id".into(), json!({ "type": "string" }));
            m.insert("text".into(), json!({ "type": "string" }));
            m.insert(
                "index".into(),
                json!({ "type": "integer", "description": "1-based character index" }),
            );
            m
        },
        &["doc_id", "text", "index"],
    );
    out.push(
        GoogleTool::new(
            "google.docs.insert",
            "Insert text at a character index in a Google Doc",
            insert_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(docs_insert(input, api))),
        )
        .arc(),
    );

    // ─── find-replace ────────────────────────────────────────────────────────
    let fr_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("doc_id".into(), json!({ "type": "string" }));
            m.insert("find".into(), json!({ "type": "string" }));
            m.insert("replace".into(), json!({ "type": "string" }));
            m.insert("match_case".into(), json!({ "type": "boolean" }));
            m
        },
        &["doc_id", "find", "replace"],
    );
    out.push(
        GoogleTool::new(
            "google.docs.find_replace",
            "Find-and-replace text in a Google Doc",
            fr_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(docs_find_replace(input, api))),
        )
        .arc(),
    );

    // ─── copy (uses Drive API) ───────────────────────────────────────────────
    let copy_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("doc_id".into(), json!({ "type": "string" }));
            m.insert("new_title".into(), json!({ "type": "string" }));
            m
        },
        &["doc_id"],
    );
    out.push(
        GoogleTool::new(
            "google.docs.copy",
            "Duplicate a Google Doc",
            copy_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(docs_copy(input, api))),
        )
        .arc(),
    );

    // ─── batch_update escape hatch ───────────────────────────────────────────
    let batch_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("doc_id".into(), json!({ "type": "string" }));
            m.insert("requests".into(), json!({ "type": "array" }));
            m
        },
        &["doc_id", "requests"],
    );
    out.push(
        GoogleTool::new(
            "google.docs.batch_update",
            "Run an arbitrary Docs batchUpdate (advanced)",
            batch_schema,
            ctx,
            handler(|input, api| Box::pin(docs_batch_update(input, api))),
        )
        .arc(),
    );

    out
}

async fn fetch_document(doc_id: &str, api: &GoogleApi) -> Result<Value> {
    let url = format!("{BASE}/{id}", id = urlencode(doc_id));
    api.get_json(&url).await
}

async fn docs_cat(input: Value, api: &GoogleApi) -> Result<Value> {
    let doc_id = required_str(&input, "doc_id")?;
    let doc = fetch_document(doc_id, api).await?;
    let text = flatten_to_text(&doc);
    let title = doc
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(json!({
        "doc_id": doc_id,
        "title": title,
        "content": text
    }))
}

async fn docs_info(input: Value, api: &GoogleApi) -> Result<Value> {
    let doc_id = required_str(&input, "doc_id")?;
    let doc = fetch_document(doc_id, api).await?;
    // Strip the body (huge) — only return top-level metadata.
    let mut trimmed = Map::new();
    for key in [
        "documentId",
        "title",
        "revisionId",
        "documentStyle",
        "namedStyles",
        "lists",
        "tabs",
    ] {
        if let Some(v) = doc.get(key) {
            trimmed.insert(key.to_string(), v.clone());
        }
    }
    Ok(Value::Object(trimmed))
}

async fn docs_create(input: Value, api: &GoogleApi) -> Result<Value> {
    let title = required_str(&input, "title")?;
    let body = json!({ "title": title });
    api.post_json(BASE, &body).await
}

async fn docs_append(input: Value, api: &GoogleApi) -> Result<Value> {
    let doc_id = required_str(&input, "doc_id")?;
    let text = required_str(&input, "text")?;

    // Fetch document to find the end index.
    let doc = fetch_document(doc_id, api).await?;
    let end_index = body_end_index(&doc).unwrap_or(1);
    // Docs API forbids inserting *at* the newline at the end — insert one before.
    let insert_at = end_index.saturating_sub(1).max(1);

    let requests = json!([{
        "insertText": {
            "location": { "index": insert_at },
            "text": text
        }
    }]);
    batch_update_call(doc_id, &requests, api).await
}

async fn docs_insert(input: Value, api: &GoogleApi) -> Result<Value> {
    let doc_id = required_str(&input, "doc_id")?;
    let text = required_str(&input, "text")?;
    let index = input
        .get("index")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| eyre::eyre!("index is required and must be an integer"))?;

    let requests = json!([{
        "insertText": {
            "location": { "index": index },
            "text": text
        }
    }]);
    batch_update_call(doc_id, &requests, api).await
}

async fn docs_find_replace(input: Value, api: &GoogleApi) -> Result<Value> {
    let doc_id = required_str(&input, "doc_id")?;
    let find = required_str(&input, "find")?;
    let replace = required_str(&input, "replace")?;
    let match_case = input
        .get("match_case")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let requests = json!([{
        "replaceAllText": {
            "containsText": { "text": find, "matchCase": match_case },
            "replaceText": replace
        }
    }]);
    batch_update_call(doc_id, &requests, api).await
}

async fn docs_copy(input: Value, api: &GoogleApi) -> Result<Value> {
    let doc_id = required_str(&input, "doc_id")?;
    let new_title = optional_str(&input, "new_title");

    let url = format!(
        "https://www.googleapis.com/drive/v3/files/{id}/copy",
        id = urlencode(doc_id)
    );
    let body = if let Some(t) = new_title {
        json!({ "name": t })
    } else {
        json!({})
    };
    api.post_json(&url, &body).await
}

async fn docs_batch_update(input: Value, api: &GoogleApi) -> Result<Value> {
    let doc_id = required_str(&input, "doc_id")?;
    let requests = input
        .get("requests")
        .cloned()
        .ok_or_else(|| eyre::eyre!("requests array is required"))?;
    batch_update_call(doc_id, &requests, api).await
}

async fn batch_update_call(doc_id: &str, requests: &Value, api: &GoogleApi) -> Result<Value> {
    let url = format!("{BASE}/{id}:batchUpdate", id = urlencode(doc_id));
    let body = json!({ "requests": requests });
    api.post_json(&url, &body).await
}

/// Flatten the Docs structural element tree into a plain-text approximation.
///
/// Walks `body.content[*].paragraph.elements[*].textRun.content`. Tables and
/// other complex structures are omitted for brevity — sufficient for summary
/// reads; use `batch_update` / `info` for structural work.
fn flatten_to_text(doc: &Value) -> String {
    let mut out = String::new();
    let Some(body) = doc.get("body") else {
        return out;
    };
    let Some(content) = body.get("content").and_then(|v| v.as_array()) else {
        return out;
    };
    for element in content {
        if let Some(paragraph) = element.get("paragraph") {
            if let Some(elements) = paragraph.get("elements").and_then(|v| v.as_array()) {
                for e in elements {
                    if let Some(text) = e
                        .get("textRun")
                        .and_then(|tr| tr.get("content"))
                        .and_then(|c| c.as_str())
                    {
                        out.push_str(text);
                    }
                }
            }
        } else if let Some(table) = element.get("table") {
            // Best-effort: iterate rows/cells, joining cell text with tabs.
            if let Some(rows) = table.get("tableRows").and_then(|v| v.as_array()) {
                for row in rows {
                    if let Some(cells) = row.get("tableCells").and_then(|v| v.as_array()) {
                        let cell_texts: Vec<String> = cells
                            .iter()
                            .map(|c| {
                                let sub = json!({ "body": { "content": c.get("content") } });
                                flatten_to_text(&sub)
                            })
                            .collect();
                        out.push_str(&cell_texts.join("\t"));
                        out.push('\n');
                    }
                }
            }
        }
    }
    out
}

/// Find the body end index — the position just past the last structural element.
fn body_end_index(doc: &Value) -> Option<u64> {
    let body = doc.get("body")?;
    let content = body.get("content")?.as_array()?;
    content.last()?.get("endIndex")?.as_u64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatten_simple_paragraph() {
        let doc = json!({
            "body": {
                "content": [
                    {
                        "paragraph": {
                            "elements": [
                                { "textRun": { "content": "Hello " } },
                                { "textRun": { "content": "world\n" } }
                            ]
                        }
                    }
                ]
            }
        });
        assert_eq!(flatten_to_text(&doc), "Hello world\n");
    }

    #[test]
    fn body_end_index_returns_last() {
        let doc = json!({
            "body": {
                "content": [
                    { "endIndex": 1 },
                    { "endIndex": 42 }
                ]
            }
        });
        assert_eq!(body_end_index(&doc), Some(42));
    }
}
