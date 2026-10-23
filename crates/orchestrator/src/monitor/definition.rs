//! Monitor definition — parsed from TOML.
//!
//! A monitor durably watches either a channel on a messaging surface or a
//! *source* — anything a tool can read — and dispatches an action per matching
//! message or observation. Stored and revisioned like a workflow
//! (`workflow_defs`, kind = `monitor`); the durability state — cursors,
//! observations and dispatches — lives in [`super::store`].

use std::fs;
use std::path::Path;

use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr};
use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use workforce_common::channels::InboundEvent;

use super::condition::{as_number, CompiledCondition};

/// `binding.surface` value meaning "every connected surface".
pub const SURFACE_ANY: &str = "any";

/// The default poll cadence of a source, in seconds.
pub const DEFAULT_EVERY_SECS: u64 = 300;

/// The pointer a source's items carry their id at when it names none.
pub const DEFAULT_ITEM_ID: &str = "/id";

/// A complete monitor definition parsed from TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorDefinition {
    pub monitor: MonitorMeta,
}

/// Top-level monitor metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorMeta {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// The agent whose engine runs the monitor (its bot watches a channel; its
    /// tools read a source). Absent means the action's agent when it names
    /// one, else the hub's default agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Stop after the first dispatch that completes.
    #[serde(default, skip_serializing_if = "is_false")]
    pub once: bool,
    /// Stop at this instant, fired or not.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_instant"
    )]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<Binding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<Filter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<Condition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decide: Option<Decide>,
    pub action: Action,
}

static FILTER_ALL: Filter = Filter::All;

impl MonitorMeta {
    /// The explicitly watching agent: `agent`, else the action's agent. `None`
    /// means the hub's default agent.
    pub fn watcher(&self) -> Option<&str> {
        self.agent
            .as_deref()
            .or(match &self.action {
                Action::AgentTask { agent, .. } => agent.as_deref(),
                Action::Persona { agent, .. } => Some(agent.as_str()),
                Action::RunWorkflow { .. } | Action::Notify { .. } | Action::RunSkill { .. } => {
                    None
                }
            })
            .filter(|a| !a.is_empty())
    }

    /// The channel this monitor watches, when it watches one.
    pub fn binding(&self) -> Option<&Binding> {
        self.binding.as_ref()
    }

    /// The source this monitor polls, when it polls one.
    pub fn source(&self) -> Option<&Source> {
        self.source.as_ref()
    }

    /// The message filter a channel monitor applies; `all` when it names none.
    pub fn filter(&self) -> &Filter {
        self.filter.as_ref().unwrap_or(&FILTER_ALL)
    }

    /// The condition a source monitor evaluates: its own, else the source's
    /// default (every new item; any change of a value).
    pub fn condition(&self) -> Option<Condition> {
        self.source.as_ref().map(|source| {
            self.condition
                .clone()
                .unwrap_or_else(|| source.default_condition())
        })
    }

    /// Whether `now` is past this monitor's expiry.
    pub fn expired_at(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|at| at <= now)
    }
}

fn default_true() -> bool {
    true
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_empty_object(v: &Value) -> bool {
    v.as_object().is_some_and(|o| o.is_empty())
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

/// An instant written as a quoted RFC 3339 string or a bare TOML datetime.
fn deserialize_instant<'de, D: Deserializer<'de>>(
    de: D,
) -> Result<Option<DateTime<Utc>>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Written {
        Text(String),
        Datetime(toml::value::Datetime),
    }
    let Some(written) = Option::<Written>::deserialize(de)? else {
        return Ok(None);
    };
    let text = match written {
        Written::Text(text) => text,
        Written::Datetime(dt) => dt.to_string(),
    };
    DateTime::parse_from_rfc3339(&text)
        .map(|at| Some(at.with_timezone(&Utc)))
        .map_err(|e| serde::de::Error::custom(format!("expires_at '{text}' is not RFC 3339: {e}")))
}

/// Which channel, on which surface, the monitor watches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    /// `slack` | `teams` | `any`.
    pub surface: String,
    /// Platform channel id.
    pub channel: String,
    /// When true, the monitor ignores messages posted as replies inside a
    /// thread, firing only on top-level channel messages (and thread roots).
    /// Skipped when serializing if false so an enable/disable round-trip never
    /// injects it into a config that didn't set it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub exclude_thread_replies: bool,
}

/// What a source monitor reads, how often, and which part of the reading it
/// watches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Source {
    /// The tool that reads the source, run as the monitor's owner.
    pub tool: String,
    /// The tool's input, as the tool's schema describes it.
    #[serde(default = "empty_object", skip_serializing_if = "is_empty_object")]
    pub input: Value,
    /// Poll cadence in seconds; [`DEFAULT_EVERY_SECS`] when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_secs: Option<u64>,
    /// Pointer to the array of items in the tool's output. Present, the
    /// monitor watches items by id; absent, it watches the output as one value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<String>,
    /// Pointer within each item to its id; [`DEFAULT_ITEM_ID`] when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    /// Pointer to the watched part of each item (or of the output); the whole
    /// of it when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Pointer to the text an observation renders as `{{text}}`; the watched
    /// part's compact JSON when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl Source {
    pub fn every_secs(&self) -> u64 {
        self.every_secs.unwrap_or(DEFAULT_EVERY_SECS)
    }

    pub fn item_id(&self) -> &str {
        self.item_id.as_deref().unwrap_or(DEFAULT_ITEM_ID)
    }

    pub fn path(&self) -> &str {
        self.path.as_deref().unwrap_or("")
    }

    /// Whether this source is watched item by item rather than as one value.
    pub fn watches_items(&self) -> bool {
        self.items.is_some()
    }

    /// The condition a source evaluates when its monitor names none.
    pub fn default_condition(&self) -> Condition {
        Condition::of_kind(if self.watches_items() {
            ConditionKind::Always
        } else {
            ConditionKind::Changed
        })
    }

    /// Whether two sources read the same thing the same way, so observations
    /// under one are comparable with the other's.
    pub fn reads_like(&self, other: &Source) -> bool {
        self.tool == other.tool
            && self.input == other.input
            && self.items == other.items
            && self.item_id() == other.item_id()
            && self.path() == other.path()
    }
}

