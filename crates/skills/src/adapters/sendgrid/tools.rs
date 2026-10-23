//! SendGrid agent-callable tools.
//!
//! Every tool takes an optional `workspace` selecting which SendGrid account
//! (API key) to use. The declarative [`ToolSpec`] table is the single source
//! of truth for names, schemas, policies, behaviour and approval summaries.
//! User-authored fields that come back from SendGrid (subjects, sender names,
//! bounce reasons, template names) pass through `wrap_external_content`;
//! everything else stays structured JSON.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::{eyre, Result};
use futures::future::BoxFuture;
use futures::FutureExt;
use reqwest::Method;
use serde_json::{json, Map, Value};
use tracing::{info, warn};

use workforce_agent::tools::{Tool, ToolResult};

use super::client::SendGridClient;
use crate::adapters::utils::{
    artifact_attachments_schema, coerce_to_u64, html_with_inline_images, instance_choices,
    resolve_artifact_attachments, truncate_string_fields, wrap_fields_in_place,
};
use crate::definition::{SkillPolicy, SkillTool};

type Clients = Arc<HashMap<String, Arc<SendGridClient>>>;
type Handler =
    Box<dyn Fn(Arc<SendGridClient>, Value) -> BoxFuture<'static, Result<Value>> + Send + Sync>;
type Summary = fn(&Ctx, &Value) -> Option<String>;

const MAX_PERSONALIZATIONS: usize = 1000;
const MAX_RECIPIENTS: usize = 1000;
const MAX_CATEGORIES: usize = 10;
const MAX_MESSAGE_BYTES: usize = 30 * 1024 * 1024;
const TEMPLATE_BODY_MAX_BYTES: usize = 25_000;
const SINGLE_MESSAGE_FIELDS: &[&str] = &["dynamic_template_data"];
const BATCH_MESSAGE_FIELDS: &[&str] = &[
    "subject",
    "dynamic_template_data",
    "custom_args",
    "headers",
    "send_at",
];

struct Limit {
    default: u64,
    max: u64,
}

impl Limit {
    fn clamp(&self, requested: Option<u64>) -> u64 {
        match requested {
            None | Some(0) => self.default,
            Some(n) => n.min(self.max),
        }
    }
}

const TEMPLATE_PAGE_SIZE: Limit = Limit {
    default: 50,
    max: 200,
};
const MESSAGES_LIMIT: Limit = Limit {
    default: 10,
    max: 1000,
};
const SUPPRESSION_LIMIT: Limit = Limit {
    default: 50,
    max: 500,
};

#[derive(Clone)]
struct Ctx {
    clients: Clients,
    default_workspace: String,
}

impl Ctx {
    fn client(&self, input: &Value) -> Result<Arc<SendGridClient>> {
        let name = workforce_agent::llm::environment::resolve_instance(
            "sendgrid",
            input["workspace"].as_str(),
            &self.default_workspace,
            &*self.clients,
        )
        .map_err(|d| eyre!("{}", d.message))?;
        self.clients.get(&name).cloned().ok_or_else(|| {
            eyre!(
                "Unknown SendGrid workspace: '{name}'. Available: {}",
                self.names().join(", ")
            )
        })
    }

    fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.clients.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    fn workspace_prop(&self) -> Value {
        if self.clients.len() <= 1 {
            return generic_workspace_prop();
        }
        prop(
            "string",
            &format!(
                "SendGrid workspace name. {}",
                instance_choices(&*self.clients, &self.default_workspace, "workspace")
            ),
        )
    }
}

fn generic_workspace_prop() -> Value {
    prop(
        "string",
        "SendGrid workspace name. Uses the default if omitted.",
    )
}

async fn run<F, Fut>(ctx: &Ctx, input: Value, f: F) -> Result<ToolResult>
where
    F: FnOnce(Arc<SendGridClient>, Value) -> Fut,
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
            warn!(error = %e, "SendGrid tool failed");
            Ok(ToolResult::from_report(&e, elapsed(start)))
        }
    }
}

fn elapsed(start: std::time::Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

struct SendGridTool {
    ctx: Ctx,
    spec: ToolSpec,
}

#[async_trait]
impl Tool for SendGridTool {
    fn name(&self) -> &str {
        self.spec.name
    }

    fn description(&self) -> &str {
        self.spec.description
    }

    fn input_schema(&self) -> Value {
        with_workspace(self.spec.schema.clone(), self.ctx.workspace_prop())
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| {
            (self.spec.handler)(client, input)
        })
        .await
    }

    fn approval_summary(&self, input: &Value) -> Option<String> {
        self.spec
            .summary
            .and_then(|summary| summary(&self.ctx, input))
    }
}

struct ToolSpec {
    name: &'static str,
    description: &'static str,
    schema: Value,
    require_approval: bool,
    handler: Handler,
    summary: Option<Summary>,
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
            summary: None,
        }
    }

    fn write(
        name: &'static str,
        description: &'static str,
        schema: Value,
        handler: Handler,
    ) -> Self {
        Self {
            require_approval: true,
            ..Self::read(name, description, schema, handler)
        }
    }

    fn summarised(self, summary: Summary) -> Self {
        Self {
            summary: Some(summary),
            ..self
        }
    }
}

// ── Schema helpers ──────────────────────────────────────────────────────────

fn prop(kind: &str, description: &str) -> Value {
    json!({ "type": kind, "description": description })
}

fn enum_prop(values: &[&str], description: &str) -> Value {
    json!({ "type": "string", "enum": values, "description": description })
}

fn string_list_prop(description: &str) -> Value {
    json!({ "type": "array", "items": { "type": "string" }, "description": description })
}

fn obj(properties: Value) -> Value {
    json!({ "type": "object", "properties": properties })
}

fn obj_required(properties: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": properties, "required": required })
}

fn with_workspace(mut schema: Value, workspace: Value) -> Value {
    if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        props.insert("workspace".to_string(), workspace);
    }
    schema
}

fn merged(mut base: Value, extra: Value) -> Value {
    if let (Some(target), Some(source)) = (base.as_object_mut(), extra.as_object()) {
        target.extend(source.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    base
}

const ADDRESS_HINT: &str = "'a@b.com' or 'Name <a@b.com>'";

fn recipients_prop(label: &str) -> Value {
    json!({
        "type": "array",
        "items": { "type": "string" },
        "description": format!("{label} addresses, each {ADDRESS_HINT} (a single string is also accepted)")
    })
}

fn mail_envelope_props() -> Value {
    json!({
        "from":                  prop("string", &format!("Sender, {ADDRESS_HINT}. Defaults to the workspace's from_email; must be a verified sender identity or on an authenticated domain.")),
        "reply_to":              prop("string", &format!("Reply-To address, {ADDRESS_HINT}")),
        "subject":               prop("string", "Subject. Required unless template_id is set."),
        "text":                  prop("string", "Plain-text body. At least one of text/html is required unless template_id is set."),
        "html":                  prop("string", "HTML body."),
        "template_id":           prop("string", "Dynamic template id (d-…). The template supplies subject and body; pass data via dynamic_template_data."),
        "categories":            string_list_prop("Up to 10 category tags for stats."),
        "custom_args":           prop("object", "String key/value metadata echoed in Event Webhook and Email Activity."),
        "headers":               prop("object", "Extra message headers as {name: value}."),
        "send_at":               prop("integer", "Unix timestamp to schedule delivery, at most 72 hours ahead."),
        "batch_id":              prop("string", "Batch id for a scheduled send (from POST /v3/mail/batch)."),
        "asm_group_id":          prop("integer", "Unsubscribe group id (see sendgrid.unsubscribe_groups_list)."),
        "attachments":           {
            "type": "array",
            "description": "Base64 attachments.",
            "items": { "type": "object", "properties": {
                "content":     prop("string", "Base64-encoded file content."),
                "filename":    prop("string", "Filename including extension."),
                "type":        prop("string", "MIME type."),
                "disposition": enum_prop(&["attachment", "inline"], "Default attachment."),
                "content_id":  prop("string", "cid for inline images.")
            }, "required": ["content", "filename"] }
        },
        "attach_artifact_ids":   artifact_attachments_schema("attachments"),
        "sandbox":               prop("boolean", "Validate the request without sending (mail_settings.sandbox_mode).")
    })
}

fn message_props() -> Value {
    json!({
        "to":                    recipients_prop("Recipient"),
        "cc":                    recipients_prop("Cc"),
        "bcc":                   recipients_prop("Bcc"),
        "subject":               prop("string", "Per-message subject override."),
        "dynamic_template_data": prop("object", "Handlebars data for the dynamic template."),
        "custom_args":           prop("object", "Per-message custom_args."),
        "headers":               prop("object", "Per-message headers."),
        "send_at":               prop("integer", "Per-message scheduled send time (unix).")
    })
}

// ── Input helpers ───────────────────────────────────────────────────────────

fn require_str<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    opt_str(input, key).ok_or_else(|| eyre!("Missing required parameter '{key}'"))
}

fn opt_str<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input[key].as_str().map(str::trim).filter(|s| !s.is_empty())
}

