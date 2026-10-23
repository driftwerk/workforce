//! FreeAgent agent-callable tools.
//!
//! Every tool accepts an optional `workspace` selecting which connected
//! FreeAgent company to target (default `default`). Read tools default to
//! `allow`; mutating tools to `require_approval` so the agent proposes and a
//! human confirms before anything touches the ledger. All response bodies are
//! routed through `wrap_external_content` to neutralise prompt injection in
//! attacker-influenceable fields (contact names, invoice references, notes).

use std::sync::Arc;

use async_trait::async_trait;
use eyre::{eyre, Result};
use futures::future::BoxFuture;
use futures::FutureExt;
use serde_json::{json, Value};
use tracing::warn;

use workforce_agent::security::external_content::wrap_external_content;
use workforce_agent::tools::{Tool, ToolResult};

use super::client::{clamp_page, clamp_per_page, FreeAgentClient, FreeAgentContext};
use crate::adapters::utils::coerce_to_u64;

type Handler =
    Box<dyn Fn(Arc<FreeAgentClient>, Value) -> BoxFuture<'static, Result<Value>> + Send + Sync>;

#[derive(Clone)]
struct Ctx {
    context: Arc<FreeAgentContext>,
}

impl Ctx {
    fn client(&self, input: &Value) -> Arc<FreeAgentClient> {
        let workspace = input["workspace"].as_str().unwrap_or("default");
        self.context.client(workspace)
    }
}

async fn run<F, Fut>(ctx: &Ctx, input: Value, f: F) -> Result<ToolResult>
where
    F: FnOnce(Arc<FreeAgentClient>, Value) -> Fut,
    Fut: std::future::Future<Output = Result<Value>>,
{
    let start = std::time::Instant::now();
    let client = ctx.client(&input);
    match f(client, input).await {
        Ok(output) => Ok(ToolResult::success(output, elapsed(start))),
        Err(e) => {
            warn!(error = %e, "FreeAgent tool failed");
            Ok(ToolResult::from_report(&e, elapsed(start)))
        }
    }
}

fn elapsed(start: std::time::Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

/// Serialise a FreeAgent response and wrap it so every field that crosses the
/// trust boundary carries injection markers.
fn wrap_payload(value: Value, source: &str) -> Value {
    let rendered = serde_json::to_string(&value).unwrap_or_default();
    let wrapped = wrap_external_content(&rendered, source).wrapped_content;
    json!({ "source": source, "data": wrapped })
}

fn workspace_prop() -> Value {
    json!({
        "type": "string",
        "description": "Connected FreeAgent company (default: 'default'). Use a per-company name when more than one is connected."
    })
}

/// Insert the `workspace` property into a schema's `properties` block.
fn with_workspace(mut schema: Value, prop: Value) -> Value {
    if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        props.insert("workspace".to_string(), prop);
    }
    schema
}

fn obj(properties: Value) -> Value {
    json!({ "type": "object", "properties": properties })
}

fn obj_required(properties: Value, required: Value) -> Value {
    json!({ "type": "object", "properties": properties, "required": required })
}

/// Standard paged-list schema properties, merged with any endpoint-specific
/// `extra` filters.
fn paged_props(extra: Value) -> Value {
    let mut base = json!({
        "page": { "type": "integer", "minimum": 1, "description": "1-based page number" },
        "per_page": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Rows per page (max 100, default 25)" },
    });
    if let (Some(base_obj), Some(extra_obj)) = (base.as_object_mut(), extra.as_object()) {
        for (k, v) in extra_obj {
            base_obj.insert(k.clone(), v.clone());
        }
    }
    base
}

fn view_prop(values: &str) -> Value {
    json!({ "type": "string", "description": format!("FreeAgent `view` filter ({values})") })
}

fn date_prop(label: &str) -> Value {
    json!({ "type": "string", "description": format!("{label} (YYYY-MM-DD)") })
}

fn url_prop(label: &str) -> Value {
    json!({ "type": "string", "description": format!("{label} — the resource's full FreeAgent `url`") })
}

