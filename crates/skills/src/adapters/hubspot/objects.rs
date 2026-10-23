//! Generic CRM object tools — any object type, including the engagements
//! (notes, tasks, calls, emails, meetings, communications) and custom
//! objects the typed `contact_*`/`ticket_*` tools do not reach — plus
//! associations, owners and property definitions.

use async_trait::async_trait;
use eyre::{eyre, Result};
use serde_json::{json, Value};
use tracing::info;

use workforce_agent::tools::{Tool, ToolResult};

use super::config::clamp_limit;
use super::tools::{
    append_associations_query, append_properties_query, coerce_id, extract_paging, hubspot_tool,
    list_query_params, run, wrap_crm_properties, wrap_crm_results,
};
use crate::adapters::utils::coerce_to_u64;

/// The CRM records HubSpot names in `/crm/v3/objects/{type}`.
const RECORDS: &[&str] = &["contacts", "companies", "deals", "tickets"];

/// The engagement types, whose records carry a required `hs_timestamp`.
const ENGAGEMENTS: &[&str] = &[
    "notes",
    "tasks",
    "calls",
    "emails",
    "meetings",
    "communications",
];

/// A batch read takes at most this many ids.
pub const BATCH_MAX: usize = 100;

/// Every built-in object type, for messages and schemas.
fn known_types() -> String {
    RECORDS
        .iter()
        .chain(ENGAGEMENTS)
        .copied()
        .collect::<Vec<_>>()
        .join(", ")
}

/// A HubSpot object type: a built-in one, or a custom object by its fully
/// qualified name (`p<portalId>_<name>`) or its `<n>-<n>` type id.
pub fn valid_object_type(name: &str) -> bool {
    let custom_name = name
        .strip_prefix('p')
        .and_then(|rest| rest.split_once('_'))
        .is_some_and(|(portal, name)| {
            !portal.is_empty()
                && portal.chars().all(|c| c.is_ascii_digit())
                && !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
    let custom_id = name.split_once('-').is_some_and(|(a, b)| {
        !a.is_empty() && !b.is_empty() && a.chars().chain(b.chars()).all(|c| c.is_ascii_digit())
    });
    RECORDS.contains(&name) || ENGAGEMENTS.contains(&name) || custom_name || custom_id
}

/// The object type named by `key`, validated.
fn typed_object<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    let name = input[key]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| eyre!("Missing '{key}' parameter"))?;
    if valid_object_type(name) {
        Ok(name)
    } else {
        Err(eyre!(
            "{key} must be one of {} or a custom object type (p<portalId>_<name> or <n>-<n>), not '{name}'",
            known_types()
        ))
    }
}

/// The tool input's `object_type`, validated.
pub fn object_type(input: &Value) -> Result<&str> {
    typed_object(input, "object_type")
}

pub fn object_type_prop(description: &str) -> Value {
    json!({
        "type": "string",
        "description": format!(
            "{description}: one of {}, or a custom object type (its fully qualified name p<portalId>_<name>, or its numeric id)",
            known_types()
        )
    })
}

fn is_engagement(object_type: &str) -> bool {
    ENGAGEMENTS.contains(&object_type)
}

/// An engagement without a timestamp is refused by HubSpot; the moment of
/// creation is what a caller who left it out means.
pub fn with_engagement_timestamp(object_type: &str, mut properties: Value, now: &str) -> Value {
    if is_engagement(object_type) && properties.get("hs_timestamp").is_none_or(Value::is_null) {
        properties["hs_timestamp"] = json!(now);
    }
    properties
}

/// The v4 association endpoint that lets HubSpot pick the default type
/// between two objects.
pub fn default_association_path(from: &str, from_id: &str, to: &str, to_id: &str) -> String {
    format!("/crm/v4/objects/{from}/{from_id}/associations/default/{to}/{to_id}")
}

fn association_path(from: &str, from_id: &str, to: &str, to_id: &str) -> String {
    format!("/crm/v4/objects/{from}/{from_id}/associations/{to}/{to_id}")
}

