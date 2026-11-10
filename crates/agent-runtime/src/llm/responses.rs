//! Responses API client for the ChatGPT backend
//! (`chatgpt.com/backend-api/codex`), the wire under the hub's ChatGPT
//! subscription provider. It holds no credentials: every call takes the
//! login it runs on.
//!
//! Wire decisions are settled by the fixtures recorded in
//! `testdata/chatgpt/` (recorded on gpt-5.5, 2026-09-29; the model catalogue
//! on 2026-10-16): function-call item ids are not sent, `max_output_tokens`
//! is never sent (the backend 400s on it), and a reasoning item goes back
//! with its id, summary and encrypted content.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::StreamExt;
use reqwest::header::{HeaderMap, ACCEPT, RETRY_AFTER, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use url::Url;

use super::anthropic::CACHE_BREAK_MARKER;
use super::image_cap::{cap_images, RESPONSES_MAX_IMAGES};
use super::oai_common::budget_to_effort;
use super::provider::{effective_thinking_budget, reasoning_suppressed};
use super::sse::{event_data, SseFrames};
use super::streaming::StreamEvent;
use super::tool_schema::sanitize_tool_schema;
use super::types::{
    ContentBlock, LlmMessage, LlmToolDef, Role, StopReason, TokenUsage, OAI_REASONING_PREFIX,
};

/// The ChatGPT backend's Codex API root.
pub const CHATGPT_CODEX_BASE: &str = "https://chatgpt.com/backend-api/codex";

const ORIGINATOR: &str = "workforce";
const USER_AGENT_VALUE: &str = concat!("workforce/", env!("CARGO_PKG_VERSION"));

/// Context window of every model the backend serves on a ChatGPT plan but
/// gpt-6.1-sol (codex `models.json`), and of one it hasn't said.
pub const CONTEXT_WINDOW: u32 = 272_000;

/// A reasoning item rides in a [`ContentBlock::Thinking`] signature under
/// this prefix, so it round-trips through the runner untouched.
const REASONING_SIGNATURE_V1: &str = "v1:";

/// The login a call runs on.
#[derive(Debug, Clone, Copy)]
pub struct ChatGptAccess<'a> {
    pub access_token: &'a str,
    pub account_id: &'a str,
}

/// One request. `thinking_budget` is the client's configured budget; the
/// per-turn override and suppression scopes are applied to it here.
#[derive(Debug, Clone, Copy)]
pub struct Call<'a> {
    pub system: Option<&'a str>,
    pub messages: &'a [LlmMessage],
    pub tools: &'a [LlmToolDef],
    pub model: ChatGptModel,
    pub cache_key: &'a str,
    pub thinking_budget: Option<u32>,
}

/// A model the backend serves to a ChatGPT sign-in. The built-in ones are
/// the listed models of its catalogue as recorded
/// (`testdata/chatgpt/models.json`, 2026-10-16), each verified live to take
/// plain function tools; the backend's live catalogue
/// ([`ResponsesClient::models`]) adds the ones released since, once the hub
/// has read it ([`ChatGptModel::learn`]). A model it lists that a call then
/// finds refused moves the chain on like any other refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatGptModel {
    id: &'static str,
    context_window: u32,
    /// Its catalogue entry offers the backend's faster tier ([`FAST_TIER`]).
    fast: bool,
}

/// The service tier Codex calls Fast: what a request names to be served
/// ahead of the standard queue (`service_tier = "priority"`, CLI 0.166).
pub const FAST_TIER: &str = "priority";

/// The `client_version` the catalogue is asked for: the Codex CLI release
/// its listing was verified against.
pub const CATALOG_CLIENT_VERSION: &str = "0.166.0";

/// Models the backend no longer serves to a ChatGPT sign-in (OpenAI's Codex
/// changelog: gpt-5.5 on 2026-10-14, gpt-5.4 and gpt-5.4-mini on 2026-08-31,
/// gpt-5.3-codex and gpt-5.2 before that).
const RETIRED_MODELS: [&str; 5] = [
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.3-codex",
    "gpt-5.2",
];

/// A model name [`ChatGptModel::parse`] refuses. A call for either moves the
/// chain on.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnsupportedModel {
    #[error("{0} retired from ChatGPT sign-ins on 2026-10-14; use gpt-6.1-sol")]
    Retired(String),
    #[error("{0} is not served on a ChatGPT plan here; use gpt-6.1-sol")]
    Unknown(String),
}

/// The listed models of each login's live catalogue, as last read, by the
/// login's fingerprint (never its secret): plans differ, and one person's
/// list never decides what another's calls may name.
static LEARNED: std::sync::RwLock<BTreeMap<String, Vec<ChatGptModel>>> =
    std::sync::RwLock::new(BTreeMap::new());

fn learned_of(login: &str) -> Vec<ChatGptModel> {
    LEARNED
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(login)
        .cloned()
        .unwrap_or_default()
}

/// `name` as a `&'static str`: a built-in id, or one leaked once per new
/// name. The catalogue lists a handful of models, so this stays small.
fn intern(name: &str) -> &'static str {
    static NAMES: std::sync::Mutex<BTreeSet<&'static str>> = std::sync::Mutex::new(BTreeSet::new());
    if let Some(known) = ChatGptModel::ALL.iter().find(|m| m.id == name) {
        return known.id;
    }
    let mut names = NAMES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match names.get(name) {
        Some(&known) => known,
        None => {
            let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
            names.insert(leaked);
            leaked
        }
    }
}

impl ChatGptModel {
    pub const GPT_6_1_SOL: Self = Self::built_in("gpt-6.1-sol", 500_000);
    pub const GPT_6_ASTRA: Self = Self::built_in("gpt-6-astra", CONTEXT_WINDOW);
    pub const GPT_6_SOL: Self = Self::built_in("gpt-6-sol", CONTEXT_WINDOW);
    pub const GPT_6_LUNA: Self = Self::built_in("gpt-6-luna", CONTEXT_WINDOW);
    pub const GPT_5_6_SOL: Self = Self::built_in("gpt-5.6-sol", CONTEXT_WINDOW);
    pub const GPT_5_6_TERRA: Self = Self::built_in("gpt-5.6-terra", CONTEXT_WINDOW);
    pub const GPT_5_6_LUNA: Self = Self::built_in("gpt-5.6-luna", CONTEXT_WINDOW);

    /// The built-in models, served when the live catalogue can't be read.
    pub const ALL: &'static [Self] = &[
        Self::GPT_6_1_SOL,
        Self::GPT_6_ASTRA,
        Self::GPT_6_SOL,
        Self::GPT_6_LUNA,
        Self::GPT_5_6_SOL,
        Self::GPT_5_6_TERRA,
        Self::GPT_5_6_LUNA,
    ];

    /// A built-in model; whether it takes the faster tier is the
    /// catalogue's to say, so until it is read none does.
    const fn built_in(id: &'static str, context_window: u32) -> Self {
        Self {
            id,
            context_window,
            fast: false,
        }
    }

    pub const fn id(self) -> &'static str {
        self.id
    }

    /// The context window the catalogue reports for the model.
    pub const fn context_window(self) -> u32 {
        self.context_window
    }

    /// Whether its catalogue entry offers the faster tier ([`FAST_TIER`]).
    pub const fn fast(self) -> bool {
        self.fast
    }

    /// The model for `name` on the login `login` names: one its live
    /// catalogue lists, else a built-in one.
    pub fn parse_for(login: &str, name: &str) -> Result<Self, UnsupportedModel> {
        match learned_of(login).into_iter().find(|m| m.id == name) {
            Some(model) => Ok(model),
            None => Self::parse_built_in(name),
        }
    }

    /// The model for `name` as any login's catalogue or the built-in list
    /// knows it: only for a config check's warning, which runs for no one
    /// in particular. A call names its login ([`Self::parse_for`]).
    pub fn parse(name: &str) -> Result<Self, UnsupportedModel> {
        let learned = LEARNED
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match learned.values().flatten().find(|m| m.id == name) {
            Some(model) => Ok(*model),
            None => Self::parse_built_in(name),
        }
    }

    /// The built-in model for `name`, whatever the live catalogue says: what
    /// a config check reads, so its answer doesn't depend on whether the
    /// catalogue has been read yet.
    pub fn parse_built_in(name: &str) -> Result<Self, UnsupportedModel> {
        Self::ALL
            .iter()
            .copied()
            .find(|m| m.id == name)
            .ok_or_else(|| {
                if RETIRED_MODELS.contains(&name) {
                    UnsupportedModel::Retired(name.to_string())
                } else {
                    UnsupportedModel::Unknown(name.to_string())
                }
            })
    }

    /// Every model a call on `login` may name: its live catalogue's in its
    /// order, then the built-in ones it doesn't list.
    pub fn known_for(login: &str) -> Vec<Self> {
        let learned = learned_of(login);
        let built_in = Self::ALL
            .iter()
            .copied()
            .filter(|m| !learned.iter().any(|l| l.id == m.id));
        learned.iter().copied().chain(built_in).collect()
    }

    /// Whether `login`'s live catalogue has been read since the hub started.
    pub fn learned_for(login: &str) -> bool {
        !learned_of(login).is_empty()
    }

    /// Take the listed models of `catalog` (the backend's live list for the
    /// login `login` names) as the ones it serves that login, with their
    /// context windows and tiers.
    pub fn learn(login: &str, catalog: &[CatalogModel]) {
        let listed: Vec<Self> = catalog
            .iter()
            .filter(|m| m.listed())
            .map(|m| Self {
                id: intern(&m.slug),
                context_window: m.context_window.unwrap_or(CONTEXT_WINDOW),
                fast: m.fast_tier().is_some(),
            })
            .collect();
        if listed.is_empty() {
            return;
        }
        LEARNED
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(login.to_string(), listed);
    }
}