/// Which messages a channel monitor acts on.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Filter {
    All,
    ContainsAny { values: Vec<String> },
    Regex { pattern: String },
    FromUser { user: String },
    FromBot,
    FromHuman,
    HasFile,
}

/// What a condition tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionKind {
    Always,
    Changed,
    Equals,
    Contains,
    Regex,
    Gt,
    Lt,
    Gte,
    Lte,
    Exists,
    All,
    Any,
}

impl ConditionKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::Changed => "changed",
            Self::Equals => "equals",
            Self::Contains => "contains",
            Self::Regex => "regex",
            Self::Gt => "gt",
            Self::Lt => "lt",
            Self::Gte => "gte",
            Self::Lte => "lte",
            Self::Exists => "exists",
            Self::All => "all",
            Self::Any => "any",
        }
    }

    fn is_combinator(self) -> bool {
        matches!(self, Self::All | Self::Any)
    }
}

/// Which observations a source monitor acts on: a test of the watched part,
/// at `path` within it when set, or a combination of such tests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    #[serde(rename = "type")]
    pub kind: ConditionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub negate: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub of: Vec<Condition>,
}

impl Condition {
    pub fn of_kind(kind: ConditionKind) -> Self {
        Self {
            kind,
            path: None,
            value: None,
            values: Vec::new(),
            pattern: None,
            negate: false,
            of: Vec::new(),
        }
    }

    /// Whether this condition is an *event* — it contains `changed`, so it
    /// fires each time it holds — rather than a *level* that fires on the
    /// transition into holding.
    pub fn is_event(&self) -> bool {
        self.kind == ConditionKind::Changed || self.of.iter().any(Condition::is_event)
    }

    fn validate(&self, id: &str) -> Result<()> {
        let needs =
            |what: &str| eyre::eyre!("Monitor '{id}' {} condition needs {what}", self.kind.name());
        if self.kind.is_combinator() {
            if self.of.is_empty() {
                return Err(needs("at least one condition in `of`"));
            }
            if self.path.is_some()
                || self.value.is_some()
                || !self.values.is_empty()
                || self.pattern.is_some()
            {
                return Err(eyre::eyre!(
                    "Monitor '{id}' {} condition takes only `of` (and `negate`); put a path or value on its parts",
                    self.kind.name()
                ));
            }
            return self.of.iter().try_for_each(|part| part.validate(id));
        }
        validate_pointer(id, "condition.path", self.path.as_deref())?;
        if !self.of.is_empty() {
            return Err(eyre::eyre!(
                "Monitor '{id}' {} condition takes no `of` (only all and any do)",
                self.kind.name()
            ));
        }
        match self.kind {
            ConditionKind::Equals if self.value.is_none() => Err(needs("a `value`")),
            ConditionKind::Gt | ConditionKind::Lt | ConditionKind::Gte | ConditionKind::Lte
                if self.value.as_ref().and_then(as_number).is_none() =>
            {
                Err(needs("a numeric `value`"))
            }
            ConditionKind::Contains if self.values.iter().all(|v| v.is_empty()) => {
                Err(needs("at least one of `values`"))
            }
            ConditionKind::Regex => match self.pattern.as_deref() {
                Some(pattern) => Regex::new(pattern).map(drop).wrap_err_with(|| {
                    format!("Monitor '{id}': invalid condition regex '{pattern}'")
                }),
                None => Err(needs("a `pattern`")),
            },
            _ => Ok(()),
        }
    }
}

/// The one question the decision model is asked before the action runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decide {
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_probability: Option<f64>,
}

/// What a monitor does. `run_workflow` / `run_skill` / `notify` / `agent_task`
/// fire per matching message or observation; `persona` is a standing binding
/// (it makes an agent converse in the channel) applied when the monitor is
/// enabled, not a per-message action.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    RunWorkflow {
        workflow: String,
    },
    /// One direct tool call, no model turn; every string in `input` is
    /// rendered with the event's placeholders.
    RunSkill {
        tool: String,
        #[serde(default = "empty_object", skip_serializing_if = "is_empty_object")]
        input: Value,
    },
    Notify {
        target: String,
        template: String,
        /// The surface to deliver on; a channel monitor's own when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        surface: Option<String>,
    },
    /// Run a one-shot agent turn for the matching message and deliver its reply
    /// to `deliver_to` (a `#channel` id or `@user` DM).
    AgentTask {
        #[serde(default)]
        agent: Option<String>,
        prompt: String,
        deliver_to: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        surface: Option<String>,
    },
    /// Bind `agent` to converse in the watched channel with `prompt` as a
    /// channel-scoped system prompt (response mode defaults to `all`). A standing
    /// binding, applied live while the monitor is enabled.
    Persona {
        agent: String,
        prompt: String,
        #[serde(default)]
        mode: Option<String>,
    },
}

impl Action {
    /// Whether this action fires per matching message (vs. a standing binding).
    pub fn is_per_message(&self) -> bool {
        !matches!(self, Action::Persona { .. })
    }