/// `{to_id, to_type}` pairs from a tool's `associations` array.
fn association_targets(input: &Value) -> Result<Vec<(String, String)>> {
    input["associations"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|a| {
            let to_id = coerce_id(&a["to_id"]).map_err(|_| eyre!("association missing 'to_id'"))?;
            let to_type = typed_object(a, "to_type")?;
            Ok((to_type.to_string(), to_id))
        })
        .collect()
}

/// The body of `POST /crm/v3/objects/{type}/batch/read`.
pub fn batch_read_body(ids: &[String], properties: &Value) -> Value {
    let mut body = json!({
        "inputs": ids.iter().map(|id| json!({ "id": id })).collect::<Vec<_>>(),
    });
    if let Some(props) = properties.as_array().filter(|p| !p.is_empty()) {
        body["properties"] = json!(props);
    }
    body
}

fn ids_from(input: &Value) -> Result<Vec<String>> {
    let ids: Vec<String> = input["ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| coerce_id(v).ok())
        .collect();
    if ids.is_empty() {
        return Err(eyre!("'ids' must name at least one record"));
    }
    if ids.len() > BATCH_MAX {
        return Err(eyre!("'ids' may name at most {BATCH_MAX} records per call"));
    }
    Ok(ids)
}

fn source(object_type: &str) -> String {
    format!("hubspot_{object_type}")
}

// ── hubspot.object_get ──────────────────────────────────────────────

hubspot_tool!(ObjectGetTool);

#[async_trait]
impl Tool for ObjectGetTool {
    fn name(&self) -> &str {
        "hubspot.object_get"
    }
    fn description(&self) -> &str {
        "Get one CRM record of any object type by id — contacts, companies, deals, tickets, notes, tasks, calls, emails, meetings, communications, or a custom object — optionally with the ids of its associated records."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "object_type": object_type_prop("Object type of the record"),
                "id": { "type": "string", "description": "Record id" },
                "properties": { "type": "array", "items": { "type": "string" }, "description": "Property names to include (HubSpot returns only defaults otherwise)" },
                "associations": { "type": "array", "items": { "type": "string" }, "description": "Object types whose associated record ids to include, e.g. [\"contacts\", \"notes\"]" },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["object_type", "id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let object_type = object_type(&input)?.to_string();
            let id = coerce_id(&input["id"])?;
            let mut query = Vec::new();
            append_properties_query(&mut query, &input);
            append_associations_query(&mut query, &input);
            let record = client
                .get(&format!("/crm/v3/objects/{object_type}/{id}"), &query)
                .await?;
            Ok(wrap_crm_properties(record, &source(&object_type)))
        })
        .await
    }
}

// ── hubspot.object_list ─────────────────────────────────────────────

hubspot_tool!(ObjectListTool);

#[async_trait]
impl Tool for ObjectListTool {
    fn name(&self) -> &str {
        "hubspot.object_list"
    }
    fn description(&self) -> &str {
        "List CRM records of any object type in creation order, paginated. For filtering or sorting use hubspot.crm_search; for the records attached to one record use hubspot.association_list or hubspot.note_list."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "object_type": object_type_prop("Object type to list"),
                "limit": { "type": "integer", "description": "Max records (default/max 100)", "minimum": 1, "maximum": 100 },
                "after": { "type": "string", "description": "Pagination cursor" },
                "properties": { "type": "array", "items": { "type": "string" }, "description": "Property names to include" },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["object_type"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let object_type = object_type(&input)?.to_string();
            let mut query = list_query_params(&input, client.max_results);
            append_properties_query(&mut query, &input);
            let resp = client
                .get(&format!("/crm/v3/objects/{object_type}"), &query)
                .await?;
            let results = resp["results"].as_array().cloned().unwrap_or_default();
            let wrapped = wrap_crm_results(results, &source(&object_type));
            let paging = extract_paging(&resp);
            info!(count = wrapped.len(), object_type, "Listed HubSpot records");
            Ok(json!({ "count": wrapped.len(), "results": wrapped, "paging": paging }))
        })
        .await
    }
}

// ── hubspot.object_batch_read ───────────────────────────────────────

hubspot_tool!(ObjectBatchReadTool);

