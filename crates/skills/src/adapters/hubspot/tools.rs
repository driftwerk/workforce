//! HubSpot agent-callable tools.
//!
//! Every tool accepts an optional `portal` parameter for multi-portal
//! selection (explicit -> default). CRM property values are wrapped via
//! `wrap_external_content` to defend against prompt injection.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use serde_json::{json, Value};
use tracing::{info, warn};

use workforce_agent::security::external_content::wrap_external_content;
use workforce_agent::tools::{Tool, ToolResult};

use super::client::HubspotClient;
use super::config::clamp_limit;
use crate::adapters::utils::{coerce_to_u64, instance_choices};

// ── shared helpers ──────────────────────────────────────────────────

fn resolve_client(
    clients: &HashMap<String, Arc<HubspotClient>>,
    default_portal: &str,
    input: &Value,
) -> eyre::Result<Arc<HubspotClient>> {
    if let Some(name) = input["portal"].as_str() {
        return clients.get(name).cloned().ok_or_else(|| {
            eyre::eyre!(
                "Unknown HubSpot portal: '{name}'. Available: {:?}",
                clients.keys().collect::<Vec<_>>()
            )
        });
    }
    clients.get(default_portal).cloned().ok_or_else(|| {
        eyre::eyre!(
            "Unknown HubSpot portal: '{default_portal}'. Available: {:?}",
            clients.keys().collect::<Vec<_>>()
        )
    })
}

fn portal_description(
    clients: &HashMap<String, Arc<HubspotClient>>,
    default_portal: &str,
) -> String {
    if clients.len() <= 1 {
        return "HubSpot portal name (for multi-portal setups)".to_string();
    }
    format!(
        "HubSpot portal. {}",
        instance_choices(clients, default_portal, "portal")
    )
}

fn portal_schema_prop(
    clients: &HashMap<String, Arc<HubspotClient>>,
    default_portal: &str,
) -> Value {
    json!({
        "type": "string",
        "description": portal_description(clients, default_portal)
    })
}

/// Wrap all property values in a CRM object response to defend against
/// prompt injection from user-controlled CRM data.
pub(super) fn wrap_crm_properties(mut value: Value, source: &str) -> Value {
    if let Some(props) = value.get_mut("properties").and_then(|p| p.as_object_mut()) {
        for (_key, val) in props.iter_mut() {
            if let Some(s) = val.as_str() {
                if !s.is_empty() {
                    *val = Value::String(wrap_external_content(s, source).wrapped_content);
                }
            }
        }
    }
    value
}

/// Wrap results array from a list/search endpoint.
pub(super) fn wrap_crm_results(results: Vec<Value>, source: &str) -> Vec<Value> {
    results
        .into_iter()
        .map(|r| wrap_crm_properties(r, source))
        .collect()
}

/// Build query params for list endpoints — limit and after cursor.
pub(super) fn list_query_params(input: &Value, max_results: u64) -> Vec<(&'static str, String)> {
    let mut query = Vec::new();
    let limit = clamp_limit(coerce_to_u64(&input["limit"]), max_results);
    query.push(("limit", limit.to_string()));
    if let Some(after) = input["after"].as_str() {
        query.push(("after", after.to_string()));
    }
    query
}

/// Append `properties` query params (HubSpot only returns defaults unless
/// you explicitly request others).
pub(super) fn append_properties_query(query: &mut Vec<(&str, String)>, input: &Value) {
    if let Some(props) = input["properties"].as_array() {
        for prop in props {
            if let Some(s) = prop.as_str() {
                query.push(("properties", s.to_string()));
            }
        }
    }
}

/// Extract paging info from HubSpot response.
pub(super) fn extract_paging(resp: &Value) -> Value {
    resp.get("paging").cloned().unwrap_or(Value::Null)
}

#[derive(Clone)]
pub(super) struct Ctx {
    pub(super) clients: Arc<HashMap<String, Arc<HubspotClient>>>,
    pub(super) default_portal: String,
}

impl Ctx {
    fn client(&self, input: &Value) -> Result<Arc<HubspotClient>> {
        resolve_client(&self.clients, &self.default_portal, input)
    }
    pub(super) fn portal_prop(&self) -> Value {
        portal_schema_prop(&self.clients, &self.default_portal)
    }
}

macro_rules! hubspot_tool {
    ($struct:ident) => {
        pub struct $struct {
            ctx: $crate::adapters::hubspot::tools::Ctx,
        }
        impl $struct {
            pub fn new(
                clients: ::std::sync::Arc<
                    ::std::collections::HashMap<
                        String,
                        ::std::sync::Arc<$crate::adapters::hubspot::client::HubspotClient>,
                    >,
                >,
                default_portal: String,
            ) -> Self {
                Self {
                    ctx: $crate::adapters::hubspot::tools::Ctx {
                        clients,
                        default_portal,
                    },
                }
            }
        }
    };
}
pub(super) use hubspot_tool;