/// One model of the backend's catalogue (`GET …/codex/models`), as Codex
/// reads it: only what the hub uses, every field optional but the slug.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CatalogModel {
    pub slug: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// `list` for a model a picker shows; `hide` for one it doesn't.
    #[serde(default)]
    pub visibility: Option<String>,
    /// Lower first.
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub default_reasoning_level: Option<String>,
    #[serde(default)]
    pub supported_reasoning_levels: Vec<CatalogEffort>,
    #[serde(default)]
    pub service_tiers: Vec<CatalogTier>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CatalogEffort {
    pub effort: String,
}

/// A service tier a model offers, as the catalogue words it ("Fast", "2x
/// speed, increased usage").
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CatalogTier {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
}

impl CatalogModel {
    /// Whether a picker shows it.
    pub fn listed(&self) -> bool {
        self.visibility.as_deref() == Some("list")
    }

    /// Its faster tier, when it offers one.
    pub fn fast_tier(&self) -> Option<&CatalogTier> {
        self.service_tiers.iter().find(|t| t.id == FAST_TIER)
    }

    /// The efforts it takes that the hub sends ([`CHATGPT_EFFORTS`]),
    /// lowest first.
    pub fn efforts(&self) -> Vec<&'static str> {
        CHATGPT_EFFORTS
            .iter()
            .copied()
            .filter(|e| {
                self.supported_reasoning_levels
                    .iter()
                    .any(|l| l.effort == *e)
            })
            .collect()
    }
}

/// The models of a catalogue body, in its priority order: entries that
/// don't decode are skipped, never the whole list.
pub fn parse_catalog(body: &Value) -> Vec<CatalogModel> {
    let mut models: Vec<CatalogModel> = body["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| serde_json::from_value(m.clone()).ok())
        .collect();
    models.sort_by_key(|m| m.priority.unwrap_or(i64::MAX));
    models
}

/// Why a call failed. Each `Display` carries the `"ChatGPT API error
/// ({status}): …"` shape that `failover::classify_error` reads.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ResponsesError {
    #[error("ChatGPT API error (429): usage limit reached{}", limit_detail(.plan, .resets_at))]
    UsageLimit {
        resets_at: Option<DateTime<Utc>>,
        plan: Option<String>,
    },
    #[error("ChatGPT API error (429): rate limited{}", retry_after_hint(.retry_after))]
    Throttled { retry_after: Option<Duration> },
    #[error("ChatGPT API error (401): {0}")]
    Unauthorized(String),
    #[error("ChatGPT API error (403): {0}")]
    Forbidden(String),
    #[error("ChatGPT API error (400): {0}")]
    ModelNotInPlan(String),
    #[error("ChatGPT API error (400): context_length_exceeded: {0}")]
    ContextOverflow(String),
    #[error("ChatGPT API error (400): invalid_encrypted_content: {0}")]
    InvalidReasoning(String),
    #[error("ChatGPT API error (400): response stopped by the content filter ({0})")]
    ContentFiltered(String),
    #[error("ChatGPT API error (529): {0}")]
    Overloaded(String),
    #[error("ChatGPT API error ({status}): {body}")]
    Http { status: u16, body: String },
    #[error("ChatGPT API error: {0}")]
    Stream(String),
}

fn limit_detail(plan: &Option<String>, resets_at: &Option<DateTime<Utc>>) -> String {
    let plan = plan.as_deref().map(|p| format!(" on the {p} plan"));
    let reset = resets_at.map(|t| format!(", resets at {}", t.to_rfc3339()));
    format!("{}{}", plan.unwrap_or_default(), reset.unwrap_or_default())
}

fn retry_after_hint(retry_after: &Option<Duration>) -> String {
    retry_after
        .map(|d| format!(" [retry-after: {}s]", d.as_secs_f64().ceil() as u64))
        .unwrap_or_default()
}

/// Plan usage as the backend reports it, from the `x-codex-*` response
/// headers or a `codex.rate_limits` stream event.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LimitSnapshot {
    pub plan: Option<String>,
    pub active_limit: Option<String>,
    pub primary: Option<LimitWindow>,
    pub secondary: Option<LimitWindow>,
    pub credits: Option<Credits>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LimitWindow {
    pub used_percent: f64,
    pub window_minutes: Option<i64>,
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credits {
    pub has_credits: bool,
    pub unlimited: bool,
    pub balance: Option<String>,
}

impl LimitSnapshot {
    /// The snapshot in `headers`, or `None` when they carry no plan data.
    pub fn from_headers(headers: &HeaderMap) -> Option<Self> {
        let text = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|v| !v.is_empty())
        };
        let flag = |name: &str| {
            text(name).and_then(|v| match v.to_ascii_lowercase().as_str() {
                "true" | "1" => Some(true),
                "false" | "0" => Some(false),
                _ => None,
            })
        };
        let window = |which: &str| {
            LimitWindow::new(
                text(&format!("x-codex-{which}-used-percent"))?
                    .parse()
                    .ok()?,
                text(&format!("x-codex-{which}-window-minutes")).and_then(|v| v.parse().ok()),
                text(&format!("x-codex-{which}-reset-at")).and_then(|v| v.parse().ok()),
            )
        };
        let credits = flag("x-codex-credits-has-credits")
            .zip(flag("x-codex-credits-unlimited"))
            .map(|(has_credits, unlimited)| Credits {
                has_credits,
                unlimited,
                balance: text("x-codex-credits-balance").map(str::to_string),
            });
        let snapshot = Self {
            plan: text("x-codex-plan-type").map(str::to_string),
            active_limit: text("x-codex-active-limit").map(str::to_string),
            primary: window("primary"),
            secondary: window("secondary"),
            credits,
        };
        (snapshot != Self::default()).then_some(snapshot)
    }

    fn from_event(event: &Value) -> Self {
        let window = |w: &Value| {
            LimitWindow::new(
                w["used_percent"].as_f64()?,
                w["window_minutes"].as_i64(),
                w["reset_at"].as_i64(),
            )
        };
        let credits = &event["credits"];
        Self {
            plan: event["plan_type"].as_str().map(str::to_string),
            active_limit: event["metered_limit_name"]
                .as_str()
                .or_else(|| event["limit_name"].as_str())
                .map(str::to_string),
            primary: window(&event["rate_limits"]["primary"]),
            secondary: window(&event["rate_limits"]["secondary"]),
            credits: credits["has_credits"]
                .as_bool()
                .zip(credits["unlimited"].as_bool())
                .map(|(has_credits, unlimited)| Credits {
                    has_credits,
                    unlimited,
                    balance: credits["balance"].as_str().map(str::to_string),
                }),
        }
    }
}

impl LimitWindow {
    /// A window, or `None` when it carries nothing (Codex's rule: an unused,
    /// zero-length window with no reset is an absent one).
    fn new(used_percent: f64, window_minutes: Option<i64>, reset_at: Option<i64>) -> Option<Self> {
        let resets_at = reset_at.and_then(|s| DateTime::from_timestamp(s, 0));
        (used_percent.is_finite()
            && (used_percent != 0.0
                || window_minutes.is_some_and(|m| m != 0)
                || resets_at.is_some()))
        .then_some(Self {
            used_percent,
            window_minutes,
            resets_at,
        })
    }
}

/// A pure wire client for the ChatGPT backend's Responses endpoint.
#[derive(Debug, Clone)]
pub struct ResponsesClient {
    http: reqwest::Client,
    base: Url,
}

impl ResponsesClient {
    /// A client for `base` ([`CHATGPT_CODEX_BASE`], or a privacy gateway's
    /// route to it).
    pub fn new(http: reqwest::Client, base: Url) -> Self {
        Self { http, base }
    }

    fn post(&self, path: &str, access: &ChatGptAccess<'_>) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::POST, path, access)
    }

    fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        access: &ChatGptAccess<'_>,
    ) -> reqwest::RequestBuilder {
        let url = format!("{}/{path}", self.base.as_str().trim_end_matches('/'));
        self.http
            .request(method, url)
            .bearer_auth(access.access_token)
            .header("ChatGPT-Account-ID", access.account_id)
            .header("originator", ORIGINATOR)
            .header(USER_AGENT, USER_AGENT_VALUE)
    }

    /// The models the backend serves to this login, as Codex lists them
    /// (`GET models?client_version=…`, codex-rs `endpoint/models.rs`), in
    /// their priority order.
    pub async fn models(
        &self,
        access: &ChatGptAccess<'_>,
    ) -> Result<Vec<CatalogModel>, ResponsesError> {
        let response = self
            .request(reqwest::Method::GET, "models", access)
            .query(&[("client_version", CATALOG_CLIENT_VERSION)])
            .header(ACCEPT, "application/json")
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| ResponsesError::Stream(format!("catalogue request failed: {e}")))?;
        let response = error_for_status(response).await?;
        let body: Value = response
            .json()
            .await
            .map_err(|e| ResponsesError::Stream(format!("catalogue unreadable: {e}")))?;
        Ok(parse_catalog(&body))
    }

    /// Stream one response into `tx`. `limits` sees every plan-usage
    /// snapshot: the response headers (error responses included) and any
    /// `codex.rate_limits` event.
    pub async fn stream(
        &self,
        access: &ChatGptAccess<'_>,
        call: Call<'_>,
        tx: mpsc::Sender<StreamEvent>,
        limits: &(dyn Fn(LimitSnapshot) + Send + Sync),
    ) -> Result<(), ResponsesError> {
        let response = self
            .post("responses", access)
            .header(ACCEPT, "text/event-stream")
            .header("session-id", call.cache_key)
            .json(&request_body(&call))
            .send()
            .await
            .map_err(|e| ResponsesError::Stream(format!("request failed: {e}")))?;
        if let Some(snapshot) = LimitSnapshot::from_headers(response.headers()) {
            limits(snapshot);
        }
        let response = error_for_status(response).await?;

        let mut frames = SseFrames::default();
        let mut state = ResponseState::default();
        let mut body = response.bytes_stream();
        while let Some(chunk) = body.next().await {
            let chunk =
                chunk.map_err(|e| ResponsesError::Stream(format!("stream read failed: {e}")))?;
            for event in frames.push(&chunk).iter().filter_map(|f| sse_json(f)) {
                for emit in state.apply(&event)? {
                    match emit {
                        Emit::Limits(snapshot) => limits(snapshot),
                        Emit::Event(event) => {
                            let done = matches!(event, StreamEvent::MessageDone { .. });
                            if tx.send(event).await.is_err() || done {
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }
        Err(ResponsesError::Stream(
            "stream ended without response.completed".to_string(),
        ))
    }
}

async fn error_for_status(
    response: reqwest::Response,
) -> Result<reqwest::Response, ResponsesError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let headers = response.headers().clone();
    let body = response.text().await.unwrap_or_default();
    Err(http_error(status.as_u16(), &headers, &body))
}

// ─── Request ────────────────────────────────────────────────────────────────

fn request_body(call: &Call<'_>) -> Value {
    let messages = cap_images(Cow::Borrowed(call.messages), RESPONSES_MAX_IMAGES);
    let mut body = json!({
        "model": call.model.id(),
        "instructions": call
            .system
            .map(|s| s.replace(CACHE_BREAK_MARKER, "\n\n"))
            .unwrap_or_default(),
        "input": messages.iter().flat_map(input_items).collect::<Vec<_>>(),
        "tool_choice": "auto",
        "reasoning": reasoning(call.thinking_budget),
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": call.cache_key,
    });
    if !call.tools.is_empty() {
        body["tools"] = call.tools.iter().map(tool_def).collect();
        body["parallel_tool_calls"] = json!(true);
    }
    // Codex sends a tier only to a model its catalogue says takes it.
    if super::provider::fast_mode() && call.model.fast() {
        body["service_tier"] = json!(FAST_TIER);
    }
    body
}

/// The `reasoning.effort` levels the models here take, lowest first.
pub const CHATGPT_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh"];

/// The models here take low…xhigh, so "don't think" (suppression, or a zero
/// budget) is clamped to `low`, and `max` to `xhigh`. The turn's configured
/// effort beats the budget; neither leaves the model its default.
fn reasoning(configured: Option<u32>) -> Value {
    let effort = if reasoning_suppressed() {
        Some("low")
    } else if let Some(effort) = super::provider::effective_effort() {
        Some(match effort {
            super::anthropic::Effort::Max => "xhigh",
            effort => effort.as_str(),
        })
    } else {
        effective_thinking_budget(configured).map(|budget| match budget_to_effort(budget) {
            "none" => "low",
            effort => effort,
        })
    };
    match effort {
        Some(effort) => json!({ "effort": effort, "summary": "auto" }),
        None => json!({ "summary": "auto" }),
    }
}

fn tool_def(tool: &LlmToolDef) -> Value {
    let parameters = match sanitize_tool_schema(tool.input_schema.clone()) {
        Value::Object(mut schema) => {
            schema.entry("type").or_insert_with(|| json!("object"));
            Value::Object(schema)
        }
        _ => json!({ "type": "object", "properties": {} }),
    };
    json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": parameters,
        "strict": false,
    })
}

fn input_items(message: &LlmMessage) -> Vec<Value> {
    match message.role {
        Role::User => {
            let parts: Vec<Value> = message.content.iter().filter_map(input_part).collect();
            message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => Some(json!({
                        "type": "function_call_output",
                        "call_id": tool_use_id,
                        "output": content,
                    })),
                    _ => None,
                })
                .chain(
                    (!parts.is_empty())
                        .then(|| json!({ "type": "message", "role": "user", "content": parts })),
                )
                .collect()
        }
        Role::Assistant => message.content.iter().filter_map(output_item).collect(),
    }
}

fn input_part(block: &ContentBlock) -> Option<Value> {
    match block {
        ContentBlock::Text { text } => Some(json!({ "type": "input_text", "text": text })),
        ContentBlock::Image { source } => {
            Some(json!({ "type": "input_image", "image_url": source.data_url() }))
        }
        ContentBlock::Document { source } => Some(json!({
            "type": "input_file",
            "filename": format!(
                "attachment.{}",
                source.media_type.rsplit('/').next().unwrap_or("bin")
            ),
            "file_data": source.data_url(),
        })),
        _ => None,
    }
}

fn output_item(block: &ContentBlock) -> Option<Value> {
    match block {
        ContentBlock::Text { text } if !text.is_empty() => Some(json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": text }],
        })),
        ContentBlock::ToolUse {
            id, name, input, ..
        } => Some(json!({
            "type": "function_call",
            "call_id": id,
            "name": name,
            "arguments": input.to_string(),
        })),
        ContentBlock::Thinking { .. } => block
            .oai_reasoning_signature()
            .and_then(ReasoningItem::decode)
            .map(|item| item.input()),
        _ => None,
    }
}

/// A reasoning output item, carried in a Thinking signature so it can go
/// back verbatim on the next round (`store: false` keeps nothing server
/// side).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ReasoningItem {
    id: String,
    summary: Vec<String>,
    #[serde(rename = "enc")]
    encrypted_content: String,
}

impl ReasoningItem {
    fn from_output(item: &Value) -> Option<Self> {
        Some(Self {
            id: item["id"].as_str()?.to_string(),
            summary: summary_texts(item).map(str::to_string).collect(),
            encrypted_content: item["encrypted_content"].as_str()?.to_string(),
        })
    }

    fn encode(&self) -> String {
        format!(
            "{OAI_REASONING_PREFIX}{REASONING_SIGNATURE_V1}{}",
            serde_json::to_string(self).unwrap_or_default()
        )
    }

    fn decode(signature: &str) -> Option<Self> {
        signature
            .strip_prefix(OAI_REASONING_PREFIX)?
            .strip_prefix(REASONING_SIGNATURE_V1)
            .and_then(|payload| serde_json::from_str(payload).ok())
    }

    fn input(&self) -> Value {
        json!({
            "type": "reasoning",
            "id": self.id,
            "summary": self
                .summary
                .iter()
                .map(|text| json!({ "type": "summary_text", "text": text }))
                .collect::<Vec<_>>(),
            "content": [],
            "encrypted_content": self.encrypted_content,
        })
    }
}

/// The summary texts of a reasoning output item.
fn summary_texts(item: &Value) -> impl Iterator<Item = &str> {
    item["summary"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| part["text"].as_str())
}

// ─── Response stream ────────────────────────────────────────────────────────

/// The JSON payload of one SSE frame.
fn sse_json(frame: &str) -> Option<Value> {
    serde_json::from_str(&event_data(frame)?)
        .inspect_err(|error| tracing::debug!(%error, "Skipping unparsable ChatGPT SSE data"))
        .ok()
}

