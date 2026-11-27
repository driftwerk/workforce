//! Data-driven Google tool dispatcher and shared helpers for service modules.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use workforce_agent::tools::{SurfaceEffect, Tool, ToolResult};

use super::api::{GoogleApi, GoogleApis};
use crate::adapters::utils::{unknown_instance, LIVE_INSTANCE_DESCRIPTION};

/// Boxed future type for tool handlers.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub type HandlerFn = Arc<
    dyn for<'a> Fn(serde_json::Value, &'a GoogleApi) -> BoxFuture<'a, Result<serde_json::Value>>
        + Send
        + Sync,
>;

/// What a call of a tool that writes to someone does to its surface.
pub type EffectFn = fn(&serde_json::Value) -> SurfaceEffect;

pub struct GoogleTool {
    name: String,
    description: String,
    schema: serde_json::Value,
    ctx: Arc<GoogleApis>,
    handler: HandlerFn,
    effect: Option<EffectFn>,
}

/// The Google tools that return what someone else wrote (a message, an
/// event's description, a shared document): a turn that reads one is
/// guarded from then on.
pub const OUTSIDE_READS: &[&str] = &[
    "google.gmail.search",
    "google.gmail.get",
    "google.gmail.attachment",
    "google.gmail.thread_get",
    "google.gmail.history",
    "google.chat.messages_list",
    "google.chat.threads_list",
    "google.calendar.events",
    "google.calendar.event_get",
    "google.calendar.search",
    "google.docs.cat",
    "google.drive.download",
    "google.drive.comments_list",
    "google.sheets.get",
    "google.slides.read_slide",
];

/// The Google tools that only read the person's own account: its
/// calendars, files, labels, settings, tasks and contacts.
pub const READS: &[&str] = &[
    "google.calendar.calendars",
    "google.calendar.freebusy",
    "google.calendar.colors",
    "google.drive.ls",
    "google.drive.search",
    "google.drive.get",
    "google.drive.permissions",
    "google.drive.drives",
    "google.sheets.metadata",
    "google.docs.info",
    "google.gmail.labels_list",
    "google.gmail.label_get",
    "google.gmail.draft_get",
    "google.gmail.drafts_list",
    "google.gmail.filters_list",
    "google.gmail.forwarding_list",
    "google.gmail.sendas_list",
    "google.gmail.vacation_get",
    "google.tasks.lists",
    "google.tasks.list",
    "google.tasks.get",
    "google.contacts.list",
    "google.contacts.get",
    "google.contacts.search",
    "google.contacts.other_list",
    "google.contacts.directory_search",
    "google.people.me",
    "google.people.get",
    "google.people.search",
    "google.chat.spaces_list",
    "google.chat.spaces_find",
    "google.chat.reactions_list",
    "google.slides.info",
    "google.slides.list_slides",
];

impl GoogleTool {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        schema: serde_json::Value,
        ctx: Arc<GoogleApis>,
        handler: HandlerFn,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            schema,
            ctx,
            handler,
            effect: None,
        }
    }

    pub fn arc(self) -> Arc<dyn Tool> {
        Arc::new(self)
    }
}

#[async_trait]
impl Tool for GoogleTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> serde_json::Value {
        self.schema.clone()
    }

    fn surface_effect(&self, input: &serde_json::Value) -> Option<SurfaceEffect> {
        self.effect.map(|effect| effect(input))
    }

    fn reads_outside_content(&self) -> bool {
        OUTSIDE_READS.contains(&self.name.as_str())
    }

    fn reach(&self, _input: &serde_json::Value) -> Option<workforce_agent::tools::Reach> {
        [OUTSIDE_READS, READS]
            .iter()
            .any(|names| names.contains(&self.name.as_str()))
            .then_some(workforce_agent::tools::Reach::Reads)
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let result = match resolve(&self.ctx, &self.name, &input) {
            Ok(api) => (self.handler)(input, api.as_ref()).await,
            Err(e) => Err(e),
        };
        let duration = start.elapsed().as_millis() as u64;
        match result {
            Ok(out) => Ok(ToolResult::success(out, duration)),
            Err(e) => Ok(ToolResult::from_report(&e, duration)),
        }
    }
}