pub(super) async fn run<F, Fut>(ctx: &Ctx, input: Value, f: F) -> Result<ToolResult>
where
    F: FnOnce(Arc<HubspotClient>, Value) -> Fut,
    Fut: std::future::Future<Output = Result<Value>>,
{
    let start = std::time::Instant::now();
    let client = match ctx.client(&input) {
        Ok(c) => c,
        Err(e) => {
            return Ok(ToolResult::failure(
                e.to_string(),
                start.elapsed().as_millis() as u64,
            ))
        }
    };
    match f(client, input).await {
        Ok(output) => Ok(ToolResult::success(
            output,
            start.elapsed().as_millis() as u64,
        )),
        Err(e) => {
            warn!(error = %e, "HubSpot tool failed");
            Ok(ToolResult::from_report(
                &e,
                start.elapsed().as_millis() as u64,
            ))
        }
    }
}

// ── hubspot.contact_list ────────────────────────────────────────────

hubspot_tool!(ContactListTool);

#[async_trait]
impl Tool for ContactListTool {
    fn name(&self) -> &str {
        "hubspot.contact_list"
    }
    fn description(&self) -> &str {
        "List CRM contacts in creation-date-ascending order (unfiltered, returns default properties unless `properties` is specified). For filter/sort/order (e.g. modified-since, by owner) use `hubspot.crm_search` or `hubspot.contact_search`."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": { "type": "integer", "description": "Max contacts to return (default/max 100)", "minimum": 1, "maximum": 100 },
                "after": { "type": "string", "description": "Pagination cursor from previous response" },
                "properties": { "type": "array", "items": { "type": "string" }, "description": "Property names to include (e.g. [\"email\", \"firstname\", \"lastname\"])" },
                "portal": self.ctx.portal_prop(),
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let mut query = list_query_params(&input, client.max_results);
            append_properties_query(&mut query, &input);
            let resp = client.get("/crm/v3/objects/contacts", &query).await?;
            let results = resp["results"].as_array().cloned().unwrap_or_default();
            let wrapped = wrap_crm_results(results, "hubspot_contact");
            let paging = extract_paging(&resp);
            info!(count = wrapped.len(), "Listed HubSpot contacts");
            Ok(json!({ "count": wrapped.len(), "contacts": wrapped, "paging": paging }))
        })
        .await
    }
}

// ── hubspot.contact_get ─────────────────────────────────────────────

hubspot_tool!(ContactGetTool);

#[async_trait]
impl Tool for ContactGetTool {
    fn name(&self) -> &str {
        "hubspot.contact_get"
    }
    fn description(&self) -> &str {
        "Get a single CRM contact by ID."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Contact ID" },
                "properties": { "type": "array", "items": { "type": "string" }, "description": "Property names to include" },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_id(&input["id"])?;
            let mut query = Vec::new();
            append_properties_query(&mut query, &input);
            let contact = client
                .get(&format!("/crm/v3/objects/contacts/{id}"), &query)
                .await?;
            Ok(wrap_crm_properties(contact, "hubspot_contact"))
        })
        .await
    }
}

// ── hubspot.contact_create ──────────────────────────────────────────

hubspot_tool!(ContactCreateTool);

#[async_trait]
impl Tool for ContactCreateTool {
    fn name(&self) -> &str {
        "hubspot.contact_create"
    }
    fn description(&self) -> &str {
        "MUTATING: create a new CRM contact."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "properties": {
                    "type": "object",
                    "description": "Contact properties (e.g. {\"email\": \"...\", \"firstname\": \"...\", \"lastname\": \"...\"})"
                },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["properties"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let properties = input
                .get("properties")
                .filter(|v| v.is_object())
                .cloned()
                .ok_or_else(|| eyre::eyre!("Missing 'properties' object"))?;
            let body = json!({ "properties": properties });
            let result = client.post("/crm/v3/objects/contacts", body).await?;
            info!("Created HubSpot contact");
            Ok(result)
        })
        .await
    }
}

// ── hubspot.contact_update ──────────────────────────────────────────

hubspot_tool!(ContactUpdateTool);

#[async_trait]
impl Tool for ContactUpdateTool {
    fn name(&self) -> &str {
        "hubspot.contact_update"
    }
    fn description(&self) -> &str {
        "MUTATING: update an existing CRM contact. Include only properties to change."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Contact ID" },
                "properties": {
                    "type": "object",
                    "description": "Properties to update"
                },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["id", "properties"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_id(&input["id"])?;
            let properties = input
                .get("properties")
                .filter(|v| v.is_object())
                .cloned()
                .ok_or_else(|| eyre::eyre!("Missing 'properties' object"))?;
            let body = json!({ "properties": properties });
            let result = client
                .patch(&format!("/crm/v3/objects/contacts/{id}"), body)
                .await?;
            info!(contact_id = %id, "Updated HubSpot contact");
            Ok(result)
        })
        .await
    }
}

// ── hubspot.contact_search ──────────────────────────────────────────

hubspot_tool!(ContactSearchTool);

