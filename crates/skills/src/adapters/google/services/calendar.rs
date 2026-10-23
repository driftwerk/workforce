//! Google Calendar v3 tools.
//! Tool names follow `google.calendar.*`.
//!
//! Endpoint base: <https://www.googleapis.com/calendar/v3/>.

use std::sync::Arc;

use eyre::Result;
use serde_json::{json, Map, Value};
use workforce_agent::tools::Tool;

use crate::adapters::google::api::{GoogleApi, GoogleApiContext};
use crate::adapters::google::tool_helpers::{
    handler, object_schema, optional_str, optional_u64, required_str, urlencode, GoogleTool,
};

const BASE: &str = "https://www.googleapis.com/calendar/v3";

/// Collect every `google.calendar.*` tool.
pub fn tools(ctx: Arc<GoogleApiContext>) -> Vec<Arc<dyn Tool>> {
    let mut out: Vec<Arc<dyn Tool>> = Vec::new();

    // ─── events list ─────────────────────────────────────────────────────────
    let events_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert(
                "calendar_id".to_string(),
                json!({
                    "type": "string",
                    "description": "Calendar ID. Default: 'primary'."
                }),
            );
            m.insert(
                "time_min".to_string(),
                json!({
                    "type": "string",
                    "description": "RFC3339 lower bound for event start time (inclusive)."
                }),
            );
            m.insert(
                "time_max".to_string(),
                json!({
                    "type": "string",
                    "description": "RFC3339 upper bound for event start time (exclusive)."
                }),
            );
            m.insert(
                "q".to_string(),
                json!({
                    "type": "string",
                    "description": "Free-text search query."
                }),
            );
            m.insert(
                "max_results".to_string(),
                json!({
                    "type": "integer",
                    "description": "Maximum events to return (default 25, max 2500)."
                }),
            );
            m.insert("single_events".to_string(), json!({
                "type": "boolean",
                "description": "Expand recurring events into individual instances (default true)."
            }));
            m.insert(
                "order_by".to_string(),
                json!({
                    "type": "string",
                    "enum": ["startTime", "updated"],
                    "description": "Sort order."
                }),
            );
            m
        },
        &[],
    );
    out.push(
        GoogleTool::new(
            "google.calendar.events",
            "List Google Calendar events",
            events_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(calendar_events(input, api))),
        )
        .arc(),
    );

    // ─── event get ───────────────────────────────────────────────────────────
    let get_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("calendar_id".to_string(), json!({ "type": "string" }));
            m.insert("event_id".to_string(), json!({ "type": "string" }));
            m
        },
        &["event_id"],
    );
    out.push(
        GoogleTool::new(
            "google.calendar.event_get",
            "Get a single Google Calendar event by ID",
            get_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(calendar_event_get(input, api))),
        )
        .arc(),
    );

    // ─── event create ────────────────────────────────────────────────────────
    let create_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert(
                "calendar_id".to_string(),
                json!({ "type": "string", "description": "Default: 'primary'" }),
            );
            m.insert(
                "summary".to_string(),
                json!({ "type": "string", "description": "Event title (required)." }),
            );
            m.insert("description".to_string(), json!({ "type": "string" }));
            m.insert("location".to_string(), json!({ "type": "string" }));
            m.insert(
                "start".to_string(),
                json!({
                    "type": "string",
                    "description": "RFC3339 start datetime or YYYY-MM-DD for all-day."
                }),
            );
            m.insert(
                "end".to_string(),
                json!({
                    "type": "string",
                    "description": "RFC3339 end datetime or YYYY-MM-DD for all-day."
                }),
            );
            m.insert(
                "attendees".to_string(),
                json!({
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "List of attendee emails."
                }),
            );
            m.insert(
                "send_updates".to_string(),
                json!({
                    "type": "string",
                    "enum": ["all", "externalOnly", "none"]
                }),
            );
            m
        },
        &["summary", "start", "end"],
    );
    out.push(
        GoogleTool::new(
            "google.calendar.create",
            "Create a Google Calendar event",
            create_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(calendar_create(input, api))),
        )
        .arc(),
    );

    // ─── event update (patch) ────────────────────────────────────────────────
    let update_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("calendar_id".to_string(), json!({ "type": "string" }));
            m.insert("event_id".to_string(), json!({ "type": "string" }));
            m.insert("summary".to_string(), json!({ "type": "string" }));
            m.insert("description".to_string(), json!({ "type": "string" }));
            m.insert("location".to_string(), json!({ "type": "string" }));
            m.insert("start".to_string(), json!({ "type": "string" }));
            m.insert("end".to_string(), json!({ "type": "string" }));
            m
        },
        &["event_id"],
    );
    out.push(
        GoogleTool::new(
            "google.calendar.update",
            "Update a Google Calendar event (partial)",
            update_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(calendar_update(input, api))),
        )
        .arc(),
    );

    // ─── event delete ────────────────────────────────────────────────────────
    let delete_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("calendar_id".to_string(), json!({ "type": "string" }));
            m.insert("event_id".to_string(), json!({ "type": "string" }));
            m
        },
        &["event_id"],
    );
    out.push(
        GoogleTool::new(
            "google.calendar.delete",
            "Delete a Google Calendar event",
            delete_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(calendar_delete(input, api))),
        )
        .arc(),
    );

    // ─── colors ──────────────────────────────────────────────────────────────
    let colors_schema = object_schema(Map::new(), &[]);
    out.push(
        GoogleTool::new(
            "google.calendar.colors",
            "List Google Calendar event colors",
            colors_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(calendar_colors(input, api))),
        )
        .arc(),
    );

    // ─── calendars list ──────────────────────────────────────────────────────
    let cals_schema = object_schema(Map::new(), &[]);
    out.push(
        GoogleTool::new(
            "google.calendar.calendars",
            "List the user's calendars (calendarList)",
            cals_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(calendar_calendars(input, api))),
        )
        .arc(),
    );

    // ─── free/busy ───────────────────────────────────────────────────────────
    let freebusy_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert(
                "time_min".to_string(),
                json!({ "type": "string", "description": "RFC3339 start" }),
            );
            m.insert(
                "time_max".to_string(),
                json!({ "type": "string", "description": "RFC3339 end" }),
            );
            m.insert(
                "calendar_ids".to_string(),
                json!({
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Calendar IDs to query (default ['primary'])."
                }),
            );
            m
        },
        &["time_min", "time_max"],
    );
    out.push(
        GoogleTool::new(
            "google.calendar.freebusy",
            "Query free/busy ranges for one or more calendars",
            freebusy_schema,
            ctx.clone(),
            handler(|input, api| Box::pin(calendar_freebusy(input, api))),
        )
        .arc(),
    );

    // ─── search (alias for events list with q) ───────────────────────────────
    let search_schema = object_schema(
        {
            let mut m = Map::new();
            m.insert("q".to_string(), json!({ "type": "string" }));
            m.insert("calendar_id".to_string(), json!({ "type": "string" }));
            m.insert("max_results".to_string(), json!({ "type": "integer" }));
            m
        },
        &["q"],
    );
    out.push(
        GoogleTool::new(
            "google.calendar.search",
            "Search Google Calendar events by text query",
            search_schema,
            ctx,
            handler(|input, api| Box::pin(calendar_events(input, api))),
        )
        .arc(),
    );

    out
}

