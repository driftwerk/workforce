//! Gmail API tools.
//! Tool names: `google.gmail.*`.
//! Base: <https://gmail.googleapis.com/gmail/v1/users/me/>.
//!
//! Covers: messages search/get/send/modify/trash/untrash/archive/
//! mark-read/mark-unread, threads get/modify, drafts CRUD, labels CRUD,
//! settings (filters, sendAs list, vacation, forwarding list),
//! history, attachments.
//!
//! Intentionally not exposed (would require scopes incompatible with the
//! agent's TV-client device flow, see `oauth.rs::SCOPES`):
//! - Permanent message deletion — needs `https://mail.google.com/`.
//!   Use `trash` / `batch_modify`; Gmail auto-purges trash after 30 days.
//! - Adding a forwarding address — needs `gmail.settings.sharing`.
//!
//! Skipped (out of scope for Tier B): watch serve, track opens/setup/status.

use std::sync::Arc;

use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine as _;
use eyre::Result;
use serde_json::{json, Map, Value};
use workforce_agent::tools::image_view::{
    stage_for_view, view_pending_note, within_view_cap, MAX_VIEW_BYTES, VIEWED_IMAGES_KEY,
};
use workforce_agent::tools::{SurfaceEffect, Tool};

use crate::adapters::google::api::{GoogleApi, GoogleApiContext};
use crate::adapters::outbound_mail::{assemble, MailPart};
use crate::adapters::utils::{
    artifact_attachments_schema, coerce_to_id_string, coerce_to_u64, resolve_artifact_attachments,
};

use crate::adapters::google::tool_helpers::{
    handler, object_schema, optional_str, optional_u64, props, required_str, sending_tool, tool,
    urlencode,
};

const BASE: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

/// Gmail's base64url, which may arrive with or without padding.
const GMAIL_BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::URL_SAFE,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