#[async_trait]
impl Tool for ContactSearchTool {
    fn name(&self) -> &str {
        "hubspot.contact_search"
    }
    fn description(&self) -> &str {
        "Search CRM contacts by query string or filter groups."
    }
    fn input_schema(&self) -> Value {
        search_schema("contacts", &self.ctx.portal_prop())
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let body = build_search_body(&input, client.max_results);
            let resp = client.post("/crm/v3/objects/contacts/search", body).await?;
            let results = resp["results"].as_array().cloned().unwrap_or_default();
            let wrapped = wrap_crm_results(results, "hubspot_contact");
            let paging = extract_paging(&resp);
            info!(count = wrapped.len(), "Searched HubSpot contacts");
            Ok(json!({ "count": wrapped.len(), "contacts": wrapped, "paging": paging }))
        })
        .await
    }
}

// ── hubspot.company_list ────────────────────────────────────────────

hubspot_tool!(CompanyListTool);

#[async_trait]
impl Tool for CompanyListTool {
    fn name(&self) -> &str {
        "hubspot.company_list"
    }
    fn description(&self) -> &str {
        "List CRM companies in creation-date-ascending order (unfiltered, returns default properties unless `properties` is specified). For filter/sort/order (e.g. modified-since, by industry) use `hubspot.crm_search` or `hubspot.company_search`."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": { "type": "integer", "description": "Max companies to return (default/max 100)", "minimum": 1, "maximum": 100 },
                "after": { "type": "string", "description": "Pagination cursor" },
                "properties": { "type": "array", "items": { "type": "string" }, "description": "Property names to include" },
                "portal": self.ctx.portal_prop(),
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let mut query = list_query_params(&input, client.max_results);
            append_properties_query(&mut query, &input);
            let resp = client.get("/crm/v3/objects/companies", &query).await?;
            let results = resp["results"].as_array().cloned().unwrap_or_default();
            let wrapped = wrap_crm_results(results, "hubspot_company");
            let paging = extract_paging(&resp);
            info!(count = wrapped.len(), "Listed HubSpot companies");
            Ok(json!({ "count": wrapped.len(), "companies": wrapped, "paging": paging }))
        })
        .await
    }
}

// ── hubspot.company_get ─────────────────────────────────────────────

hubspot_tool!(CompanyGetTool);

#[async_trait]
impl Tool for CompanyGetTool {
    fn name(&self) -> &str {
        "hubspot.company_get"
    }
    fn description(&self) -> &str {
        "Get a single CRM company by ID."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Company ID" },
                "properties": { "type": "array", "items": { "type": "string" }, "description": "Property names to include" },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_id(&input["id"])?;
            let mut query = Vec::new();
            append_properties_query(&mut query, &input);
            let company = client
                .get(&format!("/crm/v3/objects/companies/{id}"), &query)
                .await?;
            Ok(wrap_crm_properties(company, "hubspot_company"))
        })
        .await
    }
}

// ── hubspot.company_create ──────────────────────────────────────────

hubspot_tool!(CompanyCreateTool);

#[async_trait]
impl Tool for CompanyCreateTool {
    fn name(&self) -> &str {
        "hubspot.company_create"
    }
    fn description(&self) -> &str {
        "MUTATING: create a new CRM company."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "properties": {
                    "type": "object",
                    "description": "Company properties (e.g. {\"name\": \"...\", \"domain\": \"...\"})"
                },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["properties"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let properties = input
                .get("properties")
                .filter(|v| v.is_object())
                .cloned()
                .ok_or_else(|| eyre::eyre!("Missing 'properties' object"))?;
            let body = json!({ "properties": properties });
            let result = client.post("/crm/v3/objects/companies", body).await?;
            info!("Created HubSpot company");
            Ok(result)
        })
        .await
    }
}

// ── hubspot.company_update ──────────────────────────────────────────

hubspot_tool!(CompanyUpdateTool);

#[async_trait]
impl Tool for CompanyUpdateTool {
    fn name(&self) -> &str {
        "hubspot.company_update"
    }
    fn description(&self) -> &str {
        "MUTATING: update an existing CRM company. Include only properties to change."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Company ID" },
                "properties": { "type": "object", "description": "Properties to update" },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["id", "properties"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_id(&input["id"])?;
            let properties = input
                .get("properties")
                .filter(|v| v.is_object())
                .cloned()
                .ok_or_else(|| eyre::eyre!("Missing 'properties' object"))?;
            let body = json!({ "properties": properties });
            let result = client
                .patch(&format!("/crm/v3/objects/companies/{id}"), body)
                .await?;
            info!(company_id = %id, "Updated HubSpot company");
            Ok(result)
        })
        .await
    }
}

// ── hubspot.company_search ──────────────────────────────────────────

hubspot_tool!(CompanySearchTool);

