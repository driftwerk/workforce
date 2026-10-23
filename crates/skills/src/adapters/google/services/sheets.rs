//! Google Sheets v4 tools.
//! Tool names follow `google.sheets.*`.
//!
//! Base: <https://sheets.googleapis.com/v4/spreadsheets/>.

use std::sync::Arc;

use eyre::Result;
use serde_json::{json, Map, Value};
use workforce_agent::artifacts::{global_store, resolve_rows_input, ArtifactStore, RowsRequest};
use workforce_agent::runner::CURRENT_AGENT_ID;
use workforce_agent::tools::Tool;

use crate::adapters::google::api::{GoogleApi, GoogleApiContext};

use crate::adapters::google::tool_helpers::{
    handler, object_schema, optional_array, optional_str, required_array, required_str, urlencode,
    GoogleTool,
};

const BASE: &str = "https://sheets.googleapis.com/v4/spreadsheets";

fn values_schema_2d() -> Value {
    json!({
        "type": "array",
        "items": { "type": "array", "items": {} },
        "description": "2D array of rows; each row is an array of cell values (strings, numbers, or booleans). Pass the array directly — do NOT wrap it in a JSON string. Mutually exclusive with `artifact_id`."
    })
}

fn artifact_id_schema() -> Value {
    json!({
        "type": "string",
        "description": "Artifact handle returned by a producer (e.g. metabase.dataset_query_native). Use this instead of `values` to write a large result set without holding the rows in context. Mutually exclusive with `values`."
    })
}

fn row_range_schema() -> Value {
    json!({
        "type": "string",
        "description": "Optional slice of artifact rows, e.g. `0-200` to take the first 200 rows. Only valid with `artifact_id`."
    })
}

/// Add the rows-source one-of to a schema produced by [`object_schema`].
/// Sheets `update`/`append` accept either inline `values` or an
/// `artifact_id`, never both; the LLM sees the schema-level constraint.
fn with_rows_one_of(mut schema: Value) -> Value {
    if let Some(obj) = schema.as_object_mut() {
        obj.insert(
            "oneOf".to_string(),
            json!([
                {"required": ["values"]},
                {"required": ["artifact_id"]},
            ]),
        );
    }
    schema
}

/// Pull `values`, `artifact_id`, `row_range` out of an LLM-supplied
/// argument blob and hand them to [`resolve_rows_input`]. When no
/// artifact store is installed, the inline path still works and
/// `artifact_id` arguments surface as a clear runtime error.
fn resolve_rows(input: &Value) -> Result<Value> {
    let values = optional_array(input, "values");
    let artifact_id = optional_str(input, "artifact_id");
    let row_range = optional_str(input, "row_range");

    if let (Some(values), None) = (&values, artifact_id) {
        return Ok(values.clone());
    }

    let store = global_store().ok_or_else(|| {
        eyre::eyre!("artifact_id supplied but no artifact store is installed in this runtime")
    })?;
    let agent_id = CURRENT_AGENT_ID
        .try_with(|a| a.clone())
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| eyre::eyre!("artifact_id supplied outside an agent run"))?;
    let rows = resolve_rows_input(
        &*store as &dyn ArtifactStore,
        &agent_id,
        RowsRequest {
            values,
            artifact_id,
            row_range,
        },
    )?;
    Ok(Value::Array(rows.into_iter().map(Value::Array).collect()))
}

fn value_input_option_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["RAW", "USER_ENTERED"],
        "description": "USER_ENTERED (default) interprets formulae and dates, but treats empty strings (\"\") as 'leave cell unchanged'. Use RAW when writing literal empty strings or when input must be stored exactly as given."
    })
}

