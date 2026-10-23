//! Xero agent-callable tools.
//!
//! Every tool accepts an optional `entity` parameter selecting which Xero
//! organisation to target (explicit → default). Read tools default to `allow`;
//! mutating tools default to `require_approval` so the agent proposes and a
//! human confirms before anything touches the ledger. All response bodies are
//! routed through `wrap_external_content` to neutralise prompt injection in
//! attacker-influenceable fields (contact names, line descriptions, references).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::{eyre, Result};
use futures::future::BoxFuture;
use futures::FutureExt;
use serde_json::{json, Value};
use tracing::warn;

use workforce_agent::security::external_content::wrap_external_content;
use workforce_agent::tools::{Tool, ToolResult};

use super::client::XeroClient;
use super::config::{clamp_page, clamp_page_size};
use crate::adapters::utils::{coerce_to_u64, instance_choices};

type Clients = Arc<HashMap<String, Arc<XeroClient>>>;
type Handler =
    Box<dyn Fn(Arc<XeroClient>, Value) -> BoxFuture<'static, Result<Value>> + Send + Sync>;

#[derive(Clone)]
struct Ctx {
    clients: Clients,
    default_entity: String,
}

impl Ctx {
    fn client(&self, input: &Value) -> Result<Arc<XeroClient>> {
        let name = workforce_agent::llm::environment::resolve_instance(
            "xero",
            input["entity"].as_str(),
            &self.default_entity,
            &*self.clients,
        )
        .map_err(|d| eyre!("{}", d.message))?;
        self.clients.get(&name).cloned().ok_or_else(|| {
            let mut available: Vec<&str> = self.clients.keys().map(String::as_str).collect();
            available.sort_unstable();
            eyre!("Unknown Xero entity: '{name}'. Available: {available:?}")
        })
    }

    fn entity_prop(&self) -> Value {
        let description = if self.clients.len() <= 1 {
            "Xero entity name (for multi-entity setups)".to_string()
        } else {
            format!(
                "Xero entity. {}",
                instance_choices(&*self.clients, &self.default_entity, "entity")
            )
        };
        json!({ "type": "string", "description": description })
    }
}

/// Wrap and time a handler invocation, surfacing failures as `ToolResult`
/// errors rather than propagating — mirrors the other adapters' `run`.
async fn run<F, Fut>(ctx: &Ctx, input: Value, f: F) -> Result<ToolResult>
where
    F: FnOnce(Arc<XeroClient>, Value) -> Fut,
    Fut: std::future::Future<Output = Result<Value>>,
{
    let start = std::time::Instant::now();
    let client = match ctx.client(&input) {
        Ok(c) => c,
        Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
    };
    match f(client, input).await {
        Ok(output) => Ok(ToolResult::success(output, elapsed(start))),
        Err(e) => {
            warn!(error = %e, "Xero tool failed");
            Ok(ToolResult::from_report(&e, elapsed(start)))
        }
    }
}

fn elapsed(start: std::time::Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

/// Serialise a Xero response and wrap it so every cell that crosses the trust
/// boundary carries injection markers.
fn wrap_payload(value: Value, source: &str) -> Value {
    let rendered = serde_json::to_string(&value).unwrap_or_default();
    let wrapped = wrap_external_content(&rendered, source).wrapped_content;
    json!({ "source": source, "data": wrapped })
}

fn generic_entity_prop() -> Value {
    json!({ "type": "string", "description": "Xero entity name (for multi-entity setups)" })
}

/// Insert the `entity` property into a schema's `properties` block.
fn with_entity(mut schema: Value, entity: Value) -> Value {
    if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        props.insert("entity".to_string(), entity);
    }
    schema
}

/// Map snake_case input keys to Xero query keys, dropping absent ones.
pub(super) fn map_query(
    input: &Value,
    pairs: &[(&str, &'static str)],
) -> Vec<(&'static str, String)> {
    pairs
        .iter()
        .filter_map(|(input_key, xero_key)| {
            let v = &input[*input_key];
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.as_u64().map(|n| n.to_string()))
                .map(|s| (*xero_key, s))
        })
        .collect()
}

/// Standard paged-list query: `page` always, `pageSize` when supported, plus
/// optional `where`/`order` passthrough.
fn list_query(input: &Value, with_page_size: bool) -> Vec<(&'static str, String)> {
    let mut q = vec![(
        "page",
        clamp_page(coerce_to_u64(&input["page"])).to_string(),
    )];
    if with_page_size {
        q.push((
            "pageSize",
            clamp_page_size(coerce_to_u64(&input["page_size"])).to_string(),
        ));
    }
    if let Some(w) = input["where"].as_str() {
        q.push(("where", w.to_string()));
    }
    if let Some(o) = input["order"].as_str() {
        q.push(("order", o.to_string()));
    }
    q
}

fn require_str<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    input[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| eyre!("'{key}' is required"))
}

