//! Google People (Contacts) tools.
//! Tool names: `google.contacts.*` and `google.people.*`.
//!
//! Base: <https://people.googleapis.com/v1/>.

use std::sync::Arc;

use eyre::Result;
use serde_json::{json, Map, Value};
use workforce_agent::tools::Tool;

use crate::adapters::google::api::{GoogleApi, GoogleApiContext};

use crate::adapters::google::tool_helpers::{
    handler, object_schema, optional_str, optional_u64, props, required_str, tool, urlencode,
};

const BASE: &str = "https://people.googleapis.com/v1";
const PERSON_FIELDS: &str =
    "names,emailAddresses,phoneNumbers,organizations,addresses,photos,biographies";

pub fn tools(ctx: Arc<GoogleApiContext>) -> Vec<Arc<dyn Tool>> {
    let mut out: Vec<Arc<dyn Tool>> = Vec::new();

    // ── contacts list ────────────────────────────────────────────────────────
    let list_schema = object_schema(
        props(&[
            (
                "max_results",
                json!({"type":"integer","description":"Max contacts (default 100, max 1000)"}),
            ),
            ("page_token", json!({"type":"string"})),
        ]),
        &[],
    );
    out.push(tool(
        "google.contacts.list",
        "List contacts",
        list_schema,
        &ctx,
        handler(|i, a| Box::pin(contacts_list(i, a))),
    ));

    // ── contacts search ──────────────────────────────────────────────────────
    let search_schema = object_schema(
        props(&[
            ("query", json!({"type":"string"})),
            ("max_results", json!({"type":"integer"})),
        ]),
        &["query"],
    );
    out.push(tool(
        "google.contacts.search",
        "Search contacts by name/email",
        search_schema,
        &ctx,
        handler(|i, a| Box::pin(contacts_search(i, a))),
    ));

    // ── contacts get ─────────────────────────────────────────────────────────
    let get_schema = object_schema(
        props(&[(
            "resource_name",
            json!({"type":"string","description":"e.g. 'people/c123456'"}),
        )]),
        &["resource_name"],
    );
    out.push(tool(
        "google.contacts.get",
        "Get a contact by resource name",
        get_schema,
        &ctx,
        handler(|i, a| Box::pin(contacts_get(i, a))),
    ));

    // ── contacts create ──────────────────────────────────────────────────────
    let create_schema = object_schema(
        props(&[
            ("given_name", json!({"type":"string"})),
            ("family_name", json!({"type":"string"})),
            ("email", json!({"type":"string"})),
            ("phone", json!({"type":"string"})),
            ("organization", json!({"type":"string"})),
        ]),
        &["given_name"],
    );
    out.push(tool(
        "google.contacts.create",
        "Create a new contact",
        create_schema,
        &ctx,
        handler(|i, a| Box::pin(contacts_create(i, a))),
    ));

    // ── contacts update ──────────────────────────────────────────────────────
    let update_schema = object_schema(
        props(&[
            ("resource_name", json!({"type":"string"})),
            ("given_name", json!({"type":"string"})),
            ("family_name", json!({"type":"string"})),
            ("email", json!({"type":"string"})),
            ("phone", json!({"type":"string"})),
            (
                "etag",
                json!({"type":"string","description":"Required — get from contacts.get"}),
            ),
        ]),
        &["resource_name", "etag"],
    );
    out.push(tool(
        "google.contacts.update",
        "Update a contact (requires etag)",
        update_schema,
        &ctx,
        handler(|i, a| Box::pin(contacts_update(i, a))),
    ));

    // ── contacts delete ──────────────────────────────────────────────────────
    let del_schema = object_schema(
        props(&[("resource_name", json!({"type":"string"}))]),
        &["resource_name"],
    );
    out.push(tool(
        "google.contacts.delete",
        "Delete a contact",
        del_schema,
        &ctx,
        handler(|i, a| Box::pin(contacts_delete(i, a))),
    ));

    // ── directory search (org-wide) ──────────────────────────────────────────
    let dir_schema = object_schema(
        props(&[
            ("query", json!({"type":"string"})),
            ("max_results", json!({"type":"integer"})),
        ]),
        &["query"],
    );
    out.push(tool(
        "google.contacts.directory_search",
        "Search organization directory",
        dir_schema,
        &ctx,
        handler(|i, a| Box::pin(contacts_directory_search(i, a))),
    ));

    // ── other contacts list ──────────────────────────────────────────────────
    let other_schema = object_schema(props(&[("max_results", json!({"type":"integer"}))]), &[]);
    out.push(tool(
        "google.contacts.other_list",
        "List 'other contacts' (auto-saved)",
        other_schema,
        &ctx,
        handler(|i, a| Box::pin(contacts_other_list(i, a))),
    ));

    // ── people: me ───────────────────────────────────────────────────────────
    out.push(tool(
        "google.people.me",
        "Get the authenticated user's profile",
        object_schema(Map::new(), &[]),
        &ctx,
        handler(|i, a| Box::pin(people_me(i, a))),
    ));

    // ── people: get ──────────────────────────────────────────────────────────
    let people_get_schema = object_schema(
        props(&[("resource_name", json!({"type":"string"}))]),
        &["resource_name"],
    );
    out.push(tool(
        "google.people.get",
        "Get a person by resource name",
        people_get_schema,
        &ctx,
        handler(|i, a| Box::pin(people_get(i, a))),
    ));

    // ── people: search ───────────────────────────────────────────────────────
    let people_search_schema = object_schema(
        props(&[
            ("query", json!({"type":"string"})),
            ("max_results", json!({"type":"integer"})),
        ]),
        &["query"],
    );
    out.push(tool(
        "google.people.search",
        "Search people",
        people_search_schema,
        &ctx,
        handler(|i, a| Box::pin(people_search(i, a))),
    ));

    out
}