pub fn tools(ctx: Arc<GoogleApiContext>) -> Vec<Arc<dyn Tool>> {
    let mut out: Vec<Arc<dyn Tool>> = Vec::new();

    // ─── get values ──────────────────────────────────────────────────────────
    let get_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("sheet_id".into(), json!({ "type": "string" }));
            m.insert(
                "range".into(),
                json!({ "type": "string", "description": "A1 notation, e.g. 'Sheet1!A1:D20'" }),
            );
            m.insert(
                "value_render_option".into(),
                json!({ "type": "string", "enum": ["FORMATTED_VALUE", "UNFORMATTED_VALUE", "FORMULA"] }),
            );
            m
        },
        &["sheet_id", "range"],
    );
    out.push(
        GoogleTool::new(
            "google.sheets.get",
            "Read a range from a Google Sheet",
            get_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(sheets_get(input, api))),
        )
        .arc(),
    );

    // ─── update ──────────────────────────────────────────────────────────────
    let update_schema = with_rows_one_of(object_schema(
        {
            let mut m = Map::new();
            m.insert("sheet_id".into(), json!({ "type": "string" }));
            m.insert("range".into(), json!({ "type": "string" }));
            m.insert("values".into(), values_schema_2d());
            m.insert("artifact_id".into(), artifact_id_schema());
            m.insert("row_range".into(), row_range_schema());
            m.insert("value_input_option".into(), value_input_option_schema());
            m
        },
        &["sheet_id", "range"],
    ));
    out.push(
        GoogleTool::new(
            "google.sheets.update",
            "Write values to a Sheets range",
            update_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(sheets_update(input, api))),
        )
        .arc(),
    );

    // ─── append ──────────────────────────────────────────────────────────────
    let append_schema = with_rows_one_of(object_schema(
        {
            let mut m = Map::new();
            m.insert("sheet_id".into(), json!({ "type": "string" }));
            m.insert("range".into(), json!({ "type": "string" }));
            m.insert("values".into(), values_schema_2d());
            m.insert("artifact_id".into(), artifact_id_schema());
            m.insert("row_range".into(), row_range_schema());
            m.insert("value_input_option".into(), value_input_option_schema());
            m.insert(
                "insert_data_option".into(),
                json!({ "type": "string", "enum": ["INSERT_ROWS", "OVERWRITE"] }),
            );
            m
        },
        &["sheet_id", "range"],
    ));
    out.push(
        GoogleTool::new(
            "google.sheets.append",
            "Append rows to a Sheet",
            append_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(sheets_append(input, api))),
        )
        .arc(),
    );

    // ─── clear ───────────────────────────────────────────────────────────────
    let clear_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("sheet_id".into(), json!({ "type": "string" }));
            m.insert("range".into(), json!({ "type": "string" }));
            m
        },
        &["sheet_id", "range"],
    );
    out.push(
        GoogleTool::new(
            "google.sheets.clear",
            "Clear values in a Sheets range",
            clear_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(sheets_clear(input, api))),
        )
        .arc(),
    );

    // ─── metadata ────────────────────────────────────────────────────────────
    let meta_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("sheet_id".into(), json!({ "type": "string" }));
            m
        },
        &["sheet_id"],
    );
    out.push(
        GoogleTool::new(
            "google.sheets.metadata",
            "Get a Sheet's tabs and metadata",
            meta_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(sheets_metadata(input, api))),
        )
        .arc(),
    );

    // ─── create spreadsheet ──────────────────────────────────────────────────
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
            "google.sheets.create",
            "Create a new Google Spreadsheet",
            create_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(sheets_create(input, api))),
        )
        .arc(),
    );

    // ─── add-tab ─────────────────────────────────────────────────────────────
    let add_tab_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("sheet_id".into(), json!({ "type": "string" }));
            m.insert("title".into(), json!({ "type": "string" }));
            m.insert("rows".into(), json!({ "type": "integer" }));
            m.insert("columns".into(), json!({ "type": "integer" }));
            m
        },
        &["sheet_id", "title"],
    );
    out.push(
        GoogleTool::new(
            "google.sheets.add_tab",
            "Add a tab to an existing spreadsheet",
            add_tab_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(sheets_add_tab(input, api))),
        )
        .arc(),
    );

    // ─── delete-tab ──────────────────────────────────────────────────────────
    let del_tab_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("sheet_id".into(), json!({ "type": "string" }));
            m.insert("tab_id".into(), json!({ "type": "integer" }));
            m
        },
        &["sheet_id", "tab_id"],
    );
    out.push(
        GoogleTool::new(
            "google.sheets.delete_tab",
            "Delete a tab from a spreadsheet",
            del_tab_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(sheets_delete_tab(input, api))),
        )
        .arc(),
    );

    // ─── rename-tab ──────────────────────────────────────────────────────────
    let rename_tab_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("sheet_id".into(), json!({ "type": "string" }));
            m.insert("tab_id".into(), json!({ "type": "integer" }));
            m.insert("new_title".into(), json!({ "type": "string" }));
            m
        },
        &["sheet_id", "tab_id", "new_title"],
    );
    out.push(
        GoogleTool::new(
            "google.sheets.rename_tab",
            "Rename a tab",
            rename_tab_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(sheets_rename_tab(input, api))),
        )
        .arc(),
    );

    // ─── batch_update (raw escape hatch) ─────────────────────────────────────
    let batch_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("sheet_id".into(), json!({ "type": "string" }));
            m.insert(
                "requests".into(),
                json!({
                    "type": "array",
                    "items": { "type": "object" },
                    "description": "Array of Sheets batchUpdate request objects (passthrough). Pass the array directly — do NOT wrap it in a JSON string."
                }),
            );
            m
        },
        &["sheet_id", "requests"],
    );
    out.push(
        GoogleTool::new(
            "google.sheets.batch_update",
            "Run an arbitrary Sheets batchUpdate (advanced)",
            batch_schema,
            ctx,
            handler(|input, api| Box::pin(sheets_batch_update(input, api))),
        )
        .arc(),
    );

    out
}

