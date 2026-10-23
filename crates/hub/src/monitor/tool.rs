//! Agent-callable tool for authoring monitors: `list`, `get`, `schema`,
//! `validate`, `save`, `set_enabled`, `delete`. A monitor durably watches a
//! channel, or polls anything a tool reads, and dispatches an action per
//! matching message or observation. Saves are owned by the turn's user
//! (private) so the monitor runs as them; an unbound turn saves a
//! system/global monitor.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;

use workforce_agent::tools::{Tool, ToolResult};
use workforce_orchestrator::monitor::{MonitorDefinition, MonitorKey};
use workforce_orchestrator::workflow_store::{EditContext, KIND_MONITOR};

use crate::authz::{always_admin, IsAdminFn};
use crate::def_write_auth::{
    write_visibility, DefOp, DefWriteDenied, DefWriteGrant, TurnDefWriteAuth,
};
use crate::monitor::binding_access::{current_turn_user, BindingAccess, BindingCaller};
use crate::monitor::engine::{invite_notice, ChannelProbe, MonitorEngine, PollReport, Probe};
use crate::monitor::runtime::{EngineAgent, MonitorRuntime, MonitorSummary};
use crate::resource_access::DefListing;

pub struct MonitorTool {
    runtime: Arc<MonitorRuntime>,
    /// Present when durable watching is enabled (Postgres); carries the
    /// catch-up policy a save or resume applies before the monitor watches.
    engine: Option<Arc<MonitorEngine>>,
    /// The admin gate and the definition-write rule it feeds. A SystemAdmin
    /// may bind a monitor to any channel and mutate global/system monitors; a
    /// non-admin may only bind to channels they are a member of and only
    /// mutate monitors in their own namespace. Defaults to `always_admin` for
    /// single-tenant CLI use; `serve.rs` threads the real identity-backed
    /// policy in cloud mode.
    write_auth: TurnDefWriteAuth,
    /// The membership gate every monitor write passes through. The default
    /// can answer for nobody, so a non-admin human's bind fails closed.
    access: BindingAccess,
}

impl MonitorTool {
    pub fn new(runtime: Arc<MonitorRuntime>, engine: Option<Arc<MonitorEngine>>) -> Self {
        Self {
            runtime,
            engine,
            write_auth: TurnDefWriteAuth::default().with_admin_policy(always_admin()),
            access: BindingAccess::default(),
        }
    }

    pub fn with_admin_policy(mut self, is_admin: IsAdminFn) -> Self {
        self.write_auth = self.write_auth.with_admin_policy(is_admin);
        self
    }

    pub fn with_write_auth(mut self, write_auth: TurnDefWriteAuth) -> Self {
        self.write_auth = write_auth;
        self
    }

    /// The shared definition-write rule, for the turn's user, on the row
    /// `owner` names.
    async fn authorize_write(
        &self,
        id: &str,
        owner: Option<&str>,
        op: DefOp,
    ) -> Result<DefWriteGrant, DefWriteDenied> {
        self.write_auth
            .authorize(
                self.runtime.def_store().cloned(),
                KIND_MONITOR,
                id,
                owner,
                op,
            )
            .await
    }

    pub fn with_binding_access(mut self, access: BindingAccess) -> Self {
        self.access = access;
        self
    }

    /// Resolve the owner namespace a monitor id lives in for this user — the
    /// one `requested` names, else their own, which shadows a global.
    /// `Some(owner)` is the namespace; `None` = not found.
    ///
    /// `allow_global` gates the fallback to the `None`/global namespace. Reads
    /// (`get`) keep the fallback — globals are visible to everyone — but
    /// mutations (`set_enabled`/`delete`) pass `allow_global = is_admin` so a
    /// non-admin with no own copy of `id` can never resolve onto (and thus
    /// disable/delete) a global/system monitor. A named owner resolves in that
    /// namespace alone; the rules below decide whether the caller may have it.
    async fn resolve_owner(
        &self,
        id: &str,
        requested: Option<&str>,
        user: Option<&str>,
        allow_global: bool,
    ) -> Option<Option<String>> {
        if let Some(owner) = requested {
            return self
                .runtime
                .definition_ownership(id, Some(owner))
                .await
                .map(|(owner, _visibility)| owner);
        }
        if let Some(u) = user {
            if self
                .runtime
                .definition_ownership(id, Some(u))
                .await
                .is_some()
            {
                return Some(Some(u.to_string()));
            }
        }
        if !allow_global {
            return None;
        }
        self.runtime
            .definition_ownership(id, None)
            .await
            .map(|_| None)
    }

    /// The row a mutation (`set_enabled`/`delete`) addresses, authorized under
    /// the shared rule. A non-admin resolves to their OWN namespace only — no
    /// global fallback — so they can never disable or delete a global/system
    /// monitor they don't own; admins may mutate globals.
    async fn resolve_write(
        &self,
        id: &str,
        requested: Option<&str>,
        op: DefOp,
        start: Instant,
    ) -> Result<DefWriteGrant, ToolResult> {
        let user = current_turn_user();
        let allow_global = self.write_auth.is_admin();
        let Some(owner) = self
            .resolve_owner(id, requested, user.as_deref(), allow_global)
            .await
        else {
            return Err(ToolResult::failure(
                format!("Unknown monitor '{id}' you can manage."),
                start.elapsed().as_millis() as u64,
            ));
        };
        self.authorize_write(id, owner.as_deref(), op)
            .await
            .map_err(|denied| {
                ToolResult::failure(
                    format!("Cannot change monitor '{id}': {denied}"),
                    start.elapsed().as_millis() as u64,
                )
            })
    }

    /// The row a read addresses: the one `requested` names, when the turn's
    /// user may read it; else their own copy of `id`, else the global one. A
    /// row the user may not read is, to them, not there.
    async fn readable_owner(&self, id: &str, requested: Option<&str>) -> Option<Option<String>> {
        let Some(named) = requested else {
            // Reads may see globals (they're visible to everyone) → allow_global.
            return self
                .resolve_owner(id, None, current_turn_user().as_deref(), true)
                .await;
        };
        let (owner, visibility) = self.runtime.definition_ownership(id, Some(named)).await?;
        self.write_auth
            .may_read(
                self.runtime.def_store().cloned(),
                KIND_MONITOR,
                id,
                owner.as_deref(),
                visibility,
            )
            .await
            .then_some(owner)
    }
}

#[derive(Deserialize)]
struct MonitorInput {
    action: String,
    #[serde(default)]
    monitor_id: Option<String>,
    #[serde(default)]
    toml_content: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    owner: Option<String>,
}