#[async_trait]
impl Tool for ObjectBatchReadTool {
    fn name(&self) -> &str {
        "hubspot.object_batch_read"
    }
    fn description(&self) -> &str {
        "Read up to 100 CRM records of one object type by id in a single call, with the properties you name."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "object_type": object_type_prop("Object type of the records"),
                "ids": { "type": "array", "items": { "type": "string" }, "description": "Record ids (max 100)" },
                "properties": { "type": "array", "items": { "type": "string" }, "description": "Property names to include" },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["object_type", "ids"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let object_type = object_type(&input)?.to_string();
            let ids = ids_from(&input)?;
            let resp = client
                .post(
                    &format!("/crm/v3/objects/{object_type}/batch/read"),
                    batch_read_body(&ids, &input["properties"]),
                )
                .await?;
            let results = resp["results"].as_array().cloned().unwrap_or_default();
            let wrapped = wrap_crm_results(results, &source(&object_type));
            info!(
                count = wrapped.len(),
                object_type, "Batch-read HubSpot records"
            );
            Ok(json!({ "count": wrapped.len(), "results": wrapped, "errors": resp["errors"] }))
        })
        .await
    }
}

// ── hubspot.object_create ───────────────────────────────────────────

hubspot_tool!(ObjectCreateTool);

#[async_trait]
impl Tool for ObjectCreateTool {
    fn name(&self) -> &str {
        "hubspot.object_create"
    }
    fn description(&self) -> &str {
        "MUTATING: create a CRM record of any object type, optionally associated to existing records (HubSpot's default association type is used). Engagements — tasks (hs_task_subject, hs_task_body, hs_task_status), calls (hs_call_title, hs_call_body), emails (hs_email_subject, hs_email_text), meetings (hs_meeting_title) — get hs_timestamp = now unless you set it. The record is created before it is associated: an association that fails is reported under association_errors, never by failing the call, so do not create the record again. Notes have hubspot.note_create."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "object_type": object_type_prop("Object type to create"),
                "properties": { "type": "object", "description": "The record's properties" },
                "associations": {
                    "type": "array",
                    "description": "Records to associate the new one with: [{\"to_type\": \"tickets\", \"to_id\": \"123\"}]",
                    "items": {
                        "type": "object",
                        "properties": {
                            "to_type": { "type": "string", "description": "Object type of the target" },
                            "to_id": { "type": "string", "description": "Target record id" }
                        },
                        "required": ["to_type", "to_id"]
                    }
                },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["object_type", "properties"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let object_type = object_type(&input)?.to_string();
            let properties = input
                .get("properties")
                .filter(|v| v.is_object())
                .cloned()
                .ok_or_else(|| eyre!("Missing 'properties' object"))?;
            let targets = association_targets(&input)?;
            let now = chrono::Utc::now().to_rfc3339();
            let body =
                json!({ "properties": with_engagement_timestamp(&object_type, properties, &now) });
            let mut created = client
                .post(&format!("/crm/v3/objects/{object_type}"), body)
                .await?;
            let id = coerce_id(&created["id"])
                .map_err(|_| eyre!("HubSpot returned no id for the new record"))?;
            // The record exists from here on: a failed association is
            // reported, never turned into an error that reads as "not created".
            let mut associated = Vec::new();
            let mut errors = Vec::new();
            for (to_type, to_id) in targets {
                match client
                    .put(
                        &default_association_path(&object_type, &id, &to_type, &to_id),
                        json!({}),
                    )
                    .await
                {
                    Ok(_) => associated.push(json!({ "to_type": to_type, "to_id": to_id })),
                    Err(e) => errors.push(
                        json!({ "to_type": to_type, "to_id": to_id, "error": e.to_string() }),
                    ),
                }
            }
            created["associated"] = json!(associated);
            if !errors.is_empty() {
                created["association_errors"] = json!(errors);
                created["warning"] = json!(format!(
                    "record {id} was created but {} association(s) failed; associate it with hubspot.association_create rather than creating it again",
                    errors.len()
                ));
            }
            info!(object_type, id, "Created HubSpot record");
            Ok(created)
        })
        .await
    }
}

// ── hubspot.object_update ───────────────────────────────────────────

hubspot_tool!(ObjectUpdateTool);