async fn sheets_get(input: Value, api: &GoogleApi) -> Result<Value> {
    let sheet_id = required_str(&input, "sheet_id")?;
    let range = required_str(&input, "range")?;
    let url = format!(
        "{BASE}/{id}/values/{range}",
        id = urlencode(sheet_id),
        range = urlencode(range)
    );
    let mut query = Vec::new();
    if let Some(o) = optional_str(&input, "value_render_option") {
        query.push(("valueRenderOption".to_string(), o.to_string()));
    }
    api.get_json_with_query(&url, Some(&query)).await
}

async fn sheets_update(input: Value, api: &GoogleApi) -> Result<Value> {
    let sheet_id = required_str(&input, "sheet_id")?;
    let range = required_str(&input, "range")?;
    let values = resolve_rows(&input)?;
    let option = optional_str(&input, "value_input_option").unwrap_or("USER_ENTERED");

    let url = format!(
        "{BASE}/{id}/values/{range}",
        id = urlencode(sheet_id),
        range = urlencode(range)
    );
    let query = vec![("valueInputOption".to_string(), option.to_string())];
    let body = json!({ "values": values });
    api.put_json_with_query(&url, Some(&query), &body).await
}

async fn sheets_append(input: Value, api: &GoogleApi) -> Result<Value> {
    let sheet_id = required_str(&input, "sheet_id")?;
    let range = required_str(&input, "range")?;
    let values = resolve_rows(&input)?;
    let value_input = optional_str(&input, "value_input_option").unwrap_or("USER_ENTERED");
    let insert = optional_str(&input, "insert_data_option").unwrap_or("INSERT_ROWS");

    let url = format!(
        "{BASE}/{id}/values/{range}:append",
        id = urlencode(sheet_id),
        range = urlencode(range)
    );
    let query = vec![
        ("valueInputOption".to_string(), value_input.to_string()),
        ("insertDataOption".to_string(), insert.to_string()),
    ];
    let body = json!({ "values": values });
    api.post_json_with_query(&url, Some(&query), Some(&body))
        .await
}

async fn sheets_clear(input: Value, api: &GoogleApi) -> Result<Value> {
    let sheet_id = required_str(&input, "sheet_id")?;
    let range = required_str(&input, "range")?;
    let url = format!(
        "{BASE}/{id}/values/{range}:clear",
        id = urlencode(sheet_id),
        range = urlencode(range)
    );
    api.post_json(&url, &json!({})).await
}