#[async_trait]
impl Tool for CompanySearchTool {
    fn name(&self) -> &str {
        "hubspot.company_search"
    }
    fn description(&self) -> &str {
        "Search CRM companies by query string or filter groups."
    }
    fn input_schema(&self) -> Value {
        search_schema("companies", &self.ctx.portal_prop())
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let body = build_search_body(&input, client.max_results);
            let resp = client
                .post("/crm/v3/objects/companies/search", body)
                .await?;
            let results = resp["results"].as_array().cloned().unwrap_or_default();
            let wrapped = wrap_crm_results(results, "hubspot_company");
            let paging = extract_paging(&resp);
            info!(count = wrapped.len(), "Searched HubSpot companies");
            Ok(json!({ "count": wrapped.len(), "companies": wrapped, "paging": paging }))
        })
        .await
    }
}

// ── hubspot.deal_list ───────────────────────────────────────────────

hubspot_tool!(DealListTool);

#[async_trait]
impl Tool for DealListTool {
    fn name(&self) -> &str {
        "hubspot.deal_list"
    }
    fn description(&self) -> &str {
        "List CRM deals in creation-date-ascending order (unfiltered, returns default properties unless `properties` is specified). For filter/sort/order (e.g. modified-since, by stage, by amount) use `hubspot.crm_search` or `hubspot.deal_search`."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": { "type": "integer", "description": "Max deals to return (default/max 100)", "minimum": 1, "maximum": 100 },
                "after": { "type": "string", "description": "Pagination cursor" },
                "properties": { "type": "array", "items": { "type": "string" }, "description": "Property names to include" },
                "portal": self.ctx.portal_prop(),
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let mut query = list_query_params(&input, client.max_results);
            append_properties_query(&mut query, &input);
            let resp = client.get("/crm/v3/objects/deals", &query).await?;
            let results = resp["results"].as_array().cloned().unwrap_or_default();
            let wrapped = wrap_crm_results(results, "hubspot_deal");
            let paging = extract_paging(&resp);
            info!(count = wrapped.len(), "Listed HubSpot deals");
            Ok(json!({ "count": wrapped.len(), "deals": wrapped, "paging": paging }))
        })
        .await
    }
}

// ── hubspot.deal_get ────────────────────────────────────────────────

hubspot_tool!(DealGetTool);

#[async_trait]
impl Tool for DealGetTool {
    fn name(&self) -> &str {
        "hubspot.deal_get"
    }
    fn description(&self) -> &str {
        "Get a single CRM deal by ID."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Deal ID" },
                "properties": { "type": "array", "items": { "type": "string" }, "description": "Property names to include" },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_id(&input["id"])?;
            let mut query = Vec::new();
            append_properties_query(&mut query, &input);
            let deal = client
                .get(&format!("/crm/v3/objects/deals/{id}"), &query)
                .await?;
            Ok(wrap_crm_properties(deal, "hubspot_deal"))
        })
        .await
    }
}

// ── hubspot.deal_create ─────────────────────────────────────────────

hubspot_tool!(DealCreateTool);

#[async_trait]
impl Tool for DealCreateTool {
    fn name(&self) -> &str {
        "hubspot.deal_create"
    }
    fn description(&self) -> &str {
        "MUTATING: create a new CRM deal."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "properties": {
                    "type": "object",
                    "description": "Deal properties (e.g. {\"dealname\": \"...\", \"dealstage\": \"...\", \"amount\": \"...\", \"pipeline\": \"...\"})"
                },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["properties"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let properties = input
                .get("properties")
                .filter(|v| v.is_object())
                .cloned()
                .ok_or_else(|| eyre::eyre!("Missing 'properties' object"))?;
            let body = json!({ "properties": properties });
            let result = client.post("/crm/v3/objects/deals", body).await?;
            info!("Created HubSpot deal");
            Ok(result)
        })
        .await
    }
}

// ── hubspot.deal_update ─────────────────────────────────────────────

hubspot_tool!(DealUpdateTool);

#[async_trait]
impl Tool for DealUpdateTool {
    fn name(&self) -> &str {
        "hubspot.deal_update"
    }
    fn description(&self) -> &str {
        "MUTATING: update an existing CRM deal. Include only properties to change."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Deal ID" },
                "properties": { "type": "object", "description": "Properties to update" },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["id", "properties"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_id(&input["id"])?;
            let properties = input
                .get("properties")
                .filter(|v| v.is_object())
                .cloned()
                .ok_or_else(|| eyre::eyre!("Missing 'properties' object"))?;
            let body = json!({ "properties": properties });
            let result = client
                .patch(&format!("/crm/v3/objects/deals/{id}"), body)
                .await?;
            info!(deal_id = %id, "Updated HubSpot deal");
            Ok(result)
        })
        .await
    }
}

// ── hubspot.deal_search ─────────────────────────────────────────────

hubspot_tool!(DealSearchTool);

