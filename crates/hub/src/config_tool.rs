//! `system.config` — the agent reads and changes its own workspace
//! `config.toml` by dotted key (`llm.voice_model`), through the same write
//! the config API makes: admitted, recorded as a revision, hot-applied.
//!
//! The text is edited in place, so comments and keys the agent does not
//! model survive. Credentials never pass through it: they live in the vault.
//!
//! An agent sets itself up, so it may change its own `[skills]`. A change
//! that widens what it reaches (a connector on, a connection it now uses,
//! one it may now change things through) is a connection write to Desk's
//! gate: below unbound it asks first, Allow once or Deny, as
//! `connections.use` does, and is kept in Desk's Activity; once saved, the
//! person is told in the agent's conversation.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde::Deserialize;
use toml_edit::{DocumentMut, Item, TableLike};
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::settings::AgentWorkspaceConfig;
use workforce_orchestrator::workflow_store::WorkflowStore;

use crate::agent_config_write;
use crate::serve::AgentService;

pub struct ConfigTool {
    service: Arc<AgentService>,
    /// Where each change is recorded as a revision; `None` without Postgres.
    revisions: Option<Arc<WorkflowStore>>,
}

impl ConfigTool {
    pub fn new(service: Arc<AgentService>, revisions: Option<Arc<WorkflowStore>>) -> Self {
        Self { service, revisions }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    Get,
    Set,
    Unset,
}

#[derive(Deserialize)]
struct Input {
    action: Action,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    value: Option<serde_json::Value>,
}

impl ConfigTool {
    /// The file as it is, and as `input` would leave it; `None` for a read,
    /// or a change that can't be made.
    fn proposed(&self, input: &serde_json::Value) -> Option<(String, String)> {
        let input: Input = serde_json::from_value(input.clone()).ok()?;
        let key = input.key.as_deref()?;
        let path = key_path(key).ok()?;
        let read = self.read().ok()?.unwrap_or_default();
        let mut doc = parse_document(&read).ok()?;
        match input.action {
            Action::Get => return None,
            Action::Set => assign(
                doc.as_table_mut(),
                &path,
                toml_value(input.value.as_ref()?).ok()?,
            )
            .ok()?,
            Action::Unset => {
                remove(doc.as_table_mut(), &path);
            }
        }
        Some((read, doc.to_string()))
    }

    /// What `input` would widen of this agent's reach, in words.
    fn widens(&self, input: &serde_json::Value) -> Vec<String> {
        self.proposed(input)
            .map(|(was, now)| widened_between(&was, &now))
            .unwrap_or_default()
    }
}

/// What going from config `was` to `now` widens of an agent's reach.
fn widened_between(was: &str, now: &str) -> Vec<String> {
    let binding = |body: &str| {
        AgentWorkspaceConfig::parse(body)
            .ok()
            .and_then(|c| c.connectors().ok())
            .unwrap_or_default()
    };
    workforce_common::connector_binding::widened(&binding(was), &binding(now))
}

#[async_trait]
impl Tool for ConfigTool {
    fn name(&self) -> &str {
        "system.config"
    }