/// Build a paged query: `page` + `per_page` always, plus any of `extra_keys`
/// present as a non-empty string (the FreeAgent query key equals the input
/// key — both snake_case).
fn paged_query(input: &Value, extra_keys: &[&'static str]) -> Vec<(&'static str, String)> {
    let mut q = vec![
        (
            "page",
            clamp_page(coerce_to_u64(&input["page"])).to_string(),
        ),
        (
            "per_page",
            clamp_per_page(coerce_to_u64(&input["per_page"])).to_string(),
        ),
    ];
    for key in extra_keys {
        if let Some(v) = input[*key].as_str().filter(|s| !s.is_empty()) {
            q.push((key, v.to_string()));
        }
    }
    q
}

/// Optional string filters (no paging) — used by the reports endpoint.
fn filter_query(input: &Value, keys: &[&'static str]) -> Vec<(&'static str, String)> {
    keys.iter()
        .filter_map(|key| {
            input[*key]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|v| (*key, v.to_string()))
        })
        .collect()
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
            "'{key}' must be a JSON object matching the FreeAgent payload"
        ))
    }
}

/// FreeAgent resource ids are full API URLs in responses; tools accept either
/// a full URL or a bare id and address the endpoint by the trailing segment.
fn resource_id(raw: &str) -> &str {
    raw.trim_end_matches('/').rsplit('/').next().unwrap_or(raw)
}

struct FreeAgentTool {
    ctx: Ctx,
    spec: ToolSpec,
}

#[async_trait]
impl Tool for FreeAgentTool {
    fn name(&self) -> &str {
        self.spec.name
    }

    fn description(&self) -> &str {
        self.spec.description
    }

