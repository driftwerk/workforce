//! Google Chat v1 tools.
//! Tool names: `google.chat.*`.
//! Base: <https://chat.googleapis.com/v1/>.

use std::sync::Arc;

use eyre::{Result, WrapErr};
use serde_json::{json, Map, Value};
use workforce_agent::tools::{SurfaceEffect, Tool};

use crate::adapters::google::api::{GoogleApi, GoogleApiContext};

use crate::adapters::google::tool_helpers::{
    handler, object_schema, optional_str, optional_u64, props, required_str, sending_tool, tool,
    urlencode,
};

const BASE: &str = "https://chat.googleapis.com/v1";

/// A write to Google Chat, at what `field` of the call names.
fn chat_write(input: &Value, field: &str) -> SurfaceEffect {
    SurfaceEffect::Writes {
        platform: "google_chat".to_string(),
        destination: input[field].as_str().unwrap_or_default().to_string(),
    }
}

pub fn tools(ctx: Arc<GoogleApiContext>) -> Vec<Arc<dyn Tool>> {
    let mut out: Vec<Arc<dyn Tool>> = Vec::new();

    // ── spaces list ──────────────────────────────────────────────────────────
    out.push(tool(
        "google.chat.spaces_list",
        "List Google Chat spaces",
        object_schema(Map::new(), &[]),
        &ctx,
        handler(|i, a| Box::pin(spaces_list(i, a))),
    ));

    // ── spaces create ────────────────────────────────────────────────────────
    let create_schema = object_schema(
        props(&[
            ("display_name", json!({"type":"string"})),
            (
                "space_type",
                json!({"type":"string","enum":["SPACE","GROUP_CHAT"]}),
            ),
        ]),
        &["display_name"],
    );
    out.push(tool(
        "google.chat.spaces_create",
        "Create a Google Chat space",
        create_schema,
        &ctx,
        handler(|i, a| Box::pin(spaces_create(i, a))),
    ));

    // ── spaces find ──────────────────────────────────────────────────────────
    let find_schema = object_schema(
        props(&[(
            "query",
            json!({"type":"string","description":"Space name to search for"}),
        )]),
        &["query"],
    );
    out.push(tool(
        "google.chat.spaces_find",
        "Find a Chat space by name",
        find_schema,
        &ctx,
        handler(|i, a| Box::pin(spaces_find(i, a))),
    ));

    // ── messages list ────────────────────────────────────────────────────────
    let msg_list_schema = object_schema(
        props(&[
            (
                "space",
                json!({"type":"string","description":"Space resource name, e.g. 'spaces/AAAA'"}),
            ),
            ("max_results", json!({"type":"integer"})),
            ("page_token", json!({"type":"string"})),
        ]),
        &["space"],
    );
    out.push(tool(
        "google.chat.messages_list",
        "List messages in a Chat space",
        msg_list_schema,
        &ctx,
        handler(|i, a| Box::pin(messages_list(i, a))),
    ));

    // ── messages send ────────────────────────────────────────────────────────
    let send_schema = object_schema(
        props(&[
            ("space", json!({"type":"string"})),
            (
                "text",
                json!({"type":"string","description":"Plain text content"}),
            ),
            (
                "thread_key",
                json!({"type":"string","description":"Thread key to reply in a thread"}),
            ),
        ]),
        &["space", "text"],
    );
    out.push(sending_tool(
        "google.chat.messages_send",
        "Send a message in a Chat space",
        send_schema,
        &ctx,
        handler(|i, a| Box::pin(messages_send(i, a))),
        |input| chat_write(input, "space"),
    ));

    // ── dm send ──────────────────────────────────────────────────────────────
    let dm_schema = object_schema(
        props(&[
            (
                "user",
                json!({"type":"string","description":"User resource name (people/xxx or users/xxx)"}),
            ),
            ("text", json!({"type":"string"})),
        ]),
        &["user", "text"],
    );
    out.push(sending_tool(
        "google.chat.dm_send",
        "Send a direct message to a user",
        dm_schema,
        &ctx,
        handler(|i, a| Box::pin(dm_send(i, a))),
        |input| SurfaceEffect::Writes {
            platform: "google_chat".to_string(),
            destination: format!("@{}", input["user"].as_str().unwrap_or_default()),
        },
    ));

    // ── reactions create ─────────────────────────────────────────────────────
    let react_schema = object_schema(
        props(&[
            (
                "message",
                json!({"type":"string","description":"Message resource name"}),
            ),
            (
                "emoji",
                json!({"type":"string","description":"Unicode emoji character"}),
            ),
        ]),
        &["message", "emoji"],
    );
    out.push(sending_tool(
        "google.chat.reactions_create",
        "Add a reaction to a message",
        react_schema,
        &ctx,
        handler(|i, a| Box::pin(reactions_create(i, a))),
        |input| chat_write(input, "message"),
    ));

    // ── reactions list ───────────────────────────────────────────────────────
    let react_list_schema = object_schema(
        props(&[("message", json!({"type":"string"}))]),
        &["message"],
    );
    out.push(tool(
        "google.chat.reactions_list",
        "List reactions on a message",
        react_list_schema,
        &ctx,
        handler(|i, a| Box::pin(reactions_list(i, a))),
    ));

    // ── reactions delete ─────────────────────────────────────────────────────
    let react_del_schema = object_schema(
        props(&[(
            "reaction",
            json!({"type":"string","description":"Reaction resource name"}),
        )]),
        &["reaction"],
    );
    out.push(sending_tool(
        "google.chat.reactions_delete",
        "Remove a reaction",
        react_del_schema,
        &ctx,
        handler(|i, a| Box::pin(reactions_delete(i, a))),
        |input| chat_write(input, "reaction"),
    ));

    // ── threads list ─────────────────────────────────────────────────────────
    let threads_schema = object_schema(
        props(&[
            ("space", json!({"type":"string"})),
            ("max_results", json!({"type":"integer"})),
        ]),
        &["space"],
    );
    out.push(tool(
        "google.chat.threads_list",
        "List threads in a Chat space",
        threads_schema,
        &ctx,
        handler(|i, a| Box::pin(threads_list(i, a))),
    ));

    out
}