async fn contacts_list(input: Value, api: &GoogleApi) -> Result<Value> {
    let max = optional_u64(&input, "max_results").unwrap_or(100).min(1000);
    let mut query: Vec<(String, String)> = vec![
        ("pageSize".to_string(), max.to_string()),
        ("personFields".to_string(), PERSON_FIELDS.to_string()),
    ];
    if let Some(pt) = optional_str(&input, "page_token") {
        query.push(("pageToken".to_string(), pt.to_string()));
    }
    let url = format!("{BASE}/people/me/connections");
    api.get_json_with_query(&url, Some(&query)).await
}

async fn contacts_search(input: Value, api: &GoogleApi) -> Result<Value> {
    let q = required_str(&input, "query")?;
    let max = optional_u64(&input, "max_results").unwrap_or(30);
    let query: Vec<(String, String)> = vec![
        ("query".to_string(), q.to_string()),
        ("pageSize".to_string(), max.to_string()),
        ("readMask".to_string(), PERSON_FIELDS.to_string()),
    ];
    let url = format!("{BASE}/people:searchContacts");
    api.get_json_with_query(&url, Some(&query)).await
}

async fn contacts_get(input: Value, api: &GoogleApi) -> Result<Value> {
    let rn = required_str(&input, "resource_name")?;
    let url = format!(
        "{BASE}/{rn}?personFields={PERSON_FIELDS}",
        rn = urlencode(rn)
    );
    api.get_json(&url).await
}

async fn contacts_create(input: Value, api: &GoogleApi) -> Result<Value> {
    let given = required_str(&input, "given_name")?;
    let family = optional_str(&input, "family_name").unwrap_or("");
    let mut body = json!({
        "names": [{ "givenName": given, "familyName": family }]
    });
    if let Some(e) = optional_str(&input, "email") {
        body["emailAddresses"] = json!([{"value": e}]);
    }
    if let Some(p) = optional_str(&input, "phone") {
        body["phoneNumbers"] = json!([{"value": p}]);
    }
    if let Some(o) = optional_str(&input, "organization") {
        body["organizations"] = json!([{"name": o}]);
    }
    let url = format!("{BASE}/people:createContact");
    api.post_json(&url, &body).await
}