    fn description(&self) -> &str {
        "Read or change this agent's own config.toml, the per-agent settings that override \
         workforce.toml, by dotted key such as `llm.voice_model` or `compaction.keep_recent_messages`. \
         `get` without a key returns the whole file. `set` and `unset` save it as the settings UI \
         does: refused if the agent would not load it, recorded in its history, applied at once. \
         Credentials belong in the vault, never here."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["get", "set", "unset"]},
                "key": {
                    "type": "string",
                    "description": "Dotted path of the setting, e.g. `llm.voice_model`. Optional for get."
                },
                "value": {"description": "The new value, for set: a string, number, boolean, array or object."}
            },
            "required": ["action"]
        })
    }

    /// A change that widens what the agent reaches is a connection write
    /// to Desk's gate; any other change touches nothing it decides.
    fn effect(&self, input: &serde_json::Value) -> workforce_core::computer::Effect {
        match self.widens(input).is_empty() {
            true => workforce_core::computer::Effect::None,
            false => workforce_core::computer::Effect::Connector {
                connector: "connections".into(),
                write: true,
            },
        }
    }

    fn approval_summary(&self, input: &serde_json::Value) -> Option<String> {
        let widens = self.widens(input);
        (!widens.is_empty()).then(|| {
            format!(
                "Let {} reach more: {}",
                self.service.agent_id,
                widens.join("; ")
            )
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let widening = self.widens(&input);
        let input: Input = serde_json::from_value(input)?;
        let outcome = match input.action {
            Action::Get => self.get(input.key.as_deref()),
            Action::Set => match (input.key.as_deref(), input.value) {
                (Some(key), Some(value)) => self.set(key, &value).await,
                _ => Err("set needs a `key` and a `value`".to_string()),
            },
            Action::Unset => match input.key.as_deref() {
                Some(key) => self.unset(key).await,
                None => Err("unset needs a `key`".to_string()),
            },
        };
        if outcome.is_ok() && !widening.is_empty() {
            self.tell_the_person(&widening).await;
        }
        let ms = start.elapsed().as_millis() as u64;
        Ok(match outcome {
            Ok(result) => ToolResult::success(result, ms),
            Err(reason) => ToolResult::failure(reason, ms),
        })
    }
}

impl ConfigTool {
    fn read(&self) -> Result<Option<String>, String> {
        workforce_common::config_files::read(
            &self
                .service
                .workspace
                .join(workforce_common::config_files::WORKSPACE_CONFIG),
        )
        .map_err(|e| format!("could not read config.toml: {e}"))
    }

    fn get(&self, key: Option<&str>) -> Result<serde_json::Value, String> {
        let body = self.read()?.unwrap_or_default();
        let mut doc = parse_document(&body)?;
        redact(doc.as_table_mut());
        let Some(key) = key else {
            return Ok(serde_json::json!({ "config_toml": doc.to_string() }));
        };
        let path = key_path(key)?;
        Ok(match lookup(doc.as_table(), &path) {
            Some(item) => serde_json::json!({ "key": key, "value": item.to_string().trim() }),
            None => serde_json::json!({
                "key": key,
                "set": false,
                "note": "not set in config.toml; the agent uses the install's default",
            }),
        })
    }

    async fn set(&self, key: &str, value: &serde_json::Value) -> Result<serde_json::Value, String> {
        let path = key_path(key)?;
        human_only(&path)?;
        if let Some(secret) = path
            .iter()
            .find(|segment| is_secret_name(segment))
            .filter(|_| !is_binding(&path))
        {
            return Err(format!(
                "`{secret}` is a credential; store it in the vault, not config.toml"
            ));
        }
        let value = toml_value(value)?;
        let read = self.read()?;
        let mut doc = parse_document(read.as_deref().unwrap_or_default())?;
        assign(doc.as_table_mut(), &path, value)?;
        let body = doc.to_string();
        let parsed = AgentWorkspaceConfig::parse(&body)
            .map_err(|e| format!("`{key}` cannot take that value: {e}"))?;
        if !reads_key(&parsed, &path)? {
            return Err(format!("`{key}` is not a setting this agent reads"));
        }
        self.save(read.as_deref(), &body).await
    }

    async fn unset(&self, key: &str) -> Result<serde_json::Value, String> {
        let path = key_path(key)?;
        human_only(&path)?;
        let read = self.read()?;
        let mut doc = parse_document(read.as_deref().unwrap_or_default())?;
        if !remove(doc.as_table_mut(), &path) {
            return Ok(serde_json::json!({ "key": key, "changed": false, "note": "was not set" }));
        }
        self.save(read.as_deref(), &doc.to_string()).await
    }

    /// Tells the person, in this agent's Desk conversation, what it just
    /// let itself reach.
    async fn tell_the_person(&self, widened: &[String]) {
        tracing::info!(agent = %self.service.agent_id, widened = ?widened, "an agent widened its own connectors");
        #[cfg(feature = "postgres")]
        {
            use workforce_common::channels::desk::{DeliveryKind, DeskPlace, DeskTarget};
            let Some(person) = crate::user_config::current_identity_id() else {
                return;
            };
            if !self.service.config.desk.enabled {
                return;
            }
            let text = format!(
                "I changed my own connectors, so I reach more now: {}. Change it back in my Connectors.",
                widened.join("; ")
            );
            let target = DeskTarget::of_turn(
                &DeskPlace::Dm,
                workforce_common::channels::DESK_CHANNEL,
                None,
                false,
            );
            if let Err(e) = crate::desk_surface::deliver(
                &*self.service,
                &person,
                &target,
                crate::desk_surface::Delivery::text(DeliveryKind::Notice, text),
            )
            .await
            {
                tracing::warn!(error = %format!("{e:#}"), "telling the person an agent widened its connectors failed");
            }
        }
    }

    /// Save `body` over the file this tool read as `read`.
    async fn save(&self, read: Option<&str>, body: &str) -> Result<serde_json::Value, String> {
        let actor = crate::user_config::current_identity_id();
        let edit = workforce_orchestrator::workflow_store::EditContext::new(
            "agent_tool",
            actor.as_deref(),
        );
        let saved = agent_config_write::save(
            &self.service,
            self.service.agent_id.as_str(),
            self.revisions.as_deref(),
            body,
            edit,
            |current| current == read,
        )
        .await
        .map_err(|refused| refused.to_string())?;
        Ok(serde_json::json!({
            "changed": true,
            "findings": saved.findings,
            "restart_required": saved.restart_required,
        }))
    }
}

fn parse_document(body: &str) -> Result<DocumentMut, String> {
    body.parse::<DocumentMut>()
        .map_err(|e| format!("config.toml does not parse: {e}"))
}

fn key_path(key: &str) -> Result<Vec<&str>, String> {
    let path: Vec<&str> = key.split('.').map(str::trim).collect();
    if path.iter().any(|segment| segment.is_empty()) {
        return Err(format!(
            "`{key}` is not a dotted key such as `llm.voice_model`"
        ));
    }
    Ok(path)
}

/// Refuse the settings only a human changes: the agent's environments decide
/// which instances it may reach, `[oversight]` what it oversees and may do
/// there, and `[desk_boss]` whether a Desk may make it a boss, so it never
/// edits them itself.
fn human_only(path: &[&str]) -> Result<(), String> {
    match path.first() {
        Some(&"oversight") => Err(
            "oversight is human-only: the person sets what an agent oversees in Desk".to_string(),
        ),
        Some(&"environments") => Err(
            "environments are human-only: a person changes them with `cmd: env …` or in the \
             agent's settings"
                .to_string(),
        ),
        Some(&"desk_boss") => Err(
            "desk_boss is human-only: an admin decides whether this agent takes boss work \
             from Desk"
                .to_string(),
        ),
        _ => Ok(()),
    }
}

/// `[skills] own_credential`: which connectors use the agent's own
/// connection. A binding, not a secret, though its name says credential.
pub(crate) fn is_binding(path: &[&str]) -> bool {
    path.first() == Some(&"skills") && path.get(1).is_none_or(|k| *k == "own_credential")
}

/// Key names whose values are credentials.
pub(crate) fn is_secret_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "api_key",
        "secret",
        "password",
        "credential",
        "private_key",
        "authorization",
        "bearer",
    ]
    .iter()
    .any(|word| name.contains(word))
        || name == "token"
        || name.ends_with("_token")
        || name == "key"
        || name == "auth"
}