#[async_trait]
impl Tool for DealSearchTool {
    fn name(&self) -> &str {
        "hubspot.deal_search"
    }
    fn description(&self) -> &str {
        "Search CRM deals by query string or filter groups."
    }
    fn input_schema(&self) -> Value {
        search_schema("deals", &self.ctx.portal_prop())
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let body = build_search_body(&input, client.max_results);
            let resp = client.post("/crm/v3/objects/deals/search", body).await?;
            let results = resp["results"].as_array().cloned().unwrap_or_default();
            let wrapped = wrap_crm_results(results, "hubspot_deal");
            let paging = extract_paging(&resp);
            info!(count = wrapped.len(), "Searched HubSpot deals");
            Ok(json!({ "count": wrapped.len(), "deals": wrapped, "paging": paging }))
        })
        .await
    }
}

// ── hubspot.ticket_list ─────────────────────────────────────────────

hubspot_tool!(TicketListTool);

#[async_trait]
impl Tool for TicketListTool {
    fn name(&self) -> &str {
        "hubspot.ticket_list"
    }
    fn description(&self) -> &str {
        "List CRM tickets in creation-date-ascending order (unfiltered, returns default properties unless `properties` is specified). For filter/sort/order (e.g. modified-since, by stage, by priority) use `hubspot.crm_search` with `object_type: \"tickets\"`."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": { "type": "integer", "description": "Max tickets to return (default/max 100)", "minimum": 1, "maximum": 100 },
                "after": { "type": "string", "description": "Pagination cursor" },
                "properties": { "type": "array", "items": { "type": "string" }, "description": "Property names to include" },
                "portal": self.ctx.portal_prop(),
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let mut query = list_query_params(&input, client.max_results);
            append_properties_query(&mut query, &input);
            let resp = client.get("/crm/v3/objects/tickets", &query).await?;
            let results = resp["results"].as_array().cloned().unwrap_or_default();
            let wrapped = wrap_crm_results(results, "hubspot_ticket");
            let paging = extract_paging(&resp);
            info!(count = wrapped.len(), "Listed HubSpot tickets");
            Ok(json!({ "count": wrapped.len(), "tickets": wrapped, "paging": paging }))
        })
        .await
    }
}

// ── hubspot.ticket_get ──────────────────────────────────────────────

hubspot_tool!(TicketGetTool);

#[async_trait]
impl Tool for TicketGetTool {
    fn name(&self) -> &str {
        "hubspot.ticket_get"
    }
    fn description(&self) -> &str {
        "Get a single CRM ticket by ID."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Ticket ID" },
                "properties": { "type": "array", "items": { "type": "string" }, "description": "Property names to include" },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_id(&input["id"])?;
            let mut query = Vec::new();
            append_properties_query(&mut query, &input);
            let ticket = client
                .get(&format!("/crm/v3/objects/tickets/{id}"), &query)
                .await?;
            Ok(wrap_crm_properties(ticket, "hubspot_ticket"))
        })
        .await
    }
}

// ── hubspot.ticket_create ───────────────────────────────────────────

hubspot_tool!(TicketCreateTool);

#[async_trait]
impl Tool for TicketCreateTool {
    fn name(&self) -> &str {
        "hubspot.ticket_create"
    }
    fn description(&self) -> &str {
        "MUTATING: create a new CRM ticket."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "properties": {
                    "type": "object",
                    "description": "Ticket properties (e.g. {\"subject\": \"...\", \"hs_pipeline\": \"...\", \"hs_pipeline_stage\": \"...\"})"
                },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["properties"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let properties = input
                .get("properties")
                .filter(|v| v.is_object())
                .cloned()
                .ok_or_else(|| eyre::eyre!("Missing 'properties' object"))?;
            let body = json!({ "properties": properties });
            let result = client.post("/crm/v3/objects/tickets", body).await?;
            info!("Created HubSpot ticket");
            Ok(result)
        })
        .await
    }
}

// ── hubspot.ticket_update ───────────────────────────────────────────

hubspot_tool!(TicketUpdateTool);

#[async_trait]
impl Tool for TicketUpdateTool {
    fn name(&self) -> &str {
        "hubspot.ticket_update"
    }
    fn description(&self) -> &str {
        "MUTATING: update an existing CRM ticket. Include only properties to change."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Ticket ID" },
                "properties": { "type": "object", "description": "Properties to update" },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["id", "properties"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_id(&input["id"])?;
            let properties = input
                .get("properties")
                .filter(|v| v.is_object())
                .cloned()
                .ok_or_else(|| eyre::eyre!("Missing 'properties' object"))?;
            let body = json!({ "properties": properties });
            let result = client
                .patch(&format!("/crm/v3/objects/tickets/{id}"), body)
                .await?;
            info!(ticket_id = %id, "Updated HubSpot ticket");
            Ok(result)
        })
        .await
    }
}

// ── hubspot.pipeline_list ───────────────────────────────────────────

hubspot_tool!(PipelineListTool);