async fn contacts_update(input: Value, api: &GoogleApi) -> Result<Value> {
    let rn = required_str(&input, "resource_name")?;
    let etag = required_str(&input, "etag")?;
    let mut fields_to_update = Vec::new();
    let mut body = json!({"etag": etag});
    if let Some(v) = optional_str(&input, "given_name") {
        body["names"] = json!([{"givenName": v, "familyName": optional_str(&input, "family_name").unwrap_or("")}]);
        fields_to_update.push("names");
    }
    if let Some(v) = optional_str(&input, "email") {
        body["emailAddresses"] = json!([{"value": v}]);
        fields_to_update.push("emailAddresses");
    }
    if let Some(v) = optional_str(&input, "phone") {
        body["phoneNumbers"] = json!([{"value": v}]);
        fields_to_update.push("phoneNumbers");
    }
    let update_mask = fields_to_update.join(",");
    let query: Vec<(String, String)> = vec![("updatePersonFields".to_string(), update_mask)];
    let url = format!("{BASE}/{rn}:updateContact", rn = urlencode(rn));
    api.patch_json_with_query(&url, Some(&query), &body).await
}

async fn contacts_delete(input: Value, api: &GoogleApi) -> Result<Value> {
    let rn = required_str(&input, "resource_name")?;
    let url = format!("{BASE}/{rn}:deleteContact", rn = urlencode(rn));
    api.delete(&url).await?;
    Ok(json!({"deleted": true, "resource_name": rn}))
}

async fn contacts_directory_search(input: Value, api: &GoogleApi) -> Result<Value> {
    let q = required_str(&input, "query")?;
    let max = optional_u64(&input, "max_results").unwrap_or(30);
    let query: Vec<(String, String)> = vec![
        ("query".to_string(), q.to_string()),
        ("pageSize".to_string(), max.to_string()),
        ("readMask".to_string(), PERSON_FIELDS.to_string()),
        (
            "sources".to_string(),
            "DIRECTORY_SOURCE_TYPE_DOMAIN_PROFILE".to_string(),
        ),
    ];
    let url = format!("{BASE}/people:searchDirectoryPeople");
    api.get_json_with_query(&url, Some(&query)).await
}

async fn contacts_other_list(input: Value, api: &GoogleApi) -> Result<Value> {
    let max = optional_u64(&input, "max_results").unwrap_or(100);
    let query: Vec<(String, String)> = vec![
        ("pageSize".to_string(), max.to_string()),
        ("readMask".to_string(), "names,emailAddresses".to_string()),
    ];
    let url = format!("{BASE}/otherContacts");
    api.get_json_with_query(&url, Some(&query)).await
}

async fn people_me(_input: Value, api: &GoogleApi) -> Result<Value> {
    let url = format!("{BASE}/people/me?personFields={PERSON_FIELDS}");
    api.get_json(&url).await
}

async fn people_get(input: Value, api: &GoogleApi) -> Result<Value> {
    let rn = required_str(&input, "resource_name")?;
    let url = format!(
        "{BASE}/{rn}?personFields={PERSON_FIELDS}",
        rn = urlencode(rn)
    );
    api.get_json(&url).await
}

async fn people_search(input: Value, api: &GoogleApi) -> Result<Value> {
    let q = required_str(&input, "query")?;
    let max = optional_u64(&input, "max_results").unwrap_or(30);
    let query: Vec<(String, String)> = vec![
        ("query".to_string(), q.to_string()),
        ("pageSize".to_string(), max.to_string()),
        ("readMask".to_string(), PERSON_FIELDS.to_string()),
    ];
    let url = format!("{BASE}/people:searchContacts");
    api.get_json_with_query(&url, Some(&query)).await
}