    /// Where a `notify` or `agent_task` delivers: the target and the surface
    /// it names for it, if any.
    pub fn delivery(&self) -> Option<(&str, Option<&str>)> {
        match self {
            Action::Notify {
                target, surface, ..
            } => Some((target, surface.as_deref())),
            Action::AgentTask {
                deliver_to,
                surface,
                ..
            } => Some((deliver_to, surface.as_deref())),
            Action::RunWorkflow { .. } | Action::RunSkill { .. } | Action::Persona { .. } => None,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Action::RunWorkflow { .. } => "run_workflow",
            Action::RunSkill { .. } => "run_skill",
            Action::Notify { .. } => "notify",
            Action::AgentTask { .. } => "agent_task",
            Action::Persona { .. } => "persona",
        }
    }
}

/// A channel monitor's filter compiled for matching. The regex is built once
/// (at load) so the hot path never recompiles and a bad pattern can never reach
/// dispatch.
pub enum CompiledFilter {
    All,
    ContainsAny(Vec<String>),
    Regex(Regex),
    FromUser(String),
    FromBot,
    FromHuman,
    HasFile,
}

impl CompiledFilter {
    pub fn compile(filter: &Filter) -> Result<Self> {
        Ok(match filter {
            Filter::All => Self::All,
            Filter::ContainsAny { values } => {
                Self::ContainsAny(values.iter().map(|v| v.to_lowercase()).collect())
            }
            Filter::Regex { pattern } => Self::Regex(
                Regex::new(pattern)
                    .wrap_err_with(|| format!("invalid filter regex '{pattern}'"))?,
            ),
            Filter::FromUser { user } => Self::FromUser(user.clone()),
            Filter::FromBot => Self::FromBot,
            Filter::FromHuman => Self::FromHuman,
            Filter::HasFile => Self::HasFile,
        })
    }

    pub fn matches(&self, ev: &InboundEvent) -> bool {
        match self {
            Self::All => true,
            Self::ContainsAny(needles) => {
                let text = ev.text.to_lowercase();
                needles.iter().any(|n| text.contains(n))
            }
            Self::Regex(re) => re.is_match(&ev.text),
            Self::FromUser(u) => ev.user.as_deref() == Some(u.as_str()),
            Self::FromBot => ev.is_bot(),
            Self::FromHuman => !ev.is_bot(),
            Self::HasFile => ev.has_files,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::All => "all",
            Self::ContainsAny(_) => "contains_any",
            Self::Regex(_) => "regex",
            Self::FromUser(_) => "from_user",
            Self::FromBot => "from_bot",
            Self::FromHuman => "from_human",
            Self::HasFile => "has_file",
        }
    }
}

/// The key the TOML parser hides a bare datetime behind when it is read into
/// JSON; a tool would receive that object, so inputs must quote their dates.
const TOML_DATETIME_KEY: &str = "$__toml_private_datetime";

fn holds_toml_datetime(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.contains_key(TOML_DATETIME_KEY) || map.values().any(holds_toml_datetime)
        }
        Value::Array(items) => items.iter().any(holds_toml_datetime),
        _ => false,
    }
}

fn validate_input(id: &str, what: &str, input: &Value) -> Result<()> {
    if !input.is_object() {
        return Err(eyre::eyre!("Monitor '{id}' {what} must be a table"));
    }
    if holds_toml_datetime(input) {
        return Err(eyre::eyre!(
            "Monitor '{id}' {what} holds a bare date; quote it as a string"
        ));
    }
    Ok(())
}

/// A JSON pointer is empty (the whole document) or starts at its root.
pub fn validate_pointer(id: &str, field: &str, pointer: Option<&str>) -> Result<()> {
    match pointer {
        Some(p) if !p.is_empty() && !p.starts_with('/') => Err(eyre::eyre!(
            "Monitor '{id}' {field} '{p}' is not a JSON pointer (it must be empty or start with '/')"
        )),
        _ => Ok(()),
    }
}

impl MonitorDefinition {
    /// Parse a monitor definition from a TOML string.
    pub fn from_toml(toml_str: &str) -> Result<Self> {
        let def: Self = toml::from_str(toml_str).wrap_err("Failed to parse monitor TOML")?;
        def.validate()?;
        Ok(def)
    }

    /// Load a monitor definition from a TOML file.
    pub fn from_file(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .wrap_err_with(|| format!("Failed to read {}", path.display()))?;
        Self::from_toml(&content)
    }