#[async_trait]
impl Tool for PipelineListTool {
    fn name(&self) -> &str {
        "hubspot.pipeline_list"
    }
    fn description(&self) -> &str {
        "List pipelines (and their stages) for deals or tickets."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "object_type": {
                    "type": "string",
                    "description": "Object type: 'deals' or 'tickets'",
                    "enum": ["deals", "tickets"]
                },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["object_type"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let object_type = input["object_type"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("Missing 'object_type' parameter"))?;
            match object_type {
                "deals" | "tickets" => {}
                _ => return Err(eyre::eyre!("object_type must be 'deals' or 'tickets'")),
            }
            let resp = client
                .get(&format!("/crm/v3/pipelines/{object_type}"), &[])
                .await?;
            let pipelines = resp["results"].as_array().cloned().unwrap_or_default();
            info!(
                count = pipelines.len(),
                object_type, "Listed HubSpot pipelines"
            );
            Ok(json!({ "count": pipelines.len(), "pipelines": pipelines }))
        })
        .await
    }
}

// ── hubspot.owner_list ──────────────────────────────────────────────

hubspot_tool!(OwnerListTool);

#[async_trait]
impl Tool for OwnerListTool {
    fn name(&self) -> &str {
        "hubspot.owner_list"
    }
    fn description(&self) -> &str {
        "List HubSpot owners (users who can be assigned to CRM records)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": { "type": "integer", "description": "Max owners to return (default/max 100)", "minimum": 1, "maximum": 100 },
                "after": { "type": "string", "description": "Pagination cursor" },
                "email": { "type": "string", "description": "Filter by email address" },
                "portal": self.ctx.portal_prop(),
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let mut query = list_query_params(&input, client.max_results);
            if let Some(email) = input["email"].as_str() {
                query.push(("email", email.to_string()));
            }
            let resp = client.get("/crm/v3/owners/", &query).await?;
            let owners = resp["results"].as_array().cloned().unwrap_or_default();
            let paging = extract_paging(&resp);
            info!(count = owners.len(), "Listed HubSpot owners");
            Ok(json!({ "count": owners.len(), "owners": owners, "paging": paging }))
        })
        .await
    }
}

// ── hubspot.property_list ───────────────────────────────────────────

hubspot_tool!(PropertyListTool);

#[async_trait]
impl Tool for PropertyListTool {
    fn name(&self) -> &str {
        "hubspot.property_list"
    }
    fn description(&self) -> &str {
        "List property definitions for a CRM object type (any of contacts, companies, deals, tickets, notes, tasks, calls, emails, meetings, communications, or a custom object). Use this to discover available properties before creating/updating/searching records; `hubspot.property_get` reads one in full."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "object_type": super::objects::object_type_prop("CRM object type"),
                "portal": self.ctx.portal_prop(),
            },
            "required": ["object_type"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let object_type = super::objects::object_type(&input)?;
            let resp = client
                .get(&format!("/crm/v3/properties/{object_type}"), &[])
                .await?;
            let properties = resp["results"].as_array().cloned().unwrap_or_default();
            let slim: Vec<Value> = properties
                .into_iter()
                .map(|p| {
                    let mut out = json!({
                        "name": p["name"],
                        "label": p["label"],
                        "type": p["type"],
                        "fieldType": p["fieldType"],
                        "groupName": p["groupName"],
                    });
                    if let Some(opts) = p.get("options").filter(|v| v.is_array()) {
                        out["options"] = opts.clone();
                    }
                    out
                })
                .collect();
            info!(count = slim.len(), object_type, "Listed HubSpot properties");
            Ok(json!({ "count": slim.len(), "properties": slim }))
        })
        .await
    }
}

// ── hubspot.note_create ─────────────────────────────────────────────

hubspot_tool!(NoteCreateTool);

/// Well-known HubSpot association type IDs for notes.
fn note_association_type_id(object_type: &str) -> Option<i64> {
    match object_type {
        "contacts" | "contact" => Some(202),
        "companies" | "company" => Some(190),
        "deals" | "deal" => Some(214),
        // 228 = HUBSPOT_DEFINED Note→Ticket. (206 is Call→Ticket — wrong
        // "from" object type, rejected with a 0-46-vs-0-48 VALIDATION_ERROR.)
        "tickets" | "ticket" => Some(228),
        _ => None,
    }
}