async fn calendar_events(input: Value, api: &GoogleApi) -> Result<Value> {
    let calendar_id = optional_str(&input, "calendar_id").unwrap_or("primary");
    let mut query: Vec<(String, String)> = Vec::new();
    if let Some(tmin) = optional_str(&input, "time_min") {
        query.push(("timeMin".into(), tmin.into()));
    }
    if let Some(tmax) = optional_str(&input, "time_max") {
        query.push(("timeMax".into(), tmax.into()));
    }
    if let Some(q) = optional_str(&input, "q") {
        query.push(("q".into(), q.into()));
    }
    let max = optional_u64(&input, "max_results").unwrap_or(25);
    query.push(("maxResults".into(), max.to_string()));
    let single = input
        .get("single_events")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    query.push(("singleEvents".into(), single.to_string()));
    if let Some(order) = optional_str(&input, "order_by") {
        query.push(("orderBy".into(), order.into()));
    } else if single {
        query.push(("orderBy".into(), "startTime".into()));
    }

    let url = format!(
        "{BASE}/calendars/{cal}/events",
        cal = urlencode(calendar_id)
    );
    let resp: Value = api.get_json_with_query(&url, Some(&query)).await?;
    Ok(resp)
}

async fn calendar_event_get(input: Value, api: &GoogleApi) -> Result<Value> {
    let calendar_id = optional_str(&input, "calendar_id").unwrap_or("primary");
    let event_id = required_str(&input, "event_id")?;
    let url = format!(
        "{BASE}/calendars/{cal}/events/{ev}",
        cal = urlencode(calendar_id),
        ev = urlencode(event_id),
    );
    api.get_json(&url).await
}