pub fn tools(ctx: Arc<GoogleApiContext>) -> Vec<Arc<dyn Tool>> {
    let mut out: Vec<Arc<dyn Tool>> = Vec::new();

    // ── messages search ──────────────────────────────────────────────────────
    let search_schema = object_schema(
        props(&[
            (
                "q",
                json!({"type":"string","description":"Gmail search query (same syntax as Gmail search box)"}),
            ),
            (
                "max_results",
                json!({"type":"integer","description":"Max messages (default 20)"}),
            ),
            ("page_token", json!({"type":"string"})),
            (
                "label_ids",
                json!({"type":"array","items":{"type":"string"}}),
            ),
        ]),
        &["q"],
    );
    out.push(tool(
        "google.gmail.search",
        "Search Gmail messages",
        search_schema,
        &ctx,
        handler(|i, a| Box::pin(gmail_search(i, a))),
    ));

    // ── messages get ─────────────────────────────────────────────────────────
    let get_schema = object_schema(
        props(&[
            ("message_id", json!({"type":"string"})),
            (
                "format",
                json!({"type":"string","enum":["full","metadata","minimal","raw"]}),
            ),
        ]),
        &["message_id"],
    );
    out.push(tool(
        "google.gmail.get",
        "Get a Gmail message by ID. A message with a payload also carries a top-level \
         `attachments` list of `{part_id, filename, mime_type, size}`; open one with \
         google.gmail.attachment.",
        get_schema,
        &ctx,
        handler(|i, a| Box::pin(gmail_get(i, a))),
    ));

    // ── send ─────────────────────────────────────────────────────────────────
    let send_schema = object_schema(
        props(&[
            (
                "to",
                json!({"type":"string","description":"Recipient email(s), comma-separated"}),
            ),
            ("subject", json!({"type":"string"})),
            (
                "body",
                json!({"type":"string","description":"Plain-text body"}),
            ),
            (
                "html_body",
                json!({"type":"string","description":"HTML body (optional, overrides body)"}),
            ),
            ("cc", json!({"type":"string"})),
            ("bcc", json!({"type":"string"})),
            (
                "in_reply_to",
                json!({"type":"string","description":"Message-ID header to reply to"}),
            ),
            (
                "thread_id",
                json!({"type":"string","description":"Thread to place this message in"}),
            ),
            (
                "attach_artifact_ids",
                artifact_attachments_schema("html_body"),
            ),
        ]),
        &["to", "subject"],
    );
    out.push(sending_tool(
        "google.gmail.send",
        "Send a Gmail message",
        send_schema,
        &ctx,
        handler(|i, a| Box::pin(gmail_send(i, a))),
        SurfaceEffect::email,
    ));

    // ── modify (add/remove labels) ───────────────────────────────────────────
    let modify_schema = object_schema(
        props(&[
            ("message_id", json!({"type":"string"})),
            (
                "add_label_ids",
                json!({"type":"array","items":{"type":"string"}}),
            ),
            (
                "remove_label_ids",
                json!({"type":"array","items":{"type":"string"}}),
            ),
        ]),
        &["message_id"],
    );
    out.push(tool(
        "google.gmail.modify",
        "Modify labels on a Gmail message",
        modify_schema,
        &ctx,
        handler(|i, a| Box::pin(gmail_modify(i, a))),
    ));

    // ── trash / untrash ──────────────────────────────────────────────────────
    let msg_id_schema = object_schema(
        props(&[("message_id", json!({"type":"string"}))]),
        &["message_id"],
    );
    out.push(tool(
        "google.gmail.trash",
        "Move a message to Trash",
        msg_id_schema.clone(),
        &ctx,
        handler(|i, a| Box::pin(gmail_trash(i, a))),
    ));
    out.push(tool(
        "google.gmail.untrash",
        "Remove a message from Trash",
        msg_id_schema.clone(),
        &ctx,
        handler(|i, a| Box::pin(gmail_untrash(i, a))),
    ));

    // ── archive (remove INBOX label) ─────────────────────────────────────────
    out.push(tool(
        "google.gmail.archive",
        "Archive a message (remove INBOX label)",
        msg_id_schema.clone(),
        &ctx,
        handler(|i, a| Box::pin(gmail_archive(i, a))),
    ));

    // ── mark read / unread ───────────────────────────────────────────────────
    out.push(tool(
        "google.gmail.mark_read",
        "Mark a message as read",
        msg_id_schema.clone(),
        &ctx,
        handler(|i, a| Box::pin(gmail_mark_read(i, a))),
    ));
    out.push(tool(
        "google.gmail.mark_unread",
        "Mark a message as unread",
        msg_id_schema,
        &ctx,
        handler(|i, a| Box::pin(gmail_mark_unread(i, a))),
    ));

    // ── attachment get ────────────────────────────────────────────────────────
    let attach_schema = object_schema(
        props(&[
            ("message_id", json!({"type":"string"})),
            (
                "part_id",
                json!({"type":"string","description":"The `part_id` of a google.gmail.get `attachments` entry"}),
            ),
        ]),
        &["message_id", "part_id"],
    );
    out.push(tool(
        "google.gmail.attachment",
        "Download a Gmail attachment and render it into your view so you can read it — \
         documents, PDFs, spreadsheets, images, audio or video. Pass the `part_id` from a \
         google.gmail.get `attachments` entry. The file appears on your next step; wait for it \
         before describing the contents.",
        attach_schema,
        &ctx,
        handler(|i, a| Box::pin(gmail_attachment(i, a))),
    ));

    // ── thread get ───────────────────────────────────────────────────────────
    let thread_get_schema = object_schema(
        props(&[
            ("thread_id", json!({"type":"string"})),
            (
                "format",
                json!({"type":"string","enum":["full","metadata","minimal"]}),
            ),
        ]),
        &["thread_id"],
    );
    out.push(tool(
        "google.gmail.thread_get",
        "Get a Gmail thread by ID",
        thread_get_schema,
        &ctx,
        handler(|i, a| Box::pin(gmail_thread_get(i, a))),
    ));

    // ── thread modify ────────────────────────────────────────────────────────
    let thread_modify_schema = object_schema(
        props(&[
            ("thread_id", json!({"type":"string"})),
            (
                "add_label_ids",
                json!({"type":"array","items":{"type":"string"}}),
            ),
            (
                "remove_label_ids",
                json!({"type":"array","items":{"type":"string"}}),
            ),
        ]),
        &["thread_id"],
    );
    out.push(tool(
        "google.gmail.thread_modify",
        "Modify labels on an entire thread",
        thread_modify_schema,
        &ctx,
        handler(|i, a| Box::pin(gmail_thread_modify(i, a))),
    ));

    // ── drafts ───────────────────────────────────────────────────────────────
    let drafts_list_schema =
        object_schema(props(&[("max_results", json!({"type":"integer"}))]), &[]);
    out.push(tool(
        "google.gmail.drafts_list",
        "List Gmail drafts",
        drafts_list_schema,
        &ctx,
        handler(|i, a| Box::pin(gmail_drafts_list(i, a))),
    ));

    let draft_get_schema = object_schema(
        props(&[("draft_id", json!({"type":"string"}))]),
        &["draft_id"],
    );
    out.push(tool(
        "google.gmail.draft_get",
        "Get a draft by ID",
        draft_get_schema,
        &ctx,
        handler(|i, a| Box::pin(gmail_draft_get(i, a))),
    ));

    let draft_create_schema = object_schema(
        props(&[
            ("to", json!({"type":"string"})),
            ("subject", json!({"type":"string"})),
            ("body", json!({"type":"string"})),
            ("html_body", json!({"type":"string"})),
            (
                "attach_artifact_ids",
                artifact_attachments_schema("html_body"),
            ),
        ]),
        &["to", "subject"],
    );
    out.push(tool(
        "google.gmail.draft_create",
        "Create a new draft",
        draft_create_schema,
        &ctx,
        handler(|i, a| Box::pin(gmail_draft_create(i, a))),
    ));

    let draft_send_schema = object_schema(
        props(&[("draft_id", json!({"type":"string"}))]),
        &["draft_id"],
    );
    out.push(sending_tool(
        "google.gmail.draft_send",
        "Send an existing draft",
        draft_send_schema,
        &ctx,
        handler(|i, a| Box::pin(gmail_draft_send(i, a))),
        |_| SurfaceEffect::Writes {
            platform: "email".to_string(),
            destination: String::new(),
        },
    ));

    let draft_delete_schema = object_schema(
        props(&[("draft_id", json!({"type":"string"}))]),
        &["draft_id"],
    );
    out.push(tool(
        "google.gmail.draft_delete",
        "Permanently delete a draft",
        draft_delete_schema,
        &ctx,
        handler(|i, a| Box::pin(gmail_draft_delete(i, a))),
    ));

    // ── labels ───────────────────────────────────────────────────────────────
    let labels_list_schema = object_schema(Map::new(), &[]);
    out.push(tool(
        "google.gmail.labels_list",
        "List all Gmail labels",
        labels_list_schema,
        &ctx,
        handler(|i, a| Box::pin(gmail_labels_list(i, a))),
    ));

    let label_get_schema = object_schema(
        props(&[("label_id", json!({"type":"string"}))]),
        &["label_id"],
    );
    out.push(tool(
        "google.gmail.label_get",
        "Get a label by ID",
        label_get_schema,
        &ctx,
        handler(|i, a| Box::pin(gmail_label_get(i, a))),
    ));

    let label_create_schema = object_schema(
        props(&[
            ("name", json!({"type":"string"})),
            (
                "label_list_visibility",
                json!({"type":"string","enum":["labelShow","labelShowIfUnread","labelHide"]}),
            ),
            (
                "message_list_visibility",
                json!({"type":"string","enum":["show","hide"]}),
            ),
        ]),
        &["name"],
    );
    out.push(tool(
        "google.gmail.label_create",
        "Create a new Gmail label",
        label_create_schema,
        &ctx,
        handler(|i, a| Box::pin(gmail_label_create(i, a))),
    ));

    let label_delete_schema = object_schema(
        props(&[("label_id", json!({"type":"string"}))]),
        &["label_id"],
    );
    out.push(tool(
        "google.gmail.label_delete",
        "Delete a Gmail label",
        label_delete_schema,
        &ctx,
        handler(|i, a| Box::pin(gmail_label_delete(i, a))),
    ));

    let label_update_schema = object_schema(
        props(&[
            ("label_id", json!({"type":"string"})),
            ("name", json!({"type":"string"})),
            ("label_list_visibility", json!({"type":"string"})),
            ("message_list_visibility", json!({"type":"string"})),
        ]),
        &["label_id"],
    );
    out.push(tool(
        "google.gmail.label_update",
        "Update a Gmail label",
        label_update_schema,
        &ctx,
        handler(|i, a| Box::pin(gmail_label_update(i, a))),
    ));

    // ── history ──────────────────────────────────────────────────────────────
    let history_schema = object_schema(
        props(&[
            ("start_history_id", json!({"type":"string"})),
            ("max_results", json!({"type":"integer"})),
            ("label_id", json!({"type":"string"})),
        ]),
        &["start_history_id"],
    );
    out.push(tool(
        "google.gmail.history",
        "List mailbox history changes since a history ID",
        history_schema,
        &ctx,
        handler(|i, a| Box::pin(gmail_history(i, a))),
    ));

    // ── settings: filters ────────────────────────────────────────────────────
    out.push(tool(
        "google.gmail.filters_list",
        "List Gmail filters",
        object_schema(Map::new(), &[]),
        &ctx,
        handler(|i, a| Box::pin(gmail_filters_list(i, a))),
    ));

    let filter_create_schema = object_schema(
        props(&[
            (
                "criteria",
                json!({"type":"object","description":"Filter criteria object (from, to, subject, query, etc.)"}),
            ),
            (
                "action",
                json!({"type":"object","description":"Filter action object (addLabelIds, removeLabelIds, forward)"}),
            ),
        ]),
        &["criteria", "action"],
    );
    out.push(tool(
        "google.gmail.filter_create",
        "Create a Gmail filter",
        filter_create_schema,
        &ctx,
        handler(|i, a| Box::pin(gmail_filter_create(i, a))),
    ));

    let filter_delete_schema = object_schema(
        props(&[("filter_id", json!({"type":"string"}))]),
        &["filter_id"],
    );
    out.push(tool(
        "google.gmail.filter_delete",
        "Delete a Gmail filter",
        filter_delete_schema,
        &ctx,
        handler(|i, a| Box::pin(gmail_filter_delete(i, a))),
    ));

    // ── settings: sendAs ─────────────────────────────────────────────────────
    out.push(tool(
        "google.gmail.sendas_list",
        "List send-as aliases",
        object_schema(Map::new(), &[]),
        &ctx,
        handler(|i, a| Box::pin(gmail_sendas_list(i, a))),
    ));

    // ── settings: vacation ───────────────────────────────────────────────────
    out.push(tool(
        "google.gmail.vacation_get",
        "Get vacation/auto-reply settings",
        object_schema(Map::new(), &[]),
        &ctx,
        handler(|i, a| Box::pin(gmail_vacation_get(i, a))),
    ));

    let vacation_set_schema = object_schema(
        props(&[
            ("enable_auto_reply", json!({"type":"boolean"})),
            ("response_subject", json!({"type":"string"})),
            ("response_body_plain_text", json!({"type":"string"})),
            ("response_body_html", json!({"type":"string"})),
            ("restrict_to_contacts", json!({"type":"boolean"})),
            ("restrict_to_domain", json!({"type":"boolean"})),
            (
                "start_time",
                json!({"type":"string","description":"epoch millis"}),
            ),
            (
                "end_time",
                json!({"type":"string","description":"epoch millis"}),
            ),
        ]),
        &["enable_auto_reply"],
    );
    out.push(tool(
        "google.gmail.vacation_set",
        "Update vacation/auto-reply settings",
        vacation_set_schema,
        &ctx,
        handler(|i, a| Box::pin(gmail_vacation_set(i, a))),
    ));

    // ── settings: forwarding ─────────────────────────────────────────────────
    out.push(tool(
        "google.gmail.forwarding_list",
        "List forwarding addresses",
        object_schema(Map::new(), &[]),
        &ctx,
        handler(|i, a| Box::pin(gmail_forwarding_list(i, a))),
    ));

    // ── batch modify ─────────────────────────────────────────────────────────
    let batch_mod_schema = object_schema(
        props(&[
            ("ids", json!({"type":"array","items":{"type":"string"}})),
            (
                "add_label_ids",
                json!({"type":"array","items":{"type":"string"}}),
            ),
            (
                "remove_label_ids",
                json!({"type":"array","items":{"type":"string"}}),
            ),
        ]),
        &["ids"],
    );
    out.push(tool(
        "google.gmail.batch_modify",
        "Modify labels on multiple messages at once",
        batch_mod_schema,
        &ctx,
        handler(|i, a| Box::pin(gmail_batch_modify(i, a))),
    ));

    out
}