/// Blank every credential in `table` at any depth: a string under a
/// credential's name, or one that looks like a credential whatever its name.
fn redact(table: &mut dyn TableLike) {
    for (key, item) in table.iter_mut() {
        let named = is_secret_name(key.get());
        if let Some(value) = item.as_value_mut() {
            redact_value(value, named);
        } else if let Some(inner) = item.as_table_like_mut() {
            redact(inner);
        } else if let Some(tables) = item.as_array_of_tables_mut() {
            tables.iter_mut().for_each(|t| redact(t));
        }
    }
}

fn redact_value(value: &mut toml_edit::Value, named: bool) {
    match value {
        toml_edit::Value::String(s)
            if named || workforce_common::redact::contains_secret(s.value()) =>
        {
            let decor = s.decor().clone();
            *s = toml_edit::Formatted::new("[redacted]".to_string());
            *s.decor_mut() = decor;
        }
        toml_edit::Value::Array(items) => items.iter_mut().for_each(|v| redact_value(v, named)),
        toml_edit::Value::InlineTable(table) => {
            for (key, v) in table.iter_mut() {
                let named = named || is_secret_name(key.get());
                redact_value(v, named);
            }
        }
        _ => {}
    }
}

fn lookup<'a>(table: &'a dyn TableLike, path: &[&str]) -> Option<&'a Item> {
    let (last, parents) = path.split_last()?;
    let mut table = table;
    for segment in parents {
        table = table.get(segment)?.as_table_like()?;
    }
    table.get(last)
}