/// The connection a call of `tool` names (`workspace`, or `account` as a
/// saved prompt still says it), else the caller's default.
pub fn resolve(ctx: &GoogleApis, tool: &str, input: &serde_json::Value) -> Result<Arc<GoogleApi>> {
    let requested = input
        .get("workspace")
        .or_else(|| account_alias(tool, input))
        .and_then(serde_json::Value::as_str);
    let (clients, default) = ctx.for_caller();
    let name = workforce_agent::llm::environment::resolve_instance(
        "google", requested, &default, &clients,
    )
    .map_err(|d| eyre::eyre!("{}", d.message))?;
    clients
        .get(&name)
        .cloned()
        .ok_or_else(|| unknown_instance("Google account", &name, &clients))
}

fn account_alias<'a>(tool: &str, input: &'a serde_json::Value) -> Option<&'a serde_json::Value> {
    let account = input.get("account")?;
    tracing::debug!(tool, "google: `account` read as `workspace`");
    Some(account)
}

// ─── Schema helpers ──────────────────────────────────────────────────────────

pub fn workspace_property() -> serde_json::Value {
    serde_json::json!({
        "type": "string",
        "description": LIVE_INSTANCE_DESCRIPTION,
    })
}

pub fn object_schema(
    mut properties: serde_json::Map<String, serde_json::Value>,
    required: &[&str],
) -> serde_json::Value {
    properties.insert("workspace".to_string(), workspace_property());
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
    })
}

/// Build a properties map from `(&str, Value)` pairs — shared across all
/// service modules to avoid per-file `props()` duplication.
pub fn props(pairs: &[(&str, serde_json::Value)]) -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    for (k, v) in pairs {
        m.insert((*k).to_string(), v.clone());
    }
    m
}

/// Shorthand: build a `GoogleTool` and wrap it in `Arc<dyn Tool>`.
pub fn tool(
    name: &str,
    desc: &str,
    schema: serde_json::Value,
    ctx: &Arc<GoogleApis>,
    h: HandlerFn,
) -> Arc<dyn Tool> {
    GoogleTool::new(name, desc, schema, ctx.clone(), h).arc()
}

/// Shorthand for a tool that sends to someone: the cross-surface gate holds
/// a call of it in a Desk turn for the person's say.
pub fn sending_tool(
    name: &str,
    desc: &str,
    schema: serde_json::Value,
    ctx: &Arc<GoogleApis>,
    h: HandlerFn,
    effect: EffectFn,
) -> Arc<dyn Tool> {
    GoogleTool {
        effect: Some(effect),
        ..GoogleTool::new(name, desc, schema, ctx.clone(), h)
    }
    .arc()
}

// ─── Input helpers ───────────────────────────────────────────────────────────

pub fn required_str<'a>(input: &'a serde_json::Value, field: &str) -> Result<&'a str> {
    input
        .get(field)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| eyre::eyre!("missing required field `{field}`"))
}

pub fn optional_str<'a>(input: &'a serde_json::Value, field: &str) -> Option<&'a str> {
    input.get(field).and_then(|v| v.as_str())
}