fn ids_from_array(input: &Value, key: &str) -> Vec<String> {
    input
        .get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Gmail's per-message cap, applied to staged attachments.
const MAX_GMAIL_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;

fn staged_parts(input: &Value) -> Result<Vec<MailPart>> {
    resolve_artifact_attachments(input, MAX_GMAIL_ATTACHMENT_BYTES, "Gmail's 25 MB limit")
        .map(|staged| staged.into_iter().map(MailPart::from).collect())
        .map_err(|e| eyre::eyre!(e))
}

fn build_rfc2822(input: &Value, parts: &[MailPart]) -> String {
    let to = optional_str(input, "to").unwrap_or("");
    let subject = optional_str(input, "subject").unwrap_or("");
    let cc = optional_str(input, "cc").unwrap_or("");
    let bcc = optional_str(input, "bcc").unwrap_or("");
    let in_reply_to = optional_str(input, "in_reply_to").unwrap_or("");
    let html_body = optional_str(input, "html_body");
    let plain_body = optional_str(input, "body").unwrap_or("");

    let mut headers = String::new();
    headers.push_str(&format!("To: {to}\r\n"));
    headers.push_str(&format!("Subject: {subject}\r\n"));
    if !cc.is_empty() {
        headers.push_str(&format!("Cc: {cc}\r\n"));
    }
    if !bcc.is_empty() {
        headers.push_str(&format!("Bcc: {bcc}\r\n"));
    }
    if !in_reply_to.is_empty() {
        headers.push_str(&format!("In-Reply-To: {in_reply_to}\r\n"));
        headers.push_str(&format!("References: {in_reply_to}\r\n"));
    }

    headers.push_str("MIME-Version: 1.0\r\n");
    if !parts.is_empty() {
        let body = assemble(plain_body, html_body, parts).formatted();
        headers.push_str(&String::from_utf8_lossy(&body));
    } else if let Some(html) = html_body {
        headers.push_str("Content-Type: text/html; charset=UTF-8\r\n");
        headers.push_str("\r\n");
        headers.push_str(html);
    } else {
        headers.push_str("Content-Type: text/plain; charset=UTF-8\r\n");
        headers.push_str("\r\n");
        headers.push_str(plain_body);
    }
    headers
}

fn encode_raw(raw: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw.as_bytes())
}