async fn sheets_metadata(input: Value, api: &GoogleApi) -> Result<Value> {
    let sheet_id = required_str(&input, "sheet_id")?;
    let url = format!(
        "{BASE}/{id}?fields=spreadsheetId,properties,sheets(properties)",
        id = urlencode(sheet_id)
    );
    api.get_json(&url).await
}

async fn sheets_create(input: Value, api: &GoogleApi) -> Result<Value> {
    let title = required_str(&input, "title")?;
    let body = json!({ "properties": { "title": title } });
    let created: Value = api.post_json(BASE, &body).await?;
    // Sheets has eventual consistency on create — poll metadata so an
    // immediate sheets.get / sheets.update from the agent doesn't 404.
    if let Some(id) = created.get("spreadsheetId").and_then(|v| v.as_str()) {
        let confirm_url = format!("{BASE}/{}", urlencode(id));
        api.confirm_exists_with_retry(&confirm_url, 3).await?;
    }
    Ok(created)
}

async fn sheets_add_tab(input: Value, api: &GoogleApi) -> Result<Value> {
    let sheet_id = required_str(&input, "sheet_id")?;
    let title = required_str(&input, "title")?;
    let mut grid = Map::new();
    if let Some(r) = input.get("rows").and_then(|v| v.as_u64()) {
        grid.insert("rowCount".into(), json!(r));
    }
    if let Some(c) = input.get("columns").and_then(|v| v.as_u64()) {
        grid.insert("columnCount".into(), json!(c));
    }
    let mut props = json!({ "title": title });
    if !grid.is_empty() {
        props["gridProperties"] = Value::Object(grid);
    }
    let requests = json!([{ "addSheet": { "properties": props } }]);
    batch_update_call(sheet_id, &requests, api).await
}

