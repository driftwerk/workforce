//! Notes on CRM records: read one, list a record's, edit, remove. Creating
//! one is `hubspot.note_create` in `tools.rs`.

use async_trait::async_trait;
use eyre::{eyre, Result};
use serde_json::{json, Value};
use tracing::info;

use workforce_agent::tools::{Tool, ToolResult};

use super::client::HubspotClient;
use super::config::clamp_limit;
use super::objects::{batch_read_body, object_type, BATCH_MAX};
use super::tools::{
    append_associations_query, coerce_id, hubspot_tool, run, wrap_crm_properties, wrap_crm_results,
};
use crate::adapters::utils::coerce_to_u64;

/// What a note is, unless the caller asks for more.
pub const NOTE_PROPERTIES: &[&str] = &[
    "hs_note_body",
    "hs_timestamp",
    "hs_createdate",
    "hs_lastmodifieddate",
    "hubspot_owner_id",
];

const SOURCE: &str = "hubspot_note";

/// The caller's `properties`, or the defaults.
fn note_properties(input: &Value) -> Value {
    match input["properties"].as_array().filter(|p| !p.is_empty()) {
        Some(props) => Value::Array(props.clone()),
        None => json!(NOTE_PROPERTIES),
    }
}

/// Newest first by `hs_timestamp`, then `hs_createdate`; a note without
/// either sorts last.
pub fn newest_first(mut notes: Vec<Value>) -> Vec<Value> {
    let stamp = |note: &Value| -> String {
        ["hs_timestamp", "hs_createdate"]
            .iter()
            .find_map(|key| note["properties"][key].as_str().map(str::to_string))
            .unwrap_or_default()
    };
    notes.sort_by_key(|note| std::cmp::Reverse(stamp(note)));
    notes
}

/// The associations endpoint's page size, and the most notes one record is
/// read for: enough for any support ticket, bounded for a contact with years
/// of history.
const ASSOCIATION_PAGE: u64 = 500;
const MOST_NOTES: usize = 2000;

/// The ids of every note on a record. HubSpot lists associations in its
/// own order, so all of them are needed before the newest can be picked.
async fn note_ids(client: &HubspotClient, object_type: &str, id: &str) -> Result<Vec<String>> {
    let path = format!("/crm/v4/objects/{object_type}/{id}/associations/notes");
    let mut ids = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let mut query = vec![("limit", ASSOCIATION_PAGE.to_string())];
        if let Some(cursor) = &after {
            query.push(("after", cursor.clone()));
        }
        let page = client.get(&path, &query).await?;
        ids.extend(
            page["results"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| coerce_id(&a["toObjectId"]).ok()),
        );
        after = page["paging"]["next"]["after"].as_str().map(str::to_string);
        if after.is_none() || ids.len() >= MOST_NOTES {
            return Ok(ids);
        }
    }
}

/// The notes on one record, newest first, at most `limit` of them: every
/// note id, the notes in batch reads, then the sort and the cut.
async fn notes_on(
    client: &HubspotClient,
    object_type: &str,
    id: &str,
    limit: usize,
    properties: Value,
) -> Result<Vec<Value>> {
    let ids = note_ids(client, object_type, id).await?;
    let mut notes = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(BATCH_MAX) {
        let read = client
            .post(
                "/crm/v3/objects/notes/batch/read",
                batch_read_body(chunk, &properties),
            )
            .await?;
        notes.extend(read["results"].as_array().cloned().unwrap_or_default());
    }
    // Sort on the bare timestamps: wrapping comes last, or the markers
    // would sort instead of the dates.
    let mut newest = newest_first(notes);
    newest.truncate(limit);
    Ok(wrap_crm_results(newest, SOURCE))
}

// ── hubspot.note_get ────────────────────────────────────────────────

hubspot_tool!(NoteGetTool);