async fn gmail_search(input: Value, api: &GoogleApi) -> Result<Value> {
    let q = required_str(&input, "q")?;
    let max = optional_u64(&input, "max_results").unwrap_or(20).min(500);
    let mut query: Vec<(String, String)> = vec![
        ("q".to_string(), q.to_string()),
        ("maxResults".to_string(), max.to_string()),
    ];
    if let Some(pt) = optional_str(&input, "page_token") {
        query.push(("pageToken".to_string(), pt.to_string()));
    }
    let label_ids = ids_from_array(&input, "label_ids");
    for lid in &label_ids {
        query.push(("labelIds".to_string(), lid.clone()));
    }
    let url = format!("{BASE}/messages");
    let list: Value = api.get_json_with_query(&url, Some(&query)).await?;

    // Fetch each message in metadata format for useful snippets.
    let msg_ids: Vec<String> = list
        .get("messages")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let mut messages = Vec::new();
    for mid in msg_ids.iter().take(max as usize) {
        let url = format!("{BASE}/messages/{id}?format=metadata&metadataHeaders=From&metadataHeaders=To&metadataHeaders=Subject&metadataHeaders=Date", id = urlencode(mid));
        let msg: Value = api.get_json(&url).await?;
        messages.push(msg);
    }

    Ok(json!({
        "resultSizeEstimate": list.get("resultSizeEstimate"),
        "nextPageToken": list.get("nextPageToken"),
        "messages": messages
    }))
}

