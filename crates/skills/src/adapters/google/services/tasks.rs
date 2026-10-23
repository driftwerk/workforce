//! Google Tasks v1 tools.
//! Tool names: `google.tasks.*`.
//! Base: <https://tasks.googleapis.com/tasks/v1/>.

use std::sync::Arc;

use eyre::Result;
use serde_json::{json, Map, Value};
use workforce_agent::tools::Tool;

use crate::adapters::google::api::{GoogleApi, GoogleApiContext};

use crate::adapters::google::tool_helpers::{
    handler, object_schema, optional_str, optional_u64, props, required_str, tool, urlencode,
};

const BASE: &str = "https://tasks.googleapis.com/tasks/v1";

pub fn tools(ctx: Arc<GoogleApiContext>) -> Vec<Arc<dyn Tool>> {
    let mut out: Vec<Arc<dyn Tool>> = Vec::new();

    // ── task lists ───────────────────────────────────────────────────────────
    out.push(tool(
        "google.tasks.lists",
        "List all task lists",
        object_schema(Map::new(), &[]),
        &ctx,
        handler(|i, a| Box::pin(task_lists(i, a))),
    ));

    let list_create_schema =
        object_schema(props(&[("title", json!({"type":"string"}))]), &["title"]);
    out.push(tool(
        "google.tasks.lists_create",
        "Create a new task list",
        list_create_schema,
        &ctx,
        handler(|i, a| Box::pin(task_lists_create(i, a))),
    ));

    // ── tasks ────────────────────────────────────────────────────────────────
    let list_schema = object_schema(
        props(&[
            (
                "tasklist",
                json!({"type":"string","description":"Task list ID (default '@default')"}),
            ),
            ("max_results", json!({"type":"integer"})),
            ("show_completed", json!({"type":"boolean"})),
            ("show_hidden", json!({"type":"boolean"})),
        ]),
        &[],
    );
    out.push(tool(
        "google.tasks.list",
        "List tasks in a task list",
        list_schema,
        &ctx,
        handler(|i, a| Box::pin(task_list(i, a))),
    ));

    let get_schema = object_schema(
        props(&[
            ("tasklist", json!({"type":"string"})),
            ("task_id", json!({"type":"string"})),
        ]),
        &["task_id"],
    );
    out.push(tool(
        "google.tasks.get",
        "Get a task by ID",
        get_schema,
        &ctx,
        handler(|i, a| Box::pin(task_get(i, a))),
    ));

    let add_schema = object_schema(
        props(&[
            ("tasklist", json!({"type":"string"})),
            ("title", json!({"type":"string"})),
            ("notes", json!({"type":"string"})),
            (
                "due",
                json!({"type":"string","description":"RFC3339 due date"}),
            ),
            (
                "parent",
                json!({"type":"string","description":"Parent task ID for subtask"}),
            ),
        ]),
        &["title"],
    );
    out.push(tool(
        "google.tasks.add",
        "Create a new task",
        add_schema,
        &ctx,
        handler(|i, a| Box::pin(task_add(i, a))),
    ));

    let update_schema = object_schema(
        props(&[
            ("tasklist", json!({"type":"string"})),
            ("task_id", json!({"type":"string"})),
            ("title", json!({"type":"string"})),
            ("notes", json!({"type":"string"})),
            ("due", json!({"type":"string"})),
            (
                "status",
                json!({"type":"string","enum":["needsAction","completed"]}),
            ),
        ]),
        &["task_id"],
    );
    out.push(tool(
        "google.tasks.update",
        "Update a task",
        update_schema,
        &ctx,
        handler(|i, a| Box::pin(task_update(i, a))),
    ));

    let del_schema = object_schema(
        props(&[
            ("tasklist", json!({"type":"string"})),
            ("task_id", json!({"type":"string"})),
        ]),
        &["task_id"],
    );
    out.push(tool(
        "google.tasks.delete",
        "Delete a task",
        del_schema.clone(),
        &ctx,
        handler(|i, a| Box::pin(task_delete(i, a))),
    ));

    let done_schema = object_schema(
        props(&[
            ("tasklist", json!({"type":"string"})),
            ("task_id", json!({"type":"string"})),
        ]),
        &["task_id"],
    );
    out.push(tool(
        "google.tasks.done",
        "Mark a task as completed",
        done_schema,
        &ctx,
        handler(|i, a| Box::pin(task_done(i, a))),
    ));

    let undo_schema = object_schema(
        props(&[
            ("tasklist", json!({"type":"string"})),
            ("task_id", json!({"type":"string"})),
        ]),
        &["task_id"],
    );
    out.push(tool(
        "google.tasks.undo",
        "Mark a completed task as needs-action",
        undo_schema,
        &ctx,
        handler(|i, a| Box::pin(task_undo(i, a))),
    ));

    let clear_schema = object_schema(props(&[("tasklist", json!({"type":"string"}))]), &[]);
    out.push(tool(
        "google.tasks.clear",
        "Clear all completed tasks from a list",
        clear_schema,
        &ctx,
        handler(|i, a| Box::pin(task_clear(i, a))),
    ));

    out
}