#[async_trait]
impl Tool for NoteCreateTool {
    fn name(&self) -> &str {
        "hubspot.note_create"
    }
    fn description(&self) -> &str {
        "MUTATING: create a note and optionally associate it with contacts, companies, deals, or tickets. The body may be simple HTML (p, br, strong, em, ul/ol/li, a, code, blockquote). Read, list, edit and remove notes with hubspot.note_get, hubspot.note_list, hubspot.note_update and hubspot.note_delete."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "body": { "type": "string", "description": "The note body: plain text or simple HTML" },
                "associations": {
                    "type": "array",
                    "description": "Objects to associate with. Each item: {\"to_id\": \"123\", \"to_type\": \"contacts\"}",
                    "items": {
                        "type": "object",
                        "properties": {
                            "to_id": { "type": "string", "description": "Target object ID" },
                            "to_type": { "type": "string", "description": "Target object type: contacts, companies, deals, or tickets" }
                        },
                        "required": ["to_id", "to_type"]
                    }
                },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["body"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let body_text = input["body"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("Missing 'body' parameter"))?;
            let timestamp = chrono::Utc::now().to_rfc3339();

            let mut body = json!({
                "properties": {
                    "hs_note_body": body_text,
                    "hs_timestamp": timestamp,
                }
            });

            if let Some(assocs) = input["associations"].as_array() {
                let mut associations = Vec::new();
                for a in assocs {
                    let to_id = coerce_id(&a["to_id"])
                        .map_err(|_| eyre::eyre!("association missing 'to_id'"))?;
                    let to_type = a["to_type"]
                        .as_str()
                        .ok_or_else(|| eyre::eyre!("association missing 'to_type'"))?;
                    let type_id = note_association_type_id(to_type).ok_or_else(|| {
                        eyre::eyre!(
                            "Unknown association type: '{to_type}'. Use: contacts, companies, deals, or tickets"
                        )
                    })?;
                    associations.push(json!({
                        "to": { "id": to_id },
                        "types": [{
                            "associationCategory": "HUBSPOT_DEFINED",
                            "associationTypeId": type_id,
                        }]
                    }));
                }
                body["associations"] = json!(associations);
            }

            let result = client.post("/crm/v3/objects/notes", body).await?;
            info!("Created HubSpot note");
            Ok(result)
        })
        .await
    }
}

// ── hubspot.crm_search ──────────────────────────────────────────────

hubspot_tool!(CrmSearchTool);

#[async_trait]
impl Tool for CrmSearchTool {
    fn name(&self) -> &str {
        "hubspot.crm_search"
    }
    fn description(&self) -> &str {
        "Search any CRM object type — contacts, companies, deals, tickets, notes, tasks, calls, emails, meetings, communications, or a custom object — with filter groups, sorts, and pagination. Use this (not `*_list`) whenever results must be filtered, sorted, or ordered other than creation-date-ascending. Engagements on a record: filter on `associations.ticket` (or `.contact`, `.company`, `.deal`) EQ the record id. Discover filterable property names via `hubspot.property_list`."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "object_type": super::objects::object_type_prop("CRM object type to search"),
                "query": { "type": "string", "description": "Text to search for" },
                "filter_groups": {
                    "type": "array",
                    "description": "HubSpot filter groups for advanced filtering",
                    "items": { "type": "object" }
                },
                "sorts": {
                    "type": "array",
                    "description": "Sort criteria",
                    "items": { "type": "object" }
                },
                "properties": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Property names to include in results"
                },
                "limit": { "type": "integer", "description": "Max results (default/max 100)", "minimum": 1, "maximum": 100 },
                "after": { "type": "string", "description": "Pagination cursor" },
                "portal": self.ctx.portal_prop(),
            },
            "required": ["object_type"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let object_type = super::objects::object_type(&input)?;
            let body = build_search_body(&input, client.max_results);
            let resp = client
                .post(&format!("/crm/v3/objects/{object_type}/search"), body)
                .await?;
            let results = resp["results"].as_array().cloned().unwrap_or_default();
            let source = format!("hubspot_{object_type}");
            let wrapped = wrap_crm_results(results, &source);
            let paging = extract_paging(&resp);
            info!(count = wrapped.len(), object_type, "HubSpot CRM search");
            Ok(json!({ "count": wrapped.len(), "results": wrapped, "paging": paging }))
        })
        .await
    }
}

// ── shared search helpers ───────────────────────────────────────────

fn search_schema(object_type: &str, portal_prop: &Value) -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": format!("Text to search for in {object_type}") },
            "filter_groups": {
                "type": "array",
                "description": "HubSpot filter groups for advanced filtering. Each group: {\"filters\": [{\"propertyName\": \"...\", \"operator\": \"EQ\", \"value\": \"...\"}]}",
                "items": { "type": "object" }
            },
            "sorts": {
                "type": "array",
                "description": "Sort criteria: [{\"propertyName\": \"...\", \"direction\": \"ASCENDING\"}]",
                "items": { "type": "object" }
            },
            "properties": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Property names to include in results"
            },
            "limit": { "type": "integer", "description": "Max results (default/max 100)", "minimum": 1, "maximum": 100 },
            "after": { "type": "string", "description": "Pagination cursor" },
            "portal": portal_prop.clone(),
        }
    })
}

pub(super) fn build_search_body(input: &Value, max_results: u64) -> Value {
    let limit = clamp_limit(coerce_to_u64(&input["limit"]), max_results);
    let mut body = json!({ "limit": limit });

    if let Some(q) = input["query"].as_str() {
        body["query"] = json!(q);
    }
    if let Some(fg) = input.get("filter_groups").filter(|v| v.is_array()) {
        body["filterGroups"] = fg.clone();
    }
    if let Some(sorts) = input.get("sorts").filter(|v| v.is_array()) {
        body["sorts"] = sorts.clone();
    }
    if let Some(props) = input.get("properties").filter(|v| v.is_array()) {
        body["properties"] = props.clone();
    }
    if let Some(after) = input["after"].as_str() {
        body["after"] = json!(after);
    }
    body
}