async fn spaces_list(_input: Value, api: &GoogleApi) -> Result<Value> {
    api.get_json(&format!("{BASE}/spaces")).await
}

async fn spaces_create(input: Value, api: &GoogleApi) -> Result<Value> {
    let name = required_str(&input, "display_name")?;
    let space_type = optional_str(&input, "space_type").unwrap_or("SPACE");
    let body = json!({
        "displayName": name,
        "spaceType": space_type,
    });
    api.post_json(&format!("{BASE}/spaces"), &body).await
}

async fn spaces_find(input: Value, api: &GoogleApi) -> Result<Value> {
    let q = required_str(&input, "query")?;
    let query: Vec<(String, String)> = vec![("filter".to_string(), format!("displayName=\"{q}\""))];
    let url = format!("{BASE}/spaces:search");
    api.get_json_with_query(&url, Some(&query)).await
}

async fn messages_list(input: Value, api: &GoogleApi) -> Result<Value> {
    let space = required_str(&input, "space")?;
    let max = optional_u64(&input, "max_results").unwrap_or(25);
    let mut query: Vec<(String, String)> = vec![("pageSize".to_string(), max.to_string())];
    if let Some(pt) = optional_str(&input, "page_token") {
        query.push(("pageToken".to_string(), pt.to_string()));
    }
    let url = format!("{BASE}/{space}/messages", space = urlencode(space));
    api.get_json_with_query(&url, Some(&query)).await
}

async fn messages_send(input: Value, api: &GoogleApi) -> Result<Value> {
    let space = required_str(&input, "space")?;
    let text = required_str(&input, "text")?;
    let mut body = json!({"text": text});
    if let Some(tk) = optional_str(&input, "thread_key") {
        body["thread"] = json!({"threadKey": tk});
    }
    let url = format!("{BASE}/{space}/messages", space = urlencode(space));
    api.post_json(&url, &body).await
}

async fn dm_send(input: Value, api: &GoogleApi) -> Result<Value> {
    let user = required_str(&input, "user")?;
    let text = required_str(&input, "text")?;
    // Set up DM space first.
    let dm_url = format!("{BASE}/spaces:setup");
    let dm_setup_body = json!({
        "spaceType": "DIRECT_MESSAGE",
        "memberships": [{"member": {"name": user, "type": "HUMAN"}}],
    });
    let space: Value = api.post_json(&dm_url, &dm_setup_body).await.wrap_err(
        "DM setup failed — ensure the user resource name is correct (e.g., 'users/1234')",
    )?;
    let space_name = space.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let body = json!({"text": text});
    let url = format!("{BASE}/{space}/messages", space = urlencode(space_name));
    api.post_json(&url, &body).await
}

async fn reactions_create(input: Value, api: &GoogleApi) -> Result<Value> {
    let msg = required_str(&input, "message")?;
    let emoji = required_str(&input, "emoji")?;
    let body = json!({
        "emoji": {"unicode": emoji}
    });
    let url = format!("{BASE}/{msg}/reactions", msg = urlencode(msg));
    api.post_json(&url, &body).await
}

async fn reactions_list(input: Value, api: &GoogleApi) -> Result<Value> {
    let msg = required_str(&input, "message")?;
    let url = format!("{BASE}/{msg}/reactions", msg = urlencode(msg));
    api.get_json(&url).await
}

async fn reactions_delete(input: Value, api: &GoogleApi) -> Result<Value> {
    let rn = required_str(&input, "reaction")?;
    let url = format!("{BASE}/{rn}", rn = urlencode(rn));
    api.delete(&url).await?;
    Ok(json!({"deleted": true}))
}

async fn threads_list(input: Value, api: &GoogleApi) -> Result<Value> {
    // Chat API doesn't have a direct "threads list" — use messages list with
    // thread grouping. Best approximation: list messages and group.
    let space = required_str(&input, "space")?;
    let max = optional_u64(&input, "max_results").unwrap_or(50);
    let query: Vec<(String, String)> = vec![("pageSize".to_string(), max.to_string())];
    let url = format!("{BASE}/{space}/messages", space = urlencode(space));
    api.get_json_with_query(&url, Some(&query)).await
}