    fn input_schema(&self) -> Value {
        with_workspace(self.spec.schema.clone(), workspace_prop())
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

/// The full FreeAgent tool surface — the single source of truth for names,
/// schemas, policies, and behaviour.
fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec::read(
            "freeagent.company",
            "Get the connected company's profile: name, type, base currency, and financial year end.",
            obj(json!({})),
            Box::new(|c, _input| {
                async move { Ok(wrap_payload(c.get("/company", &[]).await?, "freeagent_company")) }
                    .boxed()
            }),
        ),
        ToolSpec::read(
            "freeagent.contacts_list",
            "List contacts (customers and suppliers), paged.",
            obj(paged_props(json!({
                "view": view_prop("active, all, clients, suppliers, active_projects, …"),
                "sort": { "type": "string", "description": "Sort key, e.g. `name` or `-created_at`" },
            }))),
            Box::new(|c, input| {
                async move {
                    let q = paged_query(&input, &["view", "sort"]);
                    Ok(wrap_payload(c.get_paged("/contacts", &q).await?, "freeagent_contacts"))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "freeagent.contact_get",
            "Get a single contact by id (numeric id or full FreeAgent url).",
            obj_required(
                json!({ "id": { "type": "string", "description": "Contact id or url" } }),
                json!(["id"]),
            ),
            Box::new(|c, input| {
                async move {
                    let id = resource_id(require_str(&input, "id")?);
                    Ok(wrap_payload(
                        c.get(&format!("/contacts/{id}"), &[]).await?,
                        "freeagent_contact",
                    ))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "freeagent.invoices_list",
            "List invoices, paged. Use `view` for ageing/overdue analysis and `contact`/`project` to filter.",
            obj(paged_props(json!({
                "view": view_prop("all, recent_open_or_overdue, open, overdue, draft, scheduled, paid, …"),
                "contact": url_prop("Filter by contact"),
                "project": url_prop("Filter by project"),
                "sort": { "type": "string", "description": "Sort key, e.g. `-dated_on`" },
            }))),
            Box::new(|c, input| {
                async move {
                    let q = paged_query(&input, &["view", "contact", "project", "sort"]);
                    Ok(wrap_payload(c.get_paged("/invoices", &q).await?, "freeagent_invoices"))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "freeagent.invoice_get",
            "Get a single invoice by id (numeric id or full FreeAgent url).",
            obj_required(
                json!({ "id": { "type": "string", "description": "Invoice id or url" } }),
                json!(["id"]),
            ),
            Box::new(|c, input| {
                async move {
                    let id = resource_id(require_str(&input, "id")?);
                    Ok(wrap_payload(
                        c.get(&format!("/invoices/{id}"), &[]).await?,
                        "freeagent_invoice",
                    ))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "freeagent.bills_list",
            "List bills (money the company owes), paged.",
            obj(paged_props(json!({
                "view": view_prop("all, open, overdue, paid, recurring, …"),
                "contact": url_prop("Filter by contact"),
                "project": url_prop("Filter by project"),
            }))),
            Box::new(|c, input| {
                async move {
                    let q = paged_query(&input, &["view", "contact", "project"]);
                    Ok(wrap_payload(c.get_paged("/bills", &q).await?, "freeagent_bills"))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "freeagent.bill_get",
            "Get a single bill by id (numeric id or full FreeAgent url).",
            obj_required(
                json!({ "id": { "type": "string", "description": "Bill id or url" } }),
                json!(["id"]),
            ),
            Box::new(|c, input| {
                async move {
                    let id = resource_id(require_str(&input, "id")?);
                    Ok(wrap_payload(
                        c.get(&format!("/bills/{id}"), &[]).await?,
                        "freeagent_bill",
                    ))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "freeagent.estimates_list",
            "List estimates and quotes, paged.",
            obj(paged_props(json!({
                "view": view_prop("all, draft, sent, approved, rejected, invoiced, …"),
                "contact": url_prop("Filter by contact"),
            }))),
            Box::new(|c, input| {
                async move {
                    let q = paged_query(&input, &["view", "contact"]);
                    Ok(wrap_payload(c.get_paged("/estimates", &q).await?, "freeagent_estimates"))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "freeagent.expenses_list",
            "List expenses, paged. Filter by date range and/or user.",
            obj(paged_props(json!({
                "from_date": date_prop("Earliest expense date"),
                "to_date": date_prop("Latest expense date"),
                "user": url_prop("Filter by the user who incurred the expense"),
                "project": url_prop("Filter by project"),
            }))),
            Box::new(|c, input| {
                async move {
                    let q = paged_query(&input, &["from_date", "to_date", "user", "project"]);
                    Ok(wrap_payload(c.get_paged("/expenses", &q).await?, "freeagent_expenses"))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "freeagent.bank_accounts_list",
            "List bank accounts and their balances.",
            obj(paged_props(json!({
                "view": view_prop("standard_bank_accounts, credit_card_accounts, paypal_accounts, …"),
            }))),
            Box::new(|c, input| {
                async move {
                    let q = paged_query(&input, &["view"]);
                    Ok(wrap_payload(
                        c.get_paged("/bank_accounts", &q).await?,
                        "freeagent_bank_accounts",
                    ))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "freeagent.bank_transactions_list",
            "List bank transactions for a bank account, paged.",
            obj_required(
                paged_props(json!({
                    "bank_account": url_prop("The bank account to list transactions for"),
                    "view": view_prop("all, unexplained, explained, manual, imported, marked_for_review, …"),
                    "from_date": date_prop("Earliest transaction date"),
                    "to_date": date_prop("Latest transaction date"),
                })),
                json!(["bank_account"]),
            ),
            Box::new(|c, input| {
                async move {
                    require_str(&input, "bank_account")?;
                    let q = paged_query(&input, &["bank_account", "view", "from_date", "to_date"]);
                    Ok(wrap_payload(
                        c.get_paged("/bank_transactions", &q).await?,
                        "freeagent_bank_transactions",
                    ))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "freeagent.projects_list",
            "List projects, paged.",
            obj(paged_props(json!({
                "view": view_prop("active, completed, cancelled, hidden, all"),
                "contact": url_prop("Filter by contact"),
            }))),
            Box::new(|c, input| {
                async move {
                    let q = paged_query(&input, &["view", "contact"]);
                    Ok(wrap_payload(c.get_paged("/projects", &q).await?, "freeagent_projects"))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "freeagent.project_get",
            "Get a single project by id (numeric id or full FreeAgent url).",
            obj_required(
                json!({ "id": { "type": "string", "description": "Project id or url" } }),
                json!(["id"]),
            ),
            Box::new(|c, input| {
                async move {
                    let id = resource_id(require_str(&input, "id")?);
                    Ok(wrap_payload(
                        c.get(&format!("/projects/{id}"), &[]).await?,
                        "freeagent_project",
                    ))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "freeagent.tasks_list",
            "List tasks, paged. Filter by project.",
            obj(paged_props(json!({
                "project": url_prop("Filter by project"),
                "view": view_prop("active, completed, hidden, all"),
            }))),
            Box::new(|c, input| {
                async move {
                    let q = paged_query(&input, &["project", "view"]);
                    Ok(wrap_payload(c.get_paged("/tasks", &q).await?, "freeagent_tasks"))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "freeagent.timeslips_list",
            "List timeslips (tracked time), paged. Filter by date range, user, task, or project.",
            obj(paged_props(json!({
                "from_date": date_prop("Earliest timeslip date"),
                "to_date": date_prop("Latest timeslip date"),
                "user": url_prop("Filter by user"),
                "task": url_prop("Filter by task"),
                "project": url_prop("Filter by project"),
                "view": view_prop("all, unbilled, running, …"),
            }))),
            Box::new(|c, input| {
                async move {
                    let q = paged_query(
                        &input,
                        &["from_date", "to_date", "user", "task", "project", "view"],
                    );
                    Ok(wrap_payload(c.get_paged("/timeslips", &q).await?, "freeagent_timeslips"))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "freeagent.categories_list",
            "List accounting categories (the chart of accounts: income, costs, admin, …).",
            obj(json!({})),
            Box::new(|c, _input| {
                async move {
                    Ok(wrap_payload(c.get("/categories", &[]).await?, "freeagent_categories"))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "freeagent.users_list",
            "List the users in the FreeAgent account.",
            obj(paged_props(json!({}))),
            Box::new(|c, input| {
                async move {
                    let q = paged_query(&input, &[]);
                    Ok(wrap_payload(c.get_paged("/users", &q).await?, "freeagent_users"))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "freeagent.profit_and_loss_summary",
            "Profit and Loss summary. Defaults to the current accounting year to date; \
             narrow with from_date/to_date or accounting_period (YYYY/YY).",
            obj(json!({
                "from_date": date_prop("Period start"),
                "to_date": date_prop("Period end"),
                "accounting_period": { "type": "string", "description": "Accounting period, e.g. 2024/25" },
            })),
            Box::new(|c, input| {
                async move {
                    let q = filter_query(&input, &["from_date", "to_date", "accounting_period"]);
                    Ok(wrap_payload(
                        c.get("/accounting/profit_and_loss/summary", &q).await?,
                        "freeagent_profit_and_loss",
                    ))
                }
                .boxed()
            }),
        ),
        ToolSpec::write(
            "freeagent.contact_create",
            "Create a contact. `contact` is the FreeAgent contact object (e.g. {\"first_name\": …, \"organisation_name\": …}).",
            obj_required(json!({ "contact": body_prop("contact") }), json!(["contact"])),
            Box::new(|c, input| {
                async move {
                    let contact = require_object(&input, "contact")?.clone();
                    Ok(wrap_payload(
                        c.post("/contacts", json!({ "contact": contact })).await?,
                        "freeagent_contact",
                    ))
                }
                .boxed()
            }),
        ),
        ToolSpec::write(
            "freeagent.contact_update",
            "Update a contact by id. `contact` holds the fields to change.",
            obj_required(
                json!({
                    "id": { "type": "string", "description": "Contact id or url" },
                    "contact": body_prop("contact"),
                }),
                json!(["id", "contact"]),
            ),
            Box::new(|c, input| {
                async move {
                    let id = resource_id(require_str(&input, "id")?);
                    let contact = require_object(&input, "contact")?.clone();
                    Ok(wrap_payload(
                        c.put(&format!("/contacts/{id}"), json!({ "contact": contact }))
                            .await?,
                        "freeagent_contact",
                    ))
                }
                .boxed()
            }),
        ),
        ToolSpec::write(
            "freeagent.invoice_create",
            "Create an invoice. `invoice` is the FreeAgent invoice object (contact url, dated_on, \
             payment_terms_in_days, invoice_items: [...]).",
            obj_required(json!({ "invoice": body_prop("invoice") }), json!(["invoice"])),
            Box::new(|c, input| {
                async move {
                    let invoice = require_object(&input, "invoice")?.clone();
                    Ok(wrap_payload(
                        c.post("/invoices", json!({ "invoice": invoice })).await?,
                        "freeagent_invoice",
                    ))
                }
                .boxed()
            }),
        ),
        ToolSpec::write(
            "freeagent.timeslip_create",
            "Log a timeslip. `timeslip` is the FreeAgent object (task url, user url, project url, \
             dated_on, hours).",
            obj_required(json!({ "timeslip": body_prop("timeslip") }), json!(["timeslip"])),
            Box::new(|c, input| {
                async move {
                    let timeslip = require_object(&input, "timeslip")?.clone();
                    Ok(wrap_payload(
                        c.post("/timeslips", json!({ "timeslip": timeslip })).await?,
                        "freeagent_timeslip",
                    ))
                }
                .boxed()
            }),
        ),
        ToolSpec::write(
            "freeagent.expense_create",
            "Record an expense. `expense` is the FreeAgent object (user url, category url, \
             dated_on, gross_value, description).",
            obj_required(json!({ "expense": body_prop("expense") }), json!(["expense"])),
            Box::new(|c, input| {
                async move {
                    let expense = require_object(&input, "expense")?.clone();
                    Ok(wrap_payload(
                        c.post("/expenses", json!({ "expense": expense })).await?,
                        "freeagent_expense",
                    ))
                }
                .boxed()
            }),
        ),
    ]
}

fn body_prop(resource: &str) -> Value {
    json!({ "type": "object", "description": format!("The FreeAgent {resource} object") })
}

pub(super) fn build_tools(context: Arc<FreeAgentContext>) -> Vec<Arc<dyn Tool>> {
    let ctx = Ctx { context };
    specs()
        .into_iter()
        .map(|spec| {
            Arc::new(FreeAgentTool {
                ctx: ctx.clone(),
                spec,
            }) as Arc<dyn Tool>
        })
        .collect()
}

pub(super) fn tool_names() -> Vec<&'static str> {
    specs().into_iter().map(|s| s.name).collect()
}

pub(super) fn skill_tools_and_policies() -> (
    Vec<crate::definition::SkillTool>,
    Vec<crate::definition::SkillPolicy>,
) {
    use crate::definition::{SkillPolicy, SkillTool};
    specs()
        .into_iter()
        .map(|spec| {
            let tool = SkillTool {
                name: spec.name.to_string(),
                description: spec.description.to_string(),
                input_schema: with_workspace(spec.schema.clone(), workspace_prop()),
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
        .unzip()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use workforce_common::settings::SecretStore;
    use workforce_oauth_client::OAuthClientCreds;

    use crate::adapters::test_support::InMem;

    /// A context whose data calls hit the wiremock server, with a pre-seeded
    /// far-future access token so the token endpoint is never contacted.
    fn ctx_for(server: &MockServer) -> Ctx {
        let far_future = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 100_000)
            .to_string();
        let store = InMem::from(&[
            ("skills/freeagent/workspaces/default/refresh_token", "rt"),
            (
                "skills/freeagent/workspaces/default/access_token",
                "at-test",
            ),
        ]);
        store
            .set(
                "skills/freeagent/workspaces/default/expires_at",
                &far_future,
            )
            .unwrap();

        let creds = OAuthClientCreds {
            client_id: "cid".into(),
            client_secret: Some("secret".into()),
            redirect_uri: None,
        };
        let context = FreeAgentContext::with_base_for_test(
            store as Arc<dyn SecretStore>,
            super::super::client::build_http_client().unwrap(),
            creds,
            server.uri(),
        );
        Ctx {
            context: Arc::new(context),
        }
    }

    fn tool(name: &str, ctx: &Ctx) -> Arc<dyn Tool> {
        build_tools(ctx.context.clone())
            .into_iter()
            .find(|t| t.name() == name)
            .unwrap_or_else(|| panic!("tool {name} not found"))
    }

    #[tokio::test]
    async fn invoices_list_sends_paging_and_wraps() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/invoices"))
            .and(query_param("page", "2"))
            .and(query_param("per_page", "25"))
            .and(query_param("view", "overdue"))
            .and(header("authorization", "Bearer at-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "invoices": [] })))
            .mount(&server)
            .await;

        let ctx = ctx_for(&server);
        let res = tool("freeagent.invoices_list", &ctx)
            .execute(json!({ "page": 2, "view": "overdue" }))
            .await
            .unwrap();
        assert!(res.success, "{:?}", res.error);
        assert_eq!(res.output["source"], "freeagent_invoices");
        assert!(res.output["data"].as_str().unwrap().contains("invoices"));
    }

    #[tokio::test]
    async fn contact_create_posts_wrapped_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/contacts"))
            .and(wiremock::matchers::body_json(
                json!({ "contact": { "organisation_name": "Acme" } }),
            ))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(json!({ "contact": { "url": "u" } })),
            )
            .mount(&server)
            .await;

        let ctx = ctx_for(&server);
        let res = tool("freeagent.contact_create", &ctx)
            .execute(json!({ "contact": { "organisation_name": "Acme" } }))
            .await
            .unwrap();
        assert!(res.success, "{:?}", res.error);
    }

    #[tokio::test]
    async fn unknown_workspace_falls_back_to_its_own_client() {
        // A never-connected workspace simply has no token; the call surfaces a
        // clear auth error rather than panicking.
        let server = MockServer::start().await;
        let ctx = ctx_for(&server);
        let res = tool("freeagent.company", &ctx)
            .execute(json!({ "workspace": "ghost" }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("refresh token"));
    }

    #[test]
    fn resource_id_handles_url_and_bare() {
        assert_eq!(
            resource_id("https://api.freeagent.com/v2/contacts/123"),
            "123"
        );
        assert_eq!(resource_id("456"), "456");
        assert_eq!(
            resource_id("https://api.freeagent.com/v2/contacts/789/"),
            "789"
        );
    }

    #[test]
    fn paged_query_clamps_and_passes_filters() {
        let input = json!({ "page": 2, "per_page": 5000, "view": "overdue", "sort": "" });
        let q = paged_query(&input, &["view", "sort"]);
        assert!(q.contains(&("page", "2".to_string())));
        assert!(q.contains(&("per_page", "100".to_string())));
        assert!(q.contains(&("view", "overdue".to_string())));
        // Empty `sort` is dropped.
        assert!(!q.iter().any(|(k, _)| *k == "sort"));
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
        assert_eq!(action("freeagent.invoices_list"), "allow");
        assert_eq!(action("freeagent.company"), "allow");
        assert_eq!(action("freeagent.invoice_create"), "require_approval");
        assert_eq!(action("freeagent.timeslip_create"), "require_approval");
    }

    #[test]
    fn every_tool_has_workspace_prop_and_policy() {
        let (tools, policies) = skill_tools_and_policies();
        assert_eq!(tools.len(), policies.len());
        for t in &tools {
            assert!(
                t.input_schema["properties"].get("workspace").is_some(),
                "{} missing workspace prop",
                t.name
            );
        }
        assert_eq!(tool_names().len(), tools.len());
    }

    #[tokio::test]
    async fn write_without_body_fails_before_calling() {
        let server = MockServer::start().await;
        let ctx = ctx_for(&server);
        let res = tool("freeagent.invoice_create", &ctx)
            .execute(json!({}))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("invoice"));
    }
}