async fn gmail_get(input: Value, api: &GoogleApi) -> Result<Value> {
    let id = required_str(&input, "message_id")?;
    let format = optional_str(&input, "format").unwrap_or("full");
    let url = format!("{BASE}/messages/{id}?format={format}", id = urlencode(id));
    Ok(with_attachment_listing(api.get_json(&url).await?))
}

/// Add a top-level `attachments` summary to a message that has a `payload`, so
/// the agent finds each file's `part_id` without walking the part tree.
fn with_attachment_listing(mut message: Value) -> Value {
    let attachments = message.get("payload").map(attachment_parts);
    if let (Some(attachments), Some(fields)) = (attachments, message.as_object_mut()) {
        fields.insert("attachments".to_string(), Value::Array(attachments));
    }
    message
}

/// `payload` and every nested part, depth-first.
fn flatten_parts(payload: &Value) -> Vec<&Value> {
    std::iter::once(payload)
        .chain(
            payload["parts"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(flatten_parts),
        )
        .collect()
}

fn part_id(part: &Value) -> &str {
    part["partId"].as_str().unwrap_or_default()
}

/// The part whose `partId` is `id`; the root payload's is `""`. Unlike
/// `attachmentId`, which changes on every fetch, `partId` is stable.
fn find_part<'a>(payload: &'a Value, id: &str) -> Option<&'a Value> {
    flatten_parts(payload)
        .into_iter()
        .find(|p| part_id(p) == id)
}

/// The named file parts of a message payload.
fn attachment_parts(payload: &Value) -> Vec<Value> {
    flatten_parts(payload)
        .into_iter()
        .filter(|p| p["filename"].as_str().is_some_and(|f| !f.is_empty()))
        .map(|p| {
            json!({
                "part_id": part_id(p),
                "filename": p["filename"],
                "mime_type": p["mimeType"],
                "size": p["body"]["size"],
            })
        })
        .collect()
}

fn decode_gmail_base64(data: &str) -> Result<Vec<u8>> {
    GMAIL_BASE64
        .decode(data.trim())
        .map_err(|e| eyre::eyre!("attachment data is not valid base64url: {e}"))
}

async fn gmail_send(input: Value, api: &GoogleApi) -> Result<Value> {
    let raw = build_rfc2822(&input, &staged_parts(&input)?);
    let encoded = encode_raw(&raw);
    let mut body = json!({ "raw": encoded });
    if let Some(tid) = optional_str(&input, "thread_id") {
        body["threadId"] = json!(tid);
    }
    let url = format!("{BASE}/messages/send");
    api.post_json(&url, &body).await
}

async fn gmail_modify(input: Value, api: &GoogleApi) -> Result<Value> {
    let id = required_str(&input, "message_id")?;
    let body = json!({
        "addLabelIds": ids_from_array(&input, "add_label_ids"),
        "removeLabelIds": ids_from_array(&input, "remove_label_ids"),
    });
    let url = format!("{BASE}/messages/{id}/modify", id = urlencode(id));
    api.post_json(&url, &body).await
}

async fn gmail_trash(input: Value, api: &GoogleApi) -> Result<Value> {
    let id = required_str(&input, "message_id")?;
    let url = format!("{BASE}/messages/{id}/trash", id = urlencode(id));
    api.post_json(&url, &json!({})).await
}

async fn gmail_untrash(input: Value, api: &GoogleApi) -> Result<Value> {
    let id = required_str(&input, "message_id")?;
    let url = format!("{BASE}/messages/{id}/untrash", id = urlencode(id));
    api.post_json(&url, &json!({})).await
}

async fn gmail_archive(input: Value, api: &GoogleApi) -> Result<Value> {
    let id = required_str(&input, "message_id")?;
    let body = json!({ "removeLabelIds": ["INBOX"] });
    let url = format!("{BASE}/messages/{id}/modify", id = urlencode(id));
    api.post_json(&url, &body).await
}

async fn gmail_mark_read(input: Value, api: &GoogleApi) -> Result<Value> {
    let id = required_str(&input, "message_id")?;
    let body = json!({ "removeLabelIds": ["UNREAD"] });
    let url = format!("{BASE}/messages/{id}/modify", id = urlencode(id));
    api.post_json(&url, &body).await
}

async fn gmail_mark_unread(input: Value, api: &GoogleApi) -> Result<Value> {
    let id = required_str(&input, "message_id")?;
    let body = json!({ "addLabelIds": ["UNREAD"] });
    let url = format!("{BASE}/messages/{id}/modify", id = urlencode(id));
    api.post_json(&url, &body).await
}