async fn sheets_delete_tab(input: Value, api: &GoogleApi) -> Result<Value> {
    let sheet_id = required_str(&input, "sheet_id")?;
    let tab_id = input
        .get("tab_id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| eyre::eyre!("tab_id is required and must be an integer"))?;
    let requests = json!([{ "deleteSheet": { "sheetId": tab_id } }]);
    batch_update_call(sheet_id, &requests, api).await
}

async fn sheets_rename_tab(input: Value, api: &GoogleApi) -> Result<Value> {
    let sheet_id = required_str(&input, "sheet_id")?;
    let tab_id = input
        .get("tab_id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| eyre::eyre!("tab_id is required and must be an integer"))?;
    let new_title = required_str(&input, "new_title")?;
    let requests = json!([{
        "updateSheetProperties": {
            "properties": { "sheetId": tab_id, "title": new_title },
            "fields": "title"
        }
    }]);
    batch_update_call(sheet_id, &requests, api).await
}

async fn sheets_batch_update(input: Value, api: &GoogleApi) -> Result<Value> {
    let sheet_id = required_str(&input, "sheet_id")?;
    let requests = required_array(&input, "requests")?;
    batch_update_call(sheet_id, &requests, api).await
}

async fn batch_update_call(sheet_id: &str, requests: &Value, api: &GoogleApi) -> Result<Value> {
    let url = format!("{BASE}/{id}:batchUpdate", id = urlencode(sheet_id));
    let body = json!({ "requests": requests });
    api.post_json(&url, &body).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_support::fresh_agent_id as mint_agent_id;
    use workforce_agent::artifacts::{ArtifactBody, ArtifactDescriptor, ContentType, DEFAULT_TTL};

    #[test]
    fn the_rows_source_choice_reaches_the_model_after_sanitising() {
        let schema =
            workforce_agent::llm::tool_schema::sanitize_tool_schema(with_rows_one_of(json!({
                "type": "object",
                "properties": { "values": values_schema_2d(), "artifact_id": artifact_id_schema() },
            })));
        assert!(schema.get("oneOf").is_none());
        assert_eq!(
            schema["description"],
            json!("Provide exactly one of: {values} | {artifact_id}.")
        );
        assert_eq!(schema["properties"]["values"], values_schema_2d());
    }

    fn fresh_agent_id() -> String {
        mint_agent_id("sheets-test-agent")
    }

    fn json_artifact(rows: Value, agent: &str) -> String {
        let store = global_store().expect("store installed");
        store
            .write(
                agent,
                ArtifactDescriptor {
                    producer: "test".into(),
                    mime: ContentType::ApplicationJson,
                    schema: None,
                    row_count: rows.as_array().map(|a| a.len()),
                    ttl_seconds: DEFAULT_TTL.as_secs(),
                    agent_id: agent.into(),
                    session_id: None,
                    user_id: None,
                    filename: None,
                    media_type: None,
                },
                ArtifactBody::Json(rows),
            )
            .unwrap()
            .id
            .to_string()
    }

    fn csv_artifact(text: &str, agent: &str) -> String {
        let store = global_store().expect("store installed");
        store
            .write(
                agent,
                ArtifactDescriptor {
                    producer: "test".into(),
                    mime: ContentType::TextCsv,
                    schema: None,
                    row_count: None,
                    ttl_seconds: DEFAULT_TTL.as_secs(),
                    agent_id: agent.into(),
                    session_id: None,
                    user_id: None,
                    filename: None,
                    media_type: None,
                },
                ArtifactBody::Csv(text.into()),
            )
            .unwrap()
            .id
            .to_string()
    }

    fn run_in_agent_scope<F, T>(agent: String, f: F) -> T
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
    fn resolve_rows_inline_passthrough() {
        let agent = fresh_agent_id();
        let input = json!({ "values": [[1, 2], [3, 4]] });
        let rows = run_in_agent_scope(agent, move || resolve_rows(&input).unwrap());
        assert_eq!(rows, json!([[1, 2], [3, 4]]));
    }

    #[test]
    fn resolve_rows_via_artifact_loads_from_store() {
        let agent = fresh_agent_id();
        let id = json_artifact(json!([[10, 20], [30, 40]]), &agent);
        let input = json!({ "artifact_id": id });
        let rows = run_in_agent_scope(agent, move || resolve_rows(&input).unwrap());
        assert_eq!(rows, json!([[10, 20], [30, 40]]));
    }

    #[test]
    fn resolve_rows_with_row_range_slices_artifact() {
        let agent = fresh_agent_id();
        let id = json_artifact(json!([[0], [1], [2], [3]]), &agent);
        let input = json!({ "artifact_id": id, "row_range": "1-3" });
        let rows = run_in_agent_scope(agent, move || resolve_rows(&input).unwrap());
        assert_eq!(rows, json!([[1], [2]]));
    }

    #[test]
    fn resolve_rows_rejects_mime_mismatch() {
        let agent = fresh_agent_id();
        let id = csv_artifact("a,b\n1,2\n", &agent);
        let input = json!({ "artifact_id": id });
        let err = run_in_agent_scope(agent, move || resolve_rows(&input).unwrap_err());
        assert!(err.to_string().contains("application/json"), "got: {err}");
    }

    #[test]
    fn resolve_rows_rejects_both_values_and_artifact_id() {
        let agent = fresh_agent_id();
        let id = json_artifact(json!([[1]]), &agent);
        let input = json!({ "values": [[2]], "artifact_id": id });
        let err = run_in_agent_scope(agent, move || resolve_rows(&input).unwrap_err());
        assert!(err.to_string().contains("not both"), "got: {err}");
    }

    #[test]
    fn resolve_rows_rejects_artifact_outside_agent_scope() {
        let agent = fresh_agent_id();
        let id = json_artifact(json!([[1]]), &agent);
        let input = json!({ "artifact_id": id });
        let err = resolve_rows(&input).unwrap_err();
        assert!(err.to_string().contains("agent run"), "got: {err}");
    }

    #[test]
    fn resolve_rows_rejects_cross_agent_artifact() {
        let owner = fresh_agent_id();
        let other = fresh_agent_id();
        let id = json_artifact(json!([[1]]), &owner);
        let input = json!({ "artifact_id": id });
        let err = run_in_agent_scope(other, move || resolve_rows(&input).unwrap_err());
        assert!(err.to_string().contains("not found"), "got: {err}");
    }
}