#[async_trait]
impl Tool for ObjectUpdateTool {
    fn name(&self) -> &str {
        "hubspot.object_update"
    }
    fn description(&self) -> &str {
        "MUTATING: update a CRM record of any object type. Include only the properties to change."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "object_type": object_type_prop("Object type of the record"),
                "id": { "type": "string", "description": "Record id" },
                "properties": { "type": "object", "description": "Properties to update" },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["object_type", "id", "properties"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let object_type = object_type(&input)?.to_string();
            let id = coerce_id(&input["id"])?;
            let properties = input
                .get("properties")
                .filter(|v| v.is_object())
                .cloned()
                .ok_or_else(|| eyre!("Missing 'properties' object"))?;
            let updated = client
                .patch(
                    &format!("/crm/v3/objects/{object_type}/{id}"),
                    json!({ "properties": properties }),
                )
                .await?;
            info!(object_type, id, "Updated HubSpot record");
            Ok(updated)
        })
        .await
    }
}

// ── hubspot.object_delete ───────────────────────────────────────────

hubspot_tool!(ObjectDeleteTool);

#[async_trait]
impl Tool for ObjectDeleteTool {
    fn name(&self) -> &str {
        "hubspot.object_delete"
    }
    fn description(&self) -> &str {
        "MUTATING: archive (delete) a CRM record of any object type — a contact, company, deal, ticket, task, call, email, meeting, or custom object. HubSpot keeps archived records in its recycle bin for 90 days. Notes have hubspot.note_delete."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "object_type": object_type_prop("Object type of the record"),
                "id": { "type": "string", "description": "Record id" },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["object_type", "id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let object_type = object_type(&input)?.to_string();
            let id = coerce_id(&input["id"])?;
            client
                .delete(&format!("/crm/v3/objects/{object_type}/{id}"), &[])
                .await?;
            info!(object_type, id, "Archived HubSpot record");
            Ok(json!({ "archived": true, "object_type": object_type, "id": id }))
        })
        .await
    }
}

// ── hubspot.association_list ────────────────────────────────────────

hubspot_tool!(AssociationListTool);

#[async_trait]
impl Tool for AssociationListTool {
    fn name(&self) -> &str {
        "hubspot.association_list"
    }
    fn description(&self) -> &str {
        "List the records of one object type associated with a record: the contacts on a ticket, the notes or tasks on a contact, the deals of a company. Returns ids and association types; read the records with hubspot.object_batch_read (or hubspot.note_list for notes)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "object_type": object_type_prop("Object type of the record"),
                "id": { "type": "string", "description": "Record id" },
                "to_object_type": object_type_prop("Object type of the associated records"),
                "limit": { "type": "integer", "description": "Max associations (default/max 500)", "minimum": 1, "maximum": 500 },
                "after": { "type": "string", "description": "Pagination cursor" },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["object_type", "id", "to_object_type"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let object_type = object_type(&input)?.to_string();
            let id = coerce_id(&input["id"])?;
            let to = typed_object(&input, "to_object_type")?.to_string();
            let limit = clamp_limit(coerce_to_u64(&input["limit"]), 500);
            let mut query = vec![("limit", limit.to_string())];
            if let Some(after) = input["after"].as_str() {
                query.push(("after", after.to_string()));
            }
            let resp = client
                .get(&format!("/crm/v4/objects/{object_type}/{id}/associations/{to}"), &query)
                .await?;
            let results = resp["results"].as_array().cloned().unwrap_or_default();
            info!(count = results.len(), object_type, id, to, "Listed HubSpot associations");
            Ok(json!({ "count": results.len(), "results": results, "paging": extract_paging(&resp) }))
        })
        .await
    }
}

// ── hubspot.association_create ──────────────────────────────────────

hubspot_tool!(AssociationCreateTool);