#[async_trait]
impl Tool for NoteGetTool {
    fn name(&self) -> &str {
        "hubspot.note_get"
    }
    fn description(&self) -> &str {
        "Get one note by id: its body (HTML as HubSpot stores it), timestamp, owner, and optionally the ids of the records it is attached to."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Note id" },
                "properties": { "type": "array", "items": { "type": "string" }, "description": "Property names to include (default: body, timestamp, created, modified, owner)" },
                "associations": { "type": "array", "items": { "type": "string" }, "description": "Object types whose associated record ids to include, e.g. [\"tickets\"]" },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_id(&input["id"])?;
            let mut query: Vec<(&str, String)> = note_properties(&input)
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p.as_str().map(|s| ("properties", s.to_string())))
                .collect();
            append_associations_query(&mut query, &input);
            let note = client
                .get(&format!("/crm/v3/objects/notes/{id}"), &query)
                .await?;
            Ok(wrap_crm_properties(note, SOURCE))
        })
        .await
    }
}

// ── hubspot.note_list ───────────────────────────────────────────────

hubspot_tool!(NoteListTool);

#[async_trait]
impl Tool for NoteListTool {
    fn name(&self) -> &str {
        "hubspot.note_list"
    }
    fn description(&self) -> &str {
        "List the notes on a record — a ticket, contact, company or deal — newest first, with their bodies. This is how to read what has been noted on a ticket. Every note on the record is read before the newest are picked, so `limit` is the number returned, not a page."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "object_type": super::objects::object_type_prop("Object type of the record the notes are on"),
                "object_id": { "type": "string", "description": "Record id" },
                "limit": { "type": "integer", "description": "Max notes (default/max 100)", "minimum": 1, "maximum": 100 },
                "properties": { "type": "array", "items": { "type": "string" }, "description": "Property names to include (default: body, timestamp, created, modified, owner)" },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["object_type", "object_id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let object_type = object_type(&input)?.to_string();
            let id = coerce_id(&input["object_id"]).map_err(|_| eyre!("Missing 'object_id'"))?;
            let limit = clamp_limit(coerce_to_u64(&input["limit"]), BATCH_MAX as u64) as usize;
            let notes = notes_on(&client, &object_type, &id, limit, note_properties(&input)).await?;
            info!(count = notes.len(), object_type, id, "Listed HubSpot notes");
            Ok(json!({ "count": notes.len(), "object_type": object_type, "object_id": id, "notes": notes }))
        })
        .await
    }
}

// ── hubspot.note_update ─────────────────────────────────────────────

hubspot_tool!(NoteUpdateTool);

#[async_trait]
impl Tool for NoteUpdateTool {
    fn name(&self) -> &str {
        "hubspot.note_update"
    }
    fn description(&self) -> &str {
        "MUTATING: edit a note. `body` replaces its text (plain or simple HTML); `properties` sets any other note property, such as hs_timestamp or hubspot_owner_id."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Note id" },
                "body": { "type": "string", "description": "The new body" },
                "properties": { "type": "object", "description": "Other properties to set" },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_id(&input["id"])?;
            let mut properties = input
                .get("properties")
                .filter(|v| v.is_object())
                .cloned()
                .unwrap_or_else(|| json!({}));
            if let Some(body) = input["body"].as_str() {
                properties["hs_note_body"] = json!(body);
            }
            if properties.as_object().is_some_and(|p| p.is_empty()) {
                return Err(eyre!("Nothing to change: pass 'body' or 'properties'"));
            }
            let updated = client
                .patch(
                    &format!("/crm/v3/objects/notes/{id}"),
                    json!({ "properties": properties }),
                )
                .await?;
            info!(id, "Updated HubSpot note");
            Ok(updated)
        })
        .await
    }
}

// ── hubspot.note_delete ─────────────────────────────────────────────

hubspot_tool!(NoteDeleteTool);