/// Set `path` to `value`, making the tables on the way and keeping the
/// comments around a value it replaces.
fn assign(root: &mut dyn TableLike, path: &[&str], value: toml_edit::Value) -> Result<(), String> {
    let (last, parents) = path.split_last().expect("key_path is never empty");
    let mut table = root;
    for segment in parents {
        table = table
            .entry(segment)
            .or_insert_with(|| {
                let mut new = toml_edit::Table::new();
                new.set_implicit(true);
                Item::Table(new)
            })
            .as_table_like_mut()
            .ok_or_else(|| format!("`{segment}` holds a value, not a table of settings"))?;
    }
    match table.get_mut(last).and_then(Item::as_value_mut) {
        Some(existing) => {
            let decor = existing.decor().clone();
            *existing = value;
            *existing.decor_mut() = decor;
        }
        None => {
            table.insert(last, Item::Value(value));
        }
    }
    Ok(())
}

/// Remove `path`; whether it was there.
fn remove(root: &mut dyn TableLike, path: &[&str]) -> bool {
    let Some((last, parents)) = path.split_last() else {
        return false;
    };
    let mut table = root;
    for segment in parents {
        match table.get_mut(segment).and_then(Item::as_table_like_mut) {
            Some(inner) => table = inner,
            None => return false,
        }
    }
    table.remove(last).is_some()
}

/// Whether the agent reads `path` once `config` is loaded: a key it does not
/// model is dropped on parsing, so it is missing when the config is written
/// back out.
fn reads_key(config: &AgentWorkspaceConfig, path: &[&str]) -> Result<bool, String> {
    let root = toml::Value::try_from(config)
        .map_err(|e| format!("could not check the key against the agent's settings: {e}"))?;
    let mut current = &root;
    for segment in path {
        match current.get(segment) {
            Some(next) => current = next,
            None => return Ok(false),
        }
    }
    Ok(true)
}