fn require_object<'a>(input: &'a Value, key: &str) -> Result<&'a Value> {
    let v = &input[key];
    if v.is_object() {
        Ok(v)
    } else {
        Err(eyre!(
            "'{key}' must be a JSON object matching the Xero payload"
        ))
    }
}

/// One agent-callable tool, generated from a [`ToolSpec`]. Sharing this struct
/// across every endpoint keeps the surface declarative — the spec table is the
/// single source of truth for names, schemas, policies, and behaviour.
struct XeroTool {
    ctx: Ctx,
    spec: ToolSpec,
}

#[async_trait]
impl Tool for XeroTool {
    fn name(&self) -> &str {
        self.spec.name
    }

    fn description(&self) -> &str {
        self.spec.description
    }

    fn input_schema(&self) -> Value {
        with_entity(self.spec.schema.clone(), self.ctx.entity_prop())
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| {
            (self.spec.handler)(client, input)
        })
        .await
    }
}

struct ToolSpec {
    name: &'static str,
    description: &'static str,
    schema: Value,
    require_approval: bool,
    handler: Handler,
}

impl ToolSpec {
    fn read(
        name: &'static str,
        description: &'static str,
        schema: Value,
        handler: Handler,
    ) -> Self {
        Self {
            name,
            description,
            schema,
            require_approval: false,
            handler,
        }
    }

    fn write(
        name: &'static str,
        description: &'static str,
        schema: Value,
        handler: Handler,
    ) -> Self {
        Self {
            name,
            description,
            schema,
            require_approval: true,
            handler,
        }
    }
}

fn obj(properties: Value) -> Value {
    json!({ "type": "object", "properties": properties })
}

fn obj_required(properties: Value, required: Value) -> Value {
    json!({ "type": "object", "properties": properties, "required": required })
}

fn paged_props(extra: Value) -> Value {
    let mut base = json!({
        "page": { "type": "integer", "minimum": 1, "description": "1-based page number" },
        "page_size": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Rows per page (max 100)" },
        "where": { "type": "string", "description": "Xero filter expression" },
        "order": { "type": "string", "description": "Xero order-by expression" },
    });
    if let (Some(base_obj), Some(extra_obj)) = (base.as_object_mut(), extra.as_object()) {
        for (k, v) in extra_obj {
            base_obj.insert(k.clone(), v.clone());
        }
    }
    base
}

fn body_prop() -> Value {
    json!({ "type": "object", "description": "Xero request payload (the element wrapper, e.g. {\"Invoices\": [...]})" })
}