async fn gmail_attachment(input: Value, api: &GoogleApi) -> Result<Value> {
    let msg_id = required_str(&input, "message_id")?;
    let part_id = input
        .get("part_id")
        .and_then(coerce_to_id_string)
        .ok_or_else(|| eyre::eyre!("missing required field `part_id`"))?;
    let mid = urlencode(msg_id);
    let message: Value = api
        .get_json(&format!("{BASE}/messages/{mid}?format=full"))
        .await?;
    let payload = &message["payload"];
    let part = find_part(payload, &part_id).ok_or_else(|| {
        let available: Vec<String> = attachment_parts(payload)
            .iter()
            .map(|a| {
                format!(
                    "'{}' ({})",
                    a["part_id"].as_str().unwrap_or_default(),
                    a["filename"].as_str().unwrap_or_default()
                )
            })
            .collect();
        if available.is_empty() {
            eyre::eyre!("part `{part_id}` not found — message {msg_id} has no attachments")
        } else {
            eyre::eyre!(
                "part `{part_id}` not found in message {msg_id} — available parts: {}",
                available.join(", ")
            )
        }
    })?;
    let filename = part["filename"]
        .as_str()
        .filter(|f| !f.is_empty())
        .unwrap_or("attachment");
    let media_type = part["mimeType"]
        .as_str()
        .filter(|m| !m.is_empty())
        .unwrap_or("application/octet-stream");
    let body = &part["body"];
    let size = coerce_to_u64(&body["size"]).unwrap_or(0);
    eyre::ensure!(
        within_view_cap(size),
        "attachment `{filename}` is {size} bytes, over the {MAX_VIEW_BYTES}-byte limit"
    );

    let inline_data = body["data"].as_str().filter(|d| !d.is_empty());
    let data = match (inline_data, body["attachmentId"].as_str()) {
        (Some(data), _) => data.to_string(),
        (None, Some(aid)) => {
            let url = format!(
                "{BASE}/messages/{mid}/attachments/{aid}",
                aid = urlencode(aid)
            );
            let attachment: Value = api.get_json(&url).await?;
            attachment["data"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("attachment `{filename}` came back without data"))?
                .to_string()
        }
        (None, None) => eyre::bail!("part `{part_id}` of message {msg_id} has no file body"),
    };
    let bytes = decode_gmail_base64(&data)?;
    eyre::ensure!(
        within_view_cap(bytes.len() as u64),
        "attachment `{filename}` exceeds the {MAX_VIEW_BYTES}-byte limit"
    );

    let staged = stage_for_view("google.gmail.attachment", bytes, media_type, filename)
        .map_err(|e| eyre::eyre!(e))?;
    Ok(json!({
        VIEWED_IMAGES_KEY: staged.marker,
        "note": view_pending_note(filename),
    }))
}

async fn gmail_thread_get(input: Value, api: &GoogleApi) -> Result<Value> {
    let id = required_str(&input, "thread_id")?;
    let format = optional_str(&input, "format").unwrap_or("full");
    let url = format!("{BASE}/threads/{id}?format={format}", id = urlencode(id));
    api.get_json(&url).await
}

async fn gmail_thread_modify(input: Value, api: &GoogleApi) -> Result<Value> {
    let id = required_str(&input, "thread_id")?;
    let body = json!({
        "addLabelIds": ids_from_array(&input, "add_label_ids"),
        "removeLabelIds": ids_from_array(&input, "remove_label_ids"),
    });
    let url = format!("{BASE}/threads/{id}/modify", id = urlencode(id));
    api.post_json(&url, &body).await
}

// ── Drafts ───────────────────────────────────────────────────────────────────

async fn gmail_drafts_list(input: Value, api: &GoogleApi) -> Result<Value> {
    let max = optional_u64(&input, "max_results").unwrap_or(20);
    let url = format!("{BASE}/drafts?maxResults={max}");
    api.get_json(&url).await
}

async fn gmail_draft_get(input: Value, api: &GoogleApi) -> Result<Value> {
    let id = required_str(&input, "draft_id")?;
    let url = format!("{BASE}/drafts/{id}", id = urlencode(id));
    api.get_json(&url).await
}

async fn gmail_draft_create(input: Value, api: &GoogleApi) -> Result<Value> {
    let raw = build_rfc2822(&input, &staged_parts(&input)?);
    let encoded = encode_raw(&raw);
    let body = json!({ "message": { "raw": encoded } });
    let url = format!("{BASE}/drafts");
    api.post_json(&url, &body).await
}

async fn gmail_draft_send(input: Value, api: &GoogleApi) -> Result<Value> {
    let id = required_str(&input, "draft_id")?;
    let body = json!({ "id": id });
    let url = format!("{BASE}/drafts/send");
    api.post_json(&url, &body).await
}

async fn gmail_draft_delete(input: Value, api: &GoogleApi) -> Result<Value> {
    let id = required_str(&input, "draft_id")?;
    let url = format!("{BASE}/drafts/{id}", id = urlencode(id));
    api.delete(&url).await?;
    Ok(json!({"deleted": true, "draft_id": id}))
}

// ── Labels ───────────────────────────────────────────────────────────────────

async fn gmail_labels_list(_input: Value, api: &GoogleApi) -> Result<Value> {
    let url = format!("{BASE}/labels");
    api.get_json(&url).await
}

async fn gmail_label_get(input: Value, api: &GoogleApi) -> Result<Value> {
    let id = required_str(&input, "label_id")?;
    let url = format!("{BASE}/labels/{id}", id = urlencode(id));
    api.get_json(&url).await
}