fn tl(input: &Value) -> &str {
    optional_str(input, "tasklist").unwrap_or("@default")
}

async fn task_lists(_input: Value, api: &GoogleApi) -> Result<Value> {
    api.get_json(&format!("{BASE}/users/@me/lists")).await
}

async fn task_lists_create(input: Value, api: &GoogleApi) -> Result<Value> {
    let title = required_str(&input, "title")?;
    let body = json!({"title": title});
    api.post_json(&format!("{BASE}/users/@me/lists"), &body)
        .await
}

async fn task_list(input: Value, api: &GoogleApi) -> Result<Value> {
    let tasklist = tl(&input);
    let max = optional_u64(&input, "max_results").unwrap_or(100);
    let show_completed = input
        .get("show_completed")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let show_hidden = input
        .get("show_hidden")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let query: Vec<(String, String)> = vec![
        ("maxResults".to_string(), max.to_string()),
        ("showCompleted".to_string(), show_completed.to_string()),
        ("showHidden".to_string(), show_hidden.to_string()),
    ];
    let url = format!("{BASE}/lists/{tl}/tasks", tl = urlencode(tasklist));
    api.get_json_with_query(&url, Some(&query)).await
}

async fn task_get(input: Value, api: &GoogleApi) -> Result<Value> {
    let tasklist = tl(&input);
    let task_id = required_str(&input, "task_id")?;
    let url = format!(
        "{BASE}/lists/{tl}/tasks/{tid}",
        tl = urlencode(tasklist),
        tid = urlencode(task_id)
    );
    api.get_json(&url).await
}

async fn task_add(input: Value, api: &GoogleApi) -> Result<Value> {
    let tasklist = tl(&input);
    let title = required_str(&input, "title")?;
    let mut body = json!({"title": title});
    if let Some(n) = optional_str(&input, "notes") {
        body["notes"] = json!(n);
    }
    if let Some(d) = optional_str(&input, "due") {
        body["due"] = json!(d);
    }
    let mut query: Vec<(String, String)> = Vec::new();
    if let Some(p) = optional_str(&input, "parent") {
        query.push(("parent".to_string(), p.to_string()));
    }
    let url = format!("{BASE}/lists/{tl}/tasks", tl = urlencode(tasklist));
    api.post_json_with_query(&url, Some(&query), Some(&body))
        .await
}

async fn task_update(input: Value, api: &GoogleApi) -> Result<Value> {
    let tasklist = tl(&input);
    let task_id = required_str(&input, "task_id")?;
    let mut body = Map::new();
    if let Some(v) = optional_str(&input, "title") {
        body.insert("title".into(), json!(v));
    }
    if let Some(v) = optional_str(&input, "notes") {
        body.insert("notes".into(), json!(v));
    }
    if let Some(v) = optional_str(&input, "due") {
        body.insert("due".into(), json!(v));
    }
    if let Some(v) = optional_str(&input, "status") {
        body.insert("status".into(), json!(v));
    }
    let url = format!(
        "{BASE}/lists/{tl}/tasks/{tid}",
        tl = urlencode(tasklist),
        tid = urlencode(task_id)
    );
    api.patch_json(&url, &Value::Object(body)).await
}

async fn task_delete(input: Value, api: &GoogleApi) -> Result<Value> {
    let tasklist = tl(&input);
    let task_id = required_str(&input, "task_id")?;
    let url = format!(
        "{BASE}/lists/{tl}/tasks/{tid}",
        tl = urlencode(tasklist),
        tid = urlencode(task_id)
    );
    api.delete(&url).await?;
    Ok(json!({"deleted": true, "task_id": task_id}))
}

async fn task_done(input: Value, api: &GoogleApi) -> Result<Value> {
    let tasklist = tl(&input);
    let task_id = required_str(&input, "task_id")?;
    let url = format!(
        "{BASE}/lists/{tl}/tasks/{tid}",
        tl = urlencode(tasklist),
        tid = urlencode(task_id)
    );
    api.patch_json(&url, &json!({"status": "completed"})).await
}

async fn task_undo(input: Value, api: &GoogleApi) -> Result<Value> {
    let tasklist = tl(&input);
    let task_id = required_str(&input, "task_id")?;
    let url = format!(
        "{BASE}/lists/{tl}/tasks/{tid}",
        tl = urlencode(tasklist),
        tid = urlencode(task_id)
    );
    api.patch_json(&url, &json!({"status": "needsAction"}))
        .await
}

async fn task_clear(input: Value, api: &GoogleApi) -> Result<Value> {
    let tasklist = tl(&input);
    let url = format!("{BASE}/lists/{tl}/clear", tl = urlencode(tasklist));
    api.post_json(&url, &json!({})).await
}