#[derive(Debug)]
enum Emit {
    Event(StreamEvent),
    Limits(LimitSnapshot),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ItemKind {
    Message,
    Reasoning,
    FunctionCall,
}

struct ItemState {
    kind: ItemKind,
    streamed: bool,
    summary_part: Option<u64>,
}

/// Maps Responses stream events onto [`StreamEvent`]s, keyed by each item's
/// `output_index`.
#[derive(Default)]
struct ResponseState {
    items: BTreeMap<usize, ItemState>,
    called_tools: bool,
    unknown: BTreeSet<String>,
}

impl ResponseState {
    fn apply(&mut self, event: &Value) -> Result<Vec<Emit>, ResponsesError> {
        let index = event["output_index"].as_u64().unwrap_or(0) as usize;
        let text = |key: &str| event[key].as_str().unwrap_or_default().to_string();
        let events = match event["type"].as_str().unwrap_or_default() {
            "response.output_item.added" => self.item_added(index, &event["item"]),
            "response.output_text.delta" | "response.refusal.delta" => {
                self.mark_streamed(index);
                vec![StreamEvent::TextDelta {
                    index,
                    text: text("delta"),
                }]
            }
            "response.reasoning_summary_text.delta" => {
                self.summary_delta(index, event["summary_index"].as_u64(), text("delta"))
            }
            "response.function_call_arguments.delta" => {
                self.mark_streamed(index);
                vec![StreamEvent::InputJsonDelta {
                    index,
                    partial: text("delta"),
                }]
            }
            "response.output_item.done" => self.item_done(index, &event["item"]),
            "response.completed" => vec![StreamEvent::MessageDone {
                stop_reason: if self.called_tools {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                },
                usage: usage(&event["response"]["usage"]),
            }],
            "response.incomplete" => {
                match event["response"]["incomplete_details"]["reason"].as_str() {
                    Some("max_output_tokens") => vec![StreamEvent::MessageDone {
                        stop_reason: StopReason::MaxTokens,
                        usage: usage(&event["response"]["usage"]),
                    }],
                    Some(reason @ "content_filter") => {
                        return Err(ResponsesError::ContentFiltered(reason.to_string()))
                    }
                    reason => {
                        return Err(ResponsesError::Stream(format!(
                            "response incomplete: {}",
                            reason.unwrap_or("unknown")
                        )))
                    }
                }
            }
            "response.failed" => return Err(failed_error(&event["response"]["error"])),
            "error" => return Err(failed_error(event)),
            "codex.rate_limits" => return Ok(vec![Emit::Limits(LimitSnapshot::from_event(event))]),
            "response.created"
            | "response.in_progress"
            | "response.content_part.added"
            | "response.content_part.done"
            | "response.output_text.done"
            | "response.refusal.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary_text.done"
            | "response.function_call_arguments.done"
            | "response.metadata"
            | "codex.response.metadata" => Vec::new(),
            other => {
                self.note_unknown("event", other);
                Vec::new()
            }
        };
        Ok(events.into_iter().map(Emit::Event).collect())
    }

    fn item_added(&mut self, index: usize, item: &Value) -> Vec<StreamEvent> {
        let (kind, event) = match item["type"].as_str().unwrap_or_default() {
            "message" => (ItemKind::Message, StreamEvent::ContentStart { index }),
            "reasoning" => (ItemKind::Reasoning, StreamEvent::ContentStart { index }),
            "function_call" => {
                self.called_tools = true;
                (
                    ItemKind::FunctionCall,
                    StreamEvent::ToolUseStart {
                        index,
                        id: item["call_id"].as_str().unwrap_or_default().to_string(),
                        name: item["name"].as_str().unwrap_or_default().to_string(),
                        signature: None,
                    },
                )
            }
            other => {
                self.note_unknown("item", other);
                return Vec::new();
            }
        };
        self.items.insert(
            index,
            ItemState {
                kind,
                streamed: false,
                summary_part: None,
            },
        );
        vec![event]
    }

    fn mark_streamed(&mut self, index: usize) {
        if let Some(item) = self.items.get_mut(&index) {
            item.streamed = true;
        }
    }

    fn summary_delta(
        &mut self,
        index: usize,
        part: Option<u64>,
        delta: String,
    ) -> Vec<StreamEvent> {
        let Some(item) = self.items.get_mut(&index) else {
            return Vec::new();
        };
        let separator = item
            .summary_part
            .is_some_and(|previous| Some(previous) != part)
            .then(|| StreamEvent::ThinkingDelta {
                index,
                thinking: "\n\n".to_string(),
            });
        item.summary_part = part;
        item.streamed = true;
        separator
            .into_iter()
            .chain(std::iter::once(StreamEvent::ThinkingDelta {
                index,
                thinking: delta,
            }))
            .collect()
    }