async fn gmail_label_create(input: Value, api: &GoogleApi) -> Result<Value> {
    let name = required_str(&input, "name")?;
    let mut body = json!({ "name": name });
    if let Some(v) = optional_str(&input, "label_list_visibility") {
        body["labelListVisibility"] = json!(v);
    }
    if let Some(v) = optional_str(&input, "message_list_visibility") {
        body["messageListVisibility"] = json!(v);
    }
    let url = format!("{BASE}/labels");
    api.post_json(&url, &body).await
}

async fn gmail_label_delete(input: Value, api: &GoogleApi) -> Result<Value> {
    let id = required_str(&input, "label_id")?;
    let url = format!("{BASE}/labels/{id}", id = urlencode(id));
    api.delete(&url).await?;
    Ok(json!({"deleted": true, "label_id": id}))
}

async fn gmail_label_update(input: Value, api: &GoogleApi) -> Result<Value> {
    let id = required_str(&input, "label_id")?;
    let mut body = Map::new();
    if let Some(n) = optional_str(&input, "name") {
        body.insert("name".into(), json!(n));
    }
    if let Some(v) = optional_str(&input, "label_list_visibility") {
        body.insert("labelListVisibility".into(), json!(v));
    }
    if let Some(v) = optional_str(&input, "message_list_visibility") {
        body.insert("messageListVisibility".into(), json!(v));
    }
    let url = format!("{BASE}/labels/{id}", id = urlencode(id));
    api.patch_json(&url, &Value::Object(body)).await
}

// ── History ──────────────────────────────────────────────────────────────────

async fn gmail_history(input: Value, api: &GoogleApi) -> Result<Value> {
    let start = required_str(&input, "start_history_id")?;
    let max = optional_u64(&input, "max_results").unwrap_or(100);
    let mut query: Vec<(String, String)> = vec![
        ("startHistoryId".to_string(), start.to_string()),
        ("maxResults".to_string(), max.to_string()),
    ];
    if let Some(lid) = optional_str(&input, "label_id") {
        query.push(("labelId".to_string(), lid.to_string()));
    }
    let url = format!("{BASE}/history");
    api.get_json_with_query(&url, Some(&query)).await
}

// ── Filters ──────────────────────────────────────────────────────────────────

async fn gmail_filters_list(_input: Value, api: &GoogleApi) -> Result<Value> {
    let url = format!("{BASE}/settings/filters");
    api.get_json(&url).await
}

async fn gmail_filter_create(input: Value, api: &GoogleApi) -> Result<Value> {
    let criteria = input
        .get("criteria")
        .cloned()
        .ok_or_else(|| eyre::eyre!("criteria is required"))?;
    let action = input
        .get("action")
        .cloned()
        .ok_or_else(|| eyre::eyre!("action is required"))?;
    let body = json!({ "criteria": criteria, "action": action });
    let url = format!("{BASE}/settings/filters");
    api.post_json(&url, &body).await
}

async fn gmail_filter_delete(input: Value, api: &GoogleApi) -> Result<Value> {
    let id = required_str(&input, "filter_id")?;
    let url = format!("{BASE}/settings/filters/{id}", id = urlencode(id));
    api.delete(&url).await?;
    Ok(json!({"deleted": true, "filter_id": id}))
}

// ── SendAs ───────────────────────────────────────────────────────────────────

async fn gmail_sendas_list(_input: Value, api: &GoogleApi) -> Result<Value> {
    let url = format!("{BASE}/settings/sendAs");
    api.get_json(&url).await
}

// ── Vacation ─────────────────────────────────────────────────────────────────

async fn gmail_vacation_get(_input: Value, api: &GoogleApi) -> Result<Value> {
    let url = format!("{BASE}/settings/vacation");
    api.get_json(&url).await
}

async fn gmail_vacation_set(input: Value, api: &GoogleApi) -> Result<Value> {
    let mut body = Map::new();
    if let Some(v) = input.get("enable_auto_reply").and_then(|v| v.as_bool()) {
        body.insert("enableAutoReply".into(), json!(v));
    }
    if let Some(v) = optional_str(&input, "response_subject") {
        body.insert("responseSubject".into(), json!(v));
    }
    if let Some(v) = optional_str(&input, "response_body_plain_text") {
        body.insert("responseBodyPlainText".into(), json!(v));
    }
    if let Some(v) = optional_str(&input, "response_body_html") {
        body.insert("responseBodyHtml".into(), json!(v));
    }
    if let Some(v) = input.get("restrict_to_contacts").and_then(|b| b.as_bool()) {
        body.insert("restrictToContacts".into(), json!(v));
    }
    if let Some(v) = input.get("restrict_to_domain").and_then(|b| b.as_bool()) {
        body.insert("restrictToDomain".into(), json!(v));
    }
    if let Some(v) = optional_str(&input, "start_time") {
        body.insert("startTime".into(), json!(v));
    }
    if let Some(v) = optional_str(&input, "end_time") {
        body.insert("endTime".into(), json!(v));
    }
    let url = format!("{BASE}/settings/vacation");
    api.put_json(&url, &Value::Object(body)).await
}

// ── Forwarding ───────────────────────────────────────────────────────────────