#[async_trait]
impl Tool for AssociationCreateTool {
    fn name(&self) -> &str {
        "hubspot.association_create"
    }
    fn description(&self) -> &str {
        "MUTATING: associate two existing records — a contact with a ticket, a note with a deal, a company with a contact. Without association_type_id HubSpot's default type between the two object types is used; pass one for a labelled association."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "from_type": object_type_prop("Object type of the first record"),
                "from_id": { "type": "string", "description": "First record id" },
                "to_type": object_type_prop("Object type of the second record"),
                "to_id": { "type": "string", "description": "Second record id" },
                "association_type_id": { "type": "integer", "description": "An association type id, when the default is not wanted" },
                "association_category": { "type": "string", "enum": ["HUBSPOT_DEFINED", "USER_DEFINED"], "description": "The type id's category (default HUBSPOT_DEFINED; USER_DEFINED for a custom label)" },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["from_type", "from_id", "to_type", "to_id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let from = typed_object(&input, "from_type")?;
            let from_id = coerce_id(&input["from_id"]).map_err(|_| eyre!("Missing 'from_id'"))?;
            let to = typed_object(&input, "to_type")?;
            let to_id = coerce_id(&input["to_id"]).map_err(|_| eyre!("Missing 'to_id'"))?;
            let category = match input["association_category"]
                .as_str()
                .unwrap_or("HUBSPOT_DEFINED")
            {
                c @ ("HUBSPOT_DEFINED" | "USER_DEFINED") => c,
                other => {
                    return Err(eyre!(
                        "association_category must be HUBSPOT_DEFINED or USER_DEFINED, not '{other}'"
                    ))
                }
            };
            let result = match input["association_type_id"].as_i64() {
                Some(type_id) => {
                    client
                        .put(
                            &association_path(from, &from_id, to, &to_id),
                            json!([{ "associationCategory": category, "associationTypeId": type_id }]),
                        )
                        .await?
                }
                None => {
                    client
                        .put(&default_association_path(from, &from_id, to, &to_id), json!({}))
                        .await?
                }
            };
            info!(from, from_id, to, to_id, "Associated HubSpot records");
            Ok(json!({ "associated": true, "from_type": from, "from_id": from_id, "to_type": to, "to_id": to_id, "result": result }))
        })
        .await
    }
}

// ── hubspot.association_delete ──────────────────────────────────────

hubspot_tool!(AssociationDeleteTool);

#[async_trait]
impl Tool for AssociationDeleteTool {
    fn name(&self) -> &str {
        "hubspot.association_delete"
    }
    fn description(&self) -> &str {
        "MUTATING: remove every association between two records. Neither record is deleted."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "from_type": object_type_prop("Object type of the first record"),
                "from_id": { "type": "string", "description": "First record id" },
                "to_type": object_type_prop("Object type of the second record"),
                "to_id": { "type": "string", "description": "Second record id" },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["from_type", "from_id", "to_type", "to_id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let from = typed_object(&input, "from_type")?;
            let from_id = coerce_id(&input["from_id"]).map_err(|_| eyre!("Missing 'from_id'"))?;
            let to = typed_object(&input, "to_type")?;
            let to_id = coerce_id(&input["to_id"]).map_err(|_| eyre!("Missing 'to_id'"))?;
            client
                .delete(&association_path(from, &from_id, to, &to_id), &[])
                .await?;
            info!(from, from_id, to, to_id, "Removed HubSpot association");
            Ok(json!({ "removed": true, "from_type": from, "from_id": from_id, "to_type": to, "to_id": to_id }))
        })
        .await
    }
}

// ── hubspot.owner_get ───────────────────────────────────────────────

hubspot_tool!(OwnerGetTool);

#[async_trait]
impl Tool for OwnerGetTool {
    fn name(&self) -> &str {
        "hubspot.owner_get"
    }
    fn description(&self) -> &str {
        "Get one HubSpot owner (a user who can be assigned records) by owner id — the value of a record's hubspot_owner_id."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Owner id" },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_id(&input["id"])?;
            client.get(&format!("/crm/v3/owners/{id}"), &[]).await
        })
        .await
    }
}

// ── hubspot.property_get ────────────────────────────────────────────

hubspot_tool!(PropertyGetTool);