    /// Close the item. Content that arrived only in the done event is
    /// emitted first, and a reasoning item always closes with a
    /// `ContentStop`, since the next item's `ContentStart` clears the
    /// collector's buffers.
    fn item_done(&mut self, index: usize, item: &Value) -> Vec<StreamEvent> {
        let Some(state) = self.items.remove(&index) else {
            return Vec::new();
        };
        let unstreamed = |text: String| (!state.streamed && !text.is_empty()).then_some(text);
        let content = match state.kind {
            ItemKind::Message => unstreamed(
                item["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|part| part["text"].as_str().or(part["refusal"].as_str()))
                    .collect(),
            )
            .map(|text| StreamEvent::TextDelta { index, text })
            .into_iter()
            .collect(),
            ItemKind::FunctionCall => {
                unstreamed(item["arguments"].as_str().unwrap_or_default().to_string())
                    .map(|partial| StreamEvent::InputJsonDelta { index, partial })
                    .into_iter()
                    .collect()
            }
            ItemKind::Reasoning => {
                let reasoning = ReasoningItem::from_output(item);
                let summary = unstreamed(summary_texts(item).collect::<Vec<_>>().join("\n\n"))
                    .map(|thinking| StreamEvent::ThinkingDelta { index, thinking });
                summary
                    .into_iter()
                    .chain(reasoning.map(|r| StreamEvent::SignatureDelta {
                        index,
                        signature: r.encode(),
                    }))
                    .collect::<Vec<_>>()
            }
        };
        content
            .into_iter()
            .chain(std::iter::once(StreamEvent::ContentStop { index }))
            .collect()
    }

    fn note_unknown(&mut self, what: &str, kind: &str) {
        if self.unknown.insert(format!("{what}:{kind}")) {
            tracing::debug!(what, kind, "Ignoring unknown ChatGPT response {what}");
        }
    }
}

fn usage(usage: &Value) -> TokenUsage {
    let count = |v: &Value| v.as_u64().unwrap_or(0) as u32;
    TokenUsage::from_oai(
        count(&usage["input_tokens"]),
        count(&usage["output_tokens"]),
        count(&usage["input_tokens_details"]["cached_tokens"]),
        count(&usage["input_tokens_details"]["cache_write_tokens"]),
    )
}

// ─── Errors ─────────────────────────────────────────────────────────────────

/// The error a `response.failed` / `error` event's error object names.
fn failed_error(error: &Value) -> ResponsesError {
    let message = error["message"].as_str().unwrap_or_default().to_string();
    match error["code"].as_str().or_else(|| error["type"].as_str()) {
        Some("context_length_exceeded") => ResponsesError::ContextOverflow(message),
        Some("server_is_overloaded" | "slow_down") => ResponsesError::Overloaded(message),
        Some("invalid_encrypted_content") => ResponsesError::InvalidReasoning(message),
        Some("usage_not_included") => ResponsesError::ModelNotInPlan(message),
        Some("usage_limit_reached") => usage_limit(error),
        Some("rate_limit_exceeded") => ResponsesError::Throttled {
            retry_after: retry_after_in(&message),
        },
        code => ResponsesError::Stream(format!(
            "response failed ({}): {message}",
            code.unwrap_or("unknown")
        )),
    }
}

fn usage_limit(error: &Value) -> ResponsesError {
    ResponsesError::UsageLimit {
        resets_at: error["resets_at"]
            .as_i64()
            .and_then(|s| DateTime::from_timestamp(s, 0)),
        plan: error["plan_type"].as_str().map(str::to_string),
    }
}

/// The error an HTTP error response names. Bodies come as
/// `{"detail": "…"}` or `{"error": {"type", "code", "message", …}}`.
fn http_error(status: u16, headers: &HeaderMap, body: &str) -> ResponsesError {
    let json: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let error = &json["error"];
    let message = json["detail"]
        .as_str()
        .or_else(|| error["message"].as_str())
        .unwrap_or(body)
        .to_string();
    let code = error["code"].as_str();
    match status {
        401 => ResponsesError::Unauthorized(message),
        403 if !(body.contains("Cloudflare") && body.contains("blocked")) => {
            ResponsesError::Forbidden(message)
        }
        429 => match error["type"].as_str() {
            Some("usage_limit_reached") => usage_limit(error),
            Some("usage_not_included") => ResponsesError::ModelNotInPlan(message),
            _ => ResponsesError::Throttled {
                retry_after: headers
                    .get(RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<f64>().ok())
                    .filter(|s| s.is_finite() && *s >= 0.0)
                    .map(Duration::from_secs_f64)
                    .or_else(|| retry_after_in(&message)),
            },
        },
        _ if matches!(code, Some("server_is_overloaded" | "slow_down")) => {
            ResponsesError::Overloaded(message)
        }
        _ if code == Some("context_length_exceeded") => ResponsesError::ContextOverflow(message),
        _ if code == Some("invalid_encrypted_content") => ResponsesError::InvalidReasoning(message),
        404 if error["param"].as_str() == Some("model") => ResponsesError::ModelNotInPlan(message),
        400 if message.contains("not supported when using Codex with a ChatGPT account") => {
            ResponsesError::ModelNotInPlan(message)
        }
        _ => ResponsesError::Http {
            status,
            body: body.chars().take(500).collect(),
        },
    }
}

/// The wait a rate-limit message names ("… try again in 11.054s").
fn retry_after_in(message: &str) -> Option<Duration> {
    static TRY_AGAIN: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?i)try again in\s*(\d+(?:\.\d+)?)\s*(ms|s|seconds?)\b")
            .expect("static regex")
    });
    let captures = TRY_AGAIN.captures(message)?;
    let value: f64 = captures[1].parse().ok()?;
    Some(match captures[2].to_ascii_lowercase().as_str() {
        "ms" => Duration::from_secs_f64(value / 1000.0),
        _ => Duration::from_secs_f64(value),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::failover::{classify_error, ErrorKind};
    use crate::llm::provider::{with_reasoning_suppressed, with_thinking_budget_override};
    use crate::llm::streaming::StreamCollector;
    use crate::llm::types::ImageSource;
    use reqwest::header::{HeaderName, HeaderValue};

    const SYSTEM: &str = "You are Allan, a concise assistant. Use tools when asked.";

    /// A recorded exchange: status line, headers, blank line, body.
    fn recorded(raw: &str) -> (u16, HeaderMap, &str) {
        let (head, body) = raw.split_once("\n\n").expect("head and body");
        let mut lines = head.lines();
        let status = lines
            .next()
            .and_then(|l| l.strip_prefix("HTTP "))
            .and_then(|s| s.parse().ok())
            .expect("status line");
        let headers = lines
            .filter_map(|l| l.split_once(':'))
            .filter_map(|(k, v)| {
                Some((
                    HeaderName::from_bytes(k.trim().as_bytes()).ok()?,
                    HeaderValue::from_str(v.trim()).ok()?,
                ))
            })
            .collect();
        (status, headers, body)
    }

    fn emits(body: &str) -> Result<Vec<Emit>, ResponsesError> {
        let mut state = ResponseState::default();
        let mut out = Vec::new();
        for event in SseFrames::default()
            .push(body.as_bytes())
            .iter()
            .filter_map(|f| sse_json(f))
        {
            out.extend(state.apply(&event)?);
        }
        Ok(out)
    }

    fn events(body: &str) -> Vec<StreamEvent> {
        emits(body)
            .expect("stream parses")
            .into_iter()
            .filter_map(|e| match e {
                Emit::Event(event) => Some(event),
                Emit::Limits(_) => None,
            })
            .collect()
    }

    fn collected(body: &str) -> (Vec<ContentBlock>, StopReason, TokenUsage) {
        let mut collector = StreamCollector::new();
        events(body)
            .into_iter()
            .try_for_each(|e| collector.process(e))
            .expect("no error events");
        collector.finish()
    }

    /// A recorded request, on the model the test calls run: the fixtures were
    /// recorded on gpt-5.5 before it retired from ChatGPT sign-ins, and the
    /// model id is no wire decision.
    fn request_fixture(raw: &str) -> Value {
        let mut fixture: Value = serde_json::from_str(raw).expect("fixture json");
        fixture["model"] = json!(ChatGptModel::GPT_6_1_SOL.id());
        fixture
    }

    fn city_tool(name: &str, description: &str) -> LlmToolDef {
        LlmToolDef {
            name: name.into(),
            description: description.into(),
            input_schema: json!({
                "type": "object",
                "properties": { "city": { "type": "string" } },
                "required": ["city"],
            }),
        }
    }

    fn tools() -> Vec<LlmToolDef> {
        vec![
            city_tool("weather_get", "Get the weather for a city"),
            city_tool("time_get", "Get the local time for a city"),
        ]
    }

    fn call<'a>(
        messages: &'a [LlmMessage],
        tools: &'a [LlmToolDef],
        cache_key: &'a str,
        thinking_budget: Option<u32>,
    ) -> Call<'a> {
        Call {
            system: Some(SYSTEM),
            messages,
            tools,
            model: ChatGptModel::GPT_6_1_SOL,
            cache_key,
            thinking_budget,
        }
    }

    fn assistant(content: Vec<ContentBlock>) -> LlmMessage {
        LlmMessage {
            role: Role::Assistant,
            content,
        }
    }

    // ── Request ─────────────────────────────────────────────────────────

    #[test]
    fn responses_request_matches_captured_body() {
        let tools = tools();
        let messages = [
            LlmMessage::user("What are the weather and the local time in Lisbon? Call both tools."),
            assistant(vec![
                ContentBlock::tool_use(
                    "call_i0R5QIs1iUydymKO9v7MRxoP".into(),
                    "weather_get".into(),
                    json!({"city": "Lisbon"}),
                ),
                ContentBlock::tool_use(
                    "call_k94qa7gmcyenx7QAN6oXUVxM".into(),
                    "time_get".into(),
                    json!({"city": "Lisbon"}),
                ),
            ]),
            LlmMessage::tool_results(vec![
                (
                    "call_i0R5QIs1iUydymKO9v7MRxoP".into(),
                    "sunny 21C".into(),
                    false,
                ),
                (
                    "call_k94qa7gmcyenx7QAN6oXUVxM".into(),
                    "10:42".into(),
                    false,
                ),
            ]),
        ];
        let body = request_body(&call(
            &messages,
            &tools,
            "00000000-0000-4000-8000-000000000001",
            Some(5_000),
        ));

        assert_eq!(
            body,
            request_fixture(include_str!(
                "testdata/chatgpt/tool_round2_without_ids.request.json"
            ))
        );
        assert!(
            body.get("max_output_tokens").is_none(),
            "the backend 400s on max_output_tokens (testdata/chatgpt/max_output_tokens.sse)"
        );
    }

    #[test]
    fn responses_input_matches_captured_media_and_history() {
        let input = |messages: &[LlmMessage]| {
            request_body(&call(messages, &[], "k", None))["input"].clone()
        };
        let recorded_input = |raw: &str| request_fixture(raw)["input"].clone();

        let history = [
            LlmMessage::user("My name is Kai."),
            LlmMessage::assistant("Nice to meet you, Kai."),
            LlmMessage::user("What is my name?"),
        ];
        assert_eq!(
            input(&history),
            recorded_input(include_str!(
                "testdata/chatgpt/assistant_history.request.json"
            ))
        );

        let image = recorded_input(include_str!("testdata/chatgpt/input_image.request.json"));
        let png = image[0]["content"][0]["image_url"]
            .as_str()
            .and_then(|u| u.strip_prefix("data:image/png;base64,"))
            .expect("recorded png");
        let with_image = [LlmMessage {
            role: Role::User,
            content: vec![
                ContentBlock::image_base64("image/png", png.to_string()),
                ContentBlock::Text {
                    text: "What word is in this image and what colour is it?".into(),
                },
            ],
        }];
        assert_eq!(input(&with_image), image);

        let mut pdf = recorded_input(include_str!("testdata/chatgpt/input_file_pdf.request.json"));
        let data = pdf[0]["content"][0]["file_data"]
            .as_str()
            .and_then(|u| u.strip_prefix("data:application/pdf;base64,"))
            .expect("recorded pdf")
            .to_string();
        let with_pdf = [LlmMessage {
            role: Role::User,
            content: vec![
                ContentBlock::Document {
                    source: ImageSource {
                        source_type: "base64".into(),
                        media_type: "application/pdf".into(),
                        data,
                    },
                },
                ContentBlock::Text {
                    text: "What number is on this invoice?".into(),
                },
            ],
        }];
        pdf[0]["content"][0]["filename"] = json!("attachment.pdf");
        assert_eq!(input(&with_pdf), pdf);
    }

    #[test]
    fn tool_results_beside_media_follow_as_a_user_message() {
        let messages = [LlmMessage {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "staged".into(),
                    is_error: None,
                    loaded: None,
                },
                ContentBlock::image_base64("image/png", "AAAA".into()),
            ],
        }];
        let input = request_body(&call(&messages, &[], "k", None))["input"].clone();
        assert_eq!(
            input,
            json!([
                {"type": "function_call_output", "call_id": "call_1", "output": "staged"},
                {"type": "message", "role": "user", "content": [
                    {"type": "input_image", "image_url": "data:image/png;base64,AAAA"}
                ]},
            ])
        );
    }

    #[test]
    fn a_request_keeps_within_the_responses_image_limit() {
        let image = |tag: &str| ContentBlock::image_base64("image/png", tag.into());
        let text = |text: String| ContentBlock::Text { text };
        let frames = 8;
        let video = (1..=frames).flat_map(|i| {
            [
                text(format!(
                    "{}:",
                    crate::llm::image_cap::frame_label(i, frames, "pan.mp4", "0:01.0")
                )),
                image(&format!("frame{i}")),
            ]
        });
        let photos = |n: usize| (0..n).map(|i| image(&format!("photo{i}")));
        let sent = |content: Vec<ContentBlock>| {
            let messages = [LlmMessage {
                role: Role::User,
                content,
            }];
            let body = request_body(&call(&messages, &[], "k", None));
            body["input"][0]["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|part| part["image_url"].as_str())
                .map(|url| url.rsplit(',').next().unwrap().to_string())
                .collect::<Vec<_>>()
        };

        let within: Vec<ContentBlock> = photos(2).chain(video.clone()).collect();
        assert_eq!(sent(within).len(), 2 + frames, "a video's frames all go");

        let over = 3;
        let kept = sent(
            photos(RESPONSES_MAX_IMAGES - frames + over)
                .chain(video)
                .collect(),
        );
        assert_eq!(kept.len(), RESPONSES_MAX_IMAGES);
        assert_eq!(
            kept.iter().filter(|tag| tag.starts_with("photo")).count(),
            RESPONSES_MAX_IMAGES - frames + over,
            "frames give way before any other image"
        );
        assert_eq!(
            kept.iter()
                .filter(|tag| tag.starts_with("frame"))
                .collect::<Vec<_>>(),
            ["frame4", "frame5", "frame6", "frame7", "frame8"],
            "the oldest frames first"
        );
    }

    #[tokio::test]
    async fn suppressed_reasoning_sends_low_not_none() {
        let effort = |budget: Option<u32>| reasoning(budget)["effort"].clone();

        assert_eq!(
            with_reasoning_suppressed(async { effort(Some(20_000)) }).await,
            "low"
        );
        assert_eq!(
            with_thinking_budget_override(Some(0), async { effort(Some(20_000)) }).await,
            "low"
        );
        assert_eq!(effort(Some(0)), "low");
        assert_eq!(effort(Some(5_000)), "medium");
        assert_eq!(reasoning(None), json!({ "summary": "auto" }));
    }

    #[tokio::test]
    async fn the_configured_effort_is_the_reasoning_effort_unless_reasoning_is_held_back() {
        use crate::llm::anthropic::Effort;
        use crate::llm::provider::with_effort_override;
        let effort = |budget: Option<u32>| reasoning(budget)["effort"].clone();
        for (level, sent) in [
            (Effort::Low, "low"),
            (Effort::Medium, "medium"),
            (Effort::High, "high"),
            (Effort::Xhigh, "xhigh"),
            (Effort::Max, "xhigh"),
        ] {
            assert_eq!(
                with_effort_override(Some(level), async { effort(Some(1_000)) }).await,
                sent
            );
            assert_eq!(
                with_effort_override(Some(level), async { effort(None) }).await,
                sent
            );
        }
        assert_eq!(
            with_effort_override(
                Some(Effort::Xhigh),
                with_reasoning_suppressed(async { effort(None) })
            )
            .await,
            "low",
            "a background extraction still thinks the least"
        );
        assert_eq!(
            with_effort_override(
                Some(Effort::Xhigh),
                with_thinking_budget_override(Some(0), async { effort(None) })
            )
            .await,
            "low",
            "a voice turn's budget beats the agent's effort"
        );
        for level in CHATGPT_EFFORTS {
            assert!(Effort::parse(level).is_some(), "{level}");
        }
    }

    #[test]
    fn the_live_catalogue_is_read_in_priority_order_and_bad_entries_are_skipped() {
        let body: Value =
            serde_json::from_str(include_str!("testdata/chatgpt/models_live.json")).expect("json");
        let models = parse_catalog(&body);
        let slugs: Vec<&str> = models.iter().map(|m| m.slug.as_str()).collect();
        assert_eq!(
            slugs,
            [
                "gpt-6.1-sol",
                "gpt-6-astra",
                "gpt-6.2-nova",
                "gpt-6-luna",
                "codex-auto-review"
            ]
        );
        let nova = &models[2];
        assert!(nova.listed());
        assert_eq!(nova.efforts(), ["low", "medium", "high"]);
        assert_eq!(
            nova.fast_tier().map(|t| t.description.as_str()),
            Some("1.5x speed")
        );
        assert_eq!(
            models[0].efforts(),
            CHATGPT_EFFORTS,
            "max and ultra are not sent"
        );
        assert!(models[3].fast_tier().is_none());
        assert!(!models[4].listed());
        // The recorded catalogue, from before the backend listed tiers.
        let recorded: Value =
            serde_json::from_str(include_str!("testdata/chatgpt/models.json")).expect("json");
        assert_eq!(parse_catalog(&recorded).len(), 9);
    }

    #[test]
    fn a_model_the_live_catalogue_lists_parses_with_its_window_and_tier() {
        // Its own slug, so no other test's built-in model changes under it.
        let catalog = [CatalogModel {
            slug: "gpt-test-learned".into(),
            display_name: None,
            description: None,
            visibility: Some("list".into()),
            priority: Some(1),
            context_window: Some(400_000),
            default_reasoning_level: None,
            supported_reasoning_levels: Vec::new(),
            service_tiers: vec![CatalogTier {
                id: FAST_TIER.into(),
                name: "Fast".into(),
                description: "1.5x speed".into(),
            }],
        }];
        assert!(ChatGptModel::parse_for("acct-a", "gpt-test-learned").is_err());
        ChatGptModel::learn("acct-a", &catalog);
        let learned = ChatGptModel::parse_for("acct-a", "gpt-test-learned").expect("learned");
        assert!(
            ChatGptModel::parse_for("acct-b", "gpt-test-learned").is_err(),
            "another login's list is its own"
        );
        assert!(!ChatGptModel::learned_for("acct-b"));
        assert_eq!(learned.id(), "gpt-test-learned");
        assert_eq!(learned.context_window(), 400_000);
        assert!(learned.fast());
        assert!(ChatGptModel::known_for("acct-a").contains(&learned));
        assert!(
            ChatGptModel::known_for("acct-a").contains(&ChatGptModel::GPT_6_LUNA),
            "a built-in model the catalogue doesn't list stays callable"
        );
        assert_eq!(
            ChatGptModel::parse("gpt-6.1-sol"),
            Ok(ChatGptModel::GPT_6_1_SOL)
        );
        // A catalogue with nothing listed keeps what was learned.
        ChatGptModel::learn("acct-a", &[]);
        assert!(ChatGptModel::parse_for("acct-a", "gpt-test-learned").is_ok());
    }

    #[tokio::test]
    async fn fast_mode_asks_the_priority_tier_only_of_a_model_that_offers_it() {
        use crate::llm::provider::with_fast_mode;
        let messages = [LlmMessage::user("hi")];
        let fast_model = ChatGptModel {
            fast: true,
            ..ChatGptModel::GPT_6_1_SOL
        };
        let on = |model| {
            let messages = &messages;
            async move {
                with_fast_mode(true, async {
                    request_body(&Call {
                        model,
                        ..call(messages, &[], "k", None)
                    })
                })
                .await
            }
        };
        assert_eq!(on(fast_model).await["service_tier"], json!("priority"));
        assert!(
            on(ChatGptModel::GPT_6_1_SOL)
                .await
                .get("service_tier")
                .is_none(),
            "no tier the catalogue hasn't offered"
        );
        let off = request_body(&Call {
            model: fast_model,
            ..call(&messages, &[], "k", None)
        });
        assert!(off.get("service_tier").is_none(), "off unless asked");
    }

    #[tokio::test]
    async fn the_catalogue_is_asked_with_the_login_and_a_client_version() {
        use wiremock::matchers::{header, method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/backend-api/codex/models"))
            .and(query_param("client_version", CATALOG_CLIENT_VERSION))
            .and(header("authorization", "Bearer at-123"))
            .and(header("chatgpt-account-id", "acct-9"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                include_str!("testdata/chatgpt/models_live.json"),
                "application/json",
            ))
            .mount(&server)
            .await;
        let base = Url::parse(&format!("{}/backend-api/codex", server.uri())).expect("url");
        let client = ResponsesClient::new(reqwest::Client::new(), base);
        let access = ChatGptAccess {
            access_token: "at-123",
            account_id: "acct-9",
        };
        let models = client.models(&access).await.expect("listed");
        assert_eq!(models.len(), 5);

        let refused = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_raw(r#"{"detail":"expired"}"#, "application/json"),
            )
            .mount(&refused)
            .await;
        let base = Url::parse(&format!("{}/backend-api/codex", refused.uri())).expect("url");
        let err = ResponsesClient::new(reqwest::Client::new(), base)
            .models(&access)
            .await
            .unwrap_err();
        assert!(matches!(err, ResponsesError::Unauthorized(_)), "{err}");
    }

    #[test]
    fn every_listed_model_parses_and_hidden_or_retired_ones_do_not() {
        let listed: Value =
            serde_json::from_str(include_str!("testdata/chatgpt/models.json")).expect("json");
        let models = listed["models"].as_array().expect("models");
        let shown: Vec<&Value> = models
            .iter()
            .filter(|m| m["visibility"] == "list")
            .collect();
        assert_eq!(shown.len(), ChatGptModel::ALL.len());
        for listed in shown {
            let slug = listed["slug"].as_str().expect("slug");
            let model = ChatGptModel::parse(slug).unwrap_or_else(|e| panic!("{slug}: {e}"));
            assert_eq!(model.id(), slug);
            assert_eq!(
                u64::from(model.context_window()),
                listed["context_window"].as_u64().expect("context_window"),
                "{slug}"
            );
        }
        for hidden in ["gpt-reserve", "codex-auto-review"] {
            assert_eq!(
                ChatGptModel::parse(hidden),
                Err(UnsupportedModel::Unknown(hidden.into()))
            );
        }
        assert_eq!(
            ChatGptModel::parse("gpt-6.1-sol"),
            Ok(ChatGptModel::GPT_6_1_SOL)
        );
        assert_eq!(
            ChatGptModel::parse("gpt-5.5").unwrap_err().to_string(),
            "gpt-5.5 retired from ChatGPT sign-ins on 2026-10-14; use gpt-6.1-sol"
        );
        for retired in ["gpt-5.4", "gpt-5.4-mini", "gpt-5.2"] {
            assert_eq!(
                ChatGptModel::parse(retired),
                Err(UnsupportedModel::Retired(retired.into()))
            );
        }
        assert_eq!(
            ChatGptModel::parse("gpt-4o").unwrap_err().to_string(),
            "gpt-4o is not served on a ChatGPT plan here; use gpt-6.1-sol"
        );
        assert_eq!(ChatGptModel::GPT_6_1_SOL.context_window(), 500_000);
        assert_eq!(ChatGptModel::GPT_6_LUNA.context_window(), 272_000);
    }

    // ── Response stream ─────────────────────────────────────────────────

    #[test]
    fn responses_sse_text_fixture_yields_text_and_usage() {
        let (_, _, body) = recorded(include_str!("testdata/chatgpt/text_reasoning.sse"));
        let (blocks, stop, usage) = collected(body);
        assert!(matches!(
            blocks.as_slice(),
            [ContentBlock::Text { text }] if text == "The sky is blue because air molecules \
                scatter shorter-wavelength blue sunlight more than other colors, making blue \
                light come from all directions."
        ));
        assert_eq!(stop, StopReason::EndTurn);
        assert_eq!((usage.input_tokens, usage.output_tokens), (33, 30));

        let cached = body.replace(
            r#""input_tokens":33,"input_tokens_details":{"cache_write_tokens":0,"cached_tokens":0}"#,
            r#""input_tokens":33,"input_tokens_details":{"cache_write_tokens":0,"cached_tokens":20}"#,
        );
        assert_ne!(cached, body);
        let (_, _, usage) = collected(&cached);
        assert_eq!(
            (usage.input_tokens, usage.cache_read_input_tokens),
            (13, 20),
            "input_tokens is the fresh portion; cached tokens are billed apart"
        );
    }

    #[test]
    fn responses_sse_tool_call_fixture_yields_tool_use_and_tool_stop_reason() {
        let (_, _, body) = recorded(include_str!("testdata/chatgpt/tool_round1_parallel.sse"));
        let (blocks, stop, _) = collected(body);
        let calls: Vec<(&str, &str, &Value)> = blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse {
                    id, name, input, ..
                } => Some((id.as_str(), name.as_str(), input)),
                _ => None,
            })
            .collect();
        assert_eq!(
            calls,
            [
                (
                    "call_i0R5QIs1iUydymKO9v7MRxoP",
                    "weather_get",
                    &json!({"city": "Lisbon"})
                ),
                (
                    "call_k94qa7gmcyenx7QAN6oXUVxM",
                    "time_get",
                    &json!({"city": "Lisbon"})
                ),
            ]
        );
        assert_eq!(blocks.len(), 2);
        assert_eq!(stop, StopReason::ToolUse);
    }

    #[test]
    fn responses_reasoning_round_trips_encrypted_content_into_next_request() {
        let (_, _, body) = recorded(include_str!("testdata/chatgpt/reasoning_round1.sse"));
        let (blocks, stop, usage) = collected(body);
        assert_eq!(stop, StopReason::ToolUse);
        assert_eq!(usage.output_tokens, 307);
        let ContentBlock::Thinking { thinking, .. } = &blocks[0] else {
            panic!("reasoning first: {blocks:?}");
        };
        assert_eq!(
            thinking,
            "**Calculating precise arrival time**\n\n**Planning to retrieve weather data**"
        );

        let task = "A train leaves at 09:17 and travels 263 km at 71 km/h, then waits 13 \
                    minutes, then 118 km at 88 km/h. Work out the arrival time carefully, then \
                    call weather_get for the destination city Porto.";
        let tools = tools();
        let messages = [
            LlmMessage::user(task),
            assistant(blocks),
            LlmMessage::tool_results(vec![(
                "call_6JPb4dhlUdE5VixNMYxOcxX3".into(),
                "cloudy 18C".into(),
                false,
            )]),
        ];
        let body = request_body(&call(
            &messages,
            &tools,
            "00000000-0000-4000-8000-000000000004",
            Some(20_000),
        ));

        // Recorded with the function call's server id and status, which
        // tool_round2_without_ids shows the backend doesn't need.
        let mut expected = request_fixture(include_str!(
            "testdata/chatgpt/reasoning_round2_with_encrypted.request.json"
        ));
        for item in expected["input"].as_array_mut().into_iter().flatten() {
            if item["type"] == "function_call" {
                let item = item.as_object_mut().expect("item object");
                item.remove("id");
                item.remove("status");
            }
        }
        assert_eq!(body, expected);
    }

    #[test]
    fn reasoning_item_done_emits_content_stop_before_next_item() {
        let (_, _, body) = recorded(include_str!("testdata/chatgpt/reasoning_round1.sse"));
        let events = events(body);
        let position = |pred: &dyn Fn(&StreamEvent) -> bool| {
            events.iter().position(pred).expect("event present")
        };
        let signature = position(&|e| matches!(e, StreamEvent::SignatureDelta { index: 0, .. }));
        let stop = position(&|e| matches!(e, StreamEvent::ContentStop { index: 0 }));
        let next = position(&|e| matches!(e, StreamEvent::ToolUseStart { index: 1, .. }));
        assert!(signature < stop && stop < next, "{events:?}");
        assert!(matches!(events[0], StreamEvent::ContentStart { index: 0 }));
    }

    #[test]
    fn function_call_args_only_in_done_event_are_kept() {
        let body = [
            json!({"type": "response.output_item.added", "output_index": 0, "item":
                {"type": "function_call", "call_id": "call_1", "name": "weather_get", "arguments": ""}}),
            json!({"type": "response.output_item.done", "output_index": 0, "item":
                {"type": "function_call", "call_id": "call_1", "name": "weather_get",
                 "arguments": "{\"city\":\"Porto\"}"}}),
            json!({"type": "response.completed", "response": {"usage": {"input_tokens": 5, "output_tokens": 3}}}),
        ]
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap_or_default()))
        .collect::<String>();
        let (blocks, stop, _) = collected(&body);
        assert!(matches!(
            blocks.as_slice(),
            [ContentBlock::ToolUse { id, input, .. }] if id == "call_1" && input == &json!({"city": "Porto"})
        ));
        assert_eq!(stop, StopReason::ToolUse);
    }

    #[test]
    fn incomplete_max_output_tokens_is_max_tokens_not_error() {
        let body = format!(
            "data: {}\n\n",
            json!({"type": "response.incomplete", "response": {
                "incomplete_details": {"reason": "max_output_tokens"},
                "usage": {"input_tokens": 9, "output_tokens": 64}}})
        );
        let (_, stop, usage) = collected(&body);
        assert_eq!(stop, StopReason::MaxTokens);
        assert_eq!(usage.output_tokens, 64);
    }

    #[test]
    fn content_filter_is_a_client_error_naming_the_reason() {
        let filtered = format!(
            "data: {}\n\n",
            json!({"type": "response.incomplete", "response": {
                "incomplete_details": {"reason": "content_filter"}}})
        );
        let err = emits(&filtered).expect_err("filtered");
        assert_eq!(
            err,
            ResponsesError::ContentFiltered("content_filter".into())
        );
        assert!(err.to_string().contains("content_filter"), "{err}");
        assert_eq!(classify_error(&err.to_string()), ErrorKind::ClientError);
    }

    #[test]
    fn a_refusal_reaches_the_user_as_text() {
        let stream = |events: &[Value]| {
            events
                .iter()
                .map(|e| format!("data: {e}\n\n"))
                .collect::<String>()
        };
        let added = json!({"type": "response.output_item.added", "output_index": 0,
            "item": {"type": "message", "content": []}});
        let completed = json!({"type": "response.completed", "response": {"usage": {}}});
        let refusal_item = json!({"type": "message", "content": [
            {"type": "refusal", "refusal": "I can't help with that."}]});

        let streamed = stream(&[
            added.clone(),
            json!({"type": "response.refusal.delta", "output_index": 0, "delta": "I can't help with that."}),
            json!({"type": "response.refusal.done", "output_index": 0, "refusal": "I can't help with that."}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": refusal_item}),
            completed.clone(),
        ]);
        let only_in_done = stream(&[
            added,
            json!({"type": "response.output_item.done", "output_index": 0, "item": refusal_item}),
            completed,
        ]);
        for body in [streamed, only_in_done] {
            let (blocks, stop, _) = collected(&body);
            assert!(
                matches!(blocks.as_slice(), [ContentBlock::Text { text }] if text == "I can't help with that."),
                "{blocks:?}"
            );
            assert_eq!(stop, StopReason::EndTurn);
        }
    }

    // ── Plan usage ──────────────────────────────────────────────────────

    #[test]
    fn codex_headers_and_rate_limit_event_produce_limit_snapshot() {
        let (_, headers, _) = recorded(include_str!("testdata/chatgpt/text_reasoning.sse"));
        assert_eq!(
            LimitSnapshot::from_headers(&headers),
            Some(LimitSnapshot {
                plan: Some("prolite".into()),
                active_limit: Some("premium".into()),
                primary: Some(LimitWindow {
                    used_percent: 0.0,
                    window_minutes: Some(10_080),
                    resets_at: DateTime::from_timestamp(1_791_270_464, 0),
                }),
                secondary: None,
                credits: Some(Credits {
                    has_credits: false,
                    unlimited: false,
                    balance: Some("0".into()),
                }),
            })
        );
        assert_eq!(LimitSnapshot::from_headers(&HeaderMap::new()), None);

        let event = json!({
            "type": "codex.rate_limits",
            "plan_type": "plus",
            "rate_limits": {
                "allowed": true,
                "limit_reached": false,
                "primary": {"used_percent": 42, "window_minutes": 60, "reset_at": 1_700_000_000},
                "secondary": null
            },
            "credits": {"has_credits": true, "unlimited": false, "balance": "123"}
        });
        let emitted = emits(&format!("data: {event}\n\n")).expect("parses");
        let [Emit::Limits(snapshot)] = emitted.as_slice() else {
            panic!("one snapshot: {emitted:?}");
        };
        assert_eq!(snapshot.plan.as_deref(), Some("plus"));
        assert_eq!(
            snapshot.primary,
            Some(LimitWindow {
                used_percent: 42.0,
                window_minutes: Some(60),
                resets_at: DateTime::from_timestamp(1_700_000_000, 0),
            })
        );
        assert_eq!(snapshot.secondary, None);
        assert_eq!(
            snapshot.credits.as_ref().and_then(|c| c.balance.as_deref()),
            Some("123")
        );
    }

    // ── Errors ──────────────────────────────────────────────────────────

    fn recorded_error(raw: &str) -> ResponsesError {
        let (status, headers, body) = recorded(raw);
        http_error(status, &headers, body)
    }

    #[test]
    fn recorded_error_bodies_map_to_typed_errors() {
        let unauthorized = recorded_error(include_str!("testdata/chatgpt/unauthorized.sse"));
        assert_eq!(
            unauthorized,
            ResponsesError::Unauthorized(
                "Could not parse your authentication token. Please try signing in again.".into()
            )
        );
        assert_eq!(classify_error(&unauthorized.to_string()), ErrorKind::Auth);

        let not_in_plan = recorded_error(include_str!("testdata/chatgpt/model_not_supported.sse"));
        assert!(matches!(&not_in_plan, ResponsesError::ModelNotInPlan(m) if m.contains("gpt-4o")));
        assert_eq!(
            classify_error(&not_in_plan.to_string()),
            ErrorKind::ClientError
        );

        let tampered = recorded_error(include_str!(
            "testdata/chatgpt/reasoning_round2_tampered.sse"
        ));
        assert!(
            matches!(&tampered, ResponsesError::InvalidReasoning(m) if m.contains("rs_05d7ba13"))
        );
        assert_eq!(
            classify_error(&tampered.to_string()),
            ErrorKind::ClientError
        );

        let max_output = recorded_error(include_str!("testdata/chatgpt/max_output_tokens.sse"));
        assert!(matches!(
            max_output,
            ResponsesError::Http { status: 400, .. }
        ));
        assert_eq!(
            classify_error(&max_output.to_string()),
            ErrorKind::ClientError
        );
    }

    #[test]
    fn older_models_refused_on_a_chatgpt_plan_are_not_in_plan() {
        for (model, raw) in [
            ("gpt-5.4", include_str!("testdata/chatgpt/gpt_5_4_text.sse")),
            ("gpt-5.4", include_str!("testdata/chatgpt/gpt_5_4_tool.sse")),
            (
                "gpt-5.4-mini",
                include_str!("testdata/chatgpt/gpt_5_4_mini_text.sse"),
            ),
            (
                "gpt-5.4-mini",
                include_str!("testdata/chatgpt/gpt_5_4_mini_tool.sse"),
            ),
            ("gpt-5.2", include_str!("testdata/chatgpt/gpt_5_2_text.sse")),
            ("gpt-5.2", include_str!("testdata/chatgpt/gpt_5_2_tool.sse")),
        ] {
            let error = recorded_error(raw);
            assert!(
                matches!(&error, ResponsesError::ModelNotInPlan(m) if m.contains(&format!("'{model}'"))),
                "{error}"
            );
        }
    }

    #[test]
    fn usage_limit_429_maps_to_limit_with_resets_at() {
        let limit = http_error(
            429,
            &HeaderMap::new(),
            r#"{"error":{"type":"usage_limit_reached","plan_type":"pro","resets_at":1791270464}}"#,
        );
        assert_eq!(
            limit,
            ResponsesError::UsageLimit {
                resets_at: DateTime::from_timestamp(1_791_270_464, 0),
                plan: Some("pro".into()),
            }
        );
        assert_eq!(
            http_error(
                429,
                &HeaderMap::new(),
                r#"{"error":{"type":"usage_limit_reached","plan_type":"pro"}}"#,
            ),
            ResponsesError::UsageLimit {
                resets_at: None,
                plan: Some("pro".into()),
            }
        );

        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("30"));
        let throttled = http_error(429, &headers, r#"{"detail":"Too many requests"}"#);
        assert_eq!(
            throttled,
            ResponsesError::Throttled {
                retry_after: Some(Duration::from_secs(30)),
            }
        );
        assert!(throttled.to_string().contains("[retry-after: 30s]"));
        assert_eq!(
            classify_error(&throttled.to_string()),
            ErrorKind::RateLimited
        );
    }

    #[test]
    fn context_length_exceeded_in_response_failed_classifies_context_overflow() {
        let failed = json!({"type": "response.failed", "response": {"error": {
            "code": "context_length_exceeded",
            "message": "Your input exceeds the context window of this model."}}});
        let err = emits(&format!("data: {failed}\n\n")).expect_err("failed response");
        assert!(matches!(err, ResponsesError::ContextOverflow(_)));
        assert_eq!(classify_error(&err.to_string()), ErrorKind::ContextOverflow);

        let throttled = json!({"type": "response.failed", "response": {"error": {
            "code": "rate_limit_exceeded",
            "message": "Rate limit reached for gpt-6.1-sol. Please try again in 11.054s."}}});
        let err = emits(&format!("data: {throttled}\n\n")).expect_err("failed response");
        assert!(err.to_string().contains("[retry-after: 12s]"), "{err}");

        let overloaded = json!({"type": "response.failed", "response": {"error": {
            "code": "server_is_overloaded", "message": "busy"}}});
        let err = emits(&format!("data: {overloaded}\n\n")).expect_err("failed response");
        assert_eq!(classify_error(&err.to_string()), ErrorKind::Overloaded);
    }

    // ── Client ──────────────────────────────────────────────────────────

    fn assert_send<T: Send>(_: &T) {}

    #[tokio::test]
    async fn stream_sends_login_headers_and_no_turn_state() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let (status, headers, body) = recorded(include_str!("testdata/chatgpt/text_reasoning.sse"));
        let server = MockServer::start().await;
        let template = headers.iter().fold(
            ResponseTemplate::new(status).set_body_raw(body, "text/event-stream"),
            |t, (k, v)| t.insert_header(k.as_str(), v.to_str().unwrap_or_default()),
        );
        Mock::given(method("POST"))
            .and(path("/backend-api/codex/responses"))
            .respond_with(template)
            .mount(&server)
            .await;

        let base = Url::parse(&format!("{}/backend-api/codex", server.uri())).expect("url");
        let client = ResponsesClient::new(reqwest::Client::new(), base);
        let access = ChatGptAccess {
            access_token: "at-123",
            account_id: "acct-9",
        };
        let messages = [LlmMessage::user("In one sentence, why is the sky blue?")];
        let snapshots = std::sync::Mutex::new(Vec::new());
        let record = |s: LimitSnapshot| snapshots.lock().expect("lock").push(s);

        for _ in 0..2 {
            let (tx, rx) = mpsc::channel(256);
            let streaming =
                client.stream(&access, call(&messages, &[], "sess-1", None), tx, &record);
            assert_send(&streaming);
            streaming.await.expect("streams");
            let (blocks, _, _) = crate::llm::streaming::collect_stream(rx)
                .await
                .expect("collects");
            assert_eq!(blocks.len(), 1);
        }
        assert_eq!(snapshots.lock().expect("lock").len(), 2);

        let requests = server.received_requests().await.expect("recorded");
        assert_eq!(requests.len(), 2);
        for request in &requests {
            let header = |name: &str| {
                request
                    .headers
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            };
            assert_eq!(header("authorization").as_deref(), Some("Bearer at-123"));
            assert_eq!(header("chatgpt-account-id").as_deref(), Some("acct-9"));
            assert_eq!(header("session-id").as_deref(), Some("sess-1"));
            assert_eq!(header("originator").as_deref(), Some("workforce"));
            assert_eq!(header("accept").as_deref(), Some("text/event-stream"));
            assert_eq!(header("user-agent").as_deref(), Some(USER_AGENT_VALUE));
            assert_eq!(header("x-codex-turn-state"), None);
            let sent: Value = serde_json::from_slice(&request.body).expect("json body");
            assert_eq!(sent["prompt_cache_key"], "sess-1");
            assert_eq!(sent["store"], false);
        }
    }
}