/// A record id from a string or integer input. An id is one path segment:
/// nothing in it may open a query, a fragment, an escape or another segment,
/// or the tool would call an endpoint of the caller's choosing.
pub(super) fn coerce_id(val: &Value) -> eyre::Result<String> {
    let id = match val {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        _ => return Err(eyre::eyre!("Missing or invalid 'id' parameter")),
    };
    if id.is_empty()
        || id.contains(['/', '\\', '?', '#', '%', '.'])
        || id.chars().any(char::is_whitespace)
    {
        return Err(eyre::eyre!("'{id}' is not a HubSpot record id"));
    }
    Ok(id)
}

/// `associations=a,b` from the tool's `associations` array, so a read
/// carries the ids of the records linked to it.
pub(super) fn append_associations_query(query: &mut Vec<(&str, String)>, input: &Value) {
    let names: Vec<&str> = input["associations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if !names.is_empty() {
        query.push(("associations", names.join(",")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_description_single() {
        let mut clients = HashMap::new();
        clients.insert(
            "default".to_string(),
            Arc::new(HubspotClient::new("pat-test", 100).unwrap()),
        );
        let desc = portal_description(&clients, "default");
        assert!(desc.contains("multi-portal"));
    }

    #[test]
    fn portal_description_multi() {
        let mut clients = HashMap::new();
        clients.insert(
            "prod".to_string(),
            Arc::new(HubspotClient::new("pat-prod", 100).unwrap()),
        );
        clients.insert(
            "sandbox".to_string(),
            Arc::new(HubspotClient::new("pat-sandbox", 100).unwrap()),
        );
        let desc = portal_description(&clients, "prod");
        assert!(desc.contains("prod"));
        assert!(desc.contains("sandbox"));
        assert!(desc.contains("Default: prod"));
        assert!(portal_description(&clients, "gone").contains("Default: none — pass portal"));
    }

    #[test]
    fn resolve_client_explicit_unknown() {
        let mut clients = HashMap::new();
        clients.insert(
            "prod".to_string(),
            Arc::new(HubspotClient::new("pat-prod", 100).unwrap()),
        );
        let err = resolve_client(&clients, "prod", &json!({"portal": "nope"}))
            .err()
            .unwrap();
        assert!(err.to_string().contains("Unknown HubSpot portal"));
    }

    #[test]
    fn wrap_crm_properties_wraps_nonempty() {
        let obj = json!({
            "id": "123",
            "properties": {
                "firstname": "Alice",
                "lastname": "Bob",
                "empty_field": ""
            }
        });
        let wrapped = wrap_crm_properties(obj, "test");
        let props = wrapped["properties"].as_object().unwrap();
        assert_ne!(props["firstname"].as_str().unwrap(), "Alice");
        assert_ne!(props["lastname"].as_str().unwrap(), "Bob");
        assert_eq!(props["empty_field"].as_str().unwrap(), "");
    }

    #[test]
    fn coerce_id_string_and_int() {
        assert_eq!(coerce_id(&json!("123")).unwrap(), "123");
        assert_eq!(coerce_id(&json!(456)).unwrap(), "456");
        assert!(coerce_id(&json!(null)).is_err());
    }

    #[test]
    fn an_id_is_one_path_segment() {
        for bad in [
            "", " ", "1/2", "../x", "1?x=y", "1#f", "%2e", "1 2", "a.b", "1\\2",
        ] {
            assert!(coerce_id(&json!(bad)).is_err(), "{bad:?}");
        }
        assert_eq!(coerce_id(&json!(" 42 ")).unwrap(), "42");
        assert!(coerce_id(&json!(null)).is_err());
    }

    #[test]
    fn build_search_body_basic() {
        let input = json!({"query": "test", "limit": 50});
        let body = build_search_body(&input, 100);
        assert_eq!(body["query"], "test");
        assert_eq!(body["limit"], 50);
    }

    #[test]
    fn build_search_body_with_filters() {
        let input = json!({
            "filter_groups": [{"filters": [{"propertyName": "email", "operator": "EQ", "value": "test@example.com"}]}],
            "properties": ["email", "firstname"]
        });
        let body = build_search_body(&input, 100);
        assert!(body["filterGroups"].is_array());
        assert!(body["properties"].is_array());
    }

    #[test]
    fn note_association_type_id_valid() {
        assert_eq!(note_association_type_id("contacts"), Some(202));
        assert_eq!(note_association_type_id("contact"), Some(202));
        assert_eq!(note_association_type_id("companies"), Some(190));
        assert_eq!(note_association_type_id("deals"), Some(214));
        assert_eq!(note_association_type_id("tickets"), Some(228));
        assert_eq!(note_association_type_id("unknown"), None);
    }
}