impl MonitorInput {
    /// The owner namespace the call names, if any — an empty string names none.
    fn owner(&self) -> Option<&str> {
        self.owner.as_deref().filter(|owner| !owner.is_empty())
    }
}

#[async_trait]
impl Tool for MonitorTool {
    fn name(&self) -> &str {
        "monitor"
    }

    fn description(&self) -> &str {
        "Author durable monitors: list, get, schema, validate, save, set_enabled, delete. A monitor \
         watches a chat channel, or polls anything a tool reads (a Gmail search, a Jira query, a \
         GitHub run, a web page through http.request, a Metabase card, a WASM skill), and runs an \
         action — a workflow, a skill tool, a notification, or an agent turn — for each matching \
         message or each new/changed observation. Code decides what fires; a model is asked only \
         by an optional [monitor.decide] yes/no or an agent_task. Survives restarts and catches up. \
         A channel monitor's bot must be a member of the channel; save and set_enabled report the \
         channel read or the first poll. Call action='schema' first for the TOML format."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "get", "schema", "validate", "save", "set_enabled", "delete"],
                    "description": "Action to perform."
                },
                "monitor_id": {
                    "type": "string",
                    "description": "Monitor id (required for get, save, set_enabled, delete)."
                },
                "toml_content": {
                    "type": "string",
                    "description": "Raw TOML monitor definition (for validate and save)."
                },
                "enabled": {
                    "type": "boolean",
                    "description": "Target enabled state (for set_enabled)."
                },
                "owner": {
                    "type": "string",
                    "description": "Identity id of the monitor's owner, to read, change or delete another member's monitor that was shared with you (for 'get', 'save', 'set_enabled' and 'delete'). Omit for your own — or, as an administrator, the shared system monitor."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: MonitorInput = serde_json::from_value(input)
            .map_err(|e| eyre::eyre!("Invalid monitor input: {}", e))?;
        match input.action.as_str() {
            "list" => Ok(self.handle_list(start).await),
            "get" => self.handle_get(input, start).await,
            "schema" => Ok(handle_schema(start)),
            "validate" => Ok(handle_validate(input, start)),
            "save" => self.handle_save(input, start).await,
            "set_enabled" => self.handle_set_enabled(input, start).await,
            "delete" => self.handle_delete(input, start).await,
            other => Ok(ToolResult::failure(
                format!(
                    "Unknown action '{other}'. Use list, get, schema, validate, save, set_enabled, or delete."
                ),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

fn missing(field: &str, what: &str, start: Instant) -> ToolResult {
    ToolResult::failure(
        format!("Missing '{field}' — {what}"),
        start.elapsed().as_millis() as u64,
    )
}

fn merge(into: &mut serde_json::Value, extra: serde_json::Value) {
    if let (Some(into), Some(extra)) = (into.as_object_mut(), extra.as_object()) {
        into.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
}

/// What a save or resume tells its author about the first poll of a source.
fn poll_note(def: &MonitorDefinition, report: &PollReport) -> String {
    let Some(source) = def.monitor.source() else {
        return String::new();
    };
    let every = source.every_secs();
    match &report.error {
        Some(error) => format!(
            "Polling {} failed ({error}); it is tried again in {every}s, and the monitor is \
             disabled after repeated failures.",
            source.tool
        ),
        None => format!(
            "Polled {}: {} observed, {} dispatched; next poll in {every}s.",
            source.tool, report.seen, report.fired
        ),
    }
}

/// What a save or resume tells its author about the read it just did: the
/// concrete probe result when the saving agent watches the monitor, else the
/// standing rule for the agent that does.
fn watch_note(
    def: &MonitorDefinition,
    summary: &MonitorSummary,
    bot: Option<&EngineAgent>,
    probe: Option<&Probe>,
) -> String {
    let binding = def.monitor.binding();
    let channel = binding.map_or("", |b| b.channel.as_str());
    match (probe, bot, binding) {
        (Some(Probe::Source(report)), _, _) => poll_note(def, report),
        (Some(Probe::Channel(ChannelProbe::Readable)), Some(bot), _) => {
            format!("@{} can read channel {channel} — watching.", bot.name)
        }
        (Some(Probe::Channel(ChannelProbe::NotAMember(error))), Some(bot), Some(binding)) => {
            invite_notice(def, bot, &binding.surface, channel, error)
        }
        (Some(Probe::Channel(ChannelProbe::Failed(error))), _, _) => format!(
            "Reading channel {channel} failed ({error}); the sweep retries every two minutes."
        ),
        _ if !def.monitor.enabled => "Paused — resume it with set_enabled.".to_string(),
        (_, _, None) => format!(
            "Polled by agent '{}' every {}s as the monitor's owner, with their credentials.",
            summary.agent.as_deref().unwrap_or("default"),
            def.monitor.source().map_or(0, |s| s.every_secs())
        ),
        _ => format!(
            "Watched by agent '{}': its bot must be a member of channel {channel}. If it isn't \
             yet, invite it there — the owner is DMed when the channel can't be read, and the \
             status shows awaiting_invite until it can.",
            summary.agent.as_deref().unwrap_or("default")
        ),
    }
}

impl MonitorTool {
    /// Every monitor the turn's user may reach: their own, the global ones,
    /// any shared with them, and — for an administrator — all of them.
    async fn list_visible(&self, scope: &DefListing) -> Vec<MonitorSummary> {
        match &self.engine {
            Some(engine) => engine.list_visible(scope).await,
            None => self.runtime.list_definitions_visible(scope).await,
        }
    }

    async fn summary(&self, id: &str, owner: Option<&str>) -> Option<MonitorSummary> {
        match &self.engine {
            Some(engine) => engine.summary_of(id, owner).await,
            None => self.runtime.summary_of(id, owner).await,
        }
    }

    /// The listing fields plus the read note for a just-saved or just-resumed
    /// monitor. Probes first so the note and status agree.
    async fn watch_report(&self, id: &str, owner: Option<&str>) -> serde_json::Value {
        let def = self
            .runtime
            .get_definition_source(id, owner)
            .ok()
            .and_then(|toml| MonitorDefinition::from_toml(&toml).ok());
        let probe = match (&self.engine, &def) {
            (Some(engine), Some(def)) => engine.probe(def, owner).await,
            _ => None,
        };
        match (def, self.summary(id, owner).await) {
            (Some(def), Some(summary)) => serde_json::json!({
                "agent": summary.agent,
                "status": summary.status,
                "watch": watch_note(
                    &def,
                    &summary,
                    self.engine.as_deref().map(MonitorEngine::agent),
                    probe.as_ref(),
                ),
            }),
            _ => serde_json::json!({}),
        }
    }

    async fn handle_list(&self, start: Instant) -> ToolResult {
        let scope =
            DefListing::for_turn(&self.write_auth, self.runtime.def_store(), KIND_MONITOR).await;
        let summaries = self.list_visible(&scope).await;
        ToolResult::success(
            serde_json::json!({ "monitors": summaries, "count": summaries.len() }),
            start.elapsed().as_millis() as u64,
        )
    }

    async fn handle_get(&self, input: MonitorInput, start: Instant) -> Result<ToolResult> {
        let requested = input.owner().map(str::to_string);
        let Some(id) = input.monitor_id.filter(|s| !s.is_empty()) else {
            return Ok(missing("monitor_id", "which monitor to retrieve?", start));
        };
        let Some(owner) = self.readable_owner(&id, requested.as_deref()).await else {
            return Ok(ToolResult::failure(
                format!("Unknown monitor '{id}'."),
                start.elapsed().as_millis() as u64,
            ));
        };
        match self.runtime.get_definition_source(&id, owner.as_deref()) {
            Ok(toml) => {
                let summary = self.summary(&id, owner.as_deref()).await;
                Ok(ToolResult::success(
                    serde_json::json!({
                        "monitor_id": id,
                        "toml": toml,
                        "agent": summary.as_ref().and_then(|s| s.agent.clone()),
                        "status": summary.as_ref().map(|s| s.status),
                        "block": summary.and_then(|s| s.block),
                    }),
                    start.elapsed().as_millis() as u64,
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("{e:#}"),
                start.elapsed().as_millis() as u64,
            )),
        }
    }

    async fn handle_save(&self, input: MonitorInput, start: Instant) -> Result<ToolResult> {
        let requested = input.owner().map(str::to_string);
        let Some(id) = input.monitor_id.filter(|s| !s.is_empty()) else {
            return Ok(missing(
                "monitor_id",
                "what id should this monitor have?",
                start,
            ));
        };
        let Some(toml_content) = input.toml_content.filter(|s| !s.is_empty()) else {
            return Ok(missing("toml_content", "the monitor TOML to save.", start));
        };
        // The rule places the save: the named owner's row when the caller may
        // write it, else an administrator on the system row, a member on their
        // own, an unbound turn nowhere.
        let grant = match self
            .authorize_write(&id, requested.as_deref(), DefOp::Save)
            .await
        {
            Ok(grant) => grant,
            Err(denied) => {
                return Ok(ToolResult::failure(
                    format!("Cannot save monitor '{id}': {denied}"),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };
        let parsed = MonitorDefinition::from_toml(&toml_content).ok();
        let previous = self
            .runtime
            .stored_definition(&id, grant.owner.as_deref())
            .await;
        if let Some(def) = &parsed {
            let checked = match &self.engine {
                Some(engine) => engine.preflight(def).await,
                None => self
                    .runtime
                    .check_definition(def)
                    .await
                    .map_err(|e| e.to_string()),
            };
            if let Err(refused) = checked {
                return Ok(ToolResult::failure(
                    refused,
                    start.elapsed().as_millis() as u64,
                ));
            }
            // Membership gate: a non-admin may only point a monitor at a channel
            // they can reach themselves — otherwise a member could bind a
            // notify/agent_task monitor onto a channel they're not in and have
            // its contents delivered to them. Only what this save changes is
            // checked; administrators and the hub's own turns bypass it.
            if let Err(denied) = self
                .access
                .authorize(
                    &BindingCaller::for_turn(self.write_auth.is_admin()),
                    def,
                    previous.as_ref(),
                )
                .await
            {
                return Ok(ToolResult::failure(
                    denied.to_string(),
                    start.elapsed().as_millis() as u64,
                ));
            }
        }
        let stored = self
            .runtime
            .definition_ownership(&id, grant.owner.as_deref())
            .await
            .map(|(_, visibility)| visibility);
        let visibility = match write_visibility(&grant, None, stored) {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        match self
            .runtime
            .save_definition_source(
                &id,
                &toml_content,
                grant.owner.as_deref(),
                visibility,
                EditContext::new("agent_tool", grant.actor.as_deref()),
            )
            .await
        {
            Ok(()) => {
                if let Some(engine) = &self.engine {
                    // Point the catch-up BEFORE the watch report probes, which
                    // sweeps: without this a monitor authored here is dispatched
                    // over the channel's backlog inside its own save.
                    if let Some(def) = &parsed {
                        engine
                            .apply_catch_up_policy(
                                &MonitorKey::new(&id, grant.owner.as_deref()),
                                def,
                                previous.as_ref(),
                            )
                            .await;
                    }
                    engine.reconcile_personas().await;
                }
                let mut output = serde_json::json!({ "saved": true, "monitor_id": id });
                merge(
                    &mut output,
                    self.watch_report(&id, grant.owner.as_deref()).await,
                );
                Ok(ToolResult::success(
                    output,
                    start.elapsed().as_millis() as u64,
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to save monitor: {e:#}"),
                start.elapsed().as_millis() as u64,
            )),
        }
    }

    async fn handle_set_enabled(&self, input: MonitorInput, start: Instant) -> Result<ToolResult> {
        let requested = input.owner().map(str::to_string);
        let Some(id) = input.monitor_id.filter(|s| !s.is_empty()) else {
            return Ok(missing(
                "monitor_id",
                "which monitor to enable/disable?",
                start,
            ));
        };
        let Some(enabled) = input.enabled else {
            return Ok(missing("enabled", "true or false?", start));
        };
        let grant = match self
            .resolve_write(&id, requested.as_deref(), DefOp::Save, start)
            .await
        {
            Ok(grant) => grant,
            Err(refused) => return Ok(refused),
        };
        let owner = grant.owner;
        let result = self
            .runtime
            .set_definition_enabled(
                &id,
                owner.as_deref(),
                enabled,
                EditContext::new("agent_tool", grant.actor.as_deref()),
            )
            .await;
        match result {
            Ok((was_enabled, def)) => {
                // A disabled→enabled resume starts this monitor from now — the
                // paused gap is the owner's decision, and no other monitor on
                // the channel is affected.
                if let Some(engine) = &self.engine {
                    let mut previous = def.clone();
                    previous.monitor.enabled = was_enabled;
                    engine
                        .apply_catch_up_policy(
                            &MonitorKey::new(&id, owner.as_deref()),
                            &def,
                            Some(&previous),
                        )
                        .await;
                    engine.reconcile_personas().await;
                }
                let mut output = serde_json::json!({ "monitor_id": id, "enabled": enabled });
                merge(&mut output, self.watch_report(&id, owner.as_deref()).await);
                Ok(ToolResult::success(
                    output,
                    start.elapsed().as_millis() as u64,
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to update monitor: {e:#}"),
                start.elapsed().as_millis() as u64,
            )),
        }
    }

    async fn handle_delete(&self, input: MonitorInput, start: Instant) -> Result<ToolResult> {
        let requested = input.owner().map(str::to_string);
        let Some(id) = input.monitor_id.filter(|s| !s.is_empty()) else {
            return Ok(missing("monitor_id", "which monitor to delete?", start));
        };
        let grant = match self
            .resolve_write(&id, requested.as_deref(), DefOp::Delete, start)
            .await
        {
            Ok(grant) => grant,
            Err(refused) => return Ok(refused),
        };
        let owner = grant.owner;
        let edit = EditContext::new("agent_tool", grant.actor.as_deref());
        let deleted = match &self.engine {
            Some(engine) => engine.delete_monitor(&id, owner.as_deref(), edit).await,
            None => {
                self.runtime
                    .delete_definition(&id, owner.as_deref(), edit)
                    .await
            }
        };
        match deleted {
            Ok(()) => Ok(ToolResult::success(
                serde_json::json!({ "deleted": true, "monitor_id": id }),
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to delete monitor: {e:#}"),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

fn handle_schema(start: Instant) -> ToolResult {
    ToolResult::success(
        serde_json::json!({ "schema_reference": MONITOR_SCHEMA_REFERENCE }),
        start.elapsed().as_millis() as u64,
    )
}

fn handle_validate(input: MonitorInput, start: Instant) -> ToolResult {
    let Some(toml_content) = input.toml_content.filter(|s| !s.is_empty()) else {
        return ToolResult::success(
            serde_json::json!({ "valid": false, "error": "Missing 'toml_content' to validate." }),
            start.elapsed().as_millis() as u64,
        );
    };
    match MonitorDefinition::from_toml(&toml_content) {
        Ok(def) => ToolResult::success(
            serde_json::json!({ "valid": true, "monitor_id": def.monitor.id, "name": def.monitor.name }),
            start.elapsed().as_millis() as u64,
        ),
        Err(e) => ToolResult::success(
            serde_json::json!({ "valid": false, "error": format!("{e:#}") }),
            start.elapsed().as_millis() as u64,
        ),
    }
}

const MONITOR_SCHEMA_REFERENCE: &str = r#"# Monitor TOML format

A monitor durably watches ONE thing — a chat channel ([monitor.binding]) or a
source, anything a tool reads ([monitor.source]) — and runs an action for each
matching message or each new/changed observation. It survives restarts and
catches up on what it missed. Everything it does is code, except an optional
[monitor.decide] yes/no and the agent_task action.

A channel monitor's agent must have its bot in the channel (`/invite @<agent>`);
`save` and `set_enabled` report the channel read. A source monitor polls as its
OWNER, with the owner's credentials, through the same gates an agent call faces;
`save` reports the first poll.

```toml
[monitor]
id = "invoice-watch"               # unique, kebab-case
name = "Invoices from Acme"
description = "File each new invoice email."
enabled = true
# agent = "lynx"                   # optional: whose engine runs it (its bot / its tools) — defaults
#                                  # to the action's agent, else the hub's default agent
# once = true                      # optional: stop after the first successful fire
# expires_at = "2026-10-20T09:00:00Z"   # optional: stop then, telling the owner

# ── EITHER a channel ───────────────────────────────────────────────────────
[monitor.binding]
surface = "slack"                  # "slack" | "teams" | "any"
channel = "C0RELEASES"             # channel id
# exclude_thread_replies = true    # optional: top-level messages only (persona: no thread replies)

[monitor.filter]                   # channel only; default: every message
type = "contains_any"              # all | contains_any | regex | from_user | from_bot | from_human | has_file
values = ["released", "shipped"]   # contains_any
# pattern = "v\\d+"                # regex
# user = "U123"                     # from_user

# ── OR a source ────────────────────────────────────────────────────────────
[monitor.source]
tool = "google.gmail.search"       # any tool the owner may call: an adapter, http.request, a WASM skill
input = { query = "from:billing@acme.com newer_than:7d", max_results = 20 }
every_secs = 300                   # poll cadence; default 300; the hub sets a minimum
items = "/messages"                # JSON pointer to the output's array → ITEM mode (omit for VALUE mode)
item_id = "/id"                    # pointer within each item (default "/id")
# path = "/snippet"                # pointer to the watched part of each item / of the output (default: all of it)
# text = "/snippet"                # pointer to what {{text}} renders (default: the watched part's JSON)

[monitor.condition]                # source only; default: items → "always" (every new item), value → "changed"
type = "contains"                  # always | changed | equals | contains | regex | gt | lt | gte | lte | exists | all | any
path = "/snippet"                  # pointer into the watched part (default: the part itself)
values = ["invoice"]               # contains: any of, case-insensitive
# value = "completed"              # equals / gt / lt / gte / lte (numbers compare numerically)
# pattern = "INV-\\d+"             # regex
# negate = true                    # invert any leaf
# of = [ { type = "gte", path = "/amount", value = 100 }, { type = "changed" } ]   # all / any
# A condition containing `changed` fires each time it holds; any other fires on the
# transition into holding. A first poll of items baselines (nothing fires); a first
# poll of a value evaluates it.

# ── OPTIONAL: the one model call, after the condition, before the action ──
# [monitor.decide]
# question = "Is this an invoice that needs paying, not a receipt?"
# yes = "a bill with an amount due"     # optional criteria
# no = "a receipt or a payment confirmation"
# min_probability = 0.7

[monitor.action]
type = "run_skill"                 # run_workflow | run_skill | notify | agent_task | persona (channel only)
tool = "acme-books.file_invoice"   # run_skill: one direct tool call, no model turn
input = { gmail_id = "{{id}}", subject = "{{json:/snippet}}" }
# --- run a workflow with the message/observation as its trigger payload ---
# type = "run_workflow"
# workflow = "release-note"
# --- notify a channel / DM a user (static template) ---
# type = "notify"
# target = "C0OUT"                 # channel id, or "@U123" to DM a user
# template = "{{id}} is now {{value}} (was {{previous}})"
# surface = "slack"                # source monitors: where to deliver (optional when the hub has one surface)
# --- run a one-shot agent turn and deliver its reply ---
# type = "agent_task"
# agent = "lynx"                   # optional
# prompt = "Summarise this and tell the user what changed."
# deliver_to = "@U123"             # channel id or @user DM
# surface = "slack"                # as for notify
# --- make an agent converse in the channel (channel monitors only) ---
# type = "persona"
# agent = "lynx"
# prompt = "You are the release triage assistant for this channel. ..."
# mode = "all"                     # all | mention_only | quiet
```

Placeholders in templates, prompts and run_skill inputs: {{text}} {{id}} {{channel}}
{{user}} {{ts}} {{value}} {{previous}} and {{json:/a/b}} (a pointer into the message
or the whole item; a pointer cannot contain `}`). For an observation, {{id}} is the
item id ("value" in value mode), {{channel}} the tool, {{value}} the watched part,
{{previous}} the previous one. Dates in `input` must be quoted strings.

## Authoring procedure
1. Call action 'schema' (this) for the format.
2. Draft the TOML.
3. Call action 'validate' with toml_content to check it.
4. Call action 'save' with monitor_id and toml_content to persist + load it.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
[monitor]
id = "demo"
name = "Demo"
[monitor.binding]
surface = "slack"
channel = "C1"
[monitor.filter]
type = "all"
[monitor.action]
type = "notify"
target = "C2"
template = "saw: {{text}}"
"#;

    fn input(
        action: &str,
        id: Option<&str>,
        toml: Option<&str>,
        enabled: Option<bool>,
    ) -> serde_json::Value {
        serde_json::json!({
            "action": action,
            "monitor_id": id,
            "toml_content": toml,
            "enabled": enabled,
        })
    }

    #[test]
    fn validate_accepts_good_rejects_bad() {
        let good = handle_validate(
            serde_json::from_value(input("validate", None, Some(GOOD), None)).unwrap(),
            Instant::now(),
        );
        assert_eq!(good.output["valid"], serde_json::json!(true));
        let bad = handle_validate(
            serde_json::from_value(input("validate", None, Some("[monitor]\nid=\"x\""), None))
                .unwrap(),
            Instant::now(),
        );
        assert_eq!(bad.output["valid"], serde_json::json!(false));
    }

    #[tokio::test]
    async fn save_get_set_enabled_delete_round_trip() {
        let dir = std::env::temp_dir().join(format!("montool-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = MonitorTool::new(Arc::new(MonitorRuntime::new(dir.clone())), None);

        let saved = tool
            .execute(input("save", Some("demo"), Some(GOOD), None))
            .await
            .unwrap();
        assert!(saved.success, "{saved:?}");

        assert_eq!(saved.output["status"], serde_json::json!("active"));
        let watch = saved.output["watch"].as_str().unwrap();
        assert!(watch.contains("must be a member of channel C1"), "{watch}");

        let got = tool
            .execute(input("get", Some("demo"), None, None))
            .await
            .unwrap();
        assert!(got.output["toml"].as_str().unwrap().contains("Demo"));
        assert_eq!(got.output["status"], serde_json::json!("active"));

        let listed = tool.execute(input("list", None, None, None)).await.unwrap();
        assert_eq!(listed.output["count"], serde_json::json!(1));
        assert_eq!(
            listed.output["monitors"][0]["status"],
            serde_json::json!("active")
        );

        // set_enabled round-trips the typed def back to TOML.
        let disabled = tool
            .execute(input("set_enabled", Some("demo"), None, Some(false)))
            .await
            .unwrap();
        assert!(disabled.success, "{disabled:?}");
        assert_eq!(disabled.output["status"], serde_json::json!("paused"));
        assert!(
            disabled.output["watch"]
                .as_str()
                .unwrap()
                .starts_with("Paused"),
            "{disabled:?}"
        );
        let got = tool
            .execute(input("get", Some("demo"), None, None))
            .await
            .unwrap();
        let def = MonitorDefinition::from_toml(got.output["toml"].as_str().unwrap()).unwrap();
        assert!(!def.monitor.enabled);

        // id ≠ TOML id is rejected.
        let mismatch = tool
            .execute(input("save", Some("other"), Some(GOOD), None))
            .await
            .unwrap();
        assert!(!mismatch.success);

        let deleted = tool
            .execute(input("delete", Some("demo"), None, None))
            .await
            .unwrap();
        assert!(deleted.success);
        let gone = tool
            .execute(input("get", Some("demo"), None, None))
            .await
            .unwrap();
        assert!(!gone.success);

        let _ = std::fs::remove_dir_all(&dir);
    }

    const SOURCE: &str = r#"
[monitor]
id = "invoices"
name = "Invoices"
[monitor.source]
tool = "google.gmail.search"
input = { query = "from:billing@acme.com" }
items = "/messages"
[monitor.action]
type = "run_skill"
tool = "books.file"
input = { id = "{{id}}" }
"#;

    #[tokio::test]
    async fn a_source_monitor_saves_without_an_engine_and_says_how_it_will_be_polled() {
        let dir = std::env::temp_dir().join(format!("montool-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = MonitorTool::new(Arc::new(MonitorRuntime::new(dir.clone())), None);

        let saved = tool
            .execute(input("save", Some("invoices"), Some(SOURCE), None))
            .await
            .unwrap();
        assert!(saved.success, "{saved:?}");
        let watch = saved.output["watch"].as_str().unwrap();
        assert!(
            watch.contains("Polled by agent") && watch.contains("every 300s"),
            "{watch}"
        );

        let listed = tool.execute(input("list", None, None, None)).await.unwrap();
        let entry = &listed.output["monitors"][0];
        assert_eq!(entry["kind"], serde_json::json!("source"));
        assert_eq!(entry["source"], serde_json::json!("google.gmail.search"));
        assert_eq!(entry["filter"], serde_json::json!("always"));
        assert!(entry.get("channel").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn poll_note_reports_the_first_poll_or_its_failure() {
        let def = MonitorDefinition::from_toml(SOURCE).unwrap();
        let note = poll_note(
            &def,
            &PollReport {
                seen: 14,
                fired: 2,
                error: None,
            },
        );
        assert_eq!(
            note,
            "Polled google.gmail.search: 14 observed, 2 dispatched; next poll in 300s."
        );
        let failed = poll_note(
            &def,
            &PollReport {
                error: Some("no credential".into()),
                ..PollReport::default()
            },
        );
        assert!(
            failed.contains("Polling google.gmail.search failed (no credential)"),
            "{failed}"
        );
        assert!(failed.contains("tried again in 300s"), "{failed}");
    }

    // ───────────────────── authorization (gaps a + b) ──────────────────────

    use crate::authz::{always_admin, never_admin};
    use std::collections::HashSet;
    use tokio::sync::RwLock;
    use workforce_agent::runner::{CURRENT_PLATFORM_USER_ID, CURRENT_USER_ID};
    use workforce_common::channels::{
        ChannelCapabilities, ChannelSurface, Reaction, SurfaceKey, SurfaceRegistry,
    };

    /// Mock surface that is channel-aware: `is_channel_member` is `true` only for
    /// the channels in `members`. This is what lets the *delivery* gate be tested
    /// honestly — a caller can be a member of the watched channel (so the bind
    /// gate passes) yet NOT a member of the delivery target (so the delivery gate
    /// is the thing under test, not the bind gate firing first).
    struct MemberSurface {
        members: HashSet<String>,
    }

    #[async_trait]
    impl ChannelSurface for MemberSurface {
        fn platform(&self) -> &str {
            "slack"
        }
        fn capabilities(&self) -> ChannelCapabilities {
            ChannelCapabilities::default()
        }
        async fn add_reaction(&self, _: &Reaction) -> Result<()> {
            Ok(())
        }
        async fn remove_reaction(&self, _: &Reaction) -> Result<()> {
            Ok(())
        }
        async fn is_channel_member(&self, channel: &str, _user: &str) -> Result<bool> {
            Ok(self.members.contains(channel))
        }
    }

    /// A registry whose mock surface reports membership of exactly `channels`.
    fn registry_with_channels(channels: &[&str]) -> Arc<RwLock<SurfaceRegistry>> {
        let mut reg = SurfaceRegistry::new();
        reg.register(
            SurfaceKey::workspace("slack", "test-ws"),
            Arc::new(MemberSurface {
                members: channels.iter().map(|c| c.to_string()).collect(),
            }) as Arc<dyn ChannelSurface>,
        );
        Arc::new(RwLock::new(reg))
    }

    fn fs_tool(member: bool, admin: bool) -> (MonitorTool, std::path::PathBuf) {
        fs_tool_in(if member { &["C1"] } else { &[] }, admin)
    }

    /// Like [`fs_tool`] but with an explicit set of member channels, for the
    /// delivery-gate tests (member of the watched channel, not the target).
    fn fs_tool_in(channels: &[&str], admin: bool) -> (MonitorTool, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("montool-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let is_admin = if admin { always_admin() } else { never_admin() };
        let tool = MonitorTool::new(Arc::new(MonitorRuntime::new(dir.clone())), None)
            .with_admin_policy(is_admin)
            .with_binding_access(BindingAccess::new(
                Some(registry_with_channels(channels)),
                crate::monitor::binding_access::no_platform_ids(),
            ));
        (tool, dir)
    }

    /// Run `fut` as a human turn: both `CURRENT_USER_ID` (so it isn't treated as
    /// a trusted system turn) and `CURRENT_PLATFORM_USER_ID` (the membership key)
    /// are in scope.
    async fn as_user<F, T>(uid: &str, platform_uid: &str, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        CURRENT_USER_ID
            .scope(
                uid.to_string(),
                CURRENT_PLATFORM_USER_ID.scope(platform_uid.to_string(), fut),
            )
            .await
    }

    // ── the membership gate, end to end through `save` ──
    // The rule itself is unit-tested in `binding_access`; these pin that every
    // refusal it returns reaches the author and stores nothing.

    /// End-to-end: a non-member non-admin's `save` is refused *and nothing is
    /// stored* (the gate fires before persistence).
    #[tokio::test]
    async fn save_refused_for_non_member_stores_nothing() {
        let (tool, dir) = fs_tool(false, false);
        let res = as_user(
            "u1",
            "U1",
            tool.execute(input("save", Some("demo"), Some(GOOD), None)),
        )
        .await
        .unwrap();
        assert!(!res.success, "non-member save must be refused: {res:?}");
        // Nothing persisted: the monitor is not gettable.
        let gone = tool
            .execute(input("get", Some("demo"), None, None))
            .await
            .unwrap();
        assert!(!gone.success, "refused save must not persist anything");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unbound turn addresses the system row, which only an admin holds:
    /// a non-admin system turn can no longer write a global monitor.
    #[tokio::test]
    async fn save_refused_for_unbound_non_admin() {
        let (tool, dir) = fs_tool(false, false);
        let res = tool
            .execute(input("save", Some("demo"), Some(GOOD), None))
            .await
            .unwrap();
        assert!(!res.success, "{res:?}");
        assert!(res
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("not signed in"));
        assert!(!dir.join("demo.toml").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An admin may bind a monitor to a channel even when not a member.
    #[tokio::test]
    async fn save_allowed_for_admin_non_member() {
        // Admin + no user → global save on the filesystem runtime, succeeds.
        let (tool, dir) = fs_tool(false, true);
        let res = tool
            .execute(input("save", Some("demo"), Some(GOOD), None))
            .await
            .unwrap();
        assert!(res.success, "admin must be able to bind anywhere: {res:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── gap (G15): the delivery-target gate ──
    // A member of the watched channel (C1) must not be able to direct a
    // notify/agent_task monitor to a delivery target they can't reach. The mock
    // surface is channel-aware (member of C1 only), so the bind gate on C1
    // passes and the *delivery* gate is the thing genuinely exercised.

    /// A `notify` monitor watching C1 (member) that delivers to `target`.
    fn notify_to(target: &str) -> String {
        format!(
            "[monitor]\nid = \"demo\"\nname = \"Demo\"\n\
             [monitor.binding]\nsurface = \"slack\"\nchannel = \"C1\"\n\
             [monitor.filter]\ntype = \"all\"\n\
             [monitor.action]\ntype = \"notify\"\ntarget = \"{target}\"\ntemplate = \"x: {{{{text}}}}\"\n"
        )
    }

    /// An `agent_task` monitor watching C1 (member) that delivers to `deliver_to`.
    fn agent_task_to(deliver_to: &str) -> String {
        format!(
            "[monitor]\nid = \"demo\"\nname = \"Demo\"\n\
             [monitor.binding]\nsurface = \"slack\"\nchannel = \"C1\"\n\
             [monitor.filter]\ntype = \"all\"\n\
             [monitor.action]\ntype = \"agent_task\"\nprompt = \"summarise\"\ndeliver_to = \"{deliver_to}\"\n"
        )
    }

    /// `execute(save)` as the given member-of-C1 non-admin user; assert refusal
    /// and that nothing was persisted.
    async fn assert_save_refused_nothing_stored(toml: &str) {
        let (tool, dir) = fs_tool_in(&["C1"], false);
        let res = as_user(
            "u1",
            "U1",
            tool.execute(input("save", Some("demo"), Some(toml), None)),
        )
        .await
        .unwrap();
        assert!(
            !res.success,
            "delivery to an unreachable target must be refused: {res:?}"
        );
        let gone = tool
            .execute(input("get", Some("demo"), None, None))
            .await
            .unwrap();
        assert!(!gone.success, "refused save must not persist anything");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn save_refused_when_notify_delivers_to_unjoined_channel() {
        // Member of C1 (bind ok), NOT a member of C3 (delivery refused).
        assert_save_refused_nothing_stored(&notify_to("C3")).await;
    }

    #[tokio::test]
    async fn save_refused_when_notify_dms_another_user() {
        // Member of C1 (bind ok), but DMs @other (not self) → refused.
        assert_save_refused_nothing_stored(&notify_to("@U_OTHER")).await;
    }

    #[tokio::test]
    async fn save_refused_when_agent_task_delivers_to_unjoined_channel() {
        assert_save_refused_nothing_stored(&agent_task_to("C3")).await;
    }

    #[tokio::test]
    async fn save_refused_when_agent_task_dms_another_user() {
        assert_save_refused_nothing_stored(&agent_task_to("@U_OTHER")).await;
    }

    #[tokio::test]
    async fn admin_can_deliver_anywhere() {
        // Admin + no user → global save on the filesystem runtime succeeds even
        // though the delivery target (C3) and DM (@U_OTHER) are unreachable.
        let (tool, dir) = fs_tool_in(&[], true);
        let chan = tool
            .execute(input("save", Some("demo"), Some(&notify_to("C3")), None))
            .await
            .unwrap();
        assert!(chan.success, "admin must deliver to any channel: {chan:?}");
        let dm = tool
            .execute(input(
                "save",
                Some("demo"),
                Some(&agent_task_to("@U_OTHER")),
                None,
            ))
            .await
            .unwrap();
        assert!(dm.success, "admin must DM any user: {dm:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── the `owner` input: another member's row, when the shares allow ──

    fn owned_input(
        action: &str,
        id: &str,
        owner: Option<&str>,
        enabled: Option<bool>,
    ) -> serde_json::Value {
        serde_json::json!({
            "action": action,
            "monitor_id": id,
            "enabled": enabled,
            "owner": owner,
        })
    }

    /// A monitor runtime over a real definition store, with `demo` seeded for
    /// each owner. Seeding through the store rather than `save` keeps the
    /// channel-bind gate (proven above) out of this test.
    async fn store_backed(
        store: &Arc<workforce_orchestrator::workflow_store::WorkflowStore>,
        owners: &[(&str, &str)],
    ) -> (Arc<MonitorRuntime>, std::path::PathBuf) {
        use workforce_core::access::Visibility;
        use workforce_orchestrator::workflow_store::RunAsWrite;
        let dir = std::env::temp_dir().join(format!("montool-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let runtime = Arc::new(MonitorRuntime::new(dir.clone()));
        runtime.set_store(Arc::clone(store));
        for (owner, toml) in owners {
            store
                .save(
                    "demo",
                    KIND_MONITOR,
                    toml,
                    None,
                    Some(owner),
                    Visibility::Private,
                    RunAsWrite::Unchanged,
                    EditContext::new("test", Some(owner)),
                )
                .await
                .unwrap();
        }
        runtime.reload().await.unwrap();
        (runtime, dir)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn owner_routes_reads_and_writes_exactly_as_far_as_the_shares_do() {
        workforce_postgres::test_support::with_test_pool(
            workforce_orchestrator::workflow_store::WorkflowStore::build_pool,
            |pool| async move {
                use workforce_core::access::{Grant, GrantLevel, Visibility};
                use workforce_orchestrator::workflow_store::WorkflowStore;
                let store = WorkflowStore::new(pool).await.expect("store");
                let hers = GOOD.replace("saw: {{text}}", "alice saw: {{text}}");
                let (runtime, dir) = store_backed(&store, &[("alice", &hers), ("bob", GOOD)]).await;
                let tool = MonitorTool::new(Arc::clone(&runtime), None)
                    .with_write_auth(TurnDefWriteAuth::default().with_admin_policy(never_admin()))
                    .with_binding_access(BindingAccess::new(
                        Some(registry_with_channels(&["C1", "C2"])),
                        crate::monitor::binding_access::no_platform_ids(),
                    ));
                store
                    .set_shares(
                        KIND_MONITOR,
                        "demo",
                        "alice",
                        &[Grant::user("bob", GrantLevel::Manage)],
                        "alice",
                    )
                    .await
                    .unwrap();

                // Bare id → the caller's own row; `owner` → hers.
                let mine = as_user(
                    "bob",
                    "U1",
                    tool.execute(owned_input("get", "demo", None, None)),
                )
                .await
                .unwrap();
                assert!(mine.output["toml"]
                    .as_str()
                    .unwrap()
                    .contains("saw: {{text}}"));
                assert!(!mine.output["toml"].as_str().unwrap().contains("alice saw"));
                let hers_read = as_user(
                    "bob",
                    "U1",
                    tool.execute(owned_input("get", "demo", Some("alice"), None)),
                )
                .await
                .unwrap();
                assert!(hers_read.output["toml"]
                    .as_str()
                    .unwrap()
                    .contains("alice saw"));

                // A row nobody shared with carol is, to her, not there.
                let carol = as_user(
                    "carol",
                    "U2",
                    tool.execute(owned_input("get", "demo", Some("alice"), None)),
                )
                .await
                .unwrap();
                assert!(!carol.success, "{carol:?}");
                assert_eq!(carol.error.as_deref(), Some("Unknown monitor 'demo'."));

                // A manage grantee changes hers; a stranger cannot.
                let paused = as_user(
                    "bob",
                    "U1",
                    tool.execute(owned_input(
                        "set_enabled",
                        "demo",
                        Some("alice"),
                        Some(false),
                    )),
                )
                .await
                .unwrap();
                assert!(paused.success, "{paused:?}");
                let refused = as_user(
                    "carol",
                    "U2",
                    tool.execute(owned_input(
                        "set_enabled",
                        "demo",
                        Some("alice"),
                        Some(false),
                    )),
                )
                .await
                .unwrap();
                assert!(!refused.success, "{refused:?}");

                // A listing now carries what a grantee may manage, and nothing
                // else: bob sees his own and alice's, carol neither.
                let bobs = as_user(
                    "bob",
                    "U1",
                    tool.execute(owned_input("list", "demo", None, None)),
                )
                .await
                .unwrap();
                assert_eq!(bobs.output["count"], serde_json::json!(2), "{bobs:?}");
                let carols = as_user(
                    "carol",
                    "U2",
                    tool.execute(owned_input("list", "demo", None, None)),
                )
                .await
                .unwrap();
                assert_eq!(carols.output["count"], serde_json::json!(0), "{carols:?}");

                let deleted = as_user(
                    "carol",
                    "U2",
                    tool.execute(owned_input("delete", "demo", Some("alice"), None)),
                )
                .await
                .unwrap();
                assert!(!deleted.success, "{deleted:?}");
                assert!(store
                    .get("demo", KIND_MONITOR, Some("alice"))
                    .await
                    .unwrap()
                    .is_some());

                // A save lands on the row `owner` names when the shares allow
                // it, and nowhere at all when they do not.
                let edited = hers.replace("alice saw", "bob edited");
                let save = |toml: &str, owner: &str| {
                    serde_json::json!({
                        "action": "save",
                        "monitor_id": "demo",
                        "toml_content": toml,
                        "owner": owner,
                    })
                };
                let bobs_edit = as_user("bob", "U1", tool.execute(save(&edited, "alice")))
                    .await
                    .unwrap();
                assert!(bobs_edit.success, "{bobs_edit:?}");
                assert!(store
                    .get("demo", KIND_MONITOR, Some("alice"))
                    .await
                    .unwrap()
                    .unwrap()
                    .contains("bob edited"));
                let carols_edit = as_user("carol", "U2", tool.execute(save(&edited, "alice")))
                    .await
                    .unwrap();
                assert!(!carols_edit.success, "{carols_edit:?}");
                assert!(store
                    .get("demo", KIND_MONITOR, Some("carol"))
                    .await
                    .unwrap()
                    .is_none());

                // A save is an edit, not a re-scope: a reach its owner chose
                // elsewhere (`cmd: monitor visibility`, the UI) survives the
                // next agent save, which never sends a visibility of its own.
                let is_global = |owner: &'static str| {
                    let store = Arc::clone(&store);
                    async move {
                        store
                            .list(KIND_MONITOR)
                            .await
                            .unwrap()
                            .into_iter()
                            .find(|d| d.id == "demo" && d.owner.as_deref() == Some(owner))
                            .unwrap()
                            .visibility
                            == Visibility::Global
                    }
                };
                store
                    .set_visibility(KIND_MONITOR, "demo", "bob", Visibility::Global)
                    .await
                    .unwrap();
                runtime.reload().await.unwrap();
                let own_edit = as_user("bob", "U1", tool.execute(save(GOOD, "")))
                    .await
                    .unwrap();
                assert!(own_edit.success, "{own_edit:?}");
                assert!(is_global("bob").await, "the owner's own reach survives");
                assert!(!is_global("alice").await, "and nobody else's moved");

                // An administrator lists every row — the standing `may_read_def`
                // already grants them, which a listing used to withhold.
                let admin = MonitorTool::new(Arc::clone(&runtime), None)
                    .with_write_auth(TurnDefWriteAuth::default().with_admin_policy(always_admin()));
                let all = as_user(
                    "carol",
                    "U2",
                    admin.execute(owned_input("list", "demo", None, None)),
                )
                .await
                .unwrap();
                assert_eq!(all.output["count"], serde_json::json!(2), "{all:?}");

                let _ = std::fs::remove_dir_all(&dir);
            },
        )
        .await;
    }

    // ── gap (b): global-namespace fallback on mutations ──

    /// Seed a global monitor (admin, system turn) into a fresh tool's dir.
    async fn seed_global(dir: &std::path::Path) {
        let admin = MonitorTool::new(Arc::new(MonitorRuntime::new(dir.to_path_buf())), None)
            .with_admin_policy(always_admin());
        let res = admin
            .execute(input("save", Some("demo"), Some(GOOD), None))
            .await
            .unwrap();
        assert!(res.success, "seed failed: {res:?}");
    }

    #[tokio::test]
    async fn non_admin_cannot_set_enabled_global() {
        let dir = std::env::temp_dir().join(format!("montool-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        seed_global(&dir).await;
        // Fresh non-admin tool over the SAME dir, no own copy of "demo".
        let tool = MonitorTool::new(Arc::new(MonitorRuntime::new(dir.clone())), None)
            .with_admin_policy(never_admin());
        let res = as_user(
            "u1",
            "U1",
            tool.execute(input("set_enabled", Some("demo"), None, Some(false))),
        )
        .await
        .unwrap();
        assert!(!res.success, "non-admin must not disable a global monitor");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn non_admin_cannot_delete_global() {
        let dir = std::env::temp_dir().join(format!("montool-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        seed_global(&dir).await;
        let tool = MonitorTool::new(Arc::new(MonitorRuntime::new(dir.clone())), None)
            .with_admin_policy(never_admin());
        let res = as_user(
            "u1",
            "U1",
            tool.execute(input("delete", Some("demo"), None, None)),
        )
        .await
        .unwrap();
        assert!(!res.success, "non-admin must not delete a global monitor");
        // Still present for an admin.
        let admin = MonitorTool::new(Arc::new(MonitorRuntime::new(dir.clone())), None)
            .with_admin_policy(always_admin());
        let got = admin
            .execute(input("get", Some("demo"), None, None))
            .await
            .unwrap();
        assert!(
            got.success,
            "global monitor must survive the refused delete"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn admin_can_set_enabled_and_delete_global() {
        let dir = std::env::temp_dir().join(format!("montool-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        seed_global(&dir).await;
        let admin = MonitorTool::new(Arc::new(MonitorRuntime::new(dir.clone())), None)
            .with_admin_policy(always_admin());
        let disabled = admin
            .execute(input("set_enabled", Some("demo"), None, Some(false)))
            .await
            .unwrap();
        assert!(disabled.success, "admin must mutate globals: {disabled:?}");
        let deleted = admin
            .execute(input("delete", Some("demo"), None, None))
            .await
            .unwrap();
        assert!(deleted.success, "admin must delete globals: {deleted:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