async fn calendar_create(input: Value, api: &GoogleApi) -> Result<Value> {
    let calendar_id = optional_str(&input, "calendar_id").unwrap_or("primary");
    let summary = required_str(&input, "summary")?;
    let start = required_str(&input, "start")?;
    let end = required_str(&input, "end")?;

    let body = build_event_body(summary, start, end, &input);

    let url = format!(
        "{BASE}/calendars/{cal}/events",
        cal = urlencode(calendar_id)
    );
    let mut query = Vec::new();
    if let Some(s) = optional_str(&input, "send_updates") {
        query.push(("sendUpdates".to_string(), s.to_string()));
    }
    api.post_json_with_query(&url, Some(&query), Some(&body))
        .await
}

async fn calendar_update(input: Value, api: &GoogleApi) -> Result<Value> {
    let calendar_id = optional_str(&input, "calendar_id").unwrap_or("primary");
    let event_id = required_str(&input, "event_id")?;
    let mut body = Map::new();
    if let Some(s) = optional_str(&input, "summary") {
        body.insert("summary".into(), json!(s));
    }
    if let Some(s) = optional_str(&input, "description") {
        body.insert("description".into(), json!(s));
    }
    if let Some(s) = optional_str(&input, "location") {
        body.insert("location".into(), json!(s));
    }
    if let Some(s) = optional_str(&input, "start") {
        body.insert("start".into(), time_or_date(s));
    }
    if let Some(s) = optional_str(&input, "end") {
        body.insert("end".into(), time_or_date(s));
    }
    let url = format!(
        "{BASE}/calendars/{cal}/events/{ev}",
        cal = urlencode(calendar_id),
        ev = urlencode(event_id),
    );
    api.patch_json(&url, &Value::Object(body)).await
}

async fn calendar_delete(input: Value, api: &GoogleApi) -> Result<Value> {
    let calendar_id = optional_str(&input, "calendar_id").unwrap_or("primary");
    let event_id = required_str(&input, "event_id")?;
    let url = format!(
        "{BASE}/calendars/{cal}/events/{ev}",
        cal = urlencode(calendar_id),
        ev = urlencode(event_id),
    );
    api.delete(&url).await?;
    Ok(json!({ "deleted": true, "event_id": event_id }))
}

async fn calendar_colors(_input: Value, api: &GoogleApi) -> Result<Value> {
    let url = format!("{BASE}/colors");
    api.get_json(&url).await
}

async fn calendar_calendars(_input: Value, api: &GoogleApi) -> Result<Value> {
    let url = format!("{BASE}/users/me/calendarList");
    api.get_json(&url).await
}

async fn calendar_freebusy(input: Value, api: &GoogleApi) -> Result<Value> {
    let time_min = required_str(&input, "time_min")?;
    let time_max = required_str(&input, "time_max")?;
    let ids: Vec<String> = input
        .get("calendar_ids")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_else(|| vec!["primary".to_string()]);

    let items: Vec<Value> = ids.iter().map(|id| json!({ "id": id })).collect();
    let body = json!({
        "timeMin": time_min,
        "timeMax": time_max,
        "items": items,
    });
    let url = format!("{BASE}/freeBusy");
    api.post_json(&url, &body).await
}

fn time_or_date(s: &str) -> Value {
    // Crude heuristic: YYYY-MM-DD = all-day `date`, else `dateTime`.
    if s.len() == 10 && s.chars().filter(|c| *c == '-').count() == 2 {
        json!({ "date": s })
    } else {
        json!({ "dateTime": s })
    }
}

fn build_event_body(summary: &str, start: &str, end: &str, input: &Value) -> Value {
    let mut body = Map::new();
    body.insert("summary".into(), json!(summary));
    if let Some(s) = optional_str(input, "description") {
        body.insert("description".into(), json!(s));
    }
    if let Some(s) = optional_str(input, "location") {
        body.insert("location".into(), json!(s));
    }
    body.insert("start".into(), time_or_date(start));
    body.insert("end".into(), time_or_date(end));
    if let Some(arr) = input.get("attendees").and_then(|v| v.as_array()) {
        let attendees: Vec<Value> = arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(|e| json!({ "email": e }))
            .collect();
        body.insert("attendees".into(), Value::Array(attendees));
    }
    Value::Object(body)
}