fn opt_u64(input: &Value, key: &str) -> Option<u64> {
    coerce_to_u64(&input[key])
}

fn copy_fields(target: &mut Map<String, Value>, input: &Value, keys: &[&str]) {
    for &key in keys {
        if let Some(v) = input.get(key).filter(|v| !v.is_null()) {
            target.insert(key.to_string(), v.clone());
        }
    }
}

fn push_str(query: &mut Vec<(&'static str, String)>, input: &Value, key: &'static str) {
    if let Some(v) = opt_str(input, key) {
        query.push((key, v.to_string()));
    }
}

fn push_u64(query: &mut Vec<(&'static str, String)>, input: &Value, key: &'static str) {
    if let Some(v) = opt_u64(input, key) {
        query.push((key, v.to_string()));
    }
}

/// Bare array responses become `{count, items}` so every tool returns an
/// object the model can address; other shapes pass through.
fn count_items(value: Value) -> Value {
    match value {
        Value::Array(items) => json!({ "count": items.len(), "items": items }),
        other => other,
    }
}

fn wrap_each(list: &mut Value, keys: &[&str], prefix: &str) {
    if let Some(items) = list.as_array_mut() {
        for item in items {
            wrap_fields_in_place(item, keys, prefix);
        }
    }
}

fn wrap_nested(response: &mut Value, key: &str, fields: &[&str], prefix: &str) {
    if let Some(list) = response.get_mut(key) {
        wrap_each(list, fields, prefix);
    }
}

/// Reject anything that cannot be interpolated as one URL path segment.
fn path_segment(raw: &str) -> Result<&str> {
    let segment = raw.trim();
    if segment.is_empty()
        || segment.contains(['/', '?', '#'])
        || segment.contains(char::is_whitespace)
    {
        return Err(eyre!("'{raw}' is not a valid identifier"));
    }
    Ok(segment)
}

fn valid_email(raw: &str) -> Result<&str> {
    let email = raw.trim();
    if email.is_empty() || !email.contains('@') || email.contains(char::is_whitespace) {
        return Err(eyre!("'{raw}' is not an email address"));
    }
    Ok(email)
}

fn email_segment(raw: &str) -> Result<&str> {
    path_segment(valid_email(raw)?).map_err(|_| eyre!("'{raw}' cannot be used as a path segment"))
}

fn v3_path(raw: &str) -> Result<String> {
    let path = raw.trim();
    if !path.starts_with('/') {
        return Err(eyre!("'path' must start with '/', e.g. '/v3/templates'"));
    }
    Ok(if path == "/v3" || path.starts_with("/v3/") {
        path.to_string()
    } else {
        format!("/v3{path}")
    })
}

fn query_object(input: &Value) -> Vec<(&str, String)> {
    input["query"]
        .as_object()
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| match v {
                    Value::String(s) => Some((k.as_str(), s.clone())),
                    Value::Number(n) => Some((k.as_str(), n.to_string())),
                    Value::Bool(b) => Some((k.as_str(), b.to_string())),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

// ── Addresses and mail payloads ─────────────────────────────────────────────

fn address_json(email: &str, name: Option<&str>) -> Value {
    let mut address = Map::new();
    address.insert("email".to_string(), json!(email));
    if let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) {
        address.insert("name".to_string(), json!(name));
    }
    Value::Object(address)
}

fn parse_address(raw: &str) -> Result<Value> {
    let raw = raw.trim();
    let (name, email) = raw
        .strip_suffix('>')
        .and_then(|head| head.rsplit_once('<'))
        .map_or((None, raw), |(name, email)| {
            (Some(name.trim().trim_matches('"')), email)
        });
    Ok(address_json(valid_email(email)?, name))
}

fn address(value: &Value) -> Result<Value> {
    match value {
        Value::String(s) => parse_address(s),
        Value::Object(o) => {
            let email = o
                .get("email")
                .and_then(Value::as_str)
                .ok_or_else(|| eyre!("address object needs an 'email'"))?;
            Ok(address_json(
                valid_email(email)?,
                o.get("name").and_then(Value::as_str),
            ))
        }
        other => Err(eyre!(
            "{other} is not an address (use {ADDRESS_HINT} or {{email, name}})"
        )),
    }
}

fn addresses(input: &Value, key: &str) -> Result<Vec<Value>> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items.iter().map(address).collect(),
        Some(single) => address(single).map(|a| vec![a]),
    }
    .map_err(|e| eyre!("'{key}': {e}"))
}

fn resolve_from(client: &SendGridClient, input: &Value) -> Result<Value> {
    match input.get("from").filter(|v| !v.is_null()) {
        Some(from) => address(from).map_err(|e| eyre!("'from': {e}")),
        None => client
            .default_sender()
            .map(|s| address_json(&s.email, s.name.as_deref()))
            .ok_or_else(|| {
                eyre!(
                    "No From address: pass `from`, or configure the workspace's from_email \
                     (skills/sendgrid/from_email or skills/sendgrid/workspaces/<name>/from_email)"
                )
            }),
    }
}

/// One `personalizations[]` entry: recipients plus whichever per-message
/// `fields` the calling tool lets the message carry.
fn personalization(message: &Value, fields: &[&str]) -> Result<Value> {
    let to = addresses(message, "to")?;
    if to.is_empty() {
        return Err(eyre!("'to' needs at least one recipient"));
    }
    let mut entry = Map::new();
    entry.insert("to".to_string(), Value::Array(to));
    for key in ["cc", "bcc"] {
        let list = addresses(message, key)?;
        if !list.is_empty() {
            entry.insert(key.to_string(), Value::Array(list));
        }
    }
    copy_fields(&mut entry, message, fields);
    if entry.remove("subject").is_some() {
        if let Some(subject) = opt_str(message, "subject") {
            entry.insert("subject".to_string(), json!(subject));
        }
    }
    Ok(Value::Object(entry))
}

fn attachments(input: &Value) -> Result<Vec<Value>> {
    let explicit = input["attachments"].as_array().cloned().unwrap_or_default();
    for (i, attachment) in explicit.iter().enumerate() {
        for key in ["content", "filename"] {
            if opt_str(attachment, key).is_none() {
                return Err(eyre!("attachments[{i}] needs a non-empty '{key}'"));
            }
        }
    }
    let explicit_bytes: usize = explicit
        .iter()
        .map(|a| a["content"].as_str().map_or(0, str::len))
        .sum();
    if explicit_bytes > MAX_MESSAGE_BYTES {
        return Err(eyre!(
            "attachments total {explicit_bytes} bytes (base64), over SendGrid's 30 MB message limit"
        ));
    }
    let staged = resolve_artifact_attachments(
        input,
        MAX_MESSAGE_BYTES - explicit_bytes,
        "SendGrid's 30 MB message limit",
    )
    .map_err(|e| eyre!(e))?
    .into_iter()
    .map(|a| {
        let mut entry =
            json!({ "filename": a.name, "content": a.content_base64(), "type": a.content_type });
        if let Some(cid) = &a.content_id {
            entry["disposition"] = json!("inline");
            entry["content_id"] = json!(cid);
        }
        entry
    });
    Ok(explicit.into_iter().chain(staged).collect())
}

/// `(content_id, filename)` of every attachment SendGrid will render inline,
/// for the `<img>` safety net on the HTML body.
fn inline_parts(attachments: &[Value]) -> impl Iterator<Item = (&str, &str)> {
    attachments
        .iter()
        .filter(|a| opt_str(a, "disposition") == Some("inline"))
        .filter_map(|a| {
            let cid = opt_str(a, "content_id")?;
            Some((cid, opt_str(a, "filename").unwrap_or_default()))
        })
}

/// Compile the flat tool input plus its personalizations into a v3
/// `mail/send` body, validating everything SendGrid would reject.
fn build_mail(
    client: &SendGridClient,
    input: &Value,
    personalizations: Vec<Value>,
) -> Result<Value> {
    if personalizations.is_empty() {
        return Err(eyre!("at least one message is required"));
    }
    if personalizations.len() > MAX_PERSONALIZATIONS {
        return Err(eyre!(
            "SendGrid accepts at most {MAX_PERSONALIZATIONS} messages per request (got {})",
            personalizations.len()
        ));
    }
    let recipients = recipient_count(&personalizations);
    if recipients > MAX_RECIPIENTS {
        return Err(eyre!(
            "SendGrid accepts at most {MAX_RECIPIENTS} recipients (to + cc + bcc) per request (got {recipients})"
        ));
    }
    let template_id = opt_str(input, "template_id");
    let subject = opt_str(input, "subject");
    let attachments = attachments(input)?;
    let content: Vec<Value> = [("text", "text/plain"), ("html", "text/html")]
        .into_iter()
        .filter_map(|(key, mime)| {
            let value = opt_str(input, key)?;
            let value = match key {
                "html" => html_with_inline_images(value, inline_parts(&attachments)),
                _ => value.to_string(),
            };
            Some(json!({ "type": mime, "value": value }))
        })
        .collect();
    if template_id.is_none() {
        let every_message_has_subject = personalizations.iter().all(|p| p.get("subject").is_some());
        if subject.is_none() && !every_message_has_subject {
            return Err(eyre!(
                "'subject' is required (top-level or on every message) unless 'template_id' is set"
            ));
        }
        if content.is_empty() {
            return Err(eyre!(
                "at least one of 'text' or 'html' is required unless 'template_id' is set"
            ));
        }
    }
    if input["categories"]
        .as_array()
        .is_some_and(|c| c.len() > MAX_CATEGORIES)
    {
        return Err(eyre!(
            "SendGrid allows at most {MAX_CATEGORIES} categories per message"
        ));
    }

    let mut mail = Map::new();
    mail.insert(
        "personalizations".to_string(),
        Value::Array(personalizations),
    );
    mail.insert("from".to_string(), resolve_from(client, input)?);
    if let Some(reply_to) = input.get("reply_to").filter(|v| !v.is_null()) {
        mail.insert(
            "reply_to".to_string(),
            address(reply_to).map_err(|e| eyre!("'reply_to': {e}"))?,
        );
    }
    if let Some(subject) = subject {
        mail.insert("subject".to_string(), json!(subject));
    }
    if !content.is_empty() {
        mail.insert("content".to_string(), Value::Array(content));
    }
    if let Some(template_id) = template_id {
        mail.insert("template_id".to_string(), json!(template_id));
    }
    copy_fields(
        &mut mail,
        input,
        &[
            "categories",
            "custom_args",
            "headers",
            "send_at",
            "batch_id",
        ],
    );
    if let Some(group_id) = opt_u64(input, "asm_group_id") {
        mail.insert("asm".to_string(), json!({ "group_id": group_id }));
    }
    if !attachments.is_empty() {
        mail.insert("attachments".to_string(), Value::Array(attachments));
    }
    if input["sandbox"].as_bool() == Some(true) {
        mail.insert(
            "mail_settings".to_string(),
            json!({ "sandbox_mode": { "enable": true } }),
        );
    }
    Ok(Value::Object(mail))
}

fn recipient_count(personalizations: &[Value]) -> usize {
    personalizations
        .iter()
        .flat_map(|p| ["to", "cc", "bcc"].map(|key| p[key].as_array().map_or(0, Vec::len)))
        .sum()
}

async fn send(client: &SendGridClient, body: Value) -> Result<Value> {
    let personalizations = body["personalizations"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    let messages = personalizations.len();
    let recipients = recipient_count(personalizations);
    let mut out = client.mail_send(&body).await?;
    out["messages"] = json!(messages);
    out["recipients"] = json!(recipients);
    info!(message_id = ?out["message_id"], messages, recipients, "SendGrid mail accepted");
    Ok(out)
}

fn address_count(value: Option<&Value>) -> usize {
    match value {
        Some(Value::Array(items)) => items.len(),
        Some(Value::String(_)) | Some(Value::Object(_)) => 1,
        _ => 0,
    }
}

fn from_clause(ctx: &Ctx, input: &Value) -> String {
    let explicit = match input.get("from") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Object(o)) => o.get("email").and_then(Value::as_str).map(str::to_string),
        _ => None,
    };
    explicit
        .or_else(|| {
            ctx.client(input)
                .ok()
                .and_then(|c| c.default_sender())
                .map(|s| s.email)
        })
        .map(|from| format!(" from {from}"))
        .unwrap_or_else(|| " (no From configured)".to_string())
}