fn toml_value(value: &serde_json::Value) -> Result<toml_edit::Value, String> {
    use serde_json::Value as J;
    Ok(match value {
        J::Null => return Err("a value cannot be null; use unset to remove a setting".into()),
        J::Bool(b) => (*b).into(),
        J::Number(n) => match n.as_i64() {
            Some(i) => i.into(),
            None => n
                .as_f64()
                .ok_or_else(|| format!("{n} is out of range for TOML"))?
                .into(),
        },
        J::String(s) => s.as_str().into(),
        J::Array(items) => items
            .iter()
            .map(toml_value)
            .collect::<Result<toml_edit::Array, _>>()?
            .into(),
        J::Object(fields) => {
            let mut table = toml_edit::InlineTable::new();
            for (k, v) in fields {
                table.insert(k, toml_value(v)?);
            }
            table.into()
        }
    })
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_change_to_its_own_skills_that_widens_its_reach_is_told_apart() {
        let was = "[skills]\nenabled = [\"jira\"]\nread_only = [\"jira:globex\"]\n\n[skills.connections]\njira = [\"acme\", \"globex\"]\n";
        let mut doc = parse_document(was).unwrap();
        assign(
            doc.as_table_mut(),
            &["skills", "read_only"],
            toml_value(&serde_json::json!([])).unwrap(),
        )
        .unwrap();
        assert_eq!(
            widened_between(was, &doc.to_string()),
            ["jira: may now change things through `globex`"]
        );
        let mut doc = parse_document(was).unwrap();
        assign(
            doc.as_table_mut(),
            &["llm", "voice_model"],
            toml_value(&serde_json::json!("x")).unwrap(),
        )
        .unwrap();
        assert!(widened_between(was, &doc.to_string()).is_empty());
        let mut doc = parse_document(was).unwrap();
        remove(doc.as_table_mut(), &["skills", "connections"]);
        assert_eq!(
            widened_between(was, &doc.to_string()),
            ["jira: now uses every connection"]
        );
    }

    use super::*;

    const SCREENSHOT: &str = "# Per-agent configuration — change it in the agent's settings.\n\
# Overrides global defaults from workforce.toml.\n\
\n\
[llm]\n\
provider = \"gemini\"\n\
default_model = \"gemini-3.8-flash\"\n\
max_tokens = 65536\n\
voice_model = \"gemini-3.8-flash\" # spoken turns\n";

    fn assigned(body: &str, key: &str, value: serde_json::Value) -> String {
        let mut doc = parse_document(body).unwrap();
        assign(
            doc.as_table_mut(),
            &key_path(key).unwrap(),
            toml_value(&value).unwrap(),
        )
        .unwrap();
        doc.to_string()
    }

    #[test]
    fn set_replaces_the_value_and_keeps_every_comment() {
        let body = assigned(
            SCREENSHOT,
            "llm.voice_model",
            serde_json::json!("gemini-3.1-pro"),
        );
        assert!(body.starts_with("# Per-agent configuration"), "{body}");
        assert!(
            body.contains("voice_model = \"gemini-3.1-pro\" # spoken turns"),
            "{body}"
        );
        assert!(body.contains("max_tokens = 65536\n"), "{body}");
    }

    #[test]
    fn environments_are_human_only_at_any_depth() {
        for key in [
            "environments",
            "environments.default",
            "environments.enabled",
            "environments.defs.work.skills.linear",
        ] {
            let err = human_only(&key_path(key).unwrap()).unwrap_err();
            assert!(err.contains("human-only"), "{key}: {err}");
        }
        assert!(human_only(&key_path("llm.voice_model").unwrap()).is_ok());
        for key in [
            "oversight",
            "oversight.group",
            "oversight.act.hand_out_work",
            // Its own limits at Act fully: never its to change.
            "oversight.level",
            "oversight.full.repos",
            "oversight.full.budget_usd_per_day",
            "oversight.wake.hold_minutes",
            // Whether it may be made a Desk group's boss.
            "desk_boss",
            "desk_boss.accept",
        ] {
            assert!(
                human_only(&key_path(key).unwrap()).is_err(),
                "{key} is the person's"
            );
        }
    }

    #[test]
    fn integers_stay_integers() {
        let body = assigned(SCREENSHOT, "llm.max_rounds", serde_json::json!(75));
        assert!(body.contains("max_rounds = 75\n"), "{body}");
        let parsed = AgentWorkspaceConfig::parse(&body).unwrap();
        assert_eq!(parsed.llm.unwrap().max_rounds, Some(75));
    }

    #[test]
    fn set_makes_missing_tables() {
        let body = assigned("", "compaction.keep_recent_messages", serde_json::json!(12));
        let parsed = AgentWorkspaceConfig::parse(&body).unwrap();
        assert!(
            reads_key(&parsed, &["compaction", "keep_recent_messages"]).unwrap(),
            "{body}"
        );
    }

    #[test]
    fn a_misspelt_key_is_not_one_the_agent_reads() {
        let body = assigned(SCREENSHOT, "llm.voice_modle", serde_json::json!("x"));
        let parsed = AgentWorkspaceConfig::parse(&body).unwrap();
        assert!(!reads_key(&parsed, &["llm", "voice_modle"]).unwrap());
        assert!(reads_key(&parsed, &["llm", "voice_model"]).unwrap());
    }

    #[test]
    fn unset_removes_only_that_key() {
        let mut doc = parse_document(SCREENSHOT).unwrap();
        assert!(remove(doc.as_table_mut(), &["llm", "voice_model"]));
        assert!(!remove(doc.as_table_mut(), &["llm", "voice_model"]));
        let body = doc.to_string();
        assert!(!body.contains("voice_model"), "{body}");
        assert!(
            body.contains("default_model = \"gemini-3.8-flash\""),
            "{body}"
        );
    }

    #[test]
    fn get_blanks_credentials_entirely() {
        let mut doc =
            parse_document("[llm]\napi_key = \"sk-ant-abcdef123456\"\nmax_tokens = 10\n").unwrap();
        redact(doc.as_table_mut());
        let body = doc.to_string();
        assert!(!body.contains("sk-ant"), "{body}");
        assert!(body.contains("api_key = \"[redacted]\""), "{body}");
        assert!(body.contains("max_tokens = 10"), "{body}");
    }

    /// The whole file from the settings screenshot, set the way allan was asked.
    #[test]
    fn voice_model_sets_on_the_real_config() {
        let real = "# Per-agent configuration — change it in the agent's settings.\n\
# Overrides global defaults from workforce.toml.\n\
# This file survives CI deploys.\n\
\n\
[llm]\n\
provider = \"gemini\"\n\
default_model = \"gemini-3.8-flash\"\n\
max_tokens = 65536\n\
max_rounds = 75\n\
failover = [\"claude-code:sonnet\"]\n\
thinking_budget_tokens = 10000\n\
voice_model = \"gemini-3.8-flash\"\n\
voice_thinking_budget_tokens = 0\n\
fetch_pricing = true\n";
        let body = assigned(real, "llm.voice_model", serde_json::json!("gemini-3.1-pro"));
        assert_eq!(
            body,
            real.replace(
                "voice_model = \"gemini-3.8-flash\"",
                "voice_model = \"gemini-3.1-pro\""
            )
        );
        let parsed = AgentWorkspaceConfig::parse(&body).unwrap();
        assert!(reads_key(&parsed, &["llm", "voice_model"]).unwrap());
        assert_eq!(
            parsed.llm.unwrap().voice_model.as_deref(),
            Some("gemini-3.1-pro")
        );
    }

    #[test]
    fn get_blanks_credentials_under_any_name() {
        let mut doc = parse_document(
            "[mcp.servers.github]\nheaders = { Authorization = \"Bearer abc\", X-Trace = \"on\" }\n\
             env = [\"ghp_abcdefghijklmnopqrstuvwxyz0123456789\"]\n",
        )
        .unwrap();
        redact(doc.as_table_mut());
        let body = doc.to_string();
        assert!(!body.contains("Bearer abc"), "{body}");
        assert!(!body.contains("ghp_"), "{body}");
        assert!(body.contains("X-Trace = \"on\""), "{body}");
    }

    #[test]
    fn token_counts_are_not_credentials() {
        assert!(!is_secret_name("max_tokens"));
        assert!(!is_secret_name("voice_thinking_budget_tokens"));
        assert!(is_secret_name("api_key"));
        assert!(is_secret_name("access_token"));
    }
}