#[async_trait]
impl Tool for PropertyGetTool {
    fn name(&self) -> &str {
        "hubspot.property_get"
    }
    fn description(&self) -> &str {
        "Get one property definition of an object type in full: type, field type, options with their values and labels, whether it is read-only or calculated."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "object_type": object_type_prop("Object type the property belongs to"),
                "name": { "type": "string", "description": "Internal property name, e.g. hs_pipeline_stage" },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["object_type", "name"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let object_type = object_type(&input)?.to_string();
            let name = input["name"]
                .as_str()
                .map(str::trim)
                .filter(|s| {
                    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                })
                .ok_or_else(|| eyre!("'name' must be an internal property name"))?;
            client
                .get(&format!("/crm/v3/properties/{object_type}/{name}"), &[])
                .await
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_types_are_the_known_ones_or_custom_object_ids() {
        for known in RECORDS.iter().chain(ENGAGEMENTS) {
            assert!(valid_object_type(known), "{known}");
        }
        assert!(valid_object_type("p24111421_cars"));
        assert!(valid_object_type("2-12345"));
        for bad in [
            "",
            "Contacts",
            "p_shift",
            "p_",
            "p24_bad-name",
            "2-",
            "-1",
            "tickets/1",
            "../x",
            "p",
        ] {
            assert!(!valid_object_type(bad), "{bad}");
        }
        assert_eq!(
            object_type(&json!({ "object_type": " notes " })).unwrap(),
            "notes"
        );
        let err = object_type(&json!({ "object_type": "widgets" })).unwrap_err();
        assert!(err.to_string().contains("custom object type"), "{err}");
        assert!(object_type(&json!({})).is_err());
    }

    #[test]
    fn engagements_get_a_timestamp_records_do_not() {
        let stamped = with_engagement_timestamp(
            "tasks",
            json!({ "hs_task_subject": "x" }),
            "2026-09-13T08:00:00Z",
        );
        assert_eq!(stamped["hs_timestamp"], "2026-09-13T08:00:00Z");
        let kept = with_engagement_timestamp(
            "notes",
            json!({ "hs_timestamp": "2020-01-01T00:00:00Z" }),
            "now",
        );
        assert_eq!(kept["hs_timestamp"], "2020-01-01T00:00:00Z");
        let untouched = with_engagement_timestamp("contacts", json!({ "email": "a@b.co" }), "now");
        assert!(untouched.get("hs_timestamp").is_none());
    }

    #[test]
    fn association_paths_and_targets() {
        assert_eq!(
            default_association_path("notes", "1", "tickets", "2"),
            "/crm/v4/objects/notes/1/associations/default/tickets/2"
        );
        assert_eq!(
            association_path("contacts", "7", "companies", "8"),
            "/crm/v4/objects/contacts/7/associations/companies/8"
        );
        let targets = association_targets(&json!({ "associations": [
            { "to_type": "tickets", "to_id": 123 },
            { "to_type": "contacts", "to_id": "9" },
        ]}))
        .unwrap();
        assert_eq!(
            targets,
            vec![
                ("tickets".to_string(), "123".to_string()),
                ("contacts".to_string(), "9".to_string())
            ]
        );
        assert!(
            association_targets(&json!({ "associations": [{ "to_type": "tickets" }] })).is_err()
        );
        assert!(association_targets(
            &json!({ "associations": [{ "to_type": "nope", "to_id": "1" }] })
        )
        .is_err());
        assert!(association_targets(&json!({})).unwrap().is_empty());
    }

    #[test]
    fn batch_reads_take_ids_and_optional_properties() {
        let body = batch_read_body(&["1".into(), "2".into()], &json!(["hs_note_body"]));
        assert_eq!(
            body,
            json!({ "inputs": [{ "id": "1" }, { "id": "2" }], "properties": ["hs_note_body"] })
        );
        let bare = batch_read_body(&["1".into()], &Value::Null);
        assert_eq!(bare, json!({ "inputs": [{ "id": "1" }] }));
        assert_eq!(
            ids_from(&json!({ "ids": ["1", 2] })).unwrap(),
            vec!["1", "2"]
        );
        assert!(ids_from(&json!({ "ids": [] })).is_err());
        let too_many: Vec<String> = (0..=BATCH_MAX).map(|i| i.to_string()).collect();
        assert!(ids_from(&json!({ "ids": too_many })).is_err());
    }
}