fn subject_clause(input: &Value) -> String {
    opt_str(input, "subject")
        .map(str::to_string)
        .or_else(|| opt_str(input, "template_id").map(|t| format!("template {t}")))
        .unwrap_or_else(|| "(no subject)".to_string())
}

fn input_recipients(message: &Value) -> usize {
    ["to", "cc", "bcc"]
        .iter()
        .map(|key| address_count(message.get(*key)))
        .sum()
}

fn send_summary(ctx: &Ctx, input: &Value) -> Option<String> {
    Some(format!(
        "send 1 email{} to {} recipient(s): {}",
        from_clause(ctx, input),
        input_recipients(input),
        subject_clause(input)
    ))
}

fn batch_summary(ctx: &Ctx, input: &Value) -> Option<String> {
    let messages = input["messages"].as_array().map_or(&[][..], Vec::as_slice);
    let recipients: usize = messages.iter().map(input_recipients).sum();
    Some(format!(
        "send {} email(s){} to {recipients} recipient(s): {}",
        messages.len(),
        from_clause(ctx, input),
        subject_clause(input)
    ))
}

// ── Suppressions ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SuppressionKind {
    Bounces,
    Blocks,
    SpamReports,
    InvalidEmails,
    Unsubscribes,
}

impl SuppressionKind {
    const ALL: [SuppressionKind; 5] = [
        SuppressionKind::Bounces,
        SuppressionKind::Blocks,
        SuppressionKind::SpamReports,
        SuppressionKind::InvalidEmails,
        SuppressionKind::Unsubscribes,
    ];

    fn parse(raw: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == raw.trim())
            .ok_or_else(|| {
                eyre!(
                    "Unknown suppression kind '{raw}' (expected one of {})",
                    Self::names().join(", ")
                )
            })
    }

    fn as_str(self) -> &'static str {
        match self {
            SuppressionKind::Bounces => "bounces",
            SuppressionKind::Blocks => "blocks",
            SuppressionKind::SpamReports => "spam_reports",
            SuppressionKind::InvalidEmails => "invalid_emails",
            SuppressionKind::Unsubscribes => "unsubscribes",
        }
    }

    fn names() -> Vec<&'static str> {
        Self::ALL.into_iter().map(Self::as_str).collect()
    }

    fn list_path(self) -> String {
        format!("/v3/suppression/{}", self.as_str())
    }

    /// Global unsubscribes are addressed through the ASM API; the other
    /// lists live under `/v3/suppression`.
    fn item_path(self, email: &str) -> String {
        match self {
            SuppressionKind::Unsubscribes => format!("/v3/asm/suppressions/global/{email}"),
            kind => format!("/v3/suppression/{}/{email}", kind.as_str()),
        }
    }
}

fn kind_prop() -> Value {
    enum_prop(&SuppressionKind::names(), "Which suppression list")
}

fn suppression_target(input: &Value) -> Result<(SuppressionKind, String)> {
    let kind = SuppressionKind::parse(require_str(input, "kind")?)?;
    let email = email_segment(require_str(input, "email")?)?;
    Ok((kind, email.to_string()))
}

/// Normalise the two per-address shapes SendGrid returns — an array of
/// records, or `{recipient_email}` / `{}` for global unsubscribes.
fn suppression_records(kind: SuppressionKind, email: &str, response: Value) -> Value {
    let mut records = match response {
        Value::Array(items) => Value::Array(items),
        Value::Object(o) if o.is_empty() => Value::Array(Vec::new()),
        other => Value::Array(vec![other]),
    };
    wrap_each(&mut records, &["reason"], "sendgrid.suppression_get");
    let suppressed = records.as_array().is_some_and(|r| !r.is_empty());
    json!({ "kind": kind.as_str(), "email": email, "suppressed": suppressed, "records": records })
}

fn with_kind(kind: SuppressionKind, value: Value) -> Value {
    let mut out = count_items(value);
    if let Some(obj) = out.as_object_mut() {
        obj.insert("kind".to_string(), json!(kind.as_str()));
    }
    out
}

// ── The tool table ──────────────────────────────────────────────────────────

fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec::read(
            "sendgrid.senders_list",
            "List verified sender identities — the From addresses this account may send as.",
            obj(json!({
                "limit":        prop("integer", "Max results."),
                "last_seen_id": prop("integer", "Return senders with an id after this one (pagination).")
            })),
            Box::new(|c, input| {
                async move {
                    let mut query = Vec::new();
                    push_u64(&mut query, &input, "limit");
                    if let Some(id) = opt_u64(&input, "last_seen_id") {
                        query.push(("lastSeenID", id.to_string()));
                    }
                    let mut resp = c.get("/v3/verified_senders", &query).await?;
                    wrap_nested(
                        &mut resp,
                        "results",
                        &["nickname", "from_name", "reply_to_name"],
                        "sendgrid.senders_list",
                    );
                    Ok(resp)
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "sendgrid.domains_list",
            "List authenticated sending domains and whether their DNS validates.",
            obj(json!({
                "limit":  prop("integer", "Page size."),
                "offset": prop("integer", "Items to skip."),
                "domain": prop("string", "Filter by domain name.")
            })),
            Box::new(|c, input| {
                async move {
                    let mut query = Vec::new();
                    push_u64(&mut query, &input, "limit");
                    push_u64(&mut query, &input, "offset");
                    push_str(&mut query, &input, "domain");
                    Ok(count_items(c.get("/v3/whitelabel/domains", &query).await?))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "sendgrid.templates_list",
            "List transactional templates (dynamic and legacy) with their versions.",
            obj(json!({
                "generations": enum_prop(&["dynamic", "legacy", "legacy,dynamic"], "Template generations to include (default legacy,dynamic)."),
                "page_size":   prop("integer", "Templates per page, 1-200 (default 50)."),
                "page_token":  prop("string", "Cursor from a previous response's _metadata.")
            })),
            Box::new(|c, input| {
                async move {
                    let mut query = vec![
                        (
                            "generations",
                            opt_str(&input, "generations")
                                .unwrap_or("legacy,dynamic")
                                .to_string(),
                        ),
                        (
                            "page_size",
                            TEMPLATE_PAGE_SIZE
                                .clamp(opt_u64(&input, "page_size"))
                                .to_string(),
                        ),
                    ];
                    push_str(&mut query, &input, "page_token");
                    let mut resp = c.get("/v3/templates", &query).await?;
                    wrap_nested(&mut resp, "result", &["name"], "sendgrid.templates_list");
                    Ok(resp)
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "sendgrid.template_get",
            "Get one template with its versions (subject, bodies, active flag).",
            obj_required(
                json!({ "template_id": prop("string", "Template id (d-… for dynamic templates).") }),
                &["template_id"],
            ),
            Box::new(|c, input| {
                async move {
                    let id = path_segment(require_str(&input, "template_id")?)?;
                    let mut resp = c.get(&format!("/v3/templates/{id}"), &[]).await?;
                    wrap_fields_in_place(&mut resp, &["name"], "sendgrid.template_get");
                    if let Some(versions) = resp.get_mut("versions").and_then(Value::as_array_mut) {
                        for version in versions {
                            truncate_string_fields(
                                version,
                                &["html_content", "plain_content"],
                                TEMPLATE_BODY_MAX_BYTES,
                            );
                            wrap_fields_in_place(
                                version,
                                &["name", "subject", "html_content", "plain_content"],
                                "sendgrid.template_get.version",
                            );
                        }
                    }
                    Ok(resp)
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "sendgrid.stats_global",
            "Account-wide email statistics (requests, delivered, opens, clicks, bounces…) by day, week or month.",
            obj_required(
                json!({
                    "start_date":    prop("string", "YYYY-MM-DD (required)."),
                    "end_date":      prop("string", "YYYY-MM-DD (default today)."),
                    "aggregated_by": enum_prop(&["day", "week", "month"], "Bucket size."),
                    "limit":         prop("integer", "Max buckets."),
                    "offset":        prop("integer", "Buckets to skip.")
                }),
                &["start_date"],
            ),
            Box::new(|c, input| {
                async move {
                    let mut query = vec![("start_date", require_str(&input, "start_date")?.to_string())];
                    push_str(&mut query, &input, "end_date");
                    push_str(&mut query, &input, "aggregated_by");
                    push_u64(&mut query, &input, "limit");
                    push_u64(&mut query, &input, "offset");
                    Ok(count_items(c.get("/v3/stats", &query).await?))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "sendgrid.stats_categories",
            "Email statistics broken down by category (up to 10 categories, last 13 months).",
            obj_required(
                json!({
                    "start_date":    prop("string", "YYYY-MM-DD (required)."),
                    "categories":    string_list_prop("1-10 category names (required)."),
                    "end_date":      prop("string", "YYYY-MM-DD (default today)."),
                    "aggregated_by": enum_prop(&["day", "week", "month"], "Bucket size.")
                }),
                &["start_date", "categories"],
            ),
            Box::new(|c, input| {
                async move {
                    let categories: Vec<String> = input["categories"]
                        .as_array()
                        .map(|list| {
                            list.iter()
                                .filter_map(Value::as_str)
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    if categories.is_empty() || categories.len() > MAX_CATEGORIES {
                        return Err(eyre!(
                            "'categories' must list between 1 and {MAX_CATEGORIES} category names"
                        ));
                    }
                    let mut query = vec![("start_date", require_str(&input, "start_date")?.to_string())];
                    query.extend(categories.into_iter().map(|c| ("categories", c)));
                    push_str(&mut query, &input, "end_date");
                    push_str(&mut query, &input, "aggregated_by");
                    Ok(count_items(c.get("/v3/categories/stats", &query).await?))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "sendgrid.messages_search",
            "Search email activity (requires the Email Activity history add-on). Query grammar: to_email=\"a@b.com\" AND status=\"delivered\" AND last_event_time>\"2026-09-01T00:00:00Z\".",
            obj_required(
                json!({
                    "query": prop("string", "SendGrid activity query, e.g. to_email=\"a@b.com\" AND status=\"not_delivered\"."),
                    "limit": prop("integer", "Messages to return, 1-1000 (default 10).")
                }),
                &["query"],
            ),
            Box::new(|c, input| {
                async move {
                    let query = vec![
                        ("query", require_str(&input, "query")?.to_string()),
                        (
                            "limit",
                            MESSAGES_LIMIT.clamp(opt_u64(&input, "limit")).to_string(),
                        ),
                    ];
                    let mut resp = c.get("/v3/messages", &query).await?;
                    wrap_nested(&mut resp, "messages", &["subject"], "sendgrid.messages_search");
                    Ok(resp)
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "sendgrid.message_get",
            "Get one message's full event history (processed, delivered, bounced, opened, clicked…).",
            obj_required(
                json!({ "msg_id": prop("string", "Message id from messages_search or the send response.") }),
                &["msg_id"],
            ),
            Box::new(|c, input| {
                async move {
                    let id = path_segment(require_str(&input, "msg_id")?)?;
                    let mut resp = c.get(&format!("/v3/messages/{id}"), &[]).await?;
                    wrap_fields_in_place(&mut resp, &["subject"], "sendgrid.message_get");
                    wrap_nested(&mut resp, "events", &["reason"], "sendgrid.message_get.event");
                    Ok(resp)
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "sendgrid.suppressions_list",
            "List a suppression list: bounces, blocks, spam_reports, invalid_emails, or global unsubscribes.",
            obj_required(
                json!({
                    "kind":       kind_prop(),
                    "start_time": prop("integer", "Unix timestamp lower bound (inclusive)."),
                    "end_time":   prop("integer", "Unix timestamp upper bound (inclusive)."),
                    "limit":      prop("integer", "Page size, 1-500 (default 50)."),
                    "offset":     prop("integer", "Items to skip."),
                    "email":      prop("string", "Filter by address; %25 works as a wildcard.")
                }),
                &["kind"],
            ),
            Box::new(|c, input| {
                async move {
                    let kind = SuppressionKind::parse(require_str(&input, "kind")?)?;
                    let mut query = vec![(
                        "limit",
                        SUPPRESSION_LIMIT
                            .clamp(opt_u64(&input, "limit"))
                            .to_string(),
                    )];
                    push_u64(&mut query, &input, "start_time");
                    push_u64(&mut query, &input, "end_time");
                    push_u64(&mut query, &input, "offset");
                    push_str(&mut query, &input, "email");
                    let mut resp = c.get(&kind.list_path(), &query).await?;
                    wrap_each(&mut resp, &["reason"], "sendgrid.suppressions_list");
                    Ok(with_kind(kind, resp))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "sendgrid.suppression_get",
            "Check whether one address is on a suppression list and why.",
            obj_required(
                json!({ "kind": kind_prop(), "email": prop("string", "Address to look up.") }),
                &["kind", "email"],
            ),
            Box::new(|c, input| {
                async move {
                    let (kind, email) = suppression_target(&input)?;
                    let resp = c.get(&kind.item_path(&email), &[]).await?;
                    Ok(suppression_records(kind, &email, resp))
                }
                .boxed()
            }),
        ),
        ToolSpec::read(
            "sendgrid.unsubscribe_groups_list",
            "List unsubscribe (ASM) groups, for the asm_group_id on a send.",
            obj(json!({})),
            Box::new(|c, _input| {
                async move {
                    let mut resp = c.get("/v3/asm/groups", &[]).await?;
                    wrap_each(
                        &mut resp,
                        &["name", "description"],
                        "sendgrid.unsubscribe_groups_list",
                    );
                    Ok(count_items(resp))
                }
                .boxed()
            }),
        ),
        ToolSpec::write(
            "sendgrid.email_send",
            "Send one transactional email. Provide subject + text/html, or a template_id with dynamic_template_data. From defaults to the workspace sender.",
            obj_required(
                merged(
                    json!({
                        "to":                    recipients_prop("Recipient"),
                        "cc":                    recipients_prop("Cc"),
                        "bcc":                   recipients_prop("Bcc"),
                        "dynamic_template_data": prop("object", "Handlebars data for the dynamic template.")
                    }),
                    mail_envelope_props(),
                ),
                &["to"],
            ),
            Box::new(|c, input| {
                async move {
                    let personalization = personalization(&input, SINGLE_MESSAGE_FIELDS)?;
                    send(&c, build_mail(&c, &input, vec![personalization])?).await
                }
                .boxed()
            }),
        )
        .summarised(send_summary),
        ToolSpec::write(
            "sendgrid.email_send_batch",
            "Send up to 1000 personalised emails in one request: a shared envelope (from, subject, body or template) plus one `messages` entry per recipient set.",
            obj_required(
                merged(
                    json!({
                        "messages": {
                            "type": "array",
                            "description": "One entry per message; each carries its own recipients and may override subject, dynamic_template_data, custom_args, headers, send_at.",
                            "items": obj_required(message_props(), &["to"])
                        }
                    }),
                    mail_envelope_props(),
                ),
                &["messages"],
            ),
            Box::new(|c, input| {
                async move {
                    let messages = input["messages"]
                        .as_array()
                        .ok_or_else(|| eyre!("'messages' must be an array"))?;
                    let personalizations = messages
                        .iter()
                        .enumerate()
                        .map(|(i, m)| {
                            personalization(m, BATCH_MESSAGE_FIELDS)
                                .map_err(|e| eyre!("messages[{i}]: {e}"))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    send(&c, build_mail(&c, &input, personalizations)?).await
                }
                .boxed()
            }),
        )
        .summarised(batch_summary),
        ToolSpec::write(
            "sendgrid.suppression_delete",
            "Remove one address from a suppression list so mail to it is delivered again.",
            obj_required(
                json!({ "kind": kind_prop(), "email": prop("string", "Address to remove.") }),
                &["kind", "email"],
            ),
            Box::new(|c, input| {
                async move {
                    let (kind, email) = suppression_target(&input)?;
                    c.delete(&kind.item_path(&email)).await?;
                    Ok(json!({ "kind": kind.as_str(), "email": email, "deleted": true }))
                }
                .boxed()
            }),
        ),
        ToolSpec::write(
            "sendgrid.unsubscribes_add",
            "Add addresses to the global unsubscribe list (no mail of any kind will reach them).",
            obj_required(
                json!({ "emails": string_list_prop("Addresses to suppress globally.") }),
                &["emails"],
            ),
            Box::new(|c, input| {
                async move {
                    let emails = input["emails"]
                        .as_array()
                        .ok_or_else(|| eyre!("'emails' must be an array"))?
                        .iter()
                        .map(|v| {
                            v.as_str()
                                .ok_or_else(|| eyre!("'emails' entries must be strings"))
                                .and_then(valid_email)
                                .map(str::to_string)
                        })
                        .collect::<Result<Vec<_>>>()?;
                    if emails.is_empty() {
                        return Err(eyre!("'emails' needs at least one address"));
                    }
                    c.post(
                        "/v3/asm/suppressions/global",
                        &json!({ "recipient_emails": emails }),
                    )
                    .await
                }
                .boxed()
            }),
        ),
        ToolSpec::write(
            "sendgrid.api",
            "Call any SendGrid v3 endpoint not covered by the typed tools.",
            obj_required(
                json!({
                    "method": enum_prop(&["GET", "POST", "PUT", "PATCH", "DELETE"], "HTTP method."),
                    "path":   prop("string", "Endpoint path, e.g. '/v3/user/scheduled_sends' (the /v3 prefix is added if missing)."),
                    "query":  prop("object", "Query parameters as {key: value}."),
                    "body":   prop("object", "JSON body for POST/PUT/PATCH.")
                }),
                &["method", "path"],
            ),
            Box::new(|c, input| {
                async move {
                    let method = match require_str(&input, "method")?.to_ascii_uppercase().as_str() {
                        "GET" => Method::GET,
                        "POST" => Method::POST,
                        "PUT" => Method::PUT,
                        "PATCH" => Method::PATCH,
                        "DELETE" => Method::DELETE,
                        other => {
                            return Err(eyre!(
                                "Unsupported HTTP method '{other}' (expected GET/POST/PUT/PATCH/DELETE)"
                            ))
                        }
                    };
                    let path = v3_path(require_str(&input, "path")?)?;
                    let body = input.get("body").filter(|b| !b.is_null());
                    c.request(method, &path, &query_object(&input), body).await
                }
                .boxed()
            }),
        ),
    ]
}

pub(super) fn build_tools(clients: Clients, default_workspace: String) -> Vec<Arc<dyn Tool>> {
    let ctx = Ctx {
        clients,
        default_workspace,
    };
    specs()
        .into_iter()
        .map(|spec| {
            Arc::new(SendGridTool {
                ctx: ctx.clone(),
                spec,
            }) as Arc<dyn Tool>
        })
        .collect()
}

pub(super) fn tool_names() -> Vec<&'static str> {
    specs().into_iter().map(|s| s.name).collect()
}

#[cfg(test)]
pub(super) fn names_where(require_approval: bool) -> Vec<&'static str> {
    specs()
        .into_iter()
        .filter(|s| s.require_approval == require_approval)
        .map(|s| s.name)
        .collect()
}

pub(super) fn skill_tools_and_policies() -> (Vec<SkillTool>, Vec<SkillPolicy>) {
    specs()
        .into_iter()
        .map(|spec| {
            let tool = SkillTool {
                name: spec.name.to_string(),
                description: spec.description.to_string(),
                input_schema: with_workspace(spec.schema.clone(), generic_workspace_prop()),
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
    use crate::adapters::sendgrid::{Sender, READ_TOOLS, WRITE_TOOLS};
    use crate::adapters::test_support::InMem;
    use wiremock::matchers::{body_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sender() -> Sender {
        Sender {
            email: "noreply@example.com".into(),
            name: Some("Example".into()),
        }
    }

    fn ctx_for(server: &MockServer) -> Ctx {
        let mut clients = HashMap::new();
        clients.insert(
            "default".to_string(),
            Arc::new(
                SendGridClient::new("SG.default", Some(sender()), Some(&server.uri())).unwrap(),
            ),
        );
        clients.insert(
            "bare".to_string(),
            Arc::new(SendGridClient::new("SG.bare", None, Some(&server.uri())).unwrap()),
        );
        Ctx {
            clients: Arc::new(clients),
            default_workspace: "default".to_string(),
        }
    }

    fn tool(name: &str, ctx: &Ctx) -> Arc<dyn Tool> {
        build_tools(ctx.clients.clone(), ctx.default_workspace.clone())
            .into_iter()
            .find(|t| t.name() == name)
            .unwrap_or_else(|| panic!("tool {name} not found"))
    }

    fn default_client(ctx: &Ctx) -> Arc<SendGridClient> {
        ctx.client(&json!({})).unwrap()
    }

    fn message(input: Value, personalizations: Vec<Value>, ctx: &Ctx) -> Result<Value> {
        build_mail(&default_client(ctx), &input, personalizations)
    }

    async fn local_ctx() -> (MockServer, Ctx) {
        let server = MockServer::start().await;
        let ctx = ctx_for(&server);
        (server, ctx)
    }

    // ── addresses ──

    #[test]
    fn address_accepts_every_documented_shape() {
        assert_eq!(
            address(&json!("a@b.com")).unwrap(),
            json!({ "email": "a@b.com" })
        );
        assert_eq!(
            address(&json!("  Ada Lovelace <ada@b.com> ")).unwrap(),
            json!({ "email": "ada@b.com", "name": "Ada Lovelace" })
        );
        assert_eq!(
            address(&json!("\"Quoted\" <q@b.com>")).unwrap(),
            json!({ "email": "q@b.com", "name": "Quoted" })
        );
        assert_eq!(
            address(&json!({ "email": " o@b.com ", "name": "O" })).unwrap(),
            json!({ "email": "o@b.com", "name": "O" })
        );
        assert_eq!(
            address(&json!({ "email": "o@b.com", "name": "  " })).unwrap(),
            json!({ "email": "o@b.com" })
        );
    }

    #[test]
    fn address_rejects_non_addresses() {
        for bad in [
            json!("nope"),
            json!("a b@c.com"),
            json!(""),
            json!(42),
            json!({ "name": "x" }),
        ] {
            assert!(address(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn addresses_accepts_absent_single_and_list() {
        assert!(addresses(&json!({}), "to").unwrap().is_empty());
        assert_eq!(
            addresses(&json!({ "to": "a@b.com" }), "to").unwrap().len(),
            1
        );
        assert_eq!(
            addresses(&json!({ "to": ["a@b.com", "B <b@b.com>"] }), "to")
                .unwrap()
                .len(),
            2
        );
        let err = addresses(&json!({ "cc": ["a@b.com", "bad"] }), "cc").unwrap_err();
        assert!(err.to_string().starts_with("'cc':"), "{err}");
    }

    // ── payload compilation ──

    #[tokio::test]
    async fn build_mail_applies_default_sender_and_content() {
        let (_server, ctx) = local_ctx().await;
        let p = personalization(&json!({ "to": "a@b.com" }), SINGLE_MESSAGE_FIELDS).unwrap();
        let mail = message(
            json!({ "subject": "Hi", "text": "hello", "html": "<p>hello</p>", "categories": ["x"], "asm_group_id": 7, "sandbox": true, "send_at": 1, "batch_id": "b" }),
            vec![p],
            &ctx,
        )
        .unwrap();
        assert_eq!(
            mail["from"],
            json!({ "email": "noreply@example.com", "name": "Example" })
        );
        assert_eq!(mail["subject"], "Hi");
        assert_eq!(
            mail["content"][0],
            json!({ "type": "text/plain", "value": "hello" })
        );
        assert_eq!(mail["content"][1]["type"], "text/html");
        assert_eq!(mail["asm"], json!({ "group_id": 7 }));
        assert_eq!(mail["mail_settings"]["sandbox_mode"]["enable"], true);
        assert_eq!(mail["send_at"], 1);
        assert_eq!(mail["batch_id"], "b");
        assert_eq!(mail["personalizations"][0]["to"][0]["email"], "a@b.com");
        assert!(mail.get("template_id").is_none());
        assert!(mail.get("attachments").is_none());
    }

    #[tokio::test]
    async fn build_mail_explicit_from_and_reply_to_override_default() {
        let (_server, ctx) = local_ctx().await;
        let p = personalization(&json!({ "to": "a@b.com" }), SINGLE_MESSAGE_FIELDS).unwrap();
        let mail = message(
            json!({ "from": "Ops <ops@b.com>", "reply_to": { "email": "r@b.com" }, "template_id": "d-1" }),
            vec![p],
            &ctx,
        )
        .unwrap();
        assert_eq!(mail["from"], json!({ "email": "ops@b.com", "name": "Ops" }));
        assert_eq!(mail["reply_to"], json!({ "email": "r@b.com" }));
        assert_eq!(mail["template_id"], "d-1");
        assert!(mail.get("subject").is_none());
        assert!(mail.get("content").is_none());
    }

    #[tokio::test]
    async fn build_mail_rejects_what_sendgrid_would() {
        let (_server, ctx) = local_ctx().await;
        let p = || personalization(&json!({ "to": "a@b.com" }), SINGLE_MESSAGE_FIELDS).unwrap();
        let err = |input: Value, ps: Vec<Value>| message(input, ps, &ctx).unwrap_err().to_string();

        assert!(
            err(json!({ "subject": "s", "text": "t" }), vec![]).contains("at least one message")
        );
        let too_many: Vec<Value> = (0..=MAX_PERSONALIZATIONS).map(|_| p()).collect();
        assert!(err(json!({ "subject": "s", "text": "t" }), too_many).contains("at most 1000"));
        assert!(err(json!({ "text": "t" }), vec![p()]).contains("'subject' is required"));
        assert!(err(json!({ "subject": "s" }), vec![p()]).contains("'text' or 'html'"));
        let categories: Vec<&str> = vec!["c"; MAX_CATEGORIES + 1];
        assert!(err(
            json!({ "subject": "s", "text": "t", "categories": categories }),
            vec![p()]
        )
        .contains("at most 10 categories"));
        assert!(err(
            json!({ "subject": "s", "text": "t", "from": "nope" }),
            vec![p()]
        )
        .starts_with("'from':"));
        assert!(err(
            json!({ "subject": "s", "text": "t", "attachments": [{ "content": "Zm9v" }] }),
            vec![p()]
        )
        .contains("attachments[0] needs a non-empty 'filename'"));
    }

    #[tokio::test]
    async fn build_mail_without_any_from_names_the_vault_key() {
        let (_server, ctx) = local_ctx().await;
        let bare = ctx.client(&json!({ "workspace": "bare" })).unwrap();
        let p = personalization(&json!({ "to": "a@b.com" }), SINGLE_MESSAGE_FIELDS).unwrap();
        let err = build_mail(&bare, &json!({ "subject": "s", "text": "t" }), vec![p])
            .unwrap_err()
            .to_string();
        assert!(err.contains("from_email"), "{err}");
    }

    #[tokio::test]
    async fn per_message_subjects_satisfy_the_subject_rule() {
        let (_server, ctx) = local_ctx().await;
        let ps = vec![
            personalization(
                &json!({ "to": "a@b.com", "subject": "A" }),
                BATCH_MESSAGE_FIELDS,
            )
            .unwrap(),
            personalization(
                &json!({ "to": "b@b.com", "subject": "B" }),
                BATCH_MESSAGE_FIELDS,
            )
            .unwrap(),
        ];
        let mail = message(json!({ "text": "t" }), ps, &ctx).unwrap();
        assert_eq!(mail["personalizations"][1]["subject"], "B");
        assert!(mail.get("subject").is_none());
    }

    #[test]
    fn personalization_copies_only_the_permitted_fields() {
        let msg = json!({
            "to": ["a@b.com"], "cc": "c@b.com", "subject": "S",
            "dynamic_template_data": { "k": 1 }, "custom_args": { "x": "y" }, "send_at": 5
        });
        let single = personalization(&msg, SINGLE_MESSAGE_FIELDS).unwrap();
        assert_eq!(single["dynamic_template_data"]["k"], 1);
        assert!(single.get("subject").is_none());
        assert!(single.get("custom_args").is_none());
        assert_eq!(single["cc"][0]["email"], "c@b.com");
        assert!(single.get("bcc").is_none());

        let batch = personalization(&msg, BATCH_MESSAGE_FIELDS).unwrap();
        assert_eq!(batch["subject"], "S");
        assert_eq!(batch["custom_args"]["x"], "y");
        assert_eq!(batch["send_at"], 5);

        let blank = personalization(
            &json!({ "to": "a@b.com", "subject": "  " }),
            BATCH_MESSAGE_FIELDS,
        )
        .unwrap();
        assert!(blank.get("subject").is_none());
        assert!(personalization(&json!({ "cc": "c@b.com" }), SINGLE_MESSAGE_FIELDS).is_err());
    }

    #[tokio::test]
    async fn build_mail_appends_an_img_for_an_inline_attachment_the_html_omits() {
        let (_server, ctx) = local_ctx().await;
        let p = personalization(&json!({ "to": "a@b.com" }), SINGLE_MESSAGE_FIELDS).unwrap();
        let mail = message(
            json!({
                "subject": "s", "html": "<p>Numbers</p>",
                "attachments": [{ "content": "Zm9v", "filename": "q3.png", "type": "image/png",
                                  "disposition": "inline", "content_id": "q3" }]
            }),
            vec![p],
            &ctx,
        )
        .unwrap();
        assert_eq!(
            mail["content"][0]["value"],
            "<p>Numbers</p>\n<p><img src=\"cid:q3\" alt=\"q3.png\"></p>"
        );
    }

    #[tokio::test]
    async fn build_mail_carries_a_staged_inline_part_with_its_content_id() {
        use crate::adapters::utils::test_support::{staged_png, AGENT};
        use workforce_agent::runner::{CURRENT_AGENT_ID, CURRENT_ARTIFACT_STORE};

        let (_server, ctx) = local_ctx().await;
        let (store, id, _dir) = staged_png("signups.png");
        let p = personalization(&json!({ "to": "a@b.com" }), SINGLE_MESSAGE_FIELDS).unwrap();
        let input = json!({
            "subject": "s", "html": "<p>Numbers</p>",
            "attach_artifact_ids": [{ "artifact_id": id.as_str(), "inline": true }]
        });
        let mail = CURRENT_ARTIFACT_STORE
            .scope(
                store,
                CURRENT_AGENT_ID.scope(AGENT.to_string(), async { message(input, vec![p], &ctx) }),
            )
            .await
            .unwrap();
        let cid = id.as_str().trim_start_matches("art_");
        let attachment = &mail["attachments"][0];
        assert_eq!(attachment["filename"], "signups.png");
        assert_eq!(attachment["type"], "image/png");
        assert_eq!(attachment["disposition"], "inline");
        assert_eq!(attachment["content_id"], cid);
        assert!(mail["content"][0]["value"]
            .as_str()
            .unwrap()
            .contains(&format!("cid:{cid}")));
    }

    #[tokio::test]
    async fn build_mail_caps_total_recipients_and_passes_explicit_attachments() {
        let (_server, ctx) = local_ctx().await;
        let two = |i: usize| {
            personalization(
                &json!({ "to": [format!("a{i}@b.com"), format!("c{i}@b.com")] }),
                SINGLE_MESSAGE_FIELDS,
            )
            .unwrap()
        };
        let err = message(
            json!({ "subject": "s", "text": "t" }),
            (0..501).map(two).collect(),
            &ctx,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("at most 1000 recipients"), "{err}");
        assert!(message(
            json!({ "subject": "s", "text": "t" }),
            (0..500).map(two).collect(),
            &ctx
        )
        .is_ok());

        let p = personalization(&json!({ "to": "a@b.com" }), SINGLE_MESSAGE_FIELDS).unwrap();
        let mail = message(
            json!({ "subject": "s", "text": "t", "attachments": [{ "content": "Zm9v", "filename": "a.txt", "type": "text/plain" }] }),
            vec![p.clone()],
            &ctx,
        )
        .unwrap();
        assert_eq!(mail["attachments"][0]["filename"], "a.txt");

        let oversized = "A".repeat(MAX_MESSAGE_BYTES + 1);
        let err = message(
            json!({ "subject": "s", "text": "t", "attachments": [{ "content": oversized, "filename": "big.bin" }] }),
            vec![p],
            &ctx,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("30 MB"), "{err}");
    }

    // ── suppressions ──

    #[test]
    fn suppression_kind_parses_and_routes() {
        for name in SuppressionKind::names() {
            assert_eq!(SuppressionKind::parse(name).unwrap().as_str(), name);
        }
        assert!(SuppressionKind::parse("spam").is_err());
        assert_eq!(
            SuppressionKind::Bounces.item_path("a@b.com"),
            "/v3/suppression/bounces/a@b.com"
        );
        assert_eq!(
            SuppressionKind::Unsubscribes.item_path("a@b.com"),
            "/v3/asm/suppressions/global/a@b.com"
        );
        assert_eq!(
            SuppressionKind::Unsubscribes.list_path(),
            "/v3/suppression/unsubscribes"
        );
    }

    #[test]
    fn email_segment_rejects_path_breaking_addresses() {
        assert_eq!(email_segment(" a+b@c.d ").unwrap(), "a+b@c.d");
        for bad in ["a/b@c.d", "a?b@c.d", "a#b@c.d", "a b@c.d", "nope", ""] {
            assert!(email_segment(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn suppression_records_normalises_both_shapes() {
        let none = suppression_records(SuppressionKind::Unsubscribes, "a@b.com", json!({}));
        assert_eq!(none["suppressed"], false);
        assert_eq!(none["records"], json!([]));

        let global = suppression_records(
            SuppressionKind::Unsubscribes,
            "a@b.com",
            json!({ "recipient_email": "a@b.com" }),
        );
        assert_eq!(global["suppressed"], true);
        assert_eq!(global["records"][0]["recipient_email"], "a@b.com");

        let bounce = suppression_records(
            SuppressionKind::Bounces,
            "a@b.com",
            json!([{ "email": "a@b.com", "reason": "550 no such user" }]),
        );
        assert_eq!(bounce["kind"], "bounces");
        assert_eq!(bounce["suppressed"], true);
        assert!(bounce["records"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("550"));
    }

    // ── misc helpers ──

    #[test]
    fn v3_path_normalises_and_validates() {
        assert_eq!(v3_path("/templates").unwrap(), "/v3/templates");
        assert_eq!(v3_path("/v3/templates").unwrap(), "/v3/templates");
        assert_eq!(v3_path(" /v3 ").unwrap(), "/v3");
        assert!(v3_path("templates").is_err());
    }

    #[test]
    fn limits_clamp_with_defaults() {
        assert_eq!(TEMPLATE_PAGE_SIZE.clamp(None), 50);
        assert_eq!(TEMPLATE_PAGE_SIZE.clamp(Some(0)), 50);
        assert_eq!(TEMPLATE_PAGE_SIZE.clamp(Some(5000)), 200);
        assert_eq!(MESSAGES_LIMIT.clamp(Some(3)), 3);
        assert_eq!(SUPPRESSION_LIMIT.clamp(Some(999)), 500);
    }

    #[test]
    fn count_items_only_rewraps_arrays() {
        assert_eq!(
            count_items(json!([1, 2])),
            json!({ "count": 2, "items": [1, 2] })
        );
        assert_eq!(count_items(json!({ "a": 1 })), json!({ "a": 1 }));
    }

    #[tokio::test]
    async fn approval_summaries_name_from_recipients_and_subject() {
        let (_server, ctx) = local_ctx().await;
        let single = tool("sendgrid.email_send", &ctx);
        assert_eq!(
            single.approval_summary(
                &json!({ "to": ["a@b.com", "b@b.com"], "cc": "c@b.com", "subject": "Hi" })
            ),
            Some("send 1 email from noreply@example.com to 3 recipient(s): Hi".to_string())
        );
        assert_eq!(
            single.approval_summary(
                &json!({ "to": "a@b.com", "from": "Ops <ops@b.com>", "template_id": "d-1" })
            ),
            Some("send 1 email from Ops <ops@b.com> to 1 recipient(s): template d-1".to_string())
        );
        assert_eq!(
            single.approval_summary(&json!({ "to": "a@b.com", "workspace": "bare" })),
            Some("send 1 email (no From configured) to 1 recipient(s): (no subject)".to_string())
        );
        let batch = tool("sendgrid.email_send_batch", &ctx);
        assert_eq!(
            batch.approval_summary(&json!({ "messages": [{ "to": "a@b.com", "bcc": ["c@b.com", "d@b.com"] }, { "to": ["e@b.com"] }], "subject": "S" })),
            Some("send 2 email(s) from noreply@example.com to 4 recipient(s): S".to_string())
        );
        assert!(tool("sendgrid.stats_global", &ctx)
            .approval_summary(&json!({}))
            .is_none());
    }

    // ── end to end against a mock SendGrid ──

    #[tokio::test]
    async fn email_send_posts_the_compiled_payload_and_returns_the_id() {
        let (server, ctx) = local_ctx().await;
        Mock::given(method("POST"))
            .and(path("/v3/mail/send"))
            .and(header("authorization", "Bearer SG.default"))
            .and(body_json(json!({
                "personalizations": [{ "to": [{ "email": "a@b.com", "name": "A" }], "dynamic_template_data": { "k": 1 } }],
                "from": { "email": "noreply@example.com", "name": "Example" },
                "template_id": "d-1"
            })))
            .respond_with(ResponseTemplate::new(202).insert_header("X-Message-Id", "m-1"))
            .mount(&server)
            .await;
        let res = tool("sendgrid.email_send", &ctx)
            .execute(json!({ "to": "A <a@b.com>", "template_id": "d-1", "dynamic_template_data": { "k": 1 } }))
            .await
            .unwrap();
        assert!(res.success, "{:?}", res.error);
        assert_eq!(
            res.output,
            json!({ "accepted": true, "message_id": "m-1", "messages": 1, "recipients": 1 })
        );
    }

    #[tokio::test]
    async fn email_send_batch_posts_one_personalization_per_message() {
        let (server, ctx) = local_ctx().await;
        Mock::given(method("POST"))
            .and(path("/v3/mail/send"))
            .and(body_json(json!({
                "personalizations": [
                    { "to": [{ "email": "a@b.com" }], "subject": "For A" },
                    { "to": [{ "email": "b@b.com" }], "bcc": [{ "email": "c@b.com" }] }
                ],
                "from": { "email": "noreply@example.com", "name": "Example" },
                "subject": "Shared",
                "content": [{ "type": "text/plain", "value": "hi" }]
            })))
            .respond_with(ResponseTemplate::new(202).insert_header("X-Message-Id", "m-2"))
            .mount(&server)
            .await;
        let res = tool("sendgrid.email_send_batch", &ctx)
            .execute(json!({
                "subject": "Shared", "text": "hi",
                "messages": [{ "to": "a@b.com", "subject": "For A" }, { "to": ["b@b.com"], "bcc": "c@b.com" }]
            }))
            .await
            .unwrap();
        assert!(res.success, "{:?}", res.error);
        assert_eq!(res.output["messages"], 2);
        assert_eq!(res.output["recipients"], 3);
    }

    #[tokio::test]
    async fn send_validation_fails_before_any_request() {
        let (server, ctx) = local_ctx().await;
        let res = tool("sendgrid.email_send", &ctx)
            .execute(json!({ "to": "a@b.com", "text": "no subject" }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("'subject' is required"));
        assert!(server.received_requests().await.unwrap().is_empty());

        let res = tool("sendgrid.email_send_batch", &ctx)
            .execute(json!({ "subject": "s", "text": "t", "messages": [{ "to": "a@b.com" }, { "cc": "x@b.com" }] }))
            .await
            .unwrap();
        assert!(res.error.unwrap().starts_with("messages[1]:"));
    }

    #[tokio::test]
    async fn suppressions_list_hits_the_kind_path_with_clamped_limit() {
        let (server, ctx) = local_ctx().await;
        Mock::given(method("GET"))
            .and(path("/v3/suppression/bounces"))
            .and(query_param("limit", "500"))
            .and(query_param("start_time", "100"))
            .and(query_param("email", "a@b.com"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!([{ "email": "a@b.com", "reason": "550" }])),
            )
            .mount(&server)
            .await;
        let res = tool("sendgrid.suppressions_list", &ctx)
            .execute(
                json!({ "kind": "bounces", "limit": 9999, "start_time": 100, "email": "a@b.com" }),
            )
            .await
            .unwrap();
        assert!(res.success, "{:?}", res.error);
        assert_eq!(res.output["kind"], "bounces");
        assert_eq!(res.output["count"], 1);
        assert!(res.output["items"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("550"));
    }

    #[tokio::test]
    async fn suppression_get_and_delete_route_unsubscribes_through_asm() {
        let (server, ctx) = local_ctx().await;
        Mock::given(method("GET"))
            .and(path("/v3/asm/suppressions/global/a@b.com"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/v3/asm/suppressions/global/a@b.com"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let got = tool("sendgrid.suppression_get", &ctx)
            .execute(json!({ "kind": "unsubscribes", "email": "a@b.com" }))
            .await
            .unwrap();
        assert!(got.success, "{:?}", got.error);
        assert_eq!(got.output["suppressed"], false);

        let deleted = tool("sendgrid.suppression_delete", &ctx)
            .execute(json!({ "kind": "unsubscribes", "email": "a@b.com" }))
            .await
            .unwrap();
        assert!(deleted.success, "{:?}", deleted.error);
        assert_eq!(
            deleted.output,
            json!({ "kind": "unsubscribes", "email": "a@b.com", "deleted": true })
        );

        let bad = tool("sendgrid.suppression_delete", &ctx)
            .execute(json!({ "kind": "bounces", "email": "a/b@c.d" }))
            .await
            .unwrap();
        assert!(bad.error.unwrap().contains("path segment"));
    }

    #[tokio::test]
    async fn unsubscribes_add_validates_and_posts() {
        let (server, ctx) = local_ctx().await;
        Mock::given(method("POST"))
            .and(path("/v3/asm/suppressions/global"))
            .and(body_json(
                json!({ "recipient_emails": ["a@b.com", "b@b.com"] }),
            ))
            .respond_with(
                ResponseTemplate::new(201)
                    .set_body_json(json!({ "recipient_emails": ["a@b.com", "b@b.com"] })),
            )
            .mount(&server)
            .await;
        let res = tool("sendgrid.unsubscribes_add", &ctx)
            .execute(json!({ "emails": ["a@b.com", " b@b.com "] }))
            .await
            .unwrap();
        assert!(res.success, "{:?}", res.error);
        for bad in [
            json!({ "emails": [] }),
            json!({ "emails": ["nope"] }),
            json!({ "emails": "a@b.com" }),
        ] {
            let res = tool("sendgrid.unsubscribes_add", &ctx)
                .execute(bad)
                .await
                .unwrap();
            assert!(!res.success);
        }
    }

    #[tokio::test]
    async fn templates_list_clamps_page_size_and_wraps_names() {
        let (server, ctx) = local_ctx().await;
        Mock::given(method("GET"))
            .and(path("/v3/templates"))
            .and(query_param("generations", "legacy,dynamic"))
            .and(query_param("page_size", "200"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "result": [{ "id": "d-1", "name": "Welcome" }] })),
            )
            .mount(&server)
            .await;
        let res = tool("sendgrid.templates_list", &ctx)
            .execute(json!({ "page_size": 1000 }))
            .await
            .unwrap();
        assert!(res.success, "{:?}", res.error);
        let name = res.output["result"][0]["name"].as_str().unwrap();
        assert!(name.contains("Welcome") && name != "Welcome");
    }

    #[tokio::test]
    async fn stats_categories_repeats_the_category_param() {
        let (server, ctx) = local_ctx().await;
        Mock::given(method("GET"))
            .and(path("/v3/categories/stats"))
            .and(query_param("start_date", "2026-09-01"))
            .and(query_param("categories", "a"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;
        let res = tool("sendgrid.stats_categories", &ctx)
            .execute(json!({ "start_date": "2026-09-01", "categories": ["a", "b"] }))
            .await
            .unwrap();
        assert!(res.success, "{:?}", res.error);
        let url = server.received_requests().await.unwrap()[0].url.clone();
        assert!(
            url.query().unwrap().contains("categories=a&categories=b"),
            "{url}"
        );

        let res = tool("sendgrid.stats_categories", &ctx)
            .execute(json!({ "start_date": "2026-09-01", "categories": [] }))
            .await
            .unwrap();
        assert!(res.error.unwrap().contains("between 1 and 10"));
    }

    #[tokio::test]
    async fn api_escape_hatch_normalises_path_and_forwards_query_and_body() {
        let (server, ctx) = local_ctx().await;
        Mock::given(method("PATCH"))
            .and(path("/v3/user/scheduled_sends/b1"))
            .and(query_param("x", "1"))
            .and(body_json(json!({ "status": "pause" })))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let res = tool("sendgrid.api", &ctx)
            .execute(json!({ "method": "patch", "path": "/user/scheduled_sends/b1", "query": { "x": 1 }, "body": { "status": "pause" } }))
            .await
            .unwrap();
        assert!(res.success, "{:?}", res.error);
        assert_eq!(res.output, Value::Null);

        let res = tool("sendgrid.api", &ctx)
            .execute(json!({ "method": "GET", "path": "templates" }))
            .await
            .unwrap();
        assert!(res.error.unwrap().contains("must start with '/'"));

        let res = tool("sendgrid.api", &ctx)
            .execute(json!({ "method": "TRACE", "path": "/v3/templates" }))
            .await
            .unwrap();
        assert!(res
            .error
            .unwrap()
            .contains("Unsupported HTTP method 'TRACE'"));
    }

    #[tokio::test]
    async fn unknown_workspace_fails_cleanly_and_lists_options() {
        let (_server, ctx) = local_ctx().await;
        let res = tool("sendgrid.senders_list", &ctx)
            .execute(json!({ "workspace": "ghost" }))
            .await
            .unwrap();
        assert!(!res.success);
        let err = res.error.unwrap();
        assert!(err.contains("Unknown SendGrid workspace: 'ghost'"), "{err}");
        assert!(err.contains("bare, default"), "{err}");
    }

    #[tokio::test]
    async fn store_backed_client_resolves_sender_per_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v3/mail/send"))
            .and(header("authorization", "Bearer SG.vault"))
            .and(body_json(json!({
                "personalizations": [{ "to": [{ "email": "a@b.com" }] }],
                "from": { "email": "vault@example.com" },
                "subject": "s",
                "content": [{ "type": "text/plain", "value": "t" }]
            })))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        let store = InMem::from(&[
            ("skills/sendgrid/api_key", "SG.vault"),
            ("skills/sendgrid/from_email", "vault@example.com"),
        ]);
        let client = SendGridClient::with_store(store, "default", Some(&server.uri())).unwrap();
        let ctx = Ctx {
            clients: Arc::new(HashMap::from([("default".to_string(), Arc::new(client))])),
            default_workspace: "default".to_string(),
        };
        let res = tool("sendgrid.email_send", &ctx)
            .execute(json!({ "to": "a@b.com", "subject": "s", "text": "t" }))
            .await
            .unwrap();
        assert!(res.success, "{:?}", res.error);
        assert_eq!(res.output["message_id"], Value::Null);
    }

    // ── table invariants ──

    #[test]
    fn write_tools_require_approval_and_reads_allow() {
        let (_tools, policies) = skill_tools_and_policies();
        let action = |name: &str| {
            policies
                .iter()
                .find(|p| p.tool == name)
                .map(|p| p.action.as_str())
                .unwrap()
        };
        for name in READ_TOOLS {
            assert_eq!(action(name), "allow", "{name}");
        }
        for name in WRITE_TOOLS {
            assert_eq!(action(name), "require_approval", "{name}");
        }
        assert_eq!(names_where(false), READ_TOOLS);
        assert_eq!(names_where(true), WRITE_TOOLS);
    }

    #[tokio::test]
    async fn every_tool_has_a_workspace_prop_and_a_policy() {
        let (tools, policies) = skill_tools_and_policies();
        assert_eq!(tools.len(), policies.len());
        assert_eq!(tools.len(), tool_names().len());
        for t in &tools {
            assert!(
                t.input_schema["properties"].get("workspace").is_some(),
                "{} missing workspace prop",
                t.name
            );
        }
        let (_server, ctx) = local_ctx().await;
        let live = tool("sendgrid.email_send", &ctx).input_schema();
        let desc = live["properties"]["workspace"]["description"]
            .as_str()
            .unwrap();
        assert!(desc.contains("Available: bare, default"), "{desc}");
    }
}