async fn gmail_forwarding_list(_input: Value, api: &GoogleApi) -> Result<Value> {
    let url = format!("{BASE}/settings/forwardingAddresses");
    api.get_json(&url).await
}

// ── Batch ops ────────────────────────────────────────────────────────────────

async fn gmail_batch_modify(input: Value, api: &GoogleApi) -> Result<Value> {
    let ids = ids_from_array(&input, "ids");
    if ids.is_empty() {
        return Ok(json!({"modified": 0}));
    }
    let body = json!({
        "ids": ids,
        "addLabelIds": ids_from_array(&input, "add_label_ids"),
        "removeLabelIds": ids_from_array(&input, "remove_label_ids"),
    });
    let url = format!("{BASE}/messages/batchModify");
    api.post_json(&url, &body).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> Value {
        json!({
            "partId": "",
            "mimeType": "multipart/mixed",
            "filename": "",
            "body": { "size": 0 },
            "parts": [
                {
                    "partId": "0",
                    "mimeType": "multipart/alternative",
                    "filename": "",
                    "body": { "size": 0 },
                    "parts": [
                        { "partId": "0.0", "mimeType": "text/plain", "filename": "", "body": { "size": 5, "data": "aGVsbG8" } },
                        { "partId": "0.1", "mimeType": "text/html", "filename": "", "body": { "size": 12, "data": "PHA-aGk8L3A-" } }
                    ]
                },
                {
                    "partId": "1",
                    "mimeType": "application/pdf",
                    "filename": "spec.pdf",
                    "body": { "size": 2048, "attachmentId": "ANGjdJ8" }
                },
                {
                    "partId": "2",
                    "mimeType": "image/png",
                    "filename": "logo.png",
                    "body": { "size": 3, "data": "AAEC" }
                }
            ]
        })
    }

    #[test]
    fn find_part_matches_part_id_at_any_depth() {
        let payload = payload();
        assert_eq!(find_part(&payload, "0.1").unwrap()["mimeType"], "text/html");
        assert_eq!(find_part(&payload, "1").unwrap()["filename"], "spec.pdf");
        assert_eq!(
            find_part(&payload, "").unwrap()["mimeType"],
            "multipart/mixed",
            "the root payload's partId is empty"
        );
        assert!(find_part(&payload, "7").is_none());
        assert!(
            find_part(&payload, "ANGjdJ8").is_none(),
            "attachmentId is not an address"
        );
    }

    #[test]
    fn attachment_parts_lists_named_parts_only() {
        assert_eq!(
            attachment_parts(&payload()),
            vec![
                json!({ "part_id": "1", "filename": "spec.pdf", "mime_type": "application/pdf", "size": 2048 }),
                json!({ "part_id": "2", "filename": "logo.png", "mime_type": "image/png", "size": 3 }),
            ]
        );
    }

    #[test]
    fn with_attachment_listing_adds_listing_only_when_payload_present() {
        let full = with_attachment_listing(json!({ "id": "m1", "payload": payload() }));
        assert_eq!(full["id"], "m1");
        assert_eq!(full["attachments"].as_array().map(Vec::len), Some(2));
        let minimal = with_attachment_listing(json!({ "id": "m1", "raw": "..." }));
        assert!(minimal.get("attachments").is_none());
    }

    #[test]
    fn rfc2822_without_parts_is_a_single_html_part() {
        let raw = build_rfc2822(
            &json!({ "to": "a@b.c", "subject": "s", "html_body": "<p>hi</p>" }),
            &[],
        );
        assert!(raw.starts_with(
            "To: a@b.c\r\nSubject: s\r\nMIME-Version: 1.0\r\nContent-Type: text/html"
        ));
        assert!(raw.ends_with("<p>hi</p>"));
    }

    #[test]
    fn rfc2822_with_an_inline_part_is_multipart_and_references_the_cid() {
        let part = MailPart {
            filename: "q3.png".into(),
            mime_type: "image/png".into(),
            data: b"\x89PNG".to_vec(),
            content_id: Some("q3".into()),
        };
        let raw = build_rfc2822(
            &json!({ "to": "a@b.c", "subject": "s", "body": "hi", "html_body": "<p>hi</p>" }),
            &[part],
        );
        assert!(
            raw.contains("MIME-Version: 1.0\r\nContent-Type: multipart/related;"),
            "{raw}"
        );
        assert!(
            raw.contains("Content-ID: <q3>") && raw.contains("cid:q3"),
            "{raw}"
        );
    }

    #[test]
    fn decode_gmail_base64_accepts_padded_and_unpadded_url_safe() {
        assert_eq!(
            decode_gmail_base64("AAEC_-8").unwrap(),
            vec![0, 1, 2, 255, 239]
        );
        assert_eq!(
            decode_gmail_base64("AAEC_-8=").unwrap(),
            vec![0, 1, 2, 255, 239]
        );
        assert_eq!(decode_gmail_base64("aGVsbG8").unwrap(), b"hello");
        assert_eq!(decode_gmail_base64("aGVsbG8=\n").unwrap(), b"hello");
        assert!(decode_gmail_base64("not base64!").is_err());
    }
}