pub fn optional_u64(input: &serde_json::Value, field: &str) -> Option<u64> {
    match input.get(field)? {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Accept either a JSON array or a JSON-encoded array string. Some LLMs
/// (notably smaller / non-frontier models) emit nested array tool args as
/// stringified JSON; this lets the adapter recover instead of forwarding
/// the malformed shape to Google's API.
fn coerce_array(v: &serde_json::Value) -> Option<serde_json::Value> {
    match v {
        serde_json::Value::Array(_) => Some(v.clone()),
        serde_json::Value::String(s) => match serde_json::from_str::<serde_json::Value>(s) {
            Ok(parsed @ serde_json::Value::Array(_)) => Some(parsed),
            _ => None,
        },
        _ => None,
    }
}

pub fn required_array(input: &serde_json::Value, field: &str) -> Result<serde_json::Value> {
    let raw = input
        .get(field)
        .ok_or_else(|| eyre::eyre!("missing required field `{field}`"))?;
    coerce_array(raw).ok_or_else(|| {
        eyre::eyre!("field `{field}` must be a JSON array (or a JSON-encoded array string)")
    })
}

pub fn optional_array(input: &serde_json::Value, field: &str) -> Option<serde_json::Value> {
    input.get(field).and_then(coerce_array)
}

pub fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

pub fn handler<F>(f: F) -> HandlerFn
where
    F: for<'a> Fn(serde_json::Value, &'a GoogleApi) -> BoxFuture<'a, Result<serde_json::Value>>
        + Send
        + Sync
        + 'static,
{
    Arc::new(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::adapters::google::oauth::OAuthClientCreds;
    use crate::adapters::google::token_store::TokenStore;
    use crate::adapters::test_support::{Logs, People};

    const SAM: &str = "sam";

    /// Sam's own "home", and a shared "work" that is the default.
    fn connections() -> Arc<GoogleApis> {
        let people = People::new(&[
            ("skills/google/workspaces/work/refresh_token", "rt-work"),
            ("skills/google/default_workspace", "work"),
        ]);
        people.own(
            SAM,
            "skills/google/workspaces/home/refresh_token",
            "rt-home",
        );
        let store: Arc<dyn workforce_common::settings::SecretStore> = people;
        let http = reqwest::Client::new();
        Arc::new(GoogleApis::live(
            Arc::clone(&store),
            crate::adapters::google::instance_paths(),
            Some("work".into()),
            move |name| {
                let client = OAuthClientCreds {
                    client_id: "cid".into(),
                    client_secret: "secret".into(),
                    redirect_uri: None,
                };
                let tokens = TokenStore::new(Arc::clone(&store), name.into(), client, http.clone());
                Ok(GoogleApi::new(http.clone(), tokens))
            },
        ))
    }

    /// A tool that answers with the connection it was given.
    fn which(ctx: &Arc<GoogleApis>, name: &str) -> Arc<dyn Tool> {
        tool(
            name,
            "",
            object_schema(serde_json::Map::new(), &[]),
            ctx,
            handler(|_, api| Box::pin(async move { Ok(json!(api.workspace())) })),
        )
    }

    async fn call(tool: &Arc<dyn Tool>, input: serde_json::Value) -> Result<String, String> {
        let result = tool.execute(input).await.unwrap();
        match result.success {
            true => Ok(result.output.as_str().unwrap_or_default().to_string()),
            false => Err(result.error.unwrap_or_default()),
        }
    }

    async fn as_sam<F: Future>(f: F) -> F::Output {
        workforce_agent::runner::CURRENT_USER_ID
            .scope(SAM.to_string(), f)
            .await
    }

    #[tokio::test]
    async fn workspace_resolves_per_caller_and_refuses_an_unknown_one() {
        let ctx = connections();
        let t = which(&ctx, "google.drive.get");
        assert_eq!(as_sam(call(&t, json!({}))).await.unwrap(), "work");
        assert_eq!(
            as_sam(call(&t, json!({"workspace": "home"})))
                .await
                .unwrap(),
            "home"
        );
        // Sam's own isn't anyone else's.
        let refused = call(&t, json!({"workspace": "home"})).await.unwrap_err();
        assert!(refused.contains("home"), "{refused}");
        assert!(refused.contains("work"), "{refused}");
        let unknown = as_sam(call(&t, json!({"workspace": "other"})))
            .await
            .unwrap_err();
        assert!(unknown.contains("Google account"), "{unknown}");
        assert!(
            unknown.contains("home") && unknown.contains("work"),
            "{unknown}"
        );
        let schema = t.input_schema();
        assert_eq!(
            schema["properties"]["workspace"]["description"],
            LIVE_INSTANCE_DESCRIPTION
        );
        assert!(schema["properties"].get("account").is_none());
    }

    #[tokio::test]
    async fn account_is_read_as_workspace() {
        let ctx = connections();
        let t = which(&ctx, "google.calendar.events");
        let (logs, _guard) = Logs::capture();
        assert_eq!(
            as_sam(call(&t, json!({"account": "home"}))).await.unwrap(),
            "home"
        );
        let said = logs.text();
        assert!(said.contains("DEBUG"), "{said}");
        assert!(said.contains("read as `workspace`"), "{said}");
        assert!(said.contains("google.calendar.events"), "{said}");
        // `workspace` wins, and is read without a word.
        let (logs, _guard) = Logs::capture();
        assert_eq!(
            as_sam(call(&t, json!({"workspace": "work", "account": "home"})))
                .await
                .unwrap(),
            "work"
        );
        assert!(!logs.text().contains("read as `workspace`"));
    }

    #[tokio::test]
    async fn a_write_to_a_read_only_google_connection_is_refused() {
        use workforce_common::credential_pin::{PinSlot, PinSource, TierPin, Writing};
        struct Work;
        impl PinSource for Work {
            fn pin(&self, _: &str) -> TierPin {
                TierPin::All
            }
            fn read_only_instance(&self, connector: &str, instance: &str) -> bool {
                (connector, instance) == ("google", "work")
            }
        }
        let pins = Arc::new(PinSlot::default());
        pins.fill(Arc::new(Work));
        let ctx = connections();
        let t = which(&ctx, "google.drive.upload");
        let writing = Writing {
            connector: "google".into(),
            pins,
        };
        let refused = writing
            .clone()
            .scope(as_sam(call(&t, json!({}))))
            .await
            .unwrap_err();
        assert!(refused.contains("only reads"), "{refused}");
        assert_eq!(
            writing
                .scope(as_sam(call(&t, json!({"workspace": "home"}))))
                .await
                .unwrap(),
            "home"
        );
    }

    #[tokio::test]
    async fn google_workspaces_answers_names_and_default() {
        let ctx = connections();
        let t = crate::adapters::google::services::build_all(Arc::clone(&ctx))
            .into_iter()
            .find(|t| t.name() == "google.workspaces")
            .unwrap();
        assert_eq!(
            t.reach(&json!({})),
            Some(workforce_agent::tools::Reach::Reads)
        );
        let answer = as_sam(t.execute(json!({}))).await.unwrap().output;
        assert_eq!(
            answer,
            json!({"workspaces": ["home", "work"], "default": "work"})
        );
        let shared = t.execute(json!({})).await.unwrap().output;
        assert_eq!(shared, json!({"workspaces": ["work"], "default": "work"}));
    }

    #[test]
    fn required_array_accepts_json_array() {
        let input = json!({ "values": [["a", "b"], ["c", "d"]] });
        let got = required_array(&input, "values").unwrap();
        assert_eq!(got, json!([["a", "b"], ["c", "d"]]));
    }

    #[test]
    fn required_array_accepts_stringified_array() {
        let input = json!({ "values": "[[\"a\",\"b\"]]" });
        let got = required_array(&input, "values").unwrap();
        assert_eq!(got, json!([["a", "b"]]));
    }

    #[test]
    fn required_array_rejects_string_that_parses_to_non_array() {
        let input = json!({ "values": "{\"not\":\"array\"}" });
        assert!(required_array(&input, "values").is_err());
    }

    #[test]
    fn required_array_rejects_bare_string() {
        let input = json!({ "values": "hello" });
        assert!(required_array(&input, "values").is_err());
    }

    #[test]
    fn required_array_missing_field_errors() {
        let input = json!({});
        let err = required_array(&input, "values").unwrap_err();
        assert!(err.to_string().contains("missing required field"));
    }

    #[test]
    fn optional_array_returns_none_for_absent_field() {
        let input = json!({});
        assert!(optional_array(&input, "x").is_none());
    }

    #[test]
    fn optional_array_returns_none_for_invalid_shape() {
        let input = json!({ "x": 42 });
        assert!(optional_array(&input, "x").is_none());
    }
}