    /// Load every monitor definition in a directory, skipping invalid files.
    pub fn load_dir(dir: &Path) -> Result<Vec<Self>> {
        if !dir.exists() {
            return Ok(vec![]);
        }
        let mut defs = Vec::new();
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "toml") {
                match Self::from_file(&path) {
                    Ok(def) => defs.push(def),
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "Skipping invalid monitor")
                    }
                }
            }
        }
        Ok(defs)
    }

    /// Validate ids, the one thing watched, the filter or condition (compiling
    /// every regex), the decide gate, and the action.
    pub fn validate(&self) -> Result<()> {
        let m = &self.monitor;
        let id = m.id.as_str();
        if id.trim().is_empty() {
            return Err(eyre::eyre!("Monitor id cannot be empty"));
        }
        if m.name.trim().is_empty() {
            return Err(eyre::eyre!("Monitor '{id}' name cannot be empty"));
        }
        if m.agent.as_deref().is_some_and(|a| a.trim().is_empty()) {
            return Err(eyre::eyre!(
                "Monitor '{id}' agent cannot be empty (omit it for the default agent)"
            ));
        }
        match (&m.binding, &m.source) {
            (Some(binding), None) => Self::validate_binding(id, binding, m)?,
            (None, Some(source)) => Self::validate_source(id, source, m)?,
            (Some(_), Some(_)) => {
                return Err(eyre::eyre!(
                    "Monitor '{id}' has both a binding and a source; it watches one thing"
                ))
            }
            (None, None) => {
                return Err(eyre::eyre!(
                    "Monitor '{id}' needs a [monitor.binding] (a channel) or a [monitor.source] (a tool to poll)"
                ))
            }
        }
        if let Some(decide) = &m.decide {
            if decide.question.trim().is_empty() {
                return Err(eyre::eyre!("Monitor '{id}' decide needs a question"));
            }
            if decide
                .min_probability
                .is_some_and(|p| !(0.0..=1.0).contains(&p))
            {
                return Err(eyre::eyre!(
                    "Monitor '{id}' decide min_probability must be between 0 and 1"
                ));
            }
        }
        Self::validate_action(id, &m.action)
    }

    fn validate_binding(id: &str, binding: &Binding, m: &MonitorMeta) -> Result<()> {
        if !matches!(binding.surface.as_str(), "slack" | "teams" | SURFACE_ANY) {
            return Err(eyre::eyre!(
                "Monitor '{id}' has unknown surface '{}' (use slack, teams or any)",
                binding.surface
            ));
        }
        if binding.channel.trim().is_empty() {
            return Err(eyre::eyre!(
                "Monitor '{id}' binding.channel cannot be empty"
            ));
        }
        if m.condition.is_some() {
            return Err(eyre::eyre!(
                "Monitor '{id}' watches a channel, so it takes a [monitor.filter], not a [monitor.condition]"
            ));
        }
        match m.filter() {
            Filter::ContainsAny { values } if values.is_empty() => {
                return Err(eyre::eyre!(
                    "Monitor '{id}' contains_any filter needs at least one value"
                ));
            }
            Filter::FromUser { user } if user.trim().is_empty() => {
                return Err(eyre::eyre!("Monitor '{id}' from_user filter needs a user"));
            }
            _ => {}
        }
        CompiledFilter::compile(m.filter()).map_err(|e| eyre::eyre!("Monitor '{id}': {e}"))?;
        Ok(())
    }

    fn validate_source(id: &str, source: &Source, m: &MonitorMeta) -> Result<()> {
        if source.tool.trim().is_empty() {
            return Err(eyre::eyre!("Monitor '{id}' source needs a tool"));
        }
        validate_input(id, "source.input", &source.input)?;
        if source.every_secs() == 0 {
            return Err(eyre::eyre!(
                "Monitor '{id}' source.every_secs must be greater than zero"
            ));
        }
        validate_pointer(id, "source.items", source.items.as_deref())?;
        validate_pointer(id, "source.item_id", source.item_id.as_deref())?;
        validate_pointer(id, "source.path", source.path.as_deref())?;
        validate_pointer(id, "source.text", source.text.as_deref())?;
        if source.item_id.is_some() && !source.watches_items() {
            return Err(eyre::eyre!(
                "Monitor '{id}' source.item_id needs source.items (a value has no item id)"
            ));
        }
        if m.filter.is_some() {
            return Err(eyre::eyre!(
                "Monitor '{id}' watches a source, so it takes a [monitor.condition], not a [monitor.filter]"
            ));
        }
        if let Some(condition) = &m.condition {
            condition.validate(id)?;
            CompiledCondition::compile(condition)
                .map_err(|e| eyre::eyre!("Monitor '{id}': {e}"))?;
        }
        if matches!(m.action, Action::Persona { .. }) {
            return Err(eyre::eyre!(
                "Monitor '{id}' watches a source, which a persona cannot converse in"
            ));
        }
        Ok(())
    }

    fn validate_action(id: &str, action: &Action) -> Result<()> {
        let needs =
            |what: &str| eyre::eyre!("Monitor '{id}' {} action needs {what}", action.kind());
        match action {
            Action::RunWorkflow { workflow } if workflow.trim().is_empty() => {
                Err(needs("a workflow id"))
            }
            Action::RunSkill { tool, input } => {
                if tool.trim().is_empty() {
                    return Err(needs("a tool"));
                }
                validate_input(id, "run_skill input", input)
            }
            Action::Notify {
                target,
                template,
                surface,
            } => {
                if target.trim().is_empty() {
                    return Err(needs("a target"));
                }
                if template.trim().is_empty() {
                    return Err(needs("a template"));
                }
                Self::validate_surface(id, surface.as_deref())
            }
            Action::AgentTask {
                prompt,
                deliver_to,
                surface,
                ..
            } => {
                if prompt.trim().is_empty() {
                    return Err(needs("a prompt"));
                }
                if deliver_to.trim().is_empty() {
                    return Err(needs("a deliver_to (#channel or @user)"));
                }
                Self::validate_surface(id, surface.as_deref())
            }
            Action::Persona {
                agent,
                prompt,
                mode,
            } => {
                if agent.trim().is_empty() {
                    return Err(needs("an agent"));
                }
                if prompt.trim().is_empty() {
                    return Err(needs("a prompt"));
                }
                match mode.as_deref() {
                    Some(mode) if !matches!(mode, "all" | "mention_only" | "quiet") => {
                        Err(eyre::eyre!(
                            "Monitor '{id}' persona mode '{mode}' must be all, mention_only, or quiet"
                        ))
                    }
                    _ => Ok(()),
                }
            }
            Action::RunWorkflow { .. } => Ok(()),
        }
    }

    fn validate_surface(id: &str, surface: Option<&str>) -> Result<()> {
        match surface {
            Some(s) if !matches!(s, "slack" | "teams") => Err(eyre::eyre!(
                "Monitor '{id}' action surface '{s}' must be slack or teams"
            )),
            _ => Ok(()),
        }
    }

    /// Whether this monitor watches `(surface, channel)`. `any` matches every
    /// surface; a source monitor watches no channel.
    pub fn matches_binding(&self, surface: &str, channel: &str) -> bool {
        self.monitor.binding().is_some_and(|binding| {
            binding.channel == channel
                && (binding.surface == SURFACE_ANY || binding.surface == surface)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(text: &str, user: Option<&str>, bot_id: Option<&str>, has_files: bool) -> InboundEvent {
        InboundEvent {
            surface: "slack".into(),
            workspace: "T1".into(),
            channel: "C1".into(),
            user: user.map(str::to_string),
            bot_id: bot_id.map(str::to_string),
            text: text.into(),
            ts: "1.0".into(),
            thread_ts: None,
            has_files,
        }
    }

    const VALID: &str = r#"
[monitor]
id = "release-forwarder"
name = "Release forwarder"

[monitor.binding]
surface = "slack"
channel = "C0RELEASES"

[monitor.filter]
type = "contains_any"
values = ["released", "shipped"]

[monitor.action]
type = "run_workflow"
workflow = "release-note"
"#;

    const SOURCE: &str = r#"
[monitor]
id = "invoices"
name = "Invoices"

[monitor.source]
tool = "google.gmail.search"
input = { query = "from:billing@acme.com", max_results = 20 }
items = "/messages"

[monitor.condition]
type = "contains"
path = "/snippet"
values = ["invoice"]

[monitor.action]
type = "run_skill"
tool = "books.file_invoice"
input = { id = "{{id}}" }
"#;

    #[test]
    fn parses_valid() {
        let def = MonitorDefinition::from_toml(VALID).unwrap();
        assert_eq!(def.monitor.id, "release-forwarder");
        assert!(def.monitor.enabled);
        assert!(matches!(def.monitor.action, Action::RunWorkflow { .. }));
        assert!(def.matches_binding("slack", "C0RELEASES"));
        assert!(!def.matches_binding("slack", "C0OTHER"));
        assert!(def.monitor.source().is_none());
        assert!(def.monitor.condition().is_none());
    }

    #[test]
    fn parses_a_source_monitor_with_its_defaults() {
        let def = MonitorDefinition::from_toml(SOURCE).unwrap();
        let source = def.monitor.source().expect("source");
        assert_eq!(source.tool, "google.gmail.search");
        assert_eq!(source.input["max_results"], serde_json::json!(20));
        assert_eq!(source.every_secs(), DEFAULT_EVERY_SECS);
        assert_eq!(source.item_id(), DEFAULT_ITEM_ID);
        assert!(source.watches_items());
        assert!(def.monitor.binding().is_none());
        assert!(!def.matches_binding("slack", "C0RELEASES"));
        assert_eq!(
            def.monitor.condition().unwrap().kind,
            ConditionKind::Contains
        );
        assert!(matches!(def.monitor.action, Action::RunSkill { .. }));

        let out = toml::to_string(&def).unwrap();
        assert!(
            !out.contains("every_secs"),
            "defaults are not injected: {out}"
        );
        assert!(!out.contains("item_id"), "{out}");
        assert!(!out.contains("once"), "{out}");
        let again = MonitorDefinition::from_toml(&out).unwrap();
        assert_eq!(again.monitor.source(), def.monitor.source());
    }

    #[test]
    fn a_source_defaults_its_condition_by_mode() {
        let items = MonitorDefinition::from_toml(&SOURCE.replace(
            "[monitor.condition]\ntype = \"contains\"\npath = \"/snippet\"\nvalues = [\"invoice\"]\n",
            "",
        ))
        .unwrap();
        assert_eq!(
            items.monitor.condition().unwrap().kind,
            ConditionKind::Always
        );
        let value = MonitorDefinition::from_toml(
            &SOURCE
                .replace("items = \"/messages\"\n", "")
                .replace(
                    "[monitor.condition]\ntype = \"contains\"\npath = \"/snippet\"\nvalues = [\"invoice\"]\n",
                    "",
                ),
        )
        .unwrap();
        assert_eq!(
            value.monitor.condition().unwrap().kind,
            ConditionKind::Changed
        );
    }

    #[test]
    fn exactly_one_of_binding_and_source() {
        let both = format!("{VALID}\n[monitor.source]\ntool = \"http.request\"\n");
        let err = MonitorDefinition::from_toml(&both).unwrap_err();
        assert!(
            err.to_string().contains("both a binding and a source"),
            "{err}"
        );
        let neither = VALID.replace(
            "[monitor.binding]\nsurface = \"slack\"\nchannel = \"C0RELEASES\"\n",
            "",
        );
        let err = MonitorDefinition::from_toml(&neither).unwrap_err();
        assert!(
            err.to_string().contains("needs a [monitor.binding]"),
            "{err}"
        );
    }

    #[test]
    fn filter_and_condition_belong_to_their_own_kind() {
        let err = MonitorDefinition::from_toml(&format!(
            "{VALID}\n[monitor.condition]\ntype = \"always\"\n"
        ))
        .unwrap_err();
        assert!(
            err.to_string().contains("not a [monitor.condition]"),
            "{err}"
        );
        let err =
            MonitorDefinition::from_toml(&format!("{SOURCE}\n[monitor.filter]\ntype = \"all\"\n"))
                .unwrap_err();
        assert!(err.to_string().contains("not a [monitor.filter]"), "{err}");
    }

    #[test]
    fn a_source_refuses_a_persona_and_bad_pointers_and_a_zero_cadence() {
        let persona = SOURCE.replace(
            "type = \"run_skill\"\ntool = \"books.file_invoice\"\ninput = { id = \"{{id}}\" }",
            "type = \"persona\"\nagent = \"lynx\"\nprompt = \"p\"",
        );
        assert!(MonitorDefinition::from_toml(&persona)
            .unwrap_err()
            .to_string()
            .contains("persona"));
        assert!(MonitorDefinition::from_toml(
            &SOURCE.replace("items = \"/messages\"", "items = \"messages\"")
        )
        .unwrap_err()
        .to_string()
        .contains("not a JSON pointer"));
        assert!(MonitorDefinition::from_toml(&SOURCE.replace(
            "items = \"/messages\"",
            "items = \"/messages\"\nevery_secs = 0"
        ))
        .unwrap_err()
        .to_string()
        .contains("every_secs"));
        assert!(MonitorDefinition::from_toml(
            &SOURCE.replace("items = \"/messages\"", "item_id = \"/id\"")
        )
        .unwrap_err()
        .to_string()
        .contains("needs source.items"));
    }

    #[test]
    fn a_bare_date_in_an_input_is_refused() {
        let dated = SOURCE.replace(
            "input = { query = \"from:billing@acme.com\", max_results = 20 }",
            "input = { query = \"q\", since = 2026-10-01T00:00:00Z }",
        );
        assert!(MonitorDefinition::from_toml(&dated)
            .unwrap_err()
            .to_string()
            .contains("quote it as a string"));
        let nested = SOURCE.replace(
            "input = { id = \"{{id}}\" }",
            "input = { id = \"{{id}}\", when = [{ at = 2026-10-01 }] }",
        );
        assert!(MonitorDefinition::from_toml(&nested)
            .unwrap_err()
            .to_string()
            .contains("run_skill input holds a bare date"));
    }

    #[test]
    fn conditions_are_validated_by_kind() {
        let with = |condition: &str| {
            MonitorDefinition::from_toml(&SOURCE.replace(
                "type = \"contains\"\npath = \"/snippet\"\nvalues = [\"invoice\"]",
                condition,
            ))
        };
        assert!(with("type = \"equals\"")
            .unwrap_err()
            .to_string()
            .contains("needs a `value`"));
        assert!(with("type = \"gt\"\nvalue = \"high\"")
            .unwrap_err()
            .to_string()
            .contains("numeric"));
        assert!(
            with("type = \"gt\"\nvalue = \"3\"").is_ok(),
            "a numeric string is a number"
        );
        assert!(with("type = \"contains\"\nvalues = []")
            .unwrap_err()
            .to_string()
            .contains("values"));
        assert!(with("type = \"regex\"\npattern = \"(unclosed\"")
            .unwrap_err()
            .to_string()
            .contains("regex"));
        assert!(with("type = \"all\"")
            .unwrap_err()
            .to_string()
            .contains("`of`"));
        assert!(with("type = \"exists\"\nof = [{ type = \"always\" }]")
            .unwrap_err()
            .to_string()
            .contains("takes no `of`"));
        assert!(with("type = \"all\"\nof = [{ type = \"gte\", path = \"/n\", value = 3 }, { type = \"changed\" }]").is_ok());
        assert!(
            with("type = \"any\"\npath = \"/n\"\nof = [{ type = \"always\" }]")
                .unwrap_err()
                .to_string()
                .contains("takes only `of`")
        );
        assert!(with("type = \"exists\"\npath = \"nope\"")
            .unwrap_err()
            .to_string()
            .contains("not a JSON pointer"));
    }

    #[test]
    fn is_event_sees_through_combinators() {
        let changed = Condition::of_kind(ConditionKind::Changed);
        let level = Condition::of_kind(ConditionKind::Exists);
        assert!(changed.is_event());
        assert!(!level.is_event());
        let mut all = Condition::of_kind(ConditionKind::All);
        all.of = vec![level.clone(), changed];
        assert!(all.is_event());
        let mut any = Condition::of_kind(ConditionKind::Any);
        any.of = vec![level.clone(), level];
        assert!(!any.is_event());
    }

    #[test]
    fn once_and_expires_at_parse_and_round_trip_without_injecting_defaults() {
        let def = MonitorDefinition::from_toml(VALID).unwrap();
        assert!(!def.monitor.once);
        assert!(def.monitor.expires_at.is_none());
        assert!(!def.monitor.expired_at(Utc::now()));

        let quoted = VALID.replace(
            "name = \"Release forwarder\"",
            "name = \"R\"\nonce = true\nexpires_at = \"2026-10-20T09:00:00Z\"",
        );
        let def = MonitorDefinition::from_toml(&quoted).unwrap();
        assert!(def.monitor.once);
        let at = def.monitor.expires_at.unwrap();
        assert_eq!(at.to_rfc3339(), "2026-10-20T09:00:00+00:00");
        assert!(def.monitor.expired_at(at));
        assert!(!def.monitor.expired_at(at - chrono::Duration::seconds(1)));
        let out = toml::to_string(&def).unwrap();
        assert!(out.contains("once = true"), "{out}");
        assert!(
            out.contains("expires_at = \"2026-10-20T09:00:00Z\""),
            "{out}"
        );
        assert_eq!(
            MonitorDefinition::from_toml(&out)
                .unwrap()
                .monitor
                .expires_at,
            Some(at)
        );

        let bare = VALID.replace(
            "name = \"Release forwarder\"",
            "name = \"R\"\nexpires_at = 2026-10-20T09:00:00Z",
        );
        assert_eq!(
            MonitorDefinition::from_toml(&bare)
                .unwrap()
                .monitor
                .expires_at,
            Some(at),
            "a bare TOML datetime is accepted too"
        );
        let bad = VALID.replace(
            "name = \"Release forwarder\"",
            "name = \"R\"\nexpires_at = \"tomorrow\"",
        );
        let err = MonitorDefinition::from_toml(&bad).unwrap_err();
        assert!(format!("{err:#}").contains("RFC 3339"), "{err:#}");
    }

    #[test]
    fn decide_is_validated() {
        let with = |decide: &str| {
            MonitorDefinition::from_toml(&format!("{VALID}\n[monitor.decide]\n{decide}\n"))
        };
        let def = with("question = \"Is it a release?\"\nyes = \"a shipped version\"").unwrap();
        let decide = def.monitor.decide.as_ref().unwrap();
        assert_eq!(decide.yes.as_deref(), Some("a shipped version"));
        assert!(decide.no.is_none());
        assert!(with("question = \" \"")
            .unwrap_err()
            .to_string()
            .contains("question"));
        assert!(with("question = \"q\"\nmin_probability = 1.5")
            .unwrap_err()
            .to_string()
            .contains("between 0 and 1"));
    }

    #[test]
    fn exclude_thread_replies_defaults_false_and_skips_when_serialized_false() {
        let def = MonitorDefinition::from_toml(VALID).unwrap();
        assert!(!def.monitor.binding().unwrap().exclude_thread_replies);
        let out = toml::to_string(&def).unwrap();
        assert!(!out.contains("exclude_thread_replies"), "{out}");
        let with = VALID.replace(
            "channel = \"C0RELEASES\"",
            "channel = \"C0RELEASES\"\nexclude_thread_replies = true",
        );
        let def = MonitorDefinition::from_toml(&with).unwrap();
        assert!(def.monitor.binding().unwrap().exclude_thread_replies);
        assert!(toml::to_string(&def)
            .unwrap()
            .contains("exclude_thread_replies = true"));
    }

    #[test]
    fn watcher_prefers_explicit_agent_then_action_agent() {
        let def = MonitorDefinition::from_toml(VALID).unwrap();
        assert_eq!(def.monitor.watcher(), None, "run_workflow → default agent");
        assert!(
            !toml::to_string(&def).unwrap().contains("agent"),
            "an absent agent is not injected on round-trip"
        );

        let explicit = VALID.replace(
            "name = \"Release forwarder\"",
            "name = \"R\"\nagent = \"candy\"",
        );
        let def = MonitorDefinition::from_toml(&explicit).unwrap();
        assert_eq!(def.monitor.watcher(), Some("candy"));
        assert!(toml::to_string(&def).unwrap().contains("agent = \"candy\""));

        let task = with_action(
            "[monitor.action]\ntype = \"agent_task\"\nagent = \"lynx\"\nprompt = \"p\"\ndeliver_to = \"@U1\"\n",
        );
        let def = MonitorDefinition::from_toml(&task).unwrap();
        assert_eq!(def.monitor.watcher(), Some("lynx"), "action agent watches");
        let both = task.replace(
            "name = \"Release forwarder\"",
            "name = \"R\"\nagent = \"candy\"",
        );
        let def = MonitorDefinition::from_toml(&both).unwrap();
        assert_eq!(def.monitor.watcher(), Some("candy"), "explicit wins");

        let persona = with_action(
            "[monitor.action]\ntype = \"persona\"\nagent = \"lynx\"\nprompt = \"triage\"\n",
        );
        let def = MonitorDefinition::from_toml(&persona).unwrap();
        assert_eq!(def.monitor.watcher(), Some("lynx"));

        let blank = VALID.replace(
            "name = \"Release forwarder\"",
            "name = \"R\"\nagent = \"  \"",
        );
        assert!(
            MonitorDefinition::from_toml(&blank).is_err(),
            "blank agent rejected"
        );
    }

    #[test]
    fn any_surface_matches_all() {
        let toml = VALID.replace("surface = \"slack\"", "surface = \"any\"");
        let def = MonitorDefinition::from_toml(&toml).unwrap();
        assert!(def.matches_binding("slack", "C0RELEASES"));
        assert!(def.matches_binding("teams", "C0RELEASES"));
    }

    fn with_action(action_toml: &str) -> String {
        let base = VALID.split("[monitor.action]").next().unwrap();
        format!("{base}{action_toml}")
    }

    #[test]
    fn agent_task_action_parses_and_validates() {
        let ok = with_action(
            "[monitor.action]\ntype = \"agent_task\"\nagent = \"lynx\"\nprompt = \"summarise\"\ndeliver_to = \"@U1\"\n",
        );
        let def = MonitorDefinition::from_toml(&ok).unwrap();
        assert!(def.monitor.action.is_per_message());
        assert_eq!(def.monitor.action.delivery(), Some(("@U1", None)));
        assert!(MonitorDefinition::from_toml(
            &ok.replace("prompt = \"summarise\"", "prompt = \"\"")
        )
        .is_err());
        assert!(MonitorDefinition::from_toml(
            &ok.replace("deliver_to = \"@U1\"", "deliver_to = \"\"")
        )
        .is_err());
        let surfaced = MonitorDefinition::from_toml(&format!("{ok}surface = \"teams\"\n")).unwrap();
        assert_eq!(
            surfaced.monitor.action.delivery(),
            Some(("@U1", Some("teams")))
        );
        assert!(MonitorDefinition::from_toml(&format!("{ok}surface = \"discord\"\n")).is_err());
    }

    #[test]
    fn run_skill_action_parses_and_validates() {
        let ok = with_action(
            "[monitor.action]\ntype = \"run_skill\"\ntool = \"books.file\"\ninput = { id = \"{{id}}\" }\n",
        );
        let def = MonitorDefinition::from_toml(&ok).unwrap();
        assert!(def.monitor.action.is_per_message());
        assert_eq!(def.monitor.action.delivery(), None);
        assert_eq!(def.monitor.action.kind(), "run_skill");
        assert!(
            MonitorDefinition::from_toml(&ok.replace("tool = \"books.file\"", "tool = \"\""))
                .is_err()
        );
        let bare = with_action("[monitor.action]\ntype = \"run_skill\"\ntool = \"books.file\"\n");
        let def = MonitorDefinition::from_toml(&bare).unwrap();
        assert!(
            !toml::to_string(&def).unwrap().contains("input"),
            "an empty input is not injected"
        );
    }

    #[test]
    fn persona_action_parses_is_standing_and_validates_mode() {
        let ok = with_action(
            "[monitor.action]\ntype = \"persona\"\nagent = \"lynx\"\nprompt = \"triage\"\nmode = \"all\"\n",
        );
        let def = MonitorDefinition::from_toml(&ok).unwrap();
        assert!(
            !def.monitor.action.is_per_message(),
            "persona is a standing binding"
        );
        assert!(
            MonitorDefinition::from_toml(&ok.replace("agent = \"lynx\"", "agent = \"\"")).is_err()
        );
        assert!(
            MonitorDefinition::from_toml(&ok.replace("mode = \"all\"", "mode = \"loud\"")).is_err()
        );
    }

    #[test]
    fn rejects_bad_inputs() {
        let cases = [
            ("id = \"release-forwarder\"", "id = \"\""),
            ("surface = \"slack\"", "surface = \"discord\""),
            ("channel = \"C0RELEASES\"", "channel = \"\""),
            ("values = [\"released\", \"shipped\"]", "values = []"),
            ("workflow = \"release-note\"", "workflow = \"\""),
        ];
        for (from, to) in cases {
            let toml = VALID.replace(from, to);
            assert!(
                MonitorDefinition::from_toml(&toml).is_err(),
                "expected rejection for {to}"
            );
        }
    }

    #[test]
    fn rejects_uncompilable_regex() {
        let toml = VALID.replace(
            "type = \"contains_any\"\nvalues = [\"released\", \"shipped\"]",
            "type = \"regex\"\npattern = \"(unclosed\"",
        );
        assert!(MonitorDefinition::from_toml(&toml).is_err());
    }

    #[test]
    fn a_channel_monitor_without_a_filter_takes_every_message() {
        let toml = VALID.replace(
            "[monitor.filter]\ntype = \"contains_any\"\nvalues = [\"released\", \"shipped\"]\n",
            "",
        );
        let def = MonitorDefinition::from_toml(&toml).unwrap();
        assert!(matches!(def.monitor.filter(), Filter::All));
        assert!(!toml::to_string(&def).unwrap().contains("[monitor.filter]"));
    }

    #[test]
    fn notify_action_requires_target_and_template() {
        let base = VALID.replace(
            "type = \"run_workflow\"\nworkflow = \"release-note\"",
            "type = \"notify\"\ntarget = \"C0OUT\"\ntemplate = \"new: {{text}}\"",
        );
        assert!(MonitorDefinition::from_toml(&base).is_ok());
        assert!(
            MonitorDefinition::from_toml(&base.replace("target = \"C0OUT\"", "target = \"\""))
                .is_err()
        );
        assert!(MonitorDefinition::from_toml(
            &base.replace("template = \"new: {{text}}\"", "template = \"\"")
        )
        .is_err());
    }

    #[test]
    fn filter_truth_table() {
        let compile = |f: Filter| CompiledFilter::compile(&f).unwrap();

        assert!(compile(Filter::All).matches(&ev("anything", None, None, false)));

        let contains = compile(Filter::ContainsAny {
            values: vec!["Shipped".into()],
        });
        assert!(contains.matches(&ev("We SHIPPED v2", None, None, false)));
        assert!(!contains.matches(&ev("nothing here", None, None, false)));

        let re = compile(Filter::Regex {
            pattern: r"v\d+".into(),
        });
        assert!(re.matches(&ev("release v12", None, None, false)));
        assert!(!re.matches(&ev("release", None, None, false)));

        let from_user = compile(Filter::FromUser { user: "U1".into() });
        assert!(from_user.matches(&ev("x", Some("U1"), None, false)));
        assert!(!from_user.matches(&ev("x", Some("U2"), None, false)));

        assert!(compile(Filter::FromBot).matches(&ev("x", None, Some("B1"), false)));
        assert!(!compile(Filter::FromBot).matches(&ev("x", Some("U1"), None, false)));
        assert!(compile(Filter::FromHuman).matches(&ev("x", Some("U1"), None, false)));
        assert!(!compile(Filter::FromHuman).matches(&ev("x", None, Some("B1"), false)));

        assert!(compile(Filter::HasFile).matches(&ev("x", None, None, true)));
        assert!(!compile(Filter::HasFile).matches(&ev("x", None, None, false)));
    }

    #[test]
    fn reads_like_ignores_cadence_and_text_but_not_the_query() {
        let def = MonitorDefinition::from_toml(SOURCE).unwrap();
        let source = def.monitor.source().unwrap();
        let mut same = source.clone();
        same.every_secs = Some(60);
        same.text = Some("/subject".into());
        assert!(source.reads_like(&same));
        let mut other = source.clone();
        other.input["query"] = Value::String("from:else".into());
        assert!(!source.reads_like(&other));
    }
}