#[async_trait]
impl Tool for NoteDeleteTool {
    fn name(&self) -> &str {
        "hubspot.note_delete"
    }
    fn description(&self) -> &str {
        "MUTATING: archive (delete) a note. It leaves every record it was attached to; HubSpot keeps it in the recycle bin for 90 days."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Note id" },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_id(&input["id"])?;
            client
                .delete(&format!("/crm/v3/objects/notes/{id}"), &[])
                .await?;
            info!(id, "Archived HubSpot note");
            Ok(json!({ "archived": true, "id": id }))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn notes_sort_newest_first_and_undated_last() {
        let sorted = newest_first(vec![
            json!({ "id": "old", "properties": { "hs_timestamp": "2026-01-01T00:00:00Z" } }),
            json!({ "id": "undated", "properties": {} }),
            json!({ "id": "new", "properties": { "hs_timestamp": "2026-09-01T00:00:00Z" } }),
            json!({ "id": "created", "properties": { "hs_createdate": "2026-05-01T00:00:00Z" } }),
        ]);
        let ids: Vec<&str> = sorted.iter().map(|n| n["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["new", "created", "old", "undated"]);
    }

    #[test]
    fn note_properties_default_to_the_useful_set() {
        assert_eq!(note_properties(&json!({})), json!(NOTE_PROPERTIES));
        assert_eq!(
            note_properties(&json!({ "properties": [] })),
            json!(NOTE_PROPERTIES)
        );
        assert_eq!(
            note_properties(&json!({ "properties": ["hs_note_body"] })),
            json!(["hs_note_body"])
        );
    }

    #[tokio::test]
    async fn a_records_notes_come_from_its_associations_then_one_batch_read() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(
                "/crm/v4/objects/tickets/48558109562/associations/notes",
            ))
            .and(query_param("limit", "500"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [
                    { "toObjectId": 116861675391u64, "associationTypes": [] },
                    { "toObjectId": 116861675000u64, "associationTypes": [] },
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/crm/v3/objects/notes/batch/read"))
            .and(body_partial_json(json!({
                "inputs": [{ "id": "116861675391" }, { "id": "116861675000" }],
                "properties": NOTE_PROPERTIES,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [
                    { "id": "116861675000", "properties": { "hs_note_body": "<p>older</p>", "hs_timestamp": "2026-09-12T08:00:00Z" } },
                    { "id": "116861675391", "properties": { "hs_note_body": "<p>newer <script>x</script></p>", "hs_timestamp": "2026-09-13T08:12:47Z" } },
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = HubspotClient::new("pat-test", 100)
            .unwrap()
            .with_base_url(&server.uri());
        let notes = notes_on(
            &client,
            "tickets",
            "48558109562",
            100,
            json!(NOTE_PROPERTIES),
        )
        .await
        .unwrap();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0]["id"], "116861675391", "newest first");
        let body = notes[0]["properties"]["hs_note_body"].as_str().unwrap();
        assert!(
            body.contains("EXTERNAL_UNTRUSTED_CONTENT"),
            "customer HTML is wrapped: {body}"
        );
        assert!(body.contains("<p>newer"));
    }

    #[tokio::test]
    async fn the_limit_cuts_after_the_sort() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/crm/v4/objects/tickets/1/associations/notes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [{ "toObjectId": 10 }, { "toObjectId": 11 }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/crm/v3/objects/notes/batch/read"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [
                    { "id": "10", "properties": { "hs_timestamp": "2026-01-01T00:00:00Z" } },
                    { "id": "11", "properties": { "hs_timestamp": "2026-02-01T00:00:00Z" } },
                ]
            })))
            .mount(&server)
            .await;
        let client = HubspotClient::new("pat-test", 100)
            .unwrap()
            .with_base_url(&server.uri());
        let one = notes_on(&client, "tickets", "1", 1, json!(NOTE_PROPERTIES))
            .await
            .unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0]["id"], "11");
    }

    #[tokio::test]
    async fn association_pages_are_followed_until_the_end() {
        let server = MockServer::start().await;
        let notes_path = "/crm/v4/objects/contacts/9/associations/notes";
        Mock::given(method("GET"))
            .and(path(notes_path))
            .and(query_param("after", "p2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [{ "toObjectId": 2 }]
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(notes_path))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [{ "toObjectId": 1 }],
                "paging": { "next": { "after": "p2" } }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = HubspotClient::new("pat-test", 100)
            .unwrap()
            .with_base_url(&server.uri());
        assert_eq!(
            note_ids(&client, "contacts", "9").await.unwrap(),
            vec!["1", "2"]
        );
    }

    #[tokio::test]
    async fn a_record_without_notes_reads_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/crm/v4/objects/contacts/1/associations/notes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "results": [] })))
            .mount(&server)
            .await;
        let client = HubspotClient::new("pat-test", 100)
            .unwrap()
            .with_base_url(&server.uri());
        let notes = notes_on(&client, "contacts", "1", 10, json!(NOTE_PROPERTIES))
            .await
            .unwrap();
        assert!(notes.is_empty());
    }
}