/// The full Xero tool surface. Read tools default to `allow`; mutating tools to
/// `require_approval`. Tools tagged "(needs accounting.reports.read)" ship now
/// but return a scope error until that scope is added to the connection.
fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec::read(
            "xero.organisation",
            "Get the connected organisation's details (name, base currency, financial year end).",
            obj(json!({})),
            Box::new(|c, _input| {
                async move { Ok(wrap_payload(c.get("/Organisation", &[]).await?, "xero_organisation")) }
                    .boxed()
            }),
        ),
        ToolSpec::read(
            "xero.accounts_list",
            "List the chart of accounts. Optionally filter with a Xero `where` expression.",
            obj(json!({ "where": { "type": "string", "description": "Xero filter expression" } })),
            Box::new(|c, input| {
                async move {
                    let q = input["where"]
                        .as_str()
                        .map(|w| vec![("where", w.to_string())])
                        .unwrap_or_default();
                    Ok(wrap_payload(c.get("/Accounts", &q).await?, "xero_accounts"))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "xero.contacts_list",
            "List contacts (customers and suppliers), paged.",
            obj(paged_props(json!({}))),
            Box::new(|c, input| {
                async move {
                    let q = list_query(&input, true);
                    Ok(wrap_payload(c.get("/Contacts", &q).await?, "xero_contacts"))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "xero.invoices_list",
            "List invoices (ACCREC/ACCPAY), paged. Use `where` to filter by Status, Type, or DueDate \
             for AR-ageing and overdue analysis.",
            obj(paged_props(json!({}))),
            Box::new(|c, input| {
                async move {
                    let q = list_query(&input, true);
                    Ok(wrap_payload(c.get("/Invoices", &q).await?, "xero_invoices"))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "xero.bank_transactions_list",
            "List bank transactions, paged. Requires the accounting.transactions.read scope.",
            obj(paged_props(json!({}))),
            Box::new(|c, input| {
                async move {
                    let q = list_query(&input, true);
                    Ok(wrap_payload(
                        c.get("/BankTransactions", &q).await?,
                        "xero_bank_transactions",
                    ))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "xero.manual_journals_list",
            "List manual journals, paged.",
            obj(paged_props(json!({}))),
            Box::new(|c, input| {
                async move {
                    let q = list_query(&input, false);
                    Ok(wrap_payload(
                        c.get("/ManualJournals", &q).await?,
                        "xero_manual_journals",
                    ))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "xero.budgets_list",
            "List budgets.",
            obj(json!({})),
            Box::new(|c, _input| {
                async move { Ok(wrap_payload(c.get("/Budgets", &[]).await?, "xero_budgets")) }.boxed()
            }),
        ),
        ToolSpec::read(
            "xero.profit_and_loss",
            "Profit and Loss report. Params: from_date, to_date, periods, timeframe (MONTH|QUARTER|YEAR).",
            obj(json!({
                "from_date": { "type": "string", "description": "YYYY-MM-DD" },
                "to_date": { "type": "string", "description": "YYYY-MM-DD" },
                "periods": { "type": "integer" },
                "timeframe": { "type": "string", "enum": ["MONTH", "QUARTER", "YEAR"] },
            })),
            Box::new(|c, input| {
                async move {
                    let q = map_query(
                        &input,
                        &[
                            ("from_date", "fromDate"),
                            ("to_date", "toDate"),
                            ("periods", "periods"),
                            ("timeframe", "timeframe"),
                        ],
                    );
                    Ok(wrap_payload(
                        c.get("/Reports/ProfitAndLoss", &q).await?,
                        "xero_report_pnl",
                    ))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "xero.balance_sheet",
            "Balance Sheet report. Params: date, periods, timeframe (MONTH|QUARTER|YEAR).",
            obj(json!({
                "date": { "type": "string", "description": "YYYY-MM-DD" },
                "periods": { "type": "integer" },
                "timeframe": { "type": "string", "enum": ["MONTH", "QUARTER", "YEAR"] },
            })),
            Box::new(|c, input| {
                async move {
                    let q = map_query(
                        &input,
                        &[
                            ("date", "date"),
                            ("periods", "periods"),
                            ("timeframe", "timeframe"),
                        ],
                    );
                    Ok(wrap_payload(
                        c.get("/Reports/BalanceSheet", &q).await?,
                        "xero_report_balance_sheet",
                    ))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "xero.trial_balance",
            "Trial Balance report at a date (needs accounting.reports.read scope).",
            obj(json!({ "date": { "type": "string", "description": "YYYY-MM-DD" } })),
            Box::new(|c, input| {
                async move {
                    let q = map_query(&input, &[("date", "date")]);
                    Ok(wrap_payload(
                        c.get("/Reports/TrialBalance", &q).await?,
                        "xero_report_trial_balance",
                    ))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "xero.aged_receivables",
            "Aged Receivables by Contact report (needs accounting.reports.read scope).",
            obj_required(
                json!({
                    "contact_id": { "type": "string", "description": "Xero ContactID" },
                    "date": { "type": "string", "description": "YYYY-MM-DD" },
                }),
                json!(["contact_id"]),
            ),
            Box::new(|c, input| {
                async move {
                    let q = map_query(&input, &[("contact_id", "contactID"), ("date", "date")]);
                    Ok(wrap_payload(
                        c.get("/Reports/AgedReceivablesByContact", &q).await?,
                        "xero_report_aged_receivables",
                    ))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "xero.bank_summary",
            "Bank Summary report over a date range (needs accounting.reports.read scope).",
            obj(json!({
                "from_date": { "type": "string", "description": "YYYY-MM-DD" },
                "to_date": { "type": "string", "description": "YYYY-MM-DD" },
            })),
            Box::new(|c, input| {
                async move {
                    let q = map_query(&input, &[("from_date", "fromDate"), ("to_date", "toDate")]);
                    Ok(wrap_payload(
                        c.get("/Reports/BankSummary", &q).await?,
                        "xero_report_bank_summary",
                    ))
                }
                .boxed()
            }),
        ),
        ToolSpec::write(
            "xero.invoice_create",
            "Create one or more invoices (needs a write scope). Body is the Xero {\"Invoices\": [...]} payload.",
            obj_required(json!({ "body": body_prop() }), json!(["body"])),
            Box::new(|c, input| {
                async move {
                    let body = require_object(&input, "body")?.clone();
                    Ok(wrap_payload(c.put("/Invoices", body).await?, "xero_invoice"))
                }
                .boxed()
            }),
        ),
        ToolSpec::write(
            "xero.bill_approve",
            "Approve a bill or invoice by setting it to AUTHORISED (needs a write scope).",
            obj_required(
                json!({ "invoice_id": { "type": "string", "description": "Xero InvoiceID" } }),
                json!(["invoice_id"]),
            ),
            Box::new(|c, input| {
                async move {
                    let id = require_str(&input, "invoice_id")?;
                    let data = c
                        .post(&format!("/Invoices/{id}"), json!({ "Status": "AUTHORISED" }))
                        .await?;
                    Ok(wrap_payload(data, "xero_invoice"))
                }
                .boxed()
            }),
        ),
        ToolSpec::write(
            "xero.invoice_email",
            "Email an authorised invoice to its contact (needs a write scope).",
            obj_required(
                json!({ "invoice_id": { "type": "string", "description": "Xero InvoiceID" } }),
                json!(["invoice_id"]),
            ),
            Box::new(|c, input| {
                async move {
                    let id = require_str(&input, "invoice_id")?;
                    let data = c.post(&format!("/Invoices/{id}/Email"), json!({})).await?;
                    Ok(wrap_payload(data, "xero_invoice_email"))
                }
                .boxed()
            }),
        ),
        ToolSpec::write(
            "xero.payment_create",
            "Apply a payment to an invoice or bill (needs a write scope). Body is the Xero {\"Payments\": [...]} payload.",
            obj_required(json!({ "body": body_prop() }), json!(["body"])),
            Box::new(|c, input| {
                async move {
                    let body = require_object(&input, "body")?.clone();
                    Ok(wrap_payload(c.put("/Payments", body).await?, "xero_payment"))
                }
                .boxed()
            }),
        ),
        ToolSpec::write(
            "xero.manual_journal_create",
            "Post a manual journal — accruals, prepayments, intercompany eliminations (needs a write scope). \
             Body is the Xero {\"ManualJournals\": [...]} payload.",
            obj_required(json!({ "body": body_prop() }), json!(["body"])),
            Box::new(|c, input| {
                async move {
                    let body = require_object(&input, "body")?.clone();
                    Ok(wrap_payload(
                        c.put("/ManualJournals", body).await?,
                        "xero_manual_journal",
                    ))
                }
                .boxed()
            }),
        ),
        ToolSpec::write(
            "xero.contact_create",
            "Create one or more contacts (needs a write scope). Body is the Xero {\"Contacts\": [...]} payload.",
            obj_required(json!({ "body": body_prop() }), json!(["body"])),
            Box::new(|c, input| {
                async move {
                    let body = require_object(&input, "body")?.clone();
                    Ok(wrap_payload(c.put("/Contacts", body).await?, "xero_contact"))
                }
                .boxed()
            }),
        ),
        ToolSpec::write(
            "xero.contact_update",
            "Update an existing contact by id (needs a write scope). Body holds the fields to change.",
            obj_required(
                json!({
                    "contact_id": { "type": "string", "description": "Xero ContactID" },
                    "body": body_prop(),
                }),
                json!(["contact_id", "body"]),
            ),
            Box::new(|c, input| {
                async move {
                    let id = require_str(&input, "contact_id")?;
                    let body = require_object(&input, "body")?.clone();
                    Ok(wrap_payload(
                        c.post(&format!("/Contacts/{id}"), body).await?,
                        "xero_contact",
                    ))
                }
                .boxed()
            }),
        ),
    ]
}

pub(super) fn build_tools(clients: Clients, default_entity: String) -> Vec<Arc<dyn Tool>> {
    let ctx = Ctx {
        clients: clients.clone(),
        default_entity: default_entity.clone(),
    };
    let mut tools: Vec<Arc<dyn Tool>> = specs()
        .into_iter()
        .map(|spec| {
            Arc::new(XeroTool {
                ctx: ctx.clone(),
                spec,
            }) as Arc<dyn Tool>
        })
        .collect();
    tools.push(super::export::export_tool(clients, default_entity));
    tools
}

pub(super) fn tool_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = specs().into_iter().map(|s| s.name).collect();
    names.push(super::export::EXPORT_TOOL_NAME);
    names
}

pub(super) fn skill_tools_and_policies() -> (
    Vec<crate::definition::SkillTool>,
    Vec<crate::definition::SkillPolicy>,
) {
    use crate::definition::{SkillPolicy, SkillTool};
    let entity = generic_entity_prop();
    let (mut tools, mut policies): (Vec<SkillTool>, Vec<SkillPolicy>) = specs()
        .into_iter()
        .map(|spec| {
            let tool = SkillTool {
                name: spec.name.to_string(),
                description: spec.description.to_string(),
                input_schema: with_entity(spec.schema.clone(), entity.clone()),
            };
            let policy = SkillPolicy {
                tool: spec.name.to_string(),
                action: if spec.require_approval {
                    "require_approval".to_string()
                } else {
                    "allow".to_string()
                },
            };
            (tool, policy)
        })
        .unzip();
    // The bulk-export tool is a read (it never mutates the ledger), so `allow`.
    // Its schema already carries the `entity` property.
    tools.push(SkillTool {
        name: super::export::EXPORT_TOOL_NAME.to_string(),
        description: super::export::export_description().to_string(),
        input_schema: super::export::export_schema(),
    });
    policies.push(SkillPolicy {
        tool: super::export::EXPORT_TOOL_NAME.to_string(),
        action: "allow".to_string(),
    });
    (tools, policies)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_json, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ctx_for(server: &MockServer) -> Ctx {
        let base = server.uri();
        let client = XeroClient::with_endpoints_for_test("cid", "secret", Some("T".into()), &base);
        let mut clients = HashMap::new();
        clients.insert("default".to_string(), Arc::new(client));
        Ctx {
            clients: Arc::new(clients),
            default_entity: "default".to_string(),
        }
    }

    async fn mount_token(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/connect/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "tok", "token_type": "Bearer", "expires_in": 1800
            })))
            .mount(server)
            .await;
    }

    fn tool(name: &str, ctx: &Ctx) -> Arc<dyn Tool> {
        build_tools(ctx.clients.clone(), ctx.default_entity.clone())
            .into_iter()
            .find(|t| t.name() == name)
            .unwrap_or_else(|| panic!("tool {name} not found"))
    }

    #[tokio::test]
    async fn invoices_list_sends_paging_and_wraps() {
        let server = MockServer::start().await;
        mount_token(&server).await;
        Mock::given(method("GET"))
            .and(path("/api.xro/2.0/Invoices"))
            .and(query_param("page", "2"))
            .and(query_param("pageSize", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "Invoices": [] })))
            .mount(&server)
            .await;

        let ctx = ctx_for(&server);
        let res = tool("xero.invoices_list", &ctx)
            .execute(json!({ "page": 2 }))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["source"], "xero_invoices");
        assert!(res.output["data"].as_str().unwrap().contains("Invoices"));
    }

    #[tokio::test]
    async fn bill_approve_posts_authorised_status() {
        let server = MockServer::start().await;
        mount_token(&server).await;
        Mock::given(method("POST"))
            .and(path("/api.xro/2.0/Invoices/INV-1"))
            .and(body_json(json!({ "Status": "AUTHORISED" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "Invoices": [] })))
            .mount(&server)
            .await;

        let ctx = ctx_for(&server);
        let res = tool("xero.bill_approve", &ctx)
            .execute(json!({ "invoice_id": "INV-1" }))
            .await
            .unwrap();
        assert!(res.success, "{:?}", res.error);
    }

    #[tokio::test]
    async fn write_without_body_fails_before_calling() {
        let server = MockServer::start().await;
        mount_token(&server).await;
        let ctx = ctx_for(&server);
        let res = tool("xero.invoice_create", &ctx)
            .execute(json!({}))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("body"));
    }

    #[tokio::test]
    async fn unknown_entity_is_an_error() {
        let server = MockServer::start().await;
        let ctx = ctx_for(&server);
        let res = tool("xero.organisation", &ctx)
            .execute(json!({ "entity": "nope" }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("Unknown Xero entity"));
    }

    #[test]
    fn write_tools_require_approval_reads_allow() {
        let (_tools, policies) = skill_tools_and_policies();
        let action = |name: &str| {
            policies
                .iter()
                .find(|p| p.tool == name)
                .map(|p| p.action.as_str())
                .unwrap()
        };
        assert_eq!(action("xero.invoices_list"), "allow");
        assert_eq!(action("xero.manual_journal_create"), "require_approval");
        assert_eq!(action("xero.bill_approve"), "require_approval");
    }

    #[test]
    fn every_tool_has_entity_prop_and_policy() {
        let (tools, policies) = skill_tools_and_policies();
        assert_eq!(tools.len(), policies.len());
        for t in &tools {
            assert!(
                t.input_schema["properties"].get("entity").is_some(),
                "{} missing entity prop",
                t.name
            );
        }
        assert_eq!(tool_names().len(), tools.len());
    }
}
